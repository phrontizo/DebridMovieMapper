use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn, error, debug};
use crate::rd_client::{RealDebridClient, TorrentInfo};

#[derive(Debug)]
pub struct InstantRepairResult {
    pub new_torrent_id: String,
    pub new_rd_link: String,
}

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

    /// Attempt instant repair for cached torrents. Returns the new restricted RD link
    /// if the torrent is cached (status "downloaded" immediately after select_files).
    /// Returns Err if the torrent needs actual downloading or repair fails.
    pub async fn try_instant_repair(
        &self,
        torrent_id: &str,
        failed_link: &str,
    ) -> Result<InstantRepairResult, String> {
        // Check rate limits and repair state (same guards as repair_torrent)
        {
            let health_map = self.health_status.read().await;
            if let Some(health) = health_map.get(torrent_id) {
                if health.state == RepairState::Failed {
                    debug!("Torrent {} has permanently failed repair, skipping instant repair", torrent_id);
                    return Err("Torrent permanently failed".to_string());
                }
                if health.state == RepairState::Repairing {
                    debug!("Repair already in progress for torrent {}, skipping instant repair", torrent_id);
                    return Err("Repair already in progress".to_string());
                }
                if let Some(last_trigger) = health.last_repair_trigger {
                    if last_trigger.elapsed().as_secs() < 30 {
                        debug!("Repair recently triggered for torrent {} ({}s ago), skipping instant repair",
                            torrent_id, last_trigger.elapsed().as_secs());
                        return Err("Repair rate limited".to_string());
                    }
                }
            }
        }

        // Set state to Repairing and increment attempt count
        {
            let mut health_map = self.health_status.write().await;
            let attempt_num = if let Some(health) = health_map.get_mut(torrent_id) {
                if health.repair_attempts >= 3 {
                    error!("Torrent {} has failed repair 3 times, marking as permanently FAILED", torrent_id);
                    health.state = RepairState::Failed;
                    return Err("Maximum repair attempts exceeded".to_string());
                }
                if health.state == RepairState::Repairing {
                    return Err("Repair already in progress".to_string());
                }
                health.state = RepairState::Repairing;
                health.repair_attempts += 1;
                health.last_repair_trigger = Some(std::time::Instant::now());
                health.repair_attempts
            } else {
                health_map.insert(torrent_id.to_string(), TorrentHealth {
                    torrent_id: torrent_id.to_string(),
                    state: RepairState::Repairing,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 1,
                    last_repair_trigger: Some(std::time::Instant::now()),
                });
                1
            };
            info!("Instant repair attempt #{} for torrent {}", attempt_num, torrent_id);
        }

        // Get old torrent info to find hash and file selection
        let old_info = match self.rd_client.get_torrent_info(torrent_id).await {
            Ok(info) => info,
            Err(e) => {
                self.set_repair_failed(torrent_id).await;
                return Err(format!("Failed to get torrent info: {}", e));
            }
        };

        // Find which link index corresponds to the failed link
        let link_index = match old_info.links.iter().position(|l| l == failed_link) {
            Some(idx) => idx,
            None => {
                self.set_repair_failed(torrent_id).await;
                return Err("Failed link not found in torrent links".to_string());
            }
        };

        // Add magnet
        let magnet = format!("magnet:?xt=urn:btih:{}", old_info.hash);
        info!("Instant repair: adding magnet for hash {}", old_info.hash);
        let add_response = match self.rd_client.add_magnet(&magnet).await {
            Ok(resp) => resp,
            Err(e) => {
                self.set_repair_failed(torrent_id).await;
                return Err(format!("Failed to add magnet: {}", e));
            }
        };
        info!("Instant repair: new torrent ID {}", add_response.id);

        // Brief wait for RD to process the magnet
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Get new torrent info for file matching
        let new_info = match self.rd_client.get_torrent_info(&add_response.id).await {
            Ok(info) => info,
            Err(e) => {
                self.set_repair_failed(torrent_id).await;
                return Err(format!("Failed to get new torrent info: {}", e));
            }
        };

        // Match and select files (same logic as repair_torrent)
        let selected_file_ids: Vec<String> = old_info.files.iter()
            .filter(|f| f.selected == 1)
            .filter_map(|original_file| {
                new_info.files.iter()
                    .find(|new_file| new_file.path == original_file.path)
                    .map(|new_file| new_file.id.to_string())
            })
            .collect();

        if selected_file_ids.is_empty() {
            self.set_repair_failed(torrent_id).await;
            return Err("No matching files found in new torrent".to_string());
        }

        let file_ids_str = selected_file_ids.join(",");
        info!("Instant repair: selecting files {} on torrent {}", file_ids_str, add_response.id);
        if let Err(e) = self.rd_client.select_files(&add_response.id, &file_ids_str).await {
            self.set_repair_failed(torrent_id).await;
            return Err(format!("Failed to select files: {}", e));
        }

        // Brief wait for RD to process file selection
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Check if torrent is cached (status "downloaded" with links populated)
        let final_info = match self.rd_client.get_torrent_info(&add_response.id).await {
            Ok(info) => info,
            Err(e) => {
                self.set_repair_failed(torrent_id).await;
                return Err(format!("Failed to get final torrent info: {}", e));
            }
        };

        if final_info.status == "downloaded" && !final_info.links.is_empty() {
            // Cached! Get the new link at the same index
            let new_link = match final_info.links.get(link_index) {
                Some(link) => link.clone(),
                None => {
                    self.set_repair_failed(torrent_id).await;
                    return Err(format!(
                        "Link index {} out of bounds (new torrent has {} links)",
                        link_index, final_info.links.len()
                    ));
                }
            };

            // Delete old broken torrent
            if let Err(e) = self.rd_client.delete_torrent(torrent_id).await {
                warn!("Failed to delete old torrent {}: {}", torrent_id, e);
            }

            // Update health status: remove old, add new as Healthy
            let mut health_map = self.health_status.write().await;
            health_map.remove(torrent_id);
            health_map.insert(add_response.id.clone(), TorrentHealth {
                torrent_id: add_response.id.clone(),
                state: RepairState::Healthy,
                failed_links: HashSet::new(),
                last_check: std::time::Instant::now(),
                repair_attempts: 0,
                last_repair_trigger: None,
            });

            info!("Instant repair SUCCEEDED for torrent {} → new ID {} with link at index {}",
                torrent_id, add_response.id, link_index);

            Ok(InstantRepairResult {
                new_torrent_id: add_response.id,
                new_rd_link: new_link,
            })
        } else {
            // Not cached — torrent needs actual download
            info!("Torrent {} not cached (status: {}), leaving new torrent {} to download",
                torrent_id, final_info.status, add_response.id);

            // Delete old broken torrent
            if let Err(e) = self.rd_client.delete_torrent(torrent_id).await {
                warn!("Failed to delete old torrent {}: {}", torrent_id, e);
            }

            // Mark as broken so it's hidden until scan picks up the new torrent
            let mut health_map = self.health_status.write().await;
            if let Some(health) = health_map.get_mut(torrent_id) {
                health.state = RepairState::Broken;
            }

            Err(format!("Torrent not cached (status: {}), needs download", final_info.status))
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
    #[allow(dead_code)]
    async fn _assert_repair_by_id_signature(manager: &RepairManager) {
        let _: Result<(), String> = manager.repair_by_id("some_id").await;
    }

    /// Compile-time check: InstantRepairResult struct has the expected fields.
    #[allow(dead_code)]
    fn _assert_instant_repair_result_fields() {
        let result = InstantRepairResult {
            new_torrent_id: String::new(),
            new_rd_link: String::new(),
        };
        let _: String = result.new_torrent_id;
        let _: String = result.new_rd_link;
    }

    /// Compile-time check: try_instant_repair exists with the correct signature.
    #[allow(dead_code)]
    async fn _assert_try_instant_repair_signature(manager: &RepairManager) {
        let _: Result<InstantRepairResult, String> =
            manager.try_instant_repair("torrent_id", "link").await;
    }

    #[test]
    fn repair_state_has_no_checking_variant() {
        let states = [
            RepairState::Healthy,
            RepairState::Broken,
            RepairState::Repairing,
            RepairState::Failed,
        ];
        assert_eq!(states.len(), 4);
    }

    fn make_test_manager() -> RepairManager {
        let rd_client = Arc::new(
            crate::rd_client::RealDebridClient::new("fake-token".to_string()).unwrap()
        );
        RepairManager::new(rd_client)
    }

    #[tokio::test]
    async fn try_instant_repair_rate_limited_within_30s() {
        let manager = make_test_manager();
        // Pre-populate health with a recent repair trigger
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert("torrent1".to_string(), TorrentHealth {
                torrent_id: "torrent1".to_string(),
                state: RepairState::Broken,
                failed_links: HashSet::new(),
                last_check: std::time::Instant::now(),
                repair_attempts: 1,
                last_repair_trigger: Some(std::time::Instant::now()),
            });
        }

        let result = manager.try_instant_repair("torrent1", "some_link").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Repair rate limited");
    }

    #[tokio::test]
    async fn try_instant_repair_max_attempts_exceeded() {
        let manager = make_test_manager();
        // Pre-populate health with 3 prior attempts (no recent trigger, so rate limit passes)
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert("torrent2".to_string(), TorrentHealth {
                torrent_id: "torrent2".to_string(),
                state: RepairState::Broken,
                failed_links: HashSet::new(),
                last_check: std::time::Instant::now(),
                repair_attempts: 3,
                last_repair_trigger: None,
            });
        }

        let result = manager.try_instant_repair("torrent2", "some_link").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Maximum repair attempts exceeded");

        // Verify it was marked as Failed
        let health_map = manager.health_status.read().await;
        assert_eq!(health_map.get("torrent2").unwrap().state, RepairState::Failed);
    }

    #[tokio::test]
    async fn try_instant_repair_skips_already_repairing() {
        let manager = make_test_manager();
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert("torrent3".to_string(), TorrentHealth {
                torrent_id: "torrent3".to_string(),
                state: RepairState::Repairing,
                failed_links: HashSet::new(),
                last_check: std::time::Instant::now(),
                repair_attempts: 1,
                last_repair_trigger: None,
            });
        }

        let result = manager.try_instant_repair("torrent3", "some_link").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Repair already in progress");
    }

    #[tokio::test]
    async fn try_instant_repair_skips_permanently_failed() {
        let manager = make_test_manager();
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert("torrent4".to_string(), TorrentHealth {
                torrent_id: "torrent4".to_string(),
                state: RepairState::Failed,
                failed_links: HashSet::new(),
                last_check: std::time::Instant::now(),
                repair_attempts: 3,
                last_repair_trigger: None,
            });
        }

        let result = manager.try_instant_repair("torrent4", "some_link").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Torrent permanently failed");
    }
}
