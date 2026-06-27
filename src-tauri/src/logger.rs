use std::sync::atomic::{AtomicBool, Ordering};

use log::{Level, Metadata, Record};

struct Logger;

static USE_COLORS: AtomicBool = AtomicBool::new(true);
static LOGGER: Logger = Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let ts = utc_time();
        let module = record
            .module_path()
            .unwrap_or("?")
            .split("::")
            .last()
            .unwrap_or("?");

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
    }

    fn flush(&self) {}
}

pub fn init() {
    let colors = std::env::var("NO_COLOR").is_err()
        && std::env::var("TERM").map_or(true, |t| t != "dumb");
    USE_COLORS.store(colors, Ordering::Relaxed);
    log::set_logger(&LOGGER).ok();
    log::set_max_level(log::LevelFilter::Debug);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn level_style(level: Level) -> (&'static str, &'static str) {
    match level {
        Level::Error => ("\x1b[1;31m", "ERROR"),
        Level::Warn  => ("\x1b[1;33m", " WARN"),
        Level::Info  => ("\x1b[1;32m", " INFO"),
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
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        (s / 3600) % 24,
        (s / 60) % 60,
        s % 60,
        ms
    )
}
