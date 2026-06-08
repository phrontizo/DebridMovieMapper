# SP2 — Trakt Sync — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Each task is TDD: failing test → minimal code → green → commit. The full existing suite (`cargo test`) must stay green after every task; `sync_account` behaviour is preserved (existing scan tests guard it).

**Goal:** Drive the debrid library from users' Trakt accounts — auto-acquire watchlist + in-progress titles (whole show + new episodes for tracked shows), keep wanted content available, and remove finished/abandoned titles — via a desired-state reconciler beside the existing account-mirror.

**Architecture:** A `TraktClient` (device-flow OAuth, multi-user) pulls each user's watchlist/in-progress/watched + show status into a persisted `wanted` set. A pure reconcile-core diffs `wanted` vs owned-and-available and emits acquire/remove actions per the lifecycle (Trigger A finished / Trigger B abandoned, per-user-aware union). `run_scan_loop` is split into cooperating jobs (`sync_account`, `sync_trakt`, `reconcile_wanted`, `monitor_episodes`, `verify_acquisitions`) behind a small scheduler. A local-network enrolment page links accounts. Acquisition uses the interim SP1 engine, paced (SP3 supersedes).

**Tech Stack:** Rust async (tokio), reqwest, redb, serde, async_trait, chrono, tracing. Spec: `docs/superpowers/specs/2026-06-08-sp2-trakt-sync-design.md`.

---

## File structure

**Create:**
- `src/trakt_client.rs` — `TraktClient` trait + `TraktClientImpl` (device OAuth, token refresh, reads) + `MockTrakt`. Mirrors `scraper.rs`/`provider.rs` seam style.
- `src/wanted.rs` — the wanted-set types + **pure** reconcile-core: `reconcile(wanted, owned, availability) -> Vec<Action>` and the removal-lifecycle predicates (Trigger A/B). No I/O → fully unit-testable.
- `src/scheduler.rs` — spawns/runs the cooperating periodic jobs over `AppState`; replaces the monolithic `run_scan_loop` body.
- `src/enrolment.rs` — local-network web routes for device-flow enrolment + account management.

**Modify:**
- `src/config.rs` — `TRAKT_CLIENT_ID`/`_SECRET`, `TRAKT_SYNC_INTERVAL_SECS`, `EPISODE_CHECK_INTERVAL_SECS`; Trakt-disabled when client id/secret absent.
- `src/tmdb_client.rs` — `show_status(tmdb_id) -> ShowStatus` (Ended/Returning/…), + episode air dates for the monitor.
- `src/store.rs` — `trakt_tokens` + `wanted` tables, v(N)→v(N+1) migration, typed accessors; `OwnedRecord` gains per-user provenance.
- `src/app_state.rs` — carry `Arc<dyn TraktClient>` + the wanted/token store handles.
- `src/tasks.rs` — `sync_trakt`, `reconcile_wanted`, `monitor_episodes` job bodies (or thin wrappers calling `wanted`/`trakt_client`); keep `sync_account` = today's mirror, `verify_acquisitions` = `observe()`.
- `src/main.rs` — start the scheduler instead of the single loop; serve the enrolment routes; **remove the temporary `--acquire` CLI**.
- `src/mapper.rs` — module declarations.
- `CLAUDE.md` / `README.md` — Trakt config, the scheduler/jobs, enrolment, lifecycle.

---

## Tasks

### Task 1 — Config: Trakt knobs
**Files:** modify `src/config.rs` (+ its tests).
**Builds:** parse `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET`, `TRAKT_SYNC_INTERVAL_SECS` (default 900, min 60), `EPISODE_CHECK_INTERVAL_SECS` (default 3600, min 300). A `trakt: Option<TraktConfig>` field — `Some` only when id+secret both present (else Trakt sync disabled, service runs as today).
**Tests:** both-present → `Some`; either-absent → `None`; intervals clamp/default; existing config tests stay green.
**Commit:** `feat(config): Trakt sync config (disabled when client id/secret absent)`.

### Task 2 — TMDB show status + air dates
**Files:** modify `src/tmdb_client.rs` (+ tests).
**Builds:** `ShowStatus { Ended, Returning, Other }` from TMDB show `status`; `show_status(tmdb_id)`; episode air-date lookup for a (show, season) used by the monitor. Reuses the existing retry/limiter.
**Tests:** parse "Ended"/"Returning Series"/"Canceled"/"In Production" → variants (table-driven, on canned JSON); air-date parse.
**Commit:** `feat(tmdb): show ended/returning status + episode air dates`.

