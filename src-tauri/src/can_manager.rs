#[cfg(feature = "demo")]
mod demo;
mod recording;
pub use recording::RecordingStatus;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::app_state::AppState;
use crate::can_communication::{BusStatus, Can, CanFrame, ChannelCapabilities, ChannelSettings, FrameDataSource, FrameFormat, FrameMetadata, FrameUpdate, TxFormat};
use crate::dbc_parser::{message_key, ParsedDbc, ParsedMessage};
use crate::j1939::{self, J1939Info, TpReassembler};
use crate::sim_generator::{build_frame_source, SignalGen};

#[cfg(feature = "kvaser")]
use crate::can_communication::KvaserBackend;
#[cfg(feature = "pcan")]
use crate::can_communication::PcanBackend;
#[cfg(any(feature = "linux-can", target_os = "linux"))]
use crate::can_communication::SocketCanBackend;

use log::*;

const DEFAULT_WINDOW_MS: u64 = 30_000;

pub type ManagerState = Arc<Mutex<CanManager>>;

static NEXT_CHANNEL_HANDLE: AtomicU32 = AtomicU32::new(1);

// ── Public event / query types ────────────────────────────────────────────────

/// Per-channel protocol interpretation of received frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    None,
    J1939,
}

impl Protocol {
    pub fn from_config(s: Option<&str>) -> Self {
        match s {
            Some("j1939") => Protocol::J1939,
            _ => Protocol::None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct CanFrameEvent {
    channel_handle: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    socketcand_attempt: Option<u32>,
    can_id: u32,
    is_extended: bool,
    #[serde(flatten)]
    metadata: FrameMetadata,
    /// u16: reassembled J1939 transport messages carry up to 1785 data bytes.
    dlc: u16,
    data: Vec<u8>,
    timestamp_ms: u64,
    direction: &'static str,
    dbc_message_id: Option<u32>,
    /// J1939 breakdown of the identifier; only set on J1939 channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    j1939: Option<J1939Info>,
    /// True if this row is a synthetic frame reassembled from a J1939 TP.CM /
    /// TP.DT transfer rather than a frame that actually appeared on the bus.
    #[serde(skip_serializing_if = "is_false")]
    reassembled: bool,
    /// Bus error frame description; set only on error rows (direction "err").
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Decoded signals as interleaved [value, raw, value, raw, …] pairs in DBC
    /// message signal order. Names/units/message name are NOT sent: the frontend
    /// holds the same parsed DBC and derives them by position, which keeps the
    /// per-frame payload free of strings (they dominated IPC/GC churn at high
    /// frame rates). Raw values survive the f64 round-trip up to 2^53 — the same
    /// limit JSON numbers already imposed. Signals of inactive multiplexer
    /// groups are NaN pairs, which serde_json serializes as null — the frontend
    /// skips those, keeping positions aligned with the DBC signal list.
    signals: Vec<f64>,
}

fn is_false(b: &bool) -> bool {
    !b
}

/// Emitted as a "channel-error" webview event when a channel's RX loop dies
/// (`fatal: true` — the channel no longer receives) or a send fails
/// (`fatal: false`). Rare, so emitted directly rather than batched.
#[derive(Debug, Clone, Serialize)]
struct ChannelErrorEvent {
    channel_handle: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    socketcand_attempt: Option<u32>,
    error: String,
    fatal: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameInfo {
    pub channel_handle: u32,
    pub can_id: u32,
    pub is_extended: bool,
    #[serde(flatten)]
    pub metadata: FrameMetadata,
    pub dlc: u16,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
    pub direction: &'static str,
    pub message_name: Option<String>,
    pub dbc_message_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub j1939: Option<J1939Info>,
    /// True if this row is a synthetic frame reassembled from a J1939 TP.CM /
    /// TP.DT transfer rather than a frame that actually appeared on the bus.
    #[serde(skip_serializing_if = "is_false")]
    pub reassembled: bool,
    /// Bus error frame description; set only on error rows (direction "err").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub signals: Vec<FrameSignal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameSignal {
    pub name: String,
    pub value: f64,
    pub raw: i64,
    pub unit: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SignalSample {
    pub timestamp_ms: u64,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ChannelCapabilities>,
    pub backend: String,
    pub name: String,
}

/// Result of `create_channel`: the allocated handle plus the backend the
/// channel name was actually found in (which may differ from the hint).
#[derive(Debug, Clone, Serialize)]
pub struct CreatedChannel {
    pub capabilities: Option<ChannelCapabilities>,
    pub handle: u32,
    pub backend: String,
    pub available: bool,
}

/// One channel's live health/load numbers, returned by `get_bus_stats`.
/// Rates cover the interval since the previous `get_bus_stats` call.
#[derive(Debug, Clone, Serialize)]
pub struct BusStats {
    pub channel_handle: u32,
    /// Received data frames per second.
    pub rx_fps: f64,
    /// Transmitted frames per second.
    pub tx_fps: f64,
    /// Estimated bus load in percent (0–100), from frame sizes with an
    /// average stuff-bit allowance against the configured bitrate, when known.
    pub bus_load: Option<f64>,
    /// Total bus error frames seen during the current capture.
    pub error_frames: u64,
    /// Latest controller status from the backend; `None` until first report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<BusStatus>,
}

// ── Internal frame storage ────────────────────────────────────────────────────

#[derive(Clone)]
struct StoredSignal {
    name: String,
    value: f64,
    raw: i64,
    unit: String,
}

#[derive(Clone)]
struct StoredFrame {
    metadata: FrameMetadata,
    can_id: u32,
    is_extended: bool,
    data: Vec<u8>,
    timestamp_ms: u64,
    direction: &'static str,
    message_name: Option<String>,
    j1939: Option<J1939Info>,
    /// True if this row is a synthetic frame reassembled from a J1939 TP.CM /
    /// TP.DT transfer rather than a frame that actually appeared on the bus.
    reassembled: bool,
    /// Bus error frame description; set only on error rows (direction "err").
    error: Option<String>,
    /// DBC message id this frame decoded against. On J1939 channels it can
    /// differ from `can_id` (PGN match ignores priority/source-address bits);
    /// signal-history queries key on it.
    dbc_msg_id: Option<u32>,
    signals: Vec<StoredSignal>,
}

// ── Per-channel data ──────────────────────────────────────────────────────────

/// Cumulative traffic counters plus the latest backend status report. Reset
/// each time the channel is (re)opened.
#[derive(Default)]
struct ChannelStats {
    rx_frames: u64,
    tx_frames: u64,
    /// Estimated wire bits (RX + TX), for the bus-load estimate.
    wire_bits: u64,
    error_frames: u64,
    bus_status: Option<BusStatus>,
    /// Counter snapshot at the previous `get_bus_stats` call:
    /// (when, rx_frames, tx_frames, wire_bits). Rates are deltas against it.
    last_query: Option<(std::time::Instant, u64, u64, u64)>,
}

/// Estimated number of bits one frame occupies on the wire, including an
/// average stuff-bit allowance (~1 stuff bit per 5 stuffable bits; worst case
/// is 1 per 4). Standard frames carry 47 fixed bits (SOF through EOF + IFS) of
/// which 34 are stuffable; extended frames 67 fixed / 54 stuffable. All data
/// bits are stuffable.
fn estimate_frame_bits(dlc: usize, extended: bool) -> u64 {
    let data_bits = 8 * dlc.min(8) as u64;
    let (fixed, stuffable) = if extended { (67, 54 + data_bits) } else { (47, 34 + data_bits) };
    fixed + data_bits + stuffable / 5
}

struct ChannelData {
    frames: VecDeque<StoredFrame>,
    dbc: Option<Arc<ParsedDbc>>,
    info: ChannelInfo,
    /// User-chosen display name used in CSV exports; `info.name` stays the
    /// hardware identity used for lookup.
    display_name: Option<String>,
    protocol: Protocol,
    socketcand_attempt: Option<u32>,
    tp: TpReassembler,
    /// Set by the last `open_channel` call. When true, TX entry points refuse
    /// to send so the bus stays undisturbed rather than the frame silently
    /// getting dropped by the hardware's silent/listen-only mode.
    listen_only: bool,
    /// Bitrate from the last `open_channel` call; denominator of the bus-load
    /// estimate. 0 before opening or when the bitrate is unknown.
    bitrate: u32,
    stats: ChannelStats,
}

impl ChannelData {
    fn prepare_capture(&mut self, resume_capture: bool) {
        self.tp = TpReassembler::default();
        if resume_capture {
            // Keep history and cumulative counts, but restart rate sampling
            // and discard the last known controller status.
            self.stats.last_query = None;
            self.stats.bus_status = None;
        } else {
            self.frames.clear();
            self.stats = ChannelStats::default();
        }
    }

    fn new(info: ChannelInfo) -> Self {
        Self {
            frames: VecDeque::new(),
            dbc: None,
            info,
            display_name: None,
            protocol: Protocol::None,
            socketcand_attempt: None,
            tp: TpReassembler::default(),
            listen_only: false,
            bitrate: 0,
            stats: ChannelStats::default(),
        }
    }

    /// Name shown to the user: the custom display name when set, else the
    /// hardware name.
    fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(if self.info.backend == "none" { "Unassigned" } else { &self.info.name })
    }

    /// Push a frame, evicting frames older than `window_ms` first.
    fn push(&mut self, frame: StoredFrame, window_ms: u64) {
        let cutoff = frame.timestamp_ms.saturating_sub(window_ms);
        while self.frames.front().map_or(false, |f| f.timestamp_ms < cutoff) {
            self.frames.pop_front();
        }
        self.frames.push_back(frame);
    }

    fn evict_before(&mut self, cutoff: u64) {
        while self.frames.front().map_or(false, |f| f.timestamp_ms < cutoff) {
            self.frames.pop_front();
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
                metadata: f.metadata,
                dlc: f.data.len() as u16,
                j1939: f.j1939,
                reassembled: f.reassembled,
                error: f.error.clone(),
                data: f.data.clone(),
                timestamp_ms: f.timestamp_ms,
                direction: f.direction,
                message_name: f.message_name.clone(),
                dbc_message_id: f.dbc_msg_id,
                signals: f
                    .signals
                    .iter()
                    .map(|s| FrameSignal {
                        name: s.name.clone(),
                        value: s.value,
                        raw: s.raw,
                        unit: s.unit.clone(),
                    })
                    .collect(),
            })
            .collect()
    }

    fn get_signal_history(&self, can_id: u32, signal_name: &str, since_ms: u64) -> Vec<SignalSample> {
        self.frames
            .iter()
            // Match the decoded identity, not just the wire ID: standard and
            // extended frames may share an ID, and J1939 priority/DA may vary.
            .filter(|f| f.timestamp_ms >= since_ms && f.dbc_msg_id == Some(can_id))
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
    /// Frames waiting to be emitted to the frontend. RX/TX callbacks append here
    /// and a flusher thread drains it every FLUSH_INTERVAL_MS as one batched
    /// "can-frame-batch" event — one IPC message per tick instead of per frame.
    pending_events: Vec<CanFrameEvent>,
    recorder: recording::Recorder,
}

/// How often the pending frame-event buffer is flushed to the webview. 33 ms
/// (~30 fps) is below perception for live views while collapsing thousands of
/// per-frame IPC messages per second into at most ~30.
const FLUSH_INTERVAL_MS: u64 = 33;

// ── CanManager ────────────────────────────────────────────────────────────────

// BTreeMap so the fallback search in `create_channel` visits backends in a
// deterministic (alphabetical) order when a channel name exists in several.
fn build_cans(shared: &Arc<Mutex<ManagerShared>>) -> BTreeMap<String, Can> {
    let mut cans: BTreeMap<String, Can> = BTreeMap::new();
    // Demo builds never load vendor libraries or discover hardware.
    if cfg!(feature = "demo") { return cans; }

    #[cfg(feature = "kvaser")]
    match KvaserBackend::new() {
        Ok(backend) => {
            cans.insert(
                "kvaser".to_string(),
                Can::new(
                    backend,
                    make_frame_callback("kvaser".into(), Arc::clone(shared), "rx"),
                    make_frame_callback("kvaser".into(), Arc::clone(shared), "tx"),
                    make_error_callback("kvaser".into(), Arc::clone(shared)),
                    make_status_callback("kvaser".into(), Arc::clone(shared)),
                ),
            );
            info!("Kvaser backend initialized successfully");
        }
        Err(e) => warn!("Kvaser backend unavailable (install from https://www.kvaser.com/download/): {e}"),
    }

    #[cfg(feature = "pcan")]
    match PcanBackend::new() {
        Ok(backend) => {
            cans.insert(
                "pcan".to_string(),
                Can::new(
                    backend,
                    make_frame_callback("pcan".into(), Arc::clone(shared), "rx"),
                    make_frame_callback("pcan".into(), Arc::clone(shared), "tx"),
                    make_error_callback("pcan".into(), Arc::clone(shared)),
                    make_status_callback("pcan".into(), Arc::clone(shared)),
                ),
            );
            info!("PCAN backend initialized successfully");
        }
        Err(e) => warn!(
            "PCAN backend unavailable (install from https://www.peak-system.com/products/software/development-packages/pcan-basic/): {e}"
        ),
    }

    #[cfg(any(feature = "linux-can", target_os = "linux"))]
    {
        cans.insert(
            "socketcan".to_string(),
            Can::new(
                SocketCanBackend,
                make_frame_callback("socketcan".into(), Arc::clone(shared), "rx"),
                make_frame_callback("socketcan".into(), Arc::clone(shared), "tx"),
                make_error_callback("socketcan".into(), Arc::clone(shared)),
                make_status_callback("socketcan".into(), Arc::clone(shared)),
            ),
        );
        info!("SocketCAN backend initialized successfully");
    }

    cans.insert("socketcand".into(), Can::new(
        crate::can_communication::SocketcandBackend::default(),
        make_frame_callback("socketcand".into(), Arc::clone(shared), "rx"),
        make_frame_callback("socketcand".into(), Arc::clone(shared), "tx"),
        make_error_callback("socketcand".into(), Arc::clone(shared)),
        make_status_callback("socketcand".into(), Arc::clone(shared)),
    ));
    cans
}

pub struct CanManager {
    app_state: Arc<AppState>,
    shared: Arc<Mutex<ManagerShared>>,
    cans: BTreeMap<String, Can>,
    opening_socketcand: HashMap<u32, OpeningSocketcand>,
    cancelled_socketcand: HashMap<u32, u32>,
}

struct OpeningSocketcand {
    attempt: Option<u32>,
    connection: Arc<crate::can_communication::SocketcandConnect>,
}

impl CanManager {
    pub fn new(app_state: Arc<AppState>) -> Self {
        let shared = Arc::new(Mutex::new(ManagerShared {
            app: app_state.app.clone(),
            window_ms: DEFAULT_WINDOW_MS,
            channels: HashMap::new(),
            index_to_handle: HashMap::new(),
            handle_to_index: HashMap::new(),
            pending_events: Vec::new(),
            recorder: recording::Recorder::default(),
        }));

        // Flusher: drain buffered frame events into one batched webview event per
        // tick. Lives for the whole app; the buffer is bounded by one tick of bus
        // traffic. Emitting happens outside the lock.
        let flush_shared = Arc::clone(&shared);
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(FLUSH_INTERVAL_MS));
            let batch = {
                let Ok(mut lock) = flush_shared.lock() else { continue };
                if lock.pending_events.is_empty() {
                    continue;
                }
                let batch = std::mem::take(&mut lock.pending_events);
                (lock.app.clone(), batch)
            };
            let _ = batch.0.emit("can-frame-batch", &batch.1);
        });

        let cans = build_cans(&shared);
        Self {
            app_state, shared, cans,
            opening_socketcand: HashMap::new(), cancelled_socketcand: HashMap::new(),
        }
    }

    pub fn start_recording(&self, path: String, start_ms: u64) -> Result<RecordingStatus, String> {
        let mut shared = self.shared.lock().map_err(|e| e.to_string())?;
        shared.recorder.start(path, start_ms)?;
        Ok(shared.recorder.status())
    }

    pub fn stop_recording(&self) -> RecordingStatus {
        self.shared.lock().unwrap().recorder.stop()
    }

    pub fn recording_status(&self) -> RecordingStatus {
        self.shared.lock().unwrap().recorder.status()
    }

    // ── Channel lifecycle ─────────────────────────────────────────────────────

    /// List hardware channels available on all backends. The returned `ChannelInfo`
    /// items have `handle: 0` — they are not yet registered.
    pub fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let mut out = Vec::new();
        for (backend_name, can) in &self.cans {
            for (index, ch) in can.list_channels().into_iter().enumerate() {
                debug!("Found channel '{ch}' on backend '{backend_name}'");
                out.push(ChannelInfo {
                    capabilities: u8::try_from(index).ok().map(|i| can.capabilities(i)),
                    backend: backend_name.clone(),
                    name: ch,
                });
            }
        }
        Ok(out)
    }

