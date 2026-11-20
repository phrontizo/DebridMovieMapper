# Configuration

Every environment variable the service reads, with its parser, limits and fallbacks.
The README's environment-variable table is the user-facing version.

## Required environment variables

- A debrid provider token — exactly one of:
  - `RD_API_TOKEN` — Real-Debrid API token
  - `TORBOX_API_KEY` — TorBox API token

  Set one or the other, not both.
  Setting **both** is a startup error; setting **neither** is a startup error.
- `TMDB_API_KEY` — TMDB API key

## Optional

- `SCAN_INTERVAL_SECS` (default: 60, minimum: 10) — how often to poll the active debrid provider
- `DB_PATH` (default: `metadata.db`) — path to the redb database file
- `PORT` (default: 8080) — WebDAV server listen port
- `CDN_FIRST_READ_MB` (default: 8, clamped 2–16) — size (MB) of the FIRST read-ahead CDN fetch when a media file is opened.
  Front-loads the container header + early index so a player's open/ffprobe burst is served from one cold fetch instead of several round-trips (the dominant on-demand-streaming startup cost).
  Subsequent reads use the normal `BUFFER_SIZE` (2 MB) window.
  Set `2` to disable the enlargement (= prior behaviour).
  Wired via `DebridFileSystem::with_first_read_bytes`.
- `EPISODE_FILE_NAMES` (default: `canonical`) — how the VFS names episode files.
  `canonical` serves `Show (Year) - S01E02.mkv`, built from the episode the service reads the file as (see `docs/modules/vfs.md`), so Kodi, Jellyfin and Plex read the same episode; Kodi's default rules otherwise read more episodes out of an episode title such as `Inside the Episode S2 E1`.
  `original` serves the torrents' own file names.
  A file the service can't number keeps its own name either way.
  Switching renames the library's episode files: every media server sees them as new files, and Kodi, which keys watched state by path, loses its watched marks unless it syncs them (e.g. through Trakt).
  Parsed by `config::parse_episode_file_names` (anything else falls back to `canonical` with a warning).
  Because the default is `canonical`, upgrading a deployment from a version without it renames its episode files once; set `EPISODE_FILE_NAMES=original` before upgrading to keep the old names.
  The database records the naming the VFS last served (`Store::served_episode_names`, a `meta` key; none recorded reads as original names), and a restart pre-populates the VFS in it (`tasks::prepopulate_vfs`), so the first scan after a change diffs every renamed folder and tells Jellyfin, then records the new naming.
- `RUST_LOG` (default: `info`) — log verbosity via a `tracing-subscriber` `EnvFilter` directive (the `env-filter` feature is enabled).
  Unset → `info` (unchanged behaviour); a malformed directive falls back to `info` rather than crashing startup.
  Scope it to this crate to avoid dependency noise, e.g. `RUST_LOG=debridmoviemapper=debug`, or target one module: `RUST_LOG=debridmoviemapper::acquire=debug,info`.
  Takes effect on (re)start.
  The acquisition lifecycle is instrumented for `debug`: `acquire` (scrape count, ranked top-3 with cached/resolution/source/seeders, the candidate added, idempotent/lapsed/NoAcceptableRelease outcomes), `observe` (per-tick reason a Pending torrent is still waiting — absent/no-files/selecting/downloading/probe-deferring — plus reaps at `warn` and `Verified` at `info`), `scraper` (candidate counts; never the token-bearing URL), and `tasks` (Trakt wanted-set sizes, reconcile op counts).
  `dav_fs` playback resolves log at `trace`, since they fire on every file open (e.g. `RUST_LOG=debridmoviemapper::dav_fs=trace`; never the CDN URL).
- `JELLYFIN_URL` — Jellyfin server URL (e.g. `http://jellyfin:8096`)
- `JELLYFIN_API_KEY` — Jellyfin API key
- `JELLYFIN_RCLONE_MOUNT_PATH` — rclone mount path as seen by Jellyfin (e.g. `/media`)

## Optional — Acquisition quality preferences (SP1)

