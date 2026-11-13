//! Cross-provider lifecycle integration tests (require live API tokens, `#[ignore]`).
//!
//! For each provider: add a Creative-Commons torrent (Blender's *Sintel*) → assert it
//! appears in the provider's list AND in the built VFS library → delete it → assert it
//! disappears. These tests MODIFY the live account (add/delete a torrent) and clean up
//! after themselves, also when an assertion fails. They hold `LIVE_ACCOUNT` while they
//! run, so the harness's default parallelism can't make two of them add and delete Sintel
//! in the same account at once.
//!
//! Run with: `cargo test --test lifecycle_test -- --ignored`
//! (needs `RD_API_TOKEN` and/or `TORBOX_API_KEY`, plus `TMDB_API_KEY`, in `.env`.)

mod common;

use common::{cleanup_by_hash, find_ids_by_hash, with_cleanup, SINTEL_HASH};
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::provider::DebridProvider;
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::torbox_client::TorBoxClient;
use debridmoviemapper::vfs::{DebridVfs, VfsNode};
use std::sync::Arc;
use std::time::Duration;

/// Serialises the tests in this binary. Each adds and deletes Sintel, and `lifecycle_torbox` and
/// `lifecycle_acquire_sintel_by_imdb` can share one account: run together, one test's cleanup could
/// delete the torrent the other is still checking. A tokio mutex isn't poisoned by a failed test,
/// so the rest still run.
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

/// add → appears (provider list + VFS) → delete → disappears, for any `DebridProvider`.
async fn run_lifecycle(provider: Arc<dyn DebridProvider>, tmdb: Arc<TmdbClient>, label: &str) {
    // Delete any leftover Sintel from a prior interrupted run.
    cleanup_by_hash(&provider, SINTEL_HASH).await;
    with_cleanup(
        lifecycle_steps(&provider, &tmdb, label),
        cleanup_by_hash(&provider, SINTEL_HASH),
    )
    .await;
}