    /// Register a channel (allocate data stores) without opening the hardware.
    /// The channel is looked up by name in the hinted backend first, then in
    /// every other backend — the backend stored in a saved project can be
    /// stale (e.g. the same channel name moved to different hardware). Returns
    /// the handle used for all subsequent calls plus the backend the channel
    /// was actually found in. Missing interfaces are registered with
    /// `available: false`, so their configuration remains usable offline.
    /// The backend DBC is loaded later by `open_channel`.
    /// Calling `create_channel` again for the same channel returns the
    /// existing handle.
    pub fn create_channel(&mut self, backend_name: &str, channel_name: &str) -> Result<CreatedChannel, String> {
        if let Some(can) = self.cans.get_mut(backend_name) { can.register_channel(channel_name)?; }
        let found = if backend_name == "none" { None } else { self
            .cans
            .get(backend_name)
            .and_then(|can| can.channel_index(channel_name).map(|i| (backend_name.to_string(), i)))
            .or_else(|| {
                self.cans
                    .iter()
                    .filter(|(name, _)| name.as_str() != backend_name)
                    .find_map(|(name, can)| can.channel_index(channel_name).map(|i| (name.clone(), i)))
            })
        };
        let available = found.is_some();
        let capabilities = found.as_ref().and_then(|(backend, index)| Some(self.cans.get(backend)?.capabilities(*index)));
        let hinted_backend = backend_name;
        let backend_name = found.as_ref().map(|(name, _)| name.as_str()).unwrap_or(backend_name).to_string();

        let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;

        if let Some((backend, index)) = &found {
            if backend == "socketcand" {
                if let Some(&handle) = lock.index_to_handle.get(&(backend.clone(), *index)) {
                    return Ok(CreatedChannel { handle, capabilities, backend: backend.clone(), available: true });
                }
            }
        }

        // Data and identity exist independently of the physical interface.
        let handle = lock
            .channels
            .iter()
            .find(|(_, ch)| ch.info.backend == backend_name && ch.info.name == channel_name)
            .or_else(|| {
                lock.channels
                    .iter()
                    .find(|(_, ch)| ch.info.backend == hinted_backend && ch.info.name == channel_name)
            })
            .map(|(&handle, _)| handle)
            .unwrap_or_else(|| NEXT_CHANNEL_HANDLE.fetch_add(1, Ordering::Relaxed));
        let info = ChannelInfo {
            capabilities,
            backend: backend_name.clone(),
            name: channel_name.to_string(),
        };
        if let Some((_, hw_index)) = found {
            lock.index_to_handle.insert((backend_name.clone(), hw_index), handle);
            lock.handle_to_index.insert(handle, (backend_name.clone(), hw_index));
        }
        lock.channels
            .entry(handle)
            .and_modify(|ch| ch.info = info.clone())
            .or_insert_with(|| ChannelData::new(info));
        info!("Created {backend_name} channel {channel_name} (handle: {handle})");
        Ok(CreatedChannel {
            handle,
            capabilities,
            backend: backend_name,
            available,
        })
    }

