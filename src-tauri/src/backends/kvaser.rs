use std::os::raw::{c_long, c_ulong};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libloading::Library;

use super::{now_ms, CanFrame};
use crate::app_state::AppState;

#[cfg(unix)]
const CANLIB: &str = "libcanlib.so.1";
#[cfg(windows)]
const CANLIB: &str = "canlib32.dll";

// canOpenChannel flags
const CAN_OPEN_ACCEPT_VIRTUAL: i32 = 0x0020;

// canGetChannelData item IDs
const CANLIB_CHANNEL_DATA_NAME: i32 = 13;

// Predefined CANlib bitrate constants (negative → CANlib picks timing automatically)
const BAUD_1M: c_long = -1;
const BAUD_500K: c_long = -2;
const BAUD_250K: c_long = -3;
const BAUD_125K: c_long = -4;
const BAUD_100K: c_long = -5;
const BAUD_62K: c_long = -6;
const BAUD_50K: c_long = -7;

// CAN message flags
const CAN_MSG_EXT: u32 = 0x0004;
const CAN_MSG_RTR: u32 = 0x0001;
const CAN_MSG_ERROR_FRAME: u32 = 0x0020;

const CAN_OK: i32 = 0;
const CAN_ERR_NOMSG: i32 = -2;

// CANlib function pointer types (extern "system" = __stdcall on Windows, C on Linux)
type FnInit = unsafe extern "system" fn();
type FnGetCount = unsafe extern "system" fn(*mut i32) -> i32;
type FnOpen = unsafe extern "system" fn(i32, i32) -> i32;
type FnClose = unsafe extern "system" fn(i32) -> i32;
type FnSetBus = unsafe extern "system" fn(i32, c_long, u32, u32, u32, u32, u32) -> i32;
type FnBusOn = unsafe extern "system" fn(i32) -> i32;
type FnBusOff = unsafe extern "system" fn(i32) -> i32;
type FnRead = unsafe extern "system" fn(i32, *mut c_long, *mut u8, *mut u32, *mut u32, *mut c_ulong) -> i32;
type FnWrite = unsafe extern "system" fn(i32, c_long, *const u8, u32, u32) -> i32;
type FnWriteSync = unsafe extern "system" fn(i32, c_ulong) -> i32;
type FnGetChannelData = unsafe extern "system" fn(i32, i32, *mut u8, usize) -> i32;

struct CanLib {
    _lib: Library,
    get_count: FnGetCount,
    get_channel_data: FnGetChannelData,
    open: FnOpen,
    close: FnClose,
    set_bus: FnSetBus,
    bus_on: FnBusOn,
    bus_off: FnBusOff,
    read: FnRead,
    write: FnWrite,
    write_sync: FnWriteSync,
}

// SAFETY: all fields are function pointers or a library handle kept for lifetime.
// Each CANlib channel handle is used from exactly one thread at a time.
unsafe impl Send for CanLib {}
unsafe impl Sync for CanLib {}

impl CanLib {
    fn load() -> Result<Arc<Self>, String> {
        // SAFETY: loading named CANlib symbols with their correct C signatures.
        unsafe {
            let lib = Library::new(CANLIB).map_err(|e| format!("CANlib ({CANLIB}) not found: {e}"))?;
            let init: FnInit = *lib.get(b"canInitializeLibrary\0").map_err(|e| e.to_string())?;
            init();
            macro_rules! sym {
                ($b:literal, $t:ty) => {
                    *lib.get::<$t>($b).map_err(|e| e.to_string())?
                };
            }
            Ok(Arc::new(Self {
                get_count: sym!(b"canGetNumberOfChannels\0", FnGetCount),
                get_channel_data: sym!(b"canGetChannelData\0", FnGetChannelData),
                open: sym!(b"canOpenChannel\0", FnOpen),
                close: sym!(b"canClose\0", FnClose),
                set_bus: sym!(b"canSetBusParams\0", FnSetBus),
                bus_on: sym!(b"canBusOn\0", FnBusOn),
                bus_off: sym!(b"canBusOff\0", FnBusOff),
                read: sym!(b"canRead\0", FnRead),
                write: sym!(b"canWrite\0", FnWrite),
                write_sync: sym!(b"canWriteSync\0", FnWriteSync),
                _lib: lib,
            }))
        }
    }
}

