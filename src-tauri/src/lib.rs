mod backends;
mod can_interface;
mod dbc_parser;
mod project;
pub mod signal_codec;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use backends::SocketCanBackend;
use can_interface::{CanManager, DbcState, ManagerState};
use project::Project;
use serde::Deserialize;
use tauri::{AppHandle, Manager, State};

// ── Shared app state ──────────────────────────────────────────────────────────

struct AppState {
    can: ManagerState,
    dbc: DbcState,
}

// ── CAN commands ──────────────────────────────────────────────────────────────

#[tauri::command]
fn configure_channel(
    name: String,
    bitrate: Option<u32>,
    sudo_password: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let manager = state.can.lock().map_err(|e| e.to_string())?;
    manager.configure_channel(&name, bitrate, sudo_password.as_deref())
}

#[tauri::command]
fn list_can_interfaces(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let manager = state.can.lock().map_err(|e| e.to_string())?;
    Ok(manager.list_interfaces())
}

#[tauri::command]
fn open_channel(
    name: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), String> {
    let mut manager = state.can.lock().map_err(|e| e.to_string())?;
    let dbc_arc = Arc::clone(&state.dbc);
    manager.open_channel(name, app, dbc_arc)
}

#[tauri::command]
fn close_channel(name: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut manager = state.can.lock().map_err(|e| e.to_string())?;
    manager.close_channel(&name)?;
    let mut dbc = state.dbc.write().map_err(|e| e.to_string())?;
    dbc.remove(&name);
    Ok(())
}

#[tauri::command]
fn get_open_channels(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let manager = state.can.lock().map_err(|e| e.to_string())?;
    Ok(manager.open_names())
}

#[derive(Deserialize)]
struct SendSignalCmd {
    channel: String,
    signal_name: String,
    value: f64,
}

#[tauri::command]
fn send_signal(cmd: SendSignalCmd, state: State<'_, AppState>) -> Result<(), String> {
    let (message_id, data) = {
        let dbc_guard = state.dbc.read().map_err(|e| e.to_string())?;
        let channel_dbc = dbc_guard
            .get(&cmd.channel)
            .ok_or_else(|| format!("No DBC loaded for channel '{}'", cmd.channel))?;
        let sig = channel_dbc
            .find_signal(&cmd.signal_name)
            .ok_or_else(|| format!("Signal '{}' not found in DBC", cmd.signal_name))?
            .clone();
        let dlc = channel_dbc
            .messages
            .iter()
            .find(|m| m.id == sig.message_id)
            .map(|m| m.dlc as usize)
            .unwrap_or(8);
        let mut data = vec![0u8; dlc.min(8)];
        signal_codec::encode(
            &mut data,
            cmd.value,
            sig.start_bit,
            sig.length,
            sig.little_endian,
            sig.factor,
            sig.offset,
        );
        (sig.message_id, data)
    };
    let manager = state.can.lock().map_err(|e| e.to_string())?;
    manager.send_frame(&cmd.channel, message_id, &data)
}

// ── DBC commands ──────────────────────────────────────────────────────────────

#[tauri::command]
fn load_dbc(
    channel: String,
    path: String,
    state: State<'_, AppState>,
) -> Result<dbc_parser::ParsedDbc, String> {
    let parsed = dbc_parser::parse_dbc(&path)?;
    let mut guard = state.dbc.write().map_err(|e| e.to_string())?;
    guard.insert(channel, parsed.clone());
    Ok(parsed)
}

#[tauri::command]
fn get_dbc_for_channel(
    channel: String,
    state: State<'_, AppState>,
) -> Result<Option<dbc_parser::ParsedDbc>, String> {
    let guard = state.dbc.read().map_err(|e| e.to_string())?;
    Ok(guard.get(&channel).cloned())
}

#[tauri::command]
fn get_all_dbcs(
    state: State<'_, AppState>,
) -> Result<HashMap<String, dbc_parser::ParsedDbc>, String> {
    let guard = state.dbc.read().map_err(|e| e.to_string())?;
    Ok(guard.clone())
}

// ── App path ──────────────────────────────────────────────────────────────────

#[tauri::command]
fn get_app_data_dir(app: AppHandle) -> Result<String, String> {
    app.path()
        .app_data_dir()
        .map(|p| p.to_string_lossy().to_string())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn write_text_file(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| e.to_string())
}

// ── Project commands ──────────────────────────────────────────────────────────

#[tauri::command]
fn save_project(path: String, project: Project) -> Result<(), String> {
    println!("Saving project to '{}'", path);
    project.save(&path)
}

#[tauri::command]
fn load_project(path: String, state: State<'_, AppState>) -> Result<Project, String> {
    let project = Project::load(&path)?;
    let mut dbc_guard = state.dbc.write().map_err(|e| e.to_string())?;
    for ch in &project.channels {
        if let Some(ref dbc_path) = ch.dbc_path {
            if let Ok(parsed) = dbc_parser::parse_dbc(dbc_path) {
                dbc_guard.insert(ch.name.clone(), parsed);
            }
        }
    }
    Ok(project)
}

// ── App entry point ───────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut manager = CanManager::new();
    manager.register_backend(SocketCanBackend);
    // Register additional backends here as they are added, e.g.:
    // manager.register_backend(PeakCanBackend::new());

    let app_state = AppState {
        can: Arc::new(Mutex::new(manager)),
        dbc: Arc::new(RwLock::new(HashMap::new())),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_app_data_dir,
            write_text_file,
            configure_channel,
            list_can_interfaces,
            open_channel,
            close_channel,
            get_open_channels,
            send_signal,
            load_dbc,
            get_dbc_for_channel,
            get_all_dbcs,
            save_project,
            load_project,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
