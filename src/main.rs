use std::sync::Arc;
use std::error::Error;
use tokio::sync::RwLock;
use tracing::{info, warn, error};
use std::time::Duration;
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::vfs::{DebridVfs, MediaMetadata};
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::repair::RepairManager;
use dav_server::DavHandler;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use std::net::SocketAddr;
use hyper::service::service_fn;
use hyper::Request;
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let api_token = std::env::var("RD_API_TOKEN").expect("RD_API_TOKEN must be set").trim().to_string();
    let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set").trim().to_string();

    // Parse configuration from environment variables
    let scan_interval_secs = std::env::var("SCAN_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60);

    let repair_interval_secs = std::env::var("REPAIR_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3600); // Default 1 hour

    info!("Configuration:");
    info!("  Scan interval: {} seconds", scan_interval_secs);
    info!("  Repair interval: {} seconds ({} minutes)", repair_interval_secs, repair_interval_secs / 60);

    let rd_client = Arc::new(RealDebridClient::new(api_token));
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));

    let db = sled::open("metadata.db").expect("Failed to open database");
    let tree = db.open_tree("matches").expect("Failed to open database tree");

    // Background repair task
    let repair_manager_clone = repair_manager.clone();
    let tree_repair = tree.clone();
    let repair_interval = repair_interval_secs;
    tokio::spawn(async move {
        // Wait for the configured interval before starting first repair check
        info!("Repair task: waiting {} seconds before first health check", repair_interval);
        tokio::time::sleep(Duration::from_secs(repair_interval)).await;

        loop {
            info!("Starting torrent health check...");

            // Load current torrents from DB
            let mut current_torrents = Vec::new();
            for result in tree_repair.iter().flatten() {
                let (_id_bytes, data_bytes) = result;
                if let Ok(data) = serde_json::from_slice::<(debridmoviemapper::rd_client::TorrentInfo, MediaMetadata)>(&data_bytes) {
                    current_torrents.push(data);
                }
            }

            // Check health
            let broken_ids = repair_manager_clone.check_torrent_health(&current_torrents).await;

            // Try to repair broken torrents
            if !broken_ids.is_empty() {
                info!("Found {} broken torrents, initiating repairs...", broken_ids.len());
                let mut repaired = 0;
                let mut repair_failed = 0;

                for broken_id in broken_ids {
                    if let Some((torrent_info, _)) = current_torrents.iter().find(|(t, _)| t.id == broken_id) {
                        match repair_manager_clone.repair_torrent(torrent_info).await {
                            Ok(_) => {
                                repaired += 1;
                                // Trigger VFS refresh by removing from DB (it will be re-added)
                                let _ = tree_repair.remove(&broken_id);
                            }
                            Err(e) => {
                                error!("REPAIR FAILED for torrent '{}' ({}): {}", torrent_info.filename, broken_id, e);
                                repair_failed += 1;
                            }
                        }
                    }
                }

                info!("Repair cycle complete: {} repaired, {} failed", repaired, repair_failed);
            } else {
                info!("No broken torrents found, all healthy!");
            }

            let (healthy, repairing, failed) = repair_manager_clone.get_status_summary().await;
            info!("Overall repair status: {} healthy, {} in repair, {} permanently failed", healthy, repairing, failed);

            info!("Repair task: sleeping for {} seconds until next check", repair_interval);
            tokio::time::sleep(Duration::from_secs(repair_interval)).await;
        }
    });

    // Background refresh/scan task
    let rd_client_clone = rd_client.clone();
    let tmdb_client_clone = tmdb_client.clone();
    let vfs_clone = vfs.clone();
    let tree_clone = tree.clone();
    let repair_manager_vfs = repair_manager.clone();
    let scan_interval = scan_interval_secs;
    tokio::spawn(async move {
        let mut seen_torrents: std::collections::HashMap<String, (debridmoviemapper::rd_client::TorrentInfo, MediaMetadata)> = std::collections::HashMap::new();

        // Initial load from DB
        for result in tree_clone.iter().flatten() {
            let (id_bytes, data_bytes) = result;
            let id = String::from_utf8_lossy(&id_bytes).to_string();
            if let Ok(data) = serde_json::from_slice::<(debridmoviemapper::rd_client::TorrentInfo, MediaMetadata)>(&data_bytes) {
                seen_torrents.insert(id, data);
            }
        }
        if !seen_torrents.is_empty() {
            info!("Loaded {} persistent matches from database.", seen_torrents.len());
        }

        // Run scan immediately on startup
        info!("Scan task: running initial scan immediately");

        loop {
            info!("Refreshing torrent list...");
            match rd_client_clone.get_torrents().await {
                Ok(torrents) => {
                    if torrents.is_empty() {
                        warn!("No torrents found in Real Debrid account.");
                    }
                    let mut current_data = Vec::new();
                    let mut to_identify = Vec::new();
                    for torrent in &torrents {
                        if torrent.status == "downloaded" {
                            if let Some(data) = seen_torrents.get(&torrent.id) {
                                current_data.push(data.clone());
                            } else if let Ok(Some(data_bytes)) = tree_clone.get(&torrent.id) {
                                // Try to load from DB if not in memory
                                if let Ok(data) = serde_json::from_slice::<(debridmoviemapper::rd_client::TorrentInfo, MediaMetadata)>(&data_bytes) {
                                    seen_torrents.insert(torrent.id.clone(), data.clone());
                                    current_data.push(data);
                                } else {
                                    to_identify.push(torrent.clone());
                                }
                            } else {
                                to_identify.push(torrent.clone());
                            }
                        }
                    }
                    
                    if !to_identify.is_empty() {
                        info!("Identifying {} new torrents...", to_identify.len());
                        let mut stream = futures_util::stream::iter(to_identify)
                            .map(|torrent| {
                                let rd_client = rd_client_clone.clone();
                                let tmdb_client = tmdb_client_clone.clone();
                                async move {
                                    match rd_client.get_torrent_info(&torrent.id).await {
                                        Ok(info) => {
                                            let metadata = identify_torrent(&info, &tmdb_client).await;
                                            Ok::<(String, debridmoviemapper::rd_client::TorrentInfo, MediaMetadata), reqwest::Error>((torrent.id, info, metadata))
                                        }
                                        Err(e) => Err(e),
                                    }
                                }
                            })
                            .buffer_unordered(1);

                        let new_total = torrents.iter().filter(|t| t.status == "downloaded" && !seen_torrents.contains_key(&t.id)).count();
                        let mut processed_new = 0;

                        while let Some(result) = stream.next().await {
                            processed_new += 1;
                            match result {
                                Ok((id, info, metadata)) => {
                                    seen_torrents.insert(id.clone(), (info.clone(), metadata.clone()));
                                    // Persist to DB
                                    if let Ok(data_bytes) = serde_json::to_vec(&(info.clone(), metadata.clone())) {
                                        let _ = tree_clone.insert(id, data_bytes);
                                    }
                                    current_data.push((info, metadata));
                                }
                                Err(e) => error!("Failed to identify torrent: {}", e),
                            }
                            if processed_new % 10 == 0 || processed_new == new_total {
                                info!("Progress: {}/{} new torrents identified (Total: {}/{} downloaded)", processed_new, new_total, current_data.len(), torrents.iter().filter(|t| t.status == "downloaded").count());

                                // Filter out torrents under repair
                                let mut filtered_data = Vec::new();
                                for (torrent_info, metadata) in &current_data {
                                    if !repair_manager_vfs.should_hide_torrent(&torrent_info.id).await {
                                        filtered_data.push((torrent_info.clone(), metadata.clone()));
                                    }
                                }

                                let mut vfs_lock = vfs_clone.write().await;
                                vfs_lock.update(filtered_data);
                            }
                        }
                    } else {
                        // Filter out torrents under repair
                        let mut filtered_data = Vec::new();
                        for (torrent_info, metadata) in &current_data {
                            if !repair_manager_vfs.should_hide_torrent(&torrent_info.id).await {
                                filtered_data.push((torrent_info.clone(), metadata.clone()));
                            }
                        }

                        let mut vfs_lock = vfs_clone.write().await;
                        vfs_lock.update(filtered_data);
                    }

                    // Clean up seen_torrents that are no longer in the list
                    let current_ids: std::collections::HashSet<String> = torrents.iter().map(|t| t.id.clone()).collect();
                    seen_torrents.retain(|id, _| current_ids.contains(id));
                    info!("VFS update complete.");
                }
                Err(e) => error!("Failed to get torrents: {}", e),
            }
            info!("Scan task: sleeping for {} seconds until next scan", scan_interval);
            tokio::time::sleep(Duration::from_secs(scan_interval)).await;
        }
    });

    let dav_fs = DebridFileSystem::new(rd_client.clone(), vfs.clone(), repair_manager.clone());
    let dav_handler = DavHandler::builder()
        .filesystem(Box::new(dav_fs))
        .locksystem(dav_server::fakels::FakeLs::new())
        .build_handler();

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = TcpListener::bind(addr).await?;
    info!("WebDAV server listening on http://{}", addr);

    loop {
        let (stream, addr) = listener.accept().await?;
        info!("New WebDAV connection from {}", addr);
        let io = TokioIo::new(stream);
        let dav_handler = dav_handler.clone();

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(move |req: Request<hyper::body::Incoming>| {
                    let dav_handler = dav_handler.clone();
                    async move {
                        Ok::<_, hyper::Error>(dav_handler.handle(req).await)
                    }
                }))
                .await
            {
                if let Some(io_err) = err.source().and_then(|s| s.downcast_ref::<std::io::Error>()) {
                    if io_err.kind() == std::io::ErrorKind::ConnectionReset || io_err.kind() == std::io::ErrorKind::BrokenPipe {
                        info!("WebDAV connection closed by peer ({})", io_err.kind());
                        return;
                    }
                }
                let err_str = format!("{:?}", err);
                if err_str.contains("IncompleteMessage") {
                    info!("WebDAV connection closed by peer (IncompleteMessage)");
                    return;
                }
                error!("Error serving connection: {:?}", err);
            }
        });
    }
}

