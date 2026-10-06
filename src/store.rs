//! On-disk session list, so the tabs you had open come back after a restart.
//!
//! This is a convenience, never something to fail startup over: a missing,
//! corrupt or older file is discarded silently and you start empty. It sits
//! beside `config.toml` and is written `0600` — a list of magnets is a record
//! of what you watch.

use crate::files::TorrentFile;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bumped when the shape changes, so an old file is dropped, not misread.
const VERSION: u32 = 1;

/// Sessions kept, most recent first. Older ones fall off the end.
pub const MAX_SESSIONS: usize = 20;

#[derive(Debug, Serialize, Deserialize)]
pub struct Stored {
    pub version: u32,
    /// Hash of the session that had focus.
    #[serde(default)]
    pub current: Option<String>,
    /// In tab order.
    #[serde(default)]
    pub sessions: Vec<StoredSession>,
}

impl Stored {
    /// Stamps the current version, so callers cannot forget to.
    pub fn new(current: Option<String>, sessions: Vec<StoredSession>) -> Self {
        Self { version: VERSION, current, sessions }
    }
}

impl Default for Stored {
    fn default() -> Self {
        Self::new(None, Vec::new())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoredSession {
    pub hash: String,
    pub uri: String,
    pub name: String,
    /// Kept so a restored session is playable without another API round trip.
    #[serde(default)]
    pub torrent_id: Option<i64>,
    #[serde(default)]
    pub files: Vec<TorrentFile>,
    #[serde(default)]
    pub filtered: bool,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub selected: usize,
}

pub fn path() -> Result<PathBuf> {
    Ok(crate::config::config_path()?.with_file_name("sessions.json"))
}

pub fn load(path: &Path) -> Stored {
    let Ok(text) = std::fs::read_to_string(path) else { return Stored::default() };
    match serde_json::from_str::<Stored>(&text) {
        Ok(stored) if stored.version == VERSION => stored,
        _ => Stored::default(),
    }
}

/// Writes through a temporary file, so a crash mid-write cannot leave a
/// half-written list behind.
pub fn save(path: &Path, stored: &Stored) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(stored)?;
    std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("securing {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
