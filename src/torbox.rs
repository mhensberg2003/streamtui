//! TorBox REST client.
//!
//! Endpoints used (all under `https://api.torbox.app/v1/api/torrents`):
//!   GET  checkcached?hash=&format=object&list_files=true  — file list, no account write
//!   POST createtorrent (multipart: magnet)                — 300/min cached, 60/hour uncached
//!   GET  mylist?id=&bypass_cache=true                     — 600s stale without bypass_cache
//!   GET  requestdl?token=&torrent_id=&file_id=            — CDN link, valid 3h to start

use crate::files::TorrentFile;
use anyhow::Result;
use serde::Deserialize;
use std::time::Duration;

const API_BASE: &str = "https://api.torbox.app/v1/api";

/// Every failure the user can actually hit, kept distinct so the TUI can say
/// what went wrong instead of "error".
#[derive(Debug)]
pub enum TorboxError {
    /// 401 — key missing, wrong or expired.
    Unauthorized,
    /// No active download slots left on the plan.
    NoSlots,
    /// Torrent is larger than the plan allows.
    TooLarge,
    /// The 60/hour uncached `createtorrent` quota is spent.
    QuotaExhausted,
    /// Network failure, DNS, timeout, 5xx.
    Unreachable(String),
    /// Anything else TorBox reported, passed through verbatim.
    Api(String),
}

impl std::fmt::Display for TorboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "TorBox rejected your API key (401) — check TORBOX_API_KEY or run `streamtui --set-key`"),
            Self::NoSlots => write!(f, "no active download slots left on your TorBox plan — finish or remove a download"),
            Self::TooLarge => write!(f, "this torrent exceeds the size limit for your TorBox plan"),
            Self::QuotaExhausted => write!(f, "uncached fetch quota spent (60/hour) — wait, or try a cached torrent"),
            Self::Unreachable(why) => write!(f, "cannot reach TorBox: {why}"),
            Self::Api(msg) => write!(f, "TorBox: {msg}"),
        }
    }
}

impl std::error::Error for TorboxError {}

/// Maps TorBox's `error` code plus `detail` text onto a distinct variant.
fn classify(status: u16, code: Option<&str>, detail: &str) -> TorboxError {
    if status == 401 || status == 403 {
        return TorboxError::Unauthorized;
    }
    let code = code.unwrap_or("").to_ascii_uppercase();
    let text = detail.to_ascii_lowercase();
    if code.contains("BAD_TOKEN") || code.contains("AUTH_ERROR") || code.contains("OAUTH") {
        TorboxError::Unauthorized
    } else if code.contains("ACTIVE_LIMIT") || text.contains("active download") || text.contains("slot") {
        TorboxError::NoSlots
    } else if code.contains("DOWNLOAD_TOO_LARGE") || text.contains("too large") {
        TorboxError::TooLarge
    } else if code.contains("MONTHLY_LIMIT") || code.contains("COOLDOWN") || status == 429 || text.contains("rate limit") {
        TorboxError::QuotaExhausted
    } else if status >= 500 {
        TorboxError::Unreachable(format!("server returned {status}"))
    } else {
        TorboxError::Api(if detail.is_empty() { format!("request failed ({status})") } else { detail.to_string() })
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Clone)]
pub struct Torbox {
    http: reqwest::Client,
    key: String,
}

/// What a torrent looks like once it exists in the account.
#[derive(Debug, Clone)]
pub struct TorrentStatus {
    pub download_present: bool,
    pub progress: f64,
    pub state: String,
    pub files: Vec<TorrentFile>,
}

impl Torbox {
    pub fn new(key: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("streamtui/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, key })
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<serde_json::Value, TorboxError> {
        let response = request
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(|e| TorboxError::Unreachable(e.to_string()))?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| TorboxError::Unreachable(e.to_string()))?;

        let envelope: Envelope = serde_json::from_str(&body)
            .map_err(|_| classify(status, None, body.trim()))?;

        if !envelope.success {
            let code = envelope.error.as_ref().and_then(|v| v.as_str()).map(str::to_string);
            let detail = envelope.detail.unwrap_or_default();
            return Err(classify(status, code.as_deref(), &detail));
        }
        Ok(envelope.data)
    }

    /// Fast path: does TorBox already hold this torrent, and what's inside it?
    /// Documented at under one second per 100 hashes. Returns None if uncached.
    pub async fn check_cached(&self, hash: &str) -> Result<Option<Vec<TorrentFile>>, TorboxError> {
        let url = format!("{API_BASE}/torrents/checkcached");
        let data = self
            .send(self.http.get(&url).query(&[
                ("hash", hash),
                ("format", "object"),
                ("list_files", "true"),
            ]))
            .await?;

        // `data` is an object keyed by hash when cached, empty/null when not.
        let entry = match &data {
            serde_json::Value::Object(map) if !map.is_empty() => map
                .values()
                .next()
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            serde_json::Value::Array(items) if !items.is_empty() => items[0].clone(),
            _ => return Ok(None),
        };

        Ok(Some(parse_files(entry.get("files"))))
    }

    /// Adds the torrent to the account. Instant for cached torrents; for
    /// uncached ones this consumes one of the 60 hourly fetches.
    pub async fn create_torrent(&self, magnet: &str) -> Result<i64, TorboxError> {
        let url = format!("{API_BASE}/torrents/createtorrent");
        let form = reqwest::multipart::Form::new()
            .text("magnet", magnet.to_string())
            .text("seed", "1")
            .text("allow_zip", "false");

        let data = self.send(self.http.post(&url).multipart(form)).await?;
        data.get("torrent_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| TorboxError::Api("createtorrent returned no torrent_id".into()))
    }

    /// Current state of one torrent. Always bypasses the 600s server cache —
    /// stale progress is worse than an extra request against a 300/min budget.
    pub async fn torrent_status(&self, torrent_id: i64) -> Result<TorrentStatus, TorboxError> {
        let url = format!("{API_BASE}/torrents/mylist");
        let data = self
            .send(self.http.get(&url).query(&[
                ("id", torrent_id.to_string()),
                ("bypass_cache", "true".to_string()),
            ]))
            .await?;

        let entry = match data {
            serde_json::Value::Array(mut items) if !items.is_empty() => items.remove(0),
            other => other,
        };

        Ok(TorrentStatus {
            download_present: entry
                .get("download_present")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            progress: entry.get("progress").and_then(|v| v.as_f64()).unwrap_or(0.0),
            state: entry
                .get("download_state")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            files: parse_files(entry.get("files")),
        })
    }

    /// Resolves the short-lived CDN URL. Called only by the local proxy, so the
    /// API key in this request never reaches mpv.
    pub async fn request_download_url(
        &self,
        torrent_id: i64,
        file_id: i64,
    ) -> Result<String, TorboxError> {
        let url = format!("{API_BASE}/torrents/requestdl");
        let data = self
            .send(self.http.get(&url).query(&[
                ("token", self.key.clone()),
                ("torrent_id", torrent_id.to_string()),
                ("file_id", file_id.to_string()),
                ("redirect", "false".to_string()),
            ]))
            .await?;

        match data {
            serde_json::Value::String(url) => Ok(url),
            other => other
                .get("url")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| TorboxError::Api("requestdl returned no URL".into())),
        }
    }
}

fn parse_files(value: Option<&serde_json::Value>) -> Vec<TorrentFile> {
    let Some(serde_json::Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| TorrentFile {
            id: item.get("id").and_then(|v| v.as_i64()),
            name: item
                .get("name")
                .or_else(|| item.get("short_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            size: item.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
        })
        .collect()
}
