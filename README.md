# DebridMovieMapper

A Rust-based service that maps your Real-Debrid torrent library to a Jellyfin/Plex-compatible WebDAV endpoint with automatic media identification via TMDB.

I created this project as:
* I was using the various arrs, and found it cumbersome, plus I ran out of storage space, meaning I needed to use a Debrid service of some kind
* Zurg didn't work for me as I needed the folder structure for Jellyfin

I use Debrid Media Manager for keeping Real Debrid populated at the moment, but am thinking of incorporating that into this service as well.

Future work:
* Add a web-ui to track progress and correct mismatches
* Automatically populate Read Debrid with watchlist content from Trakt and newly released episodes from already tracked shows

This was 100% vibe coded using a mix of Claude and Junie as further AI experimentation.
## Features

- **Media Identification**: Automatically identifies movies and TV shows using TMDB metadata based on torrent filenames.
- **Jellyfin/Plex Structure**: Organizes your library into a clean `Movies/` and `Shows/` directory structure.
- **Season Grouping**: Automatically groups TV show episodes into `Season XX` folders.
- **WebDAV Streaming**: Provides a standard WebDAV endpoint (port 8080) for direct streaming without downloading.
- **On-Demand Repair**: Detects broken links at playback time (503 from Real-Debrid) and automatically re-downloads the torrent via its magnet link without user intervention.
- **Persistent Cache**: Uses an embedded database (`redb`) to cache media identifications, reducing API calls and speeding up restarts.
- **Configurable Scan Interval**: Customizable scan interval via environment variable.
- **Robust Identification Logic**: Handles complex torrent naming conventions, including CamelCase splitting, technical metadata stripping, and multi-service fallback strategies.
- **Jellyfin Notifications**: Optionally notifies Jellyfin when content changes so new episodes and movies appear immediately without waiting for a full library scan.

## Prerequisites

- A **Real-Debrid** account and API Token.
- A **TMDB** API Key (The Movie Database).

## Configuration

The service is configured via environment variables. You can use a `.env` file in the project root:

```env
RD_API_TOKEN=your_real_debrid_token
TMDB_API_KEY=your_tmdb_api_key

# Optional
SCAN_INTERVAL_SECS=60       # How often to scan for new torrents (default: 60)

# Optional: Jellyfin integration (all three required to enable)
JELLYFIN_URL=http://jellyfin:8096
JELLYFIN_API_KEY=your_jellyfin_api_key
JELLYFIN_RCLONE_MOUNT_PATH=/media
```

### Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `RD_API_TOKEN` | Yes | - | Your Real-Debrid API token |
| `TMDB_API_KEY` | Yes | - | Your TMDB (The Movie Database) API key |
| `SCAN_INTERVAL_SECS` | No | 60 | Interval between torrent library scans (runs immediately on startup) |
| `JELLYFIN_URL` | No | - | Jellyfin server URL for library update notifications |
| `JELLYFIN_API_KEY` | No | - | Jellyfin API key for authentication |
| `JELLYFIN_RCLONE_MOUNT_PATH` | No | - | rclone mount path as seen by Jellyfin (e.g. `/media`) |

## Running with Docker

The easiest way to run DebridMovieMapper is using Docker. Pre-built multi-platform images are available via the GitHub Container Registry.

### 1. Pull the image
```bash
docker pull ghcr.io/phrontizo/debridmoviemapper:latest
```

### 2. Run the container
```bash
docker run -d \
  --name debridmoviemapper \
  -p 8080:8080 \
  -e RD_API_TOKEN=your_token \
  -e TMDB_API_KEY=your_api_key \
  -e SCAN_INTERVAL_SECS=60 \
  -v $(pwd)/metadata.db:/metadata.db \
  ghcr.io/phrontizo/debridmoviemapper:latest
```

### Building from source (Optional)
If you prefer to build the image yourself, you can use the provided `build.sh` script for multi-platform builds:
```bash
./build.sh
```
Or build locally for your current architecture:
```bash
docker build -t debridmoviemapper .
```

*Note: Mounting `metadata.db` ensures your media identification cache is preserved across container recreations.*

**Upgrading from sled (pre-redb):** If you previously ran an older version that used sled, your `metadata.db` will be a directory. Remove it before starting the new version: `rm -rf metadata.db`. The redb database will be recreated automatically (TMDB identifications will be re-fetched on the first scan).

## Docker Compose Setup with Jellyfin

The recommended way to use DebridMovieMapper with Jellyfin is via Docker Compose with rclone mounting the WebDAV endpoint.

A complete [`compose.yml`](compose.yml) file is provided in the repository that includes:
- DebridMovieMapper service
- rclone for mounting the WebDAV endpoint
- Jellyfin for media playback

