//! Video-file filtering for torrent contents.

use serde::{Deserialize, Serialize};

const VIDEO_EXTENSIONS: &[&str] =
    &["mkv", "mp4", "avi", "mov", "webm", "m4v", "ts", "m2ts", "mpg", "mpeg", "wmv", "flv"];

/// Files below this are trailers, samples and stray artwork, not the feature.
const MIN_SIZE_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFile {
    /// TorBox file id. None until the torrent exists in the account.
    pub id: Option<i64>,
    pub name: String,
    pub size: u64,
}

impl TorrentFile {
    pub fn short_name(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(&self.name)
    }
}

pub fn is_video(file: &TorrentFile) -> bool {
    let name = file.name.to_ascii_lowercase();
    if name.contains("sample") {
        return false;
    }
    if file.size < MIN_SIZE_BYTES {
        return false;
    }
    match name.rsplit('.').next() {
        Some(ext) => VIDEO_EXTENSIONS.contains(&ext),
        None => false,
    }
}

/// Applies the video filter. When it would empty the list, returns everything
/// with `filtered = false` — a cluttered list beats a dead end.
pub fn filter_videos(files: &[TorrentFile]) -> (Vec<TorrentFile>, bool) {
    let videos: Vec<_> = files.iter().filter(|f| is_video(f)).cloned().collect();
    if videos.is_empty() {
        (files.to_vec(), false)
    } else {
        (videos, true)
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
