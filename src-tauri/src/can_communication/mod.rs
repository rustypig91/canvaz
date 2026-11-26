mod socketcand;
pub use socketcand::SocketcandBackend;
pub(crate) use socketcand::{SocketcandConnect, SOCKETCAND_OPEN_CANCELLED};
#[cfg(feature = "kvaser")]
mod kvaser;
#[cfg(feature = "pcan")]
mod pcan;
// SocketCAN is always compiled in on Linux (the socketcan crate is a
// non-optional target dependency there); the feature only exists to force it
// on elsewhere.
#[cfg(any(feature = "linux-can", target_os = "linux"))]
mod socketcan;

use log::{debug, error, info};
use serde::{Deserialize, Serialize};

#[cfg(feature = "kvaser")]
pub use kvaser::KvaserBackend;
#[cfg(feature = "pcan")]
pub use pcan::PcanBackend;
#[cfg(any(feature = "linux-can", target_os = "linux"))]
pub use socketcan::SocketCanBackend;

use std::collections::HashMap;
use std::fmt;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ── Frame ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameFormat {
    #[default]
    Classical,
    Fd,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct FrameMetadata {
    pub format: FrameFormat,
    pub brs: bool,
    /// Received error state indicator. TX backends leave this unset.
    pub esi: bool,
}

impl FrameMetadata {
    pub fn fd(brs: bool, esi: bool) -> Self {
        Self { format: FrameFormat::Fd, brs, esi }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct TxFormat {
    pub format: Option<FrameFormat>,
    pub brs: Option<bool>,
}

impl TxFormat {
    pub fn metadata(self, default: FrameFormat) -> FrameMetadata {
        let brs = self.brs.unwrap_or(false);
        match self.format.unwrap_or(default) {
            FrameFormat::Classical => FrameMetadata { brs, ..FrameMetadata::default() },
            FrameFormat::Fd => FrameMetadata::fd(brs, false),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ChannelSettings {
    pub mode: FrameFormat,
    pub bitrate: u32,
    pub data_bitrate: Option<u32>,
    pub listen_only: bool,
}

impl ChannelSettings {
    pub fn data_bitrate(self) -> Result<u32, String> {
        self.data_bitrate.filter(|rate| *rate >= self.bitrate && self.bitrate > 0)
            .ok_or_else(|| "CAN FD requires positive nominal and data bitrates, with data bitrate at least the nominal bitrate".into())
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ChannelCapabilities {
    /// Unknown until the driver configures the controller, when None.
    pub fd: Option<bool>,
    pub external_timing: bool,
    pub fd_limitation: Option<&'static str>,
}

impl ChannelCapabilities {
    pub fn validate(self, settings: ChannelSettings) -> Result<(), String> {
        if settings.mode == FrameFormat::Fd {
            if self.fd == Some(false) {
                return Err(self.fd_limitation.unwrap_or("This channel does not support ISO CAN FD").into());
            }
            if !self.external_timing {
                settings.data_bitrate()?;
            }
        }
        Ok(())
    }
}

// The controller-specific sample point/SJW controls in #30 can supply
// different phase parameters through this shared timing calculation.
#[cfg(any(feature = "kvaser", feature = "pcan"))]
fn solve_fd_timing(
    clock_hz: u32,
    bitrate: u32,
    (max_tseg1, max_tseg2, max_sjw): (u32, u32, u32),
) -> Option<(u32, u32, u32, u32)> {
    if bitrate == 0 || clock_hz % bitrate != 0 {
        return None;
    }
    let divisor = clock_hz / bitrate;
    (3..=1 + max_tseg1 + max_tseg2)
        .rev()
        .filter_map(|tq| {
            let brp = divisor / tq;
            if brp == 0 || brp > 1024 || divisor % tq != 0 {
                return None;
            }
            let sample = ((tq * 8 + 5) / 10)
                .clamp(2.max(tq.saturating_sub(max_tseg2)), (tq - 1).min(max_tseg1 + 1));
            let tseg1 = sample - 1;
            let tseg2 = tq - sample;
            let error = (f64::from(sample) / f64::from(tq) - 0.8).abs();
            Some(((brp, tseg1, tseg2, tseg1.min(tseg2).min(max_sjw)), error))
        })
        .min_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(timing, _)| timing)
}

pub const FD_LENGTHS: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

pub fn fd_len_to_dlc(len: usize) -> Option<u8> {
    FD_LENGTHS.iter().position(|&size| size == len).map(|code| code as u8)
}

#[cfg(any(feature = "pcan", test))]
pub fn fd_dlc_to_len(dlc: u8) -> Option<usize> {
    FD_LENGTHS.get(usize::from(dlc)).copied()
}

#[derive(Debug, Clone)]
pub struct CanFrame {
    pub metadata: FrameMetadata,
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
    /// Timestamp in milliseconds since the Unix epoch. Set by the backend on
    /// both received frames (from hardware clock) and sent frames (post-send).
    /// `None` on frames that have not yet been sent or received.
    pub timestamp_ms: Option<u64>,
    /// `Some(description)` marks a bus error frame rather than a data frame.
    /// `data` then carries the backend's raw error payload (if any) and
    /// `can_id` is 0 — error frames have no meaningful identifier.
    pub error: Option<String>,
}

impl CanFrame {
    fn validate_transmit(&self) -> Result<(), String> {
        let max_id = if self.is_extended { 0x1FFF_FFFF } else { 0x7FF };
        if self.can_id > max_id {
            return Err("CAN ID is outside the selected frame format's range".into());
        }
        match self.metadata.format {
            FrameFormat::Classical if self.data.len() > 8 => return Err("Classical CAN payloads cannot exceed 8 bytes".into()),
            FrameFormat::Classical if self.metadata.brs => return Err("Bit rate switch requires CAN FD".into()),
            FrameFormat::Fd if fd_len_to_dlc(self.data.len()).is_none() =>
                return Err("CAN FD payload length must be 0–8, 12, 16, 20, 24, 32, 48, or 64 bytes".into()),
            _ => {}
        }
        if self.metadata.esi {
            return Err("ESI is set by the CAN controller on transmission".into());
        }
        if self.error.is_some() {
            return Err("Error frames cannot be transmitted".into());
        }
        Ok(())
    }
}

fn validate_period(period_ms: u64) -> Result<(), String> {
    if period_ms == 0 || Instant::now().checked_add(Duration::from_millis(period_ms)).is_none() {
        return Err("Transmit period must be positive and fit the system clock".into());
    }
    Ok(())
}

// ── Bus status ────────────────────────────────────────────────────────────────

/// CAN controller fault-confinement state, as reported by the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BusState {
    Active,
    Warning,
    Passive,
    BusOff,
}

/// Controller health snapshot polled from the backend about once a second.
/// Fields are `None` where the underlying API does not expose the value.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct BusStatus {
    pub bus_state: Option<BusState>,
    /// Transmit error counter (TEC).
    pub tx_err: Option<u32>,
    /// Receive error counter (REC).
    pub rx_err: Option<u32>,
    /// Number of RX overruns.
    pub overrun: Option<u32>,
}

// ── Error ─────────────────────────────────────────────────────────────────────

// Some variants are only constructed by platform-specific backends (e.g.
// PasswordRequired on Linux), so they read as dead on other targets.
#[derive(Debug)]
pub enum CanOpenError {
    #[allow(dead_code)]
    AlreadyOpen(String),
    #[allow(dead_code)]
    ChannelIndexOutOfRange(String),
    #[allow(dead_code)]
    PasswordRequired,
    Other(String),
}

impl fmt::Display for CanOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CanOpenError::AlreadyOpen(s) => write!(f, "channel already open: {s}"),
            CanOpenError::ChannelIndexOutOfRange(s) => write!(f, "{s}"),
            CanOpenError::PasswordRequired => write!(f, "password required"),
            CanOpenError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl From<String> for CanOpenError {
    fn from(s: String) -> Self {
        CanOpenError::Other(s)
    }
}

// ── Backend traits ────────────────────────────────────────────────────────────

pub trait TxHandle: Send + 'static {
    /// Send `frame`, setting `frame.timestamp_ms` to the transmit time in
    /// milliseconds since the Unix epoch. Backends with hardware clocks use an
    /// hw-derived value; others use a wall-clock approximation captured post-send.
    fn send(&mut self, frame: &mut CanFrame) -> Result<(), String>;
    fn close(&mut self);
}

pub trait RxHandle: Send + 'static {
    /// Block for at most `timeout_ms` milliseconds. Return `None` on timeout.
    fn receive(&mut self, timeout_ms: u64) -> Result<Option<CanFrame>, String>;
    /// Controller status (error counters, bus state), polled ~1/s by the RX
    /// loop. Default: not supported by this backend.
    fn poll_status(&mut self) -> Option<BusStatus> {
        None
    }
    fn close(&mut self);
}

pub trait CanBackend: Send + 'static {
    fn register_channel(&mut self, _name: &str) -> Result<(), String> { Ok(()) }
    fn unregister_channel(&mut self, _index: u8) {}
    fn replace_registrations(&mut self, _removed: &[u8], _names: &[&str]) -> Result<(), String> { Ok(()) }
    fn list_channels(&self) -> Vec<(u8, String)>;
    fn capabilities(&self, _index: u8) -> ChannelCapabilities {
        ChannelCapabilities { fd: None, external_timing: false, fd_limitation: None }
    }
    fn channel_index(&self, name: &str) -> Option<u8> {
        self.list_channels().into_iter().find_map(|(index, channel)| (channel == name).then_some(index))
    }
    /// `listen_only`: open the channel silent (no ACKs, no error flags injected)
    /// so the tool cannot disturb the bus. Backends with external timing and
    /// controller configuration enforce receive-only at the application layer.
    fn open_channel(
        &mut self,
        index: u8,
        settings: ChannelSettings,
        admin_password: Option<&str>,
    ) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), CanOpenError>;
    /// Reset and re-enumerate hardware. Call only after all channels are closed.
    /// Default is a no-op; backends that support hot-plug override this.
    fn reinitialize(&self) {}
}

// Allow backends that return String errors to use ? directly.
impl From<&str> for CanOpenError {
    fn from(s: &str) -> Self {
        CanOpenError::Other(s.to_owned())
    }
}

// ── Send queue ────────────────────────────────────────────────────────────────

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// Produces the data bytes for one transmission of a periodic entry. Runs on
/// the TX thread right before each send, so time-varying payloads (signal
/// generators, counters, checksums) stay jitter-free even when the UI is busy.
pub type FrameDataSource = Box<dyn FnMut() -> Vec<u8> + Send>;

enum SendEntry {
    OneShot(CanFrame),
    Periodic {
        handle: u64,
        frame: CanFrame,
        period_ms: u64,
        next: Instant,
        /// When set, called per tick to regenerate `frame.data`; `None` sends
        /// the static `frame.data` unchanged.
        source: Option<FrameDataSource>,
    },
}

pub struct FrameUpdate {
    pub data: Vec<u8>,
    pub can_id: Option<u32>,
    pub is_extended: Option<bool>,
    pub tx: TxFormat,
}

impl FrameUpdate {
    fn apply(self, previous: &CanFrame) -> CanFrame {
        let format = self.tx.format.unwrap_or(previous.metadata.format);
        let brs = self.tx.brs.unwrap_or(format == FrameFormat::Fd && previous.metadata.brs);
        CanFrame {
            can_id: self.can_id.unwrap_or(previous.can_id),
            is_extended: self.is_extended.unwrap_or(previous.is_extended),
            metadata: FrameMetadata { format, brs, esi: false },
            data: self.data,
            timestamp_ms: None,
            error: None,
        }
    }
}

type SendQueue = Arc<(Mutex<Vec<SendEntry>>, Condvar)>;

// ── Per-channel open state ────────────────────────────────────────────────────

struct OpenChannel {
    settings: ChannelSettings,
    queue: SendQueue,
    stop: Arc<AtomicBool>,
    rx_thread: JoinHandle<()>,
    tx_thread: JoinHandle<()>,
}

// ── Can ───────────────────────────────────────────────────────────────────────

pub struct Can {
    backend: Box<dyn CanBackend>,
    channels: HashMap<u8, OpenChannel>,
    on_rx: Arc<dyn Fn(u8, CanFrame) + Send + Sync + 'static>,
    on_tx: Arc<dyn Fn(u8, CanFrame) + Send + Sync + 'static>,
    /// Called with (channel, error, fatal). `fatal = true` means the RX loop
    /// died and the channel no longer receives; `fatal = false` is a TX send
    /// failure on an otherwise live channel.
    on_error: Arc<dyn Fn(u8, String, bool) + Send + Sync + 'static>,
    /// Called ~1/s with the controller status on backends that support it.
    on_status: Arc<dyn Fn(u8, BusStatus) + Send + Sync + 'static>,
    admin_password: Option<String>,
}

impl Can {
    pub fn new(
        backend: impl CanBackend,
        on_rx: impl Fn(u8, CanFrame) + Send + Sync + 'static,
        on_tx: impl Fn(u8, CanFrame) + Send + Sync + 'static,
        on_error: impl Fn(u8, String, bool) + Send + Sync + 'static,
        on_status: impl Fn(u8, BusStatus) + Send + Sync + 'static,
    ) -> Self {
        Self {
            backend: Box::new(backend),
            channels: HashMap::new(),
            on_rx: Arc::new(on_rx),
            on_tx: Arc::new(on_tx),
            on_error: Arc::new(on_error),
            on_status: Arc::new(on_status),
            admin_password: None,
        }
    }

    pub fn register_channel(&mut self, name: &str) -> Result<(), String> {
        self.backend.register_channel(name)
    }

    pub fn unregister_channel(&mut self, index: u8) {
        self.backend.unregister_channel(index);
    }

    pub fn replace_registrations(&mut self, removed: &[u8], names: &[&str]) -> Result<(), String> {
        self.backend.replace_registrations(removed, names)
    }

    pub fn channel_index(&self, name: &str) -> Option<u8> {
        self.backend.channel_index(name)
    }

    pub fn list_channels(&self) -> Vec<(u8, String)> {
        self.backend.list_channels()
    }

    pub fn capabilities(&self, index: u8) -> ChannelCapabilities {
        self.backend.capabilities(index)
    }

    pub fn reinitialize(&self) {
        self.backend.reinitialize();
    }

    pub(crate) fn ensure_closed(&self, channel: u8) -> Result<(), CanOpenError> {
        if self.is_open(channel) {
            return Err(CanOpenError::AlreadyOpen(format!("Channel {channel} is already open")));
        }
        Ok(())
    }

    pub fn open(&mut self, channel: u8, settings: ChannelSettings, admin_password: Option<&str>) -> Result<(), CanOpenError> {
        self.ensure_closed(channel)?;
        self.capabilities(channel).validate(settings)?;

        if self.admin_password.is_none() && admin_password.is_some() {
            self.admin_password = Some(admin_password.unwrap().to_string());
        }

        let (tx_handle, rx_handle) = self.backend.open_channel(channel, settings, self.admin_password.as_deref())?;
        self.start_channel(channel, settings, tx_handle, rx_handle);
        Ok(())
    }

    pub(crate) fn start_channel(&mut self, channel: u8, settings: ChannelSettings, tx_handle: Box<dyn TxHandle>, rx_handle: Box<dyn RxHandle>) {
        debug_assert!(!self.is_open(channel));
        let queue: SendQueue = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let rx_thread = {
            let stop = Arc::clone(&stop);
            let on_rx = Arc::clone(&self.on_rx);
            let on_error = Arc::clone(&self.on_error);
            let on_status = Arc::clone(&self.on_status);
            std::thread::spawn(move || rx_loop(rx_handle, stop, channel, on_rx, on_error, on_status))
        };

        let tx_thread = {
            let queue = Arc::clone(&queue);
            let stop = Arc::clone(&stop);
            let on_tx = Arc::clone(&self.on_tx);
            let on_error = Arc::clone(&self.on_error);
            std::thread::spawn(move || tx_loop(tx_handle, queue, stop, channel, on_tx, on_error))
        };

        self.channels.insert(
            channel,
            OpenChannel {
                settings,
                queue,
                stop,
                rx_thread,
                tx_thread,
            },
        );
    }

    pub fn close(&mut self, channel: u8) -> Result<(), String> {
        let state = self.channels.remove(&channel).ok_or_else(|| {
            error!("Attempted to close channel {channel} which is not open");
            format!("Channel {channel} is not open")
        })?;
        state.stop.store(true, Ordering::Relaxed);
        let _ = state.rx_thread.join();
        // Synchronize with the TX worker's stop check and entry into the
        // condition wait. RX closes the shared socket before we take the lock.
        {
            let (lock, cvar) = state.queue.as_ref();
            let _queue = lock.lock();
            cvar.notify_one();
        }
        let _ = state.tx_thread.join();
        info!("Closed channel {channel}");
        Ok(())
    }

    pub fn is_open(&self, channel: u8) -> bool {
        self.channels.contains_key(&channel)
    }

    /// Enqueue a frame to be sent exactly once.
    pub fn send_once(&self, channel: u8, frame: CanFrame) -> Result<(), String> {
        self.validate_channel_frame(channel, frame.metadata)?;
        frame.validate_transmit()?;
        debug!("Enqueuing one-shot frame on channel {channel}: id=0x{:X}", frame.can_id);
        let q = self.queue(channel)?;
        let (lock, cvar) = q.as_ref();
        lock.lock().map_err(|_| "Queue lock poisoned".to_string())?.push(SendEntry::OneShot(frame));
        cvar.notify_one();
        Ok(())
    }

    /// Enqueue a frame to be sent repeatedly every `period_ms` milliseconds.
    /// `source`, when given, regenerates the frame data before every send.
    /// Returns a unique handle that can be passed to `update_periodic` /
    /// `remove_periodic`.
    pub fn add_periodic(
        &self,
        channel: u8,
        frame: CanFrame,
        period_ms: u64,
        source: Option<FrameDataSource>,
    ) -> Result<u64, String> {
        self.validate_channel_frame(channel, frame.metadata)?;
        frame.validate_transmit()?;
        validate_period(period_ms)?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        debug!(
            "Adding periodic frame on channel {channel}: id=0x{:X}, period={}ms, handle={handle}",
            frame.can_id, period_ms
        );
        let q = self.queue(channel)?;
        let (lock, cvar) = q.as_ref();
        lock.lock().map_err(|_| "Queue lock poisoned".to_string())?.push(SendEntry::Periodic {
            handle,
            frame,
            period_ms,
            next: Instant::now(),
            source,
        });
        cvar.notify_one();
        Ok(handle)
    }

    /// Update an existing periodic frame and period in place.
    /// Unlike remove + add, the entry's `next` deadline is preserved, so there
    /// is no transmission gap or phase reset; a shortened period only pulls the
    /// deadline earlier.
    pub fn update_periodic(
        &self,
        channel: u8,
        handle: u64,
        replacement: FrameUpdate,
        source: Option<FrameDataSource>,
        period_ms: u64,
    ) -> Result<(), String> {
        validate_period(period_ms)?;
        debug!("Updating periodic frame on channel {channel}: handle={handle}, period={period_ms}ms");
        let q = self.queue(channel)?;
        let (lock, cvar) = q.as_ref();
        let mut entries = lock.lock().map_err(|_| "Queue lock poisoned".to_string())?;
        let found = entries.iter_mut().find_map(|e| match e {
            SendEntry::Periodic {
                handle: h,
                frame,
                period_ms: p,
                next,
                source: s,
            } if *h == handle => Some((frame, p, next, s)),
            _ => None,
        });
        let Some((frame, p, next, s)) = found else {
            return Err(format!("Periodic handle {handle} not found on channel {channel}"));
        };
        let candidate = replacement.apply(frame);
        self.validate_channel_frame(channel, candidate.metadata)?;
        candidate.validate_transmit()?;
        *frame = candidate;
        *s = source;
        *next = (*next).min(Instant::now() + Duration::from_millis(period_ms));
        *p = period_ms;
        drop(entries);
        // Wake the TX loop so a shortened period takes effect immediately
        // instead of after the previously computed sleep.
        cvar.notify_one();
        Ok(())
    }

    /// Remove the periodic entry identified by `handle`.
    pub fn remove_periodic(&self, channel: u8, handle: u64) -> Result<(), String> {
        debug!("Removing periodic frame on channel {channel}: handle={handle}");
        let q = self.queue(channel)?;
        let (lock, _) = q.as_ref();
        lock.lock().map_err(|_| "Queue lock poisoned".to_string())?.retain(|e| match e {
            SendEntry::Periodic { handle: h, .. } => *h != handle,
            _ => true,
        });
        Ok(())
    }

    fn validate_channel_frame(&self, channel: u8, metadata: FrameMetadata) -> Result<(), String> {
        let settings = self.channels.get(&channel).ok_or_else(|| format!("Channel {channel} is not open"))?.settings;
        if settings.listen_only {
            return Err("Channel is listen-only — sending is disabled".into());
        }
        if metadata.format == FrameFormat::Fd && settings.mode != FrameFormat::Fd {
            return Err("Enable CAN FD on the channel before sending FD frames".into());
        }
        Ok(())
    }

    fn queue(&self, channel: u8) -> Result<&SendQueue, String> {
        self.channels
            .get(&channel)
            .map(|s| &s.queue)
            .ok_or_else(|| format!("Channel {channel} is not open"))
    }
}

impl Drop for Can {
    fn drop(&mut self) {
        for state in self.channels.values() {
            state.stop.store(true, Ordering::Relaxed);
            state.queue.1.notify_one();
        }
        // Threads will notice the flag and exit on their next iteration;
        // we don't join here to avoid blocking the drop caller.
    }
}

// ── Thread loops ──────────────────────────────────────────────────────────────

/// How often the RX loop polls the backend for controller status.
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);

