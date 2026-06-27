use serde::Serialize;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Serialize)]
pub enum Direction {
    Rx = 0,
    Tx = 1,
}

#[derive(Debug, Clone, Serialize)]
pub struct CanSignal {
    pub name: String,
    pub physical: f64,
    pub raw: u64,
    pub dlc: u64,
    pub signals: Vec<CanSignal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecodedCanMessage {
    pub name: String,
    pub signals: Vec<CanSignal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CanFrame {
    pub can_id: u32,
    pub is_extended: bool,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
    pub direction: Direction,
    pub decoded: Option<DecodedCanMessage>,
}

impl CanFrame {
    pub fn new(can_id: u32, is_extended: bool, data: Vec<u8>, timestamp_ms: Option<u64>, direction: Direction) -> Self {
        Self {
            can_id,
            is_extended,
            data,
            timestamp_ms: timestamp_ms.unwrap_or_else(|| now_ms()),
            direction,
            decoded: None,
        }
    }
}
