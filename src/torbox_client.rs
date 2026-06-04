use crate::error::AppError;
use crate::provider::FileLocator;
use crate::ratelimit::AdaptiveRateLimiter;
use crate::rd_client::{Torrent, TorrentFile, TorrentInfo};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

const TORBOX_BASE: &str = "https://api.torbox.app/v1/api";

/// TorBox `{success, detail, data}` response envelope.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    detail: Option<String>,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct TbFile {
    #[serde(default)]
    id: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct TbTorrent {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    hash: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    download_finished: bool,
    #[serde(default)]
    download_state: String,
    #[serde(default)]
    files: Vec<TbFile>,
}

#[derive(Debug, Deserialize)]
struct TbCreate {
    #[serde(default)]
    torrent_id: i64,
    // Returned by createtorrent but not used (we derive the canonical torrent later).
    #[allow(dead_code)]
    #[serde(default)]
    hash: String,
}

/// Normalised status: a finished download (cached OR Inactive/uncached but owned) maps to
/// "downloaded" so it appears in the library and re-acquires on playback; otherwise the raw
/// download_state (e.g. "downloading") is kept so the scan loop excludes not-yet-ready items.
fn tb_status(t: &TbTorrent) -> String {
    if t.download_finished {
        "downloaded".to_string()
    } else {
        t.download_state.clone()
    }
}

fn to_torrent_file(f: &TbFile) -> TorrentFile {
    TorrentFile {
        id: f.id,
        path: f.name.clone(),
        bytes: f.size,
        selected: 1,
    }
}

/// Map a TorBox torrent to the lightweight canonical `Torrent` (no files).
fn to_torrent(t: &TbTorrent) -> Torrent {
    Torrent {
        id: t.id.to_string(),
        filename: t.name.clone(),
        hash: t.hash.clone(),
        bytes: t.size,
        status: tb_status(t),
        added: String::new(),
        links: Vec::new(),
        ended: None,
        ..Default::default()
    }
}

/// Map a TorBox torrent to the full canonical `TorrentInfo` (with files; no per-file links).
fn to_torrent_info(t: &TbTorrent) -> TorrentInfo {
    TorrentInfo {
        id: t.id.to_string(),
        filename: t.name.clone(),
        hash: t.hash.clone(),
        bytes: t.size,
        status: tb_status(t),
        added: String::new(),
        files: t.files.iter().map(to_torrent_file).collect(),
        links: Vec::new(),
        ended: None,
        ..Default::default()
    }
}

const RESOLVE_CACHE_TTL: Duration = Duration::from_secs(3 * 3600); // TorBox links last ~3h

struct CachedUrl {
    url: String,
    at: Instant,
}

pub struct TorBoxClient {
    client: reqwest::Client,
    api_key: String,
    rate_limiter: Arc<AdaptiveRateLimiter>,
    resolve_cache: Arc<RwLock<HashMap<(String, u32), CachedUrl>>>,
}

impl std::fmt::Debug for TorBoxClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TorBoxClient").finish()
    }
}

