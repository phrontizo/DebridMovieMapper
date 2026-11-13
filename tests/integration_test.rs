//! Live Real-Debrid identification and VFS tests (`#[ignore]`). They need `RD_API_TOKEN` and
//! `TMDB_API_KEY`, skip when `RD_API_TOKEN` is unset or blank, and only read the account.
//!
//! Identifications are cached between runs in a `Store` under Cargo's scratch directory for
//! integration tests (`CARGO_TARGET_TMPDIR`), never in the service's own `metadata.db`.

mod common;

use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::rd_client::{RealDebridClient, TorrentInfo};
use debridmoviemapper::store::Store;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::{is_video_file, DebridVfs, MediaMetadata, VfsNode};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::{OnceCell, RwLock};

/// Shared identified data, fetched and identified once and reused by every test. `None` when
/// `RD_API_TOKEN` is unset or blank, and the tests then skip.
static IDENTIFIED_DATA: OnceCell<Option<Vec<(TorrentInfo, MediaMetadata)>>> = OnceCell::const_new();

/// The identification cache's path.
fn cache_path() -> String {
    format!(
        "{}/integration_test_cache.redb",
        env!("CARGO_TARGET_TMPDIR")
    )
}

async fn get_shared_data() -> Option<&'static Vec<(TorrentInfo, MediaMetadata)>> {
    IDENTIFIED_DATA
        .get_or_init(|| async {
            tracing_subscriber::fmt::try_init().ok();
            let api_token = common::token("RD_API_TOKEN")?;
            let tmdb_client = common::tmdb_client();
            let rd_client = Arc::new(RealDebridClient::new(api_token).unwrap());
            let store = Store::open(&cache_path())
                .expect("Failed to open the integration-test identification cache");

            Some(fetch_and_identify(&rd_client, &tmdb_client, &store).await)
        })
        .await
        .as_ref()
}

