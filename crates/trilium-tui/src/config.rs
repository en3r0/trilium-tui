//! `~/.config/trilium-tui/config.json` (all fields optional).
use serde::Deserialize;

#[derive(Deserialize, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Open (creating if needed) today's journal note at startup, like a "dashboard on open" setup.
    pub open_today_on_startup: bool,
    /// Server URL to sync with when the replica doesn't remember one.
    pub server: Option<String>,
    /// Show pictures inside notes (editor and reading view) instead of `[image: …]` labels; toggled with Alt+V.
    pub inline_images: bool,
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

pub fn config_path() -> Option<std::path::PathBuf> { dirs::config_dir().map(|d| d.join("trilium-tui").join("config.json")) }

/// Remember the inline-pictures switch in a config file, keeping everything else in it.
pub fn save_inline_images_to(path: &std::path::Path, on: bool) -> std::io::Result<()> {
    let mut v: serde_json::Value = std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).filter(|v: &serde_json::Value| v.is_object()).unwrap_or_else(|| serde_json::json!({}));
    v["inlineImages"] = serde_json::Value::Bool(on);
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    std::fs::write(path, serde_json::to_string_pretty(&v).unwrap_or_default() + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picture_switch_is_saved_without_losing_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.json");
        save_inline_images_to(&path, true).unwrap();
        let cfg: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(cfg.inline_images);
        std::fs::write(&path, r#"{"server": "http://x:8080", "openTodayOnStartup": true}"#).unwrap();
        save_inline_images_to(&path, false).unwrap();
        let cfg: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!cfg.inline_images && cfg.open_today_on_startup && cfg.server.as_deref() == Some("http://x:8080"));
        std::fs::write(&path, "not json").unwrap();
        save_inline_images_to(&path, true).unwrap();
        assert!(serde_json::from_str::<Config>(&std::fs::read_to_string(&path).unwrap()).unwrap().inline_images, "a broken file is replaced");
    }
}
