use std::time::Duration;

const MIN_INTERVAL_MS: u64 = 100; // 10 req/s max (baseline)
const MAX_INTERVAL_MS: u64 = 30_000; // 30s ceiling under sustained throttling
pub const MAX_RETRY_AFTER_SECS: u64 = 300; // Cap Retry-After / proactive wait to 5 minutes
const LOW_REMAINING: u64 = 2; // proactively pause when a provider's window is nearly spent

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

    /// Record a successful request — recover quickly (multiplicative halving) toward baseline so a
    /// bulk throttle burst doesn't leave later requests crawling at the raised ceiling.
    pub async fn record_success(&self) {
        let mut state = self.state.lock().await;
        state.interval_ms = (state.interval_ms / 2).max(MIN_INTERVAL_MS);
    }

    /// Record a 429 throttle — double the interval and optionally respect Retry-After.
    pub async fn record_throttle(&self, retry_after: Option<u64>) {
        let mut state = self.state.lock().await;
        state.interval_ms = state.interval_ms.saturating_mul(2).min(MAX_INTERVAL_MS);
        if let Some(seconds) = retry_after {
            let capped_seconds = seconds.min(MAX_RETRY_AFTER_SECS);
            let retry_deadline = tokio::time::Instant::now() + Duration::from_secs(capped_seconds);
            if retry_deadline > state.next_allowed {
                state.next_allowed = retry_deadline;
            }
        }
    }

    /// Proactively pace from a provider's advertised rate-limit window (TorBox sends
    /// `x-ratelimit-remaining` + `x-ratelimit-reset`). When the window is nearly spent, hold the
    /// next request until the reset instant so we never trip a 429. Providers that don't send these
    /// headers simply never call this, leaving the reactive AIMD path untouched (Real-Debrid).
    pub async fn observe_rate_limit(&self, remaining: u64, reset_epoch_secs: f64) {
        if remaining > LOW_REMAINING {
            return;
        }
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let wait_secs = (reset_epoch_secs - now_unix).min(MAX_RETRY_AFTER_SECS as f64);
        if wait_secs > 0.0 {
            let mut state = self.state.lock().await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(wait_secs);
            if deadline > state.next_allowed {
                state.next_allowed = deadline;
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
        // Double repeatedly until the 30s ceiling: 100 -> 200 -> ... -> 30000 (capped)
        for _ in 0..12 {
            limiter.record_throttle(None).await;
        }
        let state = limiter.state.lock().await;
        assert_eq!(state.interval_ms, MAX_INTERVAL_MS);
    }

    #[tokio::test]
    async fn adaptive_limiter_backs_off_well_past_two_seconds() {
        let limiter = AdaptiveRateLimiter::new();
        // Sustained throttling must escape the old 2s cap (TorBox createtorrent needs more).
        for _ in 0..8 {
            limiter.record_throttle(None).await;
        }
        let state = limiter.state.lock().await;
        assert!(state.interval_ms > 2000, "got {}", state.interval_ms);
    }

    #[tokio::test]
    async fn adaptive_limiter_recovers_multiplicatively_on_success() {
        let limiter = AdaptiveRateLimiter::new();
        limiter.record_throttle(None).await; // 100 -> 200
        limiter.record_throttle(None).await; // 200 -> 400
        limiter.record_success().await; // 400 -> 200 (halved, not -10)
        let state = limiter.state.lock().await;
        assert_eq!(state.interval_ms, 200);
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

    fn now_unix_secs() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    #[tokio::test]
    async fn observe_rate_limit_pauses_until_reset_when_low() {
        let limiter = AdaptiveRateLimiter::new();
        let before = tokio::time::Instant::now();
        // 1 request left in the window, which resets in ~10s.
        limiter.observe_rate_limit(1, now_unix_secs() + 10.0).await;
        let state = limiter.state.lock().await;
        assert!(
            state.next_allowed >= before + Duration::from_secs(8),
            "should pause until reset"
        );
    }

    #[tokio::test]
    async fn observe_rate_limit_noop_when_ample_remaining() {
        let limiter = AdaptiveRateLimiter::new();
        let before_na = { limiter.state.lock().await.next_allowed };
        // Plenty of headroom — must not delay the next request at all.
        limiter.observe_rate_limit(50, now_unix_secs() + 60.0).await;
        let state = limiter.state.lock().await;
        assert_eq!(state.next_allowed, before_na);
    }

    #[tokio::test]
    async fn observe_rate_limit_noop_when_reset_in_past() {
        let limiter = AdaptiveRateLimiter::new();
        let before_na = { limiter.state.lock().await.next_allowed };
        // Low remaining but the window already reset — nothing to wait for.
        limiter.observe_rate_limit(0, now_unix_secs() - 5.0).await;
        let state = limiter.state.lock().await;
        assert_eq!(state.next_allowed, before_na);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_token_serializes_concurrent_callers() {
        use std::sync::Arc;
        // The whole point of the limiter: concurrent callers are serialised to distinct
        // slots at least `interval` apart, rather than all firing at once.
        let limiter = Arc::new(AdaptiveRateLimiter::new());
        let start = tokio::time::Instant::now();
        let mut handles = Vec::new();
        for _ in 0..4 {
            let l = limiter.clone();
            handles.push(tokio::spawn(async move {
                l.wait_for_token().await;
                tokio::time::Instant::now().duration_since(start)
            }));
        }
        let mut elapsed = Vec::new();
        for h in handles {
            elapsed.push(h.await.unwrap());
        }
        elapsed.sort();
        let interval = Duration::from_millis(MIN_INTERVAL_MS);
        // The k-th caller (sorted) cannot have completed before k*interval.
        for (k, e) in elapsed.iter().enumerate() {
            assert!(
                *e >= interval * k as u32,
                "caller {} completed at {:?}, before its {:?} slot",
                k,
                e,
                interval * k as u32
            );
        }
        // And the last of four is spaced a full 3 intervals out.
        assert!(*elapsed.last().unwrap() >= interval * 3);
    }
}
