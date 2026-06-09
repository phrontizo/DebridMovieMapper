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

/// Run one upgrade tick over `app`: re-score a budgeted batch of owned titles. For MOVIES, stage any
/// cached meaningful upgrade and — if the library is idle — swap selection + prune the old torrent.
/// For SHOWS, consolidate scattered per-episode torrents into a full-season cached pack (Task 10).
/// Each title (movie or show) is one budget unit, ordered least-recently-checked first.
pub async fn run_upgrade_once(app: &AppState) {
    let budget = app.config.upgrade.budget_per_tick as usize;
    let idle_window = Duration::from_secs(app.config.upgrade.idle_secs);

    // Group owned by tmdb_id (reuse the tasks helper) and pick the least-recently-checked titles.
    let groups = crate::tasks::group_owned_by_tmdb(&app.store).await;
    let mut candidates: Vec<(u64, MediaType, Vec<String>, OwnedRecord)> = Vec::new();
    for (tmdb_id, g) in &groups {
        // Representative owned record (the movie path uses it; a settled record gates both kinds).
        let Some(hash) = g.hashes.first().cloned() else { continue };
        let Some(rec) = app.store.get_owned(hash.clone()).await else { continue };
        if rec.status != OwnedStatus::Verified {
            continue; // only upgrade settled titles
        }
        candidates.push((*tmdb_id, g.media_type.clone(), g.hashes.clone(), rec));
    }
    // Least-recently-checked first.
    let mut ordered: Vec<_> = Vec::new();
    for (id, media_type, hashes, rec) in candidates {
        let last = app.store.get_upgrade_checked(id).await;
        ordered.push((last, id, media_type, hashes, rec));
    }
    ordered.sort_by_key(|(last, ..)| *last);
    ordered.truncate(budget);

    for (_, tmdb_id, media_type, hashes, rec) in ordered {
        app.store.set_upgrade_checked(tmdb_id, now_secs()).await.ok();
        match media_type {
            MediaType::Movie => {
                if let Err(e) = try_upgrade_movie(app, tmdb_id, &hashes, &rec, idle_window).await {
                    warn!("upgrade: tmdb {} skipped: {}", tmdb_id, e);
                }
            }
            MediaType::Show => {
                if let Err(e) = try_consolidate_show(app, tmdb_id, &hashes, idle_window).await {
                    warn!("consolidate: tmdb {} skipped: {}", tmdb_id, e);
                }
            }
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
        // Apply the same hard filters as acquisition (resolution ceiling, cam/telesync, dead seeders):
        // score() returns None for any release that fails them. Never "upgrade" past the ceiling.
        if release::score(&r, &app.config.acquisition.prefs).is_none() {
            continue;
        }
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
    let info = match app.provider.get_torrent_info(&added.id).await {
        Ok(i) => i,
        Err(e) => {
            let _ = app.provider.delete_torrent(&added.id).await;
            return Err(format!("info failed: {e}"));
        }
    };
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

/// Inputs to the pure consolidation decision for ONE season.
#[derive(Debug, Clone)]
pub struct ConsolidationInput {
    pub season: u32,
    /// Episodes of this season aired per TMDB (the "full season" target).
    pub aired_episodes: Vec<u32>,
    /// (episode, quality) for each episode we currently own INDIVIDUALLY in this season.
    pub owned_episode_quality: Vec<(u32, QualitySummary)>,
    pub pack_cached: bool,
    /// Episodes the candidate pack supplies for this season.
    pub pack_episodes: Vec<u32>,
    /// Quality of the pack (per-episode quality is assumed uniform across the pack).
    pub pack_quality: QualitySummary,
}

/// Pure: should we consolidate this season's scattered episodes into the candidate pack?
/// Requires: the pack is CACHED; it is a FULL-season pack (covers every aired episode); and it is
/// not a quality regression vs ANY episode we currently own (same-or-higher tier AND resolution).
pub fn consolidation_target(i: &ConsolidationInput) -> bool {
    if !i.pack_cached {
        return false;
    }
    // Full season: every aired episode must be in the pack.
    let covers_full_season = i.aired_episodes.iter().all(|e| i.pack_episodes.contains(e));
    if !covers_full_season || i.aired_episodes.is_empty() {
        return false;
    }
    // No regression vs any owned episode.
    for (_, owned_q) in &i.owned_episode_quality {
        let no_regression = i.pack_quality.source_tier >= owned_q.source_tier
            && i.pack_quality.resolution >= owned_q.resolution;
        if !no_regression {
            return false;
        }
    }
    true
}

/// Consolidate a show's scattered per-episode torrents into a full-season CACHED pack, season by
/// season. Non-destructive: a pack that fails any gate (not cached / above the resolution ceiling /
/// not a full season / a quality regression / wrong title) is deleted and the scattered episodes
/// keep their selection untouched. The idle gate drops the staged pack on defer (no dangling stage).
async fn try_consolidate_show(
    app: &AppState,
    tmdb_id: u64,
    group_hashes: &[String],
    idle_window: Duration,
) -> Result<(), String> {
    // Owned per-episode records for this show: hash -> OwnedRecord.
    let mut owned: Vec<(String, OwnedRecord)> = Vec::new();
    for h in group_hashes {
        if let Some(r) = app.store.get_owned(h.clone()).await {
            owned.push((h.clone(), r));
        }
    }
    // A representative request (for imdb id + metadata) — any owned record works.
    let Some((_, sample)) = owned.first().cloned() else { return Err("no owned records".into()) };
    let today = chrono::Utc::now().date_naive();
    let aired = crate::tasks::aired_episodes(&app.tmdb_client, tmdb_id, today).await;

    // Seasons currently held as SCATTERED single-episode torrents (provides.len()==1).
    let mut seasons: Vec<u32> = owned.iter()
        .filter(|(_, r)| r.provides.len() == 1)
        .map(|(_, r)| r.provides[0].0)
        .collect();
    seasons.sort_unstable();
    seasons.dedup();

    for season in seasons {
        let season_aired = crate::tasks::season_aired(&aired, season);
        if season_aired.is_empty() { continue; }
        // Already a full-season pack owned for this season? (any hash whose provides covers it)
        let already_pack = owned.iter().any(|(_, r)| {
            let eps: Vec<u32> = r.provides.iter().filter(|(s, _)| *s == season).map(|(_, e)| *e).collect();
            season_aired.iter().all(|e| eps.contains(e)) && r.provides.len() > 1
        });
        if already_pack { continue; }

        // Scrape the season (episode 1 query returns season packs too).
        let raws = match app.scraper.find(&sample.request.imdb_id, MediaKind::Series, Some(season), Some(1)).await {
            Ok(r) => r,
            Err(e) => { warn!("consolidate: scrape s{} failed: {}", season, e); continue; }
        };
        // Try cached candidates that look like packs (file_name absent or multiple videos after stage).
        for raw in &raws {
            let r = release::parse(raw);
            if !r.cached { continue; }
            if app.store.is_blacklisted(tmdb_id, r.info_hash.clone()).await { continue; }
            if group_hashes.iter().any(|h| h.eq_ignore_ascii_case(&r.info_hash)) { continue; }
            // Apply the same hard filters as acquisition (resolution ceiling, cam/telesync, dead
            // seeders): score() returns None for any release that fails them. A full-season pack
            // above the ceiling (e.g. cached 2160p under a 1080p ceiling) would pass the
            // no-regression check yet violate the ceiling — never adopt it.
            if release::score(&r, &app.config.acquisition.prefs).is_none() {
                continue;
            }

            // Stage: add + resolve + SE-map files.
            let magnet = format!("magnet:?xt=urn:btih:{}", r.info_hash);
            let Ok(added) = app.provider.add_magnet(&magnet).await else { continue };
            let Ok(info) = app.provider.get_torrent_info(&added.id).await else {
                let _ = app.provider.delete_torrent(&added.id).await; continue;
            };
            if info.status != "downloaded" { let _ = app.provider.delete_torrent(&added.id).await; continue; }
            // Select all videos so the pack is fully available.
            let ids: Vec<u32> = info.files.iter().filter(|f| crate::vfs::is_video_file(&f.path)).map(|f| f.id).collect();
            if !ids.is_empty() {
                let _ = app.provider.select_files(&added.id, &ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")).await;
            }
            let fresh = app.provider.get_torrent_info(&added.id).await.unwrap_or(info);
            let eps = crate::acquire::episode_files(&fresh); // (s,e,path) — pub(crate) from Task 6
            let pack_episodes: Vec<u32> = eps.iter().filter(|(s, _, _)| *s == season).map(|(_, e, _)| *e).collect();

            // Owned per-episode quality for this season.
            let owned_episode_quality: Vec<(u32, QualitySummary)> = owned.iter()
                .filter(|(_, rec)| rec.provides.len() == 1 && rec.provides[0].0 == season)
                .map(|(_, rec)| (rec.provides[0].1, rec.quality.clone().unwrap_or_default()))
                .collect();

            let input = ConsolidationInput {
                season,
                aired_episodes: season_aired.clone(),
                owned_episode_quality,
                pack_cached: true,
                pack_episodes: { let mut v = pack_episodes.clone(); v.sort_unstable(); v.dedup(); v },
                pack_quality: QualitySummary::of(&r, &app.config.acquisition.prefs),
            };
            if !consolidation_target(&input) {
                let _ = app.provider.delete_torrent(&added.id).await; // non-destructive: drop the staged pack
                continue;
            }
            // Validate the show identity on a representative episode file.
            if let Some((es, ee, path)) = eps.iter().find(|(s, _, _)| *s == season) {
                let fname = path.rsplit('/').next().unwrap_or(path).to_string();
                if !app.engine.validate_title(&fname, tmdb_id, MediaKind::Series, Some(*es), Some(*ee)).await {
                    let _ = app.provider.delete_torrent(&added.id).await;
                    let _ = app.store.blacklist_add(tmdb_id, r.info_hash.clone(), "WrongTitle", now_secs()).await;
                    continue;
                }
            }
            // Idle gate. If the library is active, drop the staged pack (no dangling stage) and
            // retry on a later tick; consolidation re-stages cheaply (the pack is cached).
            if !app.read_activity.all_idle(idle_window).await {
                info!("consolidate: tmdb {} s{} deferred (library active); dropping staged pack", tmdb_id, season);
                let _ = app.provider.delete_torrent(&added.id).await;
                return Ok(());
            }
            // Record the pack owned+verified with full-season provides + sticky provenance.
            let prov = base_req_provenance(app, tmdb_id).await;
            let provides: Vec<(u32, u32)> = eps.iter().map(|(s, e, _)| (*s, *e)).collect();
            let _ = app.store.put_owned(r.info_hash.clone(), OwnedRecord {
                request: crate::store::AcquireRequest { season: Some(season), episode: Some(1), ..sample.request.clone() },
                provenance: prov,
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: provides.clone(),
                quality: Some(QualitySummary::of(&r, &app.config.acquisition.prefs)),
            }).await;
            let _ = app.store.put_authoritative(r.info_hash.clone(), sample.request.metadata.clone()).await;
            // Repoint every episode slot of this season to the pack.
            for (s, e, path) in &eps {
                if *s != season { continue; }
                let _ = app.store.put_selection(
                    crate::store::episode_slot(tmdb_id, *s, *e),
                    crate::store::SelectionEntry { hash: r.info_hash.clone(), file_path: path.clone() },
                ).await;
            }
            // Prune the scattered episode torrents for this season.
            for (h, rec) in &owned {
                if rec.provides.len() == 1 && rec.provides[0].0 == season {
                    prune_owned_hash(app, h).await;
                }
            }
            info!("consolidate: tmdb {} season {} -> pack {}", tmdb_id, season, r.info_hash);
            break; // one pack per season per tick
        }
    }
    Ok(())
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
    /// A cached WEB 1080p candidate — same tier and resolution as the owned "hold" record.
    /// Not a meaningful upgrade; used to verify the no-churn guard.
    fn web_1080_candidate() -> RawCandidate {
        RawCandidate { name: "Torrentio\n1080p".into(), description: "M.2020.1080p.WEB-DL.x265\nRD+".into(), info_hash: "hweb".into(), file_idx: Some(0), file_name: Some("M.2020.1080p.WEB-DL.mkv".into()) }
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

    #[tokio::test]
    async fn no_meaningful_upgrade_is_a_noop() {
        let store = mem_store();
        // Owned: cached WEB 1080p movie, Verified, with a selection.
        store.put_owned("hold".into(), OwnedRecord {
            request: movie_req(), provenance: Provenance::watchlist("a"), added_at: 1, status: OwnedStatus::Verified,
            provides: vec![],
            quality: Some(QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 10 }),
        }).await.unwrap();
        store.put_selection(movie_slot(27205), SelectionEntry { hash: "hold".into(), file_path: "old.mkv".into() }).await.unwrap();

        // Scraper returns only a same-tier same-resolution WEB 1080p candidate — not an upgrade.
        let scraper = Arc::new(MockScraper { candidates: vec![web_1080_candidate()] });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        run_upgrade_once(&app).await;

        assert!(deleted.lock().unwrap().is_empty(), "nothing pruned on no-upgrade");
        assert!(store.get_owned("hweb".into()).await.is_none(), "no new owned record for same-tier candidate");
        assert_eq!(store.get_selection(movie_slot(27205)).await.unwrap().hash, "hold", "selection unchanged");
    }

    #[tokio::test]
    async fn above_ceiling_cached_release_is_not_staged() {
        let store = mem_store();
        // Owned: cached WEB 1080p movie, Verified, with a selection.
        store.put_owned("hold".into(), OwnedRecord {
            request: movie_req(), provenance: Provenance::watchlist("a"), added_at: 1, status: OwnedStatus::Verified,
            provides: vec![],
            quality: Some(QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 10 }),
        }).await.unwrap();
        store.put_selection(movie_slot(27205), SelectionEntry { hash: "hold".into(), file_path: "old.mkv".into() }).await.unwrap();

        // Scraper returns a cached 2160p REMUX — above the P1080 ceiling in AcquisitionConfig::default().
        let cand_4k = RawCandidate {
            name: "Torrentio\n2160p".into(),
            description: "M.2020.2160p.BluRay.REMUX.x265\nRD+".into(),
            info_hash: "h4k".into(),
            file_idx: Some(0),
            file_name: Some("M.2020.2160p.REMUX.mkv".into()),
        };
        let scraper = Arc::new(MockScraper { candidates: vec![cand_4k] });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        run_upgrade_once(&app).await;

        // release::score returns None for 2160p when ceiling is P1080 → candidate is filtered out.
        assert!(deleted.lock().unwrap().is_empty(), "nothing staged/pruned when candidate is above ceiling");
        assert!(store.get_owned("h4k".into()).await.is_none(), "no new owned record for above-ceiling candidate");
        assert_eq!(store.get_selection(movie_slot(27205)).await.unwrap().hash, "hold", "selection unchanged");
    }

    // ── pure consolidation-decision tests (Task 10) ───────────────────────────

    #[test]
    fn full_cached_season_pack_no_regression_consolidates() {
        // Owned scattered: E01 (WEB 1080p), E02 (WEB 1080p). Pack: cached BluRay 1080p covering E01-E03.
        let input = ConsolidationInput {
            season: 1,
            aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![
                (1, crate::release::QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 1 }),
                (2, crate::release::QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 1 }),
            ],
            pack_cached: true,
            pack_episodes: vec![1, 2, 3],
            pack_quality: crate::release::QualitySummary { cached: true, source_tier: 6_000, resolution: 1080, score: 5 },
        };
        assert!(consolidation_target(&input), "cached full-season pack, no regression → consolidate");
    }

    #[test]
    fn partial_season_pack_is_rejected() {
        let input = ConsolidationInput {
            season: 1, aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![(1, q1080_web()), (2, q1080_web())],
            pack_cached: true, pack_episodes: vec![1, 2], // missing E03
            pack_quality: q1080_bluray(),
        };
        assert!(!consolidation_target(&input), "partial-season pack must not consolidate");
    }

    #[test]
    fn quality_regression_pack_is_rejected() {
        let input = ConsolidationInput {
            season: 1, aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![(1, q2160_remux()), (2, q1080_web())], // E01 is 2160 REMUX
            pack_cached: true, pack_episodes: vec![1, 2, 3],
            pack_quality: q1080_bluray(), // worse than E01 → regression
        };
        assert!(!consolidation_target(&input), "a pack worse than any owned episode must not consolidate");
    }

    #[test]
    fn uncached_pack_is_rejected() {
        let input = ConsolidationInput {
            season: 1, aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![(1, q1080_web())],
            pack_cached: false, pack_episodes: vec![1, 2, 3], pack_quality: q1080_bluray(),
        };
        assert!(!consolidation_target(&input));
    }

    fn q1080_web() -> crate::release::QualitySummary { crate::release::QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 1 } }
    fn q1080_bluray() -> crate::release::QualitySummary { crate::release::QualitySummary { cached: true, source_tier: 6_000, resolution: 1080, score: 2 } }
    fn q2160_remux() -> crate::release::QualitySummary { crate::release::QualitySummary { cached: true, source_tier: 8_000, resolution: 2160, score: 9 } }
}
