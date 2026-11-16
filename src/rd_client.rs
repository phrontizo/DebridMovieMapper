use crate::ratelimit::{AdaptiveRateLimiter, MAX_RETRY_AFTER_SECS};
use rand::RngExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

const MAX_CACHE_SIZE: usize = 10_000;
const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour
/// Baseline spacing between RD API requests. RD documents a limit of 250 requests per minute
/// (240 ms apart) and sends no rate-limit headers, so steady pacing is the only proactive control;
/// pacing faster guarantees 429s on any burst (a first-run identification of every torrent, a
/// multi-page listing alongside other jobs).
const RD_MIN_INTERVAL_MS: u64 = 250;

use crate::provider::synthetic_bad_gateway;

/// Whether an `unrestrict/link` failure must fail the read WITHOUT instant repair: a bad token
/// (401/403), which a re-add can't fix, or throttling (429), which says nothing about whether the
/// bytes exist (repairing would re-add, and on RD delete, a healthy torrent while adding about five
/// more calls to the API that is throttling us). Everything else (a 5xx, a transport-level failure,
/// or a stale/expired-link 404 that a re-add-by-hash recovers) maps to `AppError::Unavailable` →
/// repair. Mirrors TorBox's `requestdl_status_skips_repair` so the two providers behave alike.
fn unrestrict_status_skips_repair(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::TOO_MANY_REQUESTS
    )
}

