//! Cross-provider lifecycle integration tests (require live API tokens, `#[ignore]`).
//!
//! For each provider: add a Creative-Commons torrent (Blender's *Sintel*) → assert it
//! appears in the provider's list AND in the built VFS library → delete it → assert it
//! disappears. These tests MODIFY the live account (add/delete a torrent) and clean up
//! after themselves, also when an assertion fails. Each first deletes any Sintel copy an
//! earlier, interrupted run left (`common::preclean_sintel_candidates`). They hold
//! `LIVE_ACCOUNT` while they run, so the harness's default parallelism can't make two of
//! them add and delete Sintel in the same account at once.
//!
//! Run with: `cargo test --test lifecycle_test -- --ignored --show-output`
//! (needs `RD_API_TOKEN` and/or `TORBOX_API_KEY`, plus `TMDB_API_KEY`, in `.env`.) libtest
//! captures a passing test's stderr, and `--show-output` prints it, so the cleanup's WARNINGs
//! (`common::delete_ids_then_sweep`: a Sintel copy may be left in the account;
//! `common::preclean_sintel_candidates`: a copy an earlier run left was deleted) are seen.

mod common;

use common::{cleanup_added, find_ids_by_hash, with_cleanup, AddedIds, SINTEL_HASH};
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::provider::{DebridProvider, ProviderKind};
use debridmoviemapper::scraper::{Scraper, TorrentioScraper};
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::{DebridVfs, VfsNode};
use std::sync::Arc;
use std::time::Duration;

/// Serialises the tests in this binary. Each adds and deletes Sintel, and a provider's
/// `lifecycle_*` and `lifecycle_acquire_sintel_by_imdb_*` tests share one account: run together,
/// one test's cleanup could delete the torrent the other is still checking. A tokio mutex isn't
/// poisoned by a failed test, so the rest still run.
static LIVE_ACCOUNT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Recursively search the VFS for a `MediaFile` whose path ends with `suffix`.
fn vfs_has_media_file(node: &VfsNode, suffix: &str) -> bool {
    match node {
        VfsNode::Directory { children } => children.values().any(|c| vfs_has_media_file(c, suffix)),
        VfsNode::MediaFile { locator, .. } => locator.file_path.to_lowercase().ends_with(suffix),
        VfsNode::VirtualFile { .. } => false,
    }
}

/// Whether the VFS root's `Movies/` directory holds a media file from the torrent with `hash`.
fn movies_has_torrent(root: &VfsNode, hash: &str) -> bool {
    let VfsNode::Directory { children } = root else {
        return false;
    };
    children
        .get("Movies")
        .is_some_and(|movies| has_media_from(movies, hash))
}

fn has_media_from(node: &VfsNode, hash: &str) -> bool {
    match node {
        VfsNode::Directory { children } => children.values().any(|c| has_media_from(c, hash)),
        VfsNode::MediaFile { locator, .. } => locator.hash.eq_ignore_ascii_case(hash),
        VfsNode::VirtualFile { .. } => false,
    }
}

/// The scraper the service would build for `kind` (`SCRAPER_ADDON_URL`, else Torrentio).
fn scraper_for(kind: ProviderKind, provider_token: &str) -> Arc<dyn Scraper> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    Arc::new(TorrentioScraper::new(
        // Trimmed and blank-as-unset, as the service reads it.
        common::token("SCRAPER_ADDON_URL"),
        kind,
        provider_token,
        http,
    ))
}

/// add → appears (provider list + VFS) → delete → disappears, for any `DebridProvider`.
async fn run_lifecycle(
    provider: Arc<dyn DebridProvider>,
    tmdb: Arc<TmdbClient>,
    scraper: Arc<dyn Scraper>,
    label: &str,
) {
    // Delete any Sintel copy a prior interrupted run left: this test's own torrent, or a release
    // an acquire test picked.
    common::preclean_sintel_candidates(&provider, scraper.as_ref()).await;
    // The cleanup deletes the added torrent by its id too: after the "should appear in
    // get_torrents()" failure the listing is lagging, so a sweep by hash alone could miss it.
    let added = AddedIds::default();
    with_cleanup(
        lifecycle_steps(&provider, &tmdb, &added, label),
        cleanup_added(&provider, &added, SINTEL_HASH),
    )
    .await;
}