(full descriptions in `README.md`'s env-var table)

- `MAX_RESOLUTION` (default: `1080`) — hard resolution ceiling for acquisition (`720`/`1080`/`2160`, each also accepted with a `p` suffix, or the `2160` synonyms `4k`/`uhd`); candidates above it are excluded.
- `AUDIO_LANGUAGE` (default: `original`) — required audio language: a 2-/3-letter ISO code or `original` (TMDB's original language **plus its listed `spoken_languages`** — any of them is acceptable, for both ranking and the probe; a title with no spoken languages listed behaves exactly as before, original only.
  An explicit ISO code ignores spoken languages entirely).
  A value that is not a language code (a full name, a locale, a 2-letter code `probe::is_known_language_code` doesn't know, `off`/`any`/`all`, or `und`/`mul`, which the probe knows but no parsed track carries: `config::NO_TRACK_CODES`) falls back to `original` with a warning.
  An unknown 3-letter code is used as written, with a warning (`config::parse_lang_code`), so a misspelt one gets every release rejected and blacklisted for 30 days.
  `no` is Norwegian and `na` is Nauruan; each is kept, with a warning that names the 3-letter code (`nor`, `nau`) to write instead (`config::MISREADABLE_CODES`).
- `SUBTITLE_LANGUAGE` (default: unset = `none`) — required subtitle language (ISO code), or `none`/`off`/unset to skip the check.
  Any other value goes through the same validation as `AUDIO_LANGUAGE`, and a rejected one falls back to `none`.
  A no-language code (`xx`/`zxx`, `probe::is_no_language`) is refused too and falls back to `none` with a warning (`SubReq::parse`).
  For the audio it turns the check off, but the probe's subtitle check has no such exemption, so taken as written it would reject and blacklist every release.
- `PREFER_HEVC` (default: `true`) / `PREFER_HDR` (default: `false`) — scoring preferences for HEVC and HDR/Dolby-Vision encodes.
- `STALL_TIMEOUT_SECS` (default: 1800, minimum: 60) — seconds without download progress before a Pending torrent is re-acquired.
- `MAX_ACQUIRE_ATTEMPTS` (default: 5, minimum: 1) — how many of the top-ranked candidates one acquisition pass tries to add, moving to the next only when the add itself fails (an add the provider reports busy, e.g. TorBox at its active-download limit, stops the acquisition instead; see `docs/modules/acquire.md`).
  A release later rejected by `observe` (wrong title or audio, corrupt, stalled, dead) is blacklisted and the next-best candidate is tried, so over time every non-blacklisted candidate may be tried.
- `SCRAPER_ADDON_URL` (default: auto-templated from the provider token) — override the Torrentio/Stremio addon base URL.
  A pasted addon manifest URL (`…/manifest.json`) is accepted; `TorrentioScraper::build_base_url` drops the suffix.
- `SCRAPER_PROXY_URL` (default: unset) — route ONLY the scraper's traffic through an HTTP(S) proxy (a non-http(s) scheme is a startup error); CDN/TMDB/provider stay direct.
- `SCRAPER_MIN_INTERVAL_MS` (default: 1000, clamped 100–60000) — minimum spacing between scraper requests.
  Torrentio sends no rate-limit headers and answered 429 once ~80–100 requests arrived within a minute (the hourly episode check can scrape 100–200 episodes back to back), so every scrape is paced through the scraper's own `AdaptiveRateLimiter`; a 429 doubles the interval (honouring `Retry-After`) and is retried, up to 3 requests per scrape, before surfacing as the retriable error.
  Lower it for a self-hosted addon.

## Optional — Trakt sync (SP2)

- `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET` — both required to ENABLE Trakt sync (from a Trakt API app).
  If either is absent, Trakt sync is disabled (setting only one of them logs a warning naming the missing variable, e.g. `TRAKT_CLIENT_ID is set but TRAKT_CLIENT_SECRET is not; Trakt sync is disabled`) and nothing is acquired from watchlists or watch history (`observe` still deletes and blacklists a torrent Real-Debrid reports dead, and replaces it when its record names a movie or an episode).
  The scan loop's account mirror, best-copy selection repoint, same-hash duplicate deletion (always on) and dedup (dry-run by default) still run over everything already in the account, and so does the daily upgrade engine (on by default), which adds better cached releases and deletes the copies they replace, user-added torrents included.
  `UPGRADE_INTERVAL_SECS=0` stops that.
- `TRAKT_SYNC_INTERVAL_SECS` (default: 900, minimum: 60) — Trakt sync + reconcile cadence.
- `TRAKT_EPISODE_CHECK_INTERVAL_SECS` (default: 3600, minimum: 300) — episode-monitor cadence.
  (Note this exact env var name — it carries the `TRAKT_` prefix for consistency.)
- `TRAKT_CATCHUP_LOOKBACK` (default: unset = all-time) — **catch-up** lookback window.
  A show you've *watched* (not just watchlisted/in-progress) that Trakt reports as behind is auto-acquired, but only if you last watched it within this window: every aired regular episode not watched by all of its catch-up wanters (`reconcile_title` skips the intersection of their watched sets), including episodes that aired before the last watch.
  The default all-time window therefore covers every partly watched show in the user's whole history, which can be a large number of torrents.
  Accepts a number followed (optionally after spaces, case-insensitively) by a unit: `d`/`day`/`days`, `w`/`wk`/`week`/`weeks`, `m`/`mo`/`mon`/`month`/`months` (30 days; `m` is a month, not a minute) or `y`/`yr`/`year`/`years` (365 days), e.g. `90d`, `12w`, `6mo`, `6m`, `6 months`, `1y`; a bare number = days; `0`/unset = all-time (every watched show with unwatched aired episodes qualifies).
  Anchored on Trakt's per-show `last_watched_at`.
  Any other value is treated as all-time (logged at warn with examples of the accepted forms), never a startup failure.

  **Operational cost:** each catch-up candidate costs one Trakt `show_progress` call (plus a TMDB `show_status` call) per sync cycle, so an all-time window over a large watch history issues that many extra requests every `TRAKT_SYNC_INTERVAL_SECS` (the shared rate limiter paces them, so it's absorbed, not an error).
  Set a bounded lookback (e.g. `90d`/`6mo`) to scope the catch-up set — and the per-cycle API load — to recently-watched shows.

  **Note:** because it gates the shared per-show `show_progress` check, this window *also* scopes `REMOVE_FINISHED_SHOWS` finish-cleanup — a show last watched outside the window is not a finish-removal candidate (see that option).
- `TRAKT_ENROL_ALLOWED_HOSTS` (default: unset) — extra host names accepted in the `Host` header of the `/trakt` enrolment routes, on top of IP literals, `localhost`/`*.localhost`, single-label names and `.local`/`.lan`/`.internal`/`.home.arpa` names.
  Any other `Host` gets `421 Misdirected Request` (a DNS-rebinding guard, checked before the CSRF guard, for every method; a request with no `Host` header is allowed).
  A reverse proxy must also forward the browser's `Host` header unchanged, port included (nginx: `proxy_set_header Host $http_host;`).
  The CSRF guard compares it with the browser's `Origin` and evens out only a default port, so a rewritten Host gets every POST refused with 403.
  The guard's warn line logs the Origin, Host and Sec-Fetch-Site it compared.
  Parsed by `config::parse_allowed_hosts` into `TraktConfig.enrol_allowed_hosts`.

  Accounts are linked via the local-network enrolment page (`/trakt/accounts` on the WebDAV listener), not a static list.

## Optional — Upgrade engine (SP3)

- `UPGRADE_INTERVAL_SECS` (default: 86400 = daily; `0`, or the boolean off-words `off`/`false`/`no`, disables; minimum 600 when non-zero; any other non-number keeps the daily default and warns that the engine is on, `config::parse_upgrade_interval`) — how often to run the quality-upgrade + full-season consolidation pass.
  On by default; set to `0` to disable entirely.
- `UPGRADE_BUDGET_PER_TICK` (default: unset = no cap, minimum: 1) — cap on the owned titles re-scored per upgrade pass.
  Unset, every pass visits every eligible title (see `docs/modules/upgrade.md` for the titles it skips): measured on the live library (312 titles), that is ~490 scrapes (~8 min at the 1 s scraper pacing), up to ~2,300 only if every multi-episode pack's sample episode had a better single-episode release (a season pack listed under the sample's query doesn't count: phases (1) and (2) drop a stream whose name plainly names several episodes before ranking, `upgrade::is_pack_stream`, and one that only turns out to be a pack when staged is staged once and the rest of the pack is skipped).
  The default used to be 20, which put a full rotation at ~16 days, so a copy the pass would replace (e.g. an SD Russian-dub pack serving an English show's episode) waited up to two weeks for its turn.
  Either way titles needing attention go first (`upgrade::needs_attention`, judged on the copies still in the provider account: a `Verified` copy with no recorded quality, an unknown quality — `release::is_unknown_quality` — or `quality.unverified`), each group least-recently-checked first; under a cap they get half of it (rounded down) and the rest goes to the least-recently-checked remaining titles (`upgrade::upgrade_batch`), so attention titles that never find a better copy can't starve the others.
  A title with no copy left in the account is stamped and takes no slot.
- `UPGRADE_IDLE_SECS` (default: 300, minimum: 30) — seconds without a proxy read before a copy may be swapped, pruned or removed.
  The upgrade engine checks only the torrents a step changes (`ReadActivity::torrents_idle`): a swap or prune waits while a player has read, within the window, a torrent it deletes or takes an episode away from (`upgrade::copies_serving`: every owned copy of the affected episodes, or of the film, plus what their selection slots point at), and a title with a copy being read is skipped for that pass (cursor unchanged) while the pass moves on to the next title.
  It used to check the whole library, so on 2026-11-12 Jellyfin probing the file a swap had just added (about 75 s after the swap's notification) stopped the rest of the daily pass.
  Dedup removals, `execute_remove`, the scan loop's same-hash duplicate deletion and `selection::repoint_to_better_copies` still check the whole library (`all_idle`).

  **Start-up grace:** read activity is in-memory only, so right after a (re)start there is no read history — for the first `UPGRADE_IDLE_SECS` after the process starts the library counts as **not idle**, so every idle-gated deletion defers instead of firing on the first tick (a redeploy once deleted 81 torrents 34 s after start-up).
  The upgrade job therefore waits out the grace before its first pass (`scheduler::periodic_after` with `upgrade_first_run_delay`), rather than running a start-up pass that would defer every title and leave the next one a full `UPGRADE_INTERVAL_SECS` away (a service restarted at least daily would otherwise never upgrade).
- `ACQUIRE_DEAD_TIMEOUT_SECS` (default: 600, minimum: 120) — seconds an optimistically-added `Pending` torrent may remain unresolved before `observe` reaps it as dead, blacklists the hash, and re-scrapes.
- `DEDUP_REMOVE_DUPLICATES` (default: unset/`false`) — when `true`/`1`/`yes`/`on`, the duplicate-dedup pass actually **deletes** redundant torrents from the provider.
  Default is a **dry-run** that only logs the plan (`dedup: …` lines) so it can be reviewed first.
  Removal is additionally idle-gated (`UPGRADE_IDLE_SECS`, incl. its start-up grace) so it never interrupts an active stream.
  A copy is only ever removed in favour of copies of the **same or better quality** (quality first — resolution, then source tier, then checked before unchecked, an untagged axis counting as the lowest; see `docs/modules/tasks.md`); pre-existing account-mirror copies get no protection.
  Only settled (`Verified`) copies take part — a `Pending` acquisition is neither removed nor allowed to displace one.
  A title with an owned record added within `UPGRADE_IDLE_SECS` (`tasks::fresh_dedup_groups`, e.g. an upgrade stage in flight) is left out of the plan, dry-run log included, until the window passes.
  This flag doesn't govern the scan loop's same-hash duplicate deletion, which is always on (see `docs/modules/tasks.md`).
  Nor does the dry run keep several copies of a film: an adopted movie upgrade prunes every within-ceiling owned copy of the film (`upgrade::supersede_hashes`), editions and dubs under the same TMDB id included, whatever this flag says.
  Only `UPGRADE_INTERVAL_SECS=0` stops that.
- `REMOVE_FINISHED_SHOWS` (default: unset/`false`) — when `true`, a fully-watched **Ended** show that isn't watchlisted is auto-removed (Trigger-A finish cleanup), **including shows already in the library**.
  Default `false` only **logs** the finish-removal candidates (`trakt: <user> — N finished+ended owned show(s) would be removed (preview …)`) without removing — review the preview, then enable.
  (Mechanism: `sync_trakt` folds the finished+ended owned shows — those Trakt's per-show progress reports caught-up — into the wanted-set only when this is on, so the episode monitor's (`monitor_episodes`) Trigger-A finish removal fires on its next run (every `TRAKT_EPISODE_CHECK_INTERVAL_SECS`, idle-gated).
  Trigger A and the acquire-side guard are decided by `user_finished` over the **TMDB** aired set (regular episodes — Specials/season 0 are ignored, see `wanted::SPECIALS_SEASON`) against Trakt's per-episode watched set (read with `extended=progress` — without it every watched set was empty and nothing was ever finish-removed), so removal fires and the acquire path acquires nothing **when TMDB and Trakt agree on the aired episodes**; if TMDB lists an aired episode Trakt's watched set lacks, the show reads as not-fully-watched, is kept, and catch-up acquires that genuinely-unwatched episode — consistent, not a phantom acquire.
  A watchlisted show is never finish-removed.
  **Scoped by `TRAKT_CATCHUP_LOOKBACK`:** the finish-cleanup candidate set is built from the same per-show `show_progress` check that the catch-up lookback window bounds, so a bounded lookback *also* scopes finish-cleanup — a fully-watched Ended owned show last watched **outside** the window is neither previewed nor removed.
  This coupling is deliberate: decoupling it would reintroduce an unbounded per-cycle `show_progress` call for every owned watched show.
  With the default all-time lookback every finished+ended owned show is considered.)
- `ALLOW_RESOLUTION_DOWNGRADE` (default unset/`false`) — when `true`/`1`/`yes`/`on`, the upgrade engine may perform a **corrective downgrade**: when `MAX_RESOLUTION` is lowered below an owned copy, it replaces that above-ceiling copy with a ceiling-conforming CACHED release and prunes the higher-res copy (idle-gated, and only when such a replacement exists — a sole over-ceiling copy with no conforming cached release is kept).
  It applies to engine-acquired copies (acquired under a previously-higher ceiling) and pre-existing account-mirror copies alike: a mirrored over-ceiling file has its real resolution recorded (older mirror records stored `quality: None` and are re-derived from their file names — see `docs/design/account-mirror.md`), so it is **never treated as unknown-lowest quality** — replacing it is always this gated corrective downgrade.
  Default is a **dry-run** that only logs the planned downgrades (`upgrade: … would downgrade …`).
  Pure quality upgrades are unaffected by this flag.
  For a movie or a singleton episode it never goes below a within-ceiling resolution the title already owns, a copy whose source is untagged included (`upgrade::within_ceiling_floor`): owning a 2160p copy and an untagged 1080p copy under a 1080p ceiling, only a cached 1080p release replaces them, never a 720p one.
  An over-ceiling season pack (`correct_over_ceiling_packs`) has no such floor: it is replaced by the best cached within-ceiling pack covering its episodes, whatever else the show owns.
  Only the old pack is pruned, and the scan loop's selection repoint then moves a slot to any better within-ceiling copy the show already owns for that episode.
  An over-ceiling pack holding a video file with no episode code that its record doesn't account for (`upgrade::copies_holding_codeless_files`) is not corrected at all: pack correction skips it before scraping, since keeping it beside a replacement would stage another pack on every visit.
  A pack the engine acquired whose record maps that file to an episode in its `provides` (`vfs::codeless_episode_files`, e.g. `Season 04/E09 - Title.mkv` under an explicit season) is corrected like any other.
