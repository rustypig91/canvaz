use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::app_state::AppState;
use crate::can_communication::{Can, CanFrame, FrameDataSource};
use crate::dbc_parser::ParsedDbc;
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
    can_id: u32,
    is_extended: bool,
    /// u16: reassembled J1939 transport messages carry up to 1785 data bytes.
    dlc: u16,
    data: Vec<u8>,
    timestamp_ms: u64,
    direction: &'static str,
    /// J1939 breakdown of the identifier; only set on J1939 channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    j1939: Option<J1939Info>,
    /// True if this row is a synthetic frame reassembled from a J1939 TP.CM /
    /// TP.DT transfer rather than a frame that actually appeared on the bus.
    #[serde(skip_serializing_if = "is_false")]
    reassembled: bool,
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
    error: String,
    fatal: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameInfo {
    pub channel_handle: u32,
    pub can_id: u32,
    pub is_extended: bool,
    pub dlc: u16,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
    pub direction: &'static str,
    pub message_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub j1939: Option<J1939Info>,
    /// True if this row is a synthetic frame reassembled from a J1939 TP.CM /
    /// TP.DT transfer rather than a frame that actually appeared on the bus.
    #[serde(skip_serializing_if = "is_false")]
    pub reassembled: bool,
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
    pub backend: String,
    pub name: String,
}

/// Result of `create_channel`: the allocated handle plus the backend the
/// channel name was actually found in (which may differ from the hint).
#[derive(Debug, Clone, Serialize)]
pub struct CreatedChannel {
    pub handle: u32,
    pub backend: String,
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
    /// DBC message id this frame decoded against. On J1939 channels it can
    /// differ from `can_id` (PGN match ignores priority/source-address bits);
    /// signal-history queries key on it.
    dbc_msg_id: Option<u32>,
    signals: Vec<StoredSignal>,
}

// ── Per-channel data ──────────────────────────────────────────────────────────

struct ChannelData {
    frames: VecDeque<StoredFrame>,
    dbc: Option<Arc<ParsedDbc>>,
    info: ChannelInfo,
    /// User-chosen display name used in CSV exports; `info.name` stays the
    /// hardware identity used for lookup.
    display_name: Option<String>,
    protocol: Protocol,
    tp: TpReassembler,
    /// Set by the last `open_channel` call. When true, TX entry points refuse
    /// to send so the bus stays undisturbed rather than the frame silently
    /// getting dropped by the hardware's silent/listen-only mode.
    listen_only: bool,
}

impl ChannelData {
    fn new(info: ChannelInfo) -> Self {
        Self {
            frames: VecDeque::new(),
            dbc: None,
            info,
            display_name: None,
            protocol: Protocol::None,
            tp: TpReassembler::default(),
            listen_only: false,
        }
    }

    /// Name shown to the user: the custom display name when set, else the
    /// hardware name.
    fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.info.name)
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
                dlc: f.data.len() as u16,
                j1939: f.j1939,
                reassembled: f.reassembled,
                data: f.data.clone(),
                timestamp_ms: f.timestamp_ms,
                direction: f.direction,
                message_name: f.message_name.clone(),
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
            // dbc_msg_id covers J1939 frames whose wire id differs from the DBC
            // id the caller knows (priority/SA bits vary per sender).
            .filter(|f| f.timestamp_ms >= since_ms && (f.can_id == can_id || f.dbc_msg_id == Some(can_id)))
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
            ),
        );
        info!("SocketCAN backend initialized successfully");
    }

    cans
}

