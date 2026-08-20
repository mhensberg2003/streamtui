//! Localhost pass-through proxy.
//!
//! mpv gets `http://127.0.0.1:<port>/stream/<id>` and nothing else — the API
//! key never enters mpv's argv, its log, or the process table. Range headers
//! are forwarded verbatim so mpv seeks natively against the TorBox CDN and
//! does its own readahead; we add no buffering of our own.

use crate::torbox::Torbox;
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Headers worth passing through in each direction. Everything else is noise
/// that only risks confusing the CDN or mpv.
const FORWARD_REQUEST: &[&str] = &["range", "if-range"];
const FORWARD_RESPONSE: &[&str] = &[
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
    "last-modified",
    "etag",
];

#[derive(Debug, Clone)]
struct StreamTarget {
    torrent_id: i64,
    file_id: i64,
}

#[derive(Clone)]
pub struct ProxyHandle {
    port: u16,
    next_id: Arc<AtomicU64>,
    routes: Arc<Mutex<HashMap<String, StreamTarget>>>,
}

impl ProxyHandle {
    /// Registers a file and returns the localhost URL for mpv.
    pub fn publish(&self, torrent_id: i64, file_id: i64) -> String {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let token = format!("{id}-{torrent_id}-{file_id}");
        self.routes
            .lock()
            .expect("proxy routes poisoned")
            .insert(token.clone(), StreamTarget { torrent_id, file_id });
        format!("http://127.0.0.1:{}/stream/{token}", self.port)
    }
}

#[derive(Clone)]
struct ProxyState {
    torbox: Torbox,
    http: reqwest::Client,
    routes: Arc<Mutex<HashMap<String, StreamTarget>>>,
}

/// Binds an ephemeral port on the loopback interface and serves until the
/// process exits. Returns once the port is known.
pub async fn start(torbox: Torbox) -> Result<ProxyHandle> {
    let routes: Arc<Mutex<HashMap<String, StreamTarget>>> = Arc::new(Mutex::new(HashMap::new()));

    // No timeout: a stream is a long-lived body, and the read may idle while
    // the viewer is paused.
    let http = reqwest::Client::builder()
        .user_agent(concat!("streamtui/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let state = ProxyState { torbox, http, routes: routes.clone() };
    let app = axum::Router::new()
        .route("/stream/{token}", get(stream).head(stream))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding the local stream proxy")?;
    let port = listener.local_addr()?.port();

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Ok(ProxyHandle { port, next_id: Arc::new(AtomicU64::new(1)), routes })
}

async fn stream(
    State(state): State<ProxyState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    let target = {
        let routes = state.routes.lock().expect("proxy routes poisoned");
        routes.get(&token).cloned()
    };
    let Some(target) = target else {
        return (StatusCode::NOT_FOUND, "unknown stream").into_response();
    };

    // Resolved per request rather than cached: `requestdl` links expire after
    // three hours, and one extra call against a 300/min budget is free.
    let url = match state
        .torbox
        .request_download_url(target.torrent_id, target.file_id)
        .await
    {
        Ok(url) => url,
        Err(err) => {
            return (StatusCode::BAD_GATEWAY, err.to_string()).into_response();
        }
    };

    let mut request = state.http.get(&url);
    for name in FORWARD_REQUEST {
        if let Some(value) = headers.get(*name) {
            request = request.header(*name, value);
        }
    }

    let upstream = match request.send().await {
        Ok(response) => response,
        Err(err) => {
            return (StatusCode::BAD_GATEWAY, format!("CDN request failed: {err}")).into_response();
        }
    };

    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);
    for name in FORWARD_RESPONSE {
        if let Some(value) = upstream.headers().get(*name) {
            response = response.header(*name, value);
        }
    }
    // Advertise range support even if the CDN omitted the header, so mpv will
    // attempt seeks rather than assuming a non-seekable stream.
    if upstream.headers().get(header::ACCEPT_RANGES).is_none() {
        response = response.header(header::ACCEPT_RANGES, "bytes");
    }

    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "bad response").into_response())
}
