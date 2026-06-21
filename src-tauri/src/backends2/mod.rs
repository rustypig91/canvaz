mod socketcan;
pub use socketcan::SocketCanBackend;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tauri::{AppHandle, Emitter};

pub type DbcState = Arc<RwLock<HashMap<String, ParsedDbc>>>;

struct ChannelState {
    stop_flag: Arc<AtomicBool>,
}

pub struct CanManager {
    backends: Vec<Arc<Mutex<dyn CanBackend>>>,
    channels: HashMap<String, ChannelState>,
}

pub struct CanFrame {
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
}

pub trait CanChannel {
    fn name(&self) -> &str;
    fn send(&self, frame: CanFrame) -> Result<(), String>;
    fn receive(&self) -> Result<Option<CanFrame>, String>;
    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String>;
    fn get_bitrate(&self) -> Result<u32, String>;
    fn close(&mut self) -> Result<(), String>;
    fn open(&mut self) -> Result<(), String>;
}

pub trait CanBackend: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn list_channels(&self) -> Vec<String>;
    fn open_channel(
        &mut self,
        name: &str,
        bitrate: Option<u32>,
    ) -> Result<Box<dyn CanChannel>, String>;
}

impl CanManager {
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
            channels: HashMap::new(),
        }
    }

    pub fn register_backend(&mut self, backend: impl CanBackend) {
        self.backends.push(Arc::new(Mutex::new(backend)));
    }

    pub fn list_channels(&self) -> Vec<(String, String)> {
        let mut result = Vec::new();
        for backend in &self.backends {
            if let Ok(backend) = backend.lock() {
                let backend_name = backend.name().to_string();
                for channel in backend.list_channels() {
                    result.push((backend_name.clone(), channel));
                }
            }
        }
        result
    }

    fn channel_name(backend_name: &str, channel_name: &str) -> String {
        format!("{backend_name}:{channel_name}")
    }

    pub fn open_channel(
        &mut self,
        backend_name: String,
        channel_name: String,
        app: AppHandle,
        dbc: DbcState,
        bitrate: Option<u32>,
    ) -> Result<Box<dyn CanChannel>, String> {
        let full_channel_name = Self::channel_name(&backend_name, &channel_name);
        if self.channels.contains_key(&full_channel_name) {
            return Err(format!("Channel '{full_channel_name}' is already open"));
        }
        let backend = self
            .backends
            .iter()
            .find_map(|b| {
                let guard = b.lock().ok()?;
                if guard.name() == backend_name {
                    Some(Arc::clone(b))
                } else {
                    None
                }
            })
            .ok_or_else(|| format!("No backend found for channel '{channel_name}'"))?;
        let mut backend = backend
            .lock()
            .map_err(|_| "Backend lock poisoned".to_string())?;
        let channel = backend.open_channel(&channel_name, bitrate)?;
        self.channels.insert(full_channel_name, ChannelState { stop_flag: Arc::new(AtomicBool::new(false)) });
        Ok(channel)
    }

}
