use std::os::raw::{c_long, c_ulong};
use std::sync::Arc;

use libloading::Library;

use super::{now_ms, CanFrame, Direction};
use crate::app_state::AppState;

#[cfg(unix)]
const CANLIB: &str = "libcanlib.so.1";
#[cfg(windows)]
const CANLIB: &str = "canlib32.dll";

const CAN_OPEN_ACCEPT_VIRTUAL: i32 = 0x0020;
const CANLIB_CHANNEL_DATA_NAME: i32 = 13;

const BAUD_1M: c_long = -1;
const BAUD_500K: c_long = -2;
const BAUD_250K: c_long = -3;
const BAUD_125K: c_long = -4;
const BAUD_100K: c_long = -5;
const BAUD_62K: c_long = -6;
const BAUD_50K: c_long = -7;

const CAN_MSG_EXT: u32 = 0x0004;
const CAN_MSG_RTR: u32 = 0x0001;
const CAN_MSG_ERROR_FRAME: u32 = 0x0020;

const CAN_OK: i32 = 0;
const CAN_ERR_NOMSG: i32 = -2;

type FnInit = unsafe extern "system" fn();
type FnGetCount = unsafe extern "system" fn(*mut i32) -> i32;
type FnOpen = unsafe extern "system" fn(i32, i32) -> i32;
type FnClose = unsafe extern "system" fn(i32) -> i32;
type FnSetBus = unsafe extern "system" fn(i32, c_long, u32, u32, u32, u32, u32) -> i32;
type FnBusOn = unsafe extern "system" fn(i32) -> i32;
type FnBusOff = unsafe extern "system" fn(i32) -> i32;
type FnReadWait = unsafe extern "system" fn(i32, *mut c_long, *mut u8, *mut u32, *mut u32, *mut c_ulong, c_ulong) -> i32;
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
    read_wait: FnReadWait,
    write: FnWrite,
    write_sync: FnWriteSync,
}

// SAFETY: all fields are function pointers or a library handle kept for lifetime.
// Each CANlib handle is used from exactly one thread at a time.
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
                read_wait: sym!(b"canReadWait\0", FnReadWait),
                write: sym!(b"canWrite\0", FnWrite),
                write_sync: sym!(b"canWriteSync\0", FnWriteSync),
                _lib: lib,
            }))
        }
    }
}

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
    solve_timing(hz)
}

