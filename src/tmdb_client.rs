use crate::error::AppError;
use crate::ratelimit::MAX_RETRY_AFTER_SECS;
use rand::RngExt;
use reqwest::{Client, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, error, warn};

/// Deserialize a possibly-null/absent/mistyped field to its `Default` (TMDB occasionally sends
/// `popularity: null`; a plain `#[serde(default)]` covers a MISSING key but NOT a present `null`,
/// which would otherwise fail the whole response decode).
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<T>(value).unwrap_or_default())
}

#[derive(Debug, Deserialize, Clone)]
pub struct TmdbSearchResult {
    // `u64` to match every other TMDB id in the codebase (`find_by_imdb`/`details`/`show_status`/
    // the store/acquire layer all key on `tmdb_id: u64`); a `u32` here was a latent narrowing
    // footgun at the identification boundary even though real TMDB ids sit far below `u32::MAX`.
    pub id: u64,
    // `default` so a result missing BOTH `title` and `name` decodes with an empty title (scored 0
    // and harmlessly ignored) instead of failing the whole result page (losing every other result).
    #[serde(default, alias = "name")]
    pub title: String,
    // `alias = "original_name"` catches the TV variant; the field name already matches the movie key
    // `original_title`, so no self-alias is needed.
    #[serde(alias = "original_name")]
    pub original_title: Option<String>,
    #[serde(alias = "first_air_date")]
    pub release_date: Option<String>,
    #[serde(default, deserialize_with = "null_to_default")]
    pub popularity: f64,
    pub vote_average: Option<f64>,
    pub vote_count: Option<u32>,
}

/// Deserialize the results array LENIENTLY: parse each element independently and DROP any that fail
/// (a row missing `id`, or with a malformed `vote_count`/`vote_average`) rather than failing the
/// whole page. `title`/`popularity` were already field-hardened so one bad element can't sink the
/// page; this extends that to `id`/`vote_*` at ROW granularity — losing one row is far cheaper than
/// losing every other (valid) result and failing an identification that a good match on the page
/// would have satisfied.
fn deserialize_lenient_results<'de, D>(deserializer: D) -> Result<Vec<TmdbSearchResult>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let rows = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(rows
        .into_iter()
        .filter_map(|v| serde_json::from_value::<TmdbSearchResult>(v).ok())
        .collect())
}

#[derive(Debug, Deserialize)]
struct TmdbResponse {
    #[serde(default, deserialize_with = "deserialize_lenient_results")]
    pub results: Vec<TmdbSearchResult>,
}

/// The outcome of a TMDB search. `Results` (possibly empty) is a genuine answer — an empty vec is a
/// real "no such title on TMDB". `Unavailable` means the request itself failed: a transport error,
/// a 429 or a 5xx after the retry budget (a TMDB outage), another 4xx at once (e.g. a rejected API
/// key), or a body that doesn't decode. That is NOT the same as "no match": `identify_name`
/// short-circuits its multi-stage fallback ladder on `Unavailable` so a single
/// unidentifiable-during-outage torrent doesn't re-issue a full ~2.5-min retry cycle per stage
/// (~10 min total), and makes no pick from what the other searches returned. Without the
/// distinction every stage sees "empty" and retries the whole outage.
#[derive(Debug)]
pub enum SearchOutcome {
    Results(Vec<TmdbSearchResult>),
    Unavailable,
}

impl SearchOutcome {
    /// Extract the results, setting `*unavailable = true` when the search FAILED (so the caller can
    /// stop issuing further doomed fallback searches). A failure yields an empty result set.
    pub fn into_results(self, unavailable: &mut bool) -> Vec<TmdbSearchResult> {
        match self {
            SearchOutcome::Results(v) => v,
            SearchOutcome::Unavailable => {
                *unavailable = true;
                Vec::new()
            }
        }
    }
}

tokio::task_local! {
    /// Set by [`TmdbClient::search`] when a search FAILED, inside a [`track_search_failures`] scope.
    static SEARCH_FAILED: std::cell::Cell<bool>;
}

/// Run `fut`, also reporting whether any TMDB search inside it FAILED (the request itself: a
/// transport error, a 5xx after the retry budget, or a 4xx), as opposed to a genuine empty result.
/// `identification::identify_torrent` returns the same name-derived fallback (`external_id: None`)
/// for both, so this is how the scan loop tells "TMDB has no such title" (cache it) from "TMDB could
/// not be reached" (identify it again later). Scoped to `fut`'s own polls, so a search failing in
/// another job never counts.
pub async fn track_search_failures<F: std::future::Future>(fut: F) -> (F::Output, bool) {
    SEARCH_FAILED
        .scope(std::cell::Cell::new(false), async move {
            let out = fut.await;
            (out, SEARCH_FAILED.with(|failed| failed.get()))
        })
        .await
}

