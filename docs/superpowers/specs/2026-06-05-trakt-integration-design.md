# Trakt integration — design

- **Date:** 2026-06-05
- **Status:** Approved for SP0 spec; SP1–SP4 captured as intent sketches
- **Author:** brainstorming session (Kiril + Claude)

## 1. Overview

Add automatic, preference-driven library population to DebridMovieMapper, driven
by Trakt. Today the service is **read-only** with respect to the debrid account:
it polls torrents already present in Real-Debrid / TorBox, identifies them via
TMDB, and serves them over WebDAV. This feature makes the service **acquire**
content: it reads what each Trakt user wants (watchlist, watched history,
in-progress shows), finds matching releases via a Stremio-addon scraper, adds
them to the debrid account at the configured quality, keeps shows topped up with
new episodes, and transparently upgrades to better releases when they appear —
without ever interrupting playback.

### Goals

- Mirror each configured Trakt user's watchlist + watched + in-progress into the
  library, including whole shows with ongoing new-episode monitoring.
- Acquire at a configurable quality (resolution ceiling + ranked codec / audio /
  HDR preferences), preferring releases already cached on the account.
- Verify audio/subtitle languages so a foreign-only dub never silently lands.
- Auto-recover from failed/stalled/bad releases by trying a different candidate.
- Auto-upgrade to better releases, deferring the swap until the file is idle.
- Provide a small web UI for Trakt enrolment, observability, match correction,
  settings, and ad-hoc "search and add".

### Non-goals

- No per-title manual release picking / pinning in the UI (the auto-pick is
  trusted).
- No reuse of Jellyfin-stored Trakt credentials (standalone device-flow OAuth
  only).
- No bound on library size beyond what the scraper/provider naturally impose
  (full mirror).

## 2. Decisions log

Every decision below was settled during brainstorming and is binding on the
phase specs.

| # | Topic | Decision |
|---|-------|----------|
| 1 | Torrent source | Stremio-addon scraper (Torrentio-compatible), **configured with the debrid key** so streams come back cache-flagged for the account. A `Scraper` trait abstracts it (future Prowlarr/Jackett possible). |
| 2 | Trakt auth | **Device-flow OAuth**, multi-user. Robust against profile-privacy changes; requires a small enrolment web surface. (Public-username + app-key was rejected: a user can silently break it by going private.) |
| 3 | Episode scope | **Entire show + monitor new episodes** for any tracked show. |
| 4 | Backfill bounds | **Everything, no bounds** — a full mirror. Pace our own outbound calls; do **not** build a download-slot manager. |
| 5 | Download concurrency | **Defer to the provider.** TorBox self-queues. Real-Debrid caps active torrents and errors past it → **add-and-react** (back off + retry on "too many active" / 429), reusing the existing `AdaptiveRateLimiter`. |
| 6 | Quality policy | **Resolution ceiling (hard) + ranked soft preferences** (HEVC / audio / HDR / group / size / seeders); always-fill; cached-first. The same score drives acquisition and upgrades. |
| 7 | Audio/subtitle | **Layered:** cheap title/metadata filter, then authoritative **file-probe** of the cached container's tracks (pure-Rust MKV/MP4 parse over a ranged CDN read — no ffmpeg; image is `scratch`). Enforce a **selectable audio-language requirement** (a specific language *or* `original`, resolved from the title's TMDB original language) and an **independent selectable subtitle requirement** (a specific language, or `none` = skip the subtitle check entirely — *not* a requirement that subtitles be absent). When a subtitle language is required, that track must be present in its own right, even when it is the same language as the required audio. A candidate failing either requirement is blacklisted → next candidate. |
| 8 | Failed/stalled | Acquisition is **managed**: keep the ranked candidate list, detect failure (magnet error / dead / no-seeders) or **stall** (status stuck, no progress past a timeout — observed via the scan poll), blacklist that release, promote the next candidate. |
| 9 | Blacklist | Persisted set of bad release hashes per title. Populated automatically (failure/stall/bad-audio) and manually (UI flag). |
| 10 | Upgrades | **Stage-then-swap:** acquire the better release in the background, swap the file's `FileLocator` only once that file is **idle**. |
| 11 | "Currently playing" signal | **Proxy read-activity** — the service already serves every byte, so a file read within the last N minutes counts as active. Provider- and client-agnostic; no Jellyfin dependency. |
| 12 | Retention | **Prune superseded-after-upgrade only.** Never prune on Trakt removal. Only ever delete **service-owned** torrents (tracked in a DB table), never manually-added ones. |
| 13 | Identification | Record `hash → tmdb_id` (authoritative) on add, so the scan loop skips filename-guessing for our adds. Match-correction is then only needed for externally-added torrents. |
| 14 | Web UI scope | Trakt enrolment (given) + activity dashboard + match correction + in-UI settings + ad-hoc TMDB search-and-add. **No** library browser / manual release pinning. |
| 15 | Architecture | **Re-centre, not rewrite.** Add a desired-state reconciler beside the existing account-mirror projection. Keep `DebridProvider`, `FileLocator`, VFS-as-projection, `redb`, the proxy, and the repair machinery. |
| 16 | Sequencing | **Minimal foundation first, then strangle.** A small behaviour-preserving pass (store + AppState + config shape), then fold the larger refactors into the phase that first needs them. |
| 17 | DB upgrade/recovery | `metadata.db` must **never fail startup** on a format/schema change (incl. a future `redb` version bump). `Store` stamps a schema version and runs forward migrations; an unreadable / incompatible / corrupt / newer-than-binary database is **moved aside** (`<db_path>.corrupt`) and recreated rather than erroring. SP0's only table (`matches`) is a **regenerable cache** so recreation is harmless. From SP1, tables holding **authoritative** state (owned / blacklist / wanted / tokens / settings) are **migrated, never silently dropped**; if migration is impossible the old file is preserved (moved aside) so startup still succeeds without irrecoverable loss. The versioning + migration framework lands in SP0. |