### Task 3 — Store: tokens + wanted tables
**Files:** modify `src/store.rs` (+ tests).
**Builds:** `trakt_tokens` (key = user slug → `TraktTokens{access,refresh,expires_at,username}`) and `wanted` (key = `(user, tmdb_id)` → `WantedRecord{ media_type, sources: {watchlist, in_progress}, watched_state, show_status }`). Schema-version bump + forward migration (authoritative → never dropped, per decision #17). Typed async accessors (`put/get/list` per table, `all_wanted()`).
**Tests:** round-trip both tables; migration from the current version preserves existing tables; `all_wanted` aggregates.
**Commit:** `feat(store): trakt_tokens + wanted tables (+ migration)`.

### Task 4 — OwnedRecord per-user provenance
**Files:** modify `src/store.rs` / wherever `OwnedRecord` lives (+ acquire.rs usage, tests).
**Builds:** replace hardcoded `source: "manual"` with `provenance: Provenance` recording, per title, which users + sources (watchlist/in-progress/manual) caused the add. Backward-compatible decode (old records → `manual`). Keeps the removal-safety guard (engine only deletes owned hashes).
**Tests:** old-encoding decode → manual provenance; new round-trip; multi-user provenance merge.
**Commit:** `feat(store): per-user provenance on OwnedRecord`.

### Task 5 — TraktClient (OAuth + reads) + MockTrakt
**Files:** create `src/trakt_client.rs`; declare in `mapper.rs` (+ tests).
**Builds:** `trait TraktClient` with `device_code()`, `poll_token(device_code)`, `refresh(refresh)`, `watchlist(token)`, `in_progress(token)`, `watched(token)`. `TraktClientImpl` (headers `trakt-api-version:2`, `trakt-api-key`, bearer; AIMD limiter honouring `Retry-After`). `MockTrakt` returning canned values (mirrors `MockScraper`). Endpoints per spec §4.1.
**Tests:** parse canned watchlist/in-progress/watched JSON → typed values; device-flow state machine (pending→authorized) on mock HTTP; token-refresh request shape; `MockTrakt` returns canned.
**Commit:** `feat(trakt): TraktClient device-flow OAuth + reads + MockTrakt`.

### Task 6 — Reconcile-core (pure lifecycle)
**Files:** create `src/wanted.rs`; declare in `mapper.rs` (+ tests).
**Builds:** pure functions over plain inputs (no I/O):
- `wants(user_state, title) -> bool` and the two removal triggers.
- `reconcile(combined_wanted, owned, availability) -> Vec<Action>` where `Action ∈ {AcquireMovie, AcquireEpisode, Remove}`; acquire missing/lapsed wanted (movies; all aired episodes of tracked shows), remove per Trigger A (every wanting user finished) / Trigger B (provenance-user un-watchlisted + no other user wants).
**Tests (table-driven — the heart of SP2):** finished movie → Remove; finished+ended show → Remove; returning show fully-watched → Keep; un-watchlist abandon at 0%/partial/full → Remove (Trigger B) only if no other user wants; another user watchlisted-unstarted → Keep; another user in-progress → Keep; wanted-but-unavailable → Acquire (re-acquire); already-owned-and-available → no action.
**Commit:** `feat(wanted): pure reconcile-core + removal lifecycle`.

### Task 7 — `sync_trakt` job
**Files:** modify `src/tasks.rs` (+ tests).
**Builds:** for each enrolled user: refresh token if near expiry; pull watchlist/in-progress/watched + show status; write `wanted`. On a token/refresh failure: mark the account needing re-enrolment, skip it, leave its prior `wanted` intact (never act on a failed fetch). Uses `MockTrakt` seam for tests.
**Tests:** mock user → `wanted` populated; token-expired → refresh called; fetch error → wanted unchanged + account flagged.
**Commit:** `feat(tasks): sync_trakt job (Trakt → wanted)`.

### Task 8 — `reconcile_wanted` job
**Files:** modify `src/tasks.rs` (+ tests).
**Builds:** read combined `wanted` + owned + availability (owned status: cached/inactive/missing) → `wanted::reconcile` → drive the SP1 engine (`engine.acquire`, paced) for acquires and `provider.delete_torrent` (owned-only) for removes; record provenance. Idempotent.
**Tests:** with `MockProvider`+`MockScraper`+ canned wanted: a missing wanted movie → acquire called; a Trigger-A title → delete called (owned only); a lapsed owned wanted → re-acquire; manual (non-owned) never deleted.
**Commit:** `feat(tasks): reconcile_wanted job drives engine acquire/remove`.

### Task 9 — `monitor_episodes` job
**Files:** modify `src/tasks.rs` (+ tests).
**Builds:** for tracked shows, find newly-aired episodes (TMDB air dates ≤ now, not yet owned) → enqueue acquisition. Cadence `EPISODE_CHECK_INTERVAL_SECS`.
**Tests:** episode aired-yesterday + not owned → acquire; aired-tomorrow → skip; already owned → skip (chrono boundary cases).
**Commit:** `feat(tasks): monitor_episodes auto-acquires newly-aired episodes`.

### Task 10 — Scheduler split + wiring
**Files:** create `src/scheduler.rs`; modify `src/main.rs`, `src/app_state.rs`, `src/tasks.rs`, `mapper.rs` (+ tests).
**Builds:** a scheduler that spawns the 5 jobs as independent periodic tasks over `AppState` (each its own interval); `sync_account` keeps today's mirror behaviour, `verify_acquisitions` = `observe()`. `main` starts the scheduler instead of the single `run_scan_loop`. AppState carries the Trakt client + wanted/token handles.
**Tests:** scheduler spawns/cancels cleanly; `sync_account` unchanged (existing scan tests); jobs run on their cadence (paused-time test).
**Commit:** `feat(scheduler): split run_scan_loop into cooperating jobs`.

### Task 11 — Enrolment page (local-network)
**Files:** create `src/enrolment.rs`; modify `src/main.rs` (serve routes), `mapper.rs` (+ tests).
**Builds:** minimal routes on the existing listener (local-network trust, no extra auth): `POST /trakt/enrol` (start device-flow → return `user_code` + `verification_url`), background poll to completion (store tokens), `GET /trakt/accounts` (list), `POST /trakt/accounts/:slug/refresh|remove`. Functional HTML, no styling beyond minimum.
**Tests:** enrol handler returns code+url (mock Trakt); poll-completion stores tokens; list/remove update the store.
**Commit:** `feat(enrolment): local-network Trakt device-flow enrolment page`.

### Task 12 — Remove `--acquire` CLI + disabled-path
**Files:** modify `src/main.rs` (+ tests).
**Builds:** delete the temporary `--acquire` trigger (Trakt + reconciler are the triggers now). When `config.trakt` is `None`, the Trakt jobs/enrolment are not started — the service runs exactly as today (account-mirror + repair only).
**Tests:** no-Trakt-config startup path runs the mirror only; `--acquire` arg no longer recognised.
**Commit:** `refactor(main): remove --acquire CLI; gate Trakt jobs on config`.

### Task 13 — Live smoke + docs + gate
**Files:** `tests/` (new `#[ignore]` Trakt smoke), `CLAUDE.md`, `README.md`.
**Builds:** `#[ignore]` device-flow + watchlist-pull smoke (skips when `TRAKT_CLIENT_ID` unset); update CLAUDE.md (jobs/scheduler, Trakt config, enrolment, lifecycle, `--acquire` removal) and README. Run the full pre-commit gate.
**Commit:** `test(trakt): live smoke; docs: SP2 (scheduler, Trakt, enrolment, lifecycle)`.

---

## Self-review (against the spec)
- **Spec coverage:** §2 wanted-set/lifecycle → Tasks 4,6,7,8; §4.1 client → Task 5; §4.2 store → Tasks 3,4; §4.3 scheduler → Task 10 + 7/8/9; §4.4 enrolment → Task 11; availability re-acquire → Task 8; config → Task 1; testing → every task + Task 13. No gaps.
- **Type consistency:** `WantedRecord`/`TraktTokens`/`Provenance`/`ShowStatus`/`Action` named once and reused; `TraktClient` method set fixed in Task 5 and consumed in 7/9/11.
- **Note on detail level:** task-level (interfaces + tests + commits), not every implementation line inlined — given SP2's size, per-task implementers write the TDD code against the cited patterns (`scraper.rs`/`provider.rs` for the trait+Mock seam, `store.rs` for tables/migration, `tasks.rs` for jobs). Worth a fuller per-step code pass with fresh context if preferred.

## Non-goals (deferred to SP3/SP4)
SP3 reconciliation/upgrade (better-release upgrades, episode→season-pack consolidation, optimistic-add + async reconcile — which removes SP1's interim sync fixes). Per-user libraries; custom-list sources; full web UI (SP4).
