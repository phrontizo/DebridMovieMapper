//! SP3 upgrade engine. A slow periodic job (gated on `UPGRADE_INTERVAL_SECS`, default daily) that
//! re-scores owned titles and stages meaningfully-better CACHED releases — and full-season cached
//! packs (Task 10) — swapping the persisted `selection` and pruning the superseded torrent only
//! once the slot is idle (proxy read-activity). Upgrades are non-destructive: a failed stage never
//! degrades the working release.

use crate::app_state::AppState;
use crate::now_unix_secs as now_secs;
use crate::release::{self, QualitySummary};
use crate::scraper::MediaKind;
use crate::store::{movie_slot, OwnedRecord, OwnedStatus};
use crate::vfs::MediaType;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Why an upgrade/consolidation attempt made no change. The distinction matters for the round-robin
/// cursor: a `Deferred` title was NOT actually evaluated (the library went active mid-stage, or the
/// provider/scraper was momentarily unavailable) and must be reconsidered next tick — its cursor is
/// left UNSTAMPED. A `NoChange` title WAS evaluated to completion (no meaningful upgrade exists, or
/// the chosen candidate didn't pan out) and its cursor is stamped so the budget rotates onward.
enum UpgradeSkip {
    Deferred(String),
    NoChange(String),
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
    for ((_mt, tmdb_id), g) in &groups {
        // Manual adds are never auto-managed (invariant M-1): skip upgrade/consolidation for them.
        if g.provenance.has_manual_entry() {
            tracing::debug!(
                "upgrade: tmdb {} skipped (manual provenance — never auto-managed)",
                tmdb_id
            );
            continue;
        }
        // Eligibility is gated on ONE representative record's status — the lexicographically-first
        // hash in the group (`g.hashes` is sorted) — NOT on every record in the group. This is a
        // coarse, order-dependent heuristic: for a show whose episodes are a mix of Pending and
        // Verified, whether the title is considered THIS tick depends on which hash happens to sort
        // first. That's acceptable because it self-corrects across ticks — the per-handler logic
        // re-reads each record's own state (the movie path baselines on every owned copy via
        // `best_owned_quality`; the consolidation path re-reads each episode record), and a season
        // whose representative isn't settled yet simply gets reconsidered on a later tick once it
        // lands Verified. The representative record also seeds the movie path's request metadata.
        let Some(hash) = g.hashes.first().cloned() else {
            continue;
        };
        let Some(rec) = app.store.get_owned(hash.clone()).await else {
            continue;
        };
        if rec.status != OwnedStatus::Verified {
            continue; // representative record not settled yet → reconsider next tick
        }
        candidates.push((*tmdb_id, g.media_type.clone(), g.hashes.clone(), rec));
    }
    // Least-recently-checked first.
    let mut ordered: Vec<_> = Vec::new();
    for (id, media_type, hashes, rec) in candidates {
        let last = app.store.get_upgrade_checked(&media_type, id).await;
        ordered.push((last, id, media_type, hashes, rec));
    }
    ordered.sort_by_key(|(last, ..)| *last);
    ordered.truncate(budget);

    for (_, tmdb_id, media_type, hashes, rec) in ordered {
        // Library-wide idle gate BEFORE stamping the cursor. The actual swap/prune is idle-gated
        // inside each handler, but stamping `upgrade_checked` for a title we then defer (library
        // active) would advance the round-robin cursor past it — so on a frequently-streamed library
        // a title could wait a full cursor wrap (library_size / budget ticks) before being
        // reconsidered even once the library goes idle. Since the gate is library-wide, an active
        // library blocks the whole remaining batch: break without stamping so no title is marked
        // "checked" while it could not actually be upgraded. (The handlers keep their own idle
        // re-check as the read-vs-prune race guard for activity that starts mid-tick.)
        if !app.read_activity.all_idle(idle_window).await {
            debug!(
                "upgrade: library active; deferring remaining titles this tick (cursor unchanged)"
            );
            break;
        }
        let outcome = match media_type {
            MediaType::Movie => try_upgrade_movie(app, tmdb_id, &hashes, &rec, idle_window).await,
            MediaType::Show => try_consolidate_show(app, tmdb_id, &hashes, idle_window).await,
        };
        match outcome {
            // Evaluated to completion (success or a genuine no-upgrade): advance the cursor.
            Ok(()) => {
                app.store
                    .set_upgrade_checked(&media_type, tmdb_id, now_secs())
                    .await
                    .ok();
            }
            Err(UpgradeSkip::NoChange(reason)) => {
                // "no meaningful upgrade" is the normal outcome for most titles, so this is a debug
                // detail, not a warning — keeps the default-level log clean.
                debug!("upgrade: tmdb {} skipped: {}", tmdb_id, reason);
                app.store
                    .set_upgrade_checked(&media_type, tmdb_id, now_secs())
                    .await
                    .ok();
            }
            // NOT evaluated (library active mid-stage / provider unavailable): leave the cursor
            // unstamped so this title is reconsidered next tick rather than skipped for a full wrap.
            Err(UpgradeSkip::Deferred(reason)) => {
                debug!(
                    "upgrade: tmdb {} deferred: {} (cursor unchanged)",
                    tmdb_id, reason
                );
            }
        }
    }
}

/// The best (highest-`score`) quality across ALL owned copies of a title, or `None` if ANY owned
/// copy has unknown quality (a legacy/untagged mirror record). Used as the upgrade baseline so a
/// swap that prunes every copy can only proceed against the BEST one it would delete — never
/// downgrading by comparing against a worse duplicate. `None` (unknown copy present) is a
/// conservative skip: we can't prove a candidate beats an unmeasured copy, so we don't risk
/// deleting it.
async fn best_owned_quality(app: &AppState, owned_hashes: &[String]) -> Option<QualitySummary> {
    let mut best: Option<QualitySummary> = None;
    for h in owned_hashes {
        let rec = app.store.get_owned(h.clone()).await?;
        let q = rec.quality?; // any unknown-quality owned copy → conservative skip (None)
        if best.as_ref().map(|b| q.score > b.score).unwrap_or(true) {
            best = Some(q);
        }
    }
    best
}

/// The IMDB id to scrape a group with: prefer ANY owned copy that actually carries a non-empty
/// IMDB id (Torrentio is IMDB-keyed) over only the lexicographically-first representative — in a
/// mixed group the representative can be an empty-imdb account-mirror record, which would otherwise
/// permanently exclude an otherwise-upgradeable title. `None` only when NO owned copy has an id.
async fn group_imdb_id(app: &AppState, owned_hashes: &[String]) -> Option<String> {
    for h in owned_hashes {
        if let Some(rec) = app.store.get_owned(h.clone()).await {
            if !rec.request.imdb_id.is_empty() {
                return Some(rec.request.imdb_id);
            }
        }
    }
    None
}

/// The owned record to use for a show group's scrape/stage metadata: prefer one with a non-empty
/// IMDB id (same reasoning as [`group_imdb_id`] — a mixed group's first record may be an empty-imdb
/// mirror), falling back to the first record so the no-imdb skip below still triggers cleanly.
fn scrape_sample(owned: &[(String, OwnedRecord)]) -> Option<OwnedRecord> {
    owned
        .iter()
        .find(|(_, r)| !r.request.imdb_id.is_empty())
        .or_else(|| owned.first())
        .map(|(_, r)| r.clone())
}

/// Stage + (idle-gated) swap a single movie title. Returns Err(reason) on a non-fatal skip.
async fn try_upgrade_movie(
    app: &AppState,
    tmdb_id: u64,
    owned_hashes: &[String],
    owned_rec: &OwnedRecord,
    idle_window: Duration,
) -> Result<(), UpgradeSkip> {
    // 1. Baseline = the BEST quality across ALL owned copies of this title — NOT an arbitrary one.
    //    The swap below prunes EVERY owned hash, so if a title has multiple present copies (the
    //    default state: account-mirror records them all and dedup is dry-run by default), comparing
    //    against only the representative (lexicographically-first) copy could "upgrade" past the
    //    worse copy yet DELETE a better one — e.g. own {WEB 1080p, REMUX 1080p}, baseline off WEB,
    //    adopt a BluRay 1080p, prune the REMUX → a downgrade + loss. `best_owned_quality` returns
    //    `None` (→ skip) if ANY owned copy's quality is unknown (a legacy/untagged mirror record, or
    //    above the ceiling): comparing against a default (uncached/tier-0) would treat any cached
    //    candidate as an upgrade and could delete a possibly-better unknown copy. Checked BEFORE the
    //    scrape so unupgradeable titles don't waste a scrape every tick. (`owned_rec` is still used
    //    below for the request's imdb_id/metadata — only the quality BASELINE is widened here.)
    let Some(current) = best_owned_quality(app, owned_hashes).await else {
        return Err(UpgradeSkip::NoChange(
            "current quality unknown/mixed; not upgrading (avoids regression)".into(),
        ));
    };
    // Torrentio is IMDB-keyed: a record whose IMDB id never resolved (stored empty) can't be
    // scraped, so skip before the network call rather than issuing a fruitless empty-key request
    // every budgeted tick (mirrors `build_acquire_request`'s skip on the acquisition path). Resolve
    // the id from ANY owned copy in the group, not just the representative — a mixed group whose
    // representative is an empty-imdb mirror must still be upgradeable via an engine copy's id.
    let Some(imdb_id) = group_imdb_id(app, owned_hashes).await else {
        return Err(UpgradeSkip::NoChange(
            "no imdb id in any owned copy; cannot scrape for upgrade".into(),
        ));
    };
    // Scrape/stage with the representative's metadata but the group-resolved IMDB id, so the staged
    // record carries a usable id even when the representative didn't.
    let req = crate::store::AcquireRequest {
        imdb_id: imdb_id.clone(),
        ..owned_rec.request.clone()
    };
    // 2. Scrape fresh candidates for this title, then pick the best cached meaningful upgrade not
    //    already owned/blacklisted. A scrape failure is transient → Deferred (retry next tick).
    let raws = app
        .scraper
        .find(&req.imdb_id, MediaKind::Movie, None, None)
        .await
        .map_err(|e| UpgradeSkip::Deferred(format!("scrape failed: {e}")))?;
    let mut best: Option<(release::ReleaseInfo, QualitySummary)> = None;
    for raw in &raws {
        let r = release::parse(raw);
        if owned_hashes
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&r.info_hash))
        {
            continue;
        }
        if app
            .store
            .is_blacklisted(MediaKind::Movie, tmdb_id, r.info_hash.clone())
            .await
        {
            continue;
        }
        // Apply the same hard filters as acquisition (resolution ceiling, cam/telesync, dead seeders):
        // score() returns None for any release that fails them. Never "upgrade" past the ceiling.
        if release::score(&r, &app.config.acquisition.prefs).is_none() {
            continue;
        }
        let q = QualitySummary::of(&r, &app.config.acquisition.prefs);
        if release::is_target_improvement(&current, &q, &app.config.acquisition.prefs)
            == release::Improvement::None
        {
            continue;
        }
        if best
            .as_ref()
            .map(|(_, bq)| q.score > bq.score)
            .unwrap_or(true)
        {
            best = Some((r, q));
        }
    }
    let Some((cand, _q)) = best else {
        return Err(UpgradeSkip::NoChange("no meaningful upgrade".into()));
    };

    // 3. Idle gate FIRST. Upgrade targets are cached-only (instant to add), so there is no benefit
    //    to pre-staging a download — we only commit when the library is idle, and skip otherwise.
    //    This guarantees we never hold two copies of a title (no dangling stage), which is why
    //    `UPGRADE_STAGE_MAX_SECS` is config-only/reserved on this cached path (kept for forward-compat
    //    with a future speculative-download upgrade mode; not consulted here).
    if !app.read_activity.all_idle(idle_window).await {
        return Err(UpgradeSkip::Deferred(
            "library active; deferring upgrade".into(),
        ));
    }

    // 4. Stage the cached candidate: add + validate + record Verified (non-destructive — any failure
    //    leaves the current release untouched). Returns (hash, torrent_id, selected_file_path).
    //    stage_and_verify classifies its own outcome: a transient provider/probe glitch → Deferred
    //    (retry next tick), a candidate-specific failure (not cached / wrong title / pack) → NoChange.
    let staged = stage_and_verify(app, tmdb_id, &req, &cand).await?;

    // Fetch the provider listing for the prune ONCE, and BEFORE the idle re-check below, so there is
    // NO network round-trip between the idle confirmation and the destructive prune — a read
    // starting in that window could otherwise have the old torrent deleted out from under it (the
    // same ordering the consolidation path uses). Also avoids a per-hash re-fetch.
    // Distinguish a FETCH FAILURE from a genuinely-empty account: `unwrap_or_default()` would make a
    // failed `get_torrents` look empty, so `prune_owned_hash_in` (which treats "hash absent from the
    // listing" as "already gone → drop the record") would drop the old hashes' records while leaving
    // the still-present torrents orphaned → re-adopted as duplicates. On an `Err`, roll back the
    // staged candidate and defer (mirrors the idle-defer rollback below).
    let listing = match app.provider.get_torrents().await {
        Ok(l) => l,
        Err(_) => {
            if app.provider.delete_torrent(&staged.1).await.is_ok() {
                let _ = app.store.remove_owned(staged.0.clone()).await;
                let _ = app.store.remove_authoritative(staged.0.clone()).await;
            }
            return Err(UpgradeSkip::Deferred(
                "provider listing unavailable; deferring upgrade".into(),
            ));
        }
    };

    // 4b. Re-check idle IMMEDIATELY before the destructive swap+prune. Staging above can take many
    //     seconds (add + select + cached-poll + 4 MB probe), during which a playback read may have
    //     begun anywhere in the library. If it is no longer idle, fully roll back the freshly-staged
    //     candidate (delete it + drop its records — non-destructive: the current release is
    //     untouched) and retry later, so the prune below never interrupts an in-flight stream.
    if !app.read_activity.all_idle(idle_window).await {
        // Roll back by the KNOWN torrent id, not the listing snapshot: the just-staged torrent may
        // not have propagated into `listing` yet. Only drop the store records when the delete
        // SUCCEEDED — on a transient delete failure, KEEP them so a later tick retries rather than
        // leaving a present-but-untracked torrent that `record_mirror_owned` would re-adopt as a
        // DUPLICATE (the `execute_remove`/`prune_owned_hash_in` keep-on-failure discipline).
        if app.provider.delete_torrent(&staged.1).await.is_ok() {
            let _ = app.store.remove_owned(staged.0.clone()).await;
            let _ = app.store.remove_authoritative(staged.0.clone()).await;
        }
        return Err(UpgradeSkip::Deferred(
            "library became active during staging; deferring upgrade".into(),
        ));
    }

    // 5. Swap selection → new hash, then prune every old owned hash. If the selection repoint did
    //    NOT persist, do NOT prune — pruning the old torrents while the selection still points at one
    //    of them would leave the slot resolving to a hash we're about to delete (a stale DB row at
    //    best). Defer the prune; the staged copy stays owned and is reclaimed by the duplicate-dedup
    //    pass (a put_selection failure is a rare redb-write/disk error that triggers DB self-heal
    //    anyway). Strictly safer than pruning behind a stale selection.
    if let Err(e) = app
        .store
        .put_selection(
            movie_slot(tmdb_id),
            crate::store::SelectionEntry {
                hash: staged.0.clone(),
                file_path: staged.2.clone(),
            },
        )
        .await
    {
        return Err(UpgradeSkip::Deferred(format!(
            "selection write failed; deferring prune: {e}"
        )));
    }
    for old in owned_hashes {
        if old.eq_ignore_ascii_case(&staged.0) {
            continue;
        }
        prune_owned_hash_in(app, old, &listing).await;
    }
    info!("upgrade: tmdb {} swapped to {}", tmdb_id, staged.0);
    Ok(())
}