/// Fetch downloaded torrents and identify them via TMDB, using the store's identification cache.
/// Respects INTEGRATION_TEST_LIMIT env var to limit the number of torrents processed.
async fn fetch_and_identify(
    rd_client: &Arc<RealDebridClient>,
    tmdb_client: &Arc<TmdbClient>,
    store: &Store,
) -> Vec<(TorrentInfo, MediaMetadata)> {
    println!("Fetching torrents...");
    let torrents = rd_client
        .get_torrents()
        .await
        .expect("Failed to get torrents");
    let downloaded: Vec<_> = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .collect();
    println!("Found {} downloaded torrents", downloaded.len());

    let limit = std::env::var("INTEGRATION_TEST_LIMIT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());

    let to_process: Vec<_> = if let Some(limit) = limit {
        println!(
            "INTEGRATION_TEST_LIMIT={}, processing first {} torrents",
            limit, limit
        );
        downloaded.into_iter().take(limit).collect()
    } else {
        downloaded
    };

    let mut current_data = Vec::new();
    let mut to_identify = Vec::new();

    for torrent in &to_process {
        if let Some(data) = store.get_match(torrent.id.clone()).await {
            println!("Cached: {} (ID: {:?})", data.0.filename, data.1.external_id);
            current_data.push(data);
        } else {
            to_identify.push(torrent.clone());
        }
    }

    if !to_identify.is_empty() {
        println!(
            "Identifying {} new torrents ({} cached)...",
            to_identify.len(),
            current_data.len()
        );
        let mut stream = futures_util::stream::iter(to_identify)
            .map(|torrent| {
                let rd_client = rd_client.clone();
                let tmdb_client = tmdb_client.clone();
                async move {
                    let info = rd_client
                        .get_torrent_info(&torrent.id)
                        .await
                        .expect("Failed to get torrent info");
                    let metadata = identify_torrent(&info, &tmdb_client).await;
                    (torrent.id, info, metadata)
                }
            })
            .buffer_unordered(1);

        while let Some((id, info, metadata)) = stream.next().await {
            println!(
                "Identified: {} as {:?} (ID: {:?})",
                info.filename, metadata.media_type, metadata.external_id
            );
            if let Err(e) = store
                .put_matches(vec![(id, info.clone(), metadata.clone())])
                .await
            {
                println!("WARNING: failed to cache identification: {e}");
            }
            current_data.push((info, metadata));
        }
    } else {
        println!("All {} torrents served from cache.", current_data.len());
    }

    current_data
}

/// Test 1: Torrent fetching and TMDB identification only. No VFS, no unrestrict.
/// Uses the identification cache so repeat runs are fast.
#[tokio::test]
#[ignore]
async fn test_identification() {
    let Some(data) = get_shared_data().await else {
        println!("skipping test_identification: RD_API_TOKEN not set");
        return;
    };

    let identified = data.iter().filter(|(_, m)| m.external_id.is_some()).count();
    let unidentified = data.len() - identified;

    println!("\n=== Identification Summary ===");
    println!("Total torrents: {}", data.len());
    println!("Identified:     {}", identified);
    println!("Unidentified:   {}", unidentified);

    if unidentified > 0 {
        println!("\nUnidentified torrents:");
        for (info, _) in data.iter().filter(|(_, m)| m.external_id.is_none()) {
            println!("  - {}", info.filename);
        }
    }

    assert!(
        !data.is_empty(),
        "Should have at least one downloaded torrent"
    );
    assert!(identified > 0, "Should identify at least one torrent");
}

/// Test 2: Given identified data (from the identification cache), builds VFS and checks directory
/// structure.
#[tokio::test]
#[ignore]
async fn test_vfs_structure() {
    let Some(data) = get_shared_data().await else {
        println!("skipping test_vfs_structure: RD_API_TOKEN not set");
        return;
    };
    assert!(
        !data.is_empty(),
        "Need identified torrents to test VFS structure"
    );

    println!("Building VFS...");
    let new_vfs = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());
    let vfs = Arc::new(RwLock::new(new_vfs));

    let vfs_lock = vfs.read().await;
    let root = &vfs_lock.root;

    // Root must be a directory
    let children = match root {
        VfsNode::Directory { children } => children,
        _ => panic!("Root should be a directory"),
    };

    println!("\n=== VFS Structure ===");
    println!("Root entries: {:?}", children.keys().collect::<Vec<_>>());

    // Check for Movies/ and/or Shows/ directories
    let has_movies = children.contains_key("Movies");
    let has_shows = children.contains_key("Shows");
    println!("Has Movies/: {}", has_movies);
    println!("Has Shows/:  {}", has_shows);

    assert!(
        has_movies || has_shows,
        "VFS should contain at least Movies/ or Shows/ directory"
    );

    // Check Movies structure: Movies/<Title (Year)>/<file>.mkv|.mp4
    if let Some(VfsNode::Directory { children: movies }) = children.get("Movies") {
        println!("\nMovies/ contains {} entries", movies.len());
        for (name, node) in movies.iter().take(5) {
            match node {
                VfsNode::Directory {
                    children: movie_files,
                } => {
                    let media_count = movie_files
                        .values()
                        .filter(|n| matches!(n, VfsNode::MediaFile { .. }))
                        .count();
                    println!("  {}: {} media files", name, media_count);
                }
                _ => println!("  {} (not a directory)", name),
            }
        }
    }

    // Check Shows structure: Shows/<Title (Year)>/Season XX/<file>.mkv|.mp4
    if let Some(VfsNode::Directory { children: shows }) = children.get("Shows") {
        println!("\nShows/ contains {} entries", shows.len());
        for (name, node) in shows.iter().take(5) {
            match node {
                VfsNode::Directory { children: seasons } => {
                    let season_count = seasons.keys().filter(|k| k.starts_with("Season")).count();
                    println!("  {}: {} seasons", name, season_count);
                }
                _ => println!("  {} (not a directory)", name),
            }
        }
    }
}

