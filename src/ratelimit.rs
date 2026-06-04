use std::time::Duration;

const MIN_INTERVAL_MS: u64 = 100; // 10 req/s max (baseline)
const MAX_INTERVAL_MS: u64 = 2000; // 0.5 req/s min (under heavy throttling)
const RECOVERY_MS: u64 = 10; // Decrease interval by 10ms per success
pub const MAX_RETRY_AFTER_SECS: u64 = 300; // Cap Retry-After to 5 minutes

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

impl Default for AdaptiveRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveRateLimiter {
    pub fn new() -> Self {
        Self {
            state: tokio::sync::Mutex::new(RateLimiterState {
                interval_ms: MIN_INTERVAL_MS,
                next_allowed: tokio::time::Instant::now(),
            }),
        }
    }

    /// Wait until a token is available, then reserve it.
    pub async fn wait_for_token(&self) {
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
    pub async fn record_success(&self) {
        let mut state = self.state.lock().await;
        state.interval_ms = state
            .interval_ms
            .saturating_sub(RECOVERY_MS)
            .max(MIN_INTERVAL_MS);
    }

    /// Record a 429 throttle — double the interval and optionally respect Retry-After.
    pub async fn record_throttle(&self, retry_after: Option<u64>) {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn retry_after_cap_constant() {
        assert_eq!(MAX_RETRY_AFTER_SECS, 300);
    }
}
