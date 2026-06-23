use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, RwLock,
};

use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::app_state::AppState;
use crate::backends::{default_backends, Backend, CanFrame, Channel};
use crate::dbc_parser::ParsedDbc;

pub type ManagerState = Arc<Mutex<CanManager>>;
pub type DbcState = Arc<RwLock<HashMap<String, ParsedDbc>>>;

// ── Channel identity ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub backend: String,
    pub name: String,
}

impl ChannelInfo {
    fn new(backend: &str, name: &str) -> Self {
        Self {
            id: format!("{backend}:{name}"),
            backend: backend.to_string(),
            name: name.to_string(),
        }
    }
}

// ── Wire events ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CanFrameEvent {
    pub channel_id: String,
    pub can_id: u32,
    pub is_extended: bool,
    pub dlc: u8,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
    pub direction: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SignalValueEvent {
    pub channel_id: String,
    pub signal_name: String,
    pub message_name: String,
    pub value: f64,
    pub unit: String,
    pub timestamp_ms: u64,
}

// ── Manager internals ─────────────────────────────────────────────────────────

struct OpenChannelState {
    stop_flag: Arc<AtomicBool>,
    channel: Arc<Mutex<Channel>>,
    channel_info: ChannelInfo,
}

pub struct CanManager {
    state: Arc<AppState>,
    backends: Vec<Backend>,
    channels: HashMap<String, OpenChannelState>,
}

impl CanManager {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            backends: default_backends(),
            channels: HashMap::new(),
        }
    }

    pub fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let mut out = Vec::new();
        for b in &self.backends {
            let bname = b.name().to_string();
            for ch in b.list_channels()? {
                out.push(ChannelInfo::new(&bname, &ch));
            }
        }
        Ok(out)
    }

    pub fn open_channel(
        &mut self,
        backend_name: String,
        channel_name: String,
        bitrate: Option<u32>,
        dbc: DbcState,
    ) -> Result<ChannelInfo, String> {
        let info = ChannelInfo::new(&backend_name, &channel_name);
        if self.channels.contains_key(&info.id) {
            let channel_state = self.channels.get(&info.id).unwrap();
            let mut channel = channel_state
                .channel
                .lock()
                .map_err(|_| "Channel lock poisoned".to_string())?;
            let channel_info = channel_state.channel_info.clone();
            channel.set_bitrate(bitrate.unwrap_or(0))?;
            return Ok(channel_info);
        }

        let backend = self
            .backends
            .iter()
            .find(|b| b.name() == backend_name)
            .ok_or_else(|| format!("No backend '{backend_name}'"))?;

        let mut ch = backend.open_channel(&channel_name, bitrate, Arc::clone(&self.state))?;
        ch.open()?;

        let ch = Arc::new(Mutex::new(ch));
        let stop_flag = Arc::new(AtomicBool::new(false));

        std::thread::spawn({
            let ch = Arc::clone(&ch);
            let stop = Arc::clone(&stop_flag);
            let info = info.clone();
            let state = Arc::clone(&self.state);
            move || reading_loop(ch, info, state, dbc, stop)
        });

        self.channels.insert(
            info.id.clone(),
            OpenChannelState {
                stop_flag,
                channel: ch,
                channel_info: info.clone(),
            },
        );
        Ok(info)
    }

    pub fn close_channel(&mut self, channel_id: &str) -> Result<(), String> {
        self.channels
            .remove(channel_id)
            .map(|s| s.stop_flag.store(true, Ordering::Relaxed))
            .ok_or_else(|| format!("'{channel_id}' is not open"))
    }

    pub fn open_channels_info(&self) -> Vec<ChannelInfo> {
        self.channels
            .keys()
            .filter_map(|id| {
                let (backend, name) = id.split_once(':')?;
                Some(ChannelInfo::new(backend, name))
            })
            .collect()
    }

    // Hot path — avoid allocation, HashMap lookup is O(1).
    pub fn send_frame(&self, channel_id: &str, frame: CanFrame) -> Result<(), String> {
        self.channels
            .get(channel_id)
            .ok_or_else(|| format!("'{channel_id}' is not open"))?
            .channel
            .lock()
            .map_err(|_| "Channel lock poisoned".to_string())?
            .send(frame)
    }
}

// ── Reading thread ────────────────────────────────────────────────────────────

fn reading_loop(
    channel: Arc<Mutex<Channel>>,
    info: ChannelInfo,
    state: Arc<AppState>,
    dbc: DbcState,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        let result = match channel.lock() {
            Ok(ch) => ch.receive(),
            Err(_) => break,
        };

        match result {
            Ok(Some(frame)) => {
                let ts = frame.timestamp_ms;
                let _ = state.app.emit(
                    "can-frame",
                    CanFrameEvent {
                        channel_id: info.id.clone(),
                        can_id: frame.can_id,
                        is_extended: frame.is_extended,
                        dlc: frame.data.len() as u8,
                        data: frame.data.clone(),
                        timestamp_ms: ts,
                        direction: "rx",
                    },
                );
                if let Ok(guard) = dbc.read() {
                    if let Some(channel_dbc) = guard.get(&info.id) {
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
                                let _ = state.app.emit(
                                    "signal-value",
                                    SignalValueEvent {
                                        channel_id: info.id.clone(),
                                        signal_name: sig.name.clone(),
                                        message_name: sig.message_name.clone(),
                                        value,
                                        unit: sig.unit.clone(),
                                        timestamp_ms: ts,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!("CAN read error on '{}': {e}", info.id);
                break;
            }
        }
    }

    if let Ok(mut ch) = channel.lock() {
        let _ = ch.close();
    }
}
