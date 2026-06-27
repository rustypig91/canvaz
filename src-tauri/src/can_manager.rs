use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::app_state::AppState;
use crate::can_communication::{Can, CanFrame as RawFrame};
use crate::dbc_parser::{encode, ParsedDbc};

#[cfg(feature = "kvaser")]
use crate::can_communication::KvaserBackend;
#[cfg(feature = "linux-can")]
use crate::can_communication::SocketCanBackend;

use log::{debug, error, info, warn};

const DEFAULT_WINDOW_MS: u64 = 30_000;

pub type ManagerState = Arc<Mutex<CanManager>>;

// ── Public event / query types ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct DecodedSignal {
    name: String,
    message_name: String,
    value: f64,
    unit: String,
    min: f64,
    max: f64,
}

#[derive(Debug, Clone, Serialize)]
struct CanFrameEvent {
    channel_id: String,
    can_id: u32,
    is_extended: bool,
    dlc: u8,
    data: Vec<u8>,
    timestamp_ms: u64,
    direction: &'static str,
    message_name: Option<String>,
    signals: Vec<DecodedSignal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameInfo {
    pub channel_id: String,
    pub can_id: u32,
    pub is_extended: bool,
    pub dlc: u8,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
    pub direction: &'static str,
    pub message_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SignalSample {
    pub timestamp_ms: u64,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub backend: String,
    pub name: String,
    pub dbc: Option<ParsedDbc>,
}

// ── Internal signal stats ──────────────────────────────────────────────────────

#[derive(Clone)]
struct SignalStats {
    current: f64,
    min: f64,
    max: f64,
}

impl SignalStats {
    fn new(v: f64) -> Self {
        Self {
            current: v,
            min: v,
            max: v,
        }
    }

    fn update(&mut self, v: f64) {
        self.current = v;
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
    }

    fn held_extreme(&self, v: f64) -> bool {
        (v - self.min).abs() <= f64::EPSILON || (v - self.max).abs() <= f64::EPSILON
    }
}

// ── Internal frame storage ────────────────────────────────────────────────────

#[derive(Clone)]
struct StoredSignal {
    name: String,
    value: f64,
    unit: String,
}

#[derive(Clone)]
struct StoredFrame {
    can_id: u32,
    is_extended: bool,
    data: Vec<u8>,
    timestamp_ms: u64,
    direction: &'static str,
    message_name: Option<String>,
    signals: Vec<StoredSignal>,
}

// ── Per-channel data ──────────────────────────────────────────────────────────

struct ChannelData {
    frames: VecDeque<StoredFrame>,
    signals: HashMap<String, SignalStats>,
    dbc: Option<Arc<ParsedDbc>>,
    info: ChannelInfo,
}

impl ChannelData {
    fn new(info: ChannelInfo, dbc: Option<Arc<ParsedDbc>>) -> Self {
        Self {
            frames: VecDeque::new(),
            signals: HashMap::new(),
            dbc,
            info,
        }
    }

    /// Push a frame, evict frames older than `window_ms`, rescan signal extremes
    /// if evicted frames held the current min or max. Returns events to emit:
    /// (signal_name, value, unit, message_name, min, max).
    fn push(&mut self, frame: StoredFrame, window_ms: u64) -> Vec<(String, f64, String, String, f64, f64)> {
        let cutoff = frame.timestamp_ms.saturating_sub(window_ms);
        let mut needs_rescan: HashSet<String> = HashSet::new();

        while self.frames.front().map_or(false, |f| f.timestamp_ms < cutoff) {
            let evicted = self.frames.pop_front().unwrap();
            for sig in &evicted.signals {
                if let Some(stats) = self.signals.get(&sig.name) {
                    if stats.held_extreme(sig.value) {
                        needs_rescan.insert(sig.name.clone());
                    }
                }
            }
        }

        // Push new frame first so rescans include it.
        self.frames.push_back(frame.clone());

        for sig_name in &needs_rescan {
            self.rescan(sig_name);
        }

        // Update stats and collect events for new frame's signals.
        let msg_name = frame.message_name.clone().unwrap_or_default();
        let mut events = Vec::new();
        for sig in &frame.signals {
            self.signals
                .entry(sig.name.clone())
                .and_modify(|s| s.update(sig.value))
                .or_insert_with(|| SignalStats::new(sig.value));
            let s = &self.signals[&sig.name];
            events.push((
                sig.name.clone(),
                sig.value,
                sig.unit.clone(),
                msg_name.clone(),
                s.min,
                s.max,
            ));
        }
        events
    }

    fn rescan(&mut self, sig_name: &str) {
        let vals: Vec<f64> = self
            .frames
            .iter()
            .flat_map(|f| f.signals.iter())
            .filter(|s| s.name == sig_name)
            .map(|s| s.value)
            .collect();

        if let Some(stats) = self.signals.get_mut(sig_name) {
            if vals.is_empty() {
                stats.min = stats.current;
                stats.max = stats.current;
            } else {
                stats.min = vals.iter().copied().fold(f64::INFINITY, f64::min);
                stats.max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            }
        }
    }

    fn evict_before(&mut self, cutoff: u64) {
        let mut needs_rescan: HashSet<String> = HashSet::new();
        while self.frames.front().map_or(false, |f| f.timestamp_ms < cutoff) {
            let evicted = self.frames.pop_front().unwrap();
            for sig in &evicted.signals {
                if let Some(stats) = self.signals.get(&sig.name) {
                    if stats.held_extreme(sig.value) {
                        needs_rescan.insert(sig.name.clone());
                    }
                }
            }
        }
        for sig_name in &needs_rescan {
            self.rescan(sig_name);
        }
    }

    fn get_frames(&self, limit: usize) -> Vec<FrameInfo> {
        let skip = self.frames.len().saturating_sub(limit);
        self.frames
            .iter()
            .skip(skip)
            .map(|f| FrameInfo {
                channel_id: self.info.id.clone(),
                can_id: f.can_id,
                is_extended: f.is_extended,
                dlc: f.data.len() as u8,
                data: f.data.clone(),
                timestamp_ms: f.timestamp_ms,
                direction: f.direction,
                message_name: f.message_name.clone(),
            })
            .collect()
    }

    fn get_signal_history(&self, signal_name: &str, since_ms: u64) -> Vec<SignalSample> {
        self.frames
            .iter()
            .filter(|f| f.timestamp_ms >= since_ms)
            .filter_map(|f| {
                f.signals.iter().find(|s| s.name == signal_name).map(|s| SignalSample {
                    timestamp_ms: f.timestamp_ms,
                    value: s.value,
                })
            })
            .collect()
    }
}

// ── Shared state (accessed from RX/TX callbacks without holding the CanManager lock) ──

struct ManagerShared {
    app: AppHandle,
    window_ms: u64,
    channels: HashMap<String, ChannelData>,
    /// (backend_name, hw_index) → channel_id
    index_to_id: HashMap<(String, u8), String>,
    /// channel_id → (backend_name, hw_index)
    id_to_index: HashMap<String, (String, u8)>,
}

// ── CanManager ────────────────────────────────────────────────────────────────

pub struct CanManager {
    shared: Arc<Mutex<ManagerShared>>,
    cans: HashMap<String, Can>,
}

impl CanManager {
    pub fn new(app_state: Arc<AppState>) -> Self {
        let shared = Arc::new(Mutex::new(ManagerShared {
            app: app_state.app.clone(),
            window_ms: DEFAULT_WINDOW_MS,
            channels: HashMap::new(),
            index_to_id: HashMap::new(),
            id_to_index: HashMap::new(),
        }));

        let mut cans: HashMap<String, Can> = HashMap::new();

        #[cfg(feature = "kvaser")]
        {
            let sh_rx = Arc::clone(&shared);
            let sh_tx = Arc::clone(&shared);
            cans.insert(
                "kvaser".to_string(),
                Can::new(
                    KvaserBackend,
                    make_rx_callback("kvaser".into(), sh_rx),
                    make_tx_callback("kvaser".into(), sh_tx),
                ),
            );
        }

        #[cfg(feature = "linux-can")]
        {
            #[cfg(target_os = "linux")]
            let get_pw = {
                let s = Arc::clone(&app_state);
                move || s.get_sudo_password()
            };
            #[cfg(not(target_os = "linux"))]
            let get_pw = || -> Result<String, String> { Err("sudo not supported".to_string()) };

            let sh_rx = Arc::clone(&shared);
            let sh_tx = Arc::clone(&shared);
            cans.insert(
                "socketcan".to_string(),
                Can::new(
                    SocketCanBackend::new(get_pw),
                    make_rx_callback("socketcan".into(), sh_rx),
                    make_tx_callback("socketcan".into(), sh_tx),
                ),
            );
        }

        Self { shared, cans }
    }

    // ── Channel lifecycle ─────────────────────────────────────────────────────

    pub fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let mut out = Vec::new();
        for (backend_name, can) in &self.cans {
            for ch in can.list_channels() {
                out.push(ChannelInfo {
                    id: format!("{backend_name}:{ch}"),
                    backend: backend_name.clone(),
                    name: ch,
                    dbc: None,
                });
            }
        }
        Ok(out)
    }

    pub fn open_channel(
        &mut self,
        backend_name: String,
        channel_name: String,
        bitrate: u32,
        dbc_path: Option<&str>,
    ) -> Result<ChannelInfo, String> {
        let id = format!("{backend_name}:{channel_name}");

        // If already open: close and reopen with new bitrate/DBC.
        let existing_index = self
            .shared
            .lock()
            .map_err(|_| "Lock poisoned".to_string())?
            .id_to_index
            .get(&id)
            .map(|(bn, idx)| (bn.clone(), *idx));

        if let Some((bn, idx)) = existing_index {
            let can = self.cans.get_mut(&bn).ok_or("Backend not found")?;
            can.close(idx)?;
            // Reload DBC and clear stores
            let new_dbc = dbc_path.map(|p| ParsedDbc::new(p).map(Arc::new)).transpose()?;
            {
                let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
                if let Some(ch) = lock.channels.get_mut(&id) {
                    ch.frames.clear();
                    ch.signals.clear();
                    if let Some(d) = new_dbc {
                        ch.dbc = Some(d);
                        ch.info.dbc = ch.dbc.as_deref().cloned();
                    }
                }
            }
            can.open(idx, bitrate)?;
            return Ok(self.shared.lock().unwrap().channels[&id].info.clone());
        }

        // New channel: find index in the backend's channel list.
        let can = self
            .cans
            .get_mut(&backend_name)
            .ok_or_else(|| format!("No backend '{backend_name}'"))?;
        let hw_index =
            can.list_channels()
                .iter()
                .position(|n| n == &channel_name)
                .ok_or_else(|| format!("Channel '{channel_name}' not found in '{backend_name}'"))? as u8;

        let dbc = dbc_path.map(|p| ParsedDbc::new(p).map(Arc::new)).transpose()?;
        let info = ChannelInfo {
            id: id.clone(),
            backend: backend_name.clone(),
            name: channel_name,
            dbc: dbc.as_deref().cloned(),
        };

        can.open(hw_index, bitrate)?;

        {
            let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            lock.index_to_id.insert((backend_name.clone(), hw_index), id.clone());
            lock.id_to_index.insert(id.clone(), (backend_name, hw_index));
            lock.channels.insert(id.clone(), ChannelData::new(info.clone(), dbc));
        }
        Ok(info)
    }

    pub fn close_channel(&mut self, channel_id: &str) -> Result<(), String> {
        let (backend_name, hw_index) = {
            let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            let (bn, idx) = lock
                .id_to_index
                .remove(channel_id)
                .ok_or_else(|| format!("'{channel_id}' is not open"))?;
            lock.index_to_id.remove(&(bn.clone(), idx));
            lock.channels.remove(channel_id);
            (bn, idx)
        };
        self.cans
            .get_mut(&backend_name)
            .ok_or("Backend not found")?
            .close(hw_index)
    }

    pub fn open_channels_info(&self) -> Vec<ChannelInfo> {
        self.shared
            .lock()
            .map(|l| l.channels.values().map(|c| c.info.clone()).collect())
            .unwrap_or_default()
    }

    // ── Send ──────────────────────────────────────────────────────────────────

    pub fn send_raw(&self, channel_id: &str, can_id: u32, data: Vec<u8>) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(channel_id)?;
        let can = self.cans.get(&backend_name).ok_or("Backend not found")?;
        can.send_once(
            hw_index,
            RawFrame {
                can_id,
                is_extended: can_id > 0x7FF,
                data,
            },
        )
    }

