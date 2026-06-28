use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::app_state::AppState;
use crate::can_communication::{Can, CanFrame};
use crate::dbc_parser::{encode, ParsedDbc};

#[cfg(feature = "kvaser")]
use crate::can_communication::KvaserBackend;
#[cfg(feature = "linux-can")]
use crate::can_communication::SocketCanBackend;

use log::{debug, error, info, warn};

const DEFAULT_WINDOW_MS: u64 = 30_000;

pub type ManagerState = Arc<Mutex<CanManager>>;

static NEXT_CHANNEL_HANDLE: AtomicU32 = AtomicU32::new(1);

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
    channel_handle: u32,
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
    pub channel_handle: u32,
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
    pub backend: String,
    pub name: String,
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
    fn new(info: ChannelInfo) -> Self {
        Self {
            frames: VecDeque::new(),
            signals: HashMap::new(),
            dbc: None,
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

    fn get_frames(&self, handle: u32, limit: usize) -> Vec<FrameInfo> {
        let skip = self.frames.len().saturating_sub(limit);
        self.frames
            .iter()
            .skip(skip)
            .map(|f| FrameInfo {
                channel_handle: handle,
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
    channels: HashMap<u32, ChannelData>,
    /// (backend_name, hw_index) → channel handle
    index_to_handle: HashMap<(String, u8), u32>,
    /// channel handle → (backend_name, hw_index)
    handle_to_index: HashMap<u32, (String, u8)>,
}

// ── CanManager ────────────────────────────────────────────────────────────────

pub struct CanManager {
    app_state: Arc<AppState>,
    shared: Arc<Mutex<ManagerShared>>,
    cans: HashMap<String, Can>,
}

impl CanManager {
    pub fn new(app_state: Arc<AppState>) -> Self {
        let shared = Arc::new(Mutex::new(ManagerShared {
            app: app_state.app.clone(),
            window_ms: DEFAULT_WINDOW_MS,
            channels: HashMap::new(),
            index_to_handle: HashMap::new(),
            handle_to_index: HashMap::new(),
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
            let sh_rx = Arc::clone(&shared);
            let sh_tx = Arc::clone(&shared);
            cans.insert(
                "socketcan".to_string(),
                Can::new(
                    SocketCanBackend,
                    make_rx_callback("socketcan".into(), sh_rx),
                    make_tx_callback("socketcan".into(), sh_tx),
                ),
            );
        }

        Self {
            app_state,
            shared,
            cans,
        }
    }

    // ── Channel lifecycle ─────────────────────────────────────────────────────

    /// List hardware channels available on all backends. The returned `ChannelInfo`
    /// items have `handle: 0` — they are not yet registered.
    pub fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let mut out = Vec::new();
        for (backend_name, can) in &self.cans {
            for ch in can.list_channels() {
                out.push(ChannelInfo {
                    backend: backend_name.clone(),
                    name: ch,
                });
            }
        }
        Ok(out)
    }

    /// Register a channel (allocate data stores) without opening the hardware.
    /// Returns a `u32` handle used for all subsequent calls. The DBC is loaded
    /// later by `open_channel`. Calling `create_channel` again for the same
    /// channel returns the existing handle.
    pub fn create_channel(
        &mut self,
        backend_name: &str,
        channel_name: &str,
    ) -> Result<u32, String> {
        let hw_index =
            self.cans
                .get(backend_name)
                .ok_or_else(|| format!("No backend '{backend_name}'"))?
                .list_channels()
                .iter()
                .position(|n| n == channel_name)
                .ok_or_else(|| format!("Channel '{channel_name}' not found in '{backend_name}'"))? as u8;

        let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;

        // Already registered — return existing handle.
        if let Some(&handle) = lock.index_to_handle.get(&(backend_name.to_string(), hw_index)) {
            return Ok(handle);
        }

        let handle = NEXT_CHANNEL_HANDLE.fetch_add(1, Ordering::Relaxed);
        let info = ChannelInfo {
            backend: backend_name.to_string(),
            name: channel_name.to_string(),
        };
        lock.index_to_handle
            .insert((backend_name.to_string(), hw_index), handle);
        lock.handle_to_index
            .insert(handle, (backend_name.to_string(), hw_index));
        lock.channels.insert(handle, ChannelData::new(info));
        info!("Created {backend_name} channel {channel_name} (handle: {handle})");
        Ok(handle)
    }

    pub fn remove_channel(&mut self, handle: u32) -> Result<(), String> {
        let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;

        let backend_name = lock
            .handle_to_index
            .get(&handle)
            .ok_or_else(|| format!("channel handle {handle} not found"))?
            .0
            .clone();

        let hw_index = lock
            .handle_to_index
            .get(&handle)
            .ok_or_else(|| format!("channel handle {handle} not found"))?
            .1;

        let can = self
            .cans
            .get_mut(&backend_name)
            .ok_or("Backend not found")?;

        if can.is_open(hw_index) {
            return Err(format!(
                "Cannot remove channel {handle} ({backend_name}:{hw_index}) while it is open"
            ));
        }

        lock.index_to_handle
            .remove(&(backend_name.clone(), hw_index));
        lock.handle_to_index.remove(&handle);
        lock.channels.remove(&handle);

        info!("Removed {backend_name} channel {hw_index} (handle: {handle})");

        Ok(())
    }

    /// Open the hardware for a channel registered with `create_channel`, loading
    /// the given DBC (parsed fresh from disk) for decode/encode. Returns the
    /// parsed DBC so the frontend can populate its signal tree.
    pub fn open_channel(
        &mut self,
        handle: u32,
        bitrate: u32,
        dbc_path: Option<&str>,
    ) -> Result<Option<ParsedDbc>, String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;

        // Parse the DBC before touching hardware so a bad path fails fast.
        let dbc = dbc_path.map(|p| ParsedDbc::new(p).map(Arc::new)).transpose()?;

        {
            let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            if let Some(ch) = lock.channels.get_mut(&handle) {
                ch.frames.clear();
                ch.signals.clear();
                ch.dbc = dbc.clone();
            }
        }

        let can = self.cans.get_mut(&backend_name).ok_or("Backend not found")?;
        match can.open(hw_index, bitrate, None) {
            Ok(()) => {}
            Err(crate::can_communication::CanOpenError::PasswordRequired) => {
                info!("Backend '{backend_name}' requires admin password to open channel {handle}");
                let pw = self.app_state.get_admin_password()?;
                can.open(hw_index, bitrate, Some(&pw)).map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }

        Ok(dbc.map(|d| (*d).clone()))
    }

    pub fn close_channel(&mut self, handle: u32) -> Result<(), String> {
        let (backend_name, hw_index) = {
            let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            let (bn, idx) = lock
                .handle_to_index
                .remove(&handle)
                .ok_or_else(|| format!("channel handle {handle} not found"))?;
            (bn, idx)
        };
        self.cans
            .get_mut(&backend_name)
            .ok_or("Backend not found")?
            .close(hw_index)
    }

    pub fn created_channels_info(&self) -> Vec<ChannelInfo> {
        self.shared
            .lock()
            .map(|l| l.channels.values().map(|c| c.info.clone()).collect())
            .unwrap_or_default()
    }

    // ── Send ──────────────────────────────────────────────────────────────────

    pub fn send_frame(&self, handle: u32, can_id: u32, data: Vec<u8>) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans.get(&backend_name).ok_or("Backend not found")?.send_once(
            hw_index,
            CanFrame {
                can_id,
                is_extended: can_id > 0x7FF,
                data,
            },
        )
    }

    pub fn send_message(&self, handle: u32, msg_id: u32, signal_values: &HashMap<String, f64>) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        let dbc = {
            let lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            lock.channels
                .get(&handle)
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
        self.cans.get(&backend_name).ok_or("Backend not found")?.send_once(
            hw_index,
            CanFrame {
                can_id: msg_id,
                is_extended: msg_id > 0x7FF,
                data: buf,
            },
        )
    }

    pub fn add_periodic_frame(&self, handle: u32, frame: CanFrame, period_ms: u64) -> Result<u64, String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .add_periodic(hw_index, frame, period_ms)
    }

    pub fn remove_periodic(&self, handle: u32, periodic_handle: u64) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .remove_periodic(hw_index, periodic_handle)
    }

    pub fn add_periodic_message(
        &self,
        handle: u32,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        period_ms: u64,
    ) -> Result<u64, String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        let dbc = {
            let lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            lock.channels
                .get(&handle)
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
            CanFrame {
                can_id: msg_id,
                is_extended: msg_id > 0x7FF,
                data: buf,
            },
            period_ms,
        )
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub fn get_frames(&self, handle: Option<u32>, limit: usize) -> Vec<FrameInfo> {
        let lock = match self.shared.lock() {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        if let Some(h) = handle {
            lock.channels
                .get(&h)
                .map(|c| c.get_frames(h, limit))
                .unwrap_or_default()
        } else {
            let mut all: Vec<FrameInfo> = lock
                .channels
                .iter()
                .flat_map(|(&h, c)| c.get_frames(h, limit))
                .collect();
            all.sort_unstable_by_key(|f| f.timestamp_ms);
            let skip = all.len().saturating_sub(limit);
            all[skip..].to_vec()
        }
    }

    pub fn get_signal_history(&self, handle: u32, signal_name: &str, since_ms: u64) -> Vec<SignalSample> {
        match self.shared.lock() {
            Ok(lock) => lock
                .channels
                .get(&handle)
                .map(|c| c.get_signal_history(signal_name, since_ms))
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
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

    fn backend_index(&self, handle: u32) -> Result<(String, u8), String> {
        self.shared
            .lock()
            .map_err(|_| "Lock poisoned".to_string())?
            .handle_to_index
            .get(&handle)
            .cloned()
            .ok_or_else(|| format!("channel handle {handle} not found; call create_channel first"))
    }

    fn channel_info(&self, handle: u32) -> Result<ChannelInfo, String> {
        self.shared
            .lock()
            .map_err(|_| "Lock poisoned".to_string())?
            .channels
            .get(&handle)
            .map(|ch| ch.info.clone())
            .ok_or_else(|| format!("channel handle {handle} not found"))
    }

    fn get_admin_password(&self) -> Result<String, String> {
        #[cfg(target_os = "linux")]
        return self.app_state.get_admin_password();
        #[cfg(not(target_os = "linux"))]
        Err("Administrator privileges are not supported on this platform".to_string())
    }
}

// ── Callback factories ────────────────────────────────────────────────────────

fn make_rx_callback(
    backend: String,
    shared: Arc<Mutex<ManagerShared>>,
) -> impl Fn(u8, CanFrame) + Send + Sync + 'static {
    move |hw_index, raw| {
        let ts = now_ms();

        // Hold the lock for the minimum time needed to decode and update state,
        // collecting events to emit after releasing.
        let (app, frame_event) = {
            let mut lock = match shared.lock() {
                Ok(l) => l,
                Err(_) => return,
            };

            let handle = match lock.index_to_handle.get(&(backend.clone(), hw_index)).cloned() {
                Some(h) => h,
                None => return,
            };

            let window_ms = lock.window_ms;
            let dbc = match lock.channels.get(&handle) {
                Some(ch) => ch.dbc.clone(),
                None => return,
            };

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
                .get_mut(&handle)
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
                    channel_handle: handle,
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
) -> impl Fn(u8, CanFrame) + Send + Sync + 'static {
    move |hw_index, raw| {
        let ts = now_ms();
        let (app, frame_event) = {
            let mut lock = match shared.lock() {
                Ok(l) => l,
                Err(_) => return,
            };
            let handle = match lock.index_to_handle.get(&(backend.clone(), hw_index)).cloned() {
                Some(h) => h,
                None => return,
            };
            let window_ms = lock.window_ms;
            let dbc = match lock.channels.get(&handle) {
                Some(ch) => ch.dbc.clone(),
                None => return,
            };
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
                .get_mut(&handle)
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
                    channel_handle: handle,
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
