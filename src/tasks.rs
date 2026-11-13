use crate::acquire::AcquisitionEngine;
use crate::app_state::AppState;
use crate::error::AppError;
use crate::identification::identify_torrent;
use crate::provider::DebridProvider;
use crate::rd_client::Torrent;
use crate::repair::RepairManager;
use crate::scraper::MediaKind;
use crate::store::{
    AcquireRequest, OwnedRecord, OwnedStatus, Provenance, ProvenanceEntry, Store, WantedRecord,
};
use crate::tmdb_client::TmdbClient;
use crate::vfs::{DebridVfs, MediaMetadata, MediaType};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Resolve a torrent's metadata: an authoritative `hash -> MediaMetadata` (recorded by the
/// acquisition engine for content we chose) wins over filename-based TMDB identification.
/// The hash is lowercased to match the engine's lowercased keys. Also reports whether a TMDB search
/// FAILED during the identification (see [`identification_is_cacheable`]).
async fn resolve_metadata(
    store: &Store,
    tmdb_client: &TmdbClient,
    info: &crate::rd_client::TorrentInfo,
) -> (MediaMetadata, bool) {
    match store
        .authoritative_meta(info.hash.to_ascii_lowercase())
        .await
    {
        Some(m) => (m, false),
        None => {
            crate::tmdb_client::track_search_failures(identify_torrent(info, tmdb_client)).await
        }
    }
}

/// Whether an identification may be cached (persisted, and remembered for the session). A torrent
/// that came out unidentified because a TMDB search FAILED (an outage, not "no match") is not:
/// caching it would leave it unidentified for as long as it stays listed (nothing re-identifies a
/// cached match), so the account mirror would never record it as owned and the reconciler could
/// acquire a duplicate. It is shown under its fallback title and identified again after
/// [`TMDB_RETRY_AFTER`]. A genuine no-match is cached like any identification.
fn identification_is_cacheable(metadata: &MediaMetadata, tmdb_failed: bool) -> bool {
    metadata.external_id.is_some() || !tmdb_failed
}

/// How long a torrent left unidentified by a TMDB outage waits before it is identified again. Every
/// attempt during an outage spends the TMDB retry budget (~2.5 min per search stage), so retrying it
/// on every scan tick would stall the scan loop for the length of the outage.
const TMDB_RETRY_AFTER: Duration = Duration::from_secs(15 * 60);

pub struct ScanConfig {
    pub app: AppState,
}

/// Blacklist entries older than this are pruned (the table is otherwise append-only). A rejected
/// hash becomes eligible to retry again after the TTL — long enough that a wrong-title/corrupt hash
/// isn't re-tried constantly, short enough to bound the table and let a later better release through.
const BLACKLIST_TTL_SECS: u64 = 30 * 86_400;

/// How many wholesale-empty provider listings in a row it takes, while torrents were known, before
/// the scan loop accepts that the account really is empty.
const EMPTY_LISTINGS_BEFORE_TRUSTED: u32 = 3;

/// Whether this tick's provider listing is a suspect wholesale-empty one: `observe`, the mirror
/// reaper and the selection repoint already distrust a spurious empty-but-OK listing (a provider
/// glitch), and acting on it would empty the VFS (every title vanishes from WebDAV and Jellyfin is
/// told the whole library was deleted), drop the identification cache (the next good tick
/// re-identifies the whole library through TMDB) and make every owned title read as lapsed. So
/// while torrents were known (`known > 0`), an empty listing is accepted only once it has been seen
/// [`EMPTY_LISTINGS_BEFORE_TRUSTED`] times in a row (`consecutive_empty` counts this one).
fn empty_listing_is_suspect(listing_empty: bool, known: usize, consecutive_empty: u32) -> bool {
    listing_empty && known > 0 && consecutive_empty < EMPTY_LISTINGS_BEFORE_TRUSTED
}

/// Sleep until the next scan tick. `false` when shutdown was requested meanwhile.
async fn sleep_until_next_scan(
    interval_secs: u64,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    info!("Scan task: sleeping {}s until next scan", interval_secs);
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => true,
        _ = shutdown.changed() => {
            info!("Scan task: shutdown requested, exiting");
            false
        }
    }
}

pub async fn run_scan_loop(
    scan_config: ScanConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Keep a clone of the whole bundle (all Arcs) for helpers that take `&AppState` (e.g. the
    // duplicate-dedup pass), then destructure the original for the hot-path locals.
    let app = scan_config.app.clone();
    let AppState {
        provider,
        tmdb_client,
        vfs,
        store,
        repair_manager,
        config,
        jellyfin_client,
        http_client: _,
        scraper: _,
        engine,
        trakt_client: _,
        read_activity: _,
    } = scan_config.app;
    let interval_secs = config.scan_interval_secs;
    // Load persisted matches from DB on startup
    let persisted: HashMap<String, (crate::rd_client::TorrentInfo, MediaMetadata)> =
        store.load_all_matches().await;

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
        update_vfs(&vfs, &persisted_data, &repair_manager, &None, &store).await;
        info!(
            "Pre-populated VFS with {} persisted entries",
            persisted_data.len()
        );
    }

    info!("Scan task: running initial scan immediately");

    // Torrents left unidentified by a TMDB outage (not cached, see `identification_is_cacheable`):
    // their fallback identification and when to try TMDB again.
    let mut tmdb_retry: HashMap<
        String,
        (
            tokio::time::Instant,
            (crate::rd_client::TorrentInfo, MediaMetadata),
        ),
    > = HashMap::new();

    // Prune the append-only blacklist on startup and then roughly daily, so it can't grow unbounded
    // over the deployment's lifetime (which would also slow the O(total) blacklist read paths).
    let prune_every_ticks = (86_400 / interval_secs.max(1)).max(1);
    let mut tick: u64 = 0;
    // Consecutive wholesale-empty listings (see `empty_listing_is_suspect`).
    let mut consecutive_empty: u32 = 0;

    loop {
        if *shutdown.borrow() {
            info!("Scan task: shutdown requested, exiting");
            return;
        }
        let is_prune_tick = tick.is_multiple_of(prune_every_ticks);
        if is_prune_tick {
            let now = crate::now_unix_secs();
            let pruned = store
                .prune_blacklist_before(now.saturating_sub(BLACKLIST_TTL_SECS))
                .await;
            if pruned > 0 {
                info!(
                    "Pruned {} stale blacklist entr{} (older than {}d)",
                    pruned,
                    if pruned == 1 { "y" } else { "ies" },
                    BLACKLIST_TTL_SECS / 86_400
                );
            }
            // Bound the otherwise append-only upgrade_checks cursor table: drop rows for titles that
            // have left the library (removal / dedup / downgrade prune) so it can't grow unbounded
            // over the deployment's lifetime — same rationale as the blacklist prune above.
            let cursors = store.prune_upgrade_checks_not_owned().await;
            if cursors > 0 {
                info!("Pruned {cursors} upgrade-cursor row(s) for titles no longer owned");
            }
        }
        tick = tick.saturating_add(1);
        // Consume repair replacements (new_id → old_id) before processing torrents
        let repair_replacements = repair_manager.take_repair_replacements().await;
        if !repair_replacements.is_empty() {
            info!(
                "Processing {} repair replacement(s)",
                repair_replacements.len()
            );
        }

        info!("Refreshing torrent list...");
        match provider.get_torrents().await {
            Ok(torrents) => {
                consecutive_empty = if torrents.is_empty() {
                    consecutive_empty.saturating_add(1)
                } else {
                    0
                };
                if torrents.is_empty() {
                    warn!("No torrents found in {} account.", provider.name());
                }

                engine.observe(&torrents).await;

                if empty_listing_is_suspect(
                    torrents.is_empty(),
                    seen_torrents.len(),
                    consecutive_empty,
                ) {
                    warn!(
                        "scan: empty listing while {} torrent(s) were known ({}/{} in a row); treating it as suspect and leaving the library as it was",
                        seen_torrents.len(),
                        consecutive_empty,
                        EMPTY_LISTINGS_BEFORE_TRUSTED
                    );
                    if !sleep_until_next_scan(interval_secs, &mut shutdown).await {
                        return;
                    }
                    continue;
                }

                // Deduplicate torrents by hash — keep the newest "downloaded" entry per hash.
                // Duplicates arise when repair's add_magnet leaks a torrent, or when
                // external tools (e.g. DebridMediaManager) re-add the same hash.
                let (deduped_torrents, duplicate_ids) = dedup_torrents_by_hash(&torrents);
                // Idle-gate the provider-side deletion, like every other destructive path
                // (execute_remove / dedup_owned / the upgrade prune): a redundant same-hash duplicate
                // is never urgent, so defer its removal while any stream is active rather than risk
                // deleting a torrent id a playback read is still resolving against. The duplicate is
                // re-detected and removed on a later idle tick; `deduped_torrents` already hides it
                // from the VFS this tick regardless.
                if !duplicate_ids.is_empty() {
                    let idle_window = Duration::from_secs(config.upgrade.idle_secs);
                    if app.read_activity.all_idle(idle_window).await {
                        for dup_id in duplicate_ids {
                            info!("scan: deleting duplicate torrent {}", dup_id);
                            let rd = provider.clone();
                            tokio::spawn(async move {
                                if let Err(e) = rd.delete_torrent(&dup_id).await {
                                    tracing::error!(
                                        "Failed to delete duplicate torrent {}: {}",
                                        dup_id,
                                        e
                                    );
                                }
                            });
                        }
                    } else {
                        debug!(
                            "scan: library active; deferring deletion of {} duplicate torrent(s)",
                            duplicate_ids.len()
                        );
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
                                match provider.get_torrent_info(&torrent.id).await {
                                    Ok(new_info) => {
                                        if let Err(e) = store
                                            .replace_match(
                                                old_id.clone(),
                                                torrent.id.clone(),
                                                new_info.clone(),
                                                metadata.clone(),
                                            )
                                            .await
                                        {
                                            error!(
                                                "Failed to persist repair replacement to database: {}",
                                                e
                                            );
                                        }
                                        seen_torrents.insert(
                                            torrent.id.clone(),
                                            (new_info.clone(), metadata.clone()),
                                        );
                                        current_data.push((new_info, metadata));
                                    }
                                    Err(e) => {
                                        // Fully recovered this same tick (queued for re-identification
                                        // below), so this is a warning, not an error.
                                        warn!("Failed to get info for repair replacement {}: {}, falling back to re-identification", torrent.id, e);
                                        to_identify.push((*torrent).clone());
                                    }
                                }
                            } else {
                                // Old ID not in seen_torrents (edge case), fall back to normal identification
                                info!("Repair replacement old_id {} not found in seen_torrents, re-identifying {}", old_id, torrent.id);
                                to_identify.push((*torrent).clone());
                            }
                        } else if let Some((_, data)) = tmdb_retry
                            .get(&torrent.id)
                            .filter(|(retry_at, _)| tokio::time::Instant::now() < *retry_at)
                        {
                            // Unidentified during a TMDB outage: keep showing it under its fallback
                            // title until it is due for another try.
                            current_data.push(data.clone());
                        } else {
                            let cached = store.get_match(torrent.id.clone()).await;

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
                            let provider = provider.clone();
                            let tmdb_client = tmdb_client.clone();
                            let store = store.clone();
                            async move {
                                match provider.get_torrent_info(&torrent.id).await {
                                    Ok(info) => {
                                        let (metadata, tmdb_failed) =
                                            resolve_metadata(&store, &tmdb_client, &info).await;
                                        Ok::<
                                            (
                                                String,
                                                crate::rd_client::TorrentInfo,
                                                MediaMetadata,
                                                bool,
                                            ),
                                            reqwest::Error,
                                        >((
                                            torrent.id,
                                            info,
                                            metadata,
                                            tmdb_failed,
                                        ))
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                        })
                        .buffer_unordered(1);

                    let mut processed_new = 0;
                    // Batch pending DB writes: (id, torrent info, metadata)
                    let mut pending_db_writes: Vec<(
                        String,
                        crate::rd_client::TorrentInfo,
                        MediaMetadata,
                    )> = Vec::new();

                    while let Some(result) = tokio::select! {
                        result = stream.next() => result,
                        _ = shutdown.changed() => {
                            info!("Scan task: shutdown during identification, saving progress");
                            // Flush pending writes before shutting down
                            if !pending_db_writes.is_empty() {
                                flush_db_writes(&store, &mut pending_db_writes).await;
                            }
                            update_vfs(&vfs, &current_data, &repair_manager, &jellyfin_client, &store).await;
                            return;
                        }
                    } {
                        processed_new += 1;
                        match result {
                            Ok((id, info, metadata, tmdb_failed)) => {
                                if identification_is_cacheable(&metadata, tmdb_failed) {
                                    tmdb_retry.remove(&id);
                                    pending_db_writes.push((
                                        id.clone(),
                                        info.clone(),
                                        metadata.clone(),
                                    ));
                                    seen_torrents.insert(id, (info.clone(), metadata.clone()));
                                } else {
                                    warn!(
                                        "TMDB unreachable while identifying {} ({}); showing it as \"{}\" and retrying in {}s",
                                        id,
                                        info.filename,
                                        metadata.title,
                                        TMDB_RETRY_AFTER.as_secs()
                                    );
                                    tmdb_retry.insert(
                                        id,
                                        (
                                            tokio::time::Instant::now() + TMDB_RETRY_AFTER,
                                            (info.clone(), metadata.clone()),
                                        ),
                                    );
                                }
                                current_data.push((info, metadata));
                            }
                            Err(e) => error!("Failed to identify torrent: {}", e),
                        }
                        if processed_new % 10 == 0 || processed_new == new_total {
                            // Flush batched DB writes at each progress checkpoint
                            if !pending_db_writes.is_empty() {
                                flush_db_writes(&store, &mut pending_db_writes).await;
                            }
                            info!(
                                "Progress: {}/{} new torrents identified",
                                processed_new, new_total
                            );
                            update_vfs(
                                &vfs,
                                &current_data,
                                &repair_manager,
                                &jellyfin_client,
                                &store,
                            )
                            .await;
                        }
                    }
                } else {
                    update_vfs(
                        &vfs,
                        &current_data,
                        &repair_manager,
                        &jellyfin_client,
                        &store,
                    )
                    .await;
                }

                // Account-mirror: record every identified present torrent the engine doesn't already
                // track as an owned record (empty provenance — owned + upgradeable, not a protected
                // manual class), so the reconciler sees the user's pre-existing library (no duplicate
                // re-acquire; season-pack episodes count owned).
                record_mirror_owned(
                    &store,
                    &tmdb_client,
                    &config.acquisition.prefs,
                    &current_data,
                )
                .await;

                // Duplicate-dedup: now the whole library is owned, plan + (if enabled, idle-gated)
                // remove redundant duplicate torrents. Dry-run (log only) unless
                // DEDUP_REMOVE_DUPLICATES is set.
                dedup_owned(&app, &torrents).await;

                // Serve the best copy present: a slot whose copy is beaten by another present,
                // settled copy of strictly better quality (e.g. a pre-existing or re-added REMUX
                // behind an engine-acquired BluRay pack — such a copy never gets a slot of its own)
                // is repointed to it. Idle-gated; the VFS shows the change on the next tick.
                crate::selection::repoint_to_better_copies(
                    &store,
                    &app.read_activity,
                    Duration::from_secs(config.upgrade.idle_secs),
                    &repair_manager.hidden_torrent_ids().await,
                    &current_data,
                    crate::selection::RepointRules {
                        max_resolution: config.acquisition.prefs.max_resolution.height(),
                        allow_resolution_downgrade: config.allow_resolution_downgrade,
                    },
                )
                .await;

                // Daily: reap dangling account-mirror owned records whose hash has left the provider
                // listing entirely. Without this, a `Verified` empty-provenance mirror record for a
                // deleted/lapsed torrent is never cleaned (dedup/observe/Trigger-A/B all ignore an
                // absent mirror record), so the `owned` table grows unbounded over the deployment's
                // lifetime — slowing every O(all_owned) hot path (same rationale as the blacklist /
                // upgrade-cursor prunes above). Store-only cleanup: the torrent is already gone, so
                // there is nothing to delete and no active stream to interrupt.
                if is_prune_tick {
                    let present: std::collections::HashSet<String> = torrents
                        .iter()
                        .map(|t| t.hash.to_ascii_lowercase())
                        .collect();
                    let reaped =
                        reap_absent_mirror_owned(&store, &present, torrents.is_empty()).await;
                    if reaped > 0 {
                        info!(
                            "Reaped {reaped} account-mirror owned record(s) whose hash left the provider"
                        );
                    }
                    // A mirror record written while TMDB was failing has an empty IMDB id that
                    // nothing else ever fills (the mirror skips a hash it already owns).
                    backfill_missing_imdb_ids(&store, &tmdb_client, &present).await;
                }

                let current_ids: std::collections::HashSet<&str> =
                    deduped_torrents.iter().map(|t| t.id.as_str()).collect();
                // Collect stale IDs before retain so we can clean up redb
                let stale_ids = stale_ids(&seen_torrents, &current_ids);
                seen_torrents.retain(|id, _| current_ids.contains(id.as_str()));
                tmdb_retry.retain(|id, _| current_ids.contains(id.as_str()));
                // Prune health_status entries for torrents that no longer exist
                repair_manager.prune_health_status(&current_ids).await;
                // Remove stale entries from redb to prevent them from reloading on restart
                if !stale_ids.is_empty() {
                    info!("Removing {} stale entries from database", stale_ids.len());
                    if let Err(e) = store.remove_matches(stale_ids).await {
                        error!("Failed to remove stale entries from database: {}", e);
                    }
                }
                info!("VFS update complete.");
            }
            Err(e) => error!("Failed to get torrents: {}", e),
        }

        if !sleep_until_next_scan(interval_secs, &mut shutdown).await {
            return;
        }
    }
}

/// Deduplicate torrents by hash, keeping the first-seen `downloaded` entry per hash.
/// The debrid API returns torrents newest-first, so "first seen" is the newest. Torrents
/// that are not `downloaded`, or that have no hash, are always kept (they can't be matched
/// as duplicates yet). Returns the kept torrents (in original order) plus the ids of the
/// duplicates that should be deleted.
fn dedup_torrents_by_hash(torrents: &[Torrent]) -> (Vec<&Torrent>, Vec<String>) {
    let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut seen_hashes: HashMap<&str, usize> = HashMap::new();
    let mut deduped: Vec<&Torrent> = Vec::new();
    let mut duplicate_ids: Vec<String> = Vec::new();
    for torrent in torrents {
        // The SAME physical torrent can appear twice in ONE listing — a page-boundary repeat when a
        // paginated, newest-first listing races a concurrent add (the add shifts every entry up one,
        // so the boundary item reappears at the next offset). A provider id is unique per torrent, so
        // a repeated (non-empty) id is always such an artefact, never two distinct torrents. Skip the
        // repeat entirely: it must be neither kept twice NOR marked for deletion — deleting it would
        // remove the live torrent we already kept (data loss), and double-marking a genuine duplicate
        // would spawn a redundant (error-logging on TorBox) second delete. The opposite race, a
        // concurrent delete shifting entries back so one is never returned, is handled in the
        // provider clients: a multi-page listing that overlapped one of our own deletes is re-listed
        // (`provider::DeleteTracker`).
        if !torrent.id.is_empty() && !seen_ids.insert(torrent.id.as_str()) {
            continue;
        }
        // Keep (never dedup) anything that isn't a valid removal candidate: a not-yet-downloaded
        // torrent, a hashless one, OR one with an EMPTY id. An empty id can't be deleted
        // (`delete_torrent("")` targets nothing), so a same-hash empty-id pair must both be kept
        // rather than emitting `""` into the removal set — matching the hashless treatment.
        if torrent.status != "downloaded" || torrent.hash.is_empty() || torrent.id.is_empty() {
            deduped.push(torrent);
            continue;
        }
        if let Some(&existing_idx) = seen_hashes.get(torrent.hash.as_str()) {
            let kept = &deduped[existing_idx];
            // Planning only: the deletion itself is idle-gated by the caller and may be deferred.
            debug!(
                "duplicate hash {}: keeping torrent {} ({}), {} ({}) marked for deletion",
                torrent.hash, kept.id, kept.filename, torrent.id, torrent.filename
            );
            duplicate_ids.push(torrent.id.clone());
        } else {
            seen_hashes.insert(torrent.hash.as_str(), deduped.len());
            deduped.push(torrent);
        }
    }
    (deduped, duplicate_ids)
}

/// Compute the ids present in `seen` that are no longer in `current_ids` (stale entries to
/// prune from the in-memory map and the persisted cache).
fn stale_ids<V>(
    seen: &HashMap<String, V>,
    current_ids: &std::collections::HashSet<&str>,
) -> Vec<String> {
    seen.keys()
        .filter(|id| !current_ids.contains(id.as_str()))
        .cloned()
        .collect()
}

/// Flush a batch of pending DB writes via the Store. Clears `pending_writes`.
async fn flush_db_writes(
    store: &Store,
    pending_writes: &mut Vec<(String, crate::rd_client::TorrentInfo, MediaMetadata)>,
) {
    if pending_writes.is_empty() {
        return;
    }
    let writes = std::mem::take(pending_writes);
    let count = writes.len();
    if let Err(e) = store.put_matches(writes).await {
        error!(
            "Failed to persist {} torrent identifications to database: {}",
            count, e
        );
    }
}

async fn update_vfs(
    vfs: &Arc<RwLock<DebridVfs>>,
    current_data: &[(crate::rd_client::TorrentInfo, MediaMetadata)],
    repair_manager: &Arc<RepairManager>,
    jellyfin_client: &Option<Arc<crate::jellyfin_client::JellyfinClient>>,
    store: &Store,
) {
    let hidden_ids = repair_manager.hidden_torrent_ids().await;
    let filtered: Vec<_> = current_data
        .iter()
        .filter(|(torrent_info, _)| !hidden_ids.contains(&torrent_info.id))
        .map(|(torrent_info, metadata)| (torrent_info.clone(), metadata.clone()))
        .collect();
    // SP3: resolve the live-selection map so build() shows the managed release per slot.
    let selection: crate::vfs::SelectionMap = store.all_selection().await.into_iter().collect();
    // Build the VFS off the async runtime: `build` is CPU-bound (groups + sorts + regex-matches every
    // file across the whole library), and the scan loop shares its runtime with the WebDAV server, so
    // doing it inline would stall byte-serving for large libraries. (The lock is also not held during
    // the build — only the cheap diff+swap below is.) A panic in the build task is logged and this
    // update is skipped, keeping the previous VFS rather than tearing down the scan.
    let new_vfs =
        match tokio::task::spawn_blocking(move || DebridVfs::build(filtered, &selection)).await {
            Ok(v) => v,
            Err(e) => {
                error!("VFS build task failed: {e:?}; keeping previous VFS for this tick");
                return;
            }
        };
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

// ── Trakt → wanted sync (SP2) ────────────────────────────────────────────────

/// Refresh a token this many seconds before it expires, so a sync never races the expiry. It must
/// stay well below Trakt's access-token lifetime (24 h since March 2025): a buffer as long as the
/// lifetime made every sync refresh, rotating the single-use refresh token ~96 times a day, and a
/// rotation lost to a failed write or the shutdown drain leaves the account needing re-enrolment.
/// A short buffer is safe at any `TRAKT_SYNC_INTERVAL_SECS`: the check runs before every sync's
/// reads, and the refresh token outlives its access token, so a token that expires between ticks
/// is simply refreshed on the next one.
const REFRESH_BUFFER_SECS: u64 = 3_600;

/// Combine a user's Trakt reads into their materialised wanted-set rows. PURE and deterministic:
/// it aggregates per `tmdb_id` across `watchlist` (sets `sources.watchlist`) and `in_progress`
/// (sets `sources.in_progress`) — the `media_type` comes from the item — and is keyed by a
/// `BTreeMap` so the output is sorted by `tmdb_id`. Per title: a movie's `watched_state` reflects
/// `watched.movies`; a show's reflects the matching `WatchedShow.watched_episodes` (else empty)
/// and `show_status` is taken from the map (`None` when absent). `media_type` and the
/// `WatchedState` variant always agree.
/// `catchup_cutoff`: a Unix timestamp (seconds) bounding the **catch-up** source — a show you've
/// watched is auto-wanted (to catch up on episodes aired since) only if you last watched it at/after
/// this instant. `None` = all-time (every watched show qualifies the lookback gate).
/// `catchup_behind`: the precise "behind?" gate — the set of show `tmdb_id`s Trakt reports as having
/// ≥1 unwatched aired episode. `Some(set)` ⇒ a within-lookback watched show is caught up (and so NOT
/// wanted) unless it's in the set, so fully-watched shows no longer bloat the wanted-set. `None` ⇒
/// fall back to lookback-only (the pure path / when progress is unavailable). Either way the
/// reconciler still filters which episodes to acquire with TMDB air dates.
pub(crate) fn build_wanted(
    user: &str,
    watchlist: &[crate::trakt_client::TraktItem],
    in_progress: &[crate::trakt_client::TraktItem],
    watched: &crate::trakt_client::WatchedData,
    show_status: &std::collections::HashMap<u64, crate::tmdb_client::ShowStatus>,
    catchup_cutoff: Option<i64>,
    catchup_behind: Option<&std::collections::HashSet<u64>>,
) -> Vec<crate::store::WantedRecord> {
    use crate::store::{WantedRecord, WantedSources, WatchedState};
    use crate::vfs::MediaType;

    /// Per-tmdb_id aggregation of which sources want a title.
    struct Agg {
        media_type: MediaType,
        watchlist: bool,
        in_progress: bool,
        /// First non-empty IMDB id seen for this title across its source rows.
        imdb_id: Option<String>,
    }
    // Keyed by (media_type, tmdb_id): TMDB movie and TV id-spaces are independent, so a movie and a
    // show that share a numeric id must NOT be merged into one record. BTreeMap keeps the output
    // deterministically sorted (movies before shows, then by tmdb_id).
    let mut agg: std::collections::BTreeMap<(MediaType, u64), Agg> =
        std::collections::BTreeMap::new();
    for item in watchlist {
        let a = agg
            .entry((item.media_type.clone(), item.tmdb_id))
            .or_insert_with(|| Agg {
                media_type: item.media_type.clone(),
                watchlist: false,
                in_progress: false,
                imdb_id: None,
            });
        a.watchlist = true;
        if a.imdb_id.is_none() {
            a.imdb_id = item.imdb_id.clone();
        }
    }
    for item in in_progress {
        let a = agg
            .entry((item.media_type.clone(), item.tmdb_id))
            .or_insert_with(|| Agg {
                media_type: item.media_type.clone(),
                watchlist: false,
                in_progress: false,
                imdb_id: None,
            });
        a.in_progress = true;
        if a.imdb_id.is_none() {
            a.imdb_id = item.imdb_id.clone();
        }
    }
    // Catch-up source: a show you've watched (within the lookback window) is treated as in-progress
    // so the reconciler acquires any episodes aired since you last watched. Movies are not caught up
    // (a watched movie isn't "behind"). The "behind?" / finished gate is applied later in the
    // reconciler with TMDB air dates; here we only widen the wanted-set.
    for show in &watched.shows {
        let within_lookback = match catchup_cutoff {
            None => true,
            Some(cutoff) => show.last_watched_at.is_some_and(|ts| ts >= cutoff),
        };
        if !within_lookback {
            continue;
        }
        // Precise "behind?" gate: when a behind-set is supplied, a within-lookback watched show is
        // caught up to acquire only if it has ≥1 unwatched aired episode. Without it (pure path /
        // progress unavailable) fall back to lookback-only.
        let behind = catchup_behind.is_none_or(|b| b.contains(&show.tmdb_id));
        if !behind {
            continue;
        }
        let a = agg
            .entry((MediaType::Show, show.tmdb_id))
            .or_insert_with(|| Agg {
                media_type: MediaType::Show,
                watchlist: false,
                in_progress: false,
                imdb_id: None,
            });
        // Carry Trakt's IMDB id so a catch-up-ONLY show (not watchlisted/in-progress, so no
        // TraktItem supplied one) is still scrapable — TMDB's IMDB mapping is incomplete for some
        // titles (e.g. TVDB-only shows), and without this the show resolves to no IMDB id and is
        // silently skipped by the acquire path.
        if a.imdb_id.is_none() {
            a.imdb_id = show.imdb_id.clone();
        }
        // A watchlisted show already wants its whole run, so catch-up is redundant there — only
        // mark shows that aren't already on the watchlist.
        if !a.watchlist {
            a.in_progress = true;
        }
    }

    // Index the watched shows by tmdb_id once so the per-entry lookup below is O(1), not a linear
    // scan per show (which made the final map O(shows²)).
    let watched_by_id: std::collections::HashMap<u64, &_> =
        watched.shows.iter().map(|s| (s.tmdb_id, s)).collect();
    // Same for watched movies — a `Vec::contains` per aggregated movie was O(movies × watched_movies);
    // index into a set once so the membership test below is O(1).
    let watched_movie_ids: std::collections::HashSet<u64> =
        watched.movies.iter().copied().collect();

    agg.into_iter()
        .map(|((_mt, tmdb_id), a)| {
            let (watched_state, status) = match a.media_type {
                MediaType::Movie => (
                    WatchedState::Movie {
                        watched: watched_movie_ids.contains(&tmdb_id),
                    },
                    None,
                ),
                MediaType::Show => {
                    let watched_episodes = watched_by_id
                        .get(&tmdb_id)
                        .map(|s| s.watched_episodes.clone())
                        .unwrap_or_default();
                    (
                        WatchedState::Show { watched_episodes },
                        show_status.get(&tmdb_id).copied(),
                    )
                }
            };
            WantedRecord {
                user: user.to_string(),
                tmdb_id,
                media_type: a.media_type,
                sources: WantedSources {
                    watchlist: a.watchlist,
                    in_progress: a.in_progress,
                },
                watched_state,
                show_status: status,
                imdb_id: a.imdb_id,
            }
        })
        .collect()
}

/// The subset of caught-up, owned shows that are **Ended** (`ShowStatus::Ended`) — the finish-cleanup
/// candidates. An unknown/absent status is treated conservatively as not-Ended (never finish-removed
/// on a TMDB hiccup), as is any Returning/in-production show (it may still get more episodes). PURE.
fn finished_ended_candidates(
    caught_up_owned: &std::collections::HashSet<u64>,
    statuses: &std::collections::HashMap<u64, crate::tmdb_client::ShowStatus>,
) -> std::collections::HashSet<u64> {
    caught_up_owned
        .iter()
        .copied()
        .filter(|id| statuses.get(id) == Some(&crate::tmdb_client::ShowStatus::Ended))
        .collect()
}

/// The shows to fold into the wanted-set: always the `behind` (catch-up) shows; plus the
/// `finished_ended` owned shows ONLY when finish-removal is enabled (otherwise they're preview-only
/// and must NOT be folded, so nothing is removed). PURE.
fn shows_to_fold(
    behind: &std::collections::HashSet<u64>,
    finished_ended: &std::collections::HashSet<u64>,
    remove_finished_shows: bool,
) -> std::collections::HashSet<u64> {
    let mut fold = behind.clone();
    if remove_finished_shows {
        fold.extend(finished_ended.iter().copied());
    }
    fold
}

/// Outcome of one `sync_trakt` pass: how many accounts were enrolled and how many synced without
/// error. Consumed by the scheduler to decide whether the freshly-materialised `wanted` set is
/// trustworthy enough to drive removals this cycle (see `removals_trustworthy`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub enrolled: usize,
    pub succeeded: usize,
}

impl SyncSummary {
    /// Whether Trigger-A/B removals may fire against this cycle's `wanted` set. Removal decisions are
    /// cross-user (a title is removed only when NO user wants it), so an untrustworthy `wanted` set can
    /// delete engine-acquired content that a successful sync would re-confirm. The set is trustworthy
    /// iff there are no enrolled accounts (an empty `wanted` set is then authoritative — nobody to want
    /// anything) OR at least one account synced successfully this cycle. The dangerous case it guards:
    /// the first tick after the v4→v5 migration CLEARS `wanted` while every account's sync fails (Trakt
    /// unreachable), leaving an empty set that would otherwise Trigger-B-remove owned watchlist content.
    pub fn removals_trustworthy(&self) -> bool {
        self.enrolled == 0 || self.succeeded > 0
    }
}

/// For every enrolled Trakt user, refresh near-expiry tokens, pull their Trakt reads + per-show
/// TMDB status, and rewrite their `wanted` rows. A user whose sync fails is `warn!`ed and flagged
/// for re-enrolment (`needs_reenrolment = true`); because `sync_trakt_user` performs all of its
/// `wanted` writes only after every fetch has succeeded, a failure leaves that user's existing
/// `wanted` rows untouched.
pub async fn sync_trakt(
    trakt: &std::sync::Arc<dyn crate::trakt_client::TraktClient>,
    tmdb: &crate::tmdb_client::TmdbClient,
    store: &crate::store::Store,
    catchup_lookback_secs: Option<u64>,
    remove_finished_shows: bool,
) -> SyncSummary {
    // The owned SHOW library is one shared household library, identical for every enrolled user, so
    // read it ONCE per sync cycle rather than re-scanning `all_owned()` inside each `sync_trakt_user`
    // (the catch-up finish-cleanup gate only needs the set of owned show tmdb ids). A view that is at
    // most one cycle stale is fine — finish-cleanup self-corrects next sync.
    let owned_show_ids: std::collections::HashSet<u64> = store
        .all_owned()
        .await
        .into_iter()
        .filter(|(_, r)| matches!(r.request.kind, MediaKind::Series))
        .map(|(_, r)| r.request.tmdb_id)
        .collect();

    // Read the whole `wanted` table ONCE and bucket by user, rather than re-scanning it inside every
    // `sync_trakt_user` (an O(users × total_wanted_rows) N+1 — mirrors the `owned_show_ids` hoist
    // above). Only `sync_trakt` writes `wanted`, and it runs sequentially, so a per-user prune set
    // read here is identical to reading it at the user's turn.
    let mut existing_wanted_by_user: std::collections::HashMap<
        String,
        Vec<(crate::vfs::MediaType, u64)>,
    > = std::collections::HashMap::new();
    for r in store.all_wanted().await {
        existing_wanted_by_user
            .entry(r.user.clone())
            .or_default()
            .push((r.media_type.clone(), r.tmdb_id));
    }

    let mut summary = SyncSummary::default();
    for (slug, tokens) in store.all_trakt_tokens().await {
        summary.enrolled += 1;
        let outcome = sync_trakt_user(
            trakt,
            tmdb,
            store,
            &slug,
            tokens.clone(),
            catchup_lookback_secs,
            remove_finished_shows,
            &owned_show_ids,
            existing_wanted_by_user
                .get(&slug)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
        )
        .await;
        match outcome {
            // Only a genuine materialisation counts toward `succeeded` (→ `removals_trustworthy`).
            Ok(UserSync::Synced) => summary.succeeded += 1,
            // A concurrent token rotation won the refresh CAS, or the account was removed mid-cycle:
            // nothing was materialised, so it is neither a failure (don't flag) nor a confirmation
            // (don't count) — leaving an unconfirmed `wanted` set out of the trustworthy tally.
            Ok(UserSync::Skipped) => {}
            Err(e) => {
                warn!(
                    "Trakt sync failed for {}: {}; flagging account for re-enrolment",
                    slug, e
                );
                // A successful refresh inside sync_trakt_user persists a fresh (single-use) token before a
                // later read can fail; re-read so we don't clobber it with the stale pre-refresh snapshot.
                // If the account is GONE (the operator clicked "Remove" on the enrolment page since this
                // pass snapshotted the token list), do NOT resurrect it from the stale snapshot — skip.
                let Some(current) = store.get_trakt_tokens(slug.clone()).await else {
                    // The account was removed since this pass snapshotted the token list — don't
                    // resurrect it from the stale snapshot.
                    continue;
                };
                // Flip ONLY the flag via the same refresh-keyed compare-and-swap the refresh paths use
                // (`update_trakt_tokens_if_refresh_matches`). A blind `put_trakt_tokens` here would
                // resurrect an account removed — or clobber a token rotated by a concurrent `/refresh` —
                // in the get→write window; the CAS makes both a no-op. Setting the flag never rotates
                // `refresh`, so keying on `current.refresh` is safe.
                if let Err(pe) = store
                    .update_trakt_tokens_if_refresh_matches(
                        slug.clone(),
                        current.refresh.clone(),
                        |c| {
                            c.needs_reenrolment = true;
                        },
                    )
                    .await
                {
                    error!("Failed to persist re-enrolment flag for account: {}", pe);
                }
            }
        }
    }
    summary
}

/// Outcome of syncing one Trakt user (distinct from an `Err`, which flags the account).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserSync {
    /// The user's `wanted` set was materialised (pruned + written) this cycle — a genuine
    /// confirmation, so `removals_trustworthy` may rely on it.
    Synced,
    /// Nothing was materialised this cycle: a concurrent token rotation won the refresh CAS, or the
    /// account was removed mid-cycle. A benign no-op — it must NOT count as a confirmed sync, or an
    /// unconfirmed (possibly stale/empty) `wanted` set could be treated as trustworthy for removals.
    Skipped,
}

