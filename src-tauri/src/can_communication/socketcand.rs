//! Receive-only socketcand RAW-mode connections. Bus configuration stays on the server.
use super::{CanBackend, CanFrame, CanOpenError, RxHandle, TxHandle};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct SocketcandBackend { channels: Vec<String> }

fn endpoint(name: &str) -> Result<(String, u16, String), String> {
    let (address, bus) = if let Some((label, address)) = name.split_once(" (") {
        (address.strip_suffix(')').ok_or("Invalid socketcand label")?, Some(label.split('@').next().unwrap_or(label)))
    } else { (name, None) };
    let url = url::Url::parse(address).map_err(|e| e.to_string())?;
    if url.scheme() != "can" || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("Use can://host:port/interface for socketcand".into());
    }
    let bus = bus.unwrap_or(url.path().trim_start_matches('/'));
    if bus.is_empty() || bus.len() > 15 || !bus.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
        return Err("Socketcand requires an interface, for example can://127.0.0.1:29537/vcan0".into());
    }
    let host = url.host_str().ok_or("Socketcand host missing")?.trim_matches(['[', ']']).to_owned();
    Ok((host, url.port().unwrap_or(29536), bus.to_owned()))
}

impl CanBackend for SocketcandBackend {
    fn register_channel(&mut self, name: &str) -> Result<(), String> {
        endpoint(name)?;
        if !self.channels.iter().any(|n| n == name) {
            if self.channels.len() >= 256 { return Err("Too many socketcand channels".into()); }
            self.channels.push(name.into());
        }
        Ok(())
    }
    fn list_channels(&self) -> Vec<String> { self.channels.clone() }
    fn open_channel(&mut self, index: u8, _: u32, _: bool, _: Option<&str>) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), CanOpenError> {
        let name = self.channels.get(index as usize).ok_or("Unknown socketcand channel")?;
        let (host, port, bus) = endpoint(name)?;
        let mut stream = None;
        let mut last_error = "Socketcand host has no addresses".to_string();
        for address in (host.as_str(), port).to_socket_addrs().map_err(|e| e.to_string())? {
            match TcpStream::connect_timeout(&address, Duration::from_secs(3)) {
                Ok(s) => { stream = Some(s); break; }
                Err(e) => last_error = e.to_string(),
            }
        }
        let stream = stream.ok_or(last_error)?;
        stream.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
        let mut rx = SocketcandRx { stream, pending: Vec::new() };
        rx.expect("hi")?;
        rx.stream.write_all(format!("< open {bus} >").as_bytes()).map_err(|e| e.to_string())?;
        rx.expect("ok")?;
        rx.stream.write_all(b"< rawmode >").map_err(|e| e.to_string())?;
        rx.expect("ok")?;
        Ok((Box::new(MonitorTx), Box::new(rx)))
    }
}
struct MonitorTx;
impl TxHandle for MonitorTx {
    fn send(&mut self, _: &mut CanFrame) -> Result<(), String> { Err("Socketcand connections support monitoring only".into()) }
    fn close(&mut self) {}
}
struct SocketcandRx { stream: TcpStream, pending: Vec<u8> }
impl SocketcandRx {
    fn message(&mut self, deadline: Instant) -> Result<Option<String>, String> {
        loop {
            if let Some(end) = self.pending.iter().position(|&b| b == b'>') {
                let bytes: Vec<_> = self.pending.drain(..=end).collect();
                let text = std::str::from_utf8(&bytes).map_err(|_| "Non-ASCII socketcand response")?.trim();
                let text = text.strip_prefix('<').and_then(|s| s.strip_suffix('>')).ok_or("Malformed socketcand response")?;
                return Ok(Some(text.trim().to_owned()));
            }
            if self.pending.len() > 4096 { return Err("Socketcand response too large".into()); }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return Ok(None); }
            self.stream.set_read_timeout(Some(remaining)).map_err(|e| e.to_string())?;
            let mut buffer = [0; 1024];
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err("Socketcand disconnected".into()),
                Ok(n) => self.pending.extend_from_slice(&buffer[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
    }
    fn expect(&mut self, expected: &str) -> Result<(), String> {
        let response = self.message(Instant::now() + Duration::from_secs(3))?.ok_or("Socketcand handshake timed out")?;
        if response != expected { return Err(format!("Socketcand expected '{expected}', received '{response}'")); }
        Ok(())
    }
}
impl RxHandle for SocketcandRx {
    fn receive(&mut self, timeout_ms: u64) -> Result<Option<CanFrame>, String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let Some(message) = self.message(deadline)? else { return Ok(None); };
            if message == "ok" || message == "echo" { continue; }
            return parse_frame(&message).map(Some);
        }
    }
    fn close(&mut self) { let _ = self.stream.shutdown(Shutdown::Both); }
}
fn parse_frame(message: &str) -> Result<CanFrame, String> {
    let invalid = || format!("Invalid socketcand frame: {message}");
    let parts: Vec<_> = message.split_whitespace().collect();
    if parts.len() == 3 && parts[0] == "error" {
        let mut frame = parse_frame(&format!("frame 00000000 {}", parts[2]))?;
        let class = u32::from_str_radix(parts[1], 16).map_err(|_| invalid())?;
        frame.is_extended = false;
        frame.error = Some(format!("SocketCAN bus error 0x{class:X}"));
        return Ok(frame);
    }
    if !(3..=4).contains(&parts.len()) || parts[0] != "frame" { return Err(invalid()); }
    let id = parts[1];
    let can_id = u32::from_str_radix(id, 16).map_err(|_| invalid())?;
    let is_extended = id.len() == 8;
    if !(id.len() == 3 || is_extended) || can_id > if is_extended { 0x1fffffff } else { 0x7ff } { return Err(invalid()); }
    let (seconds, micros) = parts[2].split_once('.').ok_or_else(invalid)?;
    if micros.len() != 6 || !micros.bytes().all(|b| b.is_ascii_digit()) { return Err(invalid()); }
    let timestamp_ms = seconds.parse::<u64>().ok().and_then(|s| s.checked_mul(1000)).and_then(|s| s.checked_add(micros.parse::<u64>().ok()? / 1000)).ok_or_else(invalid)?;
    let payload = parts.get(3).copied().unwrap_or("");
    if payload.len() > 16 || payload.len() % 2 != 0 || !payload.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(invalid()); }
    let data = (0..payload.len()).step_by(2).map(|i| u8::from_str_radix(&payload[i..i+2], 16).map_err(|_| invalid())).collect::<Result<_, _>>()?;
    Ok(CanFrame { can_id, is_extended, data, timestamp_ms: Some(timestamp_ms), error: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn addresses_and_frames() {
        assert_eq!(endpoint("vcan0@SIL (can://127.0.0.1:29537)").unwrap(), endpoint("can://127.0.0.1:29537/vcan0").unwrap());
        assert!(endpoint("can://localhost/").is_err());
        assert!(endpoint("can://localhost/can0%20>").is_err());
        let f = parse_frame("frame 00000123 1700000000.123456 01FF").unwrap();
        assert!(f.is_extended);
        assert_eq!(f.data, [1,255]);
        assert_eq!(f.timestamp_ms, Some(1700000000123));
        assert!(parse_frame("frame 123 1.000000").unwrap().data.is_empty());
        for bad in ["frame 800 1.000000 FF", "frame 123 1.000000 F", "frame 123 NaN FF", "error denied"] { assert!(parse_frame(bad).is_err()); }
    }
    #[test]
    fn partial_messages_survive_timeouts() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut rx = SocketcandRx { stream, pending: Vec::new() };
        sender.write_all(b"< frame 123 1.").unwrap();
        assert!(rx.receive(20).unwrap().is_none());
        sender.write_all(b"000000 AA >").unwrap();
        assert_eq!(rx.receive(100).unwrap().unwrap().data, [0xAA]);
        assert!(parse_frame("error 004 1.000000").unwrap().error.is_some());
    }
    #[test]
    fn tcp_handshake_coalesced_frames_and_disconnect() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut peer = SocketcandRx { stream, pending: vec![] };
            peer.stream.write_all(b"< hi >").unwrap();
            peer.expect("open vcan0").unwrap();
            peer.stream.write_all(b"< ok >").unwrap();
            peer.expect("rawmode").unwrap();
            peer.stream.write_all(b"< ok >< frame 123 1.000000 AA >< frame 00000123 2.000000 >").unwrap();
        });
        let mut backend = SocketcandBackend::default();
        backend.register_channel(&format!("can://{address}/vcan0")).unwrap();
        let (_, mut rx) = backend.open_channel(0, 0, true, None).unwrap();
        assert_eq!(rx.receive(100).unwrap().unwrap().data, [0xAA]);
        assert!(rx.receive(100).unwrap().unwrap().is_extended);
        assert!(rx.receive(100).is_err());
        rx.close();
        server.join().unwrap();
    }
}
