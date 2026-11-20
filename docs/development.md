# Development

The lint gate and the test gate commands are in `CLAUDE.md`, under "Development process".
This page holds the notes behind them, and the annotated commands.

## Commands

```bash
# Build
cargo build --release

# Run locally. The binary loads .env from the working directory (or a parent) through dotenvy, and
# an exported value wins, even a blank one. Setting both provider tokens is a startup error, so with
# a TorBox key in .env, blank RD_API_TOKEN or use the TORBOX_API_KEY form below.
# A .env value containing spaces must be quoted (TRAKT_CATCHUP_LOOKBACK="6 months"): a line dotenvy
# can't parse stops startup with a Configuration error.
# A local run is a full instance acting on the configured account: it mirrors every torrent, observe
# reaps and re-acquires, and once the UPGRADE_IDLE_SECS grace is over the same-hash duplicate
# deletion and the upgrade pass delete torrents (the upgrade pass stages releases and prunes the
# copies they replace). Never run it against a debrid account another deployment manages. For a
# smoke run use a test account, UPGRADE_INTERVAL_SECS=0 and a scratch DB_PATH under target/ (or
# named metadata.db*), where .gitignore covers it: the file holds the account's library and any
# Trakt tokens enrolled during the run.
RD_API_TOKEN=<token> TMDB_API_KEY=<key> cargo run
RD_API_TOKEN= TORBOX_API_KEY=<test-account-key> TMDB_API_KEY=<key> DB_PATH=target/smoke.db UPGRADE_INTERVAL_SECS=0 cargo run

# Run unit tests (no API tokens needed)
cargo test

# Run a single test
cargo test <test_name>

# Live integration tests: run them per binary, as in the pre-commit gate in CLAUDE.md.
# Never run `cargo test -- --ignored` across all binaries: #[ignore] tests call live services, several
# add and delete torrents on the configured debrid account, and trakt_integration_test is interactive.
# They need TMDB_API_KEY plus RD_API_TOKEN and/or TORBOX_API_KEY; a test whose provider token is unset
# or blank skips. Blank the token of a provider whose account is inactive, e.g. prefix them with
# RD_API_TOKEN= (see the gate). --show-output prints a passing test's stderr, where a cleanup WARNING goes.
cargo test --test lifecycle_test -- --ignored --show-output

# Live-TMDB identification regression tests in the library crate (read-only; TMDB_API_KEY only)
cargo test --lib -- --ignored

# Read-only live identification and VFS test against every configured provider
# (INTEGRATION_TEST_LIMIT, read only by integration_test, caps the torrents identified per account)
INTEGRATION_TEST_LIMIT=10 cargo test --test integration_test -- --ignored

# Acquisition is triggered by Trakt sync + the reconciler (the temporary --acquire CLI was
# removed in SP2). Configure TRAKT_CLIENT_ID/TRAKT_CLIENT_SECRET and enrol accounts at
# /trakt/accounts to drive acquisition. Without Trakt nothing is acquired from watchlists or watch
# history, but the account mirror, dedup, the selection repoint and the upgrade engine (unless
# UPGRADE_INTERVAL_SECS=0) still act on what is already in the account, and observe still replaces
# a torrent Real-Debrid reports dead when its record names a movie or an episode (a dead
# account-mirror show torrent, whose record names no episode, is deleted with no replacement).

# Docker (single platform, local)
docker build -t debridmoviemapper .

# Release: bump `version` in Cargo.toml to X.Y.Z, run `cargo build` (it refreshes Cargo.lock), commit,
# then push a matching tag. release.yml rejects a tag that differs from Cargo.toml's version. It builds
# and pushes to ghcr.io (and to Docker Hub when its secret is set); a pre-release tag (vX.Y.Z-rc.N)
# publishes only its own version tag, never moves :latest, and creates a GitHub pre-release that is
# never marked Latest. Pushes to main / v2 publish :edge / :v2edge.
git tag vX.Y.Z && git push origin vX.Y.Z

# Start full stack (WebDAV + rclone + Jellyfin)
mkdir -p rclone && chown 65534:65534 rclone
# compose.yml runs the published :latest image (the last release tag), not the image built above.
# To run this checkout, add a compose.override.yml (git-ignored) with
# `services: { debridmoviemapper: { build: ., image: debridmoviemapper } }` and run
# `docker compose up -d --build`. To run the published v2 development build instead, override only
# `image:` there, with ghcr.io/phrontizo/debridmoviemapper:v2edge.
docker compose up -d
```