/// Sync one user. Returns `Err` (so `sync_trakt` flags the account) on a token-refresh or
/// Trakt-read failure; a TMDB hiccup is tolerated (it is not a de-auth). All `wanted` writes
/// happen only after every Trakt fetch has succeeded, so an early error leaves `wanted` intact.
/// Returns `Ok(UserSync::Synced)` when the `wanted` set was materialised, or `Ok(UserSync::Skipped)`
/// when a concurrent token rotation / mid-cycle removal made it a no-op (not counted as a sync).
///
/// NOTE: store-write errors (`replace_user_wanted_if_enrolled`/the token CAS) also propagate as
/// `Err` and therefore trigger `needs_reenrolment`; this is intentional and self-healing — the
/// flag is cleared on the next successful sync.
#[allow(clippy::too_many_arguments)] // shared handles + per-user state + per-cycle inputs; one caller
async fn sync_trakt_user(
    trakt: &std::sync::Arc<dyn crate::trakt_client::TraktClient>,
    tmdb: &crate::tmdb_client::TmdbClient,
    store: &crate::store::Store,
    slug: &str,
    mut tokens: crate::store::TraktTokens,
    catchup_lookback_secs: Option<u64>,
    remove_finished_shows: bool,
    owned_show_ids: &std::collections::HashSet<u64>,
    existing_wanted: &[(crate::vfs::MediaType, u64)],
) -> Result<UserSync, crate::error::AppError> {
    use crate::store::TraktTokens;
    use crate::vfs::MediaType;

    // Refresh if at/near expiry, persisting the fresh tokens before using them.
    let now = crate::now_unix_secs();
    if tokens.expires_at <= now + REFRESH_BUFFER_SECS {
        let r = match trakt.refresh(&tokens.refresh).await {
            Ok(r) => r,
            Err(e) => {
                // Mirror `enrolment::refresh_account`: if a concurrent refresh (another `/refresh`
                // POST, or an overlapping sync) already rotated the single-use refresh token since
                // we snapshotted it, Trakt invalidated OUR token on that other success — so this
                // failure is a false alarm, not a de-auth. Skip this cycle (the rotated token is used
                // next time) rather than propagating, which would wrongly flag the healthy account
                // for re-enrolment. A genuine refresh failure (token unchanged) still propagates, and
                // a later READ failure still flags — the guard is scoped to the refresh cause only.
                if let Some(stored) = store.get_trakt_tokens(slug.to_string()).await {
                    if stored.refresh != tokens.refresh {
                        info!(
                            "trakt: refresh for '{}' failed on a since-rotated token (concurrent refresh) — skipping this cycle",
                            slug
                        );
                        return Ok(UserSync::Skipped);
                    }
                }
                return Err(e);
            }
        };
        // Match `enrolment::refresh_account`: keep the existing refresh token if Trakt returns an
        // empty one (don't blank a still-valid token and force re-enrolment), and use
        // `saturating_add` for the expiry so a pathological `created_at + expires_in` can't overflow.
        let refresh = if r.refresh_token.is_empty() {
            tokens.refresh.clone()
        } else {
            r.refresh_token
        };
        let new_tokens = TraktTokens {
            access: r.access_token,
            refresh,
            // Share `enrolment::token_expires_at` so a refresh response that omits `created_at`
            // (parsed to `0`) falls back to the wall clock instead of stamping expiry
            // `expires_in` after the 1970 epoch — which would read as permanently expired and force a
            // single-use-token refresh on every sync tick (refresh churn against the rate limiter).
            expires_at: crate::enrolment::token_expires_at(r.created_at, r.expires_in),
            username: tokens.username.clone(),
            needs_reenrolment: false,
        };
        // Persist via the store-level compare-and-swap keyed on the refresh token we just used, so the
        // read → compare → write is atomic (mirrors `enrolment::refresh_account`). If the account was
        // removed during the refresh window, or a concurrent refresh (a `/refresh` POST) already
        // rotated the single-use token, the CAS skips the write — do NOT resurrect/clobber it; skip the
        // rest of this user's sync (the stored token is used next cycle).
        let nt = new_tokens.clone();
        let applied = store
            .update_trakt_tokens_if_refresh_matches(
                slug.to_string(),
                tokens.refresh.clone(),
                move |current| {
                    *current = nt;
                },
            )
            .await?;
        if !applied {
            return Ok(UserSync::Skipped);
        }
        tokens = new_tokens;
    }

    // Pull reads. Any error propagates → the account is flagged by `sync_trakt`.
    let watchlist = trakt.watchlist(&tokens.access).await?;
    let in_progress = trakt.in_progress(&tokens.access).await?;
    let watched = trakt.watched(&tokens.access).await?;

    // Shows already wanted via watchlist/in-progress are wanted regardless of catch-up progress.
    let wl_ip_shows: std::collections::HashSet<u64> = watchlist
        .iter()
        .chain(in_progress.iter())
        .filter(|i| i.media_type == MediaType::Show)
        .map(|i| i.tmdb_id)
        .collect();

    // Precise catch-up gate: for each watched show within the lookback window that isn't already
    // wanted, ask Trakt whether the user has an unwatched *aired* episode (`show_progress`). Only
    // genuinely-behind shows enter `behind` (and so the catch-up wanted-set) — a fully-watched show
    // no longer bloats the set or drives a fruitless reconcile each cycle. On a missing Trakt id or a
    // progress error we conservatively include the show (degrade to the old lookback-only behaviour
    // rather than silently dropping a possibly-behind title; it self-corrects next sync).
    // `owned_show_ids` (the shared household show library, used to spot finished-ended owned shows
    // that are finish-cleanup candidates) is computed ONCE per sync cycle by the caller and passed
    // in, rather than re-scanning `all_owned()` for every enrolled user.

    let catchup_cutoff: Option<i64> = catchup_lookback_secs.map(|s| now.saturating_sub(s) as i64);
    let mut behind: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // Owned, caught-up (per Trakt progress) shows — finish-cleanup candidates IF they're Ended.
    let mut caught_up_owned: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for show in &watched.shows {
        let within = match catchup_cutoff {
            None => true,
            Some(c) => show.last_watched_at.is_some_and(|ts| ts >= c),
        };
        if !within || wl_ip_shows.contains(&show.tmdb_id) {
            continue;
        }
        let is_behind = match show.trakt_id {
            Some(tid) => match trakt.show_progress(&tokens.access, tid).await {
                Ok(p) => {
                    debug!(
                        "trakt: {} catch-up tmdb {} progress {}/{} behind={}",
                        slug, show.tmdb_id, p.completed, p.aired, p.behind
                    );
                    p.behind
                }
                Err(e) => {
                    warn!(
                        "trakt: {} show_progress(tmdb {}, trakt {}) failed: {}; treating as behind",
                        slug, show.tmdb_id, tid, e
                    );
                    true
                }
            },
            None => {
                debug!(
                    "trakt: {} catch-up tmdb {} has no Trakt id; treating as behind",
                    slug, show.tmdb_id
                );
                true
            }
        };
        if is_behind {
            behind.insert(show.tmdb_id);
        } else if owned_show_ids.contains(&show.tmdb_id) {
            // Caught up (all aired watched) AND owned → a finish-cleanup candidate if it's Ended.
            caught_up_owned.insert(show.tmdb_id);
        }
    }

    // Per-show TMDB status for shows that will be wanted (watchlist/in-progress + behind catch-up)
    // PLUS the caught-up-owned shows (so we can tell which are Ended → finish-cleanup candidates). A
    // TMDB failure must NOT fail the whole sync — skip that show (→ show_status None, treated
    // conservatively as not-Ended, so it's never finish-removed on an unknown status).
    let mut show_ids: std::collections::HashSet<u64> = wl_ip_shows.clone();
    show_ids.extend(behind.iter().copied());
    show_ids.extend(caught_up_owned.iter().copied());
    let mut statuses: std::collections::HashMap<u64, crate::tmdb_client::ShowStatus> =
        std::collections::HashMap::new();
    for tmdb_id in show_ids {
        match tmdb.show_status(tmdb_id).await {
            Ok(s) => {
                statuses.insert(tmdb_id, s);
            }
            Err(e) => warn!(
                "TMDB show_status({}) failed for {}: {}; treating status as unknown",
                tmdb_id, slug, e
            ),
        }
    }

    // Finish-cleanup candidates = caught-up (per Trakt's OWN progress signal) + owned + ENDED. These
    // aren't "behind" for acquisition, but folding them into the wanted-set lets the reconciler's
    // Trigger-A finish removal fire. Trigger A / the Show acquire-guard are decided by `user_finished`
    // over the TMDB aired set, so the fold removes the show AND the acquire path acquires nothing ONLY
    // when TMDB and Trakt agree on the aired episodes. If TMDB lists an aired episode that Trakt's
    // watched set lacks (specials/numbering drift), the TMDB-authoritative reconcile instead treats
    // the show as not-fully-watched: it is NOT removed and catch-up acquires that genuinely-unwatched
    // episode — internally consistent, not a phantom acquire. Gated on `remove_finished_shows`: when
    // OFF we only LOG the candidates (a preview) and do NOT fold them, so nothing is removed.
    let finished_ended = finished_ended_candidates(&caught_up_owned, &statuses);
    if !finished_ended.is_empty() {
        info!(
            "trakt: {} — {} finished+ended owned show(s) {} ({})",
            slug,
            finished_ended.len(),
            if remove_finished_shows {
                "to remove"
            } else {
                "would be removed"
            },
            if remove_finished_shows {
                "REMOVING"
            } else {
                "preview — set REMOVE_FINISHED_SHOWS=true to remove"
            }
        );
        debug!(
            "trakt: {} finished+ended owned tmdb ids: {:?}",
            slug, finished_ended
        );
    }

    // Shows to fold into the wanted-set: always the behind (catch-up) shows; plus the finished+ended
    // owned shows ONLY when removal is enabled.
    let fold = shows_to_fold(&behind, &finished_ended, remove_finished_shows);

    // Build the user's new wanted-set and write it: prune rows no longer wanted, then upsert.
    let new = build_wanted(
        slug,
        &watchlist,
        &in_progress,
        &watched,
        &statuses,
        catchup_cutoff,
        Some(&fold),
    );
    debug!(
        "trakt: {} — watchlist={} in_progress={} → {} wanted titles; in_progress=[{}]; wanted_shows={:?}",
        slug,
        watchlist.len(),
        in_progress.len(),
        new.len(),
        in_progress
            .iter()
            .map(|i| format!("{:?}:{}", i.media_type, i.tmdb_id))
            .collect::<Vec<_>>()
            .join(", "),
        new.iter()
            .filter(|r| r.media_type == MediaType::Show)
            .map(|r| r.tmdb_id)
            .collect::<Vec<_>>()
    );
    // Prune rows no longer wanted, keyed by (media_type, tmdb_id) so a movie and a show that share
    // a numeric id are tracked independently. `existing_wanted` is this user's slice of the
    // once-per-cycle `all_wanted` snapshot taken by `sync_trakt` (no per-user re-scan).
    let new_keys: std::collections::HashSet<(MediaType, u64)> = new
        .iter()
        .map(|r| (r.media_type.clone(), r.tmdb_id))
        .collect();
    let stale: Vec<(MediaType, u64)> = existing_wanted
        .iter()
        .filter(|k| !new_keys.contains(k))
        .cloned()
        .collect();
    // An account removed mid-cycle (the operator clicked "Remove" on /trakt/accounts since
    // `sync_trakt` snapshotted the token list) has already had its tokens AND its `wanted` rows
    // deleted (`enrolment::remove_account`). Our in-memory access token is still valid — removing
    // locally does not revoke it at Trakt — and no refresh may have run this cycle, so the refresh
    // CAS never fired. Writing anyway would RESURRECT the pruned rows as permanent orphans: the only
    // pruner of `wanted` is this function, which never processes a tokenless slug again, so each
    // resurrected row pins its title as wanted forever and blocks Trigger-B cleanup — exactly the
    // leak `remove_account` exists to prevent. The store checks for the token row and writes the
    // whole set in ONE transaction, so a removal can't land between the check and the writes.
    if !store
        .replace_user_wanted_if_enrolled(slug.to_string(), stale, new)
        .await?
    {
        debug!("trakt: {slug} — account removed mid-cycle; skipping wanted writes");
        return Ok(UserSync::Skipped);
    }

    // Clear a stale re-enrolment flag now that this sync has succeeded, via the same refresh-keyed
    // compare-and-swap the refresh paths use (`update_trakt_tokens_if_refresh_matches`). This branch
    // runs only when NO refresh occurred this cycle (a refresh already clears the flag and leaves
    // `tokens.needs_reenrolment == false`), so `tokens` is the pre-sync snapshot; a concurrent
    // `/refresh`/enrolment may have rotated access/refresh during this user's read window. The CAS
    // writes only while the stored refresh still equals the snapshot's, so it can neither clobber a
    // freshly-rotated token nor resurrect a concurrently-removed account (a plain get-then-put
    // would race in the get→write window). Clearing the flag never rotates `refresh`, so keying on
    // `tokens.refresh` is safe.
    if tokens.needs_reenrolment {
        store
            .update_trakt_tokens_if_refresh_matches(slug.to_string(), tokens.refresh.clone(), |c| {
                c.needs_reenrolment = false;
            })
            .await?;
    }

    Ok(UserSync::Synced)
}

// ── reconcile_wanted (SP2) ───────────────────────────────────────────────────

/// Map the scraper's `MediaKind` to the VFS `MediaType` (explicit Movie↔Movie / Series↔Show).
fn media_type_of(kind: MediaKind) -> MediaType {
    match kind {
        MediaKind::Movie => MediaType::Movie,
        MediaKind::Series => MediaType::Show,
    }
}

/// Inverse of [`media_type_of`]: the VFS `MediaType` → scraper `MediaKind`.
fn kind_of(media_type: &MediaType) -> MediaKind {
    match media_type {
        MediaType::Movie => MediaKind::Movie,
        MediaType::Show => MediaKind::Series,
    }
}

/// The quality of an owned torrent, derived from its representative video file's name — the largest
/// one the VFS serves ([`crate::vfs::emittable_video_files`]: selected, with a link where Real-Debrid
/// pairs them) —
/// for an account-mirror copy, which has no scraped release to snapshot, and for the backfill of
/// older records that have none ([`backfill_owned_quality`]) — with every axis the file name leaves
/// untagged filled from the TORRENT name (an axis the file name tags wins). `None` only when there
/// is no such video file (a lapsed TorBox entry lists none: it stays unknown-lowest, so a playable copy
/// is never removed in its favour). Every axis neither name tags is recorded as 0 — UNKNOWN, the
/// lowest at its resolution (see [`crate::release::is_unknown_quality`]), so a known cached release
/// may replace it.
/// A file ABOVE the resolution ceiling keeps its real resolution ([`QualitySummary::of_owned`]):
/// recording it as `None` would make it read as unknown-lowest, whereas it must stay under the
/// corrective-downgrade rules (`ALLOW_RESOLUTION_DOWNGRADE`).
///
/// A recorded summary is flagged `unverified` iff the representative file isn't MKV/MP4 — a
/// container the probe can never read (e.g. `.avi`), so the upgrade pass may replace it with an
/// equal-quality checked copy. A mirrored MKV/MP4 was never probed either, but is assumed fine:
/// flagging every mirror copy would churn the whole library through the upgrade pass.
///
/// [`QualitySummary::of_owned`]: crate::release::QualitySummary::of_owned
fn mirror_quality(
    info: &crate::rd_client::TorrentInfo,
    prefs: &crate::config::QualityPrefs,
) -> Option<crate::release::QualitySummary> {
    // The largest file the VFS actually serves: an unselected file (a Real-Debrid multi-version
    // torrent can list a bigger 2160p/REMUX file it never selected) is never shown, so its quality
    // must not stand for the copy.
    let file = crate::vfs::emittable_video_files(info)
        .into_iter()
        .max_by_key(|f| f.bytes)?;
    let name = file
        .path
        .rsplit('/')
        .next()
        .unwrap_or(&file.path)
        .to_string();
    let mut r = crate::release::parse(&crate::release::RawCandidate {
        name: name.clone(),
        description: String::new(),
        info_hash: info.hash.clone(),
        file_idx: None,
        file_name: Some(name),
    });
    // Episode files are often untagged while the torrent name carries the quality ("Show (S02)
    // (1080p) (WebDL)" holding "S02E01.mkv"): fill every axis the file name leaves untagged from the
    // torrent name. An axis the file name tags always wins, and the container stays the file's.
    let torrent_name = if info.filename.is_empty() {
        &info.original_filename
    } else {
        &info.filename
    };
    if !torrent_name.is_empty() {
        use crate::release::{Codec, Source};
        let t = crate::release::parse(&crate::release::RawCandidate {
            name: torrent_name.clone(),
            description: String::new(),
            info_hash: info.hash.clone(),
            file_idx: None,
            file_name: None,
        });
        if r.resolution.is_none() {
            r.resolution = t.resolution;
        }
        if r.source == Source::Other && !matches!(t.source, Source::Cam | Source::Other) {
            r.source = t.source;
        }
        if r.codec == Codec::Other {
            r.codec = t.codec;
        }
        r.hdr |= t.hdr;
    }
    r.cached = true; // present in the provider listing == available
    let mut summary = crate::release::QualitySummary::of_owned(&r, prefs);
    // `r.container` is `Container::from_name` of the file name parsed above.
    summary.unverified = !r.container.is_verifiable();
    Some(summary)
}

/// Re-derive the quality of owned records that have NONE (`quality: None` — account-mirror copies
/// recorded before their quality was captured, or recorded `None` for sitting above the ceiling)
/// from their torrent's representative file ([`mirror_quality`]), for every record whose torrent is
/// in `infos`. Without it such a copy reads as unknown-lowest — and an over-ceiling one would be
/// treated as unknown instead of staying under the corrective-downgrade rules. Only `Verified`
/// records are touched (a `Pending` one gets its quality from `observe`), and only where the store
/// still has no quality ([`Store::set_quality_if_unset`] — atomic, never overwrites). Run by the scan
/// loop's account-mirror pass (so the dedup pass right after it sees the result) and at the start of
/// every upgrade tick (from the persisted identification cache). Returns how many were filled.
pub(crate) async fn backfill_owned_quality(
    store: &Store,
    prefs: &crate::config::QualityPrefs,
    owned: &[(String, OwnedRecord)],
    infos: &[&crate::rd_client::TorrentInfo],
) -> usize {
    let missing: std::collections::HashSet<String> = owned
        .iter()
        .filter(|(_, r)| r.quality.is_none() && r.status == OwnedStatus::Verified)
        .map(|(h, _)| h.to_ascii_lowercase())
        .collect();
    if missing.is_empty() {
        return 0;
    }
    let mut filled = 0usize;
    let mut seen = std::collections::HashSet::new();
    for info in infos {
        let hash = info.hash.to_ascii_lowercase();
        if !missing.contains(&hash) || !seen.insert(hash.clone()) {
            continue;
        }
        let Some(q) = mirror_quality(info, prefs) else {
            continue; // no video file → nothing to derive
        };
        match store.set_quality_if_unset(hash.clone(), q).await {
            Ok(true) => filled += 1,
            Ok(false) => {}
            Err(e) => warn!("quality backfill for {} failed: {}", hash, e),
        }
    }
    if filled > 0 {
        info!(
            "quality: derived the quality of {} owned record(s) from their file names",
            filled
        );
    }
    filled
}

/// Owned-record hashes safe to reap because their torrent has left the provider: a `Verified`,
/// EMPTY-provenance (account-mirror) record whose lowercased hash is absent from `present` (the
/// current provider listing). The torrent is already gone, so reaping only drops a dangling
/// bookkeeping row — `record_mirror_owned` re-creates it if the hash reappears, and the reconciler
/// re-acquires it fresh if a Trakt user still wants it (so a rare wrongful reap on a transient
/// listing glitch self-heals with no lost state). Engine/provenance-bearing records are left to the
/// acquire/reconcile lifecycle. Returns empty when `listing_empty` — a wholesale-empty listing is an
/// untrustworthy reap signal (same guard as `observe`).
fn absent_mirror_records_to_reap(
    owned: &[(String, OwnedRecord)],
    present: &std::collections::HashSet<String>,
    listing_empty: bool,
) -> Vec<String> {
    if listing_empty {
        return Vec::new();
    }
    owned
        .iter()
        .filter(|(hash, rec)| {
            rec.status == OwnedStatus::Verified
                && rec.provenance.entries.is_empty()
                && !present.contains(&hash.to_ascii_lowercase())
        })
        .map(|(hash, _)| hash.clone())
        .collect()
}

/// Reap the dangling account-mirror owned records identified by [`absent_mirror_records_to_reap`],
/// cleaning up each owned record plus its authoritative-id and selection entries (mirrors the
/// blacklisted-phantom reaper in `record_mirror_owned`). Store-only — the torrent is already absent
/// from the provider, so there is nothing to delete and no stream to interrupt (no idle-gate needed).
/// Returns the number of records reaped.
async fn reap_absent_mirror_owned(
    store: &Store,
    present: &std::collections::HashSet<String>,
    listing_empty: bool,
) -> usize {
    let owned = store.all_owned().await;
    let to_reap = absent_mirror_records_to_reap(&owned, present, listing_empty);
    if to_reap.is_empty() {
        return 0;
    }
    // Index selection slots by (lowercased) hash ONCE so per-hash slot cleanup is O(1) rather than an
    // O(reaped × selection-table) nested scan.
    let slots_by_hash = index_selection_slots_by_hash(&store.all_selection().await);
    let mut reaped = 0;
    for h in &to_reap {
        // Only count a record as reaped once its owned row is ACTUALLY gone — log (don't swallow via
        // `let _`) a transient store error so a reported reap count can't overstate what happened.
        // The record survives and is retried on the next daily prune tick.
        match store.remove_owned(h.clone()).await {
            Ok(()) => reaped += 1,
            Err(e) => {
                warn!(
                    "reap: failed to remove absent-mirror owned record {} ({}); will retry next tick",
                    h, e
                );
                continue;
            }
        }
        if let Err(e) = store.remove_authoritative(h.clone()).await {
            warn!("reap: failed to remove authoritative id for {} ({})", h, e);
        }
        if let Some(slots) = slots_by_hash.get(&h.to_ascii_lowercase()) {
            for slot in slots {
                if let Err(e) = store.remove_selection(slot.clone()).await {
                    warn!(
                        "reap: failed to remove dangling selection slot {} for {} ({})",
                        slot, h, e
                    );
                }
            }
        }
    }
    reaped
}

/// Fill the empty IMDB id of every present `Verified` owned record (via TMDB `external_ids`). The
/// account mirror writes an empty id when that lookup fails transiently, and never revisits a hash it
/// already owns, so without this such a copy stays un-upgradeable (the upgrade engine scrapes by IMDB
/// id) and is never re-acquired once it lapses — for as long as the torrent stays in the account.
/// Run on the scan loop's daily prune tick. One lookup per title per pass; a definitive answer
/// (an id, "no IMDB id", or a 4xx such as an unknown TMDB id) is reused for the title's other records,
/// and only an id is written ([`Store::set_imdb_id_if_empty`] — atomic, never overwrites). A
/// TRANSIENT failure (a TMDB outage) ends the pass, so an outage costs one retry budget a day rather
/// than one per record; everything left is retried on the next daily tick. A `Pending` record is
/// `observe`'s, and an absent hash has nothing to upgrade or re-acquire from. Returns how many records
/// were filled.
pub(crate) async fn backfill_missing_imdb_ids(
    store: &Store,
    tmdb: &TmdbClient,
    present: &std::collections::HashSet<String>,
) -> usize {
    let candidates: Vec<(String, MediaType, u64)> = store
        .all_owned()
        .await
        .into_iter()
        .filter(|(h, r)| {
            r.status == OwnedStatus::Verified
                && r.request.imdb_id.is_empty()
                && present.contains(&h.to_ascii_lowercase())
        })
        .map(|(h, r)| (h, media_type_of(r.request.kind), r.request.tmdb_id))
        .collect();
    let mut answers: HashMap<(MediaType, u64), Option<String>> = HashMap::new();
    let mut filled = 0usize;
    for (hash, media_type, tmdb_id) in candidates {
        let key = (media_type.clone(), tmdb_id);
        let imdb = match answers.get(&key) {
            Some(answer) => answer.clone(),
            None => {
                let answer = match tmdb.external_imdb_id(tmdb_id, media_type).await {
                    Ok(answer) => answer,
                    Err(e) if crate::tmdb_client::error_is_permanent(&e) => {
                        debug!("imdb backfill: TMDB has no IMDB id for tmdb {tmdb_id} ({e})");
                        None
                    }
                    Err(e) => {
                        debug!(
                            "imdb backfill: TMDB lookup for tmdb {tmdb_id} failed ({e}); retrying on the next daily tick"
                        );
                        break;
                    }
                };
                answers.insert(key, answer.clone());
                answer
            }
        };
        let Some(imdb) = imdb else {
            continue;
        };
        match store.set_imdb_id_if_empty(hash.clone(), imdb).await {
            Ok(true) => filled += 1,
            Ok(false) => {}
            Err(e) => warn!("imdb backfill for {} failed: {}", hash, e),
        }
    }
    if filled > 0 {
        info!("mirror: filled the missing IMDB id of {filled} owned record(s)");
    }
    filled
}

/// Index selection slots by lowercased hash, so a destructive pass that removes many hashes can look
/// up each hash's slots in O(1) instead of re-scanning the whole selection snapshot per hash.
fn index_selection_slots_by_hash(
    selection: &[(String, crate::store::SelectionEntry)],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for (slot, entry) in selection {
        map.entry(entry.hash.to_ascii_lowercase())
            .or_default()
            .push(slot.clone());
    }
    map
}

/// Record every identified, present provider torrent the engine doesn't already track as an
/// **owned record** (the "account mirror"). This makes the reconciler aware of the user's
/// pre-existing library so it never re-acquires a title that is already present (the duplicate fix)
/// and so a season pack's episodes count as owned (kills `monitor_episodes` churn).
///
/// There is **no protected "manual" class**: a mirrored torrent is recorded with **empty
/// provenance** (no `Manual` entry), so it is owned exactly like an engine-acquired title — eligible
/// for the daily quality-upgrade / consolidation path — while still being kept (empty provenance +
/// no Trakt wanter means neither removal trigger fires). The IMDB id is resolved from TMDB so the
/// upgrade engine (IMDB-keyed) can actually re-scrape it; a per-title cache avoids repeat lookups in
/// a pass. A resolution failure is non-fatal — the record is still written, but with an empty
/// `imdb_id` it is then kept yet NOT upgrade/re-acquire-eligible (both need an IMDB id) until the
/// scan loop's daily [`backfill_missing_imdb_ids`] fills it; TMDB lookup failures are rare/transient
/// and playback still works via on-read repair, so this degrades gracefully rather than failing the
/// mirror.
///
/// Idempotent and non-destructive: a hash the engine already owns (or a previously-recorded mirror
/// hash) is left untouched, so an in-flight `Pending` acquisition or an engine record's
/// provenance/`provides` is never clobbered — the one exception being a record with NO quality,
/// which gets it filled from its file names ([`backfill_owned_quality`]). `provides` for a show is
/// the SE-parsed set of its selected video files (a movie's is empty); `quality` is derived from the
/// representative (largest) video file via [`mirror_quality`] (`None` only when there is no video).
pub(crate) async fn record_mirror_owned(
    store: &Store,
    tmdb: &TmdbClient,
    prefs: &crate::config::QualityPrefs,
    current_data: &[(crate::rd_client::TorrentInfo, MediaMetadata)],
) {
    let now = crate::now_unix_secs();
    // Resolve each title's IMDB id at most once per pass (many torrents can share a tmdb_id).
    let mut imdb_cache: HashMap<(MediaType, u64), String> = HashMap::new();
    // Snapshot the owned-hash set in ONE read transaction up front, rather than a separate
    // `get_owned` per present torrent (N sequential redb round-trips every scan tick — costly for a
    // large library). Each provider torrent has a unique hash, so this pass never inserts a hash it
    // would also need to re-check, making the snapshot safe for the whole pass.
    let all_owned = store.all_owned().await;
    let owned: std::collections::HashSet<String> = all_owned
        .iter()
        .map(|(h, _)| h.to_ascii_lowercase())
        .collect();
    // Hashes the engine has REJECTED (wrong title / bad audio / corrupt / multi-pack / dead). Never
    // re-adopt one as a mirror copy: a torrent that `observe`/upgrade-staging rejected (and
    // blacklisted) but couldn't immediately delete — or that a concurrent scan caught mid-staging —
    // must not be resurrected as a `Verified` empty-provenance record (which removal triggers can't
    // reclaim). Loaded once per tick; hash-scoped so it catches a wrong-title re-identification too.
    let blacklisted: std::collections::HashSet<String> = store.all_blacklisted_hashes().await;
    // Consistency reaper: a blacklisted hash must never be a MIRROR (empty-provenance) owned record.
    // Such a phantom can arise from a narrow race — a concurrent scan adopting a torrent that
    // upgrade-staging is about to reject+blacklist+delete (the blacklisting gates: WrongTitle /
    // MoviePack / probe-Reject) — leaving a Verified empty-provenance record for a now-absent (or
    // rejected) hash that nothing else cleans (dedup/observe ignore absent/mirror records). Reap it
    // (store-only — no provider handle here; a still-present blacklisted torrent simply stays
    // un-adopted via the skip below). Engine records (non-empty provenance) are left alone —
    // `fail_and_reacquire` owns their lifecycle and may deliberately keep one for a delete-retry.
    // (A non-blacklisting transient staging failure, e.g. probe-Defer, self-heals: the still-valid
    // candidate is re-staged and swapped on a later upgrade tick.)
    let phantoms: Vec<String> = all_owned
        .iter()
        .filter(|(h, rec)| {
            rec.provenance.entries.is_empty() && blacklisted.contains(&h.to_ascii_lowercase())
        })
        .map(|(h, _)| h.clone())
        .collect();
    if !phantoms.is_empty() {
        let slots_by_hash = index_selection_slots_by_hash(&store.all_selection().await);
        let mut reaped = 0;
        for h in &phantoms {
            // Count only records whose owned row was ACTUALLY removed; log (don't swallow) a transient
            // store error so the reaped count can't overstate what happened (retried next tick).
            match store.remove_owned(h.clone()).await {
                Ok(()) => reaped += 1,
                Err(e) => {
                    warn!(
                        "mirror: failed to reap phantom owned record {} ({}); will retry",
                        h, e
                    );
                    continue;
                }
            }
            if let Err(e) = store.remove_authoritative(h.clone()).await {
                warn!(
                    "mirror: failed to remove authoritative id for {} ({})",
                    h, e
                );
            }
            if let Some(slots) = slots_by_hash.get(&h.to_ascii_lowercase()) {
                for slot in slots {
                    if let Err(e) = store.remove_selection(slot.clone()).await {
                        warn!(
                            "mirror: failed to remove selection slot {} for {} ({})",
                            slot, h, e
                        );
                    }
                }
            }
        }
        info!(
            "mirror: reaped {} blacklisted phantom owned record(s)",
            reaped
        );
    }
    // Records already owned with NO quality (mirrored before quality capture existed, or recorded
    // `None` for being above the ceiling) get it re-derived from their file names, so the dedup pass
    // that follows this one in the scan tick ranks them by what they really are.
    let infos: Vec<&crate::rd_client::TorrentInfo> = current_data.iter().map(|(i, _)| i).collect();
    backfill_owned_quality(store, prefs, &all_owned, &infos).await;
    let mut recorded = 0usize;
    for (info, metadata) in current_data {
        let Some(tmdb_id) = crate::vfs::tmdb_id_of(metadata) else {
            continue;
        };
        let hash = info.hash.to_ascii_lowercase();
        if hash.is_empty() {
            continue;
        }
        // Engine-owned OR already-mirrored → leave untouched (idempotent; never clobber an
        // in-flight Pending acquisition or an engine record's provenance/provides/quality).
        if owned.contains(&hash) {
            continue;
        }
        // Engine-rejected → never re-adopt (see `blacklisted` above).
        if blacklisted.contains(&hash) {
            continue;
        }
        let media_type = metadata.media_type.clone();
        let kind = kind_of(&media_type);
        let provides: Vec<(u32, u32)> = match kind {
            MediaKind::Series => crate::acquire::episode_files(info)
                .into_iter()
                .map(|(s, e, _)| (s, e))
                .collect(),
            MediaKind::Movie => Vec::new(),
        };
        // Best-effort IMDB resolution (cached per title) so the upgrade engine can re-scrape it.
        // Cache only a DEFINITIVE answer (a real id, or a confirmed "no IMDB id"): a TRANSIENT TMDB
        // failure must NOT poison the cache with "", which would degrade every remaining torrent of
        // this title in the pass to non-upgradeable. On a transient error, use "" for this record
        // only and leave the cache vacant so a later torrent of the same title can retry.
        let imdb_id = match imdb_cache.get(&(media_type.clone(), tmdb_id)) {
            Some(cached) => cached.clone(),
            None => match tmdb.external_imdb_id(tmdb_id, media_type.clone()).await {
                Ok(opt) => {
                    let resolved = opt.unwrap_or_default();
                    imdb_cache.insert((media_type.clone(), tmdb_id), resolved.clone());
                    resolved
                }
                Err(e) => {
                    debug!(
                        "mirror: IMDB resolution for tmdb {} failed transiently ({}); not caching",
                        tmdb_id, e
                    );
                    String::new()
                }
            },
        };
        let rec = OwnedRecord {
            request: AcquireRequest {
                imdb_id,
                tmdb_id,
                kind,
                season: None,
                episode: None,
                // Neither TMDB language is fetched here: the upgrade pass backfills both (only-if-unset)
                // from one TMDB `details` call on the title's next visit.
                original_language: None,
                spoken_languages: None,
                metadata: metadata.clone(),
            },
            // Empty provenance: owned + upgradeable, NOT a protected manual class. No Trakt wanter
            // and no watchlist provenance means neither removal trigger fires, so the user's
            // pre-existing library is kept while still following the normal upgrade path.
            provenance: Provenance { entries: vec![] },
            added_at: now,
            status: OwnedStatus::Verified,
            provides,
            // Derived from the representative (largest) video file so the record is comparable on
            // the dedup/upgrade/consolidation paths: an untagged axis is recorded as unknown (the
            // lowest quality), an over-ceiling file keeps its real resolution. `None` only when there
            // is no video file.
            quality: mirror_quality(info, prefs),
        };
        // Write ONLY if no record exists, ATOMICALLY: the per-title TMDB lookup above is an await
        // point, so a concurrent `engine.acquire` (separate scheduler task) could have written a
        // provenance-bearing record for this hash since the snapshot was taken. A check-then-blind-
        // `put_owned` leaves a race window between the two transactions; `put_owned_if_absent` does the
        // check-and-insert in one transaction, so the mirror can never clobber real provenance with an
        // empty one (which would silently make an engine-acquired title an un-removable mirror copy).
        match store.put_owned_if_absent(hash.clone(), rec).await {
            Ok(true) => recorded += 1,
            Ok(false) => {} // a record already exists for this hash — leave it intact
            Err(e) => warn!("mirror: failed to record owned {}: {}", hash, e),
        }
    }
    if recorded > 0 {
        info!(
            "mirror: recorded {} pre-existing torrent(s) as owned",
            recorded
        );
    }
}

/// The provenance to record AT ACQUIRE TIME: one entry per (user, source) that currently wants
/// the title. PURE and deterministic — de-duplicated, never includes `Manual` (manual origins are
/// account-mirror adds, not Trakt-driven acquisitions, so they are never derived from a wanted-set).
pub(crate) fn provenance_from_wanted(wanted: &[WantedRecord]) -> Provenance {
    let mut prov = Provenance {
        entries: Vec::new(),
    };
    for r in wanted {
        if r.sources.watchlist {
            let e = ProvenanceEntry::Watchlist {
                user: r.user.clone(),
            };
            if !prov.entries.contains(&e) {
                prov.entries.push(e);
            }
        }
        if r.sources.in_progress {
            let e = ProvenanceEntry::InProgress {
                user: r.user.clone(),
            };
            if !prov.entries.contains(&e) {
                prov.entries.push(e);
            }
        }
    }
    prov
}

/// One reconcile decision, derived purely from the store + provider listing. Executed by
/// `execute_acquire` / `execute_remove`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReconcileOp {
    Acquire {
        tmdb_id: u64,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
        provenance: Provenance,
        /// IMDB id Trakt supplied for this title (preferred over re-deriving from TMDB).
        imdb_id: Option<String>,
    },
    Remove {
        tmdb_id: u64,
        hashes: Vec<String>,
    },
}

/// The IMDB id Trakt supplied for a title (first non-empty across its wanted rows). Preferred over
/// re-deriving from TMDB, whose `external_ids` are incomplete for some titles (e.g. a TVDB-only
/// show), which would otherwise leave the title permanently unacquirable.
fn imdb_hint_from_wanted(wanted: &[WantedRecord]) -> Option<String> {
    wanted
        .iter()
        .find_map(|r| r.imdb_id.clone().filter(|s| !s.trim().is_empty()))
}

/// Pick the IMDB id to scrape with: Trakt's (preferred — present whenever Trakt knows the title)
/// else TMDB's `external_ids` (can be missing for TVDB-only titles). `None` ⇒ no IMDB id anywhere,
/// so the title is unacquirable via the IMDB-keyed scraper and is skipped (not an error).
fn choose_imdb(trakt_hint: Option<String>, tmdb_external: Option<String>) -> Option<String> {
    trakt_hint
        .filter(|s| !s.trim().is_empty())
        .or_else(|| tmdb_external.filter(|s| !s.trim().is_empty()))
}

/// Build the `AcquireRequest` for `tmdb_id`: resolve the IMDB id (preferring Trakt's `imdb_hint`
/// over TMDB's `external_ids`) and (title, year, original + spoken languages) from TMDB. Returns `Ok(None)`
/// when no IMDB id exists anywhere — the title can't be scraped (Torrentio is IMDB-keyed) and is
/// skipped quietly. Any TMDB failure → `Err` (the caller logs and skips). TMDB-dependent, so the
/// happy path is exercised by the live smoke rather than unit tests.
async fn build_acquire_request(
    tmdb: &TmdbClient,
    tmdb_id: u64,
    kind: MediaKind,
    season: Option<u32>,
    episode: Option<u32>,
    imdb_hint: Option<String>,
) -> Result<Option<AcquireRequest>, AppError> {
    let media_type = media_type_of(kind);
    // Only hit TMDB's external_ids when Trakt didn't already give us a USABLE IMDB id. Gate on
    // non-emptiness (not mere `Some`-ness) so a `Some("")`/whitespace hint doesn't suppress the
    // TMDB fallback and then get filtered to `None` by `choose_imdb` — which would silently skip a
    // title TMDB could have resolved. This matches `choose_imdb`'s own non-empty filter.
    let has_usable_hint = imdb_hint.as_deref().is_some_and(|s| !s.trim().is_empty());
    let tmdb_external = if has_usable_hint {
        None
    } else {
        tmdb.external_imdb_id(tmdb_id, media_type.clone()).await?
    };
    let Some(imdb_id) = choose_imdb(imdb_hint, tmdb_external) else {
        return Ok(None);
    };
    let details = tmdb.details(tmdb_id, media_type.clone()).await?;
    Ok(Some(AcquireRequest {
        imdb_id,
        tmdb_id,
        kind,
        season,
        episode,
        original_language: details.original_language,
        // Fetched (so `Some`, even when TMDB lists none): `AUDIO_LANGUAGE=original` accepts any of
        // these as well as the original language.
        spoken_languages: Some(details.spoken_languages),
        metadata: MediaMetadata {
            title: details.title,
            year: details.year,
            media_type,
            external_id: Some(format!("tmdb:{}", tmdb_id)),
        },
    }))
}

