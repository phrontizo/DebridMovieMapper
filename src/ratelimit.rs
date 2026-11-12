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
    /// The baseline interval: the pace with no throttling, and the floor `record_success` recovers to.
    min_interval_ms: u64,
    /// The back-off ceiling: `MAX_INTERVAL_MS`, or the baseline when that is higher.
    max_interval_ms: u64,
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
    /// The providers' limiter: a 100 ms baseline (10 req/s).
    pub fn new() -> Self {
        Self::with_min_interval(Duration::from_millis(MIN_INTERVAL_MS))
    }

    /// A limiter whose baseline is `min_interval` (at least 1 ms): requests are spaced by it when
    /// nothing is throttled, a 429 backs off from it, and recovery never goes below it. The scraper
    /// uses this — Torrentio sends no rate-limit headers, so steady pacing is what keeps a burst of
    /// scrapes under its limit.
    pub fn with_min_interval(min_interval: Duration) -> Self {
        let min_interval_ms = (min_interval.as_millis() as u64).max(1);
        Self {
            state: tokio::sync::Mutex::new(RateLimiterState {
                interval_ms: min_interval_ms,
                next_allowed: tokio::time::Instant::now(),
            }),
            min_interval_ms,
            max_interval_ms: MAX_INTERVAL_MS.max(min_interval_ms),
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
        state.interval_ms = (state.interval_ms / 2).max(self.min_interval_ms);
    }

    /// Record a 429 throttle — double the interval and optionally respect Retry-After.
    ///
    /// Under concurrency, N requests that each independently receive a 429 each double the interval,
    /// so a burst can jump it by up to ~2^N in one tick (saturating to `MAX_INTERVAL_MS`); recovery
    /// is then one halving per success. This is bounded and self-correcting (the interval is capped
    /// and `record_success` walks it back), so it's accepted rather than de-duplicated per window.
    pub async fn record_throttle(&self, retry_after: Option<u64>) {
        let mut state = self.state.lock().await;
        state.interval_ms = state
            .interval_ms
            .saturating_mul(2)
            .min(self.max_interval_ms);
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
    /// headers simply never call this, leaving the reactive back-off/recovery path untouched
    /// (Real-Debrid). NB: that path is multiplicative both ways (double on 429, halve on success) —
    /// i.e. MIMD, not the additive recovery of textbook AIMD.
    pub async fn observe_rate_limit(&self, remaining: u64, reset_epoch_secs: f64) {
        // A non-finite reset (a header parsed as `inf`/`nan` — both accepted by `f64::from_str`) means
        // "no usable pacing info", NOT "wait the maximum": `inf.min(300)` and `nan.min(300)` both
        // collapse to 300, so without this guard one garbage header would park ALL provider traffic
        // (the limiter is shared) for the full cap. Ignore it. (Also avoids a `from_secs_f64(NaN)`.)
        if !reset_epoch_secs.is_finite() {
            return;
        }
        // Park only once the window is down to its last `LOW_REMAINING` slots. This deliberately
        // strands those final slots (we wait for the reset rather than spend them) as headroom
        // against a miscounted window tripping a 429 — a small, intentional throughput cost.
        if remaining > LOW_REMAINING {
            return;
        }
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let wait_secs = reset_epoch_secs - now_unix;
        // A legitimate TorBox reset lands within the current window (seconds, at most ~a minute), so
        // a delta beyond MAX_RETRY_AFTER_SECS is not a long-but-real wait — it's a bad reading: a
        // reset sent in MILLISECONDS instead of seconds (e.g. 1.78e12), or a clock set before the
        // UNIX epoch making `now_unix` 0 (so `reset - 0 ≈ 1.78e9`). Clamping such a value to the cap
        // (the old `.min(MAX_RETRY_AFTER_SECS)`) would park the SHARED limiter for the full 5 minutes
        // off one garbage header — the same availability cliff the non-finite guard above avoids — so
        // ignore an out-of-range delta entirely (the reactive 429 path still protects us).
        if wait_secs > 0.0 && wait_secs <= MAX_RETRY_AFTER_SECS as f64 {
            let mut state = self.state.lock().await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(wait_secs);
            if deadline > state.next_allowed {
                state.next_allowed = deadline;
            }
        }
    }

    /// Test-only: peek the current `next_allowed` instant so a cross-module test (e.g. the TorBox
    /// client's `observe_rate_headers` glue) can assert the shared limiter was parked. `state` is
    /// private to this module, so tests elsewhere can't reach `next_allowed` directly.
    #[cfg(test)]
    pub(crate) async fn peek_next_allowed(&self) -> tokio::time::Instant {
        self.state.lock().await.next_allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn interval(rl: &AdaptiveRateLimiter) -> u64 {
        rl.state.lock().await.interval_ms
    }

    /// A limiter built with a custom baseline (the scraper's) spaces requests by it, backs off from
    /// it on a 429, and never recovers BELOW it — while `new()` keeps the providers' 100 ms baseline.
    #[tokio::test(start_paused = true)]
    async fn a_custom_baseline_paces_requests_and_bounds_recovery() {
        let rl = AdaptiveRateLimiter::with_min_interval(Duration::from_millis(1_000));
        let t0 = tokio::time::Instant::now();
        for _ in 0..3 {
            rl.wait_for_token().await;
        }
        let spent = t0.elapsed();
        assert!(
            spent >= Duration::from_millis(2_000) && spent < Duration::from_millis(2_100),
            "three tokens take two baseline intervals, got {spent:?}"
        );
        rl.record_success().await;
        assert_eq!(
            interval(&rl).await,
            1_000,
            "success never goes below the baseline"
        );
        rl.record_throttle(None).await;
        assert_eq!(
            interval(&rl).await,
            2_000,
            "a 429 doubles from the baseline"
        );
        rl.record_success().await;
        rl.record_success().await;
        assert_eq!(interval(&rl).await, 1_000);
        assert_eq!(interval(&AdaptiveRateLimiter::new()).await, MIN_INTERVAL_MS);

        // A baseline above the usual 30 s ceiling is still a floor under back-off.
        let slow = AdaptiveRateLimiter::with_min_interval(Duration::from_secs(60));
        slow.record_throttle(None).await;
        assert_eq!(interval(&slow).await, 60_000);
    }

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

    #[tokio::test]
    async fn record_throttle_without_retry_after_doubles_interval_but_does_not_advance_next_allowed(
    ) {
        // A bare 429 (no Retry-After — Real-Debrid's case) must ONLY double the interval, never park
        // `next_allowed` in the future. An accidental unconditional advance would add spurious latency
        // to every subsequent request (incl. the on-read resolve path).
        let limiter = AdaptiveRateLimiter::new();
        let before = tokio::time::Instant::now();
        limiter.record_throttle(None).await;
        let state = limiter.state.lock().await;
        assert_eq!(state.interval_ms, 200, "interval must double");
        assert!(
            state.next_allowed <= before,
            "a bare 429 (no Retry-After) must NOT advance next_allowed"
        );
    }

    #[tokio::test]
    async fn retry_after_is_capped_to_the_max() {
        // BEHAVIOUR (not a literal pin): a Retry-After far beyond the cap must clamp the wait to
        // MAX_RETRY_AFTER_SECS — a hostile/buggy header must never park the limiter for hours.
        let limiter = AdaptiveRateLimiter::new();
        let before = tokio::time::Instant::now();
        limiter.record_throttle(Some(100_000)).await;
        let wait = limiter
            .state
            .lock()
            .await
            .next_allowed
            .duration_since(before);
        assert!(
            wait <= Duration::from_secs(MAX_RETRY_AFTER_SECS) + Duration::from_secs(1),
            "a huge Retry-After must be clamped to the cap, was {wait:?}"
        );
        assert!(
            wait >= Duration::from_secs(MAX_RETRY_AFTER_SECS) - Duration::from_secs(1),
            "the cap should still be applied (wait near the cap), was {wait:?}"
        );
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
        // Bound BOTH sides: the pause must be ~the ~10s reset window, so an over-pause (an uncapped
        // or doubled wait) is caught too — a one-sided `>= 8s` would pass a runaway pause.
        assert!(
            state.next_allowed >= before + Duration::from_secs(8),
            "should pause until the window reset"
        );
        assert!(
            state.next_allowed <= before + Duration::from_secs(12),
            "must not pause materially LONGER than the ~10s reset window (no uncapped/doubled wait)"
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
    async fn observe_rate_limit_ignores_a_non_finite_reset() {
        // A garbage/non-finite `x-ratelimit-reset` (inf/nan) must be ignored, NOT treated as a
        // max-length (300s) wait that parks all shared provider traffic.
        let limiter = AdaptiveRateLimiter::new();
        let before = { limiter.state.lock().await.next_allowed };
        limiter.observe_rate_limit(0, f64::INFINITY).await;
        limiter.observe_rate_limit(0, f64::NAN).await;
        let state = limiter.state.lock().await;
        assert_eq!(
            state.next_allowed, before,
            "a non-finite reset must not delay the next request"
        );
    }

    #[tokio::test]
    async fn observe_rate_limit_ignores_an_implausibly_large_delta() {
        // A finite-but-wrong-magnitude reset — a value sent in MILLISECONDS instead of seconds, or a
        // pre-epoch clock making `now_unix` 0 — yields a delta far beyond the cap. It must be ignored
        // (not clamped to the 300s cap, which would park all shared provider traffic for 5 minutes).
        let limiter = AdaptiveRateLimiter::new();
        let before = { limiter.state.lock().await.next_allowed };
        // `reset` in milliseconds (~1.78e12) rather than seconds → delta ≈ 1.78e12.
        limiter.observe_rate_limit(0, 1_780_000_000_000.0).await;
        // A pre-epoch clock degrades `now_unix` to 0, so `reset - 0` is a whole epoch of seconds.
        limiter.observe_rate_limit(0, now_unix_secs()).await;
        let state = limiter.state.lock().await;
        assert_eq!(
            state.next_allowed, before,
            "an out-of-range reset delta must not park the shared limiter"
        );
    }

    #[tokio::test]
    async fn observe_rate_limit_parks_at_the_low_remaining_boundary() {
        // The pause fires for `remaining <= LOW_REMAINING` and is a no-op above it. Pin the exact
        // boundary so an off-by-one in `remaining > LOW_REMAINING` is caught: LOW_REMAINING parks,
        // LOW_REMAINING + 1 does not.
        let at_boundary = AdaptiveRateLimiter::new();
        let before = { at_boundary.state.lock().await.next_allowed };
        at_boundary
            .observe_rate_limit(LOW_REMAINING, now_unix_secs() + 10.0)
            .await;
        assert!(
            at_boundary.state.lock().await.next_allowed > before,
            "remaining == LOW_REMAINING must park"
        );

        let just_above = AdaptiveRateLimiter::new();
        let before = { just_above.state.lock().await.next_allowed };
        just_above
            .observe_rate_limit(LOW_REMAINING + 1, now_unix_secs() + 10.0)
            .await;
        assert_eq!(
            just_above.state.lock().await.next_allowed,
            before,
            "remaining == LOW_REMAINING + 1 must NOT park"
        );
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
