mod socketcan;
pub use socketcan::SocketCanBackend;
use std::sync::Arc;
use serde::Serialize;
use crate::app_state::AppState;

// ── Frame ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CanFrame {
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
}

// ── Traits ────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
pub trait CanChannel: Send {
    fn name(&self) -> &str;
    fn app_state(&self) -> &Arc<AppState>;
    fn open(&mut self) -> Result<(), String>;
    fn close(&mut self) -> Result<(), String>;
    fn send(&self, frame: CanFrame) -> Result<(), String>;
    fn receive(&self) -> Result<Option<CanFrame>, String>;
    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String>;
    fn get_bitrate(&self) -> Result<u32, String>;
}

#[allow(dead_code)]
pub trait CanBackend: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn list_channels(&self) -> Vec<String>;
    fn open_channel(
        &mut self,
        name: &str,
        bitrate: Option<u32>,
        state: Arc<AppState>,
    ) -> Result<Box<dyn CanChannel>, String>;
}
