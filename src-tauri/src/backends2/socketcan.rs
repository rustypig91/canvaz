
use crate::backends2::{CanBackend, CanChannel, CanFrame};
use socketcan::embedded_can::{ExtendedId, StandardId};
use socketcan::{CanDataFrame, CanSocket, EmbeddedFrame, Socket};
use socketcan::CanFrame as SocketCanFrame;


pub struct SocketCanBackend;

#[derive(Default)]
pub struct SocketCanChannel {
    name: String,
    bitrate: Option<u32>,
    socket: Option<CanSocket>,
}

impl CanBackend for SocketCanBackend {
    fn name(&self) -> &'static str {
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
        // Each line: "N: <name>: <flags> ..."
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
        if !self.list_channels().contains(&name.to_string()) {
            return Err(format!("Channel '{name}' not found"));
        }
        let can_channel = Box::new(SocketCanChannel {
            name: name.to_string(),
            bitrate: bitrate,
            socket: None,
        });
        Ok(can_channel)
    }
}

impl SocketCanChannel {
    fn is_virtual(&self) -> bool {
        self.name.starts_with("vcan")
    }
}

impl CanChannel for SocketCanChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn send(&self, frame: CanFrame) -> Result<(), String> {
        let socket = self.socket.as_ref().ok_or_else(|| "Channel is not open".to_string())?;


        let frame = if !frame.is_extended {
            if frame.can_id > 0x7FF {
                return Err(format!("Standard CAN ID must be <= 0x7FF, got {:#x}", frame.can_id));
            }
            let sid = StandardId::new(frame.can_id as u16)
                .ok_or_else(|| format!("Invalid CAN ID: {0:#x}", frame.can_id))?;
            CanDataFrame::new(sid, frame.data.as_slice())
                .ok_or_else(|| "Failed to build CAN frame".to_string())?
        } else {
            if frame.can_id > 0x1FFF_FFFF {
                return Err(format!("Extended CAN ID must be <= 0x1FFF_FFFF, got {:#x}", frame.can_id));
            }
            let eid = ExtendedId::new(frame.can_id & 0x1FFF_FFFF)
                .ok_or_else(|| format!("Invalid extended CAN ID: {0:#x}", frame.can_id))?;
            CanDataFrame::new(eid, frame.data.as_slice())
                .ok_or_else(|| "Failed to build CAN frame".to_string())?
        };

        return socket
            .write_frame(&frame)
            .map_err(|e| format!("Write failed: {e}"));
    }

    fn receive(&self) -> Result<Option<CanFrame>, String> {
        let socket = self.socket.as_ref().ok_or_else(|| "Channel is not open".to_string())?;

        match socket.read_frame() {
            Ok(SocketCanFrame::Data(df)) => {
                Ok(Some(CanFrame {
                    can_id: df.can_id().as_raw(),
                    is_extended: df.is_extended(),
                    data: df.data().to_vec(),
                    timestamp_ms: 0,
                }))
            }
            Ok(_) => Ok(None), // remote / error frames
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        Err("Set bitrate not implemented yet".to_string())
    }
    fn get_bitrate(&self) -> Result<u32, String> {
        let out = match std::process::Command::new("ip")
            .args(["-det", "link", "show", &self.name])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return Err("Failed to get bitrate".to_string()),
        };

        if !out.contains("state UP") && !out.contains("state UNKNOWN") {
            return Err("Interface is down".to_string());
        }
        if self.is_virtual() {
            return Ok(0); // vcan bitrate is meaningless
        }

        for line in out.lines() {
            if let Some(rest) = line.trim().strip_prefix("bitrate ") {
                if let Ok(current) = rest.split_whitespace().next().unwrap_or("").parse::<u32>() {
                    return Ok(current);
                }
            }
        }
        return Err("Interface is up but bitrate unreadable".to_string());
    }

    fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn open(&mut self) -> Result<(), String> {
        let socket = CanSocket::open(self.name.as_str());
        if socket.is_err() {
            return Err(format!("Failed to open '{}': {:?}", self.name, socket.err()));
        }
        self.socket = Some(socket.unwrap());

        Ok(())
    }
}
