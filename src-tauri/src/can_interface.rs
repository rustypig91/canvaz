use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::dbc_parser::ParsedDbc;

// ── Wire events (emitted to the frontend) ─────────────────────────────────────

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

// ── Backend abstraction ───────────────────────────────────────────────────────

/// A decoded CAN data frame, backend-agnostic.
pub struct RawFrame {
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
}

/// A live receive handle for one open channel.
/// `read_frame` blocks until a frame arrives or a short timeout elapses.
pub trait CanReceiver: Send {
    /// Returns `Ok(Some(frame))` on success, `Ok(None)` on timeout, `Err` on fatal error.
    fn read_frame(&self) -> Result<Option<RawFrame>, String>;
}

/// A CAN transport backend.  Implement this trait to add a new interface type.
pub trait CanBackend: Send + Sync + 'static {
    /// Human-readable backend name used for logging and diagnostics.
    fn name(&self) -> &'static str;
    /// Returns `true` if this backend can handle the given channel name.
    fn probe(&self, channel: &str) -> bool;
    /// List all interface names visible to this backend.
    fn list_interfaces(&self) -> Vec<String>;
    /// Bring the interface up with the requested bitrate (if needed).
    /// Return `Err("needs-sudo: …")` when a privilege escalation is required
    /// but no password was provided, so the caller can prompt and retry.
    fn configure(&self, channel: &str, bitrate: Option<u32>, sudo_password: Option<&str>) -> Result<(), String>;
    /// Open a receive handle for the channel.
    fn open_receiver(&self, channel: &str) -> Result<Box<dyn CanReceiver>, String>;
    /// Transmit a single CAN frame on the channel.
    fn send_frame(&self, channel: &str, can_id: u32, data: &[u8]) -> Result<(), String>;
}

// ── Manager ───────────────────────────────────────────────────────────────────

struct ChannelState {
    stop_flag: Arc<AtomicBool>,
}

pub struct CanManager {
    backends: Vec<Arc<dyn CanBackend>>,
    channels: HashMap<String, ChannelState>,
}

impl CanManager {
    pub fn new() -> Self {
        Self { backends: Vec::new(), channels: HashMap::new() }
    }

    /// Register a backend.  Backends are probed in registration order.
    pub fn register_backend(&mut self, backend: impl CanBackend) {
        self.backends.push(Arc::new(backend));
    }

    fn backend_for(&self, channel: &str) -> Option<Arc<dyn CanBackend>> {
        self.backends.iter().find(|b| b.probe(channel)).cloned()
    }

    pub fn list_interfaces(&self) -> Vec<String> {
        self.backends.iter().flat_map(|b| b.list_interfaces()).collect()
    }

    pub fn configure_channel(
        &self,
        name: &str,
        bitrate: Option<u32>,
        sudo_password: Option<&str>,
    ) -> Result<(), String> {
        self.backend_for(name)
            .ok_or_else(|| format!("No backend available for channel '{name}'"))?
            .configure(name, bitrate, sudo_password)
    }

    pub fn open_channel(
        &mut self,
        name: String,
        app: AppHandle,
        dbc: DbcState,
    ) -> Result<(), String> {
        if self.channels.contains_key(&name) {
            return Err(format!("Channel '{name}' is already open"));
        }
        let receiver = self
            .backend_for(&name)
            .ok_or_else(|| format!("No backend available for channel '{name}'"))?
            .open_receiver(&name)?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        std::thread::spawn({
            let stop = Arc::clone(&stop_flag);
            let ch = name.clone();
            move || reading_loop(receiver, ch, app, dbc, stop)
        });
        self.channels.insert(name, ChannelState { stop_flag });
        Ok(())
    }

    pub fn close_channel(&mut self, name: &str) -> Result<(), String> {
        match self.channels.remove(name) {
            Some(state) => { state.stop_flag.store(true, Ordering::Relaxed); Ok(()) }
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

    pub fn send_frame(&self, channel: &str, can_id: u32, data: &[u8]) -> Result<(), String> {
        self.backend_for(channel)
            .ok_or_else(|| format!("No backend available for channel '{channel}'"))?
            .send_frame(channel, can_id, data)
    }
}

// ── Reading thread ────────────────────────────────────────────────────────────

fn reading_loop(
    receiver: Box<dyn CanReceiver>,
    channel: String,
    app: AppHandle,
    dbc: DbcState,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        match receiver.read_frame() {
            Ok(Some(frame)) => {
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;

                let _ = app.emit("can-frame", CanFrameEvent {
                    channel: channel.clone(),
                    can_id: frame.can_id,
                    is_extended: frame.is_extended,
                    dlc: frame.data.len() as u8,
                    data: frame.data.clone(),
                    timestamp_ms: ts,
                });

                if let Ok(guard) = dbc.read() {
                    if let Some(channel_dbc) = guard.get(&channel) {
                        if let Some(signals) = channel_dbc.signals_for_message(frame.can_id) {
                            for sig in signals {
                                let value = crate::signal_codec::decode(
                                    &frame.data,
                                    sig.start_bit, sig.length, sig.little_endian,
                                    sig.signed, sig.factor, sig.offset,
                                );
                                let _ = app.emit("signal-value", SignalValueEvent {
                                    channel: channel.clone(),
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
                log::warn!("CAN read error on '{channel}': {e}");
                break;
            }
        }
    }
}

// ── Shared state types ────────────────────────────────────────────────────────

pub type ManagerState = Arc<Mutex<CanManager>>;
pub type DbcState = Arc<RwLock<HashMap<String, ParsedDbc>>>;
