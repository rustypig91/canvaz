use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::app_state::AppState;
use crate::can_communication::{Can, CanFrame};
use crate::dbc_parser::ParsedDbc;

#[cfg(feature = "kvaser")]
use crate::can_communication::KvaserBackend;
#[cfg(feature = "pcan")]
use crate::can_communication::PcanBackend;
#[cfg(feature = "linux-can")]
use crate::can_communication::SocketCanBackend;

use log::*;

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
        Self { current: v, min: v, max: v }
    }

    fn update(&mut self, v: f64) {
        self.current = v;
        if v < self.min { self.min = v; }
        if v > self.max { self.max = v; }
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
    signals: HashMap<(u32, String), SignalStats>,
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

    /// Push a frame, evict frames older than `window_ms`. Returns events to emit:
    /// (signal_name, value, unit, message_name, min, max).
    /// Min/max are all-time since channel open; they grow but never shrink during
    /// eviction to avoid an O(N²) rescan under the shared lock.
    fn push(&mut self, frame: StoredFrame, window_ms: u64) -> Vec<(String, f64, String, String, f64, f64)> {
        let cutoff = frame.timestamp_ms.saturating_sub(window_ms);
        while self.frames.front().map_or(false, |f| f.timestamp_ms < cutoff) {
            self.frames.pop_front();
        }
        self.frames.push_back(frame.clone());

        let msg_name = frame.message_name.clone().unwrap_or_default();
        let mut events = Vec::new();
        for sig in &frame.signals {
            let sig_key = (frame.can_id, sig.name.clone());
            self.signals
                .entry(sig_key.clone())
                .and_modify(|s| s.update(sig.value))
                .or_insert_with(|| SignalStats::new(sig.value));
            let s = &self.signals[&sig_key];
            events.push((sig.name.clone(), sig.value, sig.unit.clone(), msg_name.clone(), s.min, s.max));
        }
        events
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
                dlc: f.data.len() as u8,
                data: f.data.clone(),
                timestamp_ms: f.timestamp_ms,
                direction: f.direction,
                message_name: f.message_name.clone(),
            })
            .collect()
    }

    fn get_signal_history(&self, can_id: u32, signal_name: &str, since_ms: u64) -> Vec<SignalSample> {
        self.frames
            .iter()
            .filter(|f| f.timestamp_ms >= since_ms && f.can_id == can_id)
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

fn build_cans(shared: &Arc<Mutex<ManagerShared>>) -> HashMap<String, Can> {
    let mut cans: HashMap<String, Can> = HashMap::new();

    #[cfg(feature = "kvaser")]
    match KvaserBackend::new() {
        Ok(backend) => {
            let sh_rx = Arc::clone(shared);
            let sh_tx = Arc::clone(shared);
            cans.insert(
                "kvaser".to_string(),
                Can::new(backend, make_rx_callback("kvaser".into(), sh_rx), make_tx_callback("kvaser".into(), sh_tx)),
            );
        }
        Err(e) => warn!("Kvaser backend unavailable: {e}"),
    }

    #[cfg(feature = "pcan")]
    match PcanBackend::new() {
        Ok(backend) => {
            let sh_rx = Arc::clone(shared);
            let sh_tx = Arc::clone(shared);
            cans.insert(
                "pcan".to_string(),
                Can::new(backend, make_rx_callback("pcan".into(), sh_rx), make_tx_callback("pcan".into(), sh_tx)),
            );
        }
        Err(e) => warn!("PCAN backend unavailable: {e}"),
    }

    #[cfg(feature = "linux-can")]
    {
        let sh_rx = Arc::clone(shared);
        let sh_tx = Arc::clone(shared);
        cans.insert(
            "socketcan".to_string(),
            Can::new(
                SocketCanBackend,
                make_rx_callback("socketcan".into(), sh_rx),
                make_tx_callback("socketcan".into(), sh_tx),
            ),
        );
    }

    cans
}

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
    pub fn create_channel(&mut self, backend_name: &str, channel_name: &str) -> Result<u32, String> {
        let hw_index = self
            .cans
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
        lock.index_to_handle.insert((backend_name.to_string(), hw_index), handle);
        lock.handle_to_index.insert(handle, (backend_name.to_string(), hw_index));
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

    /// Open the hardware for a channel registered with `create_channel`, loading
    /// the given DBC (parsed fresh from disk) for decode/encode. Returns the
    /// parsed DBC so the frontend can populate its signal tree.
    pub fn open_channel(&mut self, handle: u32, bitrate: u32, dbc_path: Option<&str>) -> Result<Option<ParsedDbc>, String> {
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
        info!("Reset CAN manager: all channels closed and unregistered");
    }

    /// Re-enumerate hardware in every backend and re-register all previously
    /// created channels (leaving them closed). Returns the old→new handle mapping.
    /// On Kvaser this calls canUnloadLibrary()+canInitializeLibrary() which is the
    /// documented way to detect hardware connected after the initial library init.
    pub fn reload_backends(&mut self) -> Vec<(u32, u32)> {
        let previous: Vec<(u32, ChannelInfo)> = self.shared.lock().ok().map(|lock| {
            lock.channels.iter().map(|(&h, d)| (h, d.info.clone())).collect()
        }).unwrap_or_default();

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
                Ok(new_handle) => remapped.push((old_handle, new_handle)),
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

    pub fn send_frame(&self, handle: u32, can_id: u32, data: Vec<u8>) -> Result<(), String> {
        let (backend_name, hw_index) = self.backend_index(handle)?;
        self.cans.get(&backend_name).ok_or("Backend not found")?.send_once(
            hw_index,
            CanFrame {
                can_id,
                is_extended: can_id > 0x7FF,
                data,
                timestamp_ms: None,
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
        let data = dbc.encode_message(msg_id, signal_values)?;
        self.cans.get(&backend_name).ok_or("Backend not found")?.send_once(
            hw_index,
            CanFrame {
                can_id: msg_id,
                is_extended: msg_id > 0x7FF,
                data,
                timestamp_ms: None,
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
        let data = dbc.encode_message(msg_id, signal_values)?;
        self.cans.get(&backend_name).ok_or("Backend not found")?.add_periodic(
            hw_index,
            CanFrame {
                can_id: msg_id,
                is_extended: msg_id > 0x7FF,
                data,
                timestamp_ms: None,
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
            .flat_map(|ch| ch.frames.iter().map(move |f| (f.timestamp_ms, ch.info.name.as_str(), f)))
            .collect();
        frames.sort_unstable_by_key(|r| r.0);

        let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        let mut writer = BufWriter::new(file);

        writeln!(writer, "timestamp_ms,elapsed_s,channel,can_id,direction,dlc,data,message").map_err(|e| e.to_string())?;

        for (ts, ch_name, f) in &frames {
            let elapsed = (*ts as f64 - start_ms as f64) / 1000.0;
            let id_str = if f.is_extended {
                format!("{:08X}", f.can_id)
            } else {
                format!("{:03X}", f.can_id)
            };
            let data_str = f.data.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(" ");
            let msg_str = f.message_name.as_deref().unwrap_or("").replace('"', "\"\"");
            writeln!(
                writer,
                "{},{:.3},{},{},{},{},\"{}\",\"{}\"",
                ts,
                elapsed,
                ch_name,
                id_str,
                f.direction,
                f.data.len(),
                data_str,
                msg_str
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

        let mut rows: Vec<(u64, &str, &str, f64, &str)> = Vec::new();
        for ch in lock.channels.values() {
            for f in &ch.frames {
                for sig in &f.signals {
                    rows.push((
                        f.timestamp_ms,
                        ch.info.name.as_str(),
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

        writeln!(writer, "timestamp_ms,elapsed_s,channel,signal_name,value,unit").map_err(|e| e.to_string())?;

        for (ts, ch_name, sig_name, value, unit) in &rows {
            let elapsed = (*ts as f64 - start_ms as f64) / 1000.0;
            writeln!(writer, "{},{:.3},{},{},{},{}", ts, elapsed, ch_name, sig_name, value, unit).map_err(|e| e.to_string())?;
        }

        writer.flush().map_err(|e| e.to_string())?;
        Ok(rows.len())
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

fn make_rx_callback(backend: String, shared: Arc<Mutex<ManagerShared>>) -> impl Fn(u8, CanFrame) + Send + Sync + 'static {
    move |hw_index, raw| {
        let ts = raw.timestamp_ms.unwrap_or_else(now_ms);

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

            let decoded_msg = dbc.as_ref().and_then(|d| d.decode_frame(&raw));
            let message_name = decoded_msg.as_ref().map(|m| m.name.clone());

            let stored = StoredFrame {
                can_id: raw.can_id,
                is_extended: raw.is_extended,
                data: raw.data.clone(),
                timestamp_ms: ts,
                direction: "rx",
                message_name: message_name.clone(),
                signals: decoded_msg
                    .map(|m| {
                        m.signals
                            .into_iter()
                            .map(|s| StoredSignal {
                                name: s.name,
                                value: s.physical,
                                unit: s.unit,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
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

fn make_tx_callback(backend: String, shared: Arc<Mutex<ManagerShared>>) -> impl Fn(u8, CanFrame) + Send + Sync + 'static {
    move |hw_index, raw| {
        let ts = raw.timestamp_ms.unwrap_or_else(now_ms);
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
            let decoded_msg = dbc.as_ref().and_then(|d| d.decode_frame(&raw));
            let message_name = decoded_msg.as_ref().map(|m| m.name.clone());
            let stored = StoredFrame {
                can_id: raw.can_id,
                is_extended: raw.is_extended,
                data: raw.data.clone(),
                timestamp_ms: ts,
                direction: "tx",
                message_name: message_name.clone(),
                signals: decoded_msg
                    .map(|m| {
                        m.signals
                            .into_iter()
                            .map(|s| StoredSignal {
                                name: s.name,
                                value: s.physical,
                                unit: s.unit,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
