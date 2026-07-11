use crate::ratelimit::{AdaptiveRateLimiter, MAX_RETRY_AFTER_SECS};
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

const MAX_CACHE_SIZE: usize = 10_000;
const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour

use crate::provider::synthetic_bad_gateway;

/// Whether an `unrestrict/link` failure is a genuinely PERMANENT client error (a bad token) that
/// instant repair can't fix — only 401/403. Everything else (a 5xx, a transport-level failure, or a
/// stale/expired-link 404 that a re-add-by-hash recovers) maps to `AppError::Unavailable` → repair.
/// Mirrors TorBox's `requestdl_status_is_permanent` so the two providers behave symmetrically.
fn unrestrict_status_is_permanent(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN
}

#[derive(Clone, Default, Deserialize)]
pub struct Torrent {
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub split: u64,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub added: String,
    #[serde(default)]
    pub links: Vec<String>,
    pub ended: Option<String>,
}

// Hand-written Debug: `links` are RD restricted capability links (secrets) — print only their
// count, mirroring `FileLocator`'s redacted Debug, so a future `{:?}` can't leak them.
impl std::fmt::Debug for Torrent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Torrent")
            .field("id", &self.id)
            .field("filename", &self.filename)
            .field("hash", &self.hash)
            .field("bytes", &self.bytes)
            .field("host", &self.host)
            .field("split", &self.split)
            .field("progress", &self.progress)
            .field("status", &self.status)
            .field("added", &self.added)
            .field("links", &format_args!("<{} link(s)>", self.links.len()))
            .field("ended", &self.ended)
            .finish()
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct TorrentInfo {
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub original_filename: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub original_bytes: u64,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub split: u64,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub added: String,
    #[serde(default)]
    pub files: Vec<TorrentFile>,
    // RD restricted capability links. NOTE: `Serialize` persists these to the `matches` cache in
    // `metadata.db` (the VFS reads `t.links` to build FileLocators, so they can't be `serde(skip)`ped
    // without forcing a re-resolution). The redacted `Debug` below only guards LOGS; the on-disk
    // copy is deliberate. These are *restricted* links (not the directly-fetchable signed CDN URL,
    // which is only ever held in the in-memory unrestrict cache), and unrestricting one still
    // requires the account token — so a reader of the DB file gains nothing without the token.
    #[serde(default)]
    pub links: Vec<String>,
    pub ended: Option<String>,
}

// Hand-written Debug: `links` are RD restricted capability links (secrets) — print only their
// count, mirroring `FileLocator`'s redacted Debug, so a future `{:?}` can't leak them.
impl std::fmt::Debug for TorrentInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TorrentInfo")
            .field("id", &self.id)
            .field("filename", &self.filename)
            .field("original_filename", &self.original_filename)
            .field("hash", &self.hash)
            .field("bytes", &self.bytes)
            .field("original_bytes", &self.original_bytes)
            .field("host", &self.host)
            .field("split", &self.split)
            .field("progress", &self.progress)
            .field("status", &self.status)
            .field("added", &self.added)
            .field("files", &self.files)
            .field("links", &format_args!("<{} link(s)>", self.links.len()))
            .field("ended", &self.ended)
            .finish()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TorrentFile {
    #[serde(default)]
    pub id: u32,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub selected: u32,
}

#[derive(Clone, Default, Deserialize)]
pub struct UnrestrictResponse {
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub filesize: u64,
    pub link: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub chunks: u32,
    #[serde(default)]
    pub crc: u32,
    pub download: String,
    #[serde(default)]
    pub streamable: u32,
}

// Hand-written Debug: `link` (RD restricted capability link) and `download` (signed CDN URL) are
// secrets — print presence only, mirroring `FileLocator`'s redacted Debug. `CachedUnrestrictResponse`
// derives Debug and embeds this, so it inherits the redaction.
impl std::fmt::Debug for UnrestrictResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |s: &str| {
            if s.is_empty() {
                "<none>"
            } else {
                "<redacted>"
            }
        };
        f.debug_struct("UnrestrictResponse")
            .field("id", &self.id)
            .field("filename", &self.filename)
            .field("mime_type", &self.mime_type)
            .field("filesize", &self.filesize)
            .field("link", &redact(&self.link))
            .field("host", &self.host)
            .field("chunks", &self.chunks)
            .field("crc", &self.crc)
            .field("download", &redact(&self.download))
            .field("streamable", &self.streamable)
            .finish()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AddMagnetResponse {
    pub id: String,
    pub uri: String,
}

#[derive(Debug, Clone)]
struct CachedUnrestrictResponse {
    response: UnrestrictResponse,
    cached_at: std::time::Instant,
}

pub struct RealDebridClient {
    client: reqwest::Client,
    unrestrict_cache: Arc<RwLock<HashMap<String, CachedUnrestrictResponse>>>,
    rate_limiter: Arc<AdaptiveRateLimiter>,
    /// Base URL for the RD REST API (everything up to, but not including, the leading `/` of an
    /// endpoint path, e.g. `https://api.real-debrid.com/rest/1.0`). Overridable in tests so the
    /// higher-level methods can be exercised against a loopback server.
    base_url: String,
}

// `DebridProvider` requires `Debug`, but `unrestrict_cache` holds restricted RD `link`s and signed
// CDN `download` URLs (capability URLs) — an auto-derived Debug would let a stray `{:?}` leak them.
// So redact like `TorBoxClient` (which protects its `api_key` the same way): print the type name
// only, no fields.
impl std::fmt::Debug for RealDebridClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealDebridClient").finish()
    }
}

impl RealDebridClient {
    pub fn new(api_token: String) -> Result<Self, crate::error::AppError> {
        let mut headers = HeaderMap::new();
        let auth_val = format!("Bearer {}", api_token);
        let mut auth_header = HeaderValue::from_str(&auth_val).map_err(|e| {
            crate::error::AppError::Config(format!("Invalid API token for HTTP header: {}", e))
        })?;
        auth_header.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth_header);
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(format!("DebridMovieMapper/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            // Bound the TCP/TLS connect phase separately so a black-holing endpoint fails the
            // connect in 10s rather than waiting out the full 60s request timeout — important on the
            // synchronous playback resolve path, where (with the 3-attempt budget) a dead connect now
            // fails over to repair in ~30s instead of ~3 minutes. Healthy connects are sub-second.
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| {
                crate::error::AppError::Config(format!("Failed to build HTTP client: {}", e))
            })?;

        Ok(Self {
            client,
            unrestrict_cache: Arc::new(RwLock::new(HashMap::new())),
            rate_limiter: Arc::new(AdaptiveRateLimiter::new()),
            base_url: "https://api.real-debrid.com/rest/1.0".to_string(),
        })
    }

