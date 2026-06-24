use std::sync::{Arc, Condvar, Mutex};
use tauri::{AppHandle, Emitter};

enum SudoRequestState { Idle, Waiting, Done }

struct SudoState {
    cached:  Mutex<Option<String>>,
    request: Mutex<SudoRequestState>,
    condvar: Condvar,
}

impl SudoState {
    fn new() -> Self {
        Self {
            cached:  Mutex::new(None),
            request: Mutex::new(SudoRequestState::Idle),
            condvar: Condvar::new(),
        }
    }

    fn get_or_request(&self, app: &AppHandle) -> Result<String, String> {
        {
            let c = self.cached.lock().map_err(|e| e.to_string())?;
            if let Some(pw) = c.as_ref() {
                return Ok(pw.clone());
            }
        }
        {
            let mut req = self.request.lock().map_err(|e| e.to_string())?;
            if matches!(*req, SudoRequestState::Idle | SudoRequestState::Done) {
                *req = SudoRequestState::Waiting;
                let _ = app.emit("request-sudo-password", ());
            }
        }
        {
            let mut req = self
                .condvar
                .wait_while(
                    self.request.lock().map_err(|e| e.to_string())?,
                    |r| matches!(r, SudoRequestState::Waiting),
                )
                .map_err(|e| e.to_string())?;
            *req = SudoRequestState::Idle;
        }
        self.cached
            .lock()
            .map_err(|e| e.to_string())?
            .clone()
            .ok_or_else(|| "Sudo authentication cancelled".to_string())
    }

    fn provide(&self, password: Option<String>) {
        if let Some(ref pw) = password {
            if !pw.is_empty() {
                if let Ok(mut c) = self.cached.lock() {
                    *c = Some(pw.clone());
                }
            }
        }
        if let Ok(mut req) = self.request.lock() {
            *req = SudoRequestState::Done;
        }
        self.condvar.notify_all();
    }
}

// ── Public ────────────────────────────────────────────────────────────────────

pub struct AppState {
    pub app: AppHandle,
    sudo: SudoState,
}

impl AppState {
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(Self { app, sudo: SudoState::new() })
    }

    #[cfg(target_os = "linux")]
    pub fn get_sudo_password(&self) -> Result<String, String> {
        self.sudo.get_or_request(&self.app)
    }

    pub fn provide_sudo_password(&self, password: Option<String>) {
        self.sudo.provide(password);
    }
}
