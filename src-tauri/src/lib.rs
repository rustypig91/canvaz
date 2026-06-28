mod app_state;
mod can_communication;
mod can_frame;
mod can_manager;
mod dbc_parser;
mod logger;
mod project;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use app_state::AppState;
use can_manager::{CanManager, ChannelInfo, FrameInfo, ManagerState, SignalSample};
use logger::init;
use project::Project;
use serde::Deserialize;
use tauri::{Manager, State};

use log::{debug, error, info, warn};

// ── Tauri managed state ───────────────────────────────────────────────────────

struct TauriState {
    app_state: Arc<AppState>,
    can_manager: ManagerState,
}

// ── Sudo ──────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
#[tauri::command]
fn provide_sudo_password(password: Option<String>, state: State<'_, TauriState>) {
    state.app_state.provide_sudo_password(password);
}

// ── CAN channel commands ──────────────────────────────────────────────────────

#[tauri::command]
fn list_can_interfaces(state: State<'_, TauriState>) -> Result<Vec<ChannelInfo>, String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.list_channels()
}

#[tauri::command]
fn create_channel(
    backend_name: String,
    channel_name: String,
    dbc_path: Option<String>,
    state: State<'_, TauriState>,
) -> Result<u32, String> {
    state
        .can_manager
        .lock()
        .map_err(|e| e.to_string())?
        .create_channel(&backend_name, &channel_name, dbc_path.as_deref())
}

#[tauri::command]
async fn open_channel(
    handle: u32,
    bitrate: u32,
    state: State<'_, TauriState>,
) -> Result<ChannelInfo, String> {
    let can_manager = Arc::clone(&state.can_manager);
    let result = tauri::async_runtime::spawn_blocking(move || {
        can_manager.lock().map_err(|e| e.to_string())?.open_channel(handle, bitrate)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));

    if let Ok(ref info) = result {
        info!("Opened channel '{}' (handle {}) with baudrate {}", info.id, handle, bitrate);
    } else {
        error!("Failed to open channel {handle}: {}", result.as_ref().err().unwrap());
    }
    result
}

#[tauri::command]
fn close_channel(handle: u32, state: State<'_, TauriState>) -> Result<(), String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.close_channel(handle)
}

#[tauri::command]
fn get_open_channels(state: State<'_, TauriState>) -> Result<Vec<ChannelInfo>, String> {
    Ok(state.can_manager.lock().map_err(|e| e.to_string())?.open_channels_info())
}

// ── Send commands ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SendMessageCmd {
    channel_handle: u32,
    message_id: u32,
    signal_values: HashMap<String, f64>,
}

#[tauri::command]
fn send_message(cmd: SendMessageCmd, state: State<'_, TauriState>) -> Result<(), String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.send_message(
        cmd.channel_handle,
        cmd.message_id,
        &cmd.signal_values,
    )
}

#[derive(Deserialize)]
struct SendFrameCmd {
    channel_handle: u32,
    can_id: u32,
    data: Vec<u8>,
}

#[tauri::command]
fn send_frame(cmd: SendFrameCmd, state: State<'_, TauriState>) -> Result<(), String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.send_raw(cmd.channel_handle, cmd.can_id, cmd.data)
}

#[derive(Deserialize)]
struct AddPeriodicFrameCmd {
    channel_handle: u32,
    can_id: u32,
    data: Vec<u8>,
    period_ms: u64,
}

#[tauri::command]
fn add_periodic_frame(cmd: AddPeriodicFrameCmd, state: State<'_, TauriState>) -> Result<u64, String> {
    use crate::can_communication::CanFrame as RawFrame;
    state.can_manager.lock().map_err(|e| e.to_string())?.add_periodic_frame(
        cmd.channel_handle,
        RawFrame { can_id: cmd.can_id, is_extended: cmd.can_id > 0x7FF, data: cmd.data },
        cmd.period_ms,
    )
}

#[derive(Deserialize)]
struct AddPeriodicMessageCmd {
    channel_handle: u32,
    message_id: u32,
    signal_values: HashMap<String, f64>,
    period_ms: u64,
}

#[tauri::command]
fn add_periodic_message(cmd: AddPeriodicMessageCmd, state: State<'_, TauriState>) -> Result<u64, String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.add_periodic_message(
        cmd.channel_handle,
        cmd.message_id,
        &cmd.signal_values,
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
    state.can_manager.lock().map_err(|e| e.to_string())?.remove_periodic(cmd.channel_handle, cmd.periodic_handle)
}

// ── Query commands ────────────────────────────────────────────────────────────

#[tauri::command]
fn get_frames(
    handle: Option<u32>,
    limit: Option<usize>,
    state: State<'_, TauriState>,
) -> Result<Vec<FrameInfo>, String> {
    Ok(state.can_manager.lock().map_err(|e| e.to_string())?.get_frames(handle, limit.unwrap_or(100)))
}

#[tauri::command]
fn get_signal_history(
    handle: u32,
    signal_name: String,
    since_ms: u64,
    state: State<'_, TauriState>,
) -> Result<Vec<SignalSample>, String> {
    debug!("get_signal_history: handle={handle}, signal_name={signal_name}, since_ms={since_ms}");
    Ok(state.can_manager.lock().map_err(|e| e.to_string())?.get_signal_history(handle, &signal_name, since_ms))
}

#[tauri::command]
fn set_window_ms(ms: u64, state: State<'_, TauriState>) -> Result<(), String> {
    state.can_manager.lock().map_err(|e| e.to_string())?.set_window_ms(ms)
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
fn load_project(path: String) -> Result<Project, String> {
    Project::load(&path)
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
            let app_state = AppState::new(app.handle().clone());
            let manager = CanManager::new(Arc::clone(&app_state));
            app.manage(TauriState {
                app_state,
                can_manager: Arc::new(Mutex::new(manager)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_version,
            get_app_data_dir,
            write_text_file,
            read_text_file,
            file_exists,
            #[cfg(target_os = "linux")]
            provide_sudo_password,
            list_can_interfaces,
            create_channel,
            open_channel,
            close_channel,
            get_open_channels,
            send_message,
            send_frame,
            add_periodic_frame,
            add_periodic_message,
            remove_periodic,
            get_frames,
            get_signal_history,
            set_window_ms,
            save_project,
            load_project,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
