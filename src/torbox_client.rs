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
    // Defensive decode like every other TorBox field: `#[serde(default)]` alone covers a MISSING
    // key but a present `null` or a stringized number would still fail the whole `Envelope<TbCreate>`
    // decode. `null_to_default` tolerates both, decoding an unusable id to 0; `add_magnet_raw` then
    // treats a non-positive `torrent_id` as "no usable id" and recovers the real torrent by infohash
    // (rather than recording an unusable "0" that would never resolve and get reaped/blacklisted).
    #[serde(default, deserialize_with = "null_to_default")]
    torrent_id: i64,
    // Returned by createtorrent but not used (we derive the canonical torrent later).
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "null_to_default")]
    hash: String,
}

/// Normalised status: a finished download (cached OR Inactive/uncached but owned) maps to
/// "downloaded" so it appears in the library and re-acquires on playback; otherwise the raw
/// download_state (e.g. "downloading") is kept so the scan loop excludes not-yet-ready items.
///
/// We deliberately do NOT map TorBox states to Real-Debrid's terminal vocabulary
/// (`magnet_error`/`dead`/`error`/`virus`, which `observe` instant-reaps). TorBox's libtorrent-style
/// states (`downloading`/`stalled`/`stalled (no seeds)`/`metaDL`/…) are RECOVERABLE, not
/// unambiguously terminal — a no-seeds torrent can resume when seeds return — so a "dead" mapping
/// would risk a false reap. A genuinely-stuck TorBox torrent is instead reaped by `observe`'s
/// dead-timeout / stall-timeout (a latency difference vs RD, by design, not a leak).
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

/// Map a decoded `mylist` into canonical torrents, DROPPING any entry with an unusable identity
/// (id ≤ 0 or empty hash). `null_to_default` defaults a null/missing/type-mismatched id→0 and
/// hash→"" rather than failing the whole list decode; such an entry can't be acted on (a delete
/// against id "0", or a hash-keyed lookup on ""), so skip it instead of surfacing a phantom torrent.
/// The filter is `id > 0` (not `id != 0`) to match `delete_torrent_raw`'s own `> 0` guard: a
/// (garbage) negative id would otherwise surface as an owned torrent that can never be deleted.
/// A real TorBox torrent always has a positive id and a non-empty hash.
fn map_listed_torrents(raw: &[TbTorrent]) -> Vec<Torrent> {
    let mapped: Vec<Torrent> = raw
        .iter()
        .filter(|t| t.id > 0 && !t.hash.is_empty())
        .map(to_torrent)
        .collect();
    // Surface a count when entries are dropped: normally zero, but if TorBox ever changed the wire
    // shape of `id`/`hash` (e.g. stringized id) `null_to_default` would zero them and the WHOLE
    // library would silently vanish here — a `debug!` makes that diagnosable rather than mysterious.
    let dropped = raw.len() - mapped.len();
    if dropped > 0 {
        debug!(
            "mylist: dropped {} of {} entries with an unusable identity (id 0 or empty hash)",
            dropped,
            raw.len()
        );
    }
    mapped
}

/// Drive offset/limit pagination: call `fetch_page(offset)` for `offset = 0, limit, 2*limit, …`,
/// accumulating pages until an EMPTY page signals the natural end, or `max_pages` is reached (a
/// defensive backstop, e.g. against an API that ignores `offset` and would otherwise loop forever).
/// Any page error propagates.
///
/// Termination is on an EMPTY page, NOT merely a short one (mirroring the RD client's
/// `get_torrents`): a server that returns fewer than `limit` items on a non-final page — a
/// short-but-not-last page — must not be mistaken for end-of-list, or the listing truncates and
/// downstream treats the dropped-page titles as lapsed → a re-acquisition storm + duplicate adds.
/// This costs exactly one extra (empty terminator) fetch per complete listing.
///
/// Returns `(items, hit_cap)`. `hit_cap` is `true` when the loop stopped because it reached
/// `max_pages` while the FINAL page was still full — i.e. more pages likely remain and `items` is
/// almost certainly TRUNCATED. The caller MUST treat that as a failure rather than an `Ok`, because
/// downstream treats an owned title absent from the listing as lapsed → re-acquire, so a truncated
/// listing would cause a re-acquisition storm + duplicate adds (this mirrors the RD client's
/// `get_torrents`, which fails rather than returns a partial listing at its own cap).
async fn paginate<T, E, F, Fut>(
    limit: usize,
    max_pages: usize,
    mut fetch_page: F,
) -> Result<(Vec<T>, bool), E>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<T>, E>>,
{
    let mut all: Vec<T> = Vec::new();
    let mut hit_cap = true; // cleared only if we reach a natural (empty) end
    for page in 0..max_pages {
        let items = fetch_page(page * limit).await?;
        let empty = items.is_empty();
        all.extend(items);
        if empty {
            hit_cap = false; // an empty page is the natural end — the listing is complete
            break;
        }
    }
    // If the loop exhausted `max_pages` with the final page still FULL, the account holds either
    // EXACTLY `max_pages * limit` items (complete) or more (genuinely truncated) — indistinguishable
    // without one more look. Probe the next offset: an empty probe means the listing was exactly
    // complete, so clear `hit_cap` rather than raising a false-truncation Err at the exact
    // page-count boundary. A non-empty probe confirms real truncation (its items are folded in but
    // `hit_cap` stays set, so the caller still fails the tick). This only ever fires on an account
    // at/above the defensive `max_pages` backstop, so it costs nothing in normal operation.
    if hit_cap {
        let probe = fetch_page(max_pages * limit).await?;
        if probe.is_empty() {
            hit_cap = false;
        } else {
            all.extend(probe);
        }
    }
    Ok((all, hit_cap))
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
/// evict the oldest entries. Called from `cache_put` when the cache exceeds the bound, so the cache
/// is self-limiting (and expired entries are TTL-checked on read in `cache_get`) — no external sweep.
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
    crate::provider::is_retryable_5xx(status)
}