/// An aggregated view of every owned record sharing a `tmdb_id`: all (lowercased) hashes, the
/// merged provenance across those hashes, the union of owned `(season, episode)` pairs, and the
/// `media_type` taken from the records' requests. Built by [`group_owned_by_tmdb`].
pub(crate) struct OwnedGroup {
    pub hashes: Vec<String>,
    /// The subset of `hashes` that are ENGINE-acquired — i.e. their owned record carries at least
    /// one provenance entry (Trakt watchlist/in-progress, or Manual). Account-mirror records (the
    /// user's pre-existing library) are recorded with EMPTY provenance and are therefore excluded.
    /// Trigger **B** (abandoned watchlist) removal targets ONLY these hashes, so a pre-existing mirror
    /// copy that happens to share a `tmdb_id` with a watchlist-acquired episode is never deleted when
    /// the watchlist is abandoned. Trigger **A** finish-removal likewise targets only these hashes —
    /// the FULL set (incl. mirror copies) is reclaimed in exactly ONE case: a **show** finish-cleanup
    /// when `REMOVE_FINISHED_SHOWS` is on (see [`removal_hashes`]). `hashes` (the full set) is also
    /// used for availability, the dedup pass, and `owned_episodes`.
    pub engine_hashes: Vec<String>,
    pub provenance: Provenance,
    pub owned_episodes: Vec<(u32, u32)>,
    pub media_type: MediaType,
    /// Per record: what it supplies, so `monitor_episodes` can count an episode as covered only
    /// while a record that supplies it is present or still `Pending` (see `plan_episode_actions`).
    pub coverage: Vec<RecordCoverage>,
}

/// One owned record's share of an [`OwnedGroup`].
pub(crate) struct RecordCoverage {
    /// Lowercased hash.
    pub hash: String,
    /// Still `Pending`: `observe` owns it until it settles (and it may not be listed yet).
    pub pending: bool,
    /// The `(season, episode)` pairs it supplies (its `provides`, else its request's episode).
    pub episodes: Vec<(u32, u32)>,
}

/// Group every owned record by its request's `tmdb_id` into an [`OwnedGroup`]. Shared by
/// `plan_reconcile_ops` and `monitor_episodes` so the owned-grouping +
/// per-title aggregation lives in exactly one place.
pub(crate) async fn group_owned_by_tmdb(
    store: &Store,
) -> std::collections::BTreeMap<(MediaType, u64), OwnedGroup> {
    use std::collections::BTreeMap;
    let mut owned_by: BTreeMap<(MediaType, u64), OwnedGroup> = BTreeMap::new();
    for (hash, rec) in store.all_owned().await {
        // Key by (media_type, tmdb_id): a movie and a show with the same numeric id are distinct
        // titles and must not be grouped together.
        let mt = media_type_of(rec.request.kind);
        let group = owned_by
            .entry((mt.clone(), rec.request.tmdb_id))
            .or_insert_with(|| OwnedGroup {
                hashes: Vec::new(),
                engine_hashes: Vec::new(),
                provenance: Provenance {
                    entries: Vec::new(),
                },
                owned_episodes: Vec::new(),
                media_type: mt.clone(),
                coverage: Vec::new(),
            });
        let lc = hash.to_ascii_lowercase();
        // An owned record with NO provenance entries is an account-mirror (pre-existing) record;
        // only records with provenance are engine-acquired and thus lifecycle-removable.
        if !rec.provenance.entries.is_empty() {
            group.engine_hashes.push(lc.clone());
        }
        group.provenance.merge(&rec.provenance);
        // SP3: prefer the recorded `provides` (a pack supplies many episodes); fall back to the
        // request's single (season, episode) for pre-SP3 records that have no `provides` yet.
        let episodes: Vec<(u32, u32)> = if rec.provides.is_empty() {
            match (rec.request.season, rec.request.episode) {
                (Some(s), Some(e)) => vec![(s, e)],
                _ => Vec::new(),
            }
        } else {
            rec.provides.clone()
        };
        group.owned_episodes.extend(episodes.iter().copied());
        group.coverage.push(RecordCoverage {
            hash: lc.clone(),
            pending: rec.status == OwnedStatus::Pending,
            episodes,
        });
        group.hashes.push(lc);
    }
    for g in owned_by.values_mut() {
        g.owned_episodes.sort_unstable();
        g.owned_episodes.dedup();
    }
    owned_by
}

/// The owned hashes to delete for a lifecycle removal, per the trigger reason. Pre-existing
/// account-mirror copies (empty provenance — the user's own library, in `hashes` but NOT
/// `engine_hashes`) are **protected by default** and reclaimed in exactly one Trigger-A case:
/// - `Finished` (Trigger A) **for a show with `REMOVE_FINISHED_SHOWS` on**: the FULL set — this is
///   the opt-in "remove a fully-watched ended show, incl. ones already in the library" cleanup, which
///   folds such shows into the wanted-set so this trigger fires; the mirror copy IS the thing to
///   reclaim, so a blanket `engine_hashes` here would make it a no-op.
/// - `Finished` otherwise (a **movie**, which `REMOVE_FINISHED_SHOWS` never folds; or a **show with
///   the flag off**): only the engine-acquired hashes. A genuine in-progress re-watch (the title is
///   in Trakt `/sync/playback` AND already watched) must NOT delete the user's pre-existing library
///   — that destroys content the engine never acquired.
/// - `Abandoned` (Trigger B): only the engine-acquired hashes — un-watchlisting a Trakt title must
///   never delete content the user added to their debrid account themselves (the mirror "kept"
///   guarantee). A mirror copy of an earlier season survives abandonment of a later-season watchlist.
fn removal_hashes(
    g: &OwnedGroup,
    reason: crate::wanted::RemoveReason,
    remove_finished_shows: bool,
) -> Vec<String> {
    match reason {
        crate::wanted::RemoveReason::Finished
            if g.media_type == MediaType::Show && remove_finished_shows =>
        {
            g.hashes.clone()
        }
        crate::wanted::RemoveReason::Finished | crate::wanted::RemoveReason::Abandoned => {
            g.engine_hashes.clone()
        }
    }
}

/// One title's deduplication decision: keep these owned hashes, remove these redundant ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DedupPlan {
    pub media_type: MediaType,
    pub tmdb_id: u64,
    pub keep: Vec<String>,
    pub remove: Vec<String>,
}

/// PURE duplicate planner. Within each `(media_type, tmdb_id)` group of **present** owned hashes,
/// choose a minimal keep-set that still covers all owned content and mark redundant duplicates for
/// removal — a copy is only ever removed in favour of copies of the SAME OR BETTER quality:
/// - **Movie**: every hash is the same film → keep the single top-priority hash, remove the rest.
/// - **Show**: a greedy set-cover over the episodes each hash `provides`, best first — a hash whose
///   every episode is already provided by a kept hash of same-or-better quality is redundant →
///   remove; otherwise keep it. Complementary packs/episodes are therefore all kept. A show hash with
///   **empty `provides`** (unknown coverage) is **always kept** — never removed on a guess.
///
/// Keep-priority (highest kept first): QUALITY first — [`crate::release::quality_rank`]:
/// resolution, then source tier, then checked before `unverified` (an untagged axis is 0, so an
/// unknown copy is the lowest at its resolution, yet a known resolution stays a floor: a 1080p copy
/// with an untagged source outranks a 720p one; a `None` quality is below everything) — then
/// `quality.score`, then
/// currently-served (`selected`), then more episodes provided, then lexicographically-smallest hash
/// (deterministic). Provenance is NOT a keep axis: a pre-existing account-mirror copy (empty
/// provenance) competes on quality like any engine copy ("the whole point is to upgrade
/// everything"). Consequence (accepted): when a mirror copy loses to a better ENGINE copy, the
/// title's survival then rests on that engine copy, which the Trakt lifecycle (Trigger A/B,
/// [`removal_hashes`]) may later remove — the lifecycle's own mirror protection is unchanged and
/// applies only to copies that are still mirror records. `MAX_RESOLUTION` plays no part: a copy
/// above it is never ranked down for it, so dedup never trades resolution down (a corrective
/// downgrade is the upgrade engine's job, under `ALLOW_RESOLUTION_DOWNGRADE`). A `Manual`-provenance copy is never
/// removed (invariant M-1). Only groups with at least one removal are returned. Absent
/// (not-`present`) hashes are ignored entirely — they're already gone, never a removal target or
/// coverage source — and so are unsettled (`Pending`) ones: only `Verified` copies are ranked, kept,
/// removed or count as coverage, so a still-downloading or not-yet-checked acquisition never
/// displaces a playable copy (and is judged once `observe` settles it).
pub(crate) fn plan_dedup(
    owned: &[(String, OwnedRecord)],
    present: &std::collections::HashSet<String>,
    selected: &std::collections::HashSet<String>,
) -> Vec<DedupPlan> {
    use crate::release::{quality_rank, QualityRank};
    use std::collections::{BTreeMap, HashMap, HashSet};
    let mut groups: BTreeMap<(MediaType, u64), Vec<(String, &OwnedRecord)>> = BTreeMap::new();
    for (hash, rec) in owned {
        let h = hash.to_ascii_lowercase();
        if !present.contains(&h) {
            continue;
        }
        // Only SETTLED copies take part. A `Pending` record is an acquisition `observe` hasn't
        // resolved yet — still downloading (its acquire-time quality says `cached: false`), or not
        // yet probed / title-checked — and quality ranking ignores both. Letting it win a keep slot
        // (or cover a Verified copy's episodes) would delete a playable copy in favour of one that
        // may yet stall, die or fail a check (observe then deletes it too: no copy left). It is not
        // removed either: observe owns it until it settles, and dedup judges it then.
        if rec.status != OwnedStatus::Verified {
            continue;
        }
        let mt = media_type_of(rec.request.kind);
        groups
            .entry((mt, rec.request.tmdb_id))
            .or_default()
            .push((h, rec));
    }
    let rank_of = |r: &OwnedRecord| quality_rank(r.quality.as_ref());

    let mut plans = Vec::new();
    for ((media_type, tmdb_id), mut hashes) in groups {
        if hashes.len() < 2 {
            continue; // no possible duplicate
        }
        // Sort by keep-priority DESC; deterministic tie-break on hash ASC.
        hashes.sort_by(|(ha, ra), (hb, rb)| {
            let key = |h: &str, r: &OwnedRecord| {
                (
                    rank_of(r),
                    r.quality.as_ref().map(|q| q.score).unwrap_or(i64::MIN),
                    selected.contains(h),
                    r.provides.len(),
                )
            };
            key(hb, rb).cmp(&key(ha, ra)).then_with(|| ha.cmp(hb))
        });

        let mut keep = Vec::new();
        let mut remove = Vec::new();
        match media_type {
            MediaType::Movie => {
                keep.push(hashes[0].0.clone());
                for (h, r) in &hashes[1..] {
                    // A Manual-provenance copy is a protected never-auto-remove class (invariant
                    // M-1): never dedup-remove it, even when a higher-keep-priority duplicate won the
                    // keep slot — that would silently drop its never-auto-remove designation from the
                    // surviving group.
                    if r.provenance.has_manual_entry() {
                        keep.push(h.clone());
                    } else {
                        remove.push(h.clone());
                    }
                }
            }
            MediaType::Show => {
                // Per-hash episode SETS (a complete-series pack provides hundreds of episodes), so
                // the coverage checks below are O(1) per episode.
                let provides_of: HashMap<&str, HashSet<(u32, u32)>> = hashes
                    .iter()
                    .map(|(h, r)| (h.as_str(), r.provides.iter().copied().collect()))
                    .collect();
                let rank_by_hash: HashMap<&str, QualityRank> = hashes
                    .iter()
                    .map(|(h, r)| (h.as_str(), rank_of(r)))
                    .collect();
                // Manual (M-1) and unknown-coverage (empty `provides`) hashes are never removed.
                let unremovable =
                    |r: &OwnedRecord| r.provenance.has_manual_entry() || r.provides.is_empty();
                // Every episode `h` provides is also provided by a hash in `by` (other than `h`)
                // whose quality rank is at least `h`'s — the only coverage that may make `h`
                // redundant (score ignored: same rank = same quality).
                let covered_by_same_or_better = |h: &str, by: &[String]| {
                    let mine = rank_by_hash[h];
                    provides_of[h].iter().all(|ep| {
                        by.iter().any(|k| {
                            k.as_str() != h
                                && rank_by_hash[k.as_str()] >= mine
                                && provides_of[k.as_str()].contains(ep)
                        })
                    })
                };
                // Greedy pass, best first: every hash kept so far ranks at least as high, so this
                // keeps a hash exactly when it adds an episode no kept hash provides yet.
                for (h, r) in &hashes {
                    if !unremovable(r) && covered_by_same_or_better(h, &keep) {
                        remove.push(h.clone());
                    } else {
                        keep.push(h.clone());
                    }
                }
                // Second sweep — reclaim a kept hash the greedy pass missed because it sorted BEFORE
                // the copy that also covers it (e.g. a higher-score/served singleton kept ahead of an
                // equal-quality pack covering it). Remove a kept, removable hash only when EVERY
                // episode it provides is provided by ANOTHER still-kept hash of the same or better
                // quality rank: a better copy is never removed because a worse one covers it.
                // Processed lowest-keep-priority first (keep is priority-DESC, so iterate in reverse)
                // and re-checked against the CURRENT kept set after each removal — so a
                // mutually-covering cycle (A⊆B∪C, B⊆A∪C, C⊆A∪B) drops only the redundant ones and
                // never breaks total coverage.
                let removable: HashMap<&str, bool> = hashes
                    .iter()
                    .map(|(h, r)| (h.as_str(), !unremovable(r)))
                    .collect();
                let mut i = keep.len();
                while i > 0 {
                    i -= 1;
                    let h = keep[i].clone();
                    if removable[h.as_str()] && covered_by_same_or_better(&h, &keep) {
                        keep.remove(i);
                        remove.push(h);
                    }
                }
            }
        }
        if !remove.is_empty() {
            plans.push(DedupPlan {
                media_type,
                tmdb_id,
                keep,
                remove,
            });
        }
    }
    plans
}

/// Groups `(media_type, tmdb_id)` that have an owned record added within `idle_secs` of `now`. Such a
/// group may have an upgrade stage in flight (the new hash is recorded owned before the selection is
/// repointed + the old hash pruned), so dedup must SKIP it to avoid deleting the just-staged copy
/// mid-swap. Pure, for testability.
pub(crate) fn fresh_dedup_groups(
    owned: &[(String, OwnedRecord)],
    now: u64,
    idle_secs: u64,
) -> std::collections::HashSet<(MediaType, u64)> {
    owned
        .iter()
        .filter(|(_, r)| now.saturating_sub(r.added_at) < idle_secs)
        .map(|(_, r)| (media_type_of(r.request.kind), r.request.tmdb_id))
        .collect()
}

