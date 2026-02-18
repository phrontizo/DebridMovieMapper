use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::{DebridVfs, VfsNode};
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::repair::RepairManager;
use dav_server::fs::{DavFileSystem as DavFsTrait, OpenOptions};
use dav_server::davpath::DavPath;
use std::sync::Arc;
use tokio::sync::RwLock;
use futures_util::StreamExt;

/// Test that STRM files are correctly generated with valid Real-Debrid URLs
#[tokio::test]
#[ignore]
async fn test_strm_file_generation() {
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

    let rd_client = Arc::new(RealDebridClient::new(api_token));
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));

    println!("Fetching torrents...");
    let torrents = rd_client.get_torrents().await.expect("Failed to get torrents");
    let downloaded = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(5)
        .collect::<Vec<_>>();

    if downloaded.is_empty() {
        println!("No downloaded torrents, skipping test");
        return;
    }

    println!("Processing {} downloaded torrents", downloaded.len());

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
        .buffer_unordered(2);

    while let Some(result) = stream.next().await {
        current_data.push(result);
    }

    println!("Updating VFS...");
    {
        let mut vfs_lock = vfs.write().await;
        vfs_lock.update(current_data, rd_client.clone()).await;
    }

    // Verify STRM files exist in VFS
    let mut strm_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        collect_strm_files(&vfs_lock.root, String::new(), &mut strm_files);
    }

    assert!(!strm_files.is_empty(), "No STRM files found in VFS");
    println!("Found {} STRM files in VFS", strm_files.len());

    // Test reading STRM files through WebDAV
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));
    let dav_fs = DebridFileSystem::new(rd_client.clone(), vfs.clone(), repair_manager);

    // Test a few STRM files
    for (path_str, rd_link) in strm_files.iter().take(3) {
        println!("\nTesting STRM file: {}", path_str);
        println!("  Original RD link: {}", rd_link);

        let encoded_path = encode_path_preserve_slashes(&format!("/{}", path_str));
        let path = DavPath::new(&encoded_path).unwrap();

        // Open the STRM file
        let opts = OpenOptions {
            read: true,
            ..Default::default()
        };
        let mut file = dav_fs
            .open(&path, opts)
            .await
            .expect("Failed to open STRM file");

        // Read the STRM content
        let bytes = file
            .read_bytes(1024)
            .await
            .expect("Failed to read STRM file");
        assert!(!bytes.is_empty(), "STRM file is empty");

        let content = String::from_utf8_lossy(&bytes);
        let url = content.trim();

        println!("  STRM content: {}", url);

        // Verify it's a valid URL
        assert!(
            url.starts_with("http://") || url.starts_with("https://"),
            "STRM content should be a URL, got: {}",
            url
        );

        // Verify it's a Real-Debrid download URL
        assert!(
            url.contains("real-debrid.com") && url.contains("/d/"),
            "STRM should contain Real-Debrid download URL, got: {}",
            url
        );

        // Verify filename extension is .strm
        assert!(
            path_str.ends_with(".strm"),
            "File should have .strm extension, got: {}",
            path_str
        );

        println!("  ✓ Valid STRM file with RD download URL");
    }

    println!("\n✓ All STRM files validated successfully");
}

/// Test that STRM files are properly named (video extension replaced with .strm)
#[tokio::test]
#[ignore]
async fn test_strm_filename_conversion() {
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

    let rd_client = Arc::new(RealDebridClient::new(api_token));
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));

    let torrents = rd_client.get_torrents().await.expect("Failed to get torrents");
    let downloaded = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(3)
        .collect::<Vec<_>>();

    if downloaded.is_empty() {
        println!("No downloaded torrents, skipping test");
        return;
    }

    let mut current_data = Vec::new();
    for torrent in &downloaded {
        let info = rd_client
            .get_torrent_info(&torrent.id)
            .await
            .expect("Failed to get torrent info");
        let metadata = identify_torrent(&info, &tmdb_client).await;
        current_data.push((info, metadata));
    }

    {
        let mut vfs_lock = vfs.write().await;
        vfs_lock.update(current_data, rd_client.clone()).await;
    }

    // Check that all video files have been converted to .strm
    let mut strm_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        collect_strm_files(&vfs_lock.root, String::new(), &mut strm_files);
    }

    println!("Checking {} STRM files for proper naming", strm_files.len());

    for (path, _) in &strm_files {
        // Should end with .strm
        assert!(
            path.ends_with(".strm"),
            "File should end with .strm: {}",
            path
        );

        // Should NOT end with video extensions
        let video_exts = [".mkv", ".mp4", ".avi", ".m4v", ".mov", ".wmv"];
        for ext in &video_exts {
            assert!(
                !path.ends_with(&format!("{}.strm", ext)),
                "STRM file should not have double extension: {}",
                path
            );
        }

        println!("  ✓ {}", path);
    }

    println!("\n✓ All STRM files have correct naming");
}