async fn lifecycle_steps(
    provider: &Arc<dyn DebridProvider>,
    tmdb: &Arc<TmdbClient>,
    added: &AddedIds,
    label: &str,
) {
    // 1-3. Add by magnet, select the video file(s), wait until downloaded.
    let info = common::add_sintel_downloaded(provider, added, label).await;
    let id = info.id.clone();

    // 4. Appears in the provider's list. TorBox's listing can lag an add that already reports
    // `downloaded`, so poll it every 2 s for up to 30 s before failing.
    if let Err(why) = common::poll_until_listed(
        || find_ids_by_hash(provider, SINTEL_HASH),
        16,
        Duration::from_secs(2),
    )
    .await
    {
        panic!("[{label}] torrent should appear in get_torrents(): {why}");
    }

    // 5. Appears in the built library (VFS).
    let metadata = identify_torrent(&info, tmdb).await;
    let vfs = DebridVfs::build(
        vec![(info.clone(), metadata)],
        &debridmoviemapper::vfs::SelectionMap::new(),
    );
    assert!(
        vfs_has_media_file(&vfs.root, ".mp4"),
        "[{label}] the .mp4 should appear in the VFS library"
    );
    println!("[{label}] appears in library ✓");

    // 6. Delete. The cleanup needn't delete the id again (TorBox answers that with a 500).
    provider.delete_torrent(&id).await.expect("delete_torrent");
    common::forget_added_id(added, &id);

    // 7. Disappears from the provider's list.
    let mut gone = false;
    let mut last = String::new();
    for _ in 0..20 {
        match find_ids_by_hash(provider, SINTEL_HASH).await {
            Ok(ids) if ids.is_empty() => {
                gone = true;
                break;
            }
            Ok(ids) => last = format!("still listed as {ids:?}"),
            Err(e) => last = format!("the last listing errored: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        gone,
        "[{label}] torrent should disappear after delete ({last})"
    );
    println!("[{label}] disappeared after delete ✓");
}

/// [`run_lifecycle`] on the `kind` provider, or a skip line for `test` when its token is unset.
async fn lifecycle_on(kind: ProviderKind, test: &str) {
    let _live = LIVE_ACCOUNT.lock().await;
    tracing_subscriber::fmt::try_init().ok();
    let Some(token) = common::provider_token(kind, test) else {
        return;
    };
    let scraper = scraper_for(kind, &token);
    let provider = common::client(kind, token);
    run_lifecycle(
        provider,
        common::tmdb_client(),
        scraper,
        common::label(kind),
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn lifecycle_real_debrid() {
    lifecycle_on(ProviderKind::RealDebrid, "lifecycle_real_debrid").await;
}

#[tokio::test]
#[ignore]
async fn lifecycle_torbox() {
    lifecycle_on(ProviderKind::TorBox, "lifecycle_torbox").await;
}

/// [`run_acquire`] on the `kind` provider, or a skip line for `test` when its token is unset.
async fn acquire_on(kind: ProviderKind, test: &str) {
    let _live = LIVE_ACCOUNT.lock().await;
    let Some(token) = common::provider_token(kind, test) else {
        return;
    };
    run_acquire(kind, token, common::label(kind)).await;
}

#[tokio::test]
#[ignore]
async fn lifecycle_acquire_sintel_by_imdb_real_debrid() {
    acquire_on(
        ProviderKind::RealDebrid,
        "lifecycle_acquire_sintel_by_imdb_real_debrid",
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn lifecycle_acquire_sintel_by_imdb_torbox() {
    acquire_on(
        ProviderKind::TorBox,
        "lifecycle_acquire_sintel_by_imdb_torbox",
    )
    .await;
}

/// Acquire Sintel by IMDB id through the `AcquisitionEngine` on one provider, drive `observe`
/// until it verifies, and check it lands under `Movies/`. Each provider runs it separately (like
/// `run_lifecycle`), so a `.env` that sets both tokens tests both instead of skipping: the service's
/// `choose_provider` deliberately refuses that combination.
async fn run_acquire(kind: ProviderKind, provider_token: String, label: &str) {
    // With a provider configured, TMDB is required: a missing key or a TMDB failure fails the test.
    let tmdb = common::tmdb_client();
    let provider = common::client(kind, provider_token.clone());
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    let mut dbp = std::env::temp_dir();
    dbp.push(format!(
        "dmm_sp1_lifecycle_{label}_{}.redb",
        std::process::id()
    ));
    let store = debridmoviemapper::store::Store::open(dbp.to_str().unwrap()).unwrap();
    let scraper = scraper_for(kind, &provider_token);
    let validator: Arc<dyn debridmoviemapper::acquire::TitleValidator> =
        Arc::new(debridmoviemapper::acquire::TmdbTitleValidator { tmdb: tmdb.clone() });
    let prober: Arc<dyn debridmoviemapper::acquire::Prober> =
        Arc::new(debridmoviemapper::acquire::HttpProber { http: http.clone() });
    let engine = debridmoviemapper::acquire::AcquisitionEngine::new(
        provider.clone(),
        scraper.clone(),
        validator,
        prober,
        store.clone(),
        debridmoviemapper::config::AcquisitionConfig::default().prefs,
        5,
        std::time::Duration::from_secs(1800),
        std::time::Duration::from_secs(600),
    );

    // Resolve Sintel's TMDB id from its IMDB id at runtime.
    let imdb = common::SINTEL_IMDB;
    let (tmdb_id, mtype) = match tmdb.find_by_imdb(imdb).await {
        Ok(Some(v)) => v,
        Ok(None) => panic!("[{label}] TMDB found no title for Sintel's IMDB id {imdb}"),
        Err(e) => panic!("[{label}] TMDB find_by_imdb({imdb}) failed: {e}"),
    };
    // A failed lookup must fail the test, not fall back to empty details: an empty title and no
    // original language would run acquisition and the probe with no audio-language requirement,
    // a weaker path than production takes.
    let details = tmdb
        .details(tmdb_id, mtype.clone())
        .await
        .unwrap_or_else(|e| panic!("[{label}] TMDB details({tmdb_id}) failed: {e}"));
    assert!(
        details.original_language.is_some(),
        "[{label}] TMDB details for Sintel carry no original_language, so the audio-language \
         check would go untested"
    );
    let req = debridmoviemapper::store::AcquireRequest {
        imdb_id: imdb.to_string(),
        tmdb_id,
        kind: debridmoviemapper::scraper::MediaKind::Movie,
        season: None,
        episode: None,
        original_language: details.original_language,
        spoken_languages: Some(details.spoken_languages),
        metadata: debridmoviemapper::vfs::MediaMetadata {
            title: details.title,
            year: details.year,
            media_type: mtype,
            external_id: Some(format!("tmdb:{}", tmdb_id)),
        },
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();

    // Delete any Sintel copy a prior interrupted run left. The fresh store doesn't own one, so the
    // acquire would skip it as already in the account and add another release, and this run's
    // cleanup, which deletes only what the store owns, would leave the old copy in the account.
    common::preclean_sintel_candidates(&provider, scraper.as_ref()).await;

    // The ids of the owned torrents the body sees listed. The engine's add doesn't hand back an id,
    // so the listings are where they come from; the cleanup deletes them directly, so a later
    // listing that lags or errors can't leave one behind.
    let added = AddedIds::default();
    let body = async {
        // Optimistic acquire: adds the best candidate and returns Pending; observe checks it below.
        let hash = match engine
            .acquire(req.clone(), debridmoviemapper::store::Provenance::manual())
            .await
        {
            debridmoviemapper::acquire::AcquireOutcome::Pending(h)
            | debridmoviemapper::acquire::AcquireOutcome::Acquired(h) => h,
            other => panic!("[{label}] unexpected acquire outcome: {other:?}"),
        };
        eprintln!("[{label}] acquire outcome hash: {hash}");

        // Drive observe until the store shows Verified.
        // Sintel is a small, widely-cached CC torrent, so it should verify within a few ticks.
        let mut verified = false;
        for _ in 0..20 {
            let torrents = provider.get_torrents().await.expect("get_torrents");
            record_owned_ids(&store, &torrents, &added).await;
            let deleted = engine.observe(&torrents).await;
            forget_deleted_ids(&torrents, &deleted, &added);
            if matches!(
                store.get_owned(hash.clone()).await.map(|r| r.status),
                Some(debridmoviemapper::store::OwnedStatus::Verified)
            ) {
                verified = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        assert!(
            verified,
            "[{label}] Sintel should verify via observe within the poll window"
        );

        // Build the VFS from the acquired torrent alone, the way the scan loop resolves it (the
        // engine's authoritative metadata first, filename identification as the fallback), with
        // the selection map `observe` wrote, and assert it lands under Movies/.
        let selection: debridmoviemapper::vfs::SelectionMap =
            store.all_selection().await.into_iter().collect();
        assert!(
            selection
                .values()
                .any(|e| e.hash.eq_ignore_ascii_case(&hash)),
            "[{label}] observe should write a selection entry for the verified torrent {hash}"
        );
        let torrents = provider
            .get_torrents()
            .await
            .expect("get_torrents for VFS build");
        record_owned_ids(&store, &torrents, &added).await;
        let mut acquired = Vec::new();
        for t in torrents
            .iter()
            .filter(|t| t.hash.eq_ignore_ascii_case(&hash))
        {
            let info = provider
                .get_torrent_info(&t.id)
                .await
                .expect("get_torrent_info for the acquired torrent");
            let meta = match store.authoritative_meta(hash.to_ascii_lowercase()).await {
                Some(m) => m,
                None => identify_torrent(&info, &tmdb).await,
            };
            acquired.push((info, meta));
        }
        assert!(
            !acquired.is_empty(),
            "[{label}] the acquired torrent {hash} should be listed by the provider"
        );
        let vfs = DebridVfs::build(acquired, &selection);
        assert!(
            movies_has_torrent(&vfs.root, &hash),
            "[{label}] Sintel ({hash}) should appear under Movies/ after verify"
        );
        eprintln!("[{label}] Sintel appears in Movies/ via VFS ✓");
    };

    // Cleanup, also after a failed assertion: delete the owned torrents the body saw listed by id,
    // then every torrent the engine recorded as owned by hash, then the temp store.
    let cleanup = async {
        let owned: Vec<String> = store
            .all_owned()
            .await
            .into_iter()
            .map(|(h, _)| h)
            .collect();
        common::delete_ids_then_sweep(&provider, &common::added_ids(&added), &owned).await;
        let _ = std::fs::remove_file(&dbp);
        let _ = std::fs::remove_file(format!("{}.corrupt", dbp.to_str().unwrap()));
    };
    with_cleanup(body, cleanup).await;
}

/// Record in `added` the id of every listed torrent whose hash the test's own (fresh) store owns.
async fn record_owned_ids(
    store: &debridmoviemapper::store::Store,
    torrents: &[debridmoviemapper::rd_client::Torrent],
    added: &AddedIds,
) {
    let owned = store.all_owned().await;
    for t in torrents
        .iter()
        .filter(|t| owned.iter().any(|(h, _)| t.hash.eq_ignore_ascii_case(h)))
    {
        common::record_added_id(added, &t.id);
    }
}

/// Forget from `added` the id of every listed torrent (in the `torrents` `observe` was given) whose
/// hash is in `deleted`, the hashes `observe` returned as deleted (it deletes a rejected release
/// under every listing id), so the cleanup doesn't delete them again.
fn forget_deleted_ids(
    torrents: &[debridmoviemapper::rd_client::Torrent],
    deleted: &std::collections::HashSet<String>,
    added: &AddedIds,
) {
    for t in torrents
        .iter()
        .filter(|t| deleted.contains(&t.hash.to_ascii_lowercase()))
    {
        common::forget_added_id(added, &t.id);
    }
}

// --- Offline checks of the helpers above and in `common` (no network; plain `cargo test`) ---

fn media(hash: &str, path: &str) -> VfsNode {
    VfsNode::MediaFile {
        file_size: 1,
        locator: debridmoviemapper::provider::FileLocator {
            hash: hash.to_string(),
            torrent_id: "1".to_string(),
            file_id: 1,
            file_path: path.to_string(),
            link: None,
        },
    }
}

fn dir(children: Vec<(&str, VfsNode)>) -> VfsNode {
    VfsNode::Directory {
        children: children
            .into_iter()
            .map(|(name, node)| (name.to_string(), node))
            .collect(),
    }
}

#[test]
fn movies_has_torrent_needs_that_torrent_under_movies() {
    // Another title's .mp4 under Movies/, and the acquired torrent under Shows/: any-.mp4 passes,
    // but the acquired torrent is not under Movies/.
    let root = dir(vec![
        (
            "Movies",
            dir(vec![(
                "Other (2010)",
                dir(vec![("other.mp4", media("aaaa", "other.mp4"))]),
            )]),
        ),
        (
            "Shows",
            dir(vec![(
                "Sintel",
                dir(vec![("sintel.mp4", media("BBBB", "sintel.mp4"))]),
            )]),
        ),
    ]);
    assert!(vfs_has_media_file(&root, ".mp4"));
    assert!(!movies_has_torrent(&root, "bbbb"));

    let root = dir(vec![(
        "Movies",
        dir(vec![(
            "Sintel (2010)",
            dir(vec![("sintel.mp4", media("BBBB", "sintel.mp4"))]),
        )]),
    )]);
    assert!(
        movies_has_torrent(&root, "bbbb"),
        "the hash comparison ignores case"
    );
}

#[tokio::test]
async fn with_cleanup_runs_cleanup_when_the_body_panics() {
    use futures_util::FutureExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    let cleaned = AtomicBool::new(false);
    let outcome = std::panic::AssertUnwindSafe(with_cleanup::<()>(
        async { panic!("an assertion in the body failed") },
        async { cleaned.store(true, Ordering::SeqCst) },
    ))
    .catch_unwind()
    .await;
    assert!(outcome.is_err(), "the body's panic should propagate");
    assert!(
        cleaned.load(Ordering::SeqCst),
        "cleanup should run although the body panicked"
    );
}

#[tokio::test(start_paused = true)]
async fn poll_until_listed_waits_for_a_late_listing() {
    let calls = std::cell::Cell::new(0u32);
    let ids = common::poll_until_listed(
        || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                match n {
                    1 => Err("HTTP 500".to_string()),
                    2 => Ok(Vec::new()),
                    _ => Ok(vec!["42".to_string()]),
                }
            }
        },
        16,
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(ids, Ok(vec!["42".to_string()]));
    assert_eq!(calls.get(), 3, "polling stops once the torrent is listed");
}

#[tokio::test(start_paused = true)]
async fn poll_until_listed_says_when_the_torrent_was_missing() {
    let err = common::poll_until_listed(
        || async { Ok::<_, String>(Vec::new()) },
        3,
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert!(err.contains("missing from all 3 listings"), "{err}");
    assert!(!err.contains("errored"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn poll_until_listed_says_when_the_listing_errored() {
    let err = common::poll_until_listed(
        || async { Err::<Vec<String>, _>("HTTP 503") },
        3,
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert!(err.contains("all 3 listings errored"), "{err}");
    assert!(err.contains("HTTP 503"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn poll_until_listed_reports_both_outcomes_when_they_mix() {
    let calls = std::cell::Cell::new(0u32);
    let err = common::poll_until_listed(
        || {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n.is_multiple_of(2) {
                    Err("HTTP 502".to_string())
                } else {
                    Ok(Vec::new())
                }
            }
        },
        4,
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert!(err.contains("missing from 2 listings"), "{err}");
    assert!(err.contains("2 listings errored"), "{err}");
    assert!(err.contains("HTTP 502"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn poll_until_listed_waits_at_most_its_window() {
    let start = tokio::time::Instant::now();
    let _ = common::poll_until_listed(
        || async { Ok::<_, String>(Vec::new()) },
        16,
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(start.elapsed(), Duration::from_secs(30));
}

#[tokio::test]
async fn with_cleanup_runs_cleanup_and_returns_the_body_value() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let cleaned = AtomicBool::new(false);
    let value = with_cleanup(async { 7 }, async { cleaned.store(true, Ordering::SeqCst) }).await;
    assert_eq!(value, 7);
    assert!(cleaned.load(Ordering::SeqCst));
}

// --- Offline checks of `common::delete_ids_then_sweep_with` (the cleanup core) ---

/// One scripted listing per call (the last one repeats), each `Ok(vec![(id, hash)])` or an error.
type Listing = Result<Vec<(String, String)>, String>;

fn listed(entries: &[(&str, &str)]) -> Listing {
    Ok(entries
        .iter()
        .map(|(id, hash)| (id.to_string(), hash.to_string()))
        .collect())
}

/// Run the cleanup core against scripted listings, with every delete succeeding; returns its
/// outcome, the ids it deleted (in order) and how many listings it made.
async fn sweep(
    listings: Vec<Listing>,
    ids: &[&str],
    hashes: &[&str],
) -> (Result<(), String>, Vec<String>, usize) {
    sweep_failing(listings, ids, hashes, &[]).await
}

/// [`sweep`], with every delete of an id in `failing` failing. The returned ids are every delete
/// attempted, failed ones included.
async fn sweep_failing(
    listings: Vec<Listing>,
    ids: &[&str],
    hashes: &[&str],
    failing: &[&str],
) -> (Result<(), String>, Vec<String>, usize) {
    let calls = std::cell::Cell::new(0usize);
    let deleted = std::cell::RefCell::new(Vec::new());
    let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
    let hashes: Vec<String> = hashes.iter().map(|s| s.to_string()).collect();
    let outcome = common::delete_ids_then_sweep_with(
        || {
            let n = calls.get();
            calls.set(n + 1);
            let listing = listings[n.min(listings.len() - 1)].clone();
            async move { listing }
        },
        |id: String| {
            let ok = !failing.contains(&id.as_str());
            deleted.borrow_mut().push(id);
            async move { ok }
        },
        &ids,
        &hashes,
        3,
        Duration::from_secs(2),
    )
    .await;
    (outcome, deleted.into_inner(), calls.get())
}

const OTHER_HASH: &str = "1111111111111111111111111111111111111111";

#[tokio::test(start_paused = true)]
async fn cleanup_deletes_a_known_id_the_lagging_listing_leaves_out() {
    // TorBox's listing can lag an add: the torrent the body added isn't listed yet. Cleanup must
    // still delete it by the id `add_magnet` returned, not trust the listing alone.
    let (outcome, deleted, listings) = sweep(vec![listed(&[])], &["42"], &[SINTEL_HASH]).await;
    assert_eq!(outcome, Ok(()));
    assert_eq!(deleted, vec!["42".to_string()]);
    assert_eq!(listings, 1, "a clean listing ends the sweep");
}

#[tokio::test(start_paused = true)]
async fn cleanup_deletes_what_the_listing_shows_and_rechecks() {
    let upper = SINTEL_HASH.to_ascii_uppercase();
    let (outcome, deleted, listings) = sweep(
        vec![
            listed(&[("1", OTHER_HASH), ("7", upper.as_str())]),
            listed(&[("1", OTHER_HASH)]),
        ],
        &[],
        &[SINTEL_HASH],
    )
    .await;
    assert_eq!(outcome, Ok(()));
    assert_eq!(
        deleted,
        vec!["7".to_string()],
        "only the matching hash (case-insensitively), never another torrent"
    );
    assert_eq!(listings, 2, "a second listing confirms the delete");
}

#[tokio::test(start_paused = true)]
async fn cleanup_warns_when_every_listing_errors() {
    let (outcome, deleted, listings) =
        sweep(vec![Err("HTTP 503".to_string())], &["42"], &[SINTEL_HASH]).await;
    let why = outcome.unwrap_err();
    assert!(why.contains(SINTEL_HASH), "{why}");
    assert!(why.contains("42"), "{why}");
    assert!(why.contains("HTTP 503"), "{why}");
    assert_eq!(
        deleted,
        vec!["42".to_string()],
        "the known id is deleted even though no listing succeeds"
    );
    assert_eq!(listings, 3, "it retries the listing");
}

#[tokio::test(start_paused = true)]
async fn cleanup_warns_when_the_torrent_stays_listed() {
    let (outcome, deleted, listings) =
        sweep(vec![listed(&[("7", SINTEL_HASH)])], &[], &[SINTEL_HASH]).await;
    let why = outcome.unwrap_err();
    assert!(why.contains("deleted but still listed"), "{why}");
    assert!(why.contains('7') && why.contains(SINTEL_HASH), "{why}");
    assert_eq!(
        deleted,
        vec!["7".to_string()],
        "a deleted torrent isn't deleted again"
    );
    assert_eq!(listings, 3, "it waits for a listing without the torrent");
}

#[tokio::test(start_paused = true)]
async fn cleanup_retries_a_delete_that_failed() {
    let (outcome, deleted, listings) = sweep_failing(
        vec![listed(&[("42", SINTEL_HASH)])],
        &["42"],
        &[SINTEL_HASH],
        &["42"],
    )
    .await;
    let why = outcome.unwrap_err();
    assert!(why.contains("still listed"), "{why}");
    assert!(!why.contains("deleted but"), "{why}");
    assert_eq!(
        deleted,
        vec!["42".to_string(); 4],
        "the first delete, then one on each listing that still shows it"
    );
    assert_eq!(listings, 3);
}

#[tokio::test(start_paused = true)]
async fn cleanup_does_not_delete_again_an_id_it_deleted() {
    // TorBox's listing can lag a delete too: right after the cleanup deleted the known id, the
    // listing still shows it. Deleting it again gets TorBox's 500 DATABASE_ERROR, which its client
    // retries five times, so the sweep waits for a listing without it instead.
    let (outcome, deleted, listings) = sweep(
        vec![listed(&[("42", SINTEL_HASH)]), listed(&[])],
        &["42"],
        &[SINTEL_HASH],
    )
    .await;
    assert_eq!(outcome, Ok(()));
    assert_eq!(deleted, vec!["42".to_string()], "one delete per torrent");
    assert_eq!(listings, 2);
}

#[tokio::test(start_paused = true)]
async fn cleanup_does_not_delete_again_a_swept_torrent_it_deleted() {
    let (outcome, deleted, listings) = sweep(
        vec![
            listed(&[("7", SINTEL_HASH)]),
            listed(&[("7", SINTEL_HASH)]),
            listed(&[]),
        ],
        &[],
        &[SINTEL_HASH],
    )
    .await;
    assert_eq!(outcome, Ok(()));
    assert_eq!(deleted, vec!["7".to_string()], "one delete per torrent");
    assert_eq!(listings, 3);
}

#[tokio::test(start_paused = true)]
async fn cleanup_recovers_from_an_erroring_listing() {
    let (outcome, _, listings) = sweep(
        vec![Err("HTTP 502".to_string()), listed(&[])],
        &[],
        &[SINTEL_HASH],
    )
    .await;
    assert_eq!(outcome, Ok(()));
    assert_eq!(listings, 2);
}

#[tokio::test(start_paused = true)]
async fn cleanup_with_nothing_to_clean_lists_nothing() {
    let (outcome, deleted, listings) = sweep(vec![listed(&[])], &[], &[]).await;
    assert_eq!(outcome, Ok(()));
    assert!(deleted.is_empty());
    assert_eq!(listings, 0);
}

#[tokio::test(start_paused = true)]
async fn cleanup_sweeps_at_most_its_window() {
    let start = tokio::time::Instant::now();
    let _ = sweep(vec![Err("HTTP 503".to_string())], &[], &[SINTEL_HASH]).await;
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(4),
        "3 listings, 2 s apart"
    );
}

// --- Offline checks of `common::sintel_leftovers` (what the pre-clean deletes) ---

fn listed_torrent(id: &str, hash: &str, name: &str) -> debridmoviemapper::rd_client::Torrent {
    debridmoviemapper::rd_client::Torrent {
        id: id.to_string(),
        filename: name.to_string(),
        hash: hash.to_string(),
        ..Default::default()
    }
}

fn ids_of(torrents: &[&debridmoviemapper::rd_client::Torrent]) -> Vec<String> {
    torrents.iter().map(|t| t.id.clone()).collect()
}

const SINTEL_RELEASE: &str = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
const SINTEL_COLLECTION: &str = "3333333333333333333333333333333333333333";

#[test]
fn sintel_leftovers_deletes_only_sintel_copies() {
    let upper = SINTEL_RELEASE.to_ascii_uppercase();
    let listing = vec![
        listed_torrent("1", OTHER_HASH, "Other.Film.2010.1080p"),
        // A scraped Sintel release an earlier acquire test left (hashes compare case-insensitively).
        listed_torrent("2", &upper, "Sintel.2010.1080p.BluRay"),
        // A scraped release whose name doesn't mention Sintel: reported, never deleted.
        listed_torrent("3", SINTEL_COLLECTION, "Open.Movie.Collection"),
        // The lifecycle tests' own torrent, whatever it is called.
        listed_torrent("4", SINTEL_HASH, "sintel-1024-surround"),
        // Named Sintel but not one of the scraped releases: not the tests' to delete.
        listed_torrent(
            "5",
            "4444444444444444444444444444444444444444",
            "Sintel.Fan.Edit",
        ),
    ];
    let candidates = [SINTEL_RELEASE.to_string(), SINTEL_COLLECTION.to_string()];
    let found = common::sintel_leftovers(&listing, &candidates);
    assert_eq!(ids_of(&found.delete), vec!["2", "4"]);
    assert_eq!(ids_of(&found.unnamed), vec!["3"]);
}

#[test]
fn sintel_leftovers_without_a_scrape_deletes_only_sintel_hash() {
    let listing = vec![
        listed_torrent("2", SINTEL_RELEASE, "Sintel.2010.1080p.BluRay"),
        listed_torrent("4", SINTEL_HASH, "Sintel"),
        listed_torrent("6", "", "Sintel"),
    ];
    let found = common::sintel_leftovers(&listing, &[]);
    assert_eq!(ids_of(&found.delete), vec!["4"]);
    assert!(found.unnamed.is_empty());
}

#[test]
fn record_added_id_keeps_each_id_once() {
    let added = common::AddedIds::default();
    common::record_added_id(&added, "42");
    common::record_added_id(&added, "42");
    common::record_added_id(&added, "43");
    assert_eq!(
        common::added_ids(&added),
        vec!["42".to_string(), "43".to_string()]
    );
}

#[test]
fn forget_deleted_ids_drops_the_ids_observe_deleted() {
    let added = common::AddedIds::default();
    for id in ["1", "2", "3"] {
        common::record_added_id(&added, id);
    }
    let listing = vec![
        listed_torrent(
            "1",
            &SINTEL_RELEASE.to_ascii_uppercase(),
            "Sintel.2010.720p",
        ),
        listed_torrent("2", SINTEL_HASH, "Sintel"),
        // A second listing id of the rejected hash: `observe` deletes it under every id.
        listed_torrent("3", SINTEL_RELEASE, "Sintel.2010.720p"),
    ];
    // `observe` returns the lowercased hashes whose listed torrents it deleted.
    let deleted: std::collections::HashSet<String> = [SINTEL_RELEASE.to_string()].into();
    forget_deleted_ids(&listing, &deleted, &added);
    assert_eq!(common::added_ids(&added), vec!["2".to_string()]);
}

#[test]
fn forget_added_id_drops_only_that_id() {
    let added = common::AddedIds::default();
    common::record_added_id(&added, "42");
    common::record_added_id(&added, "43");
    common::forget_added_id(&added, "42");
    common::forget_added_id(&added, "44");
    assert_eq!(common::added_ids(&added), vec!["43".to_string()]);
}
