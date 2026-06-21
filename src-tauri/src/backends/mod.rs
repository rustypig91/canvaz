mod socketcan;
pub use socketcan::SocketCanBackend;

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, RwLock,
};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::dbc_parser::ParsedDbc;

// ── Wire events ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CanFrameEvent {
    pub channel: String,
    pub can_id: u32,
    pub is_extended: bool,
    pub dlc: u8,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SignalValueEvent {
    pub channel: String,
    pub signal_name: String,
    pub message_name: String,
    pub value: f64,
    pub unit: String,
    pub timestamp_ms: u64,
}

// ── Frame ─────────────────────────────────────────────────────────────────────

pub struct CanFrame {
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
}

// ── Traits ────────────────────────────────────────────────────────────────────

pub trait CanChannel: Send {
    fn name(&self) -> &str;
    /// Configure + open the socket.  Pass a sudo password when a previous call
    /// returned `Err("needs-sudo: …")` and the user authenticated.
    fn open(&mut self, sudo_password: Option<&str>) -> Result<(), String>;
    fn close(&mut self) -> Result<(), String>;
    fn send(&self, frame: CanFrame) -> Result<(), String>;
    /// Blocks for up to the channel's read-timeout, then returns `Ok(None)`.
    fn receive(&self) -> Result<Option<CanFrame>, String>;
    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String>;
    fn get_bitrate(&self) -> Result<u32, String>;
}

pub trait CanBackend: Send + Sync + 'static {
    fn name(&self) -> &str;
    /// List all interface names currently visible to this backend.
    fn list_channels(&self) -> Vec<String>;
    /// Create a closed channel handle.  Call `open()` on it to activate.
    fn open_channel(
        &mut self,
        name: &str,
        bitrate: Option<u32>,
    ) -> Result<Box<dyn CanChannel>, String>;
}

// ── Channel info (serialized to frontend) ─────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ChannelInfo {
    pub backend: String,
    pub name: String,
}

// ── Manager ───────────────────────────────────────────────────────────────────

struct OpenChannelState {
    stop_flag: Arc<AtomicBool>,
    /// Shared with the reading thread so that sends can proceed concurrently.
    /// The reading thread holds the lock for at most ~100 ms per receive call.
    channel: Arc<Mutex<Box<dyn CanChannel>>>,
    backend_name: String,
}

pub struct CanManager {
    backends: Vec<Arc<Mutex<dyn CanBackend>>>,
    channels: HashMap<String, OpenChannelState>,
}

impl CanManager {
    pub fn new() -> Self {
        Self { backends: Vec::new(), channels: HashMap::new() }
    }

    pub fn register_backend(&mut self, backend: impl CanBackend) {
        self.backends.push(Arc::new(Mutex::new(backend)));
    }

    /// All interfaces known to all registered backends.
    pub fn list_channels(&self) -> Vec<ChannelInfo> {
        let mut result = Vec::new();
        for backend in &self.backends {
            if let Ok(b) = backend.lock() {
                let bname = b.name().to_string();
                for ch in b.list_channels() {
                    result.push(ChannelInfo { backend: bname.clone(), name: ch });
                }
            }
        }
        result
    }

    /// Open a channel and start its reading thread.
    pub fn open_channel(
        &mut self,
        backend_name: &str,
        channel_name: &str,
        bitrate: Option<u32>,
        sudo_password: Option<&str>,
        app: AppHandle,
        dbc: DbcState,
    ) -> Result<(), String> {
        if self.channels.contains_key(channel_name) {
            return Err(format!("Channel '{channel_name}' is already open"));
        }

        let backend = self
            .backends
            .iter()
            .find_map(|b| {
                let g = b.lock().ok()?;
                if g.name() == backend_name { Some(Arc::clone(b)) } else { None }
            })
            .ok_or_else(|| format!("No backend named '{backend_name}'"))?;

        let mut channel = {
            let mut b = backend.lock().map_err(|_| "Backend lock poisoned".to_string())?;
            b.open_channel(channel_name, bitrate)?
        };

        channel.open(sudo_password)?;

        let channel = Arc::new(Mutex::new(channel));
        let stop_flag = Arc::new(AtomicBool::new(false));

        std::thread::spawn({
            let channel = Arc::clone(&channel);
            let stop = Arc::clone(&stop_flag);
            let ch_name = channel_name.to_string();
            move || reading_loop(channel, ch_name, app, dbc, stop)
        });

        self.channels.insert(
            channel_name.to_string(),
            OpenChannelState { stop_flag, channel, backend_name: backend_name.to_string() },
        );
        Ok(())
    }

