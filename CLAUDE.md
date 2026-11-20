# CLAUDE.md

Guidance for Claude Code in this repository. This file is a summary and an index. The behaviour contracts, edge cases and incident history are in the pages under `docs/` listed below; read the ones for the code you are about to change.

## What this project does

DebridMovieMapper is a Rust async service that bridges a debrid service (Real-Debrid or TorBox) with media servers such as Jellyfin and Plex. It fetches the torrents in the configured debrid account, identifies them through TMDB, and serves them over WebDAV as a `Movies/` and `Shows/` tree whose media bytes are proxied from the provider's CDN on demand. Exactly one provider is active per deployment, chosen at startup by which token is set.

With Trakt sync configured, each enrolled user's watchlist and in-progress titles are acquired automatically, and a reconciler removes them again (Trigger A: finished; Trigger B: un-watchlisted and wanted by nobody). Without Trakt nothing is acquired from watchlists or watch history, but the account mirror, dedup, the selection repoint, the daily upgrade engine and `observe`'s reap of dead torrents still act on everything already in the account.

## Commands

```bash
cargo build --release

# Run locally. .env is loaded through dotenvy and an exported value wins, even a blank one.
RD_API_TOKEN=<token> TMDB_API_KEY=<key> cargo run
# Smoke run: a test account, a scratch database and the upgrade engine off.
RD_API_TOKEN= TORBOX_API_KEY=<test-account-key> TMDB_API_KEY=<key> DB_PATH=target/smoke.db UPGRADE_INTERVAL_SECS=0 cargo run

cargo test                 # unit tests, no API tokens needed
cargo test <test_name>     # a single test

# Live tests run one binary at a time (the full set is the test gate below).
cargo test --test lifecycle_test -- --ignored --show-output
cargo test --lib -- --ignored    # live TMDB identification tests, read-only, TMDB_API_KEY only
INTEGRATION_TEST_LIMIT=10 cargo test --test integration_test -- --ignored    # read-only

docker build -t debridmoviemapper .
docker compose up -d       # runs the published :latest image, not this checkout

# Release: bump `version` in Cargo.toml, run `cargo build`, commit, then push the matching tag.
git tag vX.Y.Z && git push origin vX.Y.Z
```

A local run is a full instance acting on the configured account: it mirrors every torrent, `observe` reaps and re-acquires, and once the `UPGRADE_IDLE_SECS` grace is over it deletes same-hash duplicates and the copies an upgrade replaces. Never run it against a debrid account another deployment manages.

Never run `cargo test -- --ignored` across all binaries. The ignored tests call live services, several add and delete torrents on the configured debrid account, and `trakt_integration_test` is interactive.

`docs/development.md` has the annotated commands: the `.env` rules, running this checkout under compose, and what a release tag publishes.

## Configuration

Required: `TMDB_API_KEY`, and exactly one of `RD_API_TOKEN` or `TORBOX_API_KEY`. Setting both, or neither, is a startup error.

Optional:

- Service: `SCAN_INTERVAL_SECS`, `DB_PATH`, `PORT`, `CDN_FIRST_READ_MB`, `EPISODE_FILE_NAMES`, `RUST_LOG`
- Jellyfin: `JELLYFIN_URL`, `JELLYFIN_API_KEY`, `JELLYFIN_RCLONE_MOUNT_PATH`
- Acquisition: `MAX_RESOLUTION`, `AUDIO_LANGUAGE`, `SUBTITLE_LANGUAGE`, `PREFER_HEVC`, `PREFER_HDR`, `STALL_TIMEOUT_SECS`, `MAX_ACQUIRE_ATTEMPTS`, `ACQUIRE_DEAD_TIMEOUT_SECS`, `SCRAPER_ADDON_URL`, `SCRAPER_PROXY_URL`, `SCRAPER_MIN_INTERVAL_MS`
- Trakt: `TRAKT_CLIENT_ID` and `TRAKT_CLIENT_SECRET` (sync is off unless both are set), `TRAKT_SYNC_INTERVAL_SECS`, `TRAKT_EPISODE_CHECK_INTERVAL_SECS`, `TRAKT_CATCHUP_LOOKBACK`, `TRAKT_ENROL_ALLOWED_HOSTS`
- Upgrades and cleanup: `UPGRADE_INTERVAL_SECS`, `UPGRADE_BUDGET_PER_TICK`, `UPGRADE_IDLE_SECS`, `DEDUP_REMOVE_DUPLICATES`, `REMOVE_FINISHED_SHOWS`, `ALLOW_RESOLUTION_DOWNGRADE`