/// Build `{base}/{prefix}/{seg}` with `seg` PERCENT-ENCODED as a single path segment, so a
/// caller-supplied id can't corrupt or escape the path (consistent with how the query params are
/// encoded). RD ids are alphanumeric, so this is a no-op in practice — a robustness/consistency
/// guard. Falls back to plain interpolation if `base` somehow isn't a valid absolute URL.
fn build_id_url(base: &str, prefix: &str, seg: &str) -> String {
    match reqwest::Url::parse(&format!("{base}/{prefix}")) {
        Ok(mut u) => {
            match u.path_segments_mut() {
                Ok(mut segs) => {
                    segs.push(seg);
                }
                Err(_) => return format!("{base}/{prefix}/{seg}"),
            }
            u.to_string()
        }
        Err(_) => format!("{base}/{prefix}/{seg}"),
    }
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

/// Keep only the first occurrence of each torrent id in a paginated listing, in first-seen order,
/// so every consumer sees each torrent once. Both clients page a newest-first listing, and an add
/// that lands between two page fetches shifts every entry down by one, so the next page starts
/// with the previous page's last torrent again. A provider id is unique per torrent, so a repeated
/// id is always that artefact. Left in, it reaches every caller that collects the ids of a hash
/// from the listing and deletes each one: the second delete of the same id is a 404 on Real-Debrid
/// (mapped to `Ok`), but on TorBox it is an `Err`, which those callers read as a failed delete
/// and keep the records for. An empty id (never sent by either provider) identifies nothing, so
/// such entries are all kept.
pub(crate) fn drop_repeated_ids(torrents: Vec<Torrent>, provider: &str) -> Vec<Torrent> {
    let total = torrents.len();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let kept: Vec<Torrent> = torrents
        .into_iter()
        .filter(|t| t.id.is_empty() || seen.insert(t.id.clone()))
        .collect();
    if kept.len() < total {
        debug!(
            "{} listing: dropped {} page-boundary repeat(s) of an id already listed",
            provider,
            total - kept.len()
        );
    }
    kept
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
    /// Our own deletes, so a multi-page `get_torrents` can re-list when one may have shifted its
    /// pages (see `provider::DeleteTracker`).
    deletes: crate::provider::DeleteTracker,
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
            rate_limiter: Arc::new(AdaptiveRateLimiter::with_min_interval(
                Duration::from_millis(RD_MIN_INTERVAL_MS),
            )),
            base_url: "https://api.real-debrid.com/rest/1.0".to_string(),
            deletes: crate::provider::DeleteTracker::default(),
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

    /// The whole torrent listing. If one of our own deletes may have landed while it was paging (see
    /// `provider::DeleteTracker`), the pages may have shifted under it and dropped a live torrent, so
    /// it lists again — up to `MAX_LISTING_ATTEMPTS` times, then fails rather than return a listing
    /// that may be short.
    pub async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        for attempt in 1..=crate::provider::MAX_LISTING_ATTEMPTS {
            let since = self.deletes.snapshot();
            let (torrents, spans_pages) = self.list_torrent_pages().await?;
            if !spans_pages || !self.deletes.changed_since(since) {
                return Ok(torrents);
            }
            if attempt < crate::provider::MAX_LISTING_ATTEMPTS {
                warn!(
                    "RD torrent listing overlapped a delete (attempt {}/{}); listing again in case \
                     the pages shifted",
                    attempt,
                    crate::provider::MAX_LISTING_ATTEMPTS
                );
            } else {
                warn!(
                    "RD torrent listing overlapped a delete on all {} attempts; giving up (callers \
                     skip this tick)",
                    crate::provider::MAX_LISTING_ATTEMPTS
                );
            }
        }
        Err(synthetic_bad_gateway(
            b"get_torrents: concurrent deletes kept shifting the pages",
        ))
    }

    /// Fetch every page once. Also returns whether the listing ran past a single short page — only
    /// then can a delete on a fetched page shift an unfetched item back onto it.
    async fn list_torrent_pages(&self) -> Result<(Vec<Torrent>, bool), reqwest::Error> {
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
        let mut non_empty_pages = 0usize;
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
                    non_empty_pages += 1;
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
        // A full first page means the empty terminator was fetched at a page boundary, so an item
        // can still have shifted out of reach even with only one non-empty page.
        let spans_pages = non_empty_pages > 1 || all_torrents.len() >= PAGE_LIMIT as usize;
        Ok((drop_repeated_ids(all_torrents, "RD"), spans_pages))
    }

    pub async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        let url = build_id_url(&self.base_url, "torrents/info", id);
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
        // → instant repair, rather than stalling the WebDAV read. Each attempt also gets the short
        // `READ_PATH_REQUEST_TIMEOUT` instead of the client's 60 s, so an endpoint that accepts the
        // connection and then hangs costs 3 × 15 s rather than 3 minutes.
        let response: UnrestrictResponse = self
            .fetch_with_retry_n(
                || {
                    self.client
                        .post(&url)
                        .timeout(crate::provider::READ_PATH_REQUEST_TIMEOUT)
                        .form(&[("link", link)])
                },
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
        let url = build_id_url(&self.base_url, "torrents/selectFiles", torrent_id);
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
        let url = build_id_url(&self.base_url, "torrents/delete", torrent_id);
        // Held across the retries: the delete can land on any attempt.
        let _in_flight = self.deletes.begin();
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
                            // A body that breaks off mid-stream (a reset, a body cut short of its
                            // Content-Length, the request timeout firing while it streams) is
                            // retried like a statusless transport error, with the same small
                            // backoff, so one blip on `unrestrict/link` doesn't fail the resolve
                            // over to an instant repair. With the budget spent, the body error
                            // itself is surfaced (via `last_error` after the loop). It is not a
                            // decode failure, so it doesn't count toward that cap.
                            let text = match resp.text().await {
                                Ok(text) => text,
                                Err(e) => {
                                    warn!(
                                        "RD API body read failed (attempt {}/{}): {}",
                                        attempt, max_attempts, e
                                    );
                                    last_error = Some(e);
                                    if attempt < max_attempts {
                                        let backoff = Duration::from_millis(
                                            (100 * attempt as u64).min(1_000),
                                        );
                                        tokio::time::sleep(backoff).await;
                                    }
                                    continue;
                                }
                            };
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
                                    // to spot a schema change. (Its message can quote a mismatched
                                    // SCALAR value, but `link`/`download` are strings decoded into
                                    // `String` fields, so they never appear as a mismatched value.)
                                    error!(
                                        "Failed to decode RD response: {}. Status: {}, body {} bytes (redacted)",
                                        e, status, text.len()
                                    );
                                    // Retry ONE transient decode blip, then bail — a *persistent*
                                    // decode failure is a schema change that won't fix itself on retry.
                                    // This cap (2 attempts) is DELIBERATELY tighter than TorBox's
                                    // full-budget decode-retry (`send_enveloped`, the loop behind
                                    // `send_data_n`/`send_ok`): `unrestrict_link` runs
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
                    // refused) spins the whole budget in a tight warn loop — the rate limiter's 250ms
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
                // Map to `Unavailable` (→ instant repair) UNLESS the failure is a bad token (401/403)
                // that a re-add can't fix, or throttling (429) that outlasted the read budget —
                // throttling says nothing about whether the bytes exist, so the read fails fast as
                // `Http` without a re-add (mirroring TorBox's `requestdl_status_skips_repair`). A 5xx
                // or a transport-level failure (no status) is transient; a stale/expired-link 4xx such
                // as 404 is recoverable by a re-add-by-hash, so it routes to repair too rather than
                // failing hard. unrestrict makes 5xx terminal and uses a small retry budget, so this
                // returns promptly (instead of the full ~10×60s window) and lets `dav_fs` fast-fail
                // to instant repair.
                if e.status().is_some_and(unrestrict_status_skips_repair) {
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

    #[tokio::test(start_paused = true)]
    async fn paces_requests_within_rds_documented_rate_limit() {
        // RD documents 250 requests per minute and sends no rate-limit headers, so the baseline
        // spacing is the only proactive control: it must not be tighter than 60 s / 250 = 240 ms.
        let client = RealDebridClient::new("fake".to_string()).unwrap();
        let t0 = tokio::time::Instant::now();
        client.rate_limiter.wait_for_token().await;
        client.rate_limiter.wait_for_token().await;
        assert!(
            t0.elapsed() >= Duration::from_millis(60_000 / 250),
            "two back-to-back requests were only {:?} apart",
            t0.elapsed()
        );
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

    /// Loopback server whose first `cut_first` connections get a `200 OK` that promises a longer
    /// body (`Content-Length`) than it sends before closing — a body that breaks off mid-stream —
    /// and whose later connections get `body` in full. Returns (base_url, connection count).
    async fn spawn_cut_body(
        cut_first: usize,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = count.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let seen = c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let (len, sent) = if seen < cut_first {
                    (body.len() + 100, &body[..body.len() / 2])
                } else {
                    (body.len(), body)
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(sent.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}/", addr), count)
    }

    #[tokio::test]
    async fn fetch_with_retry_retries_a_body_cut_off_mid_stream() {
        // A 200 whose body breaks off mid-stream is retried like a statusless transport error, so
        // one blip on `unrestrict/link` doesn't fail the resolve (→ an instant repair that re-adds
        // a healthy torrent under a new id and deletes the original).
        let body =
            r#"{"id":"x","link":"https://real-debrid.com/d/abc","download":"https://cdn/x"}"#;
        let (url, count) = spawn_cut_body(1, body).await;
        let client = RealDebridClient::new("fake".to_string()).unwrap();
        let r: Result<serde_json::Value, _> = client
            .fetch_with_retry_n(|| client.client.get(&url), &[], 3)
            .await;
        assert!(r.is_ok(), "the retry must recover the body: {r:?}");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fetch_with_retry_body_cut_off_on_every_attempt_surfaces_the_read_error() {
        // With the budget spent, the body error itself is returned (no HTTP status, so
        // `resolve_url` maps it to Unavailable), not the synthetic decode-failure 502.
        let body =
            r#"{"id":"x","link":"https://real-debrid.com/d/abc","download":"https://cdn/x"}"#;
        let (url, count) = spawn_cut_body(usize::MAX, body).await;
        let client = RealDebridClient::new("fake".to_string()).unwrap();
        let r: Result<serde_json::Value, _> = client
            .fetch_with_retry_n(|| client.client.get(&url), &[], 3)
            .await;
        let err = r.expect_err("a body cut off on every attempt must surface as an error");
        assert_eq!(
            err.status(),
            None,
            "must be the body error, not a synthetic 502: {err}"
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn fetch_with_retry_surfaces_synthetic_502_after_decode_cap() {
        // A 200 whose body never decodes into T is a schema change that won't fix itself on retry.
        // After the 2-failure decode cap the loop clears `last_error` and must surface the synthetic
        // 502 (not panic, not a stale prior status) — and it must stop at exactly 2 requests even when
        // the budget is larger, so the latency-sensitive resolve path fast-fails rather than burning
        // the whole budget on a permanent format change.
        let (url, count) = spawn_counting("HTTP/1.1 200 OK", "not valid json").await;
        let client = RealDebridClient::new("fake".to_string()).unwrap();
        let r: Result<serde_json::Value, _> = client
            .fetch_with_retry_n(|| client.client.get(&url), &[], 5)
            .await;
        let err = r.expect_err("a persistently-undecodable 200 must surface as an error");
        assert_eq!(
            err.status(),
            Some(reqwest::StatusCode::BAD_GATEWAY),
            "a persistent decode failure must surface as the synthetic 502"
        );
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the 2-failure decode cap stops the loop before the 5-attempt budget"
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

    /// Serve a paginated `/torrents` listing whose page 1 holds `page1_len` torrents — plus one more
    /// on page 2 when that fills the 100-item page — and, while serving page 1 of each listing that
    /// `bump` selects (1-based), record a delete on the client's tracker, as if another job deleted
    /// a torrent mid-listing. Returns the listing result and how many times page 1 was fetched.
    async fn list_with_concurrent_deletes(
        page1_len: usize,
        bump: fn(usize) -> bool,
    ) -> (Result<Vec<Torrent>, reqwest::Error>, usize) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = Arc::new(
            RealDebridClient::new("fake".to_string())
                .unwrap()
                .with_base_url(format!("http://{addr}")),
        );
        let page1_hits = Arc::new(AtomicUsize::new(0));
        let (c, hits) = (client.clone(), page1_hits.clone());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if req.contains("page=1&") {
                    if bump(hits.fetch_add(1, Ordering::SeqCst) + 1) {
                        drop(c.deletes.begin());
                    }
                    let items: Vec<String> = (0..page1_len)
                        .map(|i| format!(r#"{{"id":"a{i}"}}"#))
                        .collect();
                    format!("[{}]", items.join(","))
                } else if req.contains("page=2&") && page1_len >= 100 {
                    r#"[{"id":"b"}]"#.to_string()
                } else {
                    "[]".to_string()
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        let r = client.get_torrents().await;
        (r, page1_hits.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn get_torrents_relists_when_a_delete_lands_mid_listing() {
        // A delete during a multi-page listing may have shifted an item from page 2 onto page 1,
        // where it is never returned. The listing must not be trusted: list again.
        let (r, page1_fetches) = list_with_concurrent_deletes(100, |listing| listing == 1).await;
        assert_eq!(
            r.expect("the second, undisturbed listing succeeds").len(),
            101
        );
        assert_eq!(page1_fetches, 2, "the disturbed listing is retried once");
    }

    #[tokio::test]
    async fn get_torrents_fails_when_deletes_keep_shifting_the_pages() {
        // If every attempt overlaps a delete, fail the listing (callers skip the tick) rather than
        // return an Ok that may be missing a live torrent.
        let (r, page1_fetches) = list_with_concurrent_deletes(100, |_| true).await;
        assert!(r.is_err(), "a listing that never settles must be an Err");
        assert_eq!(
            page1_fetches,
            crate::provider::MAX_LISTING_ATTEMPTS as usize
        );
    }

    /// An add that lands between two page fetches shifts a newest-first listing down by one, so
    /// page 2 starts with page 1's last torrent again. The listing must carry each id once: every
    /// consumer that deletes by the ids it finds for a hash would otherwise delete that torrent
    /// twice.
    #[tokio::test]
    async fn get_torrents_drops_a_page_boundary_repeat() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if req.contains("page=1&") {
                    let items: Vec<String> =
                        (0..100).map(|i| format!(r#"{{"id":"a{i}"}}"#)).collect();
                    format!("[{}]", items.join(","))
                } else if req.contains("page=2&") {
                    // A concurrent add pushed a99 down onto page 2.
                    r#"[{"id":"a99"}]"#.to_string()
                } else {
                    "[]".to_string()
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(format!("http://{addr}"));
        let torrents = client.get_torrents().await.expect("listing succeeds");
        let ids: Vec<String> = torrents.iter().map(|t| t.id.clone()).collect();
        let expected: Vec<String> = (0..100).map(|i| format!("a{i}")).collect();
        assert_eq!(ids, expected, "each id once, in first-seen order");
    }

    /// Torrents with an empty id (which no RD answer should carry) are not one torrent repeated:
    /// the repeat filter leaves them all in place.
    #[test]
    fn drop_repeated_ids_keeps_every_empty_id() {
        let t = |id: &str| Torrent {
            id: id.to_string(),
            ..Default::default()
        };
        let kept = drop_repeated_ids(vec![t("a"), t(""), t("b"), t("a"), t("")], "test");
        let ids: Vec<&str> = kept.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "", "b", ""]);
    }

    #[tokio::test]
    async fn get_torrents_ignores_a_delete_when_everything_fits_on_one_page() {
        // With every torrent on one short page, a delete can't push anything past what was fetched.
        let (r, page1_fetches) = list_with_concurrent_deletes(2, |_| true).await;
        assert_eq!(r.expect("a single-page listing stands").len(), 2);
        assert_eq!(page1_fetches, 1, "no re-list for a single short page");
    }

    #[tokio::test]
    async fn select_files_and_delete_succeed_on_204_no_content_in_one_request() {
        // RD answers selectFiles and delete with `204 No Content`. fetch_with_retry decodes "[]" for
        // a 204 only (an empty-body 200 must still fail), so both calls must be Ok on the first try.
        use std::sync::atomic::Ordering;
        let (url, count) = spawn_counting("HTTP/1.1 204 No Content", "").await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(url);
        client
            .select_files("1", "2")
            .await
            .expect("a 204 on selectFiles is success");
        assert_eq!(count.load(Ordering::SeqCst), 1, "no retry on a 204");
        let before = client.deletes.snapshot();
        client
            .delete_torrent("1")
            .await
            .expect("a 204 on delete is success");
        assert_eq!(count.load(Ordering::SeqCst), 2, "no retry on a 204");
        assert!(
            client.deletes.changed_since(before),
            "a delete is recorded so a concurrent multi-page listing re-lists"
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

    /// Spawn a loopback server that CLOSES the first accepted connection without sending any HTTP
    /// response (a statusless transport error — EOF before any status/headers), then serves `body`
    /// under `status_line` on every later connection. Lets a test drive the "transient transport
    /// error → retry → succeed" arm. Returns (base_url, connection_count).
    async fn spawn_fail_first_then_serve(
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
                let seen = c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                if seen == 0 {
                    // First connection: drain the request then drop the socket WITHOUT any HTTP
                    // response, so the client's `send()` fails at the transport level (EOF before
                    // headers → no HTTP status) — exercising the statusless-transport-error retry arm.
                    drop(sock);
                    continue;
                }
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

    #[tokio::test]
    async fn unrestrict_retries_transport_error_then_succeeds() {
        // Complement to `resolve_url_maps_transport_failure_to_unavailable`, which only pins the
        // statusless-transport-error arm to FAILURE (every attempt refused). Here the FIRST connection
        // is dropped without an HTTP response (a transport error, no status) and the RETRY is served
        // valid unrestrict JSON — so the small-backoff/retry-within-budget arm must recover and
        // ultimately SUCCEED. Asserts both the Ok result and that it took exactly two connection
        // attempts (fail once, then succeed) — the retry actually re-connected, not a cache hit.
        use std::sync::atomic::Ordering;
        let body =
            r#"{"id":"x","link":"https://real-debrid.com/d/abc","download":"https://cdn/x"}"#;
        let (url, count) = spawn_fail_first_then_serve("HTTP/1.1 200 OK", body).await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(url);
        let resp = client
            .unrestrict_link("https://real-debrid.com/d/abc")
            .await
            .expect("a transient transport error must be retried and then succeed");
        assert_eq!(
            resp.download, "https://cdn/x",
            "the retry's valid response must be returned"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "the first (dropped) connection fails at transport; the second (served) succeeds"
        );
    }

    #[tokio::test]
    async fn resolve_url_maps_a_none_link_to_unavailable() {
        // A locator with no restricted `link` (e.g. a mirror/repair edge case) must resolve to
        // Unavailable → instant repair, NOT an Http error or a panic. No network is touched.
        use crate::provider::DebridProvider as _;
        let client = RealDebridClient::new("fake".to_string()).unwrap();
        let loc = crate::provider::FileLocator {
            hash: "h".to_string(),
            torrent_id: "t".to_string(),
            file_id: 0,
            file_path: "/x.mkv".to_string(),
            link: None,
        };
        assert!(
            matches!(
                client.resolve_url(&loc).await,
                Err(crate::error::AppError::Unavailable)
            ),
            "a None link must map to Unavailable"
        );
    }

    #[test]
    fn build_id_url_percent_encodes_the_id_segment() {
        // A normal id is interpolated unchanged; a path-significant char is percent-encoded so it
        // can't escape the path segment.
        assert_eq!(
            build_id_url(
                "https://api.real-debrid.com/rest/1.0",
                "torrents/info",
                "ABC123"
            ),
            "https://api.real-debrid.com/rest/1.0/torrents/info/ABC123"
        );
        let evil = build_id_url(
            "https://api.real-debrid.com/rest/1.0",
            "torrents/delete",
            "a/b?x",
        );
        assert!(evil.ends_with("/torrents/delete/a%2Fb%3Fx"), "got {evil}");
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
    async fn resolve_url_maps_a_persistent_429_to_http_not_unavailable() {
        // Throttling says nothing about whether the bytes exist. A 429 that outlasts the 3-attempt
        // read budget must fail the read as a hard `Http` error, never `Unavailable`: that would
        // send `dav_fs` into instant repair, which re-adds (and on RD deletes) a healthy torrent and
        // adds about five more calls to the API that is already throttling us.
        use crate::provider::DebridProvider as _;
        let (url, count) = spawn_counting("HTTP/1.1 429 Too Many Requests", "{}").await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(url);
        let loc = crate::provider::FileLocator {
            hash: "h".to_string(),
            torrent_id: "t".to_string(),
            file_id: 0,
            file_path: "/x.mkv".to_string(),
            link: Some("https://real-debrid.com/d/abc".to_string()),
        };
        match client.resolve_url(&loc).await {
            Err(crate::error::AppError::Http(e)) => assert_eq!(
                e.status(),
                Some(reqwest::StatusCode::TOO_MANY_REQUESTS),
                "the surfaced error must carry the real 429"
            ),
            other => panic!("a persistent 429 must map to Http, not repair: {other:?}"),
        }
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "a 429 is still retried within the 3-attempt read budget"
        );
    }

    /// Spawn a loopback server that accepts every connection and reads the request but never
    /// answers — an endpoint that is up at the TCP level and then hangs.
    async fn spawn_never_answers() -> String {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                held.push(sock); // keep it open, never reply
            }
        });
        format!("http://{}", addr)
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_url_gives_up_on_a_hanging_endpoint_within_the_read_timeout() {
        // The on-read resolve must not wait out the client's 60 s timeout on each of its 3 attempts
        // (3 minutes) when the API accepts the connection and never answers.
        use crate::provider::DebridProvider as _;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(spawn_never_answers().await);
        let loc = crate::provider::FileLocator {
            torrent_id: "t".to_string(),
            link: Some("https://real-debrid.com/d/abc".to_string()),
            ..Default::default()
        };
        let started = tokio::time::Instant::now();
        let r = client.resolve_url(&loc).await;
        assert!(
            matches!(r, Err(crate::error::AppError::Unavailable)),
            "a hanging endpoint is a transport failure → Unavailable, got {r:?}"
        );
        assert!(
            started.elapsed()
                <= 3 * crate::provider::READ_PATH_REQUEST_TIMEOUT + Duration::from_secs(5),
            "the read-path budget must bound each attempt, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn unrestrict_status_skips_repair_for_auth_and_throttling_only() {
        use reqwest::StatusCode;
        assert!(unrestrict_status_skips_repair(StatusCode::UNAUTHORIZED));
        assert!(unrestrict_status_skips_repair(StatusCode::FORBIDDEN));
        assert!(unrestrict_status_skips_repair(
            StatusCode::TOO_MANY_REQUESTS
        ));
        // A stale-link 404 and every 5xx are recoverable by a re-add, so they still repair.
        assert!(!unrestrict_status_skips_repair(StatusCode::NOT_FOUND));
        assert!(!unrestrict_status_skips_repair(
            StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(!unrestrict_status_skips_repair(
            StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[tokio::test]
    async fn resolve_url_success_returns_the_download_link() {
        // The trait-level `resolve_url` SUCCESS path returns the unrestricted `download` URL (the
        // field `dav_fs` streams bytes from), NOT `link`. The error paths are covered above; this
        // pins the happy path so a future refactor can't silently return the wrong field.
        use crate::provider::DebridProvider as _;
        let loc = crate::provider::FileLocator {
            hash: "h".to_string(),
            torrent_id: "t".to_string(),
            file_id: 0,
            file_path: "/x.mkv".to_string(),
            link: Some("https://real-debrid.com/d/abc".to_string()),
        };
        let body =
            r#"{"id":"x","link":"https://real-debrid.com/d/abc","download":"https://cdn/x"}"#;
        let (url, _c) = spawn_counting("HTTP/1.1 200 OK", body).await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(url);
        assert_eq!(
            client.resolve_url(&loc).await.unwrap(),
            "https://cdn/x",
            "resolve_url must return the `download` URL on success"
        );
    }

    #[tokio::test]
    async fn unrestrict_link_5xx_is_terminal_single_attempt_and_resolves_unavailable() {
        // A 5xx on unrestrict/link is in the terminal set, so the synchronous playback resolve must
        // hit the API exactly ONCE (fail over to instant repair) rather than run the 3-attempt budget
        // — which would add latency to a WebDAV read. The other tests use an empty terminal set (so a
        // 503 retries), leaving THIS fast-fail invariant unpinned. Assert both the single attempt and
        // the resolve_url → Unavailable mapping.
        use crate::provider::DebridProvider as _;
        let (url, count) =
            spawn_counting("HTTP/1.1 503 Service Unavailable", r#"{"error":"down"}"#).await;
        let client = RealDebridClient::new("fake".to_string())
            .unwrap()
            .with_base_url(url);
        let loc = crate::provider::FileLocator {
            hash: "h".to_string(),
            torrent_id: "t".to_string(),
            file_id: 0,
            file_path: "/x.mkv".to_string(),
            link: Some("https://real-debrid.com/d/abc".to_string()),
        };
        assert!(
            matches!(
                client.resolve_url(&loc).await,
                Err(crate::error::AppError::Unavailable)
            ),
            "a 5xx on unrestrict must map to Unavailable (→ instant repair)"
        );
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a 5xx on unrestrict is terminal — exactly one attempt, not the 3-attempt budget"
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
