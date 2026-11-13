//! Interactive LIVE Trakt integration test (#[ignore]). Drives the device-flow against the real
//! Trakt API: prints a URL + code, pauses until you authorise it at the URL, then validates every
//! read parser and runs sync_trakt end-to-end against your real account.
//!
//! REQUIRES: TRAKT_CLIENT_ID, TRAKT_CLIENT_SECRET (a Trakt API app) and TMDB_API_KEY. Skips cleanly
//! if any is unset. RUN WITH --nocapture so the prompt is visible:
//!   cargo test --test trakt_integration_test -- --ignored --nocapture

use debridmoviemapper::enrolment::poll_to_completion;
use debridmoviemapper::store::Store;
use debridmoviemapper::tasks::sync_trakt;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::trakt_client::{TraktClient, TraktClientImpl};
use std::path::PathBuf;
use std::sync::Arc;

/// The temp database holding the test's live Trakt tokens. Dropping it deletes the file and its
/// `.corrupt` sibling, so the tokens are removed when an assertion fails or the test panics, too.
struct TempDb(PathBuf);

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut corrupt = self.0.clone().into_os_string();
        corrupt.push(".corrupt");
        let _ = std::fs::remove_file(corrupt);
    }
}

#[tokio::test]
#[ignore]
async fn trakt_live_device_flow_and_sync() {
    dotenvy::dotenv().ok();

    let client_id = match std::env::var("TRAKT_CLIENT_ID") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            eprintln!("skipping: set TRAKT_CLIENT_ID, TRAKT_CLIENT_SECRET, TMDB_API_KEY to run");
            return;
        }
    };
    let client_secret = match std::env::var("TRAKT_CLIENT_SECRET") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            eprintln!("skipping: set TRAKT_CLIENT_ID, TRAKT_CLIENT_SECRET, TMDB_API_KEY to run");
            return;
        }
    };
    let tmdb_key = match std::env::var("TMDB_API_KEY") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            eprintln!("skipping: set TRAKT_CLIENT_ID, TRAKT_CLIENT_SECRET, TMDB_API_KEY to run");
            return;
        }
    };

    let trakt: Arc<dyn TraktClient> = Arc::new(TraktClientImpl::new(
        client_id,
        client_secret,
        reqwest::Client::new(),
    ));
    let tmdb = TmdbClient::new(tmdb_key).expect("tmdb client");
    let path = std::env::temp_dir().join(format!("dmm_trakt_integ_{}.redb", std::process::id()));
    let path_str = path.to_str().expect("temp path is valid UTF-8").to_string();
    // Declared before `store`, so it is dropped (and the file deleted) after the store closes.
    let _temp_db = TempDb(path);
    let store = Store::open(&path_str).expect("store");

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
/// token file and its `.corrupt` sibling.
#[test]
fn temp_db_removes_the_token_file_when_the_test_panics() {
    let path = std::env::temp_dir().join(format!(
        "dmm_trakt_tempdb_check_{}.redb",
        std::process::id()
    ));
    let mut corrupt = path.clone().into_os_string();
    corrupt.push(".corrupt");
    std::fs::write(&path, b"tokens").unwrap();
    std::fs::write(&corrupt, b"tokens").unwrap();

    let guarded = path.clone();
    let outcome = std::panic::catch_unwind::<_, ()>(move || {
        let _temp_db = TempDb(guarded);
        panic!("an assertion failed");
    });
    assert!(outcome.is_err());
    assert!(!path.exists(), "the token file should be deleted");
    assert!(
        !std::path::Path::new(&corrupt).exists(),
        "the .corrupt sibling should be deleted"
    );
}
