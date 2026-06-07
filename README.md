# DebridMovieMapper

A Rust-based service that maps your debrid torrent library — **Real-Debrid or TorBox** — to a Jellyfin/Plex-compatible WebDAV endpoint with automatic media identification via TMDB.

I created this project as:
* I was using the various arrs, and found it cumbersome, plus I ran out of storage space, meaning I needed to use a Debrid service of some kind
* Zurg didn't work for me as I needed the folder structure for Jellyfin

I use Debrid Media Manager for keeping Real Debrid populated at the moment, but am thinking of incorporating that into this service as well.

Note that this is still a work in progress and is provided as-is for educational purposes only.

Future work:
* Test with Kodi 
* Add a web-ui to track progress and correct mismatches
* Automatically populate Real-Debrid with watchlist content from Trakt and newly released episodes from already tracked shows

This was 100% vibe coded using a mix of Claude and Junie as further AI experimentation.
## Features

- **Media Identification**: Automatically identifies movies and TV shows using TMDB metadata based on torrent filenames.
- **Jellyfin/Plex Structure**: Organizes your library into a clean `Movies/` and `Shows/` directory structure.
- **Season Grouping**: Automatically groups TV show episodes into `Season XX` folders.
- **Real-Debrid *and* TorBox**: One codebase, either provider. Set `RD_API_TOKEN` for Real-Debrid or `TORBOX_API_KEY` for TorBox (exactly one) — everything else works the same.
- **WebDAV Endpoint**: Exposes a WebDAV server (port 8080) serving proxied media files with real file sizes and extensions. Media bytes are fetched on demand from the provider's CDN. Mount via rclone for use with Jellyfin/Plex.
- **On-Demand Repair**: Detects unavailable files at playback time (a 503 from Real-Debrid, or an uncached/expired file on TorBox) and attempts instant synchronous repair by re-adding the torrent. For cached content, playback continues after a ~1-2s delay; otherwise a fresh download is started automatically.
- **Persistent Cache**: Uses an embedded database (`redb`) to cache media identifications, reducing API calls and speeding up restarts.
- **Configurable Scan Interval**: Customizable scan interval via environment variable.
- **Robust Identification Logic**: Handles complex torrent naming conventions, including CamelCase splitting, technical metadata stripping, and multi-service fallback strategies.
- **Jellyfin Notifications**: Optionally notifies Jellyfin when content changes so new episodes and movies appear immediately without waiting for a full library scan.

## Prerequisites

- A **Real-Debrid** *or* **TorBox** account and API token (set exactly one).
- A **TMDB** API Key (The Movie Database).

## Configuration

The service is configured via environment variables. You can use a `.env` file in the project root:

```env
# Debrid provider — set exactly one of RD_API_TOKEN or TORBOX_API_KEY (not both)
RD_API_TOKEN=your_real_debrid_token
# TORBOX_API_KEY=your_torbox_token   # ...or use TorBox instead of Real-Debrid
TMDB_API_KEY=your_tmdb_api_key

# Optional
SCAN_INTERVAL_SECS=60       # How often to scan for new torrents (default: 60, minimum: 10)
DB_PATH=metadata.db         # Path to the redb database file (default: metadata.db)
PORT=8080                   # WebDAV server listen port (default: 8080)

# Optional: Jellyfin integration (all three required to enable)
JELLYFIN_URL=http://jellyfin:8096
JELLYFIN_API_KEY=your_jellyfin_api_key
JELLYFIN_RCLONE_MOUNT_PATH=/media

# Optional: SP1 acquisition preferences (see Acquisition section below)
# SCRAPER_ADDON_URL=https://torrentio.strem.fun/realdebrid=TOKEN   # override scraper URL
# MAX_RESOLUTION=1080        # 720 | 1080 | 2160
# AUDIO_LANGUAGE=original    # original | eng | ...
# SUBTITLE_LANGUAGE=         # none (default) | eng | ...
# PREFER_HEVC=true
# PREFER_HDR=false
# STALL_TIMEOUT_SECS=1800
# MAX_ACQUIRE_ATTEMPTS=5
```

### Environment Variables

A debrid provider token is required: set **exactly one** of `RD_API_TOKEN` or `TORBOX_API_KEY` (setting both, or neither, is a startup error). Whichever token is set selects the active provider for that deployment — both Real-Debrid and TorBox are fully supported.

