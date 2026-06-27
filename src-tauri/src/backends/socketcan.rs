use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use socketcan::embedded_can::{ExtendedId, StandardId};
use socketcan::CanFrame as SocketCanFrame;
use socketcan::{CanDataFrame, CanSocket, EmbeddedFrame, Frame, Socket};

use super::{CanFrame, Direction};
use crate::app_state::AppState;

// ── RX half — lives exclusively in the receive thread ─────────────────────────

pub(crate) struct SocketCanRxChannel {
    socket: CanSocket,
    configured_timeout_ms: u64,
}

impl SocketCanRxChannel {
    pub(super) fn receive(&mut self, timeout_ms: u64) -> Result<Option<CanFrame>, String> {
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
                timestamp_ms: super::now_ms(),
                direction: Direction::Rx,
                decoded: None,
            })),
            Ok(_) => Ok(None),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

// ── TX half — stays in Channel, used by the main thread ──────────────────────

pub(crate) struct SocketCanChannel {
    name: String,
    socket: Option<CanSocket>,
    state: Arc<AppState>,
}

impl SocketCanChannel {
    fn is_virtual(&self) -> bool {
        self.name.starts_with("vcan")
    }

    fn run_ip_auto(&self, args: &[&str]) -> Result<(), String> {
        match run_ip(args, None) {
            Ok(()) => Ok(()),
            Err(e) if e.starts_with("needs-sudo:") => {
                let pw = self.state.get_sudo_password()?;
                run_ip(args, Some(&pw))
            }
            Err(e) => Err(e),
        }
    }

    /// Opens the TX socket and returns a separate RX socket for the receive thread.
    /// Both sockets bind to the same interface; the kernel delivers a copy of every
    /// incoming frame to all bound sockets independently.
    pub(super) fn open(&mut self, bitrate: u32) -> Result<SocketCanRxChannel, String> {
        if !already_up(&self.name, bitrate) {
            if self.is_virtual() {
                let _ = self.run_ip_auto(&["link", "add", "dev", &self.name, "type", "vcan"]);
                self.run_ip_auto(&["link", "set", &self.name, "up"])?;
            } else {
                let _ = self.run_ip_auto(&["link", "set", &self.name, "down"]);
                let baud_s = bitrate.to_string();
                self.run_ip_auto(&["link", "set", &self.name, "type", "can", "bitrate", &baud_s])?;
                self.run_ip_auto(&["link", "set", &self.name, "up"])?;
            }
        }

        let tx_socket = CanSocket::open(&self.name)
            .map_err(|e| format!("Failed to open TX socket on '{}': {e}", self.name))?;

        let rx_socket = CanSocket::open(&self.name)
            .map_err(|e| format!("Failed to open RX socket on '{}': {e}", self.name))?;

        self.socket = Some(tx_socket);
        Ok(SocketCanRxChannel { socket: rx_socket, configured_timeout_ms: 0 })
    }

    pub(super) fn close(&mut self) -> Result<(), String> {
        self.socket = None;
        Ok(())
    }

    pub(super) fn send(&self, frame: CanFrame) -> Result<(), String> {
        let socket = self.socket.as_ref().ok_or("Channel is not open")?;
        let df: CanDataFrame = if frame.is_extended {
            if frame.can_id > 0x1FFF_FFFF {
                return Err(format!(
                    "Extended CAN ID must be ≤ 0x1FFF_FFFF, got {:#x}",
                    frame.can_id
                ));
            }
            let eid = ExtendedId::new(frame.can_id)
                .ok_or_else(|| format!("Invalid extended CAN ID: {:#x}", frame.can_id))?;
            CanDataFrame::new(eid, &frame.data).ok_or("Failed to build extended CAN frame")?
        } else {
            if frame.can_id > 0x7FF {
                return Err(format!("Standard CAN ID must be ≤ 0x7FF, got {:#x}", frame.can_id));
            }
            let sid = StandardId::new(frame.can_id as u16)
                .ok_or_else(|| format!("Invalid standard CAN ID: {:#x}", frame.can_id))?;
            CanDataFrame::new(sid, &frame.data).ok_or("Failed to build CAN frame")?
        };
        socket.write_frame(&df).map_err(|e| format!("Write failed: {e}"))
    }

}

// ── Backend ───────────────────────────────────────────────────────────────────

pub(crate) struct SocketCanBackend;

impl SocketCanBackend {
    pub(super) fn name(&self) -> &str {
        "socketcan"
    }

    pub(super) fn list_channels(&self) -> Result<Vec<String>, String> {
        let out = match std::process::Command::new("ip")
            .args(["-o", "link", "show", "type", "can"])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return Err("Failed to list CAN channels".to_string()),
        };
        Ok(out.lines()
            .filter_map(|line| line.split_whitespace().nth(1)?.strip_suffix(':').map(str::to_string))
            .collect())
    }

    pub(super) fn open_channel(
        &self,
        name: &str,
        state: Arc<AppState>,
    ) -> Result<SocketCanChannel, String> {
        Ok(SocketCanChannel {
            name: name.to_string(),
            socket: None,
            state,
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
        Err(if is_perm(&msg) {
            format!("needs-sudo: {msg}")
        } else {
            msg
        })
    }
}

fn already_up(name: &str, bitrate: u32) -> bool {
    let out = match std::process::Command::new("ip")
        .args(["-det", "link", "show", name])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return false,
    };
    if !out.contains("state UP") && !out.contains("state UNKNOWN") {
        return false;
    }
    if name.starts_with("vcan") {
        return true;
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