    pub fn send_message(
        &self,
        channel_id: &str,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
    ) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(channel_id)?;

        let dbc = {
            let lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            lock.channels
                .get(channel_id)
                .and_then(|c| c.dbc.clone())
                .ok_or_else(|| "No DBC loaded for this channel".to_string())?
        };

        let msg = dbc
            .messages
            .iter()
            .find(|m| m.id == msg_id)
            .ok_or_else(|| format!("Message 0x{:X} not in DBC", msg_id))?;

        let mut buf = vec![0u8; msg.dlc as usize];
        for sig in &msg.signals {
            if let Some(&v) = signal_values.get(&sig.name) {
                encode(
                    &mut buf,
                    v,
                    sig.start_bit,
                    sig.length,
                    sig.little_endian,
                    sig.factor,
                    sig.offset,
                );
            }
        }

        let can = self.cans.get(&backend_name).ok_or("Backend not found")?;
        can.send_once(
            hw_index,
            RawFrame {
                can_id: msg_id,
                is_extended: msg_id > 0x7FF,
                data: buf,
            },
        )
    }

    pub fn add_periodic_frame(&self, channel_id: &str, frame: RawFrame, period_ms: u64) -> Result<u64, String> {
        let (backend_name, hw_index) = self.backend_index(channel_id)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .add_periodic(hw_index, frame, period_ms)
    }

    pub fn remove_periodic(&self, channel_id: &str, handle: u64) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(channel_id)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .remove_periodic(hw_index, handle)
    }

    pub fn add_periodic_message(
        &self,
        channel_id: &str,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        period_ms: u64,
    ) -> Result<u64, String> {
        let (backend_name, hw_index) = self.backend_index(channel_id)?;
        let dbc = {
            let lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            lock.channels
                .get(channel_id)
                .and_then(|c| c.dbc.clone())
                .ok_or_else(|| "No DBC loaded for this channel".to_string())?
        };
        let msg = dbc
            .messages
            .iter()
            .find(|m| m.id == msg_id)
            .ok_or_else(|| format!("Message 0x{:X} not in DBC", msg_id))?;
        let mut buf = vec![0u8; msg.dlc as usize];
        for sig in &msg.signals {
            if let Some(&v) = signal_values.get(&sig.name) {
                encode(
                    &mut buf,
                    v,
                    sig.start_bit,
                    sig.length,
                    sig.little_endian,
                    sig.factor,
                    sig.offset,
                );
            }
        }
        self.cans.get(&backend_name).ok_or("Backend not found")?.add_periodic(
            hw_index,
            RawFrame {
                can_id: msg_id,
                is_extended: msg_id > 0x7FF,
                data: buf,
            },
            period_ms,
        )
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub fn get_frames(&self, channel_id: Option<&str>, limit: usize) -> Vec<FrameInfo> {
        let lock = match self.shared.lock() {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        if let Some(id) = channel_id {
            lock.channels.get(id).map(|c| c.get_frames(limit)).unwrap_or_default()
        } else {
            let mut all: Vec<FrameInfo> = lock.channels.values().flat_map(|c| c.get_frames(limit)).collect();
            all.sort_unstable_by_key(|f| f.timestamp_ms);
            let skip = all.len().saturating_sub(limit);
            all[skip..].to_vec()
        }
    }

    pub fn get_signal_history(&self, channel_id: &str, signal_name: &str, since_ms: u64) -> Vec<SignalSample> {
        let lock = match self.shared.lock() {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        lock.channels
            .get(channel_id)
            .map(|c| c.get_signal_history(signal_name, since_ms))
            .unwrap_or_default()
    }

    pub fn set_window_ms(&self, ms: u64) -> Result<(), String> {
        let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
        lock.window_ms = ms;
        let cutoff = now_ms().saturating_sub(ms);
        for ch in lock.channels.values_mut() {
            ch.evict_before(cutoff);
        }
        Ok(())
    }

    fn backend_index(&self, channel_id: &str) -> Result<(String, u8), String> {
        self.shared
            .lock()
            .map_err(|_| "Lock poisoned".to_string())?
            .id_to_index
            .get(channel_id)
            .cloned()
            .ok_or_else(|| format!("'{channel_id}' is not open"))
    }
}

// ── Callback factories ────────────────────────────────────────────────────────

fn make_rx_callback(
    backend: String,
    shared: Arc<Mutex<ManagerShared>>,
) -> impl Fn(u8, RawFrame) + Send + Sync + 'static {
    move |hw_index, raw| {
        let ts = now_ms();

        // Hold the lock for the minimum time needed to decode and update state,
        // collecting events to emit after releasing.
        let (app, frame_event) = {
            let mut lock = match shared.lock() {
                Ok(l) => l,
                Err(_) => return,
            };

            let channel_id = match lock.index_to_id.get(&(backend.clone(), hw_index)).cloned() {
                Some(id) => id,
                None => return,
            };

            let window_ms = lock.window_ms;
            let dbc = lock.channels.get(&channel_id).and_then(|c| c.dbc.clone());

            let signals = dbc
                .as_ref()
                .and_then(|d| decode_frame(d, raw.can_id, &raw.data))
                .unwrap_or_default();

            let message_name = signals.first().map(|_| {
                dbc.as_ref()
                    .and_then(|d| d.messages.iter().find(|m| m.id == raw.can_id))
                    .map(|m| m.name.clone())
                    .unwrap_or_default()
            });

            let stored = StoredFrame {
                can_id: raw.can_id,
                is_extended: raw.is_extended,
                data: raw.data.clone(),
                timestamp_ms: ts,
                direction: "rx",
                message_name: message_name.clone(),
                signals: signals
                    .iter()
                    .map(|(n, v, u)| StoredSignal {
                        name: n.clone(),
                        value: *v,
                        unit: u.clone(),
                    })
                    .collect(),
            };

            let signal_updates = lock
                .channels
                .get_mut(&channel_id)
                .map(|ch| ch.push(stored, window_ms))
                .unwrap_or_default();

            let app = lock.app.clone();
            let decoded: Vec<DecodedSignal> = signal_updates
                .into_iter()
                .map(|(sig_name, value, unit, msg_name, min, max)| DecodedSignal {
                    name: sig_name,
                    message_name: msg_name,
                    value,
                    unit,
                    min,
                    max,
                })
                .collect();

            (
                app,
                CanFrameEvent {
                    channel_id,
                    can_id: raw.can_id,
                    is_extended: raw.is_extended,
                    dlc: raw.data.len() as u8,
                    data: raw.data,
                    timestamp_ms: ts,
                    direction: "rx",
                    message_name,
                    signals: decoded,
                },
            )
        };

        let _ = app.emit("can-frame", &frame_event);
    }
}

fn make_tx_callback(
    backend: String,
    shared: Arc<Mutex<ManagerShared>>,
) -> impl Fn(u8, RawFrame) + Send + Sync + 'static {
    move |hw_index, raw| {
        let ts = now_ms();
        let (app, frame_event) = {
            let mut lock = match shared.lock() {
                Ok(l) => l,
                Err(_) => return,
            };
            let channel_id = match lock.index_to_id.get(&(backend.clone(), hw_index)).cloned() {
                Some(id) => id,
                None => return,
            };
            let window_ms = lock.window_ms;
            let dbc = lock.channels.get(&channel_id).and_then(|c| c.dbc.clone());
            let raw_signals = dbc
                .as_ref()
                .and_then(|d| decode_frame(d, raw.can_id, &raw.data))
                .unwrap_or_default();
            let message_name = raw_signals.first().and_then(|_| {
                dbc.as_ref()
                    .and_then(|d| d.messages.iter().find(|m| m.id == raw.can_id))
                    .map(|m| m.name.clone())
            });
            let stored = StoredFrame {
                can_id: raw.can_id,
                is_extended: raw.is_extended,
                data: raw.data.clone(),
                timestamp_ms: ts,
                direction: "tx",
                message_name: message_name.clone(),
                signals: raw_signals
                    .iter()
                    .map(|(n, v, u)| StoredSignal {
                        name: n.clone(),
                        value: *v,
                        unit: u.clone(),
                    })
                    .collect(),
            };
            let signal_updates = lock
                .channels
                .get_mut(&channel_id)
                .map(|ch| ch.push(stored, window_ms))
                .unwrap_or_default();
            let app = lock.app.clone();
            let decoded: Vec<DecodedSignal> = signal_updates
                .into_iter()
                .map(|(sig_name, value, unit, msg_name, min, max)| DecodedSignal {
                    name: sig_name,
                    message_name: msg_name,
                    value,
                    unit,
                    min,
                    max,
                })
                .collect();
            (
                app,
                CanFrameEvent {
                    channel_id,
                    can_id: raw.can_id,
                    is_extended: raw.is_extended,
                    dlc: raw.data.len() as u8,
                    data: raw.data,
                    timestamp_ms: ts,
                    direction: "tx",
                    message_name,
                    signals: decoded,
                },
            )
        };
        let _ = app.emit("can-frame", &frame_event);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Decode all signals in a frame. Returns (signal_name, physical_value, unit) tuples.
fn decode_frame(dbc: &ParsedDbc, can_id: u32, data: &[u8]) -> Option<Vec<(String, f64, String)>> {
    let msg = dbc.messages.iter().find(|m| m.id == can_id)?;
    let signals = msg
        .signals
        .iter()
        .map(|sig| {
            let v = crate::dbc_parser::decode(
                data,
                sig.start_bit,
                sig.length,
                sig.little_endian,
                sig.signed,
                sig.factor,
                sig.offset,
            );
            (sig.name.clone(), v, sig.unit.clone())
        })
        .collect();
    Some(signals)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