/// Run the duplicate-dedup pass over the current owned set + provider listing. Computes a
/// [`plan_dedup`] and either logs it (dry-run) or executes removals. Destructive removal is gated on
/// `config.dedup_remove_duplicates` (default `false` = dry-run, so the plan can be reviewed first)
/// AND on the library being idle (`upgrade.idle_secs`), so a removal never interrupts an active
/// stream — a redundant torrent being read is left until the next idle pass. Execution: delete each
/// redundant torrent from the provider, drop its owned record and authoritative identification,
/// and clear any `selection` slot that pointed at it (the VFS re-derives the slot from the kept
/// covering hash on the next scan).
///
/// Takes the provider listing already fetched by the scan tick (rather than re-fetching it) so the
/// scan loop makes one `get_torrents` call per tick, not two.
pub(crate) async fn dedup_owned(app: &AppState, torrents: &[Torrent]) {
    let present: std::collections::HashSet<String> = torrents
        .iter()
        .map(|t| t.hash.to_ascii_lowercase())
        .collect();
    let id_by_hash: HashMap<String, String> = torrents
        .iter()
        .map(|t| (t.hash.to_ascii_lowercase(), t.id.clone()))
        .collect();
    let owned = app.store.all_owned().await;
    let selection = app.store.all_selection().await;
    let selected: std::collections::HashSet<String> = selection
        .iter()
        .map(|(_, e)| e.hash.to_ascii_lowercase())
        .collect();
    // Index slots by hash ONCE so per-removed-hash selection cleanup is O(1), not O(removed × table).
    let slots_by_hash = index_selection_slots_by_hash(&selection);

    let mut plans = plan_dedup(&owned, &present, &selected);
    // Don't dedup a title with a FRESHLY-staged copy. The upgrade engine adds a new (better) hash and
    // records it owned BEFORE it repoints the selection + prunes the old hash; a concurrent dedup tick
    // (separate task) that saw both copies would keep the still-selected OLD hash and delete the
    // just-staged new one — and the upgrade then prunes the old hash too, losing BOTH copies
    // (permanent for a pure-mirror title). Skipping any group touched within the idle window lets an
    // in-flight swap finish; a genuine duplicate is reclaimed on a later pass once the stage settles.
    let fresh = fresh_dedup_groups(&owned, crate::now_unix_secs(), app.config.upgrade.idle_secs);
    plans.retain(|p| !fresh.contains(&(p.media_type.clone(), p.tmdb_id)));
    if plans.is_empty() {
        return;
    }
    let total: usize = plans.iter().map(|p| p.remove.len()).sum();
    let removing = app.config.dedup_remove_duplicates;
    info!(
        "dedup: {} title(s) with duplicates, {} redundant torrent(s) — {}",
        plans.len(),
        total,
        if removing {
            // NOT "REMOVING": removal is still idle-gated below, so it may be deferred this tick. The
            // "dedup: removed N" line at the end reports what was actually deleted.
            "removal enabled (idle-gated)"
        } else {
            "DRY RUN (set DEDUP_REMOVE_DUPLICATES=true to remove)"
        }
    );
    let short = |h: &str| h.get(..8).unwrap_or(h).to_string();
    for plan in &plans {
        debug!(
            "dedup: tmdb {} ({:?}) keep [{}] remove [{}]",
            plan.tmdb_id,
            plan.media_type,
            plan.keep
                .iter()
                .map(|h| short(h))
                .collect::<Vec<_>>()
                .join(","),
            plan.remove
                .iter()
                .map(|h| short(h))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    if !removing {
        return;
    }
    // Idle-gate the destructive removals (don't interrupt an active stream).
    let idle_window = Duration::from_secs(app.config.upgrade.idle_secs);
    if !app.read_activity.all_idle(idle_window).await {
        debug!("dedup: library not idle — deferring removals to a later pass");
        return;
    }
    let mut removed = 0usize;
    for plan in &plans {
        for h in &plan.remove {
            if let Some(id) = id_by_hash.get(h) {
                if let Err(e) = app.provider.delete_torrent(id).await {
                    warn!(
                        "dedup: delete torrent for {} failed: {} — skipping",
                        short(h),
                        e
                    );
                    continue;
                }
            }
            // The torrent is already gone from the provider; if dropping the owned record fails, the
            // stale record would never be reclaimed (plan_dedup only considers PRESENT hashes), so
            // log it and DON'T count the hash as removed — mirrors `execute_remove`'s discipline.
            if let Err(e) = app.store.remove_owned(h.clone()).await {
                warn!(
                    "dedup: remove_owned {} failed after delete: {} — record left stale",
                    short(h),
                    e
                );
                continue;
            }
            if let Err(e) = app.store.remove_authoritative(h.clone()).await {
                warn!("dedup: remove_authoritative {} failed: {}", short(h), e);
            }
            // Clear any selection slot that pointed at the removed hash (the VFS re-derives it from
            // the kept covering hash on the next scan).
            if let Some(slots) = slots_by_hash.get(&h.to_ascii_lowercase()) {
                for slot in slots {
                    if let Err(e) = app.store.remove_selection(slot.clone()).await {
                        warn!(
                            "dedup: remove selection slot {} for {} failed: {}",
                            slot,
                            short(h),
                            e
                        );
                    }
                }
            }
            removed += 1;
        }
    }
    info!("dedup: removed {} redundant torrent(s)", removed);
}

/// Group every wanted record by its `tmdb_id` into a `BTreeMap`. Shared by
/// `plan_reconcile_ops` and `monitor_episodes` so the wanted-grouping lives in one place.
pub(crate) async fn group_wanted_by_tmdb(
    store: &Store,
) -> std::collections::BTreeMap<(MediaType, u64), Vec<WantedRecord>> {
    use std::collections::BTreeMap;
    let mut wanted_by: BTreeMap<(MediaType, u64), Vec<WantedRecord>> = BTreeMap::new();
    for r in store.all_wanted().await {
        wanted_by
            .entry((r.media_type.clone(), r.tmdb_id))
            .or_default()
            .push(r);
    }
    wanted_by
}

/// The pure, unit-testable decision layer: diff the combined `wanted` set against owned content
/// (present in `torrents` = available) and return the `ReconcileOp`s. NO TMDB, NO engine.
///
/// Movies are fully reconciled (acquire / re-acquire / Trigger-A + Trigger-B removal) via
/// `wanted::reconcile_title`. Shows get ONLY Trigger-B removal here (abandoned watchlist, no air
/// dates needed); show-episode acquisition and Trigger-A finish removal are `monitor_episodes`'.
pub(crate) async fn plan_reconcile_ops(store: &Store, torrents: &[Torrent]) -> Vec<ReconcileOp> {
    use crate::wanted::{reconcile_title, trigger_b_abandoned, Action, Owned, TitleView};
    use std::collections::{BTreeSet, HashSet};

    // Group wanted rows by tmdb_id.
    let wanted_by = group_wanted_by_tmdb(store).await;

    // Group owned records by tmdb_id (shared helper — same grouping monitor_episodes uses).
    let owned_by = group_owned_by_tmdb(store).await;

    // Presence in the provider listing = available; an absent owned hash is lapsed/lost.
    let present: HashSet<String> = torrents
        .iter()
        .map(|t| t.hash.to_ascii_lowercase())
        .collect();
    // A wholesale-empty listing the scan loop hasn't accepted: every owned title counts as
    // available (no lapsed re-acquire), and no removal is planned — `execute_remove` can't delete a
    // torrent it can't see, and dropping the record would orphan the torrent as a mirror copy.
    let listing_suspect = listing_is_suspect(store, torrents).await;
    if listing_suspect {
        warn!(
            "reconcile: the provider listing is empty but the library is not; treating it as suspect (no re-acquire of owned titles, no removal) this tick"
        );
    }

    // Titles are keyed by (media_type, tmdb_id), so each title has a single unambiguous media_type
    // (no movie/show id collision possible) — taken straight from the key.
    let mut keys: BTreeSet<(MediaType, u64)> = BTreeSet::new();
    keys.extend(wanted_by.keys().cloned());
    keys.extend(owned_by.keys().cloned());

    let mut ops = Vec::new();
    for key in keys {
        let (media_type, tmdb_id) = key.clone();
        let wanted = wanted_by.get(&key).cloned().unwrap_or_default();
        let owned_group = owned_by.get(&key);

        // available = ANY owned hash for this title is present in the listing.
        let available = owned_group
            .map(|g| listing_suspect || g.hashes.iter().any(|h| present.contains(h)))
            .unwrap_or(false);

        match media_type {
            MediaType::Movie => {
                let view = TitleView {
                    tmdb_id,
                    media_type: MediaType::Movie,
                    wanted: wanted.clone(),
                    owned: owned_group.map(|g| Owned {
                        hash: g.hashes.first().cloned().unwrap_or_default(),
                        provenance: g.provenance.clone(),
                        available,
                        owned_episodes: Vec::new(),
                    }),
                    aired_episodes: Vec::new(),
                };
                for action in reconcile_title(&view) {
                    match action {
                        Action::AcquireMovie { tmdb_id } => {
                            let mut prov = provenance_from_wanted(&wanted);
                            // Preserve existing provenance (esp. Manual) on a lapsed re-acquire so a manually-owned
                            // title can't have its Manual origin — and thus its never-auto-remove guard — silently erased.
                            if let Some(g) = owned_group {
                                prov.merge(&g.provenance);
                            }
                            ops.push(ReconcileOp::Acquire {
                                tmdb_id,
                                kind: MediaKind::Movie,
                                season: None,
                                episode: None,
                                provenance: prov,
                                imdb_id: imdb_hint_from_wanted(&wanted),
                            });
                        }
                        // Both triggers remove only engine-acquired hashes for a movie (a movie
                        // mirror copy is never reclaimed — see `removal_hashes`). (The Action's
                        // `hash` is just a representative.)
                        Action::Remove {
                            tmdb_id, reason, ..
                        } => {
                            if let Some(g) = owned_group {
                                // Movies are never folded by `REMOVE_FINISHED_SHOWS`, so a movie
                                // mirror copy is never finish-reclaimed (pass `false`): a genuine
                                // in-progress re-watch must not delete the user's pre-existing
                                // library. `removal_hashes` enforces this via the group's media_type.
                                let hashes = removal_hashes(g, reason, false);
                                // A mirror-only movie resolves to zero hashes — don't emit a no-op
                                // Remove every tick (it would just churn an empty op + a full
                                // selection-table read in `execute_remove`).
                                if !hashes.is_empty() {
                                    ops.push(ReconcileOp::Remove { tmdb_id, hashes });
                                }
                            }
                        }
                        Action::AcquireEpisode { .. } => {} // movies never produce this
                    }
                }
            }
            MediaType::Show => {
                // Shows: REMOVAL via Trigger B only here (abandoned watchlist; no air dates).
                if let Some(g) = owned_group {
                    if !g.provenance.has_manual_entry()
                        && trigger_b_abandoned(&wanted, &g.provenance)
                    {
                        // Only the engine-acquired hashes — a pre-existing mirror copy of an earlier
                        // season must survive abandonment of a watchlist that acquired later episodes.
                        // Route through `removal_hashes` (single source of truth for trigger→hashes)
                        // and skip a no-op empty removal.
                        let hashes =
                            removal_hashes(g, crate::wanted::RemoveReason::Abandoned, false);
                        if !hashes.is_empty() {
                            ops.push(ReconcileOp::Remove { tmdb_id, hashes });
                        }
                    }
                }
                // monitor_episodes handles show-episode acquire + Trigger-A finish removal
                // (air-date dependent).
            }
        }
    }
    if listing_suspect {
        ops.retain(|o| !matches!(o, ReconcileOp::Remove { .. }));
    }
    ops
}

/// Whether `torrents`, the provider listing, is a suspect wholesale-empty listing: empty while the
/// identification cache still holds entries. The scan loop keeps that cache through a run of empty
/// listings and drops it only once it accepts the account as empty ([`empty_listing_is_suspect`]),
/// so the reconciler and episode monitor follow its verdict: until then an empty-but-OK listing (a
/// provider glitch) must not read as "everything lapsed".
async fn listing_is_suspect(store: &Store, torrents: &[Torrent]) -> bool {
    torrents.is_empty() && store.has_matches().await
}

/// Execute a `Remove`: delete every torrent of each owned hash from the provider (matched
/// case-insensitively in `torrents` — a same-hash duplicate is deleted too, or the next scan would
/// re-mirror it as an empty-provenance copy the lifecycle never removes) and drop its `owned`
/// record, authoritative identification and selection slots. Errors are logged, not fatal.
/// NO TMDB — unit-testable.
async fn execute_remove(
    provider: &Arc<dyn DebridProvider>,
    torrents: &[Torrent],
    store: &Store,
    read_activity: &Arc<crate::read_activity::ReadActivity>,
    idle_window: Duration,
    tmdb_id: u64,
    hashes: &[String],
) {
    // A reason-scoped removal can resolve to zero hashes (e.g. a Trigger-A finish on a mirror-only
    // title that protects every pre-existing copy). Skip early so we don't read the whole selection
    // table just to iterate an empty list.
    if hashes.is_empty() {
        return;
    }
    // Idle-gate the destructive removal so a Trakt Trigger-A/B removal never interrupts an active
    // stream (consistent with dedup/upgrade) — important in a multi-user household where one member
    // finishing/abandoning a title must not yank it out from under another who is watching it.
    // Library-wide: if anything is being read, defer. The owned record is left intact, so the next
    // reconcile tick re-derives and retries the removal once idle.
    if !read_activity.all_idle(idle_window).await {
        debug!(
            "reconcile: library not idle — deferring removal of tmdb {} to a later tick",
            tmdb_id
        );
        return;
    }
    // NOTE: on a delete failure we skip remove_owned so the next reconcile tick retries — leaving
    // the owned record intact means the Remove op is re-derived and the torrent is retried instead
    // of being silently orphaned on the provider.
    //
    // Read + index the selection table ONCE (we only ever remove from it below), so per-hash slot
    // cleanup is O(1) rather than an O(hashes × table) nested scan.
    let slots_by_hash = index_selection_slots_by_hash(&store.all_selection().await);
    for hash in hashes {
        let mut delete_failed = false;
        for t in torrents
            .iter()
            .filter(|t| t.hash.eq_ignore_ascii_case(hash))
        {
            if let Err(e) = provider.delete_torrent(&t.id).await {
                warn!(
                    "reconcile: delete_torrent {} (tmdb {}) failed: {}; will retry next tick",
                    t.id, tmdb_id, e
                );
                delete_failed = true;
            }
        }
        if delete_failed {
            continue; // leave the owned record so the next reconcile retries
        }
        if let Err(e) = store.remove_owned(hash.clone()).await {
            warn!("reconcile: remove_owned {} (tmdb {}) failed: {}; leaving selection intact for retry", hash, tmdb_id, e);
            continue;
        }
        if let Err(e) = store.remove_authoritative(hash.clone()).await {
            warn!(
                "reconcile: remove_authoritative {} (tmdb {}) failed: {}",
                hash, tmdb_id, e
            );
        }
        // SP3: drop any selection slots this hash represented.
        if let Some(slots) = slots_by_hash.get(&hash.to_ascii_lowercase()) {
            for slot in slots {
                if let Err(e) = store.remove_selection(slot.clone()).await {
                    warn!(
                        "reconcile: remove selection slot {} for {} failed: {}",
                        slot, hash, e
                    );
                }
            }
        }
    }
}

/// Execute an `Acquire`: build the request from TMDB (preferring Trakt's `imdb_hint`), then drive
/// the acquisition engine (SP3 optimistic-add), recording the supplied provenance. A title with no
/// IMDB id anywhere is skipped quietly (it can't be scraped). TMDB/engine-dependent — exercised by
/// the live smoke.
#[allow(clippy::too_many_arguments)]
async fn execute_acquire(
    engine: &AcquisitionEngine,
    tmdb: &TmdbClient,
    tmdb_id: u64,
    kind: MediaKind,
    season: Option<u32>,
    episode: Option<u32>,
    provenance: Provenance,
    imdb_hint: Option<String>,
) {
    match build_acquire_request(tmdb, tmdb_id, kind, season, episode, imdb_hint).await {
        Ok(Some(req)) => {
            let outcome = engine.acquire(req, provenance).await;
            info!("reconcile: acquire tmdb {} -> {:?}", tmdb_id, outcome);
        }
        Ok(None) => debug!(
            "reconcile: skip tmdb {} — no IMDB id from Trakt or TMDB (unacquirable via IMDB-keyed scraper)",
            tmdb_id
        ),
        Err(e) => warn!(
            "reconcile: build_acquire_request for tmdb {} failed: {}",
            tmdb_id, e
        ),
    }
}

/// Reconcile the combined wanted-set against owned-and-available content: acquire missing/lapsed
/// titles (recording per-user provenance) and remove engine-owned titles per the removal
/// lifecycle. Idempotent — re-derives every decision from the store + provider listing each call.
pub async fn reconcile_wanted(
    engine: &AcquisitionEngine,
    provider: &Arc<dyn DebridProvider>,
    tmdb: &TmdbClient,
    store: &Store,
    read_activity: &Arc<crate::read_activity::ReadActivity>,
    idle_window: Duration,
    removals_trustworthy: bool,
) {
    let torrents = match provider.get_torrents().await {
        Ok(t) => t,
        Err(e) => {
            warn!("reconcile_wanted: get_torrents failed ({}); skipping this tick to avoid acting on a stale/empty listing", e);
            return;
        }
    };
    let mut ops = plan_reconcile_ops(store, &torrents).await;
    // Cross-user removals are only safe against a `wanted` set a sync actually confirmed. When no
    // enrolled account synced this cycle (e.g. the first tick after the v4→v5 migration cleared
    // `wanted` with Trakt unreachable), drop every Remove so a spuriously-empty set can't delete
    // engine-acquired content; acquires are unaffected (an empty set produces none). Self-heals once
    // a sync repopulates the set. Mirrors the get_torrents / empty-listing guards elsewhere.
    if !removals_trustworthy {
        let before = ops.len();
        ops.retain(|o| !matches!(o, ReconcileOp::Remove { .. }));
        let suppressed = before - ops.len();
        if suppressed > 0 {
            warn!(
                "reconcile: suppressing {} removal(s) this tick — no Trakt account synced successfully, so the `wanted` set is untrustworthy (e.g. just after a schema migration cleared it); removals resume once a sync confirms it",
                suppressed
            );
        }
    }
    if !ops.is_empty() {
        let acquires = ops
            .iter()
            .filter(|o| matches!(o, ReconcileOp::Acquire { .. }))
            .count();
        debug!(
            "reconcile: planned {} op(s) — {} acquire, {} remove (over {} provider torrents)",
            ops.len(),
            acquires,
            ops.len() - acquires,
            torrents.len()
        );
    }
    for op in ops {
        match op {
            ReconcileOp::Remove { tmdb_id, hashes } => {
                execute_remove(
                    provider,
                    &torrents,
                    store,
                    read_activity,
                    idle_window,
                    tmdb_id,
                    &hashes,
                )
                .await
            }
            ReconcileOp::Acquire {
                tmdb_id,
                kind,
                season,
                episode,
                provenance,
                imdb_id,
            } => {
                execute_acquire(
                    engine, tmdb, tmdb_id, kind, season, episode, provenance, imdb_id,
                )
                .await
            }
        }
    }
}

// ── monitor_episodes (SP2) ───────────────────────────────────────────────────

/// Keep the episodes that have aired on/before `today` — an episode airing TODAY counts as aired
/// (the `<=` boundary) — dropping those with no air date (`None`) or a future date. Returns the
/// `(season, episode)` pairs in INPUT order. PURE — unit-tested for the chrono boundary.
pub(crate) fn aired_pairs(
    episodes: &[crate::tmdb_client::EpisodeAirDate],
    today: chrono::NaiveDate,
) -> Vec<(u32, u32)> {
    episodes
        .iter()
        .filter_map(|e| match e.air_date {
            Some(d) if d <= today => Some((e.season, e.episode)),
            _ => None,
        })
        .collect()
}

/// The aired-episode set for a show, plus whether it could be determined in FULL. `complete` is
/// `false` if season enumeration failed (→ `pairs` empty) or ANY season's air-date lookup failed
/// (→ `pairs` is missing that season). An incomplete set must NOT be trusted for removal (Trigger A
/// goes vacuously true on an empty/partial set), only for best-effort acquisition of the episodes
/// we DID confirm aired. See `guard_removal_on_incomplete_aired`.
#[derive(Debug, Clone, Default)]
pub(crate) struct AiredEpisodes {
    pub pairs: Vec<(u32, u32)>,
    pub complete: bool,
}

/// All aired `(season, episode)` pairs for a show as of `today`: enumerate the show's
/// (non-Specials) seasons, fetch each season's episode air dates, and collect `aired_pairs`
/// across them. Best-effort I/O — a failure to enumerate seasons, or to fetch ONE season's air
/// dates, is logged and skipped rather than failing the whole show, but the returned `complete`
/// flag records whether any lookup failed so callers can refuse to act on a partial set.
/// TMDB-driven, so the I/O is exercised by the live smoke rather than unit tests.
pub(crate) async fn aired_episodes(
    tmdb: &TmdbClient,
    tmdb_id: u64,
    today: chrono::NaiveDate,
) -> AiredEpisodes {
    let seasons = match tmdb.show_season_numbers(tmdb_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "monitor_episodes: show_season_numbers({}) failed: {}; aired set unknown (no removal this tick)",
                tmdb_id, e
            );
            return AiredEpisodes::default(); // pairs empty, complete=false
        }
    };
    let mut out = Vec::new();
    let mut complete = true;
    for season in seasons {
        match tmdb.season_air_dates(tmdb_id, season).await {
            Ok(eps) => out.extend(aired_pairs(&eps, today)),
            Err(e) => {
                complete = false;
                warn!(
                    "monitor_episodes: season_air_dates({}, {}) failed: {}; skipping season (no removal this tick)",
                    tmdb_id, season, e
                );
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    AiredEpisodes {
        pairs: out,
        complete,
    }
}

/// Drop any `Remove` action unless the aired set is BOTH complete AND non-empty: a partial set (a
/// TMDB lookup failed) OR an empty set must never let Trigger A vacuously fire and delete a
/// still-wanted show on a transient/quirky external signal ("a failed fetch must NOT cause
/// removal"). An EMPTY aired set is the subtle case: `aired_episodes` returns `pairs=[], complete=true`
/// when TMDB reports no non-Specials seasons, or every episode has a null air date — and
/// `user_finished`'s `aired.iter().all(..)` is then VACUOUSLY true for an Ended show, so without this
/// guard an in-progress-only Ended show whose TMDB data momentarily looks empty would be removed and
/// (via the acquire-side symmetry guard) not re-fetched. An Ended show with zero aired episodes is
/// never genuinely finishable, so refusing removal there is always safe. Acquisition actions are
/// always kept — acquiring only the episodes we confirmed aired is safe even from a partial set.
pub(crate) fn guard_removal_on_incomplete_aired(
    actions: Vec<crate::wanted::Action>,
    aired_complete: bool,
    aired_empty: bool,
) -> Vec<crate::wanted::Action> {
    if aired_complete && !aired_empty {
        return actions;
    }
    actions
        .into_iter()
        .filter(|a| !matches!(a, crate::wanted::Action::Remove { .. }))
        .collect()
}

/// Assemble the episode-monitor's per-show action list — PURE (no I/O), so the `TitleView`
/// assembly, `reconcile_title`, and incomplete-aired removal-guard wiring is unit-testable without a
/// live TMDB (which `monitor_episodes` otherwise needs to fetch the aired set). `monitor_episodes`
/// fetches the aired set, calls this, then dispatches the returned actions (acquire episodes /
/// idle-gated removals).
///
/// Availability is per EPISODE: an episode counts as covered only while a record that supplies it
/// is present in the listing or still `Pending` (`observe` owns it until it settles, and it may not
/// be listed yet). An episode whose only copy has left the account is re-acquired even while other
/// copies of the show are present — nothing else would bring it back (with no file left to read,
/// on-read repair never fires). When no record of the show is present or pending, the show is
/// lapsed as a whole (`available = false`, every owned episode listed so a manual show restores
/// them). `listing_suspect` (a wholesale-empty listing the scan loop hasn't accepted, see
/// [`listing_is_suspect`]) instead counts every owned episode as covered and drops removals.
fn plan_episode_actions(
    tmdb_id: u64,
    wanted: &[WantedRecord],
    owned_group: Option<&OwnedGroup>,
    present: &std::collections::HashSet<String>,
    aired: &AiredEpisodes,
    listing_suspect: bool,
) -> Vec<crate::wanted::Action> {
    use crate::wanted::{reconcile_title, Owned, TitleView};
    let owned = owned_group.map(|g| {
        let covering: Vec<&RecordCoverage> = g
            .coverage
            .iter()
            .filter(|c| c.pending || present.contains(&c.hash))
            .collect();
        let (available, owned_episodes) = if listing_suspect {
            (true, g.owned_episodes.clone())
        } else if covering.is_empty() {
            (false, g.owned_episodes.clone())
        } else {
            let mut eps: Vec<(u32, u32)> = covering
                .iter()
                .flat_map(|c| c.episodes.iter().copied())
                .collect();
            eps.sort_unstable();
            eps.dedup();
            (true, eps)
        };
        Owned {
            hash: g.hashes.first().cloned().unwrap_or_default(),
            provenance: g.provenance.clone(),
            available,
            owned_episodes,
        }
    });
    let view = TitleView {
        tmdb_id,
        media_type: MediaType::Show,
        wanted: wanted.to_vec(),
        owned,
        aired_episodes: aired.pairs.clone(),
    };
    // Guard: a TMDB hiccup that left the aired set partial OR empty must not delete a still-wanted
    // show (an empty `aired` makes Trigger A's "all aired watched" clause vacuously true). "Empty"
    // means no REGULAR episode: Specials never count toward the finish test (`SPECIALS_SEASON`), so
    // a specials-only set is just as vacuous. A suspect listing never removes either.
    guard_removal_on_incomplete_aired(
        reconcile_title(&view),
        aired.complete && !listing_suspect,
        !aired.pairs.iter().any(crate::wanted::is_regular_episode),
    )
}

/// Filter aired pairs to one season's episode numbers (sorted+deduped). PURE — used by the SP3
/// upgrade consolidation path to ask "what is the full aired set for THIS season?".
pub(crate) fn season_aired(aired: &[(u32, u32)], season: u32) -> Vec<u32> {
    let mut v: Vec<u32> = aired
        .iter()
        .filter(|(s, _)| *s == season)
        .map(|(_, e)| *e)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// For each tracked (wanted) SHOW, compute the episodes aired as-of-now from TMDB air dates,
/// assemble a `TitleView` with those aired episodes, run the SAME pure reconcile-core
/// (`wanted::reconcile_title`), and execute the resulting ops — acquiring aired-but-not-owned
/// episodes and removing finished/abandoned shows. This is the air-date-dependent half of show
/// handling that `reconcile_wanted` leaves out.
///
/// SCOPE: `monitor_episodes` does episode acquisition + Trigger-A finish removal (both air-date
/// dependent, hence here and not in `reconcile_wanted`). It does NOT perform Trigger-B (abandoned-watchlist)
/// removal: it iterates only titles still in the wanted set, and every persisted `WantedRecord`
/// has `wants() == true`, so `trigger_b_abandoned` — which requires that NO user wants the title —
/// is always false for anything reached here. Trigger B (for owned-but-*unwanted* titles whose
/// wanted rows were pruned) is exclusively `reconcile_wanted`'s, which iterates wanted ∪ owned.
///
/// AVAILABILITY is per episode (see [`plan_episode_actions`]): an episode is covered only while a
/// record that supplies it is present or still `Pending`, so an episode whose only copy left the
/// account is re-acquired even while the rest of the show is present.
pub async fn monitor_episodes(
    engine: &AcquisitionEngine,
    provider: &Arc<dyn DebridProvider>,
    tmdb: &TmdbClient,
    store: &Store,
    read_activity: &Arc<crate::read_activity::ReadActivity>,
    idle_window: Duration,
    remove_finished_shows: bool,
) {
    use crate::wanted::Action;
    use std::collections::HashSet;

    let today = chrono::Utc::now().date_naive();
    // NOTE: when the episode-monitor and Trakt-cycle jobs fire on overlapping ticks, this
    // and reconcile_wanted each independently fetch get_torrents() AND rebuild the owned/wanted
    // groupings (group_owned_by_tmdb / group_wanted_by_tmdb). They are separate `periodic` tasks at
    // different cadences (not one shared tick), so a fix means either merging them or a short-lived
    // shared snapshot cache — deferred, since the overlap is intermittent and the extra provider
    // round-trip is absorbed by the rate limiter; correctness is unaffected (each re-derives from a
    // fresh listing).
    let torrents = match provider.get_torrents().await {
        Ok(t) => t,
        Err(e) => {
            warn!("monitor_episodes: get_torrents failed ({}); skipping this tick to avoid acting on a stale/empty listing", e);
            return;
        }
    };

    // Group wanted rows by (media_type, tmdb_id).
    let wanted_by = group_wanted_by_tmdb(store).await;

    // Same owned-grouping + availability logic as plan_reconcile_ops (shared helper).
    let owned_by = group_owned_by_tmdb(store).await;
    let present: HashSet<String> = torrents
        .iter()
        .map(|t| t.hash.to_ascii_lowercase())
        .collect();
    let listing_suspect = listing_is_suspect(store, &torrents).await;
    if listing_suspect {
        warn!(
            "monitor_episodes: the provider listing is empty but the library is not; treating it as suspect (no re-acquire of owned episodes, no removal) this tick"
        );
    }

    // Only Show titles. (Owned-but-unwanted shows are handled by reconcile_wanted's Trigger-B path;
    // monitor_episodes is the wanted-set's air-date driver.)
    let show_count = wanted_by
        .keys()
        .filter(|(mt, _)| *mt == MediaType::Show)
        .count();
    debug!(
        "monitor_episodes: {} wanted show(s) to check (of {} wanted titles)",
        show_count,
        wanted_by.len()
    );
    for ((media_type, tmdb_id), wanted) in &wanted_by {
        if *media_type != MediaType::Show {
            continue;
        }
        let tmdb_id = *tmdb_id;
        let owned_group = owned_by.get(&(MediaType::Show, tmdb_id));
        let available = owned_group
            .map(|g| g.hashes.iter().any(|h| present.contains(h)))
            .unwrap_or(false);

        let aired = aired_episodes(tmdb, tmdb_id, today).await;
        let aired_complete = aired.complete;
        let aired_count = aired.pairs.len();
        let owned_eps = owned_group.map(|g| g.owned_episodes.len()).unwrap_or(0);

        // Assemble the TitleView, run reconcile, and apply the incomplete-aired removal guard — all
        // in the pure `plan_episode_actions` (unit-tested without a live TMDB).
        let actions = plan_episode_actions(
            tmdb_id,
            wanted,
            owned_group,
            &present,
            &aired,
            listing_suspect,
        );
        let acquires = actions
            .iter()
            .filter(|a| matches!(a, Action::AcquireEpisode { .. }))
            .count();
        debug!(
            "monitor_episodes: tmdb {} — aired={} (complete={}), owned_eps={}, available={} → {} episode acquire(s), {} remove(s)",
            tmdb_id,
            aired_count,
            aired_complete,
            owned_eps,
            available,
            acquires,
            actions.len() - acquires
        );
        for action in actions {
            match action {
                Action::AcquireEpisode {
                    tmdb_id,
                    season,
                    episode,
                } => {
                    let mut prov = provenance_from_wanted(wanted);
                    // Preserve existing provenance (esp. Manual) on re-acquire — exactly like a movie's
                    // AcquireMovie — so a manually-owned show keeps its never-auto-remove guard.
                    if let Some(g) = owned_group {
                        prov.merge(&g.provenance);
                    }
                    execute_acquire(
                        engine,
                        tmdb,
                        tmdb_id,
                        MediaKind::Series,
                        Some(season),
                        Some(episode),
                        prov,
                        imdb_hint_from_wanted(wanted),
                    )
                    .await;
                }
                // Trigger A (Finished) reclaims pre-existing mirror copies (empty-provenance records
                // for a show already in the library) ONLY when `REMOVE_FINISHED_SHOWS` is on — the
                // opt-in finish-cleanup folds such shows in for exactly this. With the flag off, a
                // Trigger-A removal can only come from a genuine in-progress re-watch, so it removes
                // only the engine-acquired hashes and keeps the user's own library. Trigger B
                // (Abandoned) always keeps mirror copies. (`hash` is a representative.)
                Action::Remove {
                    tmdb_id, reason, ..
                } => {
                    if let Some(g) = owned_group {
                        let hashes = removal_hashes(g, reason, remove_finished_shows);
                        if !hashes.is_empty() {
                            execute_remove(
                                provider,
                                &torrents,
                                store,
                                read_activity,
                                idle_window,
                                tmdb_id,
                                &hashes,
                            )
                            .await;
                        }
                    }
                }
                Action::AcquireMovie { .. } => {} // unreachable for a Show
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rd_client::Torrent;

    fn torrent(id: &str, hash: &str, status: &str) -> Torrent {
        Torrent {
            id: id.to_string(),
            hash: hash.to_string(),
            status: status.to_string(),
            filename: format!("{}.mkv", id),
            ..Default::default()
        }
    }

    #[test]
    fn dedup_keeps_first_downloaded_per_hash_and_flags_rest() {
        // The API returns newest-first, so the first downloaded entry per hash is kept and
        // any later same-hash downloaded entries are flagged for deletion.
        let torrents = vec![
            torrent("a", "H1", "downloaded"),
            torrent("b", "H1", "downloaded"), // duplicate of a
            torrent("c", "H2", "downloaded"),
            torrent("d", "H1", "downloaded"), // another duplicate of a
        ];
        let (kept, dups) = dedup_torrents_by_hash(&torrents);
        let kept_ids: Vec<&str> = kept.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(kept_ids, vec!["a", "c"]);
        assert_eq!(dups, vec!["b".to_string(), "d".to_string()]);
    }

    #[test]
    fn dedup_does_not_self_delete_a_same_id_duplicate() {
        // A paginated listing racing a concurrent add can return the SAME physical torrent twice
        // (same id AND hash) at a page boundary. That artefact must NOT be marked for deletion —
        // deleting it would delete the very torrent we kept (data loss). Only a DISTINCT id is a
        // genuine duplicate (repair leak / external re-add).
        let torrents = vec![
            torrent("a", "H1", "downloaded"),
            torrent("a", "H1", "downloaded"), // same id+hash → listing artefact, NOT a real dup
            torrent("b", "H1", "downloaded"), // distinct id → genuine duplicate of a
        ];
        let (kept, dups) = dedup_torrents_by_hash(&torrents);
        let kept_ids: Vec<&str> = kept.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(kept_ids, vec!["a"]);
        assert_eq!(
            dups,
            vec!["b".to_string()],
            "only the distinct-id duplicate is removed; the same-id artefact must never be"
        );

        // A genuine (distinct-id) duplicate that ALSO appears twice as a page-boundary artefact must
        // be marked for deletion exactly ONCE — not twice (which would spawn a redundant, error-
        // logging second delete on TorBox).
        let torrents = vec![
            torrent("a", "H1", "downloaded"),
            torrent("b", "H1", "downloaded"), // genuine duplicate of a
            torrent("b", "H1", "downloaded"), // same id again → listing artefact of the duplicate
        ];
        let (kept, dups) = dedup_torrents_by_hash(&torrents);
        let kept_ids: Vec<&str> = kept.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(kept_ids, vec!["a"]);
        assert_eq!(
            dups,
            vec!["b".to_string()],
            "the duplicate is removed exactly once"
        );
    }

    #[test]
    fn dedup_keeps_non_downloaded_and_hashless_torrents() {
        // A not-yet-downloaded torrent or a hashless one can't be matched as a duplicate,
        // so it is always kept even if its hash collides with a downloaded entry.
        let torrents = vec![
            torrent("a", "H1", "downloaded"),
            torrent("b", "H1", "downloading"), // same hash, but still downloading → keep
            torrent("c", "", "downloaded"),    // no hash → keep
        ];
        let (kept, dups) = dedup_torrents_by_hash(&torrents);
        let kept_ids: Vec<&str> = kept.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(kept_ids, vec!["a", "b", "c"]);
        assert!(dups.is_empty());
    }

    #[test]
    fn dedup_never_marks_an_empty_id_downloaded_torrent_for_deletion() {
        // A downloaded torrent with an EMPTY id can't be acted on: `delete_torrent("")` would target
        // nothing (or, worse, be rejected). Two empty-id downloaded entries sharing a hash must both
        // be KEPT and NEVER pushed into the removal set as `""` — like a hashless torrent, an
        // empty-id one is simply not a dedup candidate.
        let torrents = vec![
            torrent("", "H1", "downloaded"),
            torrent("", "H1", "downloaded"), // same hash, empty id → must NOT be flagged for deletion
        ];
        let (kept, dups) = dedup_torrents_by_hash(&torrents);
        assert_eq!(kept.len(), 2, "both empty-id torrents are kept");
        assert!(
            dups.is_empty(),
            "an empty id must never be emitted for deletion, got {dups:?}"
        );
    }

    #[test]
    fn stale_ids_returns_seen_keys_absent_from_current() {
        let mut seen: HashMap<String, u8> = HashMap::new();
        seen.insert("keep".to_string(), 1);
        seen.insert("gone".to_string(), 2);
        let current: std::collections::HashSet<&str> = ["keep", "new"].into_iter().collect();
        let mut stale = stale_ids(&seen, &current);
        stale.sort();
        assert_eq!(stale, vec!["gone".to_string()]);
    }

    /// Compile-time check: run_scan_loop has the expected signature.
    #[allow(dead_code)]
    async fn _assert_run_scan_loop_signature(
        app: AppState,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let config = ScanConfig { app };
        run_scan_loop(config, shutdown).await;
    }

    #[tokio::test]
    async fn authoritative_metadata_overrides_identification() {
        use crate::store::Store;
        use crate::vfs::{MediaMetadata, MediaType};
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap();
        let meta = MediaMetadata {
            title: "Authoritative".into(),
            year: Some("2020".into()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:99".into()),
        };
        store
            .put_authoritative("hash".to_string(), meta.clone())
            .await
            .unwrap();
        let tmdb = TmdbClient::offline().await;
        let info = crate::rd_client::TorrentInfo {
            hash: "HASH".into(),
            filename: "totally.unrelated.name.mkv".into(),
            ..Default::default()
        };
        let (got, tmdb_failed) = resolve_metadata(&store, &tmdb, &info).await;
        assert_eq!(got.title, "Authoritative");
        assert_eq!(got.external_id.as_deref(), Some("tmdb:99"));
        assert!(!tmdb_failed, "the authoritative entry needs no TMDB search");
    }

    /// A loopback "TMDB" that answers every request `200 {"results":[]}`: every search is a genuine
    /// no-match.
    async fn spawn_empty_results_tmdb() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let body = r#"{"results":[]}"#;
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// A torrent that comes out unidentified because TMDB could not be reached must not be cached as
    /// unidentified for good (it would never be identified again while listed); a genuine no-match
    /// is cached like any identification.
    #[tokio::test]
    async fn an_identification_during_a_tmdb_outage_is_not_cacheable() {
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap();
        let info = crate::rd_client::TorrentInfo {
            hash: "h".into(),
            filename: "Zzqx.mkv".into(),
            files: vec![crate::rd_client::TorrentFile {
                id: 0,
                path: "Zzqx.mkv".into(),
                bytes: 100,
                selected: 1,
            }],
            ..Default::default()
        };

        // Every search fails (404): TMDB unreachable.
        let down = TmdbClient::offline().await;
        let (meta, tmdb_failed) = resolve_metadata(&store, &down, &info).await;
        assert!(meta.external_id.is_none());
        assert!(tmdb_failed);
        assert!(!identification_is_cacheable(&meta, tmdb_failed));

        // Every search answers "no results": a genuine no-match.
        let empty =
            TmdbClient::with_base_url("k".into(), spawn_empty_results_tmdb().await).unwrap();
        let (meta, tmdb_failed) = resolve_metadata(&store, &empty, &info).await;
        assert!(meta.external_id.is_none());
        assert!(!tmdb_failed);
        assert!(identification_is_cacheable(&meta, tmdb_failed));

        // An identified title is cacheable even if some fallback search failed along the way.
        let identified = MediaMetadata {
            external_id: Some("tmdb:1".into()),
            ..meta
        };
        assert!(identification_is_cacheable(&identified, true));
    }

    #[tokio::test]
    async fn group_owned_uses_provides_for_episode_set() {
        use crate::scraper::MediaKind;
        use crate::store::{AcquireRequest, OwnedRecord, OwnedStatus, Provenance, Store};
        use crate::vfs::{MediaMetadata, MediaType};
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap();
        // A single season-pack hash acquired via an S01E01 request, but `provides` records the WHOLE
        // season — the churn fix: the group's owned_episodes must reflect every provided episode.
        let req = AcquireRequest {
            imdb_id: "tt2".into(),
            tmdb_id: 1396,
            kind: MediaKind::Series,
            season: Some(1),
            episode: Some(1),
            original_language: None,
            spoken_languages: Some(vec![]),
            metadata: MediaMetadata {
                title: "S".into(),
                year: None,
                media_type: MediaType::Show,
                external_id: Some("tmdb:1396".into()),
            },
        };
        store
            .put_owned(
                "pack".into(),
                OwnedRecord {
                    request: req,
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![(1, 1), (1, 2), (1, 3)],
                    quality: None,
                },
            )
            .await
            .unwrap();

        let groups = group_owned_by_tmdb(&store).await;
        let g = groups.get(&(MediaType::Show, 1396)).unwrap();
        let mut eps = g.owned_episodes.clone();
        eps.sort_unstable();
        assert_eq!(
            eps,
            vec![(1, 1), (1, 2), (1, 3)],
            "owned_episodes is the union of provides, not the request's single (s,e)"
        );
    }

    #[test]
    fn mirror_quality_records_every_video_file_including_unknown_and_over_ceiling() {
        let prefs = crate::config::AcquisitionConfig::default().prefs; // 1080p ceiling
        let info = |path: &str| crate::rd_client::TorrentInfo {
            hash: "h".into(),
            files: vec![crate::rd_client::TorrentFile {
                id: 0,
                path: path.into(),
                bytes: 2_000_000_000,
                selected: 1,
            }],
            ..Default::default()
        };
        let res_tier = |q: &crate::release::QualitySummary| (q.resolution, q.source_tier);
        // Untagged filename → recorded, unknown on both axes (unknown = lowest quality, so any known
        // cached release may replace it).
        let untagged = mirror_quality(&info("The Matrix (1999).mkv"), &prefs).expect("recorded");
        assert_eq!(res_tier(&untagged), (0, 0));
        assert!(crate::release::is_unknown_quality(&untagged, 1080));
        // Resolution tagged but no source → recorded with the known axis, still unknown.
        let partial = mirror_quality(&info("Movie.1080p.mkv"), &prefs).expect("recorded");
        assert_eq!(res_tier(&partial), (1080, 0));
        assert!(crate::release::is_unknown_quality(&partial, 1080));
        // Both resolution and source tagged → known.
        let q = mirror_quality(&info("Movie.2020.1080p.BluRay.x264.mkv"), &prefs)
            .expect("a fully-tagged file yields a comparable quality");
        assert!(q.cached);
        assert_eq!(q.resolution, 1080);
        assert!(q.source_tier > 0);
        assert!(!crate::release::is_unknown_quality(&q, 1080));
        // ABOVE the ceiling → its real resolution is recorded (it used to be dropped to `None`, which
        // would now read as unknown-lowest): never unknown, so only a gated corrective downgrade can
        // replace it — whatever its source tier.
        let over =
            mirror_quality(&info("Movie.2020.2160p.WEB-DL.x265.mkv"), &prefs).expect("recorded");
        assert_eq!(res_tier(&over), (2160, 3_000));
        let over_untagged =
            mirror_quality(&info("Some.Show.S04E08.2160p.WEBMux.HDR.HEVC.mkv"), &prefs)
                .expect("recorded");
        assert_eq!(over_untagged.resolution, 2160);
        assert!(!crate::release::is_unknown_quality(&over, 1080));
        assert!(!crate::release::is_unknown_quality(&over_untagged, 1080));
        // A mirrored MKV/MP4 was never probed but is assumed fine (flagging every mirror copy would
        // churn the whole library through the upgrade pass) → not flagged.
        assert!(!q.unverified, "a mirrored .mkv is not flagged unverified");
        let mp4 = mirror_quality(&info("Movie.2020.1080p.BluRay.x264.mp4"), &prefs)
            .expect("a fully-tagged .mp4 yields a comparable quality");
        assert!(!mp4.unverified, "a mirrored .mp4 is not flagged unverified");
        // A container the probe can never read (`.avi`) IS flagged, so the upgrade pass replaces it
        // with a checked copy.
        let avi = mirror_quality(&info("Movie.2020.1080p.BluRay.x264.avi"), &prefs)
            .expect("a fully-tagged .avi yields a comparable quality");
        assert!(avi.unverified, "a mirrored .avi must be flagged unverified");
        // No video file at all → None (nothing to serve, nothing to compare).
        let no_video = crate::rd_client::TorrentInfo {
            hash: "h".into(),
            files: vec![crate::rd_client::TorrentFile {
                id: 0,
                path: "readme.txt".into(),
                bytes: 1,
                selected: 1,
            }],
            ..Default::default()
        };
        assert!(mirror_quality(&no_video, &prefs).is_none());
    }

    /// The quality comes from the file the VFS actually serves: an UNSELECTED file (a Real-Debrid
    /// multi-version torrent where only the 1080p file is selected) is never the representative,
    /// however large, and a torrent whose only video file is unselected has no quality.
    #[test]
    fn mirror_quality_ignores_unselected_files() {
        let prefs = crate::config::AcquisitionConfig::default().prefs; // 1080p ceiling
        let file = |id: u32, path: &str, bytes: u64, selected: u32| crate::rd_client::TorrentFile {
            id,
            path: path.into(),
            bytes,
            selected,
        };
        let info = crate::rd_client::TorrentInfo {
            hash: "h".into(),
            files: vec![
                file(0, "Movie.2010.1080p.WEB-DL.mkv", 4_000_000_000, 1),
                file(1, "Movie.2010.2160p.REMUX.mkv", 60_000_000_000, 0),
            ],
            ..Default::default()
        };
        let q = mirror_quality(&info, &prefs).expect("the selected file is recorded");
        assert_eq!(
            q.resolution, 1080,
            "the served file, not the larger unselected one"
        );
        let only_unselected = crate::rd_client::TorrentInfo {
            hash: "h".into(),
            files: vec![file(0, "Movie.2010.2160p.REMUX.mkv", 60_000_000_000, 0)],
            ..Default::default()
        };
        assert!(mirror_quality(&only_unselected, &prefs).is_none());
    }

    /// Episode files are often untagged while the TORRENT name carries the quality ("Show (S02)
    /// (1080p) (WebDL)" holding "S02E01.mkv"). `mirror_quality` fills every axis the file name
    /// leaves untagged from the torrent name, so such a copy isn't ranked unknown-lowest — and a
    /// "2160p" torrent with untagged files keeps its over-ceiling resolution, so only the gated
    /// corrective downgrade can replace it. An axis the FILE name tags always wins.
    #[test]
    fn mirror_quality_fills_untagged_axes_from_the_torrent_name() {
        let prefs = crate::config::AcquisitionConfig::default().prefs; // 1080p ceiling
        let info = |torrent: &str, path: &str| crate::rd_client::TorrentInfo {
            hash: "h".into(),
            filename: torrent.into(),
            files: vec![crate::rd_client::TorrentFile {
                id: 0,
                path: path.into(),
                bytes: 2_000_000_000,
                selected: 1,
            }],
            ..Default::default()
        };
        let res_tier = |q: &crate::release::QualitySummary| (q.resolution, q.source_tier);
        let web = crate::release::Source::Web.tier_score();
        let bluray = crate::release::Source::BluRay.tier_score();
        let hdtv = crate::release::Source::Hdtv.tier_score();

        let both = mirror_quality(
            &info(
                "Example Show (2019) S02 (1080p NF WEB-DL x265)",
                "Example Show (2019) - S02E01.mkv",
            ),
            &prefs,
        )
        .expect("recorded");
        assert_eq!(
            res_tier(&both),
            (1080, web),
            "both axes from the torrent name"
        );
        assert!(!crate::release::is_unknown_quality(&both, 1080));

        let tier_only = mirror_quality(
            &info(
                "Another Show Temporada 3 [BluRay 1080p]",
                "AS.S03E01.1080p.mkv",
            ),
            &prefs,
        )
        .expect("recorded");
        assert_eq!(
            res_tier(&tier_only),
            (1080, bluray),
            "the tier comes from the torrent"
        );

        let file_wins = mirror_quality(
            &info(
                "Show S01 1080p BluRay x264",
                "Show.S01E01.720p.HDTV.x264.mkv",
            ),
            &prefs,
        )
        .expect("recorded");
        assert_eq!(
            res_tier(&file_wins),
            (720, hdtv),
            "axes the file name tags are never overridden"
        );

        let over = mirror_quality(
            &info("The Show S04 2160p WEB", "The Show - S04E01.mkv"),
            &prefs,
        )
        .expect("recorded");
        assert_eq!(
            over.resolution, 2160,
            "an over-ceiling torrent name is honoured"
        );
        assert!(!crate::release::is_unknown_quality(&over, 1080));

        let neither =
            mirror_quality(&info("The Show", "The Show - S01E01.mkv"), &prefs).expect("recorded");
        assert_eq!(
            res_tier(&neither),
            (0, 0),
            "nothing tagged anywhere → unknown"
        );
        assert!(
            !neither.unverified,
            "the container still comes from the file"
        );
    }

    /// Existing `quality: None` records (account-mirror copies recorded before their quality was
    /// derived, or recorded `None` for being above the ceiling) get their quality re-derived from
    /// their torrent's representative file — so an over-ceiling copy is recognised as such instead
    /// of reading as unknown-lowest. Only `Verified` records with no quality are touched.
    #[tokio::test]
    async fn backfill_owned_quality_re_derives_missing_qualities_from_file_names() {
        use crate::scraper::MediaKind;
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap();
        let base = dedup_rec(MediaKind::Movie, 1, vec![], None);
        let known = qs(720, 3_000, 5);
        for (h, quality, status) in [
            ("uhd", None, OwnedStatus::Verified),
            ("web", None, OwnedStatus::Verified),
            ("kept", Some(known.clone()), OwnedStatus::Verified),
            ("pending", None, OwnedStatus::Pending),
            ("nofile", None, OwnedStatus::Verified),
        ] {
            store
                .put_owned(
                    h.into(),
                    OwnedRecord {
                        quality,
                        status,
                        ..base.clone()
                    },
                )
                .await
                .unwrap();
        }
        let info = |hash: &str, path: &str| crate::rd_client::TorrentInfo {
            hash: hash.to_ascii_uppercase(),
            files: vec![crate::rd_client::TorrentFile {
                id: 0,
                path: path.into(),
                bytes: 2_000_000_000,
                selected: 1,
            }],
            ..Default::default()
        };
        let infos = [
            info("uhd", "Movie.2160p.BluRay.REMUX.mkv"),
            info("web", "Movie.1080p.WEB-DL.mkv"),
            info("kept", "Movie.2160p.BluRay.REMUX.mkv"),
            info("pending", "Movie.1080p.WEB-DL.mkv"),
            // "nofile" has no listing entry → nothing known about it → left None.
        ];
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let owned = store.all_owned().await;
        let infos: Vec<&crate::rd_client::TorrentInfo> = infos.iter().collect();
        let n = backfill_owned_quality(&store, &prefs, &owned, &infos).await;
        assert_eq!(n, 2);
        let q = |h: &str| {
            let store = store.clone();
            let h = h.to_string();
            async move { store.get_owned(h).await.unwrap().quality }
        };
        let uhd = q("uhd").await.expect("derived");
        assert_eq!((uhd.resolution, uhd.source_tier), (2160, 8_000));
        let web = q("web").await.expect("derived");
        assert_eq!((web.resolution, web.source_tier), (1080, 3_000));
        assert_eq!(
            q("kept").await,
            Some(known),
            "a recorded quality is never replaced"
        );
        assert_eq!(
            q("pending").await,
            None,
            "a Pending record gets its quality from observe"
        );
        assert_eq!(q("nofile").await, None);
    }

    /// The scan loop's account-mirror pass runs the backfill over the identified torrents, so an
    /// already-mirrored `quality: None` record gets its quality (and the dedup pass that follows in
    /// the same tick sees it).
    #[tokio::test]
    async fn record_mirror_owned_backfills_an_existing_mirror_record_without_quality() {
        use crate::scraper::MediaKind;
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap();
        store
            .put_owned("mir".into(), dedup_rec(MediaKind::Movie, 603, vec![], None))
            .await
            .unwrap();
        let data = vec![(
            crate::rd_client::TorrentInfo {
                hash: "MIR".into(),
                files: vec![crate::rd_client::TorrentFile {
                    id: 0,
                    path: "The.Matrix.1999.2160p.UHD.BluRay.REMUX.mkv".into(),
                    bytes: 60_000_000_000,
                    selected: 1,
                }],
                ..Default::default()
            },
            MediaMetadata {
                title: "The Matrix".into(),
                year: Some("1999".into()),
                media_type: MediaType::Movie,
                external_id: Some("tmdb:603".into()),
            },
        )];
        let tmdb = TmdbClient::offline().await; // never called: the hash is already owned
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        record_mirror_owned(&store, &tmdb, &prefs, &data).await;
        let q = store
            .get_owned("mir".into())
            .await
            .unwrap()
            .quality
            .expect("backfilled");
        assert_eq!(q.resolution, 2160);
        assert!(!crate::release::is_unknown_quality(&q, 1080));
    }

    #[test]
    fn season_aired_filters_sorts_and_dedups() {
        // Pure helper feeding the consolidation/prune gate (`season_aired.iter().all(...)`): it must
        // return only the season's episodes, sorted and de-duplicated, and empty for an absent season.
        let aired = vec![(1, 3), (2, 1), (1, 1), (1, 3), (1, 2), (3, 5)];
        assert_eq!(
            season_aired(&aired, 1),
            vec![1, 2, 3],
            "season 1 episodes, sorted + deduped"
        );
        assert_eq!(season_aired(&aired, 2), vec![1]);
        assert!(
            season_aired(&aired, 9).is_empty(),
            "an absent season yields no episodes"
        );
        assert!(season_aired(&[], 1).is_empty(), "empty input yields empty");
    }

    fn mirror_test_store() -> Store {
        Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap()
    }

    /// A present, identified movie torrent of `tmdb_id` for the account-mirror tests.
    fn mirror_movie(hash: &str, tmdb_id: u64) -> (crate::rd_client::TorrentInfo, MediaMetadata) {
        (
            crate::rd_client::TorrentInfo {
                hash: hash.into(),
                files: vec![crate::rd_client::TorrentFile {
                    id: 0,
                    path: "Film (1999).mkv".into(),
                    bytes: 100,
                    selected: 1,
                }],
                ..Default::default()
            },
            MediaMetadata {
                title: "Film".into(),
                year: Some("1999".into()),
                media_type: MediaType::Movie,
                external_id: Some(format!("tmdb:{tmdb_id}")),
            },
        )
    }

    /// A TRANSIENT TMDB failure (here a 503 on every attempt) must not poison the per-title IMDB
    /// cache: the next torrent of the same title asks again (10 attempts each), and both records are
    /// written with an empty id for the backfill to fill later.
    #[tokio::test]
    async fn record_mirror_owned_does_not_cache_a_transient_imdb_failure() {
        use crate::trakt_client::test_http::{spawn, Reply};
        let server = spawn(vec![(
            "/movie/603/external_ids?api_key=k",
            // `Retry-After: 0` keeps the 10 retries instant.
            Reply::status(503).header("Retry-After", "0"),
        )])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        let store = mirror_test_store();
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let current = vec![mirror_movie("aaa", 603), mirror_movie("bbb", 603)];

        record_mirror_owned(&store, &tmdb, &prefs, &current).await;

        assert_eq!(
            server.hits("/movie/603/external_ids"),
            20,
            "each torrent retried the lookup: the failure was not cached"
        );
        for h in ["aaa", "bbb"] {
            let rec = store.get_owned(h.into()).await.expect("recorded anyway");
            assert_eq!(rec.request.imdb_id, "");
        }
    }

    /// A DEFINITIVE answer — an id, or a confirmed "no IMDB id" — is looked up once per title per
    /// pass and reused for every torrent of that title.
    #[tokio::test]
    async fn record_mirror_owned_reuses_a_definitive_imdb_answer() {
        use crate::trakt_client::test_http::{spawn, Reply};
        let server = spawn(vec![
            (
                "/movie/603/external_ids?api_key=k",
                Reply::json(r#"{"imdb_id":"tt0133093"}"#),
            ),
            (
                "/movie/604/external_ids?api_key=k",
                Reply::json(r#"{"imdb_id":null}"#),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        let store = mirror_test_store();
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let current = vec![
            mirror_movie("aaa", 603),
            mirror_movie("bbb", 603),
            mirror_movie("ccc", 604),
            mirror_movie("ddd", 604),
        ];

        record_mirror_owned(&store, &tmdb, &prefs, &current).await;

        assert_eq!(server.hits("/movie/603/external_ids"), 1);
        assert_eq!(server.hits("/movie/604/external_ids"), 1);
        for (h, want) in [
            ("aaa", "tt0133093"),
            ("bbb", "tt0133093"),
            ("ccc", ""),
            ("ddd", ""),
        ] {
            assert_eq!(
                store.get_owned(h.into()).await.unwrap().request.imdb_id,
                want
            );
        }
    }

    fn imdb_backfill_record(tmdb_id: u64, imdb: &str, status: OwnedStatus) -> OwnedRecord {
        OwnedRecord {
            request: AcquireRequest {
                imdb_id: imdb.into(),
                tmdb_id,
                kind: MediaKind::Movie,
                season: None,
                episode: None,
                original_language: None,
                spoken_languages: None,
                metadata: mirror_movie("x", tmdb_id).1,
            },
            provenance: Provenance { entries: vec![] },
            added_at: 1,
            status,
            provides: vec![],
            quality: None,
        }
    }

    /// The daily backfill fills the empty IMDB id of a present `Verified` record (one lookup per
    /// title), leaves a recorded id, an absent hash and a `Pending` record alone, skips a title TMDB
    /// definitively doesn't know (a 4xx) and carries on past it.
    #[tokio::test]
    async fn backfill_missing_imdb_ids_fills_present_verified_records() {
        use crate::trakt_client::test_http::{spawn, Reply};
        let server = spawn(vec![(
            "/movie/603/external_ids?api_key=k",
            Reply::json(r#"{"imdb_id":"tt0133093"}"#),
        )])
        .await; // tmdb 700 has no route: a 404, i.e. permanent
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        let store = mirror_test_store();
        for (h, tmdb_id, imdb, status) in [
            ("a404", 700, "", OwnedStatus::Verified),
            ("bmirror", 603, "", OwnedStatus::Verified),
            ("cmirror", 603, "", OwnedStatus::Verified),
            ("dabsent", 603, "", OwnedStatus::Verified),
            ("epending", 603, "", OwnedStatus::Pending),
            ("fknown", 603, "tt5", OwnedStatus::Verified),
        ] {
            store
                .put_owned(h.into(), imdb_backfill_record(tmdb_id, imdb, status))
                .await
                .unwrap();
        }
        let present: std::collections::HashSet<String> =
            ["a404", "bmirror", "cmirror", "epending", "fknown"]
                .into_iter()
                .map(String::from)
                .collect();

        assert_eq!(backfill_missing_imdb_ids(&store, &tmdb, &present).await, 2);

        assert_eq!(
            server.hits("/movie/603/external_ids"),
            1,
            "one lookup per title"
        );
        let imdb = |h: &str| {
            let store = store.clone();
            let h = h.to_string();
            async move { store.get_owned(h).await.unwrap().request.imdb_id }
        };
        assert_eq!(imdb("bmirror").await, "tt0133093");
        assert_eq!(imdb("cmirror").await, "tt0133093");
        assert_eq!(imdb("a404").await, "", "TMDB doesn't know it");
        assert_eq!(
            imdb("dabsent").await,
            "",
            "absent: not this pass's business"
        );
        assert_eq!(imdb("epending").await, "", "observe owns a Pending record");
        assert_eq!(
            imdb("fknown").await,
            "tt5",
            "a recorded id is never overwritten"
        );
    }

    /// A TRANSIENT failure (an outage) stops the pass, so one outage costs one retry budget a day
    /// rather than one per record; everything is retried on the next daily tick.
    #[tokio::test]
    async fn backfill_missing_imdb_ids_stops_at_a_transient_failure() {
        use crate::trakt_client::test_http::{spawn, Reply};
        let server = spawn(vec![
            (
                "/movie/800/external_ids?api_key=k",
                Reply::status(503).header("Retry-After", "0"),
            ),
            (
                "/movie/603/external_ids?api_key=k",
                Reply::json(r#"{"imdb_id":"tt0133093"}"#),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        let store = mirror_test_store();
        store
            .put_owned(
                "aoutage".into(),
                imdb_backfill_record(800, "", OwnedStatus::Verified),
            )
            .await
            .unwrap();
        store
            .put_owned(
                "blater".into(),
                imdb_backfill_record(603, "", OwnedStatus::Verified),
            )
            .await
            .unwrap();
        let present: std::collections::HashSet<String> = ["aoutage", "blater"]
            .into_iter()
            .map(String::from)
            .collect();

        assert_eq!(backfill_missing_imdb_ids(&store, &tmdb, &present).await, 0);
        assert_eq!(
            server.hits("/movie/603/external_ids"),
            0,
            "the pass stopped"
        );
        assert_eq!(
            store
                .get_owned("blater".into())
                .await
                .unwrap()
                .request
                .imdb_id,
            ""
        );
    }

    #[tokio::test]
    async fn record_mirror_owned_adds_owned_records_and_preserves_engine() {
        use crate::scraper::MediaKind;
        use crate::store::{AcquireRequest, OwnedRecord, OwnedStatus, Provenance, Store};
        use crate::vfs::{MediaMetadata, MediaType};
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap();
        // Offline TMDB (404) → external_imdb_id fails → empty imdb (record still written, just not
        // upgradeable until `backfill_missing_imdb_ids` fills it).
        let tmdb = TmdbClient::offline().await;

        // An engine-owned hash already tracked — the mirror pass must NOT clobber it.
        store
            .put_owned(
                "eng".into(),
                OwnedRecord {
                    request: AcquireRequest {
                        imdb_id: "tt1".into(),
                        tmdb_id: 27205,
                        kind: MediaKind::Movie,
                        season: None,
                        episode: None,
                        original_language: None,
                        spoken_languages: Some(vec![]),
                        metadata: MediaMetadata {
                            title: "Inception".into(),
                            year: Some("2010".into()),
                            media_type: MediaType::Movie,
                            external_id: Some("tmdb:27205".into()),
                        },
                    },
                    provenance: Provenance::watchlist("alice"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                },
            )
            .await
            .unwrap();

        let file = |id: u32, path: &str| crate::rd_client::TorrentFile {
            id,
            path: path.into(),
            bytes: 100,
            selected: 1,
        };
        let movie = (
            crate::rd_client::TorrentInfo {
                hash: "MOVIEHASH".into(),
                files: vec![file(0, "The Matrix (1999).mkv")],
                ..Default::default()
            },
            MediaMetadata {
                title: "The Matrix".into(),
                year: Some("1999".into()),
                media_type: MediaType::Movie,
                external_id: Some("tmdb:603".into()),
            },
        );
        let pack = (
            crate::rd_client::TorrentInfo {
                hash: "PACKHASH".into(),
                files: vec![
                    file(0, "Show - S01E01.mkv"),
                    file(1, "Show - S01E02.mkv"),
                    file(2, "Menu Art.mkv"), // no SxxExx → skipped from provides
                ],
                ..Default::default()
            },
            MediaMetadata {
                title: "Show".into(),
                year: None,
                media_type: MediaType::Show,
                external_id: Some("tmdb:1396".into()),
            },
        );
        // The engine hash re-appears in the listing (upper-cased) — must stay as the engine left it.
        let eng = (
            crate::rd_client::TorrentInfo {
                hash: "ENG".into(),
                ..Default::default()
            },
            MediaMetadata {
                title: "Inception".into(),
                year: Some("2010".into()),
                media_type: MediaType::Movie,
                external_id: Some("tmdb:27205".into()),
            },
        );

        let prefs = crate::config::AcquisitionConfig::default().prefs;
        record_mirror_owned(&store, &tmdb, &prefs, &[movie, pack, eng]).await;

        let m = store
            .get_owned("moviehash".into())
            .await
            .expect("movie recorded");
        // Empty provenance — owned + upgradeable, NOT a protected manual class.
        assert!(!m.provenance.has_manual_entry());
        assert!(m.provenance.entries.is_empty());
        assert_eq!(m.status, OwnedStatus::Verified);
        assert_eq!(m.request.tmdb_id, 603);
        assert!(m.provides.is_empty(), "a movie provides no episodes");

        let p = store
            .get_owned("packhash".into())
            .await
            .expect("pack recorded");
        assert!(!p.provenance.has_manual_entry());
        let mut eps = p.provides.clone();
        eps.sort_unstable();
        assert_eq!(
            eps,
            vec![(1, 1), (1, 2)],
            "pack provides = SE-parsed episode files (Menu Art skipped)"
        );

        // Engine record untouched (still Watchlist, never overwritten).
        let e = store.get_owned("eng".into()).await.expect("engine present");
        assert_eq!(e.provenance, Provenance::watchlist("alice"));
    }

    #[tokio::test]
    async fn record_mirror_owned_skips_blacklisted_hashes() {
        use crate::store::Store;
        use crate::vfs::{MediaMetadata, MediaType};
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap();
        let tmdb = TmdbClient::offline().await;
        // The engine rejected + blacklisted this hash (under tmdb 27205) but couldn't immediately
        // delete it (or a concurrent scan caught it mid-staging). The mirror pass must NOT resurrect
        // it as a Verified empty-provenance record (which removal triggers can't reclaim).
        store
            .blacklist_add(
                crate::scraper::MediaKind::Movie,
                27205,
                "badhash".into(),
                "BadAudio",
                1,
            )
            .await
            .unwrap();
        let rejected = (
            crate::rd_client::TorrentInfo {
                hash: "BADHASH".into(),
                files: vec![crate::rd_client::TorrentFile {
                    id: 0,
                    path: "The Matrix (1999).mkv".into(),
                    bytes: 100,
                    selected: 1,
                }],
                ..Default::default()
            },
            // Re-identifies to a DIFFERENT tmdb (603) than it was blacklisted under (27205): the
            // hash-scoped blacklist must STILL catch it (the wrong-title re-identification case).
            MediaMetadata {
                title: "The Matrix".into(),
                year: Some("1999".into()),
                media_type: MediaType::Movie,
                external_id: Some("tmdb:603".into()),
            },
        );
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        record_mirror_owned(&store, &tmdb, &prefs, &[rejected]).await;
        assert!(
            store.get_owned("badhash".into()).await.is_none(),
            "a blacklisted (engine-rejected) hash must never be re-adopted as a mirror copy"
        );
    }

    #[tokio::test]
    async fn record_mirror_owned_reaps_blacklisted_phantom_mirror_records() {
        use crate::scraper::MediaKind;
        use crate::store::{AcquireRequest, OwnedRecord, OwnedStatus, Provenance, Store};
        use crate::vfs::{MediaMetadata, MediaType};
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap();
        let tmdb = TmdbClient::offline().await;
        let rec = |prov: Provenance| OwnedRecord {
            request: AcquireRequest {
                imdb_id: String::new(),
                tmdb_id: 603,
                kind: MediaKind::Movie,
                season: None,
                episode: None,
                original_language: None,
                spoken_languages: Some(vec![]),
                metadata: MediaMetadata {
                    title: "Movie".into(),
                    year: None,
                    media_type: MediaType::Movie,
                    external_id: Some("tmdb:603".into()),
                },
            },
            provenance: prov,
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![],
            quality: None,
        };
        // A phantom: a MIRROR (empty-provenance) owned record whose hash is blacklisted (e.g. left by
        // a staging-reject race). Must be reaped.
        store
            .put_owned("phantom".into(), rec(Provenance { entries: vec![] }))
            .await
            .unwrap();
        store
            .blacklist_add(
                crate::scraper::MediaKind::Movie,
                999,
                "phantom".into(),
                "WrongTitle",
                1,
            )
            .await
            .unwrap();
        // An ENGINE record (non-empty provenance) that is also blacklisted must NOT be reaped — its
        // lifecycle belongs to `fail_and_reacquire` (which may keep it for a delete-retry).
        store
            .put_owned("engine".into(), rec(Provenance::watchlist("alice")))
            .await
            .unwrap();
        store
            .blacklist_add(
                crate::scraper::MediaKind::Movie,
                603,
                "engine".into(),
                "BadAudio",
                1,
            )
            .await
            .unwrap();

        let prefs = crate::config::AcquisitionConfig::default().prefs;
        // No present torrents — the reaper runs over the owned table regardless of current_data.
        record_mirror_owned(&store, &tmdb, &prefs, &[]).await;

        assert!(
            store.get_owned("phantom".into()).await.is_none(),
            "a blacklisted mirror (empty-provenance) phantom record must be reaped"
        );
        assert!(
            store.get_owned("engine".into()).await.is_some(),
            "a blacklisted ENGINE record must be left for fail_and_reacquire to manage"
        );
    }

    /// Minimal `Verified` movie `OwnedRecord` with the given provenance, for the reaper tests.
    fn reaper_rec(tmdb: u64, prov: Provenance) -> OwnedRecord {
        OwnedRecord {
            request: AcquireRequest {
                imdb_id: String::new(),
                tmdb_id: tmdb,
                kind: crate::scraper::MediaKind::Movie,
                season: None,
                episode: None,
                original_language: None,
                spoken_languages: Some(vec![]),
                metadata: MediaMetadata {
                    title: "Movie".into(),
                    year: None,
                    media_type: MediaType::Movie,
                    external_id: Some(format!("tmdb:{tmdb}")),
                },
            },
            provenance: prov,
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![],
            quality: None,
        }
    }

    #[test]
    fn absent_mirror_records_to_reap_only_reaps_verified_empty_provenance_absent() {
        let mirror_absent = reaper_rec(1, Provenance { entries: vec![] });
        let mirror_present = reaper_rec(2, Provenance { entries: vec![] });
        let mut mirror_pending = reaper_rec(3, Provenance { entries: vec![] });
        mirror_pending.status = OwnedStatus::Pending; // observe owns absent Pending records
        let engine_absent = reaper_rec(4, Provenance::watchlist("alice")); // provenance → not a mirror
        let owned = vec![
            ("aaa".to_string(), mirror_absent),
            ("BBB".to_string(), mirror_present),
            ("ccc".to_string(), mirror_pending),
            ("ddd".to_string(), engine_absent),
        ];
        let present: std::collections::HashSet<String> = ["bbb".to_string()].into_iter().collect();
        let reap = absent_mirror_records_to_reap(&owned, &present, false);
        assert_eq!(
            reap,
            vec!["aaa".to_string()],
            "only a Verified empty-provenance record absent from the listing is reaped \
             (present mirror, Pending mirror, and engine records are all kept)"
        );
        // A wholesale-empty listing is untrustworthy: reap nothing.
        assert!(absent_mirror_records_to_reap(&owned, &Default::default(), true).is_empty());
    }

    #[tokio::test]
    async fn reap_absent_mirror_owned_drops_dangling_records_and_cleans_selection() {
        use crate::store::{SelectionEntry, Store};
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap();
        // An absent mirror record (empty provenance) with a selection entry → fully reaped.
        store
            .put_owned(
                "gone".into(),
                reaper_rec(10, Provenance { entries: vec![] }),
            )
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::movie_slot(10),
                SelectionEntry {
                    hash: "gone".into(),
                    file_path: "Movie.mkv".into(),
                },
            )
            .await
            .unwrap();
        // A present mirror record and an absent ENGINE record must both be kept.
        store
            .put_owned(
                "here".into(),
                reaper_rec(11, Provenance { entries: vec![] }),
            )
            .await
            .unwrap();
        store
            .put_owned("eng".into(), reaper_rec(12, Provenance::watchlist("bob")))
            .await
            .unwrap();

        let present: std::collections::HashSet<String> = ["here".to_string()].into_iter().collect();
        let reaped = reap_absent_mirror_owned(&store, &present, false).await;
        assert_eq!(reaped, 1);
        assert!(store.get_owned("gone".into()).await.is_none());
        assert!(
            store
                .get_selection(crate::store::movie_slot(10))
                .await
                .is_none(),
            "the reaped record's selection entry must be cleaned up too"
        );
        assert!(store.get_owned("here".into()).await.is_some());
        assert!(store.get_owned("eng".into()).await.is_some());
    }

    // ── plan_dedup (pure) ─────────────────────────────────────────────────────

    fn dedup_rec(
        kind: crate::scraper::MediaKind,
        tmdb: u64,
        provides: Vec<(u32, u32)>,
        score: Option<i64>,
    ) -> OwnedRecord {
        use crate::vfs::MediaMetadata;
        OwnedRecord {
            request: AcquireRequest {
                imdb_id: String::new(),
                tmdb_id: tmdb,
                kind,
                season: None,
                episode: None,
                original_language: None,
                spoken_languages: Some(vec![]),
                metadata: MediaMetadata {
                    title: "T".into(),
                    year: None,
                    media_type: media_type_of(kind),
                    external_id: Some(format!("tmdb:{tmdb}")),
                },
            },
            provenance: Provenance { entries: vec![] },
            added_at: 0,
            status: OwnedStatus::Verified,
            provides,
            // A KNOWN quality (1080p WEB) differing only in score, so these tests exercise the
            // score / selected / episode-count / hash axes; `None` is unknown (the lowest).
            quality: score.map(|s| qs(1080, 3_000, s)),
        }
    }

    fn hset(items: &[&str]) -> std::collections::HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// A cached quality snapshot with the given resolution / source tier / score.
    fn qs(resolution: u16, source_tier: i64, score: i64) -> crate::release::QualitySummary {
        crate::release::QualitySummary {
            cached: true,
            source_tier,
            resolution,
            score,
            unverified: false,
        }
    }

    /// `plan_dedup` (it never consults `MAX_RESOLUTION`).
    fn plan(
        owned: &[(String, OwnedRecord)],
        present: &std::collections::HashSet<String>,
        selected: &std::collections::HashSet<String>,
    ) -> Vec<DedupPlan> {
        plan_dedup(owned, present, selected)
    }

    /// A dedup fixture record with an explicit quality and provenance.
    fn qrec(
        kind: crate::scraper::MediaKind,
        tmdb: u64,
        provides: Vec<(u32, u32)>,
        quality: Option<crate::release::QualitySummary>,
        provenance: Provenance,
    ) -> OwnedRecord {
        OwnedRecord {
            quality,
            provenance,
            ..dedup_rec(kind, tmdb, provides, None)
        }
    }

    /// Every episode of `season` from `first` to `last` inclusive.
    fn eps(season: u32, first: u32, last: u32) -> Vec<(u32, u32)> {
        (first..=last).map(|e| (season, e)).collect()
    }

    /// The removal set of the plan for `tmdb` (empty when the planner proposes nothing for it).
    fn removed_for(plans: &[DedupPlan], tmdb: u64) -> Vec<String> {
        let mut rm: Vec<String> = plans
            .iter()
            .filter(|p| p.tmdb_id == tmdb)
            .flat_map(|p| p.remove.clone())
            .collect();
        rm.sort();
        rm
    }

    #[test]
    fn plan_dedup_movie_keeps_highest_quality_removes_rest() {
        use crate::scraper::MediaKind;
        let owned = vec![
            (
                "a".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(100)),
            ),
            (
                "b".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], None),
            ),
            (
                "c".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(50)),
            ),
        ];
        let plans = plan(&owned, &hset(&["a", "b", "c"]), &hset(&[]));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].keep, vec!["a"], "the highest-score hash is kept");
        let mut rm = plans[0].remove.clone();
        rm.sort();
        assert_eq!(rm, vec!["b", "c"]);
    }

    #[test]
    fn plan_dedup_movie_prefers_currently_served_only_among_equal_quality() {
        use crate::scraper::MediaKind;
        // Quality outranks `selected`: a better copy (higher score at the same resolution + tier) is
        // kept over the served one…
        let owned = vec![
            (
                "served".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(10)),
            ),
            (
                "better".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(99)),
            ),
        ];
        let plans = plan(&owned, &hset(&["served", "better"]), &hset(&["served"]));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].keep, vec!["better"], "quality first");
        assert_eq!(plans[0].remove, vec!["served"]);
        // …while between EQUAL-quality copies the served one is kept (no needless playback churn),
        // even though the other sorts first by hash.
        let owned = vec![
            (
                "aother".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(10)),
            ),
            (
                "zserved".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(10)),
            ),
        ];
        let plans = plan(&owned, &hset(&["aother", "zserved"]), &hset(&["zserved"]));
        assert_eq!(plans[0].keep, vec!["zserved"]);
        assert_eq!(plans[0].remove, vec!["aother"]);
    }

    #[test]
    fn plan_dedup_never_removes_a_manual_copy() {
        use crate::scraper::MediaKind;
        // A Manual copy ("man") plus two mirror duplicates. The `selected`, higher-score mirror
        // ("mir1") wins the keep slot — but the Manual copy must NOT be dedup-removed (M-1), even
        // though it loses the keep slot; only the genuinely-redundant mirror is removed.
        let mut man = dedup_rec(MediaKind::Movie, 1, vec![], Some(10));
        man.provenance = Provenance::manual();
        let owned = vec![
            ("man".to_string(), man),
            (
                "mir1".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(999)),
            ),
            (
                "mir2".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(1)),
            ),
        ];
        let plans = plan(&owned, &hset(&["man", "mir1", "mir2"]), &hset(&["mir1"]));
        assert_eq!(plans.len(), 1);
        assert!(
            !plans[0].remove.contains(&"man".to_string()),
            "a Manual copy must never be dedup-removed"
        );
        assert_eq!(
            plans[0].remove,
            vec!["mir2"],
            "only the redundant mirror is removed"
        );
        assert!(plans[0].keep.contains(&"man".to_string()));
        assert!(plans[0].keep.contains(&"mir1".to_string()));
    }

    #[test]
    fn plan_dedup_never_removes_a_manual_show_copy() {
        use crate::scraper::MediaKind;
        // M-1 for the SHOW branch (the Movie branch is covered by
        // `plan_dedup_never_removes_a_manual_copy`): a Manual show singleton that is FULLY covered by a
        // higher-priority pack must NEVER be dedup-removed, even though it is redundant — only the
        // genuinely-redundant NON-manual duplicate is removed. Guards both the greedy pass and the
        // second sweep (`plan_dedup`'s `unremovable` check, used by both); dropping it would delete
        // "man" here.
        let pack = dedup_rec(MediaKind::Series, 1, vec![(1, 1), (1, 2)], Some(999));
        let mut man = dedup_rec(MediaKind::Series, 1, vec![(1, 1)], Some(10));
        man.provenance = Provenance::manual();
        let dup = dedup_rec(MediaKind::Series, 1, vec![(1, 1)], Some(1));
        let owned = vec![
            ("pack".to_string(), pack),
            ("man".to_string(), man),
            ("dup".to_string(), dup),
        ];
        // `pack` is `selected` so it wins the keep slot; `man` and `dup` are both fully covered by it.
        let plans = plan(&owned, &hset(&["pack", "man", "dup"]), &hset(&["pack"]));
        assert_eq!(plans.len(), 1);
        assert!(
            !plans[0].remove.contains(&"man".to_string()),
            "a Manual show copy must never be dedup-removed, even when fully covered"
        );
        assert_eq!(
            plans[0].remove,
            vec!["dup"],
            "only the redundant non-manual duplicate is removed"
        );
        assert!(plans[0].keep.contains(&"pack".to_string()));
        assert!(plans[0].keep.contains(&"man".to_string()));
    }

    #[test]
    fn plan_dedup_show_pack_supersedes_redundant_single() {
        use crate::scraper::MediaKind;
        let owned = vec![
            (
                "pack".to_string(),
                dedup_rec(MediaKind::Series, 2, vec![(1, 1), (1, 2), (1, 3)], Some(10)),
            ),
            (
                "single".to_string(),
                dedup_rec(MediaKind::Series, 2, vec![(1, 1)], Some(5)),
            ),
        ];
        let plans = plan(&owned, &hset(&["pack", "single"]), &hset(&[]));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].keep, vec!["pack"]);
        assert_eq!(
            plans[0].remove,
            vec!["single"],
            "the single is fully covered by the pack → redundant"
        );
    }

    #[test]
    fn plan_dedup_show_reclaims_a_higher_priority_subset_covered_by_a_lower_priority_pack() {
        use crate::scraper::MediaKind;
        // TASK1 regression: a higher-SCORE single sorts BEFORE its lower-score covering pack, so the
        // greedy pass keeps BOTH. The second redundancy sweep must reclaim the single (fully covered
        // by the pack) rather than leaving it as a permanent missed dedup.
        let owned = vec![
            (
                "single".to_string(),
                dedup_rec(MediaKind::Series, 7, vec![(1, 1)], Some(100)),
            ),
            (
                "pack".to_string(),
                dedup_rec(MediaKind::Series, 7, vec![(1, 1), (1, 2)], Some(50)),
            ),
        ];
        let plans = plan(&owned, &hset(&["single", "pack"]), &hset(&[]));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].keep, vec!["pack"], "the covering pack is kept");
        assert_eq!(
            plans[0].remove,
            vec!["single"],
            "the higher-priority subset single is reclaimed (order-independent redundancy)"
        );
    }

    #[test]
    fn plan_dedup_show_keeps_a_mutually_covering_cycle_minus_the_redundant() {
        use crate::scraper::MediaKind;
        // A⊆B∪C, B⊆A∪C, C⊆A∪B — the second sweep must NOT strip all three (that would lose coverage);
        // it drops only the lowest-priority redundant one and keeps a covering set.
        let owned = vec![
            (
                "a".to_string(),
                dedup_rec(MediaKind::Series, 8, vec![(1, 1), (1, 2)], Some(30)),
            ),
            (
                "b".to_string(),
                dedup_rec(MediaKind::Series, 8, vec![(1, 2), (1, 3)], Some(20)),
            ),
            (
                "c".to_string(),
                dedup_rec(MediaKind::Series, 8, vec![(1, 1), (1, 3)], Some(10)),
            ),
        ];
        let plans = plan(&owned, &hset(&["a", "b", "c"]), &hset(&[]));
        // Coverage of the KEPT set must remain {(1,1),(1,2),(1,3)} — never lose an episode.
        let kept: std::collections::HashSet<&str> = plans
            .first()
            .map(|p| p.keep.iter().map(|s| s.as_str()).collect())
            .unwrap_or_else(|| ["a", "b", "c"].into_iter().collect());
        let mut covered = std::collections::HashSet::new();
        for (h, r) in &owned {
            if kept.contains(h.as_str()) {
                covered.extend(r.provides.iter().copied());
            }
        }
        assert!(
            [(1, 1), (1, 2), (1, 3)].iter().all(|e| covered.contains(e)),
            "the kept set must still cover every episode, got kept {kept:?}"
        );
    }

    #[test]
    fn plan_dedup_show_keeps_complementary_and_empty_provides() {
        use crate::scraper::MediaKind;
        // Complementary episode sets are NOT duplicates; an empty-provides hash is never removed.
        let owned = vec![
            (
                "a".to_string(),
                dedup_rec(MediaKind::Series, 3, vec![(1, 1), (1, 2)], Some(10)),
            ),
            (
                "b".to_string(),
                dedup_rec(MediaKind::Series, 3, vec![(1, 3), (1, 4)], Some(10)),
            ),
            (
                "unknown".to_string(),
                dedup_rec(MediaKind::Series, 3, vec![], None),
            ),
        ];
        let plans = plan(&owned, &hset(&["a", "b", "unknown"]), &hset(&[]));
        assert!(
            plans.is_empty(),
            "complementary packs + an unknown-coverage hash yield no removals"
        );
    }

    #[test]
    fn fresh_dedup_groups_flags_recently_staged_titles_only() {
        use crate::scraper::MediaKind;
        // A title with a record added "now" is fresh (an upgrade stage may be in flight → skip dedup);
        // a title whose records are all older than the idle window is not.
        let mut staged = dedup_rec(MediaKind::Movie, 100, vec![], Some(10));
        staged.added_at = 10_000; // "now"
        let mut old = dedup_rec(MediaKind::Movie, 200, vec![], Some(10));
        old.added_at = 1; // long ago
        let owned = vec![("a".to_string(), staged), ("b".to_string(), old)];
        let fresh = fresh_dedup_groups(&owned, /*now*/ 10_100, /*idle_secs*/ 300);
        assert!(
            fresh.contains(&(MediaType::Movie, 100)),
            "a title staged within the idle window is fresh"
        );
        assert!(
            !fresh.contains(&(MediaType::Movie, 200)),
            "a title untouched for longer than the idle window is not fresh"
        );
    }

    #[test]
    fn plan_dedup_keeps_the_better_copy_whether_mirror_or_engine() {
        use crate::scraper::MediaKind;
        // Pre-existing account-mirror copies (empty provenance) get NO dedup protection: the better
        // copy is kept whichever side it is on.
        let mirror = |q| {
            qrec(
                MediaKind::Movie,
                1,
                vec![],
                q,
                Provenance { entries: vec![] },
            )
        };
        let engine = |q| {
            qrec(
                MediaKind::Movie,
                1,
                vec![],
                q,
                Provenance::watchlist("alice"),
            )
        };
        let both = hset(&["mirror", "engine"]);
        // A pre-existing WEB copy loses to a better engine REMUX…
        let owned = vec![
            (
                "mirror".to_string(),
                mirror(Some(qs(1080, 3_000, 1_113_000))),
            ),
            (
                "engine".to_string(),
                engine(Some(qs(1080, 8_000, 1_118_000))),
            ),
        ];
        let plans = plan(&owned, &both, &hset(&["mirror"]));
        assert_eq!(plans[0].keep, vec!["engine"]);
        assert_eq!(plans[0].remove, vec!["mirror"]);
        // …and a pre-existing REMUX beats a worse engine WEB copy.
        let owned = vec![
            (
                "mirror".to_string(),
                mirror(Some(qs(1080, 8_000, 1_118_000))),
            ),
            (
                "engine".to_string(),
                engine(Some(qs(1080, 3_000, 1_113_000))),
            ),
        ];
        let plans = plan(&owned, &both, &hset(&["engine"]));
        assert_eq!(plans[0].keep, vec!["mirror"]);
        assert_eq!(plans[0].remove, vec!["engine"]);
        // A pre-existing copy with NO recorded quality is unknown — below any known quality, even a
        // lower resolution.
        let owned = vec![
            ("mirror".to_string(), mirror(None)),
            (
                "engine".to_string(),
                engine(Some(qs(720, 3_000, 1_075_000))),
            ),
        ];
        let plans = plan(&owned, &both, &hset(&["mirror"]));
        assert_eq!(plans[0].keep, vec!["engine"], "known before unknown");
        assert_eq!(plans[0].remove, vec!["mirror"]);
    }

    #[test]
    fn plan_dedup_show_quality_outranks_currently_served() {
        use crate::scraper::MediaKind;
        // Two packs with IDENTICAL coverage: the higher-tier one is kept even though the other is
        // the one being served (removal is idle-gated and the slots re-derive from the kept pack).
        let pack = |q| {
            qrec(
                MediaKind::Series,
                2,
                eps(1, 1, 2),
                q,
                Provenance::watchlist("a"),
            )
        };
        let owned = vec![
            ("served".to_string(), pack(Some(qs(1080, 3_000, 1_113_000)))),
            ("better".to_string(), pack(Some(qs(1080, 6_000, 1_116_000)))),
        ];
        let plans = plan(&owned, &hset(&["served", "better"]), &hset(&["served"]));
        assert_eq!(plans[0].keep, vec!["better"]);
        assert_eq!(plans[0].remove, vec!["served"]);
        // Equal quality (same rank AND score): the served pack is kept.
        let owned = vec![
            ("aother".to_string(), pack(Some(qs(1080, 6_000, 1_116_000)))),
            (
                "zserved".to_string(),
                pack(Some(qs(1080, 6_000, 1_116_000))),
            ),
        ];
        let plans = plan(&owned, &hset(&["aother", "zserved"]), &hset(&["zserved"]));
        assert_eq!(plans[0].keep, vec!["zserved"]);
        assert_eq!(plans[0].remove, vec!["aother"]);
    }

    #[test]
    fn plan_dedup_show_episode_count_tiebreak_keeps_larger_pack() {
        use crate::scraper::MediaKind;
        // `provides.len()` (episode count) is the keep-priority axis BELOW quality rank, score and
        // currently-served, and ABOVE the lexicographic hash tie-break. Three packs equal on all of
        // those: "aaa" (E1, E2), "bbb" (E2, E3) and "zzz" (E1-E3). The greedy pass keeps whatever sorts
        // first. With the episode-count axis "zzz" sorts first and covers both smaller packs, so they
        // go. Without it the hash order puts "aaa" and "bbb" first: they are kept (between them they
        // cover E1-E3), "zzz" adds nothing and goes, and neither small pack covers the other, so the
        // second sweep keeps both. The two outcomes differ, so this isolates the axis.
        let owned = vec![
            (
                "aaa".to_string(),
                dedup_rec(MediaKind::Series, 7, vec![(1, 1), (1, 2)], Some(10)),
            ),
            (
                "bbb".to_string(),
                dedup_rec(MediaKind::Series, 7, vec![(1, 2), (1, 3)], Some(10)),
            ),
            (
                "zzz".to_string(),
                dedup_rec(MediaKind::Series, 7, vec![(1, 1), (1, 2), (1, 3)], Some(10)),
            ),
        ];
        let plans = plan(&owned, &hset(&["aaa", "bbb", "zzz"]), &hset(&[]));
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].keep,
            vec!["zzz"],
            "the pack with the most episodes is kept (episode-count tie-break beats the hash order)"
        );
        assert_eq!(
            removed_for(&plans, 7),
            vec!["aaa".to_string(), "bbb".to_string()],
            "the smaller packs, both covered by it at equal quality, are removed"
        );
    }

    #[test]
    fn plan_dedup_ignores_absent_hashes_and_singletons() {
        use crate::scraper::MediaKind;
        let owned = vec![
            (
                "a".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(10)),
            ),
            (
                "b".to_string(),
                dedup_rec(MediaKind::Movie, 1, vec![], Some(5)),
            ),
        ];
        // Only `a` is present → no duplicate to act on (`b` is already gone from the provider).
        let plans = plan(&owned, &hset(&["a"]), &hset(&[]));
        assert!(plans.is_empty());
    }

    /// Replay of the 2026-11-11 incident: five titles where the old planner's second sweep deleted a
    /// BETTER copy because a worse one covered its episodes (the per-title data — provenance,
    /// `provides`, recorded quality, served slots — is the live library's at the time, but the
    /// titles, TMDB ids and 8-char hash prefixes are made up, in the same relative order). Under
    /// the quality-first keep-priority and the same-or-better second sweep none of those removals
    /// happens.
    #[test]
    fn plan_dedup_incident_replay_keeps_every_better_copy() {
        use crate::scraper::MediaKind::Series;
        let w = || Provenance::watchlist("u");
        let i = || Provenance::in_progress("u");
        let mirror = || Provenance { entries: vec![] };
        let q = |res: u16, tier: i64, score: i64, unverified: bool| {
            Some(crate::release::QualitySummary {
                unverified,
                ..qs(res, tier, score)
            })
        };
        let mut wi = w();
        wi.merge(&i());
        let show_b_pack: Vec<(u32, u32)> = (1..=4)
            .flat_map(|s| eps(s, 1, 10))
            .chain(eps(5, 1, 10))
            .chain(eps(6, 1, 13))
            .collect();
        let mut show_c_s1_web = eps(1, 1, 17);
        show_c_s1_web.extend(eps(1, 19, 22));
        let owned: Vec<(String, OwnedRecord)> = vec![
            // Show A (tmdb 900500): English 1080p WEB singles E01–E03 + the Russian SD .avi pack E01–E04.
            (
                "0c000000",
                qrec(
                    Series,
                    900500,
                    eps(1, 1, 4),
                    q(0, 3_000, 1_003_002, true),
                    wi,
                ),
            ),
            (
                "c0000000",
                qrec(
                    Series,
                    900500,
                    vec![(1, 3)],
                    q(1080, 3_000, 1_118_222, false),
                    w(),
                ),
            ),
            (
                "30000000",
                qrec(
                    Series,
                    900500,
                    vec![(1, 2)],
                    q(1080, 3_000, 1_118_302, false),
                    w(),
                ),
            ),
            (
                "b4000000",
                qrec(
                    Series,
                    900500,
                    vec![(1, 1)],
                    q(1080, 3_000, 1_118_328, false),
                    w(),
                ),
            ),
            // A mirror copy of E01 that had already left the account (absent → ignored).
            (
                "6c000000",
                qrec(
                    Series,
                    900500,
                    vec![(1, 1)],
                    q(1080, 3_000, 1_113_000, false),
                    mirror(),
                ),
            ),
            // Show B (tmdb 900200): S06 1080p REMUX + the S01–S06 1080p BluRay pack.
            (
                "60000000",
                qrec(
                    Series,
                    900200,
                    show_b_pack,
                    q(1080, 6_000, 1_122_052, false),
                    i(),
                ),
            ),
            (
                "cc000000",
                qrec(
                    Series,
                    900200,
                    eps(6, 1, 13),
                    q(1080, 8_000, 1_123_016, false),
                    i(),
                ),
            ),
            // Show C (tmdb 900100): a WEB copy (resolution untagged) + an untagged .avi pack.
            ("78000000", {
                let mut all = eps(1, 1, 22);
                all.extend(eps(2, 1, 13));
                qrec(Series, 900100, all, q(0, 0, 1_000_022, true), w())
            }),
            (
                "e4000000",
                qrec(
                    Series,
                    900100,
                    show_c_s1_web,
                    q(0, 3_000, 1_005_022, false),
                    w(),
                ),
            ),
            // Show D (tmdb 900400): S03E01–07 1080p WEB + unknown-quality copies.
            (
                "54000000",
                qrec(Series, 900400, eps(2, 1, 5), None, mirror()),
            ),
            (
                "d8000000",
                qrec(
                    Series,
                    900400,
                    eps(1, 1, 5),
                    q(1080, 0, 1_115_200, false),
                    i(),
                ),
            ),
            (
                "84000000",
                qrec(
                    Series,
                    900400,
                    vec![(2, 7)],
                    q(1080, 3_000, 1_113_434, false),
                    i(),
                ),
            ),
            (
                "24000000",
                qrec(
                    Series,
                    900400,
                    vec![(2, 6)],
                    q(1080, 3_000, 1_113_220, false),
                    i(),
                ),
            ),
            (
                "18000000",
                qrec(
                    Series,
                    900400,
                    eps(3, 1, 8),
                    q(720, 0, 1_079_004, false),
                    i(),
                ),
            ),
            (
                "48000000",
                qrec(
                    Series,
                    900400,
                    eps(3, 1, 7),
                    q(1080, 3_000, 1_118_022, false),
                    i(),
                ),
            ),
            ("90000000", qrec(Series, 900400, vec![], None, mirror())), // absent
            // Show E (tmdb 900300): pre-existing S01 + S03 1080p REMUX + the S01–S03 BluRay pack.
            ("3c000000", {
                let all: Vec<(u32, u32)> = (1..=3).flat_map(|s| eps(s, 1, 10)).collect();
                qrec(Series, 900300, all, q(1080, 6_000, 1_121_274, false), i())
            }),
            (
                "a8000000",
                qrec(
                    Series,
                    900300,
                    eps(1, 1, 10),
                    q(1080, 8_000, 1_118_000, false),
                    mirror(),
                ),
            ),
            (
                "9c000000",
                qrec(
                    Series,
                    900300,
                    eps(3, 1, 10),
                    q(1080, 8_000, 1_118_000, false),
                    mirror(),
                ),
            ),
        ]
        .into_iter()
        .map(|(h, r)| (h.to_string(), r))
        .collect();
        let present: std::collections::HashSet<String> = owned
            .iter()
            .map(|(h, _)| h.clone())
            .filter(|h| h != "6c000000" && h != "90000000")
            .collect();
        // Every engine copy served at least one slot; the mirror copies served none.
        let selected: std::collections::HashSet<String> = owned
            .iter()
            .filter(|(_, r)| !r.provenance.entries.is_empty())
            .map(|(h, _)| h.clone())
            .collect();
        let plans = plan(&owned, &present, &selected);
        // What the old planner deleted, per title.
        for (title, tmdb, deleted) in [
            ("Show A", 900500, &["c0000000", "30000000", "b4000000"][..]),
            ("Show B", 900200, &["cc000000"][..]),
            ("Show C", 900100, &["e4000000"][..]),
            ("Show D", 900400, &["48000000"][..]),
            ("Show E", 900300, &["a8000000", "9c000000"][..]),
        ] {
            let rm = removed_for(&plans, tmdb);
            for h in deleted {
                assert!(
                    !rm.contains(&h.to_string()),
                    "{title}: {h} must be kept, plan removes {rm:?}"
                );
            }
            assert!(
                rm.is_empty(),
                "{title}: nothing is redundant at same-or-better quality, got {rm:?}"
            );
        }
    }

    /// A South-Park-style library: a complete-series pack plus season packs inside it. A season pack
    /// at the SAME quality is a genuine duplicate (removed, mirror copies included); so is a worse
    /// one; a season pack of BETTER quality than the complete pack is kept, and so is an episode only
    /// one copy provides.
    #[test]
    fn plan_dedup_removes_equal_or_worse_season_packs_inside_a_complete_pack() {
        use crate::scraper::MediaKind::Series;
        let complete: Vec<(u32, u32)> = (1..=3).flat_map(|s| eps(s, 1, 10)).collect();
        let engine = || Provenance::in_progress("u");
        let mirror = || Provenance { entries: vec![] };
        let owned: Vec<(String, OwnedRecord)> = vec![
            (
                "complete",
                qrec(
                    Series,
                    2190,
                    complete,
                    Some(qs(1080, 6_000, 1_200_000)),
                    engine(),
                ),
            ),
            (
                "s1pack",
                qrec(
                    Series,
                    2190,
                    eps(1, 1, 10),
                    Some(qs(1080, 6_000, 1_190_000)),
                    engine(),
                ),
            ),
            (
                "s2pack",
                qrec(
                    Series,
                    2190,
                    eps(2, 1, 10),
                    Some(qs(1080, 6_000, 1_250_000)),
                    mirror(),
                ),
            ),
            (
                "s3web",
                qrec(
                    Series,
                    2190,
                    eps(3, 1, 10),
                    Some(qs(1080, 3_000, 1_113_000)),
                    engine(),
                ),
            ),
            (
                "s2remux",
                qrec(
                    Series,
                    2190,
                    eps(2, 1, 10),
                    Some(qs(1080, 8_000, 1_118_000)),
                    engine(),
                ),
            ),
            ("s4single", qrec(Series, 2190, vec![(4, 1)], None, mirror())),
        ]
        .into_iter()
        .map(|(h, r)| (h.to_string(), r))
        .collect();
        let all: std::collections::HashSet<String> = owned.iter().map(|(h, _)| h.clone()).collect();
        // The season packs are the ones being served.
        let plans = plan(&owned, &all, &hset(&["s1pack", "s2pack", "s3web"]));
        assert_eq!(removed_for(&plans, 2190), vec!["s1pack", "s2pack", "s3web"]);
        let mut keep = plans[0].keep.clone();
        keep.sort();
        assert_eq!(keep, vec!["complete", "s2remux", "s4single"]);
    }

    /// The second sweep removes a kept copy only when EVERY one of its episodes is provided by another
    /// kept copy of the same-or-better quality (rank: known, resolution, tier — score ignored).
    #[test]
    fn plan_dedup_second_sweep_needs_same_or_better_coverage() {
        use crate::scraper::MediaKind::Series;
        let p = || Provenance::watchlist("a");
        let all = hset(&["single", "pack"]);
        // A higher-tier single covered only by a lower-tier pack is kept…
        let owned = vec![
            (
                "single".to_string(),
                qrec(Series, 9, vec![(1, 1)], Some(qs(1080, 8_000, 1)), p()),
            ),
            (
                "pack".to_string(),
                qrec(Series, 9, eps(1, 1, 2), Some(qs(1080, 6_000, 2)), p()),
            ),
        ];
        assert!(
            plan(&owned, &all, &hset(&[])).is_empty(),
            "a worse cover never removes it"
        );
        // …and so is a known single covered only by an unknown-quality pack, whatever its score…
        let owned = vec![
            (
                "single".to_string(),
                qrec(Series, 9, vec![(1, 1)], Some(qs(720, 1_000, 1)), p()),
            ),
            (
                "pack".to_string(),
                qrec(Series, 9, eps(1, 1, 2), Some(qs(0, 8_000, 9_999_999)), p()),
            ),
        ];
        assert!(
            plan(&owned, &all, &hset(&[])).is_empty(),
            "unknown is below any known quality"
        );
        // …while an equal-rank pack reclaims it even with a LOWER score (score is ignored).
        let owned = vec![
            (
                "single".to_string(),
                qrec(Series, 9, vec![(1, 1)], Some(qs(1080, 6_000, 9)), p()),
            ),
            (
                "pack".to_string(),
                qrec(Series, 9, eps(1, 1, 2), Some(qs(1080, 6_000, 1)), p()),
            ),
        ];
        let plans = plan(&owned, &all, &hset(&["single"]));
        assert_eq!(removed_for(&plans, 9), vec!["single"]);
        // Coverage split across two kept copies: each episode needs a same-or-better provider.
        // E1 is covered by an equal pack, E2 only by a worse one → the single is kept.
        let owned = vec![
            (
                "single".to_string(),
                qrec(Series, 9, eps(1, 1, 2), Some(qs(1080, 6_000, 9)), p()),
            ),
            (
                "eq".to_string(),
                qrec(
                    Series,
                    9,
                    vec![(1, 1), (1, 3)],
                    Some(qs(1080, 6_000, 1)),
                    p(),
                ),
            ),
            (
                "worse".to_string(),
                qrec(
                    Series,
                    9,
                    vec![(1, 2), (1, 4)],
                    Some(qs(1080, 3_000, 1)),
                    p(),
                ),
            ),
        ];
        let plans = plan(&owned, &hset(&["single", "eq", "worse"]), &hset(&[]));
        assert!(
            removed_for(&plans, 9).is_empty(),
            "E2's only other provider is worse"
        );
    }

    /// Dedup never ranks by `MAX_RESOLUTION`: a copy above the ceiling is not ranked down for it (the
    /// corrective downgrade is the upgrade engine's job under `ALLOW_RESOLUTION_DOWNGRADE`), and one
    /// whose tier is untagged is not unknown-lowest either.
    #[test]
    fn plan_dedup_never_trades_an_over_ceiling_copy_down() {
        use crate::scraper::MediaKind::{Movie, Series};
        let p = || Provenance { entries: vec![] };
        for over in [qs(2160, 3_000, 1_216_000), qs(2160, 0, 1_216_000)] {
            let owned = vec![
                (
                    "uhd".to_string(),
                    qrec(Movie, 5, vec![], Some(over.clone()), p()),
                ),
                (
                    "remux".to_string(),
                    qrec(Movie, 5, vec![], Some(qs(1080, 8_000, 1_118_000)), p()),
                ),
            ];
            let plans = plan(&owned, &hset(&["uhd", "remux"]), &hset(&["remux"]));
            assert_eq!(
                plans[0].keep,
                vec!["uhd"],
                "{over:?} outranks a 1080p REMUX"
            );
            // A show pack above the ceiling, covered by within-ceiling REMUX packs, is kept too.
            let owned = vec![
                (
                    "uhd".to_string(),
                    qrec(Series, 6, eps(1, 1, 4), Some(over.clone()), p()),
                ),
                (
                    "r1".to_string(),
                    qrec(Series, 6, eps(1, 1, 2), Some(qs(1080, 8_000, 1)), p()),
                ),
                (
                    "r2".to_string(),
                    qrec(Series, 6, eps(1, 3, 4), Some(qs(1080, 8_000, 1)), p()),
                ),
            ];
            let plans = plan(&owned, &hset(&["uhd", "r1", "r2"]), &hset(&[]));
            assert!(
                !removed_for(&plans, 6).contains(&"uhd".to_string()),
                "{over:?} pack must not be removed in favour of 1080p packs"
            );
        }
    }

    /// Unknown quality (an untagged axis, or none recorded) is the lowest at a given resolution, but a
    /// KNOWN resolution stays a floor: dedup never removes a 1080p copy with an untagged source in
    /// favour of a 720p one. An untagged resolution is the lowest of all; a copy with no recorded
    /// quality is below everything.
    #[test]
    fn plan_dedup_orders_unknown_copies_below_known_ones() {
        use crate::scraper::MediaKind::Movie;
        let p = || Provenance { entries: vec![] };
        let owned = vec![
            ("none".to_string(), qrec(Movie, 7, vec![], None, p())),
            (
                "res0".to_string(),
                qrec(Movie, 7, vec![], Some(qs(0, 8_000, 9_999_999)), p()),
            ),
            (
                "tier0".to_string(),
                qrec(Movie, 7, vec![], Some(qs(1080, 0, 9_999_999)), p()),
            ),
            (
                "known720".to_string(),
                qrec(Movie, 7, vec![], Some(qs(720, 6_000, 1)), p()),
            ),
        ];
        let all = hset(&["none", "res0", "tier0", "known720", "known1080"]);
        let plans = plan(&owned, &all, &hset(&["none", "res0", "known720"]));
        assert_eq!(
            plans[0].keep,
            vec!["tier0"],
            "a known resolution is a floor: 1080p with an untagged source beats a 720p BluRay"
        );
        let mut with_known_1080 = owned.clone();
        with_known_1080.push((
            "known1080".to_string(),
            qrec(Movie, 7, vec![], Some(qs(1080, 1_000, 1)), p()),
        ));
        let plans = plan(&with_known_1080, &all, &hset(&["tier0"]));
        assert_eq!(
            plans[0].keep,
            vec!["known1080"],
            "at the same resolution a known source beats an untagged one, even served"
        );
        let low: Vec<_> = owned
            .iter()
            .filter(|(h, _)| h == "none" || h == "res0" || h == "known720")
            .cloned()
            .collect();
        let plans = plan(&low, &all, &hset(&["none", "res0"]));
        assert_eq!(
            plans[0].keep,
            vec!["known720"],
            "an untagged resolution is the lowest, whatever its tier"
        );
        let only: Vec<_> = owned
            .iter()
            .filter(|(h, _)| h == "none" || h == "res0")
            .cloned()
            .collect();
        let plans = plan(&only, &all, &hset(&["none"]));
        assert_eq!(
            plans[0].keep,
            vec!["res0"],
            "a recorded tier beats no quality at all"
        );
    }

    /// A show copy with a known resolution is never removed because a LOWER-resolution copy
    /// covers it, even when that copy's source is tagged and its own isn't.
    #[test]
    fn plan_dedup_never_removes_a_known_resolution_for_a_lower_one() {
        use crate::scraper::MediaKind::Series;
        let p = || Provenance { entries: vec![] };
        let owned = vec![
            (
                "single1080".to_string(),
                qrec(Series, 8, vec![(1, 1)], Some(qs(1080, 0, 1)), p()),
            ),
            (
                "pack720".to_string(),
                qrec(Series, 8, eps(1, 1, 3), Some(qs(720, 3_000, 9)), p()),
            ),
        ];
        let plans = plan(&owned, &hset(&["single1080", "pack720"]), &hset(&[]));
        assert!(
            removed_for(&plans, 8).is_empty(),
            "the 1080p single stays (better resolution) and the pack stays (sole holder of E2, E3)"
        );
    }

    /// At the same resolution and tier a checked copy outranks one whose tracks could not be
    /// checked (`unverified`). Phase (a2) splits an episode off an unverified pack onto a verified
    /// single of the same quality; dedup must not then remove that single because the pack still
    /// covers it — that would undo the upgrade for good (with no selection entry left, a2 never
    /// revisits the episode).
    #[test]
    fn plan_dedup_keeps_a_checked_single_over_an_unchecked_pack_of_equal_quality() {
        use crate::scraper::MediaKind::Series;
        let p = || Provenance { entries: vec![] };
        let mut unchecked = qs(1080, 3_000, 9);
        unchecked.unverified = true;
        let owned = vec![
            (
                "apack".to_string(),
                qrec(Series, 9, eps(1, 1, 3), Some(unchecked), p()),
            ),
            (
                "zsingle".to_string(),
                qrec(Series, 9, vec![(1, 1)], Some(qs(1080, 3_000, 9)), p()),
            ),
        ];
        let plans = plan(&owned, &hset(&["apack", "zsingle"]), &hset(&["zsingle"]));
        assert!(
            removed_for(&plans, 9).is_empty(),
            "the checked single is kept; the unchecked pack is the sole holder of E2, E3"
        );
        // A verified pack of the same quality still makes the single redundant.
        let mut checked = owned.clone();
        checked[0].1.quality.as_mut().unwrap().unverified = false;
        let plans = plan(&checked, &hset(&["apack", "zsingle"]), &hset(&[]));
        assert_eq!(removed_for(&plans, 9), vec!["zsingle".to_string()]);
    }

    /// Only SETTLED (`Verified`) copies take part in dedup. A `Pending` acquisition — still
    /// downloading (its acquire-time quality says `cached: false`), or downloaded but not yet
    /// probed / title-checked — must never win a keep slot over, or count as coverage for, a
    /// Verified copy: if it later stalls, dies or fails a check, observe deletes it as well and the
    /// title is left with no playable copy (for good, if the deleted copy was the user's own).
    /// Nor is a Pending copy ever removed — observe owns it until it settles, and dedup judges it
    /// then.
    #[test]
    fn plan_dedup_ignores_unsettled_pending_copies() {
        use crate::release::QualitySummary;
        use crate::scraper::MediaKind::{Movie, Series};
        let p = || Provenance { entries: vec![] };
        let pending = |rec: OwnedRecord| OwnedRecord {
            status: OwnedStatus::Pending,
            ..rec
        };
        let uncached_remux = QualitySummary {
            cached: false,
            ..qs(1080, 8_000, 5)
        };

        // Movie: a served, Verified cached 1080p WEB copy + a better but still-downloading REMUX.
        let owned = vec![
            (
                "verified".to_string(),
                qrec(Movie, 1, vec![], Some(qs(1080, 3_000, 10)), p()),
            ),
            (
                "pending".to_string(),
                pending(qrec(Movie, 1, vec![], Some(uncached_remux.clone()), p())),
            ),
        ];
        let plans = plan(
            &owned,
            &hset(&["verified", "pending"]),
            &hset(&["verified"]),
        );
        assert!(
            removed_for(&plans, 1).is_empty(),
            "an unsettled copy never displaces a settled one: {plans:?}"
        );

        // …and a Pending copy is never removed in favour of a better Verified one either; only the
        // genuinely redundant settled copy goes.
        let owned = vec![
            (
                "best".to_string(),
                qrec(Movie, 2, vec![], Some(qs(1080, 8_000, 10)), p()),
            ),
            (
                "worse".to_string(),
                qrec(Movie, 2, vec![], Some(qs(720, 3_000, 10)), p()),
            ),
            (
                "pending".to_string(),
                pending(qrec(Movie, 2, vec![], Some(qs(720, 1_000, 1)), p())),
            ),
        ];
        let plans = plan(&owned, &hset(&["best", "worse", "pending"]), &hset(&[]));
        assert_eq!(removed_for(&plans, 2), vec!["worse"]);

        // Show: a Pending REMUX season pack neither covers the Verified WEB singles (not removed)…
        let owned = vec![
            (
                "e1".to_string(),
                qrec(Series, 3, vec![(1, 1)], Some(qs(1080, 3_000, 10)), p()),
            ),
            (
                "e2".to_string(),
                qrec(Series, 3, vec![(1, 2)], Some(qs(1080, 3_000, 10)), p()),
            ),
            (
                "pack".to_string(),
                pending(qrec(Series, 3, eps(1, 1, 2), Some(uncached_remux), p())),
            ),
        ];
        let plans = plan(&owned, &hset(&["e1", "e2", "pack"]), &hset(&["e1", "e2"]));
        assert!(removed_for(&plans, 3).is_empty(), "{plans:?}");

        // …nor is a Pending single removed while a better Verified pack covers it.
        let owned = vec![
            (
                "pack".to_string(),
                qrec(Series, 4, eps(1, 1, 2), Some(qs(1080, 8_000, 10)), p()),
            ),
            (
                "e1".to_string(),
                pending(qrec(Series, 4, vec![(1, 1)], Some(qs(720, 3_000, 1)), p())),
            ),
        ];
        let plans = plan(&owned, &hset(&["pack", "e1"]), &hset(&["pack"]));
        assert!(removed_for(&plans, 4).is_empty(), "{plans:?}");
    }
}

