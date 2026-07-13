// Release builds must be warning-free; this turns any remaining warning into a
// hard error so it can't slip into a published artifact.
#![cfg_attr(not(debug_assertions), deny(warnings))]

mod app_state;
mod can_communication;
mod can_manager;
mod dbc_parser;
mod j1939;
mod logger;
mod project;
mod sim_generator;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use app_state::AppState;
use can_manager::{CanManager, ChannelInfo, FrameInfo, ManagerState, SignalSample};
use dbc_parser::ParsedDbc;
use project::Project;
use serde::{Deserialize, Serialize};
use sim_generator::SignalGen;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tauri::{Manager, State};

use log::{debug, error, info};

// ── Tauri managed state ───────────────────────────────────────────────────────

struct TauriState {
    app_state: Arc<AppState>,
    can_manager: ManagerState,
    sys: Mutex<System>,
    // Cached on first system_resources call; None = not yet discovered.
    webkit_pids: Mutex<Option<Vec<Pid>>>,
}

// ── Sudo ──────────────────────────────────────────────────────────────────────

#[tauri::command]
fn provide_admin_password(password: Option<String>, state: State<'_, TauriState>) {
    state.app_state.provide_admin_password(password);
}

// ── CAN channel commands ──────────────────────────────────────────────────────

#[tauri::command]
fn list_can_interfaces(state: State<'_, TauriState>) -> Result<Vec<ChannelInfo>, String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.list_channels()
}

#[tauri::command]
fn create_channel(backend_name: String, channel_name: String, state: State<'_, TauriState>) -> Result<can_manager::CreatedChannel, String> {
    state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .create_channel(&backend_name, &channel_name)
}

#[tauri::command]
fn remove_channel(channel_handle: u32, state: State<'_, TauriState>) -> Result<(), String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.remove_channel(channel_handle)
}

#[tauri::command]
fn set_channel_display_name(channel_handle: u32, display_name: Option<String>, state: State<'_, TauriState>) -> Result<(), String> {
    state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .set_channel_display_name(channel_handle, display_name)
}

#[tauri::command]
fn created_channels(state: State<'_, TauriState>) -> Result<Vec<ChannelInfo>, String> {
    Ok(state.can_manager.lock().map_err(|e| e.to_string())?.created_channels_info())
}

#[tauri::command]
async fn open_channel(
    channel_handle: u32,
    bitrate: u32,
    listen_only: Option<bool>,
    dbc_path: Option<String>,
    protocol: Option<String>,
    state: State<'_, TauriState>,
) -> Result<Option<ParsedDbc>, String> {
    let can_manager = Arc::clone(&state.can_manager);
    let proto = can_manager::Protocol::from_config(protocol.as_deref());
    let listen_only = listen_only.unwrap_or(false);
    let result = tauri::async_runtime::spawn_blocking(move || {
        can_manager
            .lock()
            .map_err(|e| e.to_string())?
            .open_channel(channel_handle, bitrate, listen_only, dbc_path.as_deref(), proto)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));

    match &result {
        Ok(_) => info!("Opened channel (handle {}) with baudrate {}", channel_handle, bitrate),
        Err(e) => error!("Failed to open channel {channel_handle}: {e}"),
    }
    result
}

#[tauri::command]
fn close_channel(channel_handle: u32, state: State<'_, TauriState>) -> Result<(), String> {
    debug!("Close channel request: handle={channel_handle}");
    state.can_manager.lock().map_err(|e| e.to_string())?.close_channel(channel_handle)
}

/// Full Rust log history (ring-buffered). The frontend fetches this on every
/// load and follows live "rust-log" events from there, deduping by `seq`.
#[tauri::command]
fn get_logs() -> Vec<logger::LogEntry> {
    logger::history()
}

/// Close all hardware and forget every channel. Called by the frontend on startup
/// so a page reload doesn't collide with channels left open by the previous load.
#[tauri::command]
fn reset_backend(state: State<'_, TauriState>) -> Result<(), String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.reset();
    Ok(())
}

