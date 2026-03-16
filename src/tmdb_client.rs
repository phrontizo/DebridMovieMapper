use serde::Deserialize;
use reqwest::{Client, RequestBuilder};
use rand::Rng;
use tracing::{error, warn};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

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
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            api_key,
            // Start in the past so the first request fires immediately.
            last_request: Mutex::new(Instant::now() - MIN_REQUEST_INTERVAL),
        }
    }

    pub async fn search_movie(&self, query: &str, year: Option<&str>) -> Vec<TmdbSearchResult> {
        let url = "https://api.themoviedb.org/3/search/movie";
        let mut params = vec![
            ("api_key", self.api_key.as_str()),
            ("query", query),
        ];
        let year_string;
        if let Some(y) = year {
            year_string = y.to_string();
            params.push(("primary_release_year", &year_string));
        }
        self.search(url, params).await
    }

    pub async fn search_tv(&self, query: &str, year: Option<&str>) -> Vec<TmdbSearchResult> {
        let url = "https://api.themoviedb.org/3/search/tv";
        let mut params = vec![
            ("api_key", self.api_key.as_str()),
            ("query", query),
        ];
        let year_string;
        if let Some(y) = year {
            year_string = y.to_string();
            params.push(("first_air_date_year", &year_string));
        }
        self.search(url, params).await
    }

    async fn search(&self, url: &str, params: Vec<(&str, &str)>) -> Vec<TmdbSearchResult> {
        match self.fetch_with_retry(|| self.client.get(url).query(&params)).await {
            Ok(resp) => resp.results,
            Err(e) => {
                error!("TMDB search failed: {}", e.without_url());
                Vec::new()
            }
        }
    }

    async fn fetch_with_retry(&self, make_request: impl Fn() -> RequestBuilder) -> Result<TmdbResponse, reqwest::Error> {
        let mut last_error: Option<reqwest::Error> = None;
        let max_attempts = 10;

        for attempt in 1..=max_attempts {
            if attempt > 1 {
                let backoff = (2u64.pow(attempt as u32 - 2) * 1000).min(30_000);
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
                        let retry_after = resp.headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(1);
                        let capped = std::cmp::min(retry_after, MAX_RETRY_AFTER_SECS);
                        warn!("TMDB API returned {} (attempt {}/{}). Waiting {}s", status, attempt, max_attempts, capped);
                        tokio::time::sleep(Duration::from_secs(capped)).await;
                        continue;  // Without this, the same error response falls through to error_for_status
                    }

                    match resp.error_for_status() {
                        Ok(resp) => return resp.json::<TmdbResponse>().await,
                        Err(e) => {
                            let e = e.without_url();
                            warn!("TMDB API error (attempt {}/{}): {}", attempt, max_attempts, e);
                            last_error = Some(e);
                        }
                    }
                }
                Err(e) => {
                    let e = e.without_url();
                    warn!("TMDB request failed (attempt {}/{}): {}", attempt, max_attempts, e);
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
            error!("TMDB fetch_with_retry: all {} attempts exhausted (retryable status codes)", max_attempts);
            let synthetic = reqwest::Response::from(
                hyper::Response::builder()
                    .status(reqwest::StatusCode::BAD_GATEWAY)
                    .body(hyper::body::Bytes::from_static(b"all attempts exhausted: retryable status codes"))
                    .unwrap()
            );
            Err(synthetic.error_for_status().unwrap_err())
        }
    }
}

#[cfg(test)]
mod tests {
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
}
