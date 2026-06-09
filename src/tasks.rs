use crate::acquire::AcquisitionEngine;
use crate::app_state::AppState;
use crate::error::AppError;
use crate::identification::identify_torrent;
use crate::provider::DebridProvider;
use crate::rd_client::Torrent;
use crate::repair::RepairManager;
use crate::scraper::MediaKind;
use crate::store::{AcquireRequest, Provenance, ProvenanceEntry, Store, WantedRecord};
use crate::tmdb_client::TmdbClient;
use crate::vfs::{DebridVfs, MediaMetadata, MediaType};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

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
pub(crate) fn build_wanted(
    user: &str,
    watchlist: &[crate::trakt_client::TraktItem],
    in_progress: &[crate::trakt_client::TraktItem],
    watched: &crate::trakt_client::WatchedData,
    show_status: &std::collections::HashMap<u64, crate::tmdb_client::ShowStatus>,
) -> Vec<crate::store::WantedRecord> {
    use crate::store::{WantedRecord, WantedSources, WatchedState};
    use crate::vfs::MediaType;

    /// Per-tmdb_id aggregation of which sources want a title.
    struct Agg {
        media_type: MediaType,
        watchlist: bool,
        in_progress: bool,
    }
    // BTreeMap keyed by tmdb_id keeps the output deterministically sorted.
    let mut agg: std::collections::BTreeMap<u64, Agg> = std::collections::BTreeMap::new();
    for item in watchlist {
        agg.entry(item.tmdb_id)
            .or_insert_with(|| Agg {
                media_type: item.media_type.clone(),
                watchlist: false,
                in_progress: false,
            })
            .watchlist = true;
    }
    for item in in_progress {
        agg.entry(item.tmdb_id)
            .or_insert_with(|| Agg {
                media_type: item.media_type.clone(),
                watchlist: false,
                in_progress: false,
            })
            .in_progress = true;
    }

    agg.into_iter()
        .map(|(tmdb_id, a)| {
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
            }
        })
        .collect()
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
) {
    for (slug, tokens) in store.all_trakt_tokens().await {
        if let Err(e) = sync_trakt_user(trakt, tmdb, store, &slug, tokens.clone()).await {
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

    // Per-show TMDB status. A TMDB failure must NOT fail the whole sync — skip that show
    // (→ build_wanted yields show_status: None, which the reconciler treats conservatively).
    let mut show_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for item in watchlist.iter().chain(in_progress.iter()) {
        if item.media_type == MediaType::Show {
            show_ids.insert(item.tmdb_id);
        }
    }
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

    // Build the user's new wanted-set and write it: prune rows no longer wanted, then upsert.
    let new = build_wanted(slug, &watchlist, &in_progress, &watched, &statuses);
    let existing_ids: Vec<u64> = store
        .all_wanted()
        .await
        .into_iter()
        .filter(|r| r.user == slug)
        .map(|r| r.tmdb_id)
        .collect();
    let new_ids: std::collections::HashSet<u64> = new.iter().map(|r| r.tmdb_id).collect();
    for id in existing_ids {
        if !new_ids.contains(&id) {
            store.remove_wanted(slug.to_string(), id).await?;
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
    },
    Remove {
        tmdb_id: u64,
        hashes: Vec<String>,
    },
}

/// Build the `AcquireRequest` for `tmdb_id`: resolve the IMDB id and (title, year,
/// original_language) from TMDB. Any TMDB failure → `Err` (the caller logs and skips).
/// TMDB-dependent, so it is exercised by the live smoke rather than unit tests.
async fn build_acquire_request(
    tmdb: &TmdbClient,
    tmdb_id: u64,
    kind: MediaKind,
    season: Option<u32>,
    episode: Option<u32>,
) -> Result<AcquireRequest, AppError> {
    let media_type = media_type_of(kind);
    let imdb_id = tmdb
        .external_imdb_id(tmdb_id, media_type.clone())
        .await?
        .ok_or_else(|| AppError::Config(format!("no IMDB id for tmdb {}", tmdb_id)))?;
    let (title, year, original_language) = tmdb.details(tmdb_id, media_type.clone()).await?;
    Ok(AcquireRequest {
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
    })
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
) -> std::collections::BTreeMap<u64, OwnedGroup> {
    use std::collections::BTreeMap;
    let mut owned_by: BTreeMap<u64, OwnedGroup> = BTreeMap::new();
    for (hash, rec) in store.all_owned().await {
        let group = owned_by
            .entry(rec.request.tmdb_id)
            .or_insert_with(|| OwnedGroup {
                hashes: Vec::new(),
                provenance: Provenance {
                    entries: Vec::new(),
                },
                owned_episodes: Vec::new(),
                media_type: media_type_of(rec.request.kind),
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

/// Group every wanted record by its `tmdb_id` into a `BTreeMap`. Shared by
/// `plan_reconcile_ops` and `monitor_episodes` so the wanted-grouping lives in one place.
pub(crate) async fn group_wanted_by_tmdb(
    store: &Store,
) -> std::collections::BTreeMap<u64, Vec<WantedRecord>> {
    use std::collections::BTreeMap;
    let mut wanted_by: BTreeMap<u64, Vec<WantedRecord>> = BTreeMap::new();
    for r in store.all_wanted().await {
        wanted_by.entry(r.tmdb_id).or_default().push(r);
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

    let mut ids: BTreeSet<u64> = BTreeSet::new();
    ids.extend(wanted_by.keys().copied());
    ids.extend(owned_by.keys().copied());

    let mut ops = Vec::new();
    for tmdb_id in ids {
        let wanted = wanted_by.get(&tmdb_id).cloned().unwrap_or_default();
        let owned_group = owned_by.get(&tmdb_id);

        // Prefer the wanted rows' media_type; else fall back to the owned group's.
        let wanted_type = wanted.first().map(|r| r.media_type.clone());
        if let (Some(wt), Some(og)) = (&wanted_type, owned_group) {
            if *wt != og.media_type {
                warn!(
                    "reconcile: tmdb {} media_type skew: wanted={:?} owned={:?}",
                    tmdb_id, wt, og.media_type
                );
            }
        }
        let Some(media_type) = wanted_type.or_else(|| owned_group.map(|g| g.media_type.clone()))
        else {
            continue;
        };

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
        for (slot, entry) in store.all_selection().await {
            if entry.hash.eq_ignore_ascii_case(hash) {
                let _ = store.remove_selection(slot).await;
            }
        }
    }
}

/// Execute an `Acquire`: build the request from TMDB, then drive the SP1 engine, recording the
/// supplied provenance. TMDB/engine-dependent — exercised by the live smoke.
async fn execute_acquire(
    engine: &AcquisitionEngine,
    tmdb: &TmdbClient,
    tmdb_id: u64,
    kind: MediaKind,
    season: Option<u32>,
    episode: Option<u32>,
    provenance: Provenance,
) {
    match build_acquire_request(tmdb, tmdb_id, kind, season, episode).await {
        Ok(req) => {
            let outcome = engine.acquire(req, provenance).await;
            info!("reconcile: acquire tmdb {} -> {:?}", tmdb_id, outcome);
        }
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
            } => execute_acquire(engine, tmdb, tmdb_id, kind, season, episode, provenance).await,
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

/// All aired `(season, episode)` pairs for a show as of `today`: enumerate the show's
/// (non-Specials) seasons, fetch each season's episode air dates, and collect `aired_pairs`
/// across them. Best-effort I/O — a failure to enumerate seasons, or to fetch ONE season's air
/// dates, is logged and skipped rather than failing the whole show. TMDB-driven, so exercised by
/// the live smoke rather than unit tests.
pub(crate) async fn aired_episodes(
    tmdb: &TmdbClient,
    tmdb_id: u64,
    today: chrono::NaiveDate,
) -> Vec<(u32, u32)> {
    let seasons = match tmdb.show_season_numbers(tmdb_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "monitor_episodes: show_season_numbers({}) failed: {}; skipping show",
                tmdb_id, e
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for season in seasons {
        match tmdb.season_air_dates(tmdb_id, season).await {
            Ok(eps) => out.extend(aired_pairs(&eps, today)),
            Err(e) => warn!(
                "monitor_episodes: season_air_dates({}, {}) failed: {}; skipping season",
                tmdb_id, season, e
            ),
        }
    }
    out.sort_unstable();
    out.dedup();
    out
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

    // Group wanted rows by tmdb_id.
    let wanted_by = group_wanted_by_tmdb(store).await;

    // Same owned-grouping + availability logic as plan_reconcile_ops (shared helper).
    let owned_by = group_owned_by_tmdb(store).await;
    let present: HashSet<String> = torrents
        .iter()
        .map(|t| t.hash.to_ascii_lowercase())
        .collect();

    // Only tmdb_ids that a wanted row marks as a Show. (Owned-but-unwanted shows are handled by
    // reconcile_wanted's Trigger-B path; monitor_episodes is the wanted-set's air-date driver.)
    for (&tmdb_id, wanted) in &wanted_by {
        if !wanted.iter().any(|r| r.media_type == MediaType::Show) {
            continue;
        }
        let owned_group = owned_by.get(&tmdb_id);
        let available = owned_group
            .map(|g| g.hashes.iter().any(|h| present.contains(h)))
            .unwrap_or(false);

        let aired = aired_episodes(tmdb, tmdb_id, today).await;

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
            aired_episodes: aired,
        };

        for action in reconcile_title(&view) {
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
        let g = groups.get(&1396).unwrap();
        let mut eps = g.owned_episodes.clone();
        eps.sort_unstable();
        assert_eq!(
            eps,
            vec![(1, 1), (1, 2), (1, 3)],
            "owned_episodes is the union of provides, not the request's single (s,e)"
        );
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
            }]
        );
    }

    #[test]
    fn build_wanted_watchlist_show_with_status_and_watched_episodes() {
        let watched = WatchedData {
            movies: vec![],
            shows: vec![WatchedShow {
                tmdb_id: 1396,
                watched_episodes: vec![(1, 1), (1, 2)],
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
            }]
        );
    }

    #[test]
    fn build_wanted_title_in_both_sources_sets_both_flags() {
        let got = build_wanted(
            "alice",
            &[item(MediaType::Movie, 100)],
            &[item(MediaType::Movie, 100)],
            &WatchedData::default(),
            &HashMap::new(),
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
            }]
        );
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
        );
        assert_eq!(got[0].watched_state, WatchedState::Movie { watched: true });
    }

    #[test]
    fn build_wanted_is_sorted_by_tmdb_id() {
        let wl = vec![
            item(MediaType::Movie, 30),
            item(MediaType::Movie, 10),
            item(MediaType::Movie, 20),
        ];
        let got = build_wanted("alice", &wl, &[], &WatchedData::default(), &HashMap::new());
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

        sync_trakt(&trakt, &tmdb, &store).await;

        let w = store
            .get_wanted("alice".to_string(), 27205)
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

        sync_trakt(&trakt, &tmdb, &store).await;

        let tok = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert_eq!(
            tok.access, "REFRESHED",
            "refresh must have run and persisted"
        );
        assert!(store.get_wanted("alice".to_string(), 27205).await.is_some());
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

        sync_trakt(&trakt, &tmdb, &store).await;

        assert_eq!(
            store.get_wanted("alice".to_string(), 999).await,
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

        sync_trakt(&trakt, &tmdb, &store).await;

        assert!(
            store.get_wanted("alice".to_string(), 27205).await.is_some(),
            "new title present"
        );
        assert!(
            store.get_wanted("alice".to_string(), 999).await.is_none(),
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

        sync_trakt(&trakt, &tmdb, &store).await;

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

        sync_trakt(&trakt, &tmdb, &store).await;

        // alice: refresh failed → flagged, no wanted rows written
        let alice_tok = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert!(
            alice_tok.needs_reenrolment,
            "alice must be flagged after refresh failure"
        );
        assert!(
            store.get_wanted("alice".to_string(), 27205).await.is_none(),
            "alice's wanted must be empty — error occurred before any read"
        );

        // bob: sync succeeded → wanted row present, NOT flagged
        let bob_tok = store.get_trakt_tokens("bob".to_string()).await.unwrap();
        assert!(!bob_tok.needs_reenrolment, "bob must NOT be flagged");
        assert!(
            store.get_wanted("bob".to_string(), 27205).await.is_some(),
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

        sync_trakt(&trakt, &tmdb, &store).await;

        let tok = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert!(tok.needs_reenrolment, "account must be flagged");
        assert!(!tok.refresh.is_empty(), "refresh token must not be blanked");
        // No wanted rows: the error occurred before any Trakt read
        assert!(
            store.get_wanted("alice".to_string(), 27205).await.is_none(),
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
            }]
        );
    }

    #[tokio::test]
    async fn plan_finished_owned_movie_removes_trigger_a() {
        let store = mem_store();
        // all wanters watched + owned hash present → Trigger A.
        store
            .put_wanted(wanted_movie("alice", 27205, true, false, true))
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
        assert_eq!(
            ops,
            vec![ReconcileOp::Remove {
                tmdb_id: 27205,
                hashes: vec!["abc".into()]
            }]
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
        // alice has watched the movie and it's on her watchlist (Trigger A conditions met)
        store
            .put_wanted(wanted_movie("alice", 27205, true, false, true))
            .await
            .unwrap();
        store
            .put_owned(
                "abc".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
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
        // finished movie owned under TWO hashes → one Remove op listing BOTH.
        store
            .put_wanted(wanted_movie("alice", 27205, true, false, true))
            .await
            .unwrap();
        store
            .put_owned(
                "aaa".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
            )
            .await
            .unwrap();
        store
            .put_owned(
                "bbb".into(),
                owned_record(27205, MediaKind::Movie, Provenance::watchlist("alice")),
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
}
