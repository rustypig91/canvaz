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

#[derive(Debug, Clone)]
pub struct CanFrame {
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
    /// Timestamp in milliseconds since the Unix epoch. Set by the backend on
    /// both received frames (from hardware clock) and sent frames (post-send).
    /// `None` on frames that have not yet been sent or received.
    pub timestamp_ms: Option<u64>,
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
    fn close(&mut self);
}

pub trait CanBackend: Send + 'static {
    fn list_channels(&self) -> Vec<String>;
    fn open_channel(
        &mut self,
        index: u8,
        bitrate: u32,
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

type SendQueue = Arc<(Mutex<Vec<SendEntry>>, Condvar)>;

// ── Per-channel open state ────────────────────────────────────────────────────

struct OpenChannel {
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
    admin_password: Option<String>,
}

impl Can {
    pub fn new(
        backend: impl CanBackend,
        on_rx: impl Fn(u8, CanFrame) + Send + Sync + 'static,
        on_tx: impl Fn(u8, CanFrame) + Send + Sync + 'static,
        on_error: impl Fn(u8, String, bool) + Send + Sync + 'static,
    ) -> Self {
        Self {
            backend: Box::new(backend),
            channels: HashMap::new(),
            on_rx: Arc::new(on_rx),
            on_tx: Arc::new(on_tx),
            on_error: Arc::new(on_error),
            admin_password: None,
        }
    }

    pub fn list_channels(&self) -> Vec<String> {
        self.backend.list_channels()
    }

    pub fn reinitialize(&self) {
        self.backend.reinitialize();
    }

    pub fn open(&mut self, channel: u8, bitrate: u32, admin_password: Option<&str>) -> Result<(), CanOpenError> {
        if self.is_open(channel) {
            error!("Attempted to open channel {channel} which is already open");
            return Err(CanOpenError::AlreadyOpen(format!("Channel {channel} is already open")));
        }

        if self.admin_password.is_none() && admin_password.is_some() {
            self.admin_password = Some(admin_password.unwrap().to_string());
        }

        let (tx_handle, rx_handle) = self.backend.open_channel(channel, bitrate, self.admin_password.as_deref())?;

        let queue: SendQueue = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let rx_thread = {
            let stop = Arc::clone(&stop);
            let on_rx = Arc::clone(&self.on_rx);
            let on_error = Arc::clone(&self.on_error);
            std::thread::spawn(move || rx_loop(rx_handle, stop, channel, on_rx, on_error))
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
                queue,
                stop,
                rx_thread,
                tx_thread,
            },
        );
        info!("Opened channel {channel} with baudrate {bitrate}");
        Ok(())
    }

    pub fn close(&mut self, channel: u8) -> Result<(), String> {
        let state = self.channels.remove(&channel).ok_or_else(|| {
            error!("Attempted to close channel {channel} which is not open");
            format!("Channel {channel} is not open")
        })?;
        state.stop.store(true, Ordering::Relaxed);
        state.queue.1.notify_one();
        let _ = state.rx_thread.join();
        let _ = state.tx_thread.join();
        info!("Closed channel {channel}");
        Ok(())
    }

    pub fn is_open(&self, channel: u8) -> bool {
        self.channels.contains_key(&channel)
    }

    /// Enqueue a frame to be sent exactly once.
    pub fn send_once(&self, channel: u8, frame: CanFrame) -> Result<(), String> {
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

    /// Swap the payload (and period) of an existing periodic entry in place.
    /// Unlike remove + add, the entry's `next` deadline is preserved, so there
    /// is no transmission gap or phase reset; a shortened period only pulls the
    /// deadline earlier.
    pub fn update_periodic(
        &self,
        channel: u8,
        handle: u64,
        data: Vec<u8>,
        source: Option<FrameDataSource>,
        period_ms: u64,
    ) -> Result<(), String> {
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
        frame.data = data;
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

fn rx_loop(
    mut rx: Box<dyn RxHandle>,
    stop: Arc<AtomicBool>,
    channel: u8,
    on_rx: Arc<dyn Fn(u8, CanFrame) + Send + Sync>,
    on_error: Arc<dyn Fn(u8, String, bool) + Send + Sync>,
) {
    while !stop.load(Ordering::Relaxed) {
        match rx.receive(50) {
            Ok(Some(frame)) => on_rx(channel, frame),
            Ok(None) => {}
            Err(e) => {
                error!("RX error on channel {channel}: {e}");
                // Fatal: this loop is the channel's only receive path, so the
                // channel is dead from here on — let the app surface it.
                on_error(channel, e, true);
                break;
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
        Err(e) => {
            error!("TX error on channel {channel}: {e}");
            if last_tx_err.as_deref() != Some(e.as_str()) {
                on_error(channel, e.clone(), false);
                last_tx_err = Some(e);
            }
        }
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
        while i < q.len() {
            match &mut q[i] {
                SendEntry::OneShot(_) => {
                    if let SendEntry::OneShot(mut f) = q.remove(i) {
                        let r = tx.send(&mut f);
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
                        let r = tx.send(&mut f);
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
