use std::io::Write as _;
use std::sync::{Arc, Mutex};
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

#[derive(Default)]
pub struct SocketCanBackend {
    // Registered handles use these indices for both opening and routing frames.
    // Keep each slot tied to its name across hot-plug and enumeration changes.
    channel_names: Mutex<Vec<String>>,
}

impl SocketCanBackend {
    fn index_interfaces(&self, names: impl IntoIterator<Item = String>) -> Vec<(u8, String)> {
        let mut known = self.channel_names.lock().unwrap();
        names.into_iter().filter_map(|name| {
            let index = known.iter().position(|existing| existing == &name).unwrap_or(known.len());
            let index = u8::try_from(index).ok()?;
            if usize::from(index) == known.len() {
                known.push(name.clone());
            }
            Some((index, name))
        }).collect()
    }

    fn channel_name(&self, index: u8) -> Option<String> {
        self.channel_names.lock().unwrap().get(usize::from(index)).cloned()
    }
}

impl CanBackend for SocketCanBackend {
    fn list_channels(&self) -> Vec<(u8, String)> {
        let mut channels = can_links("can");
        for name in can_links("vcan") {
            if !channels.contains(&name) {
                channels.push(name);
            }
        }
        self.index_interfaces(channels)
    }

    fn reinitialize(&self) {
        self.channel_names.lock().unwrap().clear();
    }

    fn capabilities(&self, index: u8) -> ChannelCapabilities {
        let Some(name) = self.channel_name(index) else {
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
        let name = self.channel_name(index).ok_or_else(|| {
            CanOpenError::ChannelIndexOutOfRange(format!("Unknown SocketCAN channel index {index}"))
        })?;

        let details = link_details(&name).ok_or_else(|| format!("Cannot inspect SocketCAN interface '{name}'"))?;
        let virtual_link = is_vcan_details(&details);
        let fd = settings.mode == FrameFormat::Fd;
        if virtual_link && fd && !link_mtu(&details).is_some_and(|mtu| mtu >= CAN_FD_MTU) {
            return Err(format!("Virtual interface '{name}' requires an MTU of at least {CAN_FD_MTU} for CAN FD").into());
        }

        // Share one raw socket to avoid receiving our own TX entries as RX.
        let open_socket = || {
            let open_error = |e| CanOpenError::Other(format!("Failed to open SocketCAN on '{name}': {e}"));
            if fd {
                let socket = CanFdSocket::open(&name).map_err(open_error)?;
                enable_error_frames(&socket, &name);
                Ok(SocketCanSocket::Fd(socket))
            } else {
                let socket = CanSocket::open(&name).map_err(open_error)?;
                enable_error_frames(&socket, &name);
                Ok(SocketCanSocket::Classical(socket))
            }
        };
        let run_ip = |args: &[&str]| run_ip_auto(args, admin_password);
        let socket = if !virtual_link && !configured_timing_matches(&details, settings) {
            open_reconfigured_socket(&name, &details, settings, run_ip, || link_details(&name), open_socket)?
        } else if link_is_up(&details) {
            open_socket()?
        } else {
            open_inactive_socket(&name, run_ip, open_socket)?
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
    details.lines()
        .filter_map(|line| line.trim().strip_prefix("can <"))
        .filter_map(|line| line.split_once('>'))
        .any(|(flags, _)| flags.split(',').any(|flag| flag.trim() == expected))
}

fn link_is_up(details: &str) -> bool {
    details.lines().next().and_then(|line| line.split_once('<'))
        .and_then(|(_, rest)| rest.split_once('>')).is_some_and(|(flags, _)| flags.split(',').any(|flag| flag == "UP"))
}

fn timing_fields<'a>(details: &'a str, key: &str) -> Option<Vec<&'a str>> {
    details.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let field = fields.next()?;
        if field != key {
            return None;
        }
        let value = fields.next()?;
        value.parse::<u32>().ok()?;
        Some([field, value].into_iter().chain(fields).collect())
    })
}

fn configured_timing_matches(details: &str, settings: ChannelSettings) -> bool {
    configured_mode_matches(details, settings) && bitrates_match(details, settings)
}