`docs/configuration.md` gives each variable's default, limits, parser and fallback; the README table is the user-facing version. `DEDUP_REMOVE_DUPLICATES`, `REMOVE_FINISHED_SHOWS` and `ALLOW_RESOLUTION_DOWNGRADE` default to a dry run that only logs. The upgrade engine is on by default and deletes the copies it replaces, user-added torrents included; `UPGRADE_INTERVAL_SECS=0` turns it off.

A variable the service reads must also be passed through `compose.yml`: the test `compose_passes_every_setting_through_to_the_container` fails otherwise (`docs/modules/config.md` lists the exceptions).

## Architecture

The crate is a binary (`main.rs`) and a library (`mapper.rs` is the lib root). `main.rs` spawns `scheduler::run`, which runs these jobs over the shared `AppState`:

- Scan loop (`tasks::run_scan_loop`, every `SCAN_INTERVAL_SECS`). One tick fetches one provider listing, then runs `engine.observe`, the staging sweep, the same-hash duplicate deletion, identification of new torrents and the VFS update, the account mirror, dedup, the selection repoint and the daily maintenance.
- Trakt cycle (`sync_trakt` then `reconcile_wanted`, every `TRAKT_SYNC_INTERVAL_SECS`) and episode monitor (`monitor_episodes`, every `TRAKT_EPISODE_CHECK_INTERVAL_SECS`). Both are spawned only when Trakt is configured.
- Upgrade job (`upgrade::run_upgrade_once`, every `UPGRADE_INTERVAL_SECS`). It re-scores owned titles, stages better cached releases, swaps the `selection` and prunes the copies they replace, and consolidates per-episode torrents into full-season packs.

On a WebDAV read, `dav_fs.rs` resolves the file's `FileLocator` to a CDN URL through `provider.resolve_url` and proxies the bytes through a read-ahead buffer. When the resolve returns `AppError::Unavailable`, or the CDN keeps answering 5xx, it runs a synchronous instant repair (`repair.rs`, bounded to 30 s) that re-adds the torrent by hash.

State is an embedded redb database (`metadata.db`). All access goes through `store.rs` (`Store`); modules never open transactions inline. The `owned_hashes`, `authoritative_ids`, `selection` and `trakt_tokens` tables are authoritative and exist nowhere else.

`docs/architecture.md` describes the jobs, the repair path, playback and persistence in full.

