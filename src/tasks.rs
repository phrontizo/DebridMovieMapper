use crate::identification::identify_torrent;
use crate::rd_client::RealDebridClient;
use crate::repair::RepairManager;
use crate::tmdb_client::TmdbClient;
use crate::vfs::{DebridVfs, MediaMetadata};
use futures_util::StreamExt;
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

pub const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");

pub struct ScanConfig {
    pub rd_client: Arc<RealDebridClient>,
    pub tmdb_client: Arc<TmdbClient>,
    pub vfs: Arc<RwLock<DebridVfs>>,
    pub db: Arc<redb::Database>,
    pub repair_manager: Arc<RepairManager>,
    pub interval_secs: u64,
    pub jellyfin_client: Option<Arc<crate::jellyfin_client::JellyfinClient>>,
}

pub async fn run_scan_loop(config: ScanConfig, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let ScanConfig {
        rd_client,
        tmdb_client,
        vfs,
        db,
        repair_manager,
        interval_secs,
        jellyfin_client,
    } = config;
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
                            )>(value.value())
                            {
                                map.insert(id, data);
                            }
                        }
                    }
                }
            }
            map
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to load persisted matches: {:?}", e);
            HashMap::new()
        });

    let mut seen_torrents = persisted;
    if !seen_torrents.is_empty() {
        info!(
            "Loaded {} persistent matches from database.",
            seen_torrents.len()
        );
    }

    // Pre-populate the VFS from persisted data so the first scan's diff
    // only captures genuinely new/changed content, not the entire library.
    if !seen_torrents.is_empty() {
        let persisted_data: Vec<_> = seen_torrents.values().cloned().collect();
        update_vfs(&vfs, &persisted_data, &repair_manager, &None).await;
        info!(
            "Pre-populated VFS with {} persisted entries",
            persisted_data.len()
        );
    }

    info!("Scan task: running initial scan immediately");

    loop {
        if *shutdown.borrow() {
            info!("Scan task: shutdown requested, exiting");
            return;
        }
        // Consume repair replacements (new_id → old_id) before processing torrents
        let repair_replacements = repair_manager.take_repair_replacements().await;
        if !repair_replacements.is_empty() {
            info!(
                "Processing {} repair replacement(s)",
                repair_replacements.len()
            );
        }

        info!("Refreshing torrent list...");
        match rd_client.get_torrents().await {
            Ok(torrents) => {
                if torrents.is_empty() {
                    warn!("No torrents found in Real Debrid account.");
                }

                // Deduplicate torrents by hash — keep the newest "downloaded" entry per hash.
                // Duplicates arise when repair's add_magnet leaks a torrent, or when
                // external tools (e.g. DebridMediaManager) re-add the same hash.
                let mut seen_hashes: HashMap<String, usize> = HashMap::new();
                let mut deduped_torrents: Vec<&crate::rd_client::Torrent> = Vec::new();
                for torrent in &torrents {
                    if torrent.status != "downloaded" || torrent.hash.is_empty() {
                        deduped_torrents.push(torrent);
                        continue;
                    }
                    if let Some(&existing_idx) = seen_hashes.get(&torrent.hash) {
                        // Keep the one we already have (first seen = first in API response = newest),
                        // schedule the duplicate for deletion
                        let kept = &deduped_torrents[existing_idx];
                        warn!("Duplicate hash {} found: keeping torrent {} ({}), deleting duplicate {} ({})",
                            torrent.hash, kept.id, kept.filename, torrent.id, torrent.filename);
                        let rd = rd_client.clone();
                        let dup_id = torrent.id.clone();
                        tokio::spawn(async move {
                            if let Err(e) = rd.delete_torrent(&dup_id).await {
                                tracing::error!(
                                    "Failed to delete duplicate torrent {}: {}",
                                    dup_id,
                                    e
                                );
                            }
                        });
                    } else {
                        seen_hashes.insert(torrent.hash.clone(), deduped_torrents.len());
                        deduped_torrents.push(torrent);
                    }
                }

                let mut current_data = Vec::new();
                let mut to_identify = Vec::new();
                for torrent in &deduped_torrents {
                    if torrent.status == "downloaded" {
                        if let Some(data) = seen_torrents.get(&torrent.id) {
                            current_data.push(data.clone());
                        } else if let Some(old_id) = repair_replacements.get(&torrent.id) {
                            // This torrent is a repair replacement — reuse old identification
                            if let Some((old_info, old_metadata)) = seen_torrents.get(old_id) {
                                info!(
                                    "Reusing identification for repair replacement {} → {} ({})",
                                    old_id, torrent.id, old_info.filename
                                );
                                let metadata = old_metadata.clone();
                                // Get fresh torrent info for the new ID
                                match rd_client.get_torrent_info(&torrent.id).await {
                                    Ok(new_info) => {
                                        // Serialize from references before cloning for owned storage
                                        if let Ok(data_bytes) =
                                            serde_json::to_vec(&(&new_info, &metadata))
                                        {
                                            let db_clone = db.clone();
                                            let new_id = torrent.id.clone();
                                            let old_id = old_id.clone();
                                            if let Err(e) = tokio::task::spawn_blocking(
                                                move || -> Result<(), redb::Error> {
                                                    let write_txn = db_clone.begin_write()?;
                                                    {
                                                        let mut table =
                                                            write_txn.open_table(MATCHES_TABLE)?;
                                                        table.remove(old_id.as_str())?;
                                                        table.insert(
                                                            new_id.as_str(),
                                                            data_bytes.as_slice(),
                                                        )?;
                                                    }
                                                    write_txn.commit()?;
                                                    Ok(())
                                                },
                                            )
                                            .await
                                            {
                                                error!("Failed to persist repair replacement to database: {:?}", e);
                                            }
                                        }
                                        seen_torrents.insert(
                                            torrent.id.clone(),
                                            (new_info.clone(), metadata.clone()),
                                        );
                                        current_data.push((new_info, metadata));
                                    }
                                    Err(e) => {
                                        error!("Failed to get info for repair replacement {}: {}, falling back to re-identification", torrent.id, e);
                                        to_identify.push((*torrent).clone());
                                    }
                                }
                            } else {
                                // Old ID not in seen_torrents (edge case), fall back to normal identification
                                info!("Repair replacement old_id {} not found in seen_torrents, re-identifying {}", old_id, torrent.id);
                                to_identify.push((*torrent).clone());
                            }
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
                                to_identify.push((*torrent).clone());
                            }
                        }
                    }
                }

                if !to_identify.is_empty() {
                    let new_total = to_identify.len();
                    info!("Identifying {} new torrents...", new_total);
                    let mut stream = futures_util::stream::iter(to_identify)
                        .map(|torrent| {
                            let rd_client = rd_client.clone();
                            let tmdb_client = tmdb_client.clone();
                            async move {
                                match rd_client.get_torrent_info(&torrent.id).await {
                                    Ok(info) => {
                                        let metadata = identify_torrent(&info, &tmdb_client).await;
                                        Ok::<
                                            (String, crate::rd_client::TorrentInfo, MediaMetadata),
                                            reqwest::Error,
                                        >((
                                            torrent.id, info, metadata,
                                        ))
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                        })
                        .buffer_unordered(1);

                    let mut processed_new = 0;
                    // Batch pending DB writes: (id, serialized_bytes)
                    let mut pending_db_writes: Vec<(String, Vec<u8>)> = Vec::new();

                    while let Some(result) = tokio::select! {
                        result = stream.next() => result,
                        _ = shutdown.changed() => {
                            info!("Scan task: shutdown during identification, saving progress");
                            // Flush pending writes before shutting down
                            if !pending_db_writes.is_empty() {
                                flush_db_writes(&db, &mut pending_db_writes).await;
                            }
                            update_vfs(&vfs, &current_data, &repair_manager, &jellyfin_client).await;
                            return;
                        }
                    } {
                        processed_new += 1;
                        match result {
                            Ok((id, info, metadata)) => {
                                if let Ok(data_bytes) = serde_json::to_vec(&(&info, &metadata)) {
                                    pending_db_writes.push((id.clone(), data_bytes));
                                }
                                seen_torrents.insert(id, (info.clone(), metadata.clone()));
                                current_data.push((info, metadata));
                            }
                            Err(e) => error!("Failed to identify torrent: {}", e),
                        }
                        if processed_new % 10 == 0 || processed_new == new_total {
                            // Flush batched DB writes at each progress checkpoint
                            if !pending_db_writes.is_empty() {
                                flush_db_writes(&db, &mut pending_db_writes).await;
                            }
                            info!(
                                "Progress: {}/{} new torrents identified",
                                processed_new, new_total
                            );
                            update_vfs(&vfs, &current_data, &repair_manager, &jellyfin_client)
                                .await;
                        }
                    }
                } else {
                    update_vfs(&vfs, &current_data, &repair_manager, &jellyfin_client).await;
                }

                let current_ids: std::collections::HashSet<&str> =
                    deduped_torrents.iter().map(|t| t.id.as_str()).collect();
                // Collect stale IDs before retain so we can clean up redb
                let stale_ids: Vec<String> = seen_torrents
                    .keys()
                    .filter(|id| !current_ids.contains(id.as_str()))
                    .cloned()
                    .collect();
                seen_torrents.retain(|id, _| current_ids.contains(id.as_str()));
                // Prune health_status entries for torrents that no longer exist
                repair_manager.prune_health_status(&current_ids).await;
                // Remove stale entries from redb to prevent them from reloading on restart
                if !stale_ids.is_empty() {
                    info!("Removing {} stale entries from database", stale_ids.len());
                    let db_clone = db.clone();
                    if let Err(e) =
                        tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
                            let write_txn = db_clone.begin_write()?;
                            {
                                let mut table = write_txn.open_table(MATCHES_TABLE)?;
                                for id in &stale_ids {
                                    table.remove(id.as_str())?;
                                }
                            }
                            write_txn.commit()?;
                            Ok(())
                        })
                        .await
                    {
                        error!("Failed to remove stale entries from database: {:?}", e);
                    }
                }
                info!("VFS update complete.");
            }
            Err(e) => error!("Failed to get torrents: {}", e),
        }

        info!("Scan task: sleeping {}s until next scan", interval_secs);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {}
            _ = shutdown.changed() => {
                info!("Scan task: shutdown requested, exiting");
                return;
            }
        }
    }
}

