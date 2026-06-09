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
use std::sync::Arc;

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

    let trakt: Arc<dyn TraktClient> =
        Arc::new(TraktClientImpl::new(client_id, client_secret, reqwest::Client::new()));
    let tmdb = TmdbClient::new(tmdb_key).expect("tmdb client");
    let path =
        std::env::temp_dir().join(format!("dmm_trakt_integ_{}.redb", std::process::id()));
    let path_str = path.to_str().expect("temp path is valid UTF-8").to_string();
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

    let tokens = store.get_trakt_tokens(slug.clone()).await.expect("tokens persisted");

    let wl = trakt.watchlist(&tokens.access).await.expect("watchlist");
    let ip = trakt.in_progress(&tokens.access).await.expect("in_progress");
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

    sync_trakt(&trakt, &tmdb, &store).await;
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

    // Best-effort temp-file cleanup.
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.corrupt", path_str));
}