/// A well-formed IMDB id: `tt` followed by ≥1 ASCII digit (e.g. `tt0111161`). Guards `find_by_imdb`
/// and the scraper's addon URL so a malformed/hostile value can't be interpolated into a request
/// path (unlike the numeric `tmdb_id` paths, which can't carry a URL-reserved char).
pub(crate) fn is_well_formed_imdb_id(id: &str) -> bool {
    matches!(id.strip_prefix("tt"), Some(digits) if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether a TMDB request error is PERMANENT for that request: a per-request verdict such as an
/// unknown id (a 4xx other than 401, 403 and 429). Everything else is transient and worth retrying
/// later: a 429, a 5xx after the retry budget, a transport error, and a rejected key (401/403),
/// which fails every request alike and clears once `TMDB_API_KEY` is fixed, so it says nothing
/// about the title (identification likewise treats it as no verdict).
pub(crate) fn error_is_permanent(e: &reqwest::Error) -> bool {
    use reqwest::StatusCode;
    e.status().is_some_and(|s| {
        s.is_client_error()
            && !matches!(
                s,
                StatusCode::TOO_MANY_REQUESTS | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            )
    })
}

/// Minimum interval between TMDB requests (TMDB allows ~40 req/s; 100ms is conservative).
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(100);

/// The TMDB v3 API root every request is built on.
const TMDB_API_BASE: &str = "https://api.themoviedb.org/3";

pub struct TmdbClient {
    client: Client,
    api_key: String,
    /// The API root (no trailing `/`): [`TMDB_API_BASE`], or a loopback server in tests.
    base_url: String,
    last_request: Mutex<Instant>,
}

impl TmdbClient {
    /// Construct a TMDB client. Returns a configuration error (rather than panicking) if the
    /// HTTP client cannot be built, matching `RealDebridClient::new`/`TorBoxClient::new`.
    pub fn new(api_key: String) -> Result<Self, AppError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AppError::Config(format!("Failed to build TMDB HTTP client: {}", e)))?;
        Ok(Self {
            client,
            api_key,
            base_url: TMDB_API_BASE.to_string(),
            // Start in the past so the first request fires immediately.
            last_request: Mutex::new(Instant::now() - MIN_REQUEST_INTERVAL),
        })
    }

    /// A client whose requests go to `base_url` (e.g. a loopback test server) instead of TMDB, so
    /// unit tests never reach the real API.
    #[cfg(test)]
    pub(crate) fn with_base_url(api_key: String, base_url: String) -> Result<Self, AppError> {
        let mut client = Self::new(api_key)?;
        client.base_url = base_url.trim_end_matches('/').to_string();
        Ok(client)
    }

    /// A client for unit tests that must never reach TMDB: every request goes to a loopback server
    /// that answers `404` at once (a client error, so nothing is retried or slept on).
    #[cfg(test)]
    pub(crate) async fn offline() -> Self {
        let server = crate::trakt_client::test_http::spawn(Vec::new()).await;
        Self::with_base_url("k".into(), server.base_url).unwrap()
    }

    pub async fn search_movie(&self, query: &str, year: Option<&str>) -> SearchOutcome {
        let url = format!("{}/search/movie", self.base_url);
        let mut params = vec![("api_key", self.api_key.as_str()), ("query", query)];
        let year_string;
        if let Some(y) = year {
            year_string = y.to_string();
            params.push(("primary_release_year", &year_string));
        }
        self.search(&url, params).await
    }

    pub async fn search_tv(&self, query: &str, year: Option<&str>) -> SearchOutcome {
        let url = format!("{}/search/tv", self.base_url);
        let mut params = vec![("api_key", self.api_key.as_str()), ("query", query)];
        let year_string;
        if let Some(y) = year {
            year_string = y.to_string();
            params.push(("first_air_date_year", &year_string));
        }
        self.search(&url, params).await
    }

    async fn search(&self, url: &str, params: Vec<(&str, &str)>) -> SearchOutcome {
        match self
            .fetch_with_retry::<TmdbResponse>(|| self.client.get(url).query(&params))
            .await
        {
            Ok(resp) => SearchOutcome::Results(resp.results),
            Err(e) => {
                // A failed request is NOT "no match" — surface it as `Unavailable` so `identify_name`
                // stops issuing further doomed fallback searches during a TMDB outage, and flag it to
                // a `track_search_failures` caller (outside one, there is nothing to flag).
                error!("TMDB search failed: {}", e.without_url());
                let _ = SEARCH_FAILED.try_with(|failed| failed.set(true));
                SearchOutcome::Unavailable
            }
        }
    }

    /// Resolve an IMDB id (`tt…`) to (tmdb_id, kind) via TMDB /find.
    pub async fn find_by_imdb(
        &self,
        imdb_id: &str,
    ) -> Result<Option<(u64, crate::vfs::MediaType)>, reqwest::Error> {
        // Refuse a malformed id rather than interpolate it into the path: TMDB /find is IMDB-keyed
        // (`tt<digits>`), so a non-conforming value can only produce a failed request anyway.
        if !is_well_formed_imdb_id(imdb_id) {
            debug!(
                "find_by_imdb: skipping malformed IMDB id {:?} (expected `tt<digits>`)",
                imdb_id
            );
            return Ok(None);
        }
        let url = format!("{}/find/{}", self.base_url, imdb_id);
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client.get(&url).query(&[
                    ("api_key", self.api_key.as_str()),
                    ("external_source", "imdb_id"),
                ])
            })
            .await?;
        Ok(parse_find_response(&v))
    }

    /// Fetch a title's [`TitleDetails`] (title, year, original + spoken languages) for a TMDB id.
    pub async fn details(
        &self,
        tmdb_id: u64,
        kind: crate::vfs::MediaType,
    ) -> Result<TitleDetails, reqwest::Error> {
        let path = match kind {
            crate::vfs::MediaType::Movie => "movie",
            crate::vfs::MediaType::Show => "tv",
        };
        let url = format!("{}/{}/{}", self.base_url, path, tmdb_id);
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client
                    .get(&url)
                    .query(&[("api_key", self.api_key.as_str())])
            })
            .await?;
        Ok(parse_details(&v, kind))
    }

    /// Resolve a TMDB id to its IMDB id via /{type}/{id}/external_ids.
    pub async fn external_imdb_id(
        &self,
        tmdb_id: u64,
        kind: crate::vfs::MediaType,
    ) -> Result<Option<String>, reqwest::Error> {
        let path = match kind {
            crate::vfs::MediaType::Movie => "movie",
            crate::vfs::MediaType::Show => "tv",
        };
        let url = format!("{}/{}/{}/external_ids", self.base_url, path, tmdb_id);
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client
                    .get(&url)
                    .query(&[("api_key", self.api_key.as_str())])
            })
            .await?;
        Ok(parse_external_ids(&v))
    }

    /// Fetch a series' production status from TMDB `/tv/{id}` (the `status` field).
    /// Network/HTTP failure → Err; an unrecognised or missing status → Ok(ShowStatus::Other).
    pub async fn show_status(&self, tmdb_id: u64) -> Result<ShowStatus, reqwest::Error> {
        let url = format!("{}/tv/{}", self.base_url, tmdb_id);
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client
                    .get(&url)
                    .query(&[("api_key", self.api_key.as_str())])
            })
            .await?;
        Ok(parse_show_status(&v))
    }

    /// Fetch a season's episode air dates from TMDB `/tv/{id}/season/{season}`.
    pub async fn season_air_dates(
        &self,
        tmdb_id: u64,
        season: u32,
    ) -> Result<Vec<EpisodeAirDate>, reqwest::Error> {
        let url = format!("{}/tv/{}/season/{}", self.base_url, tmdb_id, season);
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client
                    .get(&url)
                    .query(&[("api_key", self.api_key.as_str())])
            })
            .await?;
        Ok(parse_season_air_dates(&v, season))
    }

    /// Enumerate a series' (non-Specials) season numbers from TMDB `/tv/{id}` details.
    pub async fn show_season_numbers(&self, tmdb_id: u64) -> Result<Vec<u32>, reqwest::Error> {
        let url = format!("{}/tv/{}", self.base_url, tmdb_id);
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client
                    .get(&url)
                    .query(&[("api_key", self.api_key.as_str())])
            })
            .await?;
        Ok(parse_season_numbers(&v))
    }

    async fn fetch_with_retry<T: DeserializeOwned>(
        &self,
        make_request: impl Fn() -> RequestBuilder,
    ) -> Result<T, reqwest::Error> {
        let mut last_error: Option<reqwest::Error> = None;
        let max_attempts = 10;
        // Set when a retryable-status iteration already slept its explicit `Retry-After`/capped wait
        // below; suppresses the next iteration's exponential backoff so a 429/503/502/504 retry waits
        // ONCE (the Retry-After), not `Retry-After + backoff`. Transport-error/other-5xx retries leave
        // it false, so they still get the exponential backoff.
        let mut waited_explicitly = false;

        for attempt in 1..=max_attempts {
            if attempt > 1 && !waited_explicitly {
                let backoff = (2u64.saturating_pow(attempt as u32 - 2) * 1000).min(30_000);
                let jitter = rand::rng().random_range(0..500);
                tokio::time::sleep(Duration::from_millis(backoff + jitter)).await;
            }
            waited_explicitly = false;

            // Proactive rate limiting: pace requests MIN_REQUEST_INTERVAL apart. RESERVE the next
            // slot under the lock and release it BEFORE sleeping, so concurrent callers (e.g. the
            // `tokio::join!`ed search_tv + search_movie) serialise on the cheap reservation rather
            // than blocking on the lock held across the pacing sleep — preserving real concurrency
            // while still spacing the actual requests. (Mirrors `ratelimit::AdaptiveRateLimiter`.)
            let wait = {
                let mut last = self.last_request.lock().await;
                let now = Instant::now();
                let next = (*last + MIN_REQUEST_INTERVAL).max(now);
                *last = next;
                next.saturating_duration_since(now)
            };
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }

            match make_request().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
                        || status == reqwest::StatusCode::BAD_GATEWAY
                        || status == reqwest::StatusCode::GATEWAY_TIMEOUT
                    {
                        let retry_after = resp
                            .headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(1);
                        let capped = std::cmp::min(retry_after, MAX_RETRY_AFTER_SECS);
                        warn!(
                            "TMDB API returned {} (attempt {}/{}). Waiting {}s",
                            status, attempt, max_attempts, capped
                        );
                        // On the FINAL attempt there's nothing to wait for — the loop exits right
                        // after. Skip the (up to MAX_RETRY_AFTER_SECS) sleep so a TMDB throttle can't
                        // block the scan hot path for nothing (matches `rd_client::wait_for_retry`).
                        if attempt < max_attempts {
                            tokio::time::sleep(Duration::from_secs(capped)).await;
                            waited_explicitly = true; // skip the next iteration's backoff (no double wait)
                        }
                        // Record the real status error so that, on exhaustion, the surfaced error is
                        // the actual persistent 503/429/etc rather than the synthetic 502 fallback
                        // below (which would misattribute a sustained TMDB outage). Mirrors rd_client.
                        last_error = resp.error_for_status_ref().err().map(|e| e.without_url());
                        continue; // Without this, the same error response falls through to error_for_status
                    }

                    match resp.error_for_status() {
                        // Strip the URL on the decode path too: the request URL carries the
                        // `api_key` query param, so a body-decode error must not leak it to logs.
                        Ok(resp) => return resp.json::<T>().await.map_err(|e| e.without_url()),
                        Err(e) => {
                            let e = e.without_url();
                            // A 4xx client error (401 auth, 403, 404, 400) will NOT fix itself on
                            // retry — return immediately rather than burning all `max_attempts`
                            // rate-limited attempts (plus exponential backoff) on it. 429 is handled
                            // above; a 5xx falls through to retry. (Matches rd_client's terminal-4xx
                            // behaviour; also stops a bad-key test from taking minutes.)
                            if status.is_client_error() {
                                warn!(
                                    "TMDB API client error (attempt {}/{}): {} — not retrying",
                                    attempt, max_attempts, e
                                );
                                return Err(e);
                            }
                            warn!(
                                "TMDB API error (attempt {}/{}): {}",
                                attempt, max_attempts, e
                            );
                            last_error = Some(e);
                        }
                    }
                }
                Err(e) => {
                    let e = e.without_url();
                    warn!(
                        "TMDB request failed (attempt {}/{}): {}",
                        attempt, max_attempts, e
                    );
                    last_error = Some(e);
                }
            }
        }

        if let Some(e) = last_error {
            Err(e)
        } else {
            // DEFENSIVE-ONLY fallback: in practice unreachable, because every retryable-status
            // iteration records the real `error_for_status_ref()` error into `last_error` above, so a
            // budget exhausted purely on retryable statuses surfaces that REAL status via the branch
            // above. This synthetic error only guards a future refactor that stopped recording it —
            // it must never panic.
            error!(
                "TMDB fetch_with_retry: all {} attempts exhausted with no recorded error (unexpected)",
                max_attempts
            );
            Err(synthetic_exhausted_error())
        }
    }
}