fn rx_loop(
    mut rx: Box<dyn RxHandle>,
    stop: Arc<AtomicBool>,
    channel: u8,
    on_rx: Arc<dyn Fn(u8, CanFrame) + Send + Sync>,
    on_error: Arc<dyn Fn(u8, String, bool) + Send + Sync>,
    on_status: Arc<dyn Fn(u8, BusStatus) + Send + Sync>,
) {
    let mut last_status_poll = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        match rx.receive(50) {
            Ok(Some(frame)) => on_rx(channel, frame),
            Ok(None) => {}
            Err(e) => {
                if stop.load(Ordering::Relaxed) { break; }
                error!("RX error on channel {channel}: {e}");
                // Fatal: this loop is the channel's only receive path, so the
                // channel is dead from here on — let the app surface it.
                on_error(channel, e, true);
                break;
            }
        }
        // receive() returns at least every 50 ms, so this fires close to the
        // intended cadence even on a quiet bus.
        if last_status_poll.elapsed() >= STATUS_POLL_INTERVAL {
            last_status_poll = Instant::now();
            if let Some(status) = rx.poll_status() {
                on_status(channel, status);
            }
        }
    }
    rx.close();
}

fn tx_loop(
    mut tx: Box<dyn TxHandle>,
    queue: SendQueue,
    stop: Arc<AtomicBool>,
    channel: u8,
    on_tx: Arc<dyn Fn(u8, CanFrame) + Send + Sync>,
    on_error: Arc<dyn Fn(u8, String, bool) + Send + Sync>,
) {
    let (lock, cvar) = &*queue;
    // Report a TX failure once per distinct error, re-arming after a successful
    // send — a bus-off makes every periodic send fail, and one event per queued
    // frame per tick would flood the app.
    let mut last_tx_err: Option<String> = None;
    let mut report_tx = |result: Result<(), String>, f: CanFrame| match result {
        Ok(()) => {
            last_tx_err = None;
            on_tx(channel, f);
        }
        Err(e) if !stop.load(Ordering::Relaxed) => {
            // Debug, not error: TX failures are continuously visible in the
            // bus-stats footer (bus state, error counters), and a failing bus
            // would otherwise flood the user-facing log via periodic sends.
            debug!("TX error on channel {channel}: {e}");
            if last_tx_err.as_deref() != Some(e.as_str()) {
                on_error(channel, e.clone(), false);
                last_tx_err = Some(e);
            }
        }
        Err(_) => {}
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let now = Instant::now();
        let mut next_deadline: Option<Instant> = None;

        let mut q = match lock.lock() {
            Ok(q) => q,
            Err(_) => break,
        };
        let mut i = 0;
        while i < q.len() && !stop.load(Ordering::Relaxed) {
            match &mut q[i] {
                SendEntry::OneShot(_) => {
                    if let SendEntry::OneShot(mut f) = q.remove(i) {
                        let r = f.validate_transmit().and_then(|()| tx.send(&mut f));
                        report_tx(r, f);
                    }
                }
                SendEntry::Periodic {
                    frame,
                    period_ms,
                    next,
                    source,
                    ..
                } => {
                    if now >= *next {
                        if let Some(src) = source {
                            frame.data = src();
                        }
                        let mut f = frame.clone();
                        *next = now + Duration::from_millis(*period_ms);
                        let r = f.validate_transmit().and_then(|()| tx.send(&mut f));
                        report_tx(r, f);
                    }
                    next_deadline = Some(match next_deadline {
                        Some(t) => t.min(*next),
                        None => *next,
                    });
                    i += 1;
                }
            }
        }

        if stop.load(Ordering::Relaxed) { break; }

        // Sleep until the next periodic deadline, or up to 1 s if there are
        // none. notify_one() in send_once/add_periodic/close wakes us early
        // when new work arrives or the channel is shutting down.
        let timeout = next_deadline
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(1));
        let _ = cvar.wait_timeout(q, timeout);
    }
    tx.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u32, extended: bool, size: usize) -> CanFrame {
        CanFrame { metadata: Default::default(), can_id: id, is_extended: extended, data: vec![0; size], timestamp_ms: None, error: None }
    }

    #[test]
    fn validates_classic_can_transmit_boundaries() {
        for f in [frame(0x7FF, false, 8), frame(0x1FFF_FFFF, true, 8), frame(0, true, 0)] {
            assert!(f.validate_transmit().is_ok());
        }
        for f in [frame(0x800, false, 8), frame(0x2000_0000, true, 8), frame(0, false, 9)] {
            assert!(f.validate_transmit().is_err());
        }
        assert!(validate_period(0).is_err());
        assert!(validate_period(1).is_ok());
    }

    #[test]
    fn fd_dlc_maps_every_wire_length_and_rejects_gaps() {
        for (dlc, length) in [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64].into_iter().enumerate() {
            assert_eq!(fd_dlc_to_len(dlc as u8), Some(length));
            assert_eq!(fd_len_to_dlc(length), Some(dlc as u8));
            let mut fd = frame(0x123, true, length);
            fd.metadata = FrameMetadata::fd(true, false);
            assert!(fd.validate_transmit().is_ok());
        }
        assert_eq!(fd_dlc_to_len(16), None);
        for length in [9, 10, 11, 13, 63, 65] {
            let mut fd = frame(0x123, false, length);
            fd.metadata = FrameMetadata::fd(false, false);
            assert!(fd.validate_transmit().is_err());
        }
        let mut fd = frame(0x123, false, 8);
        fd.metadata = FrameMetadata::fd(false, true);
        assert!(fd.validate_transmit().is_err());
    }

    #[test]
    fn generated_payloads_are_validated_before_reaching_the_driver() {
        struct NoSend;
        impl TxHandle for NoSend {
            fn send(&mut self, _: &mut CanFrame) -> Result<(), String> {
                panic!("oversized generated payload reached the driver");
            }
            fn close(&mut self) {}
        }
        let stop = Arc::new(AtomicBool::new(false));
        let error_stop = stop.clone();
        let mut fd = frame(1, false, 8);
        fd.metadata = FrameMetadata::fd(false, false);
        let queue = Arc::new((Mutex::new(vec![SendEntry::Periodic {
            handle: 1,
            frame: fd,
            period_ms: 1,
            next: Instant::now(),
            source: Some(Box::new(|| vec![0; 9])),
        }]), Condvar::new()));
        tx_loop(Box::new(NoSend), queue, stop.clone(), 0,
            Arc::new(|_, _| panic!("invalid payload reported as transmitted")),
            Arc::new(move |_, error, fatal| {
                assert!(error.contains("CAN FD payload length"));
                assert!(!fatal);
                error_stop.store(true, Ordering::Relaxed);
            }));
        assert!(stop.load(Ordering::Relaxed));
    }

    struct UnusedBackend;
    impl CanBackend for UnusedBackend {
        fn list_channels(&self) -> Vec<(u8, String)> { vec![] }
        fn open_channel(&mut self, _: u8, _: ChannelSettings, _: Option<&str>)
            -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), CanOpenError> { unreachable!() }
    }

    #[test]
    fn invalid_updates_leave_the_previous_periodic_entry_intact() {
        let mut can = Can::new(UnusedBackend, |_, _| {}, |_, _| {}, |_, _, _| {}, |_, _| {});
        let queue = Arc::new((Mutex::new(vec![]), Condvar::new()));
        can.channels.insert(0, OpenChannel {
            settings: ChannelSettings { mode: FrameFormat::Classical, bitrate: 500_000, data_bitrate: None, listen_only: false },
            queue: queue.clone(), stop: Arc::new(AtomicBool::new(false)),
            rx_thread: std::thread::spawn(|| {}), tx_thread: std::thread::spawn(|| {}),
        });
        let mut fd = frame(0x123, true, 64);
        fd.metadata = FrameMetadata::fd(true, false);
        assert!(can.send_once(0, fd.clone()).is_err());
        can.channels.get_mut(&0).unwrap().settings.mode = FrameFormat::Fd;
        can.channels.get_mut(&0).unwrap().settings.listen_only = true;
        assert!(can.add_periodic(0, fd.clone(), 100, None).unwrap_err().contains("listen-only"));
        can.channels.get_mut(&0).unwrap().settings.listen_only = false;
        assert!(can.send_once(0, frame(0x800, false, 1)).is_err());
        assert!(can.add_periodic(0, frame(1, false, 1), 0, None).is_err());
        assert!(queue.0.lock().unwrap().is_empty());
        let handle = can.add_periodic(0, frame(1, false, 1), 100, None).unwrap();
        let update = |data| FrameUpdate { data, can_id: None, is_extended: None, tx: TxFormat::default() };
        assert!(can.update_periodic(0, handle, update(vec![1]), None, 0).is_err());
        assert!(can.update_periodic(0, handle, update(vec![1; 9]), None, 100).is_err());
        let entries = queue.0.lock().unwrap();
        match &entries[0] {
            SendEntry::Periodic { frame, period_ms, .. } => {
                assert_eq!(frame.data, [0]);
                assert_eq!(*period_ms, 100);
            }
            _ => panic!("expected unchanged periodic entry"),
        }
        let deadline = match &entries[0] {
            SendEntry::Periodic { next, .. } => *next,
            _ => unreachable!(),
        };
        drop(entries);
        let update_fd = |data| FrameUpdate {
            data, can_id: Some(fd.can_id), is_extended: Some(true),
            tx: TxFormat { format: Some(FrameFormat::Fd), brs: Some(true) },
        };
        can.update_periodic(0, handle, update_fd(vec![0xAB; 64]), None, 200).unwrap();
        assert!(can.update_periodic(0, handle, update_fd(vec![1; 63]), None, 300).is_err());
        match &queue.0.lock().unwrap()[0] {
            SendEntry::Periodic { frame, period_ms, next, .. } => {
                assert_eq!(frame.metadata, fd.metadata);
                assert!(frame.is_extended);
                assert_eq!(frame.data, vec![0xAB; 64]);
                assert_eq!(*period_ms, 200);
                assert_eq!(*next, deadline);
            }
            _ => panic!("expected updated periodic entry"),
        }
        can.close(0).unwrap();
    }
}