| Module | Purpose |
|--------|---------|
| `main.rs` | Start-up, `.env` loading, the WebDAV accept loop (connection limit, keepalive, `/trakt` routing), `--healthcheck`, shutdown |
| `config.rs` | `Config`: all env parsing and validation, `TraktConfig`, `load_dotenv` |
| `app_state.rs` | `AppState`: the `Clone` bundle of shared handles the jobs carry |
| `scheduler.rs` | `periodic` / `periodic_after`, and `run`, which spawns the jobs |
| `tasks.rs` | Scan loop (`scan_tick`), account mirror, dedup, reapers and backfills; the Trakt jobs `sync_trakt`, `reconcile_wanted` and `monitor_episodes` |
| `provider.rs` | `DebridProvider` trait, `FileLocator`, provider selection, `MockProvider`, helpers the clients share |
| `rd_client.rs` | Real-Debrid client |
| `torbox_client.rs` | TorBox client |
| `ratelimit.rs` | `AdaptiveRateLimiter`, shared by the provider, Trakt and scraper clients |
| `identification.rs` | File-name cleaning and TMDB scoring that identify a torrent as a movie or a show |
| `tmdb_client.rs` | TMDB search, details, show status and episode air dates |
| `vfs.rs` | In-memory `Movies/` and `Shows/` tree, canonical episode names, and the episode-code parsing (`parse_se_all`) the engine shares |
| `dav_fs.rs` | WebDAV view of the VFS; resolves and proxies CDN byte ranges; triggers instant repair |
| `repair.rs` | Repair state machine and the provider-neutral instant repair |
| `reacquire.rs` | `materialise`: re-add by hash and select files, for the instant repair only |
| `read_activity.rs` | `ReadActivity`: the in-memory read tracker behind the idle gates and the start-up grace |
| `scraper.rs` | `Scraper` trait and `TorrentioScraper`, a paced Stremio-addon client |
| `release.rs` | Release-name parser, `score` / `rank`, `quality_rank`, `is_target_improvement` |
| `probe.rs` | MKV/MP4 track reader that checks audio and subtitle languages from the first bytes of a CDN file |
| `acquire.rs` | `AcquisitionEngine`: optimistic `acquire`, and `observe`, which verifies or rejects `Pending` torrents on every scan tick |
| `selection.rs` | Points `selection` slots at the best copy already in the account |
| `upgrade.rs` | Daily upgrade, corrective downgrade and season consolidation; staging and the staging-intent sweep |
| `wanted.rs` | Pure reconcile core: `reconcile_title` and the Trigger A/B predicates |
| `trakt_client.rs` | `TraktClient`: device-flow OAuth and the paginated list reads |
| `enrolment.rs` | Local-network Trakt enrolment page (`/trakt/accounts`) |
| `jellyfin_client.rs` | Optional Jellyfin change notifications |
| `store.rs` | `Store`: redb tables, schema v8 and migrations, typed accessors |
| `error.rs` | `AppError` (`thiserror`) |
| `mapper.rs` | Library root: module declarations and `now_unix_secs()` |
| `test_log.rs`, `test_loopback.rs` | Test-only log capture and scripted loopback HTTP server |

## Where the detail lives

- `docs/modules/<module>.md`: one page per source module in the table above.
- `docs/design/providers.md`: the provider abstraction, TorBox specifics, `FileLocator` resolution, rate limiting and retry budgets.
- `docs/design/repair.md`: instant repair, and when a broken torrent is hidden.
- `docs/design/identification.md`: how file names become TMDB matches, and what happens during a TMDB outage.
- `docs/design/acquisition.md`: the acquisition engine, optimistic add with `observe`, and the upgrade engine.
- `docs/design/trakt.md`: the wanted set, catch-up, the removal lifecycle (Triggers A and B), availability re-acquire and enrolment.
- `docs/design/account-mirror.md`: how pre-existing torrents become owned records, and what may remove them.
- `docs/design/database.md`: which open failures recreate the database and which fail startup.
- `docs/architecture.md`, `docs/configuration.md`, `docs/development.md`: as referenced above.

`docs/plans/` and `docs/superpowers/` hold the plans and specs of past work.

## Rules that hold across modules

