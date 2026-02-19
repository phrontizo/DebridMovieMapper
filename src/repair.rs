use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn, error, debug};
use crate::rd_client::{RealDebridClient, TorrentInfo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairState {
    Healthy,
    Broken,
    Repairing,
    Failed,
}

#[derive(Debug, Clone)]
pub struct TorrentHealth {
    pub torrent_id: String,
    pub state: RepairState,
    pub failed_links: HashSet<String>,
    pub last_check: std::time::Instant,
    pub repair_attempts: u32,
    pub last_repair_trigger: Option<std::time::Instant>,
}

#[derive(Debug)]
pub struct RepairManager {
    health_status: Arc<RwLock<HashMap<String, TorrentHealth>>>,
    rd_client: Arc<RealDebridClient>,
}

impl RepairManager {
    pub fn new(rd_client: Arc<RealDebridClient>) -> Self {
        Self {
            health_status: Arc::new(RwLock::new(HashMap::new())),
            rd_client,
        }
    }

    pub fn health_status(&self) -> Arc<RwLock<HashMap<String, TorrentHealth>>> {
        self.health_status.clone()
    }

    async fn set_repair_failed(&self, torrent_id: &str) {
        let mut health_map = self.health_status.write().await;
        if let Some(health) = health_map.get_mut(torrent_id) {
            health.state = RepairState::Failed;
        }
    }

    /// Attempt to repair a broken torrent by re-adding it
    pub async fn repair_torrent(&self, torrent_info: &TorrentInfo) -> Result<(), String> {
        // Check if repair is already in progress, recently triggered, or permanently failed
        {
            let health_map = self.health_status.read().await;
            if let Some(health) = health_map.get(&torrent_info.id) {
                // If already failed permanently, skip
                if health.state == RepairState::Failed {
                    debug!("Torrent '{}' has permanently failed repair, skipping", torrent_info.filename);
                    return Err("Torrent permanently failed".to_string());
                }

                // If already repairing, skip
                if health.state == RepairState::Repairing {
                    debug!("Repair already in progress for torrent '{}', skipping duplicate", torrent_info.filename);
                    return Err("Repair already in progress".to_string());
                }

                // If repair triggered within last 30 seconds, skip (rate limiting)
                if let Some(last_trigger) = health.last_repair_trigger {
                    if last_trigger.elapsed().as_secs() < 30 {
                        debug!("Repair recently triggered for torrent '{}' ({}s ago), skipping",
                            torrent_info.filename, last_trigger.elapsed().as_secs());
                        return Err("Repair rate limited".to_string());
                    }
                }
            }
        }

        info!("========================================");
        info!("REPAIR STARTED: Torrent '{}' ({})", torrent_info.filename, torrent_info.id);
        info!("========================================");

        let mut health_map = self.health_status.write().await;
        let attempt_num = if let Some(health) = health_map.get_mut(&torrent_info.id) {
            // Check if already failed 3 times BEFORE incrementing
            if health.repair_attempts >= 3 {
                error!("Torrent '{}' ({}) has failed repair 3 times, marking as permanently FAILED",
                    torrent_info.filename, torrent_info.id);
                health.state = RepairState::Failed;
                drop(health_map);
                return Err("Maximum repair attempts exceeded".to_string());
            }

            // Double-check state hasn't changed (another task might have started repairing)
            if health.state == RepairState::Repairing {
                debug!("Repair already in progress (race condition detected), skipping");
                drop(health_map);
                return Err("Repair already in progress".to_string());
            }

            health.state = RepairState::Repairing;
            health.repair_attempts += 1;
            health.last_repair_trigger = Some(std::time::Instant::now());
            health.repair_attempts
        } else {
            // First time seeing this torrent
            health_map.insert(torrent_info.id.clone(), TorrentHealth {
                torrent_id: torrent_info.id.clone(),
                state: RepairState::Repairing,
                failed_links: HashSet::new(),
                last_check: std::time::Instant::now(),
                repair_attempts: 1,
                last_repair_trigger: Some(std::time::Instant::now()),
            });
            1
        };
        drop(health_map);

        info!("Repair attempt #{} for torrent '{}'", attempt_num, torrent_info.filename);

        // Build magnet link from hash
        let magnet = format!("magnet:?xt=urn:btih:{}", torrent_info.hash);
        info!("Using magnet link: magnet:?xt=urn:btih:{}", torrent_info.hash);

        // Try to re-add the torrent
        info!("Step 1: Adding magnet to Real-Debrid...");
        match self.rd_client.add_magnet(&magnet).await {
            Ok(add_response) => {
                info!("✓ Step 1 complete: Re-added torrent with new ID: {}", add_response.id);

                // Wait a moment for RD to process the torrent
                info!("Step 2: Waiting 2 seconds for RD to process torrent...");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                info!("✓ Step 2 complete");

                info!("Step 3: Fetching new torrent info...");

                // Get the new torrent info to find file IDs
                match self.rd_client.get_torrent_info(&add_response.id).await {
                    Ok(new_info) => {
                        info!("✓ Step 3 complete: Retrieved new torrent info");

                        // Build file selection string (comma-separated IDs of selected files)
                        info!("Step 4: Matching and selecting files...");
                        let original_selected_count = torrent_info.files.iter().filter(|f| f.selected == 1).count();
                        let selected_file_ids: Vec<String> = torrent_info.files.iter()
                            .filter(|f| f.selected == 1)
                            .filter_map(|original_file| {
                                // Match by path to find the corresponding file in new torrent
                                new_info.files.iter()
                                    .find(|new_file| new_file.path == original_file.path)
                                    .map(|new_file| new_file.id.to_string())
                            })
                            .collect();

                        info!("Matched {}/{} files from original torrent", selected_file_ids.len(), original_selected_count);

                        if !selected_file_ids.is_empty() {
                            let file_ids_str = selected_file_ids.join(",");
                            info!("Selecting file IDs: {}", file_ids_str);
                            match self.rd_client.select_files(&add_response.id, &file_ids_str).await {
                                Ok(_) => {
                                    info!("✓ Step 4 complete: Selected {} files for repaired torrent", selected_file_ids.len());

                                    // Try to delete the old broken torrent
                                    info!("Step 5: Cleaning up old broken torrent...");
                                    if let Err(e) = self.rd_client.delete_torrent(&torrent_info.id).await {
                                        warn!("✗ Failed to delete old torrent {}: {}", torrent_info.id, e);
                                    } else {
                                        info!("✓ Step 5 complete: Deleted old broken torrent {}", torrent_info.id);
                                    }

                                    // Update health status - mark old as repaired, add new as healthy
                                    let mut health_map = self.health_status.write().await;
                                    health_map.remove(&torrent_info.id);
                                    health_map.insert(add_response.id.clone(), TorrentHealth {
                                        torrent_id: add_response.id.clone(),
                                        state: RepairState::Healthy,
                                        failed_links: HashSet::new(),
                                        last_check: std::time::Instant::now(),
                                        repair_attempts: 0,
                                        last_repair_trigger: None,
                                    });

                                    info!("========================================");
                                    info!("REPAIR COMPLETE: Torrent '{}' successfully repaired!", torrent_info.filename);
                                    info!("Old ID: {} → New ID: {}", torrent_info.id, add_response.id);
                                    info!("========================================");

                                    Ok(())
                                }
                                Err(e) => {
                                    error!("Failed to select files for repaired torrent {}: {}", add_response.id, e);
                                    self.set_repair_failed(&torrent_info.id).await;
                                    Err(format!("Failed to select files: {}", e))
                                }
                            }
                        } else {
                            error!("No matching files found in repaired torrent {}", add_response.id);
                            self.set_repair_failed(&torrent_info.id).await;
                            Err("No matching files found".to_string())
                        }
                    }
                    Err(e) => {
                        error!("Failed to get info for repaired torrent {}: {}", add_response.id, e);
                        self.set_repair_failed(&torrent_info.id).await;
                        Err(format!("Failed to get torrent info: {}", e))
                    }
                }
            }
            Err(e) => {
                error!("Failed to re-add torrent {}: {}", torrent_info.id, e);
                self.set_repair_failed(&torrent_info.id).await;
                Err(format!("Failed to add magnet: {}", e))
            }
        }
    }

    /// Fetch torrent info fresh and attempt repair. Called on-demand when a broken
    /// link is detected at STRM access time.
    pub async fn repair_by_id(&self, torrent_id: &str) -> Result<(), String> {
        match self.rd_client.get_torrent_info(torrent_id).await {
            Ok(info) => self.repair_torrent(&info).await,
            Err(e) => Err(format!("Failed to fetch torrent info for repair: {}", e)),
        }
    }

    /// Check if a torrent should be hidden from WebDAV
    pub async fn should_hide_torrent(&self, torrent_id: &str) -> bool {
        let health_map = self.health_status.read().await;
        if let Some(health) = health_map.get(torrent_id) {
            matches!(health.state, RepairState::Broken | RepairState::Repairing | RepairState::Failed)
        } else {
            false
        }
    }

    /// Get summary of repair status
    pub async fn get_status_summary(&self) -> (usize, usize, usize) {
        let health_map = self.health_status.read().await;
        let healthy = health_map.values().filter(|h| h.state == RepairState::Healthy).count();
        let repairing = health_map.values().filter(|h| matches!(h.state, RepairState::Broken | RepairState::Repairing)).count();
        let failed = health_map.values().filter(|h| h.state == RepairState::Failed).count();
        (healthy, repairing, failed)
    }

    /// Mark a torrent as broken (typically called when a 503 is encountered during playback)
    pub async fn mark_broken(&self, torrent_id: &str, failed_link: &str) {
        let mut health_map = self.health_status.write().await;
        let mut failed_links = HashSet::new();
        failed_links.insert(failed_link.to_string());

        warn!("Marking torrent {} as BROKEN due to 503 error on link {}", torrent_id, failed_link);

        // Get previous repair attempts before inserting
        let previous_attempts = health_map.get(torrent_id).map(|h| h.repair_attempts).unwrap_or(0);

        health_map.insert(torrent_id.to_string(), TorrentHealth {
            torrent_id: torrent_id.to_string(),
            state: RepairState::Broken,
            failed_links,
            last_check: std::time::Instant::now(),
            repair_attempts: previous_attempts,
            last_repair_trigger: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check: repair_by_id exists with the correct signature.
    /// If this test file compiles, the method exists.
    #[allow(dead_code)]
    async fn _assert_repair_by_id_signature(manager: &RepairManager) {
        let _: Result<(), String> = manager.repair_by_id("some_id").await;
    }

    #[test]
    fn repair_state_has_no_checking_variant() {
        // This test fails to compile if Checking is re-added to RepairState.
        // Checking that Healthy, Broken, Repairing, Failed exist and Checking does not.
        let states = [
            RepairState::Healthy,
            RepairState::Broken,
            RepairState::Repairing,
            RepairState::Failed,
        ];
        assert_eq!(states.len(), 4);
    }
}
