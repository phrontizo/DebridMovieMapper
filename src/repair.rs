use crate::provider::DebridProvider;
use crate::rd_client::TorrentInfo;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Minimum seconds between repair triggers for one torrent — bounds a per-read repair storm on a
/// still-broken-on-CDN cached file (a fresh re-add each read would otherwise hammer the provider).
const REPAIR_COOLDOWN_SECS: u64 = 30;
/// Maximum consecutive failed repairs before a torrent is marked permanently `Failed`. Counts only
/// *consecutive* failures — a confirmed good read (`note_read_success`) resets the budget.
const MAX_REPAIR_ATTEMPTS: u32 = 3;

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
    pub repair_attempts: u32,
    pub last_repair_trigger: Option<std::time::Instant>,
}

#[derive(Debug)]
pub struct RepairManager {
    health_status: Arc<RwLock<HashMap<String, TorrentHealth>>>,
    /// The active debrid provider (Real-Debrid or TorBox) — repair is provider-neutral.
    provider: Arc<dyn DebridProvider>,
    /// Maps new_torrent_id -> old_torrent_id for successful repairs.
    /// The scan loop consumes this to reuse old TMDB identifications.
    repair_replacements: Arc<RwLock<HashMap<String, String>>>,
}

impl RepairManager {
    pub fn new(provider: Arc<dyn DebridProvider>) -> Self {
        Self {
            health_status: Arc::new(RwLock::new(HashMap::new())),
            provider,
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
    /// Prevents duplicate torrents from accumulating in the debrid account.
    async fn cleanup_leaked_torrent(&self, new_torrent_id: &str) {
        warn!(
            "Cleaning up leaked torrent {} after failed repair",
            new_torrent_id
        );
        if let Err(e) = self.provider.delete_torrent(new_torrent_id).await {
            error!(
                "Failed to clean up leaked torrent {}: {}",
                new_torrent_id, e
            );
        }
    }

    /// Called when a byte read SUCCEEDS, confirming the file genuinely works. Resets the torrent's
    /// repair budget (`repair_attempts` → 0, cooldown cleared) so the 3-attempt cap counts only
    /// CONSECUTIVE failed repairs (an actual storm on a still-broken file), not unrelated repair
    /// incidents accumulated over a long-running deployment. `complete_repair` carries
    /// `repair_attempts` forward across re-adds to bound a storm; without this reset on a confirmed
    /// good read, that carry would become a permanent lifetime cap — a file legitimately repaired 3
    /// separate times would be wrongly promoted to `Failed`/hidden on its 4th genuine incident. A
    /// genuinely-broken file never gets a successful read, so it still converges to `Failed`.
    ///
    /// Fast-path read lock: the common case (a clean torrent with no repair history) takes no write
    /// lock, so calling this per opened file is cheap.
    pub async fn note_read_success(&self, torrent_id: &str) {
        {
            let health_map = self.health_status.read().await;
            match health_map.get(torrent_id) {
                // Skip while a repair is mid-flight on this id: flipping Repairing→Healthy here would
                // clear the in-progress guard + cooldown, letting a concurrent reader kick off a
                // redundant re-add. The in-flight repair's own completion path resets the budget.
                Some(h)
                    if h.state != RepairState::Repairing
                        && (h.repair_attempts > 0 || h.last_repair_trigger.is_some()) => {}
                _ => return, // no entry, mid-repair, or already a clean budget — nothing to reset
            }
        }
        let mut health_map = self.health_status.write().await;
        if let Some(h) = health_map.get_mut(torrent_id) {
            // Re-check under the write lock: another task may have begun a repair between locks.
            if h.state == RepairState::Repairing {
                return;
            }
            h.repair_attempts = 0;
            h.last_repair_trigger = None;
            h.state = RepairState::Healthy;
        }
    }

    /// Reset a torrent to `Healthy` after a repair attempt (or path-mismatch) that produced NO usable
    /// replacement to swap in — a transient re-add/info/listing error, the re-added file list not
    /// resolving, or a `downloaded` re-add whose file path momentarily isn't listed. It keeps the
    /// torrent VISIBLE (`read_bytes` short-circuits on `should_hide_torrent` *before* any I/O, so a
    /// `Broken`/`Repairing`/`Failed` torrent can never re-trigger repair — leaving it hidden with no
    /// recorded replacement traps it until restart) AND resets `repair_attempts` to 0: a transient
    /// failure is not evidence the file is broken, so it must not accumulate toward the 3-attempt
    /// `Failed` cap. Without the reset, three cooldown-spaced reads with no good read between would
    /// promote a same-id (TorBox) torrent to `Failed`/hidden — unrecoverable (hidden → no reads → no
    /// `note_read_success` → never reset; still listed → never pruned), the very trap the cached and
    /// uncached same-id resets prevent. The 30s cooldown (`last_repair_trigger`) is PRESERVED, so
    /// retries stay rate-limited to one per 30s even though the budget no longer climbs.
    ///
    /// (`Broken`/`Failed` is set ONLY where a replacement IS recorded for the scan loop to swap in —
    /// the not-cached, new-id path — so the hidden original is correctly superseded.)
    async fn note_transient_repair_failure(&self, torrent_id: &str) {
        let mut health_map = self.health_status.write().await;
        if let Some(health) = health_map.get_mut(torrent_id) {
            health.state = RepairState::Healthy;
            health.repair_attempts = 0;
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
                    if last_trigger.elapsed().as_secs() < REPAIR_COOLDOWN_SECS {
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
            if health.repair_attempts >= MAX_REPAIR_ATTEMPTS {
                error!(
                    "Torrent {} has failed repair {} times, marking as permanently FAILED",
                    torrent_id, MAX_REPAIR_ATTEMPTS
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
                if last_trigger.elapsed().as_secs() < REPAIR_COOLDOWN_SECS {
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
                    repair_attempts: 1,
                    last_repair_trigger: Some(std::time::Instant::now()),
                },
            );
            1
        };

        Ok(attempt_num)
    }

    /// Add magnet, wait for the provider to process, get new torrent info, match files by path, select them.
    /// Returns (new_torrent_id, new_torrent_info) on success.
    /// On failure, cleans up the leaked torrent and marks the old torrent as failed.
    async fn add_and_select_files(
        &self,
        old_torrent_id: &str,
        old_info: &TorrentInfo,
        wait_duration: Duration,
    ) -> Result<(String, TorrentInfo), String> {
        // Selector: match the new torrent's files to the old torrent's previously-selected paths.
        let old_info_cl = old_info.clone();
        let select = move |new_info: &TorrentInfo| -> Vec<u32> {
            old_info_cl
                .files
                .iter()
                .filter(|f| f.selected == 1)
                .filter_map(|of| {
                    new_info
                        .files
                        .iter()
                        .find(|nf| nf.path == of.path)
                        .map(|nf| nf.id)
                })
                .collect()
        };
        // Repair keeps the single-poll behaviour (max_wait == settle): the re-added torrent is a
        // known-good hash whose metadata should already be available. `protect_id = old_torrent_id`
        // so that on a same-id re-add (TorBox recovers the existing torrent by infohash) a transient
        // post-add failure NEVER deletes the real torrent.
        match crate::reacquire::materialise(
            &*self.provider,
            &old_info.hash,
            wait_duration,
            wait_duration,
            Some(old_torrent_id),
            select,
        )
        .await
        {
            Ok(pair) => Ok(pair),
            Err(e) => {
                // A materialise failure is transient (re-add HTTP error, or the file list not
                // resolving within the poll window) and produced NO replacement to swap in — keep
                // the torrent VISIBLE (Healthy) so the next read can re-attempt repair under the
                // cooldown. Marking it Broken would hide it, and a hidden torrent can never
                // re-trigger repair (reads short-circuit on `should_hide_torrent`) → it would be
                // trapped until restart. A transient error is not a "broken file" signal, so reset
                // the attempt budget too — critical for a same-id (TorBox) re-add where `old` IS the
                // real, present torrent and a Failed cap-out would be unrecoverable.
                self.note_transient_repair_failure(old_torrent_id).await;
                Err(e.to_string())
            }
        }
    }

    /// Finalise a successful repair: (for a genuinely new torrent) delete the old one and record the
    /// replacement, and update the health map to track the repaired torrent.
    ///
    /// `old == new` when the provider's re-add-by-hash returns the SAME torrent id — TorBox's
    /// `createtorrent` recovers the existing entry by infohash, so a repaired-in-place torrent keeps
    /// its id. In that case we must NOT delete it (that would destroy the very torrent we repaired)
    /// and there is no replacement to record.
    ///
    /// The repaired torrent's health carries the old entry's `repair_attempts` and a FRESH cooldown
    /// trigger forward, so the 30s cooldown and 3-attempt cap survive across re-adds. Real-Debrid
    /// mints a NEW id on every re-add; without carrying state, each "successful" repair of a
    /// still-broken-on-CDN cached file would land on a fresh, trigger-less, attempts-0 health entry,
    /// so the cooldown never applied → an unbounded re-add/delete storm on every byte read. On a
    /// genuinely-fixed file there is no further 5xx, so the carried cooldown is harmless.
    async fn complete_repair(&self, old_torrent_id: &str, new_torrent_id: &str) {
        let same = old_torrent_id == new_torrent_id;
        if !same {
            if let Err(e) = self.provider.delete_torrent(old_torrent_id).await {
                warn!("Failed to delete old torrent {}: {}", old_torrent_id, e);
            }
        }

        let mut health_map = self.health_status.write().await;
        // Carry the attempt budget forward ONLY for a new-id (Real-Debrid) re-add, so the 30s
        // cooldown + 3-attempt cap bound RD's per-read new-id storm. For a same-id (TorBox) re-add
        // this success path is reached only when the file IS present and cached (`locator_for_file`
        // matched), so a still-failing byte read is a transient CDN issue, not missing content —
        // RESET the budget so it can never accumulate to the Failed/hidden state, which is
        // unrecoverable for a same-id torrent (hidden → no reads → `note_read_success` never fires).
        // The fresh `last_repair_trigger` below still rate-limits re-adds to one per 30s.
        let carried_attempts = if same {
            0
        } else {
            health_map
                .get(old_torrent_id)
                .map(|h| h.repair_attempts)
                .unwrap_or(0)
        };
        if !same {
            health_map.remove(old_torrent_id);
        }
        health_map.insert(
            new_torrent_id.to_string(),
            TorrentHealth {
                torrent_id: new_torrent_id.to_string(),
                state: RepairState::Healthy,
                repair_attempts: carried_attempts,
                last_repair_trigger: Some(std::time::Instant::now()),
            },
        );
        drop(health_map);

        if !same {
            // Record replacement so the scan loop reuses the old TMDB identification.
            self.repair_replacements
                .write()
                .await
                .insert(new_torrent_id.to_string(), old_torrent_id.to_string());
        }
    }

    /// Build a `FileLocator` for the file at `file_path` within `info`. The per-file
    /// restricted link is paired by position among selected files (Real-Debrid); for
    /// providers with no per-file link array (TorBox) `links` is empty so `link` is
    /// `None` and the file is addressed by `(torrent_id, file_id)`. Returns `None` if
    /// no selected file matches `file_path`.
    fn locator_for_file(
        info: &TorrentInfo,
        hash: &str,
        file_path: &str,
    ) -> Option<crate::provider::FileLocator> {
        let mut link_idx = 0;
        for file in &info.files {
            if file.selected == 1 {
                if file.path == file_path {
                    return Some(crate::provider::FileLocator {
                        hash: hash.to_string(),
                        torrent_id: info.id.clone(),
                        file_id: file.id,
                        file_path: file_path.to_string(),
                        link: info.links.get(link_idx).cloned(),
                    });
                }
                link_idx += 1;
            }
        }
        None
    }

    /// Attempt instant repair (re-acquire) for a broken/uncached file. Re-adds the
    /// torrent by hash; if the replacement is immediately available, returns a fresh
    /// `FileLocator` for the SAME file (matched by `file_path`). Returns `Err` if the
    /// torrent needs downloading or the repair fails.
    pub async fn try_instant_repair(
        &self,
        locator: &crate::provider::FileLocator,
    ) -> Result<crate::provider::FileLocator, String> {
        let torrent_id = locator.torrent_id.as_str();
        let attempt_num = self.check_and_begin_repair(torrent_id).await?;
        info!(
            "Instant repair attempt #{} for torrent {}",
            attempt_num, torrent_id
        );

        // Fetch old torrent info to know which files were selected (for re-selection).
        let old_info = match self.provider.get_torrent_info(torrent_id).await {
            Ok(info) => info,
            Err(e) => {
                // Transient listing failure, no replacement produced — keep the torrent VISIBLE
                // (Healthy) so the next read can re-attempt repair (Broken would hide it with no way
                // back). Reset the attempt budget (a transient error is not a broken-file signal),
                // preserving the cooldown; see `note_transient_repair_failure`.
                self.note_transient_repair_failure(torrent_id).await;
                return Err(format!("Failed to get torrent info: {}", e));
            }
        };

        info!("Instant repair: adding magnet for hash {}", old_info.hash);
        let (new_torrent_id, _new_info) = self
            .add_and_select_files(torrent_id, &old_info, Duration::from_millis(500))
            .await?;
        info!("Instant repair: new torrent ID {}", new_torrent_id);

        // TorBox re-adds by infohash return the SAME torrent id (createtorrent recovers the existing
        // entry), so the re-added torrent IS the one under repair — never delete/clean it up as a
        // "leaked" duplicate. Real-Debrid mints a fresh id, so its cleanup/delete paths are real.
        let same_torrent = new_torrent_id == torrent_id;

        // Brief wait for the provider to process file selection.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let final_info = match self.provider.get_torrent_info(&new_torrent_id).await {
            Ok(info) => info,
            Err(e) => {
                // The re-add already succeeded; only the final info fetch blipped (transient). There
                // is NO usable replacement to swap in, so keep the original torrent VISIBLE (Healthy)
                // and reset the attempt budget (preserving the cooldown) — Broken would hide it with
                // no replacement, and accumulating toward Failed on a transient error would trap a
                // same-id torrent. For a genuinely-new id (RD) also delete the leaked replacement.
                if !same_torrent {
                    self.cleanup_leaked_torrent(&new_torrent_id).await;
                }
                self.note_transient_repair_failure(torrent_id).await;
                return Err(format!("Failed to get final torrent info: {}", e));
            }
        };

        if final_info.status == "downloaded" {
            // Match the SAME file by path (provider-neutral; no positional link index).
            match Self::locator_for_file(&final_info, &locator.hash, &locator.file_path) {
                Some(new_locator) => {
                    self.complete_repair(torrent_id, &new_torrent_id).await;
                    info!(
                        "Instant repair SUCCEEDED for torrent {} -> new ID {} (file {})",
                        torrent_id, new_torrent_id, locator.file_path
                    );
                    Ok(new_locator)
                }
                None => {
                    // The re-added torrent is `downloaded` but our `file_path` isn't in it. This is
                    // usually a TRANSIENT settle blip — `final_info` is fetched only ~500ms after the
                    // add, and TorBox's `files` field is loose/momentarily-empty — not genuinely
                    // missing content. Keep the ORIGINAL torrent VISIBLE and RESET its attempt budget
                    // (`note_transient_repair_failure`), exactly like the other transient arms
                    // (info-fetch / materialise failure): trapping it `Failed`/hidden with no
                    // replacement is unrecoverable (hidden → no reads → `note_read_success` never
                    // fires → never reset; still listed → never pruned), and a SAME-id (TorBox)
                    // re-add does NOT itself reset attempts (only a confirmed good read via
                    // `note_read_success` does), so three cooldown-spaced reads would still hit the
                    // attempt cap and trap the real torrent. The cooldown still rate-limits retries.
                    // For a genuinely-new id (RD) also delete the
                    // leaked replacement (TorBox's same-id re-add IS the real torrent — never delete).
                    if !same_torrent {
                        self.cleanup_leaked_torrent(&new_torrent_id).await;
                    }
                    self.note_transient_repair_failure(torrent_id).await;
                    Err(format!(
                        "Repaired torrent missing file path {}",
                        locator.file_path
                    ))
                }
            }
        } else {
            // Not cached -- torrent needs actual download.
            info!(
                "Torrent {} not cached (status: {}), leaving new torrent {} to download",
                torrent_id, final_info.status, new_torrent_id
            );

            // For TorBox (same id) the re-add IS this torrent — deleting it would remove the very
            // download we're leaving to complete. Only the genuinely-new-id (RD) case deletes the
            // old broken torrent and records a replacement for the scan loop.
            if !same_torrent {
                if let Err(e) = self.provider.delete_torrent(torrent_id).await {
                    warn!("Failed to delete old torrent {}: {}", torrent_id, e);
                }
                self.repair_replacements
                    .write()
                    .await
                    .insert(new_torrent_id.to_string(), torrent_id.to_string());
            }

            let mut health_map = self.health_status.write().await;
            if let Some(health) = health_map.get_mut(torrent_id) {
                if same_torrent {
                    // TorBox (same id): this IS the real, re-downloading torrent. Keep it Healthy so
                    // the normal scan surfaces it once it caches. A re-download in progress is NOT a
                    // failed repair, so RESET the attempt budget — otherwise a slow re-cache (a large
                    // 4K file taking >~90s) accumulates `repair_attempts` across the cooldown-spaced
                    // read retries and `check_and_begin_repair` promotes it to `Failed`/hidden, which
                    // is UNRECOVERABLE: hidden → no reads → no `note_read_success`, and a `Failed`
                    // state short-circuits `check_and_begin_repair` (so no Healthy-setter is ever
                    // reached) while the still-listed torrent is never pruned. The 30s cooldown
                    // (`last_repair_trigger`, preserved) still bounds the retry rate.
                    health.state = RepairState::Healthy;
                    health.repair_attempts = 0;
                } else {
                    // RD (new id): the old torrent is superseded → Broken (hidden until the scan loop
                    // picks up the recorded replacement).
                    health.state = RepairState::Broken;
                }
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

    /// Summary of repair status as `(healthy, repairing, failed)`, where "repairing" counts both
    /// `Broken` and `Repairing`. Used by `repair_integration_test` to assert post-repair state.
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

    /// Force a torrent into the `Broken` state (hidden from WebDAV until a replacement
    /// is surfaced). The live playback path no longer calls this — a 503/`Unavailable`
    /// now drives synchronous instant repair (`try_instant_repair`) instead — so this is
    /// the primitive used to simulate a broken torrent in tests and remains available for
    /// any caller that needs to mark a torrent broken directly.
    pub async fn mark_broken(&self, torrent_id: &str) {
        let mut health_map = self.health_status.write().await;

        warn!("Marking torrent {} as BROKEN", torrent_id);

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
                repair_attempts: previous_attempts,
                last_repair_trigger: previous_trigger,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FileLocator;
    use std::collections::HashSet;

    #[test]
    fn repair_manager_accepts_trait_object() {
        use crate::provider::{DebridProvider, MockProvider};
        let provider: std::sync::Arc<dyn DebridProvider> =
            std::sync::Arc::new(MockProvider::default());
        let _manager = RepairManager::new(provider);
    }

    /// Compile-time check: try_instant_repair exists with the correct signature.
    #[allow(dead_code)]
    async fn _assert_try_instant_repair_signature(manager: &RepairManager) {
        let _: Result<crate::provider::FileLocator, String> =
            manager.try_instant_repair(&FileLocator::default()).await;
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

    /// `locator_for_file` advances `link_idx` ONLY for `selected == 1` files, pairing RD's
    /// per-file restricted links positionally among the SELECTED files. With a SELECTED file
    /// AND an UNSELECTED file both preceding the target, the target is the 2nd selected file
    /// (`link_idx == 1`) and must pair `links[1]`; the unselected file must NOT consume an index.
    /// The single-selected-file repair tests never reach `link_idx > 0`, so this guards it.
    #[test]
    fn locator_for_file_pairs_link_by_position_among_selected_files() {
        use crate::rd_client::{TorrentFile, TorrentInfo};

        let info = TorrentInfo {
            id: "tid".to_string(),
            hash: "H".to_string(),
            status: "downloaded".to_string(),
            files: vec![
                // Selected, but not the target → pushes link_idx to 1.
                TorrentFile {
                    id: 10,
                    path: "/SelectedFirst.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                },
                // Unselected → must be skipped WITHOUT consuming a link index.
                TorrentFile {
                    id: 20,
                    path: "/Unselected.mkv".to_string(),
                    bytes: 500,
                    selected: 0,
                },
                // The target: the 2nd selected file → must pair links[1].
                TorrentFile {
                    id: 30,
                    path: "/Target.mkv".to_string(),
                    bytes: 2000,
                    selected: 1,
                },
            ],
            // RD emits one link per SELECTED file, in selected order: [SelectedFirst, Target].
            links: vec![
                "https://rd/selected-first".to_string(),
                "https://rd/target".to_string(),
            ],
            ..Default::default()
        };

        let loc = RepairManager::locator_for_file(&info, "H", "/Target.mkv")
            .expect("target file should resolve to a locator");

        assert_eq!(loc.file_id, 30);
        assert_eq!(loc.file_path, "/Target.mkv");
        assert_eq!(loc.hash, "H");
        assert_eq!(loc.torrent_id, "tid");
        assert_eq!(
            loc.link.as_deref(),
            Some("https://rd/target"),
            "the 2nd selected file must pair links[1]; the unselected file must not consume an index"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_cached_returns_new_locator() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // MockProvider returns a "downloaded" torrent containing the target file with a link.
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "new_tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new_tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["https://rd/newlink".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));

        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "old_tid".to_string(),
            file_id: 1,
            file_path: "/Movie.mkv".to_string(),
            link: Some("https://rd/oldlink".to_string()),
        };
        let new = manager
            .try_instant_repair(&old)
            .await
            .expect("repair should succeed");
        assert_eq!(new.torrent_id, "new_tid");
        assert_eq!(new.file_id, 5);
        assert_eq!(new.file_path, "/Movie.mkv");
        assert_eq!(new.link.as_deref(), Some("https://rd/newlink"));
        assert_eq!(new.hash, "H");
    }

    #[tokio::test]
    async fn try_instant_repair_torbox_same_id_does_not_delete_the_repaired_torrent() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // TorBox: re-add by infohash returns the SAME torrent id ("tid") as the one under repair,
        // and it is downloaded (cached). Repair must succeed WITHOUT deleting "tid" — deleting it
        // would destroy the very torrent it repaired (data loss for mirror/manual content).
        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(), // SAME id as the locator's torrent — TorBox behaviour
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec![], // TorBox has no per-file links
                ..Default::default()
            }),
            deleted: deleted.clone(),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));

        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 5,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let new = manager
            .try_instant_repair(&old)
            .await
            .expect("repair should succeed");
        assert_eq!(new.torrent_id, "tid");
        assert_eq!(new.file_path, "/Movie.mkv");
        assert!(
            deleted.lock().unwrap().is_empty(),
            "the repaired-in-place torrent (same id) must NOT be deleted, got: {:?}",
            deleted.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn try_instant_repair_torbox_same_id_uncached_leaves_torrent_visible() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // TorBox: re-add returns the SAME id, but the torrent is NOT yet downloaded (re-downloading).
        // The repair must fail this read WITHOUT trapping the torrent hidden — it is the real torrent
        // and will be surfaced once it downloads. It must stay non-hidden and not be deleted.
        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(), // same id as the locator — TorBox behaviour
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloading".to_string(), // not cached
                files: vec![TorrentFile {
                    id: 0,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                ..Default::default()
            }),
            deleted: deleted.clone(),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "uncached repair returns Err for this read");
        assert!(
            deleted.lock().unwrap().is_empty(),
            "the same-id re-downloading torrent must not be deleted"
        );
        assert!(
            !manager.should_hide_torrent("tid").await,
            "the same-id re-downloading torrent must stay VISIBLE (not trapped Broken/hidden)"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_torbox_same_id_uncached_resets_attempt_budget() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // A slow-to-recache TorBox same-id torrent must NOT accumulate toward the 3-attempt Failed
        // cap — a re-download in progress is not a failed repair. Pre-seed 2 prior attempts with an
        // expired cooldown; after a same-id uncached repair the budget resets to 0 and the torrent
        // stays Healthy/visible, so a slow re-cache can never be trapped Failed/hidden (which would be
        // unrecoverable). Without the reset, this read would push attempts to 3 and the next would
        // promote it to Failed.
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloading".to_string(), // not cached
                files: vec![TorrentFile {
                    id: 0,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        {
            let mut health = manager.health_status.write().await;
            health.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 2,
                    last_repair_trigger: Some(
                        std::time::Instant::now() - std::time::Duration::from_secs(31),
                    ),
                },
            );
        }
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "uncached repair returns Err for this read");
        let health = manager.health_status.read().await;
        let h = health.get("tid").expect("health entry present");
        assert_eq!(
            h.repair_attempts, 0,
            "a same-id re-download must reset the attempt budget, not accumulate toward Failed"
        );
        assert_eq!(h.state, RepairState::Healthy);
    }

    #[tokio::test]
    async fn try_instant_repair_same_id_materialise_failure_leaves_torrent_visible() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentInfo};

        // Same-id (TorBox) re-add whose torrent transiently reports no selectable files →
        // `materialise` fails (with protect_id, NOT deleting the real torrent) → the materialise
        // failure path must leave the real torrent VISIBLE (Healthy), not hide it as Broken with no
        // recovery (a hidden torrent can never re-trigger repair).
        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(), // same id — TorBox behaviour
                uri: String::new(),
            }),
            // No files → the path-match selector finds nothing → materialise errors.
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                ..Default::default()
            }),
            deleted: deleted.clone(),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "materialise failure returns Err for this read");
        assert!(
            deleted.lock().unwrap().is_empty(),
            "protect_id must prevent deleting the real same-id torrent on materialise failure"
        );
        assert!(
            !manager.should_hide_torrent("tid").await,
            "a same-id materialise failure must leave the real torrent VISIBLE (not Broken/hidden)"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_new_id_missing_file_stays_visible_not_failed() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};
        // RD mints a NEW id on re-add. The re-added torrent is `downloaded` but does NOT contain our
        // file_path (usually a transient ~500ms settle blip). The OLD torrent must stay VISIBLE
        // (Healthy) — NOT trapped Failed/hidden with no replacement — and the leaked NEW torrent must
        // be deleted. (Regression for the iter-19 fix: this arm previously called set_repair_failed.)
        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "new_rd_id".to_string(), // NEW id (!= old) — Real-Debrid behaviour
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new_rd_id".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "/OtherFile.mkv".to_string(), // NOT the requested /Movie.mkv
                    bytes: 1000,
                    selected: 1,
                }],
                ..Default::default()
            }),
            deleted: deleted.clone(),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "old_tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "a missing file returns Err for this read");
        assert!(
            !manager.should_hide_torrent("old_tid").await,
            "an RD new-id path-mismatch must leave the OLD torrent VISIBLE (not trapped Failed/hidden)"
        );
        assert!(
            deleted.lock().unwrap().iter().any(|id| id == "new_rd_id"),
            "the leaked new RD torrent must be cleaned up"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_transient_failure_resets_attempt_budget() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentInfo};

        // A TRANSIENT repair failure (here: a same-id re-add whose torrent reports no selectable
        // files → `materialise` errors) must RESET the attempt budget, not accumulate toward the
        // 3-attempt Failed cap. Pre-seed 2 prior attempts with an expired cooldown; without the
        // reset this read would push attempts to 3 and the next would promote the (same-id) torrent
        // to Failed/hidden — unrecoverable. The 30s cooldown must still be preserved.
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(), // same id — TorBox behaviour
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                ..Default::default() // no files → materialise errors (transient)
            }),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        {
            let mut health = manager.health_status.write().await;
            health.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 2,
                    last_repair_trigger: Some(
                        std::time::Instant::now() - std::time::Duration::from_secs(31),
                    ),
                },
            );
        }
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "transient failure returns Err for this read");
        let health = manager.health_status.read().await;
        let h = health.get("tid").expect("health entry present");
        assert_eq!(
            h.repair_attempts, 0,
            "a transient repair failure must reset the attempt budget (never trap a same-id torrent)"
        );
        assert_eq!(h.state, RepairState::Healthy);
        assert!(
            h.last_repair_trigger.is_some(),
            "the 30s cooldown must be preserved so retries stay rate-limited"
        );
    }

    #[tokio::test]
    async fn complete_repair_carries_attempts_and_cooldown_forward() {
        // Real-Debrid mints a NEW id per repair. The new health entry must carry the old entry's
        // repair_attempts and a fresh cooldown trigger, so the 30s cooldown / 3-attempt cap survive
        // across re-adds (otherwise a still-broken cached file storms re-add/delete on every read).
        let manager = make_test_manager();
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "old".to_string(),
                TorrentHealth {
                    torrent_id: "old".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 2,
                    last_repair_trigger: None,
                },
            );
        }
        manager.complete_repair("old", "new").await;
        let hm = manager.health_status.read().await;
        assert!(hm.get("old").is_none(), "old id removed");
        let new = hm.get("new").expect("new id tracked");
        assert_eq!(new.state, RepairState::Healthy);
        assert_eq!(
            new.repair_attempts, 2,
            "attempts carried forward across the re-add"
        );
        assert!(
            new.last_repair_trigger.is_some(),
            "a fresh cooldown trigger is set so the next read is rate-limited (storm bound)"
        );
    }

    #[tokio::test]
    async fn note_read_success_resets_repair_budget() {
        // A confirmed good read must reset the carried repair budget, so the 3-attempt cap bounds
        // only CONSECUTIVE failed repairs (a storm) — not separate incidents over the deployment's
        // lifetime (which the carry-forward would otherwise accumulate into a permanent Failed).
        let manager = make_test_manager();
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "t".to_string(),
                TorrentHealth {
                    torrent_id: "t".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 2,
                    last_repair_trigger: Some(std::time::Instant::now()),
                },
            );
        }
        manager.note_read_success("t").await;
        let hm = manager.health_status.read().await;
        let h = hm.get("t").expect("entry present");
        assert_eq!(
            h.repair_attempts, 0,
            "budget reset on a confirmed good read"
        );
        assert!(
            h.last_repair_trigger.is_none(),
            "cooldown cleared on a confirmed good read"
        );
        assert_eq!(h.state, RepairState::Healthy);
    }

    #[tokio::test]
    async fn note_read_success_is_a_noop_for_clean_or_absent_torrent() {
        let manager = make_test_manager();
        // Absent torrent: no panic, no entry created.
        manager.note_read_success("absent").await;
        assert!(manager.health_status.read().await.get("absent").is_none());
    }

    #[tokio::test]
    async fn note_read_success_skips_while_repairing() {
        // A confirmed good read on one handle must NOT clear an in-flight repair's Repairing
        // state/cooldown on another handle — doing so would let a third reader launch a redundant
        // re-add. The in-flight repair's completion path is responsible for resetting the budget.
        let manager = make_test_manager();
        let trigger = std::time::Instant::now();
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "t".to_string(),
                TorrentHealth {
                    torrent_id: "t".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 1,
                    last_repair_trigger: Some(trigger),
                },
            );
        }
        manager.note_read_success("t").await;
        let hm = manager.health_status.read().await;
        let h = hm.get("t").expect("entry present");
        assert_eq!(
            h.state,
            RepairState::Repairing,
            "in-flight repair untouched"
        );
        assert_eq!(
            h.repair_attempts, 1,
            "attempt budget not cleared mid-repair"
        );
        assert!(
            h.last_repair_trigger.is_some(),
            "cooldown not cleared mid-repair"
        );
    }

    #[tokio::test]
    async fn complete_repair_same_id_resets_attempts() {
        // TorBox re-adds by infohash recover the SAME id. On the cached-success path the file is
        // present, so a still-failing byte read is transient CDN trouble, not missing content: the
        // attempt budget MUST reset so a same-id torrent can never accumulate to Failed/hidden
        // (unrecoverable — hidden torrents never re-trigger repair). RD's new-id carry-forward
        // (covered by `complete_repair_carries_attempts_and_cooldown_forward`) is unaffected.
        let manager = make_test_manager();
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 2,
                    last_repair_trigger: None,
                },
            );
        }
        manager.complete_repair("tid", "tid").await;
        let hm = manager.health_status.read().await;
        let h = hm.get("tid").expect("same id still tracked");
        assert_eq!(h.state, RepairState::Healthy);
        assert_eq!(
            h.repair_attempts, 0,
            "a same-id (TorBox) repair resets the attempt budget so it can never trap as Failed"
        );
        assert!(
            h.last_repair_trigger.is_some(),
            "a fresh cooldown trigger still bounds re-adds to one per 30s"
        );
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
                    repair_attempts: 1,
                    last_repair_trigger: Some(std::time::Instant::now()),
                },
            );
        }

        let result = manager
            .try_instant_repair(&FileLocator {
                torrent_id: "torrent1".to_string(),
                link: Some("some_link".to_string()),
                ..Default::default()
            })
            .await;
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
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let result = manager
            .try_instant_repair(&FileLocator {
                torrent_id: "torrent2".to_string(),
                link: Some("some_link".to_string()),
                ..Default::default()
            })
            .await;
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
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
        }

        let result = manager
            .try_instant_repair(&FileLocator {
                torrent_id: "torrent3".to_string(),
                link: Some("some_link".to_string()),
                ..Default::default()
            })
            .await;
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

    // The TOCTOU guards in check_and_begin_repair (re-checking Failed / Repairing /
    // last_repair_trigger on the write side) are covered behaviourally by
    // `concurrent_check_and_begin_repair_only_one_succeeds` (only one of ten racers wins →
    // Repairing guard) and `check_and_begin_repair_write_side_rate_limits` (the
    // last_repair_trigger re-check rejects), rather than by asserting on source text.

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
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let result = manager
            .try_instant_repair(&FileLocator {
                torrent_id: "torrent4".to_string(),
                link: Some("some_link".to_string()),
                ..Default::default()
            })
            .await;
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
                    repair_attempts: 2,
                    last_repair_trigger: Some(std::time::Instant::now()),
                },
            );
        }

        // Mark it as broken again
        manager.mark_broken("torrent_preserve").await;

        // Verify repair_attempts is preserved
        let health_map = manager.health_status.read().await;
        let health = health_map.get("torrent_preserve").unwrap();
        assert_eq!(health.state, RepairState::Broken);
        assert_eq!(
            health.repair_attempts, 2,
            "mark_broken must preserve previous repair_attempts count"
        );
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
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "b1".to_string(),
                TorrentHealth {
                    torrent_id: "b1".to_string(),
                    state: RepairState::Broken,
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "r1".to_string(),
                TorrentHealth {
                    torrent_id: "r1".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "f1".to_string(),
                TorrentHealth {
                    torrent_id: "f1".to_string(),
                    state: RepairState::Failed,
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
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "broken1".to_string(),
                TorrentHealth {
                    torrent_id: "broken1".to_string(),
                    state: RepairState::Broken,
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "repairing1".to_string(),
                TorrentHealth {
                    torrent_id: "repairing1".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "failed1".to_string(),
                TorrentHealth {
                    torrent_id: "failed1".to_string(),
                    state: RepairState::Failed,
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
    async fn non_cached_repair_marks_broken_and_records_replacement() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // MockProvider returns a re-added torrent that is NOT yet downloaded (cache lapsed),
        // driving try_instant_repair into the non-cached branch.
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "new_tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new_tid".to_string(),
                hash: "H".to_string(),
                status: "downloading".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec![],
                ..Default::default()
            }),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "old_tid".to_string(),
            file_id: 1,
            file_path: "/Movie.mkv".to_string(),
            link: Some("https://rd/oldlink".to_string()),
        };

        // The non-cached branch returns an error (no fresh locator to serve).
        assert!(manager.try_instant_repair(&old).await.is_err());

        // It must record new_tid -> old_tid so the scan loop reuses the old identification.
        let replacements = manager.take_repair_replacements().await;
        assert_eq!(
            replacements.get("new_tid").map(String::as_str),
            Some("old_tid"),
            "non-cached repair must record the new->old replacement mapping"
        );

        // And it must mark the old torrent Broken so it is hidden until the scan swaps it in.
        assert!(
            manager.should_hide_torrent("old_tid").await,
            "non-cached repair must mark the old torrent Broken"
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
                    repair_attempts: 1,
                    last_repair_trigger: Some(recent_trigger),
                },
            );
        }

        // Torrent breaks again immediately
        manager.mark_broken("rapid_torrent").await;

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