/// Map a bitrate in Hz to CANlib timing parameters.
///
/// Standard bitrates map to predefined CANlib constants (negative freq values);
/// CANlib then selects the timing internally and tseg1/tseg2/sjw can be 0.
/// Non-standard bitrates are solved against the 80 MHz Kvaser clock.
/// Returns None if the bitrate cannot be expressed exactly.
fn bitrate_params(hz: u32) -> Option<(c_long, u32, u32, u32)> {
    let predefined = match hz {
        1_000_000 => Some(BAUD_1M),
        500_000 => Some(BAUD_500K),
        250_000 => Some(BAUD_250K),
        125_000 => Some(BAUD_125K),
        100_000 => Some(BAUD_100K),
        62_500 | 62_000 => Some(BAUD_62K),
        50_000 => Some(BAUD_50K),
        _ => None,
    };
    if let Some(freq) = predefined {
        return Some((freq, 0, 0, 0));
    }
    // Solve timing at 80 MHz for non-standard bitrates
    solve_timing(hz)
}

fn solve_timing(hz: u32) -> Option<(c_long, u32, u32, u32)> {
    const CLOCK: u32 = 80_000_000;
    const SP: f32 = 0.70; // 70% sample point
    if hz == 0 || CLOCK % hz != 0 {
        return None;
    }
    let divisor = CLOCK / hz;
    let mut best: Option<((u32, u32, u32), f32)> = None;
    for total_tq in 3..=385u32 {
        if total_tq > divisor {
            break;
        }
        if divisor % total_tq != 0 {
            continue;
        }
        let prescaler = divisor / total_tq;
        if prescaler > 1024 {
            continue;
        }
        let tseg1 = ((total_tq as f32 * SP).round() as u32).saturating_sub(1);
        if tseg1 < 1 || tseg1 > 256 {
            continue;
        }
        let tseg2 = total_tq.saturating_sub(1 + tseg1);
        if tseg2 < 2 || tseg2 > 128 {
            continue;
        }
        let sjw = tseg1.min(tseg2).min(4);
        let actual = (1 + tseg1) as f32 / total_tq as f32;
        let err = (actual - SP).abs();
        if best.as_ref().map_or(true, |(_, e)| err < *e) {
            best = Some(((tseg1, tseg2, sjw), err));
        }
    }
    best.map(|((t1, t2, sjw), _)| (hz as c_long, t1, t2, sjw))
}

fn canlib_err(status: i32) -> String {
    let desc = match status {
        -1 => "error in parameter",
        -2 => "no messages available",
        -3 => "hardware not found",
        -4 => "out of memory",
        -5 => "no channels available",
        -7 => "timeout",
        -8 => "not properly initialized",
        -9 => "out of handles",
        -10 => "invalid handle",
        _ => "unknown error",
    };
    format!("CANlib error {status} ({desc})")
}

// ── Backend ───────────────────────────────────────────────────────────────────

fn kvaser_channel_name(lib: &CanLib, index: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    let s = unsafe {
        (lib.get_channel_data)(
            index,
            CANLIB_CHANNEL_DATA_NAME,
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    if s != CAN_OK { return None; }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..len].to_vec()).ok()
}


pub(crate) struct KvaserBackend;

impl KvaserBackend {
    pub(super) fn name(&self) -> &str {
        "kvaser"
    }

    pub(super) fn list_channels(&self) -> Result<Vec<String>, String> {
        let lib = CanLib::load()?;
        let mut n: i32 = 0;
        let s = unsafe { (lib.get_count)(&mut n) };
        if s < CAN_OK {
            return Err(format!("canGetNumberOfChannels failed: {}", canlib_err(s)));
        }
        let mut names = Vec::new();
        for i in 0..n.max(0) as i32 {
            let ch_name = kvaser_channel_name(&lib, i)
                .unwrap_or_else(|| format!("Channel {i}"));
            names.push(ch_name);
        }
        Ok(names)
    }

    pub(super) fn open_channel(
        &self,
        name: &str,
        bitrate: Option<u32>,
        _state: Arc<AppState>,
    ) -> Result<KvaserBackendChannel, String> {
        let lib = CanLib::load()?;
        let mut n: i32 = 0;
        unsafe { (lib.get_count)(&mut n) };

        let index = (0..n.max(0) as i32)
            .find(|&i| {
                let ch_name = kvaser_channel_name(&lib, i)
                    .unwrap_or_else(|| format!("Channel {i}"));
                ch_name == name
            })
            .ok_or_else(|| format!("Kvaser channel '{name}' not found"))?;

        Ok(KvaserBackendChannel {
            name: name.to_string(),
            channel_index: index,
            bitrate: bitrate.unwrap_or(500_000),
            lib: None,
            handle: None,
        })
    }
}

// ── Channel ───────────────────────────────────────────────────────────────────

pub(crate) struct KvaserBackendChannel {
    name: String,
    channel_index: i32,
    bitrate: u32,
    lib: Option<Arc<CanLib>>,
    handle: Option<i32>,
}

