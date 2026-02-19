use serde::{Deserialize, Serialize};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use tracing::{info, error, warn};
use std::time::Duration;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use rand::Rng;

const MIN_INTERVAL_MS: u64 = 100;   // 10 req/s max (baseline)
const MAX_INTERVAL_MS: u64 = 2000;  // 0.5 req/s min (under heavy throttling)
const RECOVERY_MS: u64 = 10;        // Decrease interval by 10ms per success
const MAX_RETRY_AFTER_SECS: u64 = 300; // Cap Retry-After to 5 minutes

const MAX_CACHE_SIZE: usize = 10_000;
const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour

struct RateLimiterState {
    /// Current interval between requests in milliseconds
    interval_ms: u64,
    /// When the next request is allowed
    next_allowed: tokio::time::Instant,
}

/// Adaptive token bucket rate limiter that slows down on 429s and recovers on success.
/// Bucket capacity is 1 (no bursting).
pub struct AdaptiveRateLimiter {
    state: tokio::sync::Mutex<RateLimiterState>,
}

impl std::fmt::Debug for AdaptiveRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaptiveRateLimiter").finish()
    }
}

impl AdaptiveRateLimiter {
    fn new() -> Self {
        Self {
            state: tokio::sync::Mutex::new(RateLimiterState {
                interval_ms: MIN_INTERVAL_MS,
                next_allowed: tokio::time::Instant::now(),
            }),
        }
    }

    /// Wait until a token is available, then reserve it.
    async fn wait_for_token(&self) {
        let deadline = {
            let mut state = self.state.lock().await;
            let now = tokio::time::Instant::now();
            if state.next_allowed < now {
                state.next_allowed = now;
            }
            let deadline = state.next_allowed;
            let interval = Duration::from_millis(state.interval_ms);
            state.next_allowed += interval;
            deadline
        };
        tokio::time::sleep_until(deadline).await;
    }

    /// Record a successful request — gradually decrease interval toward baseline.
    async fn record_success(&self) {
        let mut state = self.state.lock().await;
        state.interval_ms = state.interval_ms.saturating_sub(RECOVERY_MS).max(MIN_INTERVAL_MS);
    }

    /// Record a 429 throttle — double the interval and optionally respect Retry-After.
    async fn record_throttle(&self, retry_after: Option<u64>) {
        let mut state = self.state.lock().await;
        state.interval_ms = (state.interval_ms * 2).min(MAX_INTERVAL_MS);
        if let Some(seconds) = retry_after {
            let capped_seconds = seconds.min(MAX_RETRY_AFTER_SECS);
            let retry_deadline = tokio::time::Instant::now() + Duration::from_secs(capped_seconds);
            if retry_deadline > state.next_allowed {
                state.next_allowed = retry_deadline;
            }
        }
    }
}

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
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
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
    rate_limiter: Arc<AdaptiveRateLimiter>,
}

impl RealDebridClient {
    pub fn new(api_token: String) -> Result<Self, crate::error::AppError> {
        let mut headers = HeaderMap::new();
        let auth_val = format!("Bearer {}", api_token);
        let mut auth_header = HeaderValue::from_str(&auth_val)
            .map_err(|e| crate::error::AppError::Config(format!("Invalid API token for HTTP header: {}", e)))?;
        auth_header.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth_header);
        headers.insert(reqwest::header::ACCEPT, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent("DebridMovieMapper/0.1.0")
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| crate::error::AppError::Config(format!("Failed to build HTTP client: {}", e)))?;