    /// Atomically reassign hardware without replacing logical channel data.
    pub fn assign_channels(&mut self, assignments: Vec<(u32, ChannelInfo)>) -> Result<Vec<bool>, String> {
        validate_assignment_handles(&assignments)?;
        let shared = Arc::clone(&self.shared);
        let mut lock = shared.lock().map_err(|_| "Lock poisoned".to_string())?;
        for (handle, info) in &assignments {
            if !lock.channels.contains_key(handle) { return Err("Channel not found".into()); }
            if let Some((backend, index)) = lock.handle_to_index.get(handle) {
                if self.cans.get(backend).is_some_and(|can| can.is_open(*index)) {
                    return Err("Stop capture before changing channels".into());
                }
            }
            for (&other, channel) in &lock.channels {
                let target = assignments.iter().find(|(h, _)| *h == other).map(|(_, i)| i).unwrap_or(&channel.info);
                if other != *handle && target.backend == info.backend
                    && (target.name == info.name || (info.backend == "socketcand"
                        && crate::can_communication::SocketcandBackend::same_endpoint(&target.name, &info.name)))
                {
                    return Err("Interface already assigned".into());
                }
            }
        }

        let remote_targets: Vec<&str> = assignments.iter().filter_map(|(_, info)| {
            (info.backend == "socketcand").then_some(info.name.as_str())
        }).collect();
        let removed_remote_indices: Vec<u8> = assignments.iter().filter_map(|(handle, _)| {
            let (backend, index) = lock.handle_to_index.get(handle)?;
            let current_name = lock.channels.get(handle)?.info.name.as_str();
            (backend == "socketcand" && !remote_targets.contains(&current_name)).then_some(*index)
        }).collect();
        if !remote_targets.is_empty() || !removed_remote_indices.is_empty() {
            self.cans.get_mut("socketcand").ok_or("Socketcand backend not found")?
                .replace_registrations(&removed_remote_indices, &remote_targets)?;
        }

        let resolved: Vec<_> = assignments.iter().map(|(_, info)| {
            self.cans.get(&info.backend).and_then(|can| can.channel_index(&info.name))
        }).collect();
        for (handle, _) in &assignments {
            self.cancel_socketcand_open(*handle);
            if let Some(key) = lock.handle_to_index.remove(handle) {
                lock.index_to_handle.remove(&key);
            }
        }
        for ((handle, info), index) in assignments.into_iter().zip(&resolved) {
            if let Some(index) = index {
                let key = (info.backend.clone(), *index);
                lock.index_to_handle.insert(key.clone(), handle);
                lock.handle_to_index.insert(handle, key);
            }
            let channel = lock.channels.get_mut(&handle).unwrap();
            channel.info = info;
            channel.socketcand_attempt = None;
        }
        Ok(resolved.iter().map(Option::is_some).collect())
    }

