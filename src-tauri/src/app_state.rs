use std::sync::Arc;

use std::sync::{Condvar, Mutex};

use tauri::{AppHandle, Emitter};


enum PasswordRequestState {
    Idle,
    Waiting,
    Done,
}


struct PwdState {
    cached: Mutex<Option<String>>,
    request: Mutex<PasswordRequestState>,
    condvar: Condvar,
}

impl PwdState {
    fn new() -> Self {
        Self {
            cached: Mutex::new(None),
            request: Mutex::new(PasswordRequestState::Idle),
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
            if matches!(*req, PasswordRequestState::Idle | PasswordRequestState::Done) {
                *req = PasswordRequestState::Waiting;
                let _ = app.emit("request-admin-password", ());
            }
        }
        {
            let mut req = self
                .condvar
                .wait_while(self.request.lock().map_err(|e| e.to_string())?, |r| {
                    matches!(r, PasswordRequestState::Waiting)
                })
                .map_err(|e| e.to_string())?;
            *req = PasswordRequestState::Idle;
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
            *req = PasswordRequestState::Done;
        }
        self.condvar.notify_all();
    }
}

// ── Public ────────────────────────────────────────────────────────────────────

pub struct AppState {
    pub app: AppHandle,
    pwd: PwdState,
}

impl AppState {
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(Self {
            app,
            pwd: PwdState::new(),
        })
    }

    pub fn get_admin_password(&self) -> Result<String, String> {
        self.pwd.get_or_request(&self.app)
    }

    pub fn provide_admin_password(&self, password: Option<String>) {
        self.pwd.provide(password);
    }
}