fn configured_bitrate(details: &str, key: &str) -> Option<u32> {
    timing_fields(details, key).and_then(|fields| fields[1].parse().ok())
}

fn fd_mode_matches(details: &str, settings: ChannelSettings) -> bool {
    let fd = settings.mode == FrameFormat::Fd;
    controller_flag(details, "FD") == fd && (!fd || !controller_flag(details, "FD-NON-ISO"))
}

fn configured_mode_matches(details: &str, settings: ChannelSettings) -> bool {
    controller_flag(details, "LISTEN-ONLY") == settings.listen_only && fd_mode_matches(details, settings)
}

fn bitrates_match(details: &str, settings: ChannelSettings) -> bool {
    configured_bitrate(details, "bitrate") == Some(settings.bitrate)
        && (settings.mode != FrameFormat::Fd || configured_bitrate(details, "dbitrate") == settings.data_bitrate)
}

fn previous_timing<'a>(details: &'a str, rate: &str, quantum: &str) -> Option<Vec<&'a str>> {
    if let Some(fields) = timing_fields(details, quantum).filter(|fields| fields.len() >= 10 && fields[1] != "0") {
        // ip prints the five settable timing parameters first, then the computed prescaler.
        return Some(fields[..10].to_vec());
    }
    timing_fields(details, rate).filter(|fields| fields[1] != "0")
}

fn previous_tdc(details: &str) -> Option<Vec<&str>> {
    // ip reports several TDC settings on one line and lists supported ranges separately.
    let fields: Vec<_> = details.split_whitespace().collect();
    let value = |key| fields.windows(2).find_map(|pair| {
        (pair[0] == key && pair[1].parse::<u32>().is_ok()).then_some(pair[1])
    });
    let mut args = if controller_flag(details, "TDC-MANUAL") {
        vec!["tdc-mode", "manual", "tdcv", value("tdcv")?]
    } else if controller_flag(details, "TDC-AUTO") {
        // In auto mode an adapter can report a measured delay. Only manual mode accepts it as input.
        vec!["tdc-mode", "auto"]
    } else {
        // Without an explicit off, the kernel can enable TDC when applying the old timing.
        return Some(if fields.windows(2).any(|pair| pair[0] == "tdco" && pair[1].contains("..")) {
            vec!["tdc-mode", "off"]
        } else {
            Vec::new()
        });
    };
    args.extend(["tdco", value("tdco")?]);
    if let Some(filter) = value("tdcf") {
        args.extend(["tdcf", filter]);
    }
    Some(args)
}

