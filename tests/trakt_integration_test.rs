//! Interactive LIVE Trakt integration test (#[ignore]). Drives the device-flow against the real
//! Trakt API: prints a URL + code, pauses until you authorise it at the URL, then validates every
//! read parser and runs sync_trakt end-to-end against your real account.
//!
//! REQUIRES: TRAKT_CLIENT_ID, TRAKT_CLIENT_SECRET (a Trakt API app) and TMDB_API_KEY. Skips cleanly
//! if any is unset. RUN WITH --nocapture so the prompt is visible:
//!   cargo test --test trakt_integration_test -- --ignored --nocapture

mod common;

use debridmoviemapper::enrolment::poll_to_completion;
use debridmoviemapper::store::Store;
use debridmoviemapper::tasks::sync_trakt;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::trakt_client::{TraktClient, TraktClientImpl};
use debridmoviemapper::vfs::MediaType;
use std::path::PathBuf;
use std::sync::Arc;

/// The temp directory holding the test's live Trakt token database. Dropping it deletes the
/// directory with everything in it (the database and any `.corrupt` sibling), so the tokens are
/// removed when an assertion fails or the test panics, too. (Not on Ctrl-C/kill: the process exits
/// without unwinding, which is why the test prints the path first.)
struct TempDb(PathBuf);

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The directory the test's live Trakt tokens are kept in: one per run, inside Cargo's per-target
/// scratch dir (`CARGO_TARGET_TMPDIR`, under the git-ignored target dir) rather than the shared
/// system temp dir, where other local users could read the tokens.
fn token_db_dir() -> PathBuf {
    std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dmm_trakt_integ_{}", std::process::id()))
}

/// Creates `dir` so that only the current user can read it (mode 0700 on Unix; an existing
/// directory, e.g. from an aborted run with a reused pid, is tightened too).
fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[test]
fn token_db_dir_is_private_and_outside_the_shared_temp_dir() {
    // The live tokens must not land in the shared system temp dir, where other local users can
    // read them and where an aborted (Ctrl-C) run would leave them behind.
    let dir = token_db_dir();
    assert!(
        dir.starts_with(env!("CARGO_TARGET_TMPDIR")),
        "{} should be under Cargo's per-target scratch dir",
        dir.display()
    );
    assert_ne!(
        dir.parent(),
        Some(std::env::temp_dir().as_path()),
        "{} should not sit directly in the shared temp dir",
        dir.display()
    );

    // A separate directory from the live test's, so the two never touch each other's files.
    let probe = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dmm_trakt_integ_selftest_{}", std::process::id()));
    let _cleanup = TempDb(probe.clone());
    create_private_dir(&probe).expect("create the private dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&probe).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "the token dir should be readable by its owner only"
        );
    }
}

