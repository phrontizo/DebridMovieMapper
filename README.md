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
- **On-Demand Repair**: Detects unavailable files at playback time (any failure to resolve a file other than a bad token or throttling, such as a 5xx, a transport failure or a stale-link 404 from Real-Debrid or an uncached/expired file on TorBox, plus a persistent CDN 5xx on the byte fetch) and attempts instant synchronous repair by re-adding the torrent. For cached content, playback continues after a ~1-2s delay; otherwise a fresh download is started automatically.
- **Trakt Integration (optional)**: Drive the library from your Trakt account(s) — watchlisted and in-progress movies/shows are auto-acquired, new episodes of tracked shows are picked up as they air, and a title acquired from a watchlist is removed once nobody wants it any more (the exact rules are under [Trakt Integration](#trakt-integration)). Link one or more accounts via a local-network enrolment page. Disabled by default. Without it nothing is acquired from watchlists or watch history, but the account mirror, the duplicate handling and the daily upgrade engine still work on what is already in your account (see the † note under [Environment Variables](#environment-variables)).
- **Persistent State**: An embedded database (`redb`) caches media identifications, which reduces API calls and speeds up restarts. It also holds the service's own state: linked Trakt accounts, what it acquired and for whom, the copy chosen for each title or episode, and the release blacklist. Keep it and back it up, because that state can't be rebuilt from your debrid account.
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
PORT=8080                   # WebDAV server listen port (default: 8080); with the compose file it sets only the published host port
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
# TRAKT_CATCHUP_LOOKBACK=                  # catch-up window for watched shows: 90d/12w/6mo/1y (also 6m, "6 months"); unset=all-time
# TRAKT_ENROL_ALLOWED_HOSTS=               # extra host names the /trakt pages answer for, e.g. a reverse proxy's name

# Optional: auto-upgrade engine (on by default; set UPGRADE_INTERVAL_SECS=0 to disable)
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

Run outside a container, the service reads this `.env` itself (from the working directory or a parent), and an exported variable wins over it. In that file, quote a value that contains spaces (e.g. `TRAKT_CATCHUP_LOOKBACK="6 months"`) and list `TRAKT_ENROL_ALLOWED_HOSTS` without spaces after the commas. Wrap a value that contains `$` in single quotes, e.g. `SCRAPER_PROXY_URL='http://user:pa$word@host:3128'`: a native run and Docker Compose both expand `$NAME` in an unquoted or double-quoted `.env` value, so that password would silently lose `$word`. A line the parser can't read stops a native start with a `Configuration error`; the line itself isn't printed, since it may hold a token. A native run also listens on every interface (`0.0.0.0:PORT`), and there is no setting to bind it to one address. The WebDAV endpoint and the `/trakt` enrolment pages are unauthenticated, so run it only on a trusted network or firewall the port. With the compose file, a `.env` value reaches the container only through the variables compose.yml lists. It lists every setting below with an empty default, which means the documented default; `JELLYFIN_URL`, `JELLYFIN_RCLONE_MOUNT_PATH` and `DB_PATH` are fixed there, and `PORT` sets only the published host port. Compose reads `.env` when it creates the container, so after editing it run `docker compose up -d`: `docker compose restart` keeps the old environment.

### Environment Variables

A debrid provider token is required: set **exactly one** of `RD_API_TOKEN` or `TORBOX_API_KEY` (setting both, or neither, is a startup error). Whichever token is set selects the active provider for that deployment — both Real-Debrid and TorBox are fully supported.

| Variable                     | Required | Default | Description                                                          |
|------------------------------|----------|---------|----------------------------------------------------------------------|
| `RD_API_TOKEN`               | One of\* | -              | Your Real-Debrid API token                                           |
| `TORBOX_API_KEY`             | One of\* | -              | Your TorBox API token                                                |
| `TMDB_API_KEY`               | Yes      | -              | Your TMDB (The Movie Database) API key                               |
| `SCAN_INTERVAL_SECS`         | No       | 60             | Interval between torrent library scans in seconds (minimum: 10, runs immediately on startup) |
| `DB_PATH`                    | No       | `metadata.db`  | Path to the redb database file. If the file is genuinely corrupt/unreadable (bad header, truncated, redb repair aborted), it is automatically moved aside to `<DB_PATH>.corrupt` (or `<DB_PATH>.corrupt.<n>` if that name already exists) and recreated, so a corrupt file never crash-loops the service. The old file is kept, but the new database starts without Trakt enrolments, acquisition history (provenance) and chosen copies. A DB written by a **newer** build, an **older or newer** redb on-disk format, or a transient/operational error (permission/lock/disk) instead **fails startup**, leaving the intact file in place rather than discarding its non-regenerable authoritative tables. |
| `PORT`                       | No       | 8080           | WebDAV server listen port                                            |
| `CDN_FIRST_READ_MB`          | No       | 8              | Size (MB) of the FIRST read-ahead fetch when a media file is opened — front-loads the container header + early index so a player's open/ffprobe burst needs fewer cold CDN round-trips before playback starts. Later reads use the normal 2 MB window. Clamped to 2–16 MB; set `2` to disable the enlargement. |
| `RUST_LOG`                   | No       | `info`         | Log verbosity (`tracing-subscriber` `EnvFilter` directive). Scope it to avoid dependency noise, e.g. `debridmoviemapper=debug`, or target a module: `debridmoviemapper::acquire=debug,info`. A malformed value falls back to `info`. Takes effect on (re)start; under compose, when the container is recreated (`docker compose up -d`). |
| `JELLYFIN_URL`               | No       | -              | Jellyfin server URL for library update notifications                 |
| `JELLYFIN_API_KEY`           | No       | -              | Jellyfin API key for authentication                                  |
| `JELLYFIN_RCLONE_MOUNT_PATH` | No       | -              | rclone mount path as seen by Jellyfin (e.g. `/media`)                |
| `SCRAPER_ADDON_URL`          | No       | *(auto)*       | Override the Torrentio scraper base URL. Defaults to a URL auto-built from your provider token (`https://torrentio.strem.fun/<provider>=<token>`). A pasted addon manifest URL (`…/manifest.json`) is accepted; the suffix is dropped. |
| `SCRAPER_PROXY_URL`          | No       | -              | Route the **scraper's** requests (Torrentio / custom addon) through an HTTP(S) proxy — e.g. `http://[user:pass@]host:port`. Applies **only** to scraper traffic; the debrid CDN media reads, TMDB, and provider APIs stay direct. A set-but-invalid value (non-http(s) scheme; socks is not supported) is a startup error rather than a silent direct fallback. |
| `SCRAPER_MIN_INTERVAL_MS`    | No       | `1000`         | Minimum spacing between scraper requests, in milliseconds (clamped 100–60000). Torrentio publishes no rate limit and starts answering `429 Too Many Requests` when a burst of scrapes (e.g. the hourly episode check) arrives at once, so requests are paced; a `429` slows the pace further (honouring `Retry-After`) and the scrape is tried up to 3 times in all (2 retries). Lower it if you run your own addon. |
| `MAX_RESOLUTION`             | No       | `1080`         | Hard resolution ceiling for acquisition: `720`, `1080`, `2160` (each also accepted with a `p` suffix, e.g. `1080p`), or the `2160` synonyms `4k` / `uhd` (case-insensitive). Candidates above this height are excluded. |
| `AUDIO_LANGUAGE`             | No       | `original`     | Required audio language for acquisition: a 2- or 3-letter ISO code (e.g. `eng` or `en`) or `original` (uses the title's languages from TMDB: its original language **plus** its listed spoken languages — a release in any of them is accepted, so a film whose TMDB original language isn't its main audio, e.g. a `fi`-original, mostly-English film, isn't rejected for correct English releases; a title with no spoken languages listed uses the original language alone). A value that is not a language code (a full name like `english`, a locale like `pt-BR`, a 2-letter code that isn't ISO 639-1 such as `xy`, or `off`/`any`/`all`) is rejected back to `original` with a warning. A 3-letter code the service doesn't know (e.g. `egn`) is used as written, with a warning, since rare valid ISO 639-2 codes such as `haw` aren't in its tables; a misspelt code therefore matches no audio track, and every release is rejected and blacklisted for 30 days. `no` is Norwegian: it is accepted with a warning (write `nor` to silence it). TMDB's non-ISO codes are understood too: `cn` (Cantonese) accepts Chinese-tagged tracks, `sh` (Serbo-Croatian) accepts Croatian, Serbian and Bosnian ones, and `xx` (no language: a silent or dialogue-free title) puts no requirement on the audio. |
| `SUBTITLE_LANGUAGE`          | No       | *(none)*       | Required subtitle language for acquisition: a 2- or 3-letter ISO code, or omit / set to `none` (or `off`) to skip the check. A value that is not a language code (a full name, a locale, a 2-letter code that isn't ISO 639-1, or `any`/`all`) is rejected back to `none` with a warning. A 3-letter code the service doesn't know is used as written, with a warning, so a misspelt one matches no subtitle track and every release is rejected and blacklisted for 30 days. `no` means Norwegian, not `none`; it is accepted with a warning. |
| `PREFER_HEVC`                | No       | `true`         | Prefer HEVC/H.265 encodes when scoring acquisition candidates. |
| `PREFER_HDR`                 | No       | `false`        | Prefer HDR/Dolby Vision encodes when scoring acquisition candidates. |
| `STALL_TIMEOUT_SECS`         | No       | `1800` (min 60) | Seconds without download progress before a Pending torrent is considered stalled and re-acquired. |
| `MAX_ACQUIRE_ATTEMPTS`       | No       | `5` (min 1)    | How many of the top-ranked candidates one acquisition pass tries to add. It moves to the next candidate only when the add itself fails. A release that is later rejected (wrong title or audio, corrupt, stalled or dead) is blacklisted and the next-best candidate is tried, so over time every non-blacklisted candidate may be tried. |
| `TRAKT_CLIENT_ID`            | No†      | -              | Trakt API app client id. Both this and `TRAKT_CLIENT_SECRET` are required to enable Trakt sync. |
| `TRAKT_CLIENT_SECRET`        | No†      | -              | Trakt API app client secret. |
| `TRAKT_SYNC_INTERVAL_SECS`   | No       | `900`          | How often (seconds) to sync each enrolled Trakt account and then reconcile the library: acquire or re-acquire wanted movies, remove finished or abandoned movies, and remove shows abandoned from a watchlist (minimum: 60). Show episodes are acquired by the episode monitor (`TRAKT_EPISODE_CHECK_INTERVAL_SECS`), which works from the wanted list this sync writes. |
| `TRAKT_EPISODE_CHECK_INTERVAL_SECS` | No | `3600`      | How often (seconds) the episode monitor runs (minimum: 300). It acquires every missing aired episode of a wanted show (a newly watchlisted show's whole run, catch-up episodes and new airings) and removes finished shows. A show you add on Trakt is acquired on the first episode-monitor run after the next sync. |
| `TRAKT_CATCHUP_LOOKBACK`     | No       | all-time       | Catch-up window. A show you've *watched*, that Trakt reports as **behind** (≥1 unwatched aired episode) and that you last watched within this window, has every aired regular episode you haven't watched acquired, including ones that aired before your last watch. Fully-watched shows are skipped. A number of days, weeks, months (30 days) or years: `90d`, `12w`, `6mo` (or `6m`, `"6 months"`, quoted in a `.env` file), `1y`; a bare number = days; `0`/unset = all-time. An unrecognised value is logged as a warning and also means all-time. Anchored on Trakt's per-show `last_watched_at`. With the default all-time window this covers every partly watched show in your whole Trakt history (one watched pilot of a long show brings in the rest of it), which can mean a large number of torrents, so set a bounded lookback (e.g. `90d` or `6mo`) if your history is large. Each catch-up candidate also costs one Trakt + one TMDB call per sync cycle (rate-limited, so absorbed). This window also scopes `REMOVE_FINISHED_SHOWS` finish-cleanup (same per-show progress check). |
| `TRAKT_ENROL_ALLOWED_HOSTS`  | No       | unset          | Comma-separated extra host names (no scheme or port) that the `/trakt` enrolment pages answer for, e.g. a reverse proxy's FQDN. IP addresses, `localhost`/`*.localhost`, single-label names and `.local`/`.lan`/`.internal`/`.home.arpa` names always work. A request whose `Host` is anything else gets `421 Misdirected Request`, which guards against DNS rebinding. |
| `UPGRADE_INTERVAL_SECS`      | No       | `86400`        | How often (seconds) to run the quality-upgrade + full-season consolidation pass. `0` disables the upgrade engine entirely. Default is daily (86 400 s); minimum 600 when non-zero. |
| `UPGRADE_BUDGET_PER_TICK`    | No       | unset          | Cap on the owned titles re-scored per upgrade pass (minimum: 1). Unset, every pass covers the whole library; a pass over ~300 titles makes roughly 500 Torrentio requests (about 8 minutes at the default `SCRAPER_MIN_INTERVAL_MS`). With a cap, titles holding a copy of unknown or unchecked quality get half of it and the rest rotates through the library, least recently checked first. |
| `UPGRADE_IDLE_SECS`          | No       | `300`          | Seconds without a proxy read before a copy may be swapped, pruned or removed (minimum: 30). Prevents swapping or deleting a file that is being streamed. The upgrade engine waits only for the copies a swap changes, so a film or show being watched is skipped for that pass while the rest of the library is still upgraded; duplicate-dedup removals and Trakt lifecycle removals wait until nothing in the library has been read for this long. Read activity is tracked in memory only, so for the **first `UPGRADE_IDLE_SECS` after the service starts** the library counts as **not idle** (a start-up grace): none of the idle-gated swaps, prunes or removals happen on the first ticks after a restart, giving a player that was streaming time to reconnect. Torrents nobody can be streaming are still deleted in that time: one the provider reports dead, an acquisition that failed its checks or never resolved, and an upgrade or acquisition candidate that a previous run added but never recorded (cut off by a restart). The first upgrade pass after a start therefore waits until the grace is over (it then runs every `UPGRADE_INTERVAL_SECS` as usual), so restarting the service never postpones upgrades by a whole interval. |
| `ACQUIRE_DEAD_TIMEOUT_SECS`  | No       | `600`          | Seconds an optimistically-added `Pending` torrent may remain unresolved before `observe` reaps it as dead, blacklists its hash, and re-scrapes (minimum: 120). |
| `DEDUP_REMOVE_DUPLICATES`    | No       | `false`        | When `true`, the duplicate-dedup pass **deletes** redundant torrents (a title covered by more than one torrent → keep the best, remove the rest, only ever removing a torrent whose every episode is also held by a kept torrent of the **same or better quality** — a better copy is never removed because a worse one covers it; for a show, each episode counts at the quality of the file a copy would serve for it, so a pack whose file for one episode is a low-quality one never makes a better copy of that episode redundant). Quality decides first — resolution, then source tier, then a checked copy before one whose tracks couldn't be checked; an untagged resolution or source counts as the lowest, but a known resolution is never traded down (a 1080p copy with an untagged source beats a 720p one) — then the release score, then which copy the library currently serves; copies already in your account before the service found them get no special treatment. A torrent that holds several films (more than one selected video of 700 MB or more) is never removed, never makes a separate copy of one of its films redundant, and the upgrade engine doesn't replace it either. A copy the service is still downloading or checking, or one the library can't serve (not downloaded, or hidden after repeated failed repairs), is left alone and never counts against a playable copy. Default `false` = dry-run (logs the plan only). The dry run doesn't keep several copies of a film, though: when the upgrade engine adopts a better release of a film, it deletes every other copy of it within `MAX_RESOLUTION` (set `UPGRADE_INTERVAL_SECS=0` to stop that). Idle-gated like upgrades (including the start-up grace), so it never interrupts an active stream. Independently of this flag, a second torrent entry for the very same release (same infohash) is always removed, once nothing has been read for `UPGRADE_IDLE_SECS`. |
| `REMOVE_FINISHED_SHOWS`      | No       | `false`        | When `true`, a fully-watched **Ended** show that isn't on your watchlist is auto-removed once finished — **including shows already in your library**. Default `false` only logs the candidates (a preview) without removing. Watchlisted shows are never finish-removed, and neither is a show Trakt still reports you behind on (or whose progress the sync couldn't check). When enabled, the log reports the candidates as handed to the episode monitor, and an actual removal is logged as `reconcile: removed tmdb …`. Note: finish-cleanup reuses the `TRAKT_CATCHUP_LOOKBACK` per-show progress check, so a bounded lookback also scopes it — a show last watched outside that window is neither previewed nor removed. |
| `ALLOW_RESOLUTION_DOWNGRADE` | No       | `false`        | When `true`, the upgrade engine performs a **corrective downgrade**: when `MAX_RESOLUTION` is lowered below an owned copy's resolution, it replaces that above-ceiling copy with a ceiling-conforming **cached** release and prunes the higher-res copy — idle-gated, and only when such a replacement exists (a sole over-ceiling copy with no conforming cached release is kept). Default `false` only logs the planned downgrades (a preview) without replacing. Pure quality upgrades are unaffected by this flag. Applies to copies the service acquired and to ones already in your account alike: a file whose name shows a resolution above the ceiling is always recognised as such (never treated as unknown quality), so it is only ever replaced through this option. For a movie or a single episode it never goes below a within-ceiling resolution the title already owns, counting a copy whose source isn't tagged: with a 2160p copy and an untagged 1080p copy under a 1080p ceiling, only a cached 1080p release replaces them, never a 720p one. An over-ceiling season pack is replaced by the best cached within-ceiling pack covering its episodes, whatever else you own. Only the old pack is deleted, and on the next scan an episode you also hold in a better within-ceiling copy switches to that copy. |

\* Exactly one of `RD_API_TOKEN` / `TORBOX_API_KEY` must be set — not both, and not neither.

Boolean options (`PREFER_HEVC`, `PREFER_HDR`, `DEDUP_REMOVE_DUPLICATES`, `REMOVE_FINISHED_SHOWS`, `ALLOW_RESOLUTION_DOWNGRADE`) accept `true`/`1`/`yes`/`on` and `false`/`0`/`no`/`off`, in any case. Any other non-empty value is logged as a warning and the default is used; an empty value is the same as unset.

† Trakt sync is enabled only when **both** `TRAKT_CLIENT_ID` and `TRAKT_CLIENT_SECRET` are set. If either is absent, Trakt sync is disabled and nothing is acquired from watchlists or watch history (setting only one of the two logs a warning naming the missing one). The service still works on everything already in your account, though. It records those torrents as owned (the account mirror), switches titles it has already chosen a copy for to the best copy present, removes a second torrent entry for the same release, and runs the duplicate-dedup pass (a dry run by default). The daily upgrade engine also runs (unless `UPGRADE_INTERVAL_SECS=0`): it adds better cached releases and deletes the copies they replace, including torrents you added yourself. If you don't want the service to replace or remove copies already in your account, set `UPGRADE_INTERVAL_SECS=0` and leave `DEDUP_REMOVE_DUPLICATES` off. It then removes only a second entry for the same torrent and a torrent Real-Debrid reports as dead (`magnet_error`, `dead`, `error` or `virus`) whose IMDB id is known. A dead movie is replaced with the next-best release. A dead show torrent is deleted, and its episodes come back only when Trakt is enabled and a linked account wants the show, in which case the episode monitor re-acquires them.

## Running with Docker

The easiest way to run DebridMovieMapper is using Docker. Pre-built multi-platform images are available via the GitHub Container Registry.

> Until the v2 line is released, `:latest` is the 1.x release. It has no Trakt sync, acquisition, upgrade engine or duplicate-dedup pass, so it ignores those settings and serves no `/trakt/accounts` page. To run what this README describes, use the `:v2edge` tag (`ghcr.io/phrontizo/debridmoviemapper:v2edge`) in place of `:latest` in the commands below.

### 1. Pull the image

Images are available from both GitHub Container Registry and Docker Hub:
```bash
# GitHub Container Registry
docker pull ghcr.io/phrontizo/debridmoviemapper:latest

# Docker Hub
docker pull phrontizo/debridmoviemapper:latest
```

**Available tags:**
- `:latest` — latest stable release (updated on release tags like `v1.0.7`; a pre-release tag such as `v1.1.0-rc.1` publishes only `:1.1.0-rc.1` and never moves `:latest`)
- `:edge` — latest build from `main` branch (may be unstable)
- `:v2edge` — latest build from the `v2` development branch (may be unstable; CI publishes a per-branch rolling tag)
- `:1.0.7` pins that exact release
- `:1.0` and `:1` follow the latest 1.0.x and 1.x release (a pre-release tag never moves them)

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

To run it without Docker, build it with Rust 1.96 or newer:
```bash
cargo build --release
```
Then run `target/release/debridmoviemapper` from the directory that holds your `.env`. Its database is `metadata.db` in that directory unless `DB_PATH` is set.

Multi-platform images are built and pushed automatically by GitHub Actions. Release tags (`git tag vX.Y.Z && git push origin vX.Y.Z`) update `:latest` and the semver tags (a pre-release tag updates only its own version tag, and its GitHub Release is a pre-release, never marked Latest). Bump `version` in `Cargo.toml` to `X.Y.Z` first and run `cargo build` so `Cargo.lock` picks it up: the release workflow rejects a tag that doesn't match. Pushes to `main` update `:edge`, and pushes to the `v2` development branch update `:v2edge`.

*Note: The named volume holds the service's database: the TMDB identification cache plus the service's own state (linked Trakt accounts, what it acquired and for whom, the copy chosen for each title or episode, and the release blacklist). Keep it across container recreations and back it up. Deleting it loses that state.*

**Upgrading from a bind-mounted `metadata.db`:** If you previously used `-v $(pwd)/metadata.db:/metadata.db`, switch to a named volume. The image keeps its database at `/data/metadata.db` and never reads a file mounted at `/metadata.db`, so with only the old mount the container starts an empty database, which is lost when the container is recreated. To move your existing file into the volume, stop the old container and run, from the directory holding `metadata.db`:

```bash
docker run --rm -v debridmoviemapper-metadata:/data -v "$PWD":/src busybox \
  sh -c 'cp /src/metadata.db /data/metadata.db && chown -R 65534:65534 /data'
```

Then start the new container. The service runs as uid 65534 and the image has no shell, so the copy has to hand the file to that user: a file copied in another way (e.g. `docker cp`) is usually owned by root, and the service then can't open it and fails to start. Under Docker Compose, run `docker compose create` first so compose creates the volume, and use its name in place of `debridmoviemapper-metadata` (`<project>_metadata`, shown by `docker volume ls`). You can instead keep the bind mount and add `-e DB_PATH=/metadata.db`, with the file owned by uid 65534. Starting from an empty database is lossless only for a database from a version before Trakt and acquisition, which held nothing but TMDB identifications. A newer one also holds your Trakt enrolments and the record of what the service acquired, and an empty database loses both.

**Upgrading from sled (pre-redb):** If you previously ran an older version that used sled, your `metadata.db` will be a directory. Remove it before starting the new version: `rm -rf metadata.db`. The redb database will be recreated automatically.

## Docker Compose Setup with Jellyfin

The recommended way to use DebridMovieMapper with Jellyfin is via Docker Compose with rclone mounting the WebDAV endpoint.

A complete [`compose.yml`](compose.yml) file is provided in the repository that includes:
- DebridMovieMapper service
- rclone for mounting the WebDAV endpoint
- Jellyfin for media playback

compose.yml runs the `:latest` image, which until the v2 release is the 1.x line (see [Running with Docker](#running-with-docker)). To run the v2 development build, put a `compose.override.yml` (git-ignored) next to it containing `services: { debridmoviemapper: { image: ghcr.io/phrontizo/debridmoviemapper:v2edge } }`. To run your own checkout instead, make it `services: { debridmoviemapper: { build: ., image: debridmoviemapper } }` and start with `docker compose up -d --build`.

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

   Any other setting from the [Configuration](#configuration) section can go in the same `.env`; compose.yml passes each one through. After changing `.env` later, run `docker compose up -d` again to recreate the container (`docker compose restart` keeps the old values).

   Point only one instance at a debrid account. A second one (another deployment, or a `cargo run` in a checkout, which reads the same `.env`) mirrors, upgrades and prunes the same account with its own database, so each deletes torrents the other is tracking.

3. Start the services:
   ```bash
   docker compose up -d
   ```

4. Access Jellyfin at `http://localhost:8096` and add `/media` as a library path

5. (Optional) To enable instant library updates, set `JELLYFIN_API_KEY` in your `.env` file

### Notes

- The WebDAV server runs on port 8080 (bound to `127.0.0.1:8080` on the host by the compose file, so it is reachable only from the local machine; adjust the port mapping if a trusted LAN needs access)
- A Docker healthcheck ensures rclone only starts after the WebDAV server is ready (prevents empty mount on startup)
- The service's database (identification cache, linked Trakt accounts, acquisition history) is persisted in a named Docker volume (`metadata`). `docker compose down -v` deletes it, so leave out `-v`
- rclone mounts the WebDAV endpoint to `./rclone` on the host via a bind mount with `rshared` propagation (required for FUSE mounts to be visible to other containers)
- Jellyfin reads from `/media` which is bind-mounted from `./rclone`
- Files appear as real media files (`.mkv`, `.mp4`, etc.) with correct file sizes — media bytes are proxied from the debrid provider's CDN on demand
- Each title folder also holds a generated `movie.nfo` or `tvshow.nfo` with the title, the year and, once TMDB has identified the title, its TMDB id, so Jellyfin matches the title without a search of its own. It sets `lockdata` off, so Jellyfin still fetches the full metadata from TMDB
- Jellyfin probes and plays content normally as if the files were local

## Usage

Once running, the WebDAV server will be available at `http://localhost:8080`. Mount it with rclone and point your media server at the mount:

- **Jellyfin** (via rclone mount - see Docker Compose example above)
- **Plex** (via rclone mount)
- **Kodi** (expected to work via the rclone mount; untested)

Infuse (iOS/tvOS/macOS) and other WebDAV clients can connect to the WebDAV URL directly. The examples bind the port to `127.0.0.1`, so a client on another device needs the port mapping changed for a trusted LAN (the endpoint is unauthenticated).

## Acquisition

The acquisition engine lets the service find and add content to your debrid account automatically, using Torrentio as a scraper. It is driven by [Trakt Integration](#trakt-integration) (and the library reconciler) — there is no manual trigger.

### How Acquisition Works

Acquisition uses an **optimistic-add** model: the engine adds the best candidate and records it `Pending` immediately, then resolves it asynchronously so slow-to-seed torrents are not judged prematurely.

1. **Scrape**: Given an IMDB id and media type, `TorrentioScraper` queries a Stremio-compatible addon (default: Torrentio, URL auto-built from your provider token; override with `SCRAPER_ADDON_URL`) for candidate torrents. The IMDB id is taken from Trakt when Trakt supplies one and only re-derived from TMDB otherwise — so a title that TMDB has no IMDB id for (e.g. a TVDB-only show) is still acquirable. A title with no IMDB id anywhere can't be scraped (Torrentio is IMDB-keyed) and is skipped quietly.
2. **Score and rank**: Candidates are parsed for resolution, codec, HDR, file size, seeder count, and cached status. A hard ceiling at `MAX_RESOLUTION` excludes anything above it. Remaining candidates are ranked by a weighted **score**: cached availability dominates, then **resolution** (within the `MAX_RESOLUTION` ceiling — resolution outweighs source tier, so a higher-resolution release wins), then source tier (REMUX>BluRay>WEB>HDTV). On top of those, progressively smaller **additive** bonuses — HEVC and HDR encodes, a verifiable container, bitrate, and seeder count — are folded into the same weighted sum rather than applied as a strict tie-break chain, so (for example) the HEVC bonus can outweigh a one-step source-tier difference. Uncached zero-seeder releases are hard-filtered (undownloadable), and so are cam, telesync and screener releases. When a name has a year, episode code or `Season N`, a bare marker such as `Cam` or `TS` counts only after it, and after an episode code only from the resolution/source tags on (or inside brackets), or, on a file line with no such tags, only as a lone tag right after the code (`S01E01.CAM.mkv`, but not an episode title such as `S01E05 - Cam Girl.mkv`), so a title word like *Body Cam*, or an episode title like *Hidden Cam*, doesn't exclude every release of that title. A release whose name or Torrentio flags (e.g. `Rus`, `ITA`, `VFF`, 🇷🇺) tag only audio languages other than the required `AUDIO_LANGUAGE` (for `original`: any of the title's TMDB original/spoken languages) is heavily down-ranked — a soft penalty, not an exclusion, since a flag can also name a subtitle language; the file probe is the real check. Subtitle-only markers (`VOSTFR`, `Multi Subs`) never count as an audio language, and neither does a language written next to a subtitle marker (`[ENG SUB]`, `Eng.Subs`, `SUB.ITA`). A title that itself contains a language word (*The Italian Job*, *The Danish Girl*) is tagged with it on every release — a known limitation, shared by all of that title's releases.
3. **Optimistic add**: The top non-blacklisted candidate is added to your debrid account by hash, recorded as `Pending`, and control returns immediately — no synchronous wait. The engine reads the provider listing first and skips a candidate whose torrent is already in your account under another title or untracked, instead of adding it again; if the listing can't be read, nothing is added this time.
4. **Asynchronous resolve** (`observe`, runs every scan tick): once the torrent's files appear, the pack guard runs (multi-feature movie packs are rejected). Once the torrent has finished downloading (at once for cached content), the deferred gates run: strict title validation (the file name must identify to the requested TMDB id; two mismatches in a row get the release blacklisted, and when TMDB can't be reached the release just stays `Pending` and is checked again on a later tick) → file probe (the first 4 MB is checked for required audio/subtitle languages — a non-faststart MP4, whose track index (`moov`) sits at the end of the file, is read at that index's exact offset, and an MKV whose track list lies past the first 4 MB (behind large attachments, for example) gets one larger read of up to 8 MB; corrupt/wrong-language probes are blacklisted). While the torrent is still downloading, only the stall check (step 6) runs. On pass — marked `Verified`, `provides` (which episodes the hash covers) and a quality snapshot are recorded, and the `selection` entry is written. A release whose tracks can't be read at all (e.g. an `.avi`, or a file carrying no track metadata) is still accepted when nothing better was found, but recorded as *unchecked* so the upgrade pass replaces it with a checked copy when one exists. On fail (corrupt/wrong-language) — hash is blacklisted and the next candidate is tried. If the probe can't even fetch (the CDN is momentarily unavailable, returning a *transient* result), the release stays `Pending` and is re-probed — every tick for a short burst, then hourly — but is **never accepted unverified**, so a wrong-language release can't slip in unchecked; if a re-probe still can't fetch it after 24 h (and it has been deferred for at least 2 of them), it is treated as broken and replaced, and never without that fresh failed probe. A deferred probe also drops the cached CDN URL, so the next attempt resolves a fresh one.
5. **Dead-torrent reaping**: if a `Pending` torrent remains unresolved beyond `ACQUIRE_DEAD_TIMEOUT_SECS` (default 600 s), `observe` reaps it, blacklists the hash, and re-scrapes for the next-best candidate. A torrent Real-Debrid reports in a terminal error state (`magnet_error`, `dead`, `error` or `virus`) is reaped the same way even after it was verified, as long as its record has an IMDB id to re-scrape with. TorBox has no such state, so there a stuck torrent is reaped only by the dead and stall timeouts while it is `Pending`. A dead show torrent found in your account (a season pack or a single episode) names no episode to re-scrape, so it is deleted and its hash blacklisted with nothing added in its place. Its episodes come back only through the episode monitor, which means only when Trakt is enabled and a linked account wants the show. A torrent you added to your account yourself carries no TMDB language information until the daily upgrade pass fills it in, so with `AUDIO_LANGUAGE=original` the engine looks up the title's original and spoken languages on TMDB before replacing such a torrent; the replacement is ranked and probed against them and keeps them. If that lookup fails and the replacement's probe has no language to check, the replacement is recorded as *unchecked*, so the upgrade pass replaces it with a checked copy. A rejected or dead torrent is deleted under every entry your account lists for it, and its hash is blacklisted only once every delete has succeeded. If a delete fails, the record is kept and the delete is retried on a later scan before anything is added in its place.
6. **Stall detection**: no progress for `STALL_TIMEOUT_SECS` triggers re-acquisition via the next unblacklisted candidate.
7. **Outcome**: `Acquired` (a candidate is already owned and present, so nothing is added; it may still be resolving), `Pending` (a candidate was added and `observe` resolves it), `NoAcceptableRelease` (no non-blacklisted candidate within the ceiling and filters could be added), or `TemporarilyUnavailable` (the scraper or the provider listing is unreachable; retry later).

Blacklist entries expire after 30 days, after which a rejected release may be tried again.

### What triggers acquisition

Acquisition is triggered by [Trakt Integration](#trakt-integration). The Trakt reconciler (every `TRAKT_SYNC_INTERVAL_SECS`) acquires the missing movies on each enrolled account's watchlist and in-progress list and removes abandoned titles and finished movies. Every show episode, including a newly watchlisted show's whole run, and finished-show removal come from the episode monitor, so show acquisition follows `TRAKT_EPISODE_CHECK_INTERVAL_SECS` (hourly by default). Without Trakt, the service acquires only to replace a dead movie (step 5) and when the upgrade engine adds a better copy of something you own. (An earlier development build had a temporary `--acquire` command-line trigger; it has been removed.)

## Trakt Integration

When configured, DebridMovieMapper keeps your library in sync with one or more Trakt accounts:

- **Watchlist + in-progress** movies and shows are automatically acquired into your debrid account. A watchlisted show gets its whole aired run, including episodes you've already watched. A show that isn't on a watchlist (in progress, or a show you've watched that Trakt reports you behind on, picked up through `TRAKT_CATCHUP_LOOKBACK`) gets every aired episode that at least one of the accounts wanting it hasn't watched. Shows are acquired by the episode monitor, so a newly watchlisted show arrives on its first run after the next sync (`TRAKT_EPISODE_CHECK_INTERVAL_SECS`, hourly by default).
- **New episodes** of tracked shows are picked up as they air (using TMDB air dates).
- **Automatic removal** (the Trakt lifecycle): a title acquired from a watchlist is removed once it is un-watchlisted and no linked account still wants it. Trakt drops a watched title from the watchlist, so this is how most watchlist content leaves. A title still on anyone's watchlist is never removed for being watched, so to keep a watched title available, keep it on your watchlist. A title wanted only because it is in progress is removed for being finished only when `REMOVE_FINISHED_SHOWS` folds a show back in (a fully-watched ended show: every aired regular episode watched; specials are ignored, since Trakt doesn't report them). A title Trakt still lists as in progress is never removed for being watched, because you are partway through it (a paused re-watch included). Trakt drops a finished title from its in-progress list, so an in-progress-only movie, or a show with `REMOVE_FINISHED_SHOWS` off, otherwise stays in the library after you finish it. Un-watchlisting (Trigger B) never deletes content you added directly to your debrid account — that removal is scoped to the engine-acquired copies, so a pre-existing season pack survives even if a watchlist acquired later episodes of the same show. Such pre-existing content is, however, treated as owned: the daily upgrade engine may replace a copy with a better cached release (pruning the superseded one); the duplicate-dedup pass may remove redundant duplicate copies when `DEDUP_REMOVE_DUPLICATES=true` — such a copy gets no special protection there and is kept or removed purely on quality (if it loses to a better copy the service acquired, the title then lives on in that copy, which the lifecycle above may later remove); and a fully-watched **Ended** show is finish-removed **including ones already in your library** when `REMOVE_FINISHED_SHOWS=true` (see that option).

It is a single shared household library: acquisition is the union across all linked accounts, and removal is per-user-aware (a title acquired from a watchlist is removed only once no linked account wants it any more, and a finished title only once every account watching it has finished it; see *Automatic removal* above).

Trakt paginates the watchlist and watched-history endpoints (since its 2026 API changes), so every read follows all pages, and watched shows are read with their per-episode progress — that is what lets finish-removal and catch-up tell which episodes you have actually seen. If any page of a read fails (or the list is seen to change while it is being read: its size changes, or an entry turns up on two pages), that account's sync fails for the cycle like any other Trakt read failure (the accounts page marks it with *⚠ the last sync failed* until the next successful sync) and its previous wanted titles are kept. A failed read is never acted on. One rare case gets through: if, within the fraction of a second between two page requests of a list longer than 100 items, an entry is removed from a page already read and another is added further down, the entry that slides back across the page boundary is missed for that cycle. The reconcile that follows may remove what was acquired for it, and the next sync acquires it again.

**Trakt sync is disabled by default.** When `TRAKT_CLIENT_ID`/`TRAKT_CLIENT_SECRET` are not set, nothing is acquired from Trakt, but the account mirror, the duplicate handling and the upgrade engine still run over what is already in your account (see the † note under [Environment Variables](#environment-variables)).

### Enabling Trakt sync

1. Create a Trakt API application at <https://trakt.tv/oauth/applications> (any redirect URI is fine — the service uses device-flow OAuth). Note its **Client ID** and **Client Secret**.
2. Set the environment variables:

   ```env
   TRAKT_CLIENT_ID=your_trakt_client_id
   TRAKT_CLIENT_SECRET=your_trakt_client_secret
   # Optional cadences:
   TRAKT_SYNC_INTERVAL_SECS=900            # Trakt sync + movie reconcile (default: 900, minimum: 60)
   TRAKT_EPISODE_CHECK_INTERVAL_SECS=3600  # show-episode acquisition + finished-show removal (default: 3600, minimum: 300)
   ```

3. Restart the service. With Docker Compose run `docker compose up -d`, which recreates the container with the new `.env` values (`docker compose restart` keeps the old ones); re-create a `docker run` container with the new `-e` flags. On startup it logs that the Trakt enrolment page is available.

### Linking accounts

Visit `/trakt/accounts` on the server (e.g. `http://localhost:8080/trakt/accounts`) to link, refresh, or remove Trakt accounts. Linking uses Trakt's **device flow**: click *Enrol a new account*, then open the shown URL on any device and enter the code to approve it. Once approved, the account is linked automatically and its tokens are stored (keyed by the Trakt username slug).

> ⚠️ The enrolment page is **unauthenticated**, just like the WebDAV endpoint — it assumes the port is only reachable on a trusted local network. Do not expose it to an untrusted network. The pages answer only for local-network host names (IP addresses, `localhost`, single-label names and `.local`/`.lan`/`.internal`/`.home.arpa` names) plus any listed in `TRAKT_ENROL_ALLOWED_HOSTS`, they reject cross-origin form posts, and they refuse to be framed, so neither a DNS-rebinding page on another domain nor a page that frames them can read or change the linked accounts. If you reach them through a reverse proxy under a public-style name, add that name to `TRAKT_ENROL_ALLOWED_HOSTS`.

If an account's sync fails (for example because its token was revoked), the accounts page marks it with *⚠ the last sync failed* and says what to do. If the marker persists, click *Enrol a new account* and approve as the same Trakt user. That replaces the account's tokens in place, so don't remove the account first. A transient failure clears by itself after the next successful sync, and a token refresh that fails transiently (rate limiting, a Trakt server error or a network failure) doesn't set the marker at all.

*Remove* first opens a confirmation page that warns what removing the account will delete; confirming there unlinks the account and drops its wanted list. On the next Trakt sync, content the service acquired from that account's watchlist that no other linked account wants is deleted. Removing the last linked account therefore removes all watchlist-acquired content. Torrents that were already in your account before the service found them are not affected.

### Validating Trakt parsing against a live account

An interactive integration test (`tests/trakt_integration_test.rs`) exercises every Trakt parser and `sync_trakt` end-to-end against your real Trakt account — no static token needed. Run:

```bash
cargo test --test trakt_integration_test -- --ignored --nocapture
```

The test prints a verification URL and a short code, then waits for you to open the URL and approve it at trakt.tv/activate. Once approved it reads your watchlist, in-progress and watched history (every page), checks that watched shows carry their per-episode watched data, and runs the full `sync_trakt` path, asserting the `wanted` set is populated. Requires `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET`, and `TMDB_API_KEY` (in `.env` or environment); it skips cleanly if any is absent. Tokens are minted on demand via the device flow and kept in a private per-run directory under the build's target dir (`target/tmp/dmm_trakt_integ_<pid>`), which the test deletes when it finishes, including when it fails. If you abort the run (Ctrl-C), delete the directory whose path the test prints.

## Auto-upgrades & consolidation

The upgrade engine runs a quality-improvement pass over your library **once a day by default** (controlled by `UPGRADE_INTERVAL_SECS`, default 86 400 s). It is on by default for every owned title, including copies that were already in your account (with or without Trakt); set `UPGRADE_INTERVAL_SECS=0` to disable it entirely. Only copies still in your debrid account are upgraded. A title whose torrents have all left the account (deleted by you, or lapsed) isn't re-scraped, and a copy that has left is never added back, even one you delete while the pass is running: just before a swap replaces anything, the pass checks the account again and drops the staged release if every copy it would replace is gone. If the account listing fails or comes back empty while titles are known, the pass skips that run.

Each pass covers every owned title (`UPGRADE_BUDGET_PER_TICK` can cap it), with two exceptions. A title the service is still settling waits: while one particular copy of it (the first by infohash) is still downloading or being checked, the whole title is skipped, so a show with one pending episode gets no upgrades until that episode settles. Titles added through the removed `--acquire` command are never upgraded. Titles holding a copy of unknown or unchecked quality (see below) that is still in your account are visited first. A title with nothing left in your account doesn't use up a place under the cap. Under a cap they get half of it (rounded down) and the rest goes to the least recently checked titles, so attention titles that never find a better copy can't crowd out the others.

- **The best copy you already have is the one served:** when your account holds more than one copy of an episode or film and the library is showing a worse one, it switches to the better copy (higher resolution, then source; a copy whose tracks could be checked beats one that couldn't) on the next scan, once nothing is playing — e.g. REMUX seasons you add yourself replace the BluRay pack's files for those seasons, while the pack keeps serving the seasons only it has. This doesn't wait for the daily pass, deletes nothing, and never switches between copies of equal quality. Copies are compared on the file each would serve: a pack whose best files are 1080p BluRay but whose file for this episode is a 480p `.avi` doesn't take the slot. A film is never switched to or from a torrent holding several films, because the library shows every film of the torrent it plays from. It never switches to a copy whose name tags only audio languages other than the title's required one (`AUDIO_LANGUAGE`, resolved per title as for acquisition), the same check the upgrade engine applies before adopting a release. It applies to titles and episodes the service has already chosen a copy for (ones it acquired or upgraded). A title that exists only as copies found in your account has no chosen copy yet, so its largest streamable torrent (or, for a show, every distinct episode file) is shown until the upgrade engine adopts a copy. Two exceptions keep it from undoing the upgrade engine: a checked copy is switched to an unchecked one only for a strictly higher resolution, and with `ALLOW_RESOLUTION_DOWNGRADE` on, nothing is switched to a copy above `MAX_RESOLUTION`. (Jellyfin sees the switched episodes as new files.)
- **Quality upgrades (movies, and shows per-episode):** re-scrapes the title (or, for a show, each owned episode individually) with Torrentio and checks whether a meaningfully-better **cached** release exists, trying ranked candidates in order until one is usable (so a structurally-unusable top candidate — e.g. a mislabeled pack — doesn't block the upgrade). "Meaningfully-better" follows the engine's own quality ranking (resolution dominant within the `MAX_RESOLUTION` ceiling, then source tier): the candidate must score strictly higher **and** make a concrete category jump — uncached→cached, a higher resolution, or a higher source tier at the same resolution (e.g. WEB→BluRay/REMUX). Marginal same-tier-same-resolution differences are never acted on (avoids churn), a lower source tier at the same resolution never counts even when it scores higher (e.g. a WEB x265 release is not taken over a BluRay x264 one for its HEVC bonus), and a higher source tier is **not** chased if it would cost resolution. A candidate must also beat the owned copy when scored the way acquisition ranks it — with the wrong-language penalty for a release whose name or Torrentio flags tag only other audio languages than `AUDIO_LANGUAGE` (for `original`: the title's TMDB original/spoken languages — e.g. `Rus` or 🇷🇺 on an English-original show) — so the upgrade engine never swaps to a release acquisition itself would rank lower (which also rules out any flip-flop); a copy of unknown quality (below) is never replaced by a release carrying that penalty either. Candidates are tried in that same language-aware order, so a release tagged with another language is tried only after the correctly-tagged ones. A staged candidate goes through the same file probe as acquisition, with the same audio rule — but an upgrade only adopts a release whose tracks the probe could actually **check**: a candidate whose tracks can't be read is dropped (not blacklisted), and one whose reported file is a container the probe can't read (e.g. `.avi`) isn't even staged, so an upgrade never swaps a checked copy for an unchecked one. Conversely, an owned copy recorded as *unchecked* (accepted without a track check, or found in your account as a non-MKV/MP4 file) is replaced by a checked cached release of the **same or better** quality at the same or higher resolution, no category jump needed. **Unknown quality counts as the lowest quality:** a copy whose quality isn't known — its name doesn't tag the resolution or the source (e.g. `The Matrix (1999).mkv`, `Show.S01E01.1080p.mkv`), or no quality was ever recorded for it — is replaced by any cached release of **known** quality within the ceiling that passes the probe and isn't tagged only with other audio languages (the best-ranked one is tried first) — but never by a lower resolution than the copy's name does show, so `Show.S01E01.1080p.mkv` is only replaced by a 1080p-or-better release — and is never "replaced" by another unknown one; once replaced, the new copy's quality is known, so the next run leaves it alone. A copy whose file name shows a resolution **above** `MAX_RESOLUTION` is never treated as unknown — it is only ever replaced by the opt-in corrective downgrade below. (Quality is read from the file name, with anything it leaves out filled from the torrent name — `Show (S02) (1080p) (WebDL)` holding `S02E01.mkv` counts as 1080p WEB. Copies found in your account by an older version, recorded without a quality, have it worked out once this way.) When upgrading from an older version, the database migration flags once the copies the engine acquired and serves from a non-MKV/MP4 file; copies found in your account that an older version had already recorded are **not** flagged retroactively — only ones found from this version on. A title owned from before the `original` = original-plus-spoken-languages rule, or found already in your debrid account, has its TMDB languages fetched once — on its first upgrade visit, before anything is probed, for every owned copy and episode of the title (a copy found in your account also gets its TMDB original language, which it never had). If TMDB is temporarily unreachable the title is skipped until the next upgrade run rather than checked against the original language alone; if TMDB refuses the lookup outright (e.g. the title no longer exists there), the languages already on record are used, and with `AUDIO_LANGUAGE=original` a title with none on record (a copy found in your account) is left alone until TMDB answers, since nothing could check a replacement's audio. A rejected `TMDB_API_KEY` counts as TMDB being unreachable. When a better release is found it is staged (added to the debrid account — a release whose torrent is already in your account, e.g. under another title or added by hand, is never staged, so the upgrade pass can't delete it when it rejects it) and the VFS `selection` for the title (or episode) is swapped, but only while nothing has read the copies the swap changes for `UPGRADE_IDLE_SECS` (default 5 minutes), re-checked immediately before the swap so staging latency can't catch a stream that started meanwhile. If TorBox says a torrent the pass has just staged doesn't exist (a 404, which it sometimes answers right after the add), the pass looks again after 1, 2, 4 and 8 seconds before giving that candidate up. The superseded torrent is then pruned. For a film that means every other copy of it within `MAX_RESOLUTION`, so an adopted upgrade leaves one copy of a film whatever `DEDUP_REMOVE_DUPLICATES` says (set `UPGRADE_INTERVAL_SECS=0` to keep several, such as a dub next to the original; the library shows only one copy of a film either way). The replacement takes over the standing of the copy it replaced: a better copy of a torrent you added yourself is treated as yours again, so the Trakt lifecycle never removes it. A copy the service is still downloading or checking is left alone until it settles. A show's owned singleton episodes are re-scored before that season's pack handling runs, so an episode upgraded this tick can still be folded into a pack later the same tick. A single episode is compared against the better of its own quality and the copy that episode currently plays from (e.g. a better season pack you also have), so an upgrade never moves an episode off a better pack.
- **Episodes served from a multi-episode pack (shows):** an episode the library currently plays from a multi-episode pack (e.g. an SD foreign-dub `S01E01-E04` pack grabbed on an air date while the season's other episodes are 1080p singles) is re-scored against the pack's quality and, when a meaningfully-better **cached single-episode** release exists (or the pack is *unchecked* and a checked one of the same or better quality exists), moved to it — staged, probed and idle-gated exactly like a per-episode upgrade. The single must also be at least the pack's source tier **and** resolution — the same test full-season consolidation uses (so e.g. a 720p BluRay pack is not split into 1080p WEB singles). Because an upgrade never lowers a checked copy's source tier at the same resolution — and every copy the upgrade pass adopts is checked — the passes never undo each other: singles split from a pack are never swapped down to ones consolidation would fold back into that pack (e.g. a 1080p WEB x264 pack split into 1080p BluRay x264 singles stays split — the singles aren't traded for higher-scoring WEB x265 ones). The pack itself is deleted only when it no longer serves any episode **and** every episode it contains is held by another settled copy that is still **present** on your debrid account (a copy whose torrent has lapsed off the provider doesn't count); otherwise it is kept. To bound the cost, if the first episode served from a pack has nothing better, the rest of that pack is skipped until the next run, and a release that turns out to be a multi-episode pack is tried at most once per run rather than once per episode. The duplicate cleanup (`DEDUP_REMOVE_DUPLICATES=true`) cooperates: it only removes a torrent in favour of copies of the same or better quality, so better single episodes are kept even when the pack is still kept as the only copy of some other episode. Packs above the `MAX_RESOLUTION` ceiling are left to corrective downgrade (below). A lower-quality full-season pack can end up split into better single episodes; consolidation folds them back into a pack once one at least as good on both source tier and resolution exists — never the pack they were split from, except an *unchecked* pack split into equal-quality checked singles, which consolidation may fold back once if its own probe now checks out (it is then recorded as checked and not split again).
- **Corrective downgrade — lowering the ceiling (opt-in, `ALLOW_RESOLUTION_DOWNGRADE`):** if you lower `MAX_RESOLUTION` below a copy you already own (a movie, a singleton episode, or a whole season pack), that copy no longer conforms — but it is never silently deleted. With `ALLOW_RESOLUTION_DOWNGRADE=false` (the default) the engine only **logs** the correction it would make (a preview). With it set to `true`, the engine replaces the above-ceiling copy with the best within-ceiling **cached** release covering the same content and prunes the old one — idle-gated exactly like a normal upgrade, and only when a conforming cached replacement actually exists (a sole over-ceiling copy with nothing to replace it is left alone rather than lost). A replacement tagged only with audio languages other than `AUDIO_LANGUAGE` is never adopted. For a movie or a single episode it never goes below a within-ceiling resolution you already own, counting a copy whose source isn't tagged: with a 2160p copy and an untagged 1080p copy under a 1080p ceiling, only a cached 1080p release replaces them. A season pack is replaced by the best cached within-ceiling pack covering its episodes, whatever else you own. Only the old pack is deleted, and the next scan switches an episode you also hold in a better within-ceiling copy to that copy.
- **Full-season consolidation (shows, runs after the per-episode pass):** when scattered per-episode torrents exist for a season, the engine looks for a **cached** full-season pack that covers every aired episode (per TMDB) at same-or-better source tier and resolution (a candidate below that isn't even staged). If one is found, the season's episode `selection` slots are repointed to the pack and the individual episode torrents are pruned — idle-gated in the same way. Resolution and source are compared separately: an episode whose resolution or source isn't tagged never blocks a pack on that point (unknown is the lowest), while a pack whose resolution or source isn't tagged never replaces an episode whose quality is known on that point. Partial-season packs are never adopted, and a single-episode release is never tried as a pack. A candidate that turns out to span several seasons or to miss an aired episode isn't tried again until the service restarts. Consolidation itself never trades away quality — it is not a downgrade path. Nor does it adopt a pack tagged only with audio languages other than `AUDIO_LANGUAGE`: it compares packs on source tier and resolution, so it would otherwise not see the language penalty.

A swap or prune waits while a player is reading one of the copies it changes (a read within the last `UPGRADE_IDLE_SECS`). A title being watched is skipped for that pass and retried on the next one, and the pass carries on with the other titles, so a stream of another title, or Jellyfin reading a file a swap has just added, holds nothing back. Read activity is kept in memory, so after a service restart the library counts as **not idle** for the first `UPGRADE_IDLE_SECS` (start-up grace) — no idle-gated swap, prune or removal happens until then.

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
- `src/scheduler.rs`: Spawns the cooperating background jobs (scan loop always; Trakt cycle + episode monitor when Trakt is configured; the upgrade job unless `UPGRADE_INTERVAL_SECS=0`).
- `src/tasks.rs`: Background jobs — the scan loop (polls the active provider, identifies new torrents, updates the VFS) plus the Trakt jobs (`sync_trakt`, `reconcile_wanted`, `monitor_episodes`).
- `src/acquire.rs`: The `AcquisitionEngine` — optimistic add (`acquire`) plus the asynchronous resolver (`observe`) that selects, validates, probes, and finalises each candidate.
- `src/upgrade.rs`: Daily quality-upgrade + full-season consolidation engine (the whole library each pass, copies of unknown or unchecked quality first; a swap or prune waits while a copy it changes is being read).
- `src/reacquire.rs`: `materialise` add→poll→select primitive — used by the instant-repair path (the acquisition engine instead adds optimistically in `acquire` and resolves the verdict later in `observe`).
- `src/scraper.rs`: `Scraper` trait + `TorrentioScraper` — queries a Stremio-compatible addon for candidate streams, paced (`SCRAPER_MIN_INTERVAL_MS`) and backing off on `429`.
- `src/selection.rs`: switches a library slot to a better copy already in the account (the scan loop runs it after dedup).
- `src/release.rs`: Parses candidates into `ReleaseInfo` and scores/ranks them with the quality preferences.
- `src/probe.rs`: Hand-rolled MKV/MP4 track reader — fetches the first 4 MB of a CDN URL to verify audio/subtitle languages, plus one targeted read when the track list lies further in: a trailing MP4 `moov` at its exact offset (falling back to the file's last 4 MB when that read fails), or up to 8 MB from the start of an MKV whose `Tracks` element runs past the first 4 MB.
- `src/read_activity.rs`: In-memory proxy read-activity tracker, per torrent, that holds back swaps, prunes and removals while a file is being read (with a start-up grace).
- `src/trakt_client.rs`: Trakt API client — device-flow OAuth and the watchlist/in-progress/watched reads (paginated; watched shows with per-episode progress). Parser tests use trimmed real Trakt responses in `tests/fixtures/trakt/`, anonymised (fictional titles and ids, shifted dates).
- `src/wanted.rs`: Pure reconcile-core — diffs the wanted-set against owned content and emits acquire/remove decisions (the removal lifecycle rules).
- `src/enrolment.rs`: Local-network Trakt enrolment page (`/trakt/accounts`) served on the WebDAV listener.
- `src/rd_client.rs`: Real-Debrid implementation of `DebridProvider` (1-hour unrestrict cache).
- `src/torbox_client.rs`: TorBox implementation of `DebridProvider` (mylist / requestdl / createtorrent / controltorrent).
- `src/ratelimit.rs`: Adaptive token-bucket rate limiter, one instance each for the Real-Debrid (paced at 250 ms), TorBox (100 ms), Trakt and scraper clients. TMDB has its own fixed 100 ms pacer.
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
- `src/mapper.rs`: Library root (module declarations and the shared `now_unix_secs` helper).

## How It Works

### Background Tasks

A scheduler spawns cooperating periodic jobs. Each runs immediately on startup and then on its own interval, except the upgrade job, which first waits out the `UPGRADE_IDLE_SECS` start-up grace:

1. **Scan Task**: Polls the active debrid provider for new/updated torrents and updates the virtual filesystem
   - Repeats every `SCAN_INTERVAL_SECS` (default: 60 seconds)
   - Builds the VFS consulting the persisted `selection` table (un-managed torrents keep the pre-selection behaviour: a movie's largest streamable torrent, every distinct file name of a show episode)
   - Also runs `observe` to resolve in-flight acquisitions (deferred gates, provides recording, dead-torrent reaping)
   - Also records torrents already in your account as owned (the account mirror), removes a second torrent entry for the same release (same infohash), runs the duplicate-dedup pass (a dry run by default), switches library slots to better copies already present, and once a day notes the records of torrents found in your account that have left it, and drops them when the next scan still doesn't list them (records of content the service acquired are kept, so a title that is still wanted is re-acquired)
   - A provider listing that comes back empty while torrents were known is not trusted until it has been seen three times in a row and has stayed empty for at least 10 minutes, whatever `SCAN_INTERVAL_SECS` is. Until then only `observe` runs and the library is left as it was, while the Trakt reconcile and the episode monitor treat owned titles as still present and remove nothing, so a provider glitch can't empty the library or trigger removals
2. **Trakt Cycle** *(only when Trakt is configured)*: Syncs each enrolled account's watchlist/in-progress + watched state, then reconciles the library: acquires or re-acquires movies, removes finished or abandoned movies, and removes shows abandoned from a watchlist. Repeats every `TRAKT_SYNC_INTERVAL_SECS` (default: 900 seconds)
3. **Episode Monitor** *(only when Trakt is configured)*: Acquires every missing aired episode of a wanted show (a newly watchlisted show's whole run, catch-up episodes and new airings) and performs finished-show removal. Repeats every `TRAKT_EPISODE_CHECK_INTERVAL_SECS` (default: 3600 seconds)
4. **Upgrade Engine** *(on by default; disable with `UPGRADE_INTERVAL_SECS=0`)*: Re-scores owned titles and upgrades them to meaningfully-better cached releases; consolidates scattered per-episode torrents into full-season cached packs. Idle-gated — never swaps or prunes a file being actively streamed. Repeats every `UPGRADE_INTERVAL_SECS` (default: 86 400 seconds / daily)

### On-Demand Repair

There is no background repair loop. Instead, repair is triggered synchronously at playback time:

- When a media file is read, `dav_fs` resolves it through the provider for a fresh CDN URL (a per-file resolution cache makes this free when the content is healthy)
- If resolution reports the file is unavailable (`AppError::Unavailable`), `try_instant_repair` runs synchronously: re-adds the torrent by hash, matches the same file by path, and checks whether the replacement is already cached. Every resolve failure counts as unavailable except a bad or expired API token (401/403) or throttling (429), which fail the read without triggering repair. On Real-Debrid that covers a 5xx, a transport-level failure (timeout/connect, no HTTP status) and a stale or expired-link 404 or other 4xx on unrestrict; on TorBox, any other `requestdl` failure, such as an uncached or expired file. The resolve call fails fast (a small bounded retry budget, a 10s connect timeout and a 15s per-request timeout) rather than retrying for minutes
- A persistent CDN **5xx on the byte fetch** (the file resolved but is broken on the provider) also escalates to the same instant repair, once per read, when the first fetch and its same-URL retry (no re-resolve) both return 5xx. A 5xx that follows another failure (a connection or body error, or a stale-URL re-resolve) is retried once on the same URL; if that retry returns a 5xx too, the read fails without a repair, and the next read can still escalate. So a broken file doesn't cause an endless player retry-storm
- **Cached content** (most common): repair completes in ~1-2 seconds and the replacement file is resolved inline — playback continues after a brief delay. Real-Debrid re-adds the torrent under a new id, so until the next scan rebuilds the library, later opens of any file of that torrent (the same file again, or a sibling episode) go straight to the repaired copy
- A repair that hasn't finished within 30 seconds is abandoned for that read. The torrent stays visible and a later read retries once the 30s cooldown, counted from when the repair was abandoned, is over, so a provider outage can't hold a read for minutes
- **Non-cached content** (Real-Debrid, where the re-add mints a new id): the file returns an error, the old torrent is deleted, and the new torrent is left to download (the scan loop picks it up automatically). On TorBox the re-add by hash returns the **same** torrent, which is left in place and not hidden by the repair. While TorBox reports it as re-downloading it drops out of the library listing, and the scan loop shows it again once the download finishes
- A torrent in the `Broken` state (on the Real-Debrid new-id replacement path, this is the superseded old id) is hidden from WebDAV until the scan loop swaps in its replacement. A torrent mid-repair (`Repairing`) stays listed, and other reads of it fail until the repair settles (a few seconds, at most 30), so a scan that runs mid-repair doesn't remove its files from the library or tell Jellyfin they were deleted. The TorBox same-id re-download is kept `Healthy` (not hidden by the repair), as noted above
- After 3 consecutive failed repair attempts (the count resets on any successful read) a torrent is marked `Failed` and hidden from WebDAV until the service restarts or the torrent leaves the provider listing (e.g. you replace it)

### Jellyfin Notifications

When `JELLYFIN_URL`, `JELLYFIN_API_KEY`, and `JELLYFIN_RCLONE_MOUNT_PATH` are all set, the service notifies Jellyfin of specific changed paths after each VFS update. This uses Jellyfin's `POST /Library/Media/Updated` API to trigger targeted scans of only the affected folders (e.g. a single season directory for a new episode), avoiding full library rescans. Changes from all sources — new torrents, deletions, repairs — are detected automatically.

The client authenticates with the `Authorization: MediaBrowser Token="<JELLYFIN_API_KEY>"` header. Newer Jellyfin releases accept only this scheme by default (they disable the legacy `X-Emby-Token` header and the `api_key` query parameter), and Jellyfin 10.x accepts it too. An API key containing a double quote, backslash or comma is rejected at startup, and notifications then stay disabled.

### Archive-Only Torrents

Some torrents deliver their video inside RAR/ZIP archives rather than as plain `.mkv`/`.mp4` files. Debrid services do not extract these archives, so the contents cannot be streamed. The service detects archive-only torrents (covering `.rar`, `.zip`, `.7z`, `.tar`, `.gz`/`.bz2`, and split-volume `.r00…`/`.s00…` parts) and handles them differently depending on where the torrent came from:

- **Engine-acquired content (Trakt-driven): handled automatically — no manual action needed.** An archive-only release never resolves a streamable video file, so the acquisition resolver (`observe`) treats it like any other dead torrent: once it has stayed unresolved for `ACQUIRE_DEAD_TIMEOUT_SECS` (default 600 s) it is reaped, its hash is **blacklisted** (so the same release isn't tried again for 30 days), and the next-best candidate is acquired in its place. (Archive-ness isn't detected at scrape time — a Stremio stream object carries no such signal — so the engine can still *pick* an archive release, but it is caught and replaced here rather than lingering.)
- **Content you added yourself, or that pre-existed in your debrid account (the account mirror): movies are usually replaced by the upgrade engine, shows need manual resolution.** Such content is recorded as owned and is never blacklisted for being archive-only. A warning is logged once per torrent (later scans log it at debug level):

  ```
  WARN Torrent 'Movie.Name.1080p.BluRay' contains only archive files (RAR/ZIP) and cannot be streamed — replace it with a non-archive version on your debrid service
  ```

  With the upgrade engine on (the default), an archive-only movie counts as unknown quality, so the daily pass replaces it with a cached streamable release when one passes the probe, then deletes the archive torrent. Archive-only show torrents, and movies with no such replacement or no IMDB id, stay until you replace them yourself. To fix one, delete the torrent from your debrid account and add an alternative release whose video files are stored directly (`.mkv`, `.mp4`, etc.).

### Error Handling

- **Unavailable file** (any resolve failure except a bad token (401/403) or throttling (429): a 5xx, a transport failure or a stale-link 404 from Real-Debrid on unrestrict, or an uncached/expired file on TorBox): Triggers synchronous instant repair — succeeds inline for cached content, fails for non-cached
- **429 Rate Limit**: Adaptive token bucket rate limiter shared across the provider's API calls — on 429, the global request interval doubles (max 30s between requests) and Retry-After headers are respected; on success, the interval **halves** back toward the client's baseline (multiplicative recovery, so a throttle burst doesn't leave later requests crawling): 250 ms between requests for Real-Debrid, which documents a limit of 250 requests per minute, and 100 ms for TorBox. A 429 that persists through an on-read resolve fails that read without triggering repair
- **404 Not Found** (Real-Debrid): Treated as success for delete operations (idempotent). TorBox reports a delete of an already-removed torrent as an error. A delete that a record removal depends on always targets a torrent in the current listing, and the other deletes (rolling back a torrent the service has just added, or one an interrupted staging left behind) are retried on a later pass or given up harmlessly
- **Playback Errors**: WebDAV read failures on an unavailable file (or a persistent CDN 5xx) trigger instant repair (rate-limited to 30s cooldown, max 3 consecutive failed attempts per torrent, after which it is hidden until the service restarts or the torrent leaves the provider listing)

### Caching

- **Resolved CDN URLs**: cached to reduce API load — ~1 hour for Real-Debrid, ~3 hours for TorBox (matching each provider's link lifetime)
- **TMDB metadata**: Persisted in the embedded database (`metadata.db`) while the torrent stays in your account, and dropped once it leaves. A torrent whose TMDB search failed (rather than finding nothing) isn't cached: it is shown under its fallback title and identified again after 15 minutes. Once one identification in a scan finds TMDB unreachable, the rest of that scan's new torrents skip TMDB and wait for the same 15-minute retry, so an outage doesn't stall the scan on one long TMDB retry per torrent

## Licence

MIT (see LICENCE). The Docker image also ships THIRD-PARTY-NOTICES, which holds the licence texts of the third-party crates compiled into the binary; it is generated with cargo-about (see CLAUDE.md).
