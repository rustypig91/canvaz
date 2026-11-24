//! Classic CAN over socketcand RAW mode. Bus configuration stays on the server.
use super::{CanBackend, CanFrame, CanOpenError, RxHandle, TxHandle};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct SocketcandBackend {
    // An open channel keeps its index even when another channel is removed.
    channels: Vec<Option<String>>,
}

const MAX_MESSAGE_BYTES: usize = 4096;

fn endpoint(name: &str) -> Result<(String, u16, String), String> {
    let (address, bus) = if let Some((label, address)) = name.split_once(" (") {
        (address.strip_suffix(')').ok_or("Invalid socketcand label")?, Some(label.split('@').next().unwrap_or(label)))
    } else { (name, None) };
    let url = url::Url::parse(address).map_err(|e| e.to_string())?;
    if url.scheme() != "can" || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("Use can://host:port/interface for socketcand".into());
    }
    let path_bus = url.path().strip_prefix('/').unwrap_or(url.path());
    if bus.is_some_and(|label_bus| !path_bus.is_empty() && label_bus != path_bus) {
        return Err("Socketcand label and address specify different interfaces".into());
    }
    let bus = bus.unwrap_or(path_bus);
    if bus.is_empty() || bus.len() > 15 || !bus.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)) {
        return Err("Socketcand requires an interface, for example can://127.0.0.1:29536/vcan0".into());
    }
    let host = match url.host() {
        Some(url::Host::Ipv6(host)) => host.to_string(),
        Some(url::Host::Ipv4(host)) => host.to_string(),
        Some(url::Host::Domain(host)) if !host.is_empty()
            && host.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) => host.to_owned(),
        _ => return Err("Socketcand requires a valid host name or IP address".into()),
    };
    let port = url.port().unwrap_or(29536);
    if port == 0 {
        return Err("Socketcand port must be between 1 and 65535".into());
    }
    Ok((host, port, bus.to_owned()))
}