/// Real-Debrid reports a freshly-added torrent as `waiting_files_selection` (its file list is
/// already populated) until `select_files` is called, and only flips a *cached* torrent to
/// `downloaded` a moment after the selection registers. So an upgrade stage must select FIRST, then
/// poll for the cached verdict — gating on `status == "downloaded"` before selection would reject
/// every candidate on RD (this is exactly how the `observe` path is ordered). Returns the latest
/// `TorrentInfo` seen so the caller can inspect `status`/`files` against the post-selection state.
async fn await_downloaded(
    provider: &dyn crate::provider::DebridProvider,
    id: &str,
) -> Option<crate::rd_client::TorrentInfo> {
    const ATTEMPTS: u32 = 10;
    const SETTLE: Duration = Duration::from_secs(1);
    let mut last = None;
    for i in 0..ATTEMPTS {
        if i > 0 {
            tokio::time::sleep(SETTLE).await;
        }
        if let Ok(info) = provider.get_torrent_info(id).await {
            let done = info.status == "downloaded";
            last = Some(info);
            if done {
                break;
            }
        }
    }
    last
}

/// Add the candidate, wait briefly for it to resolve (it should be cached), validate + record it
/// Verified with sticky provenance, and return (hash, torrent_id, selected_file_path). On any
/// failure the candidate is cleaned up and the current release is left untouched (non-destructive).
async fn stage_and_verify(
    app: &AppState,
    tmdb_id: u64,
    base_req: &crate::store::AcquireRequest,
    cand: &release::ReleaseInfo,
) -> Result<(String, String, String), UpgradeSkip> {
    let hash = cand.info_hash.clone();
    let magnet = format!("magnet:?xt=urn:btih:{}", hash);
    // A provider add/info failure is transient → Deferred (retry next tick, keep the cursor) rather
    // than NoChange (which would skip this title — with its real upgrade candidate — for a full wrap).
    let added = app
        .provider
        .add_magnet(&magnet)
        .await
        .map_err(|e| UpgradeSkip::Deferred(format!("add failed: {e}")))?;
    let info = match app.provider.get_torrent_info(&added.id).await {
        Ok(i) => i,
        Err(e) => {
            let _ = app.provider.delete_torrent(&added.id).await;
            return Err(UpgradeSkip::Deferred(format!("info failed: {e}")));
        }
    };
    // Select the single feature file FIRST: RD only reports a (cached) torrent as `downloaded`
    // after selection, so gating on cached-ness before selecting would reject every candidate on
    // RD (mirrors the `observe` ordering). The file list is populated pre-selection.
    let Some(file) = info
        .files
        .iter()
        .filter(|f| crate::vfs::is_video_file(&f.path))
        .max_by_key(|f| f.bytes)
    else {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err(UpgradeSkip::NoChange("no video file".into()));
    };
    let csv = file.id.to_string();
    let selected_path = file.path.clone();
    // A select_files error is caught downstream by the `status != "downloaded"` gate, but log it so a
    // real provider failure isn't invisible on the movie path (parity with the consolidation path).
    if let Err(e) = app.provider.select_files(&added.id, &csv).await {
        warn!(
            "upgrade: select_files for staged candidate {} failed: {}",
            hash, e
        );
    }
    // Re-fetch and wait for the cached verdict; only a `downloaded` torrent may be staged (we never
    // speculatively download upgrades). All later gates run against this post-selection `fresh`.
    let fresh = await_downloaded(app.provider.as_ref(), &added.id)
        .await
        .unwrap_or(info);
    if fresh.status != "downloaded" {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err(UpgradeSkip::NoChange("candidate not cached".into()));
    }
    let file_name = selected_path
        .rsplit('/')
        .next()
        .unwrap_or(&selected_path)
        .to_string();
    // Movie-pack guard (I-2): a candidate that materialises into more than one feature-sized
    // video is a multi-movie pack (providers like TorBox auto-select all files) — never adopt it.
    // Run BEFORE title-validation to save a TMDB lookup on a pack that would be rejected anyway.
    if crate::acquire::count_feature_videos(&fresh) > 1 {
        let _ = app.provider.delete_torrent(&added.id).await;
        let _ = app
            .store
            .blacklist_add(
                MediaKind::Movie,
                tmdb_id,
                hash.clone(),
                "MoviePack",
                now_secs(),
            )
            .await;
        return Err(UpgradeSkip::NoChange("multi-feature pack".into()));
    }
    // Title validation (the engine exposes `validate_title` — see below).
    if !app
        .engine
        .validate_title(&file_name, tmdb_id, MediaKind::Movie, None, None)
        .await
    {
        let _ = app.provider.delete_torrent(&added.id).await;
        let _ = app
            .store
            .blacklist_add(
                MediaKind::Movie,
                tmdb_id,
                hash.clone(),
                "WrongTitle",
                now_secs(),
            )
            .await;
        return Err(UpgradeSkip::NoChange("title mismatch".into()));
    }
    // Probe gate (I-1): run the SAME audio/subtitle probe as acquisition before adopting. A
    // wrong-language/corrupt release is blacklisted + dropped; a transient probe failure is
    // dropped without blacklisting (retried next tick) so we never degrade the working release.
    match app
        .engine
        .probe_file(&fresh, &hash, &selected_path, base_req)
        .await
    {
        crate::acquire::VerifyResult::Pass | crate::acquire::VerifyResult::Accept => {}
        crate::acquire::VerifyResult::Reject(reason) => {
            let _ = app.provider.delete_torrent(&added.id).await;
            let _ = app
                .store
                .blacklist_add(MediaKind::Movie, tmdb_id, hash.clone(), reason, now_secs())
                .await;
            return Err(UpgradeSkip::NoChange(format!("probe rejected: {reason}")));
        }
        crate::acquire::VerifyResult::Defer => {
            let _ = app.provider.delete_torrent(&added.id).await;
            return Err(UpgradeSkip::Deferred("probe deferred".into()));
        }
    }
    // Record Verified with sticky provenance from the owned record we are upgrading.
    let prov = base_req_provenance(app, MediaType::Movie, tmdb_id).await;
    let _ = app
        .store
        .put_owned(
            hash.clone(),
            OwnedRecord {
                request: base_req.clone(),
                provenance: prov,
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: Some(QualitySummary::of(cand, &app.config.acquisition.prefs)),
            },
        )
        .await;
    let _ = app
        .store
        .put_authoritative(hash.clone(), base_req.metadata.clone())
        .await;
    Ok((hash, added.id, selected_path))
}

/// Delete a torrent (owned-only) and drop its owned record + authoritative id, against an
/// already-fetched provider listing — so a caller pruning N hashes does ONE `get_torrents()` instead
/// of N (the movie swap and the consolidation prune both fetch the listing once and reuse it).
async fn prune_owned_hash_in(app: &AppState, hash: &str, torrents: &[crate::rd_client::Torrent]) {
    // Delete the provider torrent(s) FIRST; only drop the store records when the provider side is
    // gone (deleted now, or already absent from the listing). On a transient delete failure, KEEP
    // the records so a later tick retries rather than orphaning a present-but-untracked torrent that
    // `record_mirror_owned` would re-adopt as a duplicate of the just-superseded release (mirrors
    // `execute_remove`).
    let mut all_gone = true;
    for t in torrents
        .iter()
        .filter(|t| t.hash.eq_ignore_ascii_case(hash))
    {
        if app.provider.delete_torrent(&t.id).await.is_err() {
            all_gone = false;
        }
    }
    if all_gone {
        let _ = app.store.remove_owned(hash.to_string()).await;
        let _ = app.store.remove_authoritative(hash.to_string()).await;
    }
}