## 3. Architecture assessment (why a re-centring is needed)

### Sound and retained

- `DebridProvider` + `FileLocator` are a clean seam; all components already
  depend on `Arc<dyn DebridProvider>`.
- The **VFS-as-derived-projection** pattern (rebuild from the account each scan,
  `diff_trees`, swap under `RwLock`) is good and is preserved. Acquisition and
  Trakt never mutate the VFS directly — they change the *account* and the
  *desired-state store*, and the VFS keeps projecting the account.
- `redb`, the proxy read path, and the repair *machinery* (health state machine,
  30s cooldown, 3-attempt cap, file-matching by `file_path`) are solid.
- The existing test suite is broad and behaviour-focused — the safety net for a
  strangler refactor.

### The core mismatch

The whole system today is a **one-way projection**: "describe what is in the
account." The Trakt feature is the inverse — a **desired-state reconciler**
(`wanted ↔ actual → actions`). That inversion surfaces concretely:

1. **No structured episode model.** Seasons are regex-derived from *filenames at
   build time* (`SEASON_RE` in `vfs.rs`). "Monitor new episodes" needs stored
   season/episode identity from TMDB/Trakt, independent of the filename guess.
2. **`vfs.rs::build()` hardcodes the selection policy** ("largest torrent wins"),
   in a ~2,900-line file. Upgrades need "the release the upgrade layer says is
   *live* (idle-gated) wins", so selection must be *injected* into `build()`.
3. **`repair.rs` re-adds the *same* hash** (`magnet:?xt=urn:btih:{hash}`). The
   "stalled → try a *different* release" requirement makes repair a *special
   case* of a generalised "re-acquire this title from a candidate list" core.
4. **Persistence is scattered inline `redb` transactions** over one `matches`
   table (load-all / get / batch-put / remove-stale, all in `tasks.rs`). New
   tables need a repository layer. A `hash → tmdb_id` table can also retire the
   fiddly `repair_replacements` id-remapping dance.
5. **One `run_scan_loop` does everything.** Reconcile / monitor / upgrade /
   verify jobs would make it unmaintainable; it wants splitting behind a small
   scheduler.
6. **`main.rs` is a bag of `Arc`s + a single `DavHandler` server.** The web UI
   needs an `AppState` context and request routing.

### Verdict

