//! Cross-provider lifecycle integration tests (require live API tokens, `#[ignore]`).
//!
//! For each provider: add a Creative-Commons torrent (Blender's *Sintel*) → assert it
//! appears in the provider's list AND in the built VFS library → delete it → assert it
//! disappears. These tests MODIFY the live account (add/delete a torrent) and clean up
//! after themselves.
//!
//! Run with: `cargo test --test lifecycle_test -- --ignored`
//! (needs `RD_API_TOKEN` and/or `TORBOX_API_KEY`, plus `TMDB_API_KEY`, in `.env`.)

use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::provider::DebridProvider;
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::torbox_client::TorBoxClient;
use debridmoviemapper::vfs::{is_video_file, DebridVfs, VfsNode};
use std::sync::Arc;
use std::time::Duration;

/// Blender's *Sintel* — Creative-Commons, single ~129 MB `.mp4`, widely seeded/cached.
const SINTEL_HASH: &str = "08ada5a7a6183aae1e09d831df6748d566095a10";

fn sintel_magnet() -> String {
    format!(
        "magnet:?xt=urn:btih:{}&dn=Sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce",
        SINTEL_HASH
    )
}

/// Recursively search the VFS for a `MediaFile` whose path ends with `suffix`.
fn vfs_has_media_file(node: &VfsNode, suffix: &str) -> bool {
    match node {
        VfsNode::Directory { children } => children.values().any(|c| vfs_has_media_file(c, suffix)),
        VfsNode::MediaFile { locator, .. } => locator.file_path.to_lowercase().ends_with(suffix),
        VfsNode::VirtualFile { .. } => false,
    }
}

async fn find_id_by_hash(provider: &Arc<dyn DebridProvider>, hash: &str) -> Option<String> {
    let torrents = provider.get_torrents().await.ok()?;
    torrents.into_iter().find(|t| t.hash == hash).map(|t| t.id)
}

/// Delete any leftover Sintel from a prior interrupted run.
async fn cleanup(provider: &Arc<dyn DebridProvider>, hash: &str) {
    if let Some(id) = find_id_by_hash(provider, hash).await {
        let _ = provider.delete_torrent(&id).await;
    }
}

/// add → appears (provider list + VFS) → delete → disappears, for any `DebridProvider`.
async fn run_lifecycle(provider: Arc<dyn DebridProvider>, tmdb: Arc<TmdbClient>, label: &str) {
    cleanup(&provider, SINTEL_HASH).await;

    // 1. Add by magnet.
    let added = provider
        .add_magnet(&sintel_magnet())
        .await
        .expect("add_magnet");
    let id = added.id.clone();
    println!("[{label}] add_magnet -> id={id}");

    // 2. Wait for the file list to be available (Real-Debrid converts the magnet first;
    //    TorBox lists files immediately), then select ONLY the video file(s). Selecting
    //    non-video files (subtitles/poster) makes Real-Debrid return a link array that does
    //    not align with the selected files — production torrents select just the video.
    let mut pre = None;
    for _ in 0..40 {
        if let Ok(info) = provider.get_torrent_info(&id).await {
            if !info.files.is_empty() {
                pre = Some(info);
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let pre = pre.expect("torrent file list should become available");
    let video_ids: Vec<String> = pre
        .files
        .iter()
        .filter(|f| is_video_file(&f.path))
        .map(|f| f.id.to_string())
        .collect();
    assert!(
        !video_ids.is_empty(),
        "[{label}] torrent should contain a video file"
    );
    provider
        .select_files(&id, &video_ids.join(","))
        .await
        .expect("select_files");

    // 3. Poll until downloaded (Sintel is widely cached, so this is usually near-instant).
    let mut downloaded = None;
    for _ in 0..40 {
        match provider.get_torrent_info(&id).await {
            Ok(info) if info.status == "downloaded" => {
                downloaded = Some(info);
                break;
            }
            Ok(info) => println!("[{label}] status={} — waiting", info.status),
            Err(e) => println!("[{label}] get_torrent_info error: {e} — retrying"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let info = downloaded.expect("torrent should reach 'downloaded' (Sintel is widely cached)");
    println!("[{label}] downloaded; {} file(s)", info.files.len());

    // 4. Appears in the provider's list.
    assert!(
        find_id_by_hash(&provider, SINTEL_HASH).await.is_some(),
        "[{label}] torrent should appear in get_torrents()"
    );

    // 5. Appears in the built library (VFS).
    let metadata = identify_torrent(&info, &tmdb).await;
    let vfs = DebridVfs::build(vec![(info.clone(), metadata)]);
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
        if find_id_by_hash(&provider, SINTEL_HASH).await.is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(gone, "[{label}] torrent should disappear after delete");
    println!("[{label}] disappeared after delete ✓");
}

fn tmdb_client() -> Arc<TmdbClient> {
    let key = std::env::var("TMDB_API_KEY")
        .expect("TMDB_API_KEY must be set")
        .trim()
        .to_string();
    Arc::new(TmdbClient::new(key))
}

/// Read a non-empty, trimmed env var, or `None` if unset/blank (so a test can skip when
/// that provider's token isn't configured).
fn token(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[tokio::test]
#[ignore]
async fn lifecycle_real_debrid() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::try_init().ok();
    let Some(rd) = token("RD_API_TOKEN") else {
        println!("skipping lifecycle_real_debrid: RD_API_TOKEN not set");
        return;
    };
    let provider: Arc<dyn DebridProvider> = Arc::new(RealDebridClient::new(rd).unwrap());
    run_lifecycle(provider, tmdb_client(), "real-debrid").await;
}

#[tokio::test]
#[ignore]
async fn lifecycle_torbox() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::try_init().ok();
    let Some(tb) = token("TORBOX_API_KEY") else {
        println!("skipping lifecycle_torbox: TORBOX_API_KEY not set");
        return;
    };
    let provider: Arc<dyn DebridProvider> = Arc::new(TorBoxClient::new(tb).unwrap());
    run_lifecycle(provider, tmdb_client(), "torbox").await;
}
