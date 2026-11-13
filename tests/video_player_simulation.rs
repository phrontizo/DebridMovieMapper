//! Live WebDAV player-simulation tests (`#[ignore]`). They need `TMDB_API_KEY` plus `RD_API_TOKEN`
//! and/or `TORBOX_API_KEY`, and skip a provider whose token is unset or blank.
//!
//! They only READ the account: they list it, open files from its first downloaded torrents through
//! `DebridFileSystem` and read their first bytes. Every provider call goes through
//! `common::ReadOnlyProvider`, which refuses adds, file selections and deletes. A file the provider
//! reports unavailable would otherwise trigger an instant repair of that library torrent (TorBox
//! would restart a lapsed download, Real-Debrid would delete the original once a copy is cached),
//! in an account a production deployment may share; here the repair's re-add is refused, so the
//! repair gives up and the read fails instead.

mod common;

use common::{encode_path_preserve_slashes, tmdb_client, token, ReadOnlyProvider};
use dav_server::davpath::DavPath;
use dav_server::fs::{DavFileSystem, OpenOptions};
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::error::AppError;
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::provider::{DebridProvider, FileLocator};
use debridmoviemapper::rd_client::{
    AddMagnetResponse, RealDebridClient, Torrent, TorrentFile, TorrentInfo,
};
use debridmoviemapper::repair::RepairManager;
use debridmoviemapper::torbox_client::TorBoxClient;
use debridmoviemapper::vfs::{DebridVfs, VfsNode};
use futures_util::StreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

// --- Player simulation: open media files via WebDAV and read real CDN bytes (per provider) ---

#[tokio::test]
#[ignore]
async fn test_video_player_simulation_real_debrid() {
    let _ = tracing_subscriber::fmt::try_init();
    let Some(t) = token("RD_API_TOKEN") else {
        println!("skipping test_video_player_simulation_real_debrid: RD_API_TOKEN not set");
        return;
    };
    run_player_simulation(Arc::new(RealDebridClient::new(t).unwrap()), "real-debrid").await;
}

#[tokio::test]
#[ignore]
async fn test_video_player_simulation_torbox() {
    let _ = tracing_subscriber::fmt::try_init();
    let Some(t) = token("TORBOX_API_KEY") else {
        println!("skipping test_video_player_simulation_torbox: TORBOX_API_KEY not set");
        return;
    };
    run_player_simulation(Arc::new(TorBoxClient::new(t).unwrap()), "torbox").await;
}

