//! SP3 upgrade engine. A slow periodic job (gated on `UPGRADE_INTERVAL_SECS`, default daily) that
//! re-scores owned titles and stages meaningfully-better CACHED releases — and full-season cached
//! packs (Task 10) — swapping the persisted `selection` and pruning the superseded torrent only
//! once the slot is idle (proxy read-activity). Upgrades are non-destructive: a failed stage never
//! degrades the working release.

use crate::app_state::AppState;
use crate::release::{self, QualitySummary};
use crate::scraper::MediaKind;
use crate::store::{movie_slot, OwnedRecord, OwnedStatus};
use crate::vfs::MediaType;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Run one upgrade tick over `app`: re-score a budgeted batch of owned MOVIE titles, stage any
/// cached meaningful upgrade, and — if the library is idle — swap selection + prune the old torrent.
pub async fn run_upgrade_once(app: &AppState) {
    let budget = app.config.upgrade.budget_per_tick as usize;
    let idle_window = Duration::from_secs(app.config.upgrade.idle_secs);

    // Group owned by tmdb_id (reuse the tasks helper) and pick the least-recently-checked movies.
    let groups = crate::tasks::group_owned_by_tmdb(&app.store).await;
    let mut candidates: Vec<(u64, Vec<String>, OwnedRecord)> = Vec::new();
    for (tmdb_id, g) in &groups {
        if g.media_type != MediaType::Movie {
            continue; // Task 10 handles shows/consolidation
        }
        // Representative owned record (movies have one hash).
        let Some(hash) = g.hashes.first().cloned() else { continue };
        let Some(rec) = app.store.get_owned(hash.clone()).await else { continue };
        if rec.status != OwnedStatus::Verified {
            continue; // only upgrade settled titles
        }
        candidates.push((*tmdb_id, g.hashes.clone(), rec));
    }
    // Least-recently-checked first.
    let mut ordered: Vec<_> = Vec::new();
    for (id, hashes, rec) in candidates {
        let last = app.store.get_upgrade_checked(id).await;
        ordered.push((last, id, hashes, rec));
    }
    ordered.sort_by_key(|(last, ..)| *last);
    ordered.truncate(budget);

    for (_, tmdb_id, hashes, rec) in ordered {
        app.store.set_upgrade_checked(tmdb_id, now_secs()).await.ok();
        if let Err(e) = try_upgrade_movie(app, tmdb_id, &hashes, &rec, idle_window).await {
            warn!("upgrade: tmdb {} skipped: {}", tmdb_id, e);
        }
    }
}