### rclone.conf

Create an `rclone.conf` file in the same directory with the following content:

```ini
[debrid]
type = webdav
url = http://debridmoviemapper:8080
vendor = other
```

### Setup Steps

1. Create required files:
   ```bash
   touch metadata.db rclone.conf
   ```

2. Add your Real-Debrid and TMDB credentials to the `compose.yml` file

3. Add the rclone configuration to `rclone.conf` (see above)

4. Start the services:
   ```bash
   docker compose up -d
   ```

5. Access Jellyfin at `http://localhost:8096` and add `/media` as a library path

### Notes

- The WebDAV server runs on port 8080 and is accessible within the Docker network
- rclone mounts the WebDAV endpoint to `/mnt/debrid` with minimal caching
- Jellyfin reads from `/media` which is bind-mounted from rclone
- Files appear as `.strm` files containing Real-Debrid download URLs
- Jellyfin will stream directly from Real-Debrid when playing content

## Usage

Once running, the WebDAV server will be available at `http://localhost:8080`.

You can add this URL as a network drive in your OS or directly as a WebDAV source in media players like:
- **Infuse** (iOS/tvOS/macOS)
- **VLC**
- **Kodi**
- **Jellyfin** (via rclone mount - see Docker Compose example above)
- **Plex** (via rclone mount)

## Technical Details

- **Language**: Rust 2024
- **Libraries**: 
  - `dav-server`: WebDAV protocol implementation.
  - `tokio`: Async runtime.
  - `reqwest`: HTTP client with `rustls`.
  - `redb`: Embedded ACID key-value store.
  - `serde`: Serialization/Deserialization.
  - `regex`: Filename parsing and cleaning.

## Project Structure

- `src/main.rs`: Entry point — initialises shared state and starts the WebDAV server.
- `src/tasks.rs`: Background scan loop — polls Real-Debrid, identifies new torrents, updates the VFS.
- `src/rd_client.rs`: Real-Debrid API client with adaptive token bucket rate limiter and response caching.
- `src/tmdb_client.rs`: TMDB API client for media metadata.
- `src/repair.rs`: On-demand torrent repair state machine triggered at playback time.
- `src/vfs.rs`: Virtual File System logic for library organisation.
- `src/dav_fs.rs`: WebDAV filesystem — re-unrestricts links on read and triggers repair on 503.
- `src/identification.rs`: Smart media identification and filename cleaning logic.
- `src/error.rs`: Unified error type (`AppError`) using `thiserror`.
- `src/jellyfin_client.rs`: Optional Jellyfin notification client for instant library updates.
- `src/mapper.rs`: Library root (module declarations).

## How It Works

### Background Tasks

The service runs one background task:

1. **Scan Task**: Polls Real-Debrid for new/updated torrents and updates the virtual filesystem
   - Runs immediately on startup
   - Repeats every `SCAN_INTERVAL_SECS` (default: 60 seconds)
   - Files that fail to unrestrict at scan time are silently skipped

### On-Demand Repair

There is no background repair loop. Instead, repair is triggered at playback time:

- When a `.strm` file is read, `dav_fs` re-calls `unrestrict_link` for a fresh URL (the 1-hour response cache makes this free when the torrent is healthy)
- If `unrestrict_link` returns a 503, the torrent is marked broken and a background `tokio::spawn` calls `repair_by_id`
- Broken/repairing torrents are hidden from WebDAV until healthy again

### Jellyfin Notifications

When `JELLYFIN_URL`, `JELLYFIN_API_KEY`, and `JELLYFIN_RCLONE_MOUNT_PATH` are all set, the service notifies Jellyfin of specific changed paths after each VFS update. This uses Jellyfin's `POST /Library/Media/Updated` API to trigger targeted scans of only the affected folders (e.g. a single season directory for a new episode), avoiding full library rescans. Changes from all sources — new torrents, deletions, repairs — are detected automatically.

### Error Handling

- **503 Service Unavailable**: Immediately marks torrent as broken and triggers repair without retries
- **429 Rate Limit**: Adaptive token bucket rate limiter shared across all API calls — on 429, the global request interval doubles (max 2s between requests) and Retry-After headers are respected; on success, the interval gradually recovers toward the baseline of 10 req/s
- **404 Not Found**: Treated as success for delete operations (idempotent)
- **Playback Errors**: WebDAV read failures automatically trigger repair process

### Caching

- **Unrestrict responses**: Cached for 1 hour to reduce API load
- **TMDB metadata**: Persisted to embedded database (`metadata.db`) indefinitely

## License

MIT
