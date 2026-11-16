//! Trakt API client: device-flow OAuth + the SP2 read endpoints, mirroring the
//! `scraper.rs` shape (a `#[async_trait]` trait, a real impl holding `base_url` +
//! `reqwest::Client` + a shared rate limiter, pure free functions for request/JSON
//! building and parsing, and a `#[cfg(test)]` `MockTrakt`).

use crate::error::AppError;
use crate::ratelimit::AdaptiveRateLimiter;
use crate::vfs::MediaType;
use async_trait::async_trait;
use tracing::{debug, warn};

/// A movie or show identified by its TMDB id (a watchlist or playback entry). Also carries the
/// IMDB id Trakt provided (`tt…`), if present, so acquisition can prefer it over re-deriving from
/// TMDB's (sometimes-incomplete) `external_ids`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TraktItem {
    pub media_type: MediaType,
    pub tmdb_id: u64,
    pub imdb_id: Option<String>,
}

/// Device-flow code response (`POST /oauth/device/code`).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval: u64,   // seconds between polls
    pub expires_in: u64, // seconds until the code expires
}

// Manual `Debug` redacting `device_code` (the secret used to poll for the token) — a stray
// `debug!("{dc:?}")` must not leak it, matching `TraktConfig`/`FileLocator`. The `user_code` and
// verification URL are non-secret (shown to the operator), so they stay visible.
impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceCode")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_url", &self.verification_url)
            .field("interval", &self.interval)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// OAuth token response (device token / refresh). Caller maps to `store::TraktTokens` via
/// `expires_at = created_at + expires_in`.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct TraktTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64, // seconds
    pub created_at: u64, // unix epoch seconds
}

// Manual `Debug` redacting the access/refresh tokens (also covers `DeviceTokenPoll::Authorized`,
// whose derived `Debug` defers to this).
impl std::fmt::Debug for TraktTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraktTokenResponse")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// The authenticated user's identity (`GET /users/me`). `slug` is the stable Trakt URL slug
/// used as the per-user key for stored tokens + wanted rows; `username` is for display.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TraktUser {
    pub slug: String,
    pub username: String,
}

/// Whether a `refresh` failure is transient (the cycle is skipped, the account is not flagged
/// `needs_reenrolment`): a persistent 429 (`RateLimited`), an incoherent answer (`Upstream`), a
/// transport failure (no HTTP status) or a 5xx. A 4xx such as `invalid_grant` is a genuine auth
/// failure, and so is anything else.
pub(crate) fn refresh_error_is_transient(e: &AppError) -> bool {
    match e {
        AppError::RateLimited | AppError::Upstream(_) => true,
        AppError::Http(e) => e.status().is_none_or(|s| s.is_server_error()),
        _ => false,
    }
}

/// Single-poll outcome of `POST /oauth/device/token`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DeviceTokenPoll {
    #[default]
    Pending,
    Authorized(TraktTokenResponse),
    Denied,
    Expired,
    /// Any other terminal status, carried so the enrolment log names it: a 401/403 is usually a
    /// wrong `TRAKT_CLIENT_ID` / `TRAKT_CLIENT_SECRET` (the secret is first sent on this poll).
    Rejected(u16),
}

/// Per-show watched-episode set, for the removal finish-test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedShow {
    pub tmdb_id: u64,
    /// The show's **Trakt** numeric id (`show.ids.trakt`) — the key the `/shows/{id}/progress`
    /// endpoint takes (it does not accept a TMDB id). `None` if Trakt omitted it; the catch-up
    /// "behind?" check then conservatively includes the show. Distinct from `tmdb_id`.
    pub trakt_id: Option<u64>,
    /// The show's IMDB id (`show.ids.imdb`), if Trakt supplied it. Threaded into the catch-up
    /// wanted record so a catch-up-ONLY show (watched + behind, not watchlisted/in-progress) can
    /// still be scraped: Torrentio is IMDB-keyed, and TMDB's IMDB mapping is incomplete for some
    /// titles (e.g. a TVDB-only show), so without Trakt's id such a show would resolve to no IMDB
    /// id and be silently skipped. Mirrors `TraktItem.imdb_id`.
    pub imdb_id: Option<String>,
    pub watched_episodes: Vec<(u32, u32)>,
    /// When the user last watched any episode of this show, as a Unix timestamp (seconds). `None`
    /// if Trakt omitted/malformed `last_watched_at`. Drives the catch-up lookback window (SP2.5).
    pub last_watched_at: Option<i64>,
}

/// A user's watched-progress snapshot for one show (`GET /shows/{id}/progress/watched`). `behind`
/// is the precise catch-up gate: `true` iff there is at least one **aired** episode the user has not
/// watched (Trakt's `next_episode` is non-null), i.e. they are not caught up. `aired`/`completed`
/// are kept for logging. Specials and hidden seasons are excluded from the counts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShowProgress {
    pub aired: u32,
    pub completed: u32,
    pub behind: bool,
}

/// A user's watched history.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WatchedData {
    pub movies: Vec<u64>,
    pub shows: Vec<WatchedShow>,
}

#[async_trait]
pub trait TraktClient: Send + Sync {
    /// POST /oauth/device/code — start device flow.
    async fn device_code(&self) -> Result<DeviceCode, AppError>;
    /// POST /oauth/device/token — poll ONCE; returns the current outcome (caller loops on Pending).
    async fn poll_token(&self, device_code: &str) -> Result<DeviceTokenPoll, AppError>;
    /// POST /oauth/token (grant_type=refresh_token) — exchange a refresh token for fresh tokens.
    async fn refresh(&self, refresh_token: &str) -> Result<TraktTokenResponse, AppError>;
    /// GET /users/me — the authenticated user's slug + username (keys the per-user tokens).
    async fn me(&self, access_token: &str) -> Result<TraktUser, AppError>;
    /// GET /sync/watchlist/movies + /sync/watchlist/shows — the user's watchlisted movies+shows,
    /// EVERY page (see `TraktClientImpl::authed_get_list`); any page failing is an `Err`.
    async fn watchlist(&self, access_token: &str) -> Result<Vec<TraktItem>, AppError>;
    /// GET /sync/playback — movies + (show via episodes) the user is mid-watch, deduped by show tmdb.
    async fn in_progress(&self, access_token: &str) -> Result<Vec<TraktItem>, AppError>;
    /// GET /sync/watched/movies + /sync/watched/shows?extended=progress — watched movies + per-show
    /// watched episodes, EVERY page; any page failing is an `Err`.
    async fn watched(&self, access_token: &str) -> Result<WatchedData, AppError>;
    /// GET /shows/{trakt_id}/progress/watched — whether the user has unwatched *aired* episodes
    /// (the precise catch-up gate). `trakt_id` is the show's Trakt numeric id, not its TMDB id.
    async fn show_progress(
        &self,
        access_token: &str,
        trakt_id: u64,
    ) -> Result<ShowProgress, AppError>;
}

const DEFAULT_BASE_URL: &str = "https://api.trakt.tv";

/// Page size requested on every paginated Trakt list read. Since Trakt's 2026 API changes the
/// watchlist (2026-04-15, trakt-api discussion #681) and watched (2026-07-03, #775) endpoints
/// paginate BY DEFAULT and return only page 1 unless asked for more, so every page is followed.
/// 100 is the largest page `/sync/watched/shows?extended=progress` accepts (plain watched allows
/// 250; one uniform size keeps the reads simple — a few extra requests per sync are absorbed by the
/// rate limiter).
const PAGE_LIMIT: u32 = 100;

/// Defensive cap on how many pages ONE list read follows (`PAGE_LIMIT` × this = 20,000 items): a
/// garbled or hostile `X-Pagination-Page-Count` must not drive an unbounded request loop. A list
/// claiming more pages is an `Err` — never a silently truncated read.
const MAX_PAGES: u32 = 200;

/// Trakt's pagination headers (`X-Pagination-Page`, `-Page-Count`, `-Item-Count`), as far as they
/// are present and parse. A missing or garbled value is `None` — never guessed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Pagination {
    pub page: Option<u32>,
    pub page_count: Option<u32>,
    pub item_count: Option<u64>,
}

/// Read the pagination headers off a Trakt response. PURE + total: a missing, non-UTF-8 or
/// non-numeric header yields `None` for that field (the caller then treats the read as a single
/// page, exactly like an unpaginated endpoint such as `/sync/playback`).
pub(crate) fn parse_pagination(headers: &reqwest::header::HeaderMap) -> Pagination {
    fn num<T: std::str::FromStr>(headers: &reqwest::header::HeaderMap, name: &str) -> Option<T> {
        headers.get(name)?.to_str().ok()?.trim().parse().ok()
    }
    Pagination {
        page: num(headers, "x-pagination-page"),
        page_count: num(headers, "x-pagination-page-count"),
        item_count: num(headers, "x-pagination-item-count"),
    }
}

/// Check that page `requested` (≥ 2) of a list read is coherent with page 1. The list changing
/// between page fetches shifts items across page boundaries, so a title could be missed on BOTH
/// pages — for the watchlist that would read as "un-watchlisted" and could drive a Trigger-B
/// removal. This catches a change of the page count or item count (a net add or remove mid-read)
/// and a page answered under the wrong number; the read then fails and is retried next sync. A
/// header absent on either side is not compared. PURE.
///
/// A change that keeps both counts is invisible here. `authed_get_list` also fails a read in which
/// one item appears on two pages ([`repeated_list_item`]: the same list-item `id`, or on the
/// watched lists, which carry none, the same movie or show), which catches a reorder (an item moved
/// onto an already-read page pushes another onto the next one; on the watched lists, which come
/// back newest `last_watched_at` first, a watch moves its title to the top). Still undetected: an
/// item removed from an already-read page while another is added further on, between two page
/// fetches. Every later item then slides back one place, and the first item of the next page is
/// read on neither. Accepted residual risk: it needs a list over one page (100 items) and both
/// changes inside the sub-second gap between page requests; the reconcile right after the sync may
/// then remove the missed title's engine copies (Trigger B), and the next sync acquires it again.
pub(crate) fn check_page_consistency(
    requested: u32,
    first: &Pagination,
    this: &Pagination,
) -> Result<(), String> {
    if let Some(p) = this.page {
        if p != requested {
            return Err(format!("asked for page {requested}, got page {p}"));
        }
    }
    if let (Some(a), Some(b)) = (first.page_count, this.page_count) {
        if a != b {
            return Err(format!("page count changed mid-read ({a} → {b})"));
        }
    }
    if let (Some(a), Some(b)) = (first.item_count, this.item_count) {
        if a != b {
            return Err(format!("item count changed mid-read ({a} → {b})"));
        }
    }
    Ok(())
}

/// The first item key ([`list_item_key`]) on `page_items` that an earlier page of the same read
/// already carried, if any. A list entry (keyed by its own `id`, e.g. a watchlist entry) can appear
/// only once in a list, and so can a movie or show in a watched list (keyed by its Trakt id, since
/// watched items carry no `id`). Seeing one on two pages means the list shifted between the page
/// fetches, and with the item count unchanged some other item was then missed. An item with no key
/// is not compared. PURE.
pub(crate) fn repeated_list_item(
    seen: &std::collections::HashSet<ListItemKey>,
    page_items: &[serde_json::Value],
) -> Option<ListItemKey> {
    page_items
        .iter()
        .filter_map(list_item_key)
        .find(|key| seen.contains(key))
}

/// What identifies one item of a Trakt list read, for [`repeated_list_item`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ListItemKey {
    /// The list entry's own `id` (watchlist and playback items).
    Entry(u64),
    /// A watched movie's Trakt id.
    Movie(u64),
    /// A watched show's Trakt id.
    Show(u64),
}

impl std::fmt::Display for ListItemKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Entry(id) => write!(f, "list item {id}"),
            Self::Movie(id) => write!(f, "movie {id}"),
            Self::Show(id) => write!(f, "show {id}"),
        }
    }
}

