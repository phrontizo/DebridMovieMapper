use crate::rd_client::{RealDebridClient, TorrentInfo};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

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
    /// Maps new_torrent_id -> old_torrent_id for successful repairs.
    /// The scan loop consumes this to reuse old TMDB identifications.
    repair_replacements: Arc<RwLock<HashMap<String, String>>>,
}

impl RepairManager {
    pub fn new(rd_client: Arc<RealDebridClient>) -> Self {
        Self {
            health_status: Arc::new(RwLock::new(HashMap::new())),
            rd_client,
            repair_replacements: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Drains and returns the repair replacements map (new_id -> old_id).
    /// After calling this, the internal map is empty.
    pub async fn take_repair_replacements(&self) -> HashMap<String, String> {
        let mut map = self.repair_replacements.write().await;
        std::mem::take(&mut *map)
    }

    /// Delete a torrent that was created by add_magnet but whose repair failed.
    /// Prevents duplicate torrents from accumulating in Real-Debrid.
    async fn cleanup_leaked_torrent(&self, new_torrent_id: &str) {
        warn!(
            "Cleaning up leaked torrent {} after failed repair",
            new_torrent_id
        );
        if let Err(e) = self.rd_client.delete_torrent(new_torrent_id).await {
            error!(
                "Failed to clean up leaked torrent {}: {}",
                new_torrent_id, e
            );
        }
    }

    async fn set_repair_failed(&self, torrent_id: &str) {
        let mut health_map = self.health_status.write().await;
        if let Some(health) = health_map.get_mut(torrent_id) {
            health.state = RepairState::Failed;
        }
    }

    /// Check pre-repair guards (Failed/Repairing/rate-limited) and transition to Repairing state.
    /// Returns the attempt number on success, or Err with the reason why repair cannot proceed.
    async fn check_and_begin_repair(&self, torrent_id: &str) -> Result<u32, String> {
        // Read-side guard: check state without holding write lock
        {
            let health_map = self.health_status.read().await;
            if let Some(health) = health_map.get(torrent_id) {
                if health.state == RepairState::Failed {
                    debug!(
                        "Torrent {} has permanently failed repair, skipping",
                        torrent_id
                    );
                    return Err("Torrent permanently failed".to_string());
                }
                if health.state == RepairState::Repairing {
                    debug!(
                        "Repair already in progress for torrent {}, skipping",
                        torrent_id
                    );
                    return Err("Repair already in progress".to_string());
                }
                if let Some(last_trigger) = health.last_repair_trigger {
                    if last_trigger.elapsed().as_secs() < 30 {
                        debug!(
                            "Repair recently triggered for torrent {} ({}s ago), skipping",
                            torrent_id,
                            last_trigger.elapsed().as_secs()
                        );
                        return Err("Repair rate limited".to_string());
                    }
                }
            }
        }

        // Write-side: set state to Repairing and increment attempt count
        let mut health_map = self.health_status.write().await;
        let attempt_num = if let Some(health) = health_map.get_mut(torrent_id) {
            if health.repair_attempts >= 3 {
                error!(
                    "Torrent {} has failed repair 3 times, marking as permanently FAILED",
                    torrent_id
                );
                health.state = RepairState::Failed;
                return Err("Maximum repair attempts exceeded".to_string());
            }
            // Double-check all guards: another task might have changed state or
            // started a repair between the read and write lock acquisitions.
            if health.state == RepairState::Failed {
                return Err("Torrent permanently failed".to_string());
            }
            if health.state == RepairState::Repairing {
                return Err("Repair already in progress".to_string());
            }
            if let Some(last_trigger) = health.last_repair_trigger {
                if last_trigger.elapsed().as_secs() < 30 {
                    return Err("Repair rate limited".to_string());
                }
            }
            health.state = RepairState::Repairing;
            health.repair_attempts += 1;
            health.last_repair_trigger = Some(std::time::Instant::now());
            health.repair_attempts
        } else {
            health_map.insert(
                torrent_id.to_string(),
                TorrentHealth {
                    torrent_id: torrent_id.to_string(),
                    state: RepairState::Repairing,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 1,
                    last_repair_trigger: Some(std::time::Instant::now()),
                },
            );
            1
        };

        Ok(attempt_num)
    }

    /// Add magnet, wait for RD to process, get new torrent info, match files by path, select them.
    /// Returns (new_torrent_id, new_torrent_info) on success.
    /// On failure, cleans up the leaked torrent and marks the old torrent as failed.
    async fn add_and_select_files(
        &self,
        old_torrent_id: &str,
        old_info: &TorrentInfo,
        wait_duration: Duration,
    ) -> Result<(String, TorrentInfo), String> {
        let magnet = format!("magnet:?xt=urn:btih:{}", old_info.hash);

        // Add magnet
        let add_response = match self.rd_client.add_magnet(&magnet).await {
            Ok(resp) => resp,
            Err(e) => {
                self.set_repair_failed(old_torrent_id).await;
                return Err(format!("Failed to add magnet: {}", e));
            }
        };
        let new_torrent_id = add_response.id;

        // Wait for RD to process the magnet
        tokio::time::sleep(wait_duration).await;

        // Get new torrent info for file matching
        let new_info = match self.rd_client.get_torrent_info(&new_torrent_id).await {
            Ok(info) => info,
            Err(e) => {
                self.cleanup_leaked_torrent(&new_torrent_id).await;
                self.set_repair_failed(old_torrent_id).await;
                return Err(format!("Failed to get new torrent info: {}", e));
            }
        };

        // Match files by path: find new file IDs that correspond to old selected files
        let selected_file_ids: Vec<String> = old_info
            .files
            .iter()
            .filter(|f| f.selected == 1)
            .filter_map(|original_file| {
                new_info
                    .files
                    .iter()
                    .find(|new_file| new_file.path == original_file.path)
                    .map(|new_file| new_file.id.to_string())
            })
            .collect();

        if selected_file_ids.is_empty() {
            self.cleanup_leaked_torrent(&new_torrent_id).await;
            self.set_repair_failed(old_torrent_id).await;
            return Err("No matching files found in new torrent".to_string());
        }

        let file_ids_str = selected_file_ids.join(",");
        if let Err(e) = self
            .rd_client
            .select_files(&new_torrent_id, &file_ids_str)
            .await
        {
            self.cleanup_leaked_torrent(&new_torrent_id).await;
            self.set_repair_failed(old_torrent_id).await;
            return Err(format!("Failed to select files: {}", e));
        }

        Ok((new_torrent_id, new_info))
    }

    /// Delete old torrent, update health_map (remove old, insert new as Healthy), record replacement.
    async fn complete_repair(&self, old_torrent_id: &str, new_torrent_id: &str) {
        // Delete old broken torrent
        if let Err(e) = self.rd_client.delete_torrent(old_torrent_id).await {
            warn!("Failed to delete old torrent {}: {}", old_torrent_id, e);
        }

        // Update health status: remove old, add new as Healthy
        let mut health_map = self.health_status.write().await;
        health_map.remove(old_torrent_id);
        health_map.insert(
            new_torrent_id.to_string(),
            TorrentHealth {
                torrent_id: new_torrent_id.to_string(),
                state: RepairState::Healthy,
                failed_links: HashSet::new(),
                last_check: std::time::Instant::now(),
                repair_attempts: 0,
                last_repair_trigger: None,
            },
        );
        drop(health_map);

        // Record replacement so scan loop reuses old identification
        self.repair_replacements
            .write()
            .await
            .insert(new_torrent_id.to_string(), old_torrent_id.to_string());
    }

    /// Attempt to repair a broken torrent by re-adding it
    pub async fn repair_torrent(&self, torrent_info: &TorrentInfo) -> Result<(), String> {
        let attempt_num = self.check_and_begin_repair(&torrent_info.id).await?;

        info!("========================================");
        info!(
            "REPAIR STARTED: Torrent '{}' ({})",
            torrent_info.filename, torrent_info.id
        );
        info!("========================================");
        info!(
            "Repair attempt #{} for torrent '{}'",
            attempt_num, torrent_info.filename
        );

        info!(
            "Using magnet link: magnet:?xt=urn:btih:{}",
            torrent_info.hash
        );

        info!("Step 1: Adding magnet to Real-Debrid...");
        let (new_torrent_id, _new_info) = match self
            .add_and_select_files(&torrent_info.id, torrent_info, Duration::from_secs(2))
            .await
        {
            Ok((new_id, new_info)) => {
                info!("Step 1 complete: Re-added torrent with new ID: {}", new_id);
                info!("Step 2: Waiting 2 seconds for RD to process torrent... complete");
                info!("Step 3: Fetching new torrent info... complete");
                let original_selected_count = torrent_info
                    .files
                    .iter()
                    .filter(|f| f.selected == 1)
                    .count();
                let matched_count = torrent_info
                    .files
                    .iter()
                    .filter(|f| f.selected == 1)
                    .filter(|original_file| {
                        new_info
                            .files
                            .iter()
                            .any(|new_file| new_file.path == original_file.path)
                    })
                    .count();
                info!("Step 4: Matching and selecting files...");
                info!(
                    "Matched {}/{} files from original torrent",
                    matched_count, original_selected_count
                );
                info!(
                    "Step 4 complete: Selected {} files for repaired torrent",
                    matched_count
                );
                (new_id, new_info)
            }
            Err(e) => {
                return Err(e);
            }
        };

        info!("Step 5: Cleaning up old broken torrent...");
        self.complete_repair(&torrent_info.id, &new_torrent_id)
            .await;
        info!(
            "Step 5 complete: Deleted old broken torrent {}",
            torrent_info.id
        );

        info!("========================================");
        info!(
            "REPAIR COMPLETE: Torrent '{}' successfully repaired!",
            torrent_info.filename
        );
        info!("Old ID: {} -> New ID: {}", torrent_info.id, new_torrent_id);
        info!("========================================");

        Ok(())
    }

    /// Fetch torrent info fresh and attempt repair. Called on-demand when a broken
    /// link is detected during WebDAV file read.
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
        let attempt_num = self.check_and_begin_repair(torrent_id).await?;
        info!(
            "Instant repair attempt #{} for torrent {}",
            attempt_num, torrent_id
        );

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

        info!("Instant repair: adding magnet for hash {}", old_info.hash);
        let (new_torrent_id, _new_info) = self
            .add_and_select_files(torrent_id, &old_info, Duration::from_millis(500))
            .await?;
        info!("Instant repair: new torrent ID {}", new_torrent_id);

        // Brief wait for RD to process file selection
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Check if torrent is cached (status "downloaded" with links populated)
        let final_info = match self.rd_client.get_torrent_info(&new_torrent_id).await {
            Ok(info) => info,
            Err(e) => {
                self.cleanup_leaked_torrent(&new_torrent_id).await;
                self.set_repair_failed(torrent_id).await;
                return Err(format!("Failed to get final torrent info: {}", e));
            }
        };

        if final_info.status == "downloaded" && !final_info.links.is_empty() {
            // Validate link count matches before using positional index.
            // If RD reordered or changed file selection, the index would point
            // to a different file. Fail-safe: abort if counts diverge.
            if final_info.links.len() != old_info.links.len() {
                warn!(
                    "Instant repair: link count mismatch for torrent {} (old: {}, new: {}). \
                     Aborting to avoid serving wrong file.",
                    torrent_id,
                    old_info.links.len(),
                    final_info.links.len()
                );
                self.cleanup_leaked_torrent(&new_torrent_id).await;
                self.set_repair_failed(torrent_id).await;
                return Err(format!(
                    "Link count mismatch: old torrent had {} links, new has {}",
                    old_info.links.len(),
                    final_info.links.len()
                ));
            }

            // Cached! Get the new link at the same index
            let new_link = match final_info.links.get(link_index) {
                Some(link) => link.clone(),
                None => {
                    self.cleanup_leaked_torrent(&new_torrent_id).await;
                    self.set_repair_failed(torrent_id).await;
                    return Err(format!(
                        "Link index {} out of bounds (new torrent has {} links)",
                        link_index,
                        final_info.links.len()
                    ));
                }
            };

            self.complete_repair(torrent_id, &new_torrent_id).await;

            info!(
                "Instant repair SUCCEEDED for torrent {} -> new ID {} with link at index {}",
                torrent_id, new_torrent_id, link_index
            );

            Ok(InstantRepairResult {
                new_torrent_id,
                new_rd_link: new_link,
            })
        } else {
            // Not cached -- torrent needs actual download
            info!(
                "Torrent {} not cached (status: {}), leaving new torrent {} to download",
                torrent_id, final_info.status, new_torrent_id
            );

            // Delete old broken torrent
            if let Err(e) = self.rd_client.delete_torrent(torrent_id).await {
                warn!("Failed to delete old torrent {}: {}", torrent_id, e);
            }

            // Record replacement so scan loop reuses old identification
            self.repair_replacements
                .write()
                .await
                .insert(new_torrent_id.to_string(), torrent_id.to_string());

            // Mark as broken so it's hidden until scan picks up the new torrent
            let mut health_map = self.health_status.write().await;
            if let Some(health) = health_map.get_mut(torrent_id) {
                health.state = RepairState::Broken;
            }

            Err(format!(
                "Torrent not cached (status: {}), needs download",
                final_info.status
            ))
        }
    }

    /// Check if a torrent should be hidden from WebDAV
    pub async fn should_hide_torrent(&self, torrent_id: &str) -> bool {
        let health_map = self.health_status.read().await;
        if let Some(health) = health_map.get(torrent_id) {
            matches!(
                health.state,
                RepairState::Broken | RepairState::Repairing | RepairState::Failed
            )
        } else {
            false
        }
    }

    /// Return the set of torrent IDs that should be hidden from WebDAV.
    /// This acquires the read lock once instead of per-torrent, which is
    /// significantly faster when filtering hundreds of torrents during VFS updates.
    pub async fn hidden_torrent_ids(&self) -> std::collections::HashSet<String> {
        let health_map = self.health_status.read().await;
        health_map
            .values()
            .filter(|h| {
                matches!(
                    h.state,
                    RepairState::Broken | RepairState::Repairing | RepairState::Failed
                )
            })
            .map(|h| h.torrent_id.clone())
            .collect()
    }

    /// Get summary of repair status
    pub async fn get_status_summary(&self) -> (usize, usize, usize) {
        let health_map = self.health_status.read().await;
        let healthy = health_map
            .values()
            .filter(|h| h.state == RepairState::Healthy)
            .count();
        let repairing = health_map
            .values()
            .filter(|h| matches!(h.state, RepairState::Broken | RepairState::Repairing))
            .count();
        let failed = health_map
            .values()
            .filter(|h| h.state == RepairState::Failed)
            .count();
        (healthy, repairing, failed)
    }

    /// Remove health_status entries for torrent IDs that are no longer active.
    /// This prevents unbounded growth of the health_status map over time.
    pub async fn prune_health_status(&self, active_torrent_ids: &std::collections::HashSet<&str>) {
        let mut health_map = self.health_status.write().await;
        let before = health_map.len();
        health_map.retain(|id, _| active_torrent_ids.contains(id.as_str()));
        let pruned = before - health_map.len();
        if pruned > 0 {
            info!("Pruned {} stale entries from repair health_status", pruned);
        }
    }

    /// Mark a torrent as broken (typically called when a 503 is encountered during playback)
    pub async fn mark_broken(&self, torrent_id: &str, failed_link: &str) {
        let mut health_map = self.health_status.write().await;
        let mut failed_links = HashSet::new();
        failed_links.insert(failed_link.to_string());

        warn!(
            "Marking torrent {} as BROKEN due to 503 error on link {}",
            torrent_id, failed_link
        );

        // Preserve previous repair attempts and trigger time to prevent rapid repair loops.
        // If mark_broken cleared last_repair_trigger, a torrent that breaks immediately after
        // repair would bypass the 30-second cooldown and enter a rapid repair cycle.
        let (previous_attempts, previous_trigger) = health_map
            .get(torrent_id)
            .map(|h| (h.repair_attempts, h.last_repair_trigger))
            .unwrap_or((0, None));

        health_map.insert(
            torrent_id.to_string(),
            TorrentHealth {
                torrent_id: torrent_id.to_string(),
                state: RepairState::Broken,
                failed_links,
                last_check: std::time::Instant::now(),
                repair_attempts: previous_attempts,
                last_repair_trigger: previous_trigger,
            },
        );
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

    #[tokio::test]
    async fn should_hide_torrent_for_each_state() {
        let manager = make_test_manager();

        // Healthy: should NOT hide
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "healthy".to_string(),
                TorrentHealth {
                    torrent_id: "healthy".to_string(),
                    state: RepairState::Healthy,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
        }
        assert!(
            !manager.should_hide_torrent("healthy").await,
            "Healthy torrent should not be hidden"
        );

        // Broken: should hide
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "broken".to_string(),
                TorrentHealth {
                    torrent_id: "broken".to_string(),
                    state: RepairState::Broken,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
        }
        assert!(
            manager.should_hide_torrent("broken").await,
            "Broken torrent should be hidden"
        );

        // Repairing: should hide
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "repairing".to_string(),
                TorrentHealth {
                    torrent_id: "repairing".to_string(),
                    state: RepairState::Repairing,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
        }
        assert!(
            manager.should_hide_torrent("repairing").await,
            "Repairing torrent should be hidden"
        );

        // Failed: should hide
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "failed".to_string(),
                TorrentHealth {
                    torrent_id: "failed".to_string(),
                    state: RepairState::Failed,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }
        assert!(
            manager.should_hide_torrent("failed").await,
            "Failed torrent should be hidden"
        );

        // Unknown torrent: should NOT hide
        assert!(
            !manager.should_hide_torrent("unknown").await,
            "Unknown torrent should not be hidden"
        );
    }

    fn make_test_manager() -> RepairManager {
        let rd_client =
            Arc::new(crate::rd_client::RealDebridClient::new("fake-token".to_string()).unwrap());
        RepairManager::new(rd_client)
    }

    #[tokio::test]
    async fn try_instant_repair_rate_limited_within_30s() {
        let manager = make_test_manager();
        // Pre-populate health with a recent repair trigger
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "torrent1".to_string(),
                TorrentHealth {
                    torrent_id: "torrent1".to_string(),
                    state: RepairState::Broken,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 1,
                    last_repair_trigger: Some(std::time::Instant::now()),
                },
            );
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
            health_map.insert(
                "torrent2".to_string(),
                TorrentHealth {
                    torrent_id: "torrent2".to_string(),
                    state: RepairState::Broken,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let result = manager.try_instant_repair("torrent2", "some_link").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Maximum repair attempts exceeded");

        // Verify it was marked as Failed
        let health_map = manager.health_status.read().await;
        assert_eq!(
            health_map.get("torrent2").unwrap().state,
            RepairState::Failed
        );
    }

    #[tokio::test]
    async fn try_instant_repair_skips_already_repairing() {
        let manager = make_test_manager();
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "torrent3".to_string(),
                TorrentHealth {
                    torrent_id: "torrent3".to_string(),
                    state: RepairState::Repairing,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
        }

        let result = manager.try_instant_repair("torrent3", "some_link").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Repair already in progress");
    }

    #[tokio::test]
    async fn repair_replacements_records_and_returns() {
        let manager = make_test_manager();

        // Manually insert replacements (simulating successful repairs)
        {
            let mut map = manager.repair_replacements.write().await;
            map.insert("new_id_1".to_string(), "old_id_1".to_string());
            map.insert("new_id_2".to_string(), "old_id_2".to_string());
        }

        let replacements = manager.take_repair_replacements().await;
        assert_eq!(replacements.len(), 2);
        assert_eq!(replacements.get("new_id_1").unwrap(), "old_id_1");
        assert_eq!(replacements.get("new_id_2").unwrap(), "old_id_2");
    }

    #[tokio::test]
    async fn take_repair_replacements_drains_map() {
        let manager = make_test_manager();

        {
            let mut map = manager.repair_replacements.write().await;
            map.insert("new_id".to_string(), "old_id".to_string());
        }

        let first_call = manager.take_repair_replacements().await;
        assert_eq!(first_call.len(), 1);

        let second_call = manager.take_repair_replacements().await;
        assert!(second_call.is_empty());
    }

    #[test]
    fn check_and_begin_repair_write_side_rechecks_all_guards() {
        // The check_and_begin_repair method has a TOCTOU window between its read
        // lock (fast rejection) and write lock (state transition). Between the two,
        // another task could mark the torrent as Failed, start repairing, or set
        // last_repair_trigger. The write-side MUST re-check ALL guards before
        // proceeding.
        let source = include_str!("repair.rs");
        let fn_start = source
            .find("async fn check_and_begin_repair")
            .expect("function must exist");
        let fn_body = &source[fn_start..];
        // Find the write-side section (after "Write-side")
        let write_side_start = fn_body
            .find("Write-side")
            .expect("must have Write-side comment");
        let write_side = &fn_body[write_side_start..];
        // The write-side must check for Failed state before setting Repairing
        assert!(
            write_side.contains("RepairState::Failed"),
            "check_and_begin_repair write-side must re-check for Failed state to prevent \
             TOCTOU race where another task marks the torrent as failed between the read \
             and write lock acquisitions"
        );
        // The write-side must check for Repairing state
        assert!(
            write_side.contains("RepairState::Repairing"),
            "check_and_begin_repair write-side must re-check for Repairing state"
        );
        // The write-side must re-check rate limiting (last_repair_trigger)
        assert!(
            write_side.contains("last_repair_trigger"),
            "check_and_begin_repair write-side must re-check last_repair_trigger to prevent \
             TOCTOU race where another task completes a repair between the read and write \
             lock acquisitions, setting last_repair_trigger"
        );
    }

    #[tokio::test]
    async fn concurrent_check_and_begin_repair_only_one_succeeds() {
        // Verify that when multiple tasks call check_and_begin_repair concurrently
        // for the same torrent, only one succeeds and the others are rejected.
        let manager = Arc::new(make_test_manager());

        // Spawn 10 concurrent repair attempts
        let mut handles = Vec::new();
        for _ in 0..10 {
            let mgr = manager.clone();
            handles.push(tokio::spawn(async move {
                mgr.check_and_begin_repair("concurrent_torrent").await
            }));
        }

        let mut successes = 0;
        let mut failures = 0;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(_) => successes += 1,
                Err(_) => failures += 1,
            }
        }

        // Exactly one task should succeed; all others should be rejected
        assert_eq!(successes, 1, "Exactly one concurrent repair should succeed");
        assert_eq!(
            failures, 9,
            "All other concurrent repairs should be rejected"
        );

        // Verify the torrent is in Repairing state
        let health_map = manager.health_status.read().await;
        let health = health_map.get("concurrent_torrent").unwrap();
        assert_eq!(health.state, RepairState::Repairing);
        assert_eq!(health.repair_attempts, 1);
    }

    #[tokio::test]
    async fn check_and_begin_repair_write_side_rate_limits() {
        // Verify the write-side rate limit re-check works: if another task
        // completes repair (setting last_repair_trigger) between our read
        // and write locks, we should be rejected.
        let manager = make_test_manager();

        // First repair succeeds
        let result = manager.check_and_begin_repair("rate_test").await;
        assert!(result.is_ok());

        // Simulate repair completing (back to Broken, but last_repair_trigger is recent)
        {
            let mut health_map = manager.health_status.write().await;
            let health = health_map.get_mut("rate_test").unwrap();
            health.state = RepairState::Broken;
            // last_repair_trigger was set by check_and_begin_repair, leave it as-is
        }

        // Second attempt should be rate-limited (within 30s)
        let result = manager.check_and_begin_repair("rate_test").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Repair rate limited");
    }

    #[tokio::test]
    async fn try_instant_repair_skips_permanently_failed() {
        let manager = make_test_manager();
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "torrent4".to_string(),
                TorrentHealth {
                    torrent_id: "torrent4".to_string(),
                    state: RepairState::Failed,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let result = manager.try_instant_repair("torrent4", "some_link").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Torrent permanently failed");
    }

    #[tokio::test]
    async fn mark_broken_preserves_repair_attempts() {
        let manager = make_test_manager();

        // First, set up a torrent with 2 prior repair attempts
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "torrent_preserve".to_string(),
                TorrentHealth {
                    torrent_id: "torrent_preserve".to_string(),
                    state: RepairState::Repairing,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 2,
                    last_repair_trigger: Some(std::time::Instant::now()),
                },
            );
        }

        // Mark it as broken again
        manager
            .mark_broken("torrent_preserve", "http://failed_link")
            .await;

        // Verify repair_attempts is preserved
        let health_map = manager.health_status.read().await;
        let health = health_map.get("torrent_preserve").unwrap();
        assert_eq!(health.state, RepairState::Broken);
        assert_eq!(
            health.repair_attempts, 2,
            "mark_broken must preserve previous repair_attempts count"
        );
        assert!(health.failed_links.contains("http://failed_link"));
        // last_repair_trigger should be preserved to prevent rapid repair loops
        assert!(
            health.last_repair_trigger.is_some(),
            "mark_broken must preserve last_repair_trigger to prevent rapid repair loops"
        );
    }