#[derive(Serialize)]
struct RemappedChannel {
    old_handle: u32,
    new_handle: u32,
    /// Backend the channel resolved to after the reload — may differ from the
    /// backend it had before when the name moved backends.
    backend: String,
}

#[tauri::command]
fn reload_backends(state: State<'_, TauriState>) -> Result<Vec<RemappedChannel>, String> {
    let remapped = state.can_manager.lock().map_err(|e| e.to_string())?.reload_backends();
    Ok(remapped
        .into_iter()
        .map(|(old_handle, created)| RemappedChannel {
            old_handle,
            new_handle: created.handle,
            backend: created.backend,
        })
        .collect())
}

/// Parse a DBC file from disk. Lets the frontend show a channel's signal tree
/// before the channel is opened.
#[tauri::command]
fn parse_dbc(path: String) -> Result<ParsedDbc, String> {
    ParsedDbc::new(&path)
}

// ── Send commands ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SendMessageCmd {
    channel_handle: u32,
    message_id: u32,
    signal_values: HashMap<String, f64>,
    /// Per-signal value generators, evaluated once at t=0 — a one-shot send of
    /// an E2E-protected message still gets its checksum/counter bytes.
    #[serde(default)]
    generators: HashMap<String, SignalGen>,
}

#[tauri::command]
fn send_message(cmd: SendMessageCmd, state: State<'_, TauriState>) -> Result<(), String> {
    state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .send_message(cmd.channel_handle, cmd.message_id, &cmd.signal_values, &cmd.generators)
}

#[derive(Deserialize)]
struct SendFrameCmd {
    channel_handle: u32,
    can_id: u32,
    data: Vec<u8>,
    /// Explicit frame format; omitted (None) falls back to inferring it from
    /// the id value (> 0x7FF ⇒ extended).
    is_extended: Option<bool>,
}

#[tauri::command]
fn send_frame(cmd: SendFrameCmd, state: State<'_, TauriState>) -> Result<(), String> {
    state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .send_frame(cmd.channel_handle, cmd.can_id, cmd.data, cmd.is_extended)
}

#[derive(Deserialize)]
struct AddPeriodicFrameCmd {
    channel_handle: u32,
    can_id: u32,
    data: Vec<u8>,
    period_ms: u64,
    /// Explicit frame format; omitted (None) falls back to inferring it from
    /// the id value (> 0x7FF ⇒ extended).
    is_extended: Option<bool>,
}

#[tauri::command]
fn add_periodic_frame(cmd: AddPeriodicFrameCmd, state: State<'_, TauriState>) -> Result<u64, String> {
    use crate::can_communication::CanFrame as RawFrame;
    state.can_manager.lock().map_err(|e| e.to_string())?.add_periodic_frame(
        cmd.channel_handle,
        RawFrame {
            can_id: cmd.can_id,
            is_extended: cmd.is_extended.unwrap_or(cmd.can_id > 0x7FF),
            data: cmd.data,
            timestamp_ms: None,
            error: None,
        },
        cmd.period_ms,
    )
}

#[derive(Deserialize)]
struct AddPeriodicMessageCmd {
    channel_handle: u32,
    message_id: u32,
    signal_values: HashMap<String, f64>,
    /// Per-signal value generators (keyed by signal name); signals not listed
    /// send their constant value from `signal_values`.
    #[serde(default)]
    generators: HashMap<String, SignalGen>,
    period_ms: u64,
}

#[tauri::command]
fn add_periodic_message(cmd: AddPeriodicMessageCmd, state: State<'_, TauriState>) -> Result<u64, String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.add_periodic_message(
        cmd.channel_handle,
        cmd.message_id,
        &cmd.signal_values,
        &cmd.generators,
        cmd.period_ms,
    )
}

#[derive(Deserialize)]
struct UpdatePeriodicMessageCmd {
    channel_handle: u32,
    periodic_handle: u64,
    message_id: u32,
    signal_values: HashMap<String, f64>,
    #[serde(default)]
    generators: HashMap<String, SignalGen>,
    period_ms: u64,
}