| Variable                     | Required | Default | Description                                                          |
|------------------------------|----------|---------|----------------------------------------------------------------------|
| `RD_API_TOKEN`               | One of\* | -              | Your Real-Debrid API token                                           |
| `TORBOX_API_KEY`             | One of\* | -              | Your TorBox API token                                                |
| `TMDB_API_KEY`               | Yes      | -              | Your TMDB (The Movie Database) API key                               |
| `SCAN_INTERVAL_SECS`         | No       | 60             | Interval between torrent library scans in seconds (minimum: 10, runs immediately on startup) |
| `DB_PATH`                    | No       | `metadata.db`  | Path to the redb database file. If the file is unreadable, corrupt, or from an incompatible schema version, it is automatically moved to `<DB_PATH>.corrupt` and recreated — upgrades never crash-loop. |
| `PORT`                       | No       | 8080           | WebDAV server listen port                                            |
| `JELLYFIN_URL`               | No       | -              | Jellyfin server URL for library update notifications                 |
| `JELLYFIN_API_KEY`           | No       | -              | Jellyfin API key for authentication                                  |
| `JELLYFIN_RCLONE_MOUNT_PATH` | No       | -              | rclone mount path as seen by Jellyfin (e.g. `/media`)                |
| `SCRAPER_ADDON_URL`          | No       | *(auto)*       | Override the Torrentio scraper base URL. Defaults to a URL auto-built from your provider token (`https://torrentio.strem.fun/<provider>=<token>`). |
| `MAX_RESOLUTION`             | No       | `1080`         | Hard resolution ceiling for acquisition: `720`, `1080`, `2160` / `4k`. Candidates above this height are excluded. |
| `AUDIO_LANGUAGE`             | No       | `original`     | Required audio language for acquisition: an ISO code (e.g. `eng`) or `original` (uses the title's original language from TMDB). |
| `SUBTITLE_LANGUAGE`          | No       | *(none)*       | Required subtitle language for acquisition: an ISO code, or omit / set to `none` to skip the check. |
| `PREFER_HEVC`                | No       | `true`         | Prefer HEVC/H.265 encodes when scoring acquisition candidates. |
| `PREFER_HDR`                 | No       | `false`        | Prefer HDR/Dolby Vision encodes when scoring acquisition candidates. |
| `STALL_TIMEOUT_SECS`         | No       | `1800`         | Seconds without download progress before a Pending torrent is considered stalled and re-acquired. |
| `MAX_ACQUIRE_ATTEMPTS`       | No       | `5`            | Maximum number of candidates to try before giving up on a title. |

\* Exactly one of `RD_API_TOKEN` / `TORBOX_API_KEY` must be set — not both, and not neither.

## Running with Docker

The easiest way to run DebridMovieMapper is using Docker. Pre-built multi-platform images are available via the GitHub Container Registry.

### 1. Pull the image

Images are available from both GitHub Container Registry and Docker Hub:
```bash
# GitHub Container Registry
docker pull ghcr.io/phrontizo/debridmoviemapper:latest

# Docker Hub
docker pull phrontizo/debridmoviemapper:latest
```

**Available tags:**
- `:latest` — latest stable release (updated on version tags like `v1.0.6`)
- `:edge` — latest build from `main` branch (may be unstable)
- `:1.0.6`, `:1.0`, `:1` — pinned to a specific release version

### 2. Run the container
```bash
docker run -d \
  --name debridmoviemapper \
  -p 127.0.0.1:8080:8080 \
  -e RD_API_TOKEN=your_token \
  -e TMDB_API_KEY=your_api_key \
  -e SCAN_INTERVAL_SECS=60 \
  -v debridmoviemapper-metadata:/data \
  ghcr.io/phrontizo/debridmoviemapper:latest
```

> ⚠️ The WebDAV endpoint is **unauthenticated** and proxies media fetches, so the example binds it to `127.0.0.1` (host-local only). Do not expose it to an untrusted network — if a remote consumer needs access, put it behind a reverse proxy with authentication, or restrict it to a private network.

> To run against TorBox instead, use `-e TORBOX_API_KEY=your_token` in place of `-e RD_API_TOKEN=your_token` (set exactly one).

### Building from source (Optional)
Build locally for your current architecture:
```bash
docker build -t debridmoviemapper .
```

Multi-platform images are built and pushed automatically by GitHub Actions. Release tags (e.g. `git tag v1.0.6 && git push origin v1.0.6`) update `:latest` and semver tags; pushes to `main` update `:edge`.

*Note: The named volume ensures your media identification cache is preserved across container recreations.*

**Upgrading from a bind-mounted `metadata.db`:** If you previously used `-v $(pwd)/metadata.db:/metadata.db`, switch to a named volume. You can let the database regenerate automatically (TMDB identifications will be re-fetched on the first scan), or copy your existing file into the volume.

**Upgrading from sled (pre-redb):** If you previously ran an older version that used sled, your `metadata.db` will be a directory. Remove it before starting the new version: `rm -rf metadata.db`. The redb database will be recreated automatically.

## Docker Compose Setup with Jellyfin

The recommended way to use DebridMovieMapper with Jellyfin is via Docker Compose with rclone mounting the WebDAV endpoint.

A complete [`compose.yml`](compose.yml) file is provided in the repository that includes:
- DebridMovieMapper service
- rclone for mounting the WebDAV endpoint
- Jellyfin for media playback

### Setup Steps

1. Create the rclone mount directory:
   ```bash
   mkdir -p rclone && chown 65534:65534 rclone
   ```

2. Create a `.env` file with your credentials (or export them in your environment):
   ```bash
   RD_API_TOKEN=your_real_debrid_token
   TMDB_API_KEY=your_tmdb_api_key
   JELLYFIN_API_KEY=your_jellyfin_api_key  # optional
   ```

3. Start the services:
   ```bash
   docker compose up -d
   ```

4. Access Jellyfin at `http://localhost:8096` and add `/media` as a library path

5. (Optional) To enable instant library updates, set `JELLYFIN_API_KEY` in your `.env` file

### Notes

- The WebDAV server runs on port 8080 (mapped to 8080 on the host)
- A Docker healthcheck ensures rclone only starts after the WebDAV server is ready (prevents empty mount on startup)
- Media identifications are persisted in a named Docker volume (`metadata`)
- rclone mounts the WebDAV endpoint to `./rclone` on the host via a bind mount with `rshared` propagation (required for FUSE mounts to be visible to other containers)
- Jellyfin reads from `/media` which is bind-mounted from `./rclone`
- Files appear as real media files (`.mkv`, `.mp4`, etc.) with correct file sizes — media bytes are proxied from the debrid provider's CDN on demand
- Jellyfin probes and plays content normally as if the files were local

## Usage

Once running, the WebDAV server will be available at `http://localhost:8080`. Mount it with rclone and point your media server at the mount:

- **Jellyfin** (via rclone mount - see Docker Compose example above)
- **Plex** (via rclone mount)
- **Kodi**
- **Infuse** (iOS/tvOS/macOS)

## Acquisition (SP1)

The SP1 acquisition engine lets the service find and add content to your debrid account automatically, using Torrentio as a scraper.

### How Acquisition Works

1. **Scrape**: Given an IMDB id and media type, `TorrentioScraper` queries a Stremio-compatible addon (default: Torrentio, URL auto-built from your provider token; override with `SCRAPER_ADDON_URL`) for candidate torrents.
2. **Score and rank**: Candidates are parsed for resolution, codec, HDR, file size, seeder count, and cached status. A hard ceiling at `MAX_RESOLUTION` excludes anything above it. Remaining candidates are ranked: cached content first, then by resolution, HEVC preference, verifiable container (MKV/MP4), and seeder count.
3. **Materialise**: The top candidate is added to your debrid account by hash and selected for download.
4. **Title validation**: The acquired file's name is run through the same identification logic used by the scan loop and must resolve to the requested TMDB id. Mismatches are blacklisted and the next candidate is tried.
5. **File probe**: For cached (immediately downloadable) content, the first 4 MB of the CDN URL is fetched and parsed for audio/subtitle tracks. If `AUDIO_LANGUAGE` or `SUBTITLE_LANGUAGE` is set and the tracks don't match, the candidate is blacklisted. Unknown or unsupported containers are accepted without probing.
6. **Outcome**: `Acquired` (ready to play), `Pending` (still downloading — the scan loop will verify it when complete), `NoAcceptableRelease`, or `TemporarilyUnavailable` (scraper unreachable — retry later).

The `observe` method runs each scan tick and handles Pending torrents: it re-probes completed downloads, detects stalled downloads (no progress for `STALL_TIMEOUT_SECS`), and re-acquires failed torrents using the next unblacklisted candidate.

### Temporary `--acquire` CLI

A temporary command-line trigger is included for SP1 verification:

```bash
# Acquire a movie by IMDB id
RD_API_TOKEN=<token> TMDB_API_KEY=<key> cargo run -- --acquire movie tt1727587

# Acquire a specific episode
RD_API_TOKEN=<token> TMDB_API_KEY=<key> cargo run -- --acquire series tt0903747 1 1
```

This flag is a development aid and will be removed once a proper UI or automation layer is in place.

## Technical Details

- **Language**: Rust (2021 edition)
- **Libraries**: 
  - `dav-server`: WebDAV protocol implementation.
  - `tokio`: Async runtime.
  - `reqwest`: HTTP client with `rustls`.
  - `redb`: Embedded ACID key-value store.
  - `serde`: Serialization/Deserialization.
  - `regex`: Filename parsing and cleaning.

## Project Structure

- `src/main.rs`: Entry point — selects the debrid provider, initialises shared state, starts the WebDAV server.
- `src/provider.rs`: The `DebridProvider` trait and `FileLocator`; startup provider selection (`choose_provider`).
- `src/tasks.rs`: Background scan loop — polls the active provider, identifies new torrents, updates the VFS.
- `src/rd_client.rs`: Real-Debrid implementation of `DebridProvider` (1-hour unrestrict cache).
- `src/torbox_client.rs`: TorBox implementation of `DebridProvider` (mylist / requestdl / createtorrent / controltorrent).
- `src/ratelimit.rs`: Shared adaptive token-bucket rate limiter used by both clients.
- `src/tmdb_client.rs`: TMDB API client for media metadata.
- `src/repair.rs`: Torrent repair state machine with provider-neutral instant repair for cached content.
- `src/vfs.rs`: Virtual File System logic for library organisation.
- `src/dav_fs.rs`: WebDAV filesystem — resolves a `FileLocator` to a CDN URL via the provider; attempts instant repair when a file is unavailable.
- `src/identification.rs`: Smart media identification and filename cleaning logic.
- `src/error.rs`: Unified error type (`AppError`) using `thiserror`.
- `src/jellyfin_client.rs`: Optional Jellyfin notification client for instant library updates.
- `src/mapper.rs`: Library root (module declarations).

## How It Works

### Background Tasks

The service runs one background task:

1. **Scan Task**: Polls the active debrid provider for new/updated torrents and updates the virtual filesystem
   - Runs immediately on startup
   - Repeats every `SCAN_INTERVAL_SECS` (default: 60 seconds)
   - Builds the VFS with a `FileLocator` per file; resolution to a CDN URL happens lazily at read time

### On-Demand Repair

There is no background repair loop. Instead, repair is triggered synchronously at playback time:

- When a media file is read, `dav_fs` resolves it through the provider for a fresh CDN URL (a per-file resolution cache makes this free when the content is healthy)
- If resolution reports the file is unavailable (a 503 from Real-Debrid, or an uncached/expired file on TorBox → `AppError::Unavailable`), `try_instant_repair` runs synchronously: re-adds the torrent by hash, matches the same file by path, and checks whether the replacement is already cached
- **Cached content** (most common): repair completes in ~1-2 seconds and the replacement file is resolved inline — playback continues after a brief delay
- **Non-cached content**: the file returns an error, the old torrent is deleted, and the new torrent is left to download (the scan loop picks it up automatically)
- Non-cached/repairing torrents are hidden from WebDAV until healthy again

### Jellyfin Notifications

When `JELLYFIN_URL`, `JELLYFIN_API_KEY`, and `JELLYFIN_RCLONE_MOUNT_PATH` are all set, the service notifies Jellyfin of specific changed paths after each VFS update. This uses Jellyfin's `POST /Library/Media/Updated` API to trigger targeted scans of only the affected folders (e.g. a single season directory for a new episode), avoiding full library rescans. Changes from all sources — new torrents, deletions, repairs — are detected automatically.

### Archive-Only Torrents

Some torrents contain RAR/ZIP archives instead of video files. Debrid services do not extract these archives, so they cannot be streamed. When such a torrent is detected, a warning is logged:

```
WARN Torrent 'Movie.Name.1080p.BluRay' contains only archive files (RAR/ZIP) and cannot be streamed — replace it with a non-archive version on your debrid service
```

To fix this, delete the torrent from your debrid account and find an alternative release that contains video files directly (`.mkv`, `.mp4`, etc.).

### Error Handling

- **Unavailable file** (a 503 from Real-Debrid, or an uncached file on TorBox): Triggers synchronous instant repair — succeeds inline for cached content, fails for non-cached
- **429 Rate Limit**: Adaptive token bucket rate limiter shared across the provider's API calls — on 429, the global request interval doubles (max 2s between requests) and Retry-After headers are respected; on success, the interval gradually recovers toward the baseline of 10 req/s
- **404 Not Found**: Treated as success for delete operations (idempotent)
- **Playback Errors**: WebDAV read failures on an unavailable file trigger instant repair (rate-limited to 30s cooldown, max 3 attempts per torrent)

### Caching

- **Resolved CDN URLs**: cached to reduce API load — ~1 hour for Real-Debrid, ~3 hours for TorBox (matching each provider's link lifetime)
- **TMDB metadata**: Persisted to embedded database (`metadata.db`) indefinitely

## Licence

MIT