Re-centre incrementally. Keep the primitives; add the desired-state layer, a
store, a job scheduler, and a generalised re-acquire core — phased so no single
big-bang refactor is required and every phase after SP0 ships user-facing value.

## 4. Roadmap

| Phase | Scope | Refactor it carries | Depends on |
|-------|-------|---------------------|------------|
| **SP0 — Minimal foundation** | `store` repository layer (migrate `matches`) · `AppState` context · layered `Config` shape. Behaviour-preserving. | — | — |
| **SP1 — Acquisition engine** | scraper → score → pick → add → tag-owned → record `hash→tmdb_id` → file-probe verify → fallback/blacklist | repair → shared "re-acquire from candidate list" core | SP0 |
| **SP2 — Trakt sync** | device-flow OAuth (multi-user) · wanted-set (watchlist/watched/in-progress → full shows + new-episode monitor) · paced backfill · minimal enrolment page | scan-loop → scheduler + cooperating jobs; structured episode model | SP0, SP1 |
| **SP3 — Upgrade engine** | re-score owned titles · stage-then-swap idle-gated on read-activity · prune superseded | invert `vfs::build()` selection (injected, not hardcoded) | SP1, SP2 |
| **SP4 — Web UI** | dashboard · match correction · in-UI settings (DB-backed config) · ad-hoc TMDB search-and-add | `AppState` + request router | SP1–SP3 |

Each phase gets its own spec → plan → implementation cycle. This document
details SP0 and sketches SP1–SP4.

## 5. SP0 — Minimal foundation (the implementable spec)

**Objective:** introduce the shared scaffolding the feature needs while changing
**no observable behaviour**. The existing test suite passing unchanged is the
proof of behaviour-preservation.

### 5.1 `store.rs` — persistence/repository layer

- New `Store` type wrapping `Arc<redb::Database>`, owning **all** table-definition
  constants and exposing typed, `spawn_blocking`-wrapped async accessors.
- **`Store::open(path)` never fails on a bad database.** It stamps a schema
  version (in a `meta` table), runs forward migrations, and on an unreadable /
  incompatible / corrupt / newer-than-binary file **moves it aside**
  (`<db_path>.corrupt`) and recreates a fresh one — replacing today's
  `Database::create(&db_path)?` + inline table-creation, which crash-loops on a
  format change. SP0 implements the framework with **zero migrations** (current
  unversioned DBs are simply stamped at v1, keeping their `matches` rows); SP1+
  add migration steps and the migrate-not-drop policy for authoritative tables.
- Migrate the four existing `matches` operations out of `tasks.rs` into `Store`:
  - `load_all_matches() -> HashMap<String, (TorrentInfo, MediaMetadata)>`
  - `get_match(id) -> Option<(TorrentInfo, MediaMetadata)>`
  - `put_matches(batch: &[(String, Vec<u8>)])` (or a typed equivalent)
  - `remove_matches(ids: &[String])`
  - plus the repair-replacement remap currently done inline (remove old key +
    insert new key in one txn).
- **On-disk encoding stays byte-identical.** Keep `MATCHES_TABLE` as
  `TableDefinition<&str, &[u8]>` with the same `serde_json` value encoding, so
  deployed `metadata.db` files load unchanged. No data migration.
- The table-definition constants for future phases (`owned_hashes`,
  `authoritative_ids`, `blacklist`, `wanted`, `settings`) **may** be declared
  here to establish the pattern, but are only wired up by the phase that needs
  them. SP0 wires only `matches`.
- `tasks.rs` calls `store.*` instead of inline transactions; logic and ordering
  unchanged (batch flush at the same checkpoints, same stale-pruning semantics).

### 5.2 `AppState` context

- New struct holding the shared handles currently threaded individually:
  `provider: Arc<dyn DebridProvider>`, `tmdb_client`, `vfs`, `store`,
  `repair_manager`, `config`, `jellyfin_client: Option<...>`, `http_client`.
- Built once in `main.rs`; cloned (all `Arc`s) into the scan task and `dav_fs`.
- `ScanConfig` and `DebridFileSystem::new` are simplified to take `AppState`
  (plus the per-call extras that are not shared, e.g. the scan interval can live
  in `Config`). Keep it minimal — consolidate shared handles, don't force every
  value through it.