#[cfg(test)]
mod provider_abstraction_tests {
    use super::*;
    use crate::provider::{DebridProvider, MockProvider};
    use crate::repair::RepairManager;
    use crate::tmdb_client::TmdbClient;
    use crate::vfs::DebridVfs;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[test]
    fn scan_config_holds_app_state() {
        use crate::app_state::AppState;
        use crate::config::Config;
        use crate::store::Store;

        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let db = Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let store = Store::from_database(db).unwrap();
        let config = Config::from_parts(
            None,
            Some("tb".to_string()),
            Some("k".to_string()),
            None,
            None,
            None,
        )
        .unwrap();
        let scraper: std::sync::Arc<dyn crate::scraper::Scraper> =
            std::sync::Arc::new(crate::scraper::TorrentioScraper::new(
                None,
                crate::provider::ProviderKind::TorBox,
                "tok",
                reqwest::Client::new(),
            ));
        let validator: std::sync::Arc<dyn crate::acquire::TitleValidator> =
            std::sync::Arc::new(crate::acquire::TmdbTitleValidator {
                tmdb: std::sync::Arc::new(TmdbClient::new("k".to_string()).unwrap()),
            });
        let prober: std::sync::Arc<dyn crate::acquire::Prober> =
            std::sync::Arc::new(crate::acquire::HttpProber {
                http: reqwest::Client::new(),
            });
        let engine = std::sync::Arc::new(crate::acquire::AcquisitionEngine::new(
            provider.clone(),
            scraper.clone(),
            validator,
            prober,
            store.clone(),
            crate::config::AcquisitionConfig::default().prefs,
            5,
            std::time::Duration::from_secs(1800),
            std::time::Duration::from_secs(600),
        ));
        let app = AppState {
            provider: provider.clone(),
            tmdb_client: Arc::new(TmdbClient::new("k".to_string()).unwrap()),
            vfs: Arc::new(RwLock::new(DebridVfs::new())),
            store,
            repair_manager: Arc::new(RepairManager::new(provider)),
            config: Arc::new(config),
            jellyfin_client: None,
            http_client: reqwest::Client::new(),
            scraper,
            engine,
            trakt_client: None,
            read_activity: Arc::new(crate::read_activity::ReadActivity::without_startup_grace()),
        };
        let _config = ScanConfig { app };
    }
}

