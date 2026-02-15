# DebridMovieMapper

A Rust-based service that maps your Real-Debrid torrent library to a Jellyfin/Plex-compatible WebDAV endpoint with automatic media identification via TMDB.

## Features

- **Media Identification**: Automatically identifies movies and TV shows using TMDB metadata based on torrent filenames.
- **Jellyfin/Plex Structure**: Organizes your library into a clean `Movies/` and `Shows/` directory structure.
- **Season Grouping**: Automatically groups TV show episodes into `Season XX` folders.
- **WebDAV Streaming**: Provides a standard WebDAV endpoint (port 8080) for direct streaming without downloading.
- **High-Performance Proxy**: Optimized streaming proxy with read-ahead buffering, retry logic, and concurrency control to handle transient network issues and Real-Debrid API quirks.
- **Automatic Torrent Repair**: Background health checking and automatic repair of broken torrents with configurable scheduling.
- **Smart Error Handling**: Detects 503 errors during playback and automatically triggers repair without user intervention.
- **Persistent Cache**: Uses an embedded database (`sled`) to cache media identifications and unrestrict responses, reducing API calls and speeding up restarts.
- **Configurable Scheduling**: Customizable scan and repair intervals via environment variables.
- **Robust Identification Logic**: Handles complex torrent naming conventions, including CamelCase splitting, technical metadata stripping, and multi-service fallback strategies.

## Prerequisites

- A **Real-Debrid** account and API Token.
- A **TMDB** API Key (The Movie Database).

## Configuration

The service is configured via environment variables. You can use a `.env` file in the project root:

```env
RD_API_TOKEN=your_real_debrid_token
TMDB_API_KEY=your_tmdb_api_key

# Optional: Configure background task intervals (in seconds)
SCAN_INTERVAL_SECS=60       # How often to scan for new torrents (default: 60)
REPAIR_INTERVAL_SECS=3600   # How often to check and repair broken torrents (default: 3600)
```

### Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `RD_API_TOKEN` | Yes | - | Your Real-Debrid API token |
| `TMDB_API_KEY` | Yes | - | Your TMDB (The Movie Database) API key |
| `SCAN_INTERVAL_SECS` | No | 60 | Interval between torrent library scans (runs immediately on startup) |
| `REPAIR_INTERVAL_SECS` | No | 3600 | Interval between automatic repair cycles (waits before first run) |

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
  -e REPAIR_INTERVAL_SECS=3600 \
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

## Usage

Once running, the WebDAV server will be available at `http://localhost:8080`. 

You can add this URL as a network drive in your OS or directly as a WebDAV source in media players like:
- **Infuse** (iOS/tvOS/macOS)
- **VLC**
- **Kodi**
- **Jellyfin** (via Rclone mount)
- **Plex** (via Rclone mount)

## Technical Details

- **Language**: Rust 2024
- **Libraries**: 
  - `dav-server`: WebDAV protocol implementation.
  - `tokio`: Async runtime.
  - `reqwest`: HTTP client with `rustls`.
  - `sled`: Embedded key-value store.
  - `serde`: Serialization/Deserialization.
  - `regex`: Filename parsing and cleaning.

## Project Structure

- `src/main.rs`: Entry point, background scan and repair tasks with configurable scheduling.
- `src/rd_client.rs`: Real-Debrid API client with exponential backoff, rate limiting, and response caching.
- `src/tmdb_client.rs`: TMDB API client for media metadata.
- `src/repair.rs`: Automatic torrent health checking and repair system.
- `src/vfs.rs`: Virtual File System logic for library organization.
- `src/dav_fs.rs`: WebDAV filesystem implementation with streaming proxy and error detection.
- `src/identification.rs`: Smart media identification and filename cleaning logic.
- `src/mapper.rs`: Library root and shared utilities.

## How It Works

### Background Tasks

The service runs two independent background tasks:

1. **Scan Task**: Polls Real-Debrid for new/updated torrents and updates the virtual filesystem
   - Runs immediately on startup
   - Repeats every `SCAN_INTERVAL_SECS` (default: 60 seconds)

2. **Repair Task**: Monitors torrent health and automatically repairs broken torrents
   - Waits `REPAIR_INTERVAL_SECS` before first run (default: 1 hour)
   - Repeats every `REPAIR_INTERVAL_SECS`
   - Checks all links in each torrent for availability
   - Automatically re-downloads broken torrents via magnet links
   - Hides broken/repairing torrents from WebDAV until healthy

### Error Handling

- **503 Service Unavailable**: Immediately marks torrent as broken and triggers repair without retries
- **429 Rate Limit**: Exponential backoff with Retry-After header support (2s, 4s, 8s, 16s, 32s)
- **404 Not Found**: Treated as success for delete operations (idempotent)
- **Playback Errors**: WebDAV read failures automatically trigger repair process

### Caching

- **Unrestrict responses**: Cached for 1 hour to reduce API load
- **TMDB metadata**: Persisted to embedded database (`metadata.db`) indefinitely

## License

MIT
