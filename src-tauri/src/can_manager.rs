use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::backends::{default_backends, Backend, Channel};
use crate::can_frame::CanFrame;
use crate::dbc_parser::ParsedDbc;

// Re-export so lib.rs import lines stay unchanged.
pub use crate::backends::{CanFrameEvent, SubscribedSignals};

pub type ManagerState = Arc<Mutex<CanManager>>;

// ── Channel identity ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub id: String,
    pub backend: String,
    pub name: String,
    pub dbc: Option<ParsedDbc>,
}

// ── Manager internals ─────────────────────────────────────────────────────────

struct OpenChannelState {
    channel: Arc<Mutex<Channel>>,
    channel_info: ChannelInfo,
}

pub struct CanManager {
    state: Arc<AppState>,
    backends: Vec<Backend>,
    channels: HashMap<String, OpenChannelState>,
    subscribed: SubscribedSignals,
}

impl CanManager {
    pub fn new(state: Arc<AppState>, subscribed: SubscribedSignals) -> Self {
        Self {
            state,
            backends: default_backends(),
            channels: HashMap::new(),
            subscribed,
        }
    }

    pub fn list_channels(&self) -> Result<Vec<ChannelInfo>, String> {
        let mut out = Vec::new();
        for b in &self.backends {
            let bname = b.name().to_string();
            for ch in b.list_channels()? {
                out.push(ChannelInfo { id: format!("{bname}:{ch}"), backend: bname.clone(), name: ch, dbc: None });
            }
        }
        Ok(out)
    }

    pub fn open_channel(
        &mut self,
        backend_name: String,
        channel_name: String,
        bitrate: Option<u32>,
        dbc_path: Option<&str>,
    ) -> Result<ChannelInfo, String> {
        let id = format!("{backend_name}:{channel_name}");

        if self.channels.contains_key(&id) {
            let cs = &self.channels[&id];
            if let Some(br) = bitrate {
                cs.channel.lock()
                    .map_err(|_| "Channel lock poisoned".to_string())?
                    .set_bitrate(br)?;
            }
            return Ok(cs.channel_info.clone());
        }

        let backend = self.backends.iter()
            .find(|b| b.name() == backend_name)
            .ok_or_else(|| format!("No backend '{backend_name}'"))?;

        let mut ch = backend.open_channel(
            &channel_name,
            bitrate,
            Arc::clone(&self.state),
            dbc_path,
            id.clone(),
            Arc::clone(&self.subscribed),
        )?;
        ch.open()?; // opens hardware and spawns the receive thread

        let channel_info = ChannelInfo {
            id: id.clone(),
            backend: backend_name,
            name: channel_name,
            dbc: ch.get_dbc().cloned(),
        };

        self.channels.insert(id, OpenChannelState {
            channel: Arc::new(Mutex::new(ch)),
            channel_info: channel_info.clone(),
        });
        Ok(channel_info)
    }

    pub fn close_channel(&mut self, channel_id: &str) -> Result<(), String> {
        let state = self.channels
            .remove(channel_id)
            .ok_or_else(|| format!("'{channel_id}' is not open"))?;
        let ch_arc = Arc::clone(&state.channel);
        drop(state);
        let result = ch_arc.lock()
            .map_err(|_| "Channel lock poisoned".to_string())?
            .close();
        result
    }

    pub fn open_channels_info(&self) -> Vec<ChannelInfo> {
        self.channels.values().map(|s| s.channel_info.clone()).collect()
    }

    pub fn send_frame(&self, channel_id: &str, frame: CanFrame) -> Result<(), String> {
        self.channels
            .get(channel_id)
            .ok_or_else(|| format!("'{channel_id}' is not open"))?
            .channel
            .lock()
            .map_err(|_| "Channel lock poisoned".to_string())?
            .send(frame)
    }

    /// Returns an Arc to the channel so callers can release the CanManager lock
    /// before locking the channel, avoiding lock stacking on busy buses.
    pub fn channel_arc(&self, channel_id: &str) -> Option<Arc<Mutex<Channel>>> {
        self.channels.get(channel_id).map(|s| Arc::clone(&s.channel))
    }

    /// Returns Arcs for all open channels so callers can release the CanManager
    /// lock before locking individual channels.
    pub fn all_channel_arcs(&self) -> Vec<Arc<Mutex<Channel>>> {
        self.channels.values().map(|s| Arc::clone(&s.channel)).collect()
    }
}
