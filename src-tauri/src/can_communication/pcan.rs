use std::sync::{Arc, Mutex};

use libloading::Library;

use super::{fd_dlc_to_len, fd_len_to_dlc, solve_fd_timing, BusState, BusStatus, CanBackend, CanFrame, CanOpenError, ChannelCapabilities, ChannelSettings, FrameFormat, FrameMetadata, RxHandle, TxHandle};

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
const PCAN_MESSAGE_FD: u8 = 0x04;
const PCAN_MESSAGE_BRS: u8 = 0x08;
const PCAN_MESSAGE_ESI: u8 = 0x10;
const PCAN_FEATURE_FD_CAPABLE: u32 = 0x01;
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

#[repr(C)]
struct TPCANMsgFd {
    id: u32,
    msg_type: u8,
    dlc: u8,
    data: [u8; 64],
}

#[derive(Clone, Copy)]
struct PcanFdApi {
    initialize: FnInitializeFd,
    read: FnReadFd,
    write: FnWriteFd,
}

type FnInitializeFd = unsafe extern "system" fn(TPCANHandle, *const std::ffi::c_char) -> TPCANStatus;
type FnReadFd = unsafe extern "system" fn(TPCANHandle, *mut TPCANMsgFd, *mut u64) -> TPCANStatus;
type FnWriteFd = unsafe extern "system" fn(TPCANHandle, *mut TPCANMsgFd) -> TPCANStatus;

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
    fd: Option<PcanFdApi>,
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
            let fd = (|| Some(PcanFdApi {
                initialize: *lib.get(b"CAN_InitializeFD\0").ok()?,
                read: *lib.get(b"CAN_ReadFD\0").ok()?,
                write: *lib.get(b"CAN_WriteFD\0").ok()?,
            }))();
            Ok(Arc::new(Self {
                fd,
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
    let desc = match s {
        0x00001 => "TX buffer full",
        0x00002 => "RX overrun",
        PCAN_ERROR_BUSLIGHT => "bus light error",
        PCAN_ERROR_BUSHEAVY => "bus heavy error",
        PCAN_ERROR_BUSOFF => "bus off",
        PCAN_ERROR_BUSPASSIVE => "bus passive",
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

/// Returns handle, display name, and device capabilities for every physically connected channel.
/// Uses PCAN_ATTACHED_CHANNELS (PCAN-Basic v4+) which asks the driver for the
/// current list of present devices, rather than probing 32 fixed handles.
fn enumerate_channels(lib: &PcanLib) -> Vec<(TPCANHandle, String, u32)> {
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
            Some((ch, format!("{hw_name} {bus}"), info.device_features))
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
    fd: Option<PcanFdApi>,
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
        frame.validate_transmit()?;
        let len = frame.data.len();
        let id_flag = if frame.is_extended { PCAN_MESSAGE_EXTENDED } else { 0 };
        let (s, api) = if let Some(fd) = ch.fd {
            let mut msg = TPCANMsgFd { id: frame.can_id, msg_type: id_flag, dlc: len as u8, data: [0; 64] };
            if frame.metadata.format == FrameFormat::Fd {
                msg.msg_type |= PCAN_MESSAGE_FD;
                if frame.metadata.brs { msg.msg_type |= PCAN_MESSAGE_BRS; }
                msg.dlc = fd_len_to_dlc(len).ok_or("Invalid CAN FD payload length")?;
            }
            msg.data[..len].copy_from_slice(&frame.data);
            (unsafe { (fd.write)(ch.handle, &mut msg) }, "CAN_WriteFD")
        } else {
            if frame.metadata.format == FrameFormat::Fd { return Err("PCAN channel requires Classical CAN".into()); }
            let mut msg = TPCANMsg { id: frame.can_id, msg_type: id_flag, len: len as u8, data: [0; 8] };
            msg.data[..len].copy_from_slice(&frame.data);
            (unsafe { (ch.lib.write)(ch.handle, &mut msg) }, "CAN_Write")
        };
        if s != PCAN_ERROR_OK {
            return Err(format!("{api} failed: {}", pcan_err(s)));
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
            let mut msg = TPCANMsgFd { id: 0, msg_type: 0, dlc: 0, data: [0; 64] };
            let (s, hw_ms, api) = if let Some(fd) = ch.fd {
                let mut micros = 0u64;
                let status = unsafe { (fd.read)(ch.handle, &mut msg, &mut micros) };
                (status, micros / 1_000, "CAN_ReadFD")
            } else {
                let mut classic = TPCANMsg { id: 0, msg_type: 0, len: 0, data: [0; 8] };
                let mut ts = TPCANTimestamp { millis: 0, millis_overflow: 0, micros: 0 };
                let status = unsafe { (ch.lib.read)(ch.handle, &mut classic, &mut ts) };
                msg.id = classic.id;
                msg.msg_type = classic.msg_type;
                msg.dlc = classic.len;
                msg.data[..8].copy_from_slice(&classic.data);
                (status, u64::from(ts.millis_overflow) * 0x1_0000_0000 + u64::from(ts.millis), "CAN_Read")
            };
            let s_primary = s & !PCAN_ERROR_ANYBUSERR;

            if s_primary == PCAN_ERROR_OK {
                // Skip RTR, echo and status frames; error frames pass through
                // as distinct error rows, everything else as data frames.
                if msg.msg_type & (PCAN_MESSAGE_RTR | PCAN_MESSAGE_ECHO | PCAN_MESSAGE_STATUS) != 0 {
                    continue;
                }
                let fd_frame = msg.msg_type & PCAN_MESSAGE_FD != 0 && msg.msg_type & PCAN_MESSAGE_ERRFRAME == 0;
                let len = if fd_frame {
                    fd_dlc_to_len(msg.dlc).ok_or_else(|| format!("PCAN returned invalid CAN FD DLC {}", msg.dlc))?
                } else { usize::from(msg.dlc).min(8) };
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
                        metadata: Default::default(),
                        can_id: 0,
                        is_extended: false,
                        data: msg.data[..len].to_vec(),
                        timestamp_ms: Some(abs_ms),
                        error: Some("Bus error frame".to_string()),
                    }));
                }
                return Ok(Some(CanFrame {
                    metadata: if fd_frame {
                        FrameMetadata::fd(msg.msg_type & PCAN_MESSAGE_BRS != 0, msg.msg_type & PCAN_MESSAGE_ESI != 0)
                    } else { FrameMetadata::default() },
                    can_id: msg.id,
                    is_extended: (msg.msg_type & PCAN_MESSAGE_EXTENDED) != 0,
                    data: msg.data[..len].to_vec(),
                    timestamp_ms: Some(abs_ms),
                    error: None,
                }));
            } else if s_primary == PCAN_ERROR_QRCVEMPTY {
                if std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            } else {
                return Err(format!("{api} failed: {}", pcan_err(s)));
            }
        }
    }

    fn poll_status(&mut self) -> Option<BusStatus> {
        let ch = &*self.0;
        // SAFETY: handle is a valid initialized channel.
        let s = unsafe { (ch.lib.get_status)(ch.handle) };
        Some(BusStatus {
            bus_state: Some(pcan_bus_state(s)),
            ..BusStatus::default()
        })
    }

    fn close(&mut self) {}
}

