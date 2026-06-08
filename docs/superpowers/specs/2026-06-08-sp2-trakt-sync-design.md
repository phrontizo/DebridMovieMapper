# SP2 — Trakt Sync — Design

**Goal:** Drive the debrid library from users' Trakt accounts — automatically acquire what they want to watch (and keep tracked shows current), and remove what they're done with — instead of acquisition being triggered manually.

**Status:** Phase SP2 of the Trakt-integration roadmap (`docs/superpowers/specs/2026-06-05-trakt-integration-design.md`). Builds on SP0 (Config/Store/AppState) and SP1 (acquisition engine). Feeds the existing SP1 engine via a paced work-list — the **interim** synchronous acquisition path. SP3 (reconciliation/upgrade) later replaces that path and removes SP1's interim sync fixes; SP2 is designed to drop straight onto it.

---

## 1. Scope

**In:**
- `trakt_client.rs` — multi-user device-flow OAuth + the Trakt reads we need.
- A persisted **wanted-set** (desired state) keyed by tmdb id, with a structured season/episode model.
- A **reconciler** that diffs wanted-vs-owned and drives the SP1 engine to acquire/remove.
- An **episode monitor** that auto-acquires newly-aired episodes of tracked shows.
- A **scheduler** that splits `run_scan_loop` into cooperating periodic jobs.
- A **minimal enrolment web page** to link/refresh/remove Trakt accounts.

**Out (deferred):**
- SP3 reconciliation/upgrade engine (better-release upgrades, episode→pack consolidation, the optimistic-add+async-reconcile model). SP2 uses the interim engine.
- Per-user libraries / per-user VFS (rejected — one shared household library).
- Arbitrary custom Trakt lists as a source (only watchlist + in-progress).
- Full web UI (dashboard, match-correction, settings) — SP4.

---

## 2. Wanted-set & removal lifecycle (the core semantics)

### Sources (acquisition)
Per enrolled user, the wanted-set is the **union** of:
- **Watchlist** — movies and shows.
- **In-progress** — movies and shows the user is currently watching (Trakt playback progress).

A **movie** in the wanted-set → acquire it. A **show** in the wanted-set is **tracked** → acquire **all aired episodes** (full back-catalogue) and auto-acquire newly-aired episodes as they air.

### Keep / remove (per-user-aware union)
Only **engine-owned** torrents are ever removed (never manual adds). A title is **kept** unless one of two triggers fires:

- **Trigger A — finished:** every user who has it watchlisted or in-progress has *finished* it — a movie is **watched**; a show is **100%-watched AND ended** (all aired episodes watched, and TMDB/Trakt status is not "Returning Series"/"In Production"). A user who has it watchlisted but hasn't started, or is mid-watch, has *not* finished → blocks removal.
- **Trigger B — abandoned:** the user whose **watchlist added** it has **un-watchlisted** it (explicit abandon, regardless of how much they watched), **and** no other user wants it (no other user has it watchlisted or in-progress).

Otherwise **keep**.

This requires **provenance**: each owned title records which user(s) and via which source (watchlist / in-progress) it entered the wanted-set, so "removed from the watchlist that added it" is answerable and one user's abandon never deletes a title another user wants.

### Availability ("wanted = owned AND available")
Debrid providers don't delete account entries for inactivity, but they **let cached data lapse** (TorBox marks inactive torrents `reported missing`; RD can return `503 hoster_unavailable`). So the reconciler treats a wanted title that is **owned but no longer available** (lapsed/missing/503) as needing **re-acquisition** — it re-acquires proactively rather than only on a failed playback. This reuses the existing repair/re-acquire machinery and the engine.

---

## 3. Architecture