/// The parts of a TMDB movie/tv details object the acquisition path needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitleDetails {
    pub title: String,
    /// The 4-digit release / first-air year, when TMDB has a full date.
    pub year: Option<String>,
    /// TMDB's `original_language` (ISO 639-1, as TMDB sends it).
    pub original_language: Option<String>,
    /// TMDB's `spoken_languages[].iso_639_1`, lowercased, empties dropped, in TMDB's order. Empty when
    /// TMDB lists none (or omits the key).
    pub spoken_languages: Vec<String>,
}

/// Parse a TMDB movie/tv details object into [`TitleDetails`].
pub(crate) fn parse_details(v: &serde_json::Value, kind: crate::vfs::MediaType) -> TitleDetails {
    let title = match kind {
        crate::vfs::MediaType::Movie => v.get("title"),
        crate::vfs::MediaType::Show => v.get("name"),
    }
    .and_then(|t| t.as_str())
    .unwrap_or("")
    .to_string();
    let date = match kind {
        crate::vfs::MediaType::Movie => v.get("release_date"),
        crate::vfs::MediaType::Show => v.get("first_air_date"),
    }
    .and_then(|d| d.as_str())
    .unwrap_or("");
    let year = date
        .split('-')
        .next()
        .filter(|y| y.len() == 4)
        .map(String::from);
    let original_language = v
        .get("original_language")
        .and_then(|l| l.as_str())
        .map(String::from);
    let spoken_languages = v
        .get("spoken_languages")
        .and_then(|a| a.as_array())
        .map(|langs| {
            langs
                .iter()
                .filter_map(|l| l.get("iso_639_1").and_then(|c| c.as_str()))
                .map(|c| c.trim().to_ascii_lowercase())
                .filter(|c| !c.is_empty())
                .collect()
        })
        .unwrap_or_default();
    TitleDetails {
        title,
        year,
        original_language,
        spoken_languages,
    }
}