    #[tokio::test]
    async fn get_status_summary_counts_correctly() {
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "h1".to_string(),
                TorrentHealth {
                    torrent_id: "h1".to_string(),
                    state: RepairState::Healthy,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "b1".to_string(),
                TorrentHealth {
                    torrent_id: "b1".to_string(),
                    state: RepairState::Broken,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "r1".to_string(),
                TorrentHealth {
                    torrent_id: "r1".to_string(),
                    state: RepairState::Repairing,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "f1".to_string(),
                TorrentHealth {
                    torrent_id: "f1".to_string(),
                    state: RepairState::Failed,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let (healthy, repairing, failed) = manager.get_status_summary().await;
        assert_eq!(healthy, 1, "Should have 1 healthy torrent");
        // get_status_summary counts Broken + Repairing together as "repairing"
        assert_eq!(repairing, 2, "Should have 2 repairing (broken + repairing)");
        assert_eq!(failed, 1, "Should have 1 failed torrent");
    }

    #[tokio::test]
    async fn hidden_torrent_ids_returns_non_healthy() {
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "healthy1".to_string(),
                TorrentHealth {
                    torrent_id: "healthy1".to_string(),
                    state: RepairState::Healthy,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "broken1".to_string(),
                TorrentHealth {
                    torrent_id: "broken1".to_string(),
                    state: RepairState::Broken,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "repairing1".to_string(),
                TorrentHealth {
                    torrent_id: "repairing1".to_string(),
                    state: RepairState::Repairing,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "failed1".to_string(),
                TorrentHealth {
                    torrent_id: "failed1".to_string(),
                    state: RepairState::Failed,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let hidden = manager.hidden_torrent_ids().await;
        assert_eq!(
            hidden.len(),
            3,
            "Should have 3 hidden torrents (broken, repairing, failed)"
        );
        assert!(
            !hidden.contains("healthy1"),
            "Healthy torrent should not be hidden"
        );
        assert!(
            hidden.contains("broken1"),
            "Broken torrent should be hidden"
        );
        assert!(
            hidden.contains("repairing1"),
            "Repairing torrent should be hidden"
        );
        assert!(
            hidden.contains("failed1"),
            "Failed torrent should be hidden"
        );
    }

    #[tokio::test]
    async fn hidden_torrent_ids_consistent_with_should_hide() {
        // Verify that hidden_torrent_ids returns the same set as calling
        // should_hide_torrent on each individual torrent.
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            for (id, state) in [
                ("a", RepairState::Healthy),
                ("b", RepairState::Broken),
                ("c", RepairState::Repairing),
                ("d", RepairState::Failed),
            ] {
                health_map.insert(
                    id.to_string(),
                    TorrentHealth {
                        torrent_id: id.to_string(),
                        state,
                        failed_links: HashSet::new(),
                        last_check: std::time::Instant::now(),
                        repair_attempts: 0,
                        last_repair_trigger: None,
                    },
                );
            }
        }

        let hidden_set = manager.hidden_torrent_ids().await;
        for id in ["a", "b", "c", "d"] {
            assert_eq!(
                hidden_set.contains(id),
                manager.should_hide_torrent(id).await,
                "hidden_torrent_ids and should_hide_torrent must agree for torrent '{}'",
                id
            );
        }
    }

    #[tokio::test]
    async fn non_cached_repair_records_replacement_mapping() {
        // Verify that the non-cached repair path in try_instant_repair
        // records a replacement mapping (new_torrent_id -> old_torrent_id)
        // so the scan loop can reuse old TMDB identification.
        let manager = make_test_manager();

        // Simulate the non-cached branch behavior by directly inserting
        // a replacement mapping the same way the non-cached branch does.
        {
            let mut map = manager.repair_replacements.write().await;
            map.insert("new_non_cached_id".to_string(), "old_broken_id".to_string());
        }

        let replacements = manager.take_repair_replacements().await;
        assert_eq!(replacements.len(), 1);
        assert_eq!(
            replacements.get("new_non_cached_id").unwrap(),
            "old_broken_id",
            "Non-cached repair must record new_torrent_id -> old_torrent_id mapping"
        );
    }

    #[test]
    fn non_cached_branch_inserts_repair_replacement() {
        // Source-level check: the non-cached else branch in try_instant_repair
        // must insert into repair_replacements so the scan loop can reuse
        // old TMDB identification for the replacement torrent.
        let source = include_str!("repair.rs");
        let fn_start = source
            .find("async fn try_instant_repair")
            .expect("function must exist");
        let fn_body = &source[fn_start..];

        // Find the non-cached else branch
        let else_marker = fn_body
            .find("Not cached -- torrent needs actual download")
            .expect("must have non-cached branch comment");
        let else_branch = &fn_body[else_marker..];

        // The else branch must contain repair_replacements insertion
        assert!(
            else_branch.contains("repair_replacements"),
            "Non-cached branch in try_instant_repair must insert into repair_replacements \
             so the scan loop can reuse old TMDB identification for the replacement torrent"
        );
    }

    #[tokio::test]
    async fn prune_health_status_removes_stale_keeps_active() {
        let manager = make_test_manager();

        // Populate health_status with several entries
        {
            let mut health_map = manager.health_status.write().await;
            for id in ["active1", "active2", "stale1", "stale2", "stale3"] {
                health_map.insert(
                    id.to_string(),
                    TorrentHealth {
                        torrent_id: id.to_string(),
                        state: RepairState::Healthy,
                        failed_links: HashSet::new(),
                        last_check: std::time::Instant::now(),
                        repair_attempts: 0,
                        last_repair_trigger: None,
                    },
                );
            }
        }

        // Only "active1" and "active2" are still active
        let active_ids: HashSet<&str> = ["active1", "active2"].into_iter().collect();
        manager.prune_health_status(&active_ids).await;

        let health_map = manager.health_status.read().await;
        assert_eq!(
            health_map.len(),
            2,
            "Should only have 2 active entries after pruning"
        );
        assert!(
            health_map.contains_key("active1"),
            "active1 should be retained"
        );
        assert!(
            health_map.contains_key("active2"),
            "active2 should be retained"
        );
        assert!(
            !health_map.contains_key("stale1"),
            "stale1 should be pruned"
        );
        assert!(
            !health_map.contains_key("stale2"),
            "stale2 should be pruned"
        );
        assert!(
            !health_map.contains_key("stale3"),
            "stale3 should be pruned"
        );
    }

    #[tokio::test]
    async fn prune_health_status_no_op_when_all_active() {
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            for id in ["t1", "t2"] {
                health_map.insert(
                    id.to_string(),
                    TorrentHealth {
                        torrent_id: id.to_string(),
                        state: RepairState::Healthy,
                        failed_links: HashSet::new(),
                        last_check: std::time::Instant::now(),
                        repair_attempts: 0,
                        last_repair_trigger: None,
                    },
                );
            }
        }

        let active_ids: HashSet<&str> = ["t1", "t2"].into_iter().collect();
        manager.prune_health_status(&active_ids).await;

        let health_map = manager.health_status.read().await;
        assert_eq!(
            health_map.len(),
            2,
            "All entries should be retained when all are active"
        );
    }

    #[tokio::test]
    async fn prune_health_status_empty_active_removes_all() {
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "orphan".to_string(),
                TorrentHealth {
                    torrent_id: "orphan".to_string(),
                    state: RepairState::Failed,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let active_ids: HashSet<&str> = HashSet::new();
        manager.prune_health_status(&active_ids).await;

        let health_map = manager.health_status.read().await;
        assert!(
            health_map.is_empty(),
            "All entries should be pruned when active set is empty"
        );
    }

    #[tokio::test]
    async fn mark_broken_preserves_last_repair_trigger_preventing_rapid_loops() {
        let manager = make_test_manager();

        // Simulate a torrent that was just repaired (has recent last_repair_trigger)
        let recent_trigger = std::time::Instant::now();
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "rapid_torrent".to_string(),
                TorrentHealth {
                    torrent_id: "rapid_torrent".to_string(),
                    state: RepairState::Healthy,
                    failed_links: HashSet::new(),
                    last_check: std::time::Instant::now(),
                    repair_attempts: 1,
                    last_repair_trigger: Some(recent_trigger),
                },
            );
        }

        // Torrent breaks again immediately
        manager
            .mark_broken("rapid_torrent", "http://broken_link")
            .await;

        // The 30-second cooldown should still be in effect because
        // mark_broken preserves last_repair_trigger
        let result = manager.check_and_begin_repair("rapid_torrent").await;
        assert!(
            result.is_err(),
            "Repair should be rate-limited after mark_broken preserves trigger"
        );
        assert_eq!(result.unwrap_err(), "Repair rate limited");
    }
}
