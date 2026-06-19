use crate::can_interface::{CanBackend, CanReceiver, RawFrame};

pub struct SocketCanBackend;

impl CanBackend for SocketCanBackend {
    fn name(&self) -> &'static str { "socketcan" }

    fn probe(&self, channel: &str) -> bool {
        channel.starts_with("can") || channel.starts_with("vcan")
    }

    fn list_interfaces(&self) -> Vec<String> {
        let out = match std::process::Command::new("ip")
            .args(["-o", "link", "show", "type", "can"])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return Vec::new(),
        };
        // Each line: "N: <name>: <flags> ..."
        out.lines()
            .filter_map(|line| {
                line.split_whitespace().nth(1)?.strip_suffix(':').map(str::to_string)
            })
            .collect()
    }

    fn configure(&self, channel: &str, bitrate: Option<u32>, sudo_password: Option<&str>) -> Result<(), String> {
        if already_configured(channel, bitrate) {
            return Ok(());
        }

        let is_perm = |s: &str| -> bool {
            let lo = s.to_lowercase();
            lo.contains("operation not permitted")
                || lo.contains("permission denied")
                || lo.contains("not permitted")
        };

        let run_ip = |ip_args: &[&str]| -> Result<(), String> {
            if let Some(pass) = sudo_password {
                use std::io::Write;
                let mut child = std::process::Command::new("sudo")
                    .arg("-S").arg("ip").args(ip_args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("Failed to spawn sudo: {e}"))?;
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = writeln!(stdin, "{}", pass);
                }
                let out = child.wait_with_output().map_err(|e| e.to_string())?;
                if out.status.success() { return Ok(()); }
                let raw = String::from_utf8_lossy(&out.stderr);
                let msg = raw.lines()
                    .filter(|l| { let lo = l.to_lowercase(); !lo.contains("[sudo]") && !lo.starts_with("password") })
                    .collect::<Vec<_>>().join("\n");
                Err(if msg.trim().is_empty() { raw.trim().to_string() } else { msg.trim().to_string() })
            } else {
                let out = std::process::Command::new("ip")
                    .args(ip_args)
                    .output()
                    .map_err(|e| format!("Failed to run 'ip': {e}"))?;
                if out.status.success() { return Ok(()); }
                let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if is_perm(&msg) { Err(format!("needs-sudo: {msg}")) } else { Err(msg) }
            }
        };

        if channel.starts_with("vcan") {
            let _ = run_ip(&["link", "add", "dev", channel, "type", "vcan"]);
            run_ip(&["link", "set", channel, "up"])
        } else {
            let _ = run_ip(&["link", "set", channel, "down"]);
            if let Some(baud) = bitrate {
                let baud_s = baud.to_string();
                run_ip(&["link", "set", channel, "type", "can", "bitrate", &baud_s])?;
            }
            run_ip(&["link", "set", channel, "up"])
        }
    }

    fn open_receiver(&self, channel: &str) -> Result<Box<dyn CanReceiver>, String> {
        #[cfg(feature = "linux-can")]
        {
            use socketcan::{CanSocket, Socket};
            use std::time::Duration;
            let socket = CanSocket::open(channel)
                .map_err(|e| format!("Failed to open '{channel}': {e}"))?;
            socket
                .set_read_timeout(Duration::from_millis(100))
                .map_err(|e| format!("Failed to set read timeout: {e}"))?;
            return Ok(Box::new(SocketCanReceiver { socket }));
        }
        #[cfg(not(feature = "linux-can"))]
        Err("Linux CAN sockets not available on this platform".to_string())
    }

    fn send_frame(&self, channel: &str, can_id: u32, data: &[u8]) -> Result<(), String> {
        #[cfg(feature = "linux-can")]
        {
            use socketcan::{CanDataFrame, CanSocket, EmbeddedFrame, Socket};
            use socketcan::embedded_can::StandardId;

            let socket = CanSocket::open(channel)
                .map_err(|e| format!("Failed to open '{channel}' for send: {e}"))?;
            let frame = if can_id <= 0x7FF {
                let sid = StandardId::new(can_id as u16)
                    .ok_or_else(|| format!("Invalid CAN ID: {can_id:#x}"))?;
                CanDataFrame::new(sid, data)
                    .ok_or_else(|| "Failed to build CAN frame".to_string())?
            } else {
                use socketcan::embedded_can::ExtendedId;
                let eid = ExtendedId::new(can_id & 0x1FFF_FFFF)
                    .ok_or_else(|| format!("Invalid extended CAN ID: {can_id:#x}"))?;
                CanDataFrame::new(eid, data)
                    .ok_or_else(|| "Failed to build CAN frame".to_string())?
            };
            return socket.write_frame(&frame).map_err(|e| format!("Write failed: {e}"));
        }
        #[cfg(not(feature = "linux-can"))]
        Err("Linux CAN sockets not available on this platform".to_string())
    }
}

// ── already_configured ────────────────────────────────────────────────────────

fn already_configured(name: &str, bitrate: Option<u32>) -> bool {
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
        return true; // interface is up but bitrate unreadable — assume OK
    }
    true
}

// ── SocketCanReceiver ─────────────────────────────────────────────────────────

#[cfg(feature = "linux-can")]
struct SocketCanReceiver {
    socket: socketcan::CanSocket,
}

#[cfg(feature = "linux-can")]
impl CanReceiver for SocketCanReceiver {
    fn read_frame(&self) -> Result<Option<RawFrame>, String> {
        use socketcan::{CanFrame, EmbeddedFrame, Frame, Socket};
        match self.socket.read_frame() {
            Ok(CanFrame::Data(df)) => {
                let raw = df.raw_id();
                let is_extended = raw & 0x8000_0000 != 0;
                let can_id = raw & 0x1FFF_FFFF;
                Ok(Some(RawFrame { can_id, is_extended, data: df.data().to_vec() }))
            }
            Ok(_) => Ok(None), // remote / error frames
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}