/// Re-encode a running periodic DBC message and swap it in place — unlike
/// remove + add there is no transmission gap or phase reset.
#[tauri::command]
fn update_periodic_message(cmd: UpdatePeriodicMessageCmd, state: State<'_, TauriState>) -> Result<(), String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.update_periodic_message(
        cmd.channel_handle,
        cmd.periodic_handle,
        cmd.message_id,
        &cmd.signal_values,
        &cmd.generators,
        cmd.period_ms,
    )
}

#[derive(Deserialize)]
struct UpdatePeriodicFrameCmd {
    channel_handle: u32,
    periodic_handle: u64,
    data: Vec<u8>,
    period_ms: u64,
}

/// Swap a running periodic raw frame's data/period in place (id and frame
/// format changes still go through remove + add).
#[tauri::command]
fn update_periodic_frame(cmd: UpdatePeriodicFrameCmd, state: State<'_, TauriState>) -> Result<(), String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.update_periodic_frame(
        cmd.channel_handle,
        cmd.periodic_handle,
        cmd.data,
        cmd.period_ms,
    )
}

#[derive(Deserialize)]
struct RemovePeriodicCmd {
    channel_handle: u32,
    periodic_handle: u64,
}

#[tauri::command]
fn remove_periodic(cmd: RemovePeriodicCmd, state: State<'_, TauriState>) -> Result<(), String> {
    state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .remove_periodic(cmd.channel_handle, cmd.periodic_handle)
}

// ── Query commands ────────────────────────────────────────────────────────────

#[tauri::command]
fn get_frames(handle: Option<u32>, limit: Option<usize>, state: State<'_, TauriState>) -> Result<Vec<FrameInfo>, String> {
    Ok(state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .get_frames(handle, limit.unwrap_or(100)))
}

#[tauri::command]
fn get_signal_history(
    handle: u32,
    message_id: u32,
    signal_name: String,
    since_ms: u64,
    state: State<'_, TauriState>,
) -> Result<Vec<SignalSample>, String> {
    debug!("get_signal_history: handle={handle}, message_id={message_id:#x}, signal_name={signal_name}, since_ms={since_ms}");
    Ok(state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .get_signal_history(handle, message_id, &signal_name, since_ms))
}

/// Per-channel bus statistics (frames/sec, bus load, error counters); polled
/// by the frontend at ~1 Hz while capture is running.
#[tauri::command]
fn get_bus_stats(state: State<'_, TauriState>) -> Result<Vec<can_manager::BusStats>, String> {
    Ok(state.can_manager.lock().map_err(|e| e.to_string())?.get_bus_stats())
}

#[tauri::command]
fn set_window_ms(ms: u64, state: State<'_, TauriState>) -> Result<(), String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.set_window_ms(ms)
}

#[tauri::command]
fn export_frames_csv(path: String, start_ms: u64, state: State<'_, TauriState>) -> Result<usize, String> {
    state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .export_frames_csv(&path, start_ms)
}

#[tauri::command]
fn export_signals_csv(path: String, start_ms: u64, state: State<'_, TauriState>) -> Result<usize, String> {
    state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .export_signals_csv(&path, start_ms)
}

// ── Version ───────────────────────────────────────────────────────────────────

#[tauri::command]
fn get_version() -> &'static str {
    env!("GIT_VERSION")
}

// ── App / file commands ───────────────────────────────────────────────────────

#[tauri::command]
fn get_app_data_dir(state: State<'_, TauriState>) -> Result<String, String> {
    state
        .app_state
        .app
        .path()
        .app_data_dir()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn write_text_file(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| e.to_string())
}

#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

/// Binary counterpart of `write_text_file`; used e.g. for PNG plot exports.
#[tauri::command]
fn write_binary_file(path: String, data: Vec<u8>) -> Result<(), String> {
    std::fs::write(&path, data).map_err(|e| e.to_string())
}

#[tauri::command]
fn file_exists(path: String) -> bool {
    std::path::Path::new(&path).exists()
}

