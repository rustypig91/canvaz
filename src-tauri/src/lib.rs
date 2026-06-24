mod app_state;
mod backends;
mod can_manager;
mod dbc_parser;
mod project;
pub mod signal_codec;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use app_state::AppState;

use can_manager::{CanFrameEvent, CanManager, ChannelInfo, DbcState, ManagerState};
use project::Project;
use serde::Deserialize;
use tauri::{Emitter, Manager, State};

// ── Tauri managed state ───────────────────────────────────────────────────────

struct TauriState {
    app_state: Arc<AppState>,
    can: ManagerState,
    dbc: DbcState,
}

// ── Sudo ──────────────────────────────────────────────────────────────────────

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
    state: State<'_, TauriState>,
) -> Result<ChannelInfo, String> {
    let can = Arc::clone(&state.can);
    let dbc = Arc::clone(&state.dbc);
    let result = tauri::async_runtime::spawn_blocking(move || {
        can.lock().map_err(|e| e.to_string())?.open_channel(
            backend_name,
            channel_name,
            Some(bitrate),
            dbc,
        )
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));

    if let Ok(ref info) = result {
        println!("Opened channel '{}' with baudrate {:?}", info.id, bitrate);
    } else {
        println!("Failed to open channel: {}", result.as_ref().err().unwrap());
    }
    result
}

#[tauri::command]
fn close_channel(channel_id: String, state: State<'_, TauriState>) -> Result<(), String> {
    state
        .can
        .lock()
        .map_err(|e| e.to_string())?
        .close_channel(&channel_id)?;
    state
        .dbc
        .write()
        .map_err(|e| e.to_string())?
        .remove(&channel_id);
    Ok(())
}

#[tauri::command]
fn get_open_channels(state: State<'_, TauriState>) -> Result<Vec<ChannelInfo>, String> {
    Ok(state
        .can
        .lock()
        .map_err(|e| e.to_string())?
        .open_channels_info())
}

// ── Send commands ─────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Deserialize)]
struct SendMessageCmd {
    channel_id: String,
    message_id: u32,
    signal_values: HashMap<String, f64>,
}

#[tauri::command]
fn send_message(cmd: SendMessageCmd, state: State<'_, TauriState>) -> Result<(), String> {
    let (data, is_extended) = {
        let dbc = state.dbc.read().map_err(|e| e.to_string())?;
        let channel_dbc = dbc
            .get(&cmd.channel_id)
            .ok_or_else(|| format!("No DBC for '{}'", cmd.channel_id))?;
        let msg = channel_dbc
            .messages
            .iter()
            .find(|m| m.id == cmd.message_id)
            .ok_or_else(|| format!("Message 0x{:X} not in DBC", cmd.message_id))?;
        let mut buf = vec![0u8; msg.dlc as usize];
        for sig in &msg.signals {
            if let Some(&v) = cmd.signal_values.get(&sig.name) {
                signal_codec::encode(
                    &mut buf,
                    v,
                    sig.start_bit,
                    sig.length,
                    sig.little_endian,
                    sig.factor,
                    sig.offset,
                );
            }
        }
        (buf, cmd.message_id > 0x7FF)
    };
    state.can.lock().map_err(|e| e.to_string())?.send_frame(
        &cmd.channel_id,
        backends::CanFrame {
            can_id: cmd.message_id,
            is_extended,
            data: data.clone(),
            timestamp_ms: 0,
        },
    )?;
    let _ = state.app_state.app.emit(
        "can-frame",
        CanFrameEvent {
            channel_id: cmd.channel_id,
            can_id: cmd.message_id,
            is_extended,
            dlc: data.len() as u8,
            data,
            timestamp_ms: now_ms(),
            direction: "tx",
        },
    );
    Ok(())
}

#[derive(Deserialize)]
struct SendRawFrameCmd {
    channel_id: String,
    can_id: u32,
    data: Vec<u8>,
}

#[tauri::command]
fn send_raw_frame(cmd: SendRawFrameCmd, state: State<'_, TauriState>) -> Result<(), String> {
    let is_extended = cmd.can_id > 0x7FF;
    let dlc = cmd.data.len() as u8;
    state.can.lock().map_err(|e| e.to_string())?.send_frame(
        &cmd.channel_id,
        backends::CanFrame {
            can_id: cmd.can_id,
            is_extended,
            data: cmd.data.clone(),
            timestamp_ms: 0,
        },
    )?;
    let _ = state.app_state.app.emit(
        "can-frame",
        CanFrameEvent {
            channel_id: cmd.channel_id,
            can_id: cmd.can_id,
            is_extended,
            dlc,
            data: cmd.data,
            timestamp_ms: now_ms(),
            direction: "tx",
        },
    );
    Ok(())
}

// ── DBC commands ──────────────────────────────────────────────────────────────

#[tauri::command]
fn load_dbc(
    channel_id: String,
    path: String,
    state: State<'_, TauriState>,
) -> Result<dbc_parser::ParsedDbc, String> {
    let parsed = dbc_parser::parse_dbc(&path)?;
    state
        .dbc
        .write()
        .map_err(|e| e.to_string())?
        .insert(channel_id, parsed.clone());
    Ok(parsed)
}

#[tauri::command]
fn get_dbc_for_channel(
    channel_id: String,
    state: State<'_, TauriState>,
) -> Result<Option<dbc_parser::ParsedDbc>, String> {
    Ok(state
        .dbc
        .read()
        .map_err(|e| e.to_string())?
        .get(&channel_id)
        .cloned())
}

#[tauri::command]
fn get_all_dbcs(
    state: State<'_, TauriState>,
) -> Result<HashMap<String, dbc_parser::ParsedDbc>, String> {
    Ok(state.dbc.read().map_err(|e| e.to_string())?.clone())
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
fn load_project(path: String, state: State<'_, TauriState>) -> Result<Project, String> {
    let project = Project::load(&path)?;
    let mut dbc_guard = state.dbc.write().map_err(|e| e.to_string())?;
    for ch in &project.channels {
        if let Some(ref dbc_path) = ch.dbc_path {
            if let Ok(parsed) = dbc_parser::parse_dbc(dbc_path) {
                let channel_id = format!("{}:{}", ch.backend, ch.name);
                dbc_guard.insert(channel_id, parsed);
            }
        }
    }
    Ok(project)
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
                dbc: Arc::new(RwLock::new(HashMap::new())),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_version,
            get_app_data_dir,
            write_text_file,
            read_text_file,
            file_exists,
            provide_sudo_password,
            list_can_interfaces,
            open_channel,
            close_channel,
            get_open_channels,
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
