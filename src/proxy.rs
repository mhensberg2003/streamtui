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
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// Refresh before TorBox's three-hour link lifetime ends.
const DOWNLOAD_URL_TTL: Duration = Duration::from_secs(2 * 60 * 60);

struct DownloadUrl {
    url: String,
    expires_at: Instant,
}

type UrlSlot = Arc<tokio::sync::Mutex<Option<Arc<DownloadUrl>>>>;

#[derive(Default)]
struct DownloadUrls {
    files: Mutex<HashMap<(i64, i64), UrlSlot>>,
}

impl DownloadUrls {
    fn slot(&self, target: &StreamTarget) -> UrlSlot {
        self.files
            .lock()
            .expect("download URLs poisoned")
            .entry((target.torrent_id, target.file_id))
            .or_default()
            .clone()
    }
}

async fn download_url<F, Fut>(
    slot: &UrlSlot,
    rejected: Option<&Arc<DownloadUrl>>,
    resolve: F,
) -> Result<Arc<DownloadUrl>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    // Only requests for this file wait on its URL lookup. No lock is held
    // while reading the CDN response or streaming its body.
    let mut cached = slot.lock().await;
    if let Some(url) = cached.as_ref() {
        let was_rejected = rejected.is_some_and(|old| Arc::ptr_eq(old, url));
        if !was_rejected && Instant::now() < url.expires_at {
            return Ok(url.clone());
        }
    }
    *cached = None;
    let expires_at = Instant::now() + DOWNLOAD_URL_TTL;
    let url = Arc::new(DownloadUrl {
        url: resolve().await?,
        expires_at,
    });
    *cached = Some(url.clone());
    Ok(url)
}

async fn fetch_stream<F, Fut>(
    http: &reqwest::Client,
    slot: &UrlSlot,
    headers: &HeaderMap,
    mut resolve: F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let mut url = download_url(slot, None, &mut resolve).await?;
    for attempt in 0..2 {
        let mut request = http.get(&url.url);
        for name in FORWARD_REQUEST {
            if let Some(value) = headers.get(*name) {
                request = request.header(*name, value);
            }
        }
        let response = request
            .send()
            .await
            .map_err(|err| anyhow::anyhow!("CDN request failed: {}", err.without_url()))?;
        if matches!(response.status().as_u16(), 401 | 403 | 410) {
            if attempt == 0 {
                drop(response);
                url = download_url(slot, Some(&url), &mut resolve).await?;
                continue;
            }
            // Don't retain a link that failed even after refreshing it.
            let mut cached = slot.lock().await;
            if cached
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &url))
            {
                *cached = None;
            }
        }
        return Ok(response);
    }
    unreachable!("the second CDN response is always returned")
}

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
        self.routes.lock().expect("proxy routes poisoned").insert(
            token.clone(),
            StreamTarget {
                torrent_id,
                file_id,
            },
        );
        format!("http://127.0.0.1:{}/stream/{token}", self.port)
    }
}

#[derive(Clone)]
struct ProxyState {
    torbox: Torbox,
    http: reqwest::Client,
    routes: Arc<Mutex<HashMap<String, StreamTarget>>>,
    download_urls: Arc<DownloadUrls>,
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

    let state = ProxyState {
        torbox,
        http,
        routes: routes.clone(),
        download_urls: Arc::new(DownloadUrls::default()),
    };
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

    Ok(ProxyHandle {
        port,
        next_id: Arc::new(AtomicU64::new(1)),
        routes,
    })
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

