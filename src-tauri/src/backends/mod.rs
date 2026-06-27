#[cfg(feature = "kvaser")]
mod kvaser;
#[cfg(feature = "linux-can")]
mod socketcan;

#[cfg(feature = "kvaser")]
use kvaser::{KvaserBackend, KvaserBackendChannel, KvaserRxChannel};
#[cfg(feature = "linux-can")]
use socketcan::{SocketCanBackend, SocketCanChannel, SocketCanRxChannel};

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, RwLock,
};
use std::thread::JoinHandle;

use serde::Serialize;
use tauri::Emitter;

use crate::app_state::AppState;
use crate::can_frame::{now_ms, CanFrame, Direction};
use crate::dbc_parser::*;

const DEFAULT_WINDOW_MS: u64 = 30_000;

// How long each backend receive() blocks before returning None.
// Must be short enough to notice a close() promptly.
pub(crate) const RECV_TIMEOUT_MS: u64 = 50;

// ── Shared types ──────────────────────────────────────────────────────────────

pub type SubscribedSignals = Arc<RwLock<HashMap<String, HashSet<String>>>>;

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

// ── TX half — stays in Channel, owned by the main thread ─────────────────────
// No mutex required: the outer Arc<Mutex<Channel>> serialises access here.

enum TxChannel {
    #[cfg(feature = "linux-can")]
    SocketCan(SocketCanChannel),
    #[cfg(feature = "kvaser")]
    Kvaser(KvaserBackendChannel),
}

impl TxChannel {
    // Opens the TX side and returns a ready-to-use RX handle for the receive thread.
    fn open(&mut self) -> Result<RxChannel, String> {
        match self {
            #[cfg(feature = "linux-can")]
            TxChannel::SocketCan(c) => c.open().map(RxChannel::SocketCan),
            #[cfg(feature = "kvaser")]
            TxChannel::Kvaser(c) => c.open().map(RxChannel::Kvaser),
        }
    }

    fn close(&mut self) -> Result<(), String> {
        match self {
            #[cfg(feature = "linux-can")]
            TxChannel::SocketCan(c) => c.close(),
            #[cfg(feature = "kvaser")]
            TxChannel::Kvaser(c) => c.close(),
        }
    }