    pub fn close_channel(&mut self, name: &str) -> Result<(), String> {
        match self.channels.remove(name) {
            Some(state) => {
                state.stop_flag.store(true, Ordering::Relaxed);
                Ok(())
            }
            None => Err(format!("Channel '{name}' is not open")),
        }
    }

    pub fn close_all(&mut self) {
        for (_, state) in self.channels.drain() {
            state.stop_flag.store(true, Ordering::Relaxed);
        }
    }

    pub fn open_names(&self) -> Vec<String> {
        self.channels.keys().cloned().collect()
    }

    pub fn open_channels_info(&self) -> Vec<ChannelInfo> {
        self.channels
            .iter()
            .map(|(name, state)| ChannelInfo {
                backend: state.backend_name.clone(),
                name: name.clone(),
            })
            .collect()
    }

    pub fn send_frame(&self, channel: &str, can_id: u32, data: &[u8]) -> Result<(), String> {
        let state = self
            .channels
            .get(channel)
            .ok_or_else(|| format!("Channel '{channel}' is not open"))?;
        let ch = state.channel.lock().map_err(|_| "Channel lock poisoned".to_string())?;
        ch.send(CanFrame {
            can_id,
            is_extended: can_id > 0x7FF,
            data: data.to_vec(),
            timestamp_ms: 0,
        })
    }
}

// ── Reading thread ────────────────────────────────────────────────────────────

fn reading_loop(
    channel: Arc<Mutex<Box<dyn CanChannel>>>,
    channel_name: String,
    app: AppHandle,
    dbc: DbcState,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        // Release the lock as soon as receive() returns so sends are not blocked
        // for more than one receive timeout (~100 ms).
        let result = {
            let ch = match channel.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            ch.receive()
        };

        match result {
            Ok(Some(frame)) => {
                let ts = frame.timestamp_ms;

                let _ = app.emit("can-frame", CanFrameEvent {
                    channel: channel_name.clone(),
                    can_id: frame.can_id,
                    is_extended: frame.is_extended,
                    dlc: frame.data.len() as u8,
                    data: frame.data.clone(),
                    timestamp_ms: ts,
                });

                if let Ok(guard) = dbc.read() {
                    if let Some(channel_dbc) = guard.get(&channel_name) {
                        if let Some(signals) = channel_dbc.signals_for_message(frame.can_id) {
                            for sig in signals {
                                let value = crate::signal_codec::decode(
                                    &frame.data,
                                    sig.start_bit,
                                    sig.length,
                                    sig.little_endian,
                                    sig.signed,
                                    sig.factor,
                                    sig.offset,
                                );
                                let _ = app.emit("signal-value", SignalValueEvent {
                                    channel: channel_name.clone(),
                                    signal_name: sig.name.clone(),
                                    message_name: sig.message_name.clone(),
                                    value,
                                    unit: sig.unit.clone(),
                                    timestamp_ms: ts,
                                });
                            }
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!("CAN read error on '{channel_name}': {e}");
                break;
            }
        }
    }

    if let Ok(mut ch) = channel.lock() {
        let _ = ch.close();
    }
}

// ── Shared state types ────────────────────────────────────────────────────────

pub type ManagerState = Arc<Mutex<CanManager>>;
pub type DbcState = Arc<RwLock<HashMap<String, ParsedDbc>>>;
