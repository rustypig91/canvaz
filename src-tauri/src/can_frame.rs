use serde::Serialize;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Serialize)]
pub struct CanFrame {
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
}


impl CanFrame {
    pub fn new(can_id: u32, is_extended: bool, data: Vec<u8>, timestamp_ms: Option<u64>) -> Self {
        Self {
            can_id,
            is_extended,
            data,
            timestamp_ms: timestamp_ms.unwrap_or_else(|| now_ms()),
        }
    }
}
