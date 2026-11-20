# Architecture

## What this project does

DebridMovieMapper is a Rust async service that bridges a debrid service — **Real-Debrid or TorBox** — with media servers like Jellyfin and Plex.
It fetches torrents from the configured debrid account, identifies them via TMDB metadata, and exposes a WebDAV endpoint serving proxied media files (the actual `.mkv`/`.mp4` bytes are fetched from the provider's CDN on demand).
Exactly one provider is active per deployment, selected at startup by which token is set (`RD_API_TOKEN` or `TORBOX_API_KEY`).

Optionally, the library can be **driven by users' Trakt accounts** (SP2): when Trakt sync is configured, each enrolled user's watchlist and in-progress titles auto-acquire content, and a reconciler lifecycle-removes it.
A title acquired from a watchlist is removed once it is un-watchlisted and nobody wants it (Trigger B; Trakt drops watched items from the watchlist).
A title wanted only as in-progress is removed (Trigger A) only when every in-progress wanter has finished it and it is still wanted, which in practice means a show folded back in by the `REMOVE_FINISHED_SHOWS` finish-cleanup.
A title still in `/sync/playback` never reads as finished, since the user is partway through it (a paused re-watch included).
Trakt drops a finished title from `/sync/playback`, so an in-progress-only movie, or a show with `REMOVE_FINISHED_SHOWS` off, is kept after it is finished.
Without Trakt nothing is acquired from watchlists or watch history, but the account mirror, dedup, the best-copy selection repoint and the daily upgrade engine still run over everything already in the account.
`observe` also still deletes and blacklists any owned torrent Real-Debrid reports as `magnet_error`/`dead`/`error`/`virus` (mirror and `Manual` records included, provided the record has an IMDB id) and replaces it with a freshly scraped release when the record names a movie or an episode.

The project is structured as both a binary (`main.rs`) and a library (`mapper.rs` as lib root).

## Background tasks (the scheduler model, SP2)

`main.rs` spawns `scheduler::run`, which spins up cooperating independent jobs over the shared `AppState`.
The Trakt/episode/upgrade jobs are each driven by the `scheduler::periodic` primitive (run immediately, then `interval` after each tick completes — a fixed-DELAY cadence — until shutdown; the upgrade job uses `periodic_after`, whose first run waits out the read-activity start-up grace); the scan loop instead runs under `supervise` with its own internal cadence and shutdown-aware awaits:

- **Scan loop** = `run_scan_loop` (behaviour-preserving): it loads the persisted matches, pre-populates the VFS from them (in the episode naming the previous process served, `prepopulate_vfs`) and then calls `scan_tick` every `SCAN_INTERVAL_SECS` (the tests drive `scan_tick` directly).
  On a prune tick the blacklist and `upgrade_checks` cursor prune runs first.
  One tick then fetches one provider listing (a re-acquisition `observe` triggers fetches its own) and runs, in order: acquisition verification (`engine.observe`), the staging sweep (`upgrade::sweep_staging_intents`), the same-hash duplicate deletion (`dedup_torrents_by_hash`), the re-fetch of cached identifications that list no video file (`refresh_fileless_infos`: 5 min after a torrent is first noticed, then hourly, at most 10 per tick), identification of new torrents via TMDB + `update_vfs`, `record_mirror_owned`, `dedup_owned`, `selection::repoint_to_better_copies`, and the daily maintenance (the mirror reaper and `backfill_missing_imdb_ids`).
  A suspect empty listing stops the tick after `observe`.
  See `docs/modules/tasks.md`.
  Implemented in `tasks.rs`.
- **Trakt cycle** = `sync_trakt` then `reconcile_wanted` (sequentially, so the reconciler sees the just-synced wanted set), every `TRAKT_SYNC_INTERVAL_SECS`.
  Spawned **only** when Trakt is configured.
- **Episode monitor** = `monitor_episodes` (every `TRAKT_EPISODE_CHECK_INTERVAL_SECS`).
  Spawned only when Trakt is configured.
- **Upgrade job** = `run_upgrade_once` (every `UPGRADE_INTERVAL_SECS`, default 86400s/daily, the first pass once the `UPGRADE_IDLE_SECS` start-up grace is over; gated on `UPGRADE_INTERVAL_SECS > 0`): re-scores every eligible owned title (not a `Manual` one, nor one whose representative record is still `Pending`; a title with no owned copy left in the provider listing is skipped with its cursor stamped and takes no budget slot, and a title with a copy being read is skipped for that pass with its cursor unchanged; see `docs/modules/upgrade.md`) (copies of unknown or unchecked quality first; `UPGRADE_BUDGET_PER_TICK` caps it) — including **unknown-quality** copies (no recorded quality, or an untagged resolution/source tier), which rank lowest and are replaced by any known-quality cached within-ceiling release that passes the probe, at or above the copy's resolution when that is tagged (a known resolution stays a floor) — stages meaningfully-better CACHED releases (trying ranked candidates in order until one is usable — in-tick fall-through, like the pack/consolidation paths), and performs a swap of the `selection` (held back while a player reads a copy it changes) + prune of the superseded torrents (for a movie, every within-ceiling owned copy of the film, `upgrade::supersede_hashes`, and an over-ceiling one too only with `ALLOW_RESOLUTION_DOWNGRADE` on; for shows: per singleton episode, and per episode served from a within-ceiling multi-episode pack); also consolidates scattered per-episode torrents into full-season cached packs (no quality regression).
  Disable by setting `UPGRADE_INTERVAL_SECS=0`.
  Implemented in `upgrade.rs`.

The Trakt cycle + episode monitor are gated on `scheduler::trakt_jobs_enabled` (a Trakt client **and** `config.trakt` both present); otherwise only the scan loop and (unless `UPGRADE_INTERVAL_SECS=0`) the upgrade job run.
Nothing is acquired from Trakt then, but both still act on everything already in the account (mirror, dedup, selection repoint, same-hash duplicate deletion, upgrade swaps and prunes, and `observe`'s dead-status reap, which replaces the release when its record names a movie or an episode).
The local-network Trakt enrolment routes (`/trakt/accounts`) are served on the existing WebDAV listener.

## On-demand (synchronous, during WebDAV file reads)

- **Repair:** Triggered when a media file is read and `provider.resolve_url` returns `AppError::Unavailable` (RD maps every unrestrict failure except a 401/403 or a 429 to it: a 5xx, a transport failure, a stale-link 4xx such as 404, or a missing link), **or** when the CDN returns a persistent **5xx** on the byte fetch (the file resolved but is broken on the provider, not just a stale URL — once the first fetch and its same-URL retry (no re-resolve) both 5xx (a 5xx that follows another failure, a connection or body error or a stale-URL re-resolve, is retried once on the same URL; if that retry 5xxs too, the read fails without a repair, since no attempt is left for a repaired URL, and the next read can still escalate), the byte-fetch path escalates to the same instant repair, once per read, bounded by the repair cooldown).
  For cached torrents, repair completes inline (~1-2s delay); for non-cached torrents, the file fails and a new torrent is left to download.
  A persistent 5xx is no longer an endless player retry-storm.
  A 429 on resolve fails the read without repair.
  The whole inline repair is bounded to 30 s (`repair::INSTANT_REPAIR_TIMEOUT`): its provider calls use the background retry budgets, so during an outage it could otherwise hold the read for minutes.
  A timed-out repair returns `RepairFailure::Deferred` and leaves the torrent visible with its attempt budget reset and its 30 s cooldown restarted when the repair is abandoned (`RepairManager::note_abandoned_repair`; the timeout equals the cooldown, so a cooldown counted from the start would already be over and the next read would run another 30 s repair at once).
  So does a repair cut short by a dropped read.
  In either case a new-id torrent the repair added but had not adopted is deleted in the background.

## Data flow for playback

1. Jellyfin/player opens a media file via WebDAV
2. `dav_fs.rs` resolves the file's `FileLocator` to a CDN URL via `provider.resolve_url(&FileLocator)` (cached).
   Real-Debrid resolves via the locator's restricted `link` (1h cache); TorBox resolves by `(torrent_id, file_id)` via `requestdl` (no per-file link; ~3h cache).
3. `ProxiedMediaFile` fetches bytes from the CDN URL with a 2MB read-ahead buffer (the FIRST fetch per open uses the larger `CDN_FIRST_READ_MB` window) and serves them to the player.
   A bounded `206` body is read to its natural end so reqwest returns the TCP+TLS connection to the keep-alive pool — the next window reuses it instead of re-handshaking; a Range-ignoring `200` (whole-object) is still read window-bounded then dropped (we can't drain gigabytes).

## Persistence

Embedded `redb` database (`metadata.db`), created automatically on first run.
Some tables can be rebuilt: `matches` (the TMDB identification cache), `blacklist` (TTL'd; losing it only lets rejected releases be tried again), `upgrade_checks` (the upgrade cursor), `wanted` (rebuilt by the next Trakt sync) and `staging` (open staging intents of upgrade candidates and acquisitions; losing it only forgets adds cut off before they were checked or recorded, which the account mirror may then adopt).
The rest are authoritative and exist nowhere else: `owned_hashes` (provenance, `provides`, quality), `authoritative_ids`, `selection` and `trakt_tokens`.
Deleting the file loses every Trakt enrolment and the lifecycle provenance: engine copies are re-adopted as empty-provenance mirror records that the Trigger A/B lifecycle won't remove, and managed selections fall back to the VFS default.
All `redb` access goes through `store.rs` (the `Store` type); modules never open transactions inline.

## Static linking / scratch Docker image

The Dockerfile builds with musl targets (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) producing a minimal final image on `scratch`.
The runtime image's `/etc/ssl/certs/ca-certificates.crt` is unused today (reqwest's `rustls-tls` trusts the compiled-in webpki-roots); it is copied only so a later switch to native roots keeps working on scratch.