impl CanBackend for SocketcandBackend {
    fn register_channel(&mut self, name: &str) -> Result<(), String> {
        endpoint(name)?;
        if self.channel_index(name).is_some() {
            return Ok(());
        }
        if let Some(slot) = self.channels.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(name.into());
        } else {
            if self.channels.len() > usize::from(u8::MAX) {
                return Err("Too many socketcand channels".into());
            }
            self.channels.push(Some(name.into()));
        }
        Ok(())
    }

    fn unregister_channel(&mut self, index: u8) {
        if let Some(slot) = self.channels.get_mut(usize::from(index)) {
            *slot = None;
        }
    }

    fn list_channels(&self) -> Vec<String> {
        self.channels.iter().flatten().cloned().collect()
    }

    fn channel_index(&self, name: &str) -> Option<u8> {
        let index = self.channels.iter().position(|slot| slot.as_deref() == Some(name))?;
        u8::try_from(index).ok()
    }

    fn open_channel(&mut self, index: u8, _: u32, listen_only: bool, _: Option<&str>) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), CanOpenError> {
        let name = self.channels.get(usize::from(index)).and_then(Option::as_deref)
            .ok_or("Unknown socketcand channel")?;
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
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
        let mut rx = SocketcandRx::new(stream);
        rx.expect("hi")?;
        rx.stream.write_all(format!("< open {bus} >").as_bytes()).map_err(|e| e.to_string())?;
        rx.expect("ok")?;
        rx.stream.write_all(b"< rawmode >").map_err(|e| e.to_string())?;
        rx.expect("ok")?;
        // The TX worker is the only writer after the handshake. Cloning the
        // socket lets RX block independently without locking out transmission.
        let tx = SocketcandTx { stream: rx.stream.try_clone().map_err(|e| e.to_string())?, listen_only };
        Ok((Box::new(tx), Box::new(rx)))
    }
}
struct SocketcandTx { stream: TcpStream, listen_only: bool }
impl TxHandle for SocketcandTx {
    fn send(&mut self, frame: &mut CanFrame) -> Result<(), String> {
        if self.listen_only { return Err("Socketcand channel is receive-only".into()); }
        frame.validate_transmit()?;
        // Identifier width selects standard vs extended frames, including low
        // extended IDs. socketcand's send command uses separate hex data bytes.
        let id = if frame.is_extended { format!("{:08X}", frame.can_id) } else { format!("{:03X}", frame.can_id) };
        let mut command = format!("< send {id} {}", frame.data.len());
        for byte in &frame.data {
            use std::fmt::Write;
            write!(command, " {byte:x}").unwrap();
        }
        command.push_str(" >");
        if let Err(error) = self.stream.write_all(command.as_bytes()) {
            // A failed write may have left half a command on the wire. Close
            // both halves so RX reports a fatal failure and recovery reconnects.
            let _ = self.stream.shutdown(Shutdown::Both);
            return Err(error.to_string());
        }
        // RAW mode has no positive send acknowledgement: this is the time the
        // command was written, not confirmation of delivery on the remote bus.
        frame.timestamp_ms = Some(now_ms());
        Ok(())
    }
    // RX owns shutdown. Dropping this clone must not interrupt an in-flight
    // receive and report a spurious fatal error during a deliberate Stop.
    fn close(&mut self) {}
}
struct SocketcandRx {
    stream: TcpStream,
    pending: Vec<u8>,
    clock_anchor: Option<(u64, u64)>,
}
impl SocketcandRx {
    fn new(stream: TcpStream) -> Self {
        Self { stream, pending: Vec::new(), clock_anchor: None }
    }
    fn local_timestamp(&mut self, server_ms: u64, local_ms: u64) -> u64 {
        // Match the hardware backends: anchor the first sample to this host's
        // clock, retaining server-relative timing for plots and cycle times.
        let (server_base, local_base) = *self.clock_anchor.get_or_insert((server_ms, local_ms));
        if server_ms >= server_base {
            local_base.saturating_add(server_ms - server_base)
        } else {
            local_base.saturating_sub(server_base - server_ms)
        }
    }
    fn message(&mut self, deadline: Instant) -> Result<Option<String>, String> {
        loop {
            // Bound the complete message as well as an unfinished one.
            // Bytes after the first terminator belong to later messages.
            let message_window = &self.pending[..self.pending.len().min(MAX_MESSAGE_BYTES)];
            if let Some(end) = message_window.iter().position(|&b| b == b'>') {
                let bytes: Vec<_> = self.pending.drain(..=end).collect();
                let text = std::str::from_utf8(&bytes).map_err(|_| "Non-ASCII socketcand response")?.trim();
                let text = text.strip_prefix('<').and_then(|s| s.strip_suffix('>')).ok_or("Malformed socketcand response")?;
                return Ok(Some(text.trim().to_owned()));
            }
            if self.pending.len() >= MAX_MESSAGE_BYTES { return Err("Socketcand response too large".into()); }
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
            if message == "ok" || message == "echo" || message.split_whitespace().next() == Some("fdframe") {
                // FD traffic can share the bus with classic CAN. Consume it
                // without killing capture, but still honor the RX deadline.
                if Instant::now() >= deadline { return Ok(None); }
                continue;
            }
            let mut frame = parse_frame(&message)?;
            frame.timestamp_ms = frame.timestamp_ms.map(|ts| self.local_timestamp(ts, now_ms()));
            return Ok(Some(frame));
        }
    }
    fn close(&mut self) { let _ = self.stream.shutdown(Shutdown::Both); }
}
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
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
        assert_eq!(endpoint("can://[::1]/vcan0").unwrap(), ("::1".into(), 29536, "vcan0".into()));
        assert!(endpoint("can://localhost/").is_err());
        assert!(endpoint("can://localhost/can0%20>").is_err());
        assert!(endpoint("can:///can0").is_err());
        assert!(endpoint("can://local%20host/can0").is_err());
        assert!(endpoint("can://localhost:0/can0").is_err());
        assert!(endpoint("vcan0@SIL (can://localhost/can1)").is_err());
        let f = parse_frame("frame 00000123 1700000000.123456 01FF").unwrap();
        assert!(f.is_extended);
        assert_eq!(f.data, [1,255]);
        assert_eq!(f.timestamp_ms, Some(1700000000123));
        assert!(parse_frame("frame 123 1.000000").unwrap().data.is_empty());
        for bad in ["frame 800 1.000000 FF", "frame 123 1.000000 F", "frame 123 NaN FF", "error denied"] { assert!(parse_frame(bad).is_err()); }
    }

    #[test]
    fn removed_endpoints_free_slots_without_renumbering_other_channels() {
        let mut backend = SocketcandBackend::default();
        for index in 0..=u8::MAX {
            backend.register_channel(&format!("can://localhost/can{index}")).unwrap();
        }
        let replacement = "can://localhost/vcan0";
        assert!(backend.register_channel(replacement).is_err());
        backend.unregister_channel(0);
        assert_eq!(backend.channel_index("can://localhost/can0"), None);
        assert_eq!(backend.list_channels()[0], "can://localhost/can1");
        assert_eq!(backend.channel_index("can://localhost/can1"), Some(1));
        backend.register_channel(replacement).unwrap();
        assert_eq!(backend.channel_index(replacement), Some(0));
        assert_eq!(backend.list_channels().len(), 256);
    }

    #[test]
    fn oversized_messages_are_rejected_even_with_a_terminator() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut rx = SocketcandRx::new(stream);
        let mut message = format!("<{}>", " ".repeat(MAX_MESSAGE_BYTES - 2)).into_bytes();
        message.extend_from_slice(b"< ok >");
        rx.pending = message;
        assert_eq!(rx.message(Instant::now()).unwrap().as_deref(), Some(""));
        assert_eq!(rx.message(Instant::now()).unwrap().as_deref(), Some("ok"));
        rx.pending = format!("<{}>", " ".repeat(MAX_MESSAGE_BYTES - 1)).into_bytes();
        assert_eq!(rx.message(Instant::now()).unwrap_err(), "Socketcand response too large");
    }
    #[test]
    fn partial_messages_survive_timeouts() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut rx = SocketcandRx::new(stream);
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
            let mut peer = SocketcandRx::new(stream);
            peer.stream.write_all(b"< hi >").unwrap();
            peer.expect("open vcan0").unwrap();
            peer.stream.write_all(b"< ok >").unwrap();
            peer.expect("rawmode").unwrap();
            peer.stream.write_all(b"< ok >< frame 123 1.000000 AA >< frame 00000123 2.000000 >").unwrap();
        });
        let mut backend = SocketcandBackend::default();
        backend.register_channel(&format!("can://{address}/vcan0")).unwrap();
        let (mut tx, mut rx) = backend.open_channel(0, 0, true, None).unwrap();
        assert!(tx.send(&mut frame(0x123, false, vec![1])).unwrap_err().contains("receive-only"));
        assert_eq!(rx.receive(100).unwrap().unwrap().data, [0xAA]);
        assert!(rx.receive(100).unwrap().unwrap().is_extended);
        assert!(rx.receive(100).is_err());
        rx.close();
        server.join().unwrap();
    }

    #[test]
    fn mixed_fd_traffic_keeps_classic_frames_and_errors_on_the_local_timeline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut rx = SocketcandRx::new(stream);
        rx.pending = b"< fdframe 123 01 1.000000 AABB >< frame 123 1.000000 AA >< fdframe 123 00 1.500000 FF >< error 004 2.000000 >< frame 00000123 2.250000 BB >".to_vec();
        let before = now_ms();
        let first = rx.receive(100).unwrap().unwrap();
        let first_ts = first.timestamp_ms.unwrap();
        assert!((before..=now_ms()).contains(&first_ts));
        assert_eq!(first.data, [0xAA]);
        let error = rx.receive(100).unwrap().unwrap();
        assert!(error.error.is_some());
        assert_eq!(error.timestamp_ms, Some(first_ts + 1000));
        let next = rx.receive(100).unwrap().unwrap();
        assert!(next.is_extended);
        assert_eq!(next.timestamp_ms, Some(first_ts + 1250));
        rx.pending = b"< fdframe 123 00 3.000000 AA >< frame 123 3.000000 CC >".to_vec();
        assert!(rx.receive(0).unwrap().is_none());
        assert_eq!(rx.receive(100).unwrap().unwrap().data, [0xCC]);
    }

    #[test]
    fn clock_anchor_handles_servers_ahead_of_and_behind_the_client() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut rx = SocketcandRx::new(stream);
        for server_base in [1000, 1_800_000_000_000] {
            rx.clock_anchor = None;
            assert_eq!(rx.local_timestamp(server_base, 1_700_000_000_000), 1_700_000_000_000);
            // Network jitter does not change the interval between samples.
            assert_eq!(rx.local_timestamp(server_base + 125, 1_700_000_005_000), 1_700_000_000_125);
            assert_eq!(rx.local_timestamp(server_base - 125, 1_700_000_005_100), 1_699_999_999_875);
        }
    }

    fn frame(id: u32, extended: bool, data: Vec<u8>) -> CanFrame {
        CanFrame { can_id: id, is_extended: extended, data, timestamp_ms: None, error: None }
    }

    #[test]
    fn tcp_transmit_uses_identifier_width_and_separate_payload_bytes() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut peer = SocketcandRx::new(stream);
            peer.stream.write_all(b"< hi >").unwrap();
            peer.expect("open vcan0").unwrap();
            peer.stream.write_all(b"< ok >").unwrap();
            peer.expect("rawmode").unwrap();
            peer.stream.write_all(b"< ok >< frame 123 1.000000 AA >").unwrap();
            peer.expect("send 123 2 1 ff").unwrap();
            peer.expect("send 00000123 0").unwrap();
            peer.expect("send 1FFFFFFF 8 0 1 2 3 4 5 6 ff").unwrap();
            peer.stream.write_all(b"< frame 124 1.100000 BB >").unwrap();
        });
        let mut backend = SocketcandBackend::default();
        backend.register_channel(&format!("can://{address}/vcan0")).unwrap();
        let (mut tx, mut rx) = backend.open_channel(0, 0, false, None).unwrap();
        for mut invalid in [frame(0x800, false, vec![]), frame(0x2000_0000, true, vec![]), frame(1, false, vec![0; 9])] {
            assert!(tx.send(&mut invalid).is_err());
            assert!(invalid.timestamp_ms.is_none());
        }
        for mut valid in [frame(0x123, false, vec![1, 255]), frame(0x123, true, vec![]), frame(0x1FFF_FFFF, true, vec![0, 1, 2, 3, 4, 5, 6, 255])] {
            let before = now_ms();
            tx.send(&mut valid).unwrap();
            assert!((before..=now_ms()).contains(&valid.timestamp_ms.unwrap()));
        }
        assert_eq!(rx.receive(1000).unwrap().unwrap().data, [0xAA]);
        assert_eq!(rx.receive(1000).unwrap().unwrap().data, [0xBB]);
        assert!(rx.receive(1000).is_err());
        tx.close();
        rx.close();
        server.join().unwrap();
    }

    #[test]
    fn receive_only_blocks_transmit_and_write_failures_are_reported() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        let mut tx = SocketcandTx { stream, listen_only: true };
        let mut f = frame(0x123, false, vec![1]);
        assert!(tx.send(&mut f).unwrap_err().contains("receive-only"));
        assert!(f.timestamp_ms.is_none());
        let mut peer = SocketcandRx::new(peer);
        assert!(peer.message(Instant::now() + Duration::from_millis(20)).unwrap().is_none());
        tx.listen_only = false;
        tx.stream.shutdown(Shutdown::Both).unwrap();
        assert!(tx.send(&mut f).is_err());
        assert!(f.timestamp_ms.is_none());
    }

    #[test]
    fn queued_one_shot_and_periodic_transmissions_share_the_capture_connection() {
        use super::super::Can;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, transmitted) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut peer = SocketcandRx::new(stream);
            peer.stream.write_all(b"< hi >").unwrap();
            peer.expect("open vcan0").unwrap();
            peer.stream.write_all(b"< ok >").unwrap();
            peer.expect("rawmode").unwrap();
            peer.stream.write_all(b"< ok >< frame 321 1.000000 FF >").unwrap();
            let mut once = 0;
            let mut periodic = 0;
            while once < 1 || periodic < 2 {
                match peer.message(Instant::now() + Duration::from_secs(3)).unwrap().as_deref() {
                    Some("send 123 1 1") => once += 1,
                    Some("send 00000123 1 2") => periodic += 1,
                    other => panic!("unexpected command: {other:?}"),
                }
            }
            sent.send(()).unwrap();
            // Keep the connection alive until the client deliberately closes.
            while peer.message(Instant::now() + Duration::from_secs(3)).is_ok_and(|m| m.is_some()) {}
        });
        let (received, events) = std::sync::mpsc::channel();
        let (failed, errors) = std::sync::mpsc::channel();
        let mut can = Can::new(SocketcandBackend::default(), move |_, f| { received.send(f).unwrap(); }, |_, _| {}, move |_, error, fatal| { failed.send((error, fatal)).unwrap(); }, |_, _| {});
        can.register_channel(&format!("can://{address}/vcan0")).unwrap();
        can.open(0, 0, false, None).unwrap();
        can.send_once(0, frame(0x123, false, vec![1])).unwrap();
        let periodic = can.add_periodic(0, frame(0x123, true, vec![2]), 10, None).unwrap();
        assert_eq!(events.recv_timeout(Duration::from_secs(3)).unwrap().can_id, 0x321);
        // Wait for observed commands rather than relying on a scheduling delay.
        transmitted.recv_timeout(Duration::from_secs(3)).unwrap();
        can.remove_periodic(0, periodic).unwrap();
        can.close(0).unwrap();
        server.join().unwrap();
        assert!(errors.try_recv().is_err(), "deliberate Stop must not report a channel failure");
    }
}