        Ok(Self {
            client,
            unrestrict_cache: Arc::new(RwLock::new(HashMap::new())),
            rate_limiter: Arc::new(AdaptiveRateLimiter::new()),
        })
    }

    /// Helper to handle 503 and other non-429 retryable status codes
    async fn wait_for_retry(
        status: reqwest::StatusCode,
        headers: &HeaderMap,
        attempt: u32,
        max_attempts: u32,
    ) {
        let retry_after = headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        if let Some(seconds) = retry_after {
            let capped = std::cmp::min(seconds, MAX_RETRY_AFTER_SECS);
            warn!(
                "RD API returned {} (attempt {}/{}). Respecting Retry-After: {}s (raw: {}s)",
                status, attempt, max_attempts, capped, seconds
            );
            tokio::time::sleep(Duration::from_secs(capped)).await;
        } else if attempt < max_attempts {
            // Extended exponential backoff for 503/502/504, capped at 30s
            let backoff_secs = 2u64.pow(attempt);
            let delay = Duration::from_secs(std::cmp::min(backoff_secs, 30));
            let jitter = Duration::from_millis(rand::thread_rng().gen_range(0..1000));
            let total_delay = delay + jitter;
            warn!(
                "RD API {} (attempt {}/{}). Using extended backoff: {}ms",
                status,
                attempt,
                max_attempts,
                total_delay.as_millis()
            );
            tokio::time::sleep(total_delay).await;
        }
    }

    /// Helper to check if a status code should trigger a retry
    fn should_retry_status(status: reqwest::StatusCode) -> bool {
        status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
            || status == reqwest::StatusCode::BAD_GATEWAY
            || status == reqwest::StatusCode::GATEWAY_TIMEOUT
    }

    pub async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        let mut all_torrents = Vec::new();
        let mut page = 1;
        loop {
            info!("Fetching torrents page {}...", page);
            let url = format!("https://api.real-debrid.com/rest/1.0/torrents?page={}&limit=50", page);
            let res: Result<Vec<Torrent>, reqwest::Error> = self.fetch_with_retry(|| self.client.get(&url), &[]).await;
            
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
        self.fetch_with_retry(|| self.client.get(&url), &[reqwest::StatusCode::NOT_FOUND]).await
    }

    pub async fn unrestrict_link(&self, link: &str) -> Result<UnrestrictResponse, reqwest::Error> {
        // Check cache first
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
        let response: UnrestrictResponse = self.fetch_with_retry(
            || self.client.post(url).form(&[("link", link)]),
            &[reqwest::StatusCode::SERVICE_UNAVAILABLE],
        ).await?;

        // Store in cache
        {
            let mut cache = self.unrestrict_cache.write().await;
            cache.insert(link.to_string(), CachedUnrestrictResponse {
                response: response.clone(),
                cached_at: std::time::Instant::now(),
            });
        }

        // Evict if cache is too large
        {
            let cache = self.unrestrict_cache.read().await;
            if cache.len() > MAX_CACHE_SIZE {
                drop(cache);
                self.evict_expired_cache().await;
            }
        }

        Ok(response)
    }

    /// Add a magnet link to Real-Debrid
    pub async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        let url = "https://api.real-debrid.com/rest/1.0/torrents/addMagnet";
        self.fetch_with_retry(|| {
            self.client.post(url).form(&[("magnet", magnet)])
        }, &[]).await
    }

    /// Select files for a torrent
    pub async fn select_files(&self, torrent_id: &str, file_ids: &str) -> Result<(), reqwest::Error> {
        let url = format!("https://api.real-debrid.com/rest/1.0/torrents/selectFiles/{}", torrent_id);
        // Use empty type for no response body expected
        let _: serde_json::Value = self.fetch_with_retry(|| {
            self.client.post(&url).form(&[("files", file_ids)])
        }, &[]).await?;
        Ok(())
    }

    /// Delete a torrent from Real-Debrid
    /// Returns Ok(()) even if torrent doesn't exist (404), as the end state is the same
    pub async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
        let url = format!("https://api.real-debrid.com/rest/1.0/torrents/delete/{}", torrent_id);
        let result: Result<serde_json::Value, _> = self.fetch_with_retry(
            || self.client.delete(&url),
            &[reqwest::StatusCode::NOT_FOUND],
        ).await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(reqwest::StatusCode::NOT_FOUND) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn fetch_with_retry<T, F>(
        &self,
        make_request: F,
        terminal_statuses: &[reqwest::StatusCode],
    ) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut last_error: Option<reqwest::Error> = None;
        let max_attempts = 10;

        for attempt in 1..=max_attempts {
            self.rate_limiter.wait_for_token().await;

            match make_request().send().await {
                Ok(resp) => {
                    let status = resp.status();

                    if terminal_statuses.contains(&status) {
                        warn!("RD API returned terminal status {} — not retrying (attempt {}/{})", status, attempt, max_attempts);
                        return Err(resp.error_for_status().unwrap_err());
                    }

                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let retry_after = resp.headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok());
                        self.rate_limiter.record_throttle(retry_after).await;
                        warn!("RD API returned 429 (attempt {}/{}). Adaptive limiter adjusted.", attempt, max_attempts);
                        continue;
                    }

                    if Self::should_retry_status(status) {
                        Self::wait_for_retry(status, resp.headers(), attempt, max_attempts).await;
                        continue;
                    }

                    match resp.error_for_status() {
                        Ok(resp) => {
                            let text = resp.text().await?;
                            if text.trim().is_empty() || status.as_u16() == 204 {
                                if let Ok(val) = serde_json::from_str::<T>("[]") {
                                    self.rate_limiter.record_success().await;
                                    return Ok(val);
                                }
                                warn!("RD API empty body or 204 (attempt {}/{}). Status: {}",
                                    attempt, max_attempts, status);
                                continue;
                            }
                            match serde_json::from_str::<T>(&text) {
                                Ok(val) => {
                                    self.rate_limiter.record_success().await;
                                    return Ok(val);
                                }
                                Err(e) => {
                                    error!("Failed to decode RD response: {}. Status: {}, Body: {}",
                                        e, status, text);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("RD API error (attempt {}/{}): {}. Status: {}", attempt, max_attempts, e, status);
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

        if let Some(e) = last_error {
            Err(e)
        } else {
            unreachable!("fetch_with_retry: all attempts exhausted without a recorded error")
        }
    }



    /// Evict expired entries from the unrestrict cache, and if still over
    /// MAX_CACHE_SIZE, remove the oldest entries.
    pub async fn evict_expired_cache(&self) {
        let mut cache = self.unrestrict_cache.write().await;
        cache.retain(|_, v| v.cached_at.elapsed() < CACHE_TTL);
        // If still over max size, remove oldest entries
        if cache.len() > MAX_CACHE_SIZE {
            let mut entries: Vec<_> = cache.iter().map(|(k, v)| (k.clone(), v.cached_at)).collect();
            entries.sort_by_key(|(_, t)| *t);
            let to_remove = cache.len() - MAX_CACHE_SIZE;
            for (key, _) in entries.into_iter().take(to_remove) {
                cache.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- should_retry_status ---

    #[test]
    fn should_retry_status_retries_429() {
        assert!(RealDebridClient::should_retry_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn should_retry_status_retries_503() {
        assert!(RealDebridClient::should_retry_status(reqwest::StatusCode::SERVICE_UNAVAILABLE));
    }

    #[test]
    fn should_retry_status_retries_502() {
        assert!(RealDebridClient::should_retry_status(reqwest::StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn should_retry_status_retries_504() {
        assert!(RealDebridClient::should_retry_status(reqwest::StatusCode::GATEWAY_TIMEOUT));
    }

    #[test]
    fn should_retry_status_does_not_retry_200() {
        assert!(!RealDebridClient::should_retry_status(reqwest::StatusCode::OK));
    }

    #[test]
    fn should_retry_status_does_not_retry_404() {
        assert!(!RealDebridClient::should_retry_status(reqwest::StatusCode::NOT_FOUND));
    }

    #[test]
    fn should_retry_status_does_not_retry_500() {
        assert!(!RealDebridClient::should_retry_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
    }

    // --- Compile-time signature checks for the unified fetch_with_retry ---
    // These functions are never called; they exist only to fail compilation
    // if the method signatures change unexpectedly.

    #[allow(dead_code)]
    async fn _assert_fetch_with_retry_accepts_terminal_statuses(client: &RealDebridClient) {
        // Verifies fetch_with_retry accepts &[StatusCode] as second argument.
        // If the signature changes this will not compile.
        let _: Result<serde_json::Value, _> = client
            .fetch_with_retry(
                || client.client.get("http://example.com"),
                &[reqwest::StatusCode::NOT_FOUND, reqwest::StatusCode::SERVICE_UNAVAILABLE],
            )
            .await;
    }

    #[allow(dead_code)]
    async fn _assert_fetch_with_retry_accepts_empty_terminal_statuses(client: &RealDebridClient) {
        let _: Result<serde_json::Value, _> = client
            .fetch_with_retry(|| client.client.get("http://example.com"), &[])
            .await;
    }

    // --- AdaptiveRateLimiter ---

    #[tokio::test]
    async fn adaptive_limiter_starts_at_baseline() {
        let limiter = AdaptiveRateLimiter::new();
        let state = limiter.state.lock().await;
        assert_eq!(state.interval_ms, MIN_INTERVAL_MS);
    }

    #[tokio::test]
    async fn adaptive_limiter_doubles_on_throttle() {
        let limiter = AdaptiveRateLimiter::new();
        limiter.record_throttle(None).await;
        let state = limiter.state.lock().await;
        assert_eq!(state.interval_ms, 200);
    }

    #[tokio::test]
    async fn adaptive_limiter_caps_at_max() {
        let limiter = AdaptiveRateLimiter::new();
        // Double repeatedly: 100 -> 200 -> 400 -> 800 -> 1600 -> 2000 (capped)
        for _ in 0..10 {
            limiter.record_throttle(None).await;
        }
        let state = limiter.state.lock().await;
        assert_eq!(state.interval_ms, MAX_INTERVAL_MS);
    }

    #[tokio::test]
    async fn adaptive_limiter_recovers_on_success() {
        let limiter = AdaptiveRateLimiter::new();
        // Throttle to 200ms
        limiter.record_throttle(None).await;
        // Recover: 200 -> 190
        limiter.record_success().await;
        let state = limiter.state.lock().await;
        assert_eq!(state.interval_ms, 190);
    }

    #[tokio::test]
    async fn adaptive_limiter_floors_at_min() {
        let limiter = AdaptiveRateLimiter::new();
        // Already at min, success shouldn't go below
        limiter.record_success().await;
        let state = limiter.state.lock().await;
        assert_eq!(state.interval_ms, MIN_INTERVAL_MS);
    }

    #[tokio::test]
    async fn adaptive_limiter_retry_after_advances_next_allowed() {
        let limiter = AdaptiveRateLimiter::new();
        let before = tokio::time::Instant::now();
        limiter.record_throttle(Some(5)).await;
        let state = limiter.state.lock().await;
        // next_allowed should be at least 5 seconds from now
        assert!(state.next_allowed >= before + Duration::from_secs(5));
        // interval should also have doubled
        assert_eq!(state.interval_ms, 200);
    }

    // --- Cache eviction ---

    #[tokio::test]
    async fn unrestrict_cache_evicts_expired_entries() {
        let client = RealDebridClient::new("fake-token".to_string()).unwrap();
        // Seed cache with entries - we need to directly manipulate the cache
        {
            let mut cache = client.unrestrict_cache.write().await;
            // Insert an "old" entry with a manually backdated timestamp
            cache.insert("old-link".to_string(), CachedUnrestrictResponse {
                response: UnrestrictResponse {
                    id: "old".to_string(),
                    filename: "old.mkv".to_string(),
                    mime_type: None,
                    filesize: 0,
                    link: "old-link".to_string(),
                    host: String::new(),
                    chunks: 0,
                    crc: 0,
                    download: "http://old".to_string(),
                    streamable: 0,
                },
                cached_at: std::time::Instant::now() - Duration::from_secs(7200), // 2 hours ago
            });
            cache.insert("new-link".to_string(), CachedUnrestrictResponse {
                response: UnrestrictResponse {
                    id: "new".to_string(),
                    filename: "new.mkv".to_string(),
                    mime_type: None,
                    filesize: 0,
                    link: "new-link".to_string(),
                    host: String::new(),
                    chunks: 0,
                    crc: 0,
                    download: "http://new".to_string(),
                    streamable: 0,
                },
                cached_at: std::time::Instant::now(),
            });
        }
        client.evict_expired_cache().await;
        let cache = client.unrestrict_cache.read().await;
        assert!(!cache.contains_key("old-link"), "Expired entry should be evicted");
        assert!(cache.contains_key("new-link"), "Fresh entry should be retained");
    }

    #[test]
    fn cache_constants_are_reasonable() {
        assert_eq!(MAX_CACHE_SIZE, 10_000);
        assert_eq!(CACHE_TTL, Duration::from_secs(3600));
    }

    #[test]
    fn retry_after_cap_constant() {
        assert_eq!(MAX_RETRY_AFTER_SECS, 300);
    }
}
