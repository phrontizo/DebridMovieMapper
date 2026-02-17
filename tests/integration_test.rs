use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::{DebridVfs, VfsNode, is_video_file};
use debridmoviemapper::identification::identify_torrent;
use std::sync::Arc;
use tokio::sync::RwLock;
use futures_util::StreamExt;

#[tokio::test]
async fn test_all_torrents_identification() {
    tracing_subscriber::fmt::init();
    dotenvy::dotenv().ok();

    let api_token = std::env::var("RD_API_TOKEN").expect("RD_API_TOKEN must be set").trim().to_string();
    let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set").trim().to_string();

    let rd_client = Arc::new(RealDebridClient::new(api_token));
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));

    println!("Fetching torrents...");
    let torrents = rd_client.get_torrents().await.expect("Failed to get torrents");
    let downloaded = torrents.into_iter().filter(|t| t.status == "downloaded").collect::<Vec<_>>();
    println!("Found {} downloaded torrents", downloaded.len());

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
        println!("Identified: {} as {:?} (ID: {:?})", result.0.filename, result.1.media_type, result.1.external_id);
        current_data.push(result);
    }

    println!("Updating VFS...");
    {
        let mut vfs_lock = vfs.write().await;
        vfs_lock.update(current_data.clone());
    }

    println!("Verifying VFS contents...");
    let vfs_lock = vfs.read().await;
    
    let mut vfs_links = std::collections::HashSet::new();
    collect_links(&vfs_lock.root, &mut vfs_links);

    let mut missing = Vec::new();
    for (info, _) in &current_data {
        let mut link_idx = 0;
        for file in &info.files {
            if file.selected == 1 && is_video_file(&file.path) {
                if let Some(link) = info.links.get(link_idx) {
                    if !vfs_links.contains(link) {
                        missing.push(format!("Torrent: {}, File: {}", info.filename, file.path));
                    }
                }
                link_idx += 1;
            }
        }
    }

    if !missing.is_empty() {
        println!("The following video files are missing from VFS:");
        for m in &missing {
            println!("  {}", m);
        }
        panic!("{} video files missing from VFS", missing.len());
    } else {
        println!("All video files correctly identified and presented in VFS!");
    }
}

fn collect_links(node: &VfsNode, links: &mut std::collections::HashSet<String>) {
    match node {
        VfsNode::Directory { children, .. } => {
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
