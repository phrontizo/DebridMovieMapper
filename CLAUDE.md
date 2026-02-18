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

# Run all tests
cargo test

# Run a single test
cargo test <test_name>

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
- `REPAIR_INTERVAL_SECS` (default: 3600) — how often to check torrent health

## Architecture

The project is structured as both a binary (`main.rs`) and a library (`mapper.rs` as lib root).

**Background tasks (spawned in `main.rs`):**
- **Scan task** (every `SCAN_INTERVAL_SECS`): Polls Real-Debrid → identifies torrents via TMDB → updates the in-memory VFS
- **Repair task** (every `REPAIR_INTERVAL_SECS`): Checks link health → detects broken torrents → triggers repair via magnet links

**Module responsibilities:**

| File | Purpose |
|------|---------|
| `main.rs` | Initializes shared state, spawns tasks, starts WebDAV server on port 8080 |
| `rd_client.rs` | Real-Debrid API client with exponential backoff (429 handling), 1-hour response cache |
| `identification.rs` | Filename cleaning, camelCase splitting, TMDB scoring to identify movies/shows |
| `vfs.rs` | In-memory virtual filesystem: creates `Movies/`+`Shows/` hierarchy, generates `.strm` files |
| `dav_fs.rs` | Maps VFS to WebDAV protocol via the `dav-server` crate |
| `repair.rs` | Torrent health state machine (Healthy→Checking→Broken→Repairing→Failed), auto-repair |
| `tmdb_client.rs` | TMDB search for movies and TV shows |
| `mapper.rs` | Library root — module declarations and shared utilities |

**Data flow for playback:**
1. Jellyfin/player reads a `.strm` file via WebDAV
2. The `.strm` contains a Real-Debrid unrestrict URL
3. Player streams directly from Real-Debrid (no proxying)

**Persistence:** Embedded `sled` database (`metadata.db`) caches TMDB identifications and unrestrict responses. The file must exist before starting (`touch metadata.db`).

## Key Design Decisions

- **Static linking / scratch Docker image:** The Dockerfile builds with musl targets (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) producing a minimal final image on `scratch`.
- **Rate limiting:** Real-Debrid 429 responses trigger exponential backoff (2s→4s→8s→16s→32s). 503 responses bypass retries and trigger immediate repair.
- **Identification scoring:** `identification.rs` cleans filenames extensively before TMDB lookup. Shows vs. movies are detected by file structure (presence of multiple video files with episode patterns).
- **Repair hides broken torrents:** During repair, entries are removed from the VFS so they don't appear in Jellyfin until healthy again.
