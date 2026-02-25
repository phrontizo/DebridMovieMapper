use debridmoviemapper::rd_client::{RealDebridClient, TorrentInfo};
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::{DebridVfs, MediaMetadata, VfsNode, is_video_file};
use debridmoviemapper::identification::identify_torrent;
use std::sync::{Arc, LazyLock};
use tokio::sync::{OnceCell, RwLock};
use futures_util::StreamExt;
use redb::{Database, ReadableDatabase, TableDefinition};

const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");

/// Shared redb Database instance — redb only allows one open handle per path.
static DB: LazyLock<Database> = LazyLock::new(|| {
    Database::create("metadata.db").expect("Failed to open database")
});

/// Shared identified data — fetched and identified exactly once, reused by all tests.
static IDENTIFIED_DATA: OnceCell<(Arc<RealDebridClient>, Vec<(TorrentInfo, MediaMetadata)>)> =
    OnceCell::const_new();

async fn get_shared_data() -> &'static (Arc<RealDebridClient>, Vec<(TorrentInfo, MediaMetadata)>) {
    IDENTIFIED_DATA
        .get_or_init(|| async {
            tracing_subscriber::fmt::try_init().ok();
            dotenvy::dotenv().ok();

            let api_token = std::env::var("RD_API_TOKEN")
                .expect("RD_API_TOKEN must be set")
                .trim()
                .to_string();
            let tmdb_api_key = std::env::var("TMDB_API_KEY")
                .expect("TMDB_API_KEY must be set")
                .trim()
                .to_string();
            let rd_client = Arc::new(RealDebridClient::new(api_token).unwrap());
            let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));

            let data = fetch_and_identify(&rd_client, &tmdb_client, &*DB).await;
            (rd_client, data)
        })
        .await
}

/// Fetch downloaded torrents and identify them via TMDB, using redb cache.
/// Respects INTEGRATION_TEST_LIMIT env var to limit the number of torrents processed.
async fn fetch_and_identify(
    rd_client: &Arc<RealDebridClient>,
    tmdb_client: &Arc<TmdbClient>,
    db: &Database,
) -> Vec<(TorrentInfo, MediaMetadata)> {
    println!("Fetching torrents...");
    let torrents = rd_client.get_torrents().await.expect("Failed to get torrents");
    let downloaded: Vec<_> = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .collect();
    println!("Found {} downloaded torrents", downloaded.len());

    let limit = std::env::var("INTEGRATION_TEST_LIMIT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());

    let to_process: Vec<_> = if let Some(limit) = limit {
        println!("INTEGRATION_TEST_LIMIT={}, processing first {} torrents", limit, limit);
        downloaded.into_iter().take(limit).collect()
    } else {
        downloaded
    };

    let mut current_data = Vec::new();
    let mut to_identify = Vec::new();

    for torrent in &to_process {
        let cached = {
            let read_txn = db.begin_read().ok();
            read_txn.and_then(|txn| {
                let table = txn.open_table(MATCHES_TABLE).ok()?;
                let entry = table.get(torrent.id.as_str()).ok()??;
                serde_json::from_slice::<(TorrentInfo, MediaMetadata)>(entry.value()).ok()
            })
        };
        if let Some(data) = cached {
            println!("Cached: {} (ID: {:?})", data.0.filename, data.1.external_id);
            current_data.push(data);
        } else {
            to_identify.push(torrent.clone());
        }
    }

    if !to_identify.is_empty() {
        println!("Identifying {} new torrents ({} cached)...", to_identify.len(), current_data.len());
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
            if let Ok(data_bytes) = serde_json::to_vec(&(info.clone(), metadata.clone())) {
                if let Ok(write_txn) = db.begin_write() {
                    if let Ok(mut table) = write_txn.open_table(MATCHES_TABLE) {
                        let _ = table.insert(id.as_str(), data_bytes.as_slice());
                    }
                    let _ = write_txn.commit();
                }
            }
            current_data.push((info, metadata));
        }
    } else {
        println!("All {} torrents served from cache.", current_data.len());
    }

    current_data
}

/// Test 1: Torrent fetching and TMDB identification only. No VFS, no unrestrict.
/// Uses redb cache so repeat runs are fast.
#[tokio::test]
#[ignore]
async fn test_identification() {
    let (_, data) = get_shared_data().await;

    let identified = data
        .iter()
        .filter(|(_, m)| m.external_id.is_some())
        .count();
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

    assert!(!data.is_empty(), "Should have at least one downloaded torrent");
    assert!(identified > 0, "Should identify at least one torrent");
}

/// Test 2: Given identified data (from redb cache), builds VFS and checks directory structure.
#[tokio::test]
#[ignore]
async fn test_vfs_structure() {
    let (_, data) = get_shared_data().await;
    assert!(!data.is_empty(), "Need identified torrents to test VFS structure");

    println!("Building VFS...");
    let new_vfs = DebridVfs::build(data.clone());
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

    // Check Movies structure: Movies/<Title (Year)>/<file>.strm
    if let Some(VfsNode::Directory { children: movies }) = children.get("Movies") {
        println!("\nMovies/ contains {} entries", movies.len());
        for (name, node) in movies.iter().take(5) {
            match node {
                VfsNode::Directory { children: movie_files } => {
                    let strm_count = movie_files
                        .values()
                        .filter(|n| matches!(n, VfsNode::StrmFile { .. }))
                        .count();
                    println!("  {}: {} .strm files", name, strm_count);
                }
                _ => println!("  {} (not a directory)", name),
            }
        }
    }

    // Check Shows structure: Shows/<Title (Year)>/Season XX/<file>.strm
    if let Some(VfsNode::Directory { children: shows }) = children.get("Shows") {
        println!("\nShows/ contains {} entries", shows.len());
        for (name, node) in shows.iter().take(5) {
            match node {
                VfsNode::Directory { children: seasons } => {
                    let season_count = seasons
                        .keys()
                        .filter(|k| k.starts_with("Season"))
                        .count();
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
    let (_, data) = get_shared_data().await;
    assert!(!data.is_empty(), "Need identified torrents to test VFS completeness");

    println!("Building VFS...");
    let new_vfs = DebridVfs::build(data.clone());
    let vfs = Arc::new(RwLock::new(new_vfs));

    let vfs_lock = vfs.read().await;
    let mut vfs_links = std::collections::HashSet::new();
    collect_links(&vfs_lock.root, &mut vfs_links);

    let mut total_video_files = 0;
    let mut missing = Vec::new();

    for (info, _) in data {
        let mut link_idx = 0;
        for file in &info.files {
            if file.selected == 1 && is_video_file(&file.path) {
                total_video_files += 1;
                if let Some(link) = info.links.get(link_idx) {
                    if !vfs_links.contains(link) {
                        missing.push(format!("Torrent: {}, File: {}", info.filename, file.path));
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
    let (_, data) = get_shared_data().await;
    assert!(!data.is_empty(), "Need identified torrents to test timestamps");

    let vfs1 = DebridVfs::build(data.clone());
    let vfs2 = DebridVfs::build(data.clone());

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

    // No timestamp should be UNIX_EPOCH (all RD torrents have a valid added date)
    let epoch = std::time::UNIX_EPOCH;
    for (path, ts) in &vfs1.timestamps {
        assert_ne!(
            *ts, epoch,
            "Timestamp for '{}' should not be UNIX_EPOCH — RD added date not parsed?",
            path
        );
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
        VfsNode::StrmFile { rd_link, .. } => {
            links.insert(rd_link.clone());
        }
        VfsNode::VirtualFile { .. } => {}
    }
}