#[cfg(test)]
mod trakt_sync_tests {
    use super::*;
    use crate::store::{Store, TraktTokens, WantedRecord, WantedSources, WatchedState};
    use crate::tmdb_client::{ShowStatus, TmdbClient};
    use crate::trakt_client::{
        MockTrakt, TraktClient, TraktItem, TraktTokenResponse, WatchedData, WatchedShow,
    };
    use crate::vfs::MediaType;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn mem_store() -> Store {
        let db = Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        Store::from_database(db).unwrap()
    }

    fn item(media_type: MediaType, tmdb_id: u64) -> TraktItem {
        TraktItem {
            media_type,
            tmdb_id,
            imdb_id: None,
        }
    }

    fn item_imdb(media_type: MediaType, tmdb_id: u64, imdb_id: &str) -> TraktItem {
        TraktItem {
            media_type,
            tmdb_id,
            imdb_id: Some(imdb_id.to_string()),
        }
    }

    fn tokens(access: &str, expires_at: u64, needs_reenrolment: bool) -> TraktTokens {
        TraktTokens {
            access: access.to_string(),
            refresh: "ref".to_string(),
            expires_at,
            username: "alice".to_string(),
            needs_reenrolment,
        }
    }

    fn movie_wanted_row(user: &str, tmdb_id: u64) -> WantedRecord {
        WantedRecord {
            user: user.to_string(),
            tmdb_id,
            media_type: MediaType::Movie,
            sources: WantedSources {
                watchlist: true,
                in_progress: false,
            },
            watched_state: WatchedState::Movie { watched: false },
            show_status: None,
            imdb_id: None,
        }
    }

    // ── build_wanted (pure) ───────────────────────────────────────────────────

    #[test]
    fn build_wanted_watchlist_movie() {
        let got = build_wanted(
            "alice",
            &[item(MediaType::Movie, 27205)],
            &[],
            &WatchedData::default(),
            &HashMap::new(),
            None,
            None,
        );
        assert_eq!(
            got,
            vec![WantedRecord {
                user: "alice".to_string(),
                tmdb_id: 27205,
                media_type: MediaType::Movie,
                sources: WantedSources {
                    watchlist: true,
                    in_progress: false
                },
                watched_state: WatchedState::Movie { watched: false },
                show_status: None,
                imdb_id: None,
            }]
        );
    }

    #[test]
    fn build_wanted_watchlist_show_with_status_and_watched_episodes() {
        let watched = WatchedData {
            movies: vec![],
            shows: vec![WatchedShow {
                tmdb_id: 1396,
                trakt_id: None,
                imdb_id: None,
                watched_episodes: vec![(1, 1), (1, 2)],
                last_watched_at: None,
            }],
        };
        let mut status = HashMap::new();
        status.insert(1396u64, ShowStatus::Ended);
        let got = build_wanted(
            "alice",
            &[item(MediaType::Show, 1396)],
            &[],
            &watched,
            &status,
            None,
            None,
        );
        assert_eq!(
            got,
            vec![WantedRecord {
                user: "alice".to_string(),
                tmdb_id: 1396,
                media_type: MediaType::Show,
                sources: WantedSources {
                    watchlist: true,
                    in_progress: false
                },
                watched_state: WatchedState::Show {
                    watched_episodes: vec![(1, 1), (1, 2)]
                },
                show_status: Some(ShowStatus::Ended),
                imdb_id: None,
            }]
        );
    }

    #[test]
    fn build_wanted_movie_and_show_with_same_tmdb_id_are_separate_records() {
        // A movie and a show that share a numeric TMDB id must NOT be merged into one record.
        let mut status = HashMap::new();
        status.insert(1396u64, ShowStatus::Ended);
        let got = build_wanted(
            "alice",
            &[item(MediaType::Movie, 1396)],
            &[item(MediaType::Show, 1396)],
            &WatchedData::default(),
            &status,
            None,
            None,
        );
        assert_eq!(got.len(), 2, "movie and show must be distinct records");
        let movie = got
            .iter()
            .find(|r| r.media_type == MediaType::Movie)
            .expect("movie record");
        let show = got
            .iter()
            .find(|r| r.media_type == MediaType::Show)
            .expect("show record");
        assert_eq!(movie.tmdb_id, 1396);
        assert!(movie.sources.watchlist && !movie.sources.in_progress);
        assert_eq!(show.tmdb_id, 1396);
        assert!(show.sources.in_progress && !show.sources.watchlist);
        // Movies sort before shows (Ord by MediaType declaration order).
        assert_eq!(got[0].media_type, MediaType::Movie);
        assert_eq!(got[1].media_type, MediaType::Show);
    }

