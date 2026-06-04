# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Project Does

DebridMovieMapper is a Rust async service that bridges Real-Debrid (a debrid/torrent service) with media servers like Jellyfin and Plex. It fetches torrents from a Real-Debrid account, identifies them via TMDB metadata, and exposes a WebDAV endpoint serving proxied media files (the actual `.mkv`/`.mp4` bytes are fetched from Real-Debrid CDN on demand).

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
- **Scan task** (every `SCAN_INTERVAL_SECS`): Polls Real-Debrid → identifies torrents via TMDB → updates the in-memory VFS. Implemented in `tasks.rs`.

**On-demand (synchronous, during WebDAV file reads):**
- **Repair:** Triggered when a media file is read and `provider.resolve_url` returns `AppError::Unavailable` (RD maps a 503 on unrestrict to it). For cached torrents, repair completes inline (~1-2s delay); for non-cached torrents, the file fails and a new torrent is left to download.

**Module responsibilities:**

| File | Purpose |
|------|---------|
| `main.rs` | Initializes shared state, spawns scan task, starts WebDAV server on port 8080; `--healthcheck` mode for Docker |
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
| `error.rs` | Unified error type (`AppError`) using `thiserror` |
| `jellyfin_client.rs` | Optional Jellyfin notification client — notifies Jellyfin of changed paths via `POST /Library/Media/Updated` |
| `mapper.rs` | Library root — module declarations |

**Data flow for playback:**
1. Jellyfin/player opens a media file via WebDAV
2. `dav_fs.rs` resolves the file's `FileLocator` to a CDN URL via `provider.resolve_url(&FileLocator)` (cached). Real-Debrid resolves via the locator's restricted `link` (1h cache); TorBox resolves by `(torrent_id, file_id)` via `requestdl` (no per-file link; ~3h cache).
3. `ProxiedMediaFile` fetches bytes from the CDN URL with a 2MB read-ahead buffer and serves them to the player

**Persistence:** Embedded `redb` database (`metadata.db`) caches TMDB identifications. The file is created automatically on first run.

## Key Design Decisions

- **Provider abstraction:** All components depend on `Arc<dyn DebridProvider>` (defined in `provider.rs`) rather than a concrete client. `RealDebridClient` and `TorBoxClient` are the two implementations; exactly one provider is active per deployment, chosen at startup by `choose_provider` based on which token (`RD_API_TOKEN` or `TORBOX_API_KEY`) is set.
- **TorBox specifics:** TorBox lists torrents (with files inline) via `mylist`, resolves a file by `(torrent_id, file_id)` via `requestdl` (so `FileLocator.link` is `None`), re-adds by hash via `createtorrent`, deletes via `controltorrent`, and treats `select_files` as a no-op (TorBox auto-selects). A finished download is normalised to `status = "downloaded"` even when its cache has lapsed (TorBox keeps such entries listed as Inactive), so owned-but-uncached films still appear in the library and re-acquire on playback.
- **Provider-neutral file resolution:** The VFS stores a `FileLocator { hash, torrent_id, file_id, file_path, link }` per media file rather than a raw RD link. `resolve_url(&FileLocator) -> Result<String, AppError>` is the single resolution primitive (and `invalidate(&FileLocator)` drops a cached resolution); RD resolves via the locator's restricted `link` (keeping its old unrestrict/cache logic as private inherent methods), while `(torrent_id, file_id)` is used by providers without per-file links (TorBox). `AppError::Unavailable` is the provider-neutral "bytes not currently available → re-acquire/repair" signal — RD maps a 503 on unrestrict to it, and `dav_fs` drives instant repair off `Unavailable` rather than inspecting an HTTP status. Real-Debrid behaviour is unchanged.
- **Static linking / scratch Docker image:** The Dockerfile builds with musl targets (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) producing a minimal final image on `scratch`.
- **Adaptive rate limiting:** An `AdaptiveRateLimiter` (token bucket, capacity 1) is shared across all Real-Debrid API calls. Baseline: 10 req/s (100ms interval). On 429: interval doubles (max 2000ms / 0.5 req/s) and Retry-After header is respected. On success: interval decreases by 10ms (min 100ms). This prevents 429 cascades by slowing all requests globally. 503 on `unrestrict/link` is a terminal status (no retry) — it signals a broken torrent and triggers on-demand repair.
- **Single retry method:** `rd_client.rs` has one `fetch_with_retry(make_request, terminal_statuses)` — callers pass the status codes that should abort without retrying (e.g. 404 for `get_torrent_info`, 503 for `unrestrict_link`).
- **Identification scoring:** `identification.rs` cleans filenames extensively before TMDB lookup. Shows vs. movies are detected by file structure (presence of multiple video files with episode patterns).
- **Synchronous instant repair (provider-neutral):** When a media file is read, `dav_fs.rs` resolves the locator via `provider.resolve_url` (cached, so free when healthy). On `AppError::Unavailable`, it calls `repair_manager.try_instant_repair(&FileLocator) -> Result<FileLocator, _>` synchronously — re-adds the torrent by hash, selects the same files, and checks if the replacement is already cached. Success is decided by the new torrent reaching `status == "downloaded"` **plus** a `file_path` match for the same file (there is no `InstantRepairResult` type, and no RD-specific link-count check — path matching is inherently safe, and the replacement file is located by matching `file_path` rather than the old positional index into RD's `links` array). On success it returns a fresh `FileLocator` for that file — pairing the per-file restricted `link` by position among selected files for RD, or leaving `link: None` for providers that resolve by `(torrent_id, file_id)` (TorBox, a later phase). `dav_fs` installs the returned locator (invalidating the old resolution) and re-resolves inline, so playback continues after a brief delay. If not cached, the file fails and the new torrent is left to download, the old torrent is marked Broken, and the replacement is recorded (scan loop picks it up). The health state machine (30s cooldown, max 3 attempts) is unchanged.
- **Repair hides broken torrents:** Non-cached torrents are marked Broken and hidden from WebDAV until the scan loop picks up the replacement torrent.

## Development Process

- **Test-Driven Development (TDD) is required.** When implementing new features or fixing bugs:
  1. Write a failing test first that captures the expected behavior.
  2. Write the minimum code to make the test pass.
  3. Refactor while keeping tests green.
  4. Run `cargo test` after every change to confirm nothing is broken.
- **Before every commit, run all unit and integration tests:**
  ```bash
  cargo test && INTEGRATION_TEST_LIMIT=10 cargo test --test integration_test -- --ignored && INTEGRATION_TEST_LIMIT=10 cargo test --test repair_integration_test -- --ignored
  ```
  Integration test binaries must run sequentially (not `-- --ignored` on all at once) because they share the redb database lock and the Real-Debrid API rate limit. Do not commit if any test fails.
- **Integration tests must be updated for new functionality.** When adding or changing features, update the integration tests to cover the new behavior. Integration tests are the final gate before committing.
- **Additional integration test files** (all `#[ignore]`, require API tokens): `test_all_rd_torrents.rs`, `test_identification_stats.rs`, `test_short_titles.rs`, `test_media_generation.rs`, `video_player_simulation.rs`. These are supplementary and not part of the pre-commit gate.
- **Before every commit, validate that `CLAUDE.md` and `README.md` are up-to-date.** If the commit introduces new features, env vars, modules, architectural changes, or modifies existing behavior, update both files to reflect the changes. Documentation must stay in sync with code at all times.
