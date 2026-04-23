//! Shared config file format: `$XDG_CONFIG_HOME/mesh-chat/config.toml`.
//! Any frontend can deserialize this; frontend-specific fields (e.g.
//! `log_dir` for the TUI only) are kept under `[general]` and read where
//! relevant.

use std::path::PathBuf;

use mesh_core::Network;
use serde::Deserialize;

use crate::history::HistoryConfig;

/// Top-level config. All fields are optional; a missing file parses to
/// all-defaults.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub history: HistoryConfig,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct GeneralConfig {
    /// Default serial port, overridden by CLI flag or `MESH_PORT` env var.
    pub port: Option<String>,
    /// Where to write the tracing log. Overridden by `MESH_LOG_DIR` env var.
    /// Used by the TUI; ignored by the Tauri app (which logs to the console
    /// via `tracing_subscriber::fmt`).
    pub log_dir: Option<String>,
    /// Which mesh firmware the radio on `port` is running. Controls which
    /// backend implementation gets loaded at connect time. Defaults to
    /// `meshtastic` for backwards compatibility with existing configs.
    #[serde(default)]
    pub network: NetworkChoice,
}

/// Serializable wrapper for `Network` so it can appear as `network = "meshtastic"`
/// (lowercase) in TOML. `Meshtastic` is the default for backwards compat.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkChoice {
    #[default]
    Meshtastic,
    Meshcore,
}

impl From<NetworkChoice> for Network {
    fn from(c: NetworkChoice) -> Self {
        match c {
            NetworkChoice::Meshtastic => Network::Meshtastic,
            NetworkChoice::Meshcore => Network::Meshcore,
        }
    }
}

pub fn config_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("mesh-chat").join("config.toml"))
}

/// Reads and parses the config file. Silently falls back to defaults when
/// the file is missing; prints a warning and falls back when parsing fails.
pub fn load_config() -> Config {
    let Some(path) = config_file_path() else {
        return Config::default();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    match toml::from_str::<Config>(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: failed to parse {}: {}", path.display(), e);
            Config::default()
        }
    }
}
