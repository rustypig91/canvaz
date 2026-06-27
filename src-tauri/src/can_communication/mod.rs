#[cfg(feature = "kvaser")]
mod kvaser;
#[cfg(feature = "linux-can")]
mod socketcan;

use log::{debug, error, info, warn};

#[cfg(feature = "kvaser")]
pub use kvaser::KvaserBackend;
#[cfg(feature = "linux-can")]
pub use socketcan::SocketCanBackend;

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ── Frame ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CanFrame {
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
}

// ── Backend traits ────────────────────────────────────────────────────────────

pub trait TxHandle: Send + 'static {
    fn send(&mut self, frame: &CanFrame) -> Result<(), String>;
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
    ) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), String>;
}

// ── Send queue ────────────────────────────────────────────────────────────────

enum SendEntry {
    OneShot(CanFrame),
    Periodic {
        frame: CanFrame,
        period_ms: u64,
        next: Instant,
    },
}

type SendQueue = Arc<Mutex<Vec<SendEntry>>>;

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
}

impl Can {
    pub fn new(
        backend: impl CanBackend,
        on_rx: impl Fn(u8, CanFrame) + Send + Sync + 'static,
        on_tx: impl Fn(u8, CanFrame) + Send + Sync + 'static,
    ) -> Self {
        Self {
            backend: Box::new(backend),
            channels: HashMap::new(),
            on_rx: Arc::new(on_rx),
            on_tx: Arc::new(on_tx),
        }
    }

    pub fn list_channels(&self) -> Vec<String> {
        self.backend.list_channels()
    }

    pub fn open(&mut self, channel: u8, bitrate: u32) -> Result<(), String> {
        if self.channels.contains_key(&channel) {
            return Err(format!("Channel {channel} is already open"));
        }

        let (tx_handle, rx_handle) = self.backend.open_channel(channel, bitrate)?;

        let queue: SendQueue = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let rx_thread = {
            let stop = Arc::clone(&stop);
            let on_rx = Arc::clone(&self.on_rx);
            std::thread::spawn(move || rx_loop(rx_handle, stop, channel, on_rx))
        };

        let tx_thread = {
            let queue = Arc::clone(&queue);
            let stop = Arc::clone(&stop);
            let on_tx = Arc::clone(&self.on_tx);
            std::thread::spawn(move || tx_loop(tx_handle, queue, stop, channel, on_tx))
        };

        self.channels.insert(channel, OpenChannel { queue, stop, rx_thread, tx_thread });
        info!("Opened channel {channel} with baudrate {bitrate}");
        Ok(())
    }

    pub fn close(&mut self, channel: u8) -> Result<(), String> {
        let state = self
            .channels
            .remove(&channel)
            .ok_or_else(|| format!("Channel {channel} is not open"))?;
        state.stop.store(true, Ordering::Relaxed);
        let _ = state.rx_thread.join();
        let _ = state.tx_thread.join();
        info!("Closed channel {channel}");
        Ok(())
    }

    /// Enqueue a frame to be sent exactly once.
    pub fn send_once(&self, channel: u8, frame: CanFrame) -> Result<(), String> {
        debug!("Enqueuing one-shot frame on channel {channel}: id=0x{:X}", frame.can_id);
        self.queue(channel)?
            .lock()
            .map_err(|_| "Queue lock poisoned".to_string())?
            .push(SendEntry::OneShot(frame));
        Ok(())
    }

    /// Enqueue a frame to be sent repeatedly every `period_ms` milliseconds.
    /// First transmission happens immediately. Identified by `can_id` for removal.
    pub fn add_periodic(&self, channel: u8, frame: CanFrame, period_ms: u64) -> Result<(), String> {
        debug!("Adding periodic frame on channel {channel}: id=0x{:X}, period={}ms", frame.can_id, period_ms);
        self.queue(channel)?
            .lock()
            .map_err(|_| "Queue lock poisoned".to_string())?
            .push(SendEntry::Periodic { frame, period_ms, next: Instant::now() });
        Ok(())
    }

    /// Remove all periodic entries with the given `can_id` from the send queue.
    pub fn remove_periodic(&self, channel: u8, can_id: u32) -> Result<(), String> {
        debug!("Removing periodic frame on channel {channel}: id=0x{:X}", can_id);
        self.queue(channel)?
            .lock()
            .map_err(|_| "Queue lock poisoned".to_string())?
            .retain(|e| !matches!(e, SendEntry::Periodic { frame, .. } if frame.can_id == can_id));
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
) {
    while !stop.load(Ordering::Relaxed) {
        match rx.receive(50) {
            Ok(Some(frame)) => on_rx(channel, frame),
            Ok(None) => {}
            Err(e) => {
                error!("RX error on channel {channel}: {e}");
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
) {
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let mut to_send: Vec<CanFrame> = Vec::new();

        {
            let mut q = match queue.lock() {
                Ok(q) => q,
                Err(_) => break,
            };
            let mut i = 0;
            while i < q.len() {
                match &mut q[i] {
                    SendEntry::OneShot(_) => {
                        if let SendEntry::OneShot(f) = q.remove(i) {
                            to_send.push(f);
                        }
                        // i unchanged — next element has shifted into position i
                    }
                    SendEntry::Periodic { frame, period_ms, next } => {
                        if now >= *next {
                            to_send.push(frame.clone());
                            *next = now + Duration::from_millis(*period_ms);
                        }
                        i += 1;
                    }
                }
            }
        }

        for frame in to_send {
            if tx.send(&frame).is_ok() {
                on_tx(channel, frame);
            }
        }

        std::thread::sleep(Duration::from_millis(1));
    }
    tx.close();
}
