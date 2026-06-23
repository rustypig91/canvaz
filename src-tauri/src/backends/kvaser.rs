use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use can_hal::{CanFrame as HalCanFrame, CanId, Receive, Transmit};
use can_hal_kvaser::{Classic, KvaserChannel, KvaserDriver};

use crate::app_state::AppState;
use super::CanFrame;

// ── Channel enumeration ───────────────────────────────────────────────────────

#[cfg(unix)]
const CANLIB: &str = "libcanlib.so.1";
#[cfg(windows)]
const CANLIB: &str = "canlib32.dll";

fn count_kvaser_channels() -> Result<u32, String> {
    type FnInit = unsafe extern "system" fn();
    type FnCount = unsafe extern "system" fn(&mut i32) -> i32;

    // SAFETY: loading named CANlib symbols with their correct C signatures.
    unsafe {
        let lib = match libloading::Library::new(CANLIB) {
            Ok(l) => {
                println!("Loaded CANlib '{:?}'", l);
                l
            }
            Err(_) => {
                println!("Failed to load CANlib from '{CANLIB}'");
                return Err(format!("Failed to load CANlib from '{CANLIB}'"));
            }
        };
        let Ok(init) = lib.get::<FnInit>(b"canInitializeLibrary\0") else {
            println!("CANlib does not have canInitializeLibrary symbol");
            return Err("CANlib does not have canInitializeLibrary symbol".to_string());
        };
        let Ok(get_count) = lib.get::<FnCount>(b"canGetNumberOfChannels\0") else {
            println!("CANlib does not have canGetNumberOfChannels symbol");
            return Err("CANlib does not have canGetNumberOfChannels symbol".to_string());
        };
        (init)();
        let mut n: i32 = 0;
        let status = (get_count)(&mut n);
        if status != 0 {
            println!("Failed to get number of channels: {}", status);
            return Err(format!("Failed to get number of channels: {}", status));
        }
        println!("CANlib reports {} channels", n);
        Ok(n.max(0) as u32)
    }
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub(crate) struct KvaserBackend;

impl KvaserBackend {
    pub(super) fn name(&self) -> &str {
        "kvaser"
    }

    pub(super) fn list_channels(&self) -> Result<Vec<String>, String> {
        println!("Enumerating Kvaser channels by loading CANlib from '{CANLIB}'");
        let count = count_kvaser_channels()?;
        Ok((0..count).map(|i| format!("Channel {i}")).collect())
    }

    pub(super) fn open_channel(
        &self,
        name: &str,
        bitrate: Option<u32>,
        _state: Arc<AppState>,
    ) -> Result<KvaserBackendChannel, String> {
        let index = name
            .strip_prefix("Channel ")
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| format!("Invalid Kvaser channel name: '{name}'"))?;
        Ok(KvaserBackendChannel {
            name: name.to_string(),
            channel_index: index,
            bitrate: bitrate.unwrap_or(500_000),
            channel: RefCell::new(None),
        })
    }
}

// ── Channel ───────────────────────────────────────────────────────────────────

pub(crate) struct KvaserBackendChannel {
    name: String,
    channel_index: u32,
    bitrate: u32,
    // Wrapped in RefCell so `send` and `receive` (&self) can call &mut methods on the
    // inner channel. Exclusive access is guaranteed by the Mutex<Channel> in CanManager
    // — RefCell borrows never contend across threads.
    channel: RefCell<Option<KvaserChannel<Classic>>>,
}

// RefCell<T>: Send when T: Send. KvaserChannel<Classic>: Send is asserted in can-hal-kvaser.
// The CanManager's Arc<Mutex<Channel>> ensures exclusive access at runtime.
unsafe impl Send for KvaserBackendChannel {}

impl KvaserBackendChannel {
    pub(super) fn open(&mut self) -> Result<(), String> {
        let driver = KvaserDriver::new().map_err(|e| format!("CANlib not available: {e}"))?;
        let ch = driver
            .channel(self.channel_index)
            .classic(self.bitrate)
            .map_err(|e| format!("Unsupported bitrate {}: {e}", self.bitrate))?
            .connect()
            .map_err(|e| {
                format!("Failed to open Kvaser channel {}: {e}", self.channel_index)
            })?;
        *self.channel.borrow_mut() = Some(ch);
        Ok(())
    }

    pub(super) fn close(&mut self) -> Result<(), String> {
        // Dropping KvaserChannel<Classic> calls bus_off + close via its Drop impl.
        *self.channel.borrow_mut() = None;
        Ok(())
    }

    pub(super) fn send(&self, frame: CanFrame) -> Result<(), String> {
        let mut guard = self.channel.borrow_mut();
        let ch = guard.as_mut().ok_or("Channel is not open")?;
        let id = if frame.is_extended {
            CanId::new_extended(frame.can_id)
                .ok_or_else(|| format!("Invalid extended CAN ID: {:#010x}", frame.can_id))?
        } else {
            CanId::new_standard(frame.can_id as u16)
                .ok_or_else(|| format!("Invalid standard CAN ID: {:#05x}", frame.can_id))?
        };
        let hal_frame =
            HalCanFrame::new(id, &frame.data).ok_or("Frame data exceeds 8 bytes")?;
        ch.transmit(&hal_frame).map_err(|e| e.to_string())
    }

    pub(super) fn receive(&self) -> Result<Option<CanFrame>, String> {
        let mut guard = self.channel.borrow_mut();
        let ch = guard.as_mut().ok_or("Channel is not open")?;
        match ch
            .receive_timeout(Duration::from_millis(100))
            .map_err(|e| e.to_string())?
        {
            Some(ts) => {
                let f = ts.into_frame();
                Ok(Some(CanFrame {
                    can_id: f.id().raw(),
                    is_extended: f.id().is_extended(),
                    data: f.data().to_vec(),
                    timestamp_ms: super::now_ms(),
                }))
            }
            None => Ok(None),
        }
    }

    pub(super) fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        if self.bitrate == bitrate {
            return Ok(());
        }
        let was_open = self.channel.borrow().is_some();
        if was_open {
            self.close()?;
        }
        self.bitrate = bitrate;
        if was_open {
            self.open()?;
        }
        Ok(())
    }
}

// Silence the unused field warning on `name` — it may be useful for diagnostics.
#[allow(dead_code)]
impl KvaserBackendChannel {
    fn name(&self) -> &str { &self.name }
}