fn open_reconfigured_socket<S>(
    name: &str,
    details: &str,
    settings: ChannelSettings,
    mut run_ip: impl FnMut(&[&str]) -> Result<(), CanOpenError>,
    inspect_link: impl FnOnce() -> Option<String>,
    open_socket: impl FnOnce() -> Result<S, CanOpenError>,
) -> Result<S, CanOpenError> {
    let base = ["link", "set", name, "type", "can"];
    let down = ["link", "set", name, "down"];
    let up = ["link", "set", name, "up"];
    let fd = settings.mode == FrameFormat::Fd;
    let was_fd = controller_flag(details, "FD");
    let was_non_iso = controller_flag(details, "FD-NON-ISO");
    let on_off = |enabled| if enabled { "on" } else { "off" };
    let nominal_changed = configured_bitrate(details, "bitrate") != Some(settings.bitrate);
    let data_changed = fd && configured_bitrate(details, "dbitrate") != settings.data_bitrate;
    let timing_changed = !fd_mode_matches(details, settings) || nominal_changed || data_changed;
    let old_nominal = previous_timing(details, "bitrate", "tq");
    let old_data = was_fd.then(|| previous_timing(details, "dbitrate", "dtq")).flatten();
    let timing_error = || CanOpenError::Other(format!("Cannot read the previous CAN timing on '{name}'"));
    let tdc_error = || format!("Cannot read the previous CAN FD transmitter delay compensation on '{name}'");
    let nominal = settings.bitrate.to_string();
    let mut args = base.to_vec();
    if nominal_changed {
        args.extend(["bitrate", &nominal]);
    } else if fd && timing_changed {
        // Enabling FD requires both phases even when one bitrate is unchanged.
        args.extend(old_nominal.as_deref().ok_or_else(timing_error)?);
    }
    args.extend(["listen-only", on_off(settings.listen_only)]);
    let data;
    if timing_changed {
        if fd {
            if was_fd && !data_changed {
                args.extend(old_data.as_deref().ok_or_else(timing_error)?);
                args.extend(previous_tdc(details).ok_or_else(tdc_error)?);
            } else {
                data = settings.data_bitrate()?.to_string();
                args.extend(["dbitrate", &data]);
            }
            args.extend(["fd", "on"]);
            if was_non_iso {
                args.extend(["fd-non-iso", "off"]);
            }
        } else if was_fd {
            args.extend(["fd", "off"]);
        }
    }

    let mut restore = base.to_vec();
    let was_up = link_is_up(details);
    let restore_controller = was_up || !was_fd || (old_nominal.is_some() && old_data.is_some());
    if restore_controller {
        // The kernel can apply part of a failed netlink request. Restore the
        // previous mode even on a new interface, where no prior bitrate is set.
        if timing_changed && (was_fd || nominal_changed) {
            if let Some(timing) = old_nominal {
                restore.extend(timing);
            } else if was_up {
                return Err(timing_error());
            }
        }
        restore.extend(["listen-only", on_off(controller_flag(details, "LISTEN-ONLY"))]);
        if timing_changed {
            if was_fd {
                restore.extend(old_data.ok_or_else(timing_error)?);
                restore.extend(["fd", "on"]);
                if was_non_iso {
                    restore.extend(["fd-non-iso", "on"]);
                }
                restore.extend(previous_tdc(details).ok_or_else(tdc_error)?);
            } else if fd {
                restore.extend(["fd", "off"]);
                if was_non_iso {
                    restore.extend(["fd-non-iso", "on"]);
                }
            }
        }
    }
    if was_up {
        run_ip(&down)?;
    }

    let configured = run_ip(&args);
    if !was_up && matches!(configured, Err(CanOpenError::PasswordRequired)) {
        return Err(CanOpenError::PasswordRequired);
    }

    let mut raised = false;
    let result = configured.map_err(|error| match error {
        CanOpenError::Other(message) if fd && timing_changed => CanOpenError::Other(format!(
            "Cannot configure ISO CAN FD on '{name}'. Check adapter and driver support and both bitrates: {message}"
        )),
        error => error,
    }).and_then(|()| {
        let applied = inspect_link().ok_or_else(|| format!("Cannot verify the CAN configuration on '{name}'"))?;
        if !configured_mode_matches(&applied, settings) {
            return Err(format!("SocketCAN '{name}' did not apply the requested CAN mode or listen-only setting").into());
        }
        if !bitrates_match(&applied, settings) {
            let rate = |key| configured_bitrate(&applied, key).map_or_else(|| "unknown".into(), |value| value.to_string());
            let reported = if fd {
                format!("nominal {} bit/s, data {} bit/s", rate("bitrate"), rate("dbitrate"))
            } else {
                format!("{} bit/s", rate("bitrate"))
            };
            return Err(format!("SocketCAN '{name}' did not apply the requested CAN timing (reported {reported}). Use bitrates supported by the adapter.").into());
        }
        run_ip(&up)?;
        raised = true;
        open_socket()
    });
    let Err(error) = result else { return result };
    let restore_result = (|| {
        if raised {
            run_ip(&down)?;
        }
        if restore_controller {
            run_ip(&restore)?;
        }
        if was_up {
            run_ip(&up)?;
        }
        Ok::<(), CanOpenError>(())
    })();
    if let Err(restore_error) = restore_result {
        return Err(format!("{error}. Could not restore the previous SocketCAN configuration on '{name}': {restore_error}").into());
    }
    Err(error)
}

