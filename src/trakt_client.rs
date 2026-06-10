//! Trakt API client: device-flow OAuth + the SP2 read endpoints, mirroring the
//! `scraper.rs` shape (a `#[async_trait]` trait, a real impl holding `base_url` +
//! `reqwest::Client` + a shared rate limiter, pure free functions for request/JSON
//! building and parsing, and a `#[cfg(test)]` `MockTrakt`).

use crate::error::AppError;
use crate::ratelimit::AdaptiveRateLimiter;
use crate::vfs::MediaType;
use async_trait::async_trait;

/// A movie or show identified by its TMDB id (a watchlist or playback entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraktItem {
    pub media_type: MediaType,
    pub tmdb_id: u64,
}

/// Device-flow code response (`POST /oauth/device/code`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub interval: u64,   // seconds between polls
    pub expires_in: u64, // seconds until the code expires
}

/// OAuth token response (device token / refresh). Caller maps to `store::TraktTokens` via
/// `expires_at = created_at + expires_in`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TraktTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64, // seconds
    pub created_at: u64, // unix epoch seconds
}

/// The authenticated user's identity (`GET /users/me`). `slug` is the stable Trakt URL slug
/// used as the per-user key for stored tokens + wanted rows; `username` is for display.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TraktUser {
    pub slug: String,
    pub username: String,
}

/// Single-poll outcome of `POST /oauth/device/token`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DeviceTokenPoll {
    #[default]
    Pending,
    Authorized(TraktTokenResponse),
    Denied,
    Expired,
}

/// Per-show watched-episode set, for the removal finish-test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedShow {
    pub tmdb_id: u64,
    pub watched_episodes: Vec<(u32, u32)>,
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
    /// GET /sync/watchlist/movies + /sync/watchlist/shows — the user's watchlisted movies+shows.
    async fn watchlist(&self, access_token: &str) -> Result<Vec<TraktItem>, AppError>;
    /// GET /sync/playback — movies + (show via episodes) the user is mid-watch, deduped by show tmdb.
    async fn in_progress(&self, access_token: &str) -> Result<Vec<TraktItem>, AppError>;
    /// GET /sync/watched/movies + /sync/watched/shows — watched movies + per-show watched episodes.
    async fn watched(&self, access_token: &str) -> Result<WatchedData, AppError>;
}

