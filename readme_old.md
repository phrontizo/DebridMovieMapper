# DebridMovieMapper

A Rust-based service that maps your Real-Debrid torrent library to a Jellyfin/Plex-compatible WebDAV endpoint with automatic media identification via TMDB.

## Features

- **Media Identification**: Automatically identifies movies and TV shows using TMDB metadata based on torrent filenames.
- **Jellyfin/Plex Structure**: Organizes your library into a clean `Movies/` and `Shows/` directory structure.
- **Season Grouping**: Automatically groups TV show episodes into `Season XX` folders.
- **WebDAV Streaming**: Provides a standard WebDAV endpoint (port 8080) for direct streaming without downloading.
- **High-Performance Proxy**: Optimized streaming proxy with read-ahead buffering, retry logic, and concurrency control to handle transient network issues and Real-Debrid API quirks.
- **Persistent Cache**: Uses an embedded database (`sled`) to cache media identifications, reducing API calls and speeding up restarts.
- **Real-Time Updates**: Background task polls Real-Debrid every 60 seconds to detect and map new torrents.
- **Robust Identification Logic**: Handles complex torrent naming conventions, including CamelCase splitting, technical metadata stripping, and multi-service fallback strategies.

## Prerequisites

- A **Real-Debrid** account and API Token.
- A **TMDB** API Key (The Movie Database).

## Configuration

The service is configured via environment variables. You can use a `.env` file in the project root:

```env
RD_API_TOKEN=your_real_debrid_token
TMDB_API_KEY=your_tmdb_api_key
```

## Running with Docker

The easiest way to run DebridMovieMapper is using Docker.

### 1. Build the image
```bash
docker build -t debridmoviemapper .
```

### 2. Run the container
```bash
docker run -d \
  --name debridmoviemapper \
  -p 8080:8080 \
  -e RD_API_TOKEN=your_token \
  -e TMDB_API_KEY=your_api_key \
  -v $(pwd)/metadata.db:/metadata.db \
  debridmoviemapper
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

- `src/main.rs`: Entry point and background refresh loop.
- `src/rd_client.rs`: Real-Debrid API client.
- `src/tmdb_client.rs`: TMDB API client.
- `src/vfs.rs`: Virtual File System logic for library organization.
- `src/dav_fs.rs`: WebDAV filesystem implementation and streaming proxy.
- `src/identification.rs`: Smart media identification and filename cleaning logic.
- `src/mapper.rs`: Library root and shared utilities.

## License

MIT
