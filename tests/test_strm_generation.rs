use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::{DebridVfs, VfsNode, VIDEO_EXTENSIONS};
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::repair::RepairManager;
use dav_server::fs::{DavFileSystem as DavFsTrait, OpenOptions};
use dav_server::davpath::DavPath;
use std::sync::Arc;
use tokio::sync::RwLock;
use futures_util::StreamExt;

/// Test that media files are correctly generated with valid Real-Debrid links
#[tokio::test]
#[ignore]
async fn test_media_file_generation() {
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
        let new_vfs = DebridVfs::build(current_data);
        let mut vfs_lock = vfs.write().await;
        *vfs_lock = new_vfs;
    }

    // Verify media files exist in VFS
    let mut media_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        collect_media_files(&vfs_lock.root, "", String::new(), &mut media_files);
    }

    assert!(!media_files.is_empty(), "No media files found in VFS");
    println!("Found {} media files in VFS", media_files.len());

    // Test reading media files through WebDAV (just open and check metadata)
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));
    let http_client = reqwest::Client::new();
    let dav_fs = DebridFileSystem::new(rd_client.clone(), vfs.clone(), repair_manager, http_client);

    // Test a few media files
    for (path_str, rd_link, file_size) in media_files.iter().take(3) {
        println!("\nTesting media file: {}", path_str);
        println!("  RD link: {}", rd_link);
        println!("  File size: {} bytes", file_size);

        let encoded_path = encode_path_preserve_slashes(&format!("/{}", path_str));
        let path = DavPath::new(&encoded_path).unwrap();

        // Open the media file
        let opts = OpenOptions {
            read: true,
            ..Default::default()
        };
        let mut file = dav_fs
            .open(&path, opts)
            .await
            .expect("Failed to open media file");

        // Verify metadata returns the correct file size
        let meta = file.metadata().await.expect("Failed to get file metadata");
        assert_eq!(meta.len(), *file_size, "Metadata size should match actual file size");

        // Verify it has a video file extension
        let has_video_ext = VIDEO_EXTENSIONS.iter().any(|ext| path_str.to_lowercase().ends_with(ext));
        assert!(
            has_video_ext,
            "File should have a video extension, got: {}",
            path_str
        );

        // Verify file size is reasonable (> 1KB for video files)
        assert!(
            *file_size > 1024,
            "Video file size should be > 1KB, got: {} bytes",
            file_size
        );

        println!("  ✓ Valid media file with correct size");
    }

    println!("\n✓ All media files validated successfully");
}

/// Test that media files keep their original extensions (not converted to .strm)
#[tokio::test]
#[ignore]
async fn test_media_filename_keeps_original_extension() {
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
        let new_vfs = DebridVfs::build(current_data);
        let mut vfs_lock = vfs.write().await;
        *vfs_lock = new_vfs;
    }

    // Check that all video files keep their original extensions
    let mut media_files = Vec::new();
    {
        let vfs_lock = vfs.read().await;
        collect_media_files(&vfs_lock.root, "", String::new(), &mut media_files);
    }

    println!("Checking {} media files for proper naming", media_files.len());

    for (path, _, _) in &media_files {
        // Should NOT end with .strm
        assert!(
            !path.ends_with(".strm"),
            "File should not have .strm extension: {}",
            path
        );

        // Should end with a video extension
        let has_video_ext = VIDEO_EXTENSIONS.iter().any(|ext| path.to_lowercase().ends_with(ext));
        assert!(
            has_video_ext,
            "File should have a video extension: {}",
            path
        );

        println!("  ✓ {}", path);
    }

    println!("\n✓ All media files have correct original extensions");
}

/// Test NFO files are generated alongside media files
#[tokio::test]
#[ignore]
async fn test_nfo_generation_with_media() {
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
        let new_vfs = DebridVfs::build(current_data);
        let mut vfs_lock = vfs.write().await;
        *vfs_lock = new_vfs;
    }

    // Check NFO files exist
    let mut nfo_count = 0;
    let mut media_folders = std::collections::HashSet::new();

    {
        let vfs_lock = vfs.read().await;
        count_nfo_and_folders(&vfs_lock.root, "", &mut nfo_count, &mut media_folders);
    }

    println!("Found {} NFO files", nfo_count);
    println!("Found {} folders with media files", media_folders.len());

    assert!(nfo_count > 0, "Should have generated NFO files");
    assert!(
        nfo_count >= media_folders.len(),
        "Should have at least one NFO per media folder"
    );

    println!("✓ NFO files properly generated with media files");
}

fn collect_media_files(
    node: &VfsNode,
    name: &str,
    current_path: String,
    files: &mut Vec<(String, String, u64)>,
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
                collect_media_files(child, child_name, next_path.clone(), files);
            }
        }
        VfsNode::MediaFile { rd_link, file_size, .. } => {
            let full_path = if current_path.is_empty() {
                name.to_string()
            } else {
                format!("{}/{}", current_path, name)
            };
            files.push((full_path, rd_link.clone(), *file_size));
        }
        VfsNode::VirtualFile { .. } => {}
    }
}

fn count_nfo_and_folders(
    node: &VfsNode,
    name: &str,
    nfo_count: &mut usize,
    media_folders: &mut std::collections::HashSet<String>,
) {
    match node {
        VfsNode::Directory { children } => {
            let has_media = children
                .values()
                .any(|child| matches!(child, VfsNode::MediaFile { .. }));
            let has_nfo = children
                .keys()
                .any(|k| k.ends_with(".nfo"));

            if has_media {
                media_folders.insert(name.to_string());
            }
            if has_nfo {
                *nfo_count += 1;
            }

            for (child_name, child) in children {
                count_nfo_and_folders(child, child_name, nfo_count, media_folders);
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
