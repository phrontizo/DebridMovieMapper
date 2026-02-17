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
async fn test_video_player_simulation() {
    tracing_subscriber::fmt::init();
    dotenvy::dotenv().ok();

    let api_token = std::env::var("RD_API_TOKEN").expect("RD_API_TOKEN must be set").trim().to_string();
    let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set").trim().to_string();

    let rd_client = Arc::new(RealDebridClient::new(api_token));
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
        let mut vfs_lock = vfs.write().await;
        vfs_lock.update(current_data, rd_client.clone()).await;
    }

    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));
    let dav_fs = DebridFileSystem::new(rd_client.clone(), vfs.clone(), repair_manager);
    
    let mut video_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        find_video_files(&vfs_lock.root, String::new(), &mut video_files);
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
        find_nfo_files(&vfs_lock.root, String::new(), &mut nfo_files);
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

fn encode_path_preserve_slashes(p: &str) -> String {
    p.split('/')
        .map(|seg| urlencoding::encode(seg).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn find_video_files(node: &VfsNode, current_path: String, files: &mut Vec<(String, u64)>) {
    match node {
        VfsNode::Directory { name, children, .. } => {
            let next_path = if current_path.is_empty() {
                name.clone()
            } else if name.is_empty() {
                current_path
            } else {
                format!("{}/{}", current_path, name)
            };
            for child in children.values() {
                find_video_files(child, next_path.clone(), files);
            }
        }
        VfsNode::StrmFile { name, .. } => {
            // STRM files are tiny text files, use nominal size
            let full_path = if current_path.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", current_path, name)
            };
            files.push((full_path, 200)); // STRM files are ~200 bytes
        }
        VfsNode::VirtualFile { .. } => {}
    }
}

fn find_nfo_files(node: &VfsNode, current_path: String, files: &mut Vec<(String, u64)>) {
    match node {
        VfsNode::Directory { name, children, .. } => {
            let next_path = if current_path.is_empty() {
                name.clone()
            } else if name.is_empty() {
                current_path
            } else {
                format!("{}/{}", current_path, name)
            };
            for child in children.values() {
                find_nfo_files(child, next_path.clone(), files);
            }
        }
        VfsNode::StrmFile { .. } => {}
        VfsNode::VirtualFile { name, content } => {
            if name.ends_with(".nfo") {
                let full_path = if current_path.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", current_path, name)
                };
                files.push((full_path, content.len() as u64));
            }
        }
    }
}
