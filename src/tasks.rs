use crate::app_state::AppState;
use crate::identification::identify_torrent;
use crate::repair::RepairManager;
use crate::store::Store;
use crate::tmdb_client::TmdbClient;
use crate::vfs::{DebridVfs, MediaMetadata};
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
    match store.authoritative_meta(info.hash.to_ascii_lowercase()).await {
        Some(m) => m,
        None => identify_torrent(info, tmdb_client).await,
    }
}

pub struct ScanConfig {
    pub app: AppState,
}

pub async fn run_scan_loop(scan_config: ScanConfig, mut shutdown: tokio::sync::watch::Receiver<bool>) {
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
                                        let metadata = resolve_metadata(&store, &tmdb_client, &info).await;
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
                            update_vfs(&vfs, &current_data, &repair_manager, &jellyfin_client).await;
                            return;
                        }
                    } {
                        processed_new += 1;
                        match result {
                            Ok((id, info, metadata)) => {
                                pending_db_writes.push((id.clone(), info.clone(), metadata.clone()));
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
            .or_insert_with(|| Agg { media_type: item.media_type.clone(), watchlist: false, in_progress: false })
            .watchlist = true;
    }
    for item in in_progress {
        agg.entry(item.tmdb_id)
            .or_insert_with(|| Agg { media_type: item.media_type.clone(), watchlist: false, in_progress: false })
            .in_progress = true;
    }

    agg.into_iter()
        .map(|(tmdb_id, a)| {
            let (watched_state, status) = match a.media_type {
                MediaType::Movie => (WatchedState::Movie { watched: watched.movies.contains(&tmdb_id) }, None),
                MediaType::Show => {
                    let watched_episodes = watched
                        .shows
                        .iter()
                        .find(|s| s.tmdb_id == tmdb_id)
                        .map(|s| s.watched_episodes.clone())
                        .unwrap_or_default();
                    (WatchedState::Show { watched_episodes }, show_status.get(&tmdb_id).copied())
                }
            };
            WantedRecord {
                user: user.to_string(),
                tmdb_id,
                media_type: a.media_type,
                sources: WantedSources { watchlist: a.watchlist, in_progress: a.in_progress },
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
            warn!("Trakt sync failed for {}: {}; flagging account for re-enrolment", slug, e);
            // A successful refresh inside sync_trakt_user persists a fresh (single-use) token before a
            // later read can fail; re-read so we don't clobber it with the stale pre-refresh snapshot.
            let current = store.get_trakt_tokens(slug.clone()).await.unwrap_or(tokens);
            let flagged = crate::store::TraktTokens { needs_reenrolment: true, ..current };
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
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if tokens.expires_at <= now + REFRESH_BUFFER_SECS {
        let r = trakt.refresh(&tokens.refresh).await?;
        tokens = TraktTokens {
            access: r.access_token,
            refresh: r.refresh_token,
            expires_at: r.created_at + r.expires_in,
            username: tokens.username.clone(),
            needs_reenrolment: false,
        };
        store.put_trakt_tokens(slug.to_string(), tokens.clone()).await?;
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
            Err(e) => warn!("TMDB show_status({}) failed for {}: {}; treating status as unknown", tmdb_id, slug, e),
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
            .put_trakt_tokens(slug.to_string(), TraktTokens { needs_reenrolment: false, ..tokens })
            .await?;
    }

    Ok(())
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
            redb::Database::builder().create_with_backend(redb::backends::InMemoryBackend::new()).unwrap(),
        )).unwrap();
        let meta = MediaMetadata { title: "Authoritative".into(), year: Some("2020".into()), media_type: MediaType::Movie, external_id: Some("tmdb:99".into()) };
        store.put_authoritative("hash".to_string(), meta.clone()).await.unwrap();
        let tmdb = TmdbClient::new("k".to_string()).unwrap();
        let info = crate::rd_client::TorrentInfo { hash: "HASH".into(), filename: "totally.unrelated.name.mkv".into(), ..Default::default() };
        let got = resolve_metadata(&store, &tmdb, &info).await;
        assert_eq!(got.title, "Authoritative");
        assert_eq!(got.external_id.as_deref(), Some("tmdb:99"));
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
        let scraper: std::sync::Arc<dyn crate::scraper::Scraper> = std::sync::Arc::new(
            crate::scraper::TorrentioScraper::new(None, crate::provider::ProviderKind::TorBox, "tok", reqwest::Client::new()),
        );
        let validator: std::sync::Arc<dyn crate::acquire::TitleValidator> = std::sync::Arc::new(
            crate::acquire::TmdbTitleValidator { tmdb: std::sync::Arc::new(TmdbClient::new("k".to_string()).unwrap()) },
        );
        let prober: std::sync::Arc<dyn crate::acquire::Prober> = std::sync::Arc::new(
            crate::acquire::HttpProber { http: reqwest::Client::new() },
        );
        let engine = std::sync::Arc::new(crate::acquire::AcquisitionEngine::new(
            provider.clone(), scraper.clone(), validator, prober, store.clone(),
            crate::config::AcquisitionConfig::default().prefs, 5, std::time::Duration::from_secs(1800),
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
        };
        let _config = ScanConfig { app };
    }
}

#[cfg(test)]
mod trakt_sync_tests {
    use super::*;
    use crate::store::{Store, TraktTokens, WantedRecord, WantedSources, WatchedState};
    use crate::tmdb_client::{ShowStatus, TmdbClient};
    use crate::trakt_client::{MockTrakt, TraktClient, TraktItem, TraktTokenResponse, WatchedData, WatchedShow};
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
        TraktItem { media_type, tmdb_id }
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
            sources: WantedSources { watchlist: true, in_progress: false },
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
                sources: WantedSources { watchlist: true, in_progress: false },
                watched_state: WatchedState::Movie { watched: false },
                show_status: None,
            }]
        );
    }

    #[test]
    fn build_wanted_watchlist_show_with_status_and_watched_episodes() {
        let watched = WatchedData {
            movies: vec![],
            shows: vec![WatchedShow { tmdb_id: 1396, watched_episodes: vec![(1, 1), (1, 2)] }],
        };
        let mut status = HashMap::new();
        status.insert(1396u64, ShowStatus::Ended);
        let got = build_wanted("alice", &[item(MediaType::Show, 1396)], &[], &watched, &status);
        assert_eq!(
            got,
            vec![WantedRecord {
                user: "alice".to_string(),
                tmdb_id: 1396,
                media_type: MediaType::Show,
                sources: WantedSources { watchlist: true, in_progress: false },
                watched_state: WatchedState::Show { watched_episodes: vec![(1, 1), (1, 2)] },
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
        assert_eq!(got[0].sources, WantedSources { watchlist: true, in_progress: true });
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
                sources: WantedSources { watchlist: false, in_progress: true },
                watched_state: WatchedState::Show { watched_episodes: vec![] },
                show_status: None,
            }]
        );
    }

    #[test]
    fn build_wanted_watched_movie_marks_watched() {
        let watched = WatchedData { movies: vec![27205], shows: vec![] };
        let got = build_wanted("alice", &[item(MediaType::Movie, 27205)], &[], &watched, &HashMap::new());
        assert_eq!(got[0].watched_state, WatchedState::Movie { watched: true });
    }

    #[test]
    fn build_wanted_is_sorted_by_tmdb_id() {
        let wl = vec![item(MediaType::Movie, 30), item(MediaType::Movie, 10), item(MediaType::Movie, 20)];
        let got = build_wanted("alice", &wl, &[], &WatchedData::default(), &HashMap::new());
        assert_eq!(got.iter().map(|r| r.tmdb_id).collect::<Vec<_>>(), vec![10, 20, 30]);
    }

    // ── sync_trakt (async, MockTrakt + mem Store) ─────────────────────────────

    #[tokio::test]
    async fn sync_trakt_success_populates_wanted_and_leaves_flag_false() {
        let store = mem_store();
        store.put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, false)).await.unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData { movies: vec![], shows: vec![] },
            ..Default::default()
        });
        let tmdb = TmdbClient::new("k".into()).unwrap();

        sync_trakt(&trakt, &tmdb, &store).await;

        let w = store.get_wanted("alice".to_string(), 27205).await.expect("wanted present");
        assert!(w.sources.watchlist);
        assert!(!store.get_trakt_tokens("alice".to_string()).await.unwrap().needs_reenrolment);
    }

    #[tokio::test]
    async fn sync_trakt_refreshes_near_expiry_token() {
        let store = mem_store();
        store.put_trakt_tokens("alice".to_string(), tokens("old", 0, false)).await.unwrap();
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
        assert_eq!(tok.access, "REFRESHED", "refresh must have run and persisted");
        assert!(store.get_wanted("alice".to_string(), 27205).await.is_some());
    }

    #[tokio::test]
    async fn sync_trakt_fetch_error_leaves_wanted_and_flags_account() {
        let store = mem_store();
        store.put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, false)).await.unwrap();
        let preexisting = movie_wanted_row("alice", 999);
        store.put_wanted(preexisting.clone()).await.unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt { fail_reads: true, ..Default::default() });
        let tmdb = TmdbClient::new("k".into()).unwrap();

        sync_trakt(&trakt, &tmdb, &store).await;

        assert_eq!(
            store.get_wanted("alice".to_string(), 999).await,
            Some(preexisting),
            "a fetch failure must leave existing wanted rows untouched"
        );
        assert!(store.get_trakt_tokens("alice".to_string()).await.unwrap().needs_reenrolment);
    }

    #[tokio::test]
    async fn sync_trakt_prunes_stale_wanted() {
        let store = mem_store();
        store.put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, false)).await.unwrap();
        store.put_wanted(movie_wanted_row("alice", 999)).await.unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData::default(),
            ..Default::default()
        });
        let tmdb = TmdbClient::new("k".into()).unwrap();

        sync_trakt(&trakt, &tmdb, &store).await;

        assert!(store.get_wanted("alice".to_string(), 27205).await.is_some(), "new title present");
        assert!(store.get_wanted("alice".to_string(), 999).await.is_none(), "stale title pruned");
    }

    #[tokio::test]
    async fn sync_trakt_clears_preexisting_flag_on_success() {
        let store = mem_store();
        store.put_trakt_tokens("alice".to_string(), tokens("acc", 9_999_999_999, true)).await.unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            watchlist: vec![item(MediaType::Movie, 27205)],
            watched: WatchedData::default(),
            ..Default::default()
        });
        let tmdb = TmdbClient::new("k".into()).unwrap();

        sync_trakt(&trakt, &tmdb, &store).await;

        assert!(
            !store.get_trakt_tokens("alice".to_string()).await.unwrap().needs_reenrolment,
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
        store.put_trakt_tokens("alice".to_string(), TraktTokens {
            access: "alice-acc".to_string(),
            refresh: "alice-ref".to_string(),
            expires_at: 0,
            username: "alice".to_string(),
            needs_reenrolment: false,
        }).await.unwrap();
        // bob: fresh token — no refresh needed, reads will succeed
        store.put_trakt_tokens("bob".to_string(), TraktTokens {
            access: "bob-acc".to_string(),
            refresh: "bob-ref".to_string(),
            expires_at: 9_999_999_999,
            username: "bob".to_string(),
            needs_reenrolment: false,
        }).await.unwrap();

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
        assert!(alice_tok.needs_reenrolment, "alice must be flagged after refresh failure");
        assert!(store.get_wanted("alice".to_string(), 27205).await.is_none(),
            "alice's wanted must be empty — error occurred before any read");

        // bob: sync succeeded → wanted row present, NOT flagged
        let bob_tok = store.get_trakt_tokens("bob".to_string()).await.unwrap();
        assert!(!bob_tok.needs_reenrolment, "bob must NOT be flagged");
        assert!(store.get_wanted("bob".to_string(), 27205).await.is_some(),
            "bob's wanted must be populated");
    }

    /// When refresh fails for an expired token, the account is flagged but the stored refresh
    /// token is preserved (we never blank it) and no wanted rows are written.
    #[tokio::test]
    async fn sync_trakt_fail_refresh_flags_account_and_preserves_refresh_token() {
        let store = mem_store();
        store.put_trakt_tokens("alice".to_string(), TraktTokens {
            access: "acc".to_string(),
            refresh: "original-ref".to_string(),
            expires_at: 0,
            username: "alice".to_string(),
            needs_reenrolment: false,
        }).await.unwrap();
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
        assert!(store.get_wanted("alice".to_string(), 27205).await.is_none(),
            "no wanted rows must have been written");
    }
}
