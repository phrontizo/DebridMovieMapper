# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Project Does

DebridMovieMapper is a Rust async service that bridges a debrid service — **Real-Debrid or TorBox** — with media servers like Jellyfin and Plex. It fetches torrents from the configured debrid account, identifies them via TMDB metadata, and exposes a WebDAV endpoint serving proxied media files (the actual `.mkv`/`.mp4` bytes are fetched from the provider's CDN on demand). Exactly one provider is active per deployment, selected at startup by which token is set (`RD_API_TOKEN` or `TORBOX_API_KEY`).

Optionally, the library can be **driven by users' Trakt accounts** (SP2): when Trakt sync is configured, each enrolled user's watchlist and in-progress titles auto-acquire content and a reconciler lifecycle-removes it once everyone who wanted it has finished (or abandoned) it — alongside the existing manual/account-mirror behaviour, which is unchanged when Trakt is not configured.

## Commands

```bash
# Build
cargo build --release

# Run locally (requires env vars)
RD_API_TOKEN=<token> TMDB_API_KEY=<key> cargo run

# Run unit tests (no API tokens needed)
cargo test

# Run a single test
cargo test <test_name>

# Run integration tests (requires RD_API_TOKEN + TMDB_API_KEY in .env)
cargo test -- --ignored

# Run integration tests with subset (fast feedback)
INTEGRATION_TEST_LIMIT=10 cargo test -- --ignored

# Acquisition is triggered by Trakt sync + the reconciler (the temporary --acquire CLI was
# removed in SP2). Configure TRAKT_CLIENT_ID/TRAKT_CLIENT_SECRET and enrol accounts at
# /trakt/accounts to drive acquisition; otherwise the service runs account-mirror + repair only.

# Docker (single platform, local)
docker build -t debridmoviemapper .

# Release: push a semver tag to trigger GitHub Actions build + push to ghcr.io
git tag v1.0.0 && git push origin v1.0.0

# Start full stack (WebDAV + rclone + Jellyfin)
mkdir -p rclone && chown 65534:65534 rclone
docker compose up -d
```

**Required environment variables:**
- A debrid provider token — exactly one of:
  - `RD_API_TOKEN` — Real-Debrid API token
  - `TORBOX_API_KEY` — TorBox API token

  Set one or the other, not both. Setting **both** is a startup error; setting **neither** is a startup error.
- `TMDB_API_KEY` — TMDB API key

**Optional:**
- `SCAN_INTERVAL_SECS` (default: 60, minimum: 10) — how often to poll Real-Debrid
- `DB_PATH` (default: `metadata.db`) — path to the redb database file
- `PORT` (default: 8080) — WebDAV server listen port
- `JELLYFIN_URL` — Jellyfin server URL (e.g. `http://jellyfin:8096`)
- `JELLYFIN_API_KEY` — Jellyfin API key
- `JELLYFIN_RCLONE_MOUNT_PATH` — rclone mount path as seen by Jellyfin (e.g. `/media`)

**Optional — Trakt sync (SP2):**
- `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET` — both required to ENABLE Trakt sync (from a Trakt API app). If either is absent, Trakt sync is disabled and the service runs exactly as before (account-mirror + on-demand repair only).
- `TRAKT_SYNC_INTERVAL_SECS` (default: 900, minimum: 60) — Trakt sync + reconcile cadence.
- `TRAKT_EPISODE_CHECK_INTERVAL_SECS` (default: 3600, minimum: 300) — episode-monitor cadence. (Note this exact env var name — it carries the `TRAKT_` prefix for consistency.)

  Accounts are linked via the local-network enrolment page (`/trakt/accounts` on the WebDAV listener), not a static list.

**Optional — Upgrade engine (SP3):**
- `UPGRADE_INTERVAL_SECS` (default: 86400 = daily; `0` disables; minimum 600 when non-zero) — how often to run the quality-upgrade + full-season consolidation pass. On by default; set to `0` to disable entirely.
- `UPGRADE_BUDGET_PER_TICK` (default: 20, minimum: 1) — maximum number of owned titles re-scored per upgrade tick (round-robin across the library).
- `UPGRADE_IDLE_SECS` (default: 300, minimum: 30) — seconds of proxy read inactivity before a slot is considered idle enough to swap/prune.
- `UPGRADE_STAGE_MAX_SECS` (default: 604800, minimum: 3600) — reserved; maximum age of a staged upgrade before it is abandoned (informational for now).
- `ACQUIRE_DEAD_TIMEOUT_SECS` (default: 600, minimum: 120) — seconds an optimistically-added `Pending` torrent may remain unresolved before `observe` reaps it as dead, blacklists the hash, and re-scrapes.

## Architecture

The project is structured as both a binary (`main.rs`) and a library (`mapper.rs` as lib root).

**Background tasks (the scheduler model, SP2):** `main.rs` spawns `scheduler::run`, which spins up cooperating independent periodic jobs over the shared `AppState` (each driven by the `scheduler::periodic` primitive — run immediately, then every interval, until shutdown):
- **Scan loop** = `run_scan_loop` (every `SCAN_INTERVAL_SECS`, behaviour-preserving): `sync_account` (poll the active debrid provider → identify torrents via TMDB → update the in-memory VFS) **plus** `verify_acquisitions` (= `engine.observe`), sharing one `get_torrents` per tick. Implemented in `tasks.rs`.
- **Trakt cycle** = `sync_trakt` then `reconcile_wanted` (sequentially, so the reconciler sees the just-synced wanted set), every `TRAKT_SYNC_INTERVAL_SECS`. Spawned **only** when Trakt is configured.
- **Episode monitor** = `monitor_episodes` (every `TRAKT_EPISODE_CHECK_INTERVAL_SECS`). Spawned only when Trakt is configured.
- **Upgrade job** = `run_upgrade_once` (every `UPGRADE_INTERVAL_SECS`, default 86400s/daily; gated on `UPGRADE_INTERVAL_SECS > 0`): re-scores owned titles, stages meaningfully-better CACHED releases, and performs an idle-gated swap of the `selection` + prune of the superseded torrent; also consolidates scattered per-episode torrents into full-season cached packs (no quality regression). Disable by setting `UPGRADE_INTERVAL_SECS=0`. Implemented in `upgrade.rs`.

The Trakt cycle + episode monitor are gated on `scheduler::trakt_jobs_enabled` (a Trakt client **and** `config.trakt` both present); otherwise only the scan loop runs and the service behaves exactly as before Trakt. The local-network Trakt enrolment routes (`/trakt/accounts`) are served on the existing WebDAV listener.

**On-demand (synchronous, during WebDAV file reads):**
- **Repair:** Triggered when a media file is read and `provider.resolve_url` returns `AppError::Unavailable` (RD maps a 503 on unrestrict to it). For cached torrents, repair completes inline (~1-2s delay); for non-cached torrents, the file fails and a new torrent is left to download.

**Module responsibilities:**

| File | Purpose |
|------|---------|
| `main.rs` | Initializes shared state, spawns scan task, starts WebDAV server on port 8080; `--healthcheck` mode for Docker |
| `config.rs` | `Config` — all startup env parsing/validation (`from_env`/`from_parts`); shaped for a future DB-override layer. Also parses the optional `TraktConfig` (`Some` only when both `TRAKT_CLIENT_ID` + `TRAKT_CLIENT_SECRET` are present; carries the two Trakt interval knobs) |
| `app_state.rs` | `AppState` — `Clone` bundle of shared handles (provider, tmdb, vfs, store, repair, config, jellyfin, http, scraper, engine, trakt_client) carried by the scheduler's jobs |
| `tasks.rs` | `run_scan_loop` — polls the provider, identifies new torrents, updates VFS (+ `engine.observe`). Also hosts the SP2 Trakt jobs: `sync_trakt` (materialise each user's `wanted` set), `reconcile_wanted` (movie acquire/re-acquire + show Trigger-B removal), and `monitor_episodes` (show episode acquisition + Trigger-A finish removal) |
| `provider.rs` | `DebridProvider` trait abstracting the debrid backend; `FileLocator` (the provider-neutral handle for a media file) and its `resolve_url`/`invalidate` primitives; startup provider selection (`choose_provider`); test-only `MockProvider` |
| `rd_client.rs` | Real-Debrid implementation of `DebridProvider`; shared adaptive rate limiter, 1-hour unrestrict cache |
| `torbox_client.rs` | TorBox implementation of `DebridProvider` (`mylist`/`requestdl`/`createtorrent`/`controltorrent`); resolves by `(torrent_id, file_id)`, ~3h URL cache |
| `ratelimit.rs` | Shared `AdaptiveRateLimiter` (token bucket) used by both provider clients |
| `identification.rs` | Filename cleaning, camelCase splitting, TMDB scoring to identify movies/shows |
| `vfs.rs` | In-memory virtual filesystem: creates `Movies/`+`Shows/` hierarchy with media files and NFO metadata. `build` consults the injected `SelectionMap` (slot → `{hash, file_path}`) to pick the representative file per title/episode; falls back to largest-bytes for slots with no managed selection, so external/un-managed/pre-SP3 torrents behave exactly as before. |
| `dav_fs.rs` | Maps VFS to WebDAV; on read resolves a `FileLocator` to a CDN URL via `provider.resolve_url` (cached 1h), attempts synchronous instant repair on `AppError::Unavailable` |
| `repair.rs` | Torrent repair state machine (Healthy→Broken→Repairing→Failed); provider-neutral instant repair — `try_instant_repair(&FileLocator)` re-adds by hash and, if cached, returns a fresh `FileLocator` for the same file (matched by `file_path`) for `dav_fs` to swap in (30s cooldown, max 3 attempts) |
| `tmdb_client.rs` | TMDB search for movies and TV shows; also exposes `show_status` (Ended/Returning/Other), per-season episode air dates, and season enumeration — feeding the SP2 episode monitor + finish lifecycle |
| `store.rs` | `Store` — owns all redb table definitions, schema **v4** + migration hook, `open()` with auto-recovery (moves an unreadable/incompatible DB aside and recreates it), and typed async accessors for: `matches` cache, `owned_hashes`, `authoritative_ids`, `blacklist`, `trakt_tokens` + `wanted` (SP2), and (SP3) the `selection` table (slot → `{hash, file_path}`) and `upgrade_checks` cursor. `OwnedRecord` carries per-user `Provenance` and (SP3) `provides: Vec<(season, episode)>` — the episodes this hash supplies — and `quality: Option<QualitySummary>` — compact release snapshot for upgrade comparison. |
| `scraper.rs` | `Scraper` trait + `TorrentioScraper` — calls a Stremio-compatible addon (default: Torrentio, auto-templated from the provider token) and parses stream objects into `RawCandidate` values |
| `release.rs` | `RawCandidate` → `ReleaseInfo` parser (resolution, codec, HDR, size, seeders, cached flag, source tier); `score`/`rank` with quality prefs; hard-filters the resolution ceiling, cam/telesync/screener sources, **and uncached zero-seeder releases** (quality/availability floor), then ranks by source tier (REMUX>BluRay>WEB>HDTV) and bitrate |
| `probe.rs` | Hand-rolled MKV/MP4 track reader; `probe_tracks` fetches the first 4 MB of a CDN URL and extracts audio/subtitle language codes; `verify` checks language requirements; corrupt/mismatch → blacklist, unknown → accept; a truncated/under-fetched ranged read → `Transient` (defer + retry, **not** blacklisted) |
| `reacquire.rs` | `materialise` — shared primitive (also used by `repair.rs`) that re-adds a torrent by hash, **polls** for the file list (retrying every `settle` up to a caller-set `max_wait`, so slow-to-resolve uncached metadata isn't given up on after one check; acquisition waits ~15s, repair keeps single-poll), and selects the target files; shared between the acquisition engine and the instant-repair path |
| `read_activity.rs` | In-memory proxy read-activity tracker (SP3). `ReadActivity::touch(path)` is stamped by `dav_fs` on every proxy byte read; the upgrade engine calls `all_idle(window)` — a library-wide check (if ANY path was read within the window the upgrade tick is deferred) — before swapping/pruning. `is_idle(path, window)` also exists but is not what gates swaps. Best-effort in-memory only: after a restart everything is idle. |
| `acquire.rs` | `AcquisitionEngine` — **optimistic `acquire`** (SP3): scrape → rank → add the best non-blacklisted candidate → best-effort `select_files` for an already-cached add → record `Pending` → return immediately (no synchronous validation/probe; the verdict is deferred to `observe`). **`observe`** is the resolver (every scan tick): once files resolve, runs the deferred pack-guard → strict title-validation → probe; on pass — marks `Verified`, records `provides` (which episodes the hash supplies) + `quality`, writes `selection` entries; on fail — blacklists hash → re-scrapes → next candidate. Reaps genuinely-dead torrents after `ACQUIRE_DEAD_TIMEOUT_SECS` (default 600s). Stall detection (`STALL_TIMEOUT_SECS`) and probe-retry (`MAX_VERIFY_ATTEMPTS`) are preserved. |
| `error.rs` | Unified error type (`AppError`) using `thiserror` |
| `jellyfin_client.rs` | Optional Jellyfin notification client — notifies Jellyfin of changed paths via `POST /Library/Media/Updated` |
| `trakt_client.rs` | `TraktClient` trait — device-flow OAuth (`device_code`/`poll_token`/`refresh`), reads (`watchlist`/`in_progress`/`watched`), and `me()` for the user slug — plus `TraktClientImpl` (shared AIMD rate limiter, Trakt headers) and test-only `MockTrakt` |
| `wanted.rs` | PURE reconcile-core (zero I/O): `reconcile_title`/`reconcile` + lifecycle predicates (`wants`, `user_finished`, Trigger A `trigger_a_finished`, Trigger B `trigger_b_abandoned`, `should_remove`); types `TitleView`/`Owned`/`Action` |
| `scheduler.rs` | `periodic` primitive + `run` that spawns the cooperating periodic jobs over `AppState`; the Trakt cycle + episode monitor are gated on `trakt_jobs_enabled` |
| `enrolment.rs` | Local-network Trakt device-flow enrolment page (`/trakt/accounts`; enrol/refresh/remove) served on the WebDAV listener; `poll_to_completion` stores tokens keyed by Trakt user slug |
| `upgrade.rs` | Daily quality-upgrade + full-season consolidation engine (SP3). `run_upgrade_once` processes owned titles in a round-robin budget (`UPGRADE_BUDGET_PER_TICK`): for movies — scrapes, finds a meaningfully-better CACHED release (`is_meaningful_upgrade`: uncached→cached, higher source tier, or higher resolution), stages it, and idle-gated swaps the `selection` + prunes the superseded torrent; for shows — finds a CACHED full-season pack covering all aired episodes at same-or-better quality (`consolidation_target`), stages it, and on idle-gated success repoints all episode `selection` slots and prunes scattered episode torrents. |
| `mapper.rs` | Library root — module declarations |

**Data flow for playback:**
1. Jellyfin/player opens a media file via WebDAV
2. `dav_fs.rs` resolves the file's `FileLocator` to a CDN URL via `provider.resolve_url(&FileLocator)` (cached). Real-Debrid resolves via the locator's restricted `link` (1h cache); TorBox resolves by `(torrent_id, file_id)` via `requestdl` (no per-file link; ~3h cache).
3. `ProxiedMediaFile` fetches bytes from the CDN URL with a 2MB read-ahead buffer and serves them to the player

**Persistence:** Embedded `redb` database (`metadata.db`) caches TMDB identifications. The file is created automatically on first run. All `redb` access goes through `store.rs` (the `Store` type); modules never open transactions inline.

## Key Design Decisions

- **Provider abstraction:** All components depend on `Arc<dyn DebridProvider>` (defined in `provider.rs`) rather than a concrete client. `RealDebridClient` and `TorBoxClient` are the two implementations; exactly one provider is active per deployment, chosen at startup by `choose_provider` based on which token (`RD_API_TOKEN` or `TORBOX_API_KEY`) is set.
- **TorBox specifics:** TorBox lists torrents (with files inline) via `mylist`, resolves a file by `(torrent_id, file_id)` via `requestdl` (so `FileLocator.link` is `None`), re-adds by hash via `createtorrent`, deletes via `controltorrent`, and treats `select_files` as a no-op (TorBox auto-selects). A finished download is normalised to `status = "downloaded"` even when its cache has lapsed (TorBox keeps such entries listed as Inactive), so owned-but-uncached films still appear in the library and re-acquire on playback. The `mylist` response is parsed defensively: per-torrent fields TorBox sends loosely — `size: -1` (metadata not yet resolved) and `files: null` — are tolerated (signed sizes clamped to 0, null fields defaulted) so one malformed entry cannot fail the whole decode and hide the entire library.
- **Provider-neutral file resolution:** The VFS stores a `FileLocator { hash, torrent_id, file_id, file_path, link }` per media file rather than a raw RD link. `resolve_url(&FileLocator) -> Result<String, AppError>` is the single resolution primitive (and `invalidate(&FileLocator)` drops a cached resolution); RD resolves via the locator's restricted `link` (keeping its old unrestrict/cache logic as private inherent methods), while `(torrent_id, file_id)` is used by providers without per-file links (TorBox). `AppError::Unavailable` is the provider-neutral "bytes not currently available → re-acquire/repair" signal — RD maps a 503 on unrestrict to it, and `dav_fs` drives instant repair off `Unavailable` rather than inspecting an HTTP status. Real-Debrid behaviour is unchanged.
- **Static linking / scratch Docker image:** The Dockerfile builds with musl targets (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) producing a minimal final image on `scratch`.
- **Adaptive rate limiting:** An `AdaptiveRateLimiter` (token bucket, capacity 1, in `ratelimit.rs`) is shared across a provider's API calls — both `RealDebridClient` and `TorBoxClient` use it. Baseline: 10 req/s (100ms interval). On 429: interval doubles (max 30s under sustained throttling) and `Retry-After` is respected. On success: interval **halves** toward baseline (multiplicative recovery, min 100ms) so a bulk throttle burst doesn't leave later requests crawling at the raised ceiling. **TorBox additionally paces proactively:** `send_data`/`send_ok` read `x-ratelimit-remaining`/`-reset` from every response and call `observe_rate_limit`, holding the next request until the window resets when it is nearly spent — so bulk acquisition stays under TorBox's ~60/window `createtorrent` limit instead of storming into 429s. Real-Debrid sends no rate-limit headers (confirmed live), so it stays on the reactive AIMD path. RD signals throttle as a bare `429` (`error_code 34`, no `Retry-After`); separately, 503 (`error_code 19`, `hoster_unavailable`) on `unrestrict/link` is a terminal status (no retry) signalling a genuinely unavailable torrent → on-demand repair.
- **Per-client retry helpers:** `rd_client.rs` has one `fetch_with_retry(make_request, terminal_statuses)` — callers pass the status codes that should abort without retrying (e.g. 404 for `get_torrent_info`, 503 for `unrestrict_link`). `torbox_client.rs` has `send_data`/`send_ok`, which rate-limit, retry on 429/transient errors, and unwrap TorBox's `{success, detail, data}` envelope.
- **Identification scoring:** `identification.rs` cleans filenames extensively before TMDB lookup. Shows vs. movies are detected by file structure (presence of multiple video files with episode patterns).
- **Synchronous instant repair (provider-neutral):** When a media file is read, `dav_fs.rs` resolves the locator via `provider.resolve_url` (cached, so free when healthy). On `AppError::Unavailable`, it calls `repair_manager.try_instant_repair(&FileLocator) -> Result<FileLocator, _>` synchronously — re-adds the torrent by hash, selects the same files, and checks if the replacement is already cached. Success is decided by the new torrent reaching `status == "downloaded"` **plus** a `file_path` match for the same file (there is no `InstantRepairResult` type, and no RD-specific link-count check — path matching is inherently safe, and the replacement file is located by matching `file_path` rather than the old positional index into RD's `links` array). On success it returns a fresh `FileLocator` for that file — pairing the per-file restricted `link` by position among selected files for RD, or leaving `link: None` for providers that resolve by `(torrent_id, file_id)` (TorBox). `dav_fs` installs the returned locator (invalidating the old resolution) and re-resolves inline, so playback continues after a brief delay. If not cached, the file fails and the new torrent is left to download, the old torrent is marked Broken, and the replacement is recorded (scan loop picks it up). The health state machine (30s cooldown, max 3 attempts) is unchanged.
- **Repair hides broken torrents:** Non-cached torrents are marked Broken and hidden from WebDAV until the scan loop picks up the replacement torrent.
- **SP1 Acquisition Engine:** When a title is requested (by the SP2 Trakt reconciler — `reconcile_wanted`/`monitor_episodes` — which replaced the temporary `--acquire` CLI), `AcquisitionEngine` runs: (1) **Scrape** — `TorrentioScraper` calls a Stremio-compatible addon (default: Torrentio, URL auto-templated from `<provider>=<token>`; override with `SCRAPER_ADDON_URL`). (2) **Score/rank** — `release::parse` extracts resolution, codec, HDR, size, seeders, cached flag, and source tier; `release::rank` hard-filters the resolution ceiling (`MAX_RESOLUTION`), cam/telesync/screener sources, **and uncached zero-seeder (undownloadable) releases** (so the engine never burns acquire attempts on dead torrents or leaves "checking" magnets behind), then sorts by a weighted score (cached > source tier [REMUX>BluRay>WEB>HDTV] > resolution > HEVC > verifiable container > bitrate > seeders). (3) **Materialise** — shared with `repair.rs` via `reacquire::materialise`; re-adds the torrent by hash, waits for the file list, and selects the target file; **a movie request whose materialised torrent has more than one feature-sized video is rejected as a multi-movie pack** (providers like TorBox auto-select all files). A candidate whose materialise *fails* (provider "already queued", or an add whose file list never resolves) has any leaked torrent for its hash deleted before the next candidate is tried, so a rejected/failed candidate never lingers as a dead "checking" torrent. (4) **STRICT title validation** — `TmdbTitleValidator` runs `identify_name` on the selected file's name and verifies the result resolves to the requested TMDB id; mismatches are blacklisted and the next candidate is tried. (5) **File-probe** — `HttpProber` fetches the first 4 MB of the CDN URL and uses the hand-rolled `probe::probe_tracks` (MKV/MP4) to extract audio/subtitle language codes; corrupt probes are blacklisted, wrong-language probes are blacklisted, unknown formats are accepted. (6) **Outcome** — `Acquired` (cached + verified), `Pending` (downloading or probe deferred), `NoAcceptableRelease`, or `TemporarilyUnavailable`. `observe` monitors Pending torrents each scan tick: stall detection (no progress for `STALL_TIMEOUT_SECS`), deferred probe retry (up to `MAX_VERIFY_ATTEMPTS`), and fail-and-reacquire. The `authoritative_ids` table overrides filename identification for engine-owned torrents; the `blacklist` table prevents re-adding rejected hashes.
- **Trakt-driven desired state (SP2):** A persisted `wanted` set (per user, per tmdb_id: which sources want it — watchlist / in-progress — plus watched-state and, for shows, TMDB status) is materialised from each enrolled user's Trakt account by `sync_trakt`. A PURE reconcile-core (`wanted.rs`) diffs wanted-vs-owned-and-available and emits acquire/remove `Action`s, executed by `reconcile_wanted` (movies + show Trigger-B removal) and `monitor_episodes` (show episode acquisition + Trigger-A finish removal, via TMDB air dates). There is **one shared household library**: acquisition is the union across users; removal is per-user-aware.
- **Removal lifecycle:** Only engine-owned torrents are ever removed (manual adds are never auto-removed — `Provenance::has_manual_entry` guards). **Trigger A (finished):** every user who watchlisted/in-progresses the title has finished it (movie watched; show = 100% of *aired* episodes watched AND its TMDB status is `Ended` (ended or cancelled — `ShowStatus::Ended`; a returning/in-production/planned show never qualifies even if fully watched)). **Trigger B (abandoned):** a watchlist-provenance user un-watchlisted it AND no user currently wants it (via either source). Otherwise the title is kept. Provenance is **sticky** — recorded at acquire time (one entry per wanting user+source) and preserved across re-acquire, including any `Manual` entry.
- **Availability re-acquire:** A wanted title that is owned but no longer present in the provider listing is treated as lapsed (cache expired / lost) → proactively re-acquired (movies and per-episode for shows).
- **Multi-user enrolment:** Accounts are linked via device-flow OAuth on the local-network enrolment page (`/trakt/accounts`, served on the WebDAV listener — no auth, trusted-LAN model). Tokens are keyed by Trakt **slug**. A token-refresh or read failure marks the account `needs_reenrolment` (surfaced on the page) and leaves its prior `wanted` set intact — the reconciler never acts on a failed fetch.
- **`--acquire` CLI removed:** The temporary SP1 verification CLI was deleted in SP2; Trakt sync + the reconciler are the acquisition triggers now. A `scheduler.rs` source-guard test (`acquire_cli_is_removed`) fails if the flag is reintroduced into `main.rs`.
- **Database is self-healing:** `Store::open` stamps a schema version and runs forward migrations. An unreadable / incompatible / corrupt / newer-than-binary `metadata.db` (e.g. after a `redb` format change) is **moved aside** to `<db_path>.corrupt` and recreated rather than crashing the service. The `matches` table is a regenerable cache, so this is lossless in practice; authoritative tables added in later phases are migrated, never silently dropped.
- **SP3 — Optimistic-add + asynchronous reconcile (always-on robustness + daily quality):** `acquire` now **adds the best candidate optimistically** and records it `Pending` — no synchronous 15s poll, pack-guard, title-validation, or probe. `observe` (every scan tick, no scraping) is the resolver: once files appear, the deferred gates run; on pass it marks `Verified`, records `provides` (which `(season, episode)` pairs the hash supplies) and a `quality` summary, and writes `selection` entries; on fail it blacklists the hash and re-scrapes for the next-best candidate. Genuinely-dead torrents (provider error status, or no resolution after `ACQUIRE_DEAD_TIMEOUT_SECS`) are reaped and re-scraped. **Selection inversion:** "which release represents this title/episode" is a persisted `selection` table (slot → `{hash, file_path}`) that `vfs::build` consults on every scan; a largest-bytes fallback preserves pre-SP3 behaviour for un-managed torrents. **`provides` kills season-pack churn:** the union of `provides` across a show's owned hashes is the owned-episode set — a season pack reports all its episodes as owned so `monitor_episodes` never re-acquires them. **Daily upgrade engine** (`upgrade.rs`, on by default, disable with `UPGRADE_INTERVAL_SECS=0`): re-scores owned titles in a round-robin budget and stages a meaningfully-better CACHED release (uncached→cached; higher source tier; or higher resolution); idle-gated swap (`UPGRADE_IDLE_SECS`) + prune of the superseded torrent ensure an active stream is never interrupted. **Full-season consolidation:** the same engine finds a CACHED full-season pack covering all aired episodes at same-or-better quality and consolidates scattered per-episode torrents into it, idle-gated.

## Development Process

- **Test-Driven Development (TDD) is required.** When implementing new features or fixing bugs:
  1. Write a failing test first that captures the expected behavior.
  2. Write the minimum code to make the test pass.
  3. Refactor while keeping tests green.
  4. Run `cargo test` after every change to confirm nothing is broken.
- **Before every commit, run the lint gate (this is CI's `lint` job — fmt + clippy):**
  ```bash
  cargo fmt --check && cargo clippy --all-targets -- -D warnings
  ```
  CI runs these on the latest stable Rust (`dtolnay/rust-toolchain@stable`), and each new stable
  can add clippy lints — so keep your local toolchain current (`rustup update stable`), otherwise
  lint can pass locally yet fail in CI on a lint your older clippy doesn't have. If
  `cargo fmt --check` fails, run `cargo fmt` to fix; do not hand-format around it.
- **Before every commit, run all unit and integration tests:**
  ```bash
  cargo test \
    && INTEGRATION_TEST_LIMIT=10 cargo test --test integration_test -- --ignored \
    && INTEGRATION_TEST_LIMIT=10 cargo test --test repair_integration_test -- --ignored \
    && cargo test --test lifecycle_test -- --ignored
  ```
  Integration test binaries must run sequentially (not `-- --ignored` on all at once) because they share the redb database lock and the Real-Debrid API rate limit. Do not commit if any test fails.
  - `lifecycle_test` is the cross-provider add→appears→delete→disappears check, plus a `lifecycle_acquire_sintel_by_imdb` test that exercises the SP1 `AcquisitionEngine` end-to-end against the live provider. It runs against **both** Real-Debrid and TorBox, **modifying the live account** (adds and deletes a Creative-Commons *Sintel* torrent, cleaning up after itself). Each provider's sub-test **skips** if its token (`RD_API_TOKEN` / `TORBOX_API_KEY`) is unset, so it is safe to run with only one provider configured.
- **Integration tests must be updated for new functionality.** When adding or changing features, update the integration tests to cover the new behavior. Integration tests are the final gate before committing.
- **Additional integration test files** (all `#[ignore]`, require API tokens): `test_all_rd_torrents.rs`, `test_identification_stats.rs`, `test_short_titles.rs`, `test_media_generation.rs`, `video_player_simulation.rs`, `trakt_smoke_test.rs`. These are supplementary and not part of the pre-commit gate.
- **Interactive Trakt live integration test** (`tests/trakt_integration_test.rs`): validates every Trakt parser and `sync_trakt` end-to-end against a real account without requiring a pre-existing token. Run with `cargo test --test trakt_integration_test -- --ignored --nocapture`; the test prints a URL + code, waits until you open it and approve on trakt.tv/activate, then exercises all reads and asserts the `wanted` set is populated. Requires `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET`, and `TMDB_API_KEY`; skips cleanly if any is unset.
- **Before every commit, validate that `CLAUDE.md` and `README.md` are up-to-date.** If the commit introduces new features, env vars, modules, architectural changes, or modifies existing behavior, update both files to reflect the changes. Documentation must stay in sync with code at all times.