## Lint gate

**Before every commit, run the lint gate (fmt + clippy, as in CI's `lint` job; that job also checks THIRD-PARTY-NOTICES, see below):**

The command is in `CLAUDE.md`, under "Development process".

CI runs these on the latest stable Rust (`dtolnay/rust-toolchain@stable`), and each new stable can add clippy lints — so keep your local toolchain current (`rustup update stable`), otherwise lint can pass locally yet fail in CI on a lint your older clippy doesn't have.
Cargo.toml's `rust-version` (1.96) matches the Dockerfile builder (`rust:1.96-slim`), so clippy's `incompatible_msrv` lint fails the lint job on a std API newer than the image's compiler.
CI's `msrv` job runs `cargo check --locked --all-targets` on that same toolchain (`dtolnay/rust-toolchain@1.96`), so a dependency or language feature newer than the builder fails the pull request, before it can break the post-merge Docker build.
Cargo.toml also sets `resolver = "3"`, so `cargo update` and Dependabot prefer dependency versions compatible with `rust-version`.
Bump the three together: `rust-version`, the Dockerfile `FROM` line and the `msrv` job's toolchain.
If `cargo fmt --check` fails, run `cargo fmt` to fix; do not hand-format around it.
When the commit touches Cargo.lock, also run the lint job's notices check (it writes to a scratch directory, never the repo):
```bash
cargo about generate --offline --locked -o "${TMPDIR:-/tmp}/THIRD-PARTY-NOTICES" about.hbs \
  && diff -u THIRD-PARTY-NOTICES "${TMPDIR:-/tmp}/THIRD-PARTY-NOTICES"
```

## Advisory audit

**Advisory audit:** `.github/workflows/audit.yml` runs `cargo audit --deny warnings` (RustSec vulnerabilities, yanked and unmaintained crates) on every push and pull request to main and v2, and weekly (Mondays 06:00 UTC).
The weekly run uses main's copy of the file, so it audits both lines' lockfiles only once the file is on main.
It is a separate workflow, so an advisory never blocks lint, test or publish.
Dependabot alerts cover only main's Cargo.lock, and dependabot.yml takes effect only from main.
Run `cargo audit --deny warnings` locally after a dependency change.

## Test gate

**Before every commit, run all unit and integration tests.**
The core gate runs for every configured provider:

The command is in `CLAUDE.md`, under "Development process".

Integration test binaries must run sequentially (not `-- --ignored` on all at once) because they share the live provider accounts and their rate limits, and the ignored set also holds the interactive Trakt test and full-library scans.
The live provider tests skip a provider whose token is unset or blank, so configure only the providers you hold a live account for and blank a lapsed token (dotenvy doesn't override an exported value).
`repair_integration_test` repairs live on both providers.
The `protect_id` path (a transient failure after a same-id re-add) is covered only by `MockProvider` unit tests in `repair.rs`.
Do not commit if any test fails.
(CI and the Docker build run with `--locked` — if you bump `Cargo.toml`, run `cargo test --locked` / `cargo build --locked` locally too so a stale `Cargo.lock` is caught before CI rather than silently updated.)
- `lifecycle_test` is the cross-provider add→appears→delete→disappears check, plus per-provider `lifecycle_acquire_sintel_by_imdb_real_debrid` / `lifecycle_acquire_sintel_by_imdb_torbox` tests (sharing `run_acquire`) that exercise the SP1 `AcquisitionEngine` end-to-end against each live provider.
  With both tokens set, both providers' acquire tests run; they don't go through `choose_provider`, which refuses that combination.
  It runs against **both** Real-Debrid and TorBox, **modifying the live account** (adds and deletes a Creative-Commons *Sintel* torrent, cleaning up after itself).
  Each provider's sub-test **skips** if its token (`RD_API_TOKEN` / `TORBOX_API_KEY`) is unset or blank, so it is safe to run with only one provider configured.
  The binary's tests hold a shared mutex (`LIVE_ACCOUNT`), so the default parallel harness never has two of them adding and deleting Sintel in one account, and they clean up through `common::with_cleanup`, which also runs when an assertion fails.
  A cleanup first deletes the torrent ids the body recorded (`common::AddedIds`: `add_magnet`'s id, or for `run_acquire` the ids of listed torrents its store owns), then sweeps by hash with up to 3 listings 2 s apart (`common::delete_ids_then_sweep`), so a lagging TorBox listing doesn't leave a Sintel copy behind.
  It never deletes a torrent twice: an id the body deleted itself (the lifecycle delete step, a release `observe` rejected, the original a Real-Debrid repair replaced) is dropped from `AddedIds` (`common::forget_added_id`), and a torrent the cleanup deleted that a lagging listing still shows is only listed again, since TorBox answers a delete of an id already deleted with `500 DATABASE_ERROR`, which its client retries five times.
  It prints `WARNING: live-test cleanup may have left a torrent behind: …` on stderr naming any hash or id it couldn't confirm gone (one it deleted is marked "deleted but still listed").
  `probe_integration_test` and `repair_integration_test` use the same `cleanup_added`.
  Before it adds anything, each `lifecycle_test` test runs `common::preclean_sintel_candidates`.
  It scrapes Sintel (`common::SINTEL_IMDB`, tt1727587) with the provider's Torrentio scraper (or `SCRAPER_ADDON_URL`), lists the account, and deletes every torrent with `SINTEL_HASH` and every scraped Sintel release whose name mentions Sintel, naming each in a WARNING on stderr.
  A scraped release whose name does not mention Sintel is only named, never deleted.
  Without this, a Sintel release a killed acquire run left stayed in the shared account for good: the next run's fresh store does not own it, so its acquire skips it and adds another release.
  A failed scrape still deletes `SINTEL_HASH`, and a failed listing still sweeps it.
  The live gate adds and deletes Sintel in whatever account the token names, and a deployment sharing that account mirrors the copy (and notifies Jellyfin) while the tests run.
  If a cleanup WARNING appears, delete the named torrent by hand; that deployment's mirror reaper then drops its record on its next daily prune.
  Better still, run the gate against a separate test account. libtest captures a passing test's stderr, so these WARNINGs show only with `--show-output` (which the gate passes) or `--nocapture`.
  Each `lifecycle_acquire_sintel_by_imdb_*` test builds its VFS from the acquired torrent alone and asserts that torrent is under `Movies/`; with a provider token set, a missing `TMDB_API_KEY` or a TMDB failure fails it.
  Right after an add, TorBox's backend can be briefly inconsistent: the listing may leave out a torrent whose own lookup already reports `downloaded` (the same lag behind its spurious `mylist?id=` 404s).
  So the "appears in get_torrents()" step polls the listing every 2 s for up to 30 s (`common::poll_until_listed`), and its failure message says whether the listings errored or left the torrent out; `find_ids_by_hash` returns the listing error instead of reading it as no torrent.
- `repair_integration_test` runs against every configured provider (each provider's tests skip when its token is unset or blank) and **modifies the live account**.
  `test_instant_repair_of_sintel_real_debrid` / `_torbox` add the Creative-Commons *Sintel* torrent, wait until the listing shows it (`common::poll_until_listed`, since TorBox's listing lags an add and its re-add recovers an existing hash through the listing), mark it broken, instant-repair it and delete every Sintel copy afterwards, also when the test fails.
  On Real-Debrid the repair re-adds the hash under a new id, and the test checks that the original is deleted.
  On TorBox the re-add returns the existing torrent, and the test checks that the repaired torrent kept its id and still exists (its lookup retries TorBox's spurious `mylist?id=` 404).
  Neither test ever repairs one of the account's own torrents.
  `test_broken_torrents_hidden_from_webdav_real_debrid` / `_torbox` only read the account, through `common::ReadOnlyProvider`.
- `integration_test` runs against every configured provider (Real-Debrid with `RD_API_TOKEN`, TorBox with `TORBOX_API_KEY`; a blank token counts as unset), skips when neither is set, and needs `TMDB_API_KEY`.
  It only reads the account: every call goes through `common::ReadOnlyProvider`.
  `INTEGRATION_TEST_LIMIT=N` caps the torrents identified in each account.
  Each provider's identifications are cached through `Store` at `$CARGO_TARGET_TMPDIR/integration_test_cache_<provider>.redb` (`real-debrid` or `torbox`, since torrent ids are per provider), never in the service's `metadata.db`.
  A failed TMDB search (`tmdb_client::track_search_failures`) fails the run and is never cached, as in the scan loop, and a torrent cached as unidentified is identified again on every run, so a fallback cached during an outage heals.
  An identified torrent whose cached file list holds no streamable video file (a TorBox torrent cached while it listed `files: null`, or while its cache had lapsed) has its files looked up again on each run.
  A fresh list that holds one replaces the cached list, and the identification is kept.
  A lookup that fails or still lists none keeps the cached entry, so the completeness check never covers such a torrent with no files for good.
  A torrent lookup that answers 404 is retried after 1, 2 and 4 s (TorBox's spurious `mylist?id=` 404s).
  Its `test_vfs_completeness` checks the VFS against what the build is meant to show with no managed selection: one copy per movie, and each distinct episode file name once per season folder of a show (the build it checks serves original names).
  `test_vfs_canonical_names` builds the same library with canonical episode names and fails when an episode the original names show (by its first `SxxEyy` code) has no file in that season folder, or a canonical name's code names another season; the offline `canonical_names_lose_no_episode_of_the_sample_library` runs the same check.
  A copy counts as hidden only when the season folder it belongs in already shows a file of that name, so a build that drops `Season 02/01.mkv` is not excused by `Season 01/01.mkv`.
  The test restates the build's season rule in `season_folder`, and the offline `season_folder_agrees_with_the_build` pins it to the build's placement.
  Other copies count as hidden by design, and selected video files without a positional link (selected-file/link-count mismatch) are counted separately and tolerated up to 20%; a torrent with no links at all (TorBox, which resolves by id) counts as streamable.
  Any other missing file fails it.
  `check_completeness`, the cache rules and the 404 retry are unit-tested offline in the same file, against a fictional `FakeAccount`.
- `probe_integration_test` probes a cached *Sintel* file through BOTH providers' CDNs and asserts the probe reaches a verdict without deferring, guarding the 206/200 range-response handling.
  It runs RD + TorBox, each skips if its token is unset or blank, and it cleans up after itself.

## Supplementary test files

**Additional integration test files** (all `#[ignore]`, require API tokens): `test_all_rd_torrents.rs`, `test_short_titles.rs`, `test_media_generation.rs`, `video_player_simulation.rs`, `trakt_smoke_test.rs`, `sp1_live_test.rs` (live Torrentio scraper smoke, incl. `scraper_live_through_http_proxy` which validates `SCRAPER_PROXY_URL` against a real proxy — negative control + positive case, the latter skipped when the proxy isn't routable).
These are supplementary and not part of the pre-commit gate.
`cargo test --lib -- --ignored` runs the 10 live-TMDB identification regression tests in `src/identification.rs` (short-title disambiguation, collection-prefix stripping; the absolute-episode pack check is the offline `a_dash_coded_season_pack_is_identified_by_its_show_name`).
They are read-only and need only `TMDB_API_KEY` (they panic without it rather than skip); run them when changing `identification.rs`.
They load `.env` through `crate::config::load_dotenv()`, so a `.env` it can't load fails them.
`test_all_rd_torrents.rs` and `test_media_generation.rs` skip when `RD_API_TOKEN` is unset or blank.
`video_player_simulation.rs` only reads the account: every provider call goes through `common::ReadOnlyProvider`, which refuses `add_magnet`/`select_files`/`delete_torrent`, so the instant repair a failed read would trigger can't re-add (TorBox) or delete (Real-Debrid) a library torrent; such a read fails instead.
Helpers shared by the live test binaries (`SINTEL_HASH`, `SINTEL_IMDB`, `ReadOnlyProvider`, `load_env`, `token`, `tmdb_client`, `find_ids_by_hash`, `INFO_NOT_FOUND_BACKOFF` (the 1/2/4 s 404 retry passed to `torrent_info_retrying_not_found`), the per-provider `label`/`token_var`/`client`/`provider_token`/`provider` (the token lookup with its skip line, and the client construction), `poll_until_listed`, `cleanup_by_hash`/`delete_by_hashes`/`cleanup_added`/`delete_ids_then_sweep` (with `AddedIds`/`record_added_id`/`forget_added_id`), `with_cleanup`, `preclean_sintel_candidates` (with the pure `sintel_leftovers`), `add_sintel_downloaded`, which takes an `&AddedIds` and records the new id right after the add, `encode_path_preserve_slashes`) live in `tests/common/mod.rs`.
Every live test binary loads `.env` through `common::load_env` / `common::token` (enforced by `config::tests::live_tests_load_env_only_through_load_dotenv`, which also scans `src/*.rs` bar config.rs), so a `.env` line dotenvy can't parse fails the live tests instead of letting a test whose token sits below it skip and pass.

## Interactive Trakt test

**Interactive Trakt live integration test** (`tests/trakt_integration_test.rs`): validates every Trakt parser and `sync_trakt` end-to-end against a real account without requiring a pre-existing token.
Run with `cargo test --test trakt_integration_test -- --ignored --nocapture`; the test prints a URL + code, waits until you open it and approve on trakt.tv/activate, then exercises all reads, asserts that watched shows carry their per-episode watched data (the `extended=progress` regression guard) and that the `wanted` set is populated.
It also asserts that every watched show with watched episodes carries their per-episode `last_watched_at` (`WatchedShow.episode_watched_at`), and warns when no in-progress show carries a paused regular episode (`paused_at`).
The paused re-watch detection in `tasks::build_wanted` reads both.
Requires `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET`, and `TMDB_API_KEY`; skips cleanly if any is unset.
The enrolled account's live tokens are kept in a per-run 0700 directory under Cargo's `CARGO_TARGET_TMPDIR` (not the shared system temp dir).
The test prints its path and deletes it when the test ends, including on a panic; delete it yourself if you abort the run with Ctrl-C.

## THIRD-PARTY-NOTICES

**THIRD-PARTY-NOTICES is generated, never hand-edited.** cargo-about (`cargo install --locked cargo-about@0.9.3 --features cli`, the version CI pins; see below) builds it from `about.toml` (accepted licences in preference order, the release targets x86_64/aarch64-unknown-linux-musl, dev-dependencies excluded, the crate itself skipped via `publish = false`) and the plain-text template `about.hbs`.
Regenerate it whenever Cargo.lock changes, including after a Dependabot cargo bump: `cargo about generate --offline --locked -o THIRD-PARTY-NOTICES about.hbs`.
CI enforces it: the `lint` job in ci.yml and release.yml installs cargo-about 0.9.3 (`taiki-e/install-action`), regenerates the file into `$RUNNER_TEMP` and `diff -u`s it against the committed one, so a Dependabot cargo PR fails until the regenerated file is pushed and a release with stale notices stops.
Regenerate with the same cargo-about version as the CI pin; when you upgrade cargo-about, bump the pin in both workflows and the install command above in the same commit.
It reproduces upstream licence files byte for byte, so `git diff --check` reports trailing whitespace and CRLF lines in it; that is expected.
