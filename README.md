# DebridMovieMapper

A Rust-based service that maps your debrid torrent library — **Real-Debrid or TorBox** — to a Jellyfin/Plex-compatible WebDAV endpoint with automatic media identification via TMDB.

I created this project as:
* I was using the various arrs, and found it cumbersome, plus I ran out of storage space, meaning I needed to use a Debrid service of some kind
* Zurg didn't work for me as I needed the folder structure for Jellyfin

I previously used Debrid Media Manager to keep Real-Debrid populated; that acquisition role is now built into this service (see the [Trakt Integration](#trakt-integration) section below).

Note that this is still a work in progress and is provided as-is for educational purposes only.

Future work:
* Add a web-ui to track progress and correct mismatches

Trakt-driven acquisition (populate your debrid account from Trakt watchlists/in-progress and newly aired episodes of tracked shows, plus automatic lifecycle removal) is now built in — see the [Trakt Integration](#trakt-integration) section below.

This was 100% vibe coded using a mix of Claude and Junie as further AI experimentation.
## Features

- **Media Identification**: Automatically identifies movies and TV shows using TMDB metadata based on torrent filenames.
- **Jellyfin/Plex Structure**: Organizes your library into a clean `Movies/` and `Shows/` directory structure.
- **Season Grouping**: Automatically groups TV show episodes into `Season XX` folders.
- **Real-Debrid *and* TorBox**: One codebase, either provider. Set `RD_API_TOKEN` for Real-Debrid or `TORBOX_API_KEY` for TorBox (exactly one) — everything else works the same.
- **WebDAV Endpoint**: Exposes a WebDAV server (port 8080) serving proxied media files with real file sizes and extensions. Media bytes are fetched on demand from the provider's CDN. Mount via rclone for use with Jellyfin/Plex.
- **On-Demand Repair**: Detects unavailable files at playback time (a 5xx or transport-level failure from Real-Debrid on unrestrict, an uncached/expired file on TorBox, or a persistent CDN 5xx on the byte fetch) and attempts instant synchronous repair by re-adding the torrent. For cached content, playback continues after a ~1-2s delay; otherwise a fresh download is started automatically.
- **Trakt Integration (optional)**: Drive the library from your Trakt account(s) — watchlisted and in-progress movies/shows are auto-acquired, new episodes of tracked shows are picked up as they air, and content is automatically removed once everyone who wanted it has finished (or abandoned) it. Link one or more accounts via a local-network enrolment page. Disabled by default; when not configured the service runs exactly as before.
- **Persistent Cache**: Uses an embedded database (`redb`) to cache media identifications, reducing API calls and speeding up restarts.
- **Configurable Scan Interval**: Customizable scan interval via environment variable.
- **Robust Identification Logic**: Handles complex torrent naming conventions, including CamelCase splitting, technical metadata stripping, and multi-strategy TMDB fallbacks (separate movie/TV search, year-less retry).
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
RUST_LOG=info               # Log verbosity (default: info). e.g. RUST_LOG=debridmoviemapper=debug

# Optional: Jellyfin integration (all three required to enable)
JELLYFIN_URL=http://jellyfin:8096
JELLYFIN_API_KEY=your_jellyfin_api_key
JELLYFIN_RCLONE_MOUNT_PATH=/media

# Optional: Trakt integration (both client id + secret required to enable — see Trakt Integration section)
# TRAKT_CLIENT_ID=your_trakt_client_id
# TRAKT_CLIENT_SECRET=your_trakt_client_secret
# TRAKT_SYNC_INTERVAL_SECS=900            # Trakt sync + reconcile cadence (default: 900, minimum: 60)
# TRAKT_EPISODE_CHECK_INTERVAL_SECS=3600  # episode-monitor cadence (default: 3600, minimum: 300)
# TRAKT_CATCHUP_LOOKBACK=                  # catch-up window for watched shows: 90d/12w/6mo; unset=all-time

# Optional: auto-upgrade engine (SP3 — on by default; set UPGRADE_INTERVAL_SECS=0 to disable)
# UPGRADE_INTERVAL_SECS=86400    # how often to run the quality-upgrade + consolidation pass (default: daily; 0=off)
# UPGRADE_BUDGET_PER_TICK=       # cap on titles re-scored per pass (default: unset = the whole library)
# UPGRADE_IDLE_SECS=300          # seconds without a read before a copy may be swapped or removed (default: 300)
# ACQUIRE_DEAD_TIMEOUT_SECS=600  # seconds before an unresolved Pending torrent is reaped + re-scraped (default: 600)
# DEDUP_REMOVE_DUPLICATES=false  # actually delete redundant duplicate torrents (default: false = dry-run / log only)
# REMOVE_FINISHED_SHOWS=false    # auto-remove fully-watched ended (non-watchlisted) shows (default: false = preview / log only)
# ALLOW_RESOLUTION_DOWNGRADE=false  # correct above-ceiling owned copies to a within-ceiling cached release (default: false = preview / log only)

# Optional: acquisition preferences (see Acquisition section below)
# SCRAPER_ADDON_URL=https://torrentio.strem.fun/realdebrid=TOKEN   # override scraper URL
# SCRAPER_PROXY_URL=http://user:pass@proxy:8080   # proxy the scraper (Torrentio) traffic only
# SCRAPER_MIN_INTERVAL_MS=1000   # minimum spacing between scraper requests (backs off further on 429)
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
| `DB_PATH`                    | No       | `metadata.db`  | Path to the redb database file. If the file is genuinely corrupt/unreadable (bad header, truncated, redb repair aborted), it is automatically moved aside to `<DB_PATH>.corrupt` (or `<DB_PATH>.corrupt.<n>` if that name already exists) and recreated — upgrades never crash-loop. A DB written by a **newer** build, an **older** redb on-disk format, or a transient/operational error (permission/lock/disk) instead **fails startup**, leaving the intact file in place rather than discarding its non-regenerable authoritative tables. |
| `PORT`                       | No       | 8080           | WebDAV server listen port                                            |
| `CDN_FIRST_READ_MB`          | No       | 8              | Size (MB) of the FIRST read-ahead fetch when a media file is opened — front-loads the container header + early index so a player's open/ffprobe burst needs fewer cold CDN round-trips before playback starts. Later reads use the normal 2 MB window. Clamped to 2–16 MB; set `2` to disable the enlargement. |
| `RUST_LOG`                   | No       | `info`         | Log verbosity (`tracing-subscriber` `EnvFilter` directive). Scope it to avoid dependency noise, e.g. `debridmoviemapper=debug`, or target a module: `debridmoviemapper::acquire=debug,info`. A malformed value falls back to `info`. Takes effect on (re)start. |
| `JELLYFIN_URL`               | No       | -              | Jellyfin server URL for library update notifications                 |
| `JELLYFIN_API_KEY`           | No       | -              | Jellyfin API key for authentication                                  |
| `JELLYFIN_RCLONE_MOUNT_PATH` | No       | -              | rclone mount path as seen by Jellyfin (e.g. `/media`)                |
| `SCRAPER_ADDON_URL`          | No       | *(auto)*       | Override the Torrentio scraper base URL. Defaults to a URL auto-built from your provider token (`https://torrentio.strem.fun/<provider>=<token>`). |
| `SCRAPER_PROXY_URL`          | No       | -              | Route the **scraper's** requests (Torrentio / custom addon) through an HTTP(S) proxy — e.g. `http://[user:pass@]host:port`. Applies **only** to scraper traffic; the debrid CDN media reads, TMDB, and provider APIs stay direct. A set-but-invalid value (non-http(s) scheme; socks is not supported) is a startup error rather than a silent direct fallback. |
| `SCRAPER_MIN_INTERVAL_MS`    | No       | `1000`         | Minimum spacing between scraper requests, in milliseconds (clamped 100–60000). Torrentio publishes no rate limit and starts answering `429 Too Many Requests` when a burst of scrapes (e.g. the hourly episode check) arrives at once, so requests are paced; a `429` slows the pace further (honouring `Retry-After`) and the scrape is retried up to 3 times. Lower it if you run your own addon. |
| `MAX_RESOLUTION`             | No       | `1080`         | Hard resolution ceiling for acquisition: `720`, `1080`, `2160` (each also accepted with a `p` suffix, e.g. `1080p`), or the `2160` synonyms `4k` / `uhd` (case-insensitive). Candidates above this height are excluded. |
| `AUDIO_LANGUAGE`             | No       | `original`     | Required audio language for acquisition: a 2- or 3-letter ISO code (e.g. `eng` or `en`) or `original` (uses the title's languages from TMDB: its original language **plus** its listed spoken languages — a release in any of them is accepted, so a film whose TMDB original language isn't its main audio, e.g. a `fi`-original, mostly-English film, isn't rejected for correct English releases; a title with no spoken languages listed uses the original language alone). An unrecognised value (a full name like `english`, a locale like `pt-BR`, a typo) is rejected back to `original` with a warning rather than taken verbatim (which would silently penalise every candidate). |
| `SUBTITLE_LANGUAGE`          | No       | *(none)*       | Required subtitle language for acquisition: a 2- or 3-letter ISO code, or omit / set to `none` to skip the check. An unrecognised value is rejected back to `none` (skip) with a warning. |
| `PREFER_HEVC`                | No       | `true`         | Prefer HEVC/H.265 encodes when scoring acquisition candidates. |
| `PREFER_HDR`                 | No       | `false`        | Prefer HDR/Dolby Vision encodes when scoring acquisition candidates. |
| `STALL_TIMEOUT_SECS`         | No       | `1800` (min 60) | Seconds without download progress before a Pending torrent is considered stalled and re-acquired. |
| `MAX_ACQUIRE_ATTEMPTS`       | No       | `5` (min 1)    | Maximum number of candidates to try before giving up on a title. |
| `TRAKT_CLIENT_ID`            | No†      | -              | Trakt API app client id. Both this and `TRAKT_CLIENT_SECRET` are required to enable Trakt sync. |
| `TRAKT_CLIENT_SECRET`        | No†      | -              | Trakt API app client secret. |
| `TRAKT_SYNC_INTERVAL_SECS`   | No       | `900`          | How often (seconds) to sync each enrolled Trakt account and reconcile the library (minimum: 60). |
| `TRAKT_EPISODE_CHECK_INTERVAL_SECS` | No | `3600`      | How often (seconds) to check tracked shows for newly aired episodes (minimum: 300). |
| `TRAKT_CATCHUP_LOOKBACK`     | No       | all-time       | Catch-up window: a show you've *watched* is auto-acquired to catch up on episodes aired since, if you last watched it within this window. `90d` / `12w` / `6mo` (or a bare number = days); `0`/unset = all-time. Anchored on Trakt's per-show `last_watched_at`. Only shows Trakt reports as **behind** (≥1 unwatched aired episode) are caught up — fully-watched shows are skipped. Note: each catch-up candidate costs one Trakt + one TMDB call per sync cycle, so an all-time window over a large watch history adds that many requests each cycle (rate-limited, so absorbed); set a bounded lookback to scope it. This window also scopes `REMOVE_FINISHED_SHOWS` finish-cleanup (same per-show progress check). |
| `UPGRADE_INTERVAL_SECS`      | No       | `86400`        | How often (seconds) to run the quality-upgrade + full-season consolidation pass. `0` disables the upgrade engine entirely. Default is daily (86 400 s); minimum 600 when non-zero. |
| `UPGRADE_BUDGET_PER_TICK`    | No       | unset          | Cap on the owned titles re-scored per upgrade pass (minimum: 1). Unset, every pass covers the whole library; a pass over ~300 titles makes roughly 500 Torrentio requests (about 8 minutes at the default `SCRAPER_MIN_INTERVAL_MS`). With a cap, titles holding a copy of unknown or unchecked quality get half of it and the rest rotates through the library, least recently checked first. |
| `UPGRADE_IDLE_SECS`          | No       | `300`          | Seconds without a proxy read before a copy may be swapped, pruned or removed (minimum: 30). Prevents swapping or deleting a file that is being streamed. The upgrade engine waits only for the copies a swap changes, so a film or show being watched is skipped for that pass while the rest of the library is still upgraded; duplicate-dedup removals and Trakt lifecycle removals wait until nothing in the library has been read for this long. Read activity is tracked in memory only, so for the **first `UPGRADE_IDLE_SECS` after the service starts** the library counts as **not idle** (a start-up grace): nothing is deleted on the first ticks after a restart, giving a player that was streaming time to reconnect. The first upgrade pass after a start therefore waits until the grace is over (it then runs every `UPGRADE_INTERVAL_SECS` as usual), so restarting the service never postpones upgrades by a whole interval. |
| `UPGRADE_STAGE_MAX_SECS`     | No       | `604800`       | Reserved/informational: maximum age of a staged upgrade before it is abandoned (minimum: 3600). |
| `ACQUIRE_DEAD_TIMEOUT_SECS`  | No       | `600`          | Seconds an optimistically-added `Pending` torrent may remain unresolved before `observe` reaps it as dead, blacklists its hash, and re-scrapes (minimum: 120). |
| `DEDUP_REMOVE_DUPLICATES`    | No       | `false`        | When `true`, the duplicate-dedup pass **deletes** redundant torrents (a title covered by more than one torrent → keep the best, remove the rest, only ever removing a torrent whose every episode is also held by a kept torrent of the **same or better quality** — a better copy is never removed because a worse one covers it). Quality decides first — resolution, then source tier, then a checked copy before one whose tracks couldn't be checked; an untagged resolution or source counts as the lowest, but a known resolution is never traded down (a 1080p copy with an untagged source beats a 720p one) — then which copy is being played; copies already in your account before the service found them get no special treatment. A copy the service is still downloading or checking is left alone until it settles, and never counts against a playable copy. Default `false` = dry-run (logs the plan only). Idle-gated like upgrades (including the start-up grace), so it never interrupts an active stream. |
| `REMOVE_FINISHED_SHOWS`      | No       | `false`        | When `true`, a fully-watched **Ended** show that isn't on your watchlist is auto-removed once finished — **including shows already in your library**. Default `false` only logs the candidates (a preview) without removing. Watchlisted shows are never finish-removed. Note: finish-cleanup reuses the `TRAKT_CATCHUP_LOOKBACK` per-show progress check, so a bounded lookback also scopes it — a show last watched outside that window is neither previewed nor removed. |
| `ALLOW_RESOLUTION_DOWNGRADE` | No       | `false`        | When `true`, the upgrade engine performs a **corrective downgrade**: when `MAX_RESOLUTION` is lowered below an owned copy's resolution, it replaces that above-ceiling copy with a ceiling-conforming **cached** release and prunes the higher-res copy — idle-gated, and only when such a replacement exists (a sole over-ceiling copy with no conforming cached release is kept). Default `false` only logs the planned downgrades (a preview) without replacing. Pure quality upgrades are unaffected by this flag. Applies to copies the service acquired and to ones already in your account alike: a file whose name shows a resolution above the ceiling is always recognised as such (never treated as unknown quality), so it is only ever replaced through this option. |

\* Exactly one of `RD_API_TOKEN` / `TORBOX_API_KEY` must be set — not both, and not neither.

† Trakt sync is enabled only when **both** `TRAKT_CLIENT_ID` and `TRAKT_CLIENT_SECRET` are set. If either is absent, Trakt sync is disabled and the service runs exactly as before (account-mirror + on-demand repair only).

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
- `:latest` — latest stable release (updated on version tags like `v1.0.7`)
- `:edge` — latest build from `main` branch (may be unstable)
- `:v2edge` — latest build from the `v2` development branch (may be unstable; CI publishes a per-branch rolling tag)
- `:1.0.7`, `:1.0`, `:1` — pinned to a specific release version

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

Multi-platform images are built and pushed automatically by GitHub Actions. Release tags (e.g. `git tag v1.0.7 && git push origin v1.0.7`) update `:latest` and semver tags; pushes to `main` update `:edge`, and pushes to the `v2` development branch update `:v2edge`.

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

- The WebDAV server runs on port 8080 (bound to `127.0.0.1:8080` on the host by the compose file, so it is reachable only from the local machine; adjust the port mapping if a trusted LAN needs access)
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
- **Kodi** (expected to work via the rclone mount; untested)
- **Infuse** (iOS/tvOS/macOS)

## Acquisition

The acquisition engine lets the service find and add content to your debrid account automatically, using Torrentio as a scraper. It is driven by [Trakt Integration](#trakt-integration) (and the library reconciler) — there is no manual trigger.

### How Acquisition Works

Acquisition uses an **optimistic-add** model (SP3): the engine adds the best candidate and records it `Pending` immediately, then resolves it asynchronously so slow-to-seed torrents are not judged prematurely.

1. **Scrape**: Given an IMDB id and media type, `TorrentioScraper` queries a Stremio-compatible addon (default: Torrentio, URL auto-built from your provider token; override with `SCRAPER_ADDON_URL`) for candidate torrents. The IMDB id is taken from Trakt directly when available (Trakt includes it in every item) and only re-derived from TMDB when Trakt didn't supply one — so a title that TMDB has no IMDB id for (e.g. a TVDB-only show) is still acquirable. A title with no IMDB id anywhere can't be scraped (Torrentio is IMDB-keyed) and is skipped quietly.
2. **Score and rank**: Candidates are parsed for resolution, codec, HDR, file size, seeder count, and cached status. A hard ceiling at `MAX_RESOLUTION` excludes anything above it. Remaining candidates are ranked by a weighted **score**: cached availability dominates, then **resolution** (within the `MAX_RESOLUTION` ceiling — resolution outweighs source tier, so a higher-resolution release wins), then source tier (REMUX>BluRay>WEB>HDTV). On top of those, progressively smaller **additive** bonuses — HEVC and HDR encodes, a verifiable container, bitrate, and seeder count — are folded into the same weighted sum rather than applied as a strict tie-break chain, so (for example) the HEVC bonus can outweigh a one-step source-tier difference. Uncached zero-seeder releases are hard-filtered (undownloadable). A release whose name or Torrentio flags (e.g. `Rus`, `ITA`, `VFF`, 🇷🇺) tag only audio languages other than the required `AUDIO_LANGUAGE` (for `original`: any of the title's TMDB original/spoken languages) is heavily down-ranked — a soft penalty, not an exclusion, since a flag can also name a subtitle language; the file probe is the real check. Subtitle-only markers (`VOSTFR`, `Multi Subs`) never count as an audio language, and neither does a language written next to a subtitle marker (`[ENG SUB]`, `Eng.Subs`, `SUB.ITA`). A title that itself contains a language word (*The Italian Job*, *The Danish Girl*) is tagged with it on every release — a known limitation, shared by all of that title's releases.
3. **Optimistic add**: The top non-blacklisted candidate is added to your debrid account by hash, recorded as `Pending`, and control returns immediately — no synchronous wait.
4. **Asynchronous resolve** (`observe`, runs every scan tick): once the torrent's files appear, the deferred gates run: pack-guard (multi-feature movie packs are rejected) → strict title validation (the file name must identify to the requested TMDB id; mismatches are blacklisted) → file probe (for cached content, the first 4 MB is checked for required audio/subtitle languages — a non-faststart MP4, whose track index (`moov`) sits at the end of the file, is read at that index's exact offset; corrupt/wrong-language probes are blacklisted). On pass — marked `Verified`, `provides` (which episodes the hash covers) and a quality snapshot are recorded, and the `selection` entry is written. A release whose tracks can't be read at all (e.g. an `.avi`, or a file carrying no track metadata) is still accepted when nothing better was found, but recorded as *unchecked* so the upgrade pass replaces it with a checked copy when one exists. On fail (corrupt/wrong-language) — hash is blacklisted and the next candidate is tried. If the probe can't even fetch (the CDN is momentarily unavailable, returning a *transient* result), the release stays `Pending` and is re-probed — every tick for a short burst, then hourly — but is **never accepted unverified**, so a wrong-language release can't slip in unchecked; if it stays unfetchable for 24 h it is treated as broken and replaced.
5. **Dead-torrent reaping**: if a `Pending` torrent remains unresolved beyond `ACQUIRE_DEAD_TIMEOUT_SECS` (default 600 s), or shows a terminal provider error, `observe` reaps it, blacklists the hash, and re-scrapes for the next-best candidate.
6. **Stall detection**: no progress for `STALL_TIMEOUT_SECS` triggers re-acquisition via the next unblacklisted candidate.
7. **Outcome**: `Acquired` (instantly cached + verified), `Pending` (resolving asynchronously), `NoAcceptableRelease`, or `TemporarilyUnavailable` (scraper unreachable — retry later).

### What triggers acquisition

Acquisition is triggered by [Trakt Integration](#trakt-integration): the Trakt reconciler scans each enrolled account's watchlist and in-progress titles and asks the engine to acquire anything missing, while the episode monitor picks up newly aired episodes of tracked shows. (An earlier development build had a temporary `--acquire` command-line trigger; it has been removed.)

## Trakt Integration

When configured, DebridMovieMapper keeps your library in sync with one or more Trakt accounts:

- **Watchlist + in-progress** movies and shows are automatically acquired into your debrid account.
- **New episodes** of tracked shows are picked up as they air (using TMDB air dates).
- **Automatic removal** (the Trakt lifecycle): engine-acquired content is removed once *every* user who wanted it has finished it (a watched movie; a fully-watched ended show — every aired regular episode watched; specials are ignored, since Trakt doesn't report them), or once a watchlisted title is un-watchlisted and nobody else wants it. Un-watchlisting (Trigger B) never deletes content you added directly to your debrid account — that removal is scoped to the engine-acquired copies, so a pre-existing season pack survives even if a watchlist acquired later episodes of the same show. Such pre-existing content is, however, treated as owned: the daily upgrade engine may replace a copy with a better cached release (pruning the superseded one); the duplicate-dedup pass may remove redundant duplicate copies when `DEDUP_REMOVE_DUPLICATES=true` — such a copy gets no special protection there and is kept or removed purely on quality (if it loses to a better copy the service acquired, the title then lives on in that copy, which the lifecycle above may later remove); and a fully-watched **Ended** show is finish-removed **including ones already in your library** when `REMOVE_FINISHED_SHOWS=true` (see that option).

It is a single shared household library: acquisition is the union across all linked accounts, and removal is per-user-aware (a title is only removed when no linked account still wants it).

Trakt paginates the watchlist and watched-history endpoints (since its 2026 API changes), so every read follows all pages, and watched shows are read with their per-episode progress — that is what lets finish-removal and catch-up tell which episodes you have actually seen. If any page of a read fails (or the list changes while it is being read), that account's sync fails for the cycle like any other Trakt read failure (it is flagged as *needs re-enrolment* on the accounts page until the next successful sync) and its previous wanted titles are kept — a partial read is never acted on, so nothing is removed because a page was missed.

**Trakt sync is disabled by default.** When `TRAKT_CLIENT_ID`/`TRAKT_CLIENT_SECRET` are not set, the service behaves exactly as before (account-mirror + on-demand repair only).

### Enabling Trakt sync

1. Create a Trakt API application at <https://trakt.tv/oauth/applications> (any redirect URI is fine — the service uses device-flow OAuth). Note its **Client ID** and **Client Secret**.
2. Set the environment variables:

   ```env
   TRAKT_CLIENT_ID=your_trakt_client_id
   TRAKT_CLIENT_SECRET=your_trakt_client_secret
   # Optional cadences:
   TRAKT_SYNC_INTERVAL_SECS=900            # Trakt sync + reconcile (default: 900, minimum: 60)
   TRAKT_EPISODE_CHECK_INTERVAL_SECS=3600  # newly-aired-episode check (default: 3600, minimum: 300)
   ```

3. Restart the service. On startup it logs that the Trakt enrolment page is available.

### Linking accounts

Visit `/trakt/accounts` on the server (e.g. `http://localhost:8080/trakt/accounts`) to link, refresh, or remove Trakt accounts. Linking uses Trakt's **device flow**: click *Enrol a new account*, then open the shown URL on any device and enter the code to approve it. Once approved, the account is linked automatically and its tokens are stored (keyed by the Trakt username slug).

> ⚠️ The enrolment page is **unauthenticated**, just like the WebDAV endpoint — it assumes the port is only reachable on a trusted local network. Do not expose it to an untrusted network. If a linked account later needs re-authorising (e.g. its token was revoked), it is flagged on the accounts page as *needs re-enrolment*.

### Validating Trakt parsing against a live account

An interactive integration test (`tests/trakt_integration_test.rs`) exercises every Trakt parser and `sync_trakt` end-to-end against your real Trakt account — no static token needed. Run:

```bash
cargo test --test trakt_integration_test -- --ignored --nocapture
```

The test prints a verification URL and a short code, then waits for you to open the URL and approve it at trakt.tv/activate. Once approved it reads your watchlist, in-progress and watched history (every page), checks that watched shows carry their per-episode watched data, and runs the full `sync_trakt` path, asserting the `wanted` set is populated. Requires `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET`, and `TMDB_API_KEY` (in `.env` or environment); it skips cleanly if any is absent. Tokens are minted on demand via the device-flow and stored only in a temporary file that the test cleans up afterwards.

## Auto-upgrades & consolidation (SP3)

The upgrade engine runs a quality-improvement pass over your library **once a day by default** (controlled by `UPGRADE_INTERVAL_SECS`, default 86 400 s). It is on by default for any deployment with engine-acquired titles; set `UPGRADE_INTERVAL_SECS=0` to disable it entirely.

Each pass covers every owned title (`UPGRADE_BUDGET_PER_TICK` can cap it). Titles holding a copy of unknown or unchecked quality (see below) go first, so a pass cut short by someone starting to watch has already handled them:

- **The best copy you already have is the one served:** when your account holds more than one copy of an episode or film and the library is showing a worse one, it switches to the better copy (higher resolution, then source; a copy whose tracks could be checked beats one that couldn't) on the next scan, once nothing is playing — e.g. REMUX seasons you add yourself replace the BluRay pack's files for those seasons, while the pack keeps serving the seasons only it has. This doesn't wait for the daily pass, deletes nothing, and never switches between copies of equal quality. (Jellyfin sees the switched episodes as new files.)
- **Quality upgrades (movies, and shows per-episode):** re-scrapes the title (or, for a show, each owned episode individually) with Torrentio and checks whether a meaningfully-better **cached** release exists, trying ranked candidates in order until one is usable (so a structurally-unusable top candidate — e.g. a mislabeled pack — no longer blocks the upgrade). "Meaningfully-better" follows the engine's own quality ranking (resolution dominant within the `MAX_RESOLUTION` ceiling, then source tier): the candidate must score strictly higher **and** make a concrete category jump — uncached→cached, a higher resolution, or a higher source tier at the same resolution (e.g. WEB→BluRay/REMUX). Marginal same-tier-same-resolution differences are never acted on (avoids churn), a lower source tier at the same resolution never counts even when it scores higher (e.g. a WEB x265 release is not taken over a BluRay x264 one for its HEVC bonus), and a higher source tier is **not** chased if it would cost resolution. A candidate must also beat the owned copy when scored the way acquisition ranks it — with the wrong-language penalty for a release whose name or Torrentio flags tag only other audio languages than `AUDIO_LANGUAGE` (for `original`: the title's TMDB original/spoken languages — e.g. `Rus` or 🇷🇺 on an English-original show) — so the upgrade engine never swaps to a release acquisition itself would rank lower (which also rules out any flip-flop); a copy of unknown quality (below) is never replaced by a release carrying that penalty either. Candidates are tried in that same language-aware order, so a release tagged with another language is tried only after the correctly-tagged ones. A staged candidate goes through the same file probe as acquisition, with the same audio rule — but an upgrade only adopts a release whose tracks the probe could actually **check**: a candidate whose tracks can't be read is dropped (not blacklisted), and one whose reported file is a container the probe can't read (e.g. `.avi`) isn't even staged, so an upgrade never swaps a checked copy for an unchecked one. Conversely, an owned copy recorded as *unchecked* (accepted without a track check, or found in your account as a non-MKV/MP4 file) is replaced by a checked cached release of the **same or better** quality at the same or higher resolution, no category jump needed. **Unknown quality counts as the lowest quality:** a copy whose quality isn't known — its name doesn't tag the resolution or the source (e.g. `The Matrix (1999).mkv`, `Show.S01E01.1080p.mkv`), or no quality was ever recorded for it — is replaced by any cached release of **known** quality within the ceiling that passes the probe and isn't tagged only with other audio languages (the best-ranked one is tried first) — but never by a lower resolution than the copy's name does show, so `Show.S01E01.1080p.mkv` is only replaced by a 1080p-or-better release — and is never "replaced" by another unknown one; once replaced, the new copy's quality is known, so the next run leaves it alone. A copy whose file name shows a resolution **above** `MAX_RESOLUTION` is never treated as unknown — it is only ever replaced by the opt-in corrective downgrade below. (Quality is read from the file name, with anything it leaves out filled from the torrent name — `Show (S02) (1080p) (WebDL)` holding `S02E01.mkv` counts as 1080p WEB. Copies found in your account by an older version, recorded without a quality, have it worked out once this way.) When upgrading from an older version, the database migration flags once the copies the engine acquired and serves from a non-MKV/MP4 file; copies found in your account that an older version had already recorded are **not** flagged retroactively — only ones found from this version on. A title owned from before the `original` = original-plus-spoken-languages rule, or found already in your debrid account, has its TMDB languages fetched once — on its first upgrade visit, before anything is probed, for every owned copy and episode of the title (a copy found in your account also gets its TMDB original language, which it never had). If TMDB is temporarily unreachable the title is skipped until the next upgrade run rather than checked against the original language alone; if TMDB refuses the lookup outright (e.g. the title no longer exists there), the languages already on record are used. When a better release is found it is staged (added to the debrid account — a release whose torrent is already in your account, e.g. under another title or added by hand, is never staged, so the upgrade pass can't delete it when it rejects it) and the VFS `selection` for the title (or episode) is swapped — but only once the **library has been idle for `UPGRADE_IDLE_SECS`** (default 5 minutes without a proxy read anywhere in the library), re-checked immediately before the swap so staging latency can't catch a stream that started meanwhile. The superseded torrent is then pruned. A show's owned singleton episodes are re-scored before that season's pack handling runs, so an episode upgraded this tick can still be folded into a pack later the same tick. A single episode is compared against the better of its own quality and the copy that episode currently plays from (e.g. a better season pack you also have), so an upgrade never moves an episode off a better pack.
- **Episodes served from a multi-episode pack (shows):** an episode the library currently plays from a multi-episode pack (e.g. an SD foreign-dub `S01E01-E04` pack grabbed on an air date while the season's other episodes are 1080p singles) is re-scored against the pack's quality and, when a meaningfully-better **cached single-episode** release exists (or the pack is *unchecked* and a checked one of the same or better quality exists), moved to it — staged, probed and idle-gated exactly like a per-episode upgrade. The single must also be at least the pack's source tier **and** resolution — the same test full-season consolidation uses (so e.g. a 720p BluRay pack is not split into 1080p WEB singles). Because an upgrade never lowers a checked copy's source tier at the same resolution — and every copy the upgrade pass adopts is checked — the passes never undo each other: singles split from a pack are never swapped down to ones consolidation would fold back into that pack (e.g. a 1080p WEB x264 pack split into 1080p BluRay x264 singles stays split — the singles aren't traded for higher-scoring WEB x265 ones). The pack itself is deleted only when it no longer serves any episode **and** every episode it contains is held by another settled copy that is still **present** on your debrid account (a copy whose torrent has lapsed off the provider doesn't count); otherwise it is kept. To bound the cost, if the first episode served from a pack has nothing better, the rest of that pack is skipped until the next run, and a release that turns out to be a multi-episode pack is tried at most once per run rather than once per episode. The duplicate cleanup (`DEDUP_REMOVE_DUPLICATES=true`) cooperates: it only removes a torrent in favour of copies of the same or better quality, so better single episodes are kept even when the pack is still kept as the only copy of some other episode. Packs above the `MAX_RESOLUTION` ceiling are left to corrective downgrade (below). A lower-quality full-season pack can end up split into better single episodes; consolidation folds them back into a pack once one at least as good on both source tier and resolution exists — never the pack they were split from, except an *unchecked* pack split into equal-quality checked singles, which consolidation may fold back once if its own probe now checks out (it is then recorded as checked and not split again).
- **Corrective downgrade — lowering the ceiling (opt-in, `ALLOW_RESOLUTION_DOWNGRADE`):** if you lower `MAX_RESOLUTION` below a copy you already own (a movie, a singleton episode, or a whole season pack), that copy no longer conforms — but it is never silently deleted. With `ALLOW_RESOLUTION_DOWNGRADE=false` (the default) the engine only **logs** the correction it would make (a preview). With it set to `true`, the engine replaces the above-ceiling copy with the best within-ceiling **cached** release covering the same content and prunes the old one — idle-gated exactly like a normal upgrade, and only when a conforming cached replacement actually exists (a sole over-ceiling copy with nothing to replace it is left alone rather than lost).
- **Full-season consolidation (shows, unchanged, runs after the per-episode pass):** when scattered per-episode torrents exist for a season, the engine looks for a **cached** full-season pack that covers every aired episode (per TMDB) at same-or-better source tier and resolution (a candidate below that isn't even staged). If one is found, the season's episode `selection` slots are repointed to the pack and the individual episode torrents are pruned — idle-gated in the same way. Resolution and source are compared separately: an episode whose resolution or source isn't tagged never blocks a pack on that point (unknown is the lowest), while a pack whose resolution or source isn't tagged never replaces an episode whose quality is known on that point. Partial-season packs are never adopted, and consolidation itself never trades away quality — it is not a downgrade path.

A swap or prune waits while a player is reading one of the copies it changes (a read within the last `UPGRADE_IDLE_SECS`). A title being watched is skipped for that pass and retried on the next one, and the pass carries on with the other titles, so a stream of another title, or Jellyfin reading a file a swap has just added, holds nothing back. Read activity is kept in memory, so after a service restart the library counts as **not idle** for the first `UPGRADE_IDLE_SECS` (start-up grace) — no swap, prune or removal happens until then.

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

- `src/main.rs`: Entry point — selects the debrid provider, initialises shared state, starts the scheduler and the WebDAV server (which also serves the Trakt enrolment routes).
- `src/provider.rs`: The `DebridProvider` trait and `FileLocator`; startup provider selection (`choose_provider`).
- `src/scheduler.rs`: Spawns the cooperating background jobs (scan loop always; Trakt cycle + episode monitor when Trakt is configured).
- `src/tasks.rs`: Background jobs — the scan loop (polls the active provider, identifies new torrents, updates the VFS) plus the Trakt jobs (`sync_trakt`, `reconcile_wanted`, `monitor_episodes`).
- `src/acquire.rs`: The `AcquisitionEngine` — optimistic add (`acquire`) plus the asynchronous resolver (`observe`) that selects, validates, probes, and finalises each candidate.
- `src/upgrade.rs`: Daily quality-upgrade + full-season consolidation engine (the whole library each pass, copies of unknown or unchecked quality first; a swap or prune waits while a copy it changes is being read).
- `src/reacquire.rs`: `materialise` add→poll→select primitive — used by the instant-repair path (the SP3 acquisition engine instead adds optimistically in `acquire` and resolves the verdict later in `observe`).
- `src/scraper.rs`: `Scraper` trait + `TorrentioScraper` — queries a Stremio-compatible addon for candidate streams, paced (`SCRAPER_MIN_INTERVAL_MS`) and backing off on `429`.
- `src/selection.rs`: switches a library slot to a better copy already in the account (the scan loop runs it after dedup).
- `src/release.rs`: Parses candidates into `ReleaseInfo` and scores/ranks them with the quality preferences.
- `src/probe.rs`: Hand-rolled MKV/MP4 track reader — fetches the first 4 MB of a CDN URL (plus one targeted read of a trailing MP4 `moov` at its exact offset) to verify audio/subtitle languages.
- `src/read_activity.rs`: In-memory proxy read-activity tracker, per torrent, that holds back swaps, prunes and removals while a file is being read (with a start-up grace).
- `src/trakt_client.rs`: Trakt API client — device-flow OAuth and the watchlist/in-progress/watched reads (paginated; watched shows with per-episode progress). Parser tests use trimmed real Trakt responses in `tests/fixtures/trakt/`, anonymised (fictional titles and ids, shifted dates).
- `src/wanted.rs`: Pure reconcile-core — diffs the wanted-set against owned content and emits acquire/remove decisions (the removal lifecycle rules).
- `src/enrolment.rs`: Local-network Trakt enrolment page (`/trakt/accounts`) served on the WebDAV listener.
- `src/rd_client.rs`: Real-Debrid implementation of `DebridProvider` (1-hour unrestrict cache).
- `src/torbox_client.rs`: TorBox implementation of `DebridProvider` (mylist / requestdl / createtorrent / controltorrent).
- `src/ratelimit.rs`: Shared adaptive token-bucket rate limiter used by both clients.
- `src/tmdb_client.rs`: TMDB API client for media metadata.
- `src/repair.rs`: Torrent repair state machine with provider-neutral instant repair for cached content.
- `src/vfs.rs`: Virtual File System logic for library organisation.
- `src/dav_fs.rs`: WebDAV filesystem — resolves a `FileLocator` to a CDN URL via the provider; attempts instant repair when a file is unavailable.
- `src/identification.rs`: Smart media identification and filename cleaning logic.
- `src/config.rs`: Startup env parsing/validation (`Config`, the optional `TraktConfig`, plus the always-present `UpgradeConfig`/`AcquisitionConfig`).
- `src/app_state.rs`: `AppState` — the cloneable bundle of shared handles carried by the scheduler's jobs.
- `src/store.rs`: Owns the redb database (schema + migrations, self-healing open) and all typed table accessors.
- `src/error.rs`: Unified error type (`AppError`) using `thiserror`.
- `src/jellyfin_client.rs`: Optional Jellyfin notification client for instant library updates.
- `src/mapper.rs`: Library root (module declarations).

## How It Works

### Background Tasks

A scheduler spawns cooperating periodic jobs (each runs immediately on startup, then on its own interval):

1. **Scan Task**: Polls the active debrid provider for new/updated torrents and updates the virtual filesystem
   - Repeats every `SCAN_INTERVAL_SECS` (default: 60 seconds)
   - Builds the VFS consulting the persisted `selection` table (falls back to largest-bytes for un-managed torrents)
   - Also runs `observe` to resolve in-flight acquisitions (deferred gates, provides recording, dead-torrent reaping)
2. **Trakt Cycle** *(only when Trakt is configured)*: Syncs each enrolled account's watchlist/in-progress + watched state, then reconciles the library (acquire missing titles, remove finished/abandoned ones). Repeats every `TRAKT_SYNC_INTERVAL_SECS` (default: 900 seconds)
3. **Episode Monitor** *(only when Trakt is configured)*: Checks tracked shows for newly aired episodes and acquires them. Repeats every `TRAKT_EPISODE_CHECK_INTERVAL_SECS` (default: 3600 seconds)
4. **Upgrade Engine** *(on by default; disable with `UPGRADE_INTERVAL_SECS=0`)*: Re-scores owned titles and upgrades them to meaningfully-better cached releases; consolidates scattered per-episode torrents into full-season cached packs. Idle-gated — never swaps or prunes a file being actively streamed. Repeats every `UPGRADE_INTERVAL_SECS` (default: 86 400 seconds / daily)

### On-Demand Repair

There is no background repair loop. Instead, repair is triggered synchronously at playback time:

- When a media file is read, `dav_fs` resolves it through the provider for a fresh CDN URL (a per-file resolution cache makes this free when the content is healthy)
- If resolution reports the file is unavailable (any 5xx **or** a transport-level failure — timeout/connect, no HTTP status — from Real-Debrid on unrestrict, both failed fast via a small bounded retry budget plus a 10s connect timeout rather than retried for minutes, or an uncached/expired file on TorBox → `AppError::Unavailable`; a 4xx such as a bad token stays a hard error and does not trigger repair), `try_instant_repair` runs synchronously: re-adds the torrent by hash, matches the same file by path, and checks whether the replacement is already cached
- A persistent CDN **5xx on the byte fetch** (the file resolved but is broken on the provider) also escalates to the same instant repair, once per read after a cheap fresh-URL retry still 5xxs — so a broken file is no longer an endless player retry-storm
- **Cached content** (most common): repair completes in ~1-2 seconds and the replacement file is resolved inline — playback continues after a brief delay
- **Non-cached content** (Real-Debrid, where the re-add mints a new id): the file returns an error, the old torrent is deleted, and the new torrent is left to download (the scan loop picks it up automatically). On TorBox the re-add by hash returns the **same** torrent (re-downloading), so it is left in place and stays visible — the scan loop surfaces it once it finishes downloading
- Torrents in the `Broken`/`Repairing` state — the Real-Debrid new-id replacement path (the superseded old id) and any torrent mid-repair — are hidden from WebDAV until healthy again. The TorBox same-id re-download is kept `Healthy` and therefore stays visible, as noted above

### Jellyfin Notifications

When `JELLYFIN_URL`, `JELLYFIN_API_KEY`, and `JELLYFIN_RCLONE_MOUNT_PATH` are all set, the service notifies Jellyfin of specific changed paths after each VFS update. This uses Jellyfin's `POST /Library/Media/Updated` API to trigger targeted scans of only the affected folders (e.g. a single season directory for a new episode), avoiding full library rescans. Changes from all sources — new torrents, deletions, repairs — are detected automatically.

### Archive-Only Torrents

Some torrents deliver their video inside RAR/ZIP archives rather than as plain `.mkv`/`.mp4` files. Debrid services do not extract these archives, so the contents cannot be streamed. The service detects archive-only torrents (covering `.rar`, `.zip`, `.7z`, `.tar`, `.gz`/`.bz2`, and split-volume `.r00…`/`.s00…` parts) and handles them differently depending on where the torrent came from:

- **Engine-acquired content (Trakt-driven): handled automatically — no manual action needed.** An archive-only release never resolves a streamable video file, so the acquisition resolver (`observe`) treats it like any other dead torrent: once it has stayed unresolved for `ACQUIRE_DEAD_TIMEOUT_SECS` (default 600 s) it is reaped, its hash is **blacklisted** (so the same release isn't tried again), and the next-best candidate is acquired in its place. (Archive-ness isn't detected at scrape time — a Stremio stream object carries no such signal — so the engine can still *pick* an archive release, but it is caught and replaced here rather than lingering.)
- **Content you added yourself, or that pre-existed in your debrid account (the account mirror): manual resolution required.** Such content is recorded as owned and is **not** auto-removed (it is never blacklisted), so a warning is logged on each scan instead:

  ```
  WARN Torrent 'Movie.Name.1080p.BluRay' contains only archive files (RAR/ZIP) and cannot be streamed — replace it with a non-archive version on your debrid service
  ```

  To fix it, delete the torrent from your debrid account and add an alternative release whose video files are stored directly (`.mkv`, `.mp4`, etc.).

### Error Handling

- **Unavailable file** (a 5xx or transport-level failure from Real-Debrid on unrestrict, or an uncached file on TorBox): Triggers synchronous instant repair — succeeds inline for cached content, fails for non-cached
- **429 Rate Limit**: Adaptive token bucket rate limiter shared across the provider's API calls — on 429, the global request interval doubles (max 30s between requests) and Retry-After headers are respected; on success, the interval **halves** back toward the baseline of 10 req/s (multiplicative recovery, so a throttle burst doesn't leave later requests crawling)
- **404 Not Found**: Treated as success for delete operations (idempotent)
- **Playback Errors**: WebDAV read failures on an unavailable file (or a persistent CDN 5xx) trigger instant repair (rate-limited to 30s cooldown, max 3 attempts per torrent)

### Caching

- **Resolved CDN URLs**: cached to reduce API load — ~1 hour for Real-Debrid, ~3 hours for TorBox (matching each provider's link lifetime)
- **TMDB metadata**: Persisted to embedded database (`metadata.db`) indefinitely

## Licence

MIT