/// Build a small VFS from the account, then open a couple of media files through the WebDAV
/// filesystem and read their first bytes — exercising the full provider `resolve_url` → CDN
/// fetch path (for TorBox this is `requestdl` → the selected CDN). Read-only: see the module docs.
async fn run_player_simulation(provider: Arc<dyn DebridProvider>, label: &str) {
    // Every call below, the repair manager's included, goes through the read-only wrapper.
    let provider: Arc<dyn DebridProvider> = Arc::new(ReadOnlyProvider(provider));
    let tmdb_client = tmdb_client();
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));

    println!("[{label}] Fetching torrents...");
    let torrents = provider
        .get_torrents()
        .await
        .expect("Failed to get torrents");
    let downloaded = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(20)
        .collect::<Vec<_>>();
    println!(
        "[{label}] Found {} downloaded torrents (sampled first 20)",
        downloaded.len()
    );

    let mut current_data = Vec::new();
    let mut stream = futures_util::stream::iter(downloaded)
        .map(|torrent| {
            let provider = provider.clone();
            let tmdb_client = tmdb_client.clone();
            async move {
                let info = provider
                    .get_torrent_info(&torrent.id)
                    .await
                    .expect("Failed to get torrent info");
                let metadata = identify_torrent(&info, &tmdb_client).await;
                (info, metadata)
            }
        })
        .buffer_unordered(1);
    while let Some(result) = stream.next().await {
        current_data.push(result);
    }

    println!("[{label}] Updating VFS...");
    {
        let new_vfs = DebridVfs::build(current_data, &debridmoviemapper::vfs::SelectionMap::new());
        let mut vfs_lock = vfs.write().await;
        *vfs_lock = new_vfs;
    }

    let repair_manager = Arc::new(RepairManager::new(provider.clone()));
    let http_client = reqwest::Client::new();
    let ra = std::sync::Arc::new(debridmoviemapper::read_activity::ReadActivity::new());
    let dav_fs = DebridFileSystem::new(
        provider.clone(),
        vfs.clone(),
        repair_manager,
        http_client,
        ra,
    );

    let mut video_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        find_video_files(&vfs_lock.root, "", String::new(), &mut video_files);
    }

    if video_files.is_empty() {
        println!("[{label}] No video files found in VFS, skipping simulation.");
        return;
    }

    // Select a few movies and shows at random
    // rand 0.9 moved `choose` from `SliceRandom` to the `IndexedRandom` trait.
    use rand::seq::IndexedRandom;
    let mut rng = rand::rng();

    let movies: Vec<_> = video_files
        .iter()
        .filter(|(p, _)| p.starts_with("Movies/"))
        .collect();
    let shows: Vec<_> = video_files
        .iter()
        .filter(|(p, _)| p.starts_with("Shows/"))
        .collect();

    let mut selected = Vec::new();
    if let Some(m) = movies.choose(&mut rng) {
        selected.push(*m);
    }
    if let Some(s) = shows.choose(&mut rng) {
        selected.push(*s);
    }

    // If we didn't get enough from both, just pick some from all
    if selected.len() < 2 {
        for _ in 0..(2 - selected.len()) {
            if let Some(any) = video_files.choose(&mut rng) {
                if !selected.contains(&any) {
                    selected.push(any);
                }
            }
        }
    }

    for (path_str, size) in selected {
        println!(
            "[{label}] Testing media file: {} (size: {} bytes)",
            path_str, size
        );
        let encoded_path = encode_path_preserve_slashes(&format!("/{}", path_str));
        let path = DavPath::new(&encoded_path).unwrap();
        let opts = OpenOptions {
            read: true,
            ..Default::default()
        };
        let mut file = dav_fs
            .open(&path, opts)
            .await
            .expect("Failed to open media file");

        // Verify metadata returns correct size
        let meta = file.metadata().await.expect("Failed to get file metadata");
        println!("  Metadata size: {} bytes", meta.len());
        assert_eq!(
            meta.len(),
            *size,
            "Metadata size should match VFS file size"
        );

        // Read first 64KB to simulate player probing the file header — this is the
        // real provider resolve + CDN byte fetch.
        println!("  Reading first 64KB (simulating ffprobe)...");
        let probe_bytes = file.read_bytes(65536).await.unwrap_or_else(|e| {
            panic!(
                "[{label}] reading {path_str} failed: {e:?}. If the provider reported the file \
                 unavailable, the instant repair that would follow was refused (this test never \
                 changes the account; see the read-only provider's log line above)"
            )
        });
        assert!(
            !probe_bytes.is_empty(),
            "Read 0 bytes from media file {}",
            path_str
        );
        println!("    Read {} bytes", probe_bytes.len());
        println!("    ✓ [{label}] Successfully read media bytes from the CDN proxy");
    }

    // 4. Verify NFO files
    println!("[{label}] Verifying NFO files...");
    let mut nfo_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        find_nfo_files(&vfs_lock.root, "", String::new(), &mut nfo_files);
    }

    if nfo_files.is_empty() {
        println!("[{label}] No NFO files found in VFS, skipping verification.");
    } else {
        println!("[{label}] Found {} NFO files", nfo_files.len());
        for (path_str, size) in nfo_files.iter().take(5) {
            println!("  Checking NFO: {} (size: {})", path_str, size);
            let encoded_path = encode_path_preserve_slashes(&format!("/{}", path_str));
            let path = DavPath::new(&encoded_path).unwrap();
            let opts = OpenOptions {
                read: true,
                ..Default::default()
            };
            let mut file = dav_fs
                .open(&path, opts)
                .await
                .expect("Failed to open NFO file");
            let bytes = file
                .read_bytes(*size as usize)
                .await
                .expect("Failed to read NFO file");
            assert_eq!(bytes.len() as u64, *size);
            let content = String::from_utf8_lossy(&bytes);
            assert!(content.starts_with("<?xml"));
            println!("    NFO content starts with <?xml");
        }
    }
}

// --- Metadata size consistency across WebDAV paths (per provider) ---

#[tokio::test]
#[ignore]
async fn test_media_file_size_consistency_real_debrid() {
    let _ = tracing_subscriber::fmt::try_init();
    let Some(t) = token("RD_API_TOKEN") else {
        println!("skipping test_media_file_size_consistency_real_debrid: RD_API_TOKEN not set");
        return;
    };
    run_size_consistency(Arc::new(RealDebridClient::new(t).unwrap()), "real-debrid").await;
}

#[tokio::test]
#[ignore]
async fn test_media_file_size_consistency_torbox() {
    let _ = tracing_subscriber::fmt::try_init();
    let Some(t) = token("TORBOX_API_KEY") else {
        println!("skipping test_media_file_size_consistency_torbox: TORBOX_API_KEY not set");
        return;
    };
    run_size_consistency(Arc::new(TorBoxClient::new(t).unwrap()), "torbox").await;
}

