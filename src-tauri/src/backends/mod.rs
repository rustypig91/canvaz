#[cfg(feature = "kvaser")]
mod kvaser;
#[cfg(feature = "linux-can")]
mod socketcan;

#[cfg(feature = "kvaser")]
use kvaser::{KvaserBackend, KvaserBackendChannel};
#[cfg(feature = "linux-can")]
use socketcan::{SocketCanBackend, SocketCanChannel};

use crate::app_state::AppState;
use serde::Serialize;
use std::sync::Arc;

// ── Frame ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CanFrame {
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Channel ───────────────────────────────────────────────────────────────────

pub enum Channel {
    #[cfg(feature = "linux-can")]
    SocketCan(SocketCanChannel),
    #[cfg(feature = "kvaser")]
    Kvaser(KvaserBackendChannel),
}

macro_rules! dispatch {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            #[cfg(feature = "linux-can")]
            Channel::SocketCan(c) => c.$method($($arg),*),
            #[cfg(feature = "kvaser")]
            Channel::Kvaser(c) => c.$method($($arg),*),
        }
    };
}

impl Channel {
    pub fn open(&mut self) -> Result<(), String> {
        dispatch!(self, open)
    }
    pub fn close(&mut self) -> Result<(), String> {
        dispatch!(self, close)
    }
    pub fn send(&self, frame: CanFrame) -> Result<(), String> {
        dispatch!(self, send, frame)
    }
    pub fn receive(&self) -> Result<Option<CanFrame>, String> {
        dispatch!(self, receive)
    }
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        dispatch!(self, set_bitrate, bitrate)
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
            Backend::SocketCan(backend) => backend.open_channel(name, bitrate, state).map(Channel::SocketCan),
            #[cfg(feature = "kvaser")]
            Backend::Kvaser(backend) => backend.open_channel(name, bitrate, state).map(Channel::Kvaser),
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
