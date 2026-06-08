use crate::error::AppError;
use rand::Rng;
use reqwest::{Client, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{error, warn};

const MAX_RETRY_AFTER_SECS: u64 = 300; // Cap Retry-After to 5 minutes

#[derive(Debug, Deserialize, Clone)]
pub struct TmdbSearchResult {
    pub id: u32,
    #[serde(alias = "name")]
    pub title: String,
    #[serde(alias = "original_name", alias = "original_title")]
    pub original_title: Option<String>,
    #[serde(alias = "first_air_date")]
    pub release_date: Option<String>,
    #[serde(default)]
    pub popularity: f64,
    pub vote_average: Option<f64>,
    pub vote_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TmdbResponse {
    #[serde(default)]
    pub results: Vec<TmdbSearchResult>,
}

/// Minimum interval between TMDB requests (TMDB allows ~40 req/s; 100ms is conservative).
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(100);

pub struct TmdbClient {
    client: Client,
    api_key: String,
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
            // Start in the past so the first request fires immediately.
            last_request: Mutex::new(Instant::now() - MIN_REQUEST_INTERVAL),
        })
    }

    pub async fn search_movie(&self, query: &str, year: Option<&str>) -> Vec<TmdbSearchResult> {
        let url = "https://api.themoviedb.org/3/search/movie";
        let mut params = vec![("api_key", self.api_key.as_str()), ("query", query)];
        let year_string;
        if let Some(y) = year {
            year_string = y.to_string();
            params.push(("primary_release_year", &year_string));
        }
        self.search(url, params).await
    }

    pub async fn search_tv(&self, query: &str, year: Option<&str>) -> Vec<TmdbSearchResult> {
        let url = "https://api.themoviedb.org/3/search/tv";
        let mut params = vec![("api_key", self.api_key.as_str()), ("query", query)];
        let year_string;
        if let Some(y) = year {
            year_string = y.to_string();
            params.push(("first_air_date_year", &year_string));
        }
        self.search(url, params).await
    }

    async fn search(&self, url: &str, params: Vec<(&str, &str)>) -> Vec<TmdbSearchResult> {
        match self
            .fetch_with_retry::<TmdbResponse>(|| self.client.get(url).query(&params))
            .await
        {
            Ok(resp) => resp.results,
            Err(e) => {
                error!("TMDB search failed: {}", e.without_url());
                Vec::new()
            }
        }
    }

    /// Resolve an IMDB id (`tt…`) to (tmdb_id, kind) via TMDB /find.
    pub async fn find_by_imdb(&self, imdb_id: &str) -> Result<Option<(u64, crate::vfs::MediaType)>, reqwest::Error> {
        let url = format!("https://api.themoviedb.org/3/find/{}", imdb_id);
        let api_key = self.api_key.clone();
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client
                    .get(&url)
                    .query(&[("api_key", api_key.as_str()), ("external_source", "imdb_id")])
            })
            .await?;
        Ok(parse_find_response(&v))
    }

    /// Fetch (title, year, original_language) for a TMDB id.
    pub async fn details(&self, tmdb_id: u64, kind: crate::vfs::MediaType) -> Result<(String, Option<String>, Option<String>), reqwest::Error> {
        let path = match kind { crate::vfs::MediaType::Movie => "movie", crate::vfs::MediaType::Show => "tv" };
        let url = format!("https://api.themoviedb.org/3/{}/{}", path, tmdb_id);
        let api_key = self.api_key.clone();
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client.get(&url).query(&[("api_key", api_key.as_str())])
            })
            .await?;
        Ok(parse_details(&v, kind))
    }

    /// Resolve a TMDB id to its IMDB id via /{type}/{id}/external_ids.
    pub async fn external_imdb_id(&self, tmdb_id: u64, kind: crate::vfs::MediaType) -> Result<Option<String>, reqwest::Error> {
        let path = match kind { crate::vfs::MediaType::Movie => "movie", crate::vfs::MediaType::Show => "tv" };
        let url = format!("https://api.themoviedb.org/3/{}/{}/external_ids", path, tmdb_id);
        let api_key = self.api_key.clone();
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client
                    .get(&url)
                    .query(&[("api_key", api_key.as_str())])
            })
            .await?;
        Ok(parse_external_ids(&v))
    }

    /// Fetch a series' production status from TMDB `/tv/{id}` (the `status` field).
    /// Network/HTTP failure → Err; an unrecognised or missing status → Ok(ShowStatus::Other).
    pub async fn show_status(&self, tmdb_id: u64) -> Result<ShowStatus, reqwest::Error> {
        let url = format!("https://api.themoviedb.org/3/tv/{}", tmdb_id);
        let api_key = self.api_key.clone();
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client.get(&url).query(&[("api_key", api_key.as_str())])
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
        let url = format!(
            "https://api.themoviedb.org/3/tv/{}/season/{}",
            tmdb_id, season
        );
        let api_key = self.api_key.clone();
        let v = self
            .fetch_with_retry::<serde_json::Value>(|| {
                self.client.get(&url).query(&[("api_key", api_key.as_str())])
            })
            .await?;
        Ok(parse_season_air_dates(&v, season))
    }

    async fn fetch_with_retry<T: DeserializeOwned>(
        &self,
        make_request: impl Fn() -> RequestBuilder,
    ) -> Result<T, reqwest::Error> {
        let mut last_error: Option<reqwest::Error> = None;
        let max_attempts = 10;

        for attempt in 1..=max_attempts {
            if attempt > 1 {
                let backoff = (2u64.saturating_pow(attempt as u32 - 2) * 1000).min(30_000);
                let jitter = rand::thread_rng().gen_range(0..500);
                tokio::time::sleep(Duration::from_millis(backoff + jitter)).await;
            }

            // Proactive rate limiting: wait until MIN_REQUEST_INTERVAL since last request
            {
                let mut last = self.last_request.lock().await;
                let elapsed = last.elapsed();
                if elapsed < MIN_REQUEST_INTERVAL {
                    tokio::time::sleep(MIN_REQUEST_INTERVAL - elapsed).await;
                }
                *last = Instant::now();
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
                        tokio::time::sleep(Duration::from_secs(capped)).await;
                        continue; // Without this, the same error response falls through to error_for_status
                    }

                    match resp.error_for_status() {
                        Ok(resp) => return resp.json::<T>().await,
                        Err(e) => {
                            let e = e.without_url();
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
            // All attempts exhausted without a reqwest::Error being recorded.
            // This happens when every attempt returned a retryable status code
            // (429/503/502/504) and the loop never fell through to error_for_status().
            // Build a synthetic error to avoid panicking.
            error!(
                "TMDB fetch_with_retry: all {} attempts exhausted (retryable status codes)",
                max_attempts
            );
            Err(synthetic_exhausted_error())
        }
    }
}

/// Parse a TMDB movie/tv details object into (title, year, original_language).
pub(crate) fn parse_details(v: &serde_json::Value, kind: crate::vfs::MediaType) -> (String, Option<String>, Option<String>) {
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
    let year = date.split('-').next().filter(|y| y.len() == 4).map(String::from);
    let original_language = v.get("original_language").and_then(|l| l.as_str()).map(String::from);
    (title, year, original_language)
}

/// Parse TMDB `/find/{imdb_id}?external_source=imdb_id` into (tmdb_id, kind). Movie wins over TV.
pub(crate) fn parse_find_response(v: &serde_json::Value) -> Option<(u64, crate::vfs::MediaType)> {
    if let Some(id) = v.get("movie_results").and_then(|a| a.as_array()).and_then(|a| a.first())
        .and_then(|m| m.get("id")).and_then(|i| i.as_u64()) {
        return Some((id, crate::vfs::MediaType::Movie));
    }
    if let Some(id) = v.get("tv_results").and_then(|a| a.as_array()).and_then(|a| a.first())
        .and_then(|m| m.get("id")).and_then(|i| i.as_u64()) {
        return Some((id, crate::vfs::MediaType::Show));
    }
    None
}

/// Parse `/{type}/{id}/external_ids` into the IMDB id (non-empty).
pub(crate) fn parse_external_ids(v: &serde_json::Value) -> Option<String> {
    v.get("imdb_id").and_then(|i| i.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Build the synthetic Bad Gateway error returned when every retry attempt hit a retryable
/// status code, so the function surfaces an error instead of panicking. The construction is
/// infallible by design (a fixed valid status and static body), hence the `expect`s.
fn synthetic_exhausted_error() -> reqwest::Error {
    reqwest::Response::from(
        hyper::Response::builder()
            .status(reqwest::StatusCode::BAD_GATEWAY)
            .body(hyper::body::Bytes::from_static(
                b"all attempts exhausted: retryable status codes",
            ))
            .expect("static BAD_GATEWAY response always builds"),
    )
    .error_for_status()
    .expect_err("BAD_GATEWAY always yields an error status")
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
            Some(EpisodeAirDate { season, episode, air_date })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_exhausted_error_is_a_bad_gateway_error_not_a_panic() {
        // Actually executes the exhausted-retries fallback (the source-grep test below only
        // checks the text). It must return an error carrying 502, never panic.
        let err = synthetic_exhausted_error();
        assert!(err.is_status());
        assert_eq!(err.status(), Some(reqwest::StatusCode::BAD_GATEWAY));
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
        assert_eq!(super::parse_find_response(&json), Some((27205, crate::vfs::MediaType::Movie)));
        let json2 = serde_json::json!({ "movie_results": [], "tv_results": [{"id": 1396}] });
        assert_eq!(super::parse_find_response(&json2), Some((1396, crate::vfs::MediaType::Show)));
        let empty = serde_json::json!({"movie_results": [], "tv_results": []});
        assert_eq!(super::parse_find_response(&empty), None);
    }

    #[test]
    fn parse_external_ids_extracts_imdb() {
        let json = serde_json::json!({"imdb_id": "tt0816692"});
        assert_eq!(super::parse_external_ids(&json), Some("tt0816692".to_string()));
        let none = serde_json::json!({"imdb_id": serde_json::Value::Null});
        assert_eq!(super::parse_external_ids(&none), None);
        let empty = serde_json::json!({"imdb_id": ""});
        assert_eq!(super::parse_external_ids(&empty), None);
    }

    #[test]
    fn parse_details_movie_and_show() {
        let m = serde_json::json!({"title": "Inception", "release_date": "2010-07-16", "original_language": "en"});
        assert_eq!(super::parse_details(&m, crate::vfs::MediaType::Movie), ("Inception".into(), Some("2010".into()), Some("en".into())));
        let s = serde_json::json!({"name": "Breaking Bad", "first_air_date": "2008-01-20", "original_language": "en"});
        assert_eq!(super::parse_details(&s, crate::vfs::MediaType::Show), ("Breaking Bad".into(), Some("2008".into()), Some("en".into())));
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
        assert_eq!(parse_show_status(&missing), ShowStatus::Other, "missing status → Other");

        // Mixed-case: "ended" (all lower) → Ended
        let lower = serde_json::json!({ "status": "ended" });
        assert_eq!(parse_show_status(&lower), ShowStatus::Ended, "lowercase 'ended' → Ended");

        // Mixed-case: "CANCELED" (all upper) → Ended
        let upper = serde_json::json!({ "status": "CANCELED" });
        assert_eq!(parse_show_status(&upper), ShowStatus::Ended, "uppercase 'CANCELED' → Ended");
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
            EpisodeAirDate { season: 2, episode: 2, air_date: None }
        );
        assert_eq!(
            result[2],
            EpisodeAirDate { season: 2, episode: 3, air_date: None }
        );
        assert_eq!(
            result[3],
            EpisodeAirDate { season: 2, episode: 4, air_date: None }
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
}
