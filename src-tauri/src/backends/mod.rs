#[cfg(feature = "kvaser")]
mod kvaser;
#[cfg(feature = "linux-can")]
mod socketcan;

#[cfg(feature = "kvaser")]
use kvaser::{KvaserBackend, KvaserBackendChannel};
#[cfg(feature = "linux-can")]
use socketcan::{SocketCanBackend, SocketCanChannel};

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::app_state::AppState;
use crate::can_frame::{now_ms, CanFrame, Direction};
use crate::dbc_parser::*;

const DEFAULT_WINDOW_MS: u64 = 30_000;

// ── Channel ───────────────────────────────────────────────────────────────────

enum ChannelInner {
    #[cfg(feature = "linux-can")]
    SocketCan(SocketCanChannel),
    #[cfg(feature = "kvaser")]
    Kvaser(KvaserBackendChannel),
}

impl ChannelInner {
    fn name(&self) -> &str {
        match self {
            #[cfg(feature = "linux-can")]
            ChannelInner::SocketCan(c) => c.name(),
            #[cfg(feature = "kvaser")]
            ChannelInner::Kvaser(c) => c.name(),
        }
    }
    fn open(&mut self) -> Result<(), String> {
        match self {
            #[cfg(feature = "linux-can")]
            ChannelInner::SocketCan(c) => c.open(),
            #[cfg(feature = "kvaser")]
            ChannelInner::Kvaser(c) => c.open(),
        }
    }
    fn close(&mut self) -> Result<(), String> {
        match self {
            #[cfg(feature = "linux-can")]
            ChannelInner::SocketCan(c) => c.close(),
            #[cfg(feature = "kvaser")]
            ChannelInner::Kvaser(c) => c.close(),
        }
    }
    fn send(&self, frame: CanFrame) -> Result<(), String> {
        match self {
            #[cfg(feature = "linux-can")]
            ChannelInner::SocketCan(c) => c.send(frame),
            #[cfg(feature = "kvaser")]
            ChannelInner::Kvaser(c) => c.send(frame),
        }
    }
    fn receive(&self) -> Result<Option<CanFrame>, String> {
        match self {
            #[cfg(feature = "linux-can")]
            ChannelInner::SocketCan(c) => c.receive(),
            #[cfg(feature = "kvaser")]
            ChannelInner::Kvaser(c) => c.receive(),
        }
    }
    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        match self {
            #[cfg(feature = "linux-can")]
            ChannelInner::SocketCan(c) => c.set_bitrate(bitrate),
            #[cfg(feature = "kvaser")]
            ChannelInner::Kvaser(c) => c.set_bitrate(bitrate),
        }
    }
}

pub struct Channel {
    inner: ChannelInner,
    frames: VecDeque<CanFrame>,
    window_ms: u64,
    parsed_dbc: Option<ParsedDbc>,
}

impl Channel {
    fn new(inner: ChannelInner, dbc_file: Option<String>) -> Self {
        Self {
            inner,
            frames: VecDeque::new(),
            window_ms: DEFAULT_WINDOW_MS,
            parsed_dbc: dbc_file.and_then(|path| ParsedDbc::new(&path).ok()),
        }
    }

    pub fn open(&mut self) -> Result<(), String> {
        self.frames.clear();
        if let Some(dbc) = self.parsed_dbc.as_mut() {
            dbc.reload()?;
        }
        self.inner.open()
    }

    pub fn close(&mut self) -> Result<(), String> {
        self.inner.close()
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        self.inner.set_bitrate(bitrate)
    }

    pub fn get_dbc(&self) -> Option<&ParsedDbc> {
        self.parsed_dbc.as_ref()
    }

    // Receives one frame, decodes it with the channel's DBC, and stores it.
    pub fn receive_decode_store(&mut self) -> Result<Option<CanFrame>, String> {
        let Some(mut frame) = self.inner.receive()? else {
            return Ok(None);
        };
        if let Some(dbc) = &self.parsed_dbc {
            frame.decoded = dbc.parse_frame(&frame);
        }
        self.push_frame(frame.clone());
        Ok(Some(frame))
    }

    // Reads one frame from hardware without storing it. Returns None on timeout.
    pub fn receive(&mut self) -> Result<Option<CanFrame>, String> {
        self.inner.receive()
    }

