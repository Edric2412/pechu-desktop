//! Small runtime settings file (`settings.json` in the config folder).
//! For now it only remembers which port the local server used last.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 47317;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Settings {
    pub preferred_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { preferred_port: DEFAULT_PORT }
    }
}

fn file_path(config_dir: &Path) -> PathBuf {
    config_dir.join("settings.json")
}

/// Reads the file. A missing or broken file gives the defaults: the app must always start.
pub fn load(config_dir: &Path) -> Settings {
    std::fs::read_to_string(file_path(config_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(config_dir: &Path, settings: &Settings) -> anyhow::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    std::fs::write(file_path(config_dir), serde_json::to_string_pretty(settings)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_broken_file_gives_default_and_save_round_trips() {
        let dir = std::env::temp_dir().join(format!("pechu-settings-test-{}", std::process::id()));
        assert_eq!(load(&dir), Settings::default());

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), "not json").unwrap();
        assert_eq!(load(&dir), Settings::default());

        save(&dir, &Settings { preferred_port: 5000 }).unwrap();
        assert_eq!(load(&dir).preferred_port, 5000);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
