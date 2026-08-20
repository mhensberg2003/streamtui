//! API key storage. `TORBOX_API_KEY` overrides the config file.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub api_key: String,
}

pub fn config_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::config_dir().context("cannot locate a config directory")?,
    };
    Ok(base.join("streamtui").join("config.toml"))
}

/// Returns the key from the environment, else the config file, else None.
pub fn load_key() -> Result<Option<String>> {
    if let Ok(key) = std::env::var("TORBOX_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(Some(key));
        }
    }
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let config: Config = toml::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    let key = config.api_key.trim().to_string();
    Ok(if key.is_empty() { None } else { Some(key) })
}

/// Writes the key with 0600 permissions.
pub fn save_key(key: &str) -> Result<PathBuf> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = toml::to_string(&Config { api_key: key.trim().to_string() })?;
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("securing {}", path.display()))?;
    Ok(path)
}
