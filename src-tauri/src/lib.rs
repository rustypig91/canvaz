mod app_state;
mod backends;
mod can_manager;
mod dbc_parser;
mod can_frame;
mod project;
mod can_communication;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use app_state::AppState;

use backends::Channel;
use can_frame::{CanFrame, Direction};
use can_manager::{CanFrameEvent, CanManager, ChannelInfo, ManagerState, SubscribedSignals};
use project::Project;
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};

// ── Tauri managed state ───────────────────────────────────────────────────────

struct TauriState {
    app_state: Arc<AppState>,
    can: ManagerState,
    subscribed: SubscribedSignals,
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
    state.can.lock().map_err(|e| e.to_string())?.close_channel(&channel_id)?;
    state.subscribed.write().map_err(|e| e.to_string())?.remove(&channel_id);
    Ok(())
}

#[tauri::command]
fn get_open_channels(state: State<'_, TauriState>) -> Result<Vec<ChannelInfo>, String> {
    Ok(state.can.lock().map_err(|e| e.to_string())?.open_channels_info())
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
    let ts = now_ms();
    let ch_arc = state.can.lock().map_err(|e| e.to_string())?
        .channel_arc(&cmd.channel_id)
        .ok_or_else(|| format!("'{}' is not open", cmd.channel_id))?;
    let frame = ch_arc.lock()
        .map_err(|_| "Channel lock poisoned".to_string())?
        .send_dbc_message(cmd.message_id, &cmd.signal_values, ts)?;
    let _ = state.app_state.app.emit(
        "can-frame",
        CanFrameEvent {
            channel_id: cmd.channel_id,
            can_id: cmd.message_id,
            is_extended: frame.is_extended,
            dlc: frame.data.len() as u8,
            data: frame.data,
            timestamp_ms: ts,
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
    let ts = now_ms();
    state.can.lock().map_err(|e| e.to_string())?.send_frame(
        &cmd.channel_id,
        CanFrame {
            can_id: cmd.can_id,
            is_extended,
            data: cmd.data.clone(),
            timestamp_ms: ts,
            direction: Direction::Tx,
            decoded: None,
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
            timestamp_ms: ts,
            direction: "tx",
        },
    );
    Ok(())
}

// ── DBC commands ──────────────────────────────────────────────────────────────

/// Parse a DBC file and return its contents. Does not associate with any channel.
#[tauri::command]
fn parse_dbc(path: String) -> Result<dbc_parser::ParsedDbc, String> {
    dbc_parser::parse_dbc(&path)
}

// ── Signal subscription commands ──────────────────────────────────────────────

#[tauri::command]
fn subscribe_signals(
    channel_id: String,
    signal_names: Vec<String>,
    state: State<'_, TauriState>,
) -> Result<(), String> {
    let mut subs = state.subscribed.write().map_err(|e| e.to_string())?;
    let ch_subs = subs.entry(channel_id).or_insert_with(HashSet::new);
    for name in signal_names {
        ch_subs.insert(name);
    }
    Ok(())
}

#[tauri::command]
fn unsubscribe_signals(
    channel_id: String,
    signal_names: Vec<String>,
    state: State<'_, TauriState>,
) -> Result<(), String> {
    let mut subs = state.subscribed.write().map_err(|e| e.to_string())?;
    if let Some(ch_subs) = subs.get_mut(&channel_id) {
        for name in signal_names {
            ch_subs.remove(&name);
        }
    }
    Ok(())
}

// ── Signal history command ────────────────────────────────────────────────────

#[derive(Serialize)]
struct SignalSample {
    timestamp_ms: u64,
    value: f64,
}

#[tauri::command]
fn get_signal_history(
    channel_id: String,
    signal_name: String,
    since_ms: u64,
    state: State<'_, TauriState>,
) -> Result<Vec<SignalSample>, String> {
    let ch_arc = state
        .can
        .lock()
        .map_err(|e| e.to_string())?
        .channel_arc(&channel_id);
    let ch_arc = match ch_arc {
        Some(a) => a,
        None => return Ok(vec![]),
    };

    let ch = ch_arc.lock().map_err(|_| "Channel lock poisoned".to_string())?;
    Ok(ch.frames_since(since_ms)
        .into_iter()
        .filter_map(|f| {
            let decoded = f.decoded.as_ref()?;
            let sig = decoded.signals.iter().find(|s| s.name == signal_name)?;
            Some(SignalSample { timestamp_ms: f.timestamp_ms, value: sig.physical })
        })
        .collect())
}

// ── Window size command ───────────────────────────────────────────────────────

#[tauri::command]
fn set_window_ms(ms: u64, state: State<'_, TauriState>) -> Result<(), String> {
    let channels: Vec<Arc<Mutex<Channel>>> = state
        .can
        .lock()
        .map_err(|e| e.to_string())?
        .all_channel_arcs();
    for ch in channels {
        ch.lock()
            .map_err(|_| "Channel lock poisoned".to_string())?
            .set_window_ms(ms);
    }
    Ok(())
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
            let subscribed: SubscribedSignals = Arc::new(RwLock::new(HashMap::new()));
            let manager = CanManager::new(Arc::clone(&app_state), Arc::clone(&subscribed));
            app.manage(TauriState {
                app_state,
                can: Arc::new(Mutex::new(manager)),
                subscribed,
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
            subscribe_signals,
            unsubscribe_signals,
            get_signal_history,
            set_window_ms,
            save_project,
            load_project,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