/// The key of one list item: its numeric top-level `id` when it has one, otherwise (a watched
/// movie or show) its `movie.ids.trakt` / `show.ids.trakt`. The entry id comes first because
/// playback entries carry one each while several of them share a show. An item with neither, or
/// one carrying an `episode` (a playback entry, keyed only by its own `id`), has no key. PURE.
pub(crate) fn list_item_key(item: &serde_json::Value) -> Option<ListItemKey> {
    if let Some(id) = item.get("id").and_then(serde_json::Value::as_u64) {
        return Some(ListItemKey::Entry(id));
    }
    if item.get("episode").is_some() {
        return None;
    }
    let trakt_id = |kind: &str| item.get(kind)?.get("ids")?.get("trakt")?.as_u64();
    trakt_id("movie")
        .map(ListItemKey::Movie)
        .or_else(|| trakt_id("show").map(ListItemKey::Show))
}

pub struct TraktClientImpl {
    base_url: String,
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    limiter: AdaptiveRateLimiter,
}

impl TraktClientImpl {
    pub fn new(client_id: String, client_secret: String, http: reqwest::Client) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            client_id,
            client_secret,
            http,
            limiter: AdaptiveRateLimiter::new(),
        }
    }

    /// Builder override for the API base URL (test-only: point the client at a loopback/mock server).
    #[cfg(test)]
    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    /// Build a request carrying the common Trakt headers. Authed reads add `.bearer_auth(..)`.
    ///
    /// A `User-Agent` is mandatory: Trakt's API sits behind Cloudflare, which 403s requests with
    /// no UA, and `reqwest` does not set one by default. Trakt also asks API apps to send a
    /// descriptive UA identifying the integration.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base_url, path))
            .header("trakt-api-version", "2")
            .header("trakt-api-key", self.client_id.as_str())
            .header(
                reqwest::header::USER_AGENT,
                concat!("debridmoviemapper/", env!("CARGO_PKG_VERSION")),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
    }

    /// Limiter-paced send with bounded 429 retry. Returns the raw response so callers can inspect
    /// the status themselves (`poll_token` treats 400/410/418 as outcomes, not errors). Retries
    /// transient 5xx/transport errors as well as 429.
    async fn send_with_retry(
        &self,
        make_request: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, AppError> {
        self.send_with_retry_opts(make_request, true).await
    }

    /// As `send_with_retry`, but `retry_server_errors == false` retries ONLY 429 (a rate-limit is
    /// definitely-not-processed → safe to resend), NOT a 5xx/transport error. Used by the OAuth
    /// refresh grant: its refresh token is SINGLE-USE, so if Trakt processed the grant (rotating the
    /// token) and THEN the response 5xx'd or the socket dropped, a resend would double-spend the
    /// now-consumed token. Failing fast on the first 5xx/transport error avoids that resend; the
    /// callers then treat the failure as a skipped cycle, not a re-enrolment
    /// ([`refresh_error_is_transient`]). If Trakt did rotate the token, the next cycle's refresh
    /// gets a 4xx `invalid_grant` and flags the account then.
    async fn send_with_retry_opts(
        &self,
        make_request: impl Fn() -> reqwest::RequestBuilder,
        retry_server_errors: bool,
    ) -> Result<reqwest::Response, AppError> {
        const MAX_ATTEMPTS: u32 = 5;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            self.limiter.wait_for_token().await;
            let resp = match make_request().send().await {
                Ok(r) => r,
                Err(e) => {
                    // Retry a transient transport error (connection reset / DNS blip / timeout) with
                    // the same backoff as the 5xx branch — so a flaky network doesn't drop a whole
                    // Trakt sync tick that RD/TorBox (which both retry transport errors) would ride
                    // out. The URL is scrubbed (it can't carry the OAuth token, but stay consistent).
                    // Skipped for a non-idempotent request (the refresh grant) — see the doc above.
                    if retry_server_errors && attempt < MAX_ATTEMPTS {
                        let backoff_ms =
                            200u64.saturating_mul(1u64 << (attempt - 1).min(MAX_ATTEMPTS - 1));
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                    return Err(AppError::Http(e.without_url()));
                }
            };
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                self.limiter.record_throttle(retry_after).await;
                // A 429 is always safe to retry (the request was throttled, NOT processed).
                if attempt < MAX_ATTEMPTS {
                    continue;
                }
                // Out of attempts: return the response raw. Callers that call error_for_status()
                // turn it into Err; poll_token routes it through interpret_device_token.
                return Ok(resp);
            }
            // Retry transient server errors without signalling the rate limiter — a 5xx is not
            // a rate-limit event. Add a short exponential backoff (the limiter is NOT signalled
            // here, so otherwise the retries would fire only ~100ms apart and hammer a flapping
            // upstream). Skipped for a non-idempotent request (the refresh grant).
            if status.is_server_error() {
                if retry_server_errors && attempt < MAX_ATTEMPTS {
                    let backoff_ms =
                        200u64.saturating_mul(1u64 << (attempt - 1).min(MAX_ATTEMPTS - 1));
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    continue;
                }
                return Ok(resp);
            }
            self.limiter.record_success().await;
            return Ok(resp);
        }
    }

    /// Authed GET returning parsed JSON, erroring on any non-success status.
    async fn authed_get_json(
        &self,
        path: &str,
        access_token: &str,
    ) -> Result<serde_json::Value, AppError> {
        let resp = self
            .send_with_retry(|| {
                self.request(reqwest::Method::GET, path)
                    .bearer_auth(access_token)
            })
            .await?;
        let resp = resp.error_for_status().map_err(AppError::Http)?;
        resp.json::<serde_json::Value>()
            .await
            .map_err(AppError::Http)
    }

    /// Authed GET of a Trakt LIST endpoint, following EVERY page, returning the concatenated items
    /// as one JSON array (so the per-endpoint parsers stay page-agnostic).
    ///
    /// - `limit = Some(n)`: every page is requested as `page=N&limit=n`.
    /// - `limit = None`: page 1 is requested exactly as given (no `page`/`limit` — for an endpoint
    ///   that does not paginate today, e.g. `/sync/playback`); should the response nevertheless
    ///   carry a page count > 1, the rest is fetched as `page=N` at the server's default size.
    ///
    /// The page count comes from page 1's `X-Pagination-Page-Count`; a missing/garbled/zero value
    /// reads as a single page. SAFETY: any failure is an `Err` for the WHOLE read — a page's
    /// request/status/body failing, a non-array body, a page count above `MAX_PAGES`, or the list
    /// visibly changing mid-read (`check_page_consistency`, `repeated_list_item`; see the former
    /// for the one shift neither can see) — so the caller keeps its previous state rather than
    /// acting on a silently truncated list.
    async fn authed_get_list(
        &self,
        path: &str,
        params: &[(&str, &str)],
        access_token: &str,
        limit: Option<u32>,
    ) -> Result<serde_json::Value, AppError> {
        let mut items: Vec<serde_json::Value> = Vec::new();
        let mut seen_keys: std::collections::HashSet<ListItemKey> =
            std::collections::HashSet::new();
        let mut first = Pagination::default();
        let mut total_pages: u32 = 1;
        let mut page: u32 = 1;
        loop {
            let mut query: Vec<(String, String)> = params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            if limit.is_some() || page > 1 {
                query.push(("page".to_string(), page.to_string()));
            }
            if let Some(l) = limit {
                query.push(("limit".to_string(), l.to_string()));
            }
            let resp = self
                .send_with_retry(|| {
                    self.request(reqwest::Method::GET, path)
                        .query(&query)
                        .bearer_auth(access_token)
                })
                .await?;
            let resp = resp.error_for_status().map_err(AppError::Http)?;
            let info = parse_pagination(resp.headers());
            let body = resp
                .json::<serde_json::Value>()
                .await
                .map_err(AppError::Http)?;
            let serde_json::Value::Array(page_items) = body else {
                return Err(AppError::Upstream(format!(
                    "Trakt {path} page {page}: expected a JSON array"
                )));
            };
            let page_len = page_items.len();
            if let Some(key) = repeated_list_item(&seen_keys, &page_items) {
                return Err(AppError::Upstream(format!(
                    "Trakt {path} page {page}: {key} already read on an earlier page \
                     (the list changed mid-read)"
                )));
            }
            seen_keys.extend(page_items.iter().filter_map(list_item_key));
            items.extend(page_items);
            if page == 1 {
                first = info;
                total_pages = match info.page_count {
                    Some(c) if c > MAX_PAGES => {
                        return Err(AppError::Upstream(format!(
                            "Trakt {path}: {c} pages exceeds the {MAX_PAGES}-page cap"
                        )));
                    }
                    Some(c) => c.max(1),
                    None => {
                        // Read as one page (as specified for absent/garbled headers). A FULL page
                        // with no usable count is the one hint the list may be longer — say so.
                        if limit.is_some_and(|l| page_len >= l as usize) {
                            warn!(
                                "trakt: {} returned a full page ({} items) without a usable \
                                 X-Pagination-Page-Count; reading it as a single page",
                                path, page_len
                            );
                        }
                        1
                    }
                };
            } else {
                check_page_consistency(page, &first, &info).map_err(|why| {
                    AppError::Upstream(format!("Trakt {path} page {page}: {why}"))
                })?;
            }
            if page >= total_pages {
                break;
            }
            page += 1;
        }
        debug!(
            "trakt: {} — {} item(s) over {} page(s)",
            path,
            items.len(),
            total_pages
        );
        Ok(serde_json::Value::Array(items))
    }
}

#[async_trait]
impl TraktClient for TraktClientImpl {
    async fn device_code(&self) -> Result<DeviceCode, AppError> {
        let body = build_device_code_body(&self.client_id);
        let resp = self
            .send_with_retry(|| {
                self.request(reqwest::Method::POST, "/oauth/device/code")
                    .json(&body)
            })
            .await?;
        let resp = resp.error_for_status().map_err(AppError::Http)?;
        let v: serde_json::Value = resp.json().await.map_err(AppError::Http)?;
        Ok(parse_device_code(&v))
    }

