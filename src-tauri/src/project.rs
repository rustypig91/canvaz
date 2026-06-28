use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Project {
    pub version: u32,
    pub channels: Vec<ChannelConfig>,
    /// Each entry is one plot pane; each pane holds its signal list.
    pub plot_panes: Vec<PlotPaneConfig>,
    pub simulate_signals: Vec<SimulateEntry>,
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
    #[serde(default = "default_backend")]
    pub backend: String,
    pub dbc_path: Option<String>,
    #[serde(default = "default_bitrate")]
    pub bitrate: u32,
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
}

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlotSignalEntry {
    pub signal_name: String,
    pub channel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulateEntry {
    pub signal_name: String,
    pub channel: String,
    pub value: f64,
    pub period_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulateRawFrame {
    pub channel: String,
    pub can_id: u32,
    pub is_extended: bool,
    pub dlc: u8,
    pub data: Vec<u8>,
    pub period_ms: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TraceColumnsConfig {
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub hidden: Vec<String>,
    #[serde(default)]
    pub widths: std::collections::HashMap<String, u32>,
}

#[allow(dead_code)]
impl Project {
    pub fn new() -> Self {
        Self {
            version: 1,
            ..Default::default()
        }
    }

    pub fn save(&self, path: &str) -> Result<(), String> {
        info!("Saving project to '{}'", path);
        let p = std::path::Path::new(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Dir error: {e}"))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Serialize error: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("Write error: {e}"))
    }

    pub fn load(path: &str) -> Result<Self, String> {
        info!("Loading project from '{}'", path);
        let json = std::fs::read_to_string(path).map_err(|e| format!("Read error: {e}"))?;
        serde_json::from_str(&json).map_err(|e| format!("Parse error: {e}"))
    }
}