    fn send(&self, frame: CanFrame) -> Result<(), String> {
        match self {
            #[cfg(feature = "linux-can")]
            TxChannel::SocketCan(c) => c.send(frame),
            #[cfg(feature = "kvaser")]
            TxChannel::Kvaser(c) => c.send(frame),
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) {
        match self {
            #[cfg(feature = "linux-can")]
            TxChannel::SocketCan(c) => c.set_bitrate(bitrate),
            #[cfg(feature = "kvaser")]
            TxChannel::Kvaser(c) => c.set_bitrate(bitrate),
        }
    }
}

// ── RX half — handed exclusively to the receive thread ───────────────────────
// No mutex required: only the receive thread ever touches this.

enum RxChannel {
    #[cfg(feature = "linux-can")]
    SocketCan(SocketCanRxChannel),
    #[cfg(feature = "kvaser")]
    Kvaser(KvaserRxChannel),
}

impl RxChannel {
    fn receive(&self) -> Result<Option<CanFrame>, String> {
        match self {
            #[cfg(feature = "linux-can")]
            RxChannel::SocketCan(c) => c.receive(),
            #[cfg(feature = "kvaser")]
            RxChannel::Kvaser(c) => c.receive(),
        }
    }
}

// ── Channel ───────────────────────────────────────────────────────────────────

pub struct Channel {
    tx: TxChannel,
    frames: Arc<Mutex<VecDeque<CanFrame>>>,
    window_ms: Arc<AtomicU64>,
    parsed_dbc: Option<Arc<ParsedDbc>>,
    dbc_path: Option<String>,
    // Thread management
    stop_flag: Arc<AtomicBool>,
    recv_thread: Option<JoinHandle<()>>,
    // Passed to the receive thread for event emission
    app_state: Arc<AppState>,
    channel_id: String,
    subscribed: SubscribedSignals,
}

impl Channel {
    fn new(
        tx: TxChannel,
        dbc_path: Option<String>,
        app_state: Arc<AppState>,
        channel_id: String,
        subscribed: SubscribedSignals,
    ) -> Self {
        let parsed_dbc = dbc_path.as_deref()
            .and_then(|p| ParsedDbc::new(p).ok())
            .map(Arc::new);
        Self {
            tx,
            frames: Arc::new(Mutex::new(VecDeque::new())),
            window_ms: Arc::new(AtomicU64::new(DEFAULT_WINDOW_MS)),
            parsed_dbc,
            dbc_path,
            stop_flag: Arc::new(AtomicBool::new(false)),
            recv_thread: None,
            app_state,
            channel_id,
            subscribed,
        }
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    pub fn open(&mut self) -> Result<(), String> {
        self.frames.lock().map_err(|_| "Lock poisoned".to_string())?.clear();
        if let Some(path) = &self.dbc_path {
            self.parsed_dbc = ParsedDbc::new(path).ok().map(Arc::new);
        }
        self.start()
    }

    pub fn close(&mut self) -> Result<(), String> {
        self.stop_and_close()
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        self.stop_and_close()?;
        self.tx.set_bitrate(bitrate);
        self.start()
    }

    // Opens the TX side, spawns the receive thread with the RX side.
    fn start(&mut self) -> Result<(), String> {
        let rx = self.tx.open()?;
        self.stop_flag.store(false, Ordering::Relaxed);

        let frames = Arc::clone(&self.frames);
        let window_ms = Arc::clone(&self.window_ms);
        let dbc = self.parsed_dbc.clone();
        let stop = Arc::clone(&self.stop_flag);
        let app_state = Arc::clone(&self.app_state);
        let channel_id = self.channel_id.clone();
        let subscribed = Arc::clone(&self.subscribed);

        self.recv_thread = Some(std::thread::spawn(move || {
            recv_loop(rx, frames, window_ms, dbc, stop, app_state, channel_id, subscribed);
        }));

        Ok(())
    }

    // Signals the receive thread to stop, waits for it to exit, then closes TX.
    fn stop_and_close(&mut self) -> Result<(), String> {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(t) = self.recv_thread.take() {
            let _ = t.join();
        }
        self.tx.close()
    }

    // ── Data path ─────────────────────────────────────────────────────────────

    pub fn get_dbc(&self) -> Option<&ParsedDbc> {
        self.parsed_dbc.as_deref()
    }

    pub fn send(&mut self, frame: CanFrame) -> Result<(), String> {
        // No mutex on TX — the outer Arc<Mutex<Channel>> already serialises this.
        self.tx.send(frame.clone())?;
        push_to_ring(&self.frames, frame, self.window_ms.load(Ordering::Relaxed));
        Ok(())
    }

    pub fn send_dbc_message(
        &mut self,
        msg_id: u32,
        signal_values: &HashMap<String, f64>,
        ts: u64,
    ) -> Result<CanFrame, String> {
        let dbc = self.parsed_dbc.as_ref()
            .ok_or_else(|| "No DBC loaded for this channel".to_string())?;
        let msg = dbc.messages.iter().find(|m| m.id == msg_id)
            .ok_or_else(|| format!("Message 0x{:X} not in DBC", msg_id))?;
        let mut buf = vec![0u8; msg.dlc as usize];
        for sig in &msg.signals {
            if let Some(&v) = signal_values.get(&sig.name) {
                encode(&mut buf, v, sig.start_bit, sig.length, sig.little_endian, sig.factor, sig.offset);
            }
        }
        let is_extended = msg_id > 0x7FF;
        let frame = CanFrame {
            can_id: msg_id,
            is_extended,
            data: buf,
            timestamp_ms: ts,
            direction: Direction::Tx,
            decoded: None,
        };
        self.send(frame.clone())?;
        Ok(frame)
    }

    pub fn set_window_ms(&mut self, ms: u64) {
        self.window_ms.store(ms, Ordering::Relaxed);
        let cutoff = now_ms().saturating_sub(ms);
        if let Ok(mut frames) = self.frames.lock() {
            let fresh: VecDeque<CanFrame> = frames.drain(..)
                .filter(|f| f.timestamp_ms >= cutoff)
                .collect();
            *frames = fresh;
        }
    }

    pub fn frames_since(&self, since_ms: u64) -> Vec<CanFrame> {
        self.frames.lock()
            .map(|f| f.iter().filter(|fr| fr.timestamp_ms >= since_ms).cloned().collect())
            .unwrap_or_default()
    }
}

// ── Receive loop ──────────────────────────────────────────────────────────────

fn recv_loop(
    rx: RxChannel,
    frames: Arc<Mutex<VecDeque<CanFrame>>>,
    window_ms: Arc<AtomicU64>,
    dbc: Option<Arc<ParsedDbc>>,
    stop: Arc<AtomicBool>,
    app_state: Arc<AppState>,
    channel_id: String,
    subscribed: SubscribedSignals,
) {
    while !stop.load(Ordering::Relaxed) {
        // receive() blocks for at most RECV_TIMEOUT_MS — no mutex held during the wait
        let mut frame = match rx.receive() {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("CAN read error on '{channel_id}': {e}");
                break;
            }
        };

        if let Some(d) = &dbc {
            frame.decoded = d.parse_frame(&frame);
        }

        let ts = frame.timestamp_ms;
        push_to_ring(&frames, frame.clone(), window_ms.load(Ordering::Relaxed));

        let _ = app_state.app.emit(
            "can-frame",
            CanFrameEvent {
                channel_id: channel_id.clone(),
                can_id: frame.can_id,
                is_extended: frame.is_extended,
                dlc: frame.data.len() as u8,
                data: frame.data.clone(),
                timestamp_ms: ts,
                direction: "rx",
            },
        );

        let sub_guard = match subscribed.read() {
            Ok(g) => g,
            Err(_) => continue,
        };
        let subs = match sub_guard.get(&channel_id) {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        if let Some(decoded) = &frame.decoded {
            for sig in &decoded.signals {
                if subs.contains(&sig.name) {
                    let _ = app_state.app.emit(
                        "signal-value",
                        SignalValueEvent {
                            channel_id: channel_id.clone(),
                            signal_name: sig.name.clone(),
                            message_name: decoded.name.clone(),
                            value: sig.physical,
                            unit: String::new(),
                            timestamp_ms: ts,
                        },
                    );
                }
            }
        }
    }
    // rx is dropped here, which closes the RX handle/socket via Drop.
}

fn push_to_ring(frames: &Mutex<VecDeque<CanFrame>>, frame: CanFrame, window_ms: u64) {
    let cutoff = frame.timestamp_ms.saturating_sub(window_ms);
    if let Ok(mut f) = frames.lock() {
        while f.front().map_or(false, |fr| fr.timestamp_ms < cutoff) {
            f.pop_front();
        }
        f.push_back(frame);
    }
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub enum Backend {
    #[cfg(feature = "linux-can")]
    SocketCan(SocketCanBackend),
    #[cfg(feature = "kvaser")]
    Kvaser(KvaserBackend),
}

impl Backend {
    pub fn name(&self) -> &str {
        match self {
            #[cfg(feature = "linux-can")]
            Backend::SocketCan(backend) => backend.name(),
            #[cfg(feature = "kvaser")]
            Backend::Kvaser(backend) => backend.name(),
        }
    }

    pub fn list_channels(&self) -> Result<Vec<String>, String> {
        match self {
            #[cfg(feature = "linux-can")]
            Backend::SocketCan(backend) => backend.list_channels(),
            #[cfg(feature = "kvaser")]
            Backend::Kvaser(backend) => backend.list_channels(),
        }
    }

    pub fn open_channel(
        &self,
        name: &str,
        bitrate: Option<u32>,
        state: Arc<AppState>,
        dbc_path: Option<&str>,
        channel_id: String,
        subscribed: SubscribedSignals,
    ) -> Result<Channel, String> {
        match self {
            #[cfg(feature = "linux-can")]
            Backend::SocketCan(backend) => {
                let tx = backend.open_channel(name, bitrate, Arc::clone(&state))?;
                Ok(Channel::new(TxChannel::SocketCan(tx), dbc_path.map(str::to_string), state, channel_id, subscribed))
            }
            #[cfg(feature = "kvaser")]
            Backend::Kvaser(backend) => {
                let tx = backend.open_channel(name, bitrate, Arc::clone(&state))?;
                Ok(Channel::new(TxChannel::Kvaser(tx), dbc_path.map(str::to_string), state, channel_id, subscribed))
            }
        }
    }
}

pub fn default_backends() -> Vec<Backend> {
    let mut backends = Vec::new();
    #[cfg(feature = "linux-can")]
    backends.push(Backend::SocketCan(SocketCanBackend));
    #[cfg(feature = "kvaser")]
    backends.push(Backend::Kvaser(KvaserBackend));
    backends
}