/// Test NFO files are generated alongside STRM files
#[tokio::test]
#[ignore]
async fn test_nfo_generation_with_strm() {
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

    let rd_client = Arc::new(RealDebridClient::new(api_token));
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));

    let torrents = rd_client.get_torrents().await.expect("Failed to get torrents");
    let downloaded = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(5)
        .collect::<Vec<_>>();

    if downloaded.is_empty() {
        println!("No downloaded torrents, skipping test");
        return;
    }

    let mut current_data = Vec::new();
    for torrent in &downloaded {
        let info = rd_client
            .get_torrent_info(&torrent.id)
            .await
            .expect("Failed to get torrent info");
        let metadata = identify_torrent(&info, &tmdb_client).await;
        current_data.push((info, metadata));
    }

    {
        let mut vfs_lock = vfs.write().await;
        vfs_lock.update(current_data, rd_client.clone()).await;
    }

    // Check NFO files exist
    let mut nfo_count = 0;
    let mut strm_folders = std::collections::HashSet::new();

    {
        let vfs_lock = vfs.read().await;
        count_nfo_and_folders(&vfs_lock.root, &mut nfo_count, &mut strm_folders);
    }

    println!("Found {} NFO files", nfo_count);
    println!("Found {} folders with STRM files", strm_folders.len());

    assert!(nfo_count > 0, "Should have generated NFO files");
    assert!(
        nfo_count >= strm_folders.len(),
        "Should have at least one NFO per media folder"
    );

    println!("✓ NFO files properly generated with STRM files");
}

fn collect_strm_files(
    node: &VfsNode,
    current_path: String,
    files: &mut Vec<(String, String)>,
) {
    match node {
        VfsNode::Directory { name, children } => {
            let next_path = if current_path.is_empty() {
                name.clone()
            } else if name.is_empty() {
                current_path
            } else {
                format!("{}/{}", current_path, name)
            };
            for child in children.values() {
                collect_strm_files(child, next_path.clone(), files);
            }
        }
        VfsNode::StrmFile { name, rd_link, .. } => {
            let full_path = if current_path.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", current_path, name)
            };
            files.push((full_path, rd_link.clone()));
        }
        VfsNode::VirtualFile { .. } => {}
    }
}

fn count_nfo_and_folders(
    node: &VfsNode,
    nfo_count: &mut usize,
    strm_folders: &mut std::collections::HashSet<String>,
) {
    match node {
        VfsNode::Directory {
            name,
            children,
        } => {
            let has_strm = children
                .values()
                .any(|child| matches!(child, VfsNode::StrmFile { .. }));
            let has_nfo = children
                .values()
                .any(|child| matches!(child, VfsNode::VirtualFile { name, .. } if name.ends_with(".nfo")));

            if has_strm {
                strm_folders.insert(name.clone());
            }
            if has_nfo {
                *nfo_count += 1;
            }

            for child in children.values() {
                count_nfo_and_folders(child, nfo_count, strm_folders);
            }
        }
        _ => {}
    }
}

fn encode_path_preserve_slashes(p: &str) -> String {
    p.split('/')
        .map(|seg| urlencoding::encode(seg).to_string())
        .collect::<Vec<_>>()
        .join("/")
}
