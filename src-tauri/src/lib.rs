mod app_state;
mod backends;
mod dbc_parser;
mod project;
pub mod signal_codec;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use app_state::AppState;
use backends::{CanManager, ChannelInfo, DbcState, ManagerState, SocketCanBackend};
use project::Project;
use serde::Deserialize;
use tauri::{Emitter, Manager, State};

// ── Tauri managed state ───────────────────────────────────────────────────────

struct TauriState {
    app_state: Arc<AppState>,
    can: ManagerState,
    dbc: DbcState,
}

// ── Sudo command ──────────────────────────────────────────────────────────────

#[tauri::command]
fn provide_sudo_password(password: Option<String>, state: State<'_, TauriState>) {
    state.app_state.provide_sudo_password(password);
}

// ── CAN commands ──────────────────────────────────────────────────────────────

#[tauri::command]
fn list_can_interfaces(state: State<'_, TauriState>) -> Result<Vec<ChannelInfo>, String> {
    let manager = state.can.lock().map_err(|e| e.to_string())?;
    Ok(manager.list_channels())
}

#[tauri::command]
async fn open_channel(
    backend: String,
    name: String,
    bitrate: Option<u32>,
    state: State<'_, TauriState>,
) -> Result<(), String> {
    let can = Arc::clone(&state.can);
    let dbc = Arc::clone(&state.dbc);
    // Run on a blocking thread so the tokio runtime stays free to dispatch
    // provide_sudo_password while we wait for the user to enter a password.
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut mgr = can.lock().map_err(|e| e.to_string())?;
        mgr.open_channel(&backend, &name, bitrate, dbc)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()))
}

#[tauri::command]
fn close_channel(name: String, state: State<'_, TauriState>) -> Result<(), String> {
    let mut manager = state.can.lock().map_err(|e| e.to_string())?;
    manager.close_channel(&name)?;
    let mut dbc = state.dbc.write().map_err(|e| e.to_string())?;
    dbc.remove(&name);
    Ok(())
}

#[tauri::command]
fn get_open_channels(state: State<'_, TauriState>) -> Result<Vec<ChannelInfo>, String> {
    let manager = state.can.lock().map_err(|e| e.to_string())?;
    Ok(manager.open_channels_info())
}

#[derive(Deserialize)]
struct SendSignalCmd {
    channel: String,
    signal_name: String,
    value: f64,
}

#[tauri::command]
fn send_signal(cmd: SendSignalCmd, state: State<'_, TauriState>) -> Result<(), String> {
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

// ── Message / raw frame send commands ────────────────────────────────────────

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Deserialize)]
struct SendMessageCmd {
    channel: String,
    message_id: u32,
    signal_values: HashMap<String, f64>,
}

#[tauri::command]
fn send_message(cmd: SendMessageCmd, state: State<'_, TauriState>) -> Result<(), String> {
    let (data, is_extended) = {
        let dbc = state.dbc.read().map_err(|e| e.to_string())?;
        let channel_dbc = dbc
            .get(&cmd.channel)
            .ok_or_else(|| format!("No DBC for '{}'", cmd.channel))?;
        let msg = channel_dbc
            .messages
            .iter()
            .find(|m| m.id == cmd.message_id)
            .ok_or_else(|| format!("Message 0x{:X} not in DBC", cmd.message_id))?;
        let mut buf = vec![0u8; msg.dlc as usize];
        for sig in &msg.signals {
            if let Some(&v) = cmd.signal_values.get(&sig.name) {
                signal_codec::encode(
                    &mut buf, v,
                    sig.start_bit, sig.length,
                    sig.little_endian, sig.factor, sig.offset,
                );
            }
        }
        (buf, cmd.message_id > 0x7FF)
    };
    state.can.lock().map_err(|e| e.to_string())?.send_frame(&cmd.channel, cmd.message_id, &data)?;
    let _ = state.app_state.app.emit("can-frame", backends::CanFrameEvent {
        channel: cmd.channel,
        can_id: cmd.message_id,
        is_extended,
        dlc: data.len() as u8,
        data,
        timestamp_ms: now_ms(),
        direction: "tx",
    });
    Ok(())
}

#[derive(Deserialize)]
struct SendRawFrameCmd {
    channel: String,
    can_id: u32,
    data: Vec<u8>,
}

#[tauri::command]
fn send_raw_frame(cmd: SendRawFrameCmd, state: State<'_, TauriState>) -> Result<(), String> {
    let is_extended = cmd.can_id > 0x7FF;
    let dlc = cmd.data.len() as u8;
    state.can.lock().map_err(|e| e.to_string())?.send_frame(&cmd.channel, cmd.can_id, &cmd.data)?;
    let _ = state.app_state.app.emit("can-frame", backends::CanFrameEvent {
        channel: cmd.channel,
        can_id: cmd.can_id,
        is_extended,
        dlc,
        data: cmd.data,
        timestamp_ms: now_ms(),
        direction: "tx",
    });
    Ok(())
}

// ── DBC commands ──────────────────────────────────────────────────────────────

#[tauri::command]
fn load_dbc(
    channel: String,
    path: String,
    state: State<'_, TauriState>,
) -> Result<dbc_parser::ParsedDbc, String> {
    let parsed = dbc_parser::parse_dbc(&path)?;
    let mut guard = state.dbc.write().map_err(|e| e.to_string())?;
    guard.insert(channel, parsed.clone());
    Ok(parsed)
}

#[tauri::command]
fn get_dbc_for_channel(
    channel: String,
    state: State<'_, TauriState>,
) -> Result<Option<dbc_parser::ParsedDbc>, String> {
    let guard = state.dbc.read().map_err(|e| e.to_string())?;
    Ok(guard.get(&channel).cloned())
}

#[tauri::command]
fn get_all_dbcs(
    state: State<'_, TauriState>,
) -> Result<HashMap<String, dbc_parser::ParsedDbc>, String> {
    let guard = state.dbc.read().map_err(|e| e.to_string())?;
    Ok(guard.clone())
}

// ── App path ──────────────────────────────────────────────────────────────────

#[tauri::command]
fn get_app_data_dir(state: State<'_, TauriState>) -> Result<String, String> {
    state.app_state.app
        .path()
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
    project.save(&path)
}

#[tauri::command]
fn load_project(path: String, state: State<'_, TauriState>) -> Result<Project, String> {
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
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            let app_state = AppState::new(app.handle().clone());

            let mut manager = CanManager::new(Arc::clone(&app_state));
            manager.register_backend(SocketCanBackend);

            app.manage(TauriState {
                app_state,
                can: Arc::new(Mutex::new(manager)),
                dbc: Arc::new(RwLock::new(HashMap::new())),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_data_dir,
            write_text_file,
            provide_sudo_password,
            list_can_interfaces,
            open_channel,
            close_channel,
            get_open_channels,
            send_signal,
            send_message,
            send_raw_frame,
            load_dbc,
            get_dbc_for_channel,
            get_all_dbcs,
            save_project,
            load_project,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
