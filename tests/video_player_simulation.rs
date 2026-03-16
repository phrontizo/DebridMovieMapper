use dav_server::davpath::DavPath;
use dav_server::fs::{DavFileSystem, OpenOptions};
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::repair::RepairManager;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::{DebridVfs, VfsNode};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::test]
#[ignore]
async fn test_video_player_simulation() {
    let _ = tracing_subscriber::fmt::try_init();
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
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));

    println!("Fetching torrents...");
    let torrents = rd_client
        .get_torrents()
        .await
        .expect("Failed to get torrents");
    let downloaded = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(20)
        .collect::<Vec<_>>();
    println!(
        "Found {} downloaded torrents (sampled first 20)",
        downloaded.len()
    );

    let mut current_data = Vec::new();
    let mut stream = futures_util::stream::iter(downloaded)
        .map(|torrent| {
            let rd_client = rd_client.clone();
            let tmdb_client = tmdb_client.clone();
            async move {
                let info = rd_client
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

    println!("Updating VFS...");
    {
        let new_vfs = DebridVfs::build(current_data);
        let mut vfs_lock = vfs.write().await;
        *vfs_lock = new_vfs;
    }

    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));
    let http_client = reqwest::Client::new();
    let dav_fs = DebridFileSystem::new(rd_client.clone(), vfs.clone(), repair_manager, http_client);

    let mut video_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        find_video_files(&vfs_lock.root, "", String::new(), &mut video_files);
    }

    if video_files.is_empty() {
        println!("No video files found in VFS, skipping simulation.");
        return;
    }

    // Select a few movies and shows at random
    use rand::seq::SliceRandom;
    let mut rng = rand::thread_rng();

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
        println!("Testing media file: {} (size: {} bytes)", path_str, size);
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

        // Read first 64KB to simulate player probing the file header
        println!("  Reading first 64KB (simulating ffprobe)...");
        let probe_bytes = file
            .read_bytes(65536)
            .await
            .expect("Failed to read media file");
        assert!(
            !probe_bytes.is_empty(),
            "Read 0 bytes from media file {}",
            path_str
        );
        println!("    Read {} bytes", probe_bytes.len());
        println!("    ✓ Successfully read media bytes from CDN proxy");
    }

    // 4. Verify NFO files
    println!("Verifying NFO files...");
    let mut nfo_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        find_nfo_files(&vfs_lock.root, "", String::new(), &mut nfo_files);
    }

    if nfo_files.is_empty() {
        println!("No NFO files found in VFS, skipping verification.");
    } else {
        println!("Found {} NFO files", nfo_files.len());
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

/// Test that media file metadata sizes are consistent across all WebDAV paths.
/// Verifies:
/// - DavFileSystem::metadata() (PROPFIND / GET Content-Length)
/// - DavFile::metadata() (opened file metadata)
/// - Both report the actual file size from TorrentFile::bytes
#[tokio::test]
#[ignore]
async fn test_media_file_size_consistency() {
    let _ = tracing_subscriber::fmt::try_init();
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

    // Fetch just 2 torrents — enough to get at least one media file
    let torrents = rd_client
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
            let rd_client = rd_client.clone();
            let tmdb_client = tmdb_client.clone();
            async move {
                let info = rd_client
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

    let vfs = Arc::new(RwLock::new(DebridVfs::build(current_data)));
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));
    let http_client = reqwest::Client::new();
    let dav_fs = DebridFileSystem::new(rd_client.clone(), vfs.clone(), repair_manager, http_client);

    // Find a media file
    let mut media_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        find_video_files(&vfs_lock.root, "", String::new(), &mut media_files);
    }
    assert!(!media_files.is_empty(), "No media files found in VFS");

    let (path_str, expected_size) = &media_files[0];
    println!("Testing media file: {}", path_str);

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

    println!("  ✓ Media file sizes are consistent across all metadata paths");
}

fn encode_path_preserve_slashes(p: &str) -> String {
    p.split('/')
        .map(|seg| urlencoding::encode(seg).to_string())
        .collect::<Vec<_>>()
        .join("/")
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
