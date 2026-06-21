mod backends;
mod dbc_parser;
mod project;
pub mod signal_codec;

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, RwLock};

use backends::{CanManager, ChannelInfo, DbcState, ManagerState, SocketCanBackend};
use project::Project;
use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager, State};

// ── Sudo password state ───────────────────────────────────────────────────────

enum SudoRequest {
    Idle,
    Waiting,
    Done,
}

pub struct SudoState {
    cached: Mutex<Option<String>>,
    request: Mutex<SudoRequest>,
    condvar: Condvar,
}

impl SudoState {
    fn new() -> Self {
        Self {
            cached: Mutex::new(None),
            request: Mutex::new(SudoRequest::Idle),
            condvar: Condvar::new(),
        }
    }

    /// Returns the cached password without blocking.
    fn get_cached(&self) -> Option<String> {
        self.cached.lock().unwrap().clone()
    }

    /// Returns the cached password immediately, or emits "request-sudo-password"
    /// to the frontend and blocks the calling thread until the user responds.
    /// Call this from any Tauri command that may need root.
    pub fn get_or_request(&self, app: &AppHandle) -> Result<String, String> {
        // Fast path — already have it.
        {
            let c = self.cached.lock().map_err(|e| e.to_string())?;
            if let Some(pw) = c.as_ref() {
                return Ok(pw.clone());
            }
        }

        // Transition to Waiting and ask the frontend (only once even if multiple
        // callers race here simultaneously).
        {
            let mut req = self.request.lock().map_err(|e| e.to_string())?;
            if matches!(*req, SudoRequest::Idle | SudoRequest::Done) {
                *req = SudoRequest::Waiting;
                let _ = app.emit("request-sudo-password", ());
            }
        }

        // Block until provide_sudo_password resolves the request.
        {
            let mut req = self
                .condvar
                .wait_while(
                    self.request.lock().map_err(|e| e.to_string())?,
                    |req| matches!(req, SudoRequest::Waiting),
                )
                .map_err(|e| e.to_string())?;
            // Reset so future calls can issue a new request if needed.
            *req = SudoRequest::Idle;
        }

        // provide() caches the password before notifying, so this is safe.
        self.cached
            .lock()
            .map_err(|e| e.to_string())?
            .clone()
            .ok_or_else(|| "Sudo authentication cancelled".to_string())
    }

    /// Called by the frontend with the user's password (or None if cancelled).
    fn provide(&self, password: Option<String>) {
        // Cache a valid password before waking waiters.
        if let Some(pw) = &password {
            if !pw.is_empty() {
                if let Ok(mut c) = self.cached.lock() {
                    *c = Some(pw.clone());
                }
            }
        }
        if let Ok(mut req) = self.request.lock() {
            *req = SudoRequest::Done;
        }
        self.condvar.notify_all();
    }
}

// ── Shared app state ──────────────────────────────────────────────────────────

struct AppState {
    can: ManagerState,
    dbc: DbcState,
    sudo: Arc<SudoState>,
}

// ── Sudo command ──────────────────────────────────────────────────────────────

/// Called by the frontend in response to the "request-sudo-password" event.
/// Pass `None` when the user cancels the dialog.
#[tauri::command]
fn provide_sudo_password(password: Option<String>, state: State<'_, AppState>) {
    state.sudo.provide(password);
}

// ── CAN commands ──────────────────────────────────────────────────────────────

#[tauri::command]
fn list_can_interfaces(state: State<'_, AppState>) -> Result<Vec<ChannelInfo>, String> {
    let manager = state.can.lock().map_err(|e| e.to_string())?;
    Ok(manager.list_channels())
}

#[tauri::command]
fn open_channel(
    backend: String,
    name: String,
    bitrate: Option<u32>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), String> {
    let dbc_arc = Arc::clone(&state.dbc);

    // First attempt with the cached password (may be None — works if the
    // interface is already up or doesn't need root, e.g. vcan).
    let first_err = {
        let pw = state.sudo.get_cached();
        let mut mgr = state.can.lock().map_err(|e| e.to_string())?;
        match mgr.open_channel(&backend, &name, bitrate, pw.as_deref(), app.clone(), Arc::clone(&dbc_arc)) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        }
    };

    if !first_err.starts_with("needs-sudo:") {
        return Err(first_err);
    }

    // Root is required — ask the user (blocks until the frontend responds).
    let pw = state.sudo.get_or_request(&app)?;
    let mut mgr = state.can.lock().map_err(|e| e.to_string())?;
    mgr.open_channel(&backend, &name, bitrate, Some(&pw), app, dbc_arc)
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
fn get_open_channels(state: State<'_, AppState>) -> Result<Vec<ChannelInfo>, String> {
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

    let app_state = AppState {
        can: Arc::new(Mutex::new(manager)),
        dbc: Arc::new(RwLock::new(HashMap::new())),
        sudo: Arc::new(SudoState::new()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            get_app_data_dir,
            write_text_file,
            provide_sudo_password,
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