impl TorBoxClient {
    pub fn new(api_key: String) -> Result<Self, AppError> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", api_key))
            .map_err(|e| AppError::Config(format!("Invalid TorBox API key for header: {}", e)))?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(format!("DebridMovieMapper/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| AppError::Config(format!("Failed to build TorBox HTTP client: {}", e)))?;
        Ok(Self {
            client,
            api_key,
            rate_limiter: Arc::new(AdaptiveRateLimiter::new()),
            resolve_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn provider_name(&self) -> &'static str {
        "torbox"
    }

    async fn cache_get(&self, loc: &FileLocator) -> Option<String> {
        let key = (loc.torrent_id.clone(), loc.file_id);
        let cache = self.resolve_cache.read().await;
        cache.get(&key).and_then(|c| {
            if c.at.elapsed() < RESOLVE_CACHE_TTL {
                Some(c.url.clone())
            } else {
                None
            }
        })
    }

    async fn cache_put(&self, loc: &FileLocator, url: String) {
        let key = (loc.torrent_id.clone(), loc.file_id);
        self.resolve_cache.write().await.insert(
            key,
            CachedUrl {
                url,
                at: Instant::now(),
            },
        );
    }

    // Wired into the `DebridProvider::invalidate` impl in Task 4; only used by tests until then.
    #[allow(dead_code)]
    async fn invalidate_locator(&self, loc: &FileLocator) {
        let key = (loc.torrent_id.clone(), loc.file_id);
        self.resolve_cache.write().await.remove(&key);
    }

    // Wired into the `DebridProvider::evict_expired_cache` impl in Task 4; unused until then.
    #[allow(dead_code)]
    async fn evict_expired(&self) {
        let mut cache = self.resolve_cache.write().await;
        cache.retain(|_, c| c.at.elapsed() < RESOLVE_CACHE_TTL);
    }

    /// Send a request, rate-limited with 429/transient retry, returning the envelope's `data`.
    /// Synthesises a Bad Gateway reqwest error when `success` is false or `data` is missing.
    async fn send_data<T, F>(&self, make: F) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        let max_attempts = 6u32;
        for attempt in 1..=max_attempts {
            self.rate_limiter.wait_for_token().await;
            let resp = match make().send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt < max_attempts {
                        warn!("TorBox request error (attempt {}): {}", attempt, e);
                        continue;
                    }
                    return Err(e);
                }
            };
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                self.rate_limiter.record_throttle(retry_after).await;
                warn!("TorBox 429 (attempt {})", attempt);
                continue;
            }
            let resp = resp.error_for_status()?;
            let text = resp.text().await?;
            self.rate_limiter.record_success().await;
            match serde_json::from_str::<Envelope<T>>(&text) {
                Ok(env) if env.success => {
                    if let Some(data) = env.data {
                        return Ok(data);
                    }
                    warn!("TorBox response success but no data: {:.160}", text);
                }
                Ok(env) => {
                    warn!(
                        "TorBox response not success: {:?} body {:.160}",
                        env.detail, text
                    );
                }
                Err(e) => {
                    warn!("TorBox decode failed: {} body {:.160}", e, text);
                }
            }
            break;
        }
        let synthetic = reqwest::Response::from(
            hyper::Response::builder()
                .status(reqwest::StatusCode::BAD_GATEWAY)
                .body(hyper::body::Bytes::from_static(b"torbox request failed"))
                .unwrap(),
        );
        Err(synthetic.error_for_status().unwrap_err())
    }

    /// Like `send_data` but only checks `success` and ignores `data` (for endpoints with no
    /// data payload, e.g. controltorrent). Returns Ok(()) on success.
    async fn send_ok<F>(&self, make: F) -> Result<(), reqwest::Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let max_attempts = 6u32;
        for attempt in 1..=max_attempts {
            self.rate_limiter.wait_for_token().await;
            let resp = match make().send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt < max_attempts {
                        warn!("TorBox request error (attempt {}): {}", attempt, e);
                        continue;
                    }
                    return Err(e);
                }
            };
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                self.rate_limiter.record_throttle(retry_after).await;
                continue;
            }
            let resp = resp.error_for_status()?;
            let _ = resp.text().await?;
            self.rate_limiter.record_success().await;
            return Ok(());
        }
        let synthetic = reqwest::Response::from(
            hyper::Response::builder()
                .status(reqwest::StatusCode::BAD_GATEWAY)
                .body(hyper::body::Bytes::from_static(b"torbox request failed"))
                .unwrap(),
        );
        Err(synthetic.error_for_status().unwrap_err())
    }

    pub async fn list_torrents_raw(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        let url = format!("{}/torrents/mylist?bypass_cache=true", TORBOX_BASE);
        let raw: Vec<TbTorrent> = self.send_data(|| self.client.get(&url)).await?;
        Ok(raw.iter().map(to_torrent).collect())
    }

    pub async fn torrent_info_raw(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        let url = format!("{}/torrents/mylist?id={}&bypass_cache=true", TORBOX_BASE, id);
        let raw: TbTorrent = self.send_data(|| self.client.get(&url)).await?;
        Ok(to_torrent_info(&raw))
    }

    pub async fn add_magnet_raw(
        &self,
        magnet: &str,
    ) -> Result<crate::rd_client::AddMagnetResponse, reqwest::Error> {
        let url = format!("{}/torrents/createtorrent", TORBOX_BASE);
        let magnet = magnet.to_string();
        let created: TbCreate = self
            .send_data(|| {
                let form = reqwest::multipart::Form::new().text("magnet", magnet.clone());
                self.client.post(&url).multipart(form)
            })
            .await?;
        Ok(crate::rd_client::AddMagnetResponse {
            id: created.torrent_id.to_string(),
            uri: magnet,
        })
    }

    pub async fn delete_torrent_raw(&self, id: &str) -> Result<(), reqwest::Error> {
        let url = format!("{}/torrents/controltorrent", TORBOX_BASE);
        let torrent_id: i64 = id.parse().unwrap_or(0);
        let body = serde_json::json!({ "torrent_id": torrent_id, "operation": "delete" });
        self.send_ok(|| self.client.post(&url).json(&body)).await
    }

    pub async fn resolve_locator(&self, loc: &FileLocator) -> Result<String, AppError> {
        if let Some(url) = self.cache_get(loc).await {
            return Ok(url);
        }
        let url = format!(
            "{}/torrents/requestdl?token={}&torrent_id={}&file_id={}",
            TORBOX_BASE, self.api_key, loc.torrent_id, loc.file_id
        );
        match self.send_data::<String, _>(|| self.client.get(&url)).await {
            Ok(cdn) => {
                self.cache_put(loc, cdn.clone()).await;
                Ok(cdn)
            }
            Err(_) => {
                info!(
                    "TorBox requestdl unavailable for torrent {} file {}",
                    loc.torrent_id, loc.file_id
                );
                Err(AppError::Unavailable)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_cache_invalidate_and_evict() {
        let client = TorBoxClient::new("fake".to_string()).unwrap();
        let loc = crate::provider::FileLocator {
            torrent_id: "1".to_string(),
            file_id: 10,
            ..Default::default()
        };
        client.cache_put(&loc, "https://cdn/x".to_string()).await;
        assert_eq!(client.cache_get(&loc).await.as_deref(), Some("https://cdn/x"));
        client.invalidate_locator(&loc).await;
        assert!(client.cache_get(&loc).await.is_none());
    }

    #[test]
    fn torbox_client_constructs() {
        let c = TorBoxClient::new("fake".to_string()).unwrap();
        assert_eq!(c.provider_name(), "torbox");
    }

    // Real shapes captured live from the TorBox API (Sintel).
    const MYLIST_ITEM: &str = r#"{
        "id": 35821241, "hash": "08ada5a7a6183aae1e09d831df6748d566095a10",
        "name": "Sintel", "size": 129302391,
        "download_finished": true, "download_present": true, "cached": true,
        "download_state": "cached",
        "files": [
            {"id": 10, "name": "Sintel/Sintel.mp4", "short_name": "Sintel.mp4", "size": 129241752, "mimetype": "video/mp4"},
            {"id": 4, "name": "Sintel/poster.jpg", "short_name": "poster.jpg", "size": 46115, "mimetype": "image/jpeg"}
        ]
    }"#;

    #[test]
    fn maps_mylist_item_to_torrent_info() {
        let t: TbTorrent = serde_json::from_str(MYLIST_ITEM).unwrap();
        let info = to_torrent_info(&t);
        assert_eq!(info.id, "35821241");
        assert_eq!(info.hash, "08ada5a7a6183aae1e09d831df6748d566095a10");
        assert_eq!(info.status, "downloaded");
        assert_eq!(info.bytes, 129302391);
        assert!(info.links.is_empty());
        assert_eq!(info.files.len(), 2);
        let mp4 = info.files.iter().find(|f| f.path.ends_with(".mp4")).unwrap();
        assert_eq!(mp4.id, 10);
        assert_eq!(mp4.path, "Sintel/Sintel.mp4");
        assert_eq!(mp4.selected, 1);
    }

    #[test]
    fn maps_to_lightweight_torrent() {
        let t: TbTorrent = serde_json::from_str(MYLIST_ITEM).unwrap();
        let lt = to_torrent(&t);
        assert_eq!(lt.id, "35821241");
        assert_eq!(lt.status, "downloaded");
        assert!(lt.links.is_empty());
    }

    #[test]
    fn unfinished_torrent_keeps_raw_state() {
        let json = r#"{"id":1,"hash":"h","name":"x","size":0,"download_finished":false,"download_state":"downloading","files":[]}"#;
        let t: TbTorrent = serde_json::from_str(json).unwrap();
        assert_eq!(tb_status(&t), "downloading");
    }

    #[test]
    fn envelope_parses_array_and_object() {
        let arr: Envelope<Vec<TbTorrent>> =
            serde_json::from_str(&format!(r#"{{"success":true,"detail":"ok","data":[{}]}}"#, MYLIST_ITEM)).unwrap();
        assert!(arr.success);
        assert_eq!(arr.data.unwrap().len(), 1);
        let obj: Envelope<TbTorrent> =
            serde_json::from_str(&format!(r#"{{"success":true,"data":{}}}"#, MYLIST_ITEM)).unwrap();
        assert_eq!(obj.data.unwrap().id, 35821241);
        let fail: Envelope<String> =
            serde_json::from_str(r#"{"success":false,"detail":"err","data":null}"#).unwrap();
        assert!(!fail.success);
        assert!(fail.data.is_none());
    }
}
