mod backends;

use backends::{CanBackend, CanChannel, CanFrame, ChannelInfo, DbcState};

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, RwLock,
};

use serde::Serialize;

// ── Channel info ──────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize)]
pub struct ChannelInfo {
    pub backend: String,
    pub name: String,
}

impl ChannelInfo {
    pub fn id(&self) -> String {
        format!("{}:{}", self.backend, self.name)
    }
}

// ── Wire events ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CanFrameEvent {
    pub channel: ChannelInfo,
    pub can_id: u32,
    pub is_extended: bool,
    pub dlc: u8,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
    pub direction: &'static str,
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

// ── Manager ───────────────────────────────────────────────────────────────────
struct OpenChannelState {
    stop_flag: Arc<AtomicBool>,
    channel: Arc<Mutex<Box<dyn CanChannel>>>,
    backend_name: String,
}

pub struct CanManager {
    state: Arc<AppState>,
    backends: Vec<Arc<Mutex<dyn CanBackend>>>,
    channels: HashMap<String, OpenChannelState>,
}

impl CanManager {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            backends: Vec::new(),
            channels: HashMap::new(),
        }
    }

    pub fn register_backend(&mut self, backend: impl CanBackend) {
        self.backends.push(Arc::new(Mutex::new(backend)));
    }

    pub fn list_channels(&self) -> Vec<ChannelInfo> {
        let mut result = Vec::new();
        for backend in &self.backends {
            if let Ok(b) = backend.lock() {
                let bname = b.name().to_string();
                for ch in b.list_channels() {
                    result.push(ChannelInfo {
                        backend: bname.clone(),
                        name: ch,
                    });
                }
            }
        }
        result
    }

    pub fn open_channel(
        &mut self,
        backend_name: String,
        channel_name: String,
        bitrate: Option<u32>,
        dbc: DbcState,
    ) -> Result<ChannelInfo, String> {
        let channel_info = ChannelInfo {
            backend: backend_name,
            name: channel_name,
        };
        let channel_id = channel_info.id();

        if self.channels.contains_key(&channel_id) {
            return Err(format!("Channel '{}' is already open", channel_id));
        }

        let backend = self
            .backends
            .iter()
            .find_map(|b| {
                let g = b.lock().ok()?;
                if g.name() == channel_info.backend {
                    Some(Arc::clone(b))
                } else {
                    None
                }
            })
            .ok_or_else(|| format!("No backend named '{}'", channel_info.backend))?;

        let mut channel = {
            let mut b = backend
                .lock()
                .map_err(|_| "Backend lock poisoned".to_string())?;
            b.open_channel(channel_info.name.as_str(), bitrate, Arc::clone(&self.state))?
        };

        channel.open()?;

        let channel = Arc::new(Mutex::new(channel));
        let stop_flag = Arc::new(AtomicBool::new(false));

        std::thread::spawn({
            let channel = Arc::clone(&channel);
            let stop = Arc::clone(&stop_flag);
            let ch_id = channel_info.clone();
            let state = Arc::clone(&self.state);
            move || reading_loop(channel, ch_id, state, dbc, stop)
        });

        self.channels.insert(
            channel_id.clone(),
            OpenChannelState {
                stop_flag,
                channel,
                backend_name: channel_info.backend.clone(),
            },
        );
        Ok(channel_info)
    }

    pub fn close_channel(&mut self, channel_id: &str) -> Result<(), String> {
        match self.channels.remove(channel_id) {
            Some(state) => {
                state.stop_flag.store(true, Ordering::Relaxed);
                Ok(())
            }
            None => Err(format!("Channel '{channel_id}' is not open")),
        }
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

    pub fn send_frame(&self, channel_id: &str, frame: CanFrame) -> Result<(), String> {
        let state = self
            .channels
            .get(channel_id)
            .ok_or_else(|| format!("Channel '{channel_id}' is not open"))?;
        let ch = state
            .channel
            .lock()
            .map_err(|_| "Channel lock poisoned".to_string())?;
        ch.send(frame)
    }
}

// ── Reading thread ────────────────────────────────────────────────────────────

fn reading_loop(
    channel: Arc<Mutex<Box<dyn CanChannel>>>,
    channel_info: ChannelInfo,
    state: Arc<AppState>,
    dbc: DbcState,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
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
                let _ = state.app.emit(
                    "can-frame",
                    CanFrameEvent {
                        channel: channel_info.clone(),
                        can_id: frame.can_id,
                        is_extended: frame.is_extended,
                        dlc: frame.data.len() as u8,
                        data: frame.data.clone(),
                        timestamp_ms: ts,
                        direction: "rx",
                    },
                );
                if let Ok(guard) = dbc.read() {
                    if let Some(channel_dbc) = guard.get(&channel_info) {
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
                                        channel: channel_id.clone(),
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
                log::warn!("CAN read error on '{}': {}", channel_id, e);
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