/// The provenance to keep on a staged upgrade: the merged provenance of the title's current owned
/// hashes (sticky — preserves Manual / per-user origins across the swap).
async fn base_req_provenance(
    app: &AppState,
    media_type: MediaType,
    tmdb_id: u64,
) -> crate::store::Provenance {
    let groups = crate::tasks::group_owned_by_tmdb(&app.store).await;
    groups
        .get(&(media_type, tmdb_id))
        .map(|g| g.provenance.clone())
        .unwrap_or_else(crate::store::Provenance::manual)
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

/// Per-episode owned quality for one season, gathered from EVERY owned record that supplies an
/// episode of it — scattered singletons AND multi-episode/partial packs alike. The consolidation
/// prune below supersedes *any* in-season-covered record (not just singletons), so the
/// no-regression gate must see all of them, expanded per-episode. Returns `None` if any
/// contributing record has unknown quality (`quality == None`, e.g. a legacy mirror pack): we then
/// cannot prove the candidate pack isn't a regression, so the caller must skip the season rather
/// than risk downgrading a held higher-quality pack.
fn season_owned_quality(
    owned: &[(String, OwnedRecord)],
    season: u32,
) -> Option<Vec<(u32, QualitySummary)>> {
    let mut out = Vec::new();
    for (_, rec) in owned {
        let in_season: Vec<u32> = rec
            .provides
            .iter()
            .filter(|(s, _)| *s == season)
            .map(|(_, e)| *e)
            .collect();
        if in_season.is_empty() {
            continue;
        }
        let q = rec.quality.as_ref()?; // unknown quality → cannot prove no-regression
        for e in in_season {
            out.push((e, q.clone()));
        }
    }
    Some(out)
}

/// Pure: is `season` worth consolidating? Only when ≥2 owned records supply episodes of it (genuine
/// scatter to merge). With a single owned record (e.g. a one-aired-episode season held as a
/// singleton), adopting an equal-quality single-episode "pack" is a lateral move, not a
/// consolidation — and because the no-regression gate is `>=` (not a strict gain), two equal cached
/// releases would swap A→B→A every daily tick (add + 4 MB probe + delete + selection churn →
/// spurious Jellyfin re-notify). Once episodes accumulate into ≥2 records the season consolidates; a
/// single record is left for the acquisition path to fill.
fn season_has_scatter(owned: &[(String, OwnedRecord)], season: u32) -> bool {
    owned
        .iter()
        .filter(|(_, r)| r.provides.iter().any(|(s, _)| *s == season))
        .count()
        >= 2
}

/// Rank scraped consolidation candidates: keep only the CACHED, in-ceiling (score-passing) packs not
/// already owned, sorted best-`score`-first. So consolidation adopts the BEST pack — consistent with
/// the movie upgrade path's max-by-score ranking — instead of the first acceptable one in raw scraper
/// order (which, once adopted, the `already_pack` short-circuit would strand the season on). The
/// async blacklist filter is applied by the caller (it can't be pure). Pure → unit-testable.
fn rank_cached_pack_candidates(
    raws: &[crate::release::RawCandidate],
    prefs: &crate::config::QualityPrefs,
    owned_hashes: &[String],
) -> Vec<release::ReleaseInfo> {
    let mut scored: Vec<(release::ReleaseInfo, i64)> = raws
        .iter()
        .map(release::parse)
        .filter(|r| r.cached)
        .filter(|r| {
            !owned_hashes
                .iter()
                .any(|h| h.eq_ignore_ascii_case(&r.info_hash))
        })
        .filter_map(|r| release::score(&r, prefs).map(|s| (r, s)))
        .collect();
    scored.sort_by_key(|(_, s)| std::cmp::Reverse(*s)); // best score first
    scored.into_iter().map(|(r, _)| r).collect()
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
) -> Result<(), UpgradeSkip> {
    // Owned per-episode records for this show: hash -> OwnedRecord.
    let mut owned: Vec<(String, OwnedRecord)> = Vec::new();
    for h in group_hashes {
        if let Some(r) = app.store.get_owned(h.clone()).await {
            owned.push((h.clone(), r));
        }
    }
    // A representative request (for imdb id + metadata): prefer an owned record that actually has a
    // non-empty IMDB id (a mixed group's first record may be an empty-imdb mirror — see scrape_sample).
    let Some(sample) = scrape_sample(&owned) else {
        return Err(UpgradeSkip::NoChange("no owned records".into()));
    };
    // Torrentio is IMDB-keyed: a show whose IMDB id never resolved in ANY owned copy can't be
    // scraped for a consolidation pack, so skip before the TMDB aired-episode lookup + per-season
    // scrapes rather than issuing fruitless empty-key requests each tick.
    if sample.request.imdb_id.is_empty() {
        return Err(UpgradeSkip::NoChange(
            "no imdb id; cannot scrape for consolidation".into(),
        ));
    }
    let today = chrono::Utc::now().date_naive();
    // Per-season completeness is enough here: a season only ever proceeds below when its
    // `season_aired` is non-empty, which means that season's air-date lookup succeeded — so
    // consolidation never prunes against a season whose aired set is unknown.
    let aired = crate::tasks::aired_episodes(&app.tmdb_client, tmdb_id, today)
        .await
        .pairs;
    consolidate_show_seasons(app, tmdb_id, group_hashes, &owned, &aired, idle_window).await
}

/// The post-aired-lookup body of [`try_consolidate_show`]: given the already-resolved owned records
/// and the show's aired-episode set, consolidate each scattered season into a full-season cached
/// pack. Split out from `try_consolidate_show` so the destructive staging → record → repoint → prune
/// orchestration is unit-testable WITHOUT the live TMDB aired-episode lookup (which the wrapper does,
/// and which is exercised by the live smoke). Behaviour is identical to the previously-inlined body.
async fn consolidate_show_seasons(
    app: &AppState,
    tmdb_id: u64,
    group_hashes: &[String],
    owned: &[(String, OwnedRecord)],
    aired: &[(u32, u32)],
    idle_window: Duration,
) -> Result<(), UpgradeSkip> {
    // A representative request (for imdb id + metadata): prefer an owned record that actually has a
    // non-empty IMDB id (a mixed group's first record may be an empty-imdb mirror — see scrape_sample).
    let Some(sample) = scrape_sample(owned) else {
        return Err(UpgradeSkip::NoChange("no owned records".into()));
    };

    // Seasons currently held as SCATTERED single-episode torrents (provides.len()==1).
    let mut seasons: Vec<u32> = owned
        .iter()
        .filter(|(_, r)| r.provides.len() == 1)
        .map(|(_, r)| r.provides[0].0)
        .collect();
    seasons.sort_unstable();
    seasons.dedup();

    // We have scattered episodes to consolidate but TMDB returned no aired episodes — almost
    // certainly a transient air-date lookup failure (`aired_episodes` swallows TMDB errors as empty).
    // Defer rather than stamp the cursor (which would skip this show for a full cursor wrap on a daily
    // job), mirroring the movie path's transient-failure handling.
    if !seasons.is_empty() && aired.is_empty() {
        return Err(UpgradeSkip::Deferred(
            "aired-episode lookup empty (likely transient TMDB failure)".into(),
        ));
    }

    // Fix B: hoist provenance scan outside the per-season loop (one DB scan per title, not per season).
    let prov = base_req_provenance(app, MediaType::Show, tmdb_id).await;
    for season in seasons {
        let season_aired = crate::tasks::season_aired(aired, season);
        if season_aired.is_empty() {
            continue;
        }
        // Already a full-season pack owned for this season? (any hash whose provides covers it)
        let already_pack = owned.iter().any(|(_, r)| {
            let eps: Vec<u32> = r
                .provides
                .iter()
                .filter(|(s, _)| *s == season)
                .map(|(_, e)| *e)
                .collect();
            season_aired.iter().all(|e| eps.contains(e)) && r.provides.len() > 1
        });
        if already_pack {
            continue;
        }

        // Consolidation only makes sense when there are ≥2 owned records to MERGE for this season.
        // With a single owned record (e.g. a one-aired-episode season held as a singleton), adopting
        // an equal-quality single-episode "pack" is a lateral move, not a consolidation — and because
        // the no-regression gate is `>=` (not a strict gain), two equal cached releases would swap
        // A→B→A every daily tick (add + 4 MB probe + delete + selection churn → spurious Jellyfin
        // re-notify). Require genuine scatter to merge. (Once episodes accumulate into ≥2 records the
        // season is consolidated; a single record is left for the acquisition path to fill.)
        if !season_has_scatter(owned, season) {
            continue;
        }

        // Per-episode owned quality across ALL records that supply an episode of this season (the
        // prune below supersedes any of them). Unknown quality on any contributor → skip the season
        // (can't prove the candidate pack isn't a regression).
        let Some(season_owned_q) = season_owned_quality(owned, season) else {
            debug!(
                "consolidate: tmdb {} s{} skipped (an owned record has unknown quality)",
                tmdb_id, season
            );
            continue;
        };

        // Scrape the season (episode 1 query returns season packs too).
        let raws = match app
            .scraper
            .find(
                &sample.request.imdb_id,
                MediaKind::Series,
                Some(season),
                Some(1),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // A scrape failure is transient → Deferred (don't stamp the cursor), matching the
                // movie path. Earlier seasons already consolidated this tick are durable; remaining
                // seasons are retried next tick (an already-consolidated season skips via already_pack).
                return Err(UpgradeSkip::Deferred(format!(
                    "s{season} scrape failed: {e}"
                )));
            }
        };
        // Try the CACHED, in-ceiling, not-owned candidate packs BEST-SCORE-FIRST (the hard filters —
        // resolution ceiling, cam/telesync, dead seeders — are applied by `score()` inside the helper:
        // a pack above the ceiling would pass the no-regression check yet violate it, so it's excluded).
        for r in rank_cached_pack_candidates(&raws, &app.config.acquisition.prefs, group_hashes) {
            if app
                .store
                .is_blacklisted(MediaKind::Series, tmdb_id, r.info_hash.clone())
                .await
            {
                continue;
            }

            // Stage: add + resolve + SE-map files.
            let magnet = format!("magnet:?xt=urn:btih:{}", r.info_hash);
            let Ok(added) = app.provider.add_magnet(&magnet).await else {
                continue;
            };
            let Ok(info) = app.provider.get_torrent_info(&added.id).await else {
                let _ = app.provider.delete_torrent(&added.id).await;
                continue;
            };
            // Select all videos so the pack is fully available. RD reports a cached torrent as
            // `downloaded` only AFTER selection, so the cached gate must come after select + poll
            // (gating on `info.status` here would reject every cached pack on RD).
            let ids: Vec<u32> = info
                .files
                .iter()
                .filter(|f| crate::vfs::is_video_file(&f.path))
                .map(|f| f.id)
                .collect();
            if !ids.is_empty() {
                if let Err(e) = app
                    .provider
                    .select_files(
                        &added.id,
                        &ids.iter()
                            .map(|i| i.to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    )
                    .await
                {
                    warn!(
                        "consolidate: select_files for staged pack {} failed: {}",
                        r.info_hash, e
                    );
                }
            }
            // Poll for the cached verdict (RD lags select→downloaded by a moment), then gate: never
            // adopt an uncached pack (we never speculatively download a consolidation).
            let fresh = await_downloaded(app.provider.as_ref(), &added.id)
                .await
                .unwrap_or(info);
            if fresh.status != "downloaded" {
                let _ = app.provider.delete_torrent(&added.id).await;
                continue;
            }
            let eps = crate::acquire::episode_files(&fresh); // (s,e,path) — pub(crate) from Task 6
                                                             // Fix A: consolidation adopts a single-season pack only. A multi-season ("complete series") pack
                                                             // would leave its other seasons recorded-as-owned but un-repointed/un-pruned — reject it so
                                                             // provides/selection/prune stay consistent for the target season.
            if eps.iter().any(|(s, _, _)| *s != season) {
                let _ = app.provider.delete_torrent(&added.id).await;
                continue;
            }
            let pack_episodes: Vec<u32> = eps
                .iter()
                .filter(|(s, _, _)| *s == season)
                .map(|(_, e, _)| *e)
                .collect();

            let input = ConsolidationInput {
                season,
                aired_episodes: season_aired.clone(),
                owned_episode_quality: season_owned_q.clone(),
                pack_cached: true,
                pack_episodes: {
                    let mut v = pack_episodes.clone();
                    v.sort_unstable();
                    v.dedup();
                    v
                },
                pack_quality: QualitySummary::of(&r, &app.config.acquisition.prefs),
            };
            if !consolidation_target(&input) {
                let _ = app.provider.delete_torrent(&added.id).await; // non-destructive: drop the staged pack
                continue;
            }
            // Validate the show identity on a representative episode file, then apply the SAME
            // probe gate as acquisition (I-1) on that representative episode before adopting.
            if let Some((es, ee, path)) = eps.iter().find(|(s, _, _)| *s == season) {
                let fname = path.rsplit('/').next().unwrap_or(path).to_string();
                if !app
                    .engine
                    .validate_title(&fname, tmdb_id, MediaKind::Series, Some(*es), Some(*ee))
                    .await
                {
                    let _ = app.provider.delete_torrent(&added.id).await;
                    let _ = app
                        .store
                        .blacklist_add(
                            MediaKind::Series,
                            tmdb_id,
                            r.info_hash.clone(),
                            "WrongTitle",
                            now_secs(),
                        )
                        .await;
                    continue;
                }
                // Per-episode request so the probe's language check uses the right original_language.
                let probe_req = crate::store::AcquireRequest {
                    season: Some(*es),
                    episode: Some(*ee),
                    ..sample.request.clone()
                };
                match app
                    .engine
                    .probe_file(&fresh, &r.info_hash, path, &probe_req)
                    .await
                {
                    crate::acquire::VerifyResult::Pass | crate::acquire::VerifyResult::Accept => {}
                    crate::acquire::VerifyResult::Reject(reason) => {
                        let _ = app.provider.delete_torrent(&added.id).await;
                        let _ = app
                            .store
                            .blacklist_add(
                                MediaKind::Series,
                                tmdb_id,
                                r.info_hash.clone(),
                                reason,
                                now_secs(),
                            )
                            .await;
                        continue;
                    }
                    crate::acquire::VerifyResult::Defer => {
                        // Transient probe failure: drop the staged pack and retry next tick (no blacklist).
                        let _ = app.provider.delete_torrent(&added.id).await;
                        continue;
                    }
                }
            }
            // Fetch the provider listing for the prune BEFORE the idle gate, so there is NO network
            // round-trip between the idle check and the destructive repoint+prune below — a stream
            // starting in that window could otherwise have its old episode torrent deleted out from
            // under it. One wasted listing on a deferred tick is negligible for a daily job.
            // Distinguish a FETCH FAILURE from an empty account: an `unwrap_or_default()` empty would
            // make `prune_owned_hash_in` drop the old episode records while orphaning their still-
            // present torrents (re-adopted as duplicates). On `Err`, drop the staged pack (nothing is
            // recorded yet) and defer — mirrors the idle-defer below.
            let listing = match app.provider.get_torrents().await {
                Ok(l) => l,
                Err(_) => {
                    let _ = app.provider.delete_torrent(&added.id).await;
                    return Err(UpgradeSkip::Deferred(format!(
                        "s{season} provider listing unavailable; dropped staged pack"
                    )));
                }
            };
            // Idle gate. If the library is active, drop the staged pack (no dangling stage) and
            // retry on a later tick; consolidation re-stages cheaply (the pack is cached).
            if !app.read_activity.all_idle(idle_window).await {
                let _ = app.provider.delete_torrent(&added.id).await;
                return Err(UpgradeSkip::Deferred(format!(
                    "s{season} library active; dropped staged pack"
                )));
            }
            // Record the pack owned+verified with full-season provides + sticky provenance.
            let provides: Vec<(u32, u32)> = eps.iter().map(|(s, e, _)| (*s, *e)).collect();
            let _ = app
                .store
                .put_owned(
                    r.info_hash.clone(),
                    OwnedRecord {
                        request: crate::store::AcquireRequest {
                            season: Some(season),
                            episode: Some(1),
                            ..sample.request.clone()
                        }, // episode: Some(1) is a sentinel — the authoritative full-season set is in `provides`.
                        provenance: prov.clone(),
                        added_at: now_secs(),
                        status: OwnedStatus::Verified,
                        provides: provides.clone(),
                        quality: Some(QualitySummary::of(&r, &app.config.acquisition.prefs)),
                    },
                )
                .await;
            let _ = app
                .store
                .put_authoritative(r.info_hash.clone(), sample.request.metadata.clone())
                .await;
            // Repoint every episode slot of this season to the pack. If any repoint fails to
            // persist, DEFER the prune below: deleting a scattered episode whose slot still points at
            // the old (about-to-be-deleted) hash would break that episode's playback. The pack is
            // already recorded owned, so nothing is lost — a later consolidation re-runs the
            // idempotent repoint, and the duplicate-dedup pass reclaims the now-redundant scattered
            // episodes either way. (A put_selection failure is a rare redb-write/disk error.)
            let mut repoint_ok = true;
            for (s, e, path) in &eps {
                if *s != season {
                    continue;
                }
                if app
                    .store
                    .put_selection(
                        crate::store::episode_slot(tmdb_id, *s, *e),
                        crate::store::SelectionEntry {
                            hash: r.info_hash.clone(),
                            file_path: path.clone(),
                        },
                    )
                    .await
                    .is_err()
                {
                    repoint_ok = false;
                }
            }
            if !repoint_ok {
                return Err(UpgradeSkip::Deferred(format!(
                    "s{season} selection repoint failed; deferring prune"
                )));
            }
            // Prune every owned hash this new pack fully supersedes for the season — scattered
            // single episodes AND any smaller/older pack (e.g. a prior full-season pack that didn't
            // cover a newly-aired episode). M-2 safety: prune a hash ONLY when EVERY episode it
            // provides is for this season AND covered by the new pack, so no held episode is left
            // with an un-repointed selection. (The provider listing was fetched once at the top of
            // this consolidation block, before the idle gate — reused here for all prunes.)
            let pack_eps_for_season: std::collections::HashSet<u32> = eps
                .iter()
                .filter(|(s, _, _)| *s == season)
                .map(|(_, e, _)| *e)
                .collect();
            for (h, rec) in owned {
                if h.eq_ignore_ascii_case(&r.info_hash) {
                    continue; // never prune the pack we just adopted
                }
                let superseded = !rec.provides.is_empty()
                    && rec
                        .provides
                        .iter()
                        .all(|(s, e)| *s == season && pack_eps_for_season.contains(e));
                if superseded {
                    prune_owned_hash_in(app, h, &listing).await;
                }
            }
            info!(
                "consolidate: tmdb {} season {} -> pack {}",
                tmdb_id, season, r.info_hash
            );
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
    use crate::probe::{ProbeError, Track, TrackKind};
    use crate::provider::{DebridProvider, MockProvider};
    use crate::rd_client::{AddMagnetResponse, Torrent, TorrentFile, TorrentInfo};
    use crate::release::RawCandidate;
    use crate::repair::RepairManager;
    use crate::scraper::{MockScraper, Scraper};
    use crate::store::{AcquireRequest, Provenance, SelectionEntry, Store};
    use crate::tmdb_client::TmdbClient;
    use crate::vfs::{DebridVfs, MediaMetadata, MediaType};
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn mem_store() -> Store {
        Store::from_database(Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap()
    }

    fn movie_meta() -> MediaMetadata {
        MediaMetadata {
            title: "M".into(),
            year: Some("2020".into()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:27205".into()),
        }
    }
    fn movie_req() -> AcquireRequest {
        AcquireRequest {
            imdb_id: "tt1".into(),
            tmdb_id: 27205,
            kind: MediaKind::Movie,
            season: None,
            episode: None,
            original_language: Some("eng".into()),
            metadata: movie_meta(),
        }
    }
    /// A cached REMUX 1080p candidate (a meaningful upgrade over a cached WEB 1080p).
    fn remux_candidate() -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "M.2020.1080p.BluRay.REMUX.x265\nRD+".into(),
            info_hash: "hnew".into(),
            file_idx: Some(0),
            file_name: Some("M.2020.1080p.REMUX.mkv".into()),
        }
    }
    /// A cached WEB 1080p candidate — same tier and resolution as the owned "hold" record.
    /// Not a meaningful upgrade; used to verify the no-churn guard.
    fn web_1080_candidate() -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "M.2020.1080p.WEB-DL.x265\nRD+".into(),
            info_hash: "hweb".into(),
            file_idx: Some(0),
            file_name: Some("M.2020.1080p.WEB-DL.mkv".into()),
        }
    }

    /// Deterministic title validator for the upgrade flow tests — title validation has its own
    /// coverage in `acquire.rs`; here we want the upgrade staging/swap/prune logic under test,
    /// so we never hit the network-backed `TmdbTitleValidator`.
    struct PassValidator;
    #[async_trait]
    impl crate::acquire::TitleValidator for PassValidator {
        async fn validate(
            &self,
            _f: &str,
            _t: u64,
            _k: MediaKind,
            _s: Option<u32>,
            _e: Option<u32>,
        ) -> bool {
            true
        }
    }

    /// A scraper that always errors — exercises the transient "scrape failed" → `Deferred` path
    /// (the title must NOT advance the round-robin cursor on a transient failure).
    struct FailScraper;
    #[async_trait]
    impl Scraper for FailScraper {
        async fn find(
            &self,
            _i: &str,
            _k: MediaKind,
            _s: Option<u32>,
            _e: Option<u32>,
        ) -> Result<Vec<RawCandidate>, crate::error::AppError> {
            Err(crate::error::AppError::Config("scrape boom".into()))
        }
    }

    /// Test prober with a canned result (mirrors `acquire.rs`'s `CannedProber`). The upgrade probe
    /// gate fetches from a real CDN in production; in tests we feed it a fixed track set / error.
    struct CannedProber(Result<Vec<Track>, ProbeError>);
    #[async_trait]
    impl crate::acquire::Prober for CannedProber {
        async fn probe(&self, _url: &str) -> Result<Vec<Track>, ProbeError> {
            self.0.clone()
        }
    }

    /// Default app: a PASSING prober (Unsupported → `VerifyResult::Accept`) so the staging probe
    /// gate is satisfied without reaching a real CDN.
    fn app_with(
        scraper: Arc<dyn Scraper>,
        provider: Arc<dyn DebridProvider>,
        store: Store,
    ) -> AppState {
        let prober: Arc<dyn crate::acquire::Prober> =
            Arc::new(CannedProber(Err(ProbeError::Unsupported)));
        app_with_prober(scraper, provider, store, prober)
    }

    fn app_with_prober(
        scraper: Arc<dyn Scraper>,
        provider: Arc<dyn DebridProvider>,
        store: Store,
        prober: Arc<dyn crate::acquire::Prober>,
    ) -> AppState {
        let mut config =
            Config::from_parts(None, Some("tb".into()), Some("k".into()), None, None, None)
                .unwrap();
        config.acquisition = AcquisitionConfig::default();
        let tmdb = Arc::new(TmdbClient::new("k".into()).unwrap());
        let validator: Arc<dyn crate::acquire::TitleValidator> = Arc::new(PassValidator);
        let engine = Arc::new(crate::acquire::AcquisitionEngine::new(
            provider.clone(),
            scraper.clone(),
            validator,
            prober,
            store.clone(),
            config.acquisition.prefs.clone(),
            5,
            Duration::from_secs(1800),
            Duration::from_secs(600),
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
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();

        // Scraper offers a cached REMUX (higher tier → meaningful upgrade).
        let scraper = Arc::new(MockScraper {
            candidates: vec![remux_candidate()],
        });
        // Provider: both torrents listed; the new one resolves cached/downloaded with a video file.
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![
                Torrent {
                    id: "told".into(),
                    hash: "hold".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "tnew".into(),
                    hash: "hnew".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
            ],
            add_magnet: Some(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(),
                hash: "hnew".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
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
        assert_eq!(
            sel.hash, "hnew",
            "selection swapped to the upgraded release"
        );
        assert!(
            store.get_owned("hnew".into()).await.is_some(),
            "new release recorded owned+verified"
        );
        assert!(
            store.get_owned("hold".into()).await.is_none(),
            "old release pruned from owned"
        );
        assert!(
            deleted.lock().unwrap().contains(&"told".to_string()),
            "old torrent deleted from provider"
        );
    }

    #[tokio::test]
    async fn movie_upgrade_uses_imdb_from_any_group_record_not_just_representative() {
        // Mixed group: the lexicographically-FIRST hash ("amirror") is an account-mirror record
        // with an EMPTY imdb id (its id never resolved); a later hash ("zengine") carries the real
        // id. The representative used to gate upgrade eligibility is the first hash, so the old code
        // skipped the whole title ("no imdb id") and it was PERMANENTLY un-upgradeable. The fix
        // resolves the scrape id from ANY owned copy, so the upgrade proceeds.
        let store = mem_store();
        let web_q = || {
            Some(QualitySummary {
                cached: true,
                source_tier: 3_000,
                resolution: 1080,
                score: 10,
            })
        };
        store
            .put_owned(
                "amirror".into(),
                OwnedRecord {
                    request: AcquireRequest {
                        imdb_id: "".into(), // mirror record: id never resolved
                        ..movie_req()
                    },
                    provenance: Provenance { entries: vec![] }, // empty provenance = account mirror
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: web_q(),
                },
            )
            .await
            .unwrap();
        store
            .put_owned(
                "zengine".into(),
                OwnedRecord {
                    request: movie_req(), // imdb_id "tt1"
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: web_q(),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "amirror".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();

        let scraper = Arc::new(MockScraper {
            candidates: vec![remux_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![
                Torrent {
                    id: "tmir".into(),
                    hash: "amirror".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "teng".into(),
                    hash: "zengine".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "tnew".into(),
                    hash: "hnew".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
            ],
            add_magnet: Some(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(),
                hash: "hnew".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        run_upgrade_once(&app).await;

        let sel = store.get_selection(movie_slot(27205)).await.unwrap();
        assert_eq!(
            sel.hash, "hnew",
            "upgrade proceeds using the imdb id from a non-representative owned copy"
        );
        assert!(
            store.get_owned("hnew".into()).await.is_some(),
            "staged release is recorded owned"
        );
        assert_eq!(
            store
                .get_owned("hnew".into())
                .await
                .unwrap()
                .request
                .imdb_id,
            "tt1",
            "staged record carries the group-resolved imdb id"
        );
    }

    #[test]
    fn scrape_sample_prefers_a_record_with_an_imdb_id() {
        let with_id = |hash: &str, imdb: &str| {
            (
                hash.to_string(),
                OwnedRecord {
                    request: AcquireRequest {
                        imdb_id: imdb.into(),
                        ..movie_req()
                    },
                    provenance: Provenance { entries: vec![] },
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                },
            )
        };
        // First record has no id; a later one does → that later one is chosen.
        let owned = vec![with_id("a", ""), with_id("z", "tt7")];
        assert_eq!(scrape_sample(&owned).unwrap().request.imdb_id, "tt7");
        // None have an id → falls back to the first (so the no-imdb skip still fires cleanly).
        let owned = vec![with_id("a", ""), with_id("z", "")];
        assert_eq!(scrape_sample(&owned).unwrap().request.imdb_id, "");
        // Empty group → None.
        assert!(scrape_sample(&[]).is_none());
    }

    #[tokio::test]
    async fn movie_upgrade_rolls_back_staged_candidate_when_listing_unavailable() {
        // After staging the upgrade, if the provider listing for the prune is UNAVAILABLE, the engine
        // must ROLL BACK the freshly-staged candidate (delete it + drop its records) and DEFER — never
        // prune (which would treat the still-present old torrent as gone) and never leave an orphaned
        // present-but-untracked torrent (re-adopted as a duplicate). Cursor stays unstamped.
        let store = mem_store();
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();
        let scraper = Arc::new(MockScraper {
            candidates: vec![remux_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            // get_torrents is only called AFTER staging (for the prune listing) → fail it there.
            fail_get_torrents: true,
            add_magnet: Some(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(),
                hash: "hnew".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        run_upgrade_once(&app).await;

        assert!(
            store.get_owned("hnew".into()).await.is_none(),
            "the staged record must be rolled back when the listing is unavailable"
        );
        assert!(
            deleted.lock().unwrap().contains(&"tnew".to_string()),
            "the staged torrent must be deleted on rollback"
        );
        assert!(
            store.get_owned("hold".into()).await.is_some(),
            "the current release must be untouched"
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold",
            "the selection must NOT be swapped (no prune happened)"
        );
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 27205).await,
            0,
            "a deferred title keeps its cursor unstamped"
        );
    }

    #[tokio::test]
    async fn movie_upgrade_keeps_old_record_when_prune_delete_fails() {
        // The swap succeeds (selection → new hash), but pruning the old torrent's delete FAILS. The
        // old owned record must be KEPT (not dropped) so a later tick retries — otherwise a
        // present-but-untracked old torrent would be re-adopted by record_mirror_owned as a DUPLICATE.
        let store = mem_store();
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();
        let scraper = Arc::new(MockScraper {
            candidates: vec![remux_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![
                Torrent {
                    id: "told".into(),
                    hash: "hold".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "tnew".into(),
                    hash: "hnew".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
            ],
            add_magnet: Some(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(),
                hash: "hnew".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            fail_delete: true, // the prune's delete_torrent fails
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        run_upgrade_once(&app).await;

        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hnew",
            "the selection still swaps to the upgraded release"
        );
        assert!(
            deleted.lock().unwrap().contains(&"told".to_string()),
            "the prune must ATTEMPT to delete the old torrent"
        );
        assert!(
            store.get_owned("hold".into()).await.is_some(),
            "on a failed delete the old owned record is KEPT for retry (no orphan re-adoption)"
        );
    }

    /// Provider that touches read-activity inside `get_torrent_info` — simulating a playback read that
    /// begins DURING staging (after the outer idle gate passed). Used to trip the 4b idle re-check.
    #[derive(Debug)]
    struct TouchDuringStageProvider {
        ra: Arc<crate::read_activity::ReadActivity>,
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl DebridProvider for TouchDuringStageProvider {
        fn name(&self) -> &'static str {
            "touchstage"
        }
        async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
            Ok(vec![
                Torrent {
                    id: "told".into(),
                    hash: "hold".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "tnew".into(),
                    hash: "hnew".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
            ])
        }
        async fn get_torrent_info(&self, _id: &str) -> Result<TorrentInfo, reqwest::Error> {
            // A read begins mid-stage → the 4b idle re-check must trip and force a full rollback.
            self.ra.touch("Movies/something.mkv").await;
            Ok(TorrentInfo {
                id: "tnew".into(),
                hash: "hnew".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            })
        }
        async fn add_magnet(&self, _m: &str) -> Result<AddMagnetResponse, reqwest::Error> {
            Ok(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            })
        }
        async fn select_files(&self, _t: &str, _f: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn delete_torrent(&self, t: &str) -> Result<(), reqwest::Error> {
            self.deleted.lock().unwrap().push(t.to_string());
            Ok(())
        }
        async fn resolve_url(
            &self,
            _l: &crate::provider::FileLocator,
        ) -> Result<String, crate::error::AppError> {
            Ok("https://cdn/new".into())
        }
        async fn invalidate(&self, _l: &crate::provider::FileLocator) {}
    }

    #[tokio::test]
    async fn movie_upgrade_rolls_back_when_library_goes_active_during_staging() {
        // The 4b idle re-check: the outer/inner idle gates pass, the candidate is fully staged, then a
        // read begins mid-stage. The engine must ROLL BACK the staged candidate (delete + drop
        // records) and DEFER — never prune the old torrent out from under the new read. (Distinct from
        // the outer-gate `active_movie_is_not_pruned` test, where nothing is ever staged.)
        let store = mem_store();
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();
        let scraper: Arc<dyn Scraper> = Arc::new(MockScraper {
            candidates: vec![remux_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ra = Arc::new(crate::read_activity::ReadActivity::new());
        let provider: Arc<dyn DebridProvider> = Arc::new(TouchDuringStageProvider {
            ra: ra.clone(),
            deleted: deleted.clone(),
        });
        // Build the AppState manually so the provider and the engine SHARE `ra` — the mid-stage touch
        // must be visible to run_upgrade_once's idle gates.
        let mut config =
            Config::from_parts(None, Some("tb".into()), Some("k".into()), None, None, None)
                .unwrap();
        config.acquisition = AcquisitionConfig::default();
        let validator: Arc<dyn crate::acquire::TitleValidator> = Arc::new(PassValidator);
        let prober: Arc<dyn crate::acquire::Prober> =
            Arc::new(CannedProber(Err(ProbeError::Unsupported)));
        let engine = Arc::new(crate::acquire::AcquisitionEngine::new(
            provider.clone(),
            scraper.clone(),
            validator,
            prober,
            store.clone(),
            config.acquisition.prefs.clone(),
            5,
            Duration::from_secs(1800),
            Duration::from_secs(600),
        ));
        let app = AppState {
            provider: provider.clone(),
            tmdb_client: Arc::new(TmdbClient::new("k".into()).unwrap()),
            vfs: Arc::new(RwLock::new(DebridVfs::new())),
            store: store.clone(),
            repair_manager: Arc::new(RepairManager::new(provider)),
            config: Arc::new(config),
            jellyfin_client: None,
            http_client: reqwest::Client::new(),
            scraper,
            engine,
            trakt_client: None,
            read_activity: ra.clone(),
        };
        run_upgrade_once(&app).await;

        assert!(
            deleted.lock().unwrap().contains(&"tnew".to_string()),
            "the staged candidate must be deleted on the 4b rollback"
        );
        assert!(
            store.get_owned("hnew".into()).await.is_none(),
            "the staged record must be dropped on rollback"
        );
        assert!(
            store.get_owned("hold".into()).await.is_some(),
            "the current release must be untouched"
        );
        assert!(
            !deleted.lock().unwrap().contains(&"told".to_string()),
            "the old torrent must NOT be pruned (the read must not be interrupted)"
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold",
            "the selection must NOT be swapped"
        );
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 27205).await,
            0,
            "a title deferred mid-stage keeps its cursor unstamped"
        );
    }

    /// Simulates Real-Debrid's add→select→downloaded lifecycle: a freshly-added (even cached)
    /// torrent reports `waiting_files_selection` with its file list already populated, and flips to
    /// `downloaded` only AFTER `select_files` is called. The pre-fix ordering (gate on status
    /// before selecting) rejects every candidate against this provider; the fixed ordering adopts
    /// it. This is a regression guard for the upgrade engine being silently inert on RD.
    #[derive(Debug)]
    struct RdLifecycleProvider {
        selected: std::sync::atomic::AtomicBool,
        listing: Vec<Torrent>,
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl DebridProvider for RdLifecycleProvider {
        fn name(&self) -> &'static str {
            "rd-lifecycle"
        }
        async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
            Ok(self.listing.clone())
        }
        async fn get_torrent_info(&self, _id: &str) -> Result<TorrentInfo, reqwest::Error> {
            let sel = self.selected.load(std::sync::atomic::Ordering::SeqCst);
            Ok(TorrentInfo {
                id: "tnew".into(),
                hash: "hnew".into(),
                status: if sel {
                    "downloaded".into()
                } else {
                    "waiting_files_selection".into()
                },
                files: vec![TorrentFile {
                    id: 0,
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: if sel { 1 } else { 0 },
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            })
        }
        async fn add_magnet(&self, _m: &str) -> Result<AddMagnetResponse, reqwest::Error> {
            Ok(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            })
        }
        async fn select_files(&self, _t: &str, _f: &str) -> Result<(), reqwest::Error> {
            self.selected
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn delete_torrent(&self, t: &str) -> Result<(), reqwest::Error> {
            self.deleted.lock().unwrap().push(t.to_string());
            Ok(())
        }
        async fn resolve_url(
            &self,
            _l: &crate::provider::FileLocator,
        ) -> Result<String, crate::error::AppError> {
            Ok("https://cdn/new".into())
        }
        async fn invalidate(&self, _l: &crate::provider::FileLocator) {}
    }

    #[tokio::test]
    async fn rd_lifecycle_movie_upgrade_selects_before_cached_gate() {
        let store = mem_store();
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();

        let scraper = Arc::new(MockScraper {
            candidates: vec![remux_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(RdLifecycleProvider {
            selected: std::sync::atomic::AtomicBool::new(false),
            listing: vec![
                Torrent {
                    id: "told".into(),
                    hash: "hold".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "tnew".into(),
                    hash: "hnew".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
            ],
            deleted: deleted.clone(),
        });
        let app = app_with(scraper, provider, store.clone());

        run_upgrade_once(&app).await;

        let sel = store.get_selection(movie_slot(27205)).await.unwrap();
        assert_eq!(
            sel.hash, "hnew",
            "upgrade must select files before gating on `downloaded`, so the cached REMUX is \
             adopted even though RD reports `waiting_files_selection` until selection"
        );
    }

    #[tokio::test]
    async fn upgrade_blocked_when_probe_rejects() {
        let store = mem_store();
        // Owned: cached WEB 1080p movie, Verified, with a selection pointing at it.
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();

        // Scraper offers a cached REMUX (a meaningful upgrade that passes scoring + title-validation).
        let scraper = Arc::new(MockScraper {
            candidates: vec![remux_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![
                Torrent {
                    id: "told".into(),
                    hash: "hold".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "tnew".into(),
                    hash: "hnew".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
            ],
            add_magnet: Some(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(),
                hash: "hnew".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        // Prober returns a WRONG-language audio track. movie_req()'s original_language is "eng" and
        // AcquisitionConfig::default() uses AudioReq::Original, so an "ita"-only audio set is a
        // positive language violation → probe::verify FailAudio → VerifyResult::Reject("BadAudio").
        let prober: Arc<dyn crate::acquire::Prober> = Arc::new(CannedProber(Ok(vec![Track {
            kind: TrackKind::Audio,
            language: Some("ita".into()),
        }])));
        let app = app_with_prober(scraper, provider, store.clone(), prober);

        // Idle (slot never read) ⇒ staging runs; the probe gate must reject it.
        run_upgrade_once(&app).await;

        // No swap: selection unchanged, the rejected release is NOT owned, and the OLD torrent is
        // NOT pruned. The rejected hash IS blacklisted (so it is never re-tried).
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold",
            "selection must not swap on probe reject"
        );
        assert!(
            store.get_owned("hnew".into()).await.is_none(),
            "probe-rejected release must not be recorded owned"
        );
        assert!(
            !deleted.lock().unwrap().contains(&"told".to_string()),
            "old torrent must not be pruned when the upgrade is blocked"
        );
        assert!(
            store
                .is_blacklisted(MediaKind::Movie, 27205, "hnew".into())
                .await,
            "probe-rejected hash must be blacklisted"
        );
        assert!(
            deleted.lock().unwrap().contains(&"tnew".to_string()),
            "staged torrent must be cleaned up after probe reject"
        );
    }

    #[tokio::test]
    async fn active_movie_is_not_pruned() {
        let store = mem_store();
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();
        let scraper = Arc::new(MockScraper {
            candidates: vec![remux_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "told".into(),
                hash: "hold".into(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            add_magnet: Some(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(),
                hash: "hnew".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "new.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        // The idle gate is library-wide: a recent read anywhere defers all swaps this tick.
        app.read_activity.touch("Movies/anything.mkv").await;

        run_upgrade_once(&app).await;

        // Nothing pruned, nothing staged, selection unchanged — the upgrade was deferred.
        assert!(
            deleted.lock().unwrap().is_empty(),
            "active library must not be pruned"
        );
        assert!(
            store.get_owned("hnew".into()).await.is_none(),
            "no stage while active"
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold",
            "selection unchanged"
        );
        // The round-robin cursor must NOT advance for a title deferred because the library is active
        // — otherwise it would be marked "checked" and skipped for a full cursor wrap once idle.
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 27205).await,
            0,
            "an idle-deferred title must keep its cursor unstamped"
        );
    }

    #[tokio::test]
    async fn handler_defer_keeps_cursor_unstamped_even_when_library_idle() {
        // The library is IDLE (outer gate passes), but the handler defers (here: a transient scrape
        // failure). The cursor must stay unstamped so the title is reconsidered next tick, not
        // skipped for a full cursor wrap. (Before the Deferred/NoChange split this stamped the cursor.)
        let store = mem_store();
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "told".into(),
                hash: "hold".into(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let app = app_with(Arc::new(FailScraper), provider, store.clone());
        // No reads ⇒ library idle ⇒ the outer gate passes and the handler is actually invoked.
        run_upgrade_once(&app).await;
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 27205).await,
            0,
            "a handler-deferred (transient) title must keep its cursor unstamped"
        );
    }

    #[tokio::test]
    async fn no_meaningful_upgrade_stamps_the_cursor() {
        // A genuine "no upgrade exists" outcome (library idle, scraper offers only a same-quality
        // release) IS evaluated to completion → the cursor advances so the budget rotates onward.
        let store = mem_store();
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        let scraper = Arc::new(MockScraper {
            candidates: vec![web_1080_candidate()], // same tier+resolution → not a meaningful upgrade
        });
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "told".into(),
                hash: "hold".into(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        run_upgrade_once(&app).await;
        assert!(
            store.get_upgrade_checked(&MediaType::Movie, 27205).await > 0,
            "a fully-evaluated no-upgrade title must advance the cursor"
        );
        // And prove it advanced via the genuine no-upgrade path, NOT a staging failure: the candidate
        // ("hweb") must never have been staged/recorded, and the owned record is untouched.
        assert!(
            store.get_owned("hweb".into()).await.is_none(),
            "a same-quality candidate must NOT be staged"
        );
        assert_eq!(
            store.get_owned("hold".into()).await.unwrap().status,
            OwnedStatus::Verified,
            "the existing owned record must be untouched (no swap)"
        );
    }

    #[tokio::test]
    async fn round_robin_budget_processes_only_the_stalest_title() {
        // run_upgrade_once sorts owned titles by least-recently-checked (`get_upgrade_checked`) then
        // truncates to `budget_per_tick`. Every other upgrade test seeds exactly ONE title, so the
        // sort + truncate are no-ops. Here: THREE owned movies with DISTINCT seeded cursors and a
        // budget of 1 ⇒ only the STALEST (smallest cursor) is evaluated this tick; the other two keep
        // their pre-seeded cursors untouched. Each title's scrape returns a same-quality release
        // (genuine no-upgrade), so the evaluated title advances its cursor (→ now_secs(), far above the
        // seeded epochs) without staging anything — isolating the round-robin selection.
        let store = mem_store();
        let ids = [101u64, 202, 303];
        for &id in &ids {
            let mut req = movie_req();
            req.tmdb_id = id;
            req.metadata.external_id = Some(format!("tmdb:{id}"));
            store
                .put_owned(
                    format!("h{id}"),
                    OwnedRecord {
                        request: req,
                        provenance: Provenance::watchlist("a"),
                        added_at: 1,
                        status: OwnedStatus::Verified,
                        provides: vec![],
                        quality: Some(QualitySummary {
                            cached: true,
                            source_tier: 3_000,
                            resolution: 1080,
                            score: 10,
                        }),
                    },
                )
                .await
                .unwrap();
        }
        // Distinct seeded cursors: 303 is the STALEST (smallest), 101 the freshest.
        store
            .set_upgrade_checked(&MediaType::Movie, 101, 3_000)
            .await
            .unwrap();
        store
            .set_upgrade_checked(&MediaType::Movie, 202, 2_000)
            .await
            .unwrap();
        store
            .set_upgrade_checked(&MediaType::Movie, 303, 1_000)
            .await
            .unwrap();

        let scraper = Arc::new(MockScraper {
            candidates: vec![web_1080_candidate()], // same tier+resolution → no meaningful upgrade
        });
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![
                Torrent {
                    id: "t101".into(),
                    hash: "h101".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "t202".into(),
                    hash: "h202".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "t303".into(),
                    hash: "h303".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let mut app = app_with(scraper, provider, store.clone());
        // Budget of 1: only one (the stalest) title may be processed this tick. `app.config` is the
        // sole strong Arc reference at this point, so `get_mut` succeeds.
        Arc::get_mut(&mut app.config)
            .unwrap()
            .upgrade
            .budget_per_tick = 1;

        run_upgrade_once(&app).await;

        // Only the stalest (303) was evaluated → its cursor advanced past ALL seeded epochs.
        assert!(
            store.get_upgrade_checked(&MediaType::Movie, 303).await > 3_000,
            "the stalest title must be processed (cursor advanced to now_secs)"
        );
        // The other two were over budget this tick → cursors untouched.
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 202).await,
            2_000,
            "the second-stalest title must be left for a later tick (over budget)"
        );
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 101).await,
            3_000,
            "the freshest title must be left for a later tick (over budget)"
        );
    }

    #[tokio::test]
    async fn best_owned_quality_picks_best_copy_and_skips_unknown() {
        let store = mem_store();
        let rec = |tier: i64, score: i64| OwnedRecord {
            request: movie_req(),
            provenance: Provenance::watchlist("a"),
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![],
            quality: Some(QualitySummary {
                cached: true,
                source_tier: tier,
                resolution: 1080,
                score,
            }),
        };
        // "aweb" sorts BEFORE "zremux" — the old code baselined off the first (worse) copy.
        store
            .put_owned("aweb".into(), rec(3_000, 1_003_000))
            .await
            .unwrap();
        store
            .put_owned("zremux".into(), rec(8_000, 1_008_000))
            .await
            .unwrap();
        let app = app_with(
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(MockProvider::default()),
            store.clone(),
        );
        let best = best_owned_quality(&app, &["aweb".into(), "zremux".into()])
            .await
            .expect("both qualities known");
        assert_eq!(
            best.source_tier, 8_000,
            "baseline must be the BEST owned copy (REMUX), not the lexicographically-first (WEB)"
        );

        // Any unknown-quality owned copy → None (conservative skip — can't prove a candidate beats it).
        let mut unknown = rec(3_000, 1_003_000);
        unknown.quality = None;
        store.put_owned("bunknown".into(), unknown).await.unwrap();
        assert!(
            best_owned_quality(&app, &["aweb".into(), "bunknown".into(), "zremux".into()])
                .await
                .is_none(),
            "an unknown-quality copy forces a conservative skip"
        );
    }

    #[tokio::test]
    async fn no_meaningful_upgrade_is_a_noop() {
        let store = mem_store();
        // Owned: cached WEB 1080p movie, Verified, with a selection.
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();

        // Scraper returns only a same-tier same-resolution WEB 1080p candidate — not an upgrade.
        let scraper = Arc::new(MockScraper {
            candidates: vec![web_1080_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        run_upgrade_once(&app).await;

        assert!(
            deleted.lock().unwrap().is_empty(),
            "nothing pruned on no-upgrade"
        );
        assert!(
            store.get_owned("hweb".into()).await.is_none(),
            "no new owned record for same-tier candidate"
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold",
            "selection unchanged"
        );
    }

    #[tokio::test]
    async fn above_ceiling_cached_release_is_not_staged() {
        let store = mem_store();
        // Owned: cached WEB 1080p movie, Verified, with a selection.
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();

        // Scraper returns a cached 2160p REMUX — above the P1080 ceiling in AcquisitionConfig::default().
        let cand_4k = RawCandidate {
            name: "Torrentio\n2160p".into(),
            description: "M.2020.2160p.BluRay.REMUX.x265\nRD+".into(),
            info_hash: "h4k".into(),
            file_idx: Some(0),
            file_name: Some("M.2020.2160p.REMUX.mkv".into()),
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand_4k],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        run_upgrade_once(&app).await;

        // release::score returns None for 2160p when ceiling is P1080 → candidate is filtered out.
        assert!(
            deleted.lock().unwrap().is_empty(),
            "nothing staged/pruned when candidate is above ceiling"
        );
        assert!(
            store.get_owned("h4k".into()).await.is_none(),
            "no new owned record for above-ceiling candidate"
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold",
            "selection unchanged"
        );
    }

    // ── pure consolidation-decision tests (Task 10) ───────────────────────────

    #[test]
    fn full_cached_season_pack_no_regression_consolidates() {
        // Owned scattered: E01 (WEB 1080p), E02 (WEB 1080p). Pack: cached BluRay 1080p covering E01-E03.
        let input = ConsolidationInput {
            season: 1,
            aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![
                (
                    1,
                    crate::release::QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 1,
                    },
                ),
                (
                    2,
                    crate::release::QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 1,
                    },
                ),
            ],
            pack_cached: true,
            pack_episodes: vec![1, 2, 3],
            pack_quality: crate::release::QualitySummary {
                cached: true,
                source_tier: 6_000,
                resolution: 1080,
                score: 5,
            },
        };
        assert!(
            consolidation_target(&input),
            "cached full-season pack, no regression → consolidate"
        );
    }

    #[test]
    fn partial_season_pack_is_rejected() {
        let input = ConsolidationInput {
            season: 1,
            aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![(1, q1080_web()), (2, q1080_web())],
            pack_cached: true,
            pack_episodes: vec![1, 2], // missing E03
            pack_quality: q1080_bluray(),
        };
        assert!(
            !consolidation_target(&input),
            "partial-season pack must not consolidate"
        );
    }

    #[test]
    fn quality_regression_pack_is_rejected() {
        let input = ConsolidationInput {
            season: 1,
            aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![(1, q2160_remux()), (2, q1080_web())], // E01 is 2160 REMUX
            pack_cached: true,
            pack_episodes: vec![1, 2, 3],
            pack_quality: q1080_bluray(), // worse than E01 → regression
        };
        assert!(
            !consolidation_target(&input),
            "a pack worse than any owned episode must not consolidate"
        );
    }

    #[test]
    fn uncached_pack_is_rejected() {
        let input = ConsolidationInput {
            season: 1,
            aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![(1, q1080_web())],
            pack_cached: false,
            pack_episodes: vec![1, 2, 3],
            pack_quality: q1080_bluray(),
        };
        assert!(!consolidation_target(&input));
    }

    #[test]
    fn empty_aired_set_is_rejected() {
        let input = ConsolidationInput {
            season: 1,
            aired_episodes: vec![],
            owned_episode_quality: vec![],
            pack_cached: true,
            pack_episodes: vec![1, 2, 3],
            pack_quality: q1080_bluray(),
        };
        assert!(
            !consolidation_target(&input),
            "empty aired set must not consolidate"
        );
    }

    #[tokio::test]
    async fn mirror_movie_with_unknown_quality_is_not_upgraded() {
        let store = mem_store();
        // A mirror-style record: Verified, empty provenance, but quality UNKNOWN (None) — e.g. a
        // legacy record written before quality capture. The upgrade engine must NOT swap/prune it:
        // an unknown current quality compared against a cached candidate would otherwise treat ANY
        // cached release as an upgrade and could delete a better existing copy (regression).
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance { entries: vec![] },
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
                    hash: "hold".into(),
                    file_path: "old.mkv".into(),
                },
            )
            .await
            .unwrap();
        // A cached REMUX candidate that WOULD be a meaningful upgrade if the current quality were known.
        let scraper = Arc::new(MockScraper {
            candidates: vec![remux_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        run_upgrade_once(&app).await;

        assert!(
            deleted.lock().unwrap().is_empty(),
            "must not prune a record whose current quality is unknown"
        );
        assert!(
            store.get_owned("hnew".into()).await.is_none(),
            "must not stage an upgrade over an unknown current quality"
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold",
            "selection unchanged"
        );
    }

    #[test]
    fn season_owned_quality_includes_packs_and_flags_unknown() {
        let rec = |provides: Vec<(u32, u32)>, quality: Option<QualitySummary>| OwnedRecord {
            request: movie_req(),
            provenance: Provenance { entries: vec![] },
            added_at: 1,
            status: OwnedStatus::Verified,
            provides,
            quality,
        };
        // An 8-episode REMUX pack (E1..E8) plus scattered WEB singletons E9, E10.
        let owned = vec![
            (
                "hp".to_string(),
                rec((1..=8).map(|e| (1u32, e)).collect(), Some(q2160_remux())),
            ),
            ("h9".to_string(), rec(vec![(1, 9)], Some(q1080_web()))),
            ("h10".to_string(), rec(vec![(1, 10)], Some(q1080_web()))),
        ];
        let q = season_owned_quality(&owned, 1).expect("all qualities known");
        assert_eq!(
            q.len(),
            10,
            "every in-season episode (multi-episode pack + singletons) is represented"
        );
        assert!(
            q.iter()
                .any(|(e, sq)| *e == 1 && sq.source_tier == q2160_remux().source_tier),
            "the multi-episode REMUX pack's episodes must be included, not just singletons"
        );

        // A candidate WEB full-season pack would downgrade E1..E8 (REMUX) → must be rejected.
        let input = ConsolidationInput {
            season: 1,
            aired_episodes: (1..=10).collect(),
            owned_episode_quality: q,
            pack_cached: true,
            pack_episodes: (1..=10).collect(),
            pack_quality: q1080_web(),
        };
        assert!(
            !consolidation_target(&input),
            "a WEB pack must not supersede a held higher-quality REMUX pack"
        );

        // Unknown quality on any contributor → None (caller must skip the season conservatively).
        let mut owned2 = owned;
        owned2.push(("hx".to_string(), rec(vec![(1, 11)], None)));
        assert!(
            season_owned_quality(&owned2, 1).is_none(),
            "an unknown-quality contributor forces a conservative skip"
        );
    }

    #[test]
    fn rank_cached_pack_candidates_orders_by_score_and_drops_uncached_and_owned() {
        use crate::config::AcquisitionConfig;
        let prefs = AcquisitionConfig::default().prefs;
        let uncached = RawCandidate {
            name: "Torrentio".into(),
            description: "Show.S01.1080p.BluRay.x265".into(), // no RD+/⚡ → uncached → dropped
            info_hash: "hunc".into(),
            file_idx: None,
            file_name: None,
        };
        let owned_pack = RawCandidate {
            name: "Torrentio".into(),
            description: "Show.S01.1080p.WEB-DL.x265\nRD+".into(),
            info_hash: "howned".into(), // already owned → dropped
            file_idx: None,
            file_name: None,
        };
        // remux_candidate (REMUX, hnew) outscores web_1080_candidate (WEB, hweb); both cached.
        let raws = vec![
            web_1080_candidate(),
            remux_candidate(),
            uncached,
            owned_pack,
        ];
        let ranked = rank_cached_pack_candidates(&raws, &prefs, &["howned".into()]);
        let hashes: Vec<_> = ranked.iter().map(|r| r.info_hash.clone()).collect();
        assert_eq!(
            hashes,
            vec!["hnew", "hweb"],
            "best-score (REMUX) first, WEB second; uncached + already-owned excluded"
        );
    }

    #[test]
    fn season_has_scatter_requires_two_in_season_records() {
        let rec = |provides: Vec<(u32, u32)>| OwnedRecord {
            request: movie_req(),
            provenance: Provenance { entries: vec![] },
            added_at: 1,
            status: OwnedStatus::Verified,
            provides,
            quality: Some(q1080_web()),
        };
        // A single in-season record → no scatter to merge (consolidating it is a lateral A→B→A churn).
        let one = vec![("h1".to_string(), rec(vec![(1, 1)]))];
        assert!(
            !season_has_scatter(&one, 1),
            "one in-season record is not scatter — must not consolidate (avoids daily flip-flop)"
        );
        // Two scattered singletons of the same season → genuine scatter, consolidate.
        let two = vec![
            ("h1".to_string(), rec(vec![(1, 1)])),
            ("h2".to_string(), rec(vec![(1, 2)])),
        ];
        assert!(
            season_has_scatter(&two, 1),
            "two in-season records are scatter"
        );
        // Records of a DIFFERENT season don't count toward this season's scatter.
        let other = vec![
            ("h1".to_string(), rec(vec![(1, 1)])),
            ("h2".to_string(), rec(vec![(2, 1)])),
        ];
        assert!(
            !season_has_scatter(&other, 1),
            "a different season's record must not count toward this season's scatter"
        );
    }

    // ── show-consolidation orchestration tests (Task 10) ───────────────────────
    //
    // These exercise `try_consolidate_show`'s destructive multi-step body (stage cached full-season
    // pack → record owned with full-season `provides` → repoint episode `selection` slots → prune the
    // superseded scattered episode torrents) and its recovery branches. They call the extracted
    // `consolidate_show_seasons` directly with an injected aired-episode set, so they stay fully
    // deterministic and offline: the only thing `consolidate_show_seasons` does NOT cover is the thin
    // TMDB aired-episode lookup that the `try_consolidate_show` wrapper performs (covered by the live
    // smoke, not unit tests — `aired_episodes` is documented as such).

    const SHOW_TMDB: u64 = 1399;

    fn show_meta() -> MediaMetadata {
        MediaMetadata {
            title: "S".into(),
            year: Some("2019".into()),
            media_type: MediaType::Show,
            external_id: Some("tmdb:1399".into()),
        }
    }
    /// An owned per-episode record (a single `(season, episode)` in `provides`), Series kind.
    fn ep_record(season: u32, episode: u32, q: QualitySummary) -> OwnedRecord {
        OwnedRecord {
            request: AcquireRequest {
                imdb_id: "tt9".into(),
                tmdb_id: SHOW_TMDB,
                kind: MediaKind::Series,
                season: Some(season),
                episode: Some(episode),
                original_language: Some("eng".into()),
                metadata: show_meta(),
            },
            provenance: Provenance::watchlist("a"),
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![(season, episode)],
            quality: Some(q),
        }
    }
    /// A cached BluRay 1080p full-season pack candidate — no regression over the WEB 1080p singletons.
    fn season_pack_candidate() -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "S.2019.S01.1080p.BluRay.x265\nRD+".into(),
            info_hash: "hpack".into(),
            file_idx: None,
            file_name: None,
        }
    }
    /// The provider's view of the staged pack: two SELECTED season-1 episode videos.
    fn pack_info() -> TorrentInfo {
        TorrentInfo {
            id: "tpack".into(),
            hash: "hpack".into(),
            status: "downloaded".into(),
            files: vec![
                TorrentFile {
                    id: 0,
                    path: "S.S01E01.1080p.BluRay.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                },
                TorrentFile {
                    id: 1,
                    path: "S.S01E02.1080p.BluRay.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                },
            ],
            links: vec!["https://cdn/p1".into(), "https://cdn/p2".into()],
            ..Default::default()
        }
    }
    /// The torrents the provider lists during the prune: two scattered episodes + the staged pack.
    fn show_listing() -> Vec<Torrent> {
        vec![
            Torrent {
                id: "te1".into(),
                hash: "e1".into(),
                status: "downloaded".into(),
                ..Default::default()
            },
            Torrent {
                id: "te2".into(),
                hash: "e2".into(),
                status: "downloaded".into(),
                ..Default::default()
            },
            Torrent {
                id: "tpack".into(),
                hash: "hpack".into(),
                status: "downloaded".into(),
                ..Default::default()
            },
        ]
    }
    /// Seed two scattered WEB 1080p singletons (S01E01=e1, S01E02=e2) + their selection slots.
    async fn seed_two_scattered_episodes(store: &Store) {
        store
            .put_owned("e1".into(), ep_record(1, 1, q1080_web()))
            .await
            .unwrap();
        store
            .put_owned("e2".into(), ep_record(1, 2, q1080_web()))
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 1),
                SelectionEntry {
                    hash: "e1".into(),
                    file_path: "e1.mkv".into(),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 2),
                SelectionEntry {
                    hash: "e2".into(),
                    file_path: "e2.mkv".into(),
                },
            )
            .await
            .unwrap();
    }
    fn owned_two() -> Vec<(String, OwnedRecord)> {
        vec![
            ("e1".to_string(), ep_record(1, 1, q1080_web())),
            ("e2".to_string(), ep_record(1, 2, q1080_web())),
        ]
    }
    fn group_two() -> Vec<String> {
        vec!["e1".to_string(), "e2".to_string()]
    }

    #[tokio::test]
    async fn idle_show_with_cached_full_season_pack_consolidates_repoints_and_prunes() {
        let store = mem_store();
        seed_two_scattered_episodes(&store).await;

        let scraper = Arc::new(MockScraper {
            candidates: vec![season_pack_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: show_listing(),
            add_magnet: Some(AddMagnetResponse {
                id: "tpack".into(),
                uri: String::new(),
            }),
            torrent_info: Some(pack_info()),
            resolved_url: Some("https://cdn/p1".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        // S01 aired E01-E02 (deterministic, injected — no TMDB call). Library never read ⇒ idle.
        let res = consolidate_show_seasons(
            &app,
            SHOW_TMDB,
            &group_two(),
            &owned_two(),
            &[(1, 1), (1, 2)],
            Duration::from_secs(300),
        )
        .await;
        assert!(res.is_ok(), "a clean consolidation returns Ok");

        // The pack is recorded owned+verified with the FULL-season provides.
        let pack = store
            .get_owned("hpack".into())
            .await
            .expect("pack recorded owned+verified");
        assert_eq!(pack.status, OwnedStatus::Verified);
        assert_eq!(
            pack.provides,
            vec![(1, 1), (1, 2)],
            "pack provides the full season"
        );
        // Both episode selection slots are repointed to the pack.
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            "hpack",
            "E01 slot repointed to the pack"
        );
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 2))
                .await
                .unwrap()
                .hash,
            "hpack",
            "E02 slot repointed to the pack"
        );
        // The superseded scattered episode torrents are pruned (provider + owned records).
        assert!(
            store.get_owned("e1".into()).await.is_none(),
            "e1 pruned from owned"
        );
        assert!(
            store.get_owned("e2".into()).await.is_none(),
            "e2 pruned from owned"
        );
        let d = deleted.lock().unwrap();
        assert!(
            d.contains(&"te1".to_string()) && d.contains(&"te2".to_string()),
            "both scattered episode torrents deleted from the provider"
        );
        assert!(
            !d.contains(&"tpack".to_string()),
            "the adopted pack is NOT deleted"
        );
    }

    #[tokio::test]
    async fn multi_season_complete_series_pack_is_rejected() {
        // A "complete series" pack (covers >1 season) must be rejected as a consolidation target — it
        // would leave its other seasons recorded-owned but un-repointed/un-pruned. The staged pack is
        // dropped and the scattered episodes keep their selection untouched.
        let store = mem_store();
        seed_two_scattered_episodes(&store).await;
        let scraper = Arc::new(MockScraper {
            candidates: vec![season_pack_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        // The staged pack ALSO contains a season-2 file → multi-season, not a single-season pack.
        let mut info = pack_info();
        info.files.push(TorrentFile {
            id: 2,
            path: "S.S02E01.1080p.BluRay.mkv".into(),
            bytes: 2_000_000_000,
            selected: 1,
        });
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: show_listing(),
            add_magnet: Some(AddMagnetResponse {
                id: "tpack".into(),
                uri: String::new(),
            }),
            torrent_info: Some(info),
            resolved_url: Some("https://cdn/p1".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        let res = consolidate_show_seasons(
            &app,
            SHOW_TMDB,
            &group_two(),
            &owned_two(),
            &[(1, 1), (1, 2)],
            Duration::from_secs(300),
        )
        .await;
        assert!(
            res.is_ok(),
            "the season loop completes (nothing consolidated)"
        );

        assert!(
            store.get_owned("hpack".into()).await.is_none(),
            "a multi-season pack must NOT be adopted"
        );
        assert!(
            store.get_owned("e1".into()).await.is_some()
                && store.get_owned("e2".into()).await.is_some(),
            "scattered episodes untouched"
        );
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            "e1",
            "selection unchanged"
        );
        let d = deleted.lock().unwrap();
        assert!(
            d.contains(&"tpack".to_string()),
            "the rejected multi-season pack is dropped (deleted)"
        );
        assert!(
            !d.contains(&"te1".to_string()) && !d.contains(&"te2".to_string()),
            "no episode torrent is pruned when the pack is rejected"
        );
    }

    #[tokio::test]
    async fn show_consolidation_drops_staged_pack_when_listing_unavailable() {
        // The pack stages, but the provider listing (fetched for the prune) is UNAVAILABLE → the
        // staged pack is dropped and the operation deferred, leaving the per-episode content intact.
        let store = mem_store();
        seed_two_scattered_episodes(&store).await;
        let scraper = Arc::new(MockScraper {
            candidates: vec![season_pack_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            // get_torrents (the prune listing) is fetched only AFTER staging → fail it there.
            fail_get_torrents: true,
            add_magnet: Some(AddMagnetResponse {
                id: "tpack".into(),
                uri: String::new(),
            }),
            torrent_info: Some(pack_info()),
            resolved_url: Some("https://cdn/p1".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        let res = consolidate_show_seasons(
            &app,
            SHOW_TMDB,
            &group_two(),
            &owned_two(),
            &[(1, 1), (1, 2)],
            Duration::from_secs(300),
        )
        .await;
        assert!(
            matches!(res, Err(UpgradeSkip::Deferred(_))),
            "a listing-fetch failure defers (cursor unstamped)"
        );

        assert!(
            store.get_owned("hpack".into()).await.is_none(),
            "the staged pack must not be recorded when the listing is unavailable"
        );
        assert!(
            deleted.lock().unwrap().contains(&"tpack".to_string()),
            "the staged pack must be dropped (deleted) on rollback"
        );
        assert!(
            store.get_owned("e1".into()).await.is_some()
                && store.get_owned("e2".into()).await.is_some(),
            "the existing per-episode content is intact"
        );
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            "e1",
            "selection unchanged"
        );
    }

    #[tokio::test]
    async fn show_consolidation_drops_staged_pack_when_library_active() {
        // The pack stages, but a read begins in the library before the idle gate → the staged pack is
        // dropped (no dangling stage) and the operation deferred; per-episode content stays intact.
        let store = mem_store();
        seed_two_scattered_episodes(&store).await;
        let scraper = Arc::new(MockScraper {
            candidates: vec![season_pack_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: show_listing(),
            add_magnet: Some(AddMagnetResponse {
                id: "tpack".into(),
                uri: String::new(),
            }),
            torrent_info: Some(pack_info()),
            resolved_url: Some("https://cdn/p1".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        // A recent read anywhere in the library → the consolidation idle gate trips.
        app.read_activity.touch("Shows/S/whatever.mkv").await;

        let res = consolidate_show_seasons(
            &app,
            SHOW_TMDB,
            &group_two(),
            &owned_two(),
            &[(1, 1), (1, 2)],
            Duration::from_secs(300),
        )
        .await;
        assert!(
            matches!(res, Err(UpgradeSkip::Deferred(_))),
            "an active library defers consolidation"
        );

        assert!(
            store.get_owned("hpack".into()).await.is_none(),
            "no pack recorded while the library is active"
        );
        assert!(
            deleted.lock().unwrap().contains(&"tpack".to_string()),
            "the staged pack is dropped (deleted)"
        );
        assert!(
            store.get_owned("e1".into()).await.is_some()
                && store.get_owned("e2".into()).await.is_some(),
            "per-episode content intact"
        );
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            "e1",
            "selection unchanged"
        );
        assert!(
            !deleted.lock().unwrap().contains(&"te1".to_string()),
            "no episode torrent is pruned"
        );
    }

    /// A redb backend wrapping `InMemoryBackend` that fails every `write`/`set_len` once ARMED, so a
    /// test can make a later store commit (here: the selection repoint) return `Err` deterministically.
    /// Seed the store BEFORE arming. redb is copy-on-write — a failed page write aborts the commit
    /// without flipping the committed root — so prior committed state stays readable (reads never call
    /// `write`).
    #[derive(Debug)]
    struct ArmableFailBackend {
        inner: redb::backends::InMemoryBackend,
        fail_writes: Arc<std::sync::atomic::AtomicBool>,
    }
    impl redb::StorageBackend for ArmableFailBackend {
        fn len(&self) -> Result<u64, std::io::Error> {
            self.inner.len()
        }
        fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), std::io::Error> {
            self.inner.read(offset, out)
        }
        fn set_len(&self, len: u64) -> Result<(), std::io::Error> {
            if self.fail_writes.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("set_len blocked (armed)"));
            }
            self.inner.set_len(len)
        }
        fn sync_data(&self) -> Result<(), std::io::Error> {
            self.inner.sync_data()
        }
        fn write(&self, offset: u64, data: &[u8]) -> Result<(), std::io::Error> {
            if self.fail_writes.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("write blocked (armed)"));
            }
            self.inner.write(offset, data)
        }
    }
    fn fail_after_arm_store() -> (Store, Arc<std::sync::atomic::AtomicBool>) {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let backend = ArmableFailBackend {
            inner: redb::backends::InMemoryBackend::new(),
            fail_writes: fail.clone(),
        };
        let db = redb::Database::builder()
            .create_with_backend(backend)
            .unwrap();
        (Store::from_database(Arc::new(db)).unwrap(), fail)
    }

    #[tokio::test]
    async fn show_consolidation_repoint_failure_defers_prune() {
        // If a `put_selection` repoint fails partway, the prune MUST be deferred — a scattered episode
        // whose slot still points at the about-to-be-deleted hash must never be orphaned. Nothing may
        // be deleted on the provider. (We arm the store to fail ALL writes, so the pack record also
        // doesn't persist; the asserted contract — defer + delete nothing — is what matters here.)
        let (store, fail) = fail_after_arm_store();
        seed_two_scattered_episodes(&store).await;

        let scraper = Arc::new(MockScraper {
            candidates: vec![season_pack_candidate()],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: show_listing(),
            add_magnet: Some(AddMagnetResponse {
                id: "tpack".into(),
                uri: String::new(),
            }),
            torrent_info: Some(pack_info()),
            resolved_url: Some("https://cdn/p1".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        // Arm AFTER seeding so the selection repoint (a redb commit) fails.
        fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let res = consolidate_show_seasons(
            &app,
            SHOW_TMDB,
            &group_two(),
            &owned_two(),
            &[(1, 1), (1, 2)],
            Duration::from_secs(300),
        )
        .await;
        assert!(
            matches!(res, Err(UpgradeSkip::Deferred(_))),
            "a failed repoint defers the prune"
        );

        // The prune was DEFERRED: no torrent may be deleted (neither the scattered episodes — the
        // whole point — nor the staged pack, which the repoint-fail branch intentionally leaves for
        // the duplicate-dedup pass to reclaim later).
        assert!(
            deleted.lock().unwrap().is_empty(),
            "nothing may be deleted when the selection repoint failed"
        );

        // Disarm so verification reads are pristine, then confirm the per-episode content survives.
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(
            store.get_owned("e1".into()).await.is_some()
                && store.get_owned("e2".into()).await.is_some(),
            "scattered episodes are NOT pruned"
        );
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            "e1",
            "E01 slot still points at its scattered episode"
        );
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 2))
                .await
                .unwrap()
                .hash,
            "e2",
            "E02 slot still points at its scattered episode"
        );
    }

    fn q1080_web() -> crate::release::QualitySummary {
        crate::release::QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 1,
        }
    }
    fn q1080_bluray() -> crate::release::QualitySummary {
        crate::release::QualitySummary {
            cached: true,
            source_tier: 6_000,
            resolution: 1080,
            score: 2,
        }
    }
    fn q2160_remux() -> crate::release::QualitySummary {
        crate::release::QualitySummary {
            cached: true,
            source_tier: 8_000,
            resolution: 2160,
            score: 9,
        }
    }
}