    #[test]
    fn build_wanted_title_in_both_sources_sets_both_flags() {
        let got = build_wanted(
            "alice",
            &[item(MediaType::Movie, 100)],
            &[item(MediaType::Movie, 100)],
            &WatchedData::default(),
            &HashMap::new(),
            None,
            None,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].sources,
            WantedSources {
                watchlist: true,
                in_progress: true
            }
        );
    }

    #[test]
    fn build_wanted_in_progress_only_show_sets_in_progress_and_absent_status_none() {
        let got = build_wanted(
            "alice",
            &[],
            &[item(MediaType::Show, 200)],
            &WatchedData::default(),
            &HashMap::new(),
            None,
            None,
        );
        assert_eq!(
            got,
            vec![WantedRecord {
                user: "alice".to_string(),
                tmdb_id: 200,
                media_type: MediaType::Show,
                sources: WantedSources {
                    watchlist: false,
                    in_progress: true
                },
                watched_state: WatchedState::Show {
                    watched_episodes: vec![]
                },
                show_status: None,
                imdb_id: None,
            }]
        );
    }

    #[test]
    fn build_wanted_carries_imdb_from_trakt_item() {
        let got = build_wanted(
            "alice",
            &[item_imdb(MediaType::Movie, 27205, "tt1375666")],
            &[],
            &WatchedData::default(),
            &HashMap::new(),
            None,
            None,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].imdb_id, Some("tt1375666".into()));
    }

    #[test]
    fn build_wanted_watched_movie_marks_watched() {
        let watched = WatchedData {
            movies: vec![27205],
            shows: vec![],
        };
        let got = build_wanted(
            "alice",
            &[item(MediaType::Movie, 27205)],
            &[],
            &watched,
            &HashMap::new(),
            None,
            None,
        );
        assert_eq!(got[0].watched_state, WatchedState::Movie { watched: true });
    }

    #[test]
    fn build_wanted_catchup_marks_watched_show_in_progress_and_respects_lookback() {
        use crate::trakt_client::{WatchedData, WatchedShow};
        let watched = WatchedData {
            movies: vec![],
            shows: vec![WatchedShow {
                tmdb_id: 1396,
                trakt_id: Some(1390),
                imdb_id: None,
                watched_episodes: vec![(1, 1)],
                last_watched_at: Some(1_000_000),
            }],
        };
        let mut status = HashMap::new();
        status.insert(1396u64, ShowStatus::Returning);
        // all-time (None) + no behind-gate (None) → lookback-only: the watched show is wanted via
        // catch-up (in_progress, not watchlist)
        let all = build_wanted("alice", &[], &[], &watched, &status, None, None);
        assert_eq!(all.len(), 1);
        assert_eq!(
            (all[0].tmdb_id, &all[0].media_type),
            (1396, &MediaType::Show)
        );
        assert!(all[0].sources.in_progress && !all[0].sources.watchlist);
        // cutoff AFTER last_watched_at → excluded (watched too long ago)
        assert!(
            build_wanted("alice", &[], &[], &watched, &status, Some(2_000_000), None).is_empty()
        );
        // cutoff BEFORE last_watched_at → included
        assert_eq!(
            build_wanted("alice", &[], &[], &watched, &status, Some(500_000), None).len(),
            1
        );
    }

    #[test]
    fn finished_ended_candidates_keeps_only_ended() {
        use crate::tmdb_client::ShowStatus;
        let caught_up: std::collections::HashSet<u64> = [1, 2, 3, 4].into_iter().collect();
        let mut statuses = HashMap::new();
        statuses.insert(1u64, ShowStatus::Ended);
        statuses.insert(2u64, ShowStatus::Returning);
        statuses.insert(3u64, ShowStatus::Other);
        // 4 has no status (TMDB hiccup) → treated as not-Ended.
        let got = finished_ended_candidates(&caught_up, &statuses);
        assert_eq!(got, [1u64].into_iter().collect());
    }

    #[test]
    fn shows_to_fold_includes_finished_only_when_enabled() {
        let behind: std::collections::HashSet<u64> = [10].into_iter().collect();
        let finished: std::collections::HashSet<u64> = [20].into_iter().collect();
        // Disabled → only the behind catch-up shows fold (finished-ended are preview-only).
        assert_eq!(
            shows_to_fold(&behind, &finished, false),
            [10u64].into_iter().collect()
        );
        // Enabled → finished-ended owned shows also fold (so Trigger-A removes them).
        assert_eq!(
            shows_to_fold(&behind, &finished, true),
            [10u64, 20u64].into_iter().collect()
        );
    }

    #[test]
    fn build_wanted_catchup_only_show_carries_trakt_imdb_id() {
        use crate::trakt_client::{WatchedData, WatchedShow};
        // A show that enters the wanted-set ONLY via catch-up (watched + behind, NOT
        // watchlisted/in-progress, so no TraktItem contributes an IMDB id) must still carry Trakt's
        // IMDB id — otherwise a TVDB-only show (whose TMDB IMDB mapping is missing) resolves to no
        // IMDB id and is silently skipped by the acquire path.
        let watched = WatchedData {
            movies: vec![],
            shows: vec![WatchedShow {
                tmdb_id: 1396,
                trakt_id: Some(1390),
                imdb_id: Some("tt0903747".into()),
                watched_episodes: vec![(1, 1)],
                last_watched_at: Some(1_000_000),
            }],
        };
        let mut status = HashMap::new();
        status.insert(1396u64, ShowStatus::Returning);
        let got = build_wanted("alice", &[], &[], &watched, &status, None, None);
        assert_eq!(got.len(), 1);
        assert!(got[0].sources.in_progress && !got[0].sources.watchlist);
        assert_eq!(
            got[0].imdb_id.as_deref(),
            Some("tt0903747"),
            "a catch-up-only show must carry Trakt's IMDB id so it can be scraped"
        );
    }

    #[test]
    fn build_wanted_catchup_behind_gate_excludes_caught_up_show() {
        use crate::trakt_client::{WatchedData, WatchedShow};
        let watched = WatchedData {
            movies: vec![],
            shows: vec![WatchedShow {
                tmdb_id: 1396,
                trakt_id: Some(1390),
                imdb_id: None,
                watched_episodes: vec![(1, 1)],
                last_watched_at: Some(1_000_000),
            }],
        };
        let status = HashMap::new();
        // Behind-set EXCLUDES this show (caught up) → not wanted, even within lookback.
        let empty: std::collections::HashSet<u64> = std::collections::HashSet::new();
        assert!(build_wanted("alice", &[], &[], &watched, &status, None, Some(&empty)).is_empty());
        // Behind-set INCLUDES this show → wanted via catch-up.
        let behind: std::collections::HashSet<u64> = [1396].into_iter().collect();
        let got = build_wanted("alice", &[], &[], &watched, &status, None, Some(&behind));
        assert_eq!(got.len(), 1);
        assert!(got[0].sources.in_progress && !got[0].sources.watchlist);
    }

    #[test]
    fn build_wanted_is_sorted_by_tmdb_id() {
        let wl = vec![
            item(MediaType::Movie, 30),
            item(MediaType::Movie, 10),
            item(MediaType::Movie, 20),
        ];
        let got = build_wanted(
            "alice",
            &wl,
            &[],
            &WatchedData::default(),
            &HashMap::new(),
            None,
            None,
        );
        assert_eq!(
            got.iter().map(|r| r.tmdb_id).collect::<Vec<_>>(),
            vec![10, 20, 30]
        );
    }

    // ── sync_trakt (async, MockTrakt + mem Store) ─────────────────────────────

    #[tokio::test]
    async fn sync_trakt_success_populates_wanted_and_leaves_flag_false() {
        let store = mem_store();
        store
            .put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, false))
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData {
                movies: vec![],
                shows: vec![],
            },
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        let w = store
            .get_wanted("alice".to_string(), MediaType::Movie, 27205)
            .await
            .expect("wanted present");
        assert!(w.sources.watchlist);
        assert!(
            !store
                .get_trakt_tokens("alice".to_string())
                .await
                .unwrap()
                .needs_reenrolment
        );
    }

    #[tokio::test]
    async fn sync_trakt_catchup_excludes_caught_up_includes_behind_show() {
        use crate::trakt_client::{ShowProgress, WatchedShow};
        let store = mem_store();
        store
            .put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, false))
            .await
            .unwrap();
        // Two watched shows: tmdb 500 (Trakt 100) is caught up; tmdb 600 (Trakt 200) is behind.
        let watched = WatchedData {
            movies: vec![],
            shows: vec![
                WatchedShow {
                    tmdb_id: 500,
                    trakt_id: Some(100),
                    imdb_id: None,
                    watched_episodes: vec![(1, 1)],
                    last_watched_at: Some(1_000_000),
                },
                WatchedShow {
                    tmdb_id: 600,
                    trakt_id: Some(200),
                    imdb_id: None,
                    watched_episodes: vec![(1, 1)],
                    last_watched_at: Some(1_000_000),
                },
            ],
        };
        let progress = [
            (
                100u64,
                ShowProgress {
                    aired: 10,
                    completed: 10,
                    behind: false,
                },
            ),
            (
                200u64,
                ShowProgress {
                    aired: 10,
                    completed: 4,
                    behind: true,
                },
            ),
        ]
        .into_iter()
        .collect();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            watched,
            progress,
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        // All-time lookback (None): the gate is purely "behind?".
        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        assert!(
            store
                .get_wanted("alice".to_string(), MediaType::Show, 600)
                .await
                .is_some_and(|w| w.sources.in_progress),
            "the behind show must be caught up (wanted via in_progress)"
        );
        assert!(
            store
                .get_wanted("alice".to_string(), MediaType::Show, 500)
                .await
                .is_none(),
            "the caught-up show must NOT be wanted (no fruitless catch-up)"
        );
    }

    /// Finish cleanup with a real TMDB status: an owned show that Trakt reports caught up and TMDB
    /// reports `Ended` is folded into the wanted-set only when `REMOVE_FINISHED_SHOWS` is on; a
    /// `Returning` one never is.
    #[tokio::test]
    async fn sync_trakt_folds_a_finished_ended_owned_show_only_when_enabled() {
        use crate::scraper::MediaKind;
        use crate::store::{AcquireRequest, OwnedRecord, OwnedStatus, Provenance};
        use crate::trakt_client::test_http::{spawn, Reply};
        use crate::trakt_client::{ShowProgress, WatchedShow};
        let server = spawn(vec![
            ("/tv/500?api_key=k", Reply::json(r#"{"status":"Ended"}"#)),
            (
                "/tv/501?api_key=k",
                Reply::json(r#"{"status":"Returning Series"}"#),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        let owned_show = |tmdb_id: u64| OwnedRecord {
            request: AcquireRequest {
                imdb_id: format!("tt{tmdb_id}"),
                tmdb_id,
                kind: MediaKind::Series,
                season: None,
                episode: None,
                original_language: None,
                spoken_languages: Some(vec![]),
                metadata: crate::vfs::MediaMetadata {
                    title: "Show".into(),
                    year: None,
                    media_type: MediaType::Show,
                    external_id: Some(format!("tmdb:{tmdb_id}")),
                },
            },
            provenance: Provenance::watchlist("alice"),
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![(1, 1)],
            quality: None,
        };
        let watched_show = |tmdb_id: u64, trakt_id: u64| WatchedShow {
            tmdb_id,
            trakt_id: Some(trakt_id),
            imdb_id: None,
            watched_episodes: vec![(1, 1)],
            last_watched_at: Some(1_000_000),
        };
        let caught_up = ShowProgress {
            aired: 1,
            completed: 1,
            behind: false,
        };
        for remove_finished_shows in [false, true] {
            let store = mem_store();
            store
                .put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, false))
                .await
                .unwrap();
            for id in [500, 501] {
                store
                    .put_owned(format!("h{id}"), owned_show(id))
                    .await
                    .unwrap();
            }
            let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
                watched: WatchedData {
                    movies: vec![],
                    shows: vec![watched_show(500, 100), watched_show(501, 101)],
                },
                progress: [(100u64, caught_up.clone()), (101u64, caught_up.clone())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            });

            sync_trakt(&trakt, &tmdb, &store, None, remove_finished_shows).await;

            let ended = store
                .get_wanted("alice".to_string(), MediaType::Show, 500)
                .await;
            assert_eq!(
                ended.is_some(),
                remove_finished_shows,
                "the Ended show is folded iff REMOVE_FINISHED_SHOWS is on"
            );
            if let Some(w) = ended {
                assert_eq!(w.show_status, Some(ShowStatus::Ended));
            }
            assert!(
                store
                    .get_wanted("alice".to_string(), MediaType::Show, 501)
                    .await
                    .is_none(),
                "a Returning show is never a finish-removal candidate"
            );
        }
    }

    #[tokio::test]
    async fn sync_trakt_refreshes_near_expiry_token() {
        let store = mem_store();
        store
            .put_trakt_tokens("alice".to_string(), tokens("old", 0, false))
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            token: TraktTokenResponse {
                access_token: "REFRESHED".into(),
                refresh_token: "newref".into(),
                expires_in: 86_400, // Trakt's 24 h access-token lifetime
                created_at: 1_700_000_000,
            },
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData::default(),
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        let tok = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert_eq!(
            tok.access, "REFRESHED",
            "refresh must have run and persisted"
        );
        assert!(store
            .get_wanted("alice".to_string(), MediaType::Movie, 27205)
            .await
            .is_some());
    }

    /// Trakt access tokens have lasted 24 h since March 2025. A token refreshed one sync tick ago
    /// still has ~23.75 h to run, so the next sync must NOT refresh it again: each refresh rotates
    /// the single-use refresh token, and a rotation lost to a failed write needs re-enrolment.
    #[tokio::test]
    async fn sync_trakt_does_not_refresh_a_freshly_refreshed_24h_token() {
        let store = mem_store();
        let expires_at = crate::now_unix_secs() + 86_400 - 900;
        store
            .put_trakt_tokens("alice".to_string(), tokens("acc", expires_at, false))
            .await
            .unwrap();
        // A refresh would fail and flag the account, so a clean sync proves none was attempted.
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            fail_refresh: true,
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData::default(),
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        let summary = sync_trakt(&trakt, &tmdb, &store, None, false).await;

        assert_eq!(summary.succeeded, 1);
        let tok = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert!(!tok.needs_reenrolment, "no refresh was attempted");
        assert_eq!(tok.access, "acc");
        assert!(store
            .get_wanted("alice".to_string(), MediaType::Movie, 27205)
            .await
            .is_some());
    }

    #[tokio::test]
    async fn sync_trakt_refresh_missing_created_at_anchors_expiry_on_wall_clock() {
        // Regression: a refresh response that omits `created_at` (parsed to `0`) must anchor the
        // stored expiry on the wall clock via `enrolment::token_expires_at`, NOT `0 + expires_in`
        // (which lands just after the 1970 epoch → reads as permanently expired → refresh churn
        // on every sync tick). Mirrors `enrolment`'s two token-write paths.
        let store = mem_store();
        store
            .put_trakt_tokens("alice".to_string(), tokens("old", 0, false))
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            token: TraktTokenResponse {
                access_token: "REFRESHED".into(),
                refresh_token: "newref".into(),
                expires_in: 7_776_000,
                created_at: 0, // Trakt omitted it
            },
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData::default(),
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        let before = crate::now_unix_secs();
        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        let tok = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert_eq!(
            tok.access, "REFRESHED",
            "refresh must have run and persisted"
        );
        // Wall-clock-anchored: expiry is roughly `now + expires_in`, far above the epoch-anchored
        // `0 + expires_in` (7_776_000) the buggy inline calc would have produced.
        assert!(
            tok.expires_at >= before.saturating_add(7_776_000),
            "expiry must anchor on the wall clock (got {}, expected >= {})",
            tok.expires_at,
            before.saturating_add(7_776_000)
        );
    }

    #[tokio::test]
    async fn sync_trakt_does_not_resurrect_account_removed_during_refresh() {
        // A near-expiry token triggers refresh; if the account is removed (a concurrent `/remove` on
        // the enrolment page) during the refresh network window, sync_trakt_user must NOT resurrect it
        // by writing the refreshed token back — mirrors the enrolment::refresh_account guard.
        use crate::error::AppError;
        use crate::trakt_client::{DeviceCode, DeviceTokenPoll, ShowProgress, TraktUser};
        struct RemovingTrakt {
            store: Store,
        }
        #[async_trait::async_trait]
        impl TraktClient for RemovingTrakt {
            async fn refresh(&self, _rt: &str) -> Result<TraktTokenResponse, AppError> {
                self.store
                    .remove_trakt_tokens("alice".into())
                    .await
                    .unwrap();
                Ok(TraktTokenResponse {
                    access_token: "FRESH".into(),
                    refresh_token: "NEWREF".into(),
                    expires_in: 100,
                    created_at: 2_000,
                })
            }
            async fn device_code(&self) -> Result<DeviceCode, AppError> {
                unreachable!()
            }
            async fn poll_token(&self, _d: &str) -> Result<DeviceTokenPoll, AppError> {
                unreachable!()
            }
            // Reads must NOT run once the post-refresh removed-account guard fires.
            async fn me(&self, _a: &str) -> Result<TraktUser, AppError> {
                unreachable!()
            }
            async fn watchlist(&self, _a: &str) -> Result<Vec<TraktItem>, AppError> {
                unreachable!()
            }
            async fn in_progress(&self, _a: &str) -> Result<Vec<TraktItem>, AppError> {
                unreachable!()
            }
            async fn watched(&self, _a: &str) -> Result<WatchedData, AppError> {
                unreachable!()
            }
            async fn show_progress(&self, _a: &str, _t: u64) -> Result<ShowProgress, AppError> {
                unreachable!()
            }
        }
        let store = mem_store();
        store
            .put_trakt_tokens("alice".into(), tokens("old", 0, false))
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(RemovingTrakt {
            store: store.clone(),
        });
        let tmdb = TmdbClient::offline().await;

        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        assert!(
            store.get_trakt_tokens("alice".to_string()).await.is_none(),
            "an account removed during refresh must not be resurrected"
        );
    }

    #[tokio::test]
    async fn cas_skip_on_concurrent_rotation_is_not_counted_as_a_successful_sync() {
        // A near-expiry token triggers refresh; a concurrent /refresh rotates the stored (single-use)
        // refresh token during the network window, so sync_trakt_user's refresh CAS — keyed on our
        // pre-refresh token — is a no-op and it returns Skipped WITHOUT materialising `wanted`. That
        // skip must NOT count toward `succeeded`, or `removals_trustworthy()` would treat an
        // unconfirmed (possibly stale/empty) wanted set as safe to drive removals.
        use crate::error::AppError;
        use crate::trakt_client::{DeviceCode, DeviceTokenPoll, ShowProgress, TraktUser};
        struct RotatingTrakt {
            store: Store,
        }
        #[async_trait::async_trait]
        impl TraktClient for RotatingTrakt {
            async fn refresh(&self, _rt: &str) -> Result<TraktTokenResponse, AppError> {
                // Concurrent refresh rotates the stored refresh token to a DIFFERENT value, so the
                // caller's CAS (keyed on the original "ref") will find a mismatch and skip.
                self.store
                    .put_trakt_tokens(
                        "alice".into(),
                        TraktTokens {
                            access: "OTHER".into(),
                            refresh: "ROTATED".into(),
                            expires_at: 9_999_999_999,
                            username: "alice".into(),
                            needs_reenrolment: false,
                        },
                    )
                    .await
                    .unwrap();
                Ok(TraktTokenResponse {
                    access_token: "FRESH".into(),
                    refresh_token: "NEWREF".into(),
                    expires_in: 100,
                    created_at: 2_000,
                })
            }
            async fn device_code(&self) -> Result<DeviceCode, AppError> {
                unreachable!()
            }
            async fn poll_token(&self, _d: &str) -> Result<DeviceTokenPoll, AppError> {
                unreachable!()
            }
            async fn me(&self, _a: &str) -> Result<TraktUser, AppError> {
                unreachable!()
            }
            // Reads must NOT run: the CAS-skip returns before pulling reads.
            async fn watchlist(&self, _a: &str) -> Result<Vec<TraktItem>, AppError> {
                unreachable!()
            }
            async fn in_progress(&self, _a: &str) -> Result<Vec<TraktItem>, AppError> {
                unreachable!()
            }
            async fn watched(&self, _a: &str) -> Result<WatchedData, AppError> {
                unreachable!()
            }
            async fn show_progress(&self, _a: &str, _t: u64) -> Result<ShowProgress, AppError> {
                unreachable!()
            }
        }
        let store = mem_store();
        store
            .put_trakt_tokens("alice".into(), tokens("old", 0, false))
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(RotatingTrakt {
            store: store.clone(),
        });
        let tmdb = TmdbClient::offline().await;

        let summary = sync_trakt(&trakt, &tmdb, &store, None, false).await;
        assert_eq!(summary.enrolled, 1);
        assert_eq!(
            summary.succeeded, 0,
            "a CAS-skip materialises nothing and must not count as a successful sync"
        );
        assert!(
            !summary.removals_trustworthy(),
            "an unconfirmed wanted set (only a CAS-skip this cycle) is not trustworthy for removals"
        );
    }

    #[tokio::test]
    async fn account_removed_mid_cycle_is_not_resurrected_by_wanted_writes() {
        // Token far from expiry → no refresh → the refresh CAS guard never fires. If the operator
        // removes the account mid-cycle (here: during the watchlist read), the pre-write token
        // re-check must skip ALL writes — otherwise the just-built wanted rows would be resurrected
        // as permanent orphans that pin the title forever (nothing re-prunes a tokenless slug).
        use crate::error::AppError;
        use crate::trakt_client::{DeviceCode, DeviceTokenPoll, ShowProgress, TraktUser};
        struct RemoveOnRead {
            store: Store,
        }
        #[async_trait::async_trait]
        impl TraktClient for RemoveOnRead {
            async fn watchlist(&self, _a: &str) -> Result<Vec<TraktItem>, AppError> {
                // The operator clicks "Remove" mid-cycle (deletes tokens + wanted rows).
                self.store
                    .remove_trakt_tokens("alice".into())
                    .await
                    .unwrap();
                Ok(vec![item(MediaType::Movie, 42)])
            }
            async fn in_progress(&self, _a: &str) -> Result<Vec<TraktItem>, AppError> {
                Ok(vec![])
            }
            async fn watched(&self, _a: &str) -> Result<WatchedData, AppError> {
                Ok(WatchedData::default())
            }
            async fn refresh(&self, _rt: &str) -> Result<TraktTokenResponse, AppError> {
                unreachable!("token is far from expiry — no refresh")
            }
            async fn device_code(&self) -> Result<DeviceCode, AppError> {
                unreachable!()
            }
            async fn poll_token(&self, _d: &str) -> Result<DeviceTokenPoll, AppError> {
                unreachable!()
            }
            async fn me(&self, _a: &str) -> Result<TraktUser, AppError> {
                unreachable!()
            }
            async fn show_progress(&self, _a: &str, _t: u64) -> Result<ShowProgress, AppError> {
                unreachable!()
            }
        }
        let store = mem_store();
        store
            .put_trakt_tokens("alice".into(), tokens("acc", 9_999_999_999, false))
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(RemoveOnRead {
            store: store.clone(),
        });
        let tmdb = TmdbClient::offline().await;

        let summary = sync_trakt(&trakt, &tmdb, &store, None, false).await;

        assert!(
            store.get_trakt_tokens("alice".to_string()).await.is_none(),
            "the account must stay removed"
        );
        let rows = store.all_wanted().await;
        assert!(
            rows.iter().all(|r| r.user != "alice"),
            "a removed account's wanted rows must not be resurrected (found {rows:?})"
        );
        assert_eq!(
            summary.succeeded, 0,
            "a mid-cycle removal is a skip, not a confirmed sync"
        );
    }

    #[tokio::test]
    async fn sync_trakt_fetch_error_leaves_wanted_and_flags_account() {
        let store = mem_store();
        store
            .put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, false))
            .await
            .unwrap();
        let preexisting = movie_wanted_row("alice", 999);
        store.put_wanted(preexisting.clone()).await.unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            fail_reads: true,
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        assert_eq!(
            store
                .get_wanted("alice".to_string(), MediaType::Movie, 999)
                .await,
            Some(preexisting),
            "a fetch failure must leave existing wanted rows untouched"
        );
        assert!(
            store
                .get_trakt_tokens("alice".to_string())
                .await
                .unwrap()
                .needs_reenrolment
        );
    }

    #[tokio::test]
    async fn sync_trakt_prunes_stale_wanted() {
        let store = mem_store();
        store
            .put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, false))
            .await
            .unwrap();
        store
            .put_wanted(movie_wanted_row("alice", 999))
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData::default(),
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        assert!(
            store
                .get_wanted("alice".to_string(), MediaType::Movie, 27205)
                .await
                .is_some(),
            "new title present"
        );
        assert!(
            store
                .get_wanted("alice".to_string(), MediaType::Movie, 999)
                .await
                .is_none(),
            "stale title pruned"
        );
    }

    #[tokio::test]
    async fn sync_trakt_clears_preexisting_flag_on_success() {
        let store = mem_store();
        store
            .put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, true))
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData::default(),
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        assert!(
            !store
                .get_trakt_tokens("alice".to_string())
                .await
                .unwrap()
                .needs_reenrolment,
            "a successful sync clears a stale re-enrolment flag"
        );
    }

    /// A failure for one user must not affect other users. alice has an expired token and the
    /// mock refuses to refresh (fail_refresh=true) → she is flagged and gets no wanted rows.
    /// bob has a future-expiry token (no refresh needed) and the same mock succeeds his reads →
    /// bob's wanted row is populated and his flag stays false.
    #[tokio::test]
    async fn sync_trakt_multi_user_failure_isolates_to_one_account() {
        let store = mem_store();
        // alice: expired token — refresh will fail
        store
            .put_trakt_tokens(
                "alice".to_string(),
                TraktTokens {
                    access: "alice-acc".to_string(),
                    refresh: "alice-ref".to_string(),
                    expires_at: 0,
                    username: "alice".to_string(),
                    needs_reenrolment: false,
                },
            )
            .await
            .unwrap();
        // bob: fresh token — no refresh needed, reads will succeed
        store
            .put_trakt_tokens(
                "bob".to_string(),
                TraktTokens {
                    access: "bob-acc".to_string(),
                    refresh: "bob-ref".to_string(),
                    expires_at: 9_999_999_999,
                    username: "bob".to_string(),
                    needs_reenrolment: false,
                },
            )
            .await
            .unwrap();

        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            fail_refresh: true,
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData::default(),
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        // alice: refresh failed → flagged, no wanted rows written
        let alice_tok = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert!(
            alice_tok.needs_reenrolment,
            "alice must be flagged after refresh failure"
        );
        assert!(
            store
                .get_wanted("alice".to_string(), MediaType::Movie, 27205)
                .await
                .is_none(),
            "alice's wanted must be empty — error occurred before any read"
        );

        // bob: sync succeeded → wanted row present, NOT flagged
        let bob_tok = store.get_trakt_tokens("bob".to_string()).await.unwrap();
        assert!(!bob_tok.needs_reenrolment, "bob must NOT be flagged");
        assert!(
            store
                .get_wanted("bob".to_string(), MediaType::Movie, 27205)
                .await
                .is_some(),
            "bob's wanted must be populated"
        );
    }

    /// When refresh fails for an expired token, the account is flagged but the stored refresh
    /// token is preserved (we never blank it) and no wanted rows are written.
    #[tokio::test]
    async fn sync_trakt_fail_refresh_flags_account_and_preserves_refresh_token() {
        let store = mem_store();
        store
            .put_trakt_tokens(
                "alice".to_string(),
                TraktTokens {
                    access: "acc".to_string(),
                    refresh: "original-ref".to_string(),
                    expires_at: 0,
                    username: "alice".to_string(),
                    needs_reenrolment: false,
                },
            )
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            fail_refresh: true,
            ..Default::default()
        });
        let tmdb = TmdbClient::offline().await;

        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        let tok = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert!(tok.needs_reenrolment, "account must be flagged");
        assert!(!tok.refresh.is_empty(), "refresh token must not be blanked");
        // No wanted rows: the error occurred before any Trakt read
        assert!(
            store
                .get_wanted("alice".to_string(), MediaType::Movie, 27205)
                .await
                .is_none(),
            "no wanted rows must have been written"
        );
    }

    /// A TraktClient whose `refresh` simulates a CONCURRENT refresh: it rotates the stored token's
    /// refresh string (as if another `/refresh`/overlapping sync just succeeded, invalidating ours)
    /// then fails. Mirrors `enrolment::RotatingThenFailingRefresh`.
    struct RotatingThenFailingRefresh {
        store: Store,
    }
    #[async_trait::async_trait]
    impl TraktClient for RotatingThenFailingRefresh {
        async fn device_code(
            &self,
        ) -> Result<crate::trakt_client::DeviceCode, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn poll_token(
            &self,
            _dc: &str,
        ) -> Result<crate::trakt_client::DeviceTokenPoll, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn refresh(
            &self,
            _refresh_token: &str,
        ) -> Result<TraktTokenResponse, crate::error::AppError> {
            if let Some(mut t) = self.store.get_trakt_tokens("alice".into()).await {
                t.refresh = "ROTATED".into();
                let _ = self.store.put_trakt_tokens("alice".into(), t).await;
            }
            Err(crate::error::AppError::Unavailable)
        }
        async fn me(
            &self,
            _at: &str,
        ) -> Result<crate::trakt_client::TraktUser, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn watchlist(&self, _at: &str) -> Result<Vec<TraktItem>, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn in_progress(&self, _at: &str) -> Result<Vec<TraktItem>, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn watched(&self, _at: &str) -> Result<WatchedData, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn show_progress(
            &self,
            _at: &str,
            _trakt_id: u64,
        ) -> Result<crate::trakt_client::ShowProgress, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
    }

    #[tokio::test]
    async fn sync_trakt_refresh_failure_on_a_concurrently_rotated_token_does_not_flag() {
        // The sync-side mirror of the enrolment rotation-guard test: a near-expiry token triggers a
        // refresh; a concurrent refresh rotates the stored token and ours then fails against the
        // now-stale token. That false alarm must NOT flag the (healthy) account for re-enrolment.
        let store = mem_store();
        store
            .put_trakt_tokens(
                "alice".to_string(),
                TraktTokens {
                    access: "acc".to_string(),
                    refresh: "original-ref".to_string(),
                    expires_at: 0, // already expired → refresh path runs
                    username: "alice".to_string(),
                    needs_reenrolment: false,
                },
            )
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(RotatingThenFailingRefresh {
            store: store.clone(),
        });
        let tmdb = TmdbClient::offline().await;

        sync_trakt(&trakt, &tmdb, &store, None, false).await;

        let tok = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert_eq!(
            tok.refresh, "ROTATED",
            "the concurrently-rotated token survives"
        );
        assert!(
            !tok.needs_reenrolment,
            "a stale-token refresh failure must NOT flag a healthy account"
        );
    }

    // ── real Trakt samples, end-to-end (HTTP → client → build_wanted → plan) ──

    const WATCHED_SHOWS_P1: &str =
        include_str!("../tests/fixtures/trakt/watched_shows_progress_p1.json");
    const WATCHED_SHOWS_P2: &str =
        include_str!("../tests/fixtures/trakt/watched_shows_progress_p2.json");
    const WATCHED_SHOWS_NO_EXTENDED: &str =
        include_str!("../tests/fixtures/trakt/watched_shows_no_extended.json");
    const WATCHED_MOVIES_P1: &str = include_str!("../tests/fixtures/trakt/watched_movies_p1.json");
    const WATCHED_MOVIES_P2: &str = include_str!("../tests/fixtures/trakt/watched_movies_p2.json");
    const WATCHLIST_MOVIES: &str = include_str!("../tests/fixtures/trakt/watchlist_movies.json");

    fn real_client(base_url: &str) -> Arc<dyn TraktClient> {
        Arc::new(
            crate::trakt_client::TraktClientImpl::new(
                "cid".into(),
                "secret".into(),
                reqwest::Client::new(),
            )
            .with_base_url(base_url.to_string()),
        )
    }

    /// The real samples, end-to-end: `watched()` is fetched through the REAL client from a loopback
    /// server serving both captured pages (`extended=progress`), then the wanted-set is built exactly
    /// as `sync_trakt_user` builds it and planned by the episode monitor. Before the fix (plain
    /// `/sync/watched/shows`, page 1 only) every watched-episode set was empty, so the fully-watched
    /// Ended show was never finish-removed and catch-up re-acquired episodes already watched.
    #[tokio::test]
    async fn real_samples_finish_remove_a_watched_ended_show_and_catch_up_only_unwatched() {
        use crate::trakt_client::test_http::{spawn, Reply};
        use crate::wanted::{Action, RemoveReason};
        use std::collections::HashSet;
        const MINISERIES: u64 = 911100; // sample: S1E1-4 watched, all 4 aired → caught up
        const DRAMA: u64 = 911000; // sample: S1E1-8 + S2E1-7 watched, 17 aired → behind
        const THRILLER: u64 = 910900; // only on page 2 of the sample

        let server = spawn(vec![
            (
                "/sync/watched/movies?page=1&limit=100",
                Reply::page(WATCHED_MOVIES_P1, 1, 2, 5, 100),
            ),
            (
                "/sync/watched/movies?page=2&limit=100",
                Reply::page(WATCHED_MOVIES_P2, 2, 2, 5, 100),
            ),
            (
                "/sync/watched/shows?extended=progress&page=1&limit=100",
                Reply::page(WATCHED_SHOWS_P1, 1, 2, 4, 100),
            ),
            (
                "/sync/watched/shows?extended=progress&page=2&limit=100",
                Reply::page(WATCHED_SHOWS_P2, 2, 2, 4, 100),
            ),
        ])
        .await;
        let watched = real_client(&server.base_url)
            .watched("at")
            .await
            .expect("watched");
        assert!(
            watched.shows.iter().any(|s| s.tmdb_id == THRILLER),
            "page 2 of the watched shows is read"
        );

        // As `sync_trakt_user` derives them from Trakt's per-show progress: Example Drama is behind
        // (catch-up), Example Miniseries is caught up + owned + Ended (a finish-cleanup candidate, folded
        // because REMOVE_FINISHED_SHOWS is on).
        let statuses: HashMap<u64, ShowStatus> = [
            (MINISERIES, ShowStatus::Ended),
            (DRAMA, ShowStatus::Returning),
        ]
        .into_iter()
        .collect();
        let behind: HashSet<u64> = [DRAMA].into_iter().collect();
        let caught_up_owned: HashSet<u64> = [MINISERIES].into_iter().collect();
        let finished_ended = finished_ended_candidates(&caught_up_owned, &statuses);
        let fold = shows_to_fold(&behind, &finished_ended, true);
        let plan = |watched: &WatchedData, tmdb_id: u64, owned: Option<&OwnedGroup>| {
            let wanted: Vec<WantedRecord> =
                build_wanted("alice", &[], &[], watched, &statuses, None, Some(&fold))
                    .into_iter()
                    .filter(|r| r.media_type == MediaType::Show && r.tmdb_id == tmdb_id)
                    .collect();
            assert_eq!(wanted.len(), 1, "tmdb {tmdb_id} is wanted (folded)");
            assert!(wanted[0].sources.in_progress && !wanted[0].sources.watchlist);
            // TMDB's aired set (season 0 excluded, as `aired_episodes` returns it).
            let aired = AiredEpisodes {
                pairs: match tmdb_id {
                    MINISERIES => (1..=4).map(|e| (1, e)).collect(),
                    _ => (1..=8)
                        .map(|e| (1, e))
                        .chain((1..=9).map(|e| (2, e)))
                        .collect(),
                },
                complete: true,
            };
            let present: HashSet<String> = ["adol".to_string()].into_iter().collect();
            plan_episode_actions(tmdb_id, &wanted, owned, &present, &aired, false)
        };

        // Example Miniseries: a pre-existing (account-mirror, EMPTY provenance) copy, present.
        let miniseries = OwnedGroup {
            hashes: vec!["adol".to_string()],
            engine_hashes: vec![],
            provenance: Provenance { entries: vec![] },
            owned_episodes: (1..=4).map(|e| (1, e)).collect(),
            media_type: MediaType::Show,
            coverage: vec![RecordCoverage {
                hash: "adol".to_string(),
                pending: false,
                episodes: (1..=4).map(|e| (1, e)).collect(),
            }],
        };
        assert_eq!(
            plan(&watched, MINISERIES, Some(&miniseries)),
            vec![Action::Remove {
                tmdb_id: MINISERIES,
                hash: "adol".to_string(),
                reason: RemoveReason::Finished
            }],
            "a fully-watched Ended in-progress show is now a Trigger-A finish removal"
        );
        assert_eq!(
            removal_hashes(&miniseries, RemoveReason::Finished, true),
            vec!["adol".to_string()],
            "REMOVE_FINISHED_SHOWS reclaims the library copy"
        );
        // Example Drama (not owned): catch-up acquires ONLY the two unwatched aired episodes.
        assert_eq!(
            plan(&watched, DRAMA, None),
            vec![
                Action::AcquireEpisode {
                    tmdb_id: DRAMA,
                    season: 2,
                    episode: 8
                },
                Action::AcquireEpisode {
                    tmdb_id: DRAMA,
                    season: 2,
                    episode: 9
                },
            ],
            "catch-up must skip the 15 episodes already watched"
        );

        // Contrast — the pre-fix read (no `extended`): empty watched sets, so no finish removal and
        // all 17 Example Drama episodes re-acquired.
        let old = WatchedData {
            movies: vec![],
            shows: crate::trakt_client::parse_watched_shows(
                &serde_json::from_str(WATCHED_SHOWS_NO_EXTENDED).unwrap(),
            ),
        };
        assert!(plan(&old, MINISERIES, Some(&miniseries)).is_empty());
        assert_eq!(plan(&old, DRAMA, None).len(), 17);
    }

    /// Safety: a failure on ANY page fails the whole read, so `sync_trakt` keeps the user's previous
    /// `wanted` set (exactly as for any other read failure) instead of materialising a truncated one
    /// — which for the watchlist would read titles beyond page 1 as un-watchlisted (Trigger B).
    #[tokio::test(start_paused = true)]
    async fn sync_trakt_a_failed_page_keeps_the_prior_wanted_set() {
        use crate::trakt_client::test_http::{spawn, Reply};
        let page2 = r#"[{"type":"movie","movie":{"ids":{"tmdb":11,"imdb":"tt0076759","trakt":1},"title":"Star Wars"}}]"#;
        let routes = |page2_reply: Reply| {
            vec![
                (
                    "/sync/watchlist/movies?page=1&limit=100",
                    Reply::page(WATCHLIST_MOVIES, 1, 2, 4, 100),
                ),
                ("/sync/watchlist/movies?page=2&limit=100", page2_reply),
                (
                    "/sync/watchlist/shows?page=1&limit=100",
                    Reply::page("[]", 1, 1, 0, 100),
                ),
                ("/sync/playback", Reply::json("[]")),
                (
                    "/sync/watched/movies?page=1&limit=100",
                    Reply::page("[]", 1, 1, 0, 100),
                ),
                (
                    "/sync/watched/shows?extended=progress&page=1&limit=100",
                    Reply::page("[]", 1, 1, 0, 100),
                ),
            ]
        };
        let store = mem_store();
        store
            .put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, false))
            .await
            .unwrap();
        let preexisting = movie_wanted_row("alice", 999);
        store.put_wanted(preexisting.clone()).await.unwrap();
        let tmdb = TmdbClient::offline().await;

        // Page 2 of the watchlist fails persistently (retried, then surfaced).
        let server = spawn(routes(Reply::status(503))).await;
        let summary = sync_trakt(&real_client(&server.base_url), &tmdb, &store, None, false).await;
        assert_eq!(summary.succeeded, 0, "a failed page is a failed sync");
        assert_eq!(
            store.all_wanted().await,
            vec![preexisting],
            "the prior wanted set must be left untouched — no truncated page-1-only set"
        );
        assert!(
            store
                .get_trakt_tokens("alice".to_string())
                .await
                .unwrap()
                .needs_reenrolment,
            "flagged like any other read failure (cleared by the next good sync)"
        );

        // Positive control: once page 2 answers, every page is read and adopted — the page-2 title
        // is wanted and the stale row is pruned.
        let server = spawn(routes(Reply::page(page2, 2, 2, 4, 100))).await;
        let summary = sync_trakt(&real_client(&server.base_url), &tmdb, &store, None, false).await;
        assert_eq!(summary.succeeded, 1);
        let mut ids: Vec<u64> = store
            .all_wanted()
            .await
            .into_iter()
            .map(|r| r.tmdb_id)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![11, 911300, 911400, 911700]);
    }
}