    pub fn remove_channel(&mut self, handle: u32) -> Result<(), String> {
        let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;

        if !lock.handle_to_index.contains_key(&handle) {
            lock.channels
                .remove(&handle)
                .ok_or_else(|| format!("channel handle {handle} not found"))?;
            self.cancelled_socketcand.remove(&handle);
            return Ok(());
        }

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

        let can = self.cans.get_mut(&backend_name).ok_or("Backend not found")?;

        if can.is_open(hw_index) {
            return Err(format!(
                "Cannot remove channel {handle} ({backend_name}:{hw_index}) while it is open"
            ));
        }

        lock.index_to_handle.remove(&(backend_name.clone(), hw_index));
        lock.handle_to_index.remove(&handle);
        lock.channels.remove(&handle);
        if let Some(opening) = self.opening_socketcand.remove(&handle) {
            opening.connection.cancel();
        }
        self.cancelled_socketcand.remove(&handle);
        can.unregister_channel(hw_index);

        info!("Removed {backend_name} channel {hw_index} (handle: {handle})");

        Ok(())
    }

    /// Set or clear the user-chosen display name for a channel. Used wherever
    /// the channel is shown to the user (e.g. CSV exports); the hardware name
    /// in `info` remains the identity.
    pub fn set_channel_display_name(&self, handle: u32, display_name: Option<String>) -> Result<(), String> {
        let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
        let ch = lock
            .channels
            .get_mut(&handle)
            .ok_or_else(|| format!("channel handle {handle} not found"))?;
        ch.display_name = display_name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Ok(())
    }

    /// Open the hardware for a channel registered with `create_channel`, loading
    /// the given DBC (parsed fresh from disk) for decode/encode. Returns the
    /// parsed DBC so the frontend can populate its signal tree.
    pub fn open_channel(
        manager: &Mutex<Self>,
        handle: u32,
        settings: ChannelSettings,
        dbc_path: Option<&str>,
        protocol: Protocol,
        resume_capture: bool,
        socketcand_attempt: Option<u32>,
    ) -> Result<Option<ParsedDbc>, String> {
        use crate::can_communication::{SocketcandBackend, SocketcandConnect, SOCKETCAND_OPEN_CANCELLED};

        let mut guard = manager.lock().map_err(|e| e.to_string())?;
        let (backend_name, hw_index) = guard.backend_index(handle)?;
        let remote = backend_name == "socketcand";
        if let Some(attempt) = socketcand_attempt {
            if !remote || guard.cancelled_socketcand.get(&handle).is_some_and(|&cancelled| attempt <= cancelled) {
                return Err(SOCKETCAND_OPEN_CANCELLED.into());
            }
        }
        let can = guard.cans.get(&backend_name).ok_or("Backend not found")?;
        can.ensure_closed(hw_index).map_err(|e| e.to_string())?;
        let capabilities = can.capabilities(hw_index);
        capabilities.validate(settings)?;
        if guard.opening_socketcand.contains_key(&handle) {
            return Err("Socketcand channel is already connecting".into());
        }

        // Parse the DBC before touching hardware so a bad path fails fast.
        let dbc = dbc_path.map(|p| ParsedDbc::new(p).map(Arc::new)).transpose()?;

        let endpoint = {
            let mut lock = guard.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            let ch = lock.channels.get_mut(&handle).ok_or("Channel not found")?;
            ch.prepare_capture(resume_capture);
            ch.dbc = dbc.clone();
            ch.protocol = protocol;
            ch.listen_only = settings.listen_only;
            // Bus load is available for Classical CAN with a known bit rate.
            ch.bitrate = if capabilities.external_timing || settings.mode == FrameFormat::Fd { 0 } else { settings.bitrate };
            remote.then(|| ch.info.name.clone())
        };

        if let Some(endpoint) = endpoint {
            // Resolve and negotiate without blocking IPC or TX on other channels.
            let opening = Arc::new(SocketcandConnect::default());
            guard.opening_socketcand.insert(handle, OpeningSocketcand {
                attempt: socketcand_attempt,
                connection: Arc::clone(&opening),
            });
            drop(guard);
            let connection = SocketcandBackend::connect(&endpoint, settings.listen_only, &opening);
            let mut guard = manager.lock().map_err(|e| e.to_string())?;
            if !guard.opening_socketcand.get(&handle).is_some_and(|current| Arc::ptr_eq(&current.connection, &opening)) {
                return Err(SOCKETCAND_OPEN_CANCELLED.into());
            }
            guard.opening_socketcand.remove(&handle);
            let (tx, rx) = connection.map_err(|e| e.to_string())?;
            guard.shared.lock().map_err(|_| "Lock poisoned".to_string())?
                .channels.get_mut(&handle).ok_or("Channel not found")?
                .socketcand_attempt = socketcand_attempt;
            let can = guard.cans.get_mut(&backend_name).ok_or("Backend not found")?;
            can.start_channel(hw_index, settings, tx, rx);
            return Ok(dbc.map(|d| (*d).clone()));
        }

        let manager = &mut *guard;
        let can = manager.cans.get_mut(&backend_name).ok_or("Backend not found")?;
        match can.open(hw_index, settings, None) {
            Ok(()) => {}
            Err(crate::can_communication::CanOpenError::PasswordRequired) => {
                info!("Backend '{backend_name}' requires admin password to open channel {handle}");
                let pw = manager.app_state.get_admin_password()?;
                can.open(hw_index, settings, Some(&pw)).map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }

        Ok(dbc.map(|d| (*d).clone()))
    }

    /// Close the hardware but keep the channel registered, so it can be reopened
    /// on the next Start. Unregistering the handle is `remove_channel`'s job.
    pub fn close_channel(&mut self, handle: u32, socketcand_attempt: Option<u32>) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        if let Some(attempt) = socketcand_attempt {
            if backend_name != "socketcand" { return Ok(()); }
            // Stop can reach the manager before the open worker starts. Keep
            // the cancelled attempt number so that worker also declines to open.
            let cancelled = self.cancelled_socketcand.entry(handle).or_default();
            *cancelled = (*cancelled).max(attempt);
            if let Some(opening) = self.opening_socketcand.get(&handle) {
                if opening.attempt == Some(attempt) { self.cancel_socketcand_open(handle); }
                return Ok(());
            }
            let shared = self.shared.lock().map_err(|e| e.to_string())?;
            if shared.channels.get(&handle).and_then(|ch| ch.socketcand_attempt) != Some(attempt) {
                return Ok(());
            }
        }
        if self.cancel_socketcand_open(handle) {
            return Ok(());
        }
        let can = self.cans.get_mut(&backend_name).ok_or("Backend not found")?;
        if socketcand_attempt.is_some() && !can.is_open(hw_index) {
            return Ok(());
        }
        can.close(hw_index)
    }