/// Stage + (idle-gated) swap a single movie title. Returns Err(reason) on a non-fatal skip.
async fn try_upgrade_movie(
    app: &AppState,
    tmdb_id: u64,
    owned_hashes: &[String],
    owned_rec: &OwnedRecord,
    idle_window: Duration,
) -> Result<(), String> {
    // 1. Scrape fresh candidates for this title.
    let raws = app.scraper
        .find(&owned_rec.request.imdb_id, MediaKind::Movie, None, None)
        .await
        .map_err(|e| format!("scrape failed: {e}"))?;
    let current = owned_rec.quality.clone().unwrap_or_default();
    // 2. Best cached meaningful upgrade not already owned/blacklisted.
    let mut best: Option<(release::ReleaseInfo, QualitySummary)> = None;
    for raw in &raws {
        let r = release::parse(raw);
        if owned_hashes.iter().any(|h| h.eq_ignore_ascii_case(&r.info_hash)) { continue; }
        if app.store.is_blacklisted(tmdb_id, r.info_hash.clone()).await { continue; }
        let q = QualitySummary::of(&r, &app.config.acquisition.prefs);
        if !release::is_meaningful_upgrade(&current, &q) { continue; }
        if best.as_ref().map(|(_, bq)| q.score > bq.score).unwrap_or(true) {
            best = Some((r, q));
        }
    }
    let Some((cand, _q)) = best else { return Err("no meaningful upgrade".into()) };

    // 3. Idle gate FIRST. Upgrade targets are cached-only (instant to add), so there is no benefit
    //    to pre-staging a download — we only commit when the library is idle, and skip otherwise.
    //    This guarantees we never hold two copies of a title (no dangling stage), which is why
    //    `UPGRADE_STAGE_MAX_SECS` is config-only/reserved on this cached path (kept for forward-compat
    //    with a future speculative-download upgrade mode; not consulted here).
    if !app.read_activity.all_idle(idle_window).await {
        return Err("library active; deferring upgrade".into());
    }

    // 4. Stage the cached candidate: add + validate + record Verified (non-destructive — any failure
    //    leaves the current release untouched). Returns (hash, torrent_id, selected_file_path).
    let staged = stage_and_verify(app, tmdb_id, &owned_rec.request, &cand).await?;

    // 5. Swap selection → new hash, then prune every old owned hash.
    app.store.put_selection(
        movie_slot(tmdb_id),
        crate::store::SelectionEntry { hash: staged.0.clone(), file_path: staged.2.clone() },
    ).await.ok();
    for old in owned_hashes {
        if old.eq_ignore_ascii_case(&staged.0) { continue; }
        prune_owned_hash(app, old).await;
    }
    info!("upgrade: tmdb {} swapped to {}", tmdb_id, staged.0);
    Ok(())
}

/// Add the candidate, wait briefly for it to resolve (it should be cached), validate + record it
/// Verified with sticky provenance, and return (hash, torrent_id, selected_file_path). On any
/// failure the candidate is cleaned up and the current release is left untouched (non-destructive).
async fn stage_and_verify(
    app: &AppState,
    tmdb_id: u64,
    base_req: &crate::store::AcquireRequest,
    cand: &release::ReleaseInfo,
) -> Result<(String, String, String), String> {
    let hash = cand.info_hash.clone();
    let magnet = format!("magnet:?xt=urn:btih:{}", hash);
    let added = app.provider.add_magnet(&magnet).await.map_err(|e| format!("add failed: {e}"))?;
    let info = app.provider.get_torrent_info(&added.id).await.map_err(|e| format!("info failed: {e}"))?;
    // Must be cached/downloaded with a video file to stage (we never speculatively download upgrades).
    if info.status != "downloaded" {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err("candidate not cached".into());
    }
    // Select the single feature file.
    let Some(file) = info.files.iter().filter(|f| crate::vfs::is_video_file(&f.path)).max_by_key(|f| f.bytes) else {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err("no video file".into());
    };
    let csv = file.id.to_string();
    let _ = app.provider.select_files(&added.id, &csv).await;
    let selected_path = file.path.clone();
    let file_name = selected_path.rsplit('/').next().unwrap_or(&selected_path).to_string();
    // Title validation (the engine exposes `validate_title` — see below).
    if !app.engine.validate_title(&file_name, tmdb_id, MediaKind::Movie, None, None).await {
        let _ = app.provider.delete_torrent(&added.id).await;
        let _ = app.store.blacklist_add(tmdb_id, hash.clone(), "WrongTitle", now_secs()).await;
        return Err("title mismatch".into());
    }
    // Record Verified with sticky provenance from the owned record we are upgrading.
    let prov = base_req_provenance(app, tmdb_id).await;
    let _ = app.store.put_owned(hash.clone(), OwnedRecord {
        request: base_req.clone(),
        provenance: prov,
        added_at: now_secs(),
        status: OwnedStatus::Verified,
        provides: vec![],
        quality: Some(QualitySummary::of(cand, &app.config.acquisition.prefs)),
    }).await;
    let _ = app.store.put_authoritative(hash.clone(), base_req.metadata.clone()).await;
    Ok((hash, added.id, selected_path))
}

