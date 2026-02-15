use serde::{Deserialize, Serialize};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use tracing::{info, error, warn};
use std::time::Duration;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use rand::Rng;

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

#[derive(Debug, Clone, Deserialize)]
pub struct AddMagnetResponse {
    pub id: String,
    pub uri: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AvailableHost {
    pub host: String,
    pub max_file_size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstantAvailability {
    #[serde(flatten)]
    pub files: HashMap<String, Vec<HashMap<String, serde_json::Value>>>,
}

#[derive(Debug, Clone)]
struct CachedUnrestrictResponse {
    response: UnrestrictResponse,
    cached_at: std::time::Instant,
}

#[derive(Debug)]
pub struct RealDebridClient {
    client: reqwest::Client,
    unrestrict_cache: Arc<RwLock<HashMap<String, CachedUnrestrictResponse>>>,
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

        Self {
            client,
            unrestrict_cache: Arc::new(RwLock::new(HashMap::new())),
        }
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
        // Check cache first
        const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour cache

        {
            let cache = self.unrestrict_cache.read().await;
            if let Some(cached) = cache.get(link) {
                if cached.cached_at.elapsed() < CACHE_TTL {
                    info!("Using cached unrestrict response for link: {}", link);
                    return Ok(cached.response.clone());
                }
            }
        }

        // Not in cache or expired, fetch from API
        // Special handling: 503 on unrestrict means broken torrent, no retries
        let url = "https://api.real-debrid.com/rest/1.0/unrestrict/link";
        let response: UnrestrictResponse = self.fetch_with_retry_except_503(|| {
            self.client.post(url).form(&[("link", link)])
        }).await?;

        // Store in cache
        {
            let mut cache = self.unrestrict_cache.write().await;
            cache.insert(link.to_string(), CachedUnrestrictResponse {
                response: response.clone(),
                cached_at: std::time::Instant::now(),
            });
        }

        Ok(response)
    }

    /// Add a magnet link to Real-Debrid
    pub async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        let url = "https://api.real-debrid.com/rest/1.0/torrents/addMagnet";
        self.fetch_with_retry(|| {
            self.client.post(url).form(&[("magnet", magnet)])
        }).await
    }

    /// Select files for a torrent
    pub async fn select_files(&self, torrent_id: &str, file_ids: &str) -> Result<(), reqwest::Error> {
        let url = format!("https://api.real-debrid.com/rest/1.0/torrents/selectFiles/{}", torrent_id);
        // Use empty type for no response body expected
        let _: serde_json::Value = self.fetch_with_retry(|| {
            self.client.post(&url).form(&[("files", file_ids)])
        }).await?;
        Ok(())
    }

    /// Delete a torrent from Real-Debrid
    /// Returns Ok(()) even if torrent doesn't exist (404), as the end state is the same
    pub async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
        let url = format!("https://api.real-debrid.com/rest/1.0/torrents/delete/{}", torrent_id);

