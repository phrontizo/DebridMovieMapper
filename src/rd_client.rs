use serde::{Deserialize, Serialize};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use tracing::{info, error, warn};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    pub files: Vec<TorrentFile>,
    #[serde(default)]
    pub links: Vec<String>,
    pub ended: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TorrentFile {
    pub id: u32,
    pub path: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub selected: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnrestrictResponse {
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(rename = "mimeType", default)]
    pub mime_type: String,
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

#[derive(Debug)]
pub struct RealDebridClient {
    client: reqwest::Client,
}

impl RealDebridClient {
    pub fn new(api_token: String) -> Self {
        let mut headers = HeaderMap::new();
        let auth_val = format!("Bearer {}", api_token);
        let mut auth_header = HeaderValue::from_str(&auth_val).unwrap();
        auth_header.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth_header);
        headers.insert(reqwest::header::ACCEPT, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent("DebridMovieMapper/0.1.0")
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap();

        Self { client }
    }

    pub async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        let mut all_torrents = Vec::new();
        let mut page = 1;
        loop {
            info!("Fetching torrents page {}...", page);
            let url = format!("https://api.real-debrid.com/rest/1.0/torrents?page={}&limit=50", page);
            let res: Result<Vec<Torrent>, reqwest::Error> = self.fetch_with_retry(|| self.client.get(&url)).await;
            
            match res {
                Ok(torrents) => {
                    if torrents.is_empty() {
                        break;
                    }
                    all_torrents.extend(torrents);
                    page += 1;
                }
                Err(e) => {
                    if !all_torrents.is_empty() {
                        warn!("Failed to fetch torrents page {}, but returning {} gathered torrents: {}", page, all_torrents.len(), e);
                        break;
                    }
                    return Err(e);
                }
            }
        }
        info!("Fetched {} torrents in total.", all_torrents.len());
        Ok(all_torrents)
    }

    pub async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        let url = format!("https://api.real-debrid.com/rest/1.0/torrents/info/{}", id);
        self.fetch_with_retry(|| self.client.get(&url)).await
    }

    pub async fn unrestrict_link(&self, link: &str) -> Result<UnrestrictResponse, reqwest::Error> {
        let url = "https://api.real-debrid.com/rest/1.0/unrestrict/link";
        self.fetch_with_retry(|| {
            self.client.post(url).form(&[("link", link)])
        }).await
    }

    async fn fetch_with_retry<T, F>(&self, make_request: F) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut last_error: Option<reqwest::Error> = None;
        let max_attempts = 5;

        for attempt in 1..=max_attempts {
            if attempt > 1 {
                // Exponential backoff: 1s, 2s, 4s, 8s
                let backoff = 2u64.pow(attempt as u32 - 2) * 1000;
                // Add jitter (up to 500ms)
                let jitter = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() % 500) as u64;
                let delay = Duration::from_millis(backoff + jitter);
                tokio::time::sleep(delay).await;
            }

            let request = make_request();
            match request.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let retry_after = resp.headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());
                        
                        if let Some(seconds) = retry_after {
                            warn!("RD API rate limited (429). Respecting Retry-After: {}s (attempt {}/{})", seconds, attempt, max_attempts);
                            tokio::time::sleep(Duration::from_secs(seconds)).await;
                        }
                    }

                    match resp.error_for_status() {
                        Ok(resp) => {
                            let headers = resp.headers().clone();
                            let text = resp.text().await?;
                            if text.trim().is_empty() || status.as_u16() == 204 {
                                if let Ok(val) = serde_json::from_str::<T>("[]") {
                                    return Ok(val);
                                }
                                warn!("RD API returned empty body or 204 (attempt {}/{}) - Status: {}, Headers: {:?}", attempt, max_attempts, status, headers);
                                continue;
                            }
                            match serde_json::from_str::<T>(&text) {
                                Ok(val) => return Ok(val),
                                Err(e) => {
                                    error!("Failed to decode RD response: {}. Status: {}, Body: {}, Headers: {:?}", e, status, text, headers);
                                    // This is a decoding error. reqwest::Error can be created from it.
                                    // Actually, let's just use the error from a failed .json() call to get a reqwest::Error.
                                }
                            }
                        }
                        Err(e) => {
                            warn!("RD API error status (attempt {}/{}): {}. Status: {}", attempt, max_attempts, e, status);
                            last_error = Some(e);
                        }
                    }
                }
                Err(e) => {
                    warn!("RD API request failed (attempt {}/{}): {}", attempt, max_attempts, e);
                    last_error = Some(e);
                }
            }
        }

        // If we reach here, we exhausted retries or got a terminal error.
        if let Some(e) = last_error {
            Err(e)
        } else {
            // This happens if we got empty body or decoding error every time.
            // Do one last request to get a proper reqwest::Error from .json()
            make_request().send().await?.error_for_status()?.json().await
        }
    }
}
