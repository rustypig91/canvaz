use log::{debug, info};
use serde::{Deserialize, Serialize};

use crate::sim_generator::SignalGen;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Project {
    pub version: u32,
    pub channels: Vec<ChannelConfig>,
    /// Each entry is one plot pane; each pane holds its signal list.
    pub plot_panes: Vec<PlotPaneConfig>,
    /// One entry per simulated message instance. Allows the same message to be
    /// simulated multiple times with independent values/period.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub simulate_messages: Vec<SimulateMessage>,
    #[serde(default)]
    pub simulate_raw_frames: Vec<SimulateRawFrame>,
    #[serde(default)]
    pub trace_filters: Option<TraceFiltersConfig>,
    /// Data-retention window in seconds; samples older than this are discarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_size_sec: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_columns: Option<TraceColumnsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub name: String,
    /// Optional user-chosen display name shown in the UI and used in CSV
    /// exports; `name` stays the hardware identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default = "default_backend")]
    pub backend: String,
    pub dbc_path: Option<String>,
    #[serde(default = "default_bitrate")]
    pub bitrate: u32,
    /// Protocol interpretation for received frames ("j1939"); None = raw CAN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

fn default_bitrate() -> u32 {
    500_000
}

fn default_backend() -> String {
    "socketcan".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlotPaneConfig {
    pub signals: Vec<PlotSignalEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpolation: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub show_points: bool,
    /// Manual Y-axis lock; both present = locked, absent = auto-scale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_max: Option<f64>,
}

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlotSignalEntry {
    pub signal_name: String,
    pub channel: String,
    /// DBC message id the signal belongs to; disambiguates same-named signals
    /// in different messages when a pane is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulateMessage {
    pub channel: String,
    pub message_id: u32,
    pub period_ms: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub running: bool,
    pub signals: Vec<SimulateMessageSignal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulateMessageSignal {
    pub name: String,
    pub value: f64,
    /// Value generator (ramp/sine/toggle/counter/checksum); None = constant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<SignalGen>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulateRawFrame {
    pub channel: String,
    pub can_id: u32,
    pub is_extended: bool,
    pub dlc: u8,
    pub data: Vec<u8>,
    pub period_ms: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TraceFiltersConfig {
    #[serde(default)]
    pub channels: Option<Vec<String>>,
    #[serde(default)]
    pub can_ids: Option<Vec<u32>>,
    #[serde(default)]
    pub msg_names: Option<Vec<String>>,
    #[serde(default)]
    pub dir: Option<Vec<String>>,
    /// J1939 column filters; -1 stands for frames without J1939 info.
    #[serde(default)]
    pub pgns: Option<Vec<i64>>,
    #[serde(default)]
    pub prios: Option<Vec<i64>>,
    #[serde(default)]
    pub sas: Option<Vec<i64>>,
    #[serde(default)]
    pub das: Option<Vec<i64>>,
    /// true = broadcast (PDU2) PGNs only, false = destination-specific only.
    #[serde(default)]
    pub broadcast: Option<bool>,
    #[serde(default)]
    pub dlc_min: Option<u32>,
    #[serde(default)]
    pub dlc_max: Option<u32>,
    #[serde(default)]
    pub cycle_min: Option<f64>,
    #[serde(default)]
    pub cycle_max: Option<f64>,
    #[serde(default)]
    pub data: Vec<Option<u8>>,
    #[serde(default = "default_data_format")]
    pub data_format: String,
    #[serde(default = "default_true")]
    pub overwrite: bool,
    #[serde(default)]
    pub max_rows: Option<u32>,
}

fn default_data_format() -> String {
    "hex".to_string()
}
fn default_true() -> bool {
    true
}

/// Column widths are intentionally not part of this: sessions always start at
/// the default widths (a `widths` key in older project files is ignored).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TraceColumnsConfig {
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub hidden: Vec<String>,
}

#[allow(dead_code)]
impl Project {
    pub fn new() -> Self {
        Self {
            version: 1,
            ..Default::default()
        }
    }

    pub fn has_changes(&self, path: &str) -> bool {
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(_) => return true,  // If we can't serialize, assume there are changes to save.
        };
        let current_json = std::fs::read_to_string(path).unwrap_or_default();
        json != current_json
    }

    pub fn save(&self, path: &str) -> Result<(), String> {
        let p = std::path::Path::new(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Dir error: {e}"))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Serialize error: {e}"))?;
        let current_json = std::fs::read_to_string(path).unwrap_or_default();

        if json == current_json {
            debug!("Project file at '{}' unchanged, skipping write", path);
            return Ok(());
        }
        info!("Saving project to '{}'", path);
        std::fs::write(path, json).map_err(|e| format!("Write error: {e}"))
    }

    pub fn load(path: &str) -> Result<Self, String> {
        info!("Loading project from '{}'", path);
        let json = std::fs::read_to_string(path).map_err(|e| format!("Read error: {e}"))?;
        serde_json::from_str(&json).map_err(|e| format!("Parse error: {e}"))
    }
}
