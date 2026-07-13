use std::sync::{Arc, Mutex};

use libloading::Library;

use super::{BusState, BusStatus, CanBackend, CanFrame, CanOpenError, RxHandle, TxHandle};

#[cfg(unix)]
const PCAN_LIB: &str = "libpcanbasic.so";
#[cfg(windows)]
const PCAN_LIB: &str = "PCANBasic.dll";

// ── Type aliases ──────────────────────────────────────────────────────────────

type TPCANHandle = u16;
type TPCANBaudrate = u16;
type TPCANStatus = u32;
type TPCANType = u8;

// ── Channel handle used when no specific channel is required ──────────────────
const PCAN_NONEBUS: TPCANHandle = 0x00;

// ── Constants ──────────────────────────────────────────────────────────────────

const PCAN_ERROR_OK: TPCANStatus = 0x00000;
const PCAN_ERROR_QRCVEMPTY: TPCANStatus = 0x00020;

// Bus-error flags that may be OR'd into an otherwise-valid CAN_Read status.
const PCAN_ERROR_BUSLIGHT: TPCANStatus  = 0x00004;
const PCAN_ERROR_BUSHEAVY: TPCANStatus  = 0x00008;  // = PCAN_ERROR_BUSWARNING
const PCAN_ERROR_BUSOFF: TPCANStatus    = 0x00010;
const PCAN_ERROR_BUSPASSIVE: TPCANStatus = 0x40000;
const PCAN_ERROR_ANYBUSERR: TPCANStatus =
    PCAN_ERROR_BUSLIGHT | PCAN_ERROR_BUSHEAVY | PCAN_ERROR_BUSOFF | PCAN_ERROR_BUSPASSIVE;

// MSGTYPE flags in TPCANMsg
const PCAN_MESSAGE_EXTENDED: u8  = 0x02;
const PCAN_MESSAGE_RTR: u8       = 0x01;
const PCAN_MESSAGE_ECHO: u8      = 0x20;  // TX echo frame (self-reception)
const PCAN_MESSAGE_ERRFRAME: u8  = 0x40;
const PCAN_MESSAGE_STATUS: u8    = 0x80;

// CAN_GetValue parameters (from PCANBasic.h)
#[allow(dead_code)]
const PCAN_CHANNEL_CONDITION: u8      = 0x0D;
#[allow(dead_code)]
const PCAN_HARDWARE_NAME: u8          = 0x0E;
const PCAN_ATTACHED_CHANNELS_COUNT: u8 = 0x2A;
const PCAN_ATTACHED_CHANNELS: u8      = 0x2B;
// CAN_SetValue parameter: listen-only mode (no ACKs, no error frames transmitted).
const PCAN_LISTEN_ONLY: u8 = 0x08;
// CAN_SetValue parameter: deliver bus error frames through CAN_Read
// (msg_type PCAN_MESSAGE_ERRFRAME) instead of suppressing them.
const PCAN_ALLOW_ERROR_FRAMES: u8 = 0x20;
const PCAN_PARAMETER_OFF: u32 = 0x00;
const PCAN_PARAMETER_ON: u32  = 0x01;

const PCAN_CHANNEL_AVAILABLE: u32 = 0x01;
const PCAN_CHANNEL_OCCUPIED: u32  = 0x02;

// Mirrors TPCANChannelInformation from PCANBasic.h (repr(C), 52 bytes total).
// Layout: u16 + u8 + u8 + u32 + [u8;33] + (3 pad) + u32 + u32 = 52.
const PCAN_HARDWARE_NAME_LEN: usize = 33;

#[repr(C)]
struct TPCANChannelInformation {
    channel_handle: TPCANHandle,
    device_type: u8,
    controller_number: u8,
    device_features: u32,
    device_name: [u8; PCAN_HARDWARE_NAME_LEN],
    device_id: u32,
    channel_condition: u32,
}

// ── Predefined baud rates (SJA1000 BTR0/BTR1) ────────────────────────────────