### 5.3 Layered `Config`

- New `Config` struct parsed once from env at startup, replacing the scattered
  `std::env::var` reads in `main.rs`. Covers: provider token selection
  (`choose_provider` stays as-is, fed by `Config`), `TMDB_API_KEY`,
  `SCAN_INTERVAL_SECS` (min 10), `DB_PATH`, `PORT`, and the three Jellyfin vars.
- Read through `AppState`; accessors shaped so a future **DB-override layer**
  (SP4's in-UI settings) can supply runtime values without touching call sites.
- SP0 implements **only the env layer**. Same variables, same defaults, same
  validation/error messages and exit behaviour as today.

### 5.4 Explicit non-goals for SP0

Deferred to the phase that first needs them: scan-loop → scheduler split (SP2),
repair → shared re-acquire core (SP1), structured episode/desired-state model
(SP2), `vfs::build()` selection inversion (SP3), web router and the new tables'
wiring (SP1/SP4).

### 5.5 Testing

- **All existing unit + integration tests must pass unchanged** (behaviour
  preservation).
- New unit tests:
  - `Store` round-trips against the in-memory `redb` backend already used in
    tests (`redb::backends::InMemoryBackend`): put → get, batch put → load_all,
    remove → absent, replacement remap.
  - `Config` parsing: env → struct, defaults applied, the same error cases
    (`choose_provider` both/neither set, missing TMDB key).
- TDD per `CLAUDE.md`: write the failing `Store`/`Config` tests first.

### 5.6 Risk

Low — mechanical extraction guarded by a strong test net. The one care point is
keeping the `redb` key/value encoding byte-identical so existing databases keep
loading. No new runtime dependencies in SP0.

## 6. SP1–SP4 — intent sketches

These are captured for context and will each be brainstormed into their own spec.

### SP1 — Acquisition engine

- **Modules:** `scraper.rs` (`Scraper` trait + `TorrentioScraper` + `MockScraper`;
  calls `{addon}/stream/{type}/{id}.json`, series id `tt123:S:E`), `release.rs`
  (parse stream name/description → `ReleaseInfo`; score: hard resolution ceiling,
  weighted soft prefs, large cached-first bonus; layer-1 language filter),
  `probe.rs` (ranged CDN GET → pure-Rust MKV/MP4 track-language parse; handle
  MP4 `moov`-at-end), `acquire.rs` (the engine: pick → add → verify → fallback;
  owns blacklist + ownership writes).
- **Flow:** scrape → score → drop blacklisted / out-of-ceiling → rank → add best
  (cached first) → `select_files` → record ownership + authoritative id → if
  cached, probe tracks (pass = confirmed; fail = blacklist + next; inconclusive =
  accept with warning) → if uncached, return `Pending` and let `observe()` finish.
- **`observe(&[Torrent])`** (called each scan tick): detect failure/stall on owned
  in-flight torrents → blacklist → re-acquire next candidate.
- **Refactor:** generalise `repair.rs` into a shared "re-acquire a title" core
  with a pluggable candidate source — same-hash for repair, next-candidate for
  acquisition. `hash → tmdb_id` authoritative ids subsume `repair_replacements`.
- **New tables:** `owned_hashes` (`hash → {tmdb_id, kind, source, added_at}`),
  `authoritative_ids` (`hash → {tmdb_id, kind, season, episode}`), `blacklist`
  (`(tmdb_id, hash) → {reason, at}`).
- **Matching & identification impact:** Torrentio is keyed by **IMDB id**
  (Stremio convention: `/stream/movie/tt….json`, `/stream/series/tt…:S:E.json`),
  returning *id → torrents* — it does not annotate torrents with TMDB ids. So for
  everything **we** acquire we already hold the authoritative id *before the
  torrent exists*; we record `hash → tmdb_id` (`authoritative_ids`) and the scan
  loop uses it directly, bypassing `identify_torrent`. This yields a **two-source**
  matching model: (a) **authoritative / id-first** for our Trakt/ad-hoc adds
  (always correct, never needs correction); (b) **heuristic / filename-first**
  (today's `identify_torrent`) for externally-added torrents (DMM/manual), fixable
  via SP4 match-correction. **ID plumbing:** Trakt items carry both `imdb` and
  `tmdb` ids; ad-hoc adds map `tmdb → imdb` via TMDB `external_ids`. We query
  Torrentio by IMDB but keep the **TMDB id as the library's authoritative
  identity** (NFO/Jellyfin consistency, matching today's `external_id: "tmdb:…"`).
  **Episode caveat:** a season pack maps authoritatively only to the *show* —
  episode-to-file resolution within the pack still parses S/E from filenames, now
  anchored to a known show and validated against TMDB's episode list (SP2's
  structured episode model), so far more reliable than today's blind guess but not
  free.
- **Language requirements model:** `AUDIO_LANGUAGE` = a language code **or**
  `original` (resolved per-title from TMDB's `original_language`).
  `SUBTITLE_LANGUAGE` = a language code **or** `none`; `none` means *skip the
  subtitle check entirely* (it does **not** require subtitles to be absent), and
  `original` is **not** a subtitle option. The audio requirement is always
  enforced; when a subtitle language is set it is enforced **independently** —
  that subtitle track must be present even when it is the same language as the
  required audio. Verification needs an ISO 639-1 ↔ 639-2 mapping, since TMDB uses
  639-1 (`en`) and Matroska track headers use 639-2 (`eng`). Layer-1 (title)
  filtering is best-effort; the file-probe is the authoritative gate.
- **Config:** `SCRAPER_ADDON_URL` (enables acquisition; pre-configured base with
  debrid key), `MAX_RESOLUTION`, `AUDIO_LANGUAGE`, `SUBTITLE_LANGUAGE`,
  `PREFER_HEVC`, `PREFER_HDR` + weights, `STALL_TIMEOUT_SECS`,
  `MAX_ACQUIRE_ATTEMPTS`. Acquisition is dormant if `SCRAPER_ADDON_URL` is unset
  → existing deployments unaffected.
- **Likely new dependency:** a pure-Rust container parser (MKV/MP4) for `probe`.
- **Testing:** table-driven `release` parser/scorer over real titles; `probe`
  against small fixture headers; `acquire` fallback/blacklist with
  `MockScraper` + `MockProvider`; an `#[ignore]` end-to-end test mirroring
  `lifecycle_test` (acquire Sintel → appears → cleanup).

### SP2 — Trakt sync

- **`trakt_client.rs`:** device-flow OAuth (request device code → poll → store +
  refresh tokens), multi-user; read watchlist + watched (episode-level) +
  in-progress.
- **Desired-state / `wanted` model:** title-centric (keyed by tmdb_id) with a
  **structured season/episode model** (from TMDB/Trakt, not filenames). Feeds the
  SP1 engine via a **paced** work-list (own-call politeness; no slot manager).
- **Episode monitor:** for tracked shows, acquire whole show + watch for newly
  aired episodes and acquire them.
- **Refactor:** split `run_scan_loop` into cooperating periodic jobs behind a
  small scheduler — `sync_account` (today's mirror), `reconcile_wanted`,
  `monitor_episodes`, `verify_acquisitions` — sharing state via `AppState` + the
  store.
- **Minimal enrolment page:** enough web surface to link/refresh Trakt accounts.
- **Config:** `TRAKT_CLIENT_ID` (+ secret), `TRAKT_USERS` (or dynamic enrolment),
  and poll cadences (e.g. `TRAKT_SYNC_INTERVAL_SECS`, `EPISODE_CHECK_INTERVAL_SECS`).
  `chrono` (already a dependency) covers air-date logic.

### SP3 — Upgrade engine

- **`upgrade.rs`:** periodically re-score owned titles against the scraper;
  when a better-scoring release appears, acquire it in the background, then
  **swap the `FileLocator` only once the file is idle** (proxy read-activity),
  and prune the superseded torrent (service-owned only).
- **Refactor:** invert `vfs::build()` selection — "which release represents this
  title" becomes an injected decision ("the live release per the upgrade layer"),
  replacing the hardcoded max-bytes pick.
- **Read-activity tracker:** record recent reads per `FileLocator`/path in the
  proxy; "active" = read within the last N minutes.

### SP4 — Web UI

- **`web/` + router:** the UI is served on its **own dedicated, access-controlled
  port** (token / LAN-bind), **separate from the unauthenticated WebDAV proxy
  port** — the proxy must never have to be network-exposed in order to reach the
  UI. (Decided 2026-06-09 while refreshing `compose.yml`: today the Trakt
  enrolment page shares the WebDAV port and is therefore host-local only; SP4
  splits them.)
- **Pages/APIs:** Trakt enrolment (full), activity dashboard (additions,
  downloading/queued, now-playing, recent upgrades, errors), match correction
  (override a wrong TMDB match → re-identify), in-UI settings (quality prefs +
  per-user/source toggles, **DB-backed**, layering over env via the SP0 `Config`
  shape), ad-hoc TMDB search-and-add (reuses the SP1 engine; the added title
  becomes a persisted manual `wanted` entry, monitored/upgraded like a Trakt one).
- **Likely new dependency:** a web framework (e.g. `axum`) for routing/templating.

## 7. Cross-cutting concerns

- **Provider neutrality:** every new capability goes through `DebridProvider`;
  TorBox and Real-Debrid must both work. `lifecycle_test` (cross-provider) is the
  end-to-end gate for acquisition.
- **Ownership safety:** any deletion (upgrade pruning, future cleanup) consults
  `owned_hashes` and never touches torrents the service did not add.
- **Backwards compatibility:** with `SCRAPER_ADDON_URL` and `TRAKT_CLIENT_ID`
  unset, the service behaves exactly as today (pure mirror). Each new env var has
  a safe default or disables its feature when absent.
- **Documentation:** per `CLAUDE.md`, every phase updates `CLAUDE.md` and
  `README.md` for new env vars, modules, and behaviour.
- **New dependencies** are introduced only in the phase that needs them (container
  parser in SP1; web framework in SP4); SP0 adds none.

## 8. Testing strategy

Integration tests are the final gate (per `CLAUDE.md`) and must grow with this
feature. Three layers.

### 8.1 Principles

- **Trait seams for determinism.** Add `Scraper` and `Trakt` traits with `Mock`
  implementations (mirroring the existing `MockProvider`). This lets the
  cross-component flows — acquire/fallback, reconcile, episode-monitor, upgrade —
  be tested deterministically in CI with **no API tokens and no network**.
- **Fixtures over live calls for parsing.** Commit real captured samples —
  Torrentio stream JSON, Trakt watchlist/watched/progress JSON, and small
  MKV/MP4 container headers — and table-test the parsers (`release`, `probe`,
  `trakt_client`) against them. Pins behaviour and surfaces third-party format
  drift (re-capture periodically).
- **Live smoke tests are `#[ignore]` and token-gated**, use Creative-Commons
  content (Sintel / Big Buck Bunny) for anything touching the live debrid account,
  and **clean up after themselves** — extending the cross-provider `lifecycle_test`
  pattern. They run sequentially (shared `redb` lock + API rate limits).
- **Behaviour preservation (SP0).** The entire existing suite must pass unchanged;
  that is the proof SP0 changed no behaviour.

### 8.2 Deterministic tests (CI, no tokens)

| Phase | Test | Asserts |
|-------|------|---------|
| SP0 | `store_integration_test` | Full table round-trips on real `redb`; a DB written in the **old** inline encoding loads identically through `Store` (backward compatibility). |
| SP0 | `store_recovery_test` | `Store::open` never panics/fails: a corrupt/garbage file is moved aside and recreated; an unversioned existing DB keeps its `matches` data and is stamped at the current version; a newer-than-binary version is recovered. |
| SP0 | `config_test` | env → `Config` (defaults; both/neither provider token; missing TMDB key). |
| SP1 | `acquire_integration_test` | `MockScraper` + `MockProvider`: best-candidate pick (cached-first, within ceiling); ownership + authoritative-id recorded; probe-fail / stall / dead → blacklist + promote next; idempotent re-acquire. |
| SP1 | `probe_fixture_test` | Track languages parsed from committed MKV/MP4 header fixtures; MP4 `moov`-at-end handled; inconclusive parse → accept-with-warning, not blacklist; audio (`original`/lang) and independent subtitle (lang / `none`) rules enforced. |
| SP1 | `release_parse_test` | Resolution/codec/HDR/audio/group/size/seeders/cache parsed and scored from captured Torrentio titles; ceiling excludes; layer-1 language filter. |
| SP1 | `repair_regression` | After generalising repair into the shared re-acquire core, existing repair behaviour is preserved — same-hash re-add, 30s cooldown, 3-attempt cap, broken-torrent hiding; repair is the same-hash specialisation of the shared core (existing `repair_integration_test` + unit tests pass unchanged). |
| SP1 | `backoff_429_test` | Simulated 429 / RD "too many active torrents" on add → add-and-react back-off + retry (no failed acquire); `AdaptiveRateLimiter` interval-doubling and `Retry-After` respected; backfill paces under sustained throttling. |
| SP2 | `trakt_parse_test` | Watchlist/watched/in-progress JSON fixtures → structured items carrying both `imdb` + `tmdb` ids; episode-level watched → derived in-progress. |
| SP2 | `reconcile_integration_test` | Wanted-set vs mock account → correct gap-fill acquire calls; already-owned skipped; idempotent (no duplicate adds). |
| SP2 | `episode_monitor_test` | Paused-clock (`tokio start_paused`): a newly aired episode triggers exactly one acquire. |
| SP2 | `season_pack_mapping_test` | A season-pack torrent → each episode file resolved to the correct S/E, anchored to the known show (authoritative id) and validated against TMDB's episode list; mislabelled/extra files rejected. |
| SP2 | `multiuser_dedup_test` | Two users want the same title → exactly one acquire / one torrent; the wanted entry attributes both requesters; one user dropping the want leaves the title while the other still wants it. |
| SP3 | `upgrade_integration_test` | Owned title at quality A, scraper offers better B → B staged; swap **deferred** while a simulated recent read marks the file active; swap occurs once idle; superseded torrent pruned (owned-only). |
| SP4 | `web_integration_test` | In-process router + mock services: enrolment endpoints; dashboard JSON; match-correction POST re-identifies; settings persist to DB and override env; ad-hoc add enqueues a wanted entry; unauthenticated request rejected. |

### 8.3 Live smoke tests (`#[ignore]`, token-gated)

- `scraper_live_test` (needs `SCRAPER_ADDON_URL`): query Torrentio for a known
  IMDB id → non-empty, parseable streams with cache flags.
- `trakt_live_test` (needs `TRAKT_CLIENT_ID` + an injected `TRAKT_TEST_REFRESH_TOKEN`,
  since device flow can't be automated): fetch a known account's lists → structured
  items.
- `probe_live_test` (needs a provider token): ranged-probe a real cached file's tracks.
- **`lifecycle_test` extension** (cross-provider, RD + TorBox): acquire Sintel
  **by IMDB id via Torrentio** → appears in the VFS → delete → disappears, only ever
  touching the service-owned torrent. The headline end-to-end; skips per provider
  when its token is unset.

### 8.4 Pre-commit gate

- All **deterministic** tests (8.2) join the always-run `cargo test` gate.
- The **`lifecycle_test`** extension joins the existing `-- --ignored` gate run
  (stable and self-cleaning).
- Other **live** smokes (8.3) stay **supplementary** (`#[ignore]`, not in the gate):
  third-party availability/rate-limits make them unsuitable as a hard commit gate;
  run them on demand and in periodic CI.

## 9. Open questions (to resolve in the relevant phase spec, not now)

- Exact pure-Rust MKV/MP4 parser choice and the ranged-fetch strategy for
  MP4 `moov`-at-end (SP1).
- Current Real-Debrid active-torrent limit — informational only; the add-and-react
  design does not depend on the number, but worth pinning down for backfill pacing
  (SP1/SP2). Can be web-searched when SP1 is specced.
- Web UI access-control model: token vs LAN-bind vs both (SP4). **Resolved
  2026-06-09:** the UI gets its **own authenticated port**, distinct from the
  unauthenticated WebDAV proxy port (see the SP4 sketch above).
- Whether `authoritative_ids` fully replaces `repair_replacements` or runs
  alongside it during SP1 (decide when generalising repair).
