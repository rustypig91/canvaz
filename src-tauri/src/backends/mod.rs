#[cfg(feature = "kvaser")]
mod kvaser;
#[cfg(feature = "linux-can")]
mod socketcan;


#[cfg(feature = "kvaser")]
use kvaser::{KvaserBackend, KvaserBackendChannel};
#[cfg(feature = "linux-can")]
use socketcan::{SocketCanBackend, SocketCanChannel};

use std::collections::VecDeque;
use std::sync::Arc;

use crate::app_state::AppState;
use crate::dbc_parser::*;
use crate::can_frame::{CanFrame, Direction, now_ms};

const DEFAULT_WINDOW_MS: u64 = 30_000;

// ── Channel ───────────────────────────────────────────────────────────────────

enum ChannelInner {
    #[cfg(feature = "linux-can")]
    SocketCan(SocketCanChannel),
    #[cfg(feature = "kvaser")]
    Kvaser(KvaserBackendChannel),
}

impl ChannelInner {
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
    parsed_dbc: Option<ParsedDbc>
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

    // Changes the time window, reinitialising the buffer with only frames that
    // still fall within the new window.
    pub fn set_window_ms(&mut self, ms: u64) {
        self.window_ms = ms;
        let cutoff = now_ms().saturating_sub(ms);
        let fresh: VecDeque<CanFrame> =
            self.frames.drain(..).filter(|f| f.timestamp_ms >= cutoff).collect();
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

    pub fn open_channel(&self, name: &str, bitrate: Option<u32>, state: Arc<AppState>) -> Result<Channel, String> {
        match self {
            #[cfg(feature = "linux-can")]
            Backend::SocketCan(backend) => backend
                .open_channel(name, bitrate, state)
                .map(|c| Channel::new(ChannelInner::SocketCan(c), None)),
            #[cfg(feature = "kvaser")]
            Backend::Kvaser(backend) => backend
                .open_channel(name, bitrate, state)
                .map(|c| Channel::new(ChannelInner::Kvaser(c), None)),
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
