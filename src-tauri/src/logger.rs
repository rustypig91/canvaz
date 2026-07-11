use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use log::{Level, Metadata, Record};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

struct Logger;

static USE_COLORS: AtomicBool = AtomicBool::new(true);
static LOGGER: Logger = Logger;

// ── Frontend pipe ─────────────────────────────────────────────────────────────

/// One log record as delivered to the frontend console. `seq` increases
/// monotonically so the frontend can dedup history against live events.
#[derive(Clone, Serialize)]
pub struct LogEntry {
    pub seq: u64,
    pub ts: String,
    pub level: String,
    pub module: String,
    pub message: String,
}

/// Every record is kept in a ring buffer (so the frontend can fetch the full
/// history whenever it starts or reloads) and, once the app handle is set,
/// also emitted live as a "rust-log" event.
struct Pipe {
    next_seq: u64,
    history: VecDeque<LogEntry>,
    app: Option<AppHandle>,
}

/// Ring-buffer capacity. Startup records sit at the front and are only
/// evicted once a long session has produced this many newer ones — by which
/// point every frontend load has long since fetched them.
const LOG_HISTORY_MAX: usize = 1000;

static PIPE: Mutex<Pipe> = Mutex::new(Pipe {
    next_seq: 0,
    history: VecDeque::new(),
    app: None,
});

fn publish(mut entry: LogEntry) {
    // Clone the handle out of the lock before emitting: emit can itself log,
    // and re-entering publish() while holding the lock would deadlock.
    let app = {
        let Ok(mut pipe) = PIPE.lock() else { return };
        entry.seq = pipe.next_seq;
        pipe.next_seq += 1;
        if pipe.history.len() >= LOG_HISTORY_MAX {
            pipe.history.pop_front();
        }
        pipe.history.push_back(entry.clone());
        pipe.app.clone()
    };
    if let Some(app) = app {
        let _ = app.emit("rust-log", &entry);
    }
}

/// Enable live "rust-log" events. Called once from the Tauri setup hook;
/// records logged before the webview listens are only in the history.
pub fn set_app(app: AppHandle) {
    if let Ok(mut pipe) = PIPE.lock() {
        pipe.app = Some(app);
    }
}

/// Full log history, oldest first. The frontend calls this on every load and
/// dedups against live events via `seq`.
pub fn history() -> Vec<LogEntry> {
    PIPE.lock().map(|p| p.history.iter().cloned().collect()).unwrap_or_default()
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let ts = utc_time();
        let module = record.module_path().unwrap_or("?").split("::").last().unwrap_or("?");

        if USE_COLORS.load(Ordering::Relaxed) {
            let (color, label) = level_style(record.level());
            // dim timestamp — colored bold label — dim module — message
            println!(
                "\x1b[2m{ts}\x1b[0m {color}{label}\x1b[0m \x1b[2m{module:<18}\x1b[0m {}",
                record.args()
            );
        } else {
            println!("{ts} {:<5} {module:<18} {}", record.level(), record.args());
        }

        publish(LogEntry {
            seq: 0, // assigned by publish() under the pipe lock
            ts,
            level: record.level().to_string(),
            module: module.to_string(),
            message: record.args().to_string(),
        });
    }

    fn flush(&self) {}
}

pub fn init() {
    let colors = std::env::var("NO_COLOR").is_err() && std::env::var("TERM").map_or(true, |t| t != "dumb");
    USE_COLORS.store(colors, Ordering::Relaxed);
    log::set_logger(&LOGGER).ok();
    log::set_max_level(log::LevelFilter::Debug);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn level_style(level: Level) -> (&'static str, &'static str) {
    match level {
        Level::Error => ("\x1b[1;31m", "ERROR"),
        Level::Warn => ("\x1b[1;33m", " WARN"),
        Level::Info => ("\x1b[1;32m", " INFO"),
        Level::Debug => ("\x1b[1;36m", "DEBUG"),
        Level::Trace => ("\x1b[1;37m", "TRACE"),
    }
}

/// Current UTC time as HH:MM:SS.mmm, computed without any external crate.
fn utc_time() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let s = d.as_secs();
    let ms = d.subsec_millis();
    format!("{:02}:{:02}:{:02}.{:03}", (s / 3600) % 24, (s / 60) % 60, s % 60, ms)
}