/// Flush a batch of pending DB writes in a single transaction.
/// Clears `pending_writes` on success or failure.
async fn flush_db_writes(db: &Arc<redb::Database>, pending_writes: &mut Vec<(String, Vec<u8>)>) {
    let writes = std::mem::take(pending_writes);
    let count = writes.len();
    let db_clone = db.clone();
    if let Err(e) = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
        let write_txn = db_clone.begin_write()?;
        {
            let mut table = write_txn.open_table(MATCHES_TABLE)?;
            for (id, data_bytes) in &writes {
                table.insert(id.as_str(), data_bytes.as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    })
    .await
    {
        error!(
            "Failed to persist {} torrent identifications to database: {:?}",
            count, e
        );
    }
}

async fn update_vfs(
    vfs: &Arc<RwLock<DebridVfs>>,
    current_data: &[(crate::rd_client::TorrentInfo, MediaMetadata)],
    repair_manager: &Arc<RepairManager>,
    jellyfin_client: &Option<Arc<crate::jellyfin_client::JellyfinClient>>,
) {
    let hidden_ids = repair_manager.hidden_torrent_ids().await;
    let filtered: Vec<_> = current_data
        .iter()
        .filter(|(torrent_info, _)| !hidden_ids.contains(&torrent_info.id))
        .map(|(torrent_info, metadata)| (torrent_info.clone(), metadata.clone()))
        .collect();
    // Build VFS without holding the lock to avoid blocking WebDAV reads during scans
    let new_vfs = DebridVfs::build(filtered);
    // Diff old vs new, then swap
    let mut vfs_lock = vfs.write().await;
    let changes = crate::vfs::diff_trees(&vfs_lock.root, &new_vfs.root, "");
    *vfs_lock = new_vfs;
    drop(vfs_lock);

    if !changes.is_empty() {
        if let Some(client) = jellyfin_client {
            let client = client.clone();
            tokio::spawn(async move {
                client.notify_changes(&changes).await;
            });
        }
    }
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
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let config = ScanConfig {
            rd_client,
            tmdb_client,
            vfs,
            db,
            repair_manager,
            interval_secs: 60,
            jellyfin_client: None,
        };
        run_scan_loop(config, shutdown).await;
    }
}
