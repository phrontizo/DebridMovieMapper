use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn, error};
use futures_util::StreamExt;
use redb::{ReadableTable, ReadableDatabase, TableDefinition};
use crate::rd_client::RealDebridClient;
use crate::tmdb_client::TmdbClient;
use crate::vfs::{DebridVfs, MediaMetadata};
use crate::identification::identify_torrent;
use crate::repair::RepairManager;

const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");

pub async fn run_scan_loop(
    rd_client: Arc<RealDebridClient>,
    tmdb_client: Arc<TmdbClient>,
    vfs: Arc<RwLock<DebridVfs>>,
    db: Arc<redb::Database>,
    repair_manager: Arc<RepairManager>,
    interval_secs: u64,
) {
    // Load persisted matches from DB on startup
    let db_clone = db.clone();
    let persisted: HashMap<String, (crate::rd_client::TorrentInfo, MediaMetadata)> =
        tokio::task::spawn_blocking(move || {
            let mut map = HashMap::new();
            if let Ok(read_txn) = db_clone.begin_read() {
                if let Ok(table) = read_txn.open_table(MATCHES_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (key, value) = entry;
                            let id = key.value().to_string();
                            if let Ok(data) = serde_json::from_slice::<(
                                crate::rd_client::TorrentInfo,
                                MediaMetadata,
                            )>(
                                value.value()
                            ) {
                                map.insert(id, data);
                            }
                        }
                    }
                }
            }
            map
        })
        .await
        .unwrap_or_default();

    let mut seen_torrents = persisted;
    if !seen_torrents.is_empty() {
        info!("Loaded {} persistent matches from database.", seen_torrents.len());
    }

    info!("Scan task: running initial scan immediately");

    loop {
        info!("Refreshing torrent list...");
        match rd_client.get_torrents().await {
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
                        } else {
                            let db_clone = db.clone();
                            let torrent_id = torrent.id.clone();
                            let cached = tokio::task::spawn_blocking(move || {
                                let read_txn = db_clone.begin_read().ok()?;
                                let table = read_txn.open_table(MATCHES_TABLE).ok()?;
                                let entry = table.get(torrent_id.as_str()).ok()??;
                                serde_json::from_slice::<(
                                    crate::rd_client::TorrentInfo,
                                    MediaMetadata,
                                )>(entry.value())
                                .ok()
                            })
                            .await
                            .ok()
                            .flatten();

                            if let Some(data) = cached {
                                seen_torrents.insert(torrent.id.clone(), data.clone());
                                current_data.push(data);
                            } else {
                                to_identify.push(torrent.clone());
                            }
                        }
                    }
                }

                if !to_identify.is_empty() {
                    info!("Identifying {} new torrents...", to_identify.len());
                    let mut stream = futures_util::stream::iter(to_identify)
                        .map(|torrent| {
                            let rd_client = rd_client.clone();
                            let tmdb_client = tmdb_client.clone();
                            async move {
                                match rd_client.get_torrent_info(&torrent.id).await {
                                    Ok(info) => {
                                        let metadata =
                                            identify_torrent(&info, &tmdb_client).await;
                                        Ok::<
                                            (
                                                String,
                                                crate::rd_client::TorrentInfo,
                                                MediaMetadata,
                                            ),
                                            reqwest::Error,
                                        >((torrent.id, info, metadata))
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                        })
                        .buffer_unordered(1);

                    let new_total = torrents
                        .iter()
                        .filter(|t| t.status == "downloaded" && !seen_torrents.contains_key(&t.id))
                        .count();
                    let mut processed_new = 0;

                    while let Some(result) = stream.next().await {
                        processed_new += 1;
                        match result {
                            Ok((id, info, metadata)) => {
                                seen_torrents.insert(id.clone(), (info.clone(), metadata.clone()));
                                if let Ok(data_bytes) =
                                    serde_json::to_vec(&(info.clone(), metadata.clone()))
                                {
                                    let db_clone = db.clone();
                                    let id_clone = id.clone();
                                    let _ = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
                                        let write_txn = db_clone.begin_write()?;
                                        {
                                            let mut table = write_txn.open_table(MATCHES_TABLE)?;
                                            table.insert(id_clone.as_str(), data_bytes.as_slice())?;
                                        }
                                        write_txn.commit()?;
                                        Ok(())
                                    }).await;
                                }
                                current_data.push((info, metadata));
                            }
                            Err(e) => error!("Failed to identify torrent: {}", e),
                        }
                        if processed_new % 10 == 0 || processed_new == new_total {
                            info!(
                                "Progress: {}/{} new torrents identified",
                                processed_new, new_total
                            );
                            update_vfs(&vfs, &current_data, &repair_manager, &rd_client).await;
                        }
                    }
                } else {
                    update_vfs(&vfs, &current_data, &repair_manager, &rd_client).await;
                }

                let current_ids: std::collections::HashSet<String> =
                    torrents.iter().map(|t| t.id.clone()).collect();
                seen_torrents.retain(|id, _| current_ids.contains(id));
                info!("VFS update complete.");
            }
            Err(e) => error!("Failed to get torrents: {}", e),
        }

        info!("Scan task: sleeping {}s until next scan", interval_secs);
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

async fn update_vfs(
    vfs: &Arc<RwLock<DebridVfs>>,
    current_data: &[(crate::rd_client::TorrentInfo, MediaMetadata)],
    repair_manager: &Arc<RepairManager>,
    rd_client: &Arc<RealDebridClient>,
) {
    let mut filtered = Vec::new();
    for (torrent_info, metadata) in current_data {
        if !repair_manager.should_hide_torrent(&torrent_info.id).await {
            filtered.push((torrent_info.clone(), metadata.clone()));
        }
    }
    // Build VFS without holding the lock to avoid blocking WebDAV reads during scans
    let new_vfs = DebridVfs::build(filtered, rd_client.clone()).await;
    // Only hold write lock briefly to swap
    let mut vfs_lock = vfs.write().await;
    *vfs_lock = new_vfs;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check: run_scan_loop has the expected signature.
    #[allow(dead_code)]
    async fn _assert_run_scan_loop_signature(
        rd_client: Arc<RealDebridClient>,
        tmdb_client: Arc<TmdbClient>,
        vfs: Arc<RwLock<DebridVfs>>,
        db: Arc<redb::Database>,
        repair_manager: Arc<RepairManager>,
    ) {
        run_scan_loop(rd_client, tmdb_client, vfs, db, repair_manager, 60).await;
    }

}