/// Parse TMDB `/find/{imdb_id}?external_source=imdb_id` into (tmdb_id, kind). Movie wins over TV.
pub(crate) fn parse_find_response(v: &serde_json::Value) -> Option<(u64, crate::vfs::MediaType)> {
    if let Some(id) = v
        .get("movie_results")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("id"))
        .and_then(|i| i.as_u64())
    {
        return Some((id, crate::vfs::MediaType::Movie));
    }
    if let Some(id) = v
        .get("tv_results")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("id"))
        .and_then(|i| i.as_u64())
    {
        return Some((id, crate::vfs::MediaType::Show));
    }
    None
}

/// Parse `/{type}/{id}/external_ids` into the IMDB id (non-empty).
pub(crate) fn parse_external_ids(v: &serde_json::Value) -> Option<String> {
    v.get("imdb_id")
        .and_then(|i| i.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Build the synthetic Bad Gateway error returned when every retry attempt hit a retryable
/// status code, so the function surfaces an error instead of panicking.
fn synthetic_exhausted_error() -> reqwest::Error {
    crate::provider::synthetic_bad_gateway(b"all attempts exhausted: retryable status codes")
}

/// TMDB series production status, collapsed to the three buckets the removal
/// lifecycle cares about. `Ended` = no more episodes coming (finished/cancelled);
/// `Returning` = still producing / will return; `Other` = anything else (planned, unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ShowStatus {
    Ended,
    Returning,
    Other,
}

/// One episode's air date within a season. `air_date` is None when TMDB has no
/// date yet (null/missing/unparseable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeAirDate {
    pub season: u32,
    pub episode: u32,
    pub air_date: Option<chrono::NaiveDate>,
}

