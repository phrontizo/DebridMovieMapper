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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Resolve a torrent's metadata: an authoritative `hash -> MediaMetadata` (recorded by the
/// acquisition engine for content we chose) wins over filename-based TMDB identification.
/// The hash is lowercased to match the engine's lowercased keys.
async fn resolve_metadata(
    store: &Store,
    tmdb_client: &TmdbClient,
    info: &crate::rd_client::TorrentInfo,
) -> MediaMetadata {
    match store
        .authoritative_meta(info.hash.to_ascii_lowercase())
        .await
    {
        Some(m) => m,
        None => identify_torrent(info, tmdb_client).await,
    }
}

pub struct ScanConfig {
    pub app: AppState,
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
        match provider.get_torrents().await {
            Ok(torrents) => {
                if torrents.is_empty() {
                    warn!("No torrents found in {} account.", provider.name());
                }

                engine.observe(&torrents).await;

                // Deduplicate torrents by hash — keep the newest "downloaded" entry per hash.
                // Duplicates arise when repair's add_magnet leaks a torrent, or when
                // external tools (e.g. DebridMediaManager) re-add the same hash.
                let (deduped_torrents, duplicate_ids) = dedup_torrents_by_hash(&torrents);
                for dup_id in duplicate_ids {
                    let rd = provider.clone();
                    tokio::spawn(async move {
                        if let Err(e) = rd.delete_torrent(&dup_id).await {
                            tracing::error!("Failed to delete duplicate torrent {}: {}", dup_id, e);
                        }
                    });
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
                                        let metadata =
                                            resolve_metadata(&store, &tmdb_client, &info).await;
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
                            Ok((id, info, metadata)) => {
                                pending_db_writes.push((
                                    id.clone(),
                                    info.clone(),
                                    metadata.clone(),
                                ));
                                seen_torrents.insert(id, (info.clone(), metadata.clone()));
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
                record_mirror_owned(&store, &tmdb_client, &current_data).await;

                // Duplicate-dedup: now the whole library is owned, plan + (if enabled, idle-gated)
                // remove redundant duplicate torrents. Dry-run (log only) unless
                // DEDUP_REMOVE_DUPLICATES is set.
                dedup_owned(&app).await;

                let current_ids: std::collections::HashSet<&str> =
                    deduped_torrents.iter().map(|t| t.id.as_str()).collect();
                // Collect stale IDs before retain so we can clean up redb
                let stale_ids = stale_ids(&seen_torrents, &current_ids);
                seen_torrents.retain(|id, _| current_ids.contains(id.as_str()));
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

/// Deduplicate torrents by hash, keeping the first-seen `downloaded` entry per hash.
/// The debrid API returns torrents newest-first, so "first seen" is the newest. Torrents
/// that are not `downloaded`, or that have no hash, are always kept (they can't be matched
/// as duplicates yet). Returns the kept torrents (in original order) plus the ids of the
/// duplicates that should be deleted.
fn dedup_torrents_by_hash(
    torrents: &[crate::rd_client::Torrent],
) -> (Vec<&crate::rd_client::Torrent>, Vec<String>) {
    let mut seen_hashes: HashMap<&str, usize> = HashMap::new();
    let mut deduped: Vec<&crate::rd_client::Torrent> = Vec::new();
    let mut duplicate_ids: Vec<String> = Vec::new();
    for torrent in torrents {
        if torrent.status != "downloaded" || torrent.hash.is_empty() {
            deduped.push(torrent);
            continue;
        }
        if let Some(&existing_idx) = seen_hashes.get(torrent.hash.as_str()) {
            let kept = &deduped[existing_idx];
            warn!(
                "Duplicate hash {} found: keeping torrent {} ({}), deleting duplicate {} ({})",
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
    // Build VFS without holding the lock to avoid blocking WebDAV reads during scans
    let new_vfs = DebridVfs::build(filtered, &selection);
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

// ── Trakt → wanted sync (SP2 Task 7) ─────────────────────────────────────────

/// Refresh a token this many seconds before it expires, so a sync never races the expiry.
const REFRESH_BUFFER_SECS: u64 = 86_400;

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
        // A watchlisted show already wants its whole run, so catch-up is redundant there — only
        // mark shows that aren't already on the watchlist.
        if !a.watchlist {
            a.in_progress = true;
        }
    }

    agg.into_iter()
        .map(|((_mt, tmdb_id), a)| {
            let (watched_state, status) = match a.media_type {
                MediaType::Movie => (
                    WatchedState::Movie {
                        watched: watched.movies.contains(&tmdb_id),
                    },
                    None,
                ),
                MediaType::Show => {
                    let watched_episodes = watched
                        .shows
                        .iter()
                        .find(|s| s.tmdb_id == tmdb_id)
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

/// For every enrolled Trakt user, refresh near-expiry tokens, pull their Trakt reads + per-show
/// TMDB status, and rewrite their `wanted` rows. A user whose sync fails is `warn!`ed and flagged
/// for re-enrolment (`needs_reenrolment = true`); because `sync_trakt_user` performs all of its
/// `wanted` writes only after every fetch has succeeded, a failure leaves that user's existing
/// `wanted` rows untouched.
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

pub async fn sync_trakt(
    trakt: &std::sync::Arc<dyn crate::trakt_client::TraktClient>,
    tmdb: &crate::tmdb_client::TmdbClient,
    store: &crate::store::Store,
    catchup_lookback_secs: Option<u64>,
    remove_finished_shows: bool,
) {
    for (slug, tokens) in store.all_trakt_tokens().await {
        if let Err(e) = sync_trakt_user(
            trakt,
            tmdb,
            store,
            &slug,
            tokens.clone(),
            catchup_lookback_secs,
            remove_finished_shows,
        )
        .await
        {
            warn!(
                "Trakt sync failed for {}: {}; flagging account for re-enrolment",
                slug, e
            );
            // A successful refresh inside sync_trakt_user persists a fresh (single-use) token before a
            // later read can fail; re-read so we don't clobber it with the stale pre-refresh snapshot.
            let current = store.get_trakt_tokens(slug.clone()).await.unwrap_or(tokens);
            let flagged = crate::store::TraktTokens {
                needs_reenrolment: true,
                ..current
            };
            if let Err(pe) = store.put_trakt_tokens(slug, flagged).await {
                error!("Failed to persist re-enrolment flag for account: {}", pe);
            }
        }
    }
}

/// Sync one user. Returns `Err` (so `sync_trakt` flags the account) on a token-refresh or
/// Trakt-read failure; a TMDB hiccup is tolerated (it is not a de-auth). All `wanted` writes
/// happen only after every Trakt fetch has succeeded, so an early error leaves `wanted` intact.
///
/// NOTE: store-write errors (`put_wanted`/`remove_wanted`/`put_trakt_tokens`) also propagate as
/// `Err` and therefore trigger `needs_reenrolment`; this is intentional and self-healing — the
/// flag is cleared on the next successful sync.
async fn sync_trakt_user(
    trakt: &std::sync::Arc<dyn crate::trakt_client::TraktClient>,
    tmdb: &crate::tmdb_client::TmdbClient,
    store: &crate::store::Store,
    slug: &str,
    mut tokens: crate::store::TraktTokens,
    catchup_lookback_secs: Option<u64>,
    remove_finished_shows: bool,
) -> Result<(), crate::error::AppError> {
    use crate::store::TraktTokens;
    use crate::vfs::MediaType;

    // Refresh if at/near expiry, persisting the fresh tokens before using them.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if tokens.expires_at <= now + REFRESH_BUFFER_SECS {
        let r = trakt.refresh(&tokens.refresh).await?;
        tokens = TraktTokens {
            access: r.access_token,
            refresh: r.refresh_token,
            expires_at: r.created_at + r.expires_in,
            username: tokens.username.clone(),
            needs_reenrolment: false,
        };
        store
            .put_trakt_tokens(slug.to_string(), tokens.clone())
            .await?;
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
    // The owned show library (after the account mirror, this is the user's whole library) — used to
    // identify finished-ended shows that are owned and so candidates for finish-cleanup.
    let owned_show_ids: std::collections::HashSet<u64> = store
        .all_owned()
        .await
        .into_iter()
        .filter(|(_, r)| matches!(r.request.kind, MediaKind::Series))
        .map(|(_, r)| r.request.tmdb_id)
        .collect();

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

    // Finish-cleanup candidates = caught-up + owned + ENDED. These aren't "behind" (so they're not
    // caught up for acquisition), but folding them into the wanted-set lets the reconciler's
    // Trigger-A finish removal fire (the acquire path acquires nothing — every episode is watched).
    // Gated on `remove_finished_shows`: when OFF we only LOG the candidates (a preview) and do NOT
    // fold them, so nothing is removed; when ON they fold and are removed.
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
    // a numeric id are tracked independently.
    let existing: Vec<(MediaType, u64)> = store
        .all_wanted()
        .await
        .into_iter()
        .filter(|r| r.user == slug)
        .map(|r| (r.media_type.clone(), r.tmdb_id))
        .collect();
    let new_keys: std::collections::HashSet<(MediaType, u64)> = new
        .iter()
        .map(|r| (r.media_type.clone(), r.tmdb_id))
        .collect();
    for (mt, id) in existing {
        if !new_keys.contains(&(mt.clone(), id)) {
            store.remove_wanted(slug.to_string(), mt, id).await?;
        }
    }
    for rec in new {
        store.put_wanted(rec).await?;
    }

    // Clear a stale re-enrolment flag now that this sync has succeeded.
    if tokens.needs_reenrolment {
        store
            .put_trakt_tokens(
                slug.to_string(),
                TraktTokens {
                    needs_reenrolment: false,
                    ..tokens
                },
            )
            .await?;
    }

    Ok(())
}

// ── reconcile_wanted (SP2 Task 8) ────────────────────────────────────────────

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

/// The numeric tmdb id from a `MediaMetadata.external_id` like `"tmdb:1396"` (else `None`).
fn meta_tmdb_id(metadata: &MediaMetadata) -> Option<u64> {
    metadata
        .external_id
        .as_deref()
        .and_then(|s| s.strip_prefix("tmdb:"))
        .and_then(|s| s.parse::<u64>().ok())
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
/// a pass, and a resolution failure is non-fatal (the record is still written, just not upgradeable
/// until a later pass fills the id).
///
/// Idempotent and non-destructive: a hash the engine already owns (or a previously-recorded mirror
/// hash) is left untouched, so an in-flight `Pending` acquisition or an engine record's
/// provenance/quality/`provides` is never clobbered. `provides` for a show is the SE-parsed set of
/// its selected video files (a movie's is empty); `quality` is `None`.
pub(crate) async fn record_mirror_owned(
    store: &Store,
    tmdb: &TmdbClient,
    current_data: &[(crate::rd_client::TorrentInfo, MediaMetadata)],
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Resolve each title's IMDB id at most once per pass (many torrents can share a tmdb_id).
    let mut imdb_cache: HashMap<(MediaType, u64), String> = HashMap::new();
    let mut recorded = 0usize;
    for (info, metadata) in current_data {
        let Some(tmdb_id) = meta_tmdb_id(metadata) else {
            continue;
        };
        let hash = info.hash.to_ascii_lowercase();
        if hash.is_empty() {
            continue;
        }
        // Engine-owned OR already-mirrored → leave untouched (idempotent; never clobber an
        // in-flight Pending acquisition or an engine record's provenance/provides/quality).
        if store.get_owned(hash.clone()).await.is_some() {
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
        let imdb_id = match imdb_cache.entry((media_type.clone(), tmdb_id)) {
            std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let resolved = tmdb
                    .external_imdb_id(tmdb_id, media_type.clone())
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                e.insert(resolved).clone()
            }
        };
        let rec = OwnedRecord {
            request: AcquireRequest {
                imdb_id,
                tmdb_id,
                kind,
                season: None,
                episode: None,
                original_language: None,
                metadata: metadata.clone(),
            },
            // Empty provenance: owned + upgradeable, NOT a protected manual class. No Trakt wanter
            // and no watchlist provenance means neither removal trigger fires, so the user's
            // pre-existing library is kept while still following the normal upgrade path.
            provenance: Provenance { entries: vec![] },
            added_at: now,
            status: OwnedStatus::Verified,
            provides,
            quality: None,
        };
        match store.put_owned(hash.clone(), rec).await {
            Ok(()) => recorded += 1,
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
/// over TMDB's `external_ids`) and (title, year, original_language) from TMDB. Returns `Ok(None)`
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
    // Only hit TMDB's external_ids when Trakt didn't already give us an IMDB id.
    let tmdb_external = match &imdb_hint {
        Some(_) => None,
        None => tmdb.external_imdb_id(tmdb_id, media_type.clone()).await?,
    };
    let Some(imdb_id) = choose_imdb(imdb_hint, tmdb_external) else {
        return Ok(None);
    };
    let (title, year, original_language) = tmdb.details(tmdb_id, media_type.clone()).await?;
    Ok(Some(AcquireRequest {
        imdb_id,
        tmdb_id,
        kind,
        season,
        episode,
        original_language,
        metadata: MediaMetadata {
            title,
            year,
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
    pub provenance: Provenance,
    pub owned_episodes: Vec<(u32, u32)>,
    pub media_type: MediaType,
}

/// Group every owned record by its request's `tmdb_id` into an [`OwnedGroup`]. Shared by
/// `plan_reconcile_ops` (Task 8) and `monitor_episodes` (Task 9) so the owned-grouping +
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
                provenance: Provenance {
                    entries: Vec::new(),
                },
                owned_episodes: Vec::new(),
                media_type: mt.clone(),
            });
        group.hashes.push(hash.to_ascii_lowercase());
        group.provenance.merge(&rec.provenance);
        // SP3: prefer the recorded `provides` (a pack supplies many episodes); fall back to the
        // request's single (season, episode) for pre-SP3 records that have no `provides` yet.
        if rec.provides.is_empty() {
            if let (Some(s), Some(e)) = (rec.request.season, rec.request.episode) {
                group.owned_episodes.push((s, e));
            }
        } else {
            group.owned_episodes.extend(rec.provides.iter().copied());
        }
    }
    for g in owned_by.values_mut() {
        g.owned_episodes.sort_unstable();
        g.owned_episodes.dedup();
    }
    owned_by
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
/// choose a minimal keep-set that still covers all owned content and mark fully-redundant
/// duplicates for removal:
/// - **Movie**: every hash is the same film → keep the single top-priority hash, remove the rest.
/// - **Show**: a greedy set-cover over the episodes each hash `provides` — keep a hash only if it
///   adds an episode not already covered by a higher-priority kept hash; a hash whose every episode
///   is already covered is redundant → remove. Complementary packs/episodes are therefore all kept.
///   A show hash with **empty `provides`** (unknown coverage) is **always kept** — never removed on
///   a guess.
///
/// Keep-priority (highest kept first): currently-served (`selected`) > higher `quality.score` (an
/// absent summary ranks lowest) > more episodes provided > lexicographically-smallest hash
/// (deterministic). Only groups with at least one removal are returned. Absent (not-`present`)
/// hashes are ignored entirely — they're already gone, never a removal target or coverage source.
pub(crate) fn plan_dedup(
    owned: &[(String, OwnedRecord)],
    present: &std::collections::HashSet<String>,
    selected: &std::collections::HashSet<String>,
) -> Vec<DedupPlan> {
    use std::collections::{BTreeMap, HashSet};
    let mut groups: BTreeMap<(MediaType, u64), Vec<(String, &OwnedRecord)>> = BTreeMap::new();
    for (hash, rec) in owned {
        let h = hash.to_ascii_lowercase();
        if !present.contains(&h) {
            continue;
        }
        let mt = media_type_of(rec.request.kind);
        groups
            .entry((mt, rec.request.tmdb_id))
            .or_default()
            .push((h, rec));
    }

    let mut plans = Vec::new();
    for ((media_type, tmdb_id), mut hashes) in groups {
        if hashes.len() < 2 {
            continue; // no possible duplicate
        }
        // Sort by keep-priority DESC; deterministic tie-break on hash ASC.
        hashes.sort_by(|(ha, ra), (hb, rb)| {
            let key = |h: &str, r: &OwnedRecord| {
                (
                    selected.contains(h),
                    r.quality.as_ref().map(|q| q.score).unwrap_or(i64::MIN),
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
                for (h, _) in &hashes[1..] {
                    remove.push(h.clone());
                }
            }
            MediaType::Show => {
                let mut covered: HashSet<(u32, u32)> = HashSet::new();
                for (h, r) in &hashes {
                    if r.provides.is_empty() {
                        keep.push(h.clone()); // unknown coverage → never remove on a guess
                        continue;
                    }
                    if r.provides.iter().any(|e| !covered.contains(e)) {
                        keep.push(h.clone());
                        covered.extend(r.provides.iter().copied());
                    } else {
                        remove.push(h.clone());
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

/// Run the duplicate-dedup pass over the current owned set + provider listing. Computes a
/// [`plan_dedup`] and either logs it (dry-run) or executes removals. Destructive removal is gated on
/// `config.dedup_remove_duplicates` (default `false` = dry-run, so the plan can be reviewed first)
/// AND on the library being idle (`upgrade.idle_secs`), so a removal never interrupts an active
/// stream — a redundant torrent being read is left until the next idle pass. Execution: delete each
/// redundant torrent from the provider, drop its owned record, and clear any `selection` slot that
/// pointed at it (the VFS re-derives the slot from the kept covering hash on the next scan).
pub(crate) async fn dedup_owned(app: &AppState) {
    let torrents = match app.provider.get_torrents().await {
        Ok(t) => t,
        Err(e) => {
            warn!("dedup: get_torrents failed: {} — skipping this pass", e);
            return;
        }
    };
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

    let plans = plan_dedup(&owned, &present, &selected);
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
            "REMOVING"
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
            let _ = app.store.remove_owned(h.clone()).await;
            // Clear any selection slot that pointed at the removed hash (the VFS re-derives it from
            // the kept covering hash on the next scan).
            for (slot, entry) in &selection {
                if entry.hash.eq_ignore_ascii_case(h) {
                    let _ = app.store.remove_selection(slot.clone()).await;
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
/// `wanted::reconcile_title`. Shows in Task 8 handle ONLY Trigger-B removal (abandoned watchlist,
/// no air dates needed); show-episode acquisition and Trigger-A finish removal are Task 9.
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
            .map(|g| g.hashes.iter().any(|h| present.contains(h)))
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
                        // Delete EVERY owned hash for this tmdb_id (the Action's `hash` is a representative).
                        Action::Remove { tmdb_id, .. } => {
                            if let Some(g) = owned_group {
                                ops.push(ReconcileOp::Remove {
                                    tmdb_id,
                                    hashes: g.hashes.clone(),
                                });
                            }
                        }
                        Action::AcquireEpisode { .. } => {} // movies never produce this
                    }
                }
            }
            MediaType::Show => {
                // Task 8 handles show REMOVAL via Trigger B only (abandoned watchlist; no air dates).
                if let Some(g) = owned_group {
                    if !g.provenance.has_manual_entry()
                        && trigger_b_abandoned(&wanted, &g.provenance)
                    {
                        ops.push(ReconcileOp::Remove {
                            tmdb_id,
                            hashes: g.hashes.clone(),
                        });
                    }
                }
                // Task 9 handles show-episode acquire + Trigger-A finish removal (air-date dependent).
            }
        }
    }
    ops
}

/// Execute a `Remove`: delete each owned hash's torrent from the provider (matched
/// case-insensitively in `torrents`) and drop its `owned` record. Errors are logged, not fatal.
/// NO TMDB — unit-testable.
async fn execute_remove(
    provider: &Arc<dyn DebridProvider>,
    torrents: &[Torrent],
    store: &Store,
    tmdb_id: u64,
    hashes: &[String],
) {
    // NOTE: on a delete failure we skip remove_owned so the next reconcile tick retries — leaving
    // the owned record intact means the Remove op is re-derived and the torrent is retried instead
    // of being silently orphaned on the provider. MockProvider::delete_torrent always returns Ok,
    // so this path is exercised only in integration/production; no unit test added for the failure
    // branch — the code fix is self-evident.
    //
    // Read the selection table ONCE (we only ever remove from it below), not per-hash.
    let selection = store.all_selection().await;
    for hash in hashes {
        if let Some(t) = torrents.iter().find(|t| t.hash.eq_ignore_ascii_case(hash)) {
            if let Err(e) = provider.delete_torrent(&t.id).await {
                warn!(
                    "reconcile: delete_torrent {} (tmdb {}) failed: {}; will retry next tick",
                    t.id, tmdb_id, e
                );
                continue; // leave the owned record so the next reconcile retries
            }
        }
        if let Err(e) = store.remove_owned(hash.clone()).await {
            warn!("reconcile: remove_owned {} (tmdb {}) failed: {}; leaving selection intact for retry", hash, tmdb_id, e);
            continue;
        }
        // SP3: drop any selection slots this hash represented.
        for (slot, entry) in &selection {
            if entry.hash.eq_ignore_ascii_case(hash) {
                let _ = store.remove_selection(slot.clone()).await;
            }
        }
    }
}

/// Execute an `Acquire`: build the request from TMDB (preferring Trakt's `imdb_hint`), then drive
/// the SP1 engine, recording the supplied provenance. A title with no IMDB id anywhere is skipped
/// quietly (it can't be scraped). TMDB/engine-dependent — exercised by the live smoke.
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
) {
    let torrents = match provider.get_torrents().await {
        Ok(t) => t,
        Err(e) => {
            warn!("reconcile_wanted: get_torrents failed ({}); skipping this tick to avoid acting on a stale/empty listing", e);
            return;
        }
    };
    let ops = plan_reconcile_ops(store, &torrents).await;
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
                execute_remove(provider, &torrents, store, tmdb_id, &hashes).await
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

// ── monitor_episodes (SP2 Task 9) ────────────────────────────────────────────

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

/// When the aired set is INCOMPLETE (a TMDB lookup failed), drop any `Remove` action: a partial or
/// empty aired set must never let Trigger A vacuously fire and delete a still-wanted show on a
/// transient external blip ("a failed fetch must NOT cause removal"). Acquisition actions are
/// always kept — acquiring only the episodes we confirmed aired is safe even from a partial set.
pub(crate) fn guard_removal_on_incomplete_aired(
    actions: Vec<crate::wanted::Action>,
    aired_complete: bool,
) -> Vec<crate::wanted::Action> {
    if aired_complete {
        return actions;
    }
    actions
        .into_iter()
        .filter(|a| !matches!(a, crate::wanted::Action::Remove { .. }))
        .collect()
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
/// handling that Task 8 (`reconcile_wanted`) deferred.
///
/// OVERLAP: this re-derives `reconcile_wanted`'s show Trigger-B removal, which is harmless (both
/// idempotent); `monitor_episodes` ADDITIONALLY does episode acquisition + Trigger-A finish
/// removal (both air-date dependent, hence here and not in Task 8).
///
/// AVAILABILITY is title-level: a show's owned copy counts as `available` if ANY of its owned
/// hashes is present in the provider listing. A specific episode is therefore treated as available
/// whenever any of the show's hashes is present — acceptable for PROACTIVE acquisition (per-episode
/// unavailability is still caught at playback/repair).
pub async fn monitor_episodes(
    engine: &AcquisitionEngine,
    provider: &Arc<dyn DebridProvider>,
    tmdb: &TmdbClient,
    store: &Store,
) {
    use crate::wanted::{reconcile_title, Action, Owned, TitleView};
    use std::collections::HashSet;

    let today = chrono::Utc::now().date_naive();
    // TODO(Task 10): the scheduler could pass a shared torrents snapshot to avoid a
    // duplicate get_torrents() when monitor_episodes and reconcile_wanted run on the same tick.
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

        let view = TitleView {
            tmdb_id,
            media_type: MediaType::Show,
            wanted: wanted.clone(),
            owned: owned_group.map(|g| Owned {
                hash: g.hashes.first().cloned().unwrap_or_default(),
                provenance: g.provenance.clone(),
                available,
                owned_episodes: g.owned_episodes.clone(),
            }),
            aired_episodes: aired.pairs,
        };

        // Guard: a TMDB hiccup that left the aired set partial/empty must not delete a still-wanted
        // show (an empty `aired` makes Trigger A's "all aired watched" clause vacuously true).
        let actions = guard_removal_on_incomplete_aired(reconcile_title(&view), aired_complete);
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
                    // Preserve existing provenance (esp. Manual) on re-acquire — exactly like Task 8's
                    // AcquireMovie fix — so a manually-owned show keeps its never-auto-remove guard.
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
                // Delete EVERY owned hash for this tmdb_id (the Action's `hash` is a representative).
                Action::Remove { tmdb_id, .. } => {
                    if let Some(g) = owned_group {
                        execute_remove(provider, &torrents, store, tmdb_id, &g.hashes).await;
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
        let tmdb = TmdbClient::new("k".to_string()).unwrap();
        let info = crate::rd_client::TorrentInfo {
            hash: "HASH".into(),
            filename: "totally.unrelated.name.mkv".into(),
            ..Default::default()
        };
        let got = resolve_metadata(&store, &tmdb, &info).await;
        assert_eq!(got.title, "Authoritative");
        assert_eq!(got.external_id.as_deref(), Some("tmdb:99"));
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
        // Fake key → external_imdb_id fails → empty imdb (record still written, just not upgradeable).
        let tmdb = TmdbClient::new("k".into()).unwrap();

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

        record_mirror_owned(&store, &tmdb, &[movie, pack, eng]).await;

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
            quality: score.map(|s| crate::release::QualitySummary {
                cached: true,
                source_tier: 0,
                resolution: 1080,
                score: s,
            }),
        }
    }

    fn hset(items: &[&str]) -> std::collections::HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
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
        let plans = plan_dedup(&owned, &hset(&["a", "b", "c"]), &hset(&[]));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].keep, vec!["a"], "the highest-score hash is kept");
        let mut rm = plans[0].remove.clone();
        rm.sort();
        assert_eq!(rm, vec!["b", "c"]);
    }

    #[test]
    fn plan_dedup_movie_prefers_currently_served() {
        use crate::scraper::MediaKind;
        // `selected` outranks quality: keep the served (lower-score) hash to avoid disrupting playback.
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
        let plans = plan_dedup(&owned, &hset(&["served", "better"]), &hset(&["served"]));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].keep, vec!["served"]);
        assert_eq!(plans[0].remove, vec!["better"]);
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
        let plans = plan_dedup(&owned, &hset(&["pack", "single"]), &hset(&[]));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].keep, vec!["pack"]);
        assert_eq!(
            plans[0].remove,
            vec!["single"],
            "the single is fully covered by the pack → redundant"
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
        let plans = plan_dedup(&owned, &hset(&["a", "b", "unknown"]), &hset(&[]));
        assert!(
            plans.is_empty(),
            "complementary packs + an unknown-coverage hash yield no removals"
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
        let plans = plan_dedup(&owned, &hset(&["a"]), &hset(&[]));
        assert!(plans.is_empty());
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
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
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
    fn build_wanted_catchup_behind_gate_excludes_caught_up_show() {
        use crate::trakt_client::{WatchedData, WatchedShow};
        let watched = WatchedData {
            movies: vec![],
            shows: vec![WatchedShow {
                tmdb_id: 1396,
                trakt_id: Some(1390),
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
        let tmdb = TmdbClient::new("k".into()).unwrap();

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
                    watched_episodes: vec![(1, 1)],
                    last_watched_at: Some(1_000_000),
                },
                WatchedShow {
                    tmdb_id: 600,
                    trakt_id: Some(200),
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
        let tmdb = TmdbClient::new("k".into()).unwrap();

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
                expires_in: 7_776_000,
                created_at: 1_700_000_000,
            },
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData::default(),
            ..Default::default()
        });
        let tmdb = TmdbClient::new("k".into()).unwrap();

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
        let tmdb = TmdbClient::new("k".into()).unwrap();

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
        let tmdb = TmdbClient::new("k".into()).unwrap();

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
        let tmdb = TmdbClient::new("k".into()).unwrap();

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
        let tmdb = TmdbClient::new("k".into()).unwrap();

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
        let tmdb = TmdbClient::new("k".into()).unwrap();

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
    async fn plan_show_still_wanted_no_op() {
        // Task 8 defers show acquire + Trigger-A; a still-wanted owned show yields nothing.
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
        execute_remove(&provider, &torrents, &store, 27205, &["h1".to_string()]).await;
        assert!(store.get_owned("h1".into()).await.is_none());
        assert!(
            store.get_selection(movie_slot(27205)).await.is_none(),
            "removal must clear the selection slot"
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
        execute_remove(&provider, &torrents, &store, 27205, &["h1".to_string()]).await;
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
                tmdb: Arc::new(crate::tmdb_client::TmdbClient::new("k".into()).unwrap()),
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
        let tmdb = crate::tmdb_client::TmdbClient::new("k".into()).unwrap();

        reconcile_wanted(&engine, &provider, &tmdb, &store).await;

        assert!(
            deleted.lock().unwrap().is_empty(),
            "delete_torrent must not be called when get_torrents fails"
        );
        assert!(
            store.get_owned("aaa".to_string()).await.is_some(),
            "owned record must survive a skipped tick"
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

    // ── aired_pairs (pure — the chrono boundary heart of Task 9) ───────────────

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
        use crate::wanted::Action;
        let actions = vec![
            Action::Remove {
                tmdb_id: 7,
                hash: "abc".into(),
            },
            Action::AcquireEpisode {
                tmdb_id: 7,
                season: 1,
                episode: 2,
            },
        ];
        // complete == false (a TMDB lookup failed): the Remove must be dropped, the acquire kept.
        let kept = guard_removal_on_incomplete_aired(actions, false);
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
        use crate::wanted::Action;
        let actions = vec![Action::Remove {
            tmdb_id: 7,
            hash: "abc".into(),
        }];
        // complete == true: a genuine Trigger-A removal is preserved.
        let kept = guard_removal_on_incomplete_aired(actions.clone(), true);
        assert_eq!(kept, actions);
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
            guard_removal_on_incomplete_aired(actions.clone(), false),
            actions
        );
    }
}