fn pcan_bus_state(status: TPCANStatus) -> BusState {
    if status & PCAN_ERROR_BUSOFF != 0 {
        BusState::BusOff
    } else if status & PCAN_ERROR_BUSPASSIVE != 0 {
        BusState::Passive
    } else if status & (PCAN_ERROR_BUSLIGHT | PCAN_ERROR_BUSHEAVY) != 0 {
        BusState::Warning
    } else {
        BusState::Active
    }
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
        enumerate_channels(&self.lib).into_iter().map(|(_, name, _)| name).collect()
    }

    fn capabilities(&self, index: u8) -> ChannelCapabilities {
        let (fd, fd_limitation) = if self.lib.fd.is_none() {
            (Some(false), Some("Installed PCAN-Basic driver does not expose CAN FD"))
        } else {
            let channels = enumerate_channels(&self.lib);
            let fd = channels.get(index as usize).map(|(_, _, features)| features & PCAN_FEATURE_FD_CAPABLE != 0);
            (fd, (fd == Some(false)).then_some("This PCAN adapter does not support CAN FD"))
        };
        ChannelCapabilities { fd, external_timing: false, fd_limitation }
    }

    fn open_channel(
        &mut self,
        index: u8,
        settings: ChannelSettings,
        _admin_password: Option<&str>,
    ) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), CanOpenError> {
        let bitrate = settings.bitrate;
        let listen_only = settings.listen_only;
        let fd = if settings.mode == FrameFormat::Fd {
            let api = self.lib.fd.ok_or("Installed PCAN-Basic driver does not expose CAN FD")?;
            Some((api, pcan_fd_bitrate(bitrate, settings.data_bitrate()?)?))
        } else {
            None
        };
        let baud = if fd.is_none() {
            pcan_baud(bitrate).ok_or_else(|| format!(
                "Unsupported PCAN baud rate {bitrate} bps (supported: 5k, 10k, 20k, 50k, 100k, 125k, 250k, 500k, 1M)"
            ))?
        } else { 0 };

        let lib = Arc::clone(&self.lib);
        let channels = enumerate_channels(&lib);
        let &(pcan_handle, _, features) = channels
            .get(index as usize)
            .ok_or_else(|| {
                CanOpenError::ChannelIndexOutOfRange(format!(
                    "PCAN channel index {index} out of range ({} found)",
                    channels.len()
                ))
            })?;
        if fd.is_some() && features & PCAN_FEATURE_FD_CAPABLE == 0 {
            return Err("This PCAN adapter does not support CAN FD".into());
        }

        // Uninitialize first in case the channel was left open by a previous
        // crash. CAN_Uninitialize on an already-closed channel is a no-op.
        unsafe { (lib.uninitialize)(pcan_handle) };

        // PCAN_LISTEN_ONLY must be set before CAN_Initialize — it configures
        // the driver's transmit path at initialization time.
        let mut listen_only_val: u32 = if listen_only { PCAN_PARAMETER_ON } else { PCAN_PARAMETER_OFF };
        let s = unsafe { (lib.set_value)(pcan_handle, PCAN_LISTEN_ONLY, &mut listen_only_val as *mut u32 as *mut u8, 4) };
        if s & !PCAN_ERROR_ANYBUSERR != PCAN_ERROR_OK {
            let reason = format!("CAN_SetValue(PCAN_LISTEN_ONLY) failed: {}", pcan_err(s));
            if listen_only { return Err(reason.into()); }
            log::warn!("{reason}");
        }

        let (s, api) = match &fd {
            Some((api, timing)) => (unsafe { (api.initialize)(pcan_handle, timing.as_ptr()) }, "CAN_InitializeFD"),
            None => (unsafe { (lib.initialize)(pcan_handle, baud, 0, 0, 0) }, "CAN_Initialize"),
        };
        if s & !PCAN_ERROR_ANYBUSERR != PCAN_ERROR_OK {
            return Err(format!("{api} failed: {}", pcan_err(s)).into());
        }

        // Ask the driver to deliver bus error frames through CAN_Read (as
        // PCAN_MESSAGE_ERRFRAME) — off by default. Requires an initialized
        // channel, so this comes after CAN_Initialize.
        let mut err_frames_val: u32 = PCAN_PARAMETER_ON;
        let s = unsafe { (lib.set_value)(pcan_handle, PCAN_ALLOW_ERROR_FRAMES, &mut err_frames_val as *mut u32 as *mut u8, 4) };
        if s & !PCAN_ERROR_ANYBUSERR != PCAN_ERROR_OK {
            log::warn!("CAN_SetValue(PCAN_ALLOW_ERROR_FRAMES) failed: {} — error frames will not be shown", pcan_err(s));
        }

        let channel = Arc::new(PcanChannel { lib, handle: pcan_handle, fd: fd.map(|(api, _)| api), ts_anchor: Mutex::new(None) });

        Ok((
            Box::new(PcanTxHandle(Arc::clone(&channel))),
            Box::new(PcanRxHandle(channel)),
        ))
    }
}


