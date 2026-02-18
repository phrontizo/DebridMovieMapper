use serde::Deserialize;
use reqwest::{Client, RequestBuilder};
use tracing::{error, warn};
use std::time::Duration;

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
    pub results: Vec<TmdbSearchResult>,
}

pub struct TmdbClient {
    client: Client,
    api_key: String,
}

impl TmdbClient {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            api_key,
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
            params.push(("year", &year_string));
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
                error!("TMDB search failed: {}", e);
                Vec::new()
            }
        }
    }

    async fn fetch_with_retry(&self, make_request: impl Fn() -> RequestBuilder) -> Result<TmdbResponse, reqwest::Error> {
        let mut last_error: Option<reqwest::Error> = None;
        let max_attempts = 10;

        for attempt in 1..=max_attempts {
            if attempt > 1 {
                let backoff = 2u64.pow(attempt as u32 - 2) * 1000;
                let jitter = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() % 500) as u64;
                tokio::time::sleep(Duration::from_millis(backoff + jitter)).await;
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
                        warn!("TMDB API returned {} (attempt {}/{}). Waiting {}s", status, attempt, max_attempts, retry_after);
                        tokio::time::sleep(Duration::from_secs(retry_after)).await;
                    }

                    match resp.error_for_status() {
                        Ok(resp) => return resp.json::<TmdbResponse>().await,
                        Err(e) => {
                            warn!("TMDB API error (attempt {}/{}): {}", attempt, max_attempts, e);
                            last_error = Some(e);
                        }
                    }
                }
                Err(e) => {
                    warn!("TMDB request failed (attempt {}/{}): {}", attempt, max_attempts, e);
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap())
    }
}
