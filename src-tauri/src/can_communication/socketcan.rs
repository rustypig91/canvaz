use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use socketcan::frame::FdFlags;
use socketcan::embedded_can::{ExtendedId, Id, StandardId};
use socketcan::{CanAnyFrame, CanDataFrame, CanError, CanFdFrame, CanFdSocket, CanSocket, EmbeddedFrame, Frame, Socket, SocketOptions};

use super::{BusState, BusStatus, CanBackend, CanFrame, CanOpenError, ChannelCapabilities, ChannelSettings, FrameFormat, FrameMetadata, RxHandle, TxHandle};

const CAN_FD_MTU: u32 = 72;

enum SocketCanSocket {
    Classical(CanSocket),
    Fd(CanFdSocket),
}

impl SocketCanSocket {
    fn read_frame(&self) -> std::io::Result<CanAnyFrame> {
        match self {
            Self::Classical(socket) => socket.read_frame().map(Into::into),
            Self::Fd(socket) => socket.read_frame(),
        }
    }

    fn write_frame(&self, frame: &CanAnyFrame) -> std::io::Result<()> {
        match (self, frame) {
            (Self::Classical(socket), CanAnyFrame::Normal(frame)) => socket.write_frame(frame),
            (Self::Fd(socket), frame) => socket.write_frame(frame),
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "SocketCAN channel requires Classical CAN data frames")),
        }
    }
}

// Error-frame class bits in the error frame's CAN id (linux/can/error.h).
const CAN_ERR_CRTL: u32 = 0x0000_0004; // controller problems; detail in data[1]
const CAN_ERR_BUSOFF: u32 = 0x0000_0040;
const CAN_ERR_RESTARTED: u32 = 0x0000_0100;
const CAN_ERR_CNT: u32 = 0x0000_0200; // TEC/REC in data[6]/data[7]

// data[1] detail bits of a CAN_ERR_CRTL error frame.
const CAN_ERR_CRTL_RX_WARNING: u8 = 0x04;
const CAN_ERR_CRTL_TX_WARNING: u8 = 0x08;
const CAN_ERR_CRTL_RX_PASSIVE: u8 = 0x10;
const CAN_ERR_CRTL_TX_PASSIVE: u8 = 0x20;
const CAN_ERR_CRTL_ACTIVE: u8 = 0x40;

// ── RX handle ─────────────────────────────────────────────────────────────────

pub(crate) struct SocketCanRxHandle {
    socket: Arc<SocketCanSocket>,
    configured_timeout_ms: u64,
    /// Latest controller state gleaned from kernel error frames. SocketCAN has
    /// no polling API for this on a plain RAW socket, so the state is only as
    /// fresh as the last error frame (the kernel emits one on each state
    /// change, so a healthy-again bus is reported too via CRTL_ACTIVE).
    last_status: BusStatus,
}