fn pcan_fd_bitrate(nominal: u32, data: u32) -> Result<std::ffi::CString, String> {
    // Stay within FD timing limits of older PEAK controllers as well as
    // newer hardware. Use a common clock for both phases.
    let (clock, (nom_brp, nom_tseg1, nom_tseg2, nom_sjw), (data_brp, data_tseg1, data_tseg2, data_sjw)) =
        [80, 60, 40, 30, 24, 20].into_iter().find_map(|clock| {
            let nom_timing = solve_fd_timing(clock * 1_000_000, nominal, (64, 16, 16))?;
            let data_timing = solve_fd_timing(clock * 1_000_000, data, (16, 8, 4))?;
            Some((clock, nom_timing, data_timing))
        }).ok_or_else(|| format!("Cannot compute PCAN CAN FD timing for {nominal} bps nominal and {data} bps data"))?;
    let timing = format!(
        "f_clock_mhz={clock},nom_brp={nom_brp},nom_tseg1={nom_tseg1},nom_tseg2={nom_tseg2},nom_sjw={nom_sjw},\
        data_brp={data_brp},data_tseg1={data_tseg1},data_tseg2={data_tseg2},data_sjw={data_sjw}"
    );
    std::ffi::CString::new(timing).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcan_bus_warning_is_distinct_from_error_passive() {
        assert_eq!(pcan_bus_state(PCAN_ERROR_BUSHEAVY), BusState::Warning);
        assert_eq!(pcan_bus_state(PCAN_ERROR_BUSHEAVY | PCAN_ERROR_BUSPASSIVE), BusState::Passive);
        assert_eq!(pcan_bus_state(PCAN_ERROR_BUSPASSIVE | PCAN_ERROR_BUSOFF), BusState::BusOff);
    }

    #[test]
    fn default_fd_rates_fit_older_peak_controller_limits() {
        let timing = pcan_fd_bitrate(500_000, 2_000_000).unwrap();
        assert_eq!(timing.to_str().unwrap(),
            "f_clock_mhz=80,nom_brp=2,nom_tseg1=63,nom_tseg2=16,nom_sjw=16,data_brp=2,data_tseg1=15,data_tseg2=4,data_sjw=4");
        assert!(pcan_fd_bitrate(500_000, 8_000_000).is_ok());
        assert!(pcan_fd_bitrate(0, 2_000_000).is_err());
    }
}
