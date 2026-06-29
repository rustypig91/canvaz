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

const CAN_OK: i32 = 0;
const CAN_ERR_NOMSG: i32 = -2;

// canIOCTL_SET_LOCAL_TXECHO = 32: frames sent on this handle are NOT echoed back
// to other handles open on the same physical channel (including our own RX handle).
const CANIOCTL_SET_LOCAL_TXECHO: u32 = 32;

type FnInit = unsafe extern "system" fn();
type FnUnload = unsafe extern "system" fn() -> i32;
type FnGetCount = unsafe extern "system" fn(*mut i32) -> i32;
type FnOpen = unsafe extern "system" fn(i32, i32) -> i32;
type FnClose = unsafe extern "system" fn(i32) -> i32;
type FnSetBus = unsafe extern "system" fn(i32, c_long, u32, u32, u32, u32, u32) -> i32;
type FnBusOn = unsafe extern "system" fn(i32) -> i32;
type FnBusOff = unsafe extern "system" fn(i32) -> i32;
type FnIoCtl = unsafe extern "system" fn(i32, u32, *mut std::ffi::c_void, u32) -> i32;
type FnReadWait = unsafe extern "system" fn(i32, *mut c_long, *mut u8, *mut u32, *mut u32, *mut c_ulong, c_ulong) -> i32;
type FnWriteWait = unsafe extern "system" fn(i32, c_long, *const u8, u32, u32, c_ulong) -> i32;
type FnReadTimer = unsafe extern "system" fn(i32, *mut u64) -> i32;
type FnGetChannelData = unsafe extern "system" fn(i32, i32, *mut u8, usize) -> i32;

