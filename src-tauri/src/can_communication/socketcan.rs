use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use socketcan::embedded_can::{ExtendedId, StandardId};
use socketcan::CanFrame as SocketCanFrame;
use socketcan::{CanDataFrame, CanError, CanSocket, EmbeddedFrame, Frame, Socket, SocketOptions};

use super::{BusState, BusStatus, CanBackend, CanFrame, CanOpenError, RxHandle, TxHandle};

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
    socket: Arc<CanSocket>,
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
            self.socket
                .set_read_timeout(Duration::from_millis(timeout_ms))
                .map_err(|e| format!("set_read_timeout failed: {e}"))?;
            self.configured_timeout_ms = timeout_ms;
        }
        match self.socket.read_frame() {
            Ok(SocketCanFrame::Data(df)) => Ok(Some(CanFrame {
                can_id: df.raw_id(),
                is_extended: df.is_extended(),
                data: df.data().to_vec(),
                timestamp_ms: None,
                error: None,
            })),
            Ok(SocketCanFrame::Error(ef)) => {
                let bits = ef.error_bits();
                let data = ef.data().to_vec();
                self.update_status(bits, &data);
                let desc = format!("{}", CanError::from(ef));
                Ok(Some(CanFrame {
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
    socket: Arc<CanSocket>,
}

impl TxHandle for SocketCanTxHandle {
    fn send(&mut self, frame: &mut CanFrame) -> Result<(), String> {
        let df: CanDataFrame = if frame.is_extended {
            if frame.can_id > 0x1FFF_FFFF {
                return Err(format!("Extended CAN ID must be ≤ 0x1FFF_FFFF, got {:#x}", frame.can_id));
            }
            let eid = ExtendedId::new(frame.can_id).ok_or_else(|| format!("Invalid extended CAN ID: {:#x}", frame.can_id))?;
            CanDataFrame::new(eid, &frame.data).ok_or("Failed to build extended CAN frame")?
        } else {
            if frame.can_id > 0x7FF {
                return Err(format!("Standard CAN ID must be ≤ 0x7FF, got {:#x}", frame.can_id));
            }
            let sid = StandardId::new(frame.can_id as u16).ok_or_else(|| format!("Invalid standard CAN ID: {:#x}", frame.can_id))?;
            CanDataFrame::new(sid, &frame.data).ok_or("Failed to build CAN frame")?
        };
        self.socket.write_frame(&df).map_err(|e| format!("Write failed: {e}"))?;
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
        let out = match std::process::Command::new("ip")
            .args(["-o", "link", "show", "type", "can"])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return Vec::new(),
        };
        out.lines()
            .filter_map(|line| line.split_whitespace().nth(1)?.strip_suffix(':').map(str::to_string))
            .collect()
    }

    fn open_channel(
        &mut self,
        index: u8,
        bitrate: u32,
        listen_only: bool,
        admin_password: Option<&str>,
    ) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), CanOpenError> {
        let channels = self.list_channels();
        let name = channels
            .get(index as usize)
            .ok_or_else(|| {
                CanOpenError::ChannelIndexOutOfRange(format!("SocketCAN channel index {index} out of range ({} found)", channels.len()))
            })?
            .clone();

        if !already_up(&name, bitrate, listen_only) {
            if name.starts_with("vcan") {
                let _ = run_ip_auto(&["link", "add", "dev", &name, "type", "vcan"], admin_password);
                run_ip_auto(&["link", "set", &name, "up"], admin_password)?;
            } else {
                let _ = run_ip_auto(&["link", "set", &name, "down"], admin_password);
                let baud_s = bitrate.to_string();
                let listen_only_s = if listen_only { "on" } else { "off" };
                run_ip_auto(
                    &["link", "set", &name, "type", "can", "bitrate", &baud_s, "listen-only", listen_only_s],
                    admin_password,
                )?;
                run_ip_auto(&["link", "set", &name, "up"], admin_password)?;
            }
        }

        // One socket shared by the TX and RX threads (socket reads/writes are
        // thread-safe). With a single socket the kernel defaults do exactly what
        // we want: our own transmissions are NOT read back (CAN_RAW_RECV_OWN_MSGS
        // is off) while loopback to other local sockets/processes stays enabled.
        // Two separate sockets would receive each other's transmissions via
        // loopback, duplicating every sent frame as an RX entry.
        let socket = Arc::new(
            CanSocket::open(&name).map_err(|e| CanOpenError::Other(format!("Failed to open socket on '{name}': {e}")))?,
        );

        // Ask the kernel to deliver error frames (bus errors, controller state
        // changes) on this socket — off by default on RAW sockets.
        if let Err(e) = socket.set_error_filter_accept_all() {
            log::warn!("set_error_filter failed on '{name}': {e} — error frames will not be shown");
        }

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

fn already_up(name: &str, bitrate: u32, listen_only: bool) -> bool {
    let out = match std::process::Command::new("ip").args(["-det", "link", "show", name]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return false,
    };
    if !out.contains("state UP") && !out.contains("state UNKNOWN") {
        return false;
    }
    if name.starts_with("vcan") {
        return true;
    }
    // The controller-mode flags (e.g. "can <LISTEN-ONLY> state ...") only
    // appear once `ip -details` is asked for; a mismatch here forces the
    // reconfigure branch below to reapply the mode via `ip link set`.
    if out.contains("LISTEN-ONLY") != listen_only {
        return false;
    }
    for line in out.lines() {
        if let Some(rest) = line.trim().strip_prefix("bitrate ") {
            if let Ok(current) = rest.split_whitespace().next().unwrap_or("").parse::<u32>() {
                return current == bitrate;
            }
        }
    }
    true
}