    // Stores a (possibly decoded) frame in the ring buffer.
    pub fn push_frame(&mut self, frame: CanFrame) {
        let cutoff = frame.timestamp_ms.saturating_sub(self.window_ms);
        while self.frames.front().map_or(false, |f| f.timestamp_ms < cutoff) {
            self.frames.pop_front();
        }
        self.frames.push_back(frame);
    }

    // Sends a frame to hardware and stores it in the ring buffer.
    pub fn send(&mut self, frame: CanFrame) -> Result<(), String> {
        let to_store = frame.clone();
        self.inner.send(frame)?;
        self.push_frame(to_store);
        Ok(())
    }

    // Encodes a DBC message with the given signal values and sends it.
    pub fn send_dbc_message(
        &mut self,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        ts: u64,
    ) -> Result<CanFrame, String> {
        let dbc = self.parsed_dbc.as_ref()
            .ok_or_else(|| "No DBC loaded for this channel".to_string())?;
        let msg = dbc.messages.iter().find(|m| m.id == msg_id)
            .ok_or_else(|| format!("Message 0x{:X} not in DBC", msg_id))?;
        let mut buf = vec![0u8; msg.dlc as usize];
        for sig in &msg.signals {
            if let Some(&v) = signal_values.get(&sig.name) {
                encode(&mut buf, v, sig.start_bit, sig.length, sig.little_endian, sig.factor, sig.offset);
            }
        }
        let is_extended = msg_id > 0x7FF;
        let frame = CanFrame {
            can_id: msg_id,
            is_extended,
            data: buf,
            timestamp_ms: ts,
            direction: Direction::Tx,
            decoded: None,
        };
        self.send(frame.clone())?;
        Ok(frame)
    }

    // Changes the time window, reinitialising the buffer with only frames that
    // still fall within the new window.
    pub fn set_window_ms(&mut self, ms: u64) {
        self.window_ms = ms;
        let cutoff = now_ms().saturating_sub(ms);
        let fresh: VecDeque<CanFrame> = self.frames.drain(..).filter(|f| f.timestamp_ms >= cutoff).collect();
        self.frames = fresh;
    }

    pub fn frames_since(&self, since_ms: u64) -> impl Iterator<Item = &CanFrame> {
        self.frames.iter().filter(move |f| f.timestamp_ms >= since_ms)
    }

    pub fn frame_buffer(&self) -> &VecDeque<CanFrame> {
        &self.frames
    }
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub enum Backend {
    #[cfg(feature = "linux-can")]
    SocketCan(SocketCanBackend),
    #[cfg(feature = "kvaser")]
    Kvaser(KvaserBackend),
}

impl Backend {
    pub fn name(&self) -> &str {
        match self {
            #[cfg(feature = "linux-can")]
            Backend::SocketCan(backend) => backend.name(),
            #[cfg(feature = "kvaser")]
            Backend::Kvaser(backend) => backend.name(),
        }
    }

    pub fn list_channels(&self) -> Result<Vec<String>, String> {
        match self {
            #[cfg(feature = "linux-can")]
            Backend::SocketCan(backend) => backend.list_channels(),
            #[cfg(feature = "kvaser")]
            Backend::Kvaser(backend) => backend.list_channels(),
        }
    }

    pub fn open_channel(&self, name: &str, bitrate: Option<u32>, state: Arc<AppState>, dbc_path: Option<&str>) -> Result<Channel, String> {
        match self {
            #[cfg(feature = "linux-can")]
            Backend::SocketCan(backend) => backend
                .open_channel(name, bitrate, state)
                .map(|c| Channel::new(ChannelInner::SocketCan(c), dbc_path.map(str::to_string))),
            #[cfg(feature = "kvaser")]
            Backend::Kvaser(backend) => backend
                .open_channel(name, bitrate, state)
                .map(|c| Channel::new(ChannelInner::Kvaser(c), dbc_path.map(str::to_string))),
        }
    }
}

pub fn default_backends() -> Vec<Backend> {
    let mut backends = Vec::new();
    #[cfg(feature = "linux-can")]
    backends.push(Backend::SocketCan(SocketCanBackend));
    #[cfg(feature = "kvaser")]
    backends.push(Backend::Kvaser(KvaserBackend));
    backends
}
