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
use tracing::{debug, info, warn};

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

/// Deserialize a field that TorBox may send as JSON `null`, omit, OR send with the WRONG TYPE (e.g.
/// a stringized number, or `download_state` as a number) into the type's default, instead of failing
/// the whole response decode. TorBox's `mylist` is loose: a single torrent with e.g. `"files": null`,
/// `"size": -1` (item still resolving metadata), or a type-mismatched field must not poison the decode
/// and hide the ENTIRE library. We route through `serde_json::Value` so a type mismatch degrades to the
/// default rather than erroring the surrounding `Vec<TbTorrent>` decode.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<T>(value).unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct TbFile {
    #[serde(default, deserialize_with = "null_to_default")]
    id: u32,
    #[serde(default, deserialize_with = "null_to_default")]
    name: String,
    // Signed: TorBox reports -1 for the size of an item whose metadata has not resolved yet,
    // so a `u64` here would fail to decode the whole list.
    #[serde(default, deserialize_with = "null_to_default")]
    size: i64,
}

#[derive(Debug, Deserialize)]
struct TbTorrent {
    #[serde(default, deserialize_with = "null_to_default")]
    id: i64,
    #[serde(default, deserialize_with = "null_to_default")]
    hash: String,
    #[serde(default, deserialize_with = "null_to_default")]
    name: String,
    // Signed for the same reason as `TbFile::size`: TorBox returns -1 before metadata resolves.
    #[serde(default, deserialize_with = "null_to_default")]
    size: i64,
    #[serde(default, deserialize_with = "null_to_default")]
    download_finished: bool,
    #[serde(default, deserialize_with = "null_to_default")]
    download_state: String,
    // Download progress as a 0.0–1.0 fraction. Mapped to the canonical `Torrent.progress` (scaled to
    // a 0–100 percentage to match Real-Debrid) so the acquisition engine's stall detector sees a
    // CHANGING signal for an actively-downloading uncached torrent. Without it `progress` stayed a
    // constant 0.0, turning stall detection into a flat timer that falsely reaped + blacklisted
    // legitimately-downloading TorBox releases once `STALL_TIMEOUT_SECS` elapsed.
    #[serde(default, deserialize_with = "null_to_default")]
    progress: f64,
    // ISO-8601 creation timestamp; mapped to the canonical `added` so the VFS date tiebreak works
    // cross-provider (parse_rd_date falls back to UNIX_EPOCH for any unparseable/empty value).
    #[serde(default, deserialize_with = "null_to_default")]
    created_at: String,
    #[serde(default, deserialize_with = "null_to_default")]
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

/// Clamp a possibly-negative TorBox size (-1 = "not yet known") to a non-negative byte count.
fn clamp_size(size: i64) -> u64 {
    size.max(0) as u64
}

/// Extract the BitTorrent infohash from a `magnet:?xt=urn:btih:<hash>` URI (lowercased).
/// Used to recover the existing torrent's id when TorBox rejects a re-add as already present.
fn magnet_infohash(magnet: &str) -> Option<String> {
    let lower = magnet.to_ascii_lowercase();
    let start = lower.find("btih:")? + "btih:".len();
    let hash: String = lower[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    (hash.len() >= 32).then_some(hash)
}

fn to_torrent_file(f: &TbFile) -> TorrentFile {
    TorrentFile {
        id: f.id,
        path: f.name.clone(),
        bytes: clamp_size(f.size),
        selected: 1,
    }
}

/// Map a TorBox torrent to the lightweight canonical `Torrent` (no files).
fn to_torrent(t: &TbTorrent) -> Torrent {
    Torrent {
        id: t.id.to_string(),
        filename: t.name.clone(),
        hash: t.hash.clone(),
        bytes: clamp_size(t.size),
        status: tb_status(t),
        added: t.created_at.clone(),
        progress: t.progress * 100.0, // 0.0–1.0 fraction → 0–100 percent (RD scale) for stall detection
        ..Default::default()
    }
}

/// Map a TorBox torrent to the full canonical `TorrentInfo` (with files; no per-file links).
fn to_torrent_info(t: &TbTorrent) -> TorrentInfo {
    TorrentInfo {
        id: t.id.to_string(),
        filename: t.name.clone(),
        hash: t.hash.clone(),
        bytes: clamp_size(t.size),
        status: tb_status(t),
        added: t.created_at.clone(),
        progress: t.progress * 100.0, // see `to_torrent`
        files: t.files.iter().map(to_torrent_file).collect(),
        ..Default::default()
    }
}

const RESOLVE_CACHE_TTL: Duration = Duration::from_secs(3 * 3600); // TorBox links last ~3h
const RESOLVE_CACHE_MAX: usize = 10_000; // Bound the cache like the RD client does

/// Enforce the resolve-cache bound: drop expired entries, then, if still over `max`,
/// evict the oldest entries. Keeps the cache from growing without limit between the
/// periodic `evict_expired_cache` sweeps.
fn bound_cache(cache: &mut HashMap<(String, u32), CachedUrl>, max: usize) {
    cache.retain(|_, c| c.at.elapsed() < RESOLVE_CACHE_TTL);
    if cache.len() > max {
        let mut entries: Vec<_> = cache.iter().map(|(k, c)| (k.clone(), c.at)).collect();
        entries.sort_by_key(|(_, t)| *t);
        for (key, _) in entries.into_iter().take(cache.len() - max) {
            cache.remove(&key);
        }
    }
}

/// The standard retryable 5xx set — 500/502/503/504 — worth retrying within the bounded loop
/// (mirrors the RD client). A 500 is the server failing, which is usually transient: TorBox itself
/// returns `500 DATABASE_ERROR … "Please try again later."`. The bounded attempts + backoff ride
/// out the transient ones and give up on a deterministic one (which the next scan re-attempts). The
/// DETERMINISTIC 5xx — 501 Not Implemented / 505 HTTP Version Not Supported — are NOT retried (a
/// retry can't fix them), and 4xx are terminal (the request itself is wrong).
fn is_transient_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    )
}

