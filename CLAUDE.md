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
- `RD_API_TOKEN` — Real-Debrid API token
- `TMDB_API_KEY` — TMDB API key

**Optional:**
- `SCAN_INTERVAL_SECS` (default: 60) — how often to poll Real-Debrid
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
- **Repair:** Triggered when a media file is read and its RD link returns 503. For cached torrents, repair completes inline (~1-2s delay); for non-cached torrents, the file fails and a new torrent is left to download.

**Module responsibilities:**

| File | Purpose |
|------|---------|
| `main.rs` | Initializes shared state, spawns scan task, starts WebDAV server on port 8080; `--healthcheck` mode for Docker |
| `tasks.rs` | `run_scan_loop` — polls Real-Debrid, identifies new torrents, updates VFS |
| `rd_client.rs` | Real-Debrid API client with adaptive token bucket rate limiter, 1-hour response cache |
| `identification.rs` | Filename cleaning, camelCase splitting, TMDB scoring to identify movies/shows |
| `vfs.rs` | In-memory virtual filesystem: creates `Movies/`+`Shows/` hierarchy with media files and NFO metadata |
| `dav_fs.rs` | Maps VFS to WebDAV; lazily unrestricts links on read (cached 1h), attempts synchronous instant repair on 503 |
| `repair.rs` | Torrent repair state machine (Healthy→Broken→Repairing→Failed), instant repair for cached torrents (30s cooldown, max 3 attempts) |
| `tmdb_client.rs` | TMDB search for movies and TV shows |
| `error.rs` | Unified error type (`AppError`) using `thiserror` |
| `jellyfin_client.rs` | Optional Jellyfin notification client — notifies Jellyfin of changed paths via `POST /Library/Media/Updated` |
| `mapper.rs` | Library root — module declarations |

**Data flow for playback:**
1. Jellyfin/player opens a media file via WebDAV
2. `dav_fs.rs` lazily unrestricts the RD link to get a CDN URL (cached for 1 hour)
3. `ProxiedMediaFile` fetches bytes from the CDN URL with a 2MB read-ahead buffer and serves them to the player

**Persistence:** Embedded `redb` database (`metadata.db`) caches TMDB identifications. The file is created automatically on first run.

## Key Design Decisions

- **Static linking / scratch Docker image:** The Dockerfile builds with musl targets (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) producing a minimal final image on `scratch`.
- **Adaptive rate limiting:** An `AdaptiveRateLimiter` (token bucket, capacity 1) is shared across all Real-Debrid API calls. Baseline: 10 req/s (100ms interval). On 429: interval doubles (max 2000ms / 0.5 req/s) and Retry-After header is respected. On success: interval decreases by 10ms (min 100ms). This prevents 429 cascades by slowing all requests globally. 503 on `unrestrict/link` is a terminal status (no retry) — it signals a broken torrent and triggers on-demand repair.
- **Single retry method:** `rd_client.rs` has one `fetch_with_retry(make_request, terminal_statuses)` — callers pass the status codes that should abort without retrying (e.g. 404 for `get_torrent_info`, 503 for `unrestrict_link`).
- **Identification scoring:** `identification.rs` cleans filenames extensively before TMDB lookup. Shows vs. movies are detected by file structure (presence of multiple video files with episode patterns).
- **Synchronous instant repair:** When a media file is read, `dav_fs.rs` re-unrestricts the link (cached, so free when healthy). On 503, it calls `repair_manager.try_instant_repair()` synchronously — re-adds the torrent via magnet, selects the same files, and checks if the torrent is already cached on RD. If cached (status "downloaded" within ~1s), the new link is unrestricted inline and playback continues after a brief delay. If not cached, the file fails and the new torrent is left to download (scan loop picks it up).
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