const PCAN_BAUD_1M: TPCANBaudrate = 0x0014;
const PCAN_BAUD_500K: TPCANBaudrate = 0x001C;
const PCAN_BAUD_250K: TPCANBaudrate = 0x011C;
const PCAN_BAUD_125K: TPCANBaudrate = 0x031C;
const PCAN_BAUD_100K: TPCANBaudrate = 0x432F;
const PCAN_BAUD_50K: TPCANBaudrate = 0x472F;
const PCAN_BAUD_20K: TPCANBaudrate = 0x532F;
const PCAN_BAUD_10K: TPCANBaudrate = 0x672F;
const PCAN_BAUD_5K: TPCANBaudrate = 0x7F7F;

// ── C structures ──────────────────────────────────────────────────────────────

#[repr(C)]
struct TPCANMsg {
    id: u32,
    msg_type: u8,
    len: u8,
    data: [u8; 8],
}

// millis + millis_overflow give the milliseconds since CAN_Initialize, where
// millis wraps at 0xFFFFFFFF. total_ms = millis_overflow * 2^32 + millis.
#[repr(C)]
struct TPCANTimestamp {
    millis: u32,
    millis_overflow: u16,
    micros: u16,
}

// ── Function types ────────────────────────────────────────────────────────────

type FnInitialize = unsafe extern "system" fn(TPCANHandle, TPCANBaudrate, TPCANType, u32, u16) -> TPCANStatus;
type FnUninitialize = unsafe extern "system" fn(TPCANHandle) -> TPCANStatus;
type FnRead = unsafe extern "system" fn(TPCANHandle, *mut TPCANMsg, *mut TPCANTimestamp) -> TPCANStatus;
type FnWrite = unsafe extern "system" fn(TPCANHandle, *mut TPCANMsg) -> TPCANStatus;
type FnGetValue = unsafe extern "system" fn(TPCANHandle, u8, *mut u8, u32) -> TPCANStatus;
type FnSetValue = unsafe extern "system" fn(TPCANHandle, u8, *mut u8, u32) -> TPCANStatus;
type FnGetStatus = unsafe extern "system" fn(TPCANHandle) -> TPCANStatus;

// ── Library wrapper ───────────────────────────────────────────────────────────

struct PcanLib {
    _lib: Library,
    initialize: FnInitialize,
    uninitialize: FnUninitialize,
    read: FnRead,
    write: FnWrite,
    get_value: FnGetValue,
    set_value: FnSetValue,
    get_status: FnGetStatus,
}

// SAFETY: PCAN-Basic is documented as thread-safe for concurrent CAN_Read +
// CAN_Write on the same handle from different threads.
unsafe impl Send for PcanLib {}
unsafe impl Sync for PcanLib {}