// SAFETY: CANlib channels are owned by one thread at a time via CanManager's Mutex.
unsafe impl Send for KvaserBackendChannel {}

impl KvaserBackendChannel {
    pub(super) fn open(&mut self) -> Result<(), String> {
        let (freq, tseg1, tseg2, sjw) = bitrate_params(self.bitrate).ok_or_else(|| {
            format!(
                "Cannot compute CANlib timing for {} bps \
                 (bitrate must evenly divide 80 MHz Kvaser clock)",
                self.bitrate
            )
        })?;
        let lib = CanLib::load()?;

        // CAN_OPEN_ACCEPT_VIRTUAL allows opening both real and virtual channels.
        let handle = unsafe { (lib.open)(self.channel_index, CAN_OPEN_ACCEPT_VIRTUAL) };
        if handle < 0 {
            return Err(format!(
                "Failed to open Kvaser channel {}: {}",
                self.channel_index,
                canlib_err(handle)
            ));
        }

        let result: Result<(), String> = (|| {
            // When freq is a predefined constant (< 0), tseg1/tseg2/sjw are ignored.
            let s = unsafe { (lib.set_bus)(handle, freq, tseg1, tseg2, sjw, 1, 0) };
            if s < CAN_OK {
                return Err(format!("canSetBusParams failed: {}", canlib_err(s)));
            }
            let s = unsafe { (lib.bus_on)(handle) };
            if s < CAN_OK {
                return Err(format!("canBusOn failed: {}", canlib_err(s)));
            }
            Ok(())
        })();

        if result.is_err() {
            unsafe { (lib.close)(handle) };
            return result;
        }

        self.lib = Some(lib);
        self.handle = Some(handle);
        Ok(())
    }

    pub(super) fn close(&mut self) -> Result<(), String> {
        if let (Some(lib), Some(handle)) = (self.lib.take(), self.handle.take()) {
            // SAFETY: handle was returned by canOpenChannel and is still valid
            unsafe {
                (lib.bus_off)(handle);
                (lib.close)(handle);
            }
        }
        Ok(())
    }

    pub(super) fn send(&self, frame: CanFrame) -> Result<(), String> {
        let (lib, handle) = self.lib_and_handle()?;
        let flags = if frame.is_extended { CAN_MSG_EXT } else { 0 };
        // SAFETY: handle is valid; data pointer is valid for frame.data.len() bytes
        let s = unsafe {
            (lib.write)(
                handle,
                frame.can_id as c_long,
                frame.data.as_ptr(),
                frame.data.len() as u32,
                flags,
            )
        };
        if s < CAN_OK {
            return Err(format!("canWrite failed: {}", canlib_err(s)));
        }
        let s = unsafe { (lib.write_sync)(handle, 100) };
        if s < CAN_OK {
            return Err(format!("canWriteSync failed: {}", canlib_err(s)));
        }
        Ok(())
    }

    pub(super) fn receive(&self) -> Result<Option<CanFrame>, String> {
        let (lib, handle) = self.lib_and_handle()?;
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            let mut id: c_long = 0;
            let mut data = [0u8; 8];
            let mut dlc: u32 = 0;
            let mut flags: u32 = 0;
            let mut timestamp: c_ulong = 0;
            // SAFETY: handle is valid; all out-pointers point to valid stack locations
            let s = unsafe { (lib.read)(handle, &mut id, data.as_mut_ptr(), &mut dlc, &mut flags, &mut timestamp) };
            if s == CAN_ERR_NOMSG {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            if s < CAN_OK {
                return Err(format!("canRead failed: {}", canlib_err(s)));
            }
            // Skip error frames and RTR frames — they are not data frames
            if flags & (CAN_MSG_ERROR_FRAME | CAN_MSG_RTR) != 0 {
                continue;
            }
            let dlc = (dlc as usize).min(8);
            return Ok(Some(CanFrame {
                can_id: id as u32,
                is_extended: (flags & CAN_MSG_EXT) != 0,
                data: data[..dlc].to_vec(),
                timestamp_ms: now_ms(),
            }));
        }
    }

    pub(super) fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        if self.bitrate == bitrate {
            return Ok(());
        }
        let was_open = self.handle.is_some();
        if was_open {
            self.close()?;
        }
        self.bitrate = bitrate;
        if was_open {
            self.open()?;
        }
        Ok(())
    }

    fn lib_and_handle(&self) -> Result<(Arc<CanLib>, i32), String> {
        match (&self.lib, self.handle) {
            (Some(lib), Some(h)) => Ok((Arc::clone(lib), h)),
            _ => Err("Channel is not open".to_string()),
        }
    }
}

#[allow(dead_code)]
impl KvaserBackendChannel {
    fn name(&self) -> &str {
        &self.name
    }
}
