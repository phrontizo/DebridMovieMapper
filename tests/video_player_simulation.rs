use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::{DebridVfs, VfsNode};
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::repair::RepairManager;
use dav_server::fs::{DavFileSystem, OpenOptions};
use dav_server::davpath::DavPath;
use std::sync::Arc;
use tokio::sync::RwLock;
use futures_util::StreamExt;

#[tokio::test]
#[ignore]
async fn test_video_player_simulation() {
    tracing_subscriber::fmt::init();
    dotenvy::dotenv().ok();

    let api_token = std::env::var("RD_API_TOKEN").expect("RD_API_TOKEN must be set").trim().to_string();
    let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set").trim().to_string();

    let rd_client = Arc::new(RealDebridClient::new(api_token).unwrap());
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));

    println!("Fetching torrents...");
    let torrents = rd_client.get_torrents().await.expect("Failed to get torrents");
    let downloaded = torrents.into_iter().filter(|t| t.status == "downloaded").take(20).collect::<Vec<_>>();
    println!("Found {} downloaded torrents (sampled first 20)", downloaded.len());

    let mut current_data = Vec::new();
    let mut stream = futures_util::stream::iter(downloaded)
        .map(|torrent| {
            let rd_client = rd_client.clone();
            let tmdb_client = tmdb_client.clone();
            async move {
                let info = rd_client.get_torrent_info(&torrent.id).await.expect("Failed to get torrent info");
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
    let dav_fs = DebridFileSystem::new(rd_client.clone(), vfs.clone(), repair_manager);
    
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
    
    let movies: Vec<_> = video_files.iter().filter(|(p, _)| p.starts_with("Movies/")).collect();
    let shows: Vec<_> = video_files.iter().filter(|(p, _)| p.starts_with("Shows/")).collect();

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
        println!("Testing STRM file: {} (size: {} bytes)", path_str, size);
        let encoded_path = encode_path_preserve_slashes(&format!("/{}", path_str));
        let path = DavPath::new(&encoded_path).unwrap();
        let opts = OpenOptions {
            read: true,
            ..Default::default()
        };
        let mut file = dav_fs.open(&path, opts).await.expect("Failed to open STRM file");

        // STRM files are tiny - just read the whole thing
        println!("  Reading entire STRM file...");
        let bytes = file.read_bytes(*size as usize).await.expect("Failed to read STRM file");
        assert!(!bytes.is_empty(), "Read 0 bytes from STRM file {}", path_str);
        println!("    Read {} bytes", bytes.len());

        // Verify it's a valid URL
        let content = String::from_utf8_lossy(&bytes);
        let content = content.trim();
        assert!(content.starts_with("http://") || content.starts_with("https://"),
            "STRM file should contain a URL, got: {}", content);
        assert!(content.contains("real-debrid.com") || content.contains("download"),
            "STRM file should contain Real-Debrid download URL, got: {}", content);
        println!("    ✓ Valid Real-Debrid URL: {}...", &content[..std::cmp::min(50, content.len())]);
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
            let mut file = dav_fs.open(&path, opts).await.expect("Failed to open NFO file");
            let bytes = file.read_bytes(*size as usize).await.expect("Failed to read NFO file");
            assert_eq!(bytes.len() as u64, *size);
            let content = String::from_utf8_lossy(&bytes);
            assert!(content.starts_with("<?xml"));
            println!("    NFO content starts with <?xml");
        }
    }
}