#[tokio::test]
#[ignore]
async fn trakt_live_device_flow_and_sync() {
    let (Some(client_id), Some(client_secret), Some(tmdb_key)) = (
        common::token("TRAKT_CLIENT_ID"),
        common::token("TRAKT_CLIENT_SECRET"),
        common::token("TMDB_API_KEY"),
    ) else {
        eprintln!("skipping: set TRAKT_CLIENT_ID, TRAKT_CLIENT_SECRET, TMDB_API_KEY to run");
        return;
    };

    let trakt: Arc<dyn TraktClient> = Arc::new(TraktClientImpl::new(
        client_id,
        client_secret,
        reqwest::Client::new(),
    ));
    let tmdb = TmdbClient::new(tmdb_key).expect("tmdb client");
    let dir = token_db_dir();
    // Declared before `store`, so it is dropped (and the directory deleted) after the store closes.
    let _temp_db = TempDb(dir.clone());
    create_private_dir(&dir).expect("create the token db dir");
    let path = dir.join("trakt.redb");
    let path_str = path.to_str().expect("temp path is valid UTF-8").to_string();
    let store = Store::open(&path_str).expect("store");
    eprintln!(
        "Live Trakt tokens are stored in {} (deleted when the test ends; \
         delete it yourself if you abort the run)",
        dir.display()
    );

    let dc = trakt.device_code().await.expect("device_code");

    eprintln!("\n========================================");
    eprintln!("  Authorise this test on Trakt:");
    eprintln!("  1. Open: {}", dc.verification_url);
    eprintln!("  2. Enter code: {}", dc.user_code);
    eprintln!("  Waiting up to {}s for you to approve...", dc.expires_in);
    eprintln!("========================================\n");
    use std::io::Write;
    let _ = std::io::stderr().flush();

    let slug = poll_to_completion(&trakt, &store, dc.device_code, dc.interval, dc.expires_in)
        .await
        .expect("device authorisation (did you approve it in time?)");
    eprintln!("Authorised as: {}", slug);

    let tokens = store
        .get_trakt_tokens(slug.clone())
        .await
        .expect("tokens persisted");

    let wl = trakt.watchlist(&tokens.access).await.expect("watchlist");
    let ip = trakt
        .in_progress(&tokens.access)
        .await
        .expect("in_progress");
    let watched = trakt.watched(&tokens.access).await.expect("watched");
    let me = trakt.me(&tokens.access).await.expect("me");
    eprintln!(
        "reads OK: watchlist={} in_progress={} watched_movies={} watched_shows={} (user={})",
        wl.len(),
        ip.len(),
        watched.movies.len(),
        watched.shows.len(),
        me.username
    );

    // Since Trakt's 2026-07-03 API change, watched SHOWS carry their episodes only when requested
    // with `extended=progress` (and every list read paginates). A watched history that has shows
    // but no watched episode anywhere means that request regressed — every finish-removal and
    // catch-up decision would then be wrong (Trigger A never fires; watched episodes re-acquired).
    if !watched.shows.is_empty() {
        let with_eps = watched
            .shows
            .iter()
            .filter(|s| !s.watched_episodes.is_empty())
            .count();
        eprintln!(
            "watched shows with per-episode data: {}/{}",
            with_eps,
            watched.shows.len()
        );
        assert!(
            with_eps > 0,
            "watched shows must carry their watched episodes (extended=progress)"
        );
        // Every live `extended=progress` episode carries `last_watched_at`. Without it the parser
        // still succeeds, but `tasks::build_wanted` can no longer tell a paused re-watch (a pause
        // on an episode watched before it) from a first-watch pause, so a re-watch acquires nothing.
        let without_times = watched
            .shows
            .iter()
            .filter(|s| !s.watched_episodes.is_empty() && s.episode_watched_at.is_empty())
            .count();
        eprintln!(
            "watched shows with episodes but no per-episode last_watched_at: {}",
            without_times
        );
        assert_eq!(
            without_times, 0,
            "watched episodes must carry last_watched_at (extended=progress); the paused re-watch \
             detection reads it"
        );
    }

    // A playback show's `paused_at` is the other half of the re-watch detection. A show paused
    // only on a special, or with no readable `paused_at`, legitimately carries none, and a parsed
    // item can't tell that apart from a renamed field, so this only warns.
    let ip_shows: Vec<_> = ip
        .iter()
        .filter(|i| i.media_type == MediaType::Show)
        .collect();
    let with_pause = ip_shows.iter().filter(|i| !i.paused.is_empty()).count();
    eprintln!(
        "in-progress shows with a paused regular episode: {}/{}",
        with_pause,
        ip_shows.len()
    );
    if !ip_shows.is_empty() && with_pause == 0 {
        eprintln!(
            "WARNING: no in-progress show carries a paused episode. Expected only when every \
             playback show is paused on a special; otherwise Trakt may have dropped or renamed \
             `paused_at`, and the paused re-watch detection would stop working"
        );
    }

    sync_trakt(&trakt, &tmdb, &store, None, false).await;
    let wanted = store.all_wanted().await;
    eprintln!("sync_trakt wrote {} wanted row(s)", wanted.len());
    if !wl.is_empty() || !ip.is_empty() {
        assert!(
            !wanted.is_empty(),
            "sync_trakt should populate `wanted` when the account has watchlist/in-progress items"
        );
        // every wanted row should belong to the enrolled user
        assert!(
            wanted.iter().all(|r| r.user == slug),
            "wanted rows must be keyed by the enrolled slug"
        );
    }
}

/// Offline check of `TempDb` (no network; plain `cargo test`): a panicking test still deletes the
/// token directory, with the token file and its `.corrupt` sibling in it.
#[test]
fn temp_db_removes_the_token_file_when_the_test_panics() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dmm_trakt_tempdb_check_{}", std::process::id()));
    create_private_dir(&dir).unwrap();
    let path = dir.join("trakt.redb");
    let corrupt = dir.join("trakt.redb.corrupt");
    std::fs::write(&path, b"tokens").unwrap();
    std::fs::write(&corrupt, b"tokens").unwrap();

    let guarded = dir.clone();
    let outcome = std::panic::catch_unwind::<_, ()>(move || {
        let _temp_db = TempDb(guarded);
        panic!("an assertion failed");
    });
    assert!(outcome.is_err());
    assert!(!path.exists(), "the token file should be deleted");
    assert!(!corrupt.exists(), "the .corrupt sibling should be deleted");
    assert!(!dir.exists(), "the token directory should be deleted");
}