**Re-centre, not rewrite** (decision #15). A desired-state **reconciler** sits beside the existing account-mirror projection. `DebridProvider`, `FileLocator`, the VFS-as-projection, `redb`, the proxy, repair, and the SP1 engine are unchanged.

```
Trakt (per user) ──sync_trakt──▶  wanted (redb)  ──reconcile_wanted──▶  SP1 engine.acquire (paced)
                                      ▲                                        │
TMDB air dates ──monitor_episodes─────┘                              owned_hashes / VFS / proxy
debrid account ──sync_account──▶ VFS mirror (unchanged)              verify_acquisitions = observe()
```

Acquisition stays **paced** through the shared rate limiter (own-call politeness; no slot manager), which the 2026-06-08 rate-limit-resilience work already provides (TorBox header-aware, RD AIMD).

---

## 4. Components

### 4.1 `trakt_client.rs`
A `TraktClient` trait (+ `MockTrakt` test seam, mirroring `Scraper`/`DebridProvider`) over the Trakt API (`https://api.trakt.tv`, headers `trakt-api-version: 2`, `trakt-api-key: <client_id>`, `Authorization: Bearer <token>`).

- **Device-flow OAuth:** `POST /oauth/device/code` (→ `device_code`, `user_code`, `verification_url`, `interval`, `expires_in`); poll `POST /oauth/device/token` until authorized (→ `access_token`, `refresh_token`, `expires_in`); refresh via `POST /oauth/token` (`grant_type=refresh_token`). Needs `client_id` + `client_secret`.
- **Reads (per user):**
  - Watchlist — `GET /sync/watchlist/movies`, `GET /sync/watchlist/shows`.
  - In-progress — `GET /sync/playback` (movies + episodes with progress).
  - Watched — `GET /sync/watched/movies`, `GET /sync/watched/shows` (per-show watched episodes, for the finish test).
- **Show ended/returning status** comes from TMDB (`tmdb_client` — show `status` field), not Trakt, to reuse existing identification plumbing.
- Trakt rate limits are respected with an `AdaptiveRateLimiter` (same primitive as the providers) honouring `Retry-After`.

### 4.2 Store tables (redb; authoritative → migrated, never silently dropped — decision #17)
- `trakt_tokens` — per user: access/refresh tokens, expiry, Trakt username/slug.
- `wanted` — the materialised desired state: per (user, tmdb_id) the source(s) (watchlist/in-progress), media type, watched-state (movie watched; show watched-episode set), show status, and provenance. The reconciler reads the **combined** view; per-user rows make the removal lifecycle computable. Reuses SP1's `owned_hashes` (now carrying per-user provenance via `OwnedRecord`) and `authoritative_ids`.

### 4.3 Scheduler (split `run_scan_loop` into cooperating jobs over `AppState` + store)
- `sync_account` — the existing debrid-account → VFS mirror. **Behaviour-preserving.**
- `sync_trakt` — for each enrolled user, pull watchlist/in-progress/watched + show status → update `wanted` (cadence `TRAKT_SYNC_INTERVAL_SECS`).
- `reconcile_wanted` — diff combined wanted vs owned-**and-available**: enqueue acquisitions for missing/lapsed wanted (movies; all aired episodes of tracked shows) and removals per §2 (engine-owned only). Paced.
- `monitor_episodes` — for tracked shows, detect newly-aired episodes (TMDB air dates + `chrono`) → enqueue acquisition (cadence `EPISODE_CHECK_INTERVAL_SECS`).
- `verify_acquisitions` — the existing `observe()` (verify pending, stall-reacquire).

Jobs are independent periodic tasks sharing state through `AppState` + the store; each is individually testable.

### 4.4 Enrolment page
A minimal route on the existing server (the WebDAV listener): **local-network trust** (no extra auth, consistent with WebDAV/rclone today). Lets a user start device-flow enrolment (display `user_code` + `verification_url`, poll to completion), list enrolled accounts, and refresh/remove an account. No styling beyond the functional minimum.

### 4.5 Acquisition
Feeds the SP1 engine via the paced work-list; `OwnedRecord.source` becomes per-user Trakt provenance (replacing the hardcoded `"manual"`). The engine's removal-safety guard (only deletes hashes in `owned_hashes`) is what makes Trakt-driven removal safe for manual adds.

---

## 5. Config
- `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET` — required to enable Trakt sync; absent → Trakt sync disabled (service runs exactly as today).
- `TRAKT_SYNC_INTERVAL_SECS` (default e.g. 900), `EPISODE_CHECK_INTERVAL_SECS` (default e.g. 3600).
- Accounts are added via **dynamic enrolment** (the page), not a static list.

---

## 6. Error handling
- **Token refresh failure / de-authorised account** → mark the account needing re-enrolment, skip its sync (don't fail the loop), surface on the enrolment page. Other users continue.
- **Trakt API errors / rate limits** → AIMD backoff + `Retry-After`; a failed `sync_trakt` tick leaves the last-known `wanted` intact (no destructive action on stale data).
- **Reconcile is idempotent** — it always diffs current wanted vs current owned, so a missed/failed tick self-heals next tick.
- **Removal safety** — only engine-owned hashes are ever deleted; a Trakt outage that empties a fetch must NOT be read as "user wants nothing" → removals act on a *successful, fresh* per-user fetch only, never on a fetch error.
- **DB** — new authoritative tables migrate forward; unreadable/incompatible DB moved aside per SP0's recovery.

---

## 7. Testing (TDD)
- `MockTrakt` seam for deterministic unit tests.
- **Table-driven reconcile tests**: each removal-lifecycle rule (finish movie; finish+ended show; un-watchlist abandon at various watch %; another user blocks removal via watchlist vs in-progress; provenance) and each acquire case (watchlist movie; tracked show back-catalogue; lapsed→re-acquire).
- **Scheduler-job tests** — each job in isolation over a mock store/AppState.
- **Episode-monitor tests** — air-date boundary logic (`chrono`), already-owned skip.
- **OAuth tests** — device-flow state machine + token refresh (mock HTTP).
- **`#[ignore]` live smoke** — device-flow against Trakt + a real watchlist pull (skips when `TRAKT_CLIENT_ID` unset).
- Existing suite stays green; `sync_account` behaviour-preservation guarded by current scan tests.

---

## 8. Decisions log (this brainstorm, 2026-06-08)
1. **Scope** — full SP2 in one spec (OAuth multi-user + enrolment page + reconciler + episode monitor + scheduler split).
2. **Wanted-set sources** — watchlist + in-progress, movies **and** shows. Not watched-history-as-acquisition, not custom lists.
3. **Whole-show** — track = acquire **all aired episodes** (full back-catalogue) + auto-acquire new.
4. **Multi-user** — one shared library; **union** acquire; **per-user-aware** removal.
5. **Removal lifecycle** — only engine-owned removed. **Trigger A (finished):** every user who has it watchlisted/in-progress has finished it (movie watched; show 100%-watched + ended). **Trigger B (abandoned):** the watchlist-provenance user un-watchlisted it (any watch %) **and** no other user wants it. Otherwise keep. Track **provenance**; another user's watchlist (even un-started) blocks removal.
6. **Availability** — "wanted = owned **and available**"; reconciler re-acquires lapsed/missing wanted content proactively.
7. **Enrolment auth** — local-network trust (no extra auth).
8. **Acquisition** — interim SP1 engine, paced; SP3 reconciliation supersedes it later.

---

## 9. Non-goals / deferred
- SP3 reconciliation/upgrade (better-release upgrades, episode→season-pack consolidation, optimistic-add+async-reconcile). The interim SP1 sync fixes (0-seeder filter, `cleanup_leaked`, `materialise` polling) remain until SP3.
- Per-user libraries; custom-list sources; full web UI (SP4).
- The temporary `--acquire` CLI is removed in SP2 (Trakt + reconciler are the acquisition triggers now).
