# Rate-Limit Resilience & Paced Bulk Acquisition — Design

**Goal:** Make acquisition robust under provider rate limits so *bulk* operations — the RD→TorBox migration now, and Trakt sync (SP2) later — self-pace to each provider's real limit instead of storming into a 429/503 cascade.

**Status:** SP2 groundwork. Engine/provider-layer changes shared by the migration tool and the future SP2 scheduler.

## Background

The RD→TorBox migration stress-tested the acquisition engine and exposed that it is built for *one title, on demand*, not bulk. Run against ~120 missing movies it collapsed: TorBox `createtorrent` 429-stormed (6 retries → 502 → next candidate, repeat) at ~0 throughput, and probe fetches under that load were truncated and mis-rejected as `Corrupt`. The same load profile will hit SP2 when Trakt drives a watchlist.

## Measured provider behaviour (2026-06-07, live)

| Provider | Throttle signal | Timing hint | Notes |
|---|---|---|---|
| **TorBox** | `429` on `createtorrent`; rate-limit headers on **every** response | **Explicit** | `x-ratelimit-limit: 60`, `x-ratelimit-remaining`, `x-ratelimit-reset` (unix ts), `retry-after: 60`. `createtorrent` ≈ 60/window. |
| **Real-Debrid** | `429`, body `{"error":"too_many_requests","error_code":34}` | **None** | No `Retry-After`, no rate-limit headers. Short window (recovers in seconds). Needs concurrency to trip. |
| **Real-Debrid** | `503`, body `{"error":"hoster_unavailable","error_code":19}` | n/a | **Genuine torrent-unavailable**, *not* a rate limit. Existing terminal→repair handling is correct and **unchanged**. |

Key consequences: TorBox can be paced **proactively** from its headers; RD can only be handled **reactively** (infer backoff) because it gives no timing hint. The existing `AdaptiveRateLimiter` already exists for RD's 429s — its tuning is RD's only lever.

## Changes

### 1. Improve the shared `AdaptiveRateLimiter` (`ratelimit.rs`)
Today: ×2 interval on 429 (cap **2000 ms**), −10 ms per success (additive, slow). Under sustained throttling it pins at 2 s — still above TorBox's `createtorrent` limit — and recovers far too slowly for normal single-title use afterwards.

- Raise `MAX_INTERVAL_MS` so sustained throttling can back off well past 2 s (target ~30 s).
- Replace additive recovery with **faster recovery**: multiplicative decrease on success (e.g. ×0.5 toward baseline) and/or snap to baseline after a short quiet period, so a bulk burst doesn't leave the live path crawling.
- Keep `Retry-After` honouring (cap 300 s) for providers that send it (TorBox).

This is RD's *only* lever (bare 429) and also makes TorBox's reactive fallback sane.

**Tests:** sustained 429s exceed the old 2 s cap; recovery returns to baseline quickly (multiplicative / quiet-period reset); `Retry-After` still advances `next_allowed`.

### 2. TorBox header-aware proactive pacing (`torbox_client.rs` + `ratelimit.rs`)
Add `observe_rate_limit(remaining: u64, reset_epoch: f64)` to the limiter: when `remaining` is at/below a small threshold, set `next_allowed` to the reset instant — proactively pausing until the window rolls over, so we never trip 429. `send_data`/`send_ok` parse `x-ratelimit-remaining`/`-reset` from **every** response and call it; on 429 they additionally honour `retry-after`. Providers that don't send the headers (RD) simply never call it → RD behaviour unchanged.

**Tests:** `observe_rate_limit` with low `remaining` advances `next_allowed` to the reset; with ample `remaining` it's a no-op; header parsing (string→u64/f64, missing → ignored).

### 3. Pace bulk acquisition (consumer pattern + lean migration tool)
Bulk pacing falls out of the capacity-1 shared limiter **provided consumers acquire serially** and let it pace — no parallel fan-out. Deliverables:
- Rebuild the throwaway `examples/migrate_rd_to_torbox.rs` on the lean pattern: index TorBox once; decide each title from scraper metadata (cached flag, title-validate, quality floor, pack-size); **one** `createtorrent` per accepted title through the improved limiter; no per-failure `mylist` fallback storm; no add-then-delete.
- Document that **SP2's scheduler drains the wanted-set gradually across scan ticks** (not all at once), reusing the same limiter.

**Tests:** covered indirectly (the limiter tests in #1/#2 are the pacing guarantees); the migration tool is a `#[ignore]`/example, validated live.

### 4. Probe truncation → `Transient`, not `Corrupt` (`probe.rs`)
A short/truncated 4 MB fetch (an element/box whose declared size extends **past the fetched buffer**) currently returns `Corrupt` → blacklist. That's indistinguishable from a genuinely-broken file and caused false blacklisting under load. Distinguish **under-fetch** (declared size > fetched bytes → `Transient`, i.e. defer + retry) from **malformed** (bad magic / impossible structure within the fetched bytes → `Corrupt`).

**Tests:** a container truncated mid-element → `Transient` (update the existing `mkv_truncated_is_corrupt` accordingly); a genuinely-malformed header within-bounds → still `Corrupt`; the overflow/`largesize` cases stay `Corrupt`.

## Non-goals
- RD `503`/`error_code 19` handling (genuine unavailable → repair) is **unchanged** — it is correct.
- The full SP2 scheduler (wanted-set drainer across ticks) is future SP2 work; this spec only provides the rate-limit substrate it will sit on.
- No change to the on-demand single-title path's behaviour beyond the limiter recovering faster.

## Test gate
`cargo test` green; the limiter + probe unit tests above; the lean migration tool validated live against TorBox once its limiter recovers.