    fn cancel_socketcand_open(&mut self, handle: u32) -> bool {
        if let Some(opening) = self.opening_socketcand.remove(&handle) {
            opening.connection.cancel();
            return true;
        }
        false
    }

    /// Close all open hardware and drop every channel registration/data store.
    /// Used to give a reloading frontend a clean backend to rebuild from.
    pub fn reset(&mut self) {
        self.cancelled_socketcand.clear();
        self.reset_registrations();
    }

    fn reset_registrations(&mut self) {
        for (_, opening) in self.opening_socketcand.drain() {
            opening.connection.cancel();
        }
        let indices: Vec<(String, u8)> = match self.shared.lock() {
            Ok(lock) => lock.handle_to_index.values().cloned().collect(),
            Err(_) => return,
        };
        for (backend_name, hw_index) in indices {
            if let Some(can) = self.cans.get_mut(&backend_name) {
                if can.is_open(hw_index) {
                    let _ = can.close(hw_index);
                }
                can.unregister_channel(hw_index);
            }
        }
        if let Ok(mut lock) = self.shared.lock() {
            lock.channels.clear();
            lock.index_to_handle.clear();
            lock.handle_to_index.clear();
        }
    }

    /// Re-enumerate hardware in every backend and re-register all previously
    /// created channels (leaving them closed). Returns the old handle together
    /// with the re-registration result — the resolved backend can differ from
    /// the original when the channel name moved backends across the reload.
    /// On Kvaser this calls canUnloadLibrary()+canInitializeLibrary() which is the
    /// documented way to detect hardware connected after the initial library init.
    pub fn reload_backends(&mut self, start_capture: bool) -> Vec<(u32, CreatedChannel)> {
        let previous: Vec<(u32, ChannelInfo)> = self
            .shared
            .lock()
            .ok()
            .map(|lock| lock.channels.iter().map(|(&h, d)| (h, d.info.clone())).collect())
            .unwrap_or_default();

        // Keep handles and offline data stable so plots and simulation entries
        // continue referring to the same channel after a hardware refresh.
        let channels = self
            .shared
            .lock()
            .map(|mut lock| std::mem::take(&mut lock.channels))
            .unwrap_or_default();
        self.reset_registrations();
        if let Ok(mut lock) = self.shared.lock() {
            lock.channels = channels;
            if start_capture {
                // Socketcand opens asynchronously once the new capture starts.
                // Discard the previous capture before history can be queried.
                lock.channels.values_mut().filter(|ch| ch.info.backend == "socketcand")
                    .for_each(|ch| ch.prepare_capture(false));
            }
        }

        // canUnloadLibrary() resets the "already initialised" flag inside CANlib
        // so the next canInitializeLibrary() performs a true hardware re-scan.
        // Must be called after all handles are closed (which reset() guarantees).
        for can in self.cans.values() {
            can.reinitialize();
        }

        let total = previous.len();
        let mut remapped = Vec::new();
        for (old_handle, info) in previous {
            match self.create_channel(&info.backend, &info.name) {
                Ok(created) => remapped.push((old_handle, created)),
                Err(e) => warn!("Could not re-register {} {} after reload: {e}", info.backend, info.name),
            }
        }
        info!("Reloaded backends; re-registered {}/{total} channels", remapped.len());
        remapped
    }

    pub fn created_channels_info(&self) -> Vec<ChannelInfo> {
        self.shared
            .lock()
            .map(|l| l.channels.values().map(|c| c.info.clone()).collect())
            .unwrap_or_default()
    }

    // ── Send ──────────────────────────────────────────────────────────────────

    /// Fails loudly instead of letting a listen-only channel silently drop (or
    /// disturb) a transmit attempt on the bus.
    fn ensure_not_listen_only(&self, handle: u32) -> Result<(), String> {
        let lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
        if lock.channels.get(&handle).is_some_and(|c| c.listen_only) {
            return Err("Channel is listen-only — sending is disabled".to_string());
        }
        Ok(())
    }