pub struct CanManager {
    app_state: Arc<AppState>,
    shared: Arc<Mutex<ManagerShared>>,
    cans: BTreeMap<String, Can>,
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
        Self { app_state, shared, cans }
    }

    // ── Channel lifecycle ─────────────────────────────────────────────────────

    /// List hardware channels available on all backends. The returned `ChannelInfo`
    /// items have `handle: 0` — they are not yet registered.
    pub fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let mut out = Vec::new();
        for (backend_name, can) in &self.cans {
            for ch in can.list_channels() {
                debug!("Found channel '{ch}' on backend '{backend_name}'");
                out.push(ChannelInfo {
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
    /// was actually found in. The DBC is loaded later by `open_channel`.
    /// Calling `create_channel` again for the same channel returns the
    /// existing handle.
    pub fn create_channel(&mut self, backend_name: &str, channel_name: &str) -> Result<CreatedChannel, String> {
        let find = |can: &Can| can.list_channels().iter().position(|n| n == channel_name).map(|i| i as u8);
        let (backend_name, hw_index) = self
            .cans
            .get(backend_name)
            .and_then(|can| find(can).map(|i| (backend_name.to_string(), i)))
            .or_else(|| {
                self.cans
                    .iter()
                    .filter(|(name, _)| name.as_str() != backend_name)
                    .find_map(|(name, can)| find(can).map(|i| (name.clone(), i)))
            })
            .ok_or_else(|| format!("Channel '{channel_name}' not found in any backend"))?;

        let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;

        // Already registered — return existing handle.
        if let Some(&handle) = lock.index_to_handle.get(&(backend_name.clone(), hw_index)) {
            return Ok(CreatedChannel {
                handle,
                backend: backend_name,
            });
        }

        let handle = NEXT_CHANNEL_HANDLE.fetch_add(1, Ordering::Relaxed);
        let info = ChannelInfo {
            backend: backend_name.clone(),
            name: channel_name.to_string(),
        };
        lock.index_to_handle.insert((backend_name.clone(), hw_index), handle);
        lock.handle_to_index.insert(handle, (backend_name.clone(), hw_index));
        lock.channels.insert(handle, ChannelData::new(info));
        info!("Created {backend_name} channel {channel_name} (handle: {handle})");
        Ok(CreatedChannel {
            handle,
            backend: backend_name,
        })
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

        let can = self.cans.get_mut(&backend_name).ok_or("Backend not found")?;

        if can.is_open(hw_index) {
            return Err(format!(
                "Cannot remove channel {handle} ({backend_name}:{hw_index}) while it is open"
            ));
        }

        lock.index_to_handle.remove(&(backend_name.clone(), hw_index));
        lock.handle_to_index.remove(&handle);
        lock.channels.remove(&handle);

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
        &mut self,
        handle: u32,
        bitrate: u32,
        listen_only: bool,
        dbc_path: Option<&str>,
        protocol: Protocol,
    ) -> Result<Option<ParsedDbc>, String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;

        // Parse the DBC before touching hardware so a bad path fails fast.
        let dbc = dbc_path.map(|p| ParsedDbc::new(p).map(Arc::new)).transpose()?;

        {
            let mut lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            if let Some(ch) = lock.channels.get_mut(&handle) {
                ch.frames.clear();
                ch.dbc = dbc.clone();
                ch.protocol = protocol;
                ch.tp = TpReassembler::default();
                ch.listen_only = listen_only;
            }
        }

        let can = self.cans.get_mut(&backend_name).ok_or("Backend not found")?;
        match can.open(hw_index, bitrate, listen_only, None) {
            Ok(()) => {}
            Err(crate::can_communication::CanOpenError::PasswordRequired) => {
                info!("Backend '{backend_name}' requires admin password to open channel {handle}");
                let pw = self.app_state.get_admin_password()?;
                can.open(hw_index, bitrate, listen_only, Some(&pw)).map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }

        Ok(dbc.map(|d| (*d).clone()))
    }

    /// Close the hardware but keep the channel registered, so it can be reopened
    /// on the next Start. Unregistering the handle is `remove_channel`'s job.
    pub fn close_channel(&mut self, handle: u32) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans.get_mut(&backend_name).ok_or("Backend not found")?.close(hw_index)
    }

    /// Close all open hardware and drop every channel registration/data store.
    /// Used to give a reloading frontend a clean backend to rebuild from.
    pub fn reset(&mut self) {
        let indices: Vec<(String, u8)> = match self.shared.lock() {
            Ok(lock) => lock.handle_to_index.values().cloned().collect(),
            Err(_) => return,
        };
        for (backend_name, hw_index) in indices {
            if let Some(can) = self.cans.get_mut(&backend_name) {
                if can.is_open(hw_index) {
                    let _ = can.close(hw_index);
                }
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
    pub fn reload_backends(&mut self) -> Vec<(u32, CreatedChannel)> {
        let previous: Vec<(u32, ChannelInfo)> = self
            .shared
            .lock()
            .ok()
            .map(|lock| lock.channels.iter().map(|(&h, d)| (h, d.info.clone())).collect())
            .unwrap_or_default();

        self.reset();

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
    pub fn send_frame(&self, handle: u32, can_id: u32, data: Vec<u8>, is_extended: Option<bool>) -> Result<(), String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans.get(&backend_name).ok_or("Backend not found")?.send_once(
            hw_index,
            CanFrame {
                can_id,
                is_extended: is_extended.unwrap_or(can_id > 0x7FF),
                data,
                timestamp_ms: None,
            },
        )
    }

    pub fn send_message(
        &self,
        handle: u32,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        generators: &HashMap<String, SignalGen>,
    ) -> Result<(), String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        // Evaluate generators once at t=0 so a one-shot send of an E2E message
        // still carries a valid checksum (and counter 0).
        let (data, is_extended, source) = self.message_payload(handle, msg_id, signal_values, generators)?;
        let data = match source {
            Some(mut src) => src(),
            None => data,
        };
        self.cans.get(&backend_name).ok_or("Backend not found")?.send_once(
            hw_index,
            CanFrame {
                can_id: msg_id,
                is_extended,
                data,
                timestamp_ms: None,
            },
        )
    }

    /// Encode `signal_values` against the channel's DBC message and return the
    /// data plus the message's frame format. The format comes from the DBC's
    /// extended-id flag, not from the id value — a 29-bit message with an id
    /// ≤ 0x7FF must still go out as an extended frame.
    fn encode_dbc_message(
        &self,
        handle: u32,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
    ) -> Result<(Vec<u8>, bool), String> {
        let dbc = {
            let lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            lock.channels
                .get(&handle)
                .and_then(|c| c.dbc.clone())
                .ok_or_else(|| "No DBC loaded for this channel".to_string())?
        };
        let msg = dbc
            .messages
            .get(&msg_id)
            .ok_or_else(|| format!("Message 0x{msg_id:X} not in DBC"))?;
        Ok((msg.encode_signals(signal_values), msg.is_extended))
    }

    pub fn add_periodic_frame(&self, handle: u32, frame: CanFrame, period_ms: u64) -> Result<u64, String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .add_periodic(hw_index, frame, period_ms, None)
    }

    /// Swap a periodic raw frame's data and period in place — no transmission
    /// gap or phase reset (frame id/format changes still need remove + add).
    pub fn update_periodic_frame(
        &self,
        handle: u32,
        periodic_handle: u64,
        data: Vec<u8>,
        period_ms: u64,
    ) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .update_periodic(hw_index, periodic_handle, data, None, period_ms)
    }

    pub fn remove_periodic(&self, handle: u32, periodic_handle: u64) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .remove_periodic(hw_index, periodic_handle)
    }

    /// Encoded payload plus, when any generators are declared, the per-tick
    /// data source evaluated by the TX loop.
    fn message_payload(
        &self,
        handle: u32,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        generators: &HashMap<String, SignalGen>,
    ) -> Result<(Vec<u8>, bool, Option<FrameDataSource>), String> {
        let (data, is_extended) = self.encode_dbc_message(handle, msg_id, signal_values)?;
        let source = if generators.is_empty() {
            None
        } else {
            let lock = self.shared.lock().map_err(|_| "Lock poisoned".to_string())?;
            let msg = lock
                .channels
                .get(&handle)
                .and_then(|c| c.dbc.as_ref())
                .and_then(|d| d.messages.get(&msg_id))
                .cloned()
                .ok_or_else(|| format!("Message 0x{msg_id:X} not in DBC"))?;
            Some(build_frame_source(msg, signal_values.clone(), generators.clone()))
        };
        Ok((data, is_extended, source))
    }

    pub fn add_periodic_message(
        &self,
        handle: u32,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        generators: &HashMap<String, SignalGen>,
        period_ms: u64,
    ) -> Result<u64, String> {
        self.ensure_not_listen_only(handle)?;
        let (backend_name, hw_index) = self.backend_index(handle)?;
        let (data, is_extended, source) = self.message_payload(handle, msg_id, signal_values, generators)?;
        self.cans.get(&backend_name).ok_or("Backend not found")?.add_periodic(
            hw_index,
            CanFrame {
                can_id: msg_id,
                is_extended,
                data,
                timestamp_ms: None,
            },
            period_ms,
            source,
        )
    }

    /// Re-encode a running periodic DBC message with new values/generators and
    /// swap it in place. The entry's send deadline is preserved (no gap);
    /// generator phase and counter state restart, since the payload closure is
    /// rebuilt.
    pub fn update_periodic_message(
        &self,
        handle: u32,
        periodic_handle: u64,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        generators: &HashMap<String, SignalGen>,
        period_ms: u64,
    ) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        let (data, _, source) = self.message_payload(handle, msg_id, signal_values, generators)?;
        self.cans
            .get(&backend_name)
            .ok_or("Backend not found")?
            .update_periodic(hw_index, periodic_handle, data, source, period_ms)
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

        writeln!(
            writer,
            "timestamp_ms,elapsed_s,channel,can_id,direction,dlc,data,message,pgn,src,dst,prio,reassembled"
        )
        .map_err(|e| e.to_string())?;

        for (ts, ch_name, f) in &frames {
            let elapsed = (*ts as f64 - start_ms as f64) / 1000.0;
            let id_str = if f.is_extended {
                format!("{:08X}", f.can_id)
            } else {
                format!("{:03X}", f.can_id)
            };
            let data_str = f.data.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
            let msg_str = f.message_name.as_deref().unwrap_or("").replace('"', "\"\"");
            let j1939_str = f
                .j1939
                .map(|j| format!("{:X},{:02X},{:02X},{}", j.pgn, j.sa, j.da, j.priority))
                .unwrap_or_else(|| ",,,".to_string());
            writeln!(
                writer,
                "{},{:.3},{},{},{},{},\"{}\",\"{}\",{},{}",
                ts,
                elapsed,
                ch_name,
                id_str,
                f.direction,
                f.data.len(),
                data_str,
                msg_str,
                j1939_str,
                f.reassembled as u8
            )
            .map_err(|e| e.to_string())?;
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
        let mut completed: Option<CanFrame> = None;
        if protocol == Protocol::J1939 && raw.is_extended {
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
                .map(|&handle| (handle, lock.app.clone()))
        };
        let Some((handle, app)) = resolved else { return };
        let _ = app.emit(
            "channel-error",
            &ChannelErrorEvent {
                channel_handle: handle,
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

    let j1939_info = (protocol == Protocol::J1939 && raw.is_extended).then(|| j1939::decode_id(raw.can_id));

    let decoded = match protocol {
        Protocol::J1939 => dbc.and_then(|d| d.decode_frame_j1939(&raw)),
        Protocol::None => dbc.and_then(|d| d.decode_frame(&raw)).map(|m| (m, raw.can_id)),
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
        can_id: raw.can_id,
        is_extended: raw.is_extended,
        data: raw.data.clone(),
        timestamp_ms: ts,
        direction,
        message_name,
        j1939: j1939_info,
        reassembled,
        dbc_msg_id,
        signals: stored_signals,
    };

    let event = CanFrameEvent {
        channel_handle: handle,
        can_id: raw.can_id,
        is_extended: raw.is_extended,
        dlc: raw.data.len() as u16,
        data: raw.data,
        timestamp_ms: ts,
        direction,
        j1939: j1939_info,
        reassembled,
        signals: sig_data,
    };

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
