use std::time::{Duration, SystemTime, UNIX_EPOCH};

use socketcan::embedded_can::{ExtendedId, StandardId};
use socketcan::CanFrame as SocketCanFrame;
use socketcan::{CanDataFrame, CanSocket, EmbeddedFrame, Frame, Socket};

use crate::backends2::{CanBackend, CanChannel, CanFrame};

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct SocketCanBackend;

impl CanBackend for SocketCanBackend {
    fn name(&self) -> &str {
        "socketcan"
    }

    fn list_channels(&self) -> Vec<String> {
        let out = match std::process::Command::new("ip")
            .args(["-o", "link", "show", "type", "can"])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return Vec::new(),
        };
        out.lines()
            .filter_map(|line| {
                line.split_whitespace()
                    .nth(1)?
                    .strip_suffix(':')
                    .map(str::to_string)
            })
            .collect()
    }

    fn open_channel(
        &mut self,
        name: &str,
        bitrate: Option<u32>,
    ) -> Result<Box<dyn CanChannel>, String> {
        if !name.starts_with("can") && !name.starts_with("vcan") {
            return Err(format!("SocketCanBackend does not handle channel '{name}'"));
        }
        Ok(Box::new(SocketCanChannel {
            name: name.to_string(),
            bitrate,
            socket: None,
        }))
    }
}

// ── Channel ───────────────────────────────────────────────────────────────────

pub struct SocketCanChannel {
    name: String,
    bitrate: Option<u32>,
    socket: Option<CanSocket>,
}

impl SocketCanChannel {
    fn is_virtual(&self) -> bool {
        self.name.starts_with("vcan")
    }
    fn is_open(&self) -> bool {
        self.socket.is_some()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn is_perm(msg: &str) -> bool {
    let lo = msg.to_lowercase();
    lo.contains("operation not permitted") || lo.contains("permission denied")
}

fn run_ip(args: &[&str]) -> Result<(), String> {
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

fn already_up(name: &str, bitrate: Option<u32>) -> bool {
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
    if let Some(requested) = bitrate {
        for line in out.lines() {
            if let Some(rest) = line.trim().strip_prefix("bitrate ") {
                if let Ok(current) = rest.split_whitespace().next().unwrap_or("").parse::<u32>() {
                    return current == requested;
                }
            }
        }
    }
    true
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── CanChannel impl ───────────────────────────────────────────────────────────

impl CanChannel for SocketCanChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn open(&mut self) -> Result<(), String> {
        // Configure the interface if it isn't already up with the right bitrate
        if !already_up(&self.name, self.bitrate) {
            if self.is_virtual() {
                // Create vcan interface if it doesn't exist yet, then bring it up
                let _ = run_ip(&["link", "add", "dev", &self.name, "type", "vcan"]);
                run_ip(&["link", "set", &self.name, "up"])?;
            } else {
                let _ = run_ip(&["link", "set", &self.name, "down"]); // ignore; may already be down
                if let Some(baud) = self.bitrate {
                    let baud_s = baud.to_string();
                    run_ip(&["link", "set", &self.name, "type", "can", "bitrate", &baud_s])?;
                }
                run_ip(&["link", "set", &self.name, "up"])?;
            }
        }

        let socket = CanSocket::open(&self.name)
            .map_err(|e| format!("Failed to open '{}': {e}", self.name))?;
        socket
            .set_read_timeout(Duration::from_millis(100))
            .map_err(|e| format!("Failed to set read timeout on '{}': {e}", self.name))?;
        self.socket = Some(socket);
        Ok(())
    }

    fn close(&mut self) -> Result<(), String> {
        if self.is_open() {
            self.socket = None; // drops the socket, closing the fd
        }
        Ok(())
    }

    fn send(&self, frame: CanFrame) -> Result<(), String> {
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
                return Err(format!(
                    "Standard CAN ID must be ≤ 0x7FF, got {:#x}",
                    frame.can_id
                ));
            }
            let sid = StandardId::new(frame.can_id as u16)
                .ok_or_else(|| format!("Invalid standard CAN ID: {:#x}", frame.can_id))?;
            CanDataFrame::new(sid, &frame.data).ok_or("Failed to build CAN frame")?
        };

        socket
            .write_frame(&df)
            .map_err(|e| format!("Write failed: {e}"))
    }

    fn receive(&self) -> Result<Option<CanFrame>, String> {
        let socket = self.socket.as_ref().ok_or("Channel is not open")?;

        match socket.read_frame() {
            Ok(SocketCanFrame::Data(df)) => Ok(Some(CanFrame {
                // raw_id() returns the masked ID (11-bit for standard, 29-bit for extended)
                can_id: df.raw_id(),
                is_extended: df.is_extended(),
                data: df.data().to_vec(),
                timestamp_ms: now_ms(),
            })),
            Ok(_) => Ok(None), // remote / error frames
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        if self.is_virtual() {
            return Err("vcan interfaces do not support bitrate configuration".to_string());
        }
        // Close the socket — the interface must go down before the bitrate can change
        let is_open = self.is_open();
        if is_open {
            self.close()?;
        }
        self.bitrate = Some(bitrate);

        let baud_s = bitrate.to_string();
        let _ = run_ip(&["link", "set", &self.name, "down"]); // ignore; may already be down
        run_ip(&["link", "set", &self.name, "type", "can", "bitrate", &baud_s])?;
        run_ip(&["link", "set", &self.name, "up"])?;

        // Re-open the socket now that the interface is back up
        if is_open {
            return self.open();
        }
        return Ok(());
    }

    fn get_bitrate(&self) -> Result<u32, String> {
        if self.is_virtual() {
            return Ok(0);
        }
        let out = match std::process::Command::new("ip")
            .args(["-det", "link", "show", &self.name])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return Err(format!("Failed to query interface '{}'", self.name)),
        };
        if !out.contains("state UP") && !out.contains("state UNKNOWN") {
            return Err(format!("Interface '{}' is down", self.name));
        }
        for line in out.lines() {
            if let Some(rest) = line.trim().strip_prefix("bitrate ") {
                if let Ok(baud) = rest.split_whitespace().next().unwrap_or("").parse::<u32>() {
                    return Ok(baud);
                }
            }
        }
        Err(format!(
            "Interface '{}' is up but bitrate is unreadable",
            self.name
        ))
    }
}