    /// Test-only: point the client at a loopback base URL (e.g. `http://127.0.0.1:PORT`) so the
    /// higher-level endpoint methods can be exercised without hitting the live RD API.
    #[cfg(test)]
    fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    /// Helper to handle 503 and other non-429 retryable status codes
    async fn wait_for_retry(
        status: reqwest::StatusCode,
        headers: &HeaderMap,
        attempt: u32,
        max_attempts: u32,
    ) {
        // On the FINAL attempt there is no point sleeping — the caller gives up immediately after.
        // (Without this guard a Retry-After 5xx on the last attempt would sleep up to
        // MAX_RETRY_AFTER_SECS only to then return an error.)
        if attempt >= max_attempts {
            return;
        }

        let retry_after = headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        if let Some(seconds) = retry_after {
            let capped = std::cmp::min(seconds, MAX_RETRY_AFTER_SECS);
            warn!(
                "RD API returned {} (attempt {}/{}). Respecting Retry-After: {}s (raw: {}s)",
                status, attempt, max_attempts, capped, seconds
            );
            tokio::time::sleep(Duration::from_secs(capped)).await;
        } else {
            // Extended exponential backoff for 503/502/504, capped at 30s
            let backoff_secs = 2u64.saturating_pow(attempt);
            let delay = Duration::from_secs(std::cmp::min(backoff_secs, 30));
            let jitter = Duration::from_millis(rand::rng().random_range(0..1000));
            let total_delay = delay + jitter;
            warn!(
                "RD API {} (attempt {}/{}). Using extended backoff: {}ms",
                status,
                attempt,
                max_attempts,
                total_delay.as_millis()
            );
            tokio::time::sleep(total_delay).await;
        }
    }

    /// Whether a status should trigger a retry: 429 plus the standard retryable 5xx set
    /// (500/502/503/504). A 500 is a server-side failure, usually transient, so it joins the
    /// bounded-retry set; 4xx (and the deterministic 5xx 501/505) are terminal — a retry can't fix
    /// the request. Callers can still override via `terminal_statuses` in `fetch_with_retry`, which
    /// are checked first and abort without retrying (e.g. the full 5xx set — 500/502/503/504 — for
    /// `unrestrict_link`, so the on-read playback resolve fails fast to repair).
    fn should_retry_status(status: reqwest::StatusCode) -> bool {
        status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || crate::provider::is_retryable_5xx(status)
    }