impl RxHandle for SocketCanRxHandle {
    fn receive(&mut self, timeout_ms: u64) -> Result<Option<CanFrame>, String> {
        if self.configured_timeout_ms != timeout_ms {
            let timeout = Duration::from_millis(timeout_ms);
            match self.socket.as_ref() {
                SocketCanSocket::Classical(socket) => socket.set_read_timeout(timeout),
                SocketCanSocket::Fd(socket) => socket.set_read_timeout(timeout),
            }.map_err(|e| format!("set_read_timeout failed: {e}"))?;
            self.configured_timeout_ms = timeout_ms;
        }
        match self.socket.read_frame() {
            Ok(CanAnyFrame::Normal(df)) => Ok(Some(CanFrame {
                metadata: Default::default(),
                can_id: df.raw_id(),
                is_extended: df.is_extended(),
                data: df.data().to_vec(),
                timestamp_ms: None,
                error: None,
            })),
            Ok(CanAnyFrame::Fd(fd)) => Ok(Some(CanFrame {
                metadata: FrameMetadata::fd(fd.is_brs(), fd.is_esi()),
                can_id: fd.raw_id(),
                is_extended: fd.is_extended(),
                data: fd.data().to_vec(),
                timestamp_ms: None,
                error: None,
            })),
            Ok(CanAnyFrame::Error(ef)) => {
                let bits = ef.error_bits();
                let data = ef.data().to_vec();
                self.update_status(bits, &data);
                let desc = format!("{}", CanError::from(ef));
                Ok(Some(CanFrame {
                    metadata: Default::default(),
                    can_id: 0,
                    is_extended: false,
                    data,
                    timestamp_ms: None,
                    error: Some(desc),
                }))
            }
            Ok(_) => Ok(None),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn poll_status(&mut self) -> Option<BusStatus> {
        // Report only once something is known — before the first error frame
        // there is nothing to say (and most captures never see one).
        (self.last_status.bus_state.is_some() || self.last_status.tx_err.is_some()).then_some(self.last_status)
    }

    fn close(&mut self) {}
}

impl SocketCanRxHandle {
    fn update_status(&mut self, bits: u32, data: &[u8]) {
        if bits & CAN_ERR_BUSOFF != 0 {
            self.last_status.bus_state = Some(BusState::BusOff);
        } else if bits & CAN_ERR_RESTARTED != 0 {
            self.last_status.bus_state = Some(BusState::Active);
        } else if bits & CAN_ERR_CRTL != 0 {
            if let Some(&d) = data.get(1) {
                if d & (CAN_ERR_CRTL_RX_PASSIVE | CAN_ERR_CRTL_TX_PASSIVE) != 0 {
                    self.last_status.bus_state = Some(BusState::Passive);
                } else if d & (CAN_ERR_CRTL_RX_WARNING | CAN_ERR_CRTL_TX_WARNING) != 0 {
                    self.last_status.bus_state = Some(BusState::Warning);
                } else if d & CAN_ERR_CRTL_ACTIVE != 0 {
                    self.last_status.bus_state = Some(BusState::Active);
                }
            }
        }
        // Kernels ≥ 5.16 piggyback the error counters on every error frame.
        if bits & CAN_ERR_CNT != 0 {
            self.last_status.tx_err = data.get(6).map(|&b| b as u32);
            self.last_status.rx_err = data.get(7).map(|&b| b as u32);
        }
    }
}

// ── TX handle ─────────────────────────────────────────────────────────────────

pub(crate) struct SocketCanTxHandle {
    socket: Arc<SocketCanSocket>,
}

impl TxHandle for SocketCanTxHandle {
    fn send(&mut self, frame: &mut CanFrame) -> Result<(), String> {
        let id = if frame.is_extended {
            ExtendedId::new(frame.can_id).map(Id::from)
        } else {
            u16::try_from(frame.can_id).ok().and_then(StandardId::new).map(Id::from)
        }.ok_or("Invalid CAN identifier")?;
        let native = match frame.metadata.format {
            FrameFormat::Classical => CanDataFrame::new(id, &frame.data).map(CanAnyFrame::Normal),
            FrameFormat::Fd => {
                let flags = if frame.metadata.brs { FdFlags::BRS } else { FdFlags::empty() };
                CanFdFrame::with_flags(id, &frame.data, flags).map(CanAnyFrame::Fd)
            }
        }.ok_or("Failed to build SocketCAN frame")?;
        self.socket.write_frame(&native).map_err(|e| format!("Write failed: {e}"))?;
        frame.timestamp_ms = Some(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64);
        Ok(())
    }

    fn close(&mut self) {}
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct SocketCanBackend;

impl CanBackend for SocketCanBackend {
    fn list_channels(&self) -> Vec<String> {
        let mut channels = can_links("can");
        for name in can_links("vcan") {
            if !channels.contains(&name) {
                channels.push(name);
            }
        }
        channels
    }

    fn capabilities(&self, index: u8) -> ChannelCapabilities {
        let Some(name) = self.list_channels().get(index as usize).cloned() else {
            return ChannelCapabilities { fd: None, external_timing: false, fd_limitation: None };
        };
        if is_vcan(&name) {
            // MTU can be changed independently of Canvaz; report the exact
            // constraint when opening, so a new configuration is retryable.
            return ChannelCapabilities { fd: Some(true), external_timing: true, fd_limitation: None };
        }
        let fd = socketcan_fd_capability(&name);
        ChannelCapabilities {
            fd, external_timing: false,
            fd_limitation: (fd == Some(false)).then_some("This SocketCAN interface or driver does not support ISO CAN FD"),
        }
    }

    fn open_channel(
        &mut self,
        index: u8,
        settings: ChannelSettings,
        admin_password: Option<&str>,
    ) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), CanOpenError> {
        let channels = self.list_channels();
        let name = channels
            .get(index as usize)
            .ok_or_else(|| {
                CanOpenError::ChannelIndexOutOfRange(format!("SocketCAN channel index {index} out of range ({} found)", channels.len()))
            })?
            .clone();

        let details = link_details(&name).ok_or_else(|| format!("Cannot inspect SocketCAN interface '{name}'"))?;
        let virtual_link = is_vcan_details(&details);
        let fd = settings.mode == FrameFormat::Fd;
        if virtual_link && fd && !link_mtu(&details).is_some_and(|mtu| mtu >= CAN_FD_MTU) {
            return Err(format!("Virtual interface '{name}' requires an MTU of at least {CAN_FD_MTU} for CAN FD").into());
        }

        let was_up = link_is_up(&details);
        let reconfigure = !virtual_link && !configured_timing_matches(&details, settings);
        if reconfigure {
            let nominal = settings.bitrate.to_string();
            let silent = if settings.listen_only { "on" } else { "off" };
            let mut args = vec!["link", "set", &name, "type", "can", "bitrate", &nominal, "listen-only", silent];
            let data;
            if fd {
                data = settings.data_bitrate()?.to_string();
                args.extend(["dbitrate", &data, "fd", "on"]);
                // The Linux default is ISO. Clear non-ISO mode when active.
                if controller_flag(&details, "FD-NON-ISO") {
                    args.extend(["fd-non-iso", "off"]);
                }
            } else if controller_flag(&details, "FD") {
                args.extend(["fd", "off"]);
            }
            if was_up {
                run_ip_auto(&["link", "set", &name, "down"], admin_password)?;
            }
            run_ip_auto(&args, admin_password).map_err(|e| match e {
                CanOpenError::Other(message) if fd => CanOpenError::Other(format!(
                    "Cannot configure ISO CAN FD on '{name}'. Check adapter and driver support and both bitrates: {message}"
                )),
                error => error,
            })?;
        }
        if reconfigure || !was_up {
            run_ip_auto(&["link", "set", &name, "up"], admin_password)?;
        }

        // Share one raw socket to avoid receiving our own TX entries as RX.
        let open_error = |e| CanOpenError::Other(format!("Failed to open SocketCAN on '{name}': {e}"));
        let socket = if fd {
            let socket = CanFdSocket::open(&name).map_err(open_error)?;
            enable_error_frames(&socket, &name);
            SocketCanSocket::Fd(socket)
        } else {
            let socket = CanSocket::open(&name).map_err(open_error)?;
            enable_error_frames(&socket, &name);
            SocketCanSocket::Classical(socket)
        };
        let socket = Arc::new(socket);

        Ok((
            Box::new(SocketCanTxHandle { socket: Arc::clone(&socket) }),
            Box::new(SocketCanRxHandle {
                socket,
                configured_timeout_ms: 0,
                last_status: BusStatus::default(),
            }),
        ))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Try running `ip <args>`. If the command fails with a permission error and no
/// password was supplied, return `PasswordRequired`. If a password is supplied,
/// retry under `sudo -S`.
fn run_ip_auto(args: &[&str], admin_password: Option<&str>) -> Result<(), CanOpenError> {
    match run_ip(args, None) {
        Ok(()) => Ok(()),
        Err(e) if e.starts_with("needs-sudo:") => match admin_password {
            Some(pw) => run_ip(args, Some(pw)).map_err(CanOpenError::Other),
            None => Err(CanOpenError::PasswordRequired),
        },
        Err(e) => Err(CanOpenError::Other(e)),
    }
}

fn is_perm(msg: &str) -> bool {
    let lo = msg.to_lowercase();
    lo.contains("operation not permitted") || lo.contains("permission denied")
}

fn run_ip(args: &[&str], sudo_password: Option<&str>) -> Result<(), String> {
    if let Some(pass) = sudo_password {
        let mut child = std::process::Command::new("sudo")
            .arg("-S")
            .arg("ip")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn sudo: {e}"))?;
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = writeln!(stdin, "{pass}");
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if out.status.success() {
            return Ok(());
        }
        let raw = String::from_utf8_lossy(&out.stderr);
        let msg = raw
            .lines()
            .filter(|l| {
                let lo = l.to_lowercase();
                !lo.contains("[sudo]") && !lo.starts_with("password")
            })
            .collect::<Vec<_>>()
            .join("\n");
        Err(if msg.trim().is_empty() {
            raw.trim().to_string()
        } else {
            msg.trim().to_string()
        })
    } else {
        let out = std::process::Command::new("ip")
            .args(args)
            .output()
            .map_err(|e| format!("Failed to run 'ip': {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if is_perm(&msg) { format!("needs-sudo: {msg}") } else { msg })
    }
}

fn can_links(kind: &str) -> Vec<String> {
    let Ok(output) = std::process::Command::new("ip").args(["-o", "link", "show", "type", kind]).output() else {
        return Vec::new();
    };
    if !output.status.success() { return Vec::new(); }
    String::from_utf8_lossy(&output.stdout).lines()
        .filter_map(|line| line.split_whitespace().nth(1)?.strip_suffix(':').map(str::to_string))
        .collect()
}

// Recent iproute2 reports supported and fixed controller modes in JSON.
fn socketcan_fd_capability(name: &str) -> Option<bool> {
    let output = std::process::Command::new("ip").args(["-json", "-details", "link", "show", "dev", name]).output().ok()?;
    if !output.status.success() { return None; }
    let links: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let info = &links[0]["linkinfo"]["info_data"];
    let has_mode = |modes: &[serde_json::Value], name: &str| modes.iter().any(|flag| flag.as_str() == Some(name));
    let fixed = info["ctrlmode_static"].as_array();
    if fixed.is_some_and(|modes| has_mode(modes, "FD-NON-ISO")) { return Some(false); }
    if fixed.is_some_and(|modes| has_mode(modes, "FD")) { return Some(true); }
    info["ctrlmode_supported"].as_array().map(|modes| has_mode(modes, "FD"))
}

fn is_vcan(name: &str) -> bool {
    link_details(name).is_some_and(|details| is_vcan_details(&details))
}

fn is_vcan_details(details: &str) -> bool {
    details.lines().skip(1).any(|line| line.split_whitespace().next() == Some("vcan"))
}

fn link_mtu(details: &str) -> Option<u32> {
    let mut fields = details.lines().next()?.split_whitespace();
    fields.find(|field| *field == "mtu")?;
    fields.next()?.parse().ok()
}

fn link_details(name: &str) -> Option<String> {
    let output = std::process::Command::new("ip").args(["-details", "link", "show", "dev", name]).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn enable_error_frames(socket: &impl SocketOptions, name: &str) {
    if let Err(e) = socket.set_error_filter_accept_all() {
        log::warn!("set_error_filter failed on '{name}': {e} — error frames will not be shown");
    }
}

fn controller_flag(details: &str, expected: &str) -> bool {
    details.lines().filter_map(|line| line.trim().strip_prefix("can <"))
        .filter_map(|line| line.split_once('>')).any(|(flags, _)| flags.split(',').any(|flag| flag == expected))
}

fn link_is_up(details: &str) -> bool {
    details.lines().next().and_then(|line| line.split_once('<'))
        .and_then(|(_, rest)| rest.split_once('>')).is_some_and(|(flags, _)| flags.split(',').any(|flag| flag == "UP"))
}

fn configured_timing_matches(details: &str, settings: ChannelSettings) -> bool {
    let fd = settings.mode == FrameFormat::Fd;
    if controller_flag(details, "LISTEN-ONLY") != settings.listen_only || controller_flag(details, "FD") != fd {
        return false;
    }
    let bitrate = |key: &str| details.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == key).then(|| fields.next()?.parse::<u32>().ok()).flatten()
    });
    bitrate("bitrate") == Some(settings.bitrate)
        && (!fd || (!controller_flag(details, "FD-NON-ISO") && bitrate("dbitrate") == settings.data_bitrate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a configured vcan interface with MTU of at least 72"]
    fn socketcan_vcan_mixed_classical_and_fd() {
        let name = std::env::var("CANVAZ_TEST_VCAN").expect("set CANVAZ_TEST_VCAN to the virtual interface name");
        let mut backend = SocketCanBackend;
        let index = backend.channel_index(&name).expect("virtual interface should be listed");
        let settings = ChannelSettings { mode: FrameFormat::Fd, bitrate: 0, data_bitrate: None, listen_only: false };
        let (mut tx, mut rx) = backend.open_channel(index, settings, None).unwrap();
        let peer = CanFdSocket::open(&name).unwrap();
        let id = ExtendedId::new(0x123).unwrap();
        for (len, metadata) in [(8, FrameMetadata::default()), (8, FrameMetadata::fd(false, false)), (64, FrameMetadata::fd(true, false))] {
            let mut frame = CanFrame {
                metadata, can_id: 0x123, is_extended: true, data: (0..len).collect(),
                timestamp_ms: None, error: None,
            };
            tx.send(&mut frame).unwrap();
            let received = peer.read_frame_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(received.raw_id(), frame.can_id);
            assert!(received.is_extended());
            assert_eq!(received.data(), frame.data);
            match received {
                CanAnyFrame::Normal(_) => assert_eq!(metadata.format, FrameFormat::Classical),
                CanAnyFrame::Fd(native) => {
                    assert_eq!(metadata.format, FrameFormat::Fd);
                    assert_eq!(native.is_brs(), metadata.brs);
                }
                _ => panic!("unexpected frame kind"),
            }

            if metadata.format == FrameFormat::Fd {
                let flags = FdFlags::ESI | if metadata.brs { FdFlags::BRS } else { FdFlags::empty() };
                peer.write_frame(&CanFdFrame::with_flags(id, &frame.data, flags).unwrap()).unwrap();
            } else {
                peer.write_frame(&CanDataFrame::new(id, &frame.data).unwrap()).unwrap();
            }
            let received = rx.receive(1_000).unwrap().unwrap();
            assert_eq!(received.can_id, frame.can_id);
            assert!(received.is_extended);
            assert_eq!(received.metadata.format, frame.metadata.format);
            assert_eq!(received.metadata.brs, frame.metadata.brs);
            assert_eq!(received.metadata.esi, metadata.format == FrameFormat::Fd);
            assert_eq!(received.data, frame.data);
        }
        drop((tx, rx));

        let mut channel = super::super::Can::new(backend, |_, _| {}, |_, _| {}, |_, _, _| {}, |_, _| {});
        channel.open(index, settings, None).unwrap();
        let outgoing = CanFrame {
            metadata: FrameMetadata::fd(true, false),
            can_id: 0x123,
            is_extended: true,
            data: vec![0xAB; 64],
            timestamp_ms: None,
            error: None,
        };
        let handle = channel.add_periodic(index, outgoing.clone(), 1_000, None).unwrap();
        let received = peer.read_frame_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(received.data(), outgoing.data);
        let replacement = super::super::FrameUpdate {
            data: vec![0x12; 12],
            can_id: None,
            is_extended: None,
            tx: Default::default(),
        };
        channel.update_periodic(index, handle, replacement, None, 40).unwrap();
        let received = peer.read_frame_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(received.data(), [0x12; 12]);
        channel.remove_periodic(index, handle).unwrap();
        channel.close(index).unwrap();

        channel.open(index, settings, None).unwrap();
        channel.send_once(index, outgoing.clone()).unwrap();
        let received = peer.read_frame_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(received.data(), outgoing.data);
        channel.close(index).unwrap();
    }
}
