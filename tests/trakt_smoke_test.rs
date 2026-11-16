//! Live Trakt smoke test (requires a Trakt API app, `#[ignore]`).
//!
//! Proves the SP2 Trakt OAuth start + read endpoints work against the real `api.trakt.tv`:
//!   1. `device_code` — starting the device flow needs no user approval, so it validates the
//!      OAuth start + `TRAKT_CLIENT_ID` on its own.
//!   2. `watchlist` — only possible with a pre-authorised access token, so it is exercised
//!      ONLY when `TRAKT_ACCESS_TOKEN` is also provided.
//!
//! Mirrors the other `#[ignore]` integration tests' skip-when-token-unset pattern (see
//! `tests/lifecycle_test.rs`): with `TRAKT_CLIENT_ID` unset the test runs and early-returns
//! cleanly rather than failing.
//!
//! Run with: `cargo test --test trakt_smoke_test -- --ignored`
//! (needs `TRAKT_CLIENT_ID`, optionally `TRAKT_CLIENT_SECRET` + `TRAKT_ACCESS_TOKEN`, in `.env`.)

mod common;

use debridmoviemapper::trakt_client::{TraktClient, TraktClientImpl};

#[tokio::test]
#[ignore]
async fn trakt_device_code_and_watchlist_smoke() {
    let Some(client_id) = common::token("TRAKT_CLIENT_ID") else {
        eprintln!("skipping trakt_device_code_and_watchlist_smoke: TRAKT_CLIENT_ID unset");
        return;
    };
    let client_secret = common::token("TRAKT_CLIENT_SECRET").unwrap_or_default();

    let trakt = TraktClientImpl::new(client_id, client_secret, reqwest::Client::new());

    // Device-flow start needs no user approval — this alone proves the OAuth start + client_id
    // are valid (the trait is in scope, so call it directly on the impl).
    let dc = trakt.device_code().await.expect("device_code");
    assert!(!dc.user_code.is_empty(), "user_code should be non-empty");
    assert!(
        !dc.verification_url.is_empty(),
        "verification_url should be non-empty"
    );
    eprintln!(
        "device code: enter {} at {}",
        dc.user_code, dc.verification_url
    );

    // The watchlist pull is only possible with a pre-authorised token — exercise it if provided.
    if let Some(token) = common::token("TRAKT_ACCESS_TOKEN") {
        let wl = trakt.watchlist(&token).await.expect("watchlist");
        eprintln!("watchlist returned {} items", wl.len());
    }
}