    async fn poll_token(&self, device_code: &str) -> Result<DeviceTokenPoll, AppError> {
        let body = build_poll_token_body(device_code, &self.client_id, &self.client_secret);
        let resp = self
            .send_with_retry(|| {
                self.request(reqwest::Method::POST, "/oauth/device/token")
                    .json(&body)
            })
            .await?;
        let status = resp.status();
        if status.is_server_error() {
            // send_with_retry already retried the 5xx for its short (~3s) budget. A longer Trakt
            // outage is still transient, not an expired code: return Err so `poll_to_completion`
            // retries it on the poll cadence under its consecutive-error cap.
            return Err(AppError::Upstream(format!(
                "Trakt device-token poll returned {status}"
            )));
        }
        if status == reqwest::StatusCode::OK {
            // A 200 is the authorized response and MUST carry a token body. If the body fails to
            // read (a transient mid-stream blip), bubble the error up rather than return an
            // Authorized with EMPTY tokens (`unwrap_or(Null)` → parse_token_response → ""), which
            // would then fail `me("")` with a confusing 401.
            // `poll_to_completion` re-polls it as a transient error, but Trakt has already exchanged
            // the code, so the re-poll most likely gets a 409 (code already used) and the operator
            // is told to start a new enrolment: the lost grant can't be recovered.
            let v = resp
                .json::<serde_json::Value>()
                .await
                .map_err(AppError::Http)?;
            return Ok(interpret_device_token(status, &v));
        }
        // Any other status maps purely off the code, so a missing/non-JSON body is fine (Null).
        let v = resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null);
        Ok(interpret_device_token(status, &v))
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TraktTokenResponse, AppError> {
        let body = build_refresh_body(refresh_token, &self.client_id, &self.client_secret);
        // The refresh token is single-use: do NOT retry a 5xx/transport error (Trakt may have already
        // processed the grant and rotated the token) — only 429 is retried. See `send_with_retry_opts`.
        let resp = self
            .send_with_retry_opts(
                || {
                    self.request(reqwest::Method::POST, "/oauth/token")
                        .json(&body)
                },
                false,
            )
            .await?;
        // A persistent 429 (rate-limit) exhausted the retry budget and is returned raw here. It is a
        // throttle, NOT an auth failure — the grant was never processed, so the single-use refresh
        // token is untouched. Map it to `RateLimited` so `refresh_account` treats it as transient and
        // does NOT flag `needs_reenrolment`. That flag is only an accounts-page marker (background
        // sync keeps syncing a flagged account and clears it on the next success), but a spurious one
        // tells the operator to re-enrol an account that is fine. A genuine `invalid_grant` still
        // arrives as a 4xx via error_for_status.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AppError::RateLimited);
        }
        let resp = resp.error_for_status().map_err(AppError::Http)?;
        let v: serde_json::Value = resp.json().await.map_err(AppError::Http)?;
        Ok(parse_token_response(&v))
    }

    async fn me(&self, access_token: &str) -> Result<TraktUser, AppError> {
        let v = self.authed_get_json("/users/me", access_token).await?;
        Ok(parse_user(&v))
    }

    async fn watchlist(&self, access_token: &str) -> Result<Vec<TraktItem>, AppError> {
        // Paginated by default since 2026-04-15: only page 1 came back before, so a title beyond
        // it was never acquired and read as un-watchlisted (a Trigger-B removal risk).
        let movies = self
            .authed_get_list(
                "/sync/watchlist/movies",
                &[],
                access_token,
                Some(PAGE_LIMIT),
            )
            .await?;
        let shows = self
            .authed_get_list("/sync/watchlist/shows", &[], access_token, Some(PAGE_LIMIT))
            .await?;
        let mut out = parse_watchlist(&movies, MediaType::Movie);
        out.extend(parse_watchlist(&shows, MediaType::Show));
        Ok(out)
    }

    async fn in_progress(&self, access_token: &str) -> Result<Vec<TraktItem>, AppError> {
        // `/sync/playback` does not paginate (the live response carries no pagination headers), so
        // it is requested exactly as before; `authed_get_list` still follows pages should Trakt
        // start paginating it.
        let v = self
            .authed_get_list("/sync/playback", &[], access_token, None)
            .await?;
        Ok(parse_playback(&v))
    }

    async fn watched(&self, access_token: &str) -> Result<WatchedData, AppError> {
        // Both paginated by default since 2026-07-03 (100 per page). Watched SHOWS also lost their
        // `seasons`/`episodes` unless `extended=progress` is passed (`noseasons` is now a no-op), so
        // without it every show parsed to an EMPTY watched-episode set. The `extended=progress`
        // shape is the old one plus `seasons[].episodes[]` (`show.ids` still carries tmdb/trakt/imdb
        // and `last_watched_at` is unchanged), so `parse_watched_shows` reads it as-is; it caps at
        // 100 items per page (= `PAGE_LIMIT`). Specials (season 0) are omitted by Trakt.
        let movies = self
            .authed_get_list("/sync/watched/movies", &[], access_token, Some(PAGE_LIMIT))
            .await?;
        let shows = self
            .authed_get_list(
                "/sync/watched/shows",
                &[("extended", "progress")],
                access_token,
                Some(PAGE_LIMIT),
            )
            .await?;
        Ok(WatchedData {
            movies: parse_watched_movies(&movies),
            shows: parse_watched_shows(&shows),
        })
    }

    async fn show_progress(
        &self,
        access_token: &str,
        trakt_id: u64,
    ) -> Result<ShowProgress, AppError> {
        // Exclude specials and hidden seasons so `aired`/`completed`/`next_episode` reflect the
        // canonical run — a user who's watched every regular episode reads as caught up.
        let path = format!(
            "/shows/{}/progress/watched?hidden=false&specials=false&count_specials=false",
            trakt_id
        );
        let v = self.authed_get_json(&path, access_token).await?;
        Ok(parse_show_progress(&v))
    }
}

// --- Pure request/body builders -------------------------------------------------------------

/// `POST /oauth/device/code` body.
pub fn build_device_code_body(client_id: &str) -> serde_json::Value {
    serde_json::json!({ "client_id": client_id })
}

/// `POST /oauth/device/token` body.
pub fn build_poll_token_body(
    code: &str,
    client_id: &str,
    client_secret: &str,
) -> serde_json::Value {
    serde_json::json!({ "code": code, "client_id": client_id, "client_secret": client_secret })
}

/// `POST /oauth/token` (refresh) body.
pub fn build_refresh_body(
    refresh_token: &str,
    client_id: &str,
    client_secret: &str,
) -> serde_json::Value {
    serde_json::json!({
        "refresh_token": refresh_token,
        "client_id": client_id,
        "client_secret": client_secret,
        "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
        "grant_type": "refresh_token",
    })
}

// --- Pure JSON parsers ----------------------------------------------------------------------

/// Extract a `String` field (missing/non-string → empty).
fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Extract a `u64` field (missing/non-numeric → 0).
fn u64_field(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// The common `obj.ids.tmdb` numeric extraction.
fn tmdb_id_of(obj: &serde_json::Value) -> Option<u64> {
    obj.get("ids")
        .and_then(|i| i.get("tmdb"))
        .and_then(|t| t.as_u64())
}

/// The `obj.ids.trakt` numeric extraction (the id the `/shows/{id}/progress` endpoint takes).
fn trakt_id_of(obj: &serde_json::Value) -> Option<u64> {
    obj.get("ids")
        .and_then(|i| i.get("trakt"))
        .and_then(|t| t.as_u64())
}

/// The IMDB id (`tt…`) from a Trakt item's `ids` object, if present and non-empty.
fn imdb_id_of(obj: &serde_json::Value) -> Option<String> {
    obj.get("ids")
        .and_then(|i| i.get("imdb"))
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse the `POST /oauth/device/code` response.
pub fn parse_device_code(v: &serde_json::Value) -> DeviceCode {
    DeviceCode {
        device_code: str_field(v, "device_code"),
        user_code: str_field(v, "user_code"),
        verification_url: str_field(v, "verification_url"),
        interval: u64_field(v, "interval"),
        expires_in: u64_field(v, "expires_in"),
    }
}

/// Parse an OAuth token response (device token / refresh).
pub fn parse_token_response(v: &serde_json::Value) -> TraktTokenResponse {
    TraktTokenResponse {
        access_token: str_field(v, "access_token"),
        refresh_token: str_field(v, "refresh_token"),
        expires_in: u64_field(v, "expires_in"),
        created_at: u64_field(v, "created_at"),
    }
}

/// Parse a `GET /users/me` response into the user's slug + username. Reads `ids.slug`
/// (falling back to `username` when the slug is missing/empty) and `username`. Total/no-panic.
pub(crate) fn parse_user(v: &serde_json::Value) -> TraktUser {
    let username = str_field(v, "username");
    let slug = v
        .get("ids")
        .and_then(|i| i.get("slug"))
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| username.clone());
    TraktUser { slug, username }
}

/// Map a `POST /oauth/device/token` status (+ body) to a single-poll outcome.
///
/// Trakt device-token poll status semantics:
///   200 OK            -> authorized; the body carries the OAuth tokens.
///   400 Bad Request   -> "authorization pending" — user hasn't entered the code yet (keep polling).
///   404 Not Found     -> the device_code is invalid/unknown (terminal `Rejected(404)`).
///   409 Conflict      -> the code was already used: Trakt already exchanged it for tokens
///                        (terminal `Rejected(409)`).
///   410 Gone          -> the code expired.
///   418 I'm a teapot  -> the user explicitly denied the request.
///   429 Too Many Req  -> polling too fast; normally absorbed by the retry helper, mapped to
///                        Pending defensively so a stray 429 doesn't abort enrolment.
/// 404, 409, 401/403 (typically a wrong client id/secret, since the secret is first sent on this
/// poll) and any unlisted status are terminal `Rejected(status)`, so the caller's poll loop can't
/// spin forever and its error names the status instead of claiming the code expired.
/// A 5xx never reaches this from `TraktClientImpl::poll_token`, which returns it as a retryable
/// `Err` instead.
pub fn interpret_device_token(
    status: reqwest::StatusCode,
    body: &serde_json::Value,
) -> DeviceTokenPoll {
    match status.as_u16() {
        200 => DeviceTokenPoll::Authorized(parse_token_response(body)),
        400 | 429 => DeviceTokenPoll::Pending,
        418 => DeviceTokenPoll::Denied,
        410 => DeviceTokenPoll::Expired,
        other => DeviceTokenPoll::Rejected(other),
    }
}

/// Parse a `/sync/watchlist/{movies,shows}` array, setting `media_type` from the arg.
/// Each element carries a `movie` (resp. `show`) object with `ids.tmdb`; entries without a
/// numeric tmdb id are skipped.
pub fn parse_watchlist(v: &serde_json::Value, media_type: MediaType) -> Vec<TraktItem> {
    let key = match media_type {
        MediaType::Movie => "movie",
        MediaType::Show => "show",
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let obj = e.get(key)?;
            let tmdb_id = tmdb_id_of(obj)?;
            Some(TraktItem {
                media_type: media_type.clone(),
                tmdb_id,
                imdb_id: imdb_id_of(obj),
            })
        })
        .collect()
}

/// Parse a `/sync/playback` array into deduped in-progress items. `type == "movie"` →
/// `movie.ids.tmdb`; `type == "episode"` → `show.ids.tmdb` (so a show appears once even with
/// several in-progress episodes). Entries lacking a numeric tmdb are skipped.
pub fn parse_playback(v: &serde_json::Value) -> Vec<TraktItem> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    let mut out: Vec<TraktItem> = Vec::new();
    // Dedup in first-seen order via a set membership check (O(n)) rather than a linear
    // `Vec::contains` per item (O(n²)) — a show with many in-progress episodes collapses to one entry.
    let mut seen: std::collections::HashSet<TraktItem> = std::collections::HashSet::new();
    for e in arr {
        let item = match e.get("type").and_then(|t| t.as_str()) {
            Some("movie") => e.get("movie").and_then(|m| {
                Some(TraktItem {
                    media_type: MediaType::Movie,
                    tmdb_id: tmdb_id_of(m)?,
                    imdb_id: imdb_id_of(m),
                })
            }),
            Some("episode") => e.get("show").and_then(|s| {
                Some(TraktItem {
                    media_type: MediaType::Show,
                    tmdb_id: tmdb_id_of(s)?,
                    imdb_id: imdb_id_of(s),
                })
            }),
            _ => None,
        };
        if let Some(item) = item {
            if seen.insert(item.clone()) {
                out.push(item);
            }
        }
    }
    out
}

/// Parse a `/sync/watched/movies` array into TMDB ids (skipping non-numeric).
pub fn parse_watched_movies(v: &serde_json::Value) -> Vec<u64> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| e.get("movie").and_then(tmdb_id_of))
        .collect()
}

/// An RFC 3339 timestamp field (e.g. `last_watched_at`) as Unix seconds; missing/null/malformed →
/// `None`.
fn timestamp_field(v: &serde_json::Value, key: &str) -> Option<i64> {
    v.get(key)
        .and_then(|t| t.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp())
}

/// Parse a `/sync/watched/shows?extended=progress` array into per-show watched-episode
/// `(season, episode)` sets (the `seasons[].episodes[]` that only `extended=progress` returns since
/// 2026-07-03 — without it the sets are empty). Shows without a numeric tmdb, and seasons/episodes
/// without a numeric `number`, are skipped. When the user has reset the show's progress
/// (`reset_at` set — a re-watch), an episode whose `last_watched_at` predates the reset (or is
/// missing) is unwatched for progress, matching Trakt's own per-show progress.
pub fn parse_watched_shows(v: &serde_json::Value) -> Vec<WatchedShow> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let tmdb_id = e.get("show").and_then(tmdb_id_of)?;
            let trakt_id = e.get("show").and_then(trakt_id_of);
            let imdb_id = e.get("show").and_then(imdb_id_of);
            let reset_at = timestamp_field(e, "reset_at");
            let mut watched_episodes = Vec::new();
            if let Some(seasons) = e.get("seasons").and_then(|s| s.as_array()) {
                for season in seasons {
                    let Some(season_number) = season
                        .get("number")
                        .and_then(|n| n.as_u64())
                        .and_then(|n| u32::try_from(n).ok())
                    else {
                        continue;
                    };
                    let Some(episodes) = season.get("episodes").and_then(|ep| ep.as_array()) else {
                        continue;
                    };
                    for ep in episodes {
                        if let Some(reset) = reset_at {
                            if timestamp_field(ep, "last_watched_at").is_none_or(|t| t < reset) {
                                continue; // watched before the progress reset → unwatched now
                            }
                        }
                        if let Some(episode_number) = ep
                            .get("number")
                            .and_then(|n| n.as_u64())
                            .and_then(|n| u32::try_from(n).ok())
                        {
                            watched_episodes.push((season_number, episode_number));
                        }
                    }
                }
            }
            let last_watched_at = timestamp_field(e, "last_watched_at");
            Some(WatchedShow {
                tmdb_id,
                trakt_id,
                imdb_id,
                watched_episodes,
                last_watched_at,
            })
        })
        .collect()
}

