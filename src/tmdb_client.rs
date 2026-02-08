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
        let mut url = format!(
            "https://api.themoviedb.org/3/search/movie?api_key={}&query={}",
            self.api_key,
            urlencoding::encode(query)
        );
        if let Some(y) = year {
            url.push_str(&format!("&year={}", y));
        }
        self.search(&url).await
    }

    pub async fn search_tv(&self, query: &str, year: Option<&str>) -> Vec<TmdbSearchResult> {
        let mut url = format!(
            "https://api.themoviedb.org/3/search/tv?api_key={}&query={}",
            self.api_key,
            urlencoding::encode(query)
        );
        if let Some(y) = year {
            url.push_str(&format!("&first_air_date_year={}", y));
        }
        self.search(&url).await
    }

    async fn search(&self, url: &str) -> Vec<TmdbSearchResult> {
        match self.fetch_with_retry(|| self.client.get(url)).await {
            Ok(resp) => resp.results,
            Err(e) => {
                error!("TMDB search failed: {}", e);
                Vec::new()
            }
        }
    }

    async fn fetch_with_retry(&self, make_request: impl Fn() -> RequestBuilder) -> Result<TmdbResponse, reqwest::Error> {
        let mut last_error: Option<reqwest::Error> = None;
        let max_attempts = 3;

        for attempt in 1..=max_attempts {
            if attempt > 1 {
                let backoff = 2u64.pow(attempt as u32 - 2) * 1000;
                let jitter = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() % 500) as u64;
                tokio::time::sleep(Duration::from_millis(backoff + jitter)).await;
            }

            match make_request().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let retry_after = resp.headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(1);
                        warn!("TMDB API rate limited (429). Waiting {}s (attempt {}/{})", retry_after, attempt, max_attempts);
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