/// Parse TorBox's advertised rate-limit window from response headers. TorBox sends
/// `x-ratelimit-remaining` + `x-ratelimit-reset` (a unix-epoch float) on every response,
/// letting us pace proactively. Returns `None` if either header is missing or unparseable.
fn parse_rate_headers(headers: &reqwest::header::HeaderMap) -> Option<(u64, f64)> {
    let get = |name: &str| -> Option<&str> { headers.get(name)?.to_str().ok() };
    let remaining = get("x-ratelimit-remaining")?.parse::<u64>().ok()?;
    let reset = get("x-ratelimit-reset")?.parse::<f64>().ok()?;
    Some((remaining, reset))
}

/// Build a synthetic Bad Gateway `reqwest::Error` for cases where we must return a
/// `reqwest::Error` but have no live response — exhausted retries, or unparsable
/// input we refuse to act on. Centralised so the fallible construction lives in one
/// place rather than being duplicated (with `.unwrap()`) at every call site.
fn synthetic_bad_gateway() -> reqwest::Error {
    reqwest::Response::from(
        hyper::Response::builder()
            .status(reqwest::StatusCode::BAD_GATEWAY)
            .body(hyper::body::Bytes::from_static(b"torbox request failed"))
            .expect("static BAD_GATEWAY response always builds"),
    )
    .error_for_status()
    .expect_err("BAD_GATEWAY always yields an error status")
}

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
        let mut cache = self.resolve_cache.write().await;
        cache.insert(
            key,
            CachedUrl {
                url,
                at: Instant::now(),
            },
        );
        // Only pay the O(n) sweep when actually over the bound (rare).
        if cache.len() > RESOLVE_CACHE_MAX {
            bound_cache(&mut cache, RESOLVE_CACHE_MAX);
        }
    }

    // Wired into the `DebridProvider::invalidate` impl.
    async fn invalidate_locator(&self, loc: &FileLocator) {
        let key = (loc.torrent_id.clone(), loc.file_id);
        self.resolve_cache.write().await.remove(&key);
    }

    // Wired into the `DebridProvider::evict_expired_cache` impl.
    async fn evict_expired(&self) {
        let mut cache = self.resolve_cache.write().await;
        bound_cache(&mut cache, RESOLVE_CACHE_MAX);
    }

    /// Feed TorBox's advertised rate-limit window into the limiter so it paces proactively
    /// (and never trips a 429). Called on every response — including 429s and 200s, since
    /// TorBox sends the headers on both.
    async fn observe_rate_headers(&self, resp: &reqwest::Response) {
        if let Some((remaining, reset)) = parse_rate_headers(resp.headers()) {
            self.rate_limiter.observe_rate_limit(remaining, reset).await;
        }
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
                    // Scrub the URL: it may carry the API key as a query param
                    // (`requestdl?token=...`), and reqwest errors embed the request URL.
                    let e = e.without_url();
                    if attempt < max_attempts {
                        warn!("TorBox request error (attempt {}): {}", attempt, e);
                        continue;
                    }
                    return Err(e);
                }
            };
            self.observe_rate_headers(&resp).await;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                self.rate_limiter.record_throttle(retry_after).await;
                warn!("TorBox 429 (attempt {})", attempt);
                if attempt < max_attempts {
                    continue;
                }
                // Final attempt still throttled: surface the real 429 (with the token-bearing URL
                // scrubbed) rather than the synthetic 502 below, so a sustained throttle isn't
                // misattributed to a contract failure.
                return Err(resp
                    .error_for_status_ref()
                    .err()
                    .map(|e| e.without_url())
                    .unwrap_or_else(synthetic_bad_gateway));
            }
            // Retry transient 5xx (502/503/504) with a short backoff before giving up.
            if is_transient_status(resp.status()) && attempt < max_attempts {
                warn!("TorBox {} (attempt {}), retrying", resp.status(), attempt);
                tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                continue;
            }
            let status = resp.status();
            if let Err(e) = resp.error_for_status_ref() {
                // Surface TorBox's error detail (e.g. why createtorrent 400s) before discarding it.
                let body = resp.text().await.unwrap_or_default();
                warn!("TorBox {} error body: {:.300}", status, body);
                return Err(e.without_url());
            }
            // Scrub the URL from a body-read error: the `requestdl` URL carries the token, and the
            // comment below promises send_data/send_ok never surface it. (`error_for_status_ref`
            // errors are already scrubbed above.)
            let text = resp.text().await.map_err(|e| e.without_url())?;
            match serde_json::from_str::<Envelope<T>>(&text) {
                Ok(env) if env.success => {
                    if let Some(data) = env.data {
                        // Only a genuinely useful response (success AND data) counts as success for
                        // the rate limiter — a 200 with `success:false`, or `success:true` carrying
                        // no `data`, is a soft failure we must not reward by speeding the limiter up.
                        self.rate_limiter.record_success().await;
                        return Ok(data);
                    }
                    // Redact the body: `send_data::<String>` serves `requestdl` (the resolved CDN
                    // URL is the data), so a malformed body could carry a capability URL — never log
                    // it. The structured `detail` (below) is safe; the length flags a schema change.
                    warn!(
                        "TorBox response success but no data ({} bytes, redacted)",
                        text.len()
                    );
                }
                Ok(env) => {
                    warn!(
                        "TorBox response not success: {:?} (body {} bytes, redacted)",
                        env.detail,
                        text.len()
                    );
                }
                Err(e) => {
                    warn!(
                        "TorBox decode failed: {} (body {} bytes, redacted)",
                        e,
                        text.len()
                    );
                }
            }
            break;
        }
        Err(synthetic_bad_gateway())
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
                    // Scrub the URL in case it carries the API key as a query param.
                    let e = e.without_url();
                    if attempt < max_attempts {
                        warn!("TorBox request error (attempt {}): {}", attempt, e);
                        continue;
                    }
                    return Err(e);
                }
            };
            self.observe_rate_headers(&resp).await;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                self.rate_limiter.record_throttle(retry_after).await;
                if attempt < max_attempts {
                    continue;
                }
                // Final attempt still throttled: surface the real 429, not the synthetic 502 below.
                return Err(resp
                    .error_for_status_ref()
                    .err()
                    .map(|e| e.without_url())
                    .unwrap_or_else(synthetic_bad_gateway));
            }
            if is_transient_status(resp.status()) && attempt < max_attempts {
                warn!("TorBox {} (attempt {}), retrying", resp.status(), attempt);
                tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                continue;
            }
            let status = resp.status();
            if let Err(e) = resp.error_for_status_ref() {
                let body = resp.text().await.unwrap_or_default();
                warn!("TorBox {} error body: {:.300}", status, body);
                return Err(e.without_url());
            }
            // A 200 can still carry `{"success":false}` (TorBox soft failure, e.g. controltorrent
            // rejecting a delete) — that is NOT success. Parse the envelope and require `success`,
            // so a caller's "delete the leaked torrent" invariant isn't defeated by a silent no-op.
            // Scrub the URL from a body-read error: the `requestdl` URL carries the token, and the
            // comment below promises send_data/send_ok never surface it. (`error_for_status_ref`
            // errors are already scrubbed above.)
            let text = resp.text().await.map_err(|e| e.without_url())?;
            match serde_json::from_str::<Envelope<serde_json::Value>>(&text) {
                Ok(env) if env.success => {
                    self.rate_limiter.record_success().await;
                    return Ok(());
                }
                Ok(env) => {
                    warn!(
                        "TorBox response not success: {:?} (body {} bytes, redacted)",
                        env.detail,
                        text.len()
                    );
                }
                Err(e) => {
                    warn!(
                        "TorBox decode failed: {} (body {} bytes, redacted)",
                        e,
                        text.len()
                    );
                }
            }
            break;
        }
        Err(synthetic_bad_gateway())
    }

    pub async fn list_torrents_raw(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        let url = format!("{}/torrents/mylist?bypass_cache=true", TORBOX_BASE);
        let raw: Vec<TbTorrent> = self.send_data(|| self.client.get(&url)).await?;
        Ok(raw.iter().map(to_torrent).collect())
    }

    pub async fn torrent_info_raw(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        let url = format!(
            "{}/torrents/mylist?id={}&bypass_cache=true",
            TORBOX_BASE, id
        );
        let raw: TbTorrent = self.send_data(|| self.client.get(&url)).await?;
        Ok(to_torrent_info(&raw))
    }

    pub async fn add_magnet_raw(
        &self,
        magnet: &str,
    ) -> Result<crate::rd_client::AddMagnetResponse, reqwest::Error> {
        let url = format!("{}/torrents/createtorrent", TORBOX_BASE);
        let magnet = magnet.to_string();
        let form_magnet = magnet.clone();
        match self
            .send_data::<TbCreate, _>(|| {
                let form = reqwest::multipart::Form::new().text("magnet", form_magnet.clone());
                self.client.post(&url).multipart(form)
            })
            .await
        {
            Ok(created) => Ok(crate::rd_client::AddMagnetResponse {
                id: created.torrent_id.to_string(),
                uri: magnet,
            }),
            Err(e) => {
                // TorBox 400s ("Download already queued") when the torrent is already in the
                // account. Recover idempotently by locating the existing torrent by infohash
                // rather than failing the acquisition.
                if let Some(hash) = magnet_infohash(&magnet) {
                    if let Ok(list) = self.list_torrents_raw().await {
                        if let Some(t) = list.iter().find(|t| t.hash.eq_ignore_ascii_case(&hash)) {
                            info!(
                                "TorBox createtorrent rejected but torrent already present; reusing id {}",
                                t.id
                            );
                            return Ok(crate::rd_client::AddMagnetResponse {
                                id: t.id.clone(),
                                uri: magnet,
                            });
                        }
                    }
                }
                Err(e)
            }
        }
    }

    pub async fn delete_torrent_raw(&self, id: &str) -> Result<(), reqwest::Error> {
        let url = format!("{}/torrents/controltorrent", TORBOX_BASE);
        // Refuse to act on a malformed id rather than defaulting to 0, which would
        // issue a delete against an unintended torrent and report success.
        let torrent_id: i64 = id.parse().map_err(|_| synthetic_bad_gateway())?;
        let body = serde_json::json!({ "torrent_id": torrent_id, "operation": "delete" });
        // NOTE: unlike Real-Debrid (which maps a 404 on delete to `Ok(())` for idempotency), TorBox
        // has no clean "already gone" signal here — controltorrent returns `success:false` with an
        // unstable detail string, so a delete of an already-removed torrent surfaces as `Err`. We do
        // NOT blanket-treat delete failures as success (that would hide genuine auth/network errors).
        // It is safe in practice: every caller that gates an owned-record removal on delete success
        // first filters to torrents present in the listing, and best-effort prune paths use `let _`;
        // the only residual window (torrent vanishes between listing fetch and delete) self-heals.
        self.send_ok(|| self.client.post(&url).json(&body)).await
    }

    pub async fn resolve_locator(&self, loc: &FileLocator) -> Result<String, AppError> {
        if let Some(url) = self.cache_get(loc).await {
            return Ok(url);
        }
        // TorBox's requestdl requires the token as a query param (it is not honoured
        // via the Authorization header on this endpoint). The token must therefore
        // never reach a log: send_data/send_ok scrub the URL from any reqwest error,
        // and the error below is mapped to AppError::Unavailable without logging the URL.
        let url = format!(
            "{}/torrents/requestdl?token={}&torrent_id={}&file_id={}",
            TORBOX_BASE, self.api_key, loc.torrent_id, loc.file_id
        );
        match self.send_data::<String, _>(|| self.client.get(&url)).await {
            Ok(cdn) => {
                self.cache_put(loc, cdn.clone()).await;
                Ok(cdn)
            }
            Err(e) => {
                // A 5xx / connection error / malformed response means the bytes aren't currently
                // available → signal repair (Unavailable). A clearly-permanent auth/permission
                // failure (401/403) won't be fixed by a re-add-by-hash, so surface it as Http to
                // avoid a pointless repair cycle on every read. (A 404 stays Unavailable: it can be a
                // stale torrent_id that re-add-by-hash recovers.) Log at debug — read-path, not an
                // error, and during a repair storm info! would spam.
                match e.status() {
                    Some(s)
                        if s == reqwest::StatusCode::UNAUTHORIZED
                            || s == reqwest::StatusCode::FORBIDDEN =>
                    {
                        debug!(
                            "TorBox requestdl auth/permission error ({}) for torrent {} file {}",
                            s, loc.torrent_id, loc.file_id
                        );
                        Err(AppError::Http(e))
                    }
                    _ => {
                        debug!(
                            "TorBox requestdl unavailable for torrent {} file {}",
                            loc.torrent_id, loc.file_id
                        );
                        Err(AppError::Unavailable)
                    }
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::provider::DebridProvider for TorBoxClient {
    fn name(&self) -> &'static str {
        "torbox"
    }
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        self.list_torrents_raw().await
    }
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        self.torrent_info_raw(id).await
    }
    async fn add_magnet(
        &self,
        magnet: &str,
    ) -> Result<crate::rd_client::AddMagnetResponse, reqwest::Error> {
        self.add_magnet_raw(magnet).await
    }
    async fn select_files(&self, _torrent_id: &str, _file_ids: &str) -> Result<(), reqwest::Error> {
        // TorBox auto-selects all files on createtorrent; nothing to do.
        Ok(())
    }
    async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
        self.delete_torrent_raw(torrent_id).await
    }
    async fn resolve_url(&self, loc: &FileLocator) -> Result<String, AppError> {
        self.resolve_locator(loc).await
    }
    async fn invalidate(&self, loc: &FileLocator) {
        self.invalidate_locator(loc).await
    }
    async fn evict_expired_cache(&self) {
        self.evict_expired().await
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
        assert_eq!(
            client.cache_get(&loc).await.as_deref(),
            Some("https://cdn/x")
        );
        client.invalidate_locator(&loc).await;
        assert!(client.cache_get(&loc).await.is_none());
    }

    #[test]
    fn is_transient_status_matches_retryable_5xx_only() {
        // The standard retryable 5xx set: 500/502/503/504 (a 500 is the server failing — usually
        // transient; TorBox itself returns `500 DATABASE_ERROR … "try again later"`).
        assert!(is_transient_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_transient_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(is_transient_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(is_transient_status(reqwest::StatusCode::GATEWAY_TIMEOUT));
        // The DETERMINISTIC 5xx (a retry can't fix them) and 2xx/4xx are NOT transient.
        assert!(!is_transient_status(reqwest::StatusCode::NOT_IMPLEMENTED)); // 501
        assert!(!is_transient_status(
            reqwest::StatusCode::HTTP_VERSION_NOT_SUPPORTED
        )); // 505
        assert!(!is_transient_status(reqwest::StatusCode::OK));
        assert!(!is_transient_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!is_transient_status(reqwest::StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn magnet_infohash_extracts_btih() {
        assert_eq!(
            magnet_infohash(
                "magnet:?xt=urn:btih:64877b5490208c3015c0f5121287949d62622e54&dn=Sintel"
            ),
            Some("64877b5490208c3015c0f5121287949d62622e54".to_string())
        );
        // Uppercase input is normalised to lowercase.
        assert_eq!(
            magnet_infohash("magnet:?xt=urn:btih:AABBCCDDEEFF00112233445566778899AABBCCDD"),
            Some("aabbccddeeff00112233445566778899aabbccdd".to_string())
        );
        assert_eq!(magnet_infohash("magnet:?dn=NoHash"), None);
        // A `btih:` that is PRESENT but too short (< 32 chars) is rejected by the length guard, so a
        // truncated/garbage infohash can't be used to recover an existing torrent id.
        assert_eq!(magnet_infohash("magnet:?xt=urn:btih:abc&dn=x"), None);
    }

    #[test]
    fn parse_rate_headers_reads_remaining_and_reset() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-ratelimit-remaining", "2".parse().unwrap());
        h.insert("x-ratelimit-reset", "1780870003.85".parse().unwrap());
        let (remaining, reset) = parse_rate_headers(&h).expect("should parse window");
        assert_eq!(remaining, 2);
        assert!((reset - 1780870003.85).abs() < 0.001);
    }

    #[test]
    fn parse_rate_headers_none_when_missing_or_bad() {
        assert_eq!(parse_rate_headers(&reqwest::header::HeaderMap::new()), None);
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("x-ratelimit-remaining", "lots".parse().unwrap());
        h.insert("x-ratelimit-reset", "soon".parse().unwrap());
        assert_eq!(parse_rate_headers(&h), None);
    }

    #[test]
    fn bound_cache_evicts_oldest_over_capacity() {
        let mut cache: HashMap<(String, u32), CachedUrl> = HashMap::new();
        // Insert 5 entries with increasing age (i seconds old); all within TTL.
        for i in 0..5u32 {
            cache.insert(
                (i.to_string(), i),
                CachedUrl {
                    url: format!("u{}", i),
                    at: Instant::now() - Duration::from_secs(i as u64),
                },
            );
        }
        bound_cache(&mut cache, 3);
        assert_eq!(cache.len(), 3);
        // The three newest (ages 0,1,2 → keys "0","1","2") must remain.
        assert!(cache.contains_key(&("0".to_string(), 0)));
        assert!(cache.contains_key(&("1".to_string(), 1)));
        assert!(cache.contains_key(&("2".to_string(), 2)));
    }

    #[test]
    fn bound_cache_drops_expired_entries() {
        let mut cache: HashMap<(String, u32), CachedUrl> = HashMap::new();
        cache.insert(
            ("fresh".to_string(), 1),
            CachedUrl {
                url: "u".to_string(),
                at: Instant::now(),
            },
        );
        cache.insert(
            ("stale".to_string(), 2),
            CachedUrl {
                url: "u".to_string(),
                at: Instant::now() - RESOLVE_CACHE_TTL - Duration::from_secs(1),
            },
        );
        bound_cache(&mut cache, RESOLVE_CACHE_MAX);
        assert!(cache.contains_key(&("fresh".to_string(), 1)));
        assert!(!cache.contains_key(&("stale".to_string(), 2)));
    }

    #[test]
    fn torbox_client_constructs() {
        use crate::provider::DebridProvider;
        let c = TorBoxClient::new("fake".to_string()).unwrap();
        assert_eq!(c.name(), "torbox");
    }

    #[test]
    fn torbox_client_is_a_debrid_provider() {
        use crate::provider::DebridProvider;
        let c = TorBoxClient::new("fake".to_string()).unwrap();
        let p: std::sync::Arc<dyn DebridProvider> = std::sync::Arc::new(c);
        assert_eq!(p.name(), "torbox");
    }

    /// Spawn a local HTTP server that replies once with `200 OK` and the given JSON body.
    async fn spawn_json_200(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{}/", addr)
    }

    #[tokio::test]
    async fn send_ok_rejects_200_with_success_false() {
        // TorBox returns HTTP 200 with `{"success":false}` for soft failures (e.g. controltorrent
        // rejecting a delete). That must surface as an error, not a silent Ok — otherwise the
        // acquisition engine believes a leaked torrent was deleted when it wasn't.
        let url = spawn_json_200(r#"{"success":false,"detail":"cannot delete"}"#).await;
        let client = TorBoxClient::new("fake".to_string()).unwrap();
        let body = serde_json::json!({"torrent_id": 1, "operation": "delete"});
        let r = client
            .send_ok(|| client.client.post(&url).json(&body))
            .await;
        assert!(r.is_err(), "a 200 with success:false must be an error");
    }

    #[tokio::test]
    async fn send_ok_accepts_200_with_success_true() {
        let url = spawn_json_200(r#"{"success":true,"detail":"deleted"}"#).await;
        let client = TorBoxClient::new("fake".to_string()).unwrap();
        let body = serde_json::json!({"torrent_id": 1, "operation": "delete"});
        let r = client
            .send_ok(|| client.client.post(&url).json(&body))
            .await;
        assert!(r.is_ok(), "a 200 with success:true must be Ok");
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
        let mp4 = info
            .files
            .iter()
            .find(|f| f.path.ends_with(".mp4"))
            .unwrap();
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
    fn maps_download_progress_so_stall_detection_sees_change() {
        // An actively-downloading uncached torrent must carry a CHANGING `progress` (TorBox sends a
        // 0.0–1.0 fraction → mapped to 0–100 percent). Without this it stayed 0.0 and the engine's
        // stall detector — which only resets on a progress delta — falsely reaped the download.
        let json = r#"{
            "id": 7, "hash": "h", "name": "Big.Movie.2024.2160p", "size": 50000000000,
            "download_finished": false, "download_state": "downloading", "progress": 0.42,
            "files": [{"id": 0, "name": "Big.Movie.2024.2160p.mkv", "size": 50000000000}]
        }"#;
        let t: TbTorrent = serde_json::from_str(json).expect("downloading item must decode");
        assert_eq!(to_torrent(&t).status, "downloading");
        assert!((to_torrent(&t).progress - 42.0).abs() < 1e-9);
        assert!((to_torrent_info(&t).progress - 42.0).abs() < 1e-9);
        // A missing/null progress still decodes (defensive) and defaults to 0.0.
        let no_prog = r#"{"id": 8, "hash": "h2", "name": "X", "size": 1,
            "download_finished": false, "download_state": "downloading", "files": []}"#;
        let t2: TbTorrent = serde_json::from_str(no_prog).expect("missing progress must decode");
        assert_eq!(to_torrent(&t2).progress, 0.0);
    }

    #[test]
    fn negative_size_decodes_and_clamps_to_zero() {
        // TorBox returns size -1 for items whose metadata has not resolved yet. A strict
        // u64 would fail to decode the entire mylist response, hiding the whole library.
        let json = r#"{
            "id": 1, "hash": "h", "name": "Pending", "size": -1,
            "download_finished": false, "download_state": "downloading",
            "files": [{"id": 0, "name": "Pending/file.mkv", "size": -1}]
        }"#;
        let t: TbTorrent = serde_json::from_str(json).expect("size -1 must decode");
        let info = to_torrent_info(&t);
        assert_eq!(info.bytes, 0);
        assert_eq!(info.files.len(), 1);
        assert_eq!(info.files[0].bytes, 0);
        // A mixed list (one valid, one pending) must decode as a whole.
        let arr: Vec<TbTorrent> = serde_json::from_str(&format!("[{},{}]", MYLIST_ITEM, json))
            .expect("a list containing a -1 size must still decode");
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn null_fields_decode_to_defaults() {
        // TorBox's full mylist can send `files: null` (and other null fields) per torrent.
        // `#[serde(default)]` alone does NOT cover an explicit null, so this would otherwise
        // fail with "invalid type: null, expected a sequence" and hide the whole library.
        let json = r#"{
            "id": 35928498, "hash": "abc", "name": "Sintel", "size": 100,
            "download_finished": true, "download_state": "cached", "files": null
        }"#;
        let t: TbTorrent = serde_json::from_str(json).expect("files: null must decode");
        let info = to_torrent_info(&t);
        assert_eq!(info.id, "35928498");
        assert_eq!(info.status, "downloaded");
        assert!(info.files.is_empty());
        // A whole list where one entry has null files must still decode.
        let arr: Vec<TbTorrent> = serde_json::from_str(&format!("[{},{}]", MYLIST_ITEM, json))
            .expect("a list containing null files must still decode");
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn type_mismatched_fields_decode_to_defaults_not_failing_the_list() {
        // TorBox occasionally sends a field with the WRONG type (e.g. a stringized id, or
        // download_state as a number). That must degrade to the default for that field, NOT fail the
        // whole list decode (which would hide the entire library for the tick).
        let bad = r#"{
            "id": "35928498", "hash": 123, "name": "Sintel", "size": "100",
            "download_finished": "yes", "download_state": 7, "progress": "0.5", "files": "nope"
        }"#;
        let t: TbTorrent =
            serde_json::from_str(bad).expect("type-mismatched fields must decode to defaults");
        let info = to_torrent_info(&t);
        // Each mismatched field fell back to its default rather than erroring.
        assert_eq!(info.id, "0"); // stringized id → default 0
        assert_eq!(info.filename, "Sintel"); // a correctly-typed field is still parsed
        assert!(info.files.is_empty()); // "files": "nope" → default empty
                                        // A whole list where one entry is type-mismatched must still decode the good entries.
        let arr: Vec<TbTorrent> = serde_json::from_str(&format!("[{},{}]", MYLIST_ITEM, bad))
            .expect("one type-mismatched entry must not fail the whole list");
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn unfinished_torrent_keeps_raw_state() {
        let json = r#"{"id":1,"hash":"h","name":"x","size":0,"download_finished":false,"download_state":"downloading","files":[]}"#;
        let t: TbTorrent = serde_json::from_str(json).unwrap();
        assert_eq!(tb_status(&t), "downloading");
    }

    #[test]
    fn envelope_parses_array_and_object() {
        let arr: Envelope<Vec<TbTorrent>> = serde_json::from_str(&format!(
            r#"{{"success":true,"detail":"ok","data":[{}]}}"#,
            MYLIST_ITEM
        ))
        .unwrap();
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
