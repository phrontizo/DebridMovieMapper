# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Project Does

DebridMovieMapper is a Rust async service that bridges a debrid service — **Real-Debrid or TorBox** — with media servers like Jellyfin and Plex. It fetches torrents from the configured debrid account, identifies them via TMDB metadata, and exposes a WebDAV endpoint serving proxied media files (the actual `.mkv`/`.mp4` bytes are fetched from the provider's CDN on demand). Exactly one provider is active per deployment, selected at startup by which token is set (`RD_API_TOKEN` or `TORBOX_API_KEY`).

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

# Trigger an acquisition manually (temporary SP1 verification trigger; to be removed)
# <type>: movie | series   <id>: IMDB id (tt...)   [season episode]: required for series
RD_API_TOKEN=<token> TMDB_API_KEY=<key> cargo run -- --acquire movie tt1727587
RD_API_TOKEN=<token> TMDB_API_KEY=<key> cargo run -- --acquire series tt0903747 1 1

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

## Architecture

The project is structured as both a binary (`main.rs`) and a library (`mapper.rs` as lib root).

**Background tasks (spawned in `main.rs`):**
- **Scan task** (every `SCAN_INTERVAL_SECS`): Polls the active debrid provider → identifies torrents via TMDB → updates the in-memory VFS. Implemented in `tasks.rs`.

**On-demand (synchronous, during WebDAV file reads):**
- **Repair:** Triggered when a media file is read and `provider.resolve_url` returns `AppError::Unavailable` (RD maps a 503 on unrestrict to it). For cached torrents, repair completes inline (~1-2s delay); for non-cached torrents, the file fails and a new torrent is left to download.

**Module responsibilities:**

| File | Purpose |
|------|---------|
| `main.rs` | Initializes shared state, spawns scan task, starts WebDAV server on port 8080; `--healthcheck` mode for Docker |
| `config.rs` | `Config` — all startup env parsing/validation (`from_env`/`from_parts`); shaped for a future DB-override layer |
| `app_state.rs` | `AppState` — `Clone` bundle of shared handles (provider, tmdb, vfs, store, repair, config, jellyfin, http) carried by the scan task |
| `tasks.rs` | `run_scan_loop` — polls Real-Debrid, identifies new torrents, updates VFS |
| `provider.rs` | `DebridProvider` trait abstracting the debrid backend; `FileLocator` (the provider-neutral handle for a media file) and its `resolve_url`/`invalidate` primitives; startup provider selection (`choose_provider`); test-only `MockProvider` |
| `rd_client.rs` | Real-Debrid implementation of `DebridProvider`; shared adaptive rate limiter, 1-hour unrestrict cache |
| `torbox_client.rs` | TorBox implementation of `DebridProvider` (`mylist`/`requestdl`/`createtorrent`/`controltorrent`); resolves by `(torrent_id, file_id)`, ~3h URL cache |
| `ratelimit.rs` | Shared `AdaptiveRateLimiter` (token bucket) used by both provider clients |
| `identification.rs` | Filename cleaning, camelCase splitting, TMDB scoring to identify movies/shows |
| `vfs.rs` | In-memory virtual filesystem: creates `Movies/`+`Shows/` hierarchy with media files and NFO metadata |
| `dav_fs.rs` | Maps VFS to WebDAV; on read resolves a `FileLocator` to a CDN URL via `provider.resolve_url` (cached 1h), attempts synchronous instant repair on `AppError::Unavailable` |
| `repair.rs` | Torrent repair state machine (Healthy→Broken→Repairing→Failed); provider-neutral instant repair — `try_instant_repair(&FileLocator)` re-adds by hash and, if cached, returns a fresh `FileLocator` for the same file (matched by `file_path`) for `dav_fs` to swap in (30s cooldown, max 3 attempts) |
| `tmdb_client.rs` | TMDB search for movies and TV shows |
| `store.rs` | `Store` — owns all redb table definitions, schema version + migration hook, `open()` with auto-recovery (moves an unreadable/incompatible DB aside and recreates it), and typed async accessors for the `matches` cache, `owned_hashes`, `authoritative_ids`, and `blacklist` tables |
| `scraper.rs` | `Scraper` trait + `TorrentioScraper` — calls a Stremio-compatible addon (default: Torrentio, auto-templated from the provider token) and parses stream objects into `RawCandidate` values |
| `release.rs` | `RawCandidate` → `ReleaseInfo` parser (resolution, codec, HDR, size, seeders, cached flag, source tier); `score`/`rank` with quality prefs; hard-filters the resolution ceiling, cam/telesync/screener sources, **and uncached zero-seeder releases** (quality/availability floor), then ranks by source tier (REMUX>BluRay>WEB>HDTV) and bitrate |
| `probe.rs` | Hand-rolled MKV/MP4 track reader; `probe_tracks` fetches the first 4 MB of a CDN URL and extracts audio/subtitle language codes; `verify` checks language requirements; corrupt/mismatch → blacklist, unknown → accept; a truncated/under-fetched ranged read → `Transient` (defer + retry, **not** blacklisted) |
| `reacquire.rs` | `materialise` — shared primitive (also used by `repair.rs`) that re-adds a torrent by hash, **polls** for the file list (retrying every `settle` up to a caller-set `max_wait`, so slow-to-resolve uncached metadata isn't given up on after one check; acquisition waits ~15s, repair keeps single-poll), and selects the target files; shared between the acquisition engine and the instant-repair path |
| `acquire.rs` | `AcquisitionEngine` — scrape → score → materialise → reject multi-feature packs for movie requests → STRICT title validation via `identify_name` → file-probe audio/subtitle → record in store; `observe` monitors Pending torrents each scan tick (stall detection, deferred probe retry, fail-and-reacquire) |
| `error.rs` | Unified error type (`AppError`) using `thiserror` |
| `jellyfin_client.rs` | Optional Jellyfin notification client — notifies Jellyfin of changed paths via `POST /Library/Media/Updated` |
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
- **SP1 Acquisition Engine:** When a title is requested (via the temporary `--acquire` CLI or future automation), `AcquisitionEngine` runs: (1) **Scrape** — `TorrentioScraper` calls a Stremio-compatible addon (default: Torrentio, URL auto-templated from `<provider>=<token>`; override with `SCRAPER_ADDON_URL`). (2) **Score/rank** — `release::parse` extracts resolution, codec, HDR, size, seeders, cached flag, and source tier; `release::rank` hard-filters the resolution ceiling (`MAX_RESOLUTION`), cam/telesync/screener sources, **and uncached zero-seeder (undownloadable) releases** (so the engine never burns acquire attempts on dead torrents or leaves "checking" magnets behind), then sorts by a weighted score (cached > source tier [REMUX>BluRay>WEB>HDTV] > resolution > HEVC > verifiable container > bitrate > seeders). (3) **Materialise** — shared with `repair.rs` via `reacquire::materialise`; re-adds the torrent by hash, waits for the file list, and selects the target file; **a movie request whose materialised torrent has more than one feature-sized video is rejected as a multi-movie pack** (providers like TorBox auto-select all files). A candidate whose materialise *fails* (provider "already queued", or an add whose file list never resolves) has any leaked torrent for its hash deleted before the next candidate is tried, so a rejected/failed candidate never lingers as a dead "checking" torrent. (4) **STRICT title validation** — `TmdbTitleValidator` runs `identify_name` on the selected file's name and verifies the result resolves to the requested TMDB id; mismatches are blacklisted and the next candidate is tried. (5) **File-probe** — `HttpProber` fetches the first 4 MB of the CDN URL and uses the hand-rolled `probe::probe_tracks` (MKV/MP4) to extract audio/subtitle language codes; corrupt probes are blacklisted, wrong-language probes are blacklisted, unknown formats are accepted. (6) **Outcome** — `Acquired` (cached + verified), `Pending` (downloading or probe deferred), `NoAcceptableRelease`, or `TemporarilyUnavailable`. `observe` monitors Pending torrents each scan tick: stall detection (no progress for `STALL_TIMEOUT_SECS`), deferred probe retry (up to `MAX_VERIFY_ATTEMPTS`), and fail-and-reacquire. The `authoritative_ids` table overrides filename identification for engine-owned torrents; the `blacklist` table prevents re-adding rejected hashes.
- **Database is self-healing:** `Store::open` stamps a schema version and runs forward migrations. An unreadable / incompatible / corrupt / newer-than-binary `metadata.db` (e.g. after a `redb` format change) is **moved aside** to `<db_path>.corrupt` and recreated rather than crashing the service. The `matches` table is a regenerable cache, so this is lossless in practice; authoritative tables added in later phases are migrated, never silently dropped.

## Development Process

- **Test-Driven Development (TDD) is required.** When implementing new features or fixing bugs:
  1. Write a failing test first that captures the expected behavior.
  2. Write the minimum code to make the test pass.
  3. Refactor while keeping tests green.
  4. Run `cargo test` after every change to confirm nothing is broken.
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
- **Additional integration test files** (all `#[ignore]`, require API tokens): `test_all_rd_torrents.rs`, `test_identification_stats.rs`, `test_short_titles.rs`, `test_media_generation.rs`, `video_player_simulation.rs`. These are supplementary and not part of the pre-commit gate.
- **Before every commit, validate that `CLAUDE.md` and `README.md` are up-to-date.** If the commit introduces new features, env vars, modules, architectural changes, or modifies existing behavior, update both files to reflect the changes. Documentation must stay in sync with code at all times.
