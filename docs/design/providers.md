# Providers

## Provider abstraction

All components depend on `Arc<dyn DebridProvider>` (defined in `provider.rs`) rather than a concrete client.
`RealDebridClient` and `TorBoxClient` are the two implementations; exactly one provider is active per deployment, chosen at startup by `choose_provider` based on which token (`RD_API_TOKEN` or `TORBOX_API_KEY`) is set.

## TorBox specifics

TorBox lists torrents (with files inline) via `mylist`, resolves a file by `(torrent_id, file_id)` via `requestdl` (so `FileLocator.link` is `None`), re-adds by hash via `createtorrent`, deletes via `controltorrent`, and treats `select_files` as a no-op (TorBox auto-selects).
A finished download is normalised to `status = "downloaded"` even when its cache has lapsed (TorBox keeps such entries listed as Inactive), so owned-but-uncached films still appear in the library and re-acquire on playback.
`mylist?id=` (the single-torrent `get_torrent_info`) can answer a spurious 404 ("No torrents found for this user") for a torrent that exists, so every upgrade staging site looks a freshly-added candidate up through `upgrade::staged_torrent_info`, which retries a 404 after 1, 2, 4 and 8 s (other errors return at once), while `await_downloaded` simply re-polls.
The on-read instant repair retries a 404 on its three lookups too, on a shorter 0.5 s / 1 s schedule (`provider::torrent_info_retrying_not_found` with `repair::REPAIR_INFO_NOT_FOUND_BACKOFF`), since the read is waiting.
TorBox's `progress` (a `0.0–1.0` fraction) is mapped to the canonical `Torrent.progress` as a `0–100` percentage (matching Real-Debrid) so the acquisition engine's stall detector — which resets only on a progress *change* — sees a moving signal for an actively-downloading uncached torrent (without it, `progress` stayed a constant `0.0` and a legitimate download was falsely reaped + blacklisted after `STALL_TIMEOUT_SECS`).
The `mylist` response is parsed defensively: per-torrent fields TorBox sends loosely — `size: -1` (metadata not yet resolved) and `files: null` — are tolerated (signed sizes clamped to 0, null fields defaulted) so one malformed entry cannot fail the whole decode and hide the entire library.
A torrent, file, `createtorrent` or queued id, or a size, sent as a numeric JSON string is read as the number (`lenient_int`), and so is a stringized progress (`lenient_f64`), and a `mylist` listing in which every entry has an unusable id or hash is refused as an `Err` (the tick is skipped, as for any failed listing) rather than returned as an empty account, which the scan loop would trust after 10 minutes.
An add TorBox queues because the account is at its active-download limit is deleted from the queue and fails as provider-busy, so `acquire` retries later instead of queueing more candidates (see `docs/modules/torbox_client.md`).

## Provider-neutral file resolution

The VFS stores a `FileLocator { hash, torrent_id, file_id, file_path, link }` per media file rather than a raw RD link.
`resolve_url(&FileLocator) -> Result<String, AppError>` is the single resolution primitive (and `invalidate(&FileLocator)` drops a cached resolution); RD resolves via the locator's restricted `link` (keeping its old unrestrict/cache logic as private inherent methods), while `(torrent_id, file_id)` is used by providers without per-file links (TorBox).
`AppError::Unavailable` is the provider-neutral "bytes not currently available → re-acquire/repair" signal — RD maps every unrestrict failure except a 401/403 or a 429 to it (a 5xx, a transport-level failure (timeout/connect, no HTTP status), a stale-link 4xx such as 404, or a missing link), and `dav_fs` drives instant repair off `Unavailable` rather than inspecting an HTTP status.

## Adaptive rate limiting