#[cfg(test)]
mod reconcile_wanted_tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::store::{OwnedRecord, OwnedStatus, WantedSources, WatchedState};
    use crate::tmdb_client::ShowStatus;
    use std::sync::Arc;

    fn mem_store() -> Store {
        Store::from_database(Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap()
    }

    fn wanted_movie(
        user: &str,
        tmdb_id: u64,
        watchlist: bool,
        in_progress: bool,
        watched: bool,
    ) -> WantedRecord {
        WantedRecord {
            user: user.to_string(),
            tmdb_id,
            media_type: MediaType::Movie,
            sources: WantedSources {
                watchlist,
                in_progress,
            },
            watched_state: WatchedState::Movie { watched },
            show_status: None,
            imdb_id: None,
        }
    }

    fn wanted_show(user: &str, tmdb_id: u64, watchlist: bool, in_progress: bool) -> WantedRecord {
        WantedRecord {
            user: user.to_string(),
            tmdb_id,
            media_type: MediaType::Show,
            sources: WantedSources {
                watchlist,
                in_progress,
            },
            watched_state: WatchedState::Show {
                watched_episodes: vec![],
            },
            show_status: Some(ShowStatus::Returning),
            imdb_id: None,
        }
    }

    fn owned_record(tmdb_id: u64, kind: MediaKind, provenance: Provenance) -> OwnedRecord {
        OwnedRecord {
            request: AcquireRequest {
                imdb_id: String::new(),
                tmdb_id,
                kind,
                season: None,
                episode: None,
                original_language: None,
                spoken_languages: Some(vec![]),
                metadata: MediaMetadata {
                    title: String::new(),
                    year: None,
                    media_type: media_type_of(kind),
                    external_id: None,
                },
            },
            provenance,
            added_at: 0,
            status: OwnedStatus::Pending,
            provides: vec![],
            quality: None,
        }
    }

    fn torrent(id: &str, hash: &str) -> Torrent {
        Torrent {
            id: id.to_string(),
            hash: hash.to_string(),
            status: "downloaded".to_string(),
            ..Default::default()
        }
    }

    // ── plan_episode_actions (pure: TitleView assembly + reconcile + incomplete-aired guard) ──

    fn owned_group_show(
        hashes: &[&str],
        owned_eps: &[(u32, u32)],
        provenance: Provenance,
    ) -> OwnedGroup {
        let hv: Vec<String> = hashes.iter().map(|h| h.to_string()).collect();
        OwnedGroup {
            engine_hashes: if provenance.entries.is_empty() {
                vec![]
            } else {
                hv.clone()
            },
            coverage: hv
                .iter()
                .map(|h| RecordCoverage {
                    hash: h.clone(),
                    pending: false,
                    episodes: owned_eps.to_vec(),
                })
                .collect(),
            hashes: hv,
            provenance,
            owned_episodes: owned_eps.to_vec(),
            media_type: MediaType::Show,
        }
    }

    #[test]
    fn plan_episode_actions_acquires_unowned_aired_episode() {
        use crate::wanted::Action;
        // A watchlisted show owning S01E01 (present → available) with S01E01+S01E02 aired: only the
        // unowned S01E02 is acquired, and nothing is removed (a watchlisted show is never "finished").
        let wanted = vec![wanted_show("alice", 100, true, false)];
        let owned = owned_group_show(&["h1"], &[(1, 1)], Provenance::watchlist("alice"));
        let present: std::collections::HashSet<String> = ["h1".to_string()].into_iter().collect();
        let aired = AiredEpisodes {
            pairs: vec![(1, 1), (1, 2)],
            complete: true,
        };

        let actions = plan_episode_actions(100, &wanted, Some(&owned), &present, &aired, false);
        assert_eq!(
            actions,
            vec![Action::AcquireEpisode {
                tmdb_id: 100,
                season: 1,
                episode: 2
            }],
            "only the unowned aired episode is acquired"
        );
    }

    #[test]
    fn plan_episode_actions_guard_suppresses_removal_on_incomplete_aired() {
        use crate::wanted::{Action, RemoveReason};
        // An in-progress-only, fully-watched, Ended, owned show is a Trigger-A finish removal — but
        // only when the aired set is trustworthy. A COMPLETE aired set emits the Remove; an INCOMPLETE
        // one (a TMDB hiccup) makes the guard drop it, so a still-wanted show is never wrongly deleted.
        let mut w = wanted_show("alice", 100, false, true); // in-progress, not watchlisted
        w.watched_state = WatchedState::Show {
            watched_episodes: vec![(1, 1)],
        };
        w.show_status = Some(ShowStatus::Ended);
        let wanted = vec![w];
        let owned = owned_group_show(&["h1"], &[(1, 1)], Provenance::in_progress("alice"));
        let present: std::collections::HashSet<String> = ["h1".to_string()].into_iter().collect();

        // Complete aired → the finish removal is emitted.
        let complete = AiredEpisodes {
            pairs: vec![(1, 1)],
            complete: true,
        };
        let actions = plan_episode_actions(100, &wanted, Some(&owned), &present, &complete, false);
        assert_eq!(
            actions,
            vec![Action::Remove {
                tmdb_id: 100,
                hash: "h1".to_string(),
                reason: RemoveReason::Finished
            }],
            "a fully-watched Ended in-progress show is finish-removed when the aired set is complete"
        );

        // Incomplete aired → the guard suppresses the removal.
        let incomplete = AiredEpisodes {
            pairs: vec![(1, 1)],
            complete: false,
        };
        let actions =
            plan_episode_actions(100, &wanted, Some(&owned), &present, &incomplete, false);
        assert!(
            actions.is_empty(),
            "an incomplete aired set must suppress the finish removal, got {actions:?}"
        );
    }

    #[test]
    fn plan_episode_actions_specials_do_not_block_or_fake_a_finish() {
        use crate::wanted::{Action, RemoveReason};
        // Specials (season 0) are outside the regular run on both sides of the finish comparison:
        // TMDB's aired set drops season 0 and Trakt's watched history omits it. So an aired set that
        // happens to carry an unwatched special must not block the finish removal…
        let mut w = wanted_show("alice", 100, false, true);
        w.watched_state = WatchedState::Show {
            watched_episodes: vec![(1, 1), (1, 2)],
        };
        w.show_status = Some(ShowStatus::Ended);
        let wanted = vec![w];
        let owned = owned_group_show(&["h1"], &[(1, 1), (1, 2)], Provenance::in_progress("alice"));
        let present: std::collections::HashSet<String> = ["h1".to_string()].into_iter().collect();
        let with_special = AiredEpisodes {
            pairs: vec![(0, 1), (1, 1), (1, 2)],
            complete: true,
        };
        assert_eq!(
            plan_episode_actions(100, &wanted, Some(&owned), &present, &with_special, false),
            vec![Action::Remove {
                tmdb_id: 100,
                hash: "h1".to_string(),
                reason: RemoveReason::Finished
            }],
            "an unwatched special must not block a fully-watched Ended show's finish removal"
        );
        // …and an aired set holding ONLY specials is as untrustworthy as an empty one: the "every
        // regular aired episode watched" test would be vacuously true, so the guard drops it.
        let only_specials = AiredEpisodes {
            pairs: vec![(0, 1)],
            complete: true,
        };
        let actions =
            plan_episode_actions(100, &wanted, Some(&owned), &present, &only_specials, false);
        assert!(
            actions.is_empty(),
            "a specials-only aired set must never finish-remove, got {actions:?}"
        );
    }

    /// Owned show records for the per-episode coverage tests: a present `Verified` S01 pack plus
    /// one S02E01 record of the given status (absent from the listing).
    async fn show_with_absent_single(single_status: OwnedStatus) -> OwnedGroup {
        let store = mem_store();
        let rec = |provides: Vec<(u32, u32)>, status: OwnedStatus| OwnedRecord {
            status,
            provides,
            ..owned_record(100, MediaKind::Series, Provenance::watchlist("alice"))
        };
        store
            .put_owned(
                "pack".into(),
                rec(vec![(1, 1), (1, 2), (1, 3)], OwnedStatus::Verified),
            )
            .await
            .unwrap();
        store
            .put_owned("single".into(), rec(vec![(2, 1)], single_status))
            .await
            .unwrap();
        group_owned_by_tmdb(&store)
            .await
            .remove(&(MediaType::Show, 100))
            .unwrap()
    }

    /// An episode supplied only by a record whose torrent has left the account is not covered just
    /// because another copy of the show is present: nothing else would ever bring it back (no file
    /// is left to read, so on-read repair never fires). A `Pending` record still counts — `observe`
    /// owns it until it settles, and it may simply not be listed yet.
    #[tokio::test]
    async fn plan_episode_actions_reacquires_an_episode_whose_only_copy_left_the_account() {
        use crate::wanted::Action;
        let wanted = vec![wanted_show("alice", 100, true, false)];
        let present: std::collections::HashSet<String> = ["pack".to_string()].into_iter().collect();
        let aired = AiredEpisodes {
            pairs: vec![(1, 1), (1, 2), (1, 3), (2, 1)],
            complete: true,
        };

        let lapsed = show_with_absent_single(OwnedStatus::Verified).await;
        assert_eq!(
            plan_episode_actions(100, &wanted, Some(&lapsed), &present, &aired, false),
            vec![Action::AcquireEpisode {
                tmdb_id: 100,
                season: 2,
                episode: 1
            }],
            "S02E01 is re-acquired; the present pack still covers S01"
        );

        let pending = show_with_absent_single(OwnedStatus::Pending).await;
        assert!(
            plan_episode_actions(100, &wanted, Some(&pending), &present, &aired, false).is_empty(),
            "a Pending copy is observe's: not re-acquired"
        );
    }

    /// A wholesale-empty listing while owned records exist is suspect (the scan loop hasn't accepted
    /// it): every owned episode counts as covered and nothing is removed, so a spurious empty-but-OK
    /// listing can't start a re-acquire storm. An episode nobody owns is still acquired.
    #[tokio::test]
    async fn plan_episode_actions_on_a_suspect_empty_listing_reacquires_nothing_owned() {
        use crate::wanted::Action;
        let wanted = vec![wanted_show("alice", 100, true, false)];
        let group = show_with_absent_single(OwnedStatus::Verified).await;
        let aired = AiredEpisodes {
            pairs: vec![(1, 1), (1, 2), (1, 3), (2, 1), (2, 2)],
            complete: true,
        };
        let empty = std::collections::HashSet::new();
        assert_eq!(
            plan_episode_actions(100, &wanted, Some(&group), &empty, &aired, true),
            vec![Action::AcquireEpisode {
                tmdb_id: 100,
                season: 2,
                episode: 2
            }],
            "only the never-owned episode is acquired"
        );

        // The same show, finished (Trigger A): no removal on a suspect listing either.
        let mut w = wanted_show("alice", 100, false, true);
        w.watched_state = WatchedState::Show {
            watched_episodes: vec![(1, 1), (1, 2), (1, 3), (2, 1)],
        };
        w.show_status = Some(ShowStatus::Ended);
        let finished = AiredEpisodes {
            pairs: vec![(1, 1), (1, 2), (1, 3), (2, 1)],
            complete: true,
        };
        assert!(
            plan_episode_actions(100, &[w], Some(&group), &empty, &finished, true).is_empty(),
            "a removal is dropped on a suspect listing"
        );
    }

    #[test]
    fn an_empty_listing_is_suspect_until_seen_three_times_in_a_row() {
        assert!(empty_listing_is_suspect(true, 5, 1));
        assert!(empty_listing_is_suspect(true, 5, 2));
        assert!(
            !empty_listing_is_suspect(true, 5, 3),
            "the third consecutive empty listing is accepted"
        );
        assert!(
            !empty_listing_is_suspect(true, 0, 1),
            "nothing was known: an empty account is just empty"
        );
        assert!(!empty_listing_is_suspect(false, 5, 0));
    }

    // ── provenance_from_wanted (pure) ─────────────────────────────────────────

    #[test]
    fn provenance_from_wanted_watchlist_user() {
        let w = vec![wanted_movie("alice", 1, true, false, false)];
        assert_eq!(provenance_from_wanted(&w), Provenance::watchlist("alice"));
    }

    #[test]
    fn provenance_from_wanted_in_progress_user() {
        let w = vec![wanted_movie("alice", 1, false, true, false)];
        assert_eq!(provenance_from_wanted(&w), Provenance::in_progress("alice"));
    }

    #[test]
    fn provenance_from_wanted_both_sources_one_user() {
        let w = vec![wanted_movie("alice", 1, true, true, false)];
        assert_eq!(
            provenance_from_wanted(&w).entries,
            vec![
                ProvenanceEntry::Watchlist {
                    user: "alice".into()
                },
                ProvenanceEntry::InProgress {
                    user: "alice".into()
                },
            ]
        );
    }

    #[test]
    fn provenance_from_wanted_two_users() {
        let w = vec![
            wanted_movie("alice", 1, true, false, false),
            wanted_movie("bob", 1, false, true, false),
        ];
        assert_eq!(
            provenance_from_wanted(&w).entries,
            vec![
                ProvenanceEntry::Watchlist {
                    user: "alice".into()
                },
                ProvenanceEntry::InProgress { user: "bob".into() },
            ]
        );
    }

    // ── plan_reconcile_ops (mem Store, no TMDB / engine) ──────────────────────

    #[tokio::test]
    async fn plan_missing_wanted_movie_acquires() {
        let store = mem_store();
        store
            .put_wanted(wanted_movie("alice", 27205, true, false, false))
            .await
            .unwrap();
        let ops = plan_reconcile_ops(&store, &[]).await;
        assert_eq!(
            ops,
            vec![ReconcileOp::Acquire {
                tmdb_id: 27205,
                kind: MediaKind::Movie,
                season: None,
                episode: None,
                provenance: Provenance::watchlist("alice"),
                imdb_id: None,
            }]
        );
    }

    #[tokio::test]
    async fn plan_acquire_carries_trakt_imdb_hint() {
        // A wanted movie whose Trakt row carries an IMDB id must surface that id on the Acquire op,
        // so acquisition can use it directly instead of re-deriving from TMDB (which can be empty).
        let store = mem_store();
        store
            .put_wanted(WantedRecord {
                imdb_id: Some("tt1375666".into()),
                ..wanted_movie("alice", 27205, true, false, false)
            })
            .await
            .unwrap();
        let ops = plan_reconcile_ops(&store, &[]).await;
        assert_eq!(
            ops,
            vec![ReconcileOp::Acquire {
                tmdb_id: 27205,
                kind: MediaKind::Movie,
                season: None,
                episode: None,
                provenance: Provenance::watchlist("alice"),
                imdb_id: Some("tt1375666".into()),
            }]
        );
    }

    #[test]
    fn choose_imdb_prefers_trakt_then_tmdb_then_none() {
        // Trakt hint wins when present.
        assert_eq!(
            choose_imdb(Some("tt_trakt".into()), Some("tt_tmdb".into())),
            Some("tt_trakt".into())
        );
        // Falls back to TMDB's external id when Trakt has none.
        assert_eq!(
            choose_imdb(None, Some("tt_tmdb".into())),
            Some("tt_tmdb".into())
        );
        // Blank Trakt hint is ignored (falls through to TMDB).
        assert_eq!(
            choose_imdb(Some("  ".into()), Some("tt_tmdb".into())),
            Some("tt_tmdb".into())
        );
        // No id anywhere → None (caller skips the title quietly — it can't be scraped).
        assert_eq!(choose_imdb(None, None), None);
    }

    #[test]
    fn imdb_hint_from_wanted_picks_first_non_empty() {
        let rows = vec![
            WantedRecord {
                imdb_id: None,
                ..wanted_movie("alice", 27205, true, false, false)
            },
            WantedRecord {
                imdb_id: Some("tt1375666".into()),
                ..wanted_movie("bob", 27205, false, true, false)
            },
        ];
        assert_eq!(imdb_hint_from_wanted(&rows), Some("tt1375666".into()));
        assert_eq!(imdb_hint_from_wanted(&[]), None);
    }

    #[tokio::test]
    async fn plan_finished_owned_movie_removes_trigger_a() {
        let store = mem_store();
        // all in-progress (not watchlisted) wanters watched + owned hash present → Trigger A.
        store
            .put_wanted(wanted_movie("alice", 27205, false, true, true))
            .await
            .unwrap();
        store
            .put_owned(
                "abc".into(),
                owned_record(27205, MediaKind::Movie, Provenance::in_progress("alice")),
            )
            .await
            .unwrap();
        let torrents = vec![torrent("t1", "ABC")];
        let ops = plan_reconcile_ops(&store, &torrents).await;
        assert_eq!(
            ops,
            vec![ReconcileOp::Remove {
                tmdb_id: 27205,
                hashes: vec!["abc".into()]
            }]
        );
    }

    #[tokio::test]
    async fn plan_finished_mirror_only_movie_emits_no_remove_op() {
        // A movie the user already had in their library (mirror copy, EMPTY provenance) that they
        // re-watch (in-progress + watched on Trakt) triggers Trigger-A, but `removal_hashes` protects
        // the mirror copy (movies are never folded). The planner must NOT emit a no-op empty Remove
        // op each tick — it would churn an empty op and a full selection-table read in execute_remove.
        let store = mem_store();
        store
            .put_wanted(wanted_movie("alice", 27205, false, true, true))
            .await
            .unwrap();
        store
            .put_owned(
                "mirror".into(),
                owned_record(27205, MediaKind::Movie, Provenance { entries: vec![] }),
            )
            .await
            .unwrap();
        let torrents = vec![torrent("t1", "MIRROR")];
        let ops = plan_reconcile_ops(&store, &torrents).await;
        assert!(
            ops.is_empty(),
            "a finished mirror-only movie protects its hash, so no Remove op should be emitted: {ops:?}"
        );
    }

    #[tokio::test]
    async fn plan_watchlisted_watched_movie_is_kept_not_removed() {
        // The Oldboy case: a movie watched but still on the watchlist must NOT be removed (and,
        // since owned + present, not re-acquired) — it stays available for a re-watch. Without the
        // Trigger-A watchlist guard this oscillated acquire/remove every reconcile tick.
        let store = mem_store();
        store
            .put_wanted(wanted_movie(
                "alice", 27205, /*watchlist*/ true, false, /*watched*/ true,
            ))
            .await
            .unwrap();
        store
            .put_owned(
                "abc".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        let torrents = vec![torrent("t1", "ABC")];
        let ops = plan_reconcile_ops(&store, &torrents).await;
        assert!(
            ops.is_empty(),
            "watchlisted+watched owned title must be kept, got {ops:?}"
        );
    }

    #[tokio::test]
    async fn plan_lapsed_owned_wanted_movie_reacquires() {
        let store = mem_store();
        store
            .put_wanted(wanted_movie("alice", 27205, true, false, false))
            .await
            .unwrap();
        store
            .put_owned(
                "abc".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        // No torrents → the owned hash is absent → lapsed → re-acquire.
        let ops = plan_reconcile_ops(&store, &[]).await;
        assert_eq!(
            ops,
            vec![ReconcileOp::Acquire {
                tmdb_id: 27205,
                kind: MediaKind::Movie,
                season: None,
                episode: None,
                provenance: Provenance::watchlist("alice"),
                imdb_id: None,
            }]
        );
    }

    /// A lapsed movie owned with Manual provenance (and a Trakt wanter) must keep its Manual
    /// origin after the re-acquire plan is built — so the never-auto-remove guard is not erased.
    #[tokio::test]
    async fn plan_lapsed_manual_owned_with_wanter_preserves_manual_provenance() {
        let store = mem_store();
        // alice wants it via watchlist
        store
            .put_wanted(wanted_movie("alice", 27205, true, false, false))
            .await
            .unwrap();
        // The owned record has BOTH Manual and alice's Watchlist entries
        let mut combined = Provenance::manual();
        combined.merge(&Provenance::watchlist("alice"));
        store
            .put_owned(
                "abc".into(),
                owned_record(27205, MediaKind::Movie, combined),
            )
            .await
            .unwrap();
        // No torrents → lapsed → AcquireMovie
        let ops = plan_reconcile_ops(&store, &[]).await;
        assert_eq!(ops.len(), 1, "expected one Acquire op");
        match &ops[0] {
            ReconcileOp::Acquire { provenance, .. } => {
                assert!(
                    provenance.has_manual_entry(),
                    "Manual provenance must be preserved on lapsed re-acquire"
                );
            }
            other => panic!("expected Acquire, got {:?}", other),
        }
    }

    /// A lapsed movie whose only wanter has already watched it produces a Remove — not an Acquire.
    /// Documents that `reconcile_title`'s removal precedence holds even when the title is lapsed
    /// (hash absent from listing).
    #[tokio::test]
    async fn plan_lapsed_and_finished_movie_removes_not_reacquires() {
        let store = mem_store();
        // alice has watched the movie and it's an in-progress-only (not watchlisted) title → Trigger A.
        store
            .put_wanted(wanted_movie("alice", 27205, false, true, true))
            .await
            .unwrap();
        store
            .put_owned(
                "abc".into(),
                owned_record(27205, MediaKind::Movie, Provenance::in_progress("alice")),
            )
            .await
            .unwrap();
        // No torrents → lapsed (hash absent), but removal takes precedence over re-acquire.
        let ops = plan_reconcile_ops(&store, &[]).await;
        assert_eq!(
            ops,
            vec![ReconcileOp::Remove {
                tmdb_id: 27205,
                hashes: vec!["abc".into()]
            }]
        );
    }

    /// A wholesale-empty listing while the identification cache still holds entries (the scan loop
    /// hasn't accepted the empty account) is suspect: an owned wanted movie is NOT re-acquired as
    /// lapsed, and no removal is planned (`execute_remove` couldn't see the torrent to delete it and
    /// would orphan it). Once the cache is empty the empty listing is trusted again.
    #[tokio::test]
    async fn plan_on_a_suspect_empty_listing_neither_reacquires_nor_removes() {
        let store = mem_store();
        store
            .put_wanted(wanted_movie("alice", 27205, true, false, false))
            .await
            .unwrap();
        store
            .put_owned(
                "abc".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        // An owned title nobody wants any more: a Trigger-B removal on a trusted listing.
        store
            .put_owned(
                "old".into(),
                owned_record(603, MediaKind::Movie, Provenance::watchlist("bob")),
            )
            .await
            .unwrap();
        store
            .put_matches(vec![(
                "t1".into(),
                crate::rd_client::TorrentInfo::default(),
                MediaMetadata {
                    title: "Inception".into(),
                    year: None,
                    media_type: MediaType::Movie,
                    external_id: None,
                },
            )])
            .await
            .unwrap();
        assert_eq!(
            plan_reconcile_ops(&store, &[]).await,
            vec![],
            "a suspect empty listing plans neither a re-acquire nor a removal"
        );

        store.remove_matches(vec!["t1".into()]).await.unwrap();
        let ops = plan_reconcile_ops(&store, &[]).await;
        assert!(
            ops.iter()
                .any(|o| matches!(o, ReconcileOp::Acquire { tmdb_id: 27205, .. })),
            "a trusted empty listing re-acquires the lapsed title: {ops:?}"
        );
        assert!(
            ops.iter()
                .any(|o| matches!(o, ReconcileOp::Remove { tmdb_id: 603, .. })),
            "and removes the abandoned one: {ops:?}"
        );
    }

    #[tokio::test]
    async fn plan_manual_owned_no_wanters_is_never_removed() {
        let store = mem_store();
        store
            .put_owned(
                "abc".into(),
                owned_record(27205, MediaKind::Movie, Provenance::manual()),
            )
            .await
            .unwrap();
        let torrents = vec![torrent("t1", "ABC")];
        let ops = plan_reconcile_ops(&store, &torrents).await;
        assert_eq!(ops, vec![]);
    }

    #[tokio::test]
    async fn plan_owned_available_not_finished_no_op() {
        let store = mem_store();
        store
            .put_wanted(wanted_movie("alice", 27205, true, false, false))
            .await
            .unwrap();
        store
            .put_owned(
                "abc".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        let torrents = vec![torrent("t1", "ABC")];
        let ops = plan_reconcile_ops(&store, &torrents).await;
        assert_eq!(ops, vec![]);
    }

    #[tokio::test]
    async fn plan_ignores_unrelated_torrent() {
        // A torrent whose hash is in neither wanted nor owned must never appear in ops.
        let store = mem_store();
        let torrents = vec![torrent("t1", "DEADBEEF")];
        let ops = plan_reconcile_ops(&store, &torrents).await;
        assert_eq!(ops, vec![]);
    }

    #[tokio::test]
    async fn plan_show_trigger_b_removes() {
        let store = mem_store();
        // Owned show via alice's watchlist, nobody wants it now → Trigger B.
        store
            .put_owned(
                "abc".into(),
                owned_record(1396, MediaKind::Series, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        let torrents = vec![torrent("t1", "ABC")];
        let ops = plan_reconcile_ops(&store, &torrents).await;
        assert_eq!(
            ops,
            vec![ReconcileOp::Remove {
                tmdb_id: 1396,
                hashes: vec!["abc".into()]
            }]
        );
    }

    #[tokio::test]
    async fn plan_show_trigger_b_keeps_pre_existing_mirror_hash() {
        // A pre-existing mirror copy (EMPTY provenance — e.g. an S01 pack the user already had) and a
        // watchlist-acquired episode (e.g. S02) share tmdb 1396. When the watchlist is abandoned
        // (nobody wants it now), Trigger B must remove ONLY the engine-acquired hash and KEEP the
        // mirror copy — otherwise the user's pre-existing library is silently deleted.
        let store = mem_store();
        store
            .put_owned(
                "mirror".into(),
                owned_record(1396, MediaKind::Series, Provenance { entries: vec![] }),
            )
            .await
            .unwrap();
        store
            .put_owned(
                "engine".into(),
                owned_record(1396, MediaKind::Series, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        let torrents = vec![torrent("t1", "MIRROR"), torrent("t2", "ENGINE")];
        let ops = plan_reconcile_ops(&store, &torrents).await;
        assert_eq!(
            ops,
            vec![ReconcileOp::Remove {
                tmdb_id: 1396,
                hashes: vec!["engine".into()]
            }],
            "Trigger-B must delete only the engine-acquired hash, never the pre-existing mirror copy"
        );
    }

    #[test]
    fn removal_hashes_finished_show_includes_mirror_only_when_flag_on() {
        use crate::wanted::RemoveReason;
        // A SHOW group with one pre-existing mirror hash (empty provenance, only in `hashes`) and one
        // engine-acquired hash (in both `hashes` and `engine_hashes`).
        let g = OwnedGroup {
            hashes: vec!["engine".into(), "mirror".into()],
            engine_hashes: vec!["engine".into()],
            provenance: Provenance::watchlist("alice"),
            owned_episodes: vec![],
            media_type: MediaType::Show,
            coverage: vec![],
        };
        // Trigger A (Finished) reclaims the WHOLE title — incl. the pre-existing mirror copy — ONLY
        // when REMOVE_FINISHED_SHOWS is on (the opt-in library finish-cleanup). (Regression guard: a
        // blanket `engine_hashes` here would make finish-removal of a mirror show a no-op.)
        let mut fin_on = removal_hashes(&g, RemoveReason::Finished, /*flag*/ true);
        fin_on.sort();
        assert_eq!(fin_on, vec!["engine".to_string(), "mirror".to_string()]);
        // With the flag OFF, a Trigger-A removal can only be a genuine in-progress re-watch, which
        // must KEEP the user's pre-existing mirror copy — only the engine hash is reclaimed.
        assert_eq!(
            removal_hashes(&g, RemoveReason::Finished, /*flag*/ false),
            vec!["engine".to_string()]
        );
        // Trigger B (Abandoned watchlist) keeps the mirror copy regardless of the flag.
        assert_eq!(
            removal_hashes(&g, RemoveReason::Abandoned, true),
            vec!["engine".to_string()]
        );
        assert_eq!(
            removal_hashes(&g, RemoveReason::Abandoned, false),
            vec!["engine".to_string()]
        );
    }

    #[test]
    fn removal_hashes_movie_mirror_is_never_finish_reclaimed() {
        use crate::wanted::RemoveReason;
        // A MOVIE group with a pre-existing mirror copy. REMOVE_FINISHED_SHOWS never folds movies, so
        // a movie mirror must NEVER be Trigger-A reclaimed — even with the flag on — or a re-watched
        // (in /sync/playback + watched) pre-existing movie would be deleted from the user's library.
        let g = OwnedGroup {
            hashes: vec!["engine".into(), "mirror".into()],
            engine_hashes: vec!["engine".into()],
            provenance: Provenance::in_progress("alice"),
            owned_episodes: vec![],
            media_type: MediaType::Movie,
            coverage: vec![],
        };
        for flag in [true, false] {
            assert_eq!(
                removal_hashes(&g, RemoveReason::Finished, flag),
                vec!["engine".to_string()],
                "movie mirror must survive finish-removal (flag={flag})"
            );
        }
        // A PURE mirror movie (no engine hash) is therefore never finish-removed at all.
        let pure_mirror = OwnedGroup {
            hashes: vec!["mirror".into()],
            engine_hashes: vec![],
            provenance: Provenance { entries: vec![] },
            owned_episodes: vec![],
            media_type: MediaType::Movie,
            coverage: vec![],
        };
        assert!(removal_hashes(&pure_mirror, RemoveReason::Finished, true).is_empty());
        assert!(removal_hashes(&pure_mirror, RemoveReason::Finished, false).is_empty());
    }

    #[test]
    fn removal_hashes_pure_mirror_show_is_reclaimed_only_with_flag_on() {
        use crate::wanted::RemoveReason;
        // The HEADLINE finish-cleanup case: a SHOW that exists ONLY as a pre-existing library copy
        // (empty engine_hashes) must be reclaimed by Trigger A when REMOVE_FINISHED_SHOWS is on (the
        // documented "incl. shows already in the library" cleanup). If the arm regressed to require a
        // non-empty engine_hashes, this common case would silently no-op.
        let g = OwnedGroup {
            hashes: vec!["mirror".into()],
            engine_hashes: vec![],
            provenance: Provenance { entries: vec![] },
            owned_episodes: vec![],
            media_type: MediaType::Show,
            coverage: vec![],
        };
        assert_eq!(
            removal_hashes(&g, RemoveReason::Finished, /*flag*/ true),
            vec!["mirror".to_string()],
            "flag on → a pure-mirror ended show is reclaimed"
        );
        assert!(
            removal_hashes(&g, RemoveReason::Finished, /*flag*/ false).is_empty(),
            "flag off → a pure-mirror show is never finish-removed"
        );
        // Trigger B never reclaims a mirror copy regardless of the flag.
        assert!(removal_hashes(&g, RemoveReason::Abandoned, true).is_empty());
    }

    #[tokio::test]
    async fn plan_show_still_wanted_no_op() {
        // plan_reconcile_ops leaves show acquire + Trigger-A to monitor_episodes; a still-wanted
        // owned show yields nothing.
        let store = mem_store();
        store
            .put_wanted(wanted_show("alice", 1396, true, false))
            .await
            .unwrap();
        store
            .put_owned(
                "abc".into(),
                owned_record(1396, MediaKind::Series, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        let torrents = vec![torrent("t1", "ABC")];
        let ops = plan_reconcile_ops(&store, &torrents).await;
        assert_eq!(ops, vec![]);
    }

    #[tokio::test]
    async fn plan_multi_hash_remove_lists_all_hashes() {
        let store = mem_store();
        // finished in-progress-only movie owned under TWO hashes → one Remove op listing BOTH.
        store
            .put_wanted(wanted_movie("alice", 27205, false, true, true))
            .await
            .unwrap();
        store
            .put_owned(
                "aaa".into(),
                owned_record(27205, MediaKind::Movie, Provenance::in_progress("alice")),
            )
            .await
            .unwrap();
        store
            .put_owned(
                "bbb".into(),
                owned_record(27205, MediaKind::Movie, Provenance::in_progress("alice")),
            )
            .await
            .unwrap();
        let torrents = vec![torrent("t1", "AAA"), torrent("t2", "BBB")];
        let mut ops = plan_reconcile_ops(&store, &torrents).await;
        assert_eq!(ops.len(), 1, "expected a single Remove op");
        match ops.remove(0) {
            ReconcileOp::Remove {
                tmdb_id,
                mut hashes,
            } => {
                assert_eq!(tmdb_id, 27205);
                hashes.sort();
                assert_eq!(hashes, vec!["aaa".to_string(), "bbb".to_string()]);
            }
            other => panic!("expected Remove, got {:?}", other),
        }
    }

    // ── execute_remove (mem Store + MockProvider) ─────────────────────────────

    #[tokio::test]
    async fn execute_remove_drops_owned_and_selection() {
        use crate::scraper::MediaKind;
        use crate::store::{movie_slot, OwnedRecord, OwnedStatus, SelectionEntry};
        use crate::vfs::{MediaMetadata, MediaType};
        let store = mem_store();
        let req = AcquireRequest {
            imdb_id: "tt1".into(),
            tmdb_id: 27205,
            kind: MediaKind::Movie,
            season: None,
            episode: None,
            original_language: None,
            spoken_languages: Some(vec![]),
            metadata: MediaMetadata {
                title: "M".into(),
                year: None,
                media_type: MediaType::Movie,
                external_id: Some("tmdb:27205".into()),
            },
        };
        store
            .put_owned(
                "h1".into(),
                OwnedRecord {
                    request: req,
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "h1".into(),
                    file_path: "m.mkv".into(),
                },
            )
            .await
            .unwrap();

        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "tid".into(),
                hash: "h1".into(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let torrents = provider.get_torrents().await.unwrap();
        let idle = Arc::new(crate::read_activity::ReadActivity::without_startup_grace());
        execute_remove(
            &provider,
            &torrents,
            &store,
            &idle,
            Duration::from_secs(300),
            27205,
            &["h1".to_string()],
        )
        .await;
        assert!(store.get_owned("h1".into()).await.is_none());
        assert!(
            store.get_selection(movie_slot(27205)).await.is_none(),
            "removal must clear the selection slot"
        );
    }

    #[tokio::test]
    async fn execute_remove_defers_when_library_active() {
        // A removal must NOT fire while something is being streamed (multi-user stream protection):
        // the owned record + selection are left intact for a later (idle) tick.
        use crate::store::{movie_slot, SelectionEntry};
        let store = mem_store();
        store
            .put_owned(
                "h1".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "h1".into(),
                    file_path: "m.mkv".into(),
                },
            )
            .await
            .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "tid".into(),
                hash: "h1".into(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            deleted: deleted.clone(),
            ..Default::default()
        });
        let torrents = provider.get_torrents().await.unwrap();
        let activity = Arc::new(crate::read_activity::ReadActivity::without_startup_grace());
        activity.touch("Movies/M/m.mkv").await; // an active read
        execute_remove(
            &provider,
            &torrents,
            &store,
            &activity,
            Duration::from_secs(300),
            27205,
            &["h1".to_string()],
        )
        .await;
        assert!(
            deleted.lock().unwrap().is_empty(),
            "an active library must NOT issue any provider delete"
        );
        assert!(
            store.get_owned("h1".into()).await.is_some(),
            "an active library must defer the removal (owned record kept for retry)"
        );
        assert!(
            store.get_selection(movie_slot(27205)).await.is_some(),
            "the selection slot must be kept while deferred"
        );
    }

    /// Start-up grace: a Trigger-A/B removal right after the process starts (fresh tracker, no read
    /// history yet) defers for the first idle window, exactly like an active library.
    #[tokio::test]
    async fn execute_remove_defers_during_the_startup_grace() {
        let store = mem_store();
        store
            .put_owned(
                "h1".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "tid".into(),
                hash: "h1".into(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            deleted: deleted.clone(),
            ..Default::default()
        });
        let torrents = provider.get_torrents().await.unwrap();
        let fresh = Arc::new(crate::read_activity::ReadActivity::new());
        execute_remove(
            &provider,
            &torrents,
            &store,
            &fresh,
            Duration::from_secs(300),
            27205,
            &["h1".to_string()],
        )
        .await;
        assert!(
            deleted.lock().unwrap().is_empty(),
            "no delete during the grace"
        );
        assert!(
            store.get_owned("h1".into()).await.is_some(),
            "kept for a later tick"
        );
    }

    #[tokio::test]
    async fn execute_remove_deletes_torrent_and_owned_record() {
        let store = mem_store();
        store
            .put_owned(
                "h1".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            deleted: deleted.clone(),
            ..Default::default()
        });
        let torrents = vec![torrent("t1", "H1")]; // hash case differs from stored "h1"
        let idle = Arc::new(crate::read_activity::ReadActivity::without_startup_grace());
        execute_remove(
            &provider,
            &torrents,
            &store,
            &idle,
            Duration::from_secs(300),
            27205,
            &["h1".to_string()],
        )
        .await;
        assert_eq!(
            *deleted.lock().unwrap(),
            vec!["t1".to_string()],
            "the torrent must be deleted"
        );
        assert!(
            store.get_owned("h1".to_string()).await.is_none(),
            "the owned record must be removed"
        );
    }

    /// Every listed torrent of a removed hash is deleted (a same-hash duplicate the idle-gated scan
    /// dedup hasn't deleted yet would otherwise survive and be re-mirrored as an empty-provenance
    /// copy the lifecycle never removes), and the hash's authoritative identification goes with its
    /// owned record, like on every other destructive path.
    #[tokio::test]
    async fn execute_remove_deletes_every_same_hash_torrent_and_the_authoritative_id() {
        let store = mem_store();
        store
            .put_owned(
                "h1".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        store
            .put_authoritative(
                "h1".into(),
                MediaMetadata {
                    title: "Inception".into(),
                    year: None,
                    media_type: MediaType::Movie,
                    external_id: Some("tmdb:27205".into()),
                },
            )
            .await
            .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            deleted: deleted.clone(),
            ..Default::default()
        });
        let torrents = vec![
            torrent("t1", "H1"),
            torrent("t2", "h1"),
            torrent("t3", "other"),
        ];
        let idle = Arc::new(crate::read_activity::ReadActivity::without_startup_grace());
        execute_remove(
            &provider,
            &torrents,
            &store,
            &idle,
            Duration::from_secs(300),
            27205,
            &["h1".to_string()],
        )
        .await;
        assert_eq!(
            *deleted.lock().unwrap(),
            vec!["t1".to_string(), "t2".to_string()],
            "both copies of the hash are deleted, nothing else"
        );
        assert!(store.get_owned("h1".to_string()).await.is_none());
        assert!(
            store.authoritative_meta("h1".to_string()).await.is_none(),
            "the authoritative row is dropped with the owned record"
        );
    }

    #[tokio::test]
    async fn execute_remove_keeps_owned_record_when_delete_fails() {
        // On a delete_torrent failure, execute_remove must KEEP the owned record (and its selection)
        // so the next reconcile tick re-derives and retries the removal, rather than orphaning the
        // torrent on the provider.
        use crate::store::{movie_slot, SelectionEntry};
        let store = mem_store();
        store
            .put_owned(
                "h1".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "h1".into(),
                    file_path: "m.mkv".into(),
                },
            )
            .await
            .unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            fail_delete: true,
            ..Default::default()
        });
        let torrents = vec![torrent("t1", "h1")];
        let idle = Arc::new(crate::read_activity::ReadActivity::without_startup_grace());
        execute_remove(
            &provider,
            &torrents,
            &store,
            &idle,
            Duration::from_secs(300),
            27205,
            &["h1".to_string()],
        )
        .await;
        assert!(
            store.get_owned("h1".to_string()).await.is_some(),
            "a failed delete must leave the owned record for the next tick to retry"
        );
        assert!(
            store.get_selection(movie_slot(27205)).await.is_some(),
            "a failed delete must leave the selection slot intact"
        );
    }

    /// When `get_torrents` fails, `reconcile_wanted` must early-return without executing any ops:
    /// no `delete_torrent` call and no owned records removed. Without the guard, a failed fetch
    /// defaults to an empty listing and Trigger-B fires, incorrectly removing owned content.
    #[tokio::test]
    async fn reconcile_wanted_skips_tick_when_get_torrents_fails() {
        use crate::config::AcquisitionConfig;

        let store = mem_store();
        // A show owned via watchlist with no current wanter → Trigger B fires if get_torrents
        // returns Ok([]) (empty listing). With the fail guard it must be skipped entirely.
        store
            .put_owned(
                "aaa".into(),
                owned_record(1396, MediaKind::Series, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();

        let deleted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            fail_get_torrents: true,
            deleted: deleted.clone(),
            ..Default::default()
        });

        let scraper: Arc<dyn crate::scraper::Scraper> =
            Arc::new(crate::scraper::TorrentioScraper::new(
                None,
                crate::provider::ProviderKind::TorBox,
                "tok",
                reqwest::Client::new(),
            ));
        let validator: Arc<dyn crate::acquire::TitleValidator> =
            Arc::new(crate::acquire::TmdbTitleValidator {
                tmdb: Arc::new(crate::tmdb_client::TmdbClient::offline().await),
            });
        let prober: Arc<dyn crate::acquire::Prober> = Arc::new(crate::acquire::HttpProber {
            http: reqwest::Client::new(),
        });
        let engine = crate::acquire::AcquisitionEngine::new(
            provider.clone(),
            scraper,
            validator,
            prober,
            store.clone(),
            AcquisitionConfig::default().prefs,
            5,
            std::time::Duration::from_secs(1800),
            std::time::Duration::from_secs(600),
        );
        let tmdb = crate::tmdb_client::TmdbClient::offline().await;

        let idle = Arc::new(crate::read_activity::ReadActivity::without_startup_grace());
        reconcile_wanted(
            &engine,
            &provider,
            &tmdb,
            &store,
            &idle,
            Duration::from_secs(300),
            true,
        )
        .await;

        assert!(
            deleted.lock().unwrap().is_empty(),
            "delete_torrent must not be called when get_torrents fails"
        );
        assert!(
            store.get_owned("aaa".to_string()).await.is_some(),
            "owned record must survive a skipped tick"
        );
    }

    #[test]
    fn sync_summary_removals_trustworthy_logic() {
        // No enrolled accounts → an empty `wanted` set is authoritative (nobody to want anything).
        assert!(SyncSummary {
            enrolled: 0,
            succeeded: 0
        }
        .removals_trustworthy());
        // Accounts exist and at least one synced → the set was confirmed this cycle.
        assert!(SyncSummary {
            enrolled: 2,
            succeeded: 1
        }
        .removals_trustworthy());
        // Accounts exist but NONE synced (post-migration blackout / Trakt unreachable) → untrustworthy.
        assert!(!SyncSummary {
            enrolled: 2,
            succeeded: 0
        }
        .removals_trustworthy());
        assert!(!SyncSummary {
            enrolled: 1,
            succeeded: 0
        }
        .removals_trustworthy());
    }

    #[tokio::test]
    async fn reconcile_wanted_suppresses_removal_when_wanted_untrustworthy() {
        use crate::config::AcquisitionConfig;

        // An engine-owned (watchlist-provenance) show, present in the listing, with an EMPTY `wanted`
        // set — the state left transiently by the v4→v5 migration clearing `wanted`. With an
        // untrustworthy set (no sync confirmed it), Trigger-B removal must be SUPPRESSED; once a sync
        // confirms the (still-empty) set, the same reconcile removes it. Guards finding 7.1: a failed
        // first post-migration sync must not delete engine-acquired content.
        let store = mem_store();
        store
            .put_owned(
                "aaa".into(),
                owned_record(1396, MediaKind::Series, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();

        let deleted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("t1", "AAA")],
            deleted: deleted.clone(),
            ..Default::default()
        });
        let scraper: Arc<dyn crate::scraper::Scraper> =
            Arc::new(crate::scraper::TorrentioScraper::new(
                None,
                crate::provider::ProviderKind::TorBox,
                "tok",
                reqwest::Client::new(),
            ));
        let validator: Arc<dyn crate::acquire::TitleValidator> =
            Arc::new(crate::acquire::TmdbTitleValidator {
                tmdb: Arc::new(crate::tmdb_client::TmdbClient::offline().await),
            });
        let prober: Arc<dyn crate::acquire::Prober> = Arc::new(crate::acquire::HttpProber {
            http: reqwest::Client::new(),
        });
        let engine = crate::acquire::AcquisitionEngine::new(
            provider.clone(),
            scraper,
            validator,
            prober,
            store.clone(),
            AcquisitionConfig::default().prefs,
            5,
            std::time::Duration::from_secs(1800),
            std::time::Duration::from_secs(600),
        );
        let tmdb = crate::tmdb_client::TmdbClient::offline().await;
        let idle = Arc::new(crate::read_activity::ReadActivity::without_startup_grace());

        // Untrustworthy (no sync confirmed the set): removal suppressed.
        reconcile_wanted(
            &engine,
            &provider,
            &tmdb,
            &store,
            &idle,
            Duration::from_secs(300),
            false,
        )
        .await;
        assert!(
            deleted.lock().unwrap().is_empty(),
            "an untrustworthy (unconfirmed) empty wanted set must NOT delete engine-acquired content"
        );
        assert!(
            store.get_owned("aaa".to_string()).await.is_some(),
            "the owned record must survive the suppressed tick"
        );

        // Trustworthy (a sync confirmed the set): the genuine abandonment IS removed.
        reconcile_wanted(
            &engine,
            &provider,
            &tmdb,
            &store,
            &idle,
            Duration::from_secs(300),
            true,
        )
        .await;
        assert_eq!(
            deleted.lock().unwrap().as_slice(),
            &["t1".to_string()],
            "a confirmed abandonment must delete the engine-acquired torrent"
        );
    }
}

#[cfg(test)]
mod monitor_episodes_tests {
    use super::*;
    use crate::tmdb_client::EpisodeAirDate;

    fn ep(season: u32, episode: u32, air: Option<(i32, u32, u32)>) -> EpisodeAirDate {
        EpisodeAirDate {
            season,
            episode,
            air_date: air.map(|(y, m, d)| chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap()),
        }
    }

    // ── aired_pairs (pure — the chrono boundary heart of monitor_episodes) ─────

    #[test]
    fn aired_pairs_includes_past_and_today_excludes_future_and_none() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
        let eps = vec![
            ep(1, 1, Some((2026, 6, 7))), // yesterday → included
            ep(1, 2, Some((2026, 6, 9))), // tomorrow  → excluded
            ep(1, 3, Some((2026, 6, 8))), // today     → included (≤ boundary)
            ep(1, 4, None),               // no air date → excluded
        ];
        assert_eq!(aired_pairs(&eps, today), vec![(1, 1), (1, 3)]);
    }

    #[test]
    fn aired_pairs_preserves_input_order() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
        let eps = vec![
            ep(2, 5, Some((2020, 1, 1))),
            ep(1, 1, Some((2019, 1, 1))),
            ep(1, 2, Some((2030, 1, 1))), // future → dropped
            ep(3, 1, Some((2021, 1, 1))),
        ];
        assert_eq!(aired_pairs(&eps, today), vec![(2, 5), (1, 1), (3, 1)]);
    }

    #[test]
    fn aired_pairs_empty_input_is_empty() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
        assert_eq!(aired_pairs(&[], today), Vec::<(u32, u32)>::new());
    }

    // ── guard_removal_on_incomplete_aired (the data-loss guard) ────────────────

    #[test]
    fn incomplete_aired_drops_remove_actions() {
        use crate::wanted::{Action, RemoveReason};
        let actions = vec![
            Action::Remove {
                tmdb_id: 7,
                hash: "abc".into(),
                reason: RemoveReason::Finished,
            },
            Action::AcquireEpisode {
                tmdb_id: 7,
                season: 1,
                episode: 2,
            },
        ];
        // complete == false (a TMDB lookup failed): the Remove must be dropped, the acquire kept.
        let kept = guard_removal_on_incomplete_aired(actions, false, false);
        assert_eq!(
            kept,
            vec![Action::AcquireEpisode {
                tmdb_id: 7,
                season: 1,
                episode: 2
            }]
        );
    }

    #[test]
    fn complete_aired_keeps_remove_actions() {
        use crate::wanted::{Action, RemoveReason};
        let actions = vec![Action::Remove {
            tmdb_id: 7,
            hash: "abc".into(),
            reason: RemoveReason::Finished,
        }];
        // complete == true AND non-empty: a genuine Trigger-A removal is preserved.
        let kept = guard_removal_on_incomplete_aired(actions.clone(), true, false);
        assert_eq!(kept, actions);
    }

    #[test]
    fn empty_but_complete_aired_drops_remove_actions() {
        use crate::wanted::{Action, RemoveReason};
        // The subtle data-loss case: complete == true but the aired set is EMPTY (TMDB reported no
        // non-Specials seasons, or all air dates null). `user_finished`'s `all()` over an empty set
        // is vacuously true for an Ended show, so a Remove must NOT be honoured — an Ended show with
        // zero aired episodes is never genuinely finishable.
        let actions = vec![Action::Remove {
            tmdb_id: 7,
            hash: "abc".into(),
            reason: RemoveReason::Finished,
        }];
        assert!(
            guard_removal_on_incomplete_aired(actions, /*complete*/ true, /*empty*/ true)
                .is_empty(),
            "an empty aired set must never vacuously remove a show"
        );
    }

    #[test]
    fn incomplete_aired_keeps_pure_acquire_set() {
        use crate::wanted::Action;
        let actions = vec![
            Action::AcquireEpisode {
                tmdb_id: 9,
                season: 1,
                episode: 1,
            },
            Action::AcquireEpisode {
                tmdb_id: 9,
                season: 1,
                episode: 2,
            },
        ];
        assert_eq!(
            guard_removal_on_incomplete_aired(actions.clone(), false, true),
            actions
        );
    }
}

// ── dedup_owned (async wrapper: idle-gate + removal + cleanup) ────────────────────────────────
#[cfg(test)]
mod dedup_owned_tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::config::Config;
    use crate::provider::{DebridProvider, MockProvider};
    use crate::rd_client::Torrent;
    use crate::repair::RepairManager;
    use crate::scraper::MediaKind;
    use crate::store::{
        movie_slot, AcquireRequest, OwnedRecord, OwnedStatus, Provenance, SelectionEntry, Store,
    };
    use crate::tmdb_client::TmdbClient;
    use crate::vfs::{DebridVfs, MediaMetadata, MediaType};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn mem_store() -> Store {
        let db = Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        Store::from_database(db).unwrap()
    }

    fn build_app(store: Store, provider: Arc<dyn DebridProvider>, remove_dupes: bool) -> AppState {
        let mut config =
            Config::from_parts(None, Some("tb".into()), Some("k".into()), None, None, None)
                .unwrap();
        config.dedup_remove_duplicates = remove_dupes;
        let scraper: Arc<dyn crate::scraper::Scraper> =
            Arc::new(crate::scraper::TorrentioScraper::new(
                None,
                crate::provider::ProviderKind::TorBox,
                "tok",
                reqwest::Client::new(),
            ));
        let validator: Arc<dyn crate::acquire::TitleValidator> =
            Arc::new(crate::acquire::TmdbTitleValidator {
                tmdb: Arc::new(TmdbClient::new("k".into()).unwrap()),
            });
        let prober: Arc<dyn crate::acquire::Prober> = Arc::new(crate::acquire::HttpProber {
            http: reqwest::Client::new(),
        });
        let engine = Arc::new(crate::acquire::AcquisitionEngine::new(
            provider.clone(),
            scraper.clone(),
            validator,
            prober,
            store.clone(),
            config.acquisition.prefs.clone(),
            5,
            std::time::Duration::from_secs(1800),
            std::time::Duration::from_secs(600),
        ));
        AppState {
            provider: provider.clone(),
            tmdb_client: Arc::new(TmdbClient::new("k".into()).unwrap()),
            vfs: Arc::new(RwLock::new(DebridVfs::new())),
            store,
            repair_manager: Arc::new(RepairManager::new(provider)),
            config: Arc::new(config),
            jellyfin_client: None,
            http_client: reqwest::Client::new(),
            scraper,
            engine,
            trakt_client: None,
            read_activity: Arc::new(crate::read_activity::ReadActivity::without_startup_grace()),
        }
    }

    fn movie_owned(tmdb: u64, score: i64) -> OwnedRecord {
        OwnedRecord {
            request: AcquireRequest {
                imdb_id: String::new(),
                tmdb_id: tmdb,
                kind: MediaKind::Movie,
                season: None,
                episode: None,
                original_language: None,
                spoken_languages: Some(vec![]),
                metadata: MediaMetadata {
                    title: "M".into(),
                    year: None,
                    media_type: MediaType::Movie,
                    external_id: Some(format!("tmdb:{tmdb}")),
                },
            },
            provenance: Provenance { entries: vec![] }, // mirror (empty) — dedup operates on these
            added_at: 0,
            status: OwnedStatus::Verified,
            provides: vec![],
            quality: Some(crate::release::QualitySummary {
                cached: true,
                source_tier: 0,
                resolution: 1080,
                score,
                unverified: false,
            }),
        }
    }

    fn torrent(id: &str, hash: &str) -> Torrent {
        Torrent {
            id: id.into(),
            hash: hash.into(),
            status: "downloaded".into(),
            ..Default::default()
        }
    }

    /// Two present copies of the same title; selection points at the kept (better) one so quality
    /// decides. Build the store + a MockProvider listing both, plus the deleted-id sink.
    async fn fixture() -> (
        Store,
        Arc<std::sync::Mutex<Vec<String>>>,
        Vec<Torrent>,
        Arc<dyn DebridProvider>,
    ) {
        let store = mem_store();
        store
            .put_owned("better".into(), movie_owned(27205, 100))
            .await
            .unwrap();
        store
            .put_owned("worse".into(), movie_owned(27205, 50))
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "better".into(),
                    file_path: "m.mkv".into(),
                },
            )
            .await
            .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("t_better", "better"), torrent("t_worse", "worse")],
            deleted: deleted.clone(),
            ..Default::default()
        });
        let torrents = provider.get_torrents().await.unwrap();
        (store, deleted, torrents, provider)
    }

    #[tokio::test]
    async fn removes_redundant_duplicate_when_idle_and_enabled() {
        let (store, deleted, torrents, provider) = fixture().await;
        let meta = MediaMetadata {
            title: "Inception".into(),
            year: None,
            media_type: MediaType::Movie,
            external_id: Some("tmdb:27205".into()),
        };
        store
            .put_authoritative("worse".into(), meta.clone())
            .await
            .unwrap();
        store
            .put_authoritative("better".into(), meta)
            .await
            .unwrap();
        let app = build_app(store.clone(), provider, true);
        dedup_owned(&app, &torrents).await;
        assert_eq!(
            *deleted.lock().unwrap(),
            vec!["t_worse".to_string()],
            "the lower-quality duplicate's torrent is deleted"
        );
        assert!(store.get_owned("worse".into()).await.is_none());
        assert!(
            store.authoritative_meta("worse".into()).await.is_none(),
            "the removed copy's authoritative row goes with it"
        );
        assert!(store.authoritative_meta("better".into()).await.is_some());
        assert!(
            store.get_owned("better".into()).await.is_some(),
            "the kept copy survives"
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "better",
            "the kept copy's selection is intact"
        );
    }

    #[tokio::test]
    async fn defers_when_library_active() {
        let (store, deleted, torrents, provider) = fixture().await;
        let app = build_app(store.clone(), provider, true);
        app.read_activity.touch("Movies/anything.mkv").await; // active stream anywhere
        dedup_owned(&app, &torrents).await;
        assert!(
            deleted.lock().unwrap().is_empty(),
            "an active library must defer the destructive dedup removal"
        );
        assert!(
            store.get_owned("worse".into()).await.is_some(),
            "deferred → the owned record is kept for a later idle pass"
        );
    }

    /// Start-up grace: right after the process starts (a fresh tracker, nothing read yet) the
    /// library is NOT idle for the first idle window, so enabled dedup removals defer instead of
    /// firing on the first tick (the 2026-11-11 redeploy deleted 81 torrents 34 s after start-up).
    #[tokio::test]
    async fn defers_removals_during_the_startup_grace() {
        let (store, deleted, torrents, provider) = fixture().await;
        let mut app = build_app(store.clone(), provider, true);
        app.read_activity = Arc::new(crate::read_activity::ReadActivity::new());
        dedup_owned(&app, &torrents).await;
        assert!(
            deleted.lock().unwrap().is_empty(),
            "no dedup deletion within the first idle window after start-up"
        );
        assert!(store.get_owned("worse".into()).await.is_some());
    }

    #[tokio::test]
    async fn dry_run_default_logs_but_does_not_remove() {
        let (store, deleted, torrents, provider) = fixture().await;
        let app = build_app(store.clone(), provider, false); // DEDUP_REMOVE_DUPLICATES unset
        dedup_owned(&app, &torrents).await;
        assert!(
            deleted.lock().unwrap().is_empty(),
            "dry-run (default) must not delete anything"
        );
        assert!(store.get_owned("worse".into()).await.is_some());
    }
}