/// Parse the top-level `status` string field from a TMDB `/tv/{id}` response into a `ShowStatus`.
/// Matching is case-insensitive and the value is trimmed. Unrecognised, empty, or missing → `Other`.
pub(crate) fn parse_show_status(v: &serde_json::Value) -> ShowStatus {
    let raw = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    match raw.as_str() {
        "ended" | "canceled" | "cancelled" => ShowStatus::Ended,
        "returning series" | "in production" => ShowStatus::Returning,
        _ => ShowStatus::Other,
    }
}

/// Parse the `episodes` array from a TMDB `/tv/{id}/season/{season}` response into a
/// `Vec<EpisodeAirDate>`. Elements missing a numeric `episode_number` are skipped.
/// `air_date` values that are null, missing, empty, or unparseable yield `None`.
pub(crate) fn parse_season_air_dates(v: &serde_json::Value, season: u32) -> Vec<EpisodeAirDate> {
    let Some(episodes) = v.get("episodes").and_then(|e| e.as_array()) else {
        return Vec::new();
    };
    episodes
        .iter()
        .filter_map(|ep| {
            let episode = u32::try_from(ep.get("episode_number")?.as_u64()?).ok()?;
            let air_date = ep
                .get("air_date")
                .and_then(|d| d.as_str())
                .filter(|s| !s.is_empty())
                .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
            Some(EpisodeAirDate {
                season,
                episode,
                air_date,
            })
        })
        .collect()
}

