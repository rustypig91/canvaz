mod socketcan;
pub use socketcan::SocketCanBackend;

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

pub trait CanBackend {
    fn name(&self) -> &str;
    fn list_channels(&self) -> Vec<String>;
    fn open_channel(
        &mut self,
        name: &str,
        bitrate: Option<u32>,
    ) -> Result<Box<dyn CanChannel>, String>;
}