/// Verify a media file's size is reported consistently by both `DavFileSystem::metadata()`
/// (PROPFIND / GET Content-Length) and the opened `DavFile::metadata()`. Read-only: see the module
/// docs.
async fn run_size_consistency(provider: Arc<dyn DebridProvider>, label: &str) {
    let provider: Arc<dyn DebridProvider> = Arc::new(ReadOnlyProvider(provider));
    let tmdb_client = tmdb_client();

    // Fetch just 2 torrents — enough to get at least one media file
    let torrents = provider
        .get_torrents()
        .await
        .expect("Failed to get torrents");
    let downloaded: Vec<_> = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(2)
        .collect();
    assert!(
        !downloaded.is_empty(),
        "Need at least one downloaded torrent"
    );

    let mut current_data = Vec::new();
    let mut stream = futures_util::stream::iter(downloaded)
        .map(|torrent| {
            let provider = provider.clone();
            let tmdb_client = tmdb_client.clone();
            async move {
                let info = provider
                    .get_torrent_info(&torrent.id)
                    .await
                    .expect("Failed to get torrent info");
                let metadata = identify_torrent(&info, &tmdb_client).await;
                (info, metadata)
            }
        })
        .buffer_unordered(1);
    while let Some(result) = stream.next().await {
        current_data.push(result);
    }

    let vfs = Arc::new(RwLock::new(DebridVfs::build(
        current_data,
        &debridmoviemapper::vfs::SelectionMap::new(),
    )));
    let repair_manager = Arc::new(RepairManager::new(provider.clone()));
    let http_client = reqwest::Client::new();
    let ra = std::sync::Arc::new(debridmoviemapper::read_activity::ReadActivity::new());
    let dav_fs = DebridFileSystem::new(
        provider.clone(),
        vfs.clone(),
        repair_manager,
        http_client,
        ra,
    );

    // Find a media file
    let mut media_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        find_video_files(&vfs_lock.root, "", String::new(), &mut media_files);
    }
    assert!(!media_files.is_empty(), "No media files found in VFS");

    let (path_str, expected_size) = &media_files[0];
    println!("[{label}] Testing media file: {}", path_str);

    let encoded_path = encode_path_preserve_slashes(&format!("/{}", path_str));
    let dav_path = DavPath::new(&encoded_path).unwrap();

    // Step 1: Verify DavFileSystem::metadata() reports actual file size
    let fs_meta = dav_fs
        .metadata(&dav_path)
        .await
        .expect("DavFileSystem::metadata failed");
    let fs_meta_size = fs_meta.len();
    println!("  DavFileSystem::metadata() size: {} bytes", fs_meta_size);
    assert_eq!(
        fs_meta_size, *expected_size,
        "PROPFIND metadata should report actual file size"
    );

    // Step 2: Open file and verify DavFile::metadata() matches
    let opts = OpenOptions {
        read: true,
        ..Default::default()
    };
    let mut file = dav_fs
        .open(&dav_path, opts)
        .await
        .expect("Failed to open media file");
    let file_meta = file.metadata().await.expect("DavFile::metadata failed");
    let file_meta_size = file_meta.len();
    println!("  DavFile::metadata() size: {} bytes", file_meta_size);
    assert_eq!(
        file_meta_size, fs_meta_size,
        "DavFile and DavFileSystem metadata sizes must match"
    );

    // Step 3: Verify size is reasonable for a video file
    assert!(
        file_meta_size > 1024,
        "Video file size should be > 1KB, got: {} bytes",
        file_meta_size
    );

    println!("  ✓ [{label}] Media file sizes are consistent across all metadata paths");
}

// --- Offline checks of `common::ReadOnlyProvider`, which keeps the live tests above read-only ---

/// A provider that answers reads with canned data and counts every call that would change the
/// account, so a test can show the read-only wrapper never forwards one.
#[derive(Debug, Default)]
struct CountingProvider {
    reads: AtomicUsize,
    changes: AtomicUsize,
}

impl CountingProvider {
    fn torrent_info() -> TorrentInfo {
        TorrentInfo {
            id: "42".to_string(),
            hash: "0123456789abcdef0123456789abcdef01234567".to_string(),
            status: "downloaded".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "/Some.Film.2010.1080p.mkv".to_string(),
                bytes: 1_000_000,
                selected: 1,
            }],
            ..Default::default()
        }
    }
}