impl PcanLib {
    fn load() -> Result<Arc<Self>, String> {
        // SAFETY: loading named PCAN-Basic symbols with their correct C signatures.
        unsafe {
            let lib = Library::new(PCAN_LIB)
                .map_err(|e| format!("PCAN-Basic ({PCAN_LIB}) not found: {e}"))?;
            macro_rules! sym {
                ($b:literal, $t:ty) => {
                    *lib.get::<$t>($b).map_err(|e| e.to_string())?
                };
            }
            Ok(Arc::new(Self {
                initialize: sym!(b"CAN_Initialize\0", FnInitialize),
                uninitialize: sym!(b"CAN_Uninitialize\0", FnUninitialize),
                read: sym!(b"CAN_Read\0", FnRead),
                write: sym!(b"CAN_Write\0", FnWrite),
                get_value: sym!(b"CAN_GetValue\0", FnGetValue),
                set_value: sym!(b"CAN_SetValue\0", FnSetValue),
                get_status: sym!(b"CAN_GetStatus\0", FnGetStatus),
                _lib: lib,
            }))
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn pcan_baud(hz: u32) -> Option<TPCANBaudrate> {
    match hz {
        1_000_000 => Some(PCAN_BAUD_1M),
        500_000 => Some(PCAN_BAUD_500K),
        250_000 => Some(PCAN_BAUD_250K),
        125_000 => Some(PCAN_BAUD_125K),
        100_000 => Some(PCAN_BAUD_100K),
        50_000 => Some(PCAN_BAUD_50K),
        20_000 => Some(PCAN_BAUD_20K),
        10_000 => Some(PCAN_BAUD_10K),
        5_000 => Some(PCAN_BAUD_5K),
        _ => None,
    }
}

fn pcan_err(s: TPCANStatus) -> String {
    let desc = match s & !PCAN_ERROR_ANYBUSERR {
        0x00001 => "TX buffer full",
        0x00002 => "RX overrun",
        0x00004 => "bus light error",
        0x00008 => "bus heavy error",
        0x00010 => "bus off",
        0x00020 => "RX queue empty",
        0x00200 => "no driver loaded",
        0x00400 => "hardware in use",
        0x02000 => "resource error",
        0x04000 => "illegal parameter type",
        0x08000 => "illegal parameter value",
        0x10000 => "unknown error",
        0x4000000 => "not initialized",
        _ => "unknown",
    };
    format!("PCAN error 0x{s:05X} ({desc})")
}

/// Returns `(TPCANHandle, display_name)` for every physically connected channel.
/// Uses PCAN_ATTACHED_CHANNELS (PCAN-Basic v4+) which asks the driver for the
/// current list of present devices, rather than probing 32 fixed handles.
fn enumerate_channels(lib: &PcanLib) -> Vec<(TPCANHandle, String)> {
    let mut count: u32 = 0;
    // SAFETY: count is a valid u32; CAN_GetValue writes 4 bytes.
    let s = unsafe {
        (lib.get_value)(PCAN_NONEBUS, PCAN_ATTACHED_CHANNELS_COUNT, &mut count as *mut u32 as *mut u8, 4)
    };
    if s != PCAN_ERROR_OK || count == 0 {
        return Vec::new();
    }

    let struct_size = std::mem::size_of::<TPCANChannelInformation>();
    let total = count as usize * struct_size;
    let mut buf = vec![0u8; total];

    // SAFETY: buf is exactly count * struct_size bytes.
    let s = unsafe {
        (lib.get_value)(PCAN_NONEBUS, PCAN_ATTACHED_CHANNELS, buf.as_mut_ptr(), total as u32)
    };
    if s != PCAN_ERROR_OK {
        return Vec::new();
    }

    (0..count as usize)
        .filter_map(|i| {
            // SAFETY: each struct_size slice starts on a naturally aligned offset
            // because the struct's largest alignment is 4 and the buffer is vec-allocated.
            let info = unsafe { &*(buf[i * struct_size..].as_ptr() as *const TPCANChannelInformation) };
            if info.channel_condition & (PCAN_CHANNEL_AVAILABLE | PCAN_CHANNEL_OCCUPIED) == 0 {
                return None;
            }
            let len = info.device_name.iter().position(|&b| b == 0).unwrap_or(PCAN_HARDWARE_NAME_LEN);
            let hw_name = String::from_utf8_lossy(&info.device_name[..len]);
            let ch = info.channel_handle;
            let bus: u16 = if ch >= 0x51 { ch - 0x50 } else if ch >= 0x41 { ch - 0x40 } else { ch };
            Some((ch, format!("{hw_name} {bus}")))
        })
        .collect()
}

// ── Shared channel state ──────────────────────────────────────────────────────
//
// A single PCAN channel handle is shared between the TX and RX threads via Arc.
// PCAN-Basic does not echo transmitted frames back to the receive queue by
// default, so no TX-flag filtering is needed on the RX side.

struct PcanChannel {
    lib: Arc<PcanLib>,
    handle: TPCANHandle,
    /// Anchors the PCAN hardware timer to wall clock on the first received frame.
    /// (hw_ms_at_first_frame, wall_ms_at_first_frame). None until first frame arrives.
    /// This handles drivers that don't reset their timer at CAN_Initialize.
    ts_anchor: Mutex<Option<(u64, u64)>>,
}

// SAFETY: PCAN-Basic is thread-safe for concurrent CAN_Read + CAN_Write on the
// same handle from different threads.
unsafe impl Send for PcanChannel {}
unsafe impl Sync for PcanChannel {}

impl Drop for PcanChannel {
    fn drop(&mut self) {
        // SAFETY: handle was opened by open_channel; Drop runs exactly once.
        unsafe { (self.lib.uninitialize)(self.handle); }
    }
}

// ── TX handle ─────────────────────────────────────────────────────────────────

pub(crate) struct PcanTxHandle(Arc<PcanChannel>);

unsafe impl Send for PcanTxHandle {}

impl TxHandle for PcanTxHandle {
    fn send(&mut self, frame: &mut CanFrame) -> Result<(), String> {
        let ch = &*self.0;
        let dlc = frame.data.len().min(8);
        let mut msg = TPCANMsg {
            id: frame.can_id,
            msg_type: if frame.is_extended { PCAN_MESSAGE_EXTENDED } else { 0 },
            len: dlc as u8,
            data: [0u8; 8],
        };
        msg.data[..dlc].copy_from_slice(&frame.data[..dlc]);
        // SAFETY: handle is valid; msg is a valid stack TPCANMsg
        let s = unsafe { (ch.lib.write)(ch.handle, &mut msg) };
        if s & !PCAN_ERROR_ANYBUSERR != PCAN_ERROR_OK {
            return Err(format!("CAN_Write failed: {}", pcan_err(s)));
        }
        // PCAN_Write has no built-in hardware timestamp; use wall clock post-send.
        frame.timestamp_ms = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        );
        Ok(())
    }

    fn close(&mut self) {}
}

// ── RX handle ─────────────────────────────────────────────────────────────────

pub(crate) struct PcanRxHandle(Arc<PcanChannel>);

unsafe impl Send for PcanRxHandle {}

impl RxHandle for PcanRxHandle {
    fn receive(&mut self, timeout_ms: u64) -> Result<Option<CanFrame>, String> {
        let ch = &*self.0;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

        loop {
            let mut msg = TPCANMsg { id: 0, msg_type: 0, len: 0, data: [0u8; 8] };
            let mut ts = TPCANTimestamp { millis: 0, millis_overflow: 0, micros: 0 };
            // SAFETY: handle is valid; all out-pointers are valid stack locations
            let s = unsafe { (ch.lib.read)(ch.handle, &mut msg, &mut ts) };
            let s_primary = s & !PCAN_ERROR_ANYBUSERR;

            if s_primary == PCAN_ERROR_OK {
                // Skip RTR, echo and status frames; error frames pass through
                // as distinct error rows, everything else as data frames.
                if msg.msg_type & (PCAN_MESSAGE_RTR | PCAN_MESSAGE_ECHO | PCAN_MESSAGE_STATUS) != 0 {
                    continue;
                }
                let dlc = (msg.len as usize).min(8);
                let hw_ms = ts.millis_overflow as u64 * 0x1_0000_0000 + ts.millis as u64;
                // Anchor the hw timer to wall clock on the first frame. The PCAN driver
                // may not reset its counter at CAN_Initialize (it can count from driver
                // load or device plug-in), so we cannot simply add open_time_ms.
                let abs_ms = {
                    let mut anchor = ch.ts_anchor.lock().unwrap();
                    if anchor.is_none() {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        *anchor = Some((hw_ms, now));
                    }
                    let (hw_base, wall_base) = anchor.unwrap();
                    wall_base.saturating_add(hw_ms.saturating_sub(hw_base))
                };
                if msg.msg_type & PCAN_MESSAGE_ERRFRAME != 0 {
                    // PCAN-Basic reports no structured error detail; the raw
                    // payload is kept as the frame data.
                    return Ok(Some(CanFrame {
                        can_id: 0,
                        is_extended: false,
                        data: msg.data[..dlc].to_vec(),
                        timestamp_ms: Some(abs_ms),
                        error: Some("Bus error frame".to_string()),
                    }));
                }
                return Ok(Some(CanFrame {
                    can_id: msg.id,
                    is_extended: (msg.msg_type & PCAN_MESSAGE_EXTENDED) != 0,
                    data: msg.data[..dlc].to_vec(),
                    timestamp_ms: Some(abs_ms),
                    error: None,
                }));
            } else if s_primary == PCAN_ERROR_QRCVEMPTY {
                if std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            } else {
                return Err(format!("CAN_Read failed: {}", pcan_err(s)));
            }
        }
    }

    fn poll_status(&mut self) -> Option<BusStatus> {
        let ch = &*self.0;
        // SAFETY: handle is a valid initialized channel.
        let s = unsafe { (ch.lib.get_status)(ch.handle) };
        // PCAN-Basic exposes no TEC/REC; only the coarse bus state. BUSHEAVY
        // means an error counter passed 128, which is the error-passive limit.
        let bus_state = if s & PCAN_ERROR_BUSOFF != 0 {
            BusState::BusOff
        } else if s & (PCAN_ERROR_BUSPASSIVE | PCAN_ERROR_BUSHEAVY) != 0 {
            BusState::Passive
        } else if s & PCAN_ERROR_BUSLIGHT != 0 {
            BusState::Warning
        } else {
            BusState::Active
        };
        Some(BusStatus {
            bus_state: Some(bus_state),
            ..BusStatus::default()
        })
    }

    fn close(&mut self) {}
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct PcanBackend {
    lib: Arc<PcanLib>,
}

impl PcanBackend {
    pub fn new() -> Result<Self, String> {
        Ok(Self { lib: PcanLib::load()? })
    }
}

impl CanBackend for PcanBackend {
    fn list_channels(&self) -> Vec<String> {
        enumerate_channels(&self.lib).into_iter().map(|(_, name)| name).collect()
    }

    fn open_channel(
        &mut self,
        index: u8,
        bitrate: u32,
        listen_only: bool,
        _admin_password: Option<&str>,
    ) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), CanOpenError> {
        let baud = pcan_baud(bitrate).ok_or_else(|| {
            format!(
                "Unsupported PCAN baud rate {bitrate} bps \
                 (supported: 5k, 10k, 20k, 50k, 100k, 125k, 250k, 500k, 1M)"
            )
        })?;

        let lib = Arc::clone(&self.lib);
        let channels = enumerate_channels(&lib);
        let (pcan_handle, _) = channels
            .get(index as usize)
            .ok_or_else(|| {
                CanOpenError::ChannelIndexOutOfRange(format!(
                    "PCAN channel index {index} out of range ({} found)",
                    channels.len()
                ))
            })?
            .clone();

        // Uninitialize first in case the channel was left open by a previous
        // crash. CAN_Uninitialize on an already-closed channel is a no-op.
        unsafe { (lib.uninitialize)(pcan_handle) };

        // PCAN_LISTEN_ONLY must be set before CAN_Initialize — it configures
        // the driver's transmit path at initialization time.
        let mut listen_only_val: u32 = if listen_only { PCAN_PARAMETER_ON } else { PCAN_PARAMETER_OFF };
        let s = unsafe { (lib.set_value)(pcan_handle, PCAN_LISTEN_ONLY, &mut listen_only_val as *mut u32 as *mut u8, 4) };
        if s & !PCAN_ERROR_ANYBUSERR != PCAN_ERROR_OK {
            log::warn!("CAN_SetValue(PCAN_LISTEN_ONLY) failed: {} — listen-only may not be applied", pcan_err(s));
        }

        // SAFETY: valid handle, baud, and zeros for the legacy ISA/DNG args
        let s = unsafe { (lib.initialize)(pcan_handle, baud, 0, 0, 0) };
        if s & !PCAN_ERROR_ANYBUSERR != PCAN_ERROR_OK {
            return Err(format!("CAN_Initialize failed: {}", pcan_err(s)).into());
        }

        // Ask the driver to deliver bus error frames through CAN_Read (as
        // PCAN_MESSAGE_ERRFRAME) — off by default. Requires an initialized
        // channel, so this comes after CAN_Initialize.
        let mut err_frames_val: u32 = PCAN_PARAMETER_ON;
        let s = unsafe { (lib.set_value)(pcan_handle, PCAN_ALLOW_ERROR_FRAMES, &mut err_frames_val as *mut u32 as *mut u8, 4) };
        if s & !PCAN_ERROR_ANYBUSERR != PCAN_ERROR_OK {
            log::warn!("CAN_SetValue(PCAN_ALLOW_ERROR_FRAMES) failed: {} — error frames will not be shown", pcan_err(s));
        }

        let channel = Arc::new(PcanChannel { lib, handle: pcan_handle, ts_anchor: Mutex::new(None) });

        Ok((
            Box::new(PcanTxHandle(Arc::clone(&channel))),
            Box::new(PcanRxHandle(channel)),
        ))
    }
}
