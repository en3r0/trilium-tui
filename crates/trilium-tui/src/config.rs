//! `~/.config/trilium-tui/config.json` (all fields optional).
use serde::Deserialize;

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Open (creating if needed) today's journal note at startup, like a "dashboard on open" setup.
    pub open_today_on_startup: bool,
    /// Server URL to sync with when the replica doesn't remember one.
    pub server: Option<String>,
}

impl Config {
    pub fn load() -> Self {
        dirs::config_dir()
            .map(|d| d.join("trilium-tui").join("config.json"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}
