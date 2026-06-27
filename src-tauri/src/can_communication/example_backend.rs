use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use super::{CanBackend, CanFrame, RxHandle, TxHandle};

// ── TX half ───────────────────────────────────────────────────────────────────

pub(crate) struct ExampleTxHandle;

impl TxHandle for ExampleTxHandle {
    fn send(&mut self, _frame: &CanFrame) -> Result<(), String> {
        Ok(())
    }
    fn close(&mut self) {}
}

// ── RX half ───────────────────────────────────────────────────────────────────

pub(crate) struct ExampleRxHandle {
    receiver: Receiver<CanFrame>,
}

impl RxHandle for ExampleRxHandle {
    fn receive(&mut self, timeout_ms: u64) -> Result<Option<CanFrame>, String> {
        match self.receiver.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err("Channel disconnected".to_string()),
        }
    }
    fn close(&mut self) {}
}

// ── Backend ───────────────────────────────────────────────────────────────────

/// Dummy backend for testing. Call [`ExampleBackend::inject`] to push frames
/// into the RX path of an open channel.
pub struct ExampleBackend {
    injectors: std::collections::HashMap<u8, Sender<CanFrame>>,
}

impl ExampleBackend {
    pub fn new() -> Self {
        Self { injectors: std::collections::HashMap::new() }
    }

    /// Simulate an incoming frame on `channel`. Has no effect if the channel
    /// is not open or the RX thread has already exited.
    pub fn inject(&self, channel: u8, frame: CanFrame) {
        if let Some(sender) = self.injectors.get(&channel) {
            let _ = sender.send(frame);
        }
    }
}

impl CanBackend for ExampleBackend {
    fn list_channels(&self) -> Vec<String> {
        (0u8..4).map(|i| format!("Example Channel {i}")).collect()
    }

    fn open_channel(
        &mut self,
        index: u8,
        _bitrate: u32,
    ) -> Result<(Box<dyn TxHandle>, Box<dyn RxHandle>), String> {
        let (sender, receiver) = mpsc::channel();
        self.injectors.insert(index, sender);
        Ok((Box::new(ExampleTxHandle), Box::new(ExampleRxHandle { receiver })))
    }
}