/// Delete a torrent (owned-only) and drop its owned record + authoritative id.
async fn prune_owned_hash(app: &AppState, hash: &str) {
    if let Ok(torrents) = app.provider.get_torrents().await {
        for t in torrents.iter().filter(|t| t.hash.eq_ignore_ascii_case(hash)) {
            let _ = app.provider.delete_torrent(&t.id).await;
        }
    }
    let _ = app.store.remove_owned(hash.to_string()).await;
    let _ = app.store.remove_authoritative(hash.to_string()).await;
}

/// The provenance to keep on a staged upgrade: the merged provenance of the title's current owned
/// hashes (sticky — preserves Manual / per-user origins across the swap).
async fn base_req_provenance(app: &AppState, tmdb_id: u64) -> crate::store::Provenance {
    let groups = crate::tasks::group_owned_by_tmdb(&app.store).await;
    groups.get(&tmdb_id).map(|g| g.provenance.clone()).unwrap_or_else(crate::store::Provenance::manual)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::config::{AcquisitionConfig, Config};
    use crate::provider::{DebridProvider, MockProvider};
    use crate::rd_client::{AddMagnetResponse, Torrent, TorrentFile, TorrentInfo};
    use crate::repair::RepairManager;
    use crate::scraper::{MockScraper, Scraper};
    use crate::store::{AcquireRequest, Provenance, SelectionEntry, Store};
    use crate::release::RawCandidate;
    use crate::tmdb_client::TmdbClient;
    use crate::vfs::{DebridVfs, MediaMetadata, MediaType};
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn mem_store() -> Store {
        Store::from_database(Arc::new(redb::Database::builder().create_with_backend(redb::backends::InMemoryBackend::new()).unwrap())).unwrap()
    }

    fn movie_meta() -> MediaMetadata {
        MediaMetadata { title: "M".into(), year: Some("2020".into()), media_type: MediaType::Movie, external_id: Some("tmdb:27205".into()) }
    }
    fn movie_req() -> AcquireRequest {
        AcquireRequest { imdb_id: "tt1".into(), tmdb_id: 27205, kind: MediaKind::Movie, season: None, episode: None, original_language: Some("eng".into()), metadata: movie_meta() }
    }
    /// A cached REMUX 1080p candidate (a meaningful upgrade over a cached WEB 1080p).
    fn remux_candidate() -> RawCandidate {
        RawCandidate { name: "Torrentio\n1080p".into(), description: "M.2020.1080p.BluRay.REMUX.x265\nRD+".into(), info_hash: "hnew".into(), file_idx: Some(0), file_name: Some("M.2020.1080p.REMUX.mkv".into()) }
    }

    /// Deterministic title validator for the upgrade flow tests — title validation has its own
    /// coverage in `acquire.rs`; here we want the upgrade staging/swap/prune logic under test,
    /// so we never hit the network-backed `TmdbTitleValidator`.
    struct PassValidator;
    #[async_trait]
    impl crate::acquire::TitleValidator for PassValidator {
        async fn validate(&self, _f: &str, _t: u64, _k: MediaKind, _s: Option<u32>, _e: Option<u32>) -> bool {
            true
        }
    }

    fn app_with(scraper: Arc<dyn Scraper>, provider: Arc<dyn DebridProvider>, store: Store) -> AppState {
        let mut config = Config::from_parts(None, Some("tb".into()), Some("k".into()), None, None, None).unwrap();
        config.acquisition = AcquisitionConfig::default();
        let tmdb = Arc::new(TmdbClient::new("k".into()).unwrap());
        let validator: Arc<dyn crate::acquire::TitleValidator> = Arc::new(PassValidator);
        let prober: Arc<dyn crate::acquire::Prober> = Arc::new(crate::acquire::HttpProber { http: reqwest::Client::new() });
        let engine = Arc::new(crate::acquire::AcquisitionEngine::new(
            provider.clone(), scraper.clone(), validator, prober, store.clone(),
            config.acquisition.prefs.clone(), 5, Duration::from_secs(1800), Duration::from_secs(600),
        ));
        AppState {
            provider: provider.clone(),
            tmdb_client: tmdb,
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
        }
    }

    #[tokio::test]
    async fn idle_movie_with_cached_better_release_is_staged_swapped_and_old_pruned() {
        let store = mem_store();
        // Owned: cached WEB 1080p movie, Verified, with a movie selection pointing at it.
        store.put_owned("hold".into(), OwnedRecord {
            request: movie_req(), provenance: Provenance::watchlist("a"), added_at: 1, status: OwnedStatus::Verified,
            provides: vec![],
            quality: Some(QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 10 }),
        }).await.unwrap();
        store.put_selection(movie_slot(27205), SelectionEntry { hash: "hold".into(), file_path: "old.mkv".into() }).await.unwrap();

        // Scraper offers a cached REMUX (higher tier → meaningful upgrade).
        let scraper = Arc::new(MockScraper { candidates: vec![remux_candidate()] });
        // Provider: both torrents listed; the new one resolves cached/downloaded with a video file.
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![
                Torrent { id: "told".into(), hash: "hold".into(), status: "downloaded".into(), ..Default::default() },
                Torrent { id: "tnew".into(), hash: "hnew".into(), status: "downloaded".into(), ..Default::default() },
            ],
            add_magnet: Some(AddMagnetResponse { id: "tnew".into(), uri: String::new() }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(), hash: "hnew".into(), status: "downloaded".into(),
                files: vec![TorrentFile { id: 0, path: "M.2020.1080p.REMUX.mkv".into(), bytes: 30_000_000_000, selected: 1 }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        // The movie slot has never been read ⇒ idle ⇒ swap + prune proceed.
        run_upgrade_once(&app).await;

        let sel = store.get_selection(movie_slot(27205)).await.unwrap();
        assert_eq!(sel.hash, "hnew", "selection swapped to the upgraded release");
        assert!(store.get_owned("hnew".into()).await.is_some(), "new release recorded owned+verified");
        assert!(store.get_owned("hold".into()).await.is_none(), "old release pruned from owned");
        assert!(deleted.lock().unwrap().contains(&"told".to_string()), "old torrent deleted from provider");
    }

    #[tokio::test]
    async fn active_movie_is_not_pruned() {
        let store = mem_store();
        store.put_owned("hold".into(), OwnedRecord {
            request: movie_req(), provenance: Provenance::watchlist("a"), added_at: 1, status: OwnedStatus::Verified,
            provides: vec![], quality: Some(QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 10 }),
        }).await.unwrap();
        store.put_selection(movie_slot(27205), SelectionEntry { hash: "hold".into(), file_path: "old.mkv".into() }).await.unwrap();
        let scraper = Arc::new(MockScraper { candidates: vec![remux_candidate()] });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent { id: "told".into(), hash: "hold".into(), status: "downloaded".into(), ..Default::default() }],
            add_magnet: Some(AddMagnetResponse { id: "tnew".into(), uri: String::new() }),
            torrent_info: Some(TorrentInfo { id: "tnew".into(), hash: "hnew".into(), status: "downloaded".into(), files: vec![TorrentFile { id: 0, path: "new.mkv".into(), bytes: 30_000_000_000, selected: 1 }], links: vec!["https://cdn/new".into()], ..Default::default() }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        // The idle gate is library-wide: a recent read anywhere defers all swaps this tick.
        app.read_activity.touch("Movies/anything.mkv").await;

        run_upgrade_once(&app).await;

        // Nothing pruned, nothing staged, selection unchanged — the upgrade was deferred.
        assert!(deleted.lock().unwrap().is_empty(), "active library must not be pruned");
        assert!(store.get_owned("hnew".into()).await.is_none(), "no stage while active");
        assert_eq!(store.get_selection(movie_slot(27205)).await.unwrap().hash, "hold", "selection unchanged");
    }
}
