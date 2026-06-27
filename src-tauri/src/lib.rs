mod app_state;
mod can_communication;
mod can_frame;
mod can_manager;
mod dbc_parser;
mod project;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use app_state::AppState;
use can_manager::{CanManager, ChannelInfo, FrameInfo, ManagerState, SignalSample};
use project::Project;
use serde::Deserialize;
use tauri::{Manager, State};

// ── Tauri managed state ───────────────────────────────────────────────────────

struct TauriState {
    app_state: Arc<AppState>,
    can: ManagerState,
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
    state.can.lock().map_err(|e| e.to_string())?.list_channels()
}

#[tauri::command]
async fn open_channel(
    backend_name: String,
    channel_name: String,
    bitrate: u32,
    dbc_path: Option<String>,
    state: State<'_, TauriState>,
) -> Result<ChannelInfo, String> {
    let can = Arc::clone(&state.can);
    let result = tauri::async_runtime::spawn_blocking(move || {
        can.lock()
            .map_err(|e| e.to_string())?
            .open_channel(backend_name, channel_name, bitrate, dbc_path.as_deref())
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));

    if let Ok(ref info) = result {
        println!("Opened channel '{}' with baudrate {}", info.id, bitrate);
    } else {
        println!("Failed to open channel: {}", result.as_ref().err().unwrap());
    }
    result
}

#[tauri::command]
fn close_channel(channel_id: String, state: State<'_, TauriState>) -> Result<(), String> {
    state.can.lock().map_err(|e| e.to_string())?.close_channel(&channel_id)
}

#[tauri::command]
fn get_open_channels(state: State<'_, TauriState>) -> Result<Vec<ChannelInfo>, String> {
    Ok(state.can.lock().map_err(|e| e.to_string())?.open_channels_info())
}

// ── Send commands ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SendMessageCmd {
    channel_id: String,
    message_id: u32,
    signal_values: HashMap<String, f64>,
}

#[tauri::command]
fn send_message(cmd: SendMessageCmd, state: State<'_, TauriState>) -> Result<(), String> {
    state.can.lock().map_err(|e| e.to_string())?
        .send_message(&cmd.channel_id, cmd.message_id, &cmd.signal_values)
}

#[derive(Deserialize)]
struct SendRawFrameCmd {
    channel_id: String,
    can_id: u32,
    data: Vec<u8>,
}

#[tauri::command]
fn send_raw_frame(cmd: SendRawFrameCmd, state: State<'_, TauriState>) -> Result<(), String> {
    state.can.lock().map_err(|e| e.to_string())?
        .send_raw(&cmd.channel_id, cmd.can_id, cmd.data)
}

// ── DBC commands ──────────────────────────────────────────────────────────────

#[tauri::command]
fn parse_dbc(path: String) -> Result<dbc_parser::ParsedDbc, String> {
    dbc_parser::parse_dbc(&path)
}

// ── Query commands ────────────────────────────────────────────────────────────

#[tauri::command]
fn get_frames(
    channel_id: Option<String>,
    limit: Option<usize>,
    state: State<'_, TauriState>,
) -> Result<Vec<FrameInfo>, String> {
    Ok(state.can.lock().map_err(|e| e.to_string())?
        .get_frames(channel_id.as_deref(), limit.unwrap_or(100)))
}

#[tauri::command]
fn get_signal_history(
    channel_id: String,
    signal_name: String,
    since_ms: u64,
    state: State<'_, TauriState>,
) -> Result<Vec<SignalSample>, String> {
    Ok(state.can.lock().map_err(|e| e.to_string())?
        .get_signal_history(&channel_id, &signal_name, since_ms))
}

#[tauri::command]
fn set_window_ms(ms: u64, state: State<'_, TauriState>) -> Result<(), String> {
    state.can.lock().map_err(|e| e.to_string())?.set_window_ms(ms)
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
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            let app_state = AppState::new(app.handle().clone());
            let manager = CanManager::new(Arc::clone(&app_state));
            app.manage(TauriState {
                app_state,
                can: Arc::new(Mutex::new(manager)),
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
            open_channel,
            close_channel,
            get_open_channels,
            send_message,
            send_raw_frame,
            parse_dbc,
            get_frames,
            get_signal_history,
            set_window_ms,
            save_project,
            load_project,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