fn open_inactive_socket<S>(
    name: &str,
    mut run_ip: impl FnMut(&[&str]) -> Result<(), CanOpenError>,
    open_socket: impl FnOnce() -> Result<S, CanOpenError>,
) -> Result<S, CanOpenError> {
    run_ip(&["link", "set", name, "up"])?;
    open_socket().map_err(|error| {
        match run_ip(&["link", "set", name, "down"]) {
            Ok(()) => error,
            Err(restore_error) => format!("{error}. Could not bring '{name}' back down: {restore_error}").into(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socketcan_indices_keep_the_same_interface_after_discovery_changes() {
        let backend = SocketCanBackend::default();
        let discover = |names: &[&str]| backend.index_interfaces(names.iter().map(|name| (*name).into()));

        assert_eq!(discover(&["vcan0"]), [(0, "vcan0".into())]);
        assert_eq!(discover(&["can0", "vcan0"]), [(1, "can0".into()), (0, "vcan0".into())]);
        assert_eq!(discover(&["can0"]), [(1, "can0".into())]);
        assert_eq!(backend.channel_name(0).as_deref(), Some("vcan0"));
        assert_eq!(backend.channel_name(1).as_deref(), Some("can0"));
        assert_eq!(discover(&["can0", "vcan0"]), [(1, "can0".into()), (0, "vcan0".into())]);
    }

    #[test]
    #[ignore = "requires a configured vcan interface with MTU of at least 72"]
    fn socketcan_vcan_mixed_classical_and_fd() {
        let name = std::env::var("CANVAZ_TEST_VCAN").expect("set CANVAZ_TEST_VCAN to the virtual interface name");
        let mut backend = SocketCanBackend::default();
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

    #[test]
    fn fd_timing_match_reads_each_controller_flag_with_or_without_spaces() {
        let settings = ChannelSettings {
            mode: FrameFormat::Fd,
            bitrate: 500_000,
            data_bitrate: Some(2_000_000),
            listen_only: true,
        };
        for separator in [",", ", "] {
            let details = format!(
                "can <LISTEN-ONLY{separator}BERR-REPORTING{separator}FD> state ERROR-ACTIVE\n\
                 bitrate 500000 sample-point 0.875\n\
                 dbitrate 2000000 dsample-point 0.800"
            );
            assert!(configured_timing_matches(&details, settings));

            let non_iso = details.replace("FD>", &format!("FD{separator}FD-NON-ISO>"));
            assert!(controller_flag(&non_iso, "FD-NON-ISO"));
            assert!(!configured_timing_matches(&non_iso, settings));
        }
    }

    #[test]
    fn failed_fd_reconfiguration_restores_the_active_link() {
        let details = "2: can0: <NOARP,UP,LOWER_UP> mtu 72\n\
            can <LISTEN-ONLY,FD,FD-NON-ISO> state ERROR-ACTIVE\n\
            bitrate 500000 sample-point 0.875\n\
            tq 125 prop-seg 6 phase-seg1 7 phase-seg2 2 sjw 1 brp 10\n\
            dbitrate 2000000 dsample-point 0.800\n\
            dtq 50 dprop-seg 3 dphase-seg1 4 dphase-seg2 2 dsjw 1 dbrp 4";
        let settings = ChannelSettings {
            mode: FrameFormat::Fd,
            bitrate: 250_000,
            data_bitrate: Some(3_000_000),
            listen_only: false,
        };
        for failing_step in [1, 2, 3] {
            let mut commands = Vec::new();
            let result = open_reconfigured_socket("can0", details, settings, |args| {
                commands.push(args.join(" "));
                if failing_step != 3 && commands.len() - 1 == failing_step {
                    Err("Invalid timing".into())
                } else {
                    Ok(())
                }
            }, || Some("can <FD> state STOPPED\nbitrate 250000\ndbitrate 3000000".into()), || {
                assert_eq!(failing_step, 3);
                Err::<(), _>("Socket open failed".into())
            });
            let (restore_at, error) = if failing_step == 3 {
                assert_eq!(commands[2], "link set can0 up");
                assert_eq!(commands[3], "link set can0 down");
                (4, "Socket open failed")
            } else {
                (failing_step + 1, "Invalid timing")
            };
            assert!(result.unwrap_err().to_string().contains(error));
            assert_eq!(commands[0], "link set can0 down");
            assert_eq!(commands[1], "link set can0 type can bitrate 250000 listen-only off dbitrate 3000000 fd on fd-non-iso off");
            assert_eq!(commands[restore_at], "link set can0 type can tq 125 prop-seg 6 phase-seg1 7 phase-seg2 2 sjw 1 listen-only on dtq 50 dprop-seg 3 dphase-seg1 4 dphase-seg2 2 dsjw 1 fd on fd-non-iso on");
            assert_eq!(commands[restore_at + 1], "link set can0 up");
        }
    }

    #[test]
    fn failed_fd_reconfiguration_preserves_transmitter_delay_compensation() {
        let settings = ChannelSettings {
            mode: FrameFormat::Fd,
            bitrate: 500_000,
            data_bitrate: Some(3_000_000),
            listen_only: false,
        };
        for (mode, values, expected) in [
            (",TDC-MANUAL", "tdcv 3 tdco 9 tdcf 12", "tdc-mode manual tdcv 3 tdco 9 tdcf 12"),
            (",TDC-AUTO", "tdcv 4 tdco 9 tdcf 12", "tdc-mode auto tdco 9 tdcf 12"),
            ("", "", "tdc-mode off"),
        ] {
            let details = format!(
                "2: can0: <NOARP,UP,LOWER_UP> mtu 72\n\
                 can <FD{mode}> state ERROR-ACTIVE\n\
                 bitrate 500000 sample-point 0.875\n\
                 dbitrate 4000000 dsample-point 0.800\n\
                 {values}\n\
                 tdcv 0..63 tdco 0..127 tdcf 0..127"
            );
            let mut commands = Vec::new();
            let error = open_reconfigured_socket("can0", &details, settings, |args| {
                commands.push(args.join(" "));
                if commands.len() == 2 { Err("Invalid timing".to_string().into()) } else { Ok(()) }
            }, || panic!("timing rejected by ip"), || Ok(())).unwrap_err();
            assert!(error.to_string().contains("Invalid timing"));
            assert_eq!(commands[2], format!("link set can0 type can bitrate 500000 sample-point 0.875 listen-only off dbitrate 4000000 dsample-point 0.800 fd on {expected}"));
            assert_eq!(commands[3], "link set can0 up");
        }

        let incomplete = "2: can0: <NOARP,UP,LOWER_UP> mtu 72\n\
            can <FD,TDC-MANUAL> state ERROR-ACTIVE\n\
            bitrate 500000 sample-point 0.875\n\
            dbitrate 4000000 dsample-point 0.800\n\
            tdcv 3\n\
            tdcv 0..63 tdco 0..127";
        let error = open_reconfigured_socket("can0", incomplete, settings, |_| {
            panic!("the link must stay up when its active TDC settings cannot be read");
        }, || panic!("configuration must stay intact"), || Ok(())).unwrap_err();
        assert!(error.to_string().contains("transmitter delay compensation"));
    }

    #[test]
    fn changing_listen_only_leaves_existing_fd_timing_intact() {
        let details = "2: can0: <NOARP,UP,LOWER_UP> mtu 72\n\
            can <FD,TDC-MANUAL> state ERROR-ACTIVE\n\
            bitrate 500000 sample-point 0.875\n\
            dbitrate 2000000 dsample-point 0.700\n\
            tdcv 3 tdco 9 tdcf 12";
        let settings = ChannelSettings {
            mode: FrameFormat::Fd,
            bitrate: 500_000,
            data_bitrate: Some(2_000_000),
            listen_only: true,
        };
        let mut commands = Vec::new();
        open_reconfigured_socket("can0", details, settings, |args| {
            commands.push(args.join(" "));
            Ok(())
        }, || Some(details.replace("<FD,", "<LISTEN-ONLY,FD,")), || Ok(())).unwrap();
        assert_eq!(commands, [
            "link set can0 down",
            "link set can0 type can listen-only on",
            "link set can0 up",
        ]);
    }

    #[test]
    fn fd_rate_changes_reuse_the_unchanged_phases_timing() {
        let details = "2: can0: <NOARP,UP,LOWER_UP> mtu 72\n\
            can <FD,TDC-MANUAL> state ERROR-ACTIVE\n\
            bitrate 500000 sample-point 0.875\n\
            tq 125 prop-seg 6 phase-seg1 7 phase-seg2 2 sjw 1 brp 10\n\
            dbitrate 2000000 dsample-point 0.800\n\
            dtq 50 dprop-seg 3 dphase-seg1 4 dphase-seg2 2 dsjw 1 dbrp 4\n\
            tdcv 3 tdco 9 tdcf 12";
        for (nominal, data, expected) in [
            (500_000, 3_000_000, "tq 125 prop-seg 6 phase-seg1 7 phase-seg2 2 sjw 1 listen-only off dbitrate 3000000 fd on"),
            (250_000, 2_000_000, "bitrate 250000 listen-only off dtq 50 dprop-seg 3 dphase-seg1 4 dphase-seg2 2 dsjw 1 tdc-mode manual tdcv 3 tdco 9 tdcf 12 fd on"),
        ] {
            let settings = ChannelSettings {
                mode: FrameFormat::Fd,
                bitrate: nominal,
                data_bitrate: Some(data),
                listen_only: false,
            };
            let mut commands = Vec::new();
            open_reconfigured_socket("can0", details, settings, |args| {
                commands.push(args.join(" "));
                Ok(())
            }, || Some(format!("can <FD> state STOPPED\nbitrate {nominal}\ndbitrate {data}")), || Ok(())).unwrap();
            assert_eq!(commands, ["link set can0 down".into(), format!("link set can0 type can {expected}"), "link set can0 up".into()]);
        }
    }

    #[test]
    fn failed_first_fd_configuration_restores_controller_mode() {
        let settings = ChannelSettings {
            mode: FrameFormat::Fd,
            bitrate: 500_000,
            data_bitrate: Some(3_000_000),
            listen_only: true,
        };
        for (flags, configure, restore) in [
            ("", "", ""),
            (" <FD-NON-ISO>", " fd-non-iso off", " fd-non-iso on"),
        ] {
            let details = format!("2: can0: <NOARP> mtu 16\ncan{flags} state STOPPED\nbitrate 0 sample-point 0.000");
            let mut commands = Vec::new();
            let result = open_reconfigured_socket("can0", &details, settings, |args| {
                commands.push(args.join(" "));
                if commands.len() == 1 { Err("Invalid data timing".into()) } else { Ok(()) }
            }, || panic!("configuration rejected"), || Ok(()));
            assert!(result.unwrap_err().to_string().contains("Invalid data timing"));
            assert_eq!(commands, [
                format!("link set can0 type can bitrate 500000 listen-only on dbitrate 3000000 fd on{configure}"),
                format!("link set can0 type can listen-only off fd off{restore}"),
            ]);
        }
    }

    #[test]
    fn down_configured_link_preserves_the_password_prompt() {
        let details = "2: can0: <NOARP> mtu 72\n\
            can <FD> state STOPPED\n\
            bitrate 500000 sample-point 0.875\n\
            dbitrate 2000000 dsample-point 0.800";
        let settings = ChannelSettings {
            mode: FrameFormat::Fd,
            bitrate: 500_000,
            data_bitrate: Some(3_000_000),
            listen_only: false,
        };
        let mut commands = 0;
        let error = open_reconfigured_socket("can0", details, settings, |_| {
            commands += 1;
            Err(CanOpenError::PasswordRequired)
        }, || panic!("configuration requires privileges"), || Ok(())).unwrap_err();
        assert!(matches!(error, CanOpenError::PasswordRequired));
        assert_eq!(commands, 1);
    }

    #[test]
    fn unchanged_socketcan_mode_is_rejected_before_bringing_the_link_up() {
        let details = "2: can0: <NOARP,UP,LOWER_UP> mtu 72\n\
            can <FD> state ERROR-ACTIVE\n\
            bitrate 500000 sample-point 0.875\n\
            dbitrate 2000000 dsample-point 0.800";
        let settings = ChannelSettings {
            mode: FrameFormat::Classical,
            bitrate: 500_000,
            data_bitrate: None,
            listen_only: true,
        };
        for controller in ["can <LISTEN-ONLY,FD>", "can"] {
            let mut commands = Vec::new();
            let error = open_reconfigured_socket("can0", details, settings, |args| {
                commands.push(args.join(" "));
                Ok(())
            }, || Some(details.replace("can <FD>", controller)), || -> Result<(), CanOpenError> {
                panic!("a socket must not open in the wrong mode")
            }).unwrap_err();
            assert!(error.to_string().contains("did not apply the requested CAN mode or listen-only setting"));
            assert_eq!(commands, [
                "link set can0 down",
                "link set can0 type can listen-only on fd off",
                "link set can0 type can bitrate 500000 sample-point 0.875 listen-only off dbitrate 2000000 dsample-point 0.800 fd on",
                "link set can0 up",
            ]);
        }
    }

    #[test]
    fn rounded_fd_bitrate_is_rejected_before_bringing_the_link_up() {
        let details = "2: can0: <NOARP> mtu 72\n\
            can <FD> state STOPPED\n\
            bitrate 500000 sample-point 0.875\n\
            dbitrate 2000000 dsample-point 0.800";
        let settings = ChannelSettings {
            mode: FrameFormat::Fd,
            bitrate: 500_000,
            data_bitrate: Some(3_000_000),
            listen_only: false,
        };
        let mut commands = Vec::new();
        let error = open_reconfigured_socket("can0", details, settings, |args| {
            commands.push(args.join(" "));
            Ok(())
        }, || Some(details.replace("2000000", "2962962")), || -> Result<(), CanOpenError> {
            panic!("a socket must not open with the wrong bitrate")
        }).unwrap_err();
        assert!(error.to_string().contains("2962962 bit/s"));
        assert_eq!(commands, [
            "link set can0 type can bitrate 500000 sample-point 0.875 listen-only off dbitrate 3000000 fd on",
            "link set can0 type can bitrate 500000 sample-point 0.875 listen-only off dbitrate 2000000 dsample-point 0.800 fd on",
        ]);
    }

    #[test]
    fn switching_to_classical_rejects_a_rounded_bitrate_and_restores_fd() {
        let details = "2: can0: <NOARP,UP,LOWER_UP> mtu 72\n\
            can <FD> state ERROR-ACTIVE\n\
            bitrate 500000 sample-point 0.875\n\
            dbitrate 2000000 dsample-point 0.800";
        let settings = ChannelSettings {
            mode: FrameFormat::Classical,
            bitrate: 490_000,
            data_bitrate: None,
            listen_only: false,
        };
        let mut commands = Vec::new();
        let error = open_reconfigured_socket("can0", details, settings, |args| {
            commands.push(args.join(" "));
            Ok(())
        }, || Some("can state STOPPED\nbitrate 500000 sample-point 0.875".into()), || -> Result<(), CanOpenError> {
            panic!("a socket must not open with the wrong bitrate")
        }).unwrap_err();
        assert!(error.to_string().contains("reported 500000 bit/s"));
        assert_eq!(commands, [
            "link set can0 down",
            "link set can0 type can bitrate 490000 listen-only off fd off",
            "link set can0 type can bitrate 500000 sample-point 0.875 listen-only off dbitrate 2000000 dsample-point 0.800 fd on",
            "link set can0 up",
        ]);
    }

    #[test]
    fn socket_open_failure_returns_an_inactive_link_to_down() {
        let mut commands = Vec::new();
        let error = open_inactive_socket("vcan0", |args| {
            commands.push(args.join(" "));
            Ok(())
        }, || Err::<(), _>("Socket open failed".into())).unwrap_err();
        assert!(error.to_string().contains("Socket open failed"));
        assert_eq!(commands, ["link set vcan0 up", "link set vcan0 down"]);
    }
}