/// Whether a `requestdl` failure is a clearly-PERMANENT auth/permission error (401/403) that a
/// re-add-by-hash cannot fix, so `resolve_locator` surfaces it as `Http` (hard read failure) rather
/// than `Unavailable` (trigger a pointless repair on every read). Everything else — a 5xx, a 404
/// (a stale torrent_id that re-add can recover), or a statusless transport error — is treated as
/// available-later (`Unavailable`) so the read path repairs.
fn requestdl_status_is_permanent(status: Option<reqwest::StatusCode>) -> bool {
    matches!(
        status,
        Some(s) if s == reqwest::StatusCode::UNAUTHORIZED || s == reqwest::StatusCode::FORBIDDEN
    )
}

/// Whether an `add_magnet_raw` (createtorrent) failure definitively is NOT a "download already
/// queued" duplicate, so the recovery list-fetch can be skipped: a non-400 CLIENT error (401/403
/// auth, 404) plainly failed and a list-fetch would fail the same way. Everything else still attempts
/// recovery — a 400 (the dup signal); any 5xx (a soft enveloped `success:false` surfaces as a
/// synthetic 502, indistinguishable from a real outage, so we don't risk dropping a dup signal); and
/// a statusless transport error (cheap best-effort).
fn add_error_skips_recovery(status: Option<reqwest::StatusCode>) -> bool {
    status.is_some_and(|s| s.is_client_error() && s != reqwest::StatusCode::BAD_REQUEST)
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
    crate::provider::synthetic_bad_gateway(b"torbox request failed")
}

struct CachedUrl {
    url: String,
    at: Instant,
}