        // Try to delete, but don't treat 404 as an error
        match self.client.delete(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
                    // Success or already deleted - both are fine
                    Ok(())
                } else {
                    // Other error - convert to reqwest::Error
                    resp.error_for_status().map(|_| ())
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Check if a link is valid by attempting to unrestrict it
    /// This is a simplified version that doesn't use the full retry logic
    /// because 503 errors on unrestrict indicate the torrent needs repair
    /// However, 429 (rate limit) errors are retried with exponential backoff
    pub async fn check_link_health(&self, link: &str) -> bool {
        let url = "https://api.real-debrid.com/rest/1.0/unrestrict/link";
        let max_attempts = 5; // More attempts for rate limits

        for attempt in 1..=max_attempts {
            match self.client.post(url)
                .form(&[("link", link)])
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();

                    if status.is_success() {
                        // Try to parse the response
                        match resp.json::<UnrestrictResponse>().await {
                            Ok(_) => return true,
                            Err(e) => {
                                warn!("Link health check failed to parse response for {}: {}", link, e);
                                return false;
                            }
                        }
                    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        // 429 - Rate limited, use exponential backoff
                        let retry_after = resp.headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());

                        if attempt < max_attempts {
                            // Use exponential backoff: 2s, 4s, 8s, 16s, 32s
                            // But respect Retry-After header if provided and it's longer
                            let exponential_backoff = 2u64.pow(attempt as u32);
                            let wait_time = retry_after.map(|ra| ra.max(exponential_backoff)).unwrap_or(exponential_backoff);

                            warn!("Link health check: 429 rate limited - waiting {}s (attempt {}/{})", wait_time, attempt, max_attempts);
                            tokio::time::sleep(Duration::from_secs(wait_time)).await;
                            continue;
                        } else {
                            error!("Link health check: 429 rate limited - max attempts reached after exponential backoff");
                            return false;
                        }
                    } else if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                        // 503 indicates the torrent is broken and needs repair - don't retry
                        warn!("Link health check: 503 Service Unavailable for {} - torrent needs repair", link);
                        return false;
                    } else if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::FORBIDDEN {
                        // 404/403 also indicate broken links - don't retry
                        warn!("Link health check: {} {} for {} - torrent needs repair", status.as_u16(), status.canonical_reason().unwrap_or(""), link);
                        return false;
                    } else {
                        // Other errors - don't retry
                        warn!("Link health check: unexpected status {} for {}", status, link);
                        return false;
                    }
                }
                Err(e) => {
                    if attempt < max_attempts {
                        warn!("Link health check network error for {} (attempt {}/{}): {}", link, attempt, max_attempts, e);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    } else {
                        warn!("Link health check failed for {} after {} attempts: {}", link, max_attempts, e);
                        return false;
                    }
                }
            }
        }

        false
    }

    async fn fetch_with_retry<T, F>(&self, make_request: F) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut last_error: Option<reqwest::Error> = None;
        let max_attempts = 10;

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
                    
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                       || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
                       || status == reqwest::StatusCode::BAD_GATEWAY
                       || status == reqwest::StatusCode::GATEWAY_TIMEOUT
                    {
                        let retry_after = resp.headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());

                        if let Some(seconds) = retry_after {
                            warn!("RD API returned {} (attempt {}/{}). Respecting Retry-After: {}s", status, attempt, max_attempts, seconds);
                            tokio::time::sleep(Duration::from_secs(seconds)).await;
                        } else if status == reqwest::StatusCode::SERVICE_UNAVAILABLE && attempt < max_attempts {
                            // Extended exponential backoff for 503, capped at 30s
                            let backoff_secs = 2u64.pow(attempt as u32);
                            let delay = Duration::from_secs(std::cmp::min(backoff_secs, 30));
                            let jitter = Duration::from_millis(rand::thread_rng().gen_range(0..1000));
                            let total_delay = delay + jitter;
                            warn!("RD API service unavailable (503) (attempt {}/{}). Using extended backoff: {}ms", attempt, max_attempts, total_delay.as_millis());
                            tokio::time::sleep(total_delay).await;
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

    /// Same as fetch_with_retry but treats 503 Service Unavailable as a terminal error
    /// (no retries). Used for unrestrict endpoint where 503 indicates broken torrent.
    async fn fetch_with_retry_except_503<T, F>(&self, make_request: F) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut last_error: Option<reqwest::Error> = None;
        let max_attempts = 10;

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

                    // 503 on unrestrict is a terminal error - no retries
                    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                        warn!("RD API returned 503 Service Unavailable on unrestrict - treating as broken torrent, no retries");
                        return resp.error_for_status()?.json().await;
                    }

                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                       || status == reqwest::StatusCode::BAD_GATEWAY
                       || status == reqwest::StatusCode::GATEWAY_TIMEOUT
                    {
                        let retry_after = resp.headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());

                        if let Some(seconds) = retry_after {
                            warn!("RD API returned {} (attempt {}/{}). Respecting Retry-After: {}s", status, attempt, max_attempts, seconds);
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