/// Parse the `seasons` array from a TMDB `/tv/{id}` details response into season numbers.
/// Season 0 (TMDB "Specials") is excluded; entries with a missing or non-numeric
/// `season_number` are skipped. The result is sorted ascending and de-duplicated.
pub(crate) fn parse_season_numbers(v: &serde_json::Value) -> Vec<u32> {
    let Some(seasons) = v.get("seasons").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    let mut nums: Vec<u32> = seasons
        .iter()
        .filter_map(|s| u32::try_from(s.get("season_number")?.as_u64()?).ok())
        .filter(|&n| n != 0)
        .collect();
    nums.sort_unstable();
    nums.dedup();
    nums
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `track_search_failures` tells a failed search (here a `404`) from a genuine empty result, and
    /// a search outside its scope (or in another task) is not counted.
    #[tokio::test]
    async fn track_search_failures_reports_a_failed_search_but_not_an_empty_result() {
        use crate::trakt_client::test_http::{spawn, Reply};
        let ok = spawn(vec![(
            "/search/movie?api_key=k&query=Zzqx",
            Reply::json(r#"{"results":[]}"#),
        )])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), ok.base_url.clone()).unwrap();
        let (out, failed) = track_search_failures(tmdb.search_movie("Zzqx", None)).await;
        assert!(matches!(out, SearchOutcome::Results(ref v) if v.is_empty()));
        assert!(!failed, "an empty result is a genuine no-match");

        let (out, failed) = track_search_failures(tmdb.search_tv("Zzqx", None)).await;
        assert!(matches!(out, SearchOutcome::Unavailable));
        assert!(failed, "an unanswered search is a failure");

        // Outside a scope a failure is simply not tracked (and must not panic).
        assert!(matches!(
            tmdb.search_tv("Zzqx", None).await,
            SearchOutcome::Unavailable
        ));
        assert_eq!(ok.hits("/search/movie"), 1);
    }

    #[test]
    fn tmdb_response_drops_a_malformed_row_but_keeps_valid_results() {
        // One bad element (missing `id`, or a `vote_count` that isn't a non-negative int) must NOT
        // sink the whole page: the valid rows survive and identification still sees the good match.
        let json = r#"{"results":[
            {"title":"Good","id":42},
            {"title":"NoIdRow","popularity":9.9},
            {"id":9,"title":"NullVotes","vote_count":null,"vote_average":null},
            {"id":11,"title":"BadVotes","vote_count":-1},
            {"id":7,"title":"AlsoGood","vote_count":100,"vote_average":8.1}
        ]}"#;
        let resp: TmdbResponse = serde_json::from_str(json).unwrap();
        let ids: Vec<u64> = resp.results.iter().map(|r| r.id).collect();
        // 42 & 7 (fully valid) and 9 (null votes → Option::None) survive; the id-less row and the
        // negative-vote_count row (11) are dropped rather than failing the whole page.
        assert_eq!(ids, vec![42, 9, 7]);
    }

    #[test]
    fn search_outcome_into_results_flags_only_unavailable() {
        // A genuine (possibly empty) result set leaves the flag untouched — an empty vec is a real
        // "no match", not an outage, so identification keeps trying its fallback stages.
        let mut flag = false;
        assert!(SearchOutcome::Results(Vec::new())
            .into_results(&mut flag)
            .is_empty());
        assert!(!flag, "an empty result set is NOT unavailable");

        let one = SearchOutcome::Results(vec![TmdbSearchResult {
            id: 42,
            title: "X".into(),
            original_title: None,
            release_date: None,
            popularity: 0.0,
            vote_average: None,
            vote_count: None,
        }]);
        assert_eq!(one.into_results(&mut flag).len(), 1);
        assert!(!flag);

        // A failed request sets the flag (so `identify_name` short-circuits the rest of its ladder)
        // and still yields an empty result set. Once set, a later Results must not clear it.
        assert!(SearchOutcome::Unavailable
            .into_results(&mut flag)
            .is_empty());
        assert!(flag, "a failed search must flag unavailable");
        let _ = SearchOutcome::Results(Vec::new()).into_results(&mut flag);
        assert!(flag, "the unavailable flag is sticky across later searches");
    }

    #[tokio::test]
    async fn find_by_imdb_rejects_a_malformed_id_without_a_request() {
        // A malformed id short-circuits to Ok(None) before any request — no path interpolation.
        // The client points at a loopback server, which must see no request at all.
        let server = crate::trakt_client::test_http::spawn(Vec::new()).await;
        let c = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        assert_eq!(c.find_by_imdb("../evil").await.unwrap(), None);
        assert_eq!(c.find_by_imdb("").await.unwrap(), None);
        // Prefix but no digits.
        assert_eq!(c.find_by_imdb("tt").await.unwrap(), None);
        // Non-digit tail.
        assert_eq!(c.find_by_imdb("tt12a34").await.unwrap(), None);
        assert_eq!(server.requests(), Vec::<String>::new(), "no TMDB request");
        // Well-formed ids pass the shape gate (this asserts the guard, not the network round-trip).
        assert!(is_well_formed_imdb_id("tt0111161"));
        assert!(is_well_formed_imdb_id("tt15239678"));
    }

    #[test]
    fn synthetic_exhausted_error_is_a_bad_gateway_error_not_a_panic() {
        // Actually executes the exhausted-retries fallback (the source-grep test below only
        // checks the text). It must return an error carrying 502, never panic.
        let err = synthetic_exhausted_error();
        assert!(err.is_status());
        assert_eq!(err.status(), Some(reqwest::StatusCode::BAD_GATEWAY));
        // The log line names the cause, not reqwest's placeholder URL.
        let shown = err.to_string();
        assert!(shown.contains("all attempts exhausted"), "{shown}");
        assert!(!shown.contains("no.url.provided"), "{shown}");
    }

    #[test]
    fn fetch_with_retry_no_panic_on_exhausted_retries() {
        // Verify that the old `.expect()` panic path has been replaced with
        // a synthetic error. If someone reintroduces the pattern in the retry
        // loop fallback, this test will fail.
        // Split the search string so this test doesn't self-match.
        let needle = ["last_error", ".expect("].concat();
        let source = include_str!("tmdb_client.rs");
        assert!(
            !source.contains(&needle),
            "tmdb_client.rs fetch_with_retry must not use .expect() on last_error — \
             use a synthetic error response instead to avoid panicking when all \
             attempts are exhausted by retryable status codes"
        );
    }

    /// Loopback server that replies to every request with `status` and counts the requests it saw.
    async fn spawn_counting_status(
        status: u16,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = counter.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..12 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let head = format!("HTTP/1.1 {} STATUS\r\nContent-Length: 0\r\n\r\n", status);
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}/", addr), counter)
    }

    #[tokio::test]
    async fn fetch_with_retry_does_not_retry_4xx_client_errors() {
        // A permanent 4xx (e.g. 401 bad key) must return after exactly ONE request, not burn all
        // 10 rate-limited attempts with backoff (which previously made bad-key tests take minutes).
        let (url, counter) = spawn_counting_status(401).await;
        let client = TmdbClient::new("k".into()).unwrap();
        let r: Result<serde_json::Value, _> =
            client.fetch_with_retry(|| client.client.get(&url)).await;
        assert!(r.is_err(), "a 401 must surface as an error");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a 4xx client error must not be retried"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_with_retry_surfaces_real_5xx_status_after_exhaustion() {
        // A persistent retryable 5xx must, on exhaustion, surface the REAL status (503) rather
        // than the synthetic 502 fallback — otherwise a sustained TMDB outage would be
        // misattributed as a bad gateway. `start_paused` auto-advances the backoff sleeps so the
        // 10 attempts complete instantly. Mirrors rd_client's analogous regression test.
        let (url, counter) = spawn_counting_status(503).await;
        let client = TmdbClient::new("k".into()).unwrap();
        let r: Result<serde_json::Value, _> =
            client.fetch_with_retry(|| client.client.get(&url)).await;
        let err = r.expect_err("a persistent 503 must surface as an error");
        assert_eq!(
            err.status(),
            Some(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            "the surfaced error must carry the real 503, not the synthetic 502 fallback"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            10,
            "a retryable 5xx must be retried for the full attempt budget"
        );
    }

    #[test]
    fn tmdb_response_deserializes_without_results_field() {
        // TMDB might return a response without a results key (e.g., error responses
        // that happen to return 200, or API format changes). With #[serde(default)],
        // this deserializes with an empty results vec instead of failing.
        let json = r#"{}"#;
        let resp: super::TmdbResponse = serde_json::from_str(json).unwrap();
        assert!(resp.results.is_empty());
    }

    #[test]
    fn tmdb_response_deserializes_with_results() {
        let json = r#"{"results": [{"id": 123, "title": "Test Movie", "popularity": 50.0}]}"#;
        let resp: super::TmdbResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].id, 123);
        assert_eq!(resp.results[0].title, "Test Movie");
    }

    #[test]
    fn tmdb_response_tolerates_null_popularity_and_missing_title() {
        // A present `popularity: null` (or a result missing title/name) must NOT fail the whole
        // page decode — it defaults, so every other result on the page survives.
        let json = r#"{"results": [
            {"id": 1, "title": "Good", "popularity": null},
            {"id": 2, "popularity": 12.5}
        ]}"#;
        let resp: super::TmdbResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp.results.len(),
            2,
            "no result lost to a null/missing field"
        );
        assert_eq!(resp.results[0].popularity, 0.0, "null popularity → default");
        assert_eq!(
            resp.results[1].title, "",
            "missing title → empty (ignored), not a decode error"
        );
    }

    #[test]
    fn tmdb_search_result_handles_missing_optional_fields() {
        // TMDB results may have null or missing optional fields
        let json = r#"{"id": 456, "title": "Minimal"}"#;
        let result: super::TmdbSearchResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.id, 456);
        assert_eq!(result.title, "Minimal");
        assert!(result.original_title.is_none());
        assert!(result.release_date.is_none());
        assert_eq!(result.popularity, 0.0);
        assert!(result.vote_average.is_none());
        assert!(result.vote_count.is_none());
    }

    #[test]
    fn tmdb_search_result_ignores_unknown_fields() {
        // TMDB may add new fields. Verify serde ignores them.
        let json = r#"{
            "id": 789,
            "title": "Test",
            "adult": false,
            "genre_ids": [28, 12],
            "backdrop_path": "/some/path.jpg"
        }"#;
        let result: super::TmdbSearchResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.id, 789);
        assert_eq!(result.title, "Test");
    }

    #[test]
    fn parse_find_response_extracts_tmdb_id_and_kind() {
        let json = serde_json::json!({ "movie_results": [{"id": 27205}], "tv_results": [] });
        assert_eq!(
            super::parse_find_response(&json),
            Some((27205, crate::vfs::MediaType::Movie))
        );
        let json2 = serde_json::json!({ "movie_results": [], "tv_results": [{"id": 1396}] });
        assert_eq!(
            super::parse_find_response(&json2),
            Some((1396, crate::vfs::MediaType::Show))
        );
        let empty = serde_json::json!({"movie_results": [], "tv_results": []});
        assert_eq!(super::parse_find_response(&empty), None);
    }

    #[test]
    fn parse_external_ids_extracts_imdb() {
        let json = serde_json::json!({"imdb_id": "tt0816692"});
        assert_eq!(
            super::parse_external_ids(&json),
            Some("tt0816692".to_string())
        );
        let none = serde_json::json!({"imdb_id": serde_json::Value::Null});
        assert_eq!(super::parse_external_ids(&none), None);
        let empty = serde_json::json!({"imdb_id": ""});
        assert_eq!(super::parse_external_ids(&empty), None);
    }

    #[test]
    fn parse_details_movie_and_show() {
        let m = serde_json::json!({"title": "Inception", "release_date": "2010-07-16", "original_language": "en"});
        assert_eq!(
            super::parse_details(&m, crate::vfs::MediaType::Movie),
            super::TitleDetails {
                title: "Inception".into(),
                year: Some("2010".into()),
                original_language: Some("en".into()),
                spoken_languages: vec![],
            }
        );
        let s = serde_json::json!({"name": "Breaking Bad", "first_air_date": "2008-01-20", "original_language": "en"});
        assert_eq!(
            super::parse_details(&s, crate::vfs::MediaType::Show),
            super::TitleDetails {
                title: "Breaking Bad".into(),
                year: Some("2008".into()),
                original_language: Some("en".into()),
                spoken_languages: vec![],
            }
        );
    }

    #[test]
    fn parse_details_captures_spoken_languages() {
        // A film whose TMDB original_language is `fi` but whose dialogue is mostly English — the spoken
        // languages list carries that, so `AUDIO_LANGUAGE=original` can accept an English release.
        let v = serde_json::json!({
            "title": "Film",
            "release_date": "2026-07-01",
            "original_language": "fi",
            "spoken_languages": [
                {"english_name": "English", "iso_639_1": "en", "name": "English"},
                {"iso_639_1": "FI"},
                {"iso_639_1": "es"}
            ]
        });
        let d = super::parse_details(&v, crate::vfs::MediaType::Movie);
        assert_eq!(d.original_language.as_deref(), Some("fi"));
        assert_eq!(d.spoken_languages, vec!["en", "fi", "es"]);
        // Missing key → empty (behaves exactly as before: original language only).
        let none = serde_json::json!({"title": "X", "original_language": "fi"});
        assert!(super::parse_details(&none, crate::vfs::MediaType::Movie)
            .spoken_languages
            .is_empty());
        // Empty / missing / non-string iso codes are dropped rather than becoming a bogus "" want.
        let junk = serde_json::json!({
            "name": "Y",
            "spoken_languages": [{"iso_639_1": ""}, {"iso_639_1": null}, {"name": "?"}, {"iso_639_1": " it "}]
        });
        assert_eq!(
            super::parse_details(&junk, crate::vfs::MediaType::Show).spoken_languages,
            vec!["it"]
        );
    }

    // --- ShowStatus tests ---

    #[test]
    fn parse_show_status_table_driven() {
        use super::{parse_show_status, ShowStatus};

        let cases: &[(&str, ShowStatus)] = &[
            ("Ended", ShowStatus::Ended),
            ("Returning Series", ShowStatus::Returning),
            ("Canceled", ShowStatus::Ended),
            ("Cancelled", ShowStatus::Ended),
            ("In Production", ShowStatus::Returning),
            ("Planned", ShowStatus::Other),
            ("  Ended  ", ShowStatus::Ended),
            // missing status field
        ];

        for (status_str, expected) in cases {
            let v = serde_json::json!({ "status": status_str });
            assert_eq!(
                parse_show_status(&v),
                *expected,
                "status {:?} should map to {:?}",
                status_str,
                expected
            );
        }

        // Missing status field → Other
        let missing = serde_json::json!({});
        assert_eq!(
            parse_show_status(&missing),
            ShowStatus::Other,
            "missing status → Other"
        );

        // Mixed-case: "ended" (all lower) → Ended
        let lower = serde_json::json!({ "status": "ended" });
        assert_eq!(
            parse_show_status(&lower),
            ShowStatus::Ended,
            "lowercase 'ended' → Ended"
        );

        // Mixed-case: "CANCELED" (all upper) → Ended
        let upper = serde_json::json!({ "status": "CANCELED" });
        assert_eq!(
            parse_show_status(&upper),
            ShowStatus::Ended,
            "uppercase 'CANCELED' → Ended"
        );
    }

    // --- EpisodeAirDate / parse_season_air_dates tests ---

    #[test]
    fn parse_season_air_dates_mixed_episodes() {
        use super::{parse_season_air_dates, EpisodeAirDate};

        let v = serde_json::json!({
            "episodes": [
                { "episode_number": 1, "air_date": "2022-03-15" },
                { "episode_number": 2, "air_date": serde_json::Value::Null },
                { "episode_number": 3 },                              // missing air_date key
                { "episode_number": 4, "air_date": "not-a-date" },   // unparseable
                { "air_date": "2022-03-20" },                         // no episode_number → skipped
            ]
        });

        let result = parse_season_air_dates(&v, 2);

        assert_eq!(result.len(), 4);

        assert_eq!(
            result[0],
            EpisodeAirDate {
                season: 2,
                episode: 1,
                air_date: Some(chrono::NaiveDate::from_ymd_opt(2022, 3, 15).unwrap()),
            }
        );
        assert_eq!(
            result[1],
            EpisodeAirDate {
                season: 2,
                episode: 2,
                air_date: None
            }
        );
        assert_eq!(
            result[2],
            EpisodeAirDate {
                season: 2,
                episode: 3,
                air_date: None
            }
        );
        assert_eq!(
            result[3],
            EpisodeAirDate {
                season: 2,
                episode: 4,
                air_date: None
            }
        );
        // All entries carry the threaded season value
        assert!(result.iter().all(|e| e.season == 2));
    }

    #[test]
    fn parse_season_air_dates_absent_episodes_array() {
        use super::parse_season_air_dates;

        // Missing `episodes` key → empty vec
        let no_key = serde_json::json!({});
        assert!(parse_season_air_dates(&no_key, 1).is_empty());

        // Explicit null → empty vec
        let null_array = serde_json::json!({ "episodes": serde_json::Value::Null });
        assert!(parse_season_air_dates(&null_array, 1).is_empty());

        // Empty array → empty vec
        let empty_array = serde_json::json!({ "episodes": [] });
        assert!(parse_season_air_dates(&empty_array, 1).is_empty());
    }

    // --- parse_season_numbers tests ---

    #[test]
    fn parse_season_numbers_excludes_specials_and_sorts() {
        use super::parse_season_numbers;
        // seasons arrive out of order and include season 0 (Specials), which must be dropped.
        let v = serde_json::json!({
            "seasons": [
                { "season_number": 2 },
                { "season_number": 0 }, // Specials → excluded
                { "season_number": 1 },
            ]
        });
        assert_eq!(parse_season_numbers(&v), vec![1, 2]);
    }

    #[test]
    fn parse_season_numbers_missing_key_is_empty() {
        use super::parse_season_numbers;
        assert_eq!(
            parse_season_numbers(&serde_json::json!({})),
            Vec::<u32>::new()
        );
        // An explicit empty seasons array must also yield an empty vec.
        assert_eq!(
            parse_season_numbers(&serde_json::json!({ "seasons": [] })),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn parse_season_numbers_skips_non_numeric_and_dedups() {
        use super::parse_season_numbers;
        let v = serde_json::json!({
            "seasons": [
                { "season_number": 1 },
                { "season_number": "x" }, // non-numeric → skipped
                { "name": "no number" },  // missing season_number → skipped
                { "season_number": 1 },   // duplicate → de-duped
                { "season_number": 3 },
            ]
        });
        assert_eq!(parse_season_numbers(&v), vec![1, 3]);
    }

    fn http_status_error(status: u16) -> reqwest::Error {
        reqwest::Response::from(
            hyper::Response::builder()
                .status(status)
                .body(hyper::body::Bytes::new())
                .unwrap(),
        )
        .error_for_status()
        .unwrap_err()
    }

    /// A per-request refusal (an unknown id) is permanent. A rejected key (401/403) is not: it fails
    /// every request until `TMDB_API_KEY` is fixed, so it says nothing about the title.
    #[tokio::test]
    async fn tmdb_error_permanence_classification() {
        assert!(error_is_permanent(&http_status_error(404)));
        assert!(error_is_permanent(&http_status_error(400)));
        for transient in [401, 403, 429, 500, 502] {
            assert!(
                !error_is_permanent(&http_status_error(transient)),
                "{transient} must be transient"
            );
        }
        // No HTTP status (transport / decode failure) → transient.
        let decode = reqwest::Response::from(
            hyper::Response::builder()
                .status(200)
                .body(hyper::body::Bytes::from_static(b"not json"))
                .unwrap(),
        )
        .json::<serde_json::Value>()
        .await
        .unwrap_err();
        assert!(decode.status().is_none());
        assert!(!error_is_permanent(&decode));
    }
}