    let slot = state.download_urls.slot(&target);
    let upstream = match fetch_stream(&state.http, &slot, &headers, || async {
        Ok(state
            .torbox
            .request_download_url(target.torrent_id, target.file_id)
            .await?)
    })
    .await
    {
        Ok(response) => response,
        Err(err) => {
            return (StatusCode::BAD_GATEWAY, err.to_string()).into_response();
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn shares_lookups_and_refreshes_expired_urls() {
        let cache = DownloadUrls::default();
        let target = StreamTarget {
            torrent_id: 1,
            file_id: 2,
        };
        let slot = cache.slot(&target);
        assert!(Arc::ptr_eq(&slot, &cache.slot(&target)));
        assert!(!Arc::ptr_eq(
            &slot,
            &cache.slot(&StreamTarget {
                torrent_id: 1,
                file_id: 3
            })
        ));
        let calls = AtomicUsize::new(0);
        let resolve = || async {
            calls.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            Ok("https://cdn.invalid/file".to_string())
        };
        let (a, b) = tokio::join!(
            download_url(&slot, None, resolve),
            download_url(&slot, None, resolve)
        );
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        *slot.lock().await = Some(Arc::new(DownloadUrl {
            url: "expired".into(),
            expires_at: Instant::now(),
        }));
        download_url(&slot, None, resolve).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn concurrent_rejections_refresh_once_even_if_url_is_unchanged() {
        let slot = UrlSlot::default();
        let old = download_url(&slot, None, || async { Ok("same-url".into()) })
            .await
            .unwrap();
        let calls = AtomicUsize::new(0);
        let resolve = || async {
            calls.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            Ok("same-url".into())
        };
        let (a, b) = tokio::join!(
            download_url(&slot, Some(&old), resolve),
            download_url(&slot, Some(&old), resolve),
        );
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn failed_resolution_is_not_cached() {
        let slot = UrlSlot::default();
        assert!(
            download_url(&slot, None, || async { anyhow::bail!("offline") })
                .await
                .is_err()
        );
        assert!(slot.lock().await.is_none());
        assert!(
            download_url(&slot, None, || async { Ok("recovered".into()) })
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn retries_rejected_links_once_and_preserves_range_headers() {
        for status in [401, 403, 410, 404, 416, 500] {
            let requests = Arc::new(AtomicUsize::new(0));
            let seen = requests.clone();
            let app = axum::Router::new().route(
                "/",
                get(move |headers: HeaderMap| {
                    let seen = seen.clone();
                    async move {
                        assert_eq!(headers[header::RANGE], "bytes=100-199");
                        assert_eq!(headers[header::IF_RANGE], "\"version\"");
                        seen.fetch_add(1, Ordering::Relaxed);
                        StatusCode::from_u16(status).unwrap()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let mut headers = HeaderMap::new();
            headers.insert(header::RANGE, "bytes=100-199".parse().unwrap());
            headers.insert(header::IF_RANGE, "\"version\"".parse().unwrap());
            let slot = UrlSlot::default();
            let lookups = AtomicUsize::new(0);
            let response = fetch_stream(&reqwest::Client::new(), &slot, &headers, || async {
                lookups.fetch_add(1, Ordering::Relaxed);
                Ok(url.clone())
            })
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), status);
            let rejected = matches!(status, 401 | 403 | 410);
            let expected = if rejected { 2 } else { 1 };
            assert_eq!(requests.load(Ordering::Relaxed), expected);
            assert_eq!(lookups.load(Ordering::Relaxed), expected);
            assert_eq!(slot.lock().await.is_none(), rejected);
            server.abort();
        }
    }

    #[tokio::test]
    async fn refreshed_link_streams_successfully_and_is_reused() {
        let app = axum::Router::new()
            .route("/old", get(|| async { StatusCode::FORBIDDEN }))
            .route(
                "/new",
                get(|| async { (StatusCode::PARTIAL_CONTENT, "media bytes") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let slot = UrlSlot::default();
        download_url(&slot, None, || async { Ok(format!("{base}/old")) })
            .await
            .unwrap();
        let calls = AtomicUsize::new(0);
        let http = reqwest::Client::new();
        for _ in 0..2 {
            let response = fetch_stream(&http, &slot, &HeaderMap::new(), || async {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(format!("{base}/new"))
            })
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.text().await.unwrap(), "media bytes");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        server.abort();
    }
}
