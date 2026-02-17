use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn, error, debug};
use crate::rd_client::{RealDebridClient, TorrentInfo};
use crate::vfs::MediaMetadata;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairState {
    Healthy,
    Checking,
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

    /// Check health of all torrents and identify broken ones
    pub async fn check_torrent_health(&self, torrents: &[(TorrentInfo, MediaMetadata)]) -> Vec<String> {
        info!("Starting health check on {} torrents", torrents.len());
        let mut broken_torrent_ids = Vec::new();
        let mut health_map = self.health_status.write().await;

        let mut to_check_count = 0;
        for (torrent_info, _metadata) in torrents {
            // Skip if already checking, repairing, or recently checked
            let should_check = if let Some(health) = health_map.get(&torrent_info.id) {
                if matches!(health.state, RepairState::Checking | RepairState::Repairing) {
                    false
                } else if health.last_check.elapsed().as_secs() < 300 && health.state == RepairState::Healthy {
                    // Only recheck after 5 minutes
                    false
                } else {
                    true
                }
            } else {
                true
            };

            if !should_check {
                continue;
            }

            to_check_count += 1;

            // Get previous repair attempts count before inserting
            let previous_attempts = health_map.get(&torrent_info.id).map(|h| h.repair_attempts).unwrap_or(0);

            // Mark as checking
            health_map.insert(torrent_info.id.clone(), TorrentHealth {
                torrent_id: torrent_info.id.clone(),
                state: RepairState::Checking,
                failed_links: HashSet::new(),
                last_check: std::time::Instant::now(),
                repair_attempts: previous_attempts,
                last_repair_trigger: None,
            });
        }

        if to_check_count > 0 {
            info!("Checking health of {} torrents (skipped {} cached)", to_check_count, torrents.len() - to_check_count);
        } else {
            info!("All {} torrents using cached health status", torrents.len());
        }

        let total_to_check = to_check_count;
        drop(health_map); // Release lock before async operations

        // Check each torrent's links
        let mut checked_count = 0;

        for (torrent_info, _metadata) in torrents {
            // Skip if not in checking state (cached)
            {
                let health_map = self.health_status.read().await;
                if let Some(health) = health_map.get(&torrent_info.id) {
                    if health.state != RepairState::Checking {
                        continue;
                    }
                }
            }

            checked_count += 1;
            info!("Health check progress: {}/{} - Checking torrent '{}'",
                checked_count, total_to_check, torrent_info.filename);

            let mut failed_links = HashSet::new();
            let mut any_failed = false;

            // Sample up to 3 links to check health (or all if fewer than 3)
            let links_to_check: Vec<_> = torrent_info.links.iter()
                .enumerate()
                .filter(|(idx, _)| {
                    // Check first, middle, and last link
                    *idx == 0 ||
                    *idx == torrent_info.links.len() / 2 ||
                    *idx == torrent_info.links.len().saturating_sub(1)
                })
                .map(|(_, link)| link.clone())
                .collect();

            debug!("  Checking {} sample links for torrent {}", links_to_check.len(), torrent_info.id);

            for (idx, link) in links_to_check.iter().enumerate() {
                // Add a small delay between link checks to avoid rate limiting
                if idx > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }

                if !self.rd_client.check_link_health(link).await {
                    failed_links.insert(link.clone());
                    any_failed = true;
                }
            }

            let mut health_map = self.health_status.write().await;
            if any_failed {
                warn!("Torrent '{}' ({}) has {} failed links - marking as BROKEN",
                    torrent_info.filename, torrent_info.id, failed_links.len());
                broken_torrent_ids.push(torrent_info.id.clone());

                if let Some(health) = health_map.get_mut(&torrent_info.id) {
                    health.state = RepairState::Broken;
                    health.failed_links = failed_links;
                    health.last_check = std::time::Instant::now();
                }
            } else {
                info!("Torrent '{}' ({}) is healthy", torrent_info.filename, torrent_info.id);
                if let Some(health) = health_map.get_mut(&torrent_info.id) {
                    health.state = RepairState::Healthy;
                    health.failed_links.clear();
                    health.last_check = std::time::Instant::now();
                }
            }
            drop(health_map);

            // Add a small delay between checking different torrents to avoid rate limiting
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        if checked_count > 0 {
            info!("Health check complete. Checked {} torrents, found {} broken", checked_count, broken_torrent_ids.len());
        } else {
            info!("Health check complete. All torrents using cached status");
        }
        broken_torrent_ids
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
                                    let mut health_map = self.health_status.write().await;
                                    if let Some(health) = health_map.get_mut(&torrent_info.id) {
                                        health.state = RepairState::Failed;
                                    }
                                    Err(format!("Failed to select files: {}", e))
                                }
                            }
                        } else {
                            error!("No matching files found in repaired torrent {}", add_response.id);
                            let mut health_map = self.health_status.write().await;
                            if let Some(health) = health_map.get_mut(&torrent_info.id) {
                                health.state = RepairState::Failed;
                            }
                            Err("No matching files found".to_string())
                        }
                    }
                    Err(e) => {
                        error!("Failed to get info for repaired torrent {}: {}", add_response.id, e);
                        let mut health_map = self.health_status.write().await;
                        if let Some(health) = health_map.get_mut(&torrent_info.id) {
                            health.state = RepairState::Failed;
                        }
                        Err(format!("Failed to get torrent info: {}", e))
                    }
                }
            }
            Err(e) => {
                error!("Failed to re-add torrent {}: {}", torrent_info.id, e);
                let mut health_map = self.health_status.write().await;
                if let Some(health) = health_map.get_mut(&torrent_info.id) {
                    health.state = RepairState::Failed;
                }
                Err(format!("Failed to add magnet: {}", e))
            }
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