async fn lifecycle_steps(provider: &Arc<dyn DebridProvider>, tmdb: &Arc<TmdbClient>, label: &str) {
    // 1-3. Add by magnet, select the video file(s), wait until downloaded.
    let info = common::add_sintel_downloaded(provider, label).await;
    let id = info.id.clone();

    // 4. Appears in the provider's list.
    assert!(
        !find_ids_by_hash(provider, SINTEL_HASH).await.is_empty(),
        "[{label}] torrent should appear in get_torrents()"
    );

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

    // 6. Delete.
    provider.delete_torrent(&id).await.expect("delete_torrent");

    // 7. Disappears from the provider's list.
    let mut gone = false;
    for _ in 0..20 {
        if find_ids_by_hash(provider, SINTEL_HASH).await.is_empty() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(gone, "[{label}] torrent should disappear after delete");
    println!("[{label}] disappeared after delete ✓");
}

#[tokio::test]
#[ignore]
async fn lifecycle_real_debrid() {
    let _live = LIVE_ACCOUNT.lock().await;
    tracing_subscriber::fmt::try_init().ok();
    let Some(rd) = common::token("RD_API_TOKEN") else {
        println!("skipping lifecycle_real_debrid: RD_API_TOKEN not set");
        return;
    };
    let provider: Arc<dyn DebridProvider> = Arc::new(RealDebridClient::new(rd).unwrap());
    run_lifecycle(provider, common::tmdb_client(), "real-debrid").await;
}

#[tokio::test]
#[ignore]
async fn lifecycle_torbox() {
    let _live = LIVE_ACCOUNT.lock().await;
    tracing_subscriber::fmt::try_init().ok();
    let Some(tb) = common::token("TORBOX_API_KEY") else {
        println!("skipping lifecycle_torbox: TORBOX_API_KEY not set");
        return;
    };
    let provider: Arc<dyn DebridProvider> = Arc::new(TorBoxClient::new(tb).unwrap());
    run_lifecycle(provider, common::tmdb_client(), "torbox").await;
}

#[tokio::test]
#[ignore]
async fn lifecycle_acquire_sintel_by_imdb() {
    let _live = LIVE_ACCOUNT.lock().await;
    dotenvy::dotenv().ok();
    let Ok((kind, provider_token)) = debridmoviemapper::provider::choose_provider(
        std::env::var("RD_API_TOKEN").ok(),
        std::env::var("TORBOX_API_KEY").ok(),
    ) else {
        eprintln!("skipping: no provider token");
        return;
    };
    // With a provider configured, TMDB is required: a missing key or a TMDB failure fails the test.
    let tmdb = common::tmdb_client();
    let provider: Arc<dyn debridmoviemapper::provider::DebridProvider> = match kind {
        debridmoviemapper::provider::ProviderKind::RealDebrid => Arc::new(
            debridmoviemapper::rd_client::RealDebridClient::new(provider_token.clone()).unwrap(),
        ),
        debridmoviemapper::provider::ProviderKind::TorBox => Arc::new(
            debridmoviemapper::torbox_client::TorBoxClient::new(provider_token.clone()).unwrap(),
        ),
    };
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    let mut dbp = std::env::temp_dir();
    dbp.push(format!("dmm_sp1_lifecycle_{}.redb", std::process::id()));
    let store = debridmoviemapper::store::Store::open(dbp.to_str().unwrap()).unwrap();
    let scraper: Arc<dyn debridmoviemapper::scraper::Scraper> =
        Arc::new(debridmoviemapper::scraper::TorrentioScraper::new(
            std::env::var("SCRAPER_ADDON_URL").ok(),
            kind,
            &provider_token,
            http.clone(),
        ));
    let validator: Arc<dyn debridmoviemapper::acquire::TitleValidator> =
        Arc::new(debridmoviemapper::acquire::TmdbTitleValidator { tmdb: tmdb.clone() });
    let prober: Arc<dyn debridmoviemapper::acquire::Prober> =
        Arc::new(debridmoviemapper::acquire::HttpProber { http: http.clone() });
    let engine = debridmoviemapper::acquire::AcquisitionEngine::new(
        provider.clone(),
        scraper,
        validator,
        prober,
        store.clone(),
        debridmoviemapper::config::AcquisitionConfig::default().prefs,
        5,
        std::time::Duration::from_secs(1800),
        std::time::Duration::from_secs(600),
    );

    // Resolve Sintel's TMDB id from its IMDB id at runtime.
    let imdb = "tt1727587";
    let (tmdb_id, mtype) = match tmdb.find_by_imdb(imdb).await {
        Ok(Some(v)) => v,
        Ok(None) => panic!("TMDB found no title for Sintel's IMDB id {imdb}"),
        Err(e) => panic!("TMDB find_by_imdb({imdb}) failed: {e}"),
    };
    let details = tmdb
        .details(tmdb_id, mtype.clone())
        .await
        .unwrap_or_default();
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

    let body = async {
        // Optimistic acquire: returns Pending quickly (or Acquired if the cached check is instant).
        let hash = match engine
            .acquire(req.clone(), debridmoviemapper::store::Provenance::manual())
            .await
        {
            debridmoviemapper::acquire::AcquireOutcome::Pending(h)
            | debridmoviemapper::acquire::AcquireOutcome::Acquired(h) => h,
            other => panic!("unexpected acquire outcome: {other:?}"),
        };
        eprintln!("acquire outcome hash: {}", hash);

        // Drive observe until the store shows Verified.
        // Sintel is a small, widely-cached CC torrent, so it should verify within a few ticks.
        let mut verified = false;
        for _ in 0..20 {
            let torrents = provider.get_torrents().await.expect("get_torrents");
            engine.observe(&torrents).await;
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
            "Sintel should verify via observe within the poll window"
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
            "observe should write a selection entry for the verified torrent {hash}"
        );
        let torrents = provider
            .get_torrents()
            .await
            .expect("get_torrents for VFS build");
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
            "the acquired torrent {hash} should be listed by the provider"
        );
        let vfs = DebridVfs::build(acquired, &selection);
        assert!(
            movies_has_torrent(&vfs.root, &hash),
            "Sintel ({hash}) should appear under Movies/ after verify"
        );
        eprintln!("Sintel appears in Movies/ via VFS ✓");
    };

    // Cleanup, also after a failed assertion: delete every torrent the engine recorded as owned,
    // then the temp store.
    let cleanup = async {
        let owned: Vec<String> = store
            .all_owned()
            .await
            .into_iter()
            .map(|(h, _)| h)
            .collect();
        common::delete_by_hashes(&provider, &owned).await;
        let _ = std::fs::remove_file(&dbp);
        let _ = std::fs::remove_file(format!("{}.corrupt", dbp.to_str().unwrap()));
    };
    with_cleanup(body, cleanup).await;
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

#[tokio::test]
async fn with_cleanup_runs_cleanup_and_returns_the_body_value() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let cleaned = AtomicBool::new(false);
    let value = with_cleanup(async { 7 }, async { cleaned.store(true, Ordering::SeqCst) }).await;
    assert_eq!(value, 7);
    assert!(cleaned.load(Ordering::SeqCst));
}
