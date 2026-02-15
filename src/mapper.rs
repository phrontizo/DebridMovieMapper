pub mod rd_client;
pub mod vfs;
pub mod dav_fs;
pub mod tmdb_client;
pub mod identification;
pub mod repair;

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, error};
use crate::rd_client::RealDebridClient;
use crate::vfs::{DebridVfs, MediaMetadata};
use crate::tmdb_client::TmdbClient;
use crate::identification::identify_torrent;
use futures_util::StreamExt;

pub async fn run_full_scan(
    rd_client: Arc<RealDebridClient>,
    tmdb_client: Arc<TmdbClient>,
    vfs: Arc<RwLock<DebridVfs>>
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting full scan...");
    let torrents = rd_client.get_torrents().await?;
    
    let downloaded_torrents: Vec<_> = torrents.into_iter()
        .filter(|t| t.status == "downloaded")
        .collect();
    
    info!("Identifying {} torrents...", downloaded_torrents.len());
    
    let mut current_data = Vec::new();
    let mut stream = futures_util::stream::iter(downloaded_torrents)
        .map(|torrent| {
            let rd_client = rd_client.clone();
            let tmdb_client = tmdb_client.clone();
            async move {
                match rd_client.get_torrent_info(&torrent.id).await {
                    Ok(info) => {
                        let metadata = identify_torrent(&info, &tmdb_client).await;
                        Ok::<(rd_client::TorrentInfo, MediaMetadata), reqwest::Error>((info, metadata))
                    }
                    Err(e) => Err(e),
                }
            }
        })
        .buffer_unordered(1);

    while let Some(result) = stream.next().await {
        match result {
            Ok((info, metadata)) => {
                current_data.push((info, metadata));
            }
            Err(e) => error!("Failed to identify torrent: {}", e),
        }
    }

    info!("Updating VFS with {} identified items", current_data.len());
    let mut vfs_lock = vfs.write().await;
    vfs_lock.update(current_data);
    
    Ok(())
}