pub struct TorBoxClient {
    client: reqwest::Client,
    api_key: String,
    // API base URL. Always `TORBOX_BASE` in production; overridable only in tests (`with_base`) so
    // the HTTP methods can be pointed at a local mock server.
    base: String,
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
            // Bound the connect phase separately (see rd_client) so a black-holing endpoint fails
            // fast rather than waiting out the full request timeout. Healthy connects are sub-second.
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AppError::Config(format!("Failed to build TorBox HTTP client: {}", e)))?;
        Ok(Self {
            client,
            api_key,
            base: TORBOX_BASE.to_string(),
            rate_limiter: Arc::new(AdaptiveRateLimiter::new()),
            resolve_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Test-only constructor that points the HTTP methods at a mock server base URL.
    #[cfg(test)]
    fn with_base(api_key: String, base: String) -> Self {
        let mut c = Self::new(api_key).unwrap();
        c.base = base;
        c
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

    /// Feed TorBox's advertised rate-limit window into the limiter so it paces proactively
    /// (and never trips a 429). Called on every response — including 429s and 200s, since
    /// TorBox sends the headers on both.
    async fn observe_rate_headers(&self, resp: &reqwest::Response) {
        if let Some((remaining, reset)) = parse_rate_headers(resp.headers()) {
            self.rate_limiter.observe_rate_limit(remaining, reset).await;
        }
    }

    /// Redact the account API key from a string before logging. A TorBox error body can echo the
    /// token-bearing `requestdl` URL (`?token=<key>`), so log the detail but never the secret.
    fn redact(&self, s: &str) -> String {
        if self.api_key.is_empty() {
            s.to_string()
        } else {
            s.replace(&self.api_key, "<token>")
        }
    }

    /// Send a request, rate-limited with 429/transient retry, returning the envelope's `data`.
    /// Synthesises a Bad Gateway reqwest error when `success` is false or `data` is missing.
    /// Uses the default 6-attempt budget for background/bulk callers (mylist/createtorrent/…).
    async fn send_data<T, F>(&self, make: F) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        self.send_data_n(make, 6).await
    }

    /// `send_data` with an explicit attempt budget. The synchronous on-read playback resolve
    /// (`resolve_locator`) passes a SMALL budget (3) so a black-holing endpoint (packets dropped →
    /// repeated 10s connect timeouts) fails over to instant repair in ~30s rather than ~60s — the
    /// same fast-fail rationale as the RD client's 3-attempt `unrestrict_link` read path.
    async fn send_data_n<T, F>(&self, make: F, max_attempts: u32) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
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
                        // Small backoff before retrying a statusless transport error (connection
                        // refused/reset) — mirrors the RD client; the rate limiter's baseline spacing
                        // alone would let a fast-failing endpoint spin the budget in a tight loop.
                        tokio::time::sleep(Duration::from_millis((100 * attempt as u64).min(1_000)))
                            .await;
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
            // Retry transient 5xx (500/502/503/504) with a short backoff before giving up.
            if is_transient_status(resp.status()) && attempt < max_attempts {
                warn!("TorBox {} (attempt {}), retrying", resp.status(), attempt);
                tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                continue;
            }
            let status = resp.status();
            if let Err(e) = resp.error_for_status_ref() {
                // Surface TorBox's error detail (e.g. why createtorrent 400s) before discarding it.
                let body = resp.text().await.unwrap_or_default();
                warn!("TorBox {} error body: {:.300}", status, self.redact(&body));
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
                    // Redact `detail` before logging: `send_data::<String>` serves `requestdl` and a
                    // TorBox error body can echo the token-bearing URL (`?token=<key>`), so the
                    // API-controlled `detail` string is NOT assumed safe (matches the redaction the
                    // `error_for_status_ref` branch above already applies).
                    warn!(
                        "TorBox response not success: {:?} (body {} bytes, redacted)",
                        env.detail.as_deref().map(|d| self.redact(d)),
                        text.len()
                    );
                }
                Err(e) => {
                    warn!(
                        "TorBox decode failed: {} (body {} bytes, redacted)",
                        e,
                        text.len()
                    );
                    // A truncated/partial 200 body is transient — retry rather than discard the whole
                    // call on a one-off hiccup (like the RD client, which also retries a decode blip;
                    // without this a mid-body glitch fails the entire mylist tick). NOTE the deliberate
                    // difference: this retries across the FULL budget, whereas RD's `unrestrict_link`
                    // caps decode retries at 2 because it runs on the synchronous playback resolve path
                    // and must fast-fail to repair. A persistent schema mismatch here simply exhausts
                    // the budget and returns the synthetic 502 below.
                    if attempt < max_attempts {
                        tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                        continue;
                    }
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
                        // Small backoff before retrying a statusless transport error (mirrors the RD
                        // client / send_data_n) so a fast-failing endpoint doesn't spin the budget.
                        tokio::time::sleep(Duration::from_millis((100 * attempt as u64).min(1_000)))
                            .await;
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
                warn!("TorBox {} error body: {:.300}", status, self.redact(&body));
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
                    // Redact `detail` before logging: a TorBox error body can echo the token-bearing
                    // URL, so the API-controlled `detail` string is scrubbed like the
                    // `error_for_status_ref` branch above (never assumed secret-free).
                    warn!(
                        "TorBox response not success: {:?} (body {} bytes, redacted)",
                        env.detail.as_deref().map(|d| self.redact(d)),
                        text.len()
                    );
                }
                Err(e) => {
                    warn!(
                        "TorBox decode failed: {} (body {} bytes, redacted)",
                        e,
                        text.len()
                    );
                    // A truncated/partial 200 body is transient — retry rather than discard the whole
                    // call on a one-off hiccup (symmetry with send_data and the RD client's
                    // decode-retry). A persistent schema mismatch exhausts the budget and returns the
                    // synthetic 502 below.
                    if attempt < max_attempts {
                        tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                        continue;
                    }
                }
            }
            break;
        }
        Err(synthetic_bad_gateway())
    }

    pub async fn list_torrents_raw(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        // TorBox `/torrents/mylist` returns at most `limit` torrents (standard page = 1000; up to
        // 10000 supported) and accepts `offset`. A single unbounded fetch would SILENTLY TRUNCATE
        // the library for accounts with >1000 torrents — and downstream treats an owned title absent
        // from the listing as lapsed → re-acquire, so truncation would cause a re-acquisition storm +
        // duplicate adds. Paginate until an EMPTY page, mirroring the RD client's `get_torrents`
        // (a short-but-non-final page must NOT be mistaken for end-of-list — see `paginate`).
        const PAGE: usize = 1000;
        const MAX_PAGES: usize = 50; // 50k-torrent backstop against an offset-ignoring API
        let (raw, hit_cap) = paginate(PAGE, MAX_PAGES, |offset| {
            let url = format!(
                "{}/torrents/mylist?bypass_cache=true&limit={}&offset={}",
                self.base, PAGE, offset
            );
            async move {
                self.send_data::<Vec<TbTorrent>, _>(|| self.client.get(&url))
                    .await
            }
        })
        .await?;
        if hit_cap {
            // We exhausted MAX_PAGES with a full final page → the listing is almost certainly
            // truncated. FAIL rather than return a partial Ok (which downstream would read as a
            // shrunken library → mass re-acquire + duplicate adds), exactly as RD's `get_torrents`
            // does at its cap. An offset-ignoring API also lands here (every page full) instead of
            // silently returning duplicated/partial data.
            warn!(
                "TorBox mylist pagination hit the {}-page cap (~{} torrents) with a full final page — \
                 refusing a possibly-truncated listing",
                MAX_PAGES,
                raw.len()
            );
            return Err(synthetic_bad_gateway());
        }
        Ok(map_listed_torrents(&raw))
    }

    pub async fn torrent_info_raw(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        let url = format!("{}/torrents/mylist?id={}&bypass_cache=true", self.base, id);
        let raw: TbTorrent = self.send_data(|| self.client.get(&url)).await?;
        Ok(to_torrent_info(&raw))
    }

    pub async fn add_magnet_raw(
        &self,
        magnet: &str,
    ) -> Result<crate::rd_client::AddMagnetResponse, reqwest::Error> {
        let url = format!("{}/torrents/createtorrent", self.base);
        let magnet = magnet.to_string();
        let form_magnet = magnet.clone();
        match self
            .send_data::<TbCreate, _>(|| {
                let form = reqwest::multipart::Form::new().text("magnet", form_magnet.clone());
                self.client.post(&url).multipart(form)
            })
            .await
        {
            // A usable id (> 0) is a real add. A null/missing/mistyped `torrent_id` decodes to 0
            // (see `TbCreate`) — NOT a real id — so treat that success-with-no-id like an add error
            // and recover by infohash, rather than recording an unusable "0" torrent that never
            // resolves and is eventually reaped/blacklisted.
            Ok(created) if created.torrent_id > 0 => Ok(crate::rd_client::AddMagnetResponse {
                id: created.torrent_id.to_string(),
                uri: magnet,
            }),
            Ok(_) => match self.recover_existing_by_infohash(&magnet).await {
                Some(found) => Ok(found),
                // No usable id AND no match to recover — surface a synthetic 502 (the same signal
                // `send_data` uses for a soft failure) so the caller treats it as a failed add.
                None => Err(synthetic_bad_gateway()),
            },
            Err(e) => {
                // TorBox rejects an add for a torrent already in the account ("Download already
                // queued"), commonly as an HTTP 400. Recover idempotently by locating the existing
                // torrent by infohash rather than failing the acquisition. We must NOT regress the
                // old "recover on any error" behaviour for the dup case, but we CAN skip the recovery
                // list-fetch for an error that definitively is NOT a dup: a non-400 CLIENT error
                // (401/403 auth, 404) — the add plainly failed and the list-fetch would fail the same
                // way. Everything else still attempts recovery: a 400 (the dup signal); a 5xx —
                // because a soft enveloped `success:false` is surfaced by `send_data` as a *synthetic
                // 502*, indistinguishable from a real outage, so we don't risk blocking a soft-failure
                // dup signal; and a statusless transport error (cheap best-effort, falls through to
                // the original error if no match).
                if !add_error_skips_recovery(e.status()) {
                    if let Some(found) = self.recover_existing_by_infohash(&magnet).await {
                        return Ok(found);
                    }
                }
                Err(e)
            }
        }
    }

    /// Locate a torrent already present in the account by the magnet's infohash — the shared
    /// recovery for a createtorrent that failed (dup / soft error) or returned no usable id. Returns
    /// the existing torrent's id, or `None` when the infohash can't be parsed, the list fetch fails,
    /// or nothing matches (the caller then surfaces the original error / a synthetic 502).
    async fn recover_existing_by_infohash(
        &self,
        magnet: &str,
    ) -> Option<crate::rd_client::AddMagnetResponse> {
        let hash = magnet_infohash(magnet)?;
        let list = self.list_torrents_raw().await.ok()?;
        let t = list.iter().find(|t| t.hash.eq_ignore_ascii_case(&hash))?;
        info!(
            "TorBox createtorrent rejected/idless but torrent already present; reusing id {}",
            t.id
        );
        Some(crate::rd_client::AddMagnetResponse {
            id: t.id.clone(),
            uri: magnet.to_string(),
        })
    }

    pub async fn delete_torrent_raw(&self, id: &str) -> Result<(), reqwest::Error> {
        let url = format!("{}/torrents/controltorrent", self.base);
        // Refuse to act on a malformed OR semantically-invalid id rather than defaulting to 0, which
        // would issue a delete against an unintended torrent and report success. `map_listed_torrents`
        // already drops id-0 entries, but enforce the `> 0` invariant locally so a future caller that
        // passes an unvalidated "0"/negative id can never delete against `torrent_id: 0`.
        let torrent_id: i64 = id.parse().map_err(|_| synthetic_bad_gateway())?;
        if torrent_id <= 0 {
            return Err(synthetic_bad_gateway());
        }
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
            self.base, self.api_key, loc.torrent_id, loc.file_id
        );
        // Bounded budget (3): this is the synchronous on-read resolve, so a wedged/black-holing
        // endpoint must fail over to instant repair fast rather than run the full 6-attempt loop
        // (mirrors the RD client's fast-fail `unrestrict_link` read path).
        match self
            .send_data_n::<String, _>(|| self.client.get(&url), 3)
            .await
        {
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
                if requestdl_status_is_permanent(e.status()) {
                    debug!(
                        "TorBox requestdl auth/permission error ({:?}) for torrent {} file {}",
                        e.status(),
                        loc.torrent_id,
                        loc.file_id
                    );
                    Err(AppError::Http(e))
                } else {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delete_torrent_raw_refuses_a_non_positive_or_malformed_id() {
        // The "won't act on an unusable id" guarantee must hold locally: a non-numeric, "0", or
        // negative id must error WITHOUT issuing a delete (which would hit torrent_id: 0 / a bogus id).
        let client = TorBoxClient::new("fake".to_string()).unwrap();
        for id in ["", "abc", "0", "-5", "  "] {
            assert!(
                client.delete_torrent_raw(id).await.is_err(),
                "delete_torrent_raw({id:?}) must refuse an unusable id"
            );
        }
    }

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
    fn requestdl_status_is_permanent_only_for_auth_failures() {
        use reqwest::StatusCode;
        // 401/403 are permanent → resolve_locator surfaces Http (no pointless repair).
        assert!(requestdl_status_is_permanent(Some(
            StatusCode::UNAUTHORIZED
        )));
        assert!(requestdl_status_is_permanent(Some(StatusCode::FORBIDDEN)));
        // A 5xx, a 404 (stale torrent_id, recoverable), and a statusless transport error are all
        // "available later" → Unavailable → the read path repairs.
        assert!(!requestdl_status_is_permanent(Some(
            StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!requestdl_status_is_permanent(Some(
            StatusCode::BAD_GATEWAY
        )));
        assert!(!requestdl_status_is_permanent(Some(StatusCode::NOT_FOUND)));
        assert!(!requestdl_status_is_permanent(None));
    }

    #[test]
    fn add_error_skips_recovery_only_for_non_400_client_errors() {
        use reqwest::StatusCode;
        // Non-400 client errors (401/403 auth, 404) plainly failed and are NOT a dup → skip recovery.
        assert!(add_error_skips_recovery(Some(StatusCode::UNAUTHORIZED)));
        assert!(add_error_skips_recovery(Some(StatusCode::FORBIDDEN)));
        assert!(add_error_skips_recovery(Some(StatusCode::NOT_FOUND)));
        // A 400 is the "download already queued" dup signal → MUST attempt recovery.
        assert!(!add_error_skips_recovery(Some(StatusCode::BAD_REQUEST)));
        // A 5xx (possibly a synthetic 502 masking a soft success:false dup) and a statusless
        // transport error still attempt recovery — never risk dropping a dup signal.
        assert!(!add_error_skips_recovery(Some(
            StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!add_error_skips_recovery(Some(StatusCode::BAD_GATEWAY)));
        assert!(!add_error_skips_recovery(None));
    }

    #[test]
    fn redact_scrubs_the_api_key_from_a_logged_body() {
        // A TorBox error body can echo the token-bearing requestdl URL; the redaction must remove the
        // key before it reaches a log line (the other branches already log only lengths).
        let c = TorBoxClient::new("SECRET123".to_string()).unwrap();
        let body = r#"{"error":"bad request to /requestdl?token=SECRET123&id=5"}"#;
        let red = c.redact(body);
        assert!(
            !red.contains("SECRET123"),
            "the api key must be scrubbed: {red}"
        );
        assert!(red.contains("<token>"));
        // A body that doesn't contain the key is unchanged.
        assert_eq!(c.redact("plain detail"), "plain detail");
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

    #[tokio::test]
    async fn paginate_fetches_until_an_empty_page() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        // Two FULL pages (limit=3) then a short page then the empty terminator → 7 items total,
        // 4 fetches. Termination is on the EMPTY page, not the short one, so a short-but-non-final
        // page can never truncate the listing.
        let r: Result<(Vec<u32>, bool), &str> = paginate(3, 50, |offset| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(match offset {
                    0 => vec![1, 2, 3],
                    3 => vec![4, 5, 6],
                    6 => vec![7], // short — continue past it
                    _ => vec![],  // empty → stop
                })
            }
        })
        .await;
        let (items, hit_cap) = r.unwrap();
        assert_eq!(items, vec![1, 2, 3, 4, 5, 6, 7]);
        assert!(!hit_cap, "a natural empty-page end is NOT a truncation");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "continues past the short page and stops on the empty terminator"
        );
    }

    #[tokio::test]
    async fn paginate_does_not_truncate_on_a_short_non_final_page() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        // A SHORT but NON-FINAL page (TorBox returns fewer than `limit` before the true end) must
        // NOT be mistaken for end-of-list: pagination continues until an EMPTY page, mirroring the
        // RD client. Here offset 0 is short (2 < 3) yet more items follow; stopping there would
        // silently drop items 3..6 → a truncated library → re-acquire storm + duplicate adds.
        let r: Result<(Vec<u32>, bool), &str> = paginate(3, 50, |offset| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(match offset {
                    0 => vec![1, 2],    // short, but NOT the end
                    3 => vec![3, 4, 5], // full
                    6 => vec![6],       // short again
                    _ => vec![],        // empty → true end
                })
            }
        })
        .await;
        let (items, hit_cap) = r.unwrap();
        assert_eq!(
            items,
            vec![1, 2, 3, 4, 5, 6],
            "a short non-final page must not truncate the listing"
        );
        assert!(
            !hit_cap,
            "reaching the empty terminator is a complete, non-truncated listing"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "paginates past every short page until the empty terminator"
        );
    }

    #[tokio::test]
    async fn paginate_stops_on_first_empty_page_and_propagates_errors() {
        // An immediate empty page (len 0 < limit) ends pagination after one call (not truncated).
        let r: Result<(Vec<u32>, bool), &str> =
            paginate(3, 50, |_| async move { Ok(Vec::<u32>::new()) }).await;
        let (items, hit_cap) = r.unwrap();
        assert_eq!(items, Vec::<u32>::new());
        assert!(!hit_cap);
        // A page error propagates rather than returning a partial Ok (no truncated-library re-acquire).
        let r: Result<(Vec<u32>, bool), &str> = paginate(3, 50, |offset| async move {
            if offset == 0 {
                Ok(vec![1, 2, 3])
            } else {
                Err("boom")
            }
        })
        .await;
        assert_eq!(r, Err("boom"));
    }

    #[tokio::test]
    async fn paginate_max_pages_backstops_and_flags_truncation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        // Every page is FULL (an API ignoring `offset`, or a >cap library) → bounded by max_pages
        // (+ one terminator probe), never infinite, AND `hit_cap` is set so the caller refuses the
        // truncated listing rather than returning a partial Ok (which would look like a shrunken
        // library → re-acquire storm). The probe also comes back full → truncation confirmed.
        let r: Result<(Vec<u32>, bool), &str> = paginate(2, 4, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(vec![0u32, 1]) }
        })
        .await;
        let (items, hit_cap) = r.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            5,
            "capped at max_pages + one terminator probe"
        );
        assert_eq!(items.len(), 10);
        assert!(
            hit_cap,
            "a full final page AND a non-empty probe must flag truncation (caller fails)"
        );
    }

    #[tokio::test]
    async fn paginate_exact_boundary_is_not_truncated() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        // The account holds EXACTLY max_pages*limit (4*2 = 8) items: pages 0..4 are all full, and the
        // terminator probe at offset 8 is empty → the listing is complete, NOT truncated. Without the
        // probe this exact multiple would raise a false-truncation Err (re-acquire storm).
        let r: Result<(Vec<u32>, bool), &str> = paginate(2, 4, |offset| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if offset < 8 {
                    Ok(vec![0u32, 1])
                } else {
                    Ok(vec![]) // terminator: no more items
                }
            }
        })
        .await;
        let (items, hit_cap) = r.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            5,
            "4 full pages + one empty terminator probe"
        );
        assert_eq!(items.len(), 8);
        assert!(
            !hit_cap,
            "an exactly-complete listing at the cap must NOT be flagged as truncated"
        );
    }

    #[test]
    fn tbcreate_tolerates_null_or_mistyped_torrent_id() {
        // createtorrent's torrent_id is decoded defensively (null_to_default), so a present `null`
        // or a wrong type defaults to 0 rather than failing the whole Envelope<TbCreate> decode
        // (which every other TorBox field already tolerates).
        let v: TbCreate = serde_json::from_str(r#"{"torrent_id": null, "hash": null}"#).unwrap();
        assert_eq!(v.torrent_id, 0);
        let v: TbCreate = serde_json::from_str(r#"{"torrent_id": "99"}"#).unwrap();
        assert_eq!(
            v.torrent_id, 0,
            "a stringized id defaults rather than erroring"
        );
        let v: TbCreate = serde_json::from_str(r#"{"torrent_id": 42, "hash": "abc"}"#).unwrap();
        assert_eq!(v.torrent_id, 42);
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

    /// Spawn a local HTTP server that replies to every request with `status` (empty body) and
    /// counts the requests it saw. Serves up to 8 connections — enough for a full retry budget.
    async fn spawn_status_repeating(
        status: u16,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = counter.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..8 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 {} STATUS\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    status
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}/", addr), counter)
    }

    /// Spawn a local HTTP server that replies with `first` on the first connection then `second` on
    /// every subsequent connection (both `200 OK` JSON), counting the requests it saw.
    async fn spawn_json_200_sequence(
        first: &'static str,
        second: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = counter.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for i in 0..8 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let n = c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let body = if i == 0 { first } else { second };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = n; // silence unused in case of a single-connection run
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}/", addr), counter)
    }

    /// Spawn a local HTTP server that replies to successive connections with the given
    /// `(status, body)` pairs in order (JSON bodies). Serves exactly `responses.len()` connections.
    async fn spawn_response_sequence(responses: Vec<(u16, String)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 {} STATUS\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    status,
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn add_magnet_null_torrent_id_recovers_existing_by_infohash() {
        // createtorrent returns `success:true` but a null `torrent_id` (decodes to 0 — NOT a usable
        // id). Rather than record an unusable "0" torrent, add_magnet_raw must recover the real
        // torrent by infohash from mylist. (list_torrents_raw paginates: page 0 returns the torrent,
        // the empty page 1 terminates.)
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let magnet = format!("magnet:?xt=urn:btih:{}", hash);
        let base = spawn_response_sequence(vec![
            (
                200,
                r#"{"success":true,"data":{"torrent_id":null}}"#.to_string(),
            ),
            (200, format!(r#"{{"success":true,"data":[{{"id":42,"hash":"{hash}"}}]}}"#)),
            (200, r#"{"success":true,"data":[]}"#.to_string()),
        ])
        .await;
        let client = TorBoxClient::with_base("fake".to_string(), base);
        let r = client
            .add_magnet_raw(&magnet)
            .await
            .expect("a null torrent_id must recover the existing torrent, not fail");
        assert_eq!(r.id, "42", "must reuse the existing torrent's id, not record 0");
    }

    #[tokio::test]
    async fn add_magnet_dup_400_recovers_existing_by_infohash() {
        // TorBox rejects an add for an already-present torrent (commonly HTTP 400). add_magnet_raw
        // must recover by locating it via infohash in mylist and reuse its id, not fail the acquire.
        let hash = "89abcdef0123456789abcdef0123456789abcdef";
        let magnet = format!("magnet:?xt=urn:btih:{}", hash);
        let base = spawn_response_sequence(vec![
            (
                400,
                r#"{"success":false,"detail":"Download already queued"}"#.to_string(),
            ),
            (200, format!(r#"{{"success":true,"data":[{{"id":7,"hash":"{hash}"}}]}}"#)),
            (200, r#"{"success":true,"data":[]}"#.to_string()),
        ])
        .await;
        let client = TorBoxClient::with_base("fake".to_string(), base);
        let r = client
            .add_magnet_raw(&magnet)
            .await
            .expect("a dup-400 must recover the existing torrent");
        assert_eq!(r.id, "7", "must reuse the already-present torrent's id");
    }

    #[tokio::test]
    async fn send_data_retries_a_transient_decode_failure() {
        // A one-off truncated/garbage 200 body must be RETRIED (symmetry with the RD client), not
        // give up after a single decode failure — otherwise a transient hiccup discards the whole
        // mylist tick. First response is un-decodable; the second is valid → the call recovers.
        let (url, counter) = spawn_json_200_sequence(
            "{ this is not valid json",        // decode failure on attempt 1
            r#"{"success":true,"data":"ok"}"#, // valid on attempt 2
        )
        .await;
        let client = TorBoxClient::new("fake".to_string()).unwrap();
        let r: Result<String, _> = client.send_data(|| client.client.get(&url)).await;
        assert_eq!(
            r.unwrap(),
            "ok",
            "a transient decode failure is retried and recovers"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly one retry after the transient decode failure"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn send_data_final_attempt_429_surfaces_real_status_not_synthetic_502() {
        // A persistent 429 must, after the retry budget is exhausted, surface the REAL 429 status
        // (so a sustained throttle is not misattributed to a synthetic Bad Gateway contract
        // failure). `start_paused` auto-advances the rate-limiter back-off so the test is instant.
        let (url, counter) = spawn_status_repeating(429).await;
        let client = TorBoxClient::new("fake".to_string()).unwrap();
        let r: Result<serde_json::Value, _> = client.send_data(|| client.client.get(&url)).await;
        let err = r.expect_err("a persistent 429 must surface as an error");
        assert_eq!(
            err.status(),
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS),
            "the surfaced error must carry the real 429, not a synthetic 502"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            6,
            "a 429 must be retried for the full attempt budget before surfacing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn send_data_n_honours_a_smaller_budget() {
        // The on-read resolve path (`resolve_locator`) passes a small budget (3) so a persistently
        // failing/black-holing endpoint fails over to repair fast, rather than the 6-attempt default
        // background callers use. A persistent 503 must be retried only `max_attempts` times.
        let (url, counter) = spawn_status_repeating(503).await;
        let client = TorBoxClient::new("fake".to_string()).unwrap();
        let r: Result<serde_json::Value, _> =
            client.send_data_n(|| client.client.get(&url), 3).await;
        assert!(
            r.is_err(),
            "a persistent 503 surfaces an error after the budget"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the bounded budget (3) is honoured, not the default 6"
        );
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
    fn list_drops_entries_with_unusable_identity() {
        // A type-mismatched/absent id or hash defaults to 0/"" (null_to_default) — unusable, so it
        // must be dropped from the mapped list rather than surfaced as a phantom id-0 torrent.
        let bad = r#"{
            "id": "notanumber", "hash": 123, "name": "X",
            "download_finished": true, "download_state": "cached", "files": []
        }"#;
        // A NEGATIVE id (garbage the wire could theoretically carry) is likewise unusable —
        // `delete_torrent_raw` guards `id > 0`, so a `-5` torrent could never be deleted; it must be
        // dropped here rather than surfaced as a non-actionable phantom.
        let negative = r#"{
            "id": -5, "hash": "abc", "name": "Y",
            "download_finished": true, "download_state": "cached", "files": []
        }"#;
        let raw: Vec<TbTorrent> =
            serde_json::from_str(&format!("[{},{},{}]", MYLIST_ITEM, bad, negative)).unwrap();
        assert_eq!(raw.len(), 3, "all entries decode (lenient)");
        let mapped = map_listed_torrents(&raw);
        assert_eq!(
            mapped.len(),
            1,
            "the unusable (id ≤ 0 / empty hash) entries are dropped"
        );
        assert_ne!(mapped[0].id, "0");
        assert_ne!(mapped[0].id, "-5");
        assert!(!mapped[0].hash.is_empty());
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

    #[test]
    fn torrent_info_raw_decodes_a_single_object_envelope_and_maps_it() {
        // torrent_info_raw calls send_data::<TbTorrent>, i.e. it assumes `mylist?id=X` returns `data`
        // as a SINGLE object (unlike the list endpoint's array). Pin that decode+map path so a
        // regression (e.g. TorBox returning a one-element array here) is caught in a unit test rather
        // than only by the live lifecycle test.
        let env: Envelope<TbTorrent> =
            serde_json::from_str(&format!(r#"{{"success":true,"data":{}}}"#, MYLIST_ITEM)).unwrap();
        let t = env.data.expect("single-object data");
        let info = to_torrent_info(&t);
        assert_eq!(info.id, "35821241");
        assert!(!info.hash.is_empty());
        // A finished download normalises to "downloaded" (see tb_status).
        assert_eq!(info.status, "downloaded");
    }
}