/// Test that STRM file content matches metadata size and the URL is downloadable.
/// This catches the bug where metadata reports placeholder size but read_bytes returns
/// a longer unrestricted URL, causing WebDAV clients to truncate the content.
#[tokio::test]
#[ignore]
async fn test_strm_url_complete_and_downloadable() {
    let _ = tracing_subscriber::fmt::try_init();
    dotenvy::dotenv().ok();

    let api_token = std::env::var("RD_API_TOKEN").expect("RD_API_TOKEN must be set").trim().to_string();
    let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set").trim().to_string();

    let rd_client = Arc::new(RealDebridClient::new(api_token).unwrap());
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));

    // Fetch just 2 torrents — enough to get at least one STRM file
    let torrents = rd_client.get_torrents().await.expect("Failed to get torrents");
    let downloaded: Vec<_> = torrents.into_iter().filter(|t| t.status == "downloaded").take(2).collect();
    assert!(!downloaded.is_empty(), "Need at least one downloaded torrent");

    let mut current_data = Vec::new();
    let mut stream = futures_util::stream::iter(downloaded)
        .map(|torrent| {
            let rd_client = rd_client.clone();
            let tmdb_client = tmdb_client.clone();
            async move {
                let info = rd_client.get_torrent_info(&torrent.id).await.expect("Failed to get torrent info");
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
    let dav_fs = DebridFileSystem::new(rd_client.clone(), vfs.clone(), repair_manager);

    // Find a STRM file
    let mut strm_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        find_video_files(&vfs_lock.root, "", String::new(), &mut strm_files);
    }
    assert!(!strm_files.is_empty(), "No STRM files found in VFS");

    let (path_str, _) = &strm_files[0];
    println!("Testing STRM file: {}", path_str);

    let encoded_path = encode_path_preserve_slashes(&format!("/{}", path_str));
    let path = DavPath::new(&encoded_path).unwrap();
    let opts = OpenOptions { read: true, ..Default::default() };
    let mut file = dav_fs.open(&path, opts).await.expect("Failed to open STRM file");

    // Step 1: Get metadata size — this is what WebDAV clients use for Content-Length
    let meta = file.metadata().await.expect("Failed to get file metadata");
    let meta_size = meta.len();
    println!("  Metadata reports size: {} bytes", meta_size);

    // Step 2: Read exactly metadata-size bytes (mimics real WebDAV client behavior)
    let bytes = file.read_bytes(meta_size as usize).await.expect("Failed to read STRM file");
    let url = String::from_utf8_lossy(&bytes).trim().to_string();
    println!("  STRM content: {}", url);

    // Step 3: Verify URL is complete (not truncated)
    assert!(url.starts_with("https://"), "URL should start with https://, got: {}", url);
    assert!(url.contains("real-debrid.com"), "Should be a Real-Debrid URL, got: {}", url);
    assert!(url.contains("/d/"), "URL should contain /d/ path, got: {}", url);
    // Real-Debrid download URLs have long tokens after /d/ — truncated ones are very short
    let after_d = url.rsplit("/d/").next().unwrap_or("");
    assert!(after_d.len() > 10, "URL appears truncated — /d/ token too short ({}): {}", after_d.len(), url);

    // Step 4: Verify content length matches metadata
    assert_eq!(
        bytes.len() as u64, meta_size,
        "read_bytes should return exactly metadata-size bytes"
    );

    // Step 5: Download first 100KB from the unrestricted URL
    println!("  Downloading first 100KB from URL...");
    let http_client = reqwest::Client::new();
    let resp = http_client
        .get(&url)
        .header("Range", "bytes=0-102399")
        .send()
        .await
        .expect("Failed to HTTP GET the download URL");

    let status = resp.status();
    assert!(
        status.is_success() || status == reqwest::StatusCode::PARTIAL_CONTENT,
        "Download failed with status {}", status
    );

    let body = resp.bytes().await.expect("Failed to read response body");
    assert!(body.len() > 0, "Downloaded 0 bytes from {}", url);
    println!("  Downloaded {} bytes successfully", body.len());
    println!("  STRM URL is complete and downloadable");
}

fn encode_path_preserve_slashes(p: &str) -> String {
    p.split('/')
        .map(|seg| urlencoding::encode(seg).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn find_video_files(node: &VfsNode, name: &str, current_path: String, files: &mut Vec<(String, u64)>) {
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
        VfsNode::StrmFile { .. } => {
            // STRM files are tiny text files, use nominal size
            let full_path = if current_path.is_empty() {
                name.to_string()
            } else {
                format!("{}/{}", current_path, name)
            };
            files.push((full_path, 200)); // STRM files are ~200 bytes
        }
        VfsNode::VirtualFile { .. } => {}
    }
}

fn find_nfo_files(node: &VfsNode, name: &str, current_path: String, files: &mut Vec<(String, u64)>) {
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
        VfsNode::StrmFile { .. } => {}
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
