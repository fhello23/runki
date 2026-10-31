//! Per-machine settings shared by the desktop and terminal interfaces.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub library: Option<PathBuf>,
    #[serde(default)]
    pub last_open_deck: Option<PathBuf>,
    #[serde(default)]
    pub terminal_light: bool,
    #[serde(default)]
    pub web_on: bool,
    #[serde(default = "default_web_port")]
    pub web_port: u16,
}

fn default_web_port() -> u16 {
    8765
}

impl Default for Config {
    fn default() -> Self {
        Self {
            library: None,
            last_open_deck: None,
            terminal_light: false,
            web_on: false,
            web_port: default_web_port(),
        }
    }
}

/// Import the pre-TUI desktop settings without changing the old app.ron file.
/// The directory matches eframe 0.31's `storage_dir("runki")`.
pub fn load_legacy() -> Result<Option<Config>> {
    let Some(dir) = dirs::data_dir() else {
        return Ok(None);
    };
    let dir = dir.join("runki");
    #[cfg(target_os = "windows")]
    let dir = dir.join("data");
    load_legacy_file(&dir.join("app.ron"))
}

fn load_legacy_file(path: &Path) -> Result<Option<Config>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("Could not read {}", path.display())),
    };
    let values: HashMap<String, String> =
        ron::from_str(&text).with_context(|| format!("Could not parse {}", path.display()))?;
    values
        .get("runki_config")
        .map(|value| {
            ron::from_str(value)
                .with_context(|| format!("Could not parse library settings in {}", path.display()))
        })
        .transpose()
}

pub fn file_path() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|dir| dir.join("runki").join("config.json"))
        .context("Could not find the config directory")
}

/// A missing file allows the GUI to import its older eframe settings.
pub fn load(path: &Path) -> Result<Option<Config>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("Could not read {}", path.display())),
    };
    serde_json::from_str(&text)
        .with_context(|| format!("Could not parse {}", path.display()))
        .map(Some)
}

/// Refuse to replace unreadable settings; replace valid settings atomically.
pub fn save(path: &Path, config: &Config) -> Result<()> {
    load(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(config)?)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("Could not save {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_old_desktop_config_and_defaults_new_terminal_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.ron");
        let old = "(library:Some(\"/my/decks\"),last_open_deck:None)";
        let values = HashMap::from([("runki_config", old)]);
        let text = ron::to_string(&values).unwrap();
        std::fs::write(&path, &text).unwrap();
        let config = load_legacy_file(&path).unwrap().unwrap();
        assert_eq!(config.library, Some(PathBuf::from("/my/decks")));
        assert!(!config.terminal_light);
        assert!(!config.web_on);
        assert_eq!(config.web_port, 8765);
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
        assert_eq!(
            serde_json::from_str::<Config>("{}").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn settings_roundtrip_and_unreadable_settings_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runki/config.json");
        assert_eq!(load(&path).unwrap(), None);
        let config = Config {
            library: Some(dir.path().to_path_buf()),
            last_open_deck: None,
            web_on: true,
            web_port: 9876,
            ..Default::default()
        };
        save(&path, &config).unwrap();
        assert_eq!(load(&path).unwrap(), Some(config.clone()));
        std::fs::write(&path, "broken").unwrap();
        assert!(save(&path, &config).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");
    }
}