An `AdaptiveRateLimiter` (token bucket, capacity 1, in `ratelimit.rs`) is shared across a provider's API calls — both `RealDebridClient` and `TorBoxClient` use it.
Baseline: Real-Debrid 250 ms (4 req/s; RD documents 250 requests/minute and sends no rate-limit headers, so steady pacing is its only proactive control), TorBox 100 ms (10 req/s).
On 429: interval doubles (max 30s under sustained throttling) and `Retry-After` is respected.
On success: interval **halves** toward baseline (multiplicative recovery, never below the client's baseline) so a bulk throttle burst doesn't leave later requests crawling at the raised ceiling.

**TorBox additionally paces proactively:** `send_data`/`send_ok` read `x-ratelimit-remaining`/`-reset` from every response and call `observe_rate_limit`, holding the next request until the window resets when it is nearly spent — so bulk acquisition stays under TorBox's ~60/window `createtorrent` limit instead of storming into 429s.
Real-Debrid sends no rate-limit headers (confirmed live), so it stays on the reactive MIMD path (multiplicative-increase on 429, multiplicative-decrease on success — see `ratelimit.rs`).
RD signals throttle as a bare `429` (`error_code 34`, no `Retry-After`); separately, **any 5xx** on `unrestrict/link` (503 is RD's `hoster_unavailable`, `error_code 19`; 500/502/504 are infra errors) is a terminal status (no retry) — this is the synchronous on-read playback path, so it must fail fast rather than run the full ~10-attempt backoff (~3 min) that would surface to the player as a hang.
The same path also uses a **small bounded retry budget (3, via `fetch_with_retry_n`)** plus a **10s `connect_timeout`** (separate from the 60s request timeout) so a transport-level hang (timeout/connect) can't stall a WebDAV read for the full ~10×60s window — a dead connect fails over to repair in ~30s instead.
Both read-path calls (RD `unrestrict/link`, TorBox `requestdl`) also set a 15 s per-request timeout (`provider::READ_PATH_REQUEST_TIMEOUT`) in place of the clients' 60 s, so an endpoint that accepts the connection and never answers costs 3 × 15 s, not 3 minutes.
`resolve_url` maps a 5xx, a transport-level failure (no HTTP status), **and a stale/expired-link 4xx such as 404** (recoverable by a re-add-by-hash) to `AppError::Unavailable` → on-demand repair; a permanent auth 4xx (401/403 bad token) and a 429 that outlasts the 3-attempt read budget stay a hard `Http` error that fails the read without repair (`unrestrict_status_skips_repair`, mirrored by TorBox's `requestdl_status_skips_repair`): a re-add can't fix a bad token, and throttling says nothing about whether the bytes exist, so repairing would re-add (on RD, delete) a healthy torrent inside the throttled window.
Note the fast-fail budget bounds the *attempt count and connect phase*, NOT the shared limiter's pre-request wait: since one `AdaptiveRateLimiter` is shared across a provider's calls, under **sustained** throttling a parked `next_allowed` (bounded by `MAX_RETRY_AFTER_SECS` = 300s, and for TorBox by `observe_rate_limit`) can add backpressure latency to an on-read `resolve_url` too.
This is deliberate backpressure (a heavily-throttled provider genuinely can't serve the bytes yet) rather than a bug, and is bounded; a dedicated read-path limiter would trade it for a higher 429 rate against the provider.

## Per-client retry helpers

`rd_client.rs` has `fetch_with_retry(make_request, terminal_statuses)` (default 10-attempt budget) which delegates to `fetch_with_retry_n(make_request, terminal_statuses, max_attempts)` — callers pass the status codes that should abort without retrying (e.g. 404 for `get_torrent_info`, the full 5xx set — 500/502/503/504 — for `unrestrict_link` so the playback resolve path fails fast to repair).
`unrestrict_link` uses `fetch_with_retry_n(.., 3)` (a small budget) so a transport-level hang can't stall a WebDAV read for the full ~10×60s window.
A 2xx whose body breaks off mid-read (a reset, a body cut short of its Content-Length, the request timeout firing while it streams) is retried like a statusless transport error (100 ms·attempt backoff, capped at 1 s).
With the budget spent, the body error itself is returned (no status, so `resolve_url` maps it to `Unavailable`), not the synthetic decode-failure 502.
`torbox_client.rs` has `send_data`/`send_ok`, which rate-limit, retry on 429/transient errors (including a body that breaks off mid-read or a 200 body that fails to decode), and unwrap TorBox's `{success, detail, data}` envelope.
