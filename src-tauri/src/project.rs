use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Project {
    pub version: u32,
    pub channels: Vec<ChannelConfig>,
    /// Each entry is one plot pane; each pane holds its signal list.
    pub plot_panes: Vec<PlotPaneConfig>,
    pub simulate_signals: Vec<SimulateEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(default = "default_backend")]
    pub backend: String,
    pub dbc_path: Option<String>,
    #[serde(default)]
    pub bitrate: Option<u32>,
}

fn default_backend() -> String {
    "socketcan".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlotPaneConfig {
    pub signals: Vec<PlotSignalEntry>,
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

impl Project {
    pub fn new() -> Self {
        Self {
            version: 1,
            ..Default::default()
        }
    }

    pub fn save(&self, path: &str) -> Result<(), String> {
        let p = std::path::Path::new(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Dir error: {e}"))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Serialize error: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("Write error: {e}"))
    }

    pub fn load(path: &str) -> Result<Self, String> {
        let json = std::fs::read_to_string(path)
            .map_err(|e| format!("Read error: {e}"))?;
        serde_json::from_str(&json).map_err(|e| format!("Parse error: {e}"))
    }
}