const DEFAULT_BASE_URL: &str = "https://api.trakt.tv";

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

    /// Builder override for the API base URL (e.g. a future live test pointing elsewhere).
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
    /// the status themselves (`poll_token` treats 400/410/418 as outcomes, not errors).
    async fn send_with_retry(
        &self,
        make_request: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, AppError> {
        const MAX_ATTEMPTS: u32 = 5;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            self.limiter.wait_for_token().await;
            let resp = make_request().send().await.map_err(AppError::Http)?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok());
                self.limiter.record_throttle(retry_after).await;
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
            // upstream).
            if status.is_server_error() {
                if attempt < MAX_ATTEMPTS {
                    let backoff_ms = 200u64.saturating_mul(1u64 << (attempt - 1).min(5));
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
        // Only a 200 carries a useful body; any other status maps purely off the code, so a
        // missing/non-JSON body is fine (treated as Null).
        let v = resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null);
        Ok(interpret_device_token(status, &v))
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TraktTokenResponse, AppError> {
        let body = build_refresh_body(refresh_token, &self.client_id, &self.client_secret);
        let resp = self
            .send_with_retry(|| {
                self.request(reqwest::Method::POST, "/oauth/token")
                    .json(&body)
            })
            .await?;
        let resp = resp.error_for_status().map_err(AppError::Http)?;
        let v: serde_json::Value = resp.json().await.map_err(AppError::Http)?;
        Ok(parse_token_response(&v))
    }

    async fn me(&self, access_token: &str) -> Result<TraktUser, AppError> {
        let v = self.authed_get_json("/users/me", access_token).await?;
        Ok(parse_user(&v))
    }

    async fn watchlist(&self, access_token: &str) -> Result<Vec<TraktItem>, AppError> {
        let movies = self
            .authed_get_json("/sync/watchlist/movies", access_token)
            .await?;
        let shows = self
            .authed_get_json("/sync/watchlist/shows", access_token)
            .await?;
        let mut out = parse_watchlist(&movies, MediaType::Movie);
        out.extend(parse_watchlist(&shows, MediaType::Show));
        Ok(out)
    }

    async fn in_progress(&self, access_token: &str) -> Result<Vec<TraktItem>, AppError> {
        let v = self.authed_get_json("/sync/playback", access_token).await?;
        Ok(parse_playback(&v))
    }

    async fn watched(&self, access_token: &str) -> Result<WatchedData, AppError> {
        let movies = self
            .authed_get_json("/sync/watched/movies", access_token)
            .await?;
        let shows = self
            .authed_get_json("/sync/watched/shows", access_token)
            .await?;
        Ok(WatchedData {
            movies: parse_watched_movies(&movies),
            shows: parse_watched_shows(&shows),
        })
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
///   404 Not Found     -> the device_code is invalid/unknown (terminal → treat as expired).
///   409 Conflict      -> the code was already used/approved (terminal → expired).
///   410 Gone          -> the code expired.
///   418 I'm a teapot  -> the user explicitly denied the request.
///   429 Too Many Req  -> polling too fast; normally absorbed by the retry helper, mapped to
///                        Pending defensively so a stray 429 doesn't abort enrolment.
/// Any other status is treated as terminal (Expired) so the caller's poll loop can't spin forever.
pub fn interpret_device_token(
    status: reqwest::StatusCode,
    body: &serde_json::Value,
) -> DeviceTokenPoll {
    match status.as_u16() {
        200 => DeviceTokenPoll::Authorized(parse_token_response(body)),
        400 | 429 => DeviceTokenPoll::Pending,
        418 => DeviceTokenPoll::Denied,
        410 | 404 | 409 => DeviceTokenPoll::Expired,
        _ => DeviceTokenPoll::Expired,
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
            let tmdb_id = tmdb_id_of(e.get(key)?)?;
            Some(TraktItem {
                media_type: media_type.clone(),
                tmdb_id,
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
    for e in arr {
        let item = match e.get("type").and_then(|t| t.as_str()) {
            Some("movie") => e
                .get("movie")
                .and_then(tmdb_id_of)
                .map(|tmdb_id| TraktItem {
                    media_type: MediaType::Movie,
                    tmdb_id,
                }),
            Some("episode") => e.get("show").and_then(tmdb_id_of).map(|tmdb_id| TraktItem {
                media_type: MediaType::Show,
                tmdb_id,
            }),
            _ => None,
        };
        if let Some(item) = item {
            if !out.contains(&item) {
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

/// Parse a `/sync/watched/shows` array into per-show watched-episode `(season, episode)` sets.
/// Shows without a numeric tmdb, and seasons/episodes without a numeric `number`, are skipped.
pub fn parse_watched_shows(v: &serde_json::Value) -> Vec<WatchedShow> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let tmdb_id = e.get("show").and_then(tmdb_id_of)?;
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
            Some(WatchedShow {
                tmdb_id,
                watched_episodes,
            })
        })
        .collect()
}

// --- Test-only mock -------------------------------------------------------------------------

/// Test-only client returning canned values (mirrors `MockScraper`). The `fail_reads`/
/// `fail_refresh` flags inject errors so callers (e.g. `sync_trakt`) can exercise their
/// failure-handling paths; when both are false the canned values are returned unchanged.
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
    /// When true, `watchlist`/`in_progress`/`watched` return `Err`.
    pub fail_reads: bool,
    /// When true, `refresh` returns `Err`.
    pub fail_refresh: bool,
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
        Ok(self.poll.clone())
    }
    async fn refresh(&self, _refresh_token: &str) -> Result<TraktTokenResponse, AppError> {
        if self.fail_refresh {
            return Err(AppError::Config("mock trakt refresh failure".to_string()));
        }
        Ok(self.token.clone())
    }
    async fn me(&self, _access_token: &str) -> Result<TraktUser, AppError> {
        if self.fail_reads {
            return Err(Self::read_error());
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
        // 404/409 → Expired (invalid/used code, terminal)
        assert_eq!(
            interpret_device_token(StatusCode::NOT_FOUND, &empty),
            DeviceTokenPoll::Expired
        );
        assert_eq!(
            interpret_device_token(StatusCode::CONFLICT, &empty),
            DeviceTokenPoll::Expired
        );
        // 429 defensively → Pending (the retry helper normally absorbs this)
        assert_eq!(
            interpret_device_token(StatusCode::TOO_MANY_REQUESTS, &empty),
            DeviceTokenPoll::Pending
        );
        // 500 → Expired: a 5xx surfacing here after exhausted retries is treated as terminal
        assert_eq!(
            interpret_device_token(StatusCode::INTERNAL_SERVER_ERROR, &empty),
            DeviceTokenPoll::Expired
        );
    }

    #[test]
    fn parse_watchlist_movies_extracts_tmdb_and_skips_missing() {
        let v = serde_json::json!([
            { "movie": { "title": "Inception", "ids": { "tmdb": 27205 } } },
            { "movie": { "title": "No TMDB", "ids": { "imdb": "tt1" } } },
            { "movie": { "title": "String TMDB", "ids": { "tmdb": "27205" } } }, // string → as_u64() returns None → skipped
        ]);
        assert_eq!(
            parse_watchlist(&v, MediaType::Movie),
            vec![TraktItem {
                media_type: MediaType::Movie,
                tmdb_id: 27205
            }]
        );
    }

    #[test]
    fn parse_watchlist_shows_sets_show_media_type() {
        let v = serde_json::json!([
            { "show": { "title": "Breaking Bad", "ids": { "tmdb": 1396 } } },
        ]);
        assert_eq!(
            parse_watchlist(&v, MediaType::Show),
            vec![TraktItem {
                media_type: MediaType::Show,
                tmdb_id: 1396
            }]
        );
    }

    #[test]
    fn parse_playback_dedupes_show_and_keeps_movie() {
        let v = serde_json::json!([
            { "type": "movie", "movie": { "ids": { "tmdb": 100 } } },
            { "type": "episode", "show": { "ids": { "tmdb": 200 } } },
            { "type": "episode", "show": { "ids": { "tmdb": 200 } } },
            { "type": "episode", "show": { "ids": { "imdb": "tt9" } } }, // no tmdb → skipped
            { "movie": { "ids": { "tmdb": 999 } } },                     // no "type" field → skipped
        ]);
        assert_eq!(
            parse_playback(&v),
            vec![
                TraktItem {
                    media_type: MediaType::Movie,
                    tmdb_id: 100
                },
                TraktItem {
                    media_type: MediaType::Show,
                    tmdb_id: 200
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
    fn parse_watched_shows_collects_episode_pairs_across_seasons() {
        let v = serde_json::json!([
            {
                "show": { "ids": { "tmdb": 1396 } },
                "seasons": [
                    { "number": 1, "episodes": [{ "number": 1 }, { "number": 2 }] },
                    { "number": 2, "episodes": [{ "number": 1 }] },
                ]
            }
        ]);
        assert_eq!(
            parse_watched_shows(&v),
            vec![WatchedShow {
                tmdb_id: 1396,
                watched_episodes: vec![(1, 1), (1, 2), (2, 1)]
            }]
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
            }],
            in_progress: vec![TraktItem {
                media_type: MediaType::Show,
                tmdb_id: 2,
            }],
            watched: WatchedData {
                movies: vec![3],
                shows: vec![],
            },
            fail_reads: false,
            fail_refresh: false,
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
                tmdb_id: 1
            }]
        );
        assert_eq!(
            client.in_progress("ignored").await.unwrap(),
            vec![TraktItem {
                media_type: MediaType::Show,
                tmdb_id: 2
            }]
        );
        assert_eq!(client.watched("ignored").await.unwrap().movies, vec![3]);
    }

    #[tokio::test]
    async fn mock_trakt_fail_reads_errors_reads_and_fail_refresh_errors_refresh() {
        let mock = MockTrakt {
            watchlist: vec![TraktItem {
                media_type: MediaType::Movie,
                tmdb_id: 1,
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