/// Test 3: VFS link completeness with tolerance for unrestrict failures.
#[tokio::test]
#[ignore]
async fn test_vfs_completeness() {
    let Some(data) = get_shared_data().await else {
        println!("skipping test_vfs_completeness: RD_API_TOKEN not set");
        return;
    };
    assert!(
        !data.is_empty(),
        "Need identified torrents to test VFS completeness"
    );

    println!("Building VFS...");
    let new_vfs = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());
    let vfs = Arc::new(RwLock::new(new_vfs));

    let vfs_lock = vfs.read().await;
    let mut vfs_links = std::collections::HashSet::new();
    collect_links(&vfs_lock.root, &mut vfs_links);

    let mut total_video_files = 0;
    let mut missing = Vec::new();

    for (info, _) in data {
        let mut link_idx = 0;
        for file in &info.files {
            if file.selected == 1 {
                if is_video_file(&file.path) {
                    total_video_files += 1;
                    if let Some(link) = info.links.get(link_idx) {
                        if !vfs_links.contains(link) {
                            missing
                                .push(format!("Torrent: {}, File: {}", info.filename, file.path));
                        }
                    }
                }
                link_idx += 1;
            }
        }
    }

    let missing_count = missing.len();
    let skip_rate = if total_video_files > 0 {
        missing_count as f64 / total_video_files as f64
    } else {
        0.0
    };

    println!("\n=== VFS Completeness ===");
    println!("Total video files: {}", total_video_files);
    println!("Present in VFS:    {}", total_video_files - missing_count);
    println!("Missing from VFS:  {}", missing_count);
    println!("Skip rate:         {:.1}%", skip_rate * 100.0);

    if !missing.is_empty() {
        println!("\nMissing video files (likely unrestrict failures):");
        for m in &missing {
            println!("  WARNING: {}", m);
        }
    }

    // Allow up to 20% skip rate (unrestrict failures under rate limiting)
    let max_skip_rate = 0.20;
    assert!(
        skip_rate <= max_skip_rate,
        "Too many video files missing from VFS: {}/{} ({:.1}% > {:.0}% threshold). \
         This suggests a real bug, not just rate limiting.",
        missing_count,
        total_video_files,
        skip_rate * 100.0,
        max_skip_rate * 100.0
    );

    if missing.is_empty() {
        println!("\nAll video files correctly present in VFS!");
    } else {
        println!(
            "\n{} files missing but within tolerance ({:.1}% <= {:.0}%)",
            missing_count,
            skip_rate * 100.0,
            max_skip_rate * 100.0
        );
    }
}

/// Test 4: VFS timestamps are populated from RD data and stable across rebuilds.
#[tokio::test]
#[ignore]
async fn test_vfs_timestamps_stable() {
    let Some(data) = get_shared_data().await else {
        println!("skipping test_vfs_timestamps_stable: RD_API_TOKEN not set");
        return;
    };
    assert!(
        !data.is_empty(),
        "Need identified torrents to test timestamps"
    );

    let vfs1 = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());
    let vfs2 = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());

    // Timestamps should be populated
    assert!(
        !vfs1.timestamps.is_empty(),
        "VFS timestamps should be populated from RD data"
    );

    // Every directory and file in the tree should have a timestamp
    let mut paths = Vec::new();
    collect_paths(&vfs1.root, "", &mut paths);
    for path in &paths {
        assert!(
            vfs1.timestamps.contains_key(path),
            "Missing timestamp for path: {}",
            path
        );
    }

    // Timestamps should be identical across two builds of the same data
    assert_eq!(
        vfs1.timestamps, vfs2.timestamps,
        "Timestamps must be stable across VFS rebuilds"
    );

    // No timestamp should be UNIX_EPOCH, except for empty top-level directories
    // (e.g. "Movies" when INTEGRATION_TEST_LIMIT only picks up shows)
    let epoch = std::time::UNIX_EPOCH;
    for (path, ts) in &vfs1.timestamps {
        if *ts == epoch {
            // Only top-level dirs ("Movies", "Shows") are allowed to be EPOCH when empty
            assert!(
                path == "Movies" || path == "Shows",
                "Timestamp for '{}' should not be UNIX_EPOCH — RD added date not parsed?",
                path
            );
        }
    }

    println!("\n=== VFS Timestamps ===");
    println!("Total paths with timestamps: {}", vfs1.timestamps.len());
    println!("All timestamps stable across rebuilds: ✓");
    println!("No UNIX_EPOCH fallbacks: ✓");
}

fn collect_paths(node: &VfsNode, prefix: &str, paths: &mut Vec<String>) {
    if let VfsNode::Directory { children } = node {
        for (name, child) in children {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            paths.push(path.clone());
            collect_paths(child, &path, paths);
        }
    }
}

fn collect_links(node: &VfsNode, links: &mut std::collections::HashSet<String>) {
    match node {
        VfsNode::Directory { children } => {
            for child in children.values() {
                collect_links(child, links);
            }
        }
        VfsNode::MediaFile { locator, .. } => {
            if let Some(link) = &locator.link {
                links.insert(link.clone());
            }
        }
        VfsNode::VirtualFile { .. } => {}
    }
}
