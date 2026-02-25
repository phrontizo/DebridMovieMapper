# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Project Does

DebridMovieMapper is a Rust async service that bridges Real-Debrid (a debrid/torrent service) with media servers like Jellyfin and Plex. It fetches torrents from a Real-Debrid account, identifies them via TMDB metadata, and exposes a WebDAV endpoint serving `.strm` files that point directly to Real-Debrid download URLs for streaming.

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

# Docker (single platform)
docker build -t debridmoviemapper .

# Docker multi-platform push
./build.sh

# Start full stack (WebDAV + rclone + Jellyfin)
touch metadata.db rclone.conf
docker compose up -d
```

**Required environment variables:**
- `RD_API_TOKEN` — Real-Debrid API token
- `TMDB_API_KEY` — TMDB API key

**Optional:**
- `SCAN_INTERVAL_SECS` (default: 60) — how often to poll Real-Debrid
- `JELLYFIN_URL` — Jellyfin server URL (e.g. `http://jellyfin:8096`)
- `JELLYFIN_API_KEY` — Jellyfin API key
- `JELLYFIN_RCLONE_MOUNT_PATH` — rclone mount path as seen by Jellyfin (e.g. `/media`)

## Architecture

The project is structured as both a binary (`main.rs`) and a library (`mapper.rs` as lib root).

**Background tasks (spawned in `main.rs`):**
- **Scan task** (every `SCAN_INTERVAL_SECS`): Polls Real-Debrid → identifies torrents via TMDB → updates the in-memory VFS. Implemented in `tasks.rs`.
- **Repair:** On-demand only — triggered when a `.strm` file is read and its link returns 503. No background polling.

**Module responsibilities:**

| File | Purpose |
|------|---------|
| `main.rs` | Initializes shared state, spawns scan task, starts WebDAV server on port 8080 |
| `tasks.rs` | `run_scan_loop` — polls Real-Debrid, identifies new torrents, updates VFS |
| `rd_client.rs` | Real-Debrid API client with adaptive token bucket rate limiter, 1-hour response cache |
| `identification.rs` | Filename cleaning, camelCase splitting, TMDB scoring to identify movies/shows |
| `vfs.rs` | In-memory virtual filesystem: creates `Movies/`+`Shows/` hierarchy, generates `.strm` files |
| `dav_fs.rs` | Maps VFS to WebDAV; re-unrestricts links on read and triggers on-demand repair on 503 |
| `repair.rs` | Torrent repair state machine (Healthy→Broken→Repairing→Failed), on-demand repair |
| `tmdb_client.rs` | TMDB search for movies and TV shows |
| `error.rs` | Unified error type (`AppError`) using `thiserror` |
| `jellyfin_client.rs` | Optional Jellyfin notification client — notifies Jellyfin of changed paths via `POST /Library/Media/Updated` |
| `mapper.rs` | Library root — module declarations |

**Data flow for playback:**
1. Jellyfin/player reads a `.strm` file via WebDAV
2. The `.strm` contains a Real-Debrid unrestrict URL
3. Player streams directly from Real-Debrid (no proxying)

**Persistence:** Embedded `redb` database (`metadata.db`) caches TMDB identifications. The file is created automatically on first run.

## Key Design Decisions

- **Static linking / scratch Docker image:** The Dockerfile builds with musl targets (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) producing a minimal final image on `scratch`.
- **Adaptive rate limiting:** An `AdaptiveRateLimiter` (token bucket, capacity 1) is shared across all Real-Debrid API calls. Baseline: 10 req/s (100ms interval). On 429: interval doubles (max 2000ms / 0.5 req/s) and Retry-After header is respected. On success: interval decreases by 10ms (min 100ms). This prevents 429 cascades by slowing all requests globally. 503 on `unrestrict/link` is a terminal status (no retry) — it signals a broken torrent and triggers on-demand repair.
- **Single retry method:** `rd_client.rs` has one `fetch_with_retry(make_request, terminal_statuses)` — callers pass the status codes that should abort without retrying (e.g. 404 for `get_torrent_info`, 503 for `unrestrict_link`).
- **Identification scoring:** `identification.rs` cleans filenames extensively before TMDB lookup. Shows vs. movies are detected by file structure (presence of multiple video files with episode patterns).
- **On-demand repair:** When a `.strm` file is read, `dav_fs.rs` re-unrestricts the link (cached, so free when healthy). On 503 it calls `repair_manager.repair_by_id()` in a background task and hides the torrent until repaired.
- **Repair hides broken torrents:** During repair, entries are hidden from the VFS so they don't appear in Jellyfin until healthy again.

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
