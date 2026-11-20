# `ratelimit.rs`

Shared `AdaptiveRateLimiter` (token bucket) used by TorBox and the Trakt client (`new()`: 100 ms baseline) and, with its own baseline (`with_min_interval`; recovery never drops below it, and back-off may exceed the 30 s ceiling only when the baseline does), by the Real-Debrid client (250 ms, RD's documented 250 requests/minute) and the scraper.
TMDB uses its own fixed 100 ms pacer in `tmdb_client.rs`, not this limiter.
The pure `plausible_wait_secs` holds the reset-delta sanity check `observe_rate_limit` uses