    pub async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        // RD's /torrents default page size is 100; request it explicitly (was 50) to halve the
        // per-scan round-trips + rate-limiter pressure on large libraries. (RD sends no rate-limit
        // headers, so fewer requests directly means a faster scan tick.)
        const PAGE_LIMIT: u32 = 100;
        // Runaway guard. At 100/page this allows ~200k torrents — far beyond any realistic account —
        // so legitimate large libraries paginate to completion. Hitting it means a pathological /
        // runaway listing: we must FAIL (below), never return a truncated `Ok`, because downstream
        // treats an owned title absent from the listing as lapsed and re-acquires it (the same reason
        // the per-page `Err` branch fails the whole listing). A truncated listing is worse than none.
        const MAX_PAGES: u32 = 2000;
        let mut all_torrents = Vec::new();
        let mut page = 1u32;
        loop {
            // Allow ONE page beyond the cap as an empty-terminator probe. An account holding EXACTLY
            // MAX_PAGES*PAGE_LIMIT items has a FULL final page at MAX_PAGES, so `page` advances to
            // MAX_PAGES+1 with the listing actually complete — completion is only proven by the next
            // page coming back empty (the loop's natural break). So the probe page MAX_PAGES+1 is
            // allowed to run; the error below fires only if THAT probe page is itself full, i.e. when
            // `page` has advanced to MAX_PAGES+2 (the guard is `page > MAX_PAGES + 1`). This treats
            // the exact-multiple boundary as complete instead of a false truncation (which would look
            // like a shrunken library → re-acquire storm). A genuinely-truncated account has a FULL
            // page at MAX_PAGES+1, advancing to MAX_PAGES+2 → error, as intended.
            if page > MAX_PAGES + 1 {
                warn!(
                    "Reached maximum page limit ({}) with {} torrents; failing the whole listing \
                     rather than returning a truncated list (which would churn the dropped pages)",
                    MAX_PAGES,
                    all_torrents.len()
                );
                return Err(synthetic_bad_gateway(b"get_torrents exceeded MAX_PAGES"));
            }
            debug!("Fetching torrents page {}...", page);
            let url = format!(
                "{}/torrents?page={}&limit={}",
                self.base_url, page, PAGE_LIMIT
            );
            let res: Result<Vec<Torrent>, reqwest::Error> =
                self.fetch_with_retry(|| self.client.get(&url), &[]).await;

            match res {
                Ok(torrents) => {
                    if torrents.is_empty() {
                        break;
                    }
                    all_torrents.extend(torrents);
                    page += 1;
                }
                Err(e) => {
                    // Returning the partial list here is unsafe: downstream treats a wanted, owned
                    // title that is absent from the listing as lapsed and re-acquires it, so the
                    // torrents on the dropped pages would churn. A truncated listing is worse than
                    // no listing — surface the error so callers skip this tick (their get_torrents
                    // guards early-return on Err) instead of acting on incomplete data.
                    if !all_torrents.is_empty() {
                        warn!(
                            "Failed to fetch torrents page {} after gathering {} torrents; failing \
                             the whole listing to avoid treating dropped pages as removals: {}",
                            page,
                            all_torrents.len(),
                            e
                        );
                    }
                    return Err(e);
                }
            }
        }
        debug!("Fetched {} torrents in total.", all_torrents.len());
        Ok(all_torrents)
    }

    pub async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        let url = format!("{}/torrents/info/{}", self.base_url, id);
        self.fetch_with_retry(|| self.client.get(&url), &[reqwest::StatusCode::NOT_FOUND])
            .await
    }

    /// Unrestrict a link, caching the result for 1 hour.
    ///
    /// NOTE: The cache check-then-act is not atomic. Two concurrent calls
    /// for the same link may both miss the cache and call the API. This is
    /// intentional: per-key locking would add complexity with negligible
    /// benefit since duplicate calls are idempotent and rare in practice.
    pub async fn unrestrict_link(&self, link: &str) -> Result<UnrestrictResponse, reqwest::Error> {
        // Check cache first
        {
            let cache = self.unrestrict_cache.read().await;
            if let Some(cached) = cache.get(link) {
                if cached.cached_at.elapsed() < CACHE_TTL {
                    // `debug`, not `info`: the restricted link is a sensitive capability URL — it
                    // must not be emitted on the normal (info) playback path on every cache hit.
                    debug!("Using cached unrestrict response (link redacted)");
                    return Ok(cached.response.clone());
                }
            }
        }

        // Not in cache or expired, fetch from API.
        // Any 5xx on unrestrict is terminal (no retries): this is the synchronous on-read playback
        // resolve path, so a persistent 500/502/504 must NOT run the full ~10-attempt exponential
        // backoff (~3 min) inside the WebDAV read — that surfaces to the player as a hang. A 5xx here
        // means the file isn't servable right now; `resolve_url` maps it to `AppError::Unavailable` so
        // `dav_fs` fast-fails to instant repair (re-add by hash; a cached replacement fixes playback
        // inline, bounded by the repair cooldown) instead of stalling. (503 was already terminal —
        // 500/502/504 now join it for the same reason.)
        let url = format!("{}/unrestrict/link", self.base_url);
        // Small retry budget (3, vs the default 10): this is the synchronous playback path, so a
        // transport-level hang (timeout/connect) must not retry ~10×60s. After this budget a
        // persistent failure surfaces to `resolve_url`, which maps it to `AppError::Unavailable`
        // → instant repair, rather than stalling the WebDAV read.
        let response: UnrestrictResponse = self
            .fetch_with_retry_n(
                || self.client.post(&url).form(&[("link", link)]),
                &[
                    reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    reqwest::StatusCode::BAD_GATEWAY,
                    reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    reqwest::StatusCode::GATEWAY_TIMEOUT,
                ],
                3,
            )
            .await?;

        // Store in cache
        {
            let mut cache = self.unrestrict_cache.write().await;
            cache.insert(
                link.to_string(),
                CachedUnrestrictResponse {
                    response: response.clone(),
                    cached_at: std::time::Instant::now(),
                },
            );
        }

        // Evict if cache is too large
        {
            let cache = self.unrestrict_cache.read().await;
            if cache.len() > MAX_CACHE_SIZE {
                drop(cache);
                self.evict_expired_cache().await;
            }
        }

        Ok(response)
    }

    /// Add a magnet link to Real-Debrid
    pub async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        let url = format!("{}/torrents/addMagnet", self.base_url);
        self.fetch_with_retry(|| self.client.post(&url).form(&[("magnet", magnet)]), &[])
            .await
    }

    /// Select files for a torrent
    pub async fn select_files(
        &self,
        torrent_id: &str,
        file_ids: &str,
    ) -> Result<(), reqwest::Error> {
        let url = format!("{}/torrents/selectFiles/{}", self.base_url, torrent_id);
        // RD returns 204 No Content on success. We deserialize as
        // serde_json::Value which accepts the "[]" empty-body fallback
        // in fetch_with_retry.
        let _: serde_json::Value = self
            .fetch_with_retry(|| self.client.post(&url).form(&[("files", file_ids)]), &[])
            .await?;
        Ok(())
    }

    /// Delete a torrent from Real-Debrid
    /// Returns Ok(()) even if torrent doesn't exist (404), as the end state is the same
    pub async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
        let url = format!("{}/torrents/delete/{}", self.base_url, torrent_id);
        let result: Result<serde_json::Value, _> = self
            .fetch_with_retry(
                || self.client.delete(&url),
                &[reqwest::StatusCode::NOT_FOUND],
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(reqwest::StatusCode::NOT_FOUND) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn fetch_with_retry<T, F>(
        &self,
        make_request: F,
        terminal_statuses: &[reqwest::StatusCode],
    ) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        // Default budget for background API calls (listing, add/select/delete). The synchronous
        // on-read playback resolve (`unrestrict_link`) uses a smaller budget via `fetch_with_retry_n`
        // so a hanging/black-holing RD API can't stall a single WebDAV read for the full
        // ~10×60s-timeout window (which surfaces to the player as a multi-minute hang).
        self.fetch_with_retry_n(make_request, terminal_statuses, 10)
            .await
    }

    async fn fetch_with_retry_n<T, F>(
        &self,
        make_request: F,
        terminal_statuses: &[reqwest::StatusCode],
        max_attempts: u32,
    ) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut last_error: Option<reqwest::Error> = None;
        let mut deserialization_failures = 0u32;

        for attempt in 1..=max_attempts {
            self.rate_limiter.wait_for_token().await;

            match make_request().send().await {
                Ok(resp) => {
                    let status = resp.status();

                    if terminal_statuses.contains(&status) {
                        // A 404 terminal is an expected, benign outcome (e.g. deleting an
                        // already-absent torrent, which `delete_torrent` maps to Ok) — log it at
                        // debug so routine idempotent prune/dedup/repair deletes don't spam warnings
                        // (which would also drown genuine terminal-5xx warnings). Other terminal
                        // statuses (5xx, etc.) stay at warn.
                        if status == reqwest::StatusCode::NOT_FOUND {
                            debug!(
                                "RD API returned terminal status {} — not retrying (attempt {}/{})",
                                status, attempt, max_attempts
                            );
                        } else {
                            warn!(
                                "RD API returned terminal status {} — not retrying (attempt {}/{})",
                                status, attempt, max_attempts
                            );
                        }
                        return Err(match resp.error_for_status() {
                            Err(e) => e,
                            // Defensive: a non-error status was configured terminal (no current
                            // caller does this). Surface a synthetic error rather than panicking.
                            Ok(_) => synthetic_bad_gateway(b"terminal non-error status"),
                        });
                    }

                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let retry_after = resp
                            .headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());
                        self.rate_limiter.record_throttle(retry_after).await;
                        warn!(
                            "RD API returned 429 (attempt {}/{}). Adaptive limiter adjusted.",
                            attempt, max_attempts
                        );
                        // Preserve the real 429 so that, if EVERY attempt is throttled, the surfaced
                        // error reflects throttling rather than the synthetic "deserialization
                        // failures" fallback below (which would otherwise misattribute the cause).
                        last_error = resp.error_for_status().err();
                        continue;
                    }

                    if Self::should_retry_status(status) {
                        Self::wait_for_retry(status, resp.headers(), attempt, max_attempts).await;
                        // Record the real status error so that, if this was the FINAL attempt, the
                        // loop surfaces the actual persistent 5xx (e.g. 503) instead of a synthetic
                        // 502 — preserving the diagnostic status and matching torbox_client.
                        last_error = resp.error_for_status().err();
                        continue;
                    }

                    match resp.error_for_status() {
                        Ok(resp) => {
                            let text = resp.text().await?;
                            // 204 No Content: try deserializing from "[]". This succeeds for Vec<T>
                            // and serde_json::Value, allowing callers like select_files/delete_torrent
                            // (which get a 204) to work transparently.
                            //
                            // NOTE: this is gated on the 204 status, NOT on an empty body — an empty
                            // body under a *200* must NOT decode to `[]`. `get_torrents` reads an
                            // empty `Vec<Torrent>` as end-of-list, so an anomalous empty-body 200
                            // mid-pagination (e.g. a proxy/CDN quirk) would silently truncate the
                            // listing, and downstream treats an owned title absent from the listing
                            // as lapsed → re-acquire. Letting an empty-body 200 fall through to the
                            // decode below (which fails → deserialization cap → surfaced error) makes
                            // `get_torrents` fail the whole listing instead — the safe outcome.
                            if status == reqwest::StatusCode::NO_CONTENT {
                                if let Ok(val) = serde_json::from_str::<T>("[]") {
                                    self.rate_limiter.record_success().await;
                                    return Ok(val);
                                }
                                // A 204 whose body can't be decoded into T (i.e. T is not
                                // Vec/Value) is a contract violation that won't fix itself on
                                // retry — count it toward the deserialization cap so a
                                // permanently-empty endpoint doesn't burn all 10 attempts.
                                deserialization_failures += 1;
                                warn!(
                                    "RD API 204 not decodable into the expected type (attempt {}/{}). Status: {}",
                                    attempt, max_attempts, status
                                );
                                if deserialization_failures >= 2 {
                                    error!("Aborting after {} empty/undecodable RD responses — likely a contract change", deserialization_failures);
                                    // The undecodable response is the terminating cause; drop any
                                    // earlier 429/5xx in `last_error` so the surfaced error is the
                                    // synthetic decode-failure below, not a stale prior status.
                                    last_error = None;
                                    break;
                                }
                                continue;
                            }
                            match serde_json::from_str::<T>(&text) {
                                Ok(val) => {
                                    self.rate_limiter.record_success().await;
                                    return Ok(val);
                                }
                                Err(e) => {
                                    deserialization_failures += 1;
                                    // Do NOT log the raw body: this shared helper serves
                                    // `unrestrict_link`, whose `UnrestrictResponse` carries the
                                    // restricted `link` + signed `download` CDN URL as required
                                    // fields — a partial/changed 200 body could otherwise emit a
                                    // capability URL, breaking the "never log the link" invariant. The
                                    // serde error names the failing field/offset; the length is enough
                                    // to spot a schema change.
                                    error!(
                                        "Failed to decode RD response: {}. Status: {}, body {} bytes (redacted)",
                                        e, status, text.len()
                                    );
                                    // Retry ONE transient decode blip, then bail — a *persistent*
                                    // decode failure is a schema change that won't fix itself on retry.
                                    // This cap (2 attempts) is DELIBERATELY tighter than TorBox's
                                    // full-budget decode-retry (`send_data_n`): `unrestrict_link` runs
                                    // on the synchronous playback resolve path with a small 3-attempt
                                    // budget, so it must fast-fail to instant repair rather than burn
                                    // the whole budget on a possibly-permanent format change.
                                    if deserialization_failures >= 2 {
                                        error!("Aborting after {} deserialization failures on HTTP 200 — likely a schema change", deserialization_failures);
                                        // The decode failure is the terminating cause; drop any
                                        // earlier 429/5xx in `last_error` so the surfaced error is the
                                        // synthetic decode-failure below (accurate cause), not a stale
                                        // prior status recorded on an earlier attempt.
                                        last_error = None;
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            // terminal_statuses, 429, and the retryable 5xx (500/502/503/504, via
                            // should_retry_status) are all handled above. Any remaining error status
                            // is a terminal client error (4xx — 400/401/403/…) or a deterministic
                            // 5xx (501/505) that will not fix itself on retry — return immediately
                            // rather than burning all 10 rate-limited attempts (and 10 warnings) on,
                            // e.g., a bad token.
                            warn!(
                                "RD API non-retryable error (attempt {}/{}): {}. Status: {} — not retrying",
                                attempt, max_attempts, e, status
                            );
                            return Err(e);
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "RD API request failed (attempt {}/{}): {}",
                        attempt, max_attempts, e
                    );
                    last_error = Some(e);
                    // Small backoff before retrying a statusless transport error (connection
                    // refused/reset, timeout). Without it a fast-failing endpoint (e.g. connection
                    // refused) spins the whole budget in a tight warn loop — the rate limiter's 100ms
                    // baseline spacing isn't enough. Kept SMALL (100ms·attempt, capped 1s) rather than
                    // the 5xx exponential backoff (up to 30s) so the latency-sensitive
                    // `unrestrict_link` resolve path still fails over to instant repair quickly.
                    // Skipped on the final attempt (nothing follows it).
                    if attempt < max_attempts {
                        let backoff = Duration::from_millis((100 * attempt as u64).min(1_000));
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }

        if let Some(e) = last_error {
            Err(e)
        } else {
            // All attempts exhausted without a reqwest::Error being recorded.
            // This happens when every attempt returned HTTP 200 but the response
            // body failed to deserialize (e.g. unexpected JSON schema). Build a
            // synthetic error response to surface a proper error instead of panicking.
            error!(
                "fetch_with_retry: all {} attempts exhausted due to deserialization failures",
                max_attempts
            );
            Err(synthetic_bad_gateway(
                b"all attempts exhausted: deserialization failures",
            ))
        }
    }

    /// Evict expired entries from the unrestrict cache, and if still over
    /// MAX_CACHE_SIZE, remove the oldest entries. Internal-only: the provider-neutral surface is the
    /// `DebridProvider` trait; the concrete cache is not part of the public API.
    async fn evict_expired_cache(&self) {
        let mut cache = self.unrestrict_cache.write().await;
        cache.retain(|_, v| v.cached_at.elapsed() < CACHE_TTL);
        // If still over max size, remove oldest entries
        if cache.len() > MAX_CACHE_SIZE {
            let mut entries: Vec<_> = cache
                .iter()
                .map(|(k, v)| (k.clone(), v.cached_at))
                .collect();
            entries.sort_by_key(|(_, t)| *t);
            let to_remove = cache.len() - MAX_CACHE_SIZE;
            for (key, _) in entries.into_iter().take(to_remove) {
                cache.remove(&key);
            }
        }
    }

    /// Remove a specific link from the unrestrict cache.
    /// Called after instant repair replaces a broken torrent so that stale
    /// cached responses for the old link do not trigger a second repair. Internal-only: reached via
    /// the `DebridProvider::invalidate` impl, not as public inherent API.
    async fn invalidate_unrestrict_cache(&self, link: &str) {
        let mut cache = self.unrestrict_cache.write().await;
        cache.remove(link);
    }
}

#[async_trait::async_trait]
impl crate::provider::DebridProvider for RealDebridClient {
    fn name(&self) -> &'static str {
        "real-debrid"
    }

    // Each call uses method-call syntax on `&RealDebridClient`, which resolves to
    // the inherent method (inherent methods take priority over trait methods),
    // so these delegate rather than recurse.
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        self.get_torrents().await
    }
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        self.get_torrent_info(id).await
    }
    async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        self.add_magnet(magnet).await
    }
    async fn select_files(&self, torrent_id: &str, file_ids: &str) -> Result<(), reqwest::Error> {
        self.select_files(torrent_id, file_ids).await
    }
    async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
        self.delete_torrent(torrent_id).await
    }
    async fn resolve_url(
        &self,
        loc: &crate::provider::FileLocator,
    ) -> Result<String, crate::error::AppError> {
        let link = loc
            .link
            .as_deref()
            .ok_or(crate::error::AppError::Unavailable)?;
        match self.unrestrict_link(link).await {
            Ok(resp) => Ok(resp.download),
            Err(e) => {
                // Map to `Unavailable` (→ instant repair) UNLESS the failure is a genuinely permanent
                // client error (401/403 bad token) that a re-add can't fix — mirroring TorBox's
                // `requestdl_status_is_permanent`. A 5xx or a transport-level failure (no status) is
                // transient; a stale/expired-link 4xx such as 404 is recoverable by a re-add-by-hash,
                // so it routes to repair too rather than failing hard. unrestrict makes 5xx terminal
                // and uses a small retry budget, so this returns promptly (instead of the full
                // ~10×60s window) and lets `dav_fs` fast-fail to instant repair.
                if e.status().is_some_and(unrestrict_status_is_permanent) {
                    Err(crate::error::AppError::Http(e))
                } else {
                    Err(crate::error::AppError::Unavailable)
                }
            }
        }
    }

    async fn invalidate(&self, loc: &crate::provider::FileLocator) {
        if let Some(link) = loc.link.as_deref() {
            self.invalidate_unrestrict_cache(link).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- capability-link Debug redaction ---

    #[test]
    fn debug_redacts_capability_links() {
        // A restricted link / signed CDN URL must never appear in a `{:?}` render (defence-in-depth
        // against a future debug log of one of these structs), mirroring FileLocator's redaction.
        let unr = UnrestrictResponse {
            id: "x".into(),
            link: "https://real-debrid.com/d/SECRETLINK".into(),
            download: "https://cdn.example/SIGNEDURL?token=abc".into(),
            ..Default::default()
        };
        let s = format!("{unr:?}");
        assert!(!s.contains("SECRETLINK"), "restricted link leaked: {s}");
        assert!(!s.contains("SIGNEDURL"), "signed CDN url leaked: {s}");
        assert!(s.contains("<redacted>"));
        // The cached wrapper embeds it → inherits the redaction.
        let cached = CachedUnrestrictResponse {
            response: unr,
            cached_at: std::time::Instant::now(),
        };
        assert!(!format!("{cached:?}").contains("SECRETLINK"));

        let ti = TorrentInfo {
            id: "t".into(),
            links: vec!["https://real-debrid.com/d/RESTRICTED".into()],
            ..Default::default()
        };
        let s = format!("{ti:?}");
        assert!(!s.contains("RESTRICTED"), "TorrentInfo link leaked: {s}");
        assert!(s.contains("<1 link(s)>"));

        let t = Torrent {
            id: "t".into(),
            links: vec!["https://real-debrid.com/d/RESTRICTED".into()],
            ..Default::default()
        };
        assert!(!format!("{t:?}").contains("RESTRICTED"));
    }

    // --- should_retry_status ---

    #[test]
    fn should_retry_status_retries_429() {
        assert!(RealDebridClient::should_retry_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
    }

    #[test]
    fn should_retry_status_retries_503() {
        assert!(RealDebridClient::should_retry_status(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        ));
    }

    #[test]
    fn should_retry_status_retries_502() {
        assert!(RealDebridClient::should_retry_status(
            reqwest::StatusCode::BAD_GATEWAY
        ));
    }

    #[test]
    fn should_retry_status_retries_504() {
        assert!(RealDebridClient::should_retry_status(
            reqwest::StatusCode::GATEWAY_TIMEOUT
        ));
    }

    #[test]
    fn should_retry_status_does_not_retry_200() {
        assert!(!RealDebridClient::should_retry_status(
            reqwest::StatusCode::OK
        ));
    }

    #[test]
    fn should_retry_status_does_not_retry_404() {
        assert!(!RealDebridClient::should_retry_status(
            reqwest::StatusCode::NOT_FOUND
        ));
    }

    #[test]
    fn should_retry_status_retries_500() {
        // A 500 is a server-side failure — usually transient — so it joins the bounded-retry 5xx
        // set (500/502/503/504). 4xx stay terminal (the request itself is wrong).
        assert!(RealDebridClient::should_retry_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    // --- fetch_with_retry does not retry permanent (non-retryable) error statuses ---

    /// Spawn a loopback server that replies to EVERY connection with `status_line` + `body`,
    /// counting how many connections it accepted. Used to assert the retry loop's attempt count.
    async fn spawn_counting(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = count.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status_line,
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}/", addr), count)
    }

    /// Like `spawn_counting` but adds `Retry-After: 0` so a retryable status's backoff is instant
    /// (keeps a full-retry-loop test fast instead of sleeping out the exponential backoff).
    async fn spawn_counting_retry_after_0(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = count.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "{}\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status_line,
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}/", addr), count)
    }

    #[tokio::test]
    async fn fetch_with_retry_surfaces_real_5xx_status_after_exhaustion() {
        // Every attempt returns a retryable 503 (with Retry-After:0 so backoff is instant). After all
        // attempts are exhausted, the surfaced error must carry the real 503 — not the synthetic 502
        // "deserialization failures" fallback that would misattribute a server outage to a schema bug.
        let (url, count) =
            spawn_counting_retry_after_0("HTTP/1.1 503 Service Unavailable", r#"{"error":"down"}"#)
                .await;
        let client = RealDebridClient::new("fake".to_string()).unwrap();
        let r: Result<serde_json::Value, _> = client
            .fetch_with_retry(|| client.client.get(&url), &[])
            .await;
        let err = r.expect_err("a persistent 503 must surface as an error");
        assert_eq!(
            err.status(),
            Some(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            "the persistent 503 must be surfaced, not a synthetic 502"
        );
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            10,
            "a retryable 5xx is retried the full attempt budget"
        );
    }

    #[tokio::test]
    async fn fetch_with_retry_does_not_retry_401() {
        // A persistent 401 (bad/expired token) is permanent — it must be returned on the FIRST
        // attempt, not retried all 10 times (each preceded by a rate-limiter wait + a warning).
        let (url, count) =
            spawn_counting("HTTP/1.1 401 Unauthorized", r#"{"error":"bad_token"}"#).await;
        let client = RealDebridClient::new("fake".to_string()).unwrap();
        let r: Result<serde_json::Value, _> = client
            .fetch_with_retry(|| client.client.get(&url), &[])
            .await;
        assert!(r.is_err(), "a 401 must surface as an error");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a permanent 401 must not be retried"
        );
    }

    #[tokio::test]
    async fn fetch_with_retry_n_honors_a_smaller_budget() {
        // The bounded variant used by the synchronous playback resolve must stop after `max_attempts`
        // retryable failures — NOT the default 10 — so a hanging API can't stall a WebDAV read.
        let (url, count) =
            spawn_counting_retry_after_0("HTTP/1.1 503 Service Unavailable", r#"{"error":"down"}"#)
                .await;
        let client = RealDebridClient::new("fake".to_string()).unwrap();
        let r: Result<serde_json::Value, _> = client
            .fetch_with_retry_n(|| client.client.get(&url), &[], 3)
            .await;
        assert!(r.is_err(), "a persistent 503 must surface as an error");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the bounded budget (3) must be honored, not the default 10"
        );
    }

    /// Loopback server that returns a non-empty torrents page for `page=1` and an empty array for
    /// every later page, optionally failing one specific page with a 500. Returns (base_url, count).
    async fn spawn_paged_torrents(
        fail_page: Option<&'static str>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = count.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status_line, body): (&str, String) = if let Some(fp) = fail_page {
                    if req.contains(&format!("page={}&", fp)) {
                        (
                            "HTTP/1.1 500 Internal Server Error",
                            r#"{"e":1}"#.to_string(),
                        )
                    } else if req.contains("page=1&") {
                        ("HTTP/1.1 200 OK", r#"[{"id":"a"}]"#.to_string())
                    } else {
                        ("HTTP/1.1 200 OK", "[]".to_string())
                    }
                } else if req.contains("page=1&") {
                    ("HTTP/1.1 200 OK", r#"[{"id":"a"},{"id":"b"}]"#.to_string())
                } else {
                    ("HTTP/1.1 200 OK", "[]".to_string())
                };
                // Retry-After:0 so a retryable 500 (the fail-page case) backs off instantly instead
                // of running the real exponential backoff (which would make this test take minutes).
                let head = format!(
                    "{}\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status_line,
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}", addr), count)
    }

    #[tokio::test]
    async fn get_torrents_paginates_until_an_empty_page() {
        // Page 1 returns two torrents, page 2 returns []. The loop must accumulate page 1 and stop.
        let (base, count) = spawn_paged_torrents(None).await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(base);
        let torrents = client.get_torrents().await.expect("listing succeeds");
        assert_eq!(torrents.len(), 2, "both page-1 torrents accumulate");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "fetches page 1 (data) then page 2 (empty → stop)"
        );
    }

    #[tokio::test]
    async fn get_torrents_fails_whole_listing_on_a_later_page_error() {
        // Page 1 OK, page 2 500. A truncated Ok would make downstream treat page-2 titles as lapsed
        // and re-acquire them, so the whole listing must fail instead.
        let (base, _count) = spawn_paged_torrents(Some("2")).await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(base);
        let r = client.get_torrents().await;
        assert!(
            r.is_err(),
            "a later-page error must fail the whole listing, never return a truncated Ok"
        );
    }

    #[tokio::test]
    async fn get_torrents_fails_on_an_empty_body_200_page_rather_than_truncating() {
        // A mid-pagination 200 with an EMPTY body (a reverse-proxy/CDN quirk) must NOT be read as an
        // empty page (end-of-list): that would silently drop every later page, and downstream treats
        // an owned title absent from the listing as lapsed → re-acquire. Fail the whole listing.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status_line, body): (&str, &str) = if req.contains("page=1&") {
                    ("HTTP/1.1 200 OK", r#"[{"id":"a"}]"#)
                } else {
                    // page 2+: a 200 with an empty body (NOT "[]").
                    ("HTTP/1.1 200 OK", "")
                };
                let head = format!(
                    "{}\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status_line,
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(format!("http://{}", addr));
        let r = client.get_torrents().await;
        assert!(
            r.is_err(),
            "an empty-body 200 page must fail the whole listing, not be treated as end-of-list"
        );
    }

    #[tokio::test]
    async fn delete_torrent_treats_404_as_success() {
        // Deleting an already-absent torrent must be idempotent: a 404 is the desired end state.
        let (base, _count) = spawn_counting("HTTP/1.1 404 Not Found", r#"{"e":"gone"}"#).await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(base.trim_end_matches('/').to_string());
        let r = client.delete_torrent("missing").await;
        assert!(r.is_ok(), "404 on delete must map to Ok(()), got {r:?}");
    }

    #[tokio::test]
    async fn resolve_url_maps_transport_failure_to_unavailable() {
        // Bind a port then drop the listener so connecting is refused (a transport-level failure with
        // no HTTP status). The synchronous resolve must map this to `Unavailable` (→ instant repair),
        // not a hard `Http` error, and must do so within the small bounded budget.
        use crate::provider::DebridProvider as _;
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        }; // listener dropped here → port refuses connections
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(format!("http://{}", dead));
        let loc = crate::provider::FileLocator {
            hash: "h".to_string(),
            torrent_id: "t".to_string(),
            file_id: 0,
            file_path: "/x.mkv".to_string(),
            link: Some("https://real-debrid.com/d/abc".to_string()),
        };
        let r = client.resolve_url(&loc).await;
        assert!(
            matches!(r, Err(crate::error::AppError::Unavailable)),
            "a transport failure on resolve must map to Unavailable, got {r:?}"
        );
    }

    #[tokio::test]
    async fn resolve_url_maps_stale_link_404_to_unavailable_but_auth_401_to_http() {
        // A stale/expired-link 404 on unrestrict is recoverable by a re-add-by-hash → Unavailable
        // (instant repair), matching TorBox. Only a bad token (401/403) stays a hard Http error.
        use crate::provider::DebridProvider as _;
        let loc = crate::provider::FileLocator {
            hash: "h".to_string(),
            torrent_id: "t".to_string(),
            file_id: 0,
            file_path: "/x.mkv".to_string(),
            link: Some("https://real-debrid.com/d/abc".to_string()),
        };
        let (url, _c) = spawn_counting("HTTP/1.1 404 Not Found", "{}").await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(url);
        assert!(
            matches!(
                client.resolve_url(&loc).await,
                Err(crate::error::AppError::Unavailable)
            ),
            "a 404 on unrestrict must map to Unavailable (recoverable by re-add), not a hard Http error"
        );
        let (url, _c) = spawn_counting("HTTP/1.1 401 Unauthorized", "{}").await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(url);
        assert!(
            matches!(
                client.resolve_url(&loc).await,
                Err(crate::error::AppError::Http(_))
            ),
            "a 401 (bad token) on unrestrict must stay a hard Http error"
        );
    }

    #[tokio::test]
    async fn wait_for_retry_does_not_sleep_on_final_attempt() {
        // On the final attempt there is nothing to wait for — the caller gives up immediately.
        // A Retry-After header must NOT cause a (capped, up-to-5-minute) sleep here.
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            HeaderValue::from_static("300"),
        );
        let r = tokio::time::timeout(
            Duration::from_millis(200),
            RealDebridClient::wait_for_retry(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                &headers,
                10,
                10,
            ),
        )
        .await;
        assert!(
            r.is_ok(),
            "wait_for_retry must return immediately on the final attempt, not sleep Retry-After"
        );
    }

    // --- Compile-time signature checks for the unified fetch_with_retry ---
    // These functions are never called; they exist only to fail compilation
    // if the method signatures change unexpectedly.

    #[allow(dead_code)]
    async fn _assert_fetch_with_retry_accepts_terminal_statuses(client: &RealDebridClient) {
        // Verifies fetch_with_retry accepts &[StatusCode] as second argument.
        // If the signature changes this will not compile.
        let _: Result<serde_json::Value, _> = client
            .fetch_with_retry(
                || client.client.get("https://example.com"),
                &[
                    reqwest::StatusCode::NOT_FOUND,
                    reqwest::StatusCode::SERVICE_UNAVAILABLE,
                ],
            )
            .await;
    }

    #[allow(dead_code)]
    async fn _assert_fetch_with_retry_accepts_empty_terminal_statuses(client: &RealDebridClient) {
        let _: Result<serde_json::Value, _> = client
            .fetch_with_retry(|| client.client.get("https://example.com"), &[])
            .await;
    }

    // --- Cache eviction ---

    #[tokio::test]
    async fn unrestrict_cache_evicts_expired_entries() {
        let client = RealDebridClient::new("fake-token".to_string()).unwrap();
        // Seed cache with entries - we need to directly manipulate the cache
        {
            let mut cache = client.unrestrict_cache.write().await;
            // Insert an "old" entry with a manually backdated timestamp
            cache.insert(
                "old-link".to_string(),
                CachedUnrestrictResponse {
                    response: UnrestrictResponse {
                        id: "old".to_string(),
                        filename: "old.mkv".to_string(),
                        mime_type: None,
                        filesize: 0,
                        link: "old-link".to_string(),
                        host: String::new(),
                        chunks: 0,
                        crc: 0,
                        download: "http://old".to_string(),
                        streamable: 0,
                    },
                    cached_at: std::time::Instant::now() - Duration::from_secs(7200), // 2 hours ago
                },
            );
            cache.insert(
                "new-link".to_string(),
                CachedUnrestrictResponse {
                    response: UnrestrictResponse {
                        id: "new".to_string(),
                        filename: "new.mkv".to_string(),
                        mime_type: None,
                        filesize: 0,
                        link: "new-link".to_string(),
                        host: String::new(),
                        chunks: 0,
                        crc: 0,
                        download: "http://new".to_string(),
                        streamable: 0,
                    },
                    cached_at: std::time::Instant::now(),
                },
            );
        }
        client.evict_expired_cache().await;
        let cache = client.unrestrict_cache.read().await;
        assert!(
            !cache.contains_key("old-link"),
            "Expired entry should be evicted"
        );
        assert!(
            cache.contains_key("new-link"),
            "Fresh entry should be retained"
        );
    }

    #[tokio::test]
    async fn unrestrict_cache_enforces_size_cap() {
        // BEHAVIOUR (replaces a constant-literal pin): when the cache exceeds MAX_CACHE_SIZE,
        // eviction trims it back to the cap. Exercises the size-cap branch of `evict_expired_cache`
        // that the TTL-expiry test (`unrestrict_cache_evicts_expired_entries`) never reaches.
        let client = RealDebridClient::new("fake-token".to_string()).unwrap();
        {
            let mut cache = client.unrestrict_cache.write().await;
            for i in 0..(MAX_CACHE_SIZE + 5) {
                cache.insert(
                    format!("link-{i}"),
                    CachedUnrestrictResponse {
                        response: UnrestrictResponse::default(),
                        cached_at: std::time::Instant::now(), // all fresh → only the size cap trims
                    },
                );
            }
        }
        client.evict_expired_cache().await;
        assert_eq!(
            client.unrestrict_cache.read().await.len(),
            MAX_CACHE_SIZE,
            "eviction must trim an over-cap cache back to MAX_CACHE_SIZE"
        );
    }

    #[tokio::test]
    async fn invalidate_unrestrict_cache_removes_entry() {
        let client = RealDebridClient::new("fake-token".to_string()).unwrap();
        {
            let mut cache = client.unrestrict_cache.write().await;
            cache.insert(
                "test-link".to_string(),
                CachedUnrestrictResponse {
                    response: UnrestrictResponse {
                        id: "test".to_string(),
                        filename: "test.mkv".to_string(),
                        mime_type: None,
                        filesize: 0,
                        link: "test-link".to_string(),
                        host: String::new(),
                        chunks: 0,
                        crc: 0,
                        download: "http://test".to_string(),
                        streamable: 0,
                    },
                    cached_at: std::time::Instant::now(),
                },
            );
        }
        assert!(client
            .unrestrict_cache
            .read()
            .await
            .contains_key("test-link"));
        client.invalidate_unrestrict_cache("test-link").await;
        assert!(!client
            .unrestrict_cache
            .read()
            .await
            .contains_key("test-link"));
    }

    #[tokio::test]
    async fn unrestrict_link_fetches_once_then_serves_from_cache() {
        // End-to-end cache path: the first call hits the API and stores the result; a second call for
        // the SAME link is served from cache without a second request. (The manually-seeded-cache
        // tests never exercise the fetch → store → hit round-trip.)
        use std::sync::atomic::Ordering;
        let body =
            r#"{"id":"x","link":"https://real-debrid.com/d/abc","download":"https://cdn/x"}"#;
        let (url, count) = spawn_counting("HTTP/1.1 200 OK", body).await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(url.trim_end_matches('/').to_string());
        let r1 = client
            .unrestrict_link("https://real-debrid.com/d/abc")
            .await
            .expect("first unrestrict must fetch");
        assert_eq!(r1.download, "https://cdn/x");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let r2 = client
            .unrestrict_link("https://real-debrid.com/d/abc")
            .await
            .expect("second unrestrict must be a cache hit");
        assert_eq!(r2.download, "https://cdn/x");
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "the second call for the same link must be served from cache (no second request)"
        );
    }

    // --- Serde deserialization robustness ---

    #[test]
    fn torrent_info_deserializes_without_files_field() {
        // RD could return torrent info without a files array (e.g., magnet not yet processed).
        // With #[serde(default)], this should deserialize with an empty files vec.
        let json = r#"{"id":"abc123"}"#;
        let info: TorrentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "abc123");
        assert!(info.files.is_empty());
        assert!(info.links.is_empty());
        assert_eq!(info.filename, "");
        assert_eq!(info.hash, "");
        assert_eq!(info.bytes, 0);
        assert_eq!(info.status, "");
    }

    #[test]
    fn torrent_info_deserializes_with_all_fields() {
        let json = r#"{
            "id": "abc123",
            "filename": "test.mkv",
            "original_filename": "test.mkv",
            "hash": "deadbeef",
            "bytes": 1000,
            "original_bytes": 1000,
            "host": "real-debrid.com",
            "split": 1,
            "progress": 100.0,
            "status": "downloaded",
            "added": "2023-01-01",
            "files": [{"id": 1, "path": "/test.mkv", "bytes": 1000, "selected": 1}],
            "links": ["https://link1"],
            "ended": "2023-01-01"
        }"#;
        let info: TorrentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "abc123");
        assert_eq!(info.files.len(), 1);
        assert_eq!(info.files[0].path, "/test.mkv");
        assert_eq!(info.links.len(), 1);
    }

    #[test]
    fn torrent_file_deserializes_with_minimal_fields() {
        // TorrentFile with only default values should work
        let json = r#"{}"#;
        let file: TorrentFile = serde_json::from_str(json).unwrap();
        assert_eq!(file.id, 0);
        assert_eq!(file.path, "");
        assert_eq!(file.bytes, 0);
        assert_eq!(file.selected, 0);
    }

    #[test]
    fn torrent_deserializes_with_only_id() {
        // The list endpoint might return minimal data for some torrents
        let json = r#"{"id":"xyz"}"#;
        let torrent: Torrent = serde_json::from_str(json).unwrap();
        assert_eq!(torrent.id, "xyz");
        assert_eq!(torrent.filename, "");
        assert_eq!(torrent.status, "");
        assert!(torrent.links.is_empty());
    }

    #[test]
    fn torrent_info_ignores_unknown_fields() {
        // RD may add new fields to their API responses. serde's default behavior
        // is to ignore unknown fields, but this test verifies it explicitly.
        let json = r#"{
            "id": "abc123",
            "filename": "test.mkv",
            "some_new_field": "unexpected_value",
            "another_field": 42
        }"#;
        let info: TorrentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "abc123");
        assert_eq!(info.filename, "test.mkv");
    }

    #[test]
    fn rd_structs_have_default() {
        let t = Torrent::default();
        assert_eq!(t.id, "");
        let info = TorrentInfo::default();
        assert!(info.files.is_empty());
        let f = TorrentFile::default();
        assert_eq!(f.selected, 0);
        let u = UnrestrictResponse::default();
        assert_eq!(u.download, "");
        let m = AddMagnetResponse::default();
        assert_eq!(m.id, "");
    }
}