- Destructive passes wait for an idle library. Dedup removals, `execute_remove`, the same-hash duplicate deletion and selection repoints defer while any torrent was read within `UPGRADE_IDLE_SECS` (`ReadActivity::all_idle`); an upgrade swap or prune checks the torrents it changes (`torrents_idle`). For one window after start-up the library counts as not idle.
- A provider listing that comes back empty while torrents were known is suspect (`tasks::listing_is_suspect`). On one, the scan loop runs only `observe`, the Trakt jobs plan no removal and the upgrade pass does nothing.
- Nothing adds a hash that is already in the account. `acquire` and every upgrade staging site check a provider listing first, because on TorBox the add returns the existing torrent's id and a later rejection would delete that torrent. Each add also claims a staging intent (`Store::claim_staging_intent`), so the mirror never adopts a torrent that is still being checked and the staging sweep can delete one a cancelled job left behind.
- Account-mirror records (empty provenance: torrents the user added) are owned and upgradeable like engine copies. The Trakt lifecycle removes only engine-acquired hashes, except a show finish-removed under `REMOVE_FINISHED_SHOWS`. Dedup, an upgrade that supersedes the copy and `observe`'s dead-status reap can still remove a mirror copy.
- A copy with `Manual` provenance (legacy records only) is never removed by the Trakt lifecycle or dedup, and the upgrade engine skips its whole title (invariant M-1).
- Unknown quality (none recorded, or an untagged resolution or source tier) ranks lowest, but a known resolution is a floor, and a copy above `MAX_RESOLUTION` is never unknown: replacing it is a corrective downgrade behind `ALLOW_RESOLUTION_DOWNGRADE`.
- The upgrade engine adopts only a release whose probe returns `VerifyResult::Pass`, so it never swaps a checked copy for an unchecked one.
- A Trakt read that fails on any page, or sees the list change mid-read, fails as a whole, and the user's previous `wanted` set is kept. A watchlisted title is never finish-removed.
- Episode codes are parsed in one place, `vfs::parse_se_all`, so the VFS and the engine agree on which file supplies which episode.
- A read fails over to repair only on `AppError::Unavailable` or a persistent CDN 5xx. A 401/403 or a 429 on resolve fails the read without repair.
- Logs never carry a token, the token-bearing scraper URL or a CDN URL.

## Development process

Test-driven development is required. For a feature or a bug fix, write a failing test first that captures the expected behaviour, write the minimum code to make it pass, refactor while the tests stay green, and run `cargo test` after every change.

Before every commit, run the lint gate:

```bash
cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings
```

If `cargo fmt --check` fails, run `cargo fmt`; do not hand-format around it. CI lints on the latest stable Rust, so keep the local toolchain current (`rustup update stable`). `rust-version` in Cargo.toml, the Dockerfile `FROM` line and the CI `msrv` job's toolchain are bumped together.

Then run the test gate. Do not commit if any test fails.

```bash
# Blank the token of a provider whose account is inactive first, e.g. `export RD_API_TOKEN=`
# (a blank exported value beats .env). A set token the provider rejects fails the gate.
# --show-output prints a passing test's stderr, so a cleanup WARNING (see below) is visible.
cargo test --locked \
  && cargo test --test lifecycle_test -- --ignored --show-output \
  && cargo test --test probe_integration_test -- --ignored --show-output \
  && cargo test --test repair_integration_test -- --ignored --show-output \
  && INTEGRATION_TEST_LIMIT=10 cargo test --test integration_test -- --ignored
```

The live binaries share the provider accounts and their rate limits, so they run one after another. `lifecycle_test`, `probe_integration_test` and `repair_integration_test` add and delete a Creative-Commons *Sintel* torrent in whatever account the token names and clean up after themselves; if a `WARNING: live-test cleanup may have left a torrent behind` line appears on stderr, delete the torrent it names by hand. `integration_test` only reads the account. A provider whose token is unset or blank is skipped.

When adding or changing a feature, update the integration tests to cover the new behaviour. They are the final gate before a commit.

When a commit changes Cargo.lock, run `cargo audit --deny warnings` and regenerate THIRD-PARTY-NOTICES with cargo-about 0.9.3, the version CI pins. The file is generated, never hand-edited, and CI diffs it:

```bash
cargo about generate --offline --locked -o THIRD-PARTY-NOTICES about.hbs
```

Keep the documentation current in the same commit as the code. A change a user can see goes in `README.md`. A behaviour change goes in the module's page under `docs/modules/` and, where one covers it, the `docs/design/` page. A setting goes in `docs/configuration.md` and the README table. This file is loaded into every session, so it stays a summary of about 2,000 words: change it only for a new module, page, command, setting or cross-cutting rule, in a line or two, and put the detail in `docs/`.

Don't share a `CARGO_TARGET_DIR` between git worktrees of this repo. The root package's artifacts get identical names, so one worktree's `cargo test` can run a test binary built from another worktree's sources.

`docs/development.md` describes each live test binary, the supplementary test files, the interactive Trakt test, the toolchain pins, the advisory audit workflow and the notices check.