    /// `is_extended: None` falls back to inferring the frame format from the id
    /// value (> 0x7FF ⇒ extended); pass an explicit value to send e.g. a 29-bit
    /// frame whose id fits in 11 bits.
    pub fn send_frame(&self, handle: u32, can_id: u32, data: Vec<u8>, is_extended: Option<bool>, tx: TxFormat) -> Result<(), String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans.get(&backend_name).ok_or("Backend not found")?.send_once(
            hw_index,
            CanFrame {
                metadata: tx.metadata(FrameFormat::Classical),
                can_id,
                is_extended: is_extended.unwrap_or(can_id > 0x7FF),
                data,
                timestamp_ms: None,
                error: None,
            },
        )
    }

    pub fn send_message(
        &self,
        handle: u32,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        generators: &HashMap<String, SignalGen>,
        tx: TxFormat,
    ) -> Result<(), String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        let (mut frame, source) = self.message_payload(handle, msg_id, signal_values, generators, tx)?;
        // Evaluate generators once at t=0 for checksums and counters.
        if let Some(mut source) = source {
            frame.data = source();
        }
        self.cans.get(&backend_name).ok_or("Backend not found")?.send_once(hw_index, frame)
    }

    pub fn add_periodic_frame(&self, handle: u32, frame: CanFrame, period_ms: u64) -> Result<u64, String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .add_periodic(hw_index, frame, period_ms, None)
    }

    /// Update a periodic raw frame while preserving its next deadline.
    pub fn update_periodic_frame(
        &self,
        handle: u32,
        periodic_handle: u64,
        replacement: FrameUpdate,
        period_ms: u64,
    ) -> Result<(), String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .update_periodic(hw_index, periodic_handle, replacement, None, period_ms)
    }

    pub fn remove_periodic(&self, handle: u32, periodic_handle: u64) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .remove_periodic(hw_index, periodic_handle)
    }

    /// Encode a DBC message and zero-fill from its declared size to the next
    /// legal CAN FD wire length. Generators work on the declared DBC bytes;
    /// the same zero-fill runs after generation on every transmission path.
    fn message_payload(
        &self,
        handle: u32,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        generators: &HashMap<String, SignalGen>,
        tx: TxFormat,
    ) -> Result<(CanFrame, Option<FrameDataSource>), String> {
        let dbc = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?
            .channels.get(&handle).and_then(|channel| channel.dbc.clone())
            .ok_or("No DBC loaded for this channel")?;
        let msg = dbc.messages.get(&msg_id).ok_or_else(|| format!("Message 0x{msg_id:X} not in DBC"))?;
        encode_dbc_transmission(msg, signal_values, generators, tx)
    }

    pub fn add_periodic_message(
        &self,
        handle: u32,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        generators: &HashMap<String, SignalGen>,
        period_ms: u64,
        tx: TxFormat,
    ) -> Result<u64, String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        let (frame, source) = self.message_payload(handle, msg_id, signal_values, generators, tx)?;
        self.cans.get(&backend_name).ok_or("Backend not found")?.add_periodic(hw_index, frame, period_ms, source)
    }

    pub fn update_periodic_message(
        &self,
        handle: u32,
        periodic_handle: u64,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        generators: &HashMap<String, SignalGen>,
        period_ms: u64,
        tx: TxFormat,
    ) -> Result<(), String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        let (frame, source) = self.message_payload(handle, msg_id, signal_values, generators, tx)?;
        let replacement = FrameUpdate {
            data: frame.data,
            can_id: Some(frame.can_id),
            is_extended: Some(frame.is_extended),
            tx: TxFormat { format: Some(frame.metadata.format), brs: Some(frame.metadata.brs) },
        };
        self.cans.get(&backend_name).ok_or("Backend not found")?
            .update_periodic(hw_index, periodic_handle, replacement, source, period_ms)
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub fn get_frames(&self, handle: Option<u32>, limit: usize) -> Vec<FrameInfo> {
        let lock = match self.shared.lock() {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        if let Some(h) = handle {
            lock.channels.get(&h).map(|c| c.get_frames(h, limit)).unwrap_or_default()
        } else {
            let mut all: Vec<FrameInfo> = lock.channels.iter().flat_map(|(&h, c)| c.get_frames(h, limit)).collect();
            all.sort_unstable_by_key(|f| f.timestamp_ms);
            let skip = all.len().saturating_sub(limit);
            all[skip..].to_vec()
        }
    }

    pub fn get_signal_history(&self, handle: u32, message_id: u32, signal_name: &str, since_ms: u64) -> Vec<SignalSample> {
        match self.shared.lock() {
            Ok(lock) => lock
                .channels
                .get(&handle)
                .map(|c| c.get_signal_history(message_id, signal_name, since_ms))
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Per-channel bus statistics for every currently open channel. Rates are
    /// computed against the counter snapshot taken by the previous call, so a
    /// single poller (the frontend, at ~1 Hz) gets rates over its own poll
    /// interval.
    pub fn get_bus_stats(&self) -> Vec<BusStats> {
        let open_handles: Vec<u32> = {
            let Ok(lock) = self.shared.lock() else { return Vec::new() };
            lock.handle_to_index
                .iter()
                .filter(|(_, (backend, hw_index))| {
                    self.cans.get(backend).is_some_and(|can| can.is_open(*hw_index))
                })
                .map(|(&h, _)| h)
                .collect()
        };

        let Ok(mut lock) = self.shared.lock() else { return Vec::new() };
        let now = std::time::Instant::now();
        let mut out = Vec::new();
        for handle in open_handles {
            let Some(ch) = lock.channels.get_mut(&handle) else { continue };
            let s = &mut ch.stats;
            let (rx_fps, tx_fps, bits_per_sec) = match s.last_query {
                Some((t, rx, tx, bits)) => {
                    let dt = now.duration_since(t).as_secs_f64();
                    if dt > 0.0 {
                        (
                            (s.rx_frames - rx) as f64 / dt,
                            (s.tx_frames - tx) as f64 / dt,
                            (s.wire_bits - bits) as f64 / dt,
                        )
                    } else {
                        (0.0, 0.0, 0.0)
                    }
                }
                None => (0.0, 0.0, 0.0),
            };
            let bus_load = (ch.bitrate > 0).then(|| (bits_per_sec / ch.bitrate as f64 * 100.0).min(100.0));
            s.last_query = Some((now, s.rx_frames, s.tx_frames, s.wire_bits));
            out.push(BusStats {
                channel_handle: handle,
                rx_fps,
                tx_fps,
                bus_load,
                error_frames: s.error_frames,
                status: s.bus_status,
            });
        }
        out.sort_unstable_by_key(|s| s.channel_handle);
        out
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

    /// Write all buffered frames across all channels to a CSV file, line by line.
    /// Frames are sorted by timestamp before writing.
    pub fn export_frames_csv(&self, path: &str, start_ms: u64) -> Result<usize, String> {
        use std::io::{BufWriter, Write};

        let lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;

        let mut frames: Vec<(u64, &str, &StoredFrame)> = lock
            .channels
            .values()
            .flat_map(|ch| ch.frames.iter().map(move |f| (f.timestamp_ms, ch.display_name(), f)))
            .collect();
        frames.sort_unstable_by_key(|r| r.0);

        let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        let mut writer = BufWriter::new(file);

        writer.write_all(recording::HEADER.as_bytes()).map_err(|e| e.to_string())?;
        for (_, ch_name, frame) in &frames {
            recording::write_frame(&mut writer, ch_name, frame, start_ms).map_err(|e| e.to_string())?;
        }

        writer.flush().map_err(|e| e.to_string())?;
        Ok(frames.len())
    }

    /// Write all buffered signal samples across all channels to a CSV file, line by line.
    /// Samples are sorted by timestamp before writing.
    pub fn export_signals_csv(&self, path: &str, start_ms: u64) -> Result<usize, String> {
        use std::io::{BufWriter, Write};

        let lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;

        let mut rows: Vec<(u64, &str, &str, &str, f64, &str)> = Vec::new();
        for ch in lock.channels.values() {
            for f in &ch.frames {
                let msg_name = f.message_name.as_deref().unwrap_or("");
                for sig in &f.signals {
                    rows.push((
                        f.timestamp_ms,
                        ch.display_name(),
                        msg_name,
                        sig.name.as_str(),
                        sig.value,
                        sig.unit.as_str(),
                    ));
                }
            }
        }
        rows.sort_unstable_by_key(|r| r.0);

        let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        let mut writer = BufWriter::new(file);

        writeln!(writer, "timestamp_ms,elapsed_s,channel,message,signal_name,value,unit").map_err(|e| e.to_string())?;

        for (ts, ch_name, msg_name, sig_name, value, unit) in &rows {
            let elapsed = (*ts as f64 - start_ms as f64) / 1000.0;
            writeln!(
                writer,
                "{},{:.3},{},{},{},{},{}",
                ts, elapsed, ch_name, msg_name, sig_name, value, unit
            )
            .map_err(|e| e.to_string())?;
        }

        writer.flush().map_err(|e| e.to_string())?;
        Ok(rows.len())
    }

    /// Returns (total_frame_count, estimated_heap_bytes) across all channels.
    pub fn frame_stats(&self) -> (usize, usize) {
        let Ok(lock) = self.shared.lock() else { return (0, 0) };
        let mut count = 0usize;
        let mut bytes = 0usize;
        for ch in lock.channels.values() {
            count += ch.frames.len();
            for f in &ch.frames {
                bytes += std::mem::size_of::<StoredFrame>();
                bytes += f.data.capacity();
                bytes += f.message_name.as_ref().map_or(0, |s| s.capacity());
                bytes += f.signals.capacity() * std::mem::size_of::<StoredSignal>();
                for s in &f.signals {
                    bytes += s.name.capacity() + s.unit.capacity();
                }
            }
        }
        (count, bytes)
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
}

fn encode_dbc_transmission(
    msg: &ParsedMessage,
    signal_values: &HashMap<String, f64>,
    generators: &HashMap<String, SignalGen>,
    tx: TxFormat,
) -> Result<(CanFrame, Option<FrameDataSource>), String> {
    if msg.dlc > 8 && msg.wire_format().is_none() && tx.format.is_none() {
        return Err(format!("Select a transmit frame format for {}", msg.name));
    }
    let metadata = tx.metadata(msg.wire_format().unwrap_or_default());
    let wire_len = msg.transmit_length(metadata.format)?;
    let mut data = msg.encode_signals(signal_values);
    data.resize(wire_len, 0);
    let source: Option<FrameDataSource> = if generators.is_empty() {
        None
    } else {
        let mut generate = build_frame_source(msg.clone(), signal_values.clone(), generators.clone());
        Some(Box::new(move || {
            let mut data = generate();
            data.resize(wire_len, 0);
            data
        }))
    };
    let frame = CanFrame {
        metadata, can_id: msg.id, is_extended: msg.is_extended, data, timestamp_ms: None, error: None,
    };
    Ok((frame, source))
}

// ── Callback factories ────────────────────────────────────────────────────────

fn make_frame_callback(
    backend: String,
    shared: Arc<Mutex<ManagerShared>>,
    direction: &'static str,
) -> impl Fn(u8, CanFrame) + Send + Sync + 'static {
    move |hw_index, raw| {
        let ts = raw.timestamp_ms.unwrap_or_else(now_ms);

        // Hold the lock only long enough to decode, store the frame, and queue
        // the event; the flusher thread emits queued events in batches.
        let mut lock = match shared.lock() {
            Ok(l) => l,
            Err(_) => return,
        };

        let handle = match lock.index_to_handle.get(&(backend.clone(), hw_index)).cloned() {
            Some(h) => h,
            None => return,
        };

        let (protocol, dbc) = match lock.channels.get(&handle) {
            Some(ch) => (ch.protocol, ch.dbc.clone()),
            None => return,
        };

        // J1939: feed transport-protocol frames to the reassembler. A completed
        // transfer yields a synthetic frame (announced PGN as id, data longer
        // than 8 bytes) ingested right after the raw frame that completed it.
        // Error frames have no identifier and are never J1939 traffic.
        let mut completed: Option<CanFrame> = None;
        if protocol == Protocol::J1939 && raw.metadata.format == FrameFormat::Classical && raw.is_extended && raw.error.is_none() {
            let pgn = j1939::decode_id(raw.can_id).pgn;
            if pgn == j1939::PGN_TP_CM || pgn == j1939::PGN_TP_DT {
                if let Some(ch) = lock.channels.get_mut(&handle) {
                    completed = ch.tp.handle_frame(&raw, ts);
                }
            }
        }

        ingest_frame(&mut lock, handle, protocol, dbc.as_deref(), raw, ts, direction, false);
        if let Some(frame) = completed {
            ingest_frame(&mut lock, handle, protocol, dbc.as_deref(), frame, ts, direction, true);
        }
    }
}

/// Status callback for a backend: stores the ~1/s controller status report in
/// the channel's stats. The frontend picks it up on its next `get_bus_stats`
/// poll — no webview event needed at this cadence.
fn make_status_callback(backend: String, shared: Arc<Mutex<ManagerShared>>) -> impl Fn(u8, BusStatus) + Send + Sync + 'static {
    move |hw_index, status| {
        let Ok(mut lock) = shared.lock() else { return };
        let Some(&handle) = lock.index_to_handle.get(&(backend.clone(), hw_index)) else { return };
        if let Some(ch) = lock.channels.get_mut(&handle) {
            ch.stats.bus_status = Some(status);
        }
    }
}

/// Error callback for a backend: resolves the hardware index to the channel
/// handle and emits a "channel-error" event. The lock is held only for the
/// lookup — emitting happens outside it (the callback runs on the RX/TX
/// thread that is about to exit or keep sending).
fn make_error_callback(backend: String, shared: Arc<Mutex<ManagerShared>>) -> impl Fn(u8, String, bool) + Send + Sync + 'static {
    move |hw_index, error, fatal| {
        let resolved = {
            let Ok(lock) = shared.lock() else { return };
            lock.index_to_handle
                .get(&(backend.clone(), hw_index))
                .map(|&handle| {
                    let attempt = lock.channels.get(&handle).and_then(|ch| ch.socketcand_attempt);
                    (handle, attempt, lock.app.clone())
                })
        };
        let Some((handle, socketcand_attempt, app)) = resolved else { return };
        let _ = app.emit(
            "channel-error",
            &ChannelErrorEvent {
                channel_handle: handle,
                socketcand_attempt,
                error,
                fatal,
            },
        );
    }
}

/// Decode a frame, store it in the channel's ring buffer and queue its webview
/// event. Called once per raw frame and once more for each reassembled J1939
/// transport message (`reassembled = true` in that second call).
fn ingest_frame(
    lock: &mut ManagerShared,
    handle: u32,
    protocol: Protocol,
    dbc: Option<&ParsedDbc>,
    raw: CanFrame,
    ts: u64,
    direction: &'static str,
    reassembled: bool,
) {
    let window_ms = lock.window_ms;
    let is_error = raw.error.is_some();
    let direction = if is_error { "err" } else { direction };

    // Traffic stats. Synthetic reassembled frames never appeared on the wire
    // in this form (their TP.DT frames were already counted individually).
    if let Some(ch) = lock.channels.get_mut(&handle) {
        if is_error {
            ch.stats.error_frames += 1;
        } else if !reassembled {
            match direction {
                "tx" => ch.stats.tx_frames += 1,
                _ => ch.stats.rx_frames += 1,
            }
            if raw.metadata.format == FrameFormat::Classical {
                ch.stats.wire_bits += estimate_frame_bits(raw.data.len(), raw.is_extended);
            }
        }
    }

    let j1939_info = (protocol == Protocol::J1939 && raw.metadata.format == FrameFormat::Classical && raw.is_extended && !is_error).then(|| j1939::decode_id(raw.can_id));

    // Error frames carry no decodable payload.
    let decoded = match protocol {
        _ if is_error => None,
        Protocol::J1939 => dbc.and_then(|d| d.decode_frame_j1939(&raw, reassembled)),
        Protocol::None => dbc.and_then(|d| d.decode_frame(&raw)).map(|m| (m, message_key(raw.can_id, raw.is_extended))),
    };
    let (decoded_msg, dbc_msg_id) = match decoded {
        Some((m, id)) => (Some(m), Some(id)),
        None => (None, None),
    };
    let message_name = decoded_msg.as_ref().map(|m| m.name.clone());

    // Interleaved [value, raw] pairs covering *every* signal of the DBC message
    // (decode preserves order), so the frontend's position-based name lookup
    // stays aligned even for multiplexed messages. Signals of inactive mux
    // groups become NaN pairs — serde_json emits them as null. Storage keeps
    // only active signals: history/export must not contain garbage samples.
    let mut sig_data: Vec<f64> = Vec::new();
    let mut stored_signals: Vec<StoredSignal> = Vec::new();
    if let Some(m) = decoded_msg {
        sig_data.reserve(m.signals.len() * 2);
        for s in m.signals {
            if s.active {
                sig_data.push(s.physical);
                sig_data.push(s.raw as f64);
                stored_signals.push(StoredSignal {
                    name: s.name,
                    value: s.physical,
                    raw: s.raw,
                    unit: s.unit,
                });
            } else {
                sig_data.push(f64::NAN);
                sig_data.push(f64::NAN);
            }
        }
    }

    let stored = StoredFrame {
        metadata: raw.metadata,
        can_id: raw.can_id,
        is_extended: raw.is_extended,
        data: raw.data.clone(),
        timestamp_ms: ts,
        direction,
        message_name,
        j1939: j1939_info,
        reassembled,
        error: raw.error.clone(),
        dbc_msg_id,
        signals: stored_signals,
    };

    let event = CanFrameEvent {
        metadata: raw.metadata,
        dbc_message_id: dbc_msg_id,
        channel_handle: handle,
        socketcand_attempt: lock.channels.get(&handle).and_then(|ch| ch.socketcand_attempt),
        can_id: raw.can_id,
        is_extended: raw.is_extended,
        dlc: raw.data.len() as u16,
        data: raw.data,
        timestamp_ms: ts,
        direction,
        j1939: j1939_info,
        reassembled,
        error: raw.error,
        signals: sig_data,
    };

    if lock.recorder.enabled() {
        let name = lock.channels.get(&handle).map(|ch| ch.display_name().to_owned()).unwrap_or_default();
        lock.recorder.record(&name, &stored);
    }
    if let Some(ch) = lock.channels.get_mut(&handle) {
        ch.push(stored, window_ms);
    }
    lock.pending_events.push(event);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn validate_assignment_handles(assignments: &[(u32, ChannelInfo)]) -> Result<(), String> {
    let mut handles = HashSet::with_capacity(assignments.len());
    if assignments.iter().any(|(handle, _)| !handles.insert(*handle)) {
        return Err("Channel assigned more than once".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_assignment_handles_are_rejected() {
        let info = ChannelInfo {
            capabilities: None, backend: "test".into(),
            name: "CAN 1".into(),
        };
        let assignments = vec![(7, info.clone()), (7, info)];

        assert_eq!(
            validate_assignment_handles(&assignments),
            Err("Channel assigned more than once".into())
        );
    }

    #[test]
    fn signal_history_separates_frame_formats_and_keeps_j1939_matching() {
        let mut channel = ChannelData::new(ChannelInfo { capabilities: None, backend: "test".into(), name: "test".into() });
        for (can_id, extended, dbc_id, value) in [
            (0x123, false, 0x123, 1.0),
            (0x123, true, message_key(0x123, true), 2.0),
            (0x18F00400, true, 0x0CF00400, 3.0),
        ] {
            channel.frames.push_back(StoredFrame {
                metadata: Default::default(),
                can_id, is_extended: extended, data: vec![0; 8], timestamp_ms: 100, direction: "rx",
                message_name: Some("Message".into()), j1939: None, reassembled: false, error: None,
                dbc_msg_id: Some(dbc_id),
                signals: vec![StoredSignal { name: "Value".into(), value, raw: value as i64, unit: String::new() }],
            });
        }
        for (id, expected) in [(0x123, 1.0), (message_key(0x123, true), 2.0), (0x0CF00400, 3.0)] {
            let samples = channel.get_signal_history(id, "Value", 0);
            assert_eq!(samples.len(), 1);
            assert_eq!(samples[0].value, expected);
        }

        channel.stats.error_frames = 2;
        channel.stats.last_query = Some((std::time::Instant::now(), 3, 0, 0));
        channel.prepare_capture(true);
        assert_eq!(channel.get_signal_history(0x123, "Value", 0).len(), 1);
        assert_eq!(channel.stats.error_frames, 2);
        assert!(channel.stats.last_query.is_none());

        channel.prepare_capture(false);
        assert!(channel.get_signal_history(0x123, "Value", 0).is_empty());
        assert_eq!(channel.stats.error_frames, 0);
    }

    #[test]
    fn mixed_capture_preserves_fd_metadata_and_decodes_late_signals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fd.dbc");
        std::fs::write(&path, r#"VERSION ""

NS_ :

BS_:

BU_:

BO_ 2147483939 FdMessage: 10 Vector__XXX
 SG_ Late : 72|8@1+ (1,0) [0|255] "" Vector__XXX

BA_DEF_ BO_ "VFrameFormat" ENUM "StandardCAN","ExtendedCAN_FD";
BA_ "VFrameFormat" BO_ 2147483939 1;
"#).unwrap();
        let dbc = Arc::new(ParsedDbc::new(path.to_str().unwrap()).unwrap());
        let mut channel = ChannelData::new(ChannelInfo { backend: "test".into(), name: "test".into(), capabilities: None });
        channel.dbc = Some(dbc);
        let dbc = channel.dbc.clone().unwrap();
        let message = &dbc.messages[&message_key(0x123, true)];
        let values = HashMap::from([("Late".into(), 42.0)]);
        let tx = TxFormat { format: None, brs: Some(true) };
        let (outgoing, source) = encode_dbc_transmission(message, &values, &HashMap::new(), tx).unwrap();
        assert!(source.is_none());
        assert_eq!(outgoing.metadata, FrameMetadata::fd(true, false));
        assert!(outgoing.is_extended);
        assert_eq!(outgoing.data, [0, 0, 0, 0, 0, 0, 0, 0, 0, 42, 0, 0]);
        let generators = HashMap::from([("Late".into(), SignalGen::Counter)]);
        let (_, source) = encode_dbc_transmission(message, &values, &generators, tx).unwrap();
        let mut source = source.unwrap();
        assert_eq!(&source()[9..], &[0, 0, 0]);
        assert_eq!(&source()[9..], &[1, 0, 0]);

        let mut fd = CanFrame {
            metadata: FrameMetadata::fd(true, true), can_id: 0x123, is_extended: true,
            data: vec![0; 64], timestamp_ms: None, error: None,
        };
        fd.data[9] = 42;
        let mut classic = fd.clone();
        classic.metadata = FrameMetadata::default();
        classic.data.truncate(8);
        for raw in [classic, fd] {
            let decoded = dbc.decode_frame(&raw);
            channel.push(StoredFrame {
                metadata: raw.metadata, can_id: raw.can_id, is_extended: raw.is_extended,
                data: raw.data, timestamp_ms: 100, direction: "rx",
                message_name: decoded.as_ref().map(|message| message.name.clone()),
                j1939: None, reassembled: false, error: None, dbc_msg_id: None,
                signals: decoded.into_iter().flat_map(|message| message.signals).map(|signal| StoredSignal {
                    name: signal.name, value: signal.physical, raw: signal.raw, unit: signal.unit,
                }).collect(),
            }, 30_000);
        }
        let history = channel.get_frames(1, 2);
        assert_eq!(history[0].metadata.format, FrameFormat::Classical);
        assert!(history[0].message_name.is_none());
        assert_eq!(history[1].metadata, FrameMetadata::fd(true, true));
        assert_eq!(history[1].message_name.as_deref(), Some("FdMessage"));
        assert_eq!(history[1].signals[0].value, 42.0);
        assert_eq!(history[1].data.len(), 64);
        let json = serde_json::to_value(&history[1]).unwrap();
        assert_eq!(json["format"], "fd");
        assert_eq!(json["brs"], true);
        assert_eq!(json["esi"], true);
        assert_eq!(json["dlc"], 64);
        let mut csv = Vec::new();
        for frame in &channel.frames { recording::write_frame(&mut csv, "test", frame, 100).unwrap(); }
        let csv = String::from_utf8(csv).unwrap();
        assert!(csv.lines().next().unwrap().ends_with(",classical,0,0"));
        assert!(csv.lines().nth(1).unwrap().ends_with(",fd,1,1"));
        assert_eq!(csv.lines().nth(1).unwrap().split(',').nth(3), Some("00000123"));
    }

    #[test]
    fn frame_bits_estimate_brackets_reality() {
        // A standard 8-byte frame is 111 bits before stuffing; with stuffing
        // it lands between that and the 135-bit worst case.
        let std8 = estimate_frame_bits(8, false);
        assert!((111..=135).contains(&std8), "std 8-byte estimate {std8}");
        // Extended 8-byte: 131 pre-stuffing, ~160 worst case.
        let ext8 = estimate_frame_bits(8, true);
        assert!((131..=160).contains(&ext8), "ext 8-byte estimate {ext8}");
        // More data ⇒ more bits; extended > standard at equal DLC.
        assert!(estimate_frame_bits(0, false) < std8);
        assert!(ext8 > std8);
        // Reassembled J1939 payloads (> 8 bytes) clamp at the 8-byte cost.
        assert_eq!(estimate_frame_bits(64, false), std8);
    }
}