/// Parse a `/shows/{id}/progress/watched` response. `behind` is taken from `next_episode` (Trakt's
/// authoritative "there is an aired episode to watch" signal — non-null ⇒ behind, explicit null ⇒
/// caught up); if the field is absent (malformed body) it falls back to `completed < aired`.
pub fn parse_show_progress(v: &serde_json::Value) -> ShowProgress {
    // Clamp rather than truncate (`as u32` on a pathological >u32::MAX value wraps, which could
    // invert the `completed < aired` fallback below) — matching `parse_watched_shows`' defensive
    // `u32::try_from`. Real episode counts are tiny, so the clamp never triggers in practice.
    let field = |name: &str| -> u32 {
        v.get(name)
            .and_then(|x| x.as_u64())
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .unwrap_or(0)
    };
    let aired = field("aired");
    let completed = field("completed");
    let behind = match v.get("next_episode") {
        Some(ne) if !ne.is_null() => true, // an aired episode awaits → behind
        Some(_) => false,                  // explicit null → caught up
        None => completed < aired,         // field missing → fall back to the counts
    };
    ShowProgress {
        aired,
        completed,
        behind,
    }
}

// --- Test-only mock -------------------------------------------------------------------------

/// Test-only client returning canned values (mirrors `MockScraper`). The `fail_reads`/
/// `fail_refresh`/`transient_refresh` flags and the `poll_errors_before_ok`/`me_errors_before_ok`
/// counters inject errors so callers (e.g. `sync_trakt`, `enrolment::poll_to_completion`) can
/// exercise their failure-handling paths; when none is set the canned values are returned
/// unchanged.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct MockTrakt {
    pub device_code: DeviceCode,
    pub poll: DeviceTokenPoll,
    pub token: TraktTokenResponse,
    pub user: TraktUser,
    pub watchlist: Vec<TraktItem>,
    pub in_progress: Vec<TraktItem>,
    pub watched: WatchedData,
    /// Canned `show_progress` responses keyed by Trakt show id. A show with no entry returns the
    /// default (`behind: false` = caught up), so a catch-up candidate is excluded unless set.
    pub progress: std::collections::HashMap<u64, ShowProgress>,
    /// When true, `watchlist`/`in_progress`/`watched` return `Err`.
    pub fail_reads: bool,
    /// When true, `refresh` returns `Err` (a non-transient one: an auth failure).
    pub fail_refresh: bool,
    /// When true, `refresh` returns a transient `Err` (an HTTP 503).
    pub transient_refresh: bool,
    /// Number of transient `poll_token` errors to inject before returning the canned `poll`
    /// (decrements on each call). Default 0 = never errors. Lets tests exercise the transient-retry
    /// path in `enrolment::poll_to_completion`.
    pub poll_errors_before_ok: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Number of transient `me` errors to inject before returning the canned `user` (decrements on
    /// each call). Default 0 = never errors. Lets tests exercise the `me()` retry after
    /// authorisation in `enrolment::poll_to_completion`.
    pub me_errors_before_ok: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

/// A `reqwest::Error` carrying `status`, built without any I/O.
#[cfg(test)]
pub(crate) fn http_status_error(status: u16) -> reqwest::Error {
    reqwest::Response::from(
        hyper::Response::builder()
            .status(status)
            .body(hyper::body::Bytes::new())
            .unwrap(),
    )
    .error_for_status()
    .unwrap_err()
}

#[cfg(test)]
impl MockTrakt {
    fn read_error() -> AppError {
        AppError::Config("mock trakt read failure".to_string())
    }
}

#[cfg(test)]
#[async_trait]
impl TraktClient for MockTrakt {
    async fn device_code(&self) -> Result<DeviceCode, AppError> {
        Ok(self.device_code.clone())
    }
    async fn poll_token(&self, _device_code: &str) -> Result<DeviceTokenPoll, AppError> {
        use std::sync::atomic::Ordering;
        if self.poll_errors_before_ok.load(Ordering::SeqCst) > 0 {
            self.poll_errors_before_ok.fetch_sub(1, Ordering::SeqCst);
            return Err(AppError::Config("mock transient poll error".to_string()));
        }
        Ok(self.poll.clone())
    }
    async fn refresh(&self, _refresh_token: &str) -> Result<TraktTokenResponse, AppError> {
        if self.fail_refresh {
            return Err(AppError::Config("mock trakt refresh failure".to_string()));
        }
        if self.transient_refresh {
            return Err(AppError::Http(http_status_error(503)));
        }
        Ok(self.token.clone())
    }
    async fn me(&self, _access_token: &str) -> Result<TraktUser, AppError> {
        use std::sync::atomic::Ordering;
        if self.fail_reads {
            return Err(Self::read_error());
        }
        if self.me_errors_before_ok.load(Ordering::SeqCst) > 0 {
            self.me_errors_before_ok.fetch_sub(1, Ordering::SeqCst);
            return Err(AppError::Upstream("mock transient me error".to_string()));
        }
        Ok(self.user.clone())
    }
    async fn watchlist(&self, _access_token: &str) -> Result<Vec<TraktItem>, AppError> {
        if self.fail_reads {
            return Err(Self::read_error());
        }
        Ok(self.watchlist.clone())
    }
    async fn in_progress(&self, _access_token: &str) -> Result<Vec<TraktItem>, AppError> {
        if self.fail_reads {
            return Err(Self::read_error());
        }
        Ok(self.in_progress.clone())
    }
    async fn watched(&self, _access_token: &str) -> Result<WatchedData, AppError> {
        if self.fail_reads {
            return Err(Self::read_error());
        }
        Ok(self.watched.clone())
    }
    async fn show_progress(
        &self,
        _access_token: &str,
        trakt_id: u64,
    ) -> Result<ShowProgress, AppError> {
        // Best-effort supplementary call: not gated on `fail_reads` (its failure is handled
        // gracefully by the caller). Returns the canned entry, or the caught-up default.
        Ok(self.progress.get(&trakt_id).cloned().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_device_code_body_shape() {
        assert_eq!(
            build_device_code_body("CID"),
            serde_json::json!({ "client_id": "CID" })
        );
    }

    #[test]
    fn build_poll_token_body_shape() {
        assert_eq!(
            build_poll_token_body("DCODE", "CID", "SECRET"),
            serde_json::json!({ "code": "DCODE", "client_id": "CID", "client_secret": "SECRET" })
        );
    }

    #[test]
    fn build_refresh_body_shape() {
        assert_eq!(
            build_refresh_body("RTOK", "CID", "SECRET"),
            serde_json::json!({
                "refresh_token": "RTOK",
                "client_id": "CID",
                "client_secret": "SECRET",
                "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
                "grant_type": "refresh_token"
            })
        );
    }

    #[test]
    fn parse_device_code_from_json() {
        let v = serde_json::json!({
            "device_code": "abc123",
            "user_code": "5055CC52",
            "verification_url": "https://trakt.tv/activate",
            "expires_in": 600,
            "interval": 5
        });
        assert_eq!(
            parse_device_code(&v),
            DeviceCode {
                device_code: "abc123".into(),
                user_code: "5055CC52".into(),
                verification_url: "https://trakt.tv/activate".into(),
                interval: 5,
                expires_in: 600,
            }
        );
    }

    #[test]
    fn parse_device_code_missing_fields_default() {
        assert_eq!(
            parse_device_code(&serde_json::json!({})),
            DeviceCode::default()
        );
    }

    #[test]
    fn parse_token_response_from_json() {
        let v = serde_json::json!({
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 7776000u64,
            "created_at": 1700000000u64,
            "token_type": "bearer",
            "scope": "public"
        });
        assert_eq!(
            parse_token_response(&v),
            TraktTokenResponse {
                access_token: "AT".into(),
                refresh_token: "RT".into(),
                expires_in: 7776000,
                created_at: 1700000000,
            }
        );
    }

    /// The manual `Debug` impl for `TraktTokenResponse` must redact the OAuth tokens — a stray
    /// `debug!("{resp:?}")` must never leak them. Also covers `DeviceTokenPoll::Authorized`, whose
    /// derived `Debug` defers to this manual impl (the embedding case). Guards against a future edit
    /// re-deriving `Debug` and silently re-exposing credentials.
    #[test]
    fn token_response_debug_redacts_secrets() {
        let resp = TraktTokenResponse {
            access_token: "SECRET_ACCESS_XYZ".into(),
            refresh_token: "SECRET_REFRESH_XYZ".into(),
            expires_in: 7776000,
            created_at: 1700000000,
        };
        let dbg = format!("{resp:?}");
        assert!(
            !dbg.contains("SECRET_ACCESS_XYZ"),
            "access token leaked: {dbg}"
        );
        assert!(
            !dbg.contains("SECRET_REFRESH_XYZ"),
            "refresh token leaked: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "expected redaction marker: {dbg}"
        );

        // Embedding case: the derived Debug for DeviceTokenPoll must defer to the manual redacting
        // impl, so a wrapped token response does not leak either.
        let poll = DeviceTokenPoll::Authorized(resp);
        let poll_dbg = format!("{poll:?}");
        assert!(
            !poll_dbg.contains("SECRET_ACCESS_XYZ"),
            "access token leaked through DeviceTokenPoll::Authorized: {poll_dbg}"
        );
        assert!(
            !poll_dbg.contains("SECRET_REFRESH_XYZ"),
            "refresh token leaked through DeviceTokenPoll::Authorized: {poll_dbg}"
        );
        assert!(
            poll_dbg.contains("<redacted>"),
            "expected redaction marker in wrapped poll: {poll_dbg}"
        );
    }

    /// The manual `Debug` impl for `DeviceCode` must redact the secret `device_code` (used to poll
    /// for the token) while keeping the non-secret `user_code`/`verification_url` visible (they're
    /// shown to the operator). Guards against a future re-derive leaking the polling secret.
    #[test]
    fn device_code_debug_redacts_device_code_only() {
        let dc = DeviceCode {
            device_code: "SECRET_DEVICE_XYZ".into(),
            user_code: "USERCODE123".into(),
            verification_url: "https://trakt.tv/activate".into(),
            interval: 5,
            expires_in: 600,
        };
        let dbg = format!("{dc:?}");
        assert!(
            !dbg.contains("SECRET_DEVICE_XYZ"),
            "device_code leaked: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "expected redaction marker: {dbg}"
        );
        // Non-secret fields must remain visible.
        assert!(
            dbg.contains("USERCODE123"),
            "user_code should be visible: {dbg}"
        );
        assert!(
            dbg.contains("https://trakt.tv/activate"),
            "verification_url should be visible: {dbg}"
        );
    }

    #[test]
    fn parse_user_reads_slug_and_username() {
        let v = serde_json::json!({
            "username": "Alice",
            "ids": { "slug": "alice-slug", "uuid": "x" }
        });
        assert_eq!(
            parse_user(&v),
            TraktUser {
                slug: "alice-slug".into(),
                username: "Alice".into()
            }
        );
    }

    #[test]
    fn parse_user_falls_back_to_username_when_slug_missing() {
        // slug missing entirely
        let v = serde_json::json!({ "username": "bob" });
        assert_eq!(
            parse_user(&v),
            TraktUser {
                slug: "bob".into(),
                username: "bob".into()
            }
        );
        // slug present but empty → still falls back to username
        let v2 = serde_json::json!({ "username": "carol", "ids": { "slug": "" } });
        assert_eq!(
            parse_user(&v2),
            TraktUser {
                slug: "carol".into(),
                username: "carol".into()
            }
        );
    }

    #[test]
    fn interpret_device_token_state_machine() {
        use reqwest::StatusCode;
        let token_body = serde_json::json!({
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 7776000u64,
            "created_at": 1700000000u64
        });
        // 200 → Authorized(parsed token)
        assert_eq!(
            interpret_device_token(StatusCode::OK, &token_body),
            DeviceTokenPoll::Authorized(TraktTokenResponse {
                access_token: "AT".into(),
                refresh_token: "RT".into(),
                expires_in: 7776000,
                created_at: 1700000000,
            })
        );
        let empty = serde_json::Value::Null;
        // 400 → Pending (authorization pending)
        assert_eq!(
            interpret_device_token(StatusCode::BAD_REQUEST, &empty),
            DeviceTokenPoll::Pending
        );
        // 418 → Denied
        assert_eq!(
            interpret_device_token(StatusCode::IM_A_TEAPOT, &empty),
            DeviceTokenPoll::Denied
        );
        // 410 → Expired
        assert_eq!(
            interpret_device_token(StatusCode::GONE, &empty),
            DeviceTokenPoll::Expired
        );
        // 404 (invalid code) / 409 (code already used) → Rejected: terminal, but NOT an expiry, so
        // the enrolment error names what happened instead of claiming the code expired.
        assert_eq!(
            interpret_device_token(StatusCode::NOT_FOUND, &empty),
            DeviceTokenPoll::Rejected(404)
        );
        assert_eq!(
            interpret_device_token(StatusCode::CONFLICT, &empty),
            DeviceTokenPoll::Rejected(409)
        );
        // 429 defensively → Pending (the retry helper normally absorbs this)
        assert_eq!(
            interpret_device_token(StatusCode::TOO_MANY_REQUESTS, &empty),
            DeviceTokenPoll::Pending
        );
        // 401 → Rejected(401): a wrong client secret, reported as such rather than as an expiry.
        assert_eq!(
            interpret_device_token(StatusCode::UNAUTHORIZED, &empty),
            DeviceTokenPoll::Rejected(401)
        );
        assert_eq!(
            interpret_device_token(StatusCode::FORBIDDEN, &empty),
            DeviceTokenPoll::Rejected(403)
        );
        // 500 → Rejected(500): any unlisted status is terminal here. In practice `poll_token`
        // returns a 5xx as a retryable Err before calling this (see
        // poll_token_surfaces_a_persistent_5xx_as_an_error_not_expired).
        assert_eq!(
            interpret_device_token(StatusCode::INTERNAL_SERVER_ERROR, &empty),
            DeviceTokenPoll::Rejected(500)
        );
    }

    #[test]
    fn parse_watchlist_movies_extracts_tmdb_and_imdb_and_skips_missing() {
        let v = serde_json::json!([
            { "movie": { "title": "Inception", "ids": { "tmdb": 27205, "imdb": "tt1375666" } } },
            { "movie": { "title": "No TMDB", "ids": { "imdb": "tt1" } } },
            { "movie": { "title": "String TMDB", "ids": { "tmdb": "27205" } } }, // string → as_u64() returns None → skipped
        ]);
        assert_eq!(
            parse_watchlist(&v, MediaType::Movie),
            vec![TraktItem {
                media_type: MediaType::Movie,
                tmdb_id: 27205,
                imdb_id: Some("tt1375666".into()),
            }]
        );
    }

    #[test]
    fn parse_watchlist_shows_sets_show_media_type_and_carries_imdb() {
        let v = serde_json::json!([
            { "show": { "title": "Breaking Bad", "ids": { "tmdb": 1396, "imdb": "tt0903747" } } },
            // A TVDB-only show (no imdb in ids) still parses — imdb_id is None, to be resolved later.
            { "show": { "title": "TVDB Only", "ids": { "tmdb": 1397, "tvdb": 478508 } } },
        ]);
        assert_eq!(
            parse_watchlist(&v, MediaType::Show),
            vec![
                TraktItem {
                    media_type: MediaType::Show,
                    tmdb_id: 1396,
                    imdb_id: Some("tt0903747".into()),
                },
                TraktItem {
                    media_type: MediaType::Show,
                    tmdb_id: 1397,
                    imdb_id: None,
                },
            ]
        );
    }

    #[test]
    fn parse_playback_dedupes_show_and_keeps_movie_with_imdb() {
        let v = serde_json::json!([
            { "type": "movie", "movie": { "ids": { "tmdb": 100 } } }, // no imdb → None
            { "type": "episode", "show": { "ids": { "tmdb": 200, "imdb": "tt0944947" } } },
            { "type": "episode", "show": { "ids": { "tmdb": 200, "imdb": "tt0944947" } } },
            { "type": "episode", "show": { "ids": { "imdb": "tt9" } } }, // no tmdb → skipped
            { "movie": { "ids": { "tmdb": 999 } } },                     // no "type" field → skipped
        ]);
        assert_eq!(
            parse_playback(&v),
            vec![
                TraktItem {
                    media_type: MediaType::Movie,
                    tmdb_id: 100,
                    imdb_id: None,
                },
                TraktItem {
                    media_type: MediaType::Show,
                    tmdb_id: 200,
                    imdb_id: Some("tt0944947".into()),
                },
            ]
        );
    }

    #[test]
    fn parse_watched_movies_collects_tmdb_ids() {
        let v = serde_json::json!([
            { "movie": { "ids": { "tmdb": 11 } } },
            { "movie": { "ids": { "tmdb": 22 } } },
            { "movie": { "ids": { "imdb": "tt0" } } }, // skipped
        ]);
        assert_eq!(parse_watched_movies(&v), vec![11, 22]);
    }

    #[test]
    fn parse_watched_shows_collects_episode_pairs_and_last_watched_at() {
        let v = serde_json::json!([
            {
                "last_watched_at": "2021-01-01T00:00:00.000Z",
                "show": { "ids": { "tmdb": 1396, "trakt": 1390, "imdb": "tt0903747" } },
                "seasons": [
                    { "number": 1, "episodes": [{ "number": 1 }, { "number": 2 }] },
                    { "number": 2, "episodes": [{ "number": 1 }] },
                ]
            },
            {
                // no last_watched_at → None; no trakt/imdb id → None (all still parsed)
                "show": { "ids": { "tmdb": 1399 } },
                "seasons": [{ "number": 1, "episodes": [{ "number": 1 }] }]
            }
        ]);
        assert_eq!(
            parse_watched_shows(&v),
            vec![
                WatchedShow {
                    tmdb_id: 1396,
                    trakt_id: Some(1390),
                    imdb_id: Some("tt0903747".into()),
                    watched_episodes: vec![(1, 1), (1, 2), (2, 1)],
                    last_watched_at: Some(1_609_459_200), // 2021-01-01T00:00:00Z
                },
                WatchedShow {
                    tmdb_id: 1399,
                    trakt_id: None,
                    imdb_id: None,
                    watched_episodes: vec![(1, 1)],
                    last_watched_at: None,
                },
            ]
        );
    }

    #[test]
    fn parse_show_progress_behind_from_next_episode_then_falls_back_to_counts() {
        // next_episode present (non-null) → behind, regardless of counts.
        let behind = serde_json::json!({
            "aired": 10, "completed": 10,
            "next_episode": { "season": 2, "number": 1 }
        });
        assert_eq!(
            parse_show_progress(&behind),
            ShowProgress {
                aired: 10,
                completed: 10,
                behind: true
            }
        );
        // next_episode explicitly null → caught up, even if counts look short.
        let caught_up = serde_json::json!({ "aired": 10, "completed": 9, "next_episode": null });
        assert_eq!(
            parse_show_progress(&caught_up),
            ShowProgress {
                aired: 10,
                completed: 9,
                behind: false
            }
        );
        // next_episode field absent (malformed) → fall back to completed < aired.
        let missing = serde_json::json!({ "aired": 10, "completed": 7 });
        assert_eq!(
            parse_show_progress(&missing),
            ShowProgress {
                aired: 10,
                completed: 7,
                behind: true
            }
        );
        // empty body → all zero, not behind.
        assert_eq!(
            parse_show_progress(&serde_json::json!({})),
            ShowProgress {
                aired: 0,
                completed: 0,
                behind: false
            }
        );
    }

    #[tokio::test]
    async fn mock_trakt_returns_canned() {
        use std::sync::Arc;
        let mock = MockTrakt {
            device_code: DeviceCode {
                user_code: "CODE".into(),
                ..Default::default()
            },
            poll: DeviceTokenPoll::Authorized(TraktTokenResponse {
                access_token: "X".into(),
                ..Default::default()
            }),
            token: TraktTokenResponse {
                access_token: "AT".into(),
                ..Default::default()
            },
            user: TraktUser {
                slug: "alice".into(),
                username: "Alice".into(),
            },
            watchlist: vec![TraktItem {
                media_type: MediaType::Movie,
                tmdb_id: 1,
                imdb_id: None,
            }],
            in_progress: vec![TraktItem {
                media_type: MediaType::Show,
                tmdb_id: 2,
                imdb_id: None,
            }],
            watched: WatchedData {
                movies: vec![3],
                shows: vec![],
            },
            progress: std::collections::HashMap::new(),
            fail_reads: false,
            fail_refresh: false,
            transient_refresh: false,
            poll_errors_before_ok: Default::default(),
            me_errors_before_ok: Default::default(),
        };
        let client: Arc<dyn TraktClient> = Arc::new(mock);
        assert_eq!(client.device_code().await.unwrap().user_code, "CODE");
        assert_eq!(
            client.poll_token("dc").await.unwrap(),
            DeviceTokenPoll::Authorized(TraktTokenResponse {
                access_token: "X".into(),
                ..Default::default()
            })
        );
        assert_eq!(client.refresh("ignored").await.unwrap().access_token, "AT");
        assert_eq!(
            client.me("ignored").await.unwrap(),
            TraktUser {
                slug: "alice".into(),
                username: "Alice".into()
            }
        );
        assert_eq!(
            client.watchlist("ignored").await.unwrap(),
            vec![TraktItem {
                media_type: MediaType::Movie,
                tmdb_id: 1,
                imdb_id: None,
            }]
        );
        assert_eq!(
            client.in_progress("ignored").await.unwrap(),
            vec![TraktItem {
                media_type: MediaType::Show,
                tmdb_id: 2,
                imdb_id: None,
            }]
        );
        assert_eq!(client.watched("ignored").await.unwrap().movies, vec![3]);
    }

    /// Loopback server that DROPS the first `drop_n` connections (accept then close immediately —
    /// a transport error / connection reset for the client) and then replies `200 OK` with `body`
    /// to the next request. Counts every accepted connection.
    async fn spawn_drop_then_json(
        drop_n: usize,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = counter.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut seen = 0usize;
            while let Ok((mut sock, _)) = listener.accept().await {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if seen < drop_n {
                    seen += 1;
                    // Drop the connection without responding → transport error on the client.
                    drop(sock);
                    continue;
                }
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                break;
            }
        });
        (format!("http://{}/", addr), counter)
    }

    #[tokio::test(start_paused = true)]
    async fn send_with_retry_recovers_from_a_transient_transport_error() {
        // A transient transport error (connection reset before the response) must be retried with
        // backoff rather than dropping the whole read — so a flaky network doesn't abort a Trakt
        // sync tick that RD/TorBox would ride out. The server drops the first connection, then
        // serves a valid `/users/me` body; `me()` must still succeed.
        let (url, counter) =
            spawn_drop_then_json(1, r#"{"username":"Bob","ids":{"slug":"bob"}}"#).await;
        let client = TraktClientImpl::new("cid".into(), "secret".into(), reqwest::Client::new())
            .with_base_url(url);
        let user = client
            .me("at")
            .await
            .expect("a single transient transport error must be retried, not fatal");
        assert_eq!(user.slug, "bob");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the first (dropped) connection plus the successful retry = 2 attempts"
        );
    }

    /// Loopback that replies with `status` (empty body) on every connection, counting requests.
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

    /// Loopback that replies `status` (empty body) on the first `fail_n` connections, then a 200
    /// JSON `body`, counting requests.
    async fn spawn_status_then_json(
        status: u16,
        fail_n: usize,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = counter.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut seen = 0usize;
            while let Ok((mut sock, _)) = listener.accept().await {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                if seen < fail_n {
                    seen += 1;
                    let head = format!(
                        "HTTP/1.1 {} STATUS\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        status
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.flush().await;
                    continue;
                }
                // Serve the JSON success body, then stop.
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
                break;
            }
        });
        (format!("http://{}/", addr), counter)
    }

    #[tokio::test(start_paused = true)]
    async fn send_with_retry_surfaces_a_persistent_5xx_after_exhausting_attempts() {
        // A persistent 503 must be retried the full MAX_ATTEMPTS times (with backoff) and then
        // surface as an error via the caller's error_for_status() — not hang or succeed. Guards the
        // 5xx-retry branch. `start_paused` auto-advances the exponential backoff so it is instant.
        let (url, counter) = spawn_status_repeating(503).await;
        let client = TraktClientImpl::new("cid".into(), "secret".into(), reqwest::Client::new())
            .with_base_url(url);
        assert!(
            client.me("at").await.is_err(),
            "a persistent 5xx must surface as an error"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            5,
            "a persistent 5xx is retried for the full MAX_ATTEMPTS budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn poll_token_surfaces_a_persistent_5xx_as_an_error_not_expired() {
        // A Trakt 5xx outage that outlasts send_with_retry's ~3s budget during the device-flow poll
        // must come back as Err, so poll_to_completion's bounded consecutive-error path retries it on
        // the poll cadence. Mapping it to Expired would end the enrolment with a misleading
        // "device code expired" while the code is still valid.
        let (url, counter) = spawn_status_repeating(503).await;
        let client = TraktClientImpl::new("cid".into(), "secret".into(), reqwest::Client::new())
            .with_base_url(url);
        let got = client.poll_token("dc").await;
        assert!(
            matches!(got, Err(AppError::Upstream(_))),
            "a persistent 5xx on the device poll must be a retryable error, got {got:?}"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            5,
            "the poll is idempotent, so the 5xx is first retried for the full MAX_ATTEMPTS budget"
        );
    }

    #[test]
    fn refresh_errors_are_transient_only_for_throttling_5xx_and_transport() {
        assert!(refresh_error_is_transient(&AppError::RateLimited));
        assert!(refresh_error_is_transient(&AppError::Upstream("x".into())));
        assert!(refresh_error_is_transient(&AppError::Http(
            http_status_error(503)
        )));
        assert!(refresh_error_is_transient(&AppError::Http(
            http_status_error(500)
        )));
        // No HTTP status: a request-builder error, standing in for a transport failure (both take
        // the `status() == None` branch).
        let transport = reqwest::Client::new()
            .get("")
            .build()
            .map(|_| ())
            .unwrap_err();
        assert!(transport.status().is_none());
        assert!(refresh_error_is_transient(&AppError::Http(transport)));
        // invalid_grant (4xx) and anything else are genuine failures.
        assert!(!refresh_error_is_transient(&AppError::Http(
            http_status_error(400)
        )));
        assert!(!refresh_error_is_transient(&AppError::Http(
            http_status_error(401)
        )));
        assert!(!refresh_error_is_transient(&AppError::Config("x".into())));
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_does_not_retry_a_5xx_single_use_token() {
        // The refresh grant is single-use: a 5xx may mean Trakt already processed it and rotated the
        // token, so resending would double-spend it. refresh() must NOT retry a 5xx (only a read
        // like me() does) — exactly ONE request, then surface the error, which the callers treat as
        // a skipped cycle (`refresh_error_is_transient`), not a re-enrolment. Guards the
        // retry_server_errors == false path.
        let (url, counter) = spawn_status_repeating(503).await;
        let client = TraktClientImpl::new("cid".into(), "secret".into(), reqwest::Client::new())
            .with_base_url(url);
        assert!(
            client.refresh("rt").await.is_err(),
            "a 5xx on refresh must surface as an error"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "refresh must NOT retry a 5xx (single-use token): exactly one request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_does_not_retry_a_transport_error_single_use_token() {
        // Same single-use guarantee for a transport error (dropped connection): refresh must not
        // resend (and so double-spend the token) — exactly one attempt — unlike me(), which retries
        // a transient transport error. The callers skip the cycle rather than flag the account.
        let (url, counter) = spawn_drop_then_json(1, r#"{"access_token":"a"}"#).await;
        let client = TraktClientImpl::new("cid".into(), "secret".into(), reqwest::Client::new())
            .with_base_url(url);
        assert!(
            client.refresh("rt").await.is_err(),
            "a dropped connection on refresh must surface as an error, not retry"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "refresh must NOT retry a transport error (single-use token): exactly one request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_maps_a_persistent_429_to_rate_limited_not_http() {
        // A persistent 429 exhausts the retry budget (5 attempts) and MUST surface as the transient
        // `RateLimited` signal, NOT an `AppError::Http`. `refresh_account` relies on this distinction
        // to avoid flagging a healthy account `needs_reenrolment` on mere throttling. `start_paused`
        // auto-advances the limiter's post-throttle waits so the test is instant.
        let (url, counter) = spawn_status_repeating(429).await;
        let client = TraktClientImpl::new("cid".into(), "secret".into(), reqwest::Client::new())
            .with_base_url(url);
        let err = client
            .refresh("rt")
            .await
            .expect_err("a persistent 429 must be an error");
        assert!(
            matches!(err, AppError::RateLimited),
            "a persistent 429 on refresh must map to RateLimited, got {err:?}"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            5,
            "a 429 is retried for the full MAX_ATTEMPTS budget before surfacing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn send_with_retry_recovers_from_a_429_then_success() {
        // A single 429 (rate-limited) must record the throttle and RETRY, recovering on the next
        // 200 — not surface as an error. Guards the 429 record_throttle+retry branch. `start_paused`
        // auto-advances the limiter's post-throttle wait so the test is instant.
        let (url, counter) =
            spawn_status_then_json(429, 1, r#"{"username":"Bob","ids":{"slug":"bob"}}"#).await;
        let client = TraktClientImpl::new("cid".into(), "secret".into(), reqwest::Client::new())
            .with_base_url(url);
        let user = client
            .me("at")
            .await
            .expect("a single 429 must be retried and recover");
        assert_eq!(user.slug, "bob");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the 429 attempt plus the successful retry = 2 requests"
        );
    }

    #[tokio::test]
    async fn mock_trakt_fail_reads_errors_reads_and_fail_refresh_errors_refresh() {
        let mock = MockTrakt {
            watchlist: vec![TraktItem {
                media_type: MediaType::Movie,
                tmdb_id: 1,
                imdb_id: None,
            }],
            fail_reads: true,
            fail_refresh: true,
            ..Default::default()
        };
        assert!(mock.watchlist("ignored").await.is_err());
        assert!(mock.in_progress("ignored").await.is_err());
        assert!(mock.watched("ignored").await.is_err());
        assert!(mock.refresh("ignored").await.is_err());
        // device_code / poll_token are unaffected by the flags.
        assert!(mock.device_code().await.is_ok());
        assert!(mock.poll_token("dc").await.is_ok());
    }
}

/// Minimal loopback HTTP/1.1 router for exercising `TraktClientImpl` end-to-end (pagination, page
/// failures) without the network. Each route matches a request PATH plus its query parameters
/// (order-insensitive) and may be hit any number of times; an unmatched request gets a `404` so a
/// test that issues an unexpected request fails loudly. Every request target is logged in arrival
/// order. `pub(crate)` so the `tasks` sync tests can drive `sync_trakt` through a real client.
#[cfg(test)]
pub(crate) mod test_http {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug, Clone)]
    pub(crate) struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Reply {
        /// A `200` JSON body with no pagination headers (like `/sync/playback`, captured live).
        pub(crate) fn json(body: &str) -> Self {
            Self {
                status: 200,
                headers: Vec::new(),
                body: body.to_string(),
            }
        }

        /// A `200` JSON page carrying Trakt's pagination headers, exactly as captured live
        /// (`x-pagination-page`, `-limit`, `-page-count`, `-item-count`).
        pub(crate) fn page(
            body: &str,
            page: u32,
            page_count: u32,
            item_count: u64,
            limit: u32,
        ) -> Self {
            Self::json(body)
                .header("x-pagination-page", &page.to_string())
                .header("x-pagination-limit", &limit.to_string())
                .header("x-pagination-page-count", &page_count.to_string())
                .header("x-pagination-item-count", &item_count.to_string())
        }

        /// A bare status (empty body), e.g. a `503` page failure.
        pub(crate) fn status(status: u16) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: String::new(),
            }
        }

        pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_string(), value.to_string()));
            self
        }
    }

    struct Route {
        path: String,
        query: Vec<(String, String)>,
        reply: Reply,
    }

    /// Split a request target into its path and its (sorted) query pairs.
    fn split_target(target: &str) -> (String, Vec<(String, String)>) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut pairs: Vec<(String, String)> = query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (k.to_string(), v.to_string())
            })
            .collect();
        pairs.sort();
        (path.to_string(), pairs)
    }

    pub(crate) struct Server {
        pub base_url: String,
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        /// Every request target received so far, in arrival order.
        pub(crate) fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }

        /// How many requests hit `path` (any query).
        pub(crate) fn hits(&self, path: &str) -> usize {
            self.requests()
                .iter()
                .filter(|t| t.split('?').next() == Some(path))
                .count()
        }
    }

    /// Spawn the router. `routes` = `(request target incl. query, reply)`.
    pub(crate) async fn spawn(routes: Vec<(&str, Reply)>) -> Server {
        let routes: Arc<Vec<Route>> = Arc::new(
            routes
                .into_iter()
                .map(|(target, reply)| {
                    let (path, query) = split_target(target);
                    Route { path, query, reply }
                })
                .collect(),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let routes = routes.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let target = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    log.lock().unwrap().push(target.clone());
                    let (path, query) = split_target(&target);
                    let reply = routes
                        .iter()
                        .find(|r| r.path == path && r.query == query)
                        .map(|r| r.reply.clone())
                        .unwrap_or_else(|| Reply::status(404));
                    let mut out = format!(
                        "HTTP/1.1 {} STATUS\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    for (k, v) in &reply.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    out.push_str(&reply.body);
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        Server {
            base_url: format!("http://{addr}"),
            requests,
        }
    }
}

/// Real-response fixtures + pagination behaviour. The fixtures under `tests/fixtures/trakt/` are
/// TRIMMED, ANONYMISED copies of live responses captured in Nov 2026 (after Trakt's
/// pagination/`extended=progress` changes — trakt-api discussions #681 and #775): the response
/// shapes are exactly as Trakt sent them, but every title, id, slug, Plex GUID and list-item id is
/// fictional and every date is shifted (ids keep their relative order; times of day are unchanged):
///   - `watched_shows_progress_p{1,2}.json` — `GET /sync/watched/shows?extended=progress&page=N&limit=100`
///     (live headers: item-count 129, limit 100, page-count 2);
///   - `watched_shows_no_extended.json` — `GET /sync/watched/shows` (no `extended`): no `seasons`;
///   - `watched_movies_p{1,2}.json` — `GET /sync/watched/movies?page=N&limit=100` (item-count 206,
///     page-count 3);
///   - `watchlist_{movies,shows}.json` — `GET /sync/watchlist/{movies,shows}?page=1&limit=100`;
///   - `playback.json` — `GET /sync/playback` (no pagination headers at all).
#[cfg(test)]
mod pagination_tests {
    use super::test_http::{spawn, Reply};
    use super::*;

    const WATCHED_SHOWS_P1: &str =
        include_str!("../tests/fixtures/trakt/watched_shows_progress_p1.json");
    const WATCHED_SHOWS_P2: &str =
        include_str!("../tests/fixtures/trakt/watched_shows_progress_p2.json");
    const WATCHED_SHOWS_NO_EXTENDED: &str =
        include_str!("../tests/fixtures/trakt/watched_shows_no_extended.json");
    const WATCHED_MOVIES_P1: &str = include_str!("../tests/fixtures/trakt/watched_movies_p1.json");
    const WATCHED_MOVIES_P2: &str = include_str!("../tests/fixtures/trakt/watched_movies_p2.json");
    const WATCHLIST_MOVIES: &str = include_str!("../tests/fixtures/trakt/watchlist_movies.json");
    const WATCHLIST_SHOWS: &str = include_str!("../tests/fixtures/trakt/watchlist_shows.json");
    const PLAYBACK: &str = include_str!("../tests/fixtures/trakt/playback.json");

    fn json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("fixture is valid JSON")
    }

    fn client(base_url: &str) -> TraktClientImpl {
        TraktClientImpl::new("cid".into(), "secret".into(), reqwest::Client::new())
            .with_base_url(base_url.to_string())
    }

    fn eps(season: u32, range: std::ops::RangeInclusive<u32>) -> Vec<(u32, u32)> {
        range.map(|e| (season, e)).collect()
    }

    // ── parsers on the real samples ──────────────────────────────────────────

    #[test]
    fn parse_watched_shows_reads_the_real_extended_progress_shape() {
        let p1 = parse_watched_shows(&json(WATCHED_SHOWS_P1));
        let mut drama_eps = eps(1, 1..=8);
        drama_eps.extend(eps(2, 1..=7));
        assert_eq!(
            p1,
            vec![
                WatchedShow {
                    tmdb_id: 911000,
                    trakt_id: Some(811000),
                    imdb_id: Some("tt9900900".into()),
                    watched_episodes: drama_eps,
                    last_watched_at: Some(1_759_873_440), // 2025-10-07T21:44:00.000Z
                },
                WatchedShow {
                    tmdb_id: 911100,
                    trakt_id: Some(811300),
                    imdb_id: Some("tt9901100".into()),
                    watched_episodes: eps(1, 1..=4),
                    last_watched_at: Some(1_737_450_120), // 2025-01-21T09:02:00.000Z
                },
            ],
            "extended=progress keeps every id the sync needs (tmdb/trakt/imdb) + last_watched_at, \
             and carries the per-episode watched set"
        );
        let p2 = parse_watched_shows(&json(WATCHED_SHOWS_P2));
        assert_eq!(
            p2,
            vec![
                WatchedShow {
                    tmdb_id: 910900,
                    trakt_id: Some(810900),
                    imdb_id: Some("tt9900600".into()),
                    watched_episodes: eps(1, 1..=5),
                    last_watched_at: Some(1_663_699_684), // 2022-09-20T18:48:04.000Z
                },
                WatchedShow {
                    // Trakt sends `"imdb": null` for this show → None (not an empty string).
                    tmdb_id: 910700,
                    trakt_id: Some(810700),
                    imdb_id: None,
                    watched_episodes: eps(1, 1..=2),
                    last_watched_at: Some(1_663_356_321), // 2022-09-16T19:25:21.000Z
                },
            ]
        );
        // The live samples carry no season 0: Trakt's watched history omits Specials, matching the
        // TMDB aired set (`parse_season_numbers` drops season 0) — see `wanted::SPECIALS_SEASON`.
        assert!(p1
            .iter()
            .chain(p2.iter())
            .flat_map(|s| s.watched_episodes.iter())
            .all(|(season, _)| *season != 0));
    }

    #[test]
    fn parse_watched_shows_drops_episodes_watched_before_a_progress_reset() {
        // Trakt's `reset_at`: the user reset the show's progress (a re-watch); an episode whose
        // `last_watched_at` predates it is unwatched for progress — Trakt's own per-show progress
        // (the catch-up gate) already reads it that way. Counting it would make a re-watched Ended
        // show look finished and get it finish-removed mid-re-watch. The Example Drama sample with a reset
        // injected between its plays: S1E1-8 + S2E1-5 (2025-05-01T19:37) predate the reset, S2E6
        // (20:40 that day) and S2E7 (2025-10-07) come after it.
        let mut v = json(WATCHED_SHOWS_P1);
        v[0]["reset_at"] = serde_json::json!("2025-05-01T20:00:00.000Z");
        let shows = parse_watched_shows(&v);
        assert_eq!(shows[0].tmdb_id, 911000);
        assert_eq!(shows[0].watched_episodes, vec![(2, 6), (2, 7)]);
        // `reset_at: null` (every live sample) changes nothing.
        assert_eq!(shows[1].watched_episodes, eps(1, 1..=4));
    }

    #[test]
    fn watched_shows_without_extended_carry_no_episodes() {
        // The regression's root cause, pinned on the real sample: since 2026-07-03 the plain
        // `/sync/watched/shows` returns NO `seasons`, so every show parsed to an EMPTY watched set
        // (Trigger A could never fire; catch-up re-acquired already-watched episodes). The ids and
        // `last_watched_at` are still there — only the episodes are missing.
        let shows = parse_watched_shows(&json(WATCHED_SHOWS_NO_EXTENDED));
        assert_eq!(
            shows.iter().map(|s| s.tmdb_id).collect::<Vec<_>>(),
            vec![911000, 911100]
        );
        assert!(shows.iter().all(|s| s.watched_episodes.is_empty()));
        assert!(shows
            .iter()
            .all(|s| s.trakt_id.is_some() && s.last_watched_at.is_some()));
    }

    #[test]
    fn parse_real_watchlist_watched_movies_and_playback_samples() {
        let movie = |tmdb_id: u64, imdb: &str| TraktItem {
            media_type: MediaType::Movie,
            tmdb_id,
            imdb_id: Some(imdb.into()),
        };
        let show = |tmdb_id: u64, imdb: &str| TraktItem {
            media_type: MediaType::Show,
            tmdb_id,
            imdb_id: Some(imdb.into()),
        };
        assert_eq!(
            parse_watchlist(&json(WATCHLIST_MOVIES), MediaType::Movie),
            vec![
                movie(911700, "tt9901000"),
                movie(911300, "tt9901300"),
                movie(911400, "tt9901400"),
            ]
        );
        assert_eq!(
            parse_watchlist(&json(WATCHLIST_SHOWS), MediaType::Show),
            vec![show(911200, "tt9901500"), show(910100, "tt9900100")]
        );
        assert_eq!(
            parse_watched_movies(&json(WATCHED_MOVIES_P1)),
            vec![911600, 910500, 911500]
        );
        assert_eq!(
            parse_playback(&json(PLAYBACK)),
            vec![show(910200, "tt9900400"), show(910600, "tt9901700")],
            "in-progress episodes map to their show"
        );
    }

    #[test]
    fn parse_pagination_reads_trakt_headers_and_tolerates_garbage() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut h = HeaderMap::new();
        // Exactly the live `/sync/watched/movies?page=2&limit=100` headers.
        h.insert("x-pagination-item-count", HeaderValue::from_static("206"));
        h.insert("x-pagination-limit", HeaderValue::from_static("100"));
        h.insert("x-pagination-page", HeaderValue::from_static("2"));
        h.insert("x-pagination-page-count", HeaderValue::from_static("3"));
        assert_eq!(
            parse_pagination(&h),
            Pagination {
                page: Some(2),
                page_count: Some(3),
                item_count: Some(206),
            }
        );
        // No headers at all (e.g. `/sync/playback`) → nothing known.
        assert_eq!(parse_pagination(&HeaderMap::new()), Pagination::default());
        // Garbled values are ignored, never guessed.
        let mut g = HeaderMap::new();
        g.insert("x-pagination-page", HeaderValue::from_static("one"));
        g.insert("x-pagination-page-count", HeaderValue::from_static("-1"));
        g.insert("x-pagination-item-count", HeaderValue::from_static(""));
        assert_eq!(parse_pagination(&g), Pagination::default());
    }

    // ── pagination over HTTP ─────────────────────────────────────────────────

    /// The routes for a full, healthy, two-page `watched()` read built from the real samples.
    fn watched_routes() -> Vec<(&'static str, Reply)> {
        vec![
            (
                "/sync/watched/movies?page=1&limit=100",
                Reply::page(WATCHED_MOVIES_P1, 1, 2, 5, 100),
            ),
            (
                "/sync/watched/movies?page=2&limit=100",
                Reply::page(WATCHED_MOVIES_P2, 2, 2, 5, 100),
            ),
            (
                "/sync/watched/shows?extended=progress&page=1&limit=100",
                Reply::page(WATCHED_SHOWS_P1, 1, 2, 4, 100),
            ),
            (
                "/sync/watched/shows?extended=progress&page=2&limit=100",
                Reply::page(WATCHED_SHOWS_P2, 2, 2, 4, 100),
            ),
        ]
    }

    #[tokio::test]
    async fn watched_follows_every_page_and_requests_extended_progress() {
        let server = spawn(watched_routes()).await;
        let w = client(&server.base_url)
            .watched("at")
            .await
            .expect("watched");
        assert_eq!(
            w.movies,
            vec![911600, 910500, 911500, 910400, 910300],
            "page 2's movies must not be dropped"
        );
        assert_eq!(
            w.shows.iter().map(|s| s.tmdb_id).collect::<Vec<_>>(),
            vec![911000, 911100, 910900, 910700],
            "page 2's shows must not be dropped"
        );
        assert!(
            w.shows.iter().all(|s| !s.watched_episodes.is_empty()),
            "extended=progress must yield the per-episode watched sets"
        );
        assert_eq!(server.hits("/sync/watched/movies"), 2);
        assert_eq!(server.hits("/sync/watched/shows"), 2);
        assert!(
            server
                .requests()
                .iter()
                .filter(|t| t.starts_with("/sync/watched/shows"))
                .all(|t| t.contains("extended=progress")),
            "every watched-shows page must request extended=progress: {:?}",
            server.requests()
        );
    }

    #[tokio::test]
    async fn watchlist_follows_pagination_for_movies_and_shows() {
        // A title on page 2 of the watchlist must be read — otherwise it is never acquired and, worse,
        // looks un-watchlisted to Trigger B (abandoned) and could be deleted.
        let page2 = r#"[{"type":"movie","movie":{"ids":{"tmdb":11,"imdb":"tt0076759","trakt":1},"title":"Star Wars"}}]"#;
        let server = spawn(vec![
            (
                "/sync/watchlist/movies?page=1&limit=100",
                Reply::page(WATCHLIST_MOVIES, 1, 2, 4, 100),
            ),
            (
                "/sync/watchlist/movies?page=2&limit=100",
                Reply::page(page2, 2, 2, 4, 100),
            ),
            (
                "/sync/watchlist/shows?page=1&limit=100",
                Reply::page(WATCHLIST_SHOWS, 1, 1, 2, 100),
            ),
        ])
        .await;
        let items = client(&server.base_url)
            .watchlist("at")
            .await
            .expect("watchlist");
        assert_eq!(
            items.iter().map(|i| i.tmdb_id).collect::<Vec<_>>(),
            vec![911700, 911300, 911400, 11, 911200, 910100]
        );
        assert_eq!(server.hits("/sync/watchlist/movies"), 2);
        assert_eq!(server.hits("/sync/watchlist/shows"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_later_page_is_an_err_never_a_truncated_ok() {
        // Page 1 succeeds and says there are 2 pages; page 2 fails persistently (retried, then
        // surfaced). The whole read must be Err so sync keeps the previous wanted set — returning
        // page 1 alone would be a silently truncated list.
        for failing in [
            "/sync/watched/movies?page=2&limit=100",
            "/sync/watched/shows?extended=progress&page=2&limit=100",
        ] {
            let routes: Vec<(&str, Reply)> = watched_routes()
                .into_iter()
                .map(|(t, r)| {
                    if t == failing {
                        (t, Reply::status(503))
                    } else {
                        (t, r)
                    }
                })
                .collect();
            let server = spawn(routes).await;
            assert!(
                client(&server.base_url).watched("at").await.is_err(),
                "a failure of {failing} must fail the watched() read"
            );
        }
        let server = spawn(vec![
            (
                "/sync/watchlist/movies?page=1&limit=100",
                Reply::page(WATCHLIST_MOVIES, 1, 1, 3, 100),
            ),
            (
                "/sync/watchlist/shows?page=1&limit=100",
                Reply::page(WATCHLIST_SHOWS, 1, 2, 4, 100),
            ),
            ("/sync/watchlist/shows?page=2&limit=100", Reply::status(500)),
        ])
        .await;
        assert!(client(&server.base_url).watchlist("at").await.is_err());
    }

    #[tokio::test]
    async fn missing_or_garbled_pagination_headers_read_as_a_single_page() {
        for reply in [
            Reply::json(WATCHED_MOVIES_P1),
            Reply::json(WATCHED_MOVIES_P1).header("x-pagination-page-count", "lots"),
            Reply::json(WATCHED_MOVIES_P1).header("x-pagination-page-count", "0"),
        ] {
            let server = spawn(vec![
                ("/sync/watched/movies?page=1&limit=100", reply),
                (
                    "/sync/watched/shows?extended=progress&page=1&limit=100",
                    Reply::json("[]"),
                ),
            ])
            .await;
            let w = client(&server.base_url)
                .watched("at")
                .await
                .expect("watched");
            assert_eq!(w.movies, vec![911600, 910500, 911500]);
            assert_eq!(
                server.hits("/sync/watched/movies"),
                1,
                "no page-2 request without a usable page count"
            );
        }
    }

    #[tokio::test]
    async fn a_page_count_beyond_the_cap_is_an_err_not_a_runaway_loop() {
        let server = spawn(vec![(
            "/sync/watchlist/movies?page=1&limit=100",
            Reply::page(WATCHLIST_MOVIES, 1, MAX_PAGES + 1, 1_000_000, 100),
        )])
        .await;
        assert!(client(&server.base_url).watchlist("at").await.is_err());
        assert_eq!(
            server.hits("/sync/watchlist/movies"),
            1,
            "an over-cap page count is rejected up front"
        );
    }

    #[tokio::test]
    async fn a_list_that_changes_mid_read_is_an_err() {
        // Items shifting between pages (an add/remove mid-read) could drop a title from BOTH pages —
        // for the watchlist that would read as "un-watchlisted". Detected by the pagination headers
        // changing between pages; the read is retried next sync instead.
        let changed = [
            Reply::page(WATCHED_MOVIES_P2, 2, 2, 4, 100), // item count 5 → 4
            Reply::page(WATCHED_MOVIES_P2, 2, 3, 5, 100), // page count 2 → 3
            Reply::page(WATCHED_MOVIES_P2, 1, 2, 5, 100), // asked for page 2, got page 1
        ];
        for page2 in changed {
            let server = spawn(vec![
                (
                    "/sync/watched/movies?page=1&limit=100",
                    Reply::page(WATCHED_MOVIES_P1, 1, 2, 5, 100),
                ),
                ("/sync/watched/movies?page=2&limit=100", page2),
                (
                    "/sync/watched/shows?extended=progress&page=1&limit=100",
                    Reply::json("[]"),
                ),
            ])
            .await;
            assert!(client(&server.base_url).watched("at").await.is_err());
        }
    }

    #[tokio::test]
    async fn a_list_item_seen_on_two_pages_is_an_err() {
        // A reorder between the page fetches keeps every pagination header the same, but moves an
        // item from page 2 onto the already-read page 1, pushing a page-1 item onto page 2: the
        // moved title is on NEITHER page as read (it would read as un-watchlisted, a Trigger-B
        // removal), and the pushed one is on both. Its list-item id appearing twice gives the shift
        // away; the read fails and is retried next sync.
        let page1 = r#"[
            {"type":"movie","id":5000001,"rank":1,"movie":{"ids":{"tmdb":101,"trakt":1}}},
            {"type":"movie","id":5000002,"rank":2,"movie":{"ids":{"tmdb":102,"trakt":2}}}
        ]"#;
        // After the reorder: item 5000003 (tmdb 103) moved to the top; 5000002 slid onto page 2.
        let page2 = r#"[
            {"type":"movie","id":5000002,"rank":3,"movie":{"ids":{"tmdb":102,"trakt":2}}}
        ]"#;
        let server = spawn(vec![
            (
                "/sync/watchlist/movies?page=1&limit=100",
                Reply::page(page1, 1, 2, 3, 2),
            ),
            (
                "/sync/watchlist/movies?page=2&limit=100",
                Reply::page(page2, 2, 2, 3, 2),
            ),
            (
                "/sync/watchlist/shows?page=1&limit=100",
                Reply::page(WATCHLIST_SHOWS, 1, 1, 2, 100),
            ),
        ])
        .await;
        let err = client(&server.base_url)
            .watchlist("at")
            .await
            .expect_err("a list item on two pages must fail the read");
        assert!(err.to_string().contains("5000002"), "{err}");

        // Distinct ids across pages read normally (see also
        // watchlist_follows_pagination_for_movies_and_shows). Watched items, which carry no
        // list-item id, are compared by their movie or show Trakt id (the tests below).
        let page2_ok = r#"[
            {"type":"movie","id":5000003,"rank":3,"movie":{"ids":{"tmdb":103,"trakt":3}}}
        ]"#;
        let server = spawn(vec![
            (
                "/sync/watchlist/movies?page=1&limit=100",
                Reply::page(page1, 1, 2, 3, 2),
            ),
            (
                "/sync/watchlist/movies?page=2&limit=100",
                Reply::page(page2_ok, 2, 2, 3, 2),
            ),
            (
                "/sync/watchlist/shows?page=1&limit=100",
                Reply::page(WATCHLIST_SHOWS, 1, 1, 2, 100),
            ),
        ])
        .await;
        let items = client(&server.base_url)
            .watchlist("at")
            .await
            .expect("distinct ids read normally");
        assert_eq!(
            items.iter().map(|i| i.tmdb_id).collect::<Vec<_>>(),
            vec![101, 102, 103, 911200, 910100]
        );
    }

    /// Page 2 of a watched list after a title on it moved to the top between the page fetches: it
    /// pushed page 1's last item onto page 2 and is itself read on neither page. Both counts are
    /// unchanged. The watched endpoints come back newest `last_watched_at` first, so this is what
    /// watching (or re-watching) a title past position 100 does mid-read.
    fn page2_after_a_move_to_the_top(p1: &str, p2: &str) -> String {
        let p1 = json(p1);
        let p2 = json(p2);
        let pushed = p1.as_array().unwrap().last().unwrap().clone();
        let rest = p2.as_array().unwrap()[1..].to_vec();
        serde_json::Value::Array(std::iter::once(pushed).chain(rest).collect()).to_string()
    }

    #[tokio::test]
    async fn a_watched_show_seen_on_two_pages_is_an_err() {
        // Watched items carry no list-item `id`; a show appears only once in the list, so its Trakt
        // id on two pages gives the shift away. Read as-is, the moved show (810900) would drop out
        // of the wanted set for a cycle: a catch-up-only show loses its row, and one with Watchlist
        // provenance becomes a Trigger-B removal.
        let page2 = page2_after_a_move_to_the_top(WATCHED_SHOWS_P1, WATCHED_SHOWS_P2);
        let routes: Vec<(&str, Reply)> = watched_routes()
            .into_iter()
            .map(|(t, r)| {
                if t == "/sync/watched/shows?extended=progress&page=2&limit=100" {
                    (t, Reply::page(&page2, 2, 2, 4, 100))
                } else {
                    (t, r)
                }
            })
            .collect();
        let server = spawn(routes).await;
        let err = client(&server.base_url)
            .watched("at")
            .await
            .expect_err("a watched show on two pages must fail the read");
        assert!(err.to_string().contains("show 811300"), "{err}");
    }

    #[tokio::test]
    async fn a_watched_movie_seen_on_two_pages_is_an_err() {
        let page2 = page2_after_a_move_to_the_top(WATCHED_MOVIES_P1, WATCHED_MOVIES_P2);
        let routes: Vec<(&str, Reply)> = watched_routes()
            .into_iter()
            .map(|(t, r)| {
                if t == "/sync/watched/movies?page=2&limit=100" {
                    (t, Reply::page(&page2, 2, 2, 5, 100))
                } else {
                    (t, r)
                }
            })
            .collect();
        let server = spawn(routes).await;
        let err = client(&server.base_url)
            .watched("at")
            .await
            .expect_err("a watched movie on two pages must fail the read");
        assert!(err.to_string().contains("movie 811500"), "{err}");
    }

    #[test]
    fn repeated_list_item_keys_on_the_entry_id_before_the_title_id() {
        // Playback entries are paused episodes: several share one show, each under its own `id`.
        // Keyed by entry id they never collide; keyed by show they would fail every paginated read.
        let page1 = json(
            r#"[{"type":"episode","id":5000070,"episode":{"ids":{"trakt":810800}},
                 "show":{"ids":{"trakt":810200,"tmdb":910200}}}]"#,
        );
        let page2 = json(
            r#"[{"type":"episode","id":5000071,"episode":{"ids":{"trakt":810801}},
                 "show":{"ids":{"trakt":810200,"tmdb":910200}}}]"#,
        );
        let mut seen = std::collections::HashSet::new();
        seen.extend(page1.as_array().unwrap().iter().filter_map(list_item_key));
        assert_eq!(repeated_list_item(&seen, page2.as_array().unwrap()), None);

        // Without an `id`, an episode entry is not keyed by its show (it isn't compared), while a
        // watched movie or show is keyed by its Trakt id, tagged by kind so a movie and a show
        // sharing a numeric id don't collide.
        let ep = json(r#"{"episode":{"ids":{"trakt":1}},"show":{"ids":{"trakt":7}}}"#);
        assert_eq!(list_item_key(&ep), None);
        let movie = json(r#"{"movie":{"ids":{"trakt":7}}}"#);
        let show = json(r#"{"show":{"ids":{"trakt":7}}}"#);
        assert_ne!(list_item_key(&movie), list_item_key(&show));
        let seen: std::collections::HashSet<ListItemKey> =
            list_item_key(&movie).into_iter().collect();
        assert_eq!(repeated_list_item(&seen, &[show]), None);
        assert_eq!(
            repeated_list_item(&seen, &[movie]),
            Some(ListItemKey::Movie(7))
        );
    }

    #[tokio::test]
    async fn a_non_array_page_body_is_an_err() {
        let server = spawn(vec![
            (
                "/sync/watchlist/movies?page=1&limit=100",
                Reply::json(r#"{"error":"nope"}"#),
            ),
            (
                "/sync/watchlist/shows?page=1&limit=100",
                Reply::json(WATCHLIST_SHOWS),
            ),
        ])
        .await;
        assert!(client(&server.base_url).watchlist("at").await.is_err());
    }

    #[tokio::test]
    async fn playback_is_read_unpaginated_but_follows_pages_if_trakt_starts_paginating() {
        // Live `/sync/playback` carries NO pagination headers, so it is requested exactly as before
        // (no page/limit) and read as one page.
        let server = spawn(vec![("/sync/playback", Reply::json(PLAYBACK))]).await;
        let items = client(&server.base_url)
            .in_progress("at")
            .await
            .expect("playback");
        assert_eq!(
            items.iter().map(|i| i.tmdb_id).collect::<Vec<_>>(),
            vec![910200, 910600]
        );
        assert_eq!(server.requests(), vec!["/sync/playback".to_string()]);

        // Should Trakt start paginating it (headers appear), the remaining pages are followed at the
        // server's own default page size.
        let page2 = r#"[{"type":"movie","movie":{"ids":{"tmdb":77,"trakt":7}}}]"#;
        let server = spawn(vec![
            ("/sync/playback", Reply::page(PLAYBACK, 1, 2, 3, 2)),
            ("/sync/playback?page=2", Reply::page(page2, 2, 2, 3, 2)),
        ])
        .await;
        let items = client(&server.base_url)
            .in_progress("at")
            .await
            .expect("playback");
        assert_eq!(
            items.iter().map(|i| i.tmdb_id).collect::<Vec<_>>(),
            vec![910200, 910600, 77]
        );
    }
}