fn solve_timing(hz: u32) -> Option<(c_long, u32, u32, u32)> {
    const CLOCK: u32 = 80_000_000;
    const SP: f32 = 0.70;
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

// Open and configure a single CANlib handle on the given channel index.
fn open_handle(lib: &CanLib, index: i32, freq: c_long, tseg1: u32, tseg2: u32, sjw: u32) -> Result<i32, String> {
    // SAFETY: calling canOpenChannel with valid index and flags
    let handle = unsafe { (lib.open)(index, CAN_OPEN_ACCEPT_VIRTUAL) };
    if handle < 0 {
        return Err(format!("Failed to open Kvaser channel {index}: {}", canlib_err(handle)));
    }
    // When freq is a predefined constant (< 0), tseg/sjw are ignored by CANlib.
    let s = unsafe { (lib.set_bus)(handle, freq, tseg1, tseg2, sjw, 1, 0) };
    if s < CAN_OK {
        unsafe { (lib.close)(handle) };
        return Err(format!("canSetBusParams failed: {}", canlib_err(s)));
    }
    let s = unsafe { (lib.bus_on)(handle) };
    if s < CAN_OK {
        unsafe { (lib.close)(handle) };
        return Err(format!("canBusOn failed: {}", canlib_err(s)));
    }
    Ok(handle)
}

// ── RX half — lives exclusively in the receive thread ─────────────────────────

pub(crate) struct KvaserRxChannel {
    lib: Arc<CanLib>,
    handle: i32,
}

// SAFETY: the handle is used from the receive thread only; no other thread touches it.
unsafe impl Send for KvaserRxChannel {}

impl KvaserRxChannel {
    pub(super) fn receive(&self) -> Result<Option<CanFrame>, String> {
        let mut id: c_long = 0;
        let mut data = [0u8; 8];
        let mut dlc: u32 = 0;
        let mut flags: u32 = 0;
        let mut timestamp: c_ulong = 0;
        // SAFETY: handle is valid; all out-pointers are valid stack locations
        let s = unsafe {
            (self.lib.read_wait)(
                self.handle,
                &mut id,
                data.as_mut_ptr(),
                &mut dlc,
                &mut flags,
                &mut timestamp,
                super::RECV_TIMEOUT_MS as c_ulong,
            )
        };
        if s == CAN_ERR_NOMSG {
            return Ok(None);
        }
        if s < CAN_OK {
            return Err(format!("canReadWait failed: {}", canlib_err(s)));
        }
        if flags & (CAN_MSG_ERROR_FRAME | CAN_MSG_RTR) != 0 {
            return Ok(None);
        }
        let dlc = (dlc as usize).min(8);
        Ok(Some(CanFrame {
            can_id: id as u32,
            is_extended: (flags & CAN_MSG_EXT) != 0,
            data: data[..dlc].to_vec(),
            timestamp_ms: now_ms(),
            direction: Direction::Rx,
            decoded: None,
        }))
    }
}

impl Drop for KvaserRxChannel {
    fn drop(&mut self) {
        // SAFETY: handle was opened by open() and is still valid
        unsafe {
            (self.lib.bus_off)(self.handle);
            (self.lib.close)(self.handle);
        }
    }
}

// ── TX half — stays in Channel, used by the main thread ──────────────────────

pub(crate) struct KvaserBackendChannel {
    channel_index: i32,
    lib: Option<Arc<CanLib>>,
    handle: Option<i32>,
}

// SAFETY: TX handle is accessed only from the main thread (under the outer Channel mutex).
unsafe impl Send for KvaserBackendChannel {}

impl KvaserBackendChannel {
    /// Opens the TX handle and returns a ready-to-use RX handle for the receive thread.
    /// The two handles are independent — no coordination needed between them.
    pub(super) fn open(&mut self, bitrate: u32) -> Result<KvaserRxChannel, String> {
        let (freq, tseg1, tseg2, sjw) = bitrate_params(bitrate).ok_or_else(|| {
            format!(
                "Cannot compute CANlib timing for {} bps \
                 (bitrate must evenly divide 80 MHz Kvaser clock)",
                bitrate
            )
        })?;
        let lib = CanLib::load()?;

        let tx_handle = open_handle(&*lib, self.channel_index, freq, tseg1, tseg2, sjw)?;
        let rx_handle = match open_handle(&*lib, self.channel_index, freq, tseg1, tseg2, sjw) {
            Ok(h) => h,
            Err(e) => {
                unsafe { (lib.bus_off)(tx_handle); (lib.close)(tx_handle) };
                return Err(e);
            }
        };

        self.lib = Some(Arc::clone(&lib));
        self.handle = Some(tx_handle);
        Ok(KvaserRxChannel { lib, handle: rx_handle })
    }

    pub(super) fn close(&mut self) -> Result<(), String> {
        if let (Some(lib), Some(handle)) = (self.lib.take(), self.handle.take()) {
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
            (lib.write)(handle, frame.can_id as c_long, frame.data.as_ptr(), frame.data.len() as u32, flags)
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

    fn lib_and_handle(&self) -> Result<(&Arc<CanLib>, i32), String> {
        match (&self.lib, self.handle) {
            (Some(lib), Some(h)) => Ok((lib, h)),
            _ => Err("Channel is not open".to_string()),
        }
    }
}

// ── Backend ───────────────────────────────────────────────────────────────────

fn kvaser_channel_name(lib: &CanLib, index: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    let s = unsafe { (lib.get_channel_data)(index, CANLIB_CHANNEL_DATA_NAME, buf.as_mut_ptr(), buf.len()) };
    if s != CAN_OK {
        return None;
    }
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
            let ch_name = kvaser_channel_name(&lib, i).unwrap_or_else(|| format!("Channel {i}"));
            names.push(ch_name);
        }
        Ok(names)
    }

    pub(super) fn open_channel(
        &self,
        name: &str,
        _state: Arc<AppState>,
    ) -> Result<KvaserBackendChannel, String> {
        let lib = CanLib::load()?;
        let mut n: i32 = 0;
        unsafe { (lib.get_count)(&mut n) };

        let index = (0..n.max(0) as i32)
            .find(|&i| {
                let ch_name = kvaser_channel_name(&lib, i).unwrap_or_else(|| format!("Channel {i}"));
                ch_name == name
            })
            .ok_or_else(|| format!("Kvaser channel '{name}' not found"))?;

        Ok(KvaserBackendChannel {
            channel_index: index,
            lib: None,
            handle: None,
        })
    }
}
