pub(crate) struct ExampleBackend {}

impl ExampleBackend {
    pub fn open(&mut self, channel: u8, bitrate: u32) -> Result<(), String> {
        Ok(())
    }
}
