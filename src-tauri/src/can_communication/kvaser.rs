use std::os::raw::{c_long, c_ulong};
use std::sync::Arc;

use libloading::Library;

use super::{CanBackend, CanFrame, CanOpenError, RxHandle, TxHandle};

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
// Set by CANlib on frames received by an RX handle that were sent by the TX
// handle on the same physical channel (self-reception / loopback echo).
const CAN_MSG_TX: u32 = 0x0040;

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
type FnWriteWait = unsafe extern "system" fn(i32, c_long, *const u8, u32, u32, c_ulong) -> i32;
type FnReadTimer = unsafe extern "system" fn(i32, *mut u64) -> i32;
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
    write_wait: FnWriteWait,
    read_timer: FnReadTimer,
}

// SAFETY: all fields are function pointers or a library handle kept for lifetime.
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
                write_wait: sym!(b"canWriteWait\0", FnWriteWait),
                read_timer: sym!(b"kvReadTimer64\0", FnReadTimer),
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

/// Returns `(handle, open_time_ms)` where `open_time_ms` is the wall-clock time
/// in milliseconds since the Unix epoch captured right after `canBusOn` succeeds.
/// Kvaser hardware timestamps are milliseconds relative to this moment, so
/// `open_time_ms + hw_timestamp` gives an absolute epoch-millisecond time.
fn open_handle(lib: &CanLib, index: i32, freq: c_long, tseg1: u32, tseg2: u32, sjw: u32) -> Result<(i32, u64), String> {
    // SAFETY: calling canOpenChannel with valid index and flags
    let handle = unsafe { (lib.open)(index, CAN_OPEN_ACCEPT_VIRTUAL) };
    if handle < 0 {
        return Err(format!("Failed to open Kvaser channel {index}: {}", canlib_err(handle)));
    }
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
    let open_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    Ok((handle, open_time_ms))
}

fn kvaser_channel_name(lib: &CanLib, index: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    let s = unsafe { (lib.get_channel_data)(index, CANLIB_CHANNEL_DATA_NAME, buf.as_mut_ptr(), buf.len()) };
    if s != CAN_OK {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..len].to_vec()).ok()
}

// ── TX handle ─────────────────────────────────────────────────────────────────

pub(crate) struct KvaserTxHandle {
    lib: Arc<CanLib>,
    handle: i32,
    open_time_ms: u64,
}

// SAFETY: handle is accessed only from the TX thread after open_channel returns.
unsafe impl Send for KvaserTxHandle {}

impl TxHandle for KvaserTxHandle {
    fn send(&mut self, frame: &mut CanFrame) -> Result<(), String> {
        let flags = if frame.is_extended { CAN_MSG_EXT } else { 0 };
        // SAFETY: handle is valid; data pointer covers frame.data.len() bytes
        let s = unsafe {
            (self.lib.write_wait)(
                self.handle,
                frame.can_id as c_long,
                frame.data.as_ptr(),
                frame.data.len() as u32,
                flags,
                100,
            )
        };
        if s < CAN_OK {
            return Err(format!("canWriteWait failed: {}", canlib_err(s)));
        }
        let mut hw_ts: u64 = 0;
        frame.timestamp_ms = if unsafe { (self.lib.read_timer)(self.handle, &mut hw_ts) } == CAN_OK {
            Some(self.open_time_ms + hw_ts)
        } else {
            Some(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64)
        };
        Ok(())
    }

    fn close(&mut self) {}
}

impl Drop for KvaserTxHandle {
    fn drop(&mut self) {
        // SAFETY: handle was opened by open_channel and is still valid at drop time.
        unsafe {
            (self.lib.bus_off)(self.handle);
            (self.lib.close)(self.handle);
        }
    }
}

// ── RX handle ─────────────────────────────────────────────────────────────────

pub(crate) struct KvaserRxHandle {
    lib: Arc<CanLib>,
    handle: i32,
    open_time_ms: u64,
}

// SAFETY: handle is accessed only from the RX thread after open_channel returns.
unsafe impl Send for KvaserRxHandle {}

impl RxHandle for KvaserRxHandle {
    fn receive(&mut self, timeout_ms: u64) -> Result<Option<CanFrame>, String> {
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
                timeout_ms as c_ulong,
            )
        };
        if s == CAN_ERR_NOMSG {
            return Ok(None);
        }
        if s < CAN_OK {
            return Err(format!("canReadWait failed: {}", canlib_err(s)));
        }
        if flags & (CAN_MSG_ERROR_FRAME | CAN_MSG_RTR | CAN_MSG_TX) != 0 {
            return Ok(None);
        }
        let dlc = (dlc as usize).min(8);
        Ok(Some(CanFrame {
            can_id: id as u32,
            is_extended: (flags & CAN_MSG_EXT) != 0,
            data: data[..dlc].to_vec(),
            timestamp_ms: Some(self.open_time_ms + timestamp as u64),
        }))
    }

    fn close(&mut self) {}
}

impl Drop for KvaserRxHandle {
    fn drop(&mut self) {
        // SAFETY: handle was opened by open_channel and is still valid at drop time.
        unsafe {
            (self.lib.bus_off)(self.handle);
            (self.lib.close)(self.handle);
        }
    }
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct KvaserBackend;

impl CanBackend for KvaserBackend {
    fn list_channels(&self) -> Vec<String> {
        let lib = match CanLib::load() {
            Ok(l) => l,
            Err(_) => return Vec::new(),
        };
        let mut n: i32 = 0;
        unsafe { (lib.get_count)(&mut n) };
        (0..n.max(0) as i32)
            .map(|i| kvaser_channel_name(&lib, i).unwrap_or_else(|| format!("Channel {i}")))
            .collect()
    }

    fn open_channel(
        &mut self,
        index: u8,
        bitrate: u32,
        _admin_password: Option<&str>,
    ) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), CanOpenError> {
        let (freq, tseg1, tseg2, sjw) = bitrate_params(bitrate).ok_or_else(|| {
            format!(
                "Cannot compute CANlib timing for {} bps \
                 (bitrate must evenly divide 80 MHz Kvaser clock)",
                bitrate
            )
        })?;
        let lib = CanLib::load()?;

        let (tx_handle, tx_open_time_ms) = open_handle(&lib, index as i32, freq, tseg1, tseg2, sjw)?;
        let (rx_handle, open_time_ms) = match open_handle(&lib, index as i32, freq, tseg1, tseg2, sjw) {
            Ok(pair) => pair,
            Err(e) => {
                unsafe {
                    (lib.bus_off)(tx_handle);
                    (lib.close)(tx_handle);
                }
                return Err(CanOpenError::Other(e));
            }
        };

        Ok((
            Box::new(KvaserTxHandle {
                lib: Arc::clone(&lib),
                handle: tx_handle,
                open_time_ms: tx_open_time_ms,
            }),
            Box::new(KvaserRxHandle { lib, handle: rx_handle, open_time_ms }),
        ))
    }
}