#[async_trait::async_trait]
impl DebridProvider for CountingProvider {
    fn name(&self) -> &'static str {
        "counting"
    }
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
    async fn get_torrent_info(&self, _id: &str) -> Result<TorrentInfo, reqwest::Error> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(Self::torrent_info())
    }
    async fn add_magnet(&self, _magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        self.changes.fetch_add(1, Ordering::SeqCst);
        Ok(AddMagnetResponse {
            id: "43".to_string(),
            uri: String::new(),
        })
    }
    async fn select_files(&self, _torrent_id: &str, _file_ids: &str) -> Result<(), reqwest::Error> {
        self.changes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn delete_torrent(&self, _torrent_id: &str) -> Result<(), reqwest::Error> {
        self.changes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn resolve_url(&self, _loc: &FileLocator) -> Result<String, AppError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Err(AppError::Unavailable)
    }
    async fn invalidate(&self, _loc: &FileLocator) {}
}

#[tokio::test]
async fn read_only_provider_forwards_reads_and_refuses_account_changes() {
    let inner = Arc::new(CountingProvider::default());
    let provider = ReadOnlyProvider(inner.clone());

    assert_eq!(provider.name(), "counting");
    assert!(provider.get_torrents().await.is_ok());
    assert_eq!(provider.get_torrent_info("42").await.unwrap().id, "42");
    assert!(matches!(
        provider.resolve_url(&FileLocator::default()).await,
        Err(AppError::Unavailable)
    ));
    assert_eq!(
        inner.reads.load(Ordering::SeqCst),
        3,
        "reads reach the account"
    );

    assert!(provider.add_magnet("magnet:?xt=urn:btih:00").await.is_err());
    assert!(provider.select_files("42", "1").await.is_err());
    assert!(provider.delete_torrent("42").await.is_err());
    assert_eq!(
        inner.changes.load(Ordering::SeqCst),
        0,
        "no add, select or delete may reach the account"
    );
}

fn counting_locator() -> FileLocator {
    let info = CountingProvider::torrent_info();
    FileLocator {
        hash: info.hash.clone(),
        torrent_id: info.id.clone(),
        file_id: 1,
        file_path: info.files[0].path.clone(),
        link: None,
    }
}

#[tokio::test(start_paused = true)]
async fn instant_repair_through_the_bare_provider_changes_the_account() {
    // The control for the test below: without the wrapper the same repair re-adds the torrent,
    // selects its file and deletes the original.
    let inner = Arc::new(CountingProvider::default());
    let repair = RepairManager::new(inner.clone());
    let outcome = repair.try_instant_repair(&counting_locator()).await;
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(inner.changes.load(Ordering::SeqCst) > 0);
}

#[tokio::test(start_paused = true)]
async fn instant_repair_through_the_read_only_provider_leaves_the_account_alone() {
    // A read that finds its file unavailable triggers an instant repair, which re-adds the torrent
    // (TorBox restarts a lapsed download; Real-Debrid deletes the original once a copy is cached).
    // Through the read-only wrapper the re-add is refused, so the repair gives up and changes
    // nothing.
    let inner = Arc::new(CountingProvider::default());
    let repair = RepairManager::new(Arc::new(ReadOnlyProvider(inner.clone())));

    let outcome = repair.try_instant_repair(&counting_locator()).await;
    assert!(
        outcome.is_err(),
        "the repair can't succeed without a re-add: {outcome:?}"
    );
    assert_eq!(
        inner.changes.load(Ordering::SeqCst),
        0,
        "the repair must not add, select or delete anything"
    );
}

fn find_video_files(
    node: &VfsNode,
    name: &str,
    current_path: String,
    files: &mut Vec<(String, u64)>,
) {
    match node {
        VfsNode::Directory { children } => {
            let next_path = if current_path.is_empty() {
                name.to_string()
            } else if name.is_empty() {
                current_path
            } else {
                format!("{}/{}", current_path, name)
            };
            for (child_name, child) in children {
                find_video_files(child, child_name, next_path.clone(), files);
            }
        }
        VfsNode::MediaFile { file_size, .. } => {
            let full_path = if current_path.is_empty() {
                name.to_string()
            } else {
                format!("{}/{}", current_path, name)
            };
            files.push((full_path, *file_size));
        }
        VfsNode::VirtualFile { .. } => {}
    }
}

fn find_nfo_files(
    node: &VfsNode,
    name: &str,
    current_path: String,
    files: &mut Vec<(String, u64)>,
) {
    match node {
        VfsNode::Directory { children } => {
            let next_path = if current_path.is_empty() {
                name.to_string()
            } else if name.is_empty() {
                current_path
            } else {
                format!("{}/{}", current_path, name)
            };
            for (child_name, child) in children {
                find_nfo_files(child, child_name, next_path.clone(), files);
            }
        }
        VfsNode::MediaFile { .. } => {}
        VfsNode::VirtualFile { content } => {
            if name.ends_with(".nfo") {
                let full_path = if current_path.is_empty() {
                    name.to_string()
                } else {
                    format!("{}/{}", current_path, name)
                };
                files.push((full_path, content.len() as u64));
            }
        }
    }
}