struct CanLib {
    _lib: Library,
    init: FnInit,
    unload: FnUnload,
    get_count: FnGetCount,
    get_channel_data: FnGetChannelData,
    open: FnOpen,
    close: FnClose,
    set_bus: FnSetBus,
    bus_on: FnBusOn,
    bus_off: FnBusOff,
    ioctl: FnIoCtl,
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
            let unload: FnUnload = *lib.get(b"canUnloadLibrary\0").map_err(|e| e.to_string())?;
            init();
            macro_rules! sym {
                ($b:literal, $t:ty) => {
                    *lib.get::<$t>($b).map_err(|e| e.to_string())?
                };
            }
            Ok(Arc::new(Self {
                init,
                unload,
                get_count: sym!(b"canGetNumberOfChannels\0", FnGetCount),
                get_channel_data: sym!(b"canGetChannelData\0", FnGetChannelData),
                open: sym!(b"canOpenChannel\0", FnOpen),
                close: sym!(b"canClose\0", FnClose),
                set_bus: sym!(b"canSetBusParams\0", FnSetBus),
                bus_on: sym!(b"canBusOn\0", FnBusOn),
                bus_off: sym!(b"canBusOff\0", FnBusOff),
                ioctl: sym!(b"canIoCtl\0", FnIoCtl),
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

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64
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

// ── Per-thread OS handle ──────────────────────────────────────────────────────
//
// CANlib docs: "A handle to a CAN circuit should be used in only one thread."
// The TX and RX threads therefore each hold their own OS-level handle. TX echo
// is suppressed via canIOCTL_SET_LOCAL_TXECHO so the RX handle does not receive
// frames that were sent by the TX handle on the same physical channel.

struct KvaserOsHandle {
    lib: Arc<CanLib>,
    handle: i32,
    open_time_ms: u64,
}

unsafe impl Send for KvaserOsHandle {}

impl Drop for KvaserOsHandle {
    fn drop(&mut self) {
        unsafe {
            (self.lib.bus_off)(self.handle);
            (self.lib.close)(self.handle);
        }
    }
}

// ── TX handle ─────────────────────────────────────────────────────────────────

pub(crate) struct KvaserTxHandle(KvaserOsHandle);

unsafe impl Send for KvaserTxHandle {}

impl TxHandle for KvaserTxHandle {
    fn send(&mut self, frame: &mut CanFrame) -> Result<(), String> {
        let h = &self.0;
        let flags = if frame.is_extended { CAN_MSG_EXT } else { 0 };
        let s = unsafe {
            (h.lib.write_wait)(h.handle, frame.can_id as c_long, frame.data.as_ptr(), frame.data.len() as u32, flags, 100)
        };
        if s < CAN_OK {
            return Err(format!("canWriteWait failed: {}", canlib_err(s)));
        }
        let mut hw_ts: u64 = 0;
        frame.timestamp_ms = if unsafe { (h.lib.read_timer)(h.handle, &mut hw_ts) } == CAN_OK {
            Some(h.open_time_ms + hw_ts)
        } else {
            Some(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64)
        };
        Ok(())
    }

    fn close(&mut self) {}
}

// ── RX handle ─────────────────────────────────────────────────────────────────

pub(crate) struct KvaserRxHandle(KvaserOsHandle);

unsafe impl Send for KvaserRxHandle {}

impl RxHandle for KvaserRxHandle {
    fn receive(&mut self, timeout_ms: u64) -> Result<Option<CanFrame>, String> {
        let h = &self.0;
        let mut id: c_long = 0;
        let mut data = [0u8; 8];
        let mut dlc: u32 = 0;
        let mut flags: u32 = 0;
        let mut timestamp: c_ulong = 0;
        let s = unsafe {
            (h.lib.read_wait)(h.handle, &mut id, data.as_mut_ptr(), &mut dlc, &mut flags, &mut timestamp, timeout_ms as c_ulong)
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
            timestamp_ms: Some(h.open_time_ms + timestamp as u64),
        }))
    }

    fn close(&mut self) {}
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct KvaserBackend {
    lib: Arc<CanLib>,
}

impl KvaserBackend {
    pub fn new() -> Result<Self, String> {
        Ok(Self { lib: CanLib::load()? })
    }
}

impl CanBackend for KvaserBackend {
    fn list_channels(&self) -> Vec<String> {
        let mut n: i32 = 0;
        unsafe { (self.lib.get_count)(&mut n) };
        (0..n.max(0) as i32)
            .map(|i| kvaser_channel_name(&self.lib, i).unwrap_or_else(|| format!("Channel {i}")))
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
        let lib = &self.lib;
        let idx = index as i32;

        // TX handle: has init access, sets bitrate, disables local TX echo so the
        // RX handle on the same channel does not receive our own transmitted frames.
        let tx_raw = unsafe { (lib.open)(idx, CAN_OPEN_ACCEPT_VIRTUAL) };
        if tx_raw < 0 {
            return Err(format!("Failed to open Kvaser channel {idx} (TX): {}", canlib_err(tx_raw)).into());
        }
        let s = unsafe { (lib.set_bus)(tx_raw, freq, tseg1, tseg2, sjw, 1, 0) };
        if s < CAN_OK {
            unsafe { (lib.close)(tx_raw) };
            return Err(format!("canSetBusParams failed: {}", canlib_err(s)).into());
        }
        let mut echo_off: u32 = 0;
        unsafe { (lib.ioctl)(tx_raw, CANIOCTL_SET_LOCAL_TXECHO, &mut echo_off as *mut _ as *mut _, 4) };
        let s = unsafe { (lib.bus_on)(tx_raw) };
        if s < CAN_OK {
            unsafe { (lib.close)(tx_raw) };
            return Err(format!("canBusOn (TX) failed: {}", canlib_err(s)).into());
        }

        let open_time_ms = now_ms();

        // RX handle: separate OS handle per CANlib threading requirements.
        // canBusOn must be called once per handle even on the same physical channel.
        let rx_raw = unsafe { (lib.open)(idx, CAN_OPEN_ACCEPT_VIRTUAL) };
        if rx_raw < 0 {
            unsafe { (lib.bus_off)(tx_raw); (lib.close)(tx_raw) };
            return Err(format!("Failed to open Kvaser channel {idx} (RX): {}", canlib_err(rx_raw)).into());
        }
        let s = unsafe { (lib.bus_on)(rx_raw) };
        if s < CAN_OK {
            unsafe { (lib.bus_off)(tx_raw); (lib.close)(tx_raw); (lib.close)(rx_raw) };
            return Err(format!("canBusOn (RX) failed: {}", canlib_err(s)).into());
        }

        let lib = Arc::clone(lib);
        Ok((
            Box::new(KvaserTxHandle(KvaserOsHandle { lib: Arc::clone(&lib), handle: tx_raw, open_time_ms })),
            Box::new(KvaserRxHandle(KvaserOsHandle { lib, handle: rx_raw, open_time_ms })),
        ))
    }

    fn reinitialize(&self) {
        // canUnloadLibrary() resets CANlib's internal "already initialised" flag
        // and device list. The subsequent canInitializeLibrary() then performs a
        // true fresh enumeration and picks up hardware connected since the last
        // call. This is the documented API path for hot-plug support within a
        // running process; see Kvaser CANlib user guide, "Initialization" chapter.
        // Must only be called when all channel handles are closed.
        unsafe {
            (self.lib.unload)();
            (self.lib.init)();
        }
    }
}