// ── Project commands ──────────────────────────────────────────────────────────

#[tauri::command]
fn save_project(path: String, project: Project) -> Result<(), String> {
    project.save(&path)
}

#[tauri::command]
fn project_has_changes(path: String, project: Project) -> bool {
    project.has_changes(&path)
}

#[tauri::command]
fn load_project(path: String) -> Result<Project, String> {
    Project::load(&path)
}

#[derive(Serialize)]
struct ProcessInfo {
    name: String,
    pid: u32,
    cpu: f32,
    memory: u64,
}

#[derive(Serialize)]
struct SystemResources {
    processes: Vec<ProcessInfo>,
    frame_count: usize,
    frame_bytes: usize,
}

#[tauri::command]
fn system_resources(state: State<'_, TauriState>) -> Result<SystemResources, String> {
    let main_pid = Pid::from_u32(std::process::id());
    let mut sys = state.sys.lock().map_err(|e| e.to_string())?;
    let mut webkit_pids = state.webkit_pids.lock().map_err(|e| e.to_string())?;

    let nproc = sys.cpus().len();

    if webkit_pids.is_none() {
        // One-time full scan to find WebKit child processes by name.
        sys.refresh_processes(ProcessesToUpdate::All, true);
        let found: Vec<Pid> = sys
            .processes()
            .values()
            .filter(|p| {
                let name = p.name().to_string_lossy().to_lowercase();
                (p.parent() == Some(main_pid)) && (name.contains("webkit") || name.contains("webview"))
            })
            .map(|p| p.pid())
            .collect();
        *webkit_pids = Some(found);
    } else {
        // Fast path: only refresh the exact PIDs we care about.
        let mut pids = vec![main_pid];
        pids.extend_from_slice(webkit_pids.as_ref().unwrap());

        let specifics = ProcessRefreshKind::nothing().with_cpu().with_memory();
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, specifics);
    }

    let child_pids = webkit_pids.as_ref().unwrap();
    let mut result = Vec::new();

    if let Some(p) = sys.process(main_pid) {
        result.push(ProcessInfo {
            name: p.name().to_string_lossy().into_owned(),
            pid: main_pid.as_u32(),
            cpu: p.cpu_usage() / nproc as f32,
            memory: p.memory(),
        });
    }
    for &pid in child_pids {
        if let Some(p) = sys.process(pid) {
            result.push(ProcessInfo {
                name: p.name().to_string_lossy().into_owned(),
                pid: pid.as_u32(),
                cpu: p.cpu_usage() / nproc as f32,
                memory: p.memory(),
            });
        }
    }

    let (frame_count, frame_bytes) = state.can_manager.lock().map_err(|e| e.to_string())?.frame_stats();

    Ok(SystemResources {
        processes: result,
        frame_count,
        frame_bytes,
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logger::init();

    debug!("Starting can-signals-tauri version {}", env!("GIT_VERSION"));
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            logger::set_app(app.handle().clone());
            let app_state = AppState::new(app.handle().clone());
            let manager = CanManager::new(Arc::clone(&app_state));
            app.manage(TauriState {
                app_state,
                can_manager: Arc::new(Mutex::new(manager)),
                sys: Mutex::new(System::new()),
                webkit_pids: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_version,
            get_app_data_dir,
            write_text_file,
            read_text_file,
            write_binary_file,
            file_exists,
            provide_admin_password,
            get_logs,
            list_can_interfaces,
            create_channel,
            remove_channel,
            set_channel_display_name,
            open_channel,
            close_channel,
            reset_backend,
            reload_backends,
            parse_dbc,
            created_channels,
            send_message,
            send_frame,
            add_periodic_frame,
            add_periodic_message,
            update_periodic_message,
            update_periodic_frame,
            remove_periodic,
            get_frames,
            get_signal_history,
            get_bus_stats,
            set_window_ms,
            export_frames_csv,
            export_signals_csv,
            save_project,
            project_has_changes,
            load_project,
            system_resources,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
