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
use crate::tmdb_client::TitleDetails;
use crate::vfs::MediaType;
use std::future::Future;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Why an upgrade/consolidation attempt made no change. The distinction matters for the round-robin
/// cursor: a `Deferred` title was NOT actually evaluated (the library went active mid-stage, or the
/// provider/scraper was momentarily unavailable) and must be reconsidered next tick — its cursor is
/// left UNSTAMPED. A `NoChange` title WAS evaluated to completion (no meaningful upgrade exists, or
/// the chosen candidate didn't pan out) and its cursor is stamped so the budget rotates onward.
#[derive(Debug)]
enum UpgradeSkip {
    Deferred(String),
    NoChange(String),
}

/// Run one upgrade tick over `app`: re-score a budgeted batch of owned titles. For MOVIES, stage any
/// cached meaningful upgrade and — if the library is idle — swap selection + prune the old torrent.
/// For SHOWS, four layered passes: per-singleton-episode upgrade/downgrade, upgrade of episodes
/// served from a within-ceiling multi-episode pack, over-ceiling season-pack correction, then
/// full-season consolidation of scattered per-episode torrents into a cached pack.
/// Each title (movie or show) is one budget unit, ordered least-recently-checked first.
pub async fn run_upgrade_once(app: &AppState) {
    run_upgrade_once_with(app, |tmdb_id, media_type| {
        app.tmdb_client.details(tmdb_id, media_type)
    })
    .await
}

/// [`run_upgrade_once`] with the TMDB `details` fetch (used only by the per-title language backfill,
/// [`LanguageBackfill`]) injected, so tests can drive the backfill without the network.
async fn run_upgrade_once_with<F, Fut>(app: &AppState, fetch_details: F)
where
    F: Fn(u64, MediaType) -> Fut,
    Fut: Future<Output = Result<TitleDetails, reqwest::Error>>,
{
    let budget = app.config.upgrade.budget_per_tick as usize;
    let idle_window = Duration::from_secs(app.config.upgrade.idle_secs);

    // Re-derive the quality of owned records that have NONE from their file names (the persisted
    // identifications) BEFORE ranking anything. A record without a quality is unknown — the lowest —
    // and is upgraded as such; but a copy whose file name shows a resolution above the ceiling must
    // never be mistaken for unknown (it stays under the corrective-downgrade rules), so its quality
    // has to be known first. The scan loop's account-mirror pass does the same each scan; this covers
    // an upgrade tick that runs before it (e.g. at start-up). Only a record whose file name is
    // unknown anywhere stays `None` (and is then unknown-lowest).
    {
        let owned = app.store.all_owned().await;
        if owned.iter().any(|(_, r)| r.quality.is_none()) {
            let matches = app.store.load_all_matches().await;
            let infos: Vec<&crate::rd_client::TorrentInfo> =
                matches.values().map(|(i, _)| i).collect();
            crate::tasks::backfill_owned_quality(
                &app.store,
                &app.config.acquisition.prefs,
                &owned,
                &infos,
            )
            .await;
        }
    }

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

    let mut backfill = LanguageBackfill::new(fetch_details);
    for (_, tmdb_id, media_type, hashes, mut rec) in ordered {
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
        // Legacy/mirror records predate `spoken_languages`: fetch the title's TMDB languages once
        // (before dispatch) so this visit's staging probes already accept its spoken languages under
        // AUDIO_LANGUAGE=original. A transient TMDB failure defers the title instead of probing it
        // with the original-only rule.
        let backfilled = backfill
            .run(app, tmdb_id, &media_type, &hashes, &mut rec)
            .await;
        let outcome = match backfilled {
            Err(skip) => Err(skip),
            Ok(()) => match media_type {
                MediaType::Movie => {
                    try_upgrade_movie(app, tmdb_id, &hashes, &rec, idle_window).await
                }
                MediaType::Show => try_upgrade_show(app, tmdb_id, &hashes, idle_window).await,
            },
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

/// One-time backfill of a title's TMDB languages for owned records that predate
/// `AcquireRequest.spoken_languages` (or account-mirror records, which also lack an
/// `original_language`). Such a record carries `spoken_languages: None` ("never fetched"), so
/// `AUDIO_LANGUAGE=original` would still accept only the original language when this pass probes an
/// upgrade candidate for it — e.g. blacklisting every English release of a `fi`-original,
/// mostly-English film as BadAudio.
///
/// The decision is GROUP-wide, not representative-only: the show paths stage and probe with each
/// episode/pack record's OWN request, so the fetch fires when ANY record of the group is `None` (a
/// legacy episode whose group's representative was acquired after the field existed, or a mirror
/// record that joined later). One fetch then fills every record, so it is never repeated for that
/// group. Holds a per-tick memory of a transient TMDB failure so an outage costs one (slow, retrying)
/// fetch per tick rather than one per title.
struct LanguageBackfill<F> {
    fetch: F,
    /// Set once a transient TMDB failure is seen this tick: later titles that need a backfill are
    /// deferred without another fetch.
    tmdb_unavailable: bool,
}

impl<F, Fut> LanguageBackfill<F>
where
    F: Fn(u64, MediaType) -> Fut,
    Fut: Future<Output = Result<TitleDetails, reqwest::Error>>,
{
    fn new(fetch: F) -> Self {
        Self {
            fetch,
            tmdb_unavailable: false,
        }
    }

    /// Backfill the group's TMDB languages if any of its records has never had them fetched, before
    /// the title is dispatched. `Ok(())` = proceed (nothing to do, filled, or a permanent TMDB
    /// refusal — see [`tmdb_error_is_permanent`]). `Err(Deferred)` = a transient TMDB failure (now or
    /// earlier this tick): nothing is written and the title is NOT dispatched, so no staging probe
    /// runs with the original-only rule (which could blacklist correct spoken-language releases for
    /// the blacklist TTL); its cursor stays unstamped and it is retried next tick.
    async fn run(
        &mut self,
        app: &AppState,
        tmdb_id: u64,
        media_type: &MediaType,
        hashes: &[String],
        rec: &mut OwnedRecord,
    ) -> Result<(), UpgradeSkip> {
        if !group_needs_language_backfill(app, hashes, rec).await {
            return Ok(());
        }
        if self.tmdb_unavailable {
            return Err(UpgradeSkip::Deferred(
                "TMDB language backfill pending (TMDB unavailable earlier this tick)".into(),
            ));
        }
        match (self.fetch)(tmdb_id, media_type.clone()).await {
            Ok(details) => {
                debug!(
                    "upgrade: tmdb {} backfilled languages (original {:?}, spoken {:?})",
                    tmdb_id, details.original_language, details.spoken_languages
                );
                persist_title_languages(
                    app,
                    hashes,
                    rec,
                    details.original_language,
                    details.spoken_languages,
                )
                .await;
                Ok(())
            }
            // Waiting won't fix it, and deferring would park the title at the head of the
            // round-robin forever: proceed with the recorded languages (the pre-backfill rule).
            Err(e) if tmdb_error_is_permanent(&e) => {
                debug!(
                    "upgrade: tmdb {} language backfill refused ({}); proceeding with the recorded languages",
                    tmdb_id, e
                );
                Ok(())
            }
            Err(e) => {
                self.tmdb_unavailable = true;
                Err(UpgradeSkip::Deferred(format!(
                    "TMDB language backfill failed ({e}); retrying next tick"
                )))
            }
        }
    }
}

/// A TMDB failure that will not fix itself by waiting: a 4xx other than 429 (bad API key, unknown
/// id). A 429, a 5xx, or a failure with no HTTP status (transport/decode) is transient.
fn tmdb_error_is_permanent(e: &reqwest::Error) -> bool {
    e.status()
        .is_some_and(|s| s.is_client_error() && s != reqwest::StatusCode::TOO_MANY_REQUESTS)
}

/// True when the in-hand representative `rec` or ANY owned record in `hashes` has never had its
/// TMDB spoken languages fetched (`None`). A hash with no record (pruned concurrently) is skipped.
async fn group_needs_language_backfill(
    app: &AppState,
    hashes: &[String],
    rec: &OwnedRecord,
) -> bool {
    if rec.request.spoken_languages.is_none() {
        return true;
    }
    for h in hashes {
        if let Some(r) = app.store.get_owned(h.clone()).await {
            if r.request.spoken_languages.is_none() {
                return true;
            }
        }
    }
    false
}

/// Fill the fetched TMDB languages into every unset field of the in-hand representative `rec` and of
/// every owned record in `hashes`: `spoken_languages` where it is `None`, and `original_language`
/// where it is `None` (an account-mirror record), so the staging probe checks `original ∪ spoken`
/// like an engine-acquired title. Each record is updated with the store's atomic
/// `backfill_title_languages_if_unset` (one transaction, only those fields change), so a concurrent
/// `observe` write to a Pending record of the group can't be clobbered by a stale copy. A hash with
/// no record (pruned concurrently) is skipped, never resurrected; an already-fetched list or a
/// recorded original language is kept. A failed write only delays that record (it keeps `None` and
/// triggers another fetch on a later visit).
async fn persist_title_languages(
    app: &AppState,
    hashes: &[String],
    rec: &mut OwnedRecord,
    original_language: Option<String>,
    spoken: Vec<String>,
) {
    if rec.request.spoken_languages.is_none() {
        rec.request.spoken_languages = Some(spoken.clone());
    }
    if rec.request.original_language.is_none() {
        rec.request.original_language = original_language.clone();
    }
    for h in hashes {
        if let Err(e) = app
            .store
            .backfill_title_languages_if_unset(h.clone(), original_language.clone(), spoken.clone())
            .await
        {
            debug!(
                "upgrade: persisting TMDB languages for {} failed ({}); retrying next visit",
                h, e
            );
        }
    }
}

/// The best quality across ALL owned copies of a title — the upgrade baseline — or `None` when no
/// owned record is left (all pruned concurrently; a dangling hash is skipped, not fatal). Known
/// quality beats unknown ([`release::is_unknown_quality`]: no recorded quality, or an untagged
/// resolution / source tier — the LOWEST quality); among copies on the same side, the highest
/// `effective_score` wins. A copy with no recorded quality baselines as
/// [`QualitySummary::unknown`]. Used as the upgrade baseline so a swap that prunes every copy can only
/// proceed against the BEST one it would delete — never downgrading by comparing against a worse
/// duplicate — while a title owned only as unknown copies baselines as unknown, so any known cached
/// within-ceiling candidate at or above its resolution (adopted only on a probe `Pass`) improves on
/// it. A known resolution stays a floor: an unknown within-ceiling copy at a higher resolution than
/// the best known one is the baseline instead, so the swap never trades it down. Ranking by
/// `effective_score` (rather than raw `score`) is ceiling-aware: an over-ceiling copy (never unknown)
/// no longer masquerades as "best" and block its own corrective downgrade — e.g. owning only a 2160p
/// copy under a 1080p ceiling must baseline off its (heavily penalised) effective score so a
/// within-ceiling cached candidate can be recognised as an improvement.
async fn best_owned_quality(app: &AppState, owned_hashes: &[String]) -> Option<QualitySummary> {
    let prefs = &app.config.acquisition.prefs;
    let ceiling = prefs.max_resolution.height();
    let key = |q: &QualitySummary| {
        (
            !release::is_unknown_quality(q, ceiling),
            release::effective_score(q, prefs),
        )
    };
    let mut best: Option<QualitySummary> = None;
    // The highest-resolution unknown within-ceiling copy (see the floor below).
    let mut floor: Option<QualitySummary> = None;
    for h in owned_hashes {
        // A record concurrently pruned between grouping and here is no longer part of the owned set
        // the swap would delete, so SKIP it rather than aborting the whole baseline (which would
        // leave the title un-upgradeable while a dangling hash lingers in the group).
        let Some(rec) = app.store.get_owned(h.clone()).await else {
            continue;
        };
        let q = rec.quality.unwrap_or_else(QualitySummary::unknown);
        if release::is_unknown_quality(&q, ceiling)
            && floor
                .as_ref()
                .is_none_or(|f| (q.resolution, key(&q)) > (f.resolution, key(f)))
        {
            floor = Some(q.clone());
        }
        if best.as_ref().is_none_or(|b| key(&q) > key(b)) {
            best = Some(q);
        }
    }
    // A known resolution stays a floor: an unknown within-ceiling copy at a HIGHER resolution than
    // the best within-ceiling copy (known, or another unknown that out-scored it) becomes the
    // baseline, so an improvement on it (a known release at or above that resolution) also improves
    // every other copy the swap would prune and never trades its resolution down. An over-ceiling
    // best is left alone: it already baselines the gated corrective-downgrade path.
    if let (Some(b), Some(f)) = (&best, floor) {
        if b.resolution <= ceiling && f.resolution > b.resolution {
            best = Some(f);
        }
    }
    best
}

/// The owned hashes to PRUNE when a movie upgrade is adopted. When corrective downgrade is NOT
/// enabled (`ALLOW_RESOLUTION_DOWNGRADE` unset), an owned copy whose resolution sits ABOVE the
/// `MAX_RESOLUTION` ceiling is EXCLUDED — removing an over-ceiling copy is a corrective downgrade,
/// which the contract reserves for the gated downgrade path. Without this, the movie path (whose
/// baseline is widened to the BEST copy — an over-ceiling copy being effective-score-penalised — and
/// which prunes EVERY owned hash) would delete an over-ceiling engine copy (acquired under a
/// previously-higher ceiling, so it still carries a known `quality`) as collateral of a
/// within-ceiling upgrade, silently discarding content. With the flag on, every copy is prunable.
async fn supersede_hashes(app: &AppState, owned_hashes: &[String]) -> Vec<String> {
    if app.config.allow_resolution_downgrade {
        return owned_hashes.to_vec();
    }
    let ceiling = app.config.acquisition.prefs.max_resolution.height();
    let mut prunable = Vec::with_capacity(owned_hashes.len());
    for h in owned_hashes {
        let over_ceiling = app
            .store
            .get_owned(h.clone())
            .await
            .and_then(|r| r.quality)
            .is_some_and(|q| q.resolution > ceiling);
        if !over_ceiling {
            prunable.push(h.clone());
        }
    }
    prunable
}

/// Whether `slot`'s current selection points at an owned copy of this group that is ABOVE the
/// `MAX_RESOLUTION` ceiling while corrective downgrade is gated OFF (`ALLOW_RESOLUTION_DOWNGRADE`
/// unset). In that state `supersede_hashes` deliberately RETAINS the over-ceiling copy — but the
/// swap tail still repoints the slot unconditionally, so adopting a within-ceiling "upgrade" (rated
/// `Improvement::Upgrade` relative to a *separate* within-ceiling copy in the same group, not a
/// gated `Downgrade`) would silently move the SERVED resolution down. That is a corrective downgrade
/// the contract reserves for the gated path, so callers skip the swap when this returns `true`.
/// Always `false` when the flag is on (the gated downgrade path handles it) or when the slot has no
/// selection / points outside the group / at a within-ceiling copy.
async fn served_slot_is_retained_over_ceiling(
    app: &AppState,
    slot: &str,
    owned_hashes: &[String],
) -> bool {
    if app.config.allow_resolution_downgrade {
        return false;
    }
    let Some(sel) = app.store.get_selection(slot.to_string()).await else {
        return false;
    };
    if !owned_hashes
        .iter()
        .any(|h| h.eq_ignore_ascii_case(&sel.hash))
    {
        return false;
    }
    let ceiling = app.config.acquisition.prefs.max_resolution.height();
    app.store
        .get_owned(sel.hash.clone())
        .await
        .and_then(|r| r.quality)
        .is_some_and(|q| q.resolution > ceiling)
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

/// A staged, verified replacement ready to be swapped in. `slot_files` maps each selection slot to
/// the file_path within the staged torrent that should represent it (one entry for a movie, one per
/// episode for a pack).
struct StagedUpgrade {
    hash: String,
    torrent_id: String,
    slot_files: Vec<(String, String)>,
}

/// What a swap prunes once its slot(s) are repointed — settled by [`apply_upgrade`] against the
/// provider listing it fetches just before its final idle check, so a presence-dependent decision
/// (phase a2's pack prune) is made on the same snapshot the prune itself runs against.
enum PrunePlan {
    /// Prune exactly these hashes (movie, singleton episode, over-ceiling pack correction).
    Hashes(Vec<String>),
    /// Phase (a2): prune the multi-episode pack only if [`pack_prunable_after_repoint`] holds when
    /// coverage counts only OTHER records whose torrent is PRESENT ([`PackPrune::resolve`]).
    Pack(PackPrune),
}

impl PrunePlan {
    /// The hashes to prune, given the provider `listing` fetched for this swap.
    fn resolve(&self, listing: &[crate::rd_client::Torrent]) -> Vec<String> {
        match self {
            PrunePlan::Hashes(h) => h.clone(),
            PrunePlan::Pack(p) => p.resolve(listing),
        }
    }
}

/// The inputs of one phase-(a2) pack-prune decision ([`PrunePlan::Pack`]).
struct PackPrune {
    tmdb_id: u64,
    hash: String,
    provides: Vec<(u32, u32)>,
    repointed: (u32, u32),
    /// Each of the pack's episodes → the hash its selection slot points at.
    served_by: std::collections::HashMap<(u32, u32), String>,
    /// `(hash, provides)` of the show's OTHER `Verified` owned records, re-read from the store just
    /// before the swap ([`other_owned_coverage`]).
    others: Vec<(String, Vec<(u32, u32)>)>,
    /// Hashes adopted earlier this tick ([`EpisodeTick::adopted`]): present even while the provider
    /// listing lags their add.
    adopted: Vec<String>,
}

impl PackPrune {
    /// Prune the pack iff it is prunable once coverage counts only PRESENT other records — in the
    /// provider `listing`, or adopted earlier this tick. A `Verified`-but-absent record (its torrent
    /// lapsed off the provider) still names its slot, but the VFS then falls back to whichever present
    /// torrent holds the episode — typically this pack — so counting it as coverage would delete the
    /// only present copy of that episode, and nothing would bring it back (no file left to read, so
    /// no on-read repair; `monitor_episodes` judges availability per title).
    fn resolve(&self, listing: &[crate::rd_client::Torrent]) -> Vec<String> {
        let present: std::collections::HashSet<String> = listing
            .iter()
            .map(|t| t.hash.to_ascii_lowercase())
            .chain(self.adopted.iter().map(|h| h.to_ascii_lowercase()))
            .collect();
        let covered: Vec<(u32, u32)> = self
            .others
            .iter()
            .filter(|(h, _)| present.contains(&h.to_ascii_lowercase()))
            .flat_map(|(_, p)| p.iter().copied())
            .collect();
        let (s, e) = self.repointed;
        if pack_prunable_after_repoint(
            &self.hash,
            &self.provides,
            self.repointed,
            &self.served_by,
            &covered,
        ) {
            debug!(
                "upgrade: tmdb {} pack {} fully superseded by s{}e{}'s repoint; pruning it",
                self.tmdb_id, self.hash, s, e
            );
            vec![self.hash.clone()]
        } else {
            debug!(
                "upgrade: tmdb {} pack {} kept after s{}e{}'s repoint (still serves an episode, or is the only present holder of one)",
                self.tmdb_id, self.hash, s, e
            );
            Vec::new()
        }
    }
}

/// Shared swap/prune tail used by the movie, per-episode, and pack-correction callers. Idle-gates,
/// fetches the provider listing ONCE (before the final idle re-check, so there is no network
/// round-trip between confirming idle and the destructive prune), repoints every slot to the staged
/// release, then prunes what `prune` resolves to against that listing. Non-destructive rollback on
/// defer: a mid-stage active read fully deletes the staged torrent + drops its records and leaves
/// the current release intact.
async fn apply_upgrade(
    app: &AppState,
    staged: StagedUpgrade,
    prune: PrunePlan,
    idle_window: Duration,
) -> Result<(), UpgradeSkip> {
    // Fetch the provider listing for the prune ONCE, and BEFORE the idle re-check below, so there is
    // NO network round-trip between the idle confirmation and the destructive prune — a read
    // starting in that window could otherwise have the old torrent deleted out from under it.
    // Distinguish a FETCH FAILURE from a genuinely-empty account: `unwrap_or_default()` would make a
    // failed `get_torrents` look empty, so `prune_owned_hash_in` (which treats "hash absent from the
    // listing" as "already gone → drop the record") would drop the old hashes' records while leaving
    // the still-present torrents orphaned → re-adopted as duplicates. On an `Err`, roll back the
    // staged candidate and defer.
    let listing = match app.provider.get_torrents().await {
        Ok(l) => l,
        Err(_) => {
            if app
                .provider
                .delete_torrent(&staged.torrent_id)
                .await
                .is_ok()
            {
                let _ = app.store.remove_owned(staged.hash.clone()).await;
                let _ = app.store.remove_authoritative(staged.hash.clone()).await;
            }
            return Err(UpgradeSkip::Deferred(
                "provider listing unavailable; deferring".into(),
            ));
        }
    };
    // Re-check idle IMMEDIATELY before the destructive swap+prune. Staging above can take many
    // seconds (add + select + cached-poll + 4 MB probe), during which a playback read may have begun
    // anywhere in the library. If it is no longer idle, fully roll back the freshly-staged candidate
    // (delete it + drop its records — non-destructive: the current release is untouched) and retry
    // later, so the prune below never interrupts an in-flight stream.
    if !app.read_activity.all_idle(idle_window).await {
        // Roll back by the KNOWN torrent id, not the listing snapshot: the just-staged torrent may
        // not have propagated into `listing` yet. Only drop the store records when the delete
        // SUCCEEDED — on a transient delete failure, KEEP them so a later tick retries rather than
        // leaving a present-but-untracked torrent that `record_mirror_owned` would re-adopt as a
        // DUPLICATE (the `execute_remove`/`prune_owned_hash_in` keep-on-failure discipline).
        if app
            .provider
            .delete_torrent(&staged.torrent_id)
            .await
            .is_ok()
        {
            let _ = app.store.remove_owned(staged.hash.clone()).await;
            let _ = app.store.remove_authoritative(staged.hash.clone()).await;
        }
        return Err(UpgradeSkip::Deferred(
            "library became active during staging; deferring".into(),
        ));
    }
    // Repoint every slot. If ANY selection write fails, do NOT prune (pruning behind a stale
    // selection would leave a slot resolving to a hash we're about to delete) — defer the prune; the
    // staged copy stays owned and is reclaimed by the duplicate-dedup pass.
    for (slot, file_path) in &staged.slot_files {
        if let Err(e) = app
            .store
            .put_selection(
                slot.clone(),
                crate::store::SelectionEntry {
                    hash: staged.hash.clone(),
                    file_path: file_path.clone(),
                },
            )
            .await
        {
            return Err(UpgradeSkip::Deferred(format!(
                "selection write failed; deferring prune: {e}"
            )));
        }
    }
    for old in prune.resolve(&listing) {
        if old.eq_ignore_ascii_case(&staged.hash) {
            continue;
        }
        prune_owned_hash_in(app, &old, &listing).await;
    }
    Ok(())
}

/// Whether a candidate's addon-reported file is a container the probe can never read (a known file
/// name that isn't MKV/MP4, e.g. `.avi`). Upgrade staging adopts only a probe `Pass`, so such a
/// candidate could only ever be staged and rejected — skip it before the add + cached-poll + probe.
/// A candidate with NO reported file name is kept: its container is unknown until staged.
fn known_unverifiable(r: &release::ReleaseInfo) -> bool {
    r.file_name.is_some() && !r.container.is_verifiable()
}

/// The prefs upgrade candidates are ORDERED by: the configured prefs with `AUDIO_LANGUAGE=original`
/// resolved against the title's TMDB original + spoken languages (`req`), exactly as acquisition
/// ranks them. On the raw prefs `score()` has no per-title language to penalise, so a release tagged
/// with the wrong language (`Rus`, 🇷🇺) would be staged before a correctly-tagged one — and, if its
/// audio tracks are untagged, pass the probe. Every stored `QualitySummary` stays on the raw prefs,
/// so a stored and a fresh score always compare like-for-like (no flip-flop); [`rank_changes`] uses
/// these prefs to order candidates and as a SECOND, strictly stronger improvement test (a penalty
/// only lowers the candidate's score). `score()`'s hard filters don't depend on audio, so these prefs
/// filter exactly what the raw ones do.
fn ordering_prefs(
    app: &AppState,
    req: &crate::store::AcquireRequest,
) -> crate::config::QualityPrefs {
    app.config.acquisition.prefs.with_resolved_audio(
        req.original_language.as_deref(),
        req.spoken_languages.as_deref().unwrap_or_default(),
    )
}

/// Rank all CACHED target-improvement candidates best-first, skipping owned and blacklisted
/// hashes, candidates whose reported file is a known-unverifiable container ([`known_unverifiable`])
/// and anything failing `score()`'s hard filters (ceiling / cam / dead-seeder). Improvement is judged
/// on the raw prefs AND, again, with the candidate scored under the title's [`ordering_prefs`]
/// (`req`) — so a release acquisition would rank below the owned copy (a wrong-language penalty) is
/// never a candidate; against an unknown-quality (or uncached) owned copy, whose comparison ignores
/// scores, a release carrying that penalty ([`release::wrong_audio_language`]) is dropped outright.
/// The improving candidates are then ordered by that score, so a wrong-language release is tried
/// after a correctly-tagged one. The
/// caller stages them in order until one is adopted (in-tick fall-through), so a top candidate that
/// fails a candidate-specific gate no longer ends the tick. The list is direction-homogeneous (all
/// `Upgrade` or all `Downgrade`) because `is_target_improvement` only rates a lower-resolution
/// candidate as improving when the owned copy is over the ceiling.
async fn rank_changes(
    app: &AppState,
    media: MediaKind,
    tmdb_id: u64,
    raws: &[release::RawCandidate],
    baseline: &QualitySummary,
    owned_hashes: &[String],
    req: &crate::store::AcquireRequest,
) -> Vec<(release::ReleaseInfo, release::Improvement)> {
    let prefs = &app.config.acquisition.prefs;
    let order = ordering_prefs(app, req);
    // The improvement verdict ignores scores for an uncached or unknown-quality owned copy.
    let score_blind =
        !baseline.cached || release::is_unknown_quality(baseline, prefs.max_resolution.height());
    // Fetch the per-title blacklist ONCE and test membership in memory, rather than one awaited redb
    // read per candidate (Torrentio commonly returns dozens of streams per title) — mirrors `acquire`.
    let blacklist = app.store.blacklisted_hashes_for(media, tmdb_id).await;
    let mut scored: Vec<(i64, release::ReleaseInfo, release::Improvement)> = Vec::new();
    for raw in raws {
        let r = release::parse(raw);
        if owned_hashes
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&r.info_hash))
        {
            continue;
        }
        if blacklist.contains(&r.info_hash.to_ascii_lowercase()) {
            continue;
        }
        if known_unverifiable(&r) {
            continue;
        }
        if release::score(&r, prefs).is_none() {
            continue;
        }
        let q = QualitySummary::of(&r, prefs);
        let imp = release::is_target_improvement(baseline, &q, prefs);
        if imp == release::Improvement::None {
            continue;
        }
        let order_score = release::score(&r, &order).unwrap_or(q.score);
        // It must ALSO still improve on the owned copy when scored the way acquisition ranks it
        // (the title's resolved audio): a release tagged only with other languages (`Rus`/🇷🇺 on an
        // English-original title) can be a raw-prefs category jump that acquisition, after its
        // wrong-language penalty, ranks BELOW the copy we own — and untagged audio tracks would let
        // it pass the probe. The penalty only ever LOWERS the candidate's score, so this is strictly
        // stronger than the raw test (the direction can't change): every swap still raises the raw
        // score that stored summaries carry, so there is still no flip-flop.
        let as_ranked = QualitySummary {
            score: order_score,
            ..q.clone()
        };
        if release::is_target_improvement(baseline, &as_ranked, prefs) == release::Improvement::None
        {
            continue;
        }
        // …but against an owned copy of UNKNOWN quality (or an uncached one) `is_target_improvement`
        // ignores scores — any known cached candidate improves it — so the test above can't see the
        // penalty. Drop a wrong-language release explicitly there: it never replaces the user's
        // copy, which may well be in the right language.
        if score_blind && release::wrong_audio_language(&r, &order) {
            continue;
        }
        scored.push((order_score, r, imp));
    }
    scored.sort_by_key(|(score, ..)| std::cmp::Reverse(*score)); // best first (stable for ties)
                                                                 // Dedup by info_hash: Torrentio can return the same infohash under multiple stream entries
                                                                 // (different source/title strings, sometimes scored differently). Without this, a duplicate
                                                                 // whose first entry fails and is blacklisted would still be tried again from its second entry —
                                                                 // re-adding, re-probing and re-blacklisting the SAME torrent (stage_and_verify does NOT re-check
                                                                 // the blacklist). Keep the best-scored entry (the first after the reverse-score sort).
    let mut seen = std::collections::HashSet::new();
    scored
        .into_iter()
        .filter(|(_, r, _)| seen.insert(r.info_hash.to_ascii_lowercase()))
        .map(|(_, r, imp)| (r, imp))
        .collect()
}

/// Gate a corrective downgrade on the opt-in flag. An `Upgrade` always proceeds. A `Downgrade` with
/// the flag OFF is logged (preview, including the current/old hash being downgraded) and rejected;
/// with the flag ON it proceeds.
fn downgrade_allowed_or_log(
    app: &AppState,
    tmdb_id: u64,
    imp: release::Improvement,
    old_hash: &str,
    current: &QualitySummary,
    cand: &release::ReleaseInfo,
) -> bool {
    match imp {
        release::Improvement::Upgrade => true,
        release::Improvement::Downgrade if app.config.allow_resolution_downgrade => true,
        release::Improvement::Downgrade => {
            info!(
                "upgrade: tmdb {} would downgrade {}({}p) -> {}({}p) [set ALLOW_RESOLUTION_DOWNGRADE=true to enable]",
                tmdb_id,
                old_hash,
                current.resolution,
                cand.info_hash,
                cand.resolution.unwrap_or(0)
            );
            false
        }
        release::Improvement::None => false,
    }
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
    //    The swap below prunes EVERY owned hash, so if a title has multiple present copies,
    //    comparing against only the representative (lexicographically-first) copy could "upgrade"
    //    past the worse copy yet DELETE a better one — e.g. own {WEB 1080p, REMUX 1080p}, baseline off
    //    WEB, adopt a BluRay 1080p, prune the REMUX → a downgrade + loss. Known quality beats unknown
    //    (`best_owned_quality`): an unknown-quality copy (no recorded quality, or an untagged axis) is
    //    the LOWEST quality, so it never sets the bar while a known copy exists, and a title owned only
    //    as unknown copies is upgraded by any known cached within-ceiling candidate that passes the
    //    probe. An over-ceiling copy is never unknown (the tick-start backfill re-derives a missing
    //    quality from its file name first): it is deliberately baselined via the ceiling-penalised
    //    `effective_score` (see the corrective-downgrade path). Checked BEFORE the scrape so a title
    //    with no owned record left doesn't waste a scrape. (`owned_rec` is still used below for the
    //    request's imdb_id/metadata — only the quality BASELINE is widened here.)
    let Some(current) = best_owned_quality(app, owned_hashes).await else {
        return Err(UpgradeSkip::NoChange("no owned copy left".into()));
    };
    // Corrective-downgrade guard: if the baseline is a WITHIN-ceiling copy (so any adopted candidate
    // is rated `Improvement::Upgrade`, bypassing the downgrade gate) yet the SERVED slot points at a
    // separate over-ceiling copy we are deliberately RETAINING (ALLOW_RESOLUTION_DOWNGRADE off), then
    // adopting that "upgrade" would repoint the served slot DOWN in resolution — a corrective
    // downgrade the contract reserves for the gated path. Skip (preview only). The `current` <=
    // ceiling condition is what distinguishes this from the genuine sole-over-ceiling-copy downgrade
    // case (baseline over-ceiling → candidates rated `Downgrade` → handled by `downgrade_allowed_or_log`).
    // Checked before the scrape so it costs no network call.
    let ceiling = app.config.acquisition.prefs.max_resolution.height();
    if current.resolution <= ceiling
        && served_slot_is_retained_over_ceiling(app, &movie_slot(tmdb_id), owned_hashes).await
    {
        debug!(
            "upgrade: tmdb {} served movie copy is over-ceiling and retained; not repointing (corrective downgrade gated off)",
            tmdb_id
        );
        return Err(UpgradeSkip::NoChange(
            "served copy is over-ceiling and retained (corrective downgrade gated off)".into(),
        ));
    }
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
    // 2. Scrape fresh candidates for this title, then rank the cached target-improvements not
    //    already owned/blacklisted (best-score-first). A scrape failure is transient → Deferred
    //    (retry next tick).
    let raws = app
        .scraper
        .find(&req.imdb_id, MediaKind::Movie, None, None)
        .await
        .map_err(|e| UpgradeSkip::Deferred(format!("scrape failed: {e}")))?;
    let ranked = rank_changes(
        app,
        MediaKind::Movie,
        tmdb_id,
        &raws,
        &current,
        owned_hashes,
        &req,
    )
    .await;
    if ranked.is_empty() {
        return Err(UpgradeSkip::NoChange("no meaningful upgrade".into()));
    }
    // The movie baseline spans all owned copies (the swap prunes them all), so the first hash is a
    // stable stand-in for the downgrade preview log.
    let old_hash = owned_hashes.first().map(String::as_str).unwrap_or("");
    // Merge the title's sticky provenance ONCE from records fetched by hash (a small point-lookup
    // set), instead of re-scanning the entire owned table via group_owned_by_tmdb inside
    // stage_and_verify — mirrors the show path (upgrade_show_episodes_and_packs). `owned_hashes` IS
    // the movie's full group (the swap prunes them all), so the merge equals the old group
    // provenance; an empty set (every copy raced away) merges to EMPTY — the same non-manual fallback.
    let mut group_owned: Vec<(String, OwnedRecord)> = Vec::new();
    for h in owned_hashes {
        if let Some(r) = app.store.get_owned(h.clone()).await {
            group_owned.push((h.clone(), r));
        }
    }
    let group_prov = merged_provenance(&group_owned);
    // Try ranked candidates best-first until one is adopted (in-tick fall-through), mirroring the
    // pack-correction/consolidation loops.
    for (cand, imp) in ranked {
        // Gated-off downgrade: the list is direction-homogeneous, so every remaining candidate is a
        // gated downgrade too — break (one preview log) rather than logging a preview per candidate.
        if !downgrade_allowed_or_log(app, tmdb_id, imp, old_hash, &current, &cand) {
            break;
        }
        // Idle re-check before staging each candidate (mirrors correct_over_ceiling_packs).
        if !app.read_activity.all_idle(idle_window).await {
            return Err(UpgradeSkip::Deferred(
                "library active; deferring upgrade".into(),
            ));
        }
        match stage_and_verify(app, tmdb_id, &req, &cand, &group_prov).await {
            Ok(staged) => {
                // Prune the superseded copies — but NOT an over-ceiling copy unless corrective
                // downgrade is enabled (`supersede_hashes`), so a within-ceiling upgrade never
                // deletes an over-ceiling copy as collateral.
                let supersede = supersede_hashes(app, owned_hashes).await;
                apply_upgrade(
                    app,
                    StagedUpgrade {
                        hash: staged.0.clone(),
                        torrent_id: staged.1,
                        slot_files: vec![(movie_slot(tmdb_id), staged.2)],
                    },
                    PrunePlan::Hashes(supersede),
                    idle_window,
                )
                .await?;
                info!("upgrade: tmdb {} swapped to {}", tmdb_id, staged.0);
                return Ok(());
            }
            // Candidate-specific reject (already blacklisted where warranted) → try the next best.
            Err(UpgradeSkip::NoChange(_)) => continue,
            // Transient provider/probe glitch → stop; retry the whole title next tick.
            Err(UpgradeSkip::Deferred(r)) => return Err(UpgradeSkip::Deferred(r)),
        }
    }
    Err(UpgradeSkip::NoChange(
        "no acceptable upgrade candidate".into(),
    ))
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
    prov: &crate::store::Provenance,
) -> Result<(String, String, String), UpgradeSkip> {
    let hash = cand.info_hash.clone();
    // A provider add/info failure is transient → Deferred (retry next tick, keep the cursor) rather
    // than NoChange (which would skip this title — with its real upgrade candidate — for a full wrap).
    // A candidate already in the account is never staged (NoChange → the next candidate).
    let added = add_staging_candidate(app, &hash).await?;
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
    // Unlike acquisition, an upgrade adopts ONLY a `Pass`: an `Accept` (tracks couldn't be checked —
    // unreadable container / no tracks) is dropped WITHOUT blacklisting (nothing is known to be wrong
    // with it), so an upgrade never swaps a checked copy for an unchecked one.
    match app
        .engine
        .probe_file(&fresh, &hash, &selected_path, base_req)
        .await
    {
        crate::acquire::VerifyResult::Pass => {}
        crate::acquire::VerifyResult::Accept => {
            let _ = app.provider.delete_torrent(&added.id).await;
            return Err(UpgradeSkip::NoChange("unverifiable release".into()));
        }
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
    // Record Verified with sticky provenance from the owned record we are upgrading. DEFER (don't
    // return Ok) if either record write fails: the caller repoints the selection + prunes the
    // superseded copy on Ok, so returning Ok with an unrecorded staged copy would prune against a
    // replacement that has no owned record (stranding it as an empty-provenance mirror). The staged
    // torrent is left for the account-mirror/dedup pass to reclaim; a later tick re-stages.
    // Sticky provenance = the merge across the title's current owned copies, computed ONCE by the
    // caller from records already in hand (no full owned-table re-scan) — see `merged_provenance`.
    if app
        .store
        .put_owned(
            hash.clone(),
            OwnedRecord {
                request: base_req.clone(),
                provenance: prov.clone(),
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: Some(QualitySummary::of(cand, &app.config.acquisition.prefs)),
            },
        )
        .await
        .is_err()
    {
        return Err(UpgradeSkip::Deferred(
            "staged owned-record write failed; deferring swap+prune".into(),
        ));
    }
    if app
        .store
        .put_authoritative(hash.clone(), base_req.metadata.clone())
        .await
        .is_err()
    {
        return Err(UpgradeSkip::Deferred(
            "staged authoritative write failed; deferring swap+prune".into(),
        ));
    }
    Ok((hash, added.id, selected_path))
}

/// Add `hash` to stage it as an upgrade candidate — unless its torrent is ALREADY in the provider
/// account, in which case it is never staged (`NoChange`). The ONE add path of every upgrade staging
/// site (movie [`stage_and_verify`], per-episode + pack correction via [`stage_and_await_cached`],
/// and consolidation), so the guard covers them all. Every site deletes the staged torrent by the id
/// the add returned when it rejects the candidate (not cached, wrong title, probe reject or
/// unverifiable `Accept`, …) or rolls back; on TorBox an add of an infohash already in the account
/// returns the EXISTING torrent's id, and the rankers exclude only the title's own owned hashes — so
/// without this a candidate present outside the group (identified under another title, or an
/// unidentified manual add) would be deleted, or adopted over another title's owned record. Checked
/// against a fresh listing immediately before the add (one extra `get_torrents` per staged
/// candidate); a listing failure is `Deferred`, like an add failure — presence can't be ruled out,
/// so nothing is added.
async fn add_staging_candidate(
    app: &AppState,
    hash: &str,
) -> Result<crate::rd_client::AddMagnetResponse, UpgradeSkip> {
    let listing = app
        .provider
        .get_torrents()
        .await
        .map_err(|e| UpgradeSkip::Deferred(format!("listing failed before staging: {e}")))?;
    if listing.iter().any(|t| t.hash.eq_ignore_ascii_case(hash)) {
        debug!(
            "upgrade: candidate {} is already in the provider account; not staging it",
            hash
        );
        return Err(UpgradeSkip::NoChange(
            "candidate already in the provider account".into(),
        ));
    }
    app.provider
        .add_magnet(&format!("magnet:?xt=urn:btih:{}", hash))
        .await
        .map_err(|e| UpgradeSkip::Deferred(format!("add failed: {e}")))
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

/// The merged sticky provenance across a group's owned records — preserves Manual / per-user origins
/// across an upgrade swap. Computed from records the caller ALREADY holds (the movie path builds it
/// from `owned_hashes`, the show paths from `group_hashes`) instead of re-scanning the entire `owned`
/// table; equals `group_owned_by_tmdb(..).provenance` for the same group because both fold the group's
/// records with the identical `Provenance::merge`. An empty slice (every owned copy raced away between
/// grouping and staging) folds to EMPTY provenance — a plain mirror, NOT `manual()` (which would
/// permanently mislabel an engine-staged copy as a protected manual add). NB: it builds
/// `entries: []` explicitly, since `Provenance::default()` == `manual()`.
fn merged_provenance(owned: &[(String, OwnedRecord)]) -> crate::store::Provenance {
    let mut prov = crate::store::Provenance {
        entries: Vec::new(),
    };
    for (_, r) in owned {
        prov.merge(&r.provenance);
    }
    prov
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
    pack_no_regression(&i.pack_quality, &i.owned_episode_quality)
}

/// Pure: consolidation's no-regression gate — `pack` is at least every owned episode's source tier
/// AND resolution. An UNKNOWN axis (0) is the lowest value on either side: an owned episode's unknown
/// axis never blocks a pack, and a pack's unknown axis is a regression against any known episode
/// axis. Depends only on the candidate's scraped quality, so consolidation also applies it before
/// staging (a candidate failing it would only be added, polled and deleted again).
fn pack_no_regression(
    pack: &QualitySummary,
    owned_episode_quality: &[(u32, QualitySummary)],
) -> bool {
    owned_episode_quality
        .iter()
        .all(|(_, q)| pack.source_tier >= q.source_tier && pack.resolution >= q.resolution)
}

/// Per-episode owned quality for one season, gathered from EVERY owned record that supplies an
/// episode of it — scattered singletons AND multi-episode/partial packs alike. The consolidation
/// prune below supersedes *any* in-season-covered record (not just singletons), so the
/// no-regression gate must see all of them, expanded per-episode. A contributing record with no
/// recorded quality counts as [`QualitySummary::unknown`] — both axes unknown, the lowest — so it
/// never blocks a pack (an over-ceiling copy is never unknown: its quality is re-derived from its
/// file name at tick start, and its known resolution then blocks every within-ceiling pack).
fn season_owned_quality(
    owned: &[(String, OwnedRecord)],
    season: u32,
) -> Vec<(u32, QualitySummary)> {
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
        let q = rec.quality.clone().unwrap_or_else(QualitySummary::unknown);
        for e in in_season {
            out.push((e, q.clone()));
        }
    }
    out
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
/// already owned (and not a known-unverifiable container — [`known_unverifiable`]), sorted
/// best-`score`-first under `prefs` — which callers pass as the title's [`ordering_prefs`], so a
/// wrong-language pack is tried after a correctly-tagged one (the hard filters don't depend on
/// audio; callers still judge each pack's quality on the raw prefs). So consolidation adopts the
/// BEST pack — consistent with
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
        .filter(|r| !known_unverifiable(r))
        .filter(|r| {
            !owned_hashes
                .iter()
                .any(|h| h.eq_ignore_ascii_case(&r.info_hash))
        })
        .filter_map(|r| release::score(&r, prefs).map(|s| (r, s)))
        .collect();
    scored.sort_by_key(|(_, s)| std::cmp::Reverse(*s)); // best score first
                                                        // Dedup by info_hash (same rationale as rank_changes): a duplicated pack whose first entry
                                                        // fails would otherwise be re-added/re-probed/re-blacklisted from its second entry.
    let mut seen = std::collections::HashSet::new();
    scored
        .into_iter()
        .filter(|(r, _)| seen.insert(r.info_hash.to_ascii_lowercase()))
        .map(|(r, _)| r)
        .collect()
}

// ── per-episode upgrade/downgrade + over-ceiling pack correction (Task 5) ─────────────────────

/// Add `hash`, select its video files, and poll for the cached verdict. Shared staging tail for
/// `stage_episode_candidate` and `correct_over_ceiling_packs` (episode vs pack staging differ only
/// in what they do with the resolved `TorrentInfo` afterwards — this covers exactly the add/select/
/// poll/cached-gate steps common to both). On any failure the staged torrent is deleted so nothing
/// leaks; returns the torrent id + the post-selection `TorrentInfo` on success.
async fn stage_and_await_cached(
    app: &AppState,
    hash: &str,
) -> Result<(String, crate::rd_client::TorrentInfo), UpgradeSkip> {
    let added = add_staging_candidate(app, hash).await?;
    let info = match app.provider.get_torrent_info(&added.id).await {
        Ok(i) => i,
        Err(e) => {
            let _ = app.provider.delete_torrent(&added.id).await;
            return Err(UpgradeSkip::Deferred(format!("info failed: {e}")));
        }
    };
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
                "upgrade: select_files for staged candidate {} failed: {}",
                hash, e
            );
        }
    }
    let fresh = await_downloaded(app.provider.as_ref(), &added.id)
        .await
        .unwrap_or(info);
    if fresh.status != "downloaded" {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err(UpgradeSkip::NoChange("candidate not cached".into()));
    }
    Ok((added.id, fresh))
}

/// Stage ONE per-episode candidate: add + select + poll cached (via [`stage_and_await_cached`]),
/// locate the target `(season, episode)` among the resolved files (rejecting a multi-season pack —
/// that belongs to consolidation, not the per-episode path — and a candidate that turns out to lack
/// the target episode), validate the title, and probe the same audio/subtitle gate as acquisition.
/// On success records the episode `OwnedRecord` (Verified, sticky provenance) + authoritative id and
/// returns `(hash, torrent_id, file_path)`. On any failure the staged torrent is cleaned up and the
/// current release is left untouched (non-destructive). A candidate that resolves to MORE than one
/// distinct episode is added to `multi_episode` ([`EpisodeTick::multi_episode`]): it is rejected
/// here for every target episode, so the caller skips it for the rest of the tick.
async fn stage_episode_candidate(
    app: &AppState,
    tmdb_id: u64,
    base_req: &crate::store::AcquireRequest,
    cand: &release::ReleaseInfo,
    (season, episode): (u32, u32),
    group_prov: &crate::store::Provenance,
    multi_episode: &mut std::collections::HashSet<String>,
) -> Result<(String, String, String), UpgradeSkip> {
    let hash = cand.info_hash.clone();
    let (torrent_id, fresh) = stage_and_await_cached(app, &hash).await?;
    let eps = crate::acquire::episode_files(&fresh);
    // Adopt a per-episode release ONLY when it is a TRUE singleton for the target (season, episode):
    // every parsed episode file must be exactly that episode. A multi-episode (or multi-season) pack
    // is left to the consolidation / pack-correction paths — adopting it here would record it with
    // provides=[(season, episode)] (truncated), hiding the pack's other episodes so `monitor_episodes`
    // re-acquires them as duplicate singletons and consolidation treats the pack as a singleton. That
    // is exactly the season-pack churn the `provides` mechanism exists to prevent. The per-episode
    // caller falls through to the next-best candidate on this NoChange (in-tick fall-through).
    if eps.iter().any(|(s, e, _)| *s != season || *e != episode) {
        // Two or more distinct episodes → rejected for ANY target episode, so remember it. (A
        // single-episode torrent for a DIFFERENT episode is not remembered: it is that episode's
        // valid singleton.)
        let distinct: std::collections::HashSet<(u32, u32)> =
            eps.iter().map(|(s, e, _)| (*s, *e)).collect();
        if distinct.len() > 1 {
            multi_episode.insert(hash.to_ascii_lowercase());
        }
        let _ = app.provider.delete_torrent(&torrent_id).await;
        return Err(UpgradeSkip::NoChange(
            "multi-episode/season pack; not a per-episode candidate".into(),
        ));
    }
    let Some((_, _, path)) = eps.iter().find(|(s, e, _)| *s == season && *e == episode) else {
        let _ = app.provider.delete_torrent(&torrent_id).await;
        return Err(UpgradeSkip::NoChange(
            "candidate lacks the target episode".into(),
        ));
    };
    let file_path = path.clone();
    let fname = file_path
        .rsplit('/')
        .next()
        .unwrap_or(&file_path)
        .to_string();
    if !app
        .engine
        .validate_title(
            &fname,
            tmdb_id,
            MediaKind::Series,
            Some(season),
            Some(episode),
        )
        .await
    {
        let _ = app.provider.delete_torrent(&torrent_id).await;
        let _ = app
            .store
            .blacklist_add(
                MediaKind::Series,
                tmdb_id,
                hash.clone(),
                "WrongTitle",
                now_secs(),
            )
            .await;
        return Err(UpgradeSkip::NoChange("title mismatch".into()));
    }
    let probe_req = crate::store::AcquireRequest {
        season: Some(season),
        episode: Some(episode),
        ..base_req.clone()
    };
    // Adopt only a probe `Pass` (see `stage_and_verify`): an unverifiable `Accept` is dropped
    // without blacklisting.
    match app
        .engine
        .probe_file(&fresh, &hash, &file_path, &probe_req)
        .await
    {
        crate::acquire::VerifyResult::Pass => {}
        crate::acquire::VerifyResult::Accept => {
            let _ = app.provider.delete_torrent(&torrent_id).await;
            return Err(UpgradeSkip::NoChange("unverifiable release".into()));
        }
        crate::acquire::VerifyResult::Reject(reason) => {
            let _ = app.provider.delete_torrent(&torrent_id).await;
            let _ = app
                .store
                .blacklist_add(MediaKind::Series, tmdb_id, hash.clone(), reason, now_secs())
                .await;
            return Err(UpgradeSkip::NoChange(format!("probe rejected: {reason}")));
        }
        crate::acquire::VerifyResult::Defer => {
            let _ = app.provider.delete_torrent(&torrent_id).await;
            return Err(UpgradeSkip::Deferred("probe deferred".into()));
        }
    }
    let prov = group_prov.clone();
    let _ = app
        .store
        .put_owned(
            hash.clone(),
            OwnedRecord {
                request: base_req.clone(),
                provenance: prov,
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: vec![(season, episode)],
                quality: Some(QualitySummary::of(cand, &app.config.acquisition.prefs)),
            },
        )
        .await;
    let _ = app
        .store
        .put_authoritative(hash.clone(), base_req.metadata.clone())
        .await;
    Ok((hash, torrent_id, file_path))
}

/// Pure: may the multi-episode pack `pack_hash` be PRUNED once the slot for `repointed` moves to a
/// newly-staged release? `served_by` maps each of the pack's episodes to the hash its selection slot
/// currently points at (an episode with no selection is absent); `other_provides` is the union of
/// `provides` across the group's OTHER owned records. True only when — treating `repointed` as no
/// longer served by the pack — the pack serves NO episode slot any more, AND every other episode it
/// provides is held by another owned record. Otherwise the pack is kept: it still serves an episode,
/// or deleting it would lose an episode nothing else holds. (`repointed` itself is exempt from the
/// coverage check: the staged replacement now holds it.)
fn pack_prunable_after_repoint(
    pack_hash: &str,
    pack_provides: &[(u32, u32)],
    repointed: (u32, u32),
    served_by: &std::collections::HashMap<(u32, u32), String>,
    other_provides: &[(u32, u32)],
) -> bool {
    let still_serves = pack_provides.iter().any(|ep| {
        *ep != repointed
            && served_by
                .get(ep)
                .is_some_and(|h| h.eq_ignore_ascii_case(pack_hash))
    });
    if still_serves {
        return false;
    }
    pack_provides
        .iter()
        .filter(|ep| **ep != repointed)
        .all(|ep| other_provides.contains(ep))
}

/// Pure: may phase (a2) move a slot served by a multi-episode pack of quality `pack` to `cand`? On
/// top of `is_target_improvement` (which `rank_changes` already applied — a strict score gain with a
/// higher resolution, or a higher tier at the same resolution, or Task 4's unverified rule), the
/// candidate must be no regression vs the pack on EITHER axis: source tier AND resolution at least
/// the pack's — the per-axis test [`consolidation_target`] applies. So (a2) never splits a pack into
/// singles below it on either axis (e.g. a 720p BluRay pack is not split into 1080p WEB singles).
///
/// Why no show pass undoes another: the singles (a2) adopts are at least the pack on both axes and,
/// bar the unverified rule, above it on one. They were adopted on a probe `Pass`, so they are
/// verified, and phase (a) only ever moves a verified single UP a category (a higher resolution, or a
/// higher tier at the same resolution — `is_target_improvement` never rates a same-resolution tier
/// drop an upgrade, however it scores); so consolidation's no-regression gate can never re-accept
/// the split pack against them. Singles EQUAL to the pack on both axes come only
/// from the unverified rule, and consolidation re-adopts a pack only on a probe `Pass`, recording it
/// verified, so such a pack is split and folded back at most once.
///
/// An UNKNOWN pack axis (0: untagged, or no recorded quality at all — [`QualitySummary::unknown`]) is
/// the lowest value, so any known candidate passes it; a known pack axis is still compared.
fn pack_split_no_regression(cand: &release::ReleaseInfo, pack: &QualitySummary) -> bool {
    cand.source.tier_score() >= pack.source_tier && cand.resolution.unwrap_or(0) >= pack.resolution
}

/// `(hash, provides)` for each of the show's CURRENT owned records other than `pack_hash`, re-read
/// from the store (so a single staged — or a singleton or overlapping pack pruned — earlier this
/// tick is reflected, not the tick-start snapshot). Only `Verified` records count: a `Pending` record
/// carries its target episode in `provides` from add time but may still be reaped as dead, so a pack
/// must not be pruned on the strength of it. Filters by `MediaKind::Series` so a same-id movie never
/// counts. Presence on the provider is checked later, against the swap's listing
/// ([`PackPrune::resolve`]).
async fn other_owned_coverage(
    app: &AppState,
    tmdb_id: u64,
    pack_hash: &str,
) -> Vec<(String, Vec<(u32, u32)>)> {
    app.store
        .all_owned()
        .await
        .into_iter()
        .filter(|(h, rec)| {
            !h.eq_ignore_ascii_case(pack_hash)
                && rec.request.tmdb_id == tmdb_id
                && rec.request.kind == MediaKind::Series
                && rec.status == OwnedStatus::Verified
        })
        .map(|(h, rec)| (h, rec.provides))
        .collect()
}

/// Which owned copy a per-episode swap supersedes ([`upgrade_episode_slot`]), and so what it prunes.
enum EpisodeSupersede<'a> {
    /// Phase (a): the episode's owned singleton — pruned outright once the slot is repointed.
    Singleton(&'a str),
    /// Phase (a2): the multi-episode pack currently serving the slot — pruned only when this repoint
    /// leaves it serving nothing and every other episode it provides is held by another PRESENT
    /// copy ([`pack_prunable_after_repoint`], [`PackPrune::resolve`]): coverage is re-read from the
    /// store just before the swap and checked against the swap's own provider listing.
    Pack {
        hash: &'a str,
        provides: &'a [(u32, u32)],
        served_by: &'a std::collections::HashMap<(u32, u32), String>,
    },
}

impl EpisodeSupersede<'_> {
    /// The owned copy being upgraded away from (for the downgrade-preview log).
    fn current_hash(&self) -> &str {
        match self {
            EpisodeSupersede::Singleton(h) => h,
            EpisodeSupersede::Pack { hash, .. } => hash,
        }
    }

    /// What to prune once `repointed`'s slot moves to the staged release. `adopted` = hashes adopted
    /// earlier this tick (present even while the listing lags their add).
    async fn prune_plan(
        &self,
        app: &AppState,
        tmdb_id: u64,
        repointed: (u32, u32),
        adopted: &[String],
    ) -> PrunePlan {
        match self {
            EpisodeSupersede::Singleton(h) => PrunePlan::Hashes(vec![h.to_string()]),
            EpisodeSupersede::Pack {
                hash,
                provides,
                served_by,
            } => PrunePlan::Pack(PackPrune {
                tmdb_id,
                hash: hash.to_string(),
                provides: provides.to_vec(),
                repointed,
                served_by: (*served_by).clone(),
                others: other_owned_coverage(app, tmdb_id, hash).await,
                adopted: adopted.to_vec(),
            }),
        }
    }
}

/// Per-show-tick state shared by the per-episode phases (a) and (a2) ([`upgrade_episode_slot`]).
#[derive(Default)]
struct EpisodeTick {
    /// Hashes adopted (staged + swapped in) earlier this tick. Each was confirmed downloaded by its
    /// own `get_torrent_info` at staging, so it counts as PRESENT for a pack-prune decision even while
    /// the provider listing lags the add (TorBox), and — being owned now — it is never re-staged by a
    /// later episode's scrape (on TorBox a re-add returns the SAME torrent id, so a rejection cleanup
    /// would delete the adopted torrent).
    adopted: Vec<String>,
    /// Lowercased infohashes that resolved this tick to a torrent with more than one episode file.
    /// The per-episode path adopts only a TRUE singleton, so such a torrent is rejected for EVERY
    /// target episode: skip it for the rest of the tick instead of re-staging it (add + cached poll +
    /// delete) for each later episode — Torrentio commonly lists the same season pack under every
    /// episode query.
    multi_episode: std::collections::HashSet<String>,
}

/// One episode slot to upgrade ([`upgrade_episode_slot`]): the stage request (season/episode set),
/// the baseline quality candidates were ranked against, and what a swap supersedes.
struct EpisodeTarget<'a> {
    req: &'a crate::store::AcquireRequest,
    season: u32,
    episode: u32,
    baseline: &'a QualitySummary,
    supersede: EpisodeSupersede<'a>,
}

/// Outcome of [`upgrade_episode_slot`].
enum EpisodeUpgrade {
    /// The slot was repointed to this newly-staged hash.
    Swapped(String),
    /// Nothing was adopted for this episode (candidates exhausted, a gated-off downgrade, or a
    /// transient glitch on this episode) — move on to the next episode.
    Unchanged,
    /// The library went active, or a post-stage `apply_upgrade` step deferred (it already rolled the
    /// staged copy back): stop ALL further show work this tick and signal deferral.
    Deferred,
}

/// Try `ranked` (best-first) for ONE episode slot until one is adopted: gate a downgrade on the flag,
/// re-check idle before each stage, stage a true singleton ([`stage_episode_candidate`]), then
/// idle-gated swap + prune via [`apply_upgrade`]. Shared by the per-singleton phase (a) and the
/// pack-served phase (a2), which differ only in what the swap supersedes ([`EpisodeSupersede`]).
/// Candidates already adopted, or already found to be multi-episode packs, earlier this tick
/// ([`EpisodeTick`]) are skipped without staging; an adoption is recorded in `tick.adopted`.
async fn upgrade_episode_slot(
    app: &AppState,
    tmdb_id: u64,
    target: EpisodeTarget<'_>,
    ranked: Vec<(release::ReleaseInfo, release::Improvement)>,
    group_prov: &crate::store::Provenance,
    idle_window: Duration,
    tick: &mut EpisodeTick,
) -> EpisodeUpgrade {
    let EpisodeTarget {
        req,
        season,
        episode,
        baseline,
        supersede,
    } = target;
    for (cand, imp) in ranked {
        let cand_hash = cand.info_hash.to_ascii_lowercase();
        if tick.multi_episode.contains(&cand_hash)
            || tick
                .adopted
                .iter()
                .any(|h| h.eq_ignore_ascii_case(&cand_hash))
        {
            debug!(
                "upgrade: tmdb {} s{}e{} skipping {} (already adopted, or a multi-episode pack, earlier this tick)",
                tmdb_id, season, episode, cand.info_hash
            );
            continue;
        }
        if !downgrade_allowed_or_log(app, tmdb_id, imp, supersede.current_hash(), baseline, &cand) {
            break; // direction-homogeneous list → all remaining are gated downgrades
        }
        if !app.read_activity.all_idle(idle_window).await {
            debug!(
                "upgrade: tmdb {} library active; deferring remaining show upgrade work",
                tmdb_id
            );
            // Stop ALL further staging (remaining episodes + pack correction) this tick and
            // signal deferral so the caller leaves the cursor unstamped (unchanged behaviour).
            return EpisodeUpgrade::Deferred;
        }
        match stage_episode_candidate(
            app,
            tmdb_id,
            req,
            &cand,
            (season, episode),
            group_prov,
            &mut tick.multi_episode,
        )
        .await
        {
            Ok((new_hash, tid, path)) => {
                let prune = supersede
                    .prune_plan(app, tmdb_id, (season, episode), &tick.adopted)
                    .await;
                match apply_upgrade(
                    app,
                    StagedUpgrade {
                        hash: new_hash.clone(),
                        torrent_id: tid,
                        slot_files: vec![(
                            crate::store::episode_slot(tmdb_id, season, episode),
                            path,
                        )],
                    },
                    prune,
                    idle_window,
                )
                .await
                {
                    Ok(()) => {
                        info!(
                            "upgrade: tmdb {} s{}e{} swapped to {}",
                            tmdb_id, season, episode, new_hash
                        );
                        tick.adopted.push(new_hash.clone());
                        return EpisodeUpgrade::Swapped(new_hash); // → the next episode
                    }
                    // apply_upgrade already rolled back the staged copy. A post-stage defer
                    // (library became active, or the prune listing/selection write failed) must
                    // propagate so the caller leaves the round-robin cursor unstamped and this
                    // work is reconsidered next tick — NOT swallowed as a false "swapped".
                    Err(_) => return EpisodeUpgrade::Deferred,
                }
            }
            // Candidate-specific reject → try the next-best candidate for THIS episode.
            Err(UpgradeSkip::NoChange(reason)) => {
                debug!(
                    "upgrade: tmdb {} s{}e{} no change: {}",
                    tmdb_id, season, episode, reason
                );
                continue;
            }
            // Transient glitch on this episode → give up this episode this tick (move to the
            // next episode), matching the prior per-episode fire-and-forget behaviour.
            Err(UpgradeSkip::Deferred(reason)) => {
                debug!(
                    "upgrade: tmdb {} s{}e{} deferred: {}",
                    tmdb_id, season, episode, reason
                );
                return EpisodeUpgrade::Unchanged;
            }
        }
    }
    EpisodeUpgrade::Unchanged
}

/// Replace an owned season pack that is now ABOVE the ceiling (e.g. acquired before the ceiling was
/// lowered, or mirrored from a pre-existing library) with a within-ceiling cached pack that covers
/// the SAME episodes, repointing every covered episode slot and pruning the old pack. Gated as a
/// downgrade (`ALLOW_RESOLUTION_DOWNGRADE`). Keeps the old pack when no conforming replacement
/// exists — a sole over-ceiling copy is never deleted with nothing to replace it.
///
/// `exclude_hashes` are never staged as a replacement: every hash the show owns NOW (not just in the
/// tick-start `owned` snapshot) — a single adopted earlier this tick is listed under the season's
/// episode-1 scrape like any stream, and on TorBox a re-add of an owned infohash returns the SAME
/// torrent id, so the coverage-miss cleanup below would delete the adopted torrent out from under its
/// record and selection.
///
/// Returns `true` if it DEFERRED mid-tick — the library became active (a non-idle `all_idle`
/// check) OR a post-stage `apply_upgrade` step failed (a provider-listing fetch or a `put_selection`
/// write) — so the caller can leave the round-robin cursor unstamped and reconsider next tick;
/// `false` on normal completion (whether or not a correction was applied).
async fn correct_over_ceiling_packs(
    app: &AppState,
    tmdb_id: u64,
    owned: &[(String, OwnedRecord)],
    exclude_hashes: &[String],
    imdb_id: &str,
    idle_window: Duration,
) -> bool {
    let prefs = &app.config.acquisition.prefs;
    let ceiling = prefs.max_resolution.height();
    // Merge the group's provenance once from the records in hand (no per-pack owned-table re-scan).
    let group_prov = merged_provenance(owned);
    // Fetch the per-title blacklist ONCE and test membership in memory (mirrors `rank_changes`),
    // rather than one awaited redb read per ranked candidate.
    let blacklist = app
        .store
        .blacklisted_hashes_for(MediaKind::Series, tmdb_id)
        .await;
    for (pack_hash, rec) in owned.iter().filter(|(_, r)| r.provides.len() > 1) {
        let Some(pack_q) = rec.quality.clone() else {
            // No recorded quality (its file name is unknown even after the tick-start backfill) →
            // not known to be over the ceiling: unknown-lowest, handled by phase (a2), not here.
            continue;
        };
        if pack_q.resolution <= ceiling {
            continue; // within ceiling → not a correction target
        }
        let season = rec.provides[0].0;
        if rec.provides.iter().any(|(s, _)| *s != season) {
            continue; // single-season packs only
        }
        let need: Vec<u32> = rec.provides.iter().map(|(_, e)| *e).collect();
        // The exact episode set the OLD over-ceiling pack supplied. The replacement must cover all of
        // these, but we only repoint/claim THESE — never the extra episodes a bigger replacement pack
        // happens to also contain (an extra episode may have a better within-ceiling singleton whose
        // slot must not be dragged down to this pack; see the `provides`/`slot_files` filters below).
        let need_set: std::collections::HashSet<u32> = need.iter().copied().collect();

        let Ok(raws) = app
            .scraper
            .find(imdb_id, MediaKind::Series, Some(season), Some(1))
            .await
        else {
            continue;
        };
        let order = ordering_prefs(app, &rec.request);
        for cand in rank_cached_pack_candidates(&raws, &order, exclude_hashes) {
            if blacklist.contains(&cand.info_hash.to_ascii_lowercase()) {
                continue;
            }
            let cand_q = QualitySummary::of(&cand, prefs);
            let imp = release::is_target_improvement(&pack_q, &cand_q, prefs);
            if imp == release::Improvement::None {
                continue;
            }
            if !downgrade_allowed_or_log(app, tmdb_id, imp, pack_hash, &pack_q, &cand) {
                break; // logged (dry-run); no candidate below would fare better — stop this pack
            }
            if !app.read_activity.all_idle(idle_window).await {
                return true; // library active; defer — leave the cursor unstamped, retry next tick
            }

            let (torrent_id, fresh) = match stage_and_await_cached(app, &cand.info_hash).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            let eps = crate::acquire::episode_files(&fresh);
            if eps.iter().any(|(s, _, _)| *s != season) {
                let _ = app.provider.delete_torrent(&torrent_id).await;
                continue; // multi-season pack; not a clean single-season replacement
            }
            if !need.iter().all(|e| eps.iter().any(|(_, pe, _)| pe == e)) {
                let _ = app.provider.delete_torrent(&torrent_id).await;
                continue; // must cover every episode the old pack provided
            }
            let Some((rs, re, rpath)) = eps.iter().find(|(s, _, _)| *s == season) else {
                let _ = app.provider.delete_torrent(&torrent_id).await;
                continue;
            };
            let fname = rpath.rsplit('/').next().unwrap_or(rpath).to_string();
            if !app
                .engine
                .validate_title(&fname, tmdb_id, MediaKind::Series, Some(*rs), Some(*re))
                .await
            {
                let _ = app.provider.delete_torrent(&torrent_id).await;
                let _ = app
                    .store
                    .blacklist_add(
                        MediaKind::Series,
                        tmdb_id,
                        cand.info_hash.clone(),
                        "WrongTitle",
                        now_secs(),
                    )
                    .await;
                continue;
            }
            let probe_req = crate::store::AcquireRequest {
                imdb_id: imdb_id.to_string(),
                season: Some(*rs),
                episode: Some(*re),
                ..rec.request.clone()
            };
            // Adopt only a probe `Pass` (see `stage_and_verify`): an unverifiable `Accept` is
            // dropped without blacklisting and the next candidate is tried.
            match app
                .engine
                .probe_file(&fresh, &cand.info_hash, rpath, &probe_req)
                .await
            {
                crate::acquire::VerifyResult::Pass => {}
                crate::acquire::VerifyResult::Accept => {
                    let _ = app.provider.delete_torrent(&torrent_id).await;
                    debug!(
                        "upgrade: tmdb {} s{} pack candidate {} unverifiable; not adopted",
                        tmdb_id, season, cand.info_hash
                    );
                    continue;
                }
                crate::acquire::VerifyResult::Reject(reason) => {
                    let _ = app.provider.delete_torrent(&torrent_id).await;
                    let _ = app
                        .store
                        .blacklist_add(
                            MediaKind::Series,
                            tmdb_id,
                            cand.info_hash.clone(),
                            reason,
                            now_secs(),
                        )
                        .await;
                    continue;
                }
                crate::acquire::VerifyResult::Defer => {
                    let _ = app.provider.delete_torrent(&torrent_id).await;
                    continue;
                }
            }
            let prov = group_prov.clone();
            let base_req = crate::store::AcquireRequest {
                imdb_id: imdb_id.to_string(),
                ..rec.request.clone()
            };
            // Claim/repoint ONLY the old pack's episodes (`need_set`), not every episode this
            // replacement pack contains — so an extra episode with a better within-ceiling singleton
            // keeps its own (better) selection and isn't silently downgraded onto this pack.
            let provides: Vec<(u32, u32)> = eps
                .iter()
                .filter(|(s, e, _)| *s == season && need_set.contains(e))
                .map(|(s, e, _)| (*s, *e))
                .collect();
            let _ = app
                .store
                .put_owned(
                    cand.info_hash.clone(),
                    OwnedRecord {
                        request: base_req.clone(),
                        provenance: prov,
                        added_at: now_secs(),
                        status: OwnedStatus::Verified,
                        provides,
                        quality: Some(cand_q.clone()),
                    },
                )
                .await;
            let _ = app
                .store
                .put_authoritative(cand.info_hash.clone(), base_req.metadata.clone())
                .await;
            let slot_files: Vec<(String, String)> = eps
                .iter()
                .filter(|(s, e, _)| *s == season && need_set.contains(e))
                .map(|(s, e, p)| (crate::store::episode_slot(tmdb_id, *s, *e), p.clone()))
                .collect();
            match apply_upgrade(
                app,
                StagedUpgrade {
                    hash: cand.info_hash.clone(),
                    torrent_id,
                    slot_files,
                },
                PrunePlan::Hashes(vec![pack_hash.clone()]),
                idle_window,
            )
            .await
            {
                Ok(()) => {
                    info!(
                        "upgrade: tmdb {} s{} pack corrected to within-ceiling {}",
                        tmdb_id, season, cand.info_hash
                    );
                    break; // one correction per pack per tick
                }
                // apply_upgrade already rolled back the staged pack. A post-stage defer (library
                // became active, or the prune listing/selection write failed) must propagate so the
                // caller leaves the round-robin cursor unstamped and this pack is reconsidered next
                // tick — NOT swallowed as a false "pack corrected".
                Err(_) => return true,
            }
        }
    }
    false // completed the pass without a mid-tick library-active defer
}

/// The per-singleton-episode upgrade (a) + pack-served-episode upgrade (a2) +
/// over-ceiling-pack-correction (b) body of `try_upgrade_show`,
/// WITHOUT the TMDB-driven consolidation tail (c) — split out so it is unit-testable fully offline,
/// mirroring the `try_consolidate_show` / `consolidate_show_seasons` split just below (consolidation's
/// live TMDB aired-episode lookup is exercised by the live smoke, not unit tests; see that split's
/// doc comment). Fire-and-forget per item: one episode/pack's per-candidate outcome
/// (Deferred/NoChange/staged) never aborts the rest of the group — every branch already classifies +
/// logs its own outcome.
///
/// Returns `true` if it DEFERRED mid-tick — the library became active (a non-idle `all_idle` check,
/// in the per-episode loops or in `correct_over_ceiling_packs`) OR a post-stage `apply_upgrade` step
/// failed (a provider-listing fetch or a `put_selection` write). The caller uses this to signal
/// `Deferred` so the round-robin cursor is NOT stamped and the skipped work is reconsidered next
/// tick (rather than waiting a full cursor wrap). `false` on normal completion.
async fn upgrade_show_episodes_and_packs(
    app: &AppState,
    tmdb_id: u64,
    group_hashes: &[String],
    idle_window: Duration,
) -> bool {
    let mut owned: Vec<(String, OwnedRecord)> = Vec::new();
    for h in group_hashes {
        if let Some(r) = app.store.get_owned(h.clone()).await {
            owned.push((h.clone(), r));
        }
    }
    // Torrentio is IMDB-keyed: without an id anywhere in the group there is nothing to scrape.
    // Consolidation performs the identical check independently, so it self-skips too. Not a
    // deferral — there is no work to reconsider — so this returns `false`.
    let Some(imdb_id) = group_imdb_id(app, group_hashes).await else {
        return false;
    };
    // Merge the group's sticky provenance ONCE from the records already fetched above, instead of
    // re-scanning the whole owned table per staged episode (see `merged_provenance`).
    let group_prov = merged_provenance(&owned);
    // What phases (a) and (a2) adopted / found to be multi-episode packs so far this tick.
    let mut tick = EpisodeTick::default();

    // (a) Per-singleton-episode upgrade/downgrade.
    let ceiling = app.config.acquisition.prefs.max_resolution.height();
    for (hash, rec) in owned.iter().filter(|(_, r)| r.provides.len() == 1) {
        // No recorded quality → unknown, the lowest: any known cached candidate improves on it.
        let q = rec.quality.clone().unwrap_or_else(QualitySummary::unknown);
        let (season, episode) = rec.provides[0];
        // Corrective-downgrade guard, mirroring the movie path's `served_slot_is_retained_over_ceiling`:
        // this per-episode upgrade baselines on the SINGLETON's own quality, not on what the episode
        // slot currently serves. If this singleton is within-ceiling (so any adopted candidate is
        // rated `Improvement::Upgrade`, bypassing the downgrade gate) yet the slot points at a
        // separate retained OVER-ceiling copy (e.g. a co-providing pack, ALLOW_RESOLUTION_DOWNGRADE
        // off), repointing the slot to the singleton's "upgrade" would move the SERVED resolution
        // DOWN — a corrective downgrade the contract reserves for the gated path. Skip this episode.
        // (Rare — it needs owning BOTH a within-ceiling singleton AND an over-ceiling pack for the
        // same episode with the slot on the pack — but no longer left unguarded.)
        if q.resolution <= ceiling
            && served_slot_is_retained_over_ceiling(
                app,
                &crate::store::episode_slot(tmdb_id, season, episode),
                group_hashes,
            )
            .await
        {
            debug!(
                "upgrade: tmdb {} s{}e{} served copy is over-ceiling and retained; not repointing (corrective downgrade gated off)",
                tmdb_id, season, episode
            );
            continue;
        }
        // The swap repoints the episode slot, so judge candidates against what the slot SERVES too:
        // when it points at another owned copy of this show (e.g. a better pack that also provides
        // the episode), the baseline is the better of the two (`best_owned_quality`: known before
        // unknown, then effective score). Otherwise a candidate that beats only this singleton — any
        // known one, when the singleton is unknown — would move the slot off a better pack.
        let served_other = app
            .store
            .get_selection(crate::store::episode_slot(tmdb_id, season, episode))
            .await
            .filter(|sel| !sel.hash.eq_ignore_ascii_case(hash))
            .and_then(|sel| {
                group_hashes
                    .iter()
                    .find(|h| h.eq_ignore_ascii_case(&sel.hash))
                    .cloned()
            });
        let q = match served_other {
            Some(other) => best_owned_quality(app, &[hash.clone(), other])
                .await
                .unwrap_or(q),
            None => q,
        };
        let req = crate::store::AcquireRequest {
            imdb_id: imdb_id.clone(),
            season: Some(season),
            episode: Some(episode),
            ..rec.request.clone()
        };
        // COST NOTE: this issues ONE scraper call per owned singleton episode per tick (not bounded by
        // UPGRADE_BUDGET_PER_TICK, which counts titles). A heavily-scattered season held as N
        // never-consolidated singletons produces an N-scrape burst for one title; the shared rate
        // limiter absorbs it (it's paced, not an error), but it dominates that title's tick until
        // consolidation folds the singletons into a pack.
        let raws = match app
            .scraper
            .find(&imdb_id, MediaKind::Series, Some(season), Some(episode))
            .await
        {
            Ok(r) => r,
            Err(_) => continue, // transient; other episodes/packs/consolidation still run this tick
        };
        let ranked = rank_changes(
            app,
            MediaKind::Series,
            tmdb_id,
            &raws,
            &q,
            group_hashes,
            &req,
        )
        .await;
        let target = EpisodeTarget {
            req: &req,
            season,
            episode,
            baseline: &q,
            supersede: EpisodeSupersede::Singleton(hash),
        };
        if let EpisodeUpgrade::Deferred = upgrade_episode_slot(
            app,
            tmdb_id,
            target,
            ranked,
            &group_prov,
            idle_window,
            &mut tick,
        )
        .await
        {
            return true;
        }
    }

    // (a2) Episodes served from a WITHIN-ceiling multi-episode pack (e.g. an SD/foreign-dub pack
    // grabbed on an air date while the season's other episodes are better singletons). Each episode
    // whose selection slot points at the pack is re-scored against the PACK's quality and, on a
    // better cached TRUE singleton at the same-or-better source tier AND resolution
    // ([`pack_split_no_regression`] — consolidation's no-regression test; with phase (a) never
    // dropping a single's tier at the same resolution, consolidation can't fold the split pack
    // back), swapped exactly like phase (a). The pack itself is pruned only on
    // the repoint that leaves it serving no slot AND with every episode it provides held by another
    // owned record that is PRESENT on the provider (`PackPrune::resolve`, decided inside
    // `apply_upgrade` against the swap's own listing); otherwise it is kept. Over-ceiling packs are
    // phase (b)'s. Sample-first cost bound: if the pack's first served episode yields no improving
    // candidate (or its scrape fails), the rest of that pack is skipped this visit — one scrape per
    // unimprovable pack, not one per episode it serves. A candidate that turns out to be a
    // multi-episode pack is staged at most once per tick (`EpisodeTick::multi_episode`), not once
    // per served episode.
    for (pack_hash, rec) in owned.iter().filter(|(_, r)| r.provides.len() > 1) {
        // No recorded quality → unknown on both axes, the lowest (see `pack_split_no_regression`).
        let q = rec.quality.clone().unwrap_or_else(QualitySummary::unknown);
        if q.resolution > ceiling {
            continue; // over-ceiling → pack correction (b)
        }
        // Read the pack's slots NOW (not at tick start): phase (a) above may already have moved one.
        let mut served_by: std::collections::HashMap<(u32, u32), String> =
            std::collections::HashMap::new();
        for &(s, e) in &rec.provides {
            if let Some(sel) = app
                .store
                .get_selection(crate::store::episode_slot(tmdb_id, s, e))
                .await
            {
                served_by.insert((s, e), sel.hash);
            }
        }
        let mut served: Vec<(u32, u32)> = served_by
            .iter()
            .filter(|(_, h)| h.eq_ignore_ascii_case(pack_hash))
            .map(|(ep, _)| *ep)
            .collect();
        served.sort_unstable();
        for (i, &(season, episode)) in served.iter().enumerate() {
            let req = crate::store::AcquireRequest {
                imdb_id: imdb_id.clone(),
                season: Some(season),
                episode: Some(episode),
                ..rec.request.clone()
            };
            let mut ranked = match app
                .scraper
                .find(&imdb_id, MediaKind::Series, Some(season), Some(episode))
                .await
            {
                Ok(raws) => {
                    rank_changes(
                        app,
                        MediaKind::Series,
                        tmdb_id,
                        &raws,
                        &q,
                        group_hashes,
                        &req,
                    )
                    .await
                }
                Err(_) => Vec::new(), // transient; treated as "nothing better" for this episode
            };
            // Only a candidate consolidation would also judge no-regression vs the pack (tier AND
            // resolution) — filtered before the sample-first check, so a pack whose only "better"
            // singles are lower-tier counts as unimprovable.
            ranked.retain(|(cand, _)| pack_split_no_regression(cand, &q));
            if ranked.is_empty() {
                if i == 0 {
                    debug!(
                        "upgrade: tmdb {} pack {} s{}e{} (sample) has no improving candidate; skipping the rest of the pack this visit",
                        tmdb_id, pack_hash, season, episode
                    );
                    break;
                }
                continue;
            }
            let target = EpisodeTarget {
                req: &req,
                season,
                episode,
                baseline: &q,
                supersede: EpisodeSupersede::Pack {
                    hash: pack_hash,
                    provides: &rec.provides,
                    served_by: &served_by,
                },
            };
            match upgrade_episode_slot(
                app,
                tmdb_id,
                target,
                ranked,
                &group_prov,
                idle_window,
                &mut tick,
            )
            .await
            {
                EpisodeUpgrade::Deferred => return true,
                EpisodeUpgrade::Swapped(new_hash) => {
                    served_by.insert((season, episode), new_hash);
                }
                EpisodeUpgrade::Unchanged => {}
            }
        }
    }

    // (b) Over-ceiling pack correction (flag-gated): replace a pack above the ceiling with a
    // within-ceiling cached full-season replacement that covers the SAME episodes; keeps the pack
    // when no conforming replacement exists (sole-copy safety). The per-episode loops above return
    // early on a mid-tick defer, so reaching here means they completed; propagate the pack pass's own
    // defer signal (equivalent to `episode_deferred || pack_deferred` with episode_deferred=false).
    // Its candidate exclusion is every hash the show owns NOW (re-read: singles adopted above, or a
    // record added concurrently), not just the tick-start snapshot — see `correct_over_ceiling_packs`.
    let mut exclude: Vec<String> = group_hashes.to_vec();
    exclude.extend(current_show_hashes(app, tmdb_id).await);
    correct_over_ceiling_packs(app, tmdb_id, &owned, &exclude, &imdb_id, idle_window).await
}

/// Show dispatch target for `run_upgrade_once`: per-episode upgrade/downgrade (singletons, then
/// episodes served from a within-ceiling pack) + over-ceiling pack correction
/// (`upgrade_show_episodes_and_packs`), then full-season consolidation (unchanged —
/// `try_consolidate_show` still runs every tick exactly as before this task, so its existing
/// behaviour/tests are preserved).
///
/// If the per-episode/pack phase deferred mid-tick (library became active), return `Deferred`
/// REGARDLESS of consolidation's own result, so `run_upgrade_once` leaves the round-robin cursor
/// unstamped and the skipped episodes/pack-correction are reconsidered next tick rather than
/// waiting a full cursor wrap (a `NoChange`/`Ok` from a scatter-free consolidation would otherwise
/// stamp it). When the phase did NOT defer, propagate consolidation's own `Result`. Consolidation is
/// SKIPPED (its live TMDB + per-season scrapes would be wasted) when the phase deferred AND the
/// library is still active — since consolidation's own idle gate would discard the work anyway; a
/// defer with an idle library (e.g. a transient listing failure) still lets consolidation run.
async fn try_upgrade_show(
    app: &AppState,
    tmdb_id: u64,
    group_hashes: &[String],
    idle_window: Duration,
) -> Result<(), UpgradeSkip> {
    let deferred = upgrade_show_episodes_and_packs(app, tmdb_id, group_hashes, idle_window).await;
    // If the episode/pack phase deferred because the library went ACTIVE mid-tick, consolidation's
    // own idle gate would discard its swap/prune — so skip its network calls (aired-episode lookup +
    // per-season scrapes) entirely rather than issuing them just to throw the result away.
    if deferred && !app.read_activity.all_idle(idle_window).await {
        return Err(UpgradeSkip::Deferred(
            "show per-episode/pack upgrade deferred mid-tick (library active); skipping consolidation"
                .into(),
        ));
    }
    // Re-derive the CURRENT group membership from the store before consolidation. The per-episode
    // phase above can stage a NEW hash and prune the OLD singleton within THIS tick, so the
    // tick-start `group_hashes` snapshot is stale: consolidating against it would miss a
    // just-upgraded episode (invisible to consolidation's no-regression gate → a lower-quality pack
    // silently regresses it, and the freshly-staged higher-quality copy is orphaned → later deleted
    // by dedup). Feeding consolidation the fresh set makes `season_owned_quality` see the upgraded
    // episode, so the no-regression gate correctly rejects a lower-quality pack.
    let fresh_hashes = current_show_hashes(app, tmdb_id).await;
    let consolidated = try_consolidate_show(app, tmdb_id, &fresh_hashes, idle_window).await;
    if deferred {
        return Err(UpgradeSkip::Deferred(
            "show per-episode/pack upgrade deferred mid-tick (library active)".into(),
        ));
    }
    consolidated
}

/// Re-derive the CURRENT owned hashes for a show group `(Series, tmdb_id)` from the store, lowercased.
/// Used to refresh the consolidation view after the per-episode upgrade phase mutates the group's
/// membership within a tick (see [`try_upgrade_show`]). Filters by `MediaKind::Series` so a movie
/// sharing the numeric `tmdb_id` is never folded into the show's set.
async fn current_show_hashes(app: &AppState, tmdb_id: u64) -> Vec<String> {
    app.store
        .all_owned()
        .await
        .into_iter()
        .filter(|(_, rec)| rec.request.tmdb_id == tmdb_id && rec.request.kind == MediaKind::Series)
        .map(|(hash, _)| hash.to_ascii_lowercase())
        .collect()
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

    // Fix B: hoist provenance merge outside the per-season loop. Merged from the `owned` records
    // already in hand (no `owned`-table re-scan) — see `merged_provenance`.
    let prov = merged_provenance(owned);
    // Fetch the per-title blacklist ONCE and test membership in memory (mirrors `rank_changes`),
    // rather than one awaited redb read per ranked candidate across every season.
    let blacklist = app
        .store
        .blacklisted_hashes_for(MediaKind::Series, tmdb_id)
        .await;
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
        // prune below supersedes any of them); an unknown-quality contributor is the lowest.
        let season_owned_q = season_owned_quality(owned, season);

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
        let order = ordering_prefs(app, &sample.request);
        for r in rank_cached_pack_candidates(&raws, &order, group_hashes) {
            if blacklist.contains(&r.info_hash.to_ascii_lowercase()) {
                continue;
            }
            // `consolidation_target`'s no-regression half needs only the scraped quality: skip a
            // candidate below an owned episode's tier or resolution BEFORE staging it (e.g. the
            // lower-tier pack a season's singles were split from, or a single listed under the E1
            // query) instead of adding, polling and deleting it on every visit.
            let pack_quality = QualitySummary::of(&r, &app.config.acquisition.prefs);
            if !pack_no_regression(&pack_quality, &season_owned_q) {
                continue;
            }

            // Pre-stage idle gate (mirrors the movie / per-episode / pack-correction paths): don't
            // add + select + probe (a 4 MB CDN fetch) a pack that the post-stage idle re-check would
            // only roll back if the library went active. This also short-circuits the wasted staging
            // when the per-episode phase already deferred this tick for the same reason.
            if !app.read_activity.all_idle(idle_window).await {
                return Err(UpgradeSkip::Deferred(
                    "library active; skip consolidation staging".into(),
                ));
            }

            // Stage: add + resolve + SE-map files. A pack already in the provider account is never
            // staged (see `add_staging_candidate`); it and a failed add both move on to the next pack.
            let Ok(added) = add_staging_candidate(app, &r.info_hash).await else {
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
                pack_quality,
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
                // Adopt only a probe `Pass` (see `stage_and_verify`): an unverifiable `Accept` is
                // dropped without blacklisting and the next candidate pack is tried.
                match app
                    .engine
                    .probe_file(&fresh, &r.info_hash, path, &probe_req)
                    .await
                {
                    crate::acquire::VerifyResult::Pass => {}
                    crate::acquire::VerifyResult::Accept => {
                        let _ = app.provider.delete_torrent(&added.id).await;
                        debug!(
                            "consolidate: tmdb {} s{} pack {} unverifiable; not adopted",
                            tmdb_id, season, r.info_hash
                        );
                        continue;
                    }
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
            // Record the pack's owned + authoritative rows FIRST, and DEFER the repoint+prune if
            // either write fails: pruning the scattered episodes against an unrecorded pack would
            // strand the pack as an empty-provenance mirror (losing its sticky provenance) while the
            // originals are gone. The pack torrent is already added, so a deferred tick re-runs the
            // idempotent consolidation; nothing is lost.
            if app
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
                .await
                .is_err()
            {
                return Err(UpgradeSkip::Deferred(format!(
                    "s{season} pack owned-record write failed; deferring repoint+prune"
                )));
            }
            if app
                .store
                .put_authoritative(r.info_hash.clone(), sample.request.metadata.clone())
                .await
                .is_err()
            {
                return Err(UpgradeSkip::Deferred(format!(
                    "s{season} pack authoritative write failed; deferring repoint+prune"
                )));
            }
            // Repoint every episode slot of this season to the pack. If any repoint fails to
            // persist, DEFER the prune below: deleting a scattered episode whose slot still points at
            // the old (about-to-be-deleted) hash would break that episode's playback. Nothing is lost
            // — every episode is still served by its (un-pruned) scattered hash, and the pack is
            // already recorded owned. NOTE: once the pack owned-record has persisted, `already_pack`
            // short-circuits this season on subsequent ticks, so consolidation does NOT re-run the
            // failed repoint; recovery of the now-redundant scattered records is via the
            // duplicate-dedup pass. (A put_selection failure is a rare redb-write/disk error.)
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
            spoken_languages: Some(vec![]),
            metadata: movie_meta(),
        }
    }
    /// Like `movie_req`, but parameterised over tmdb/imdb id — used by the fall-through tests, which
    /// exercise distinct titles (`movie_req()`'s fixed 27205/"tt1" is shared by many other tests).
    fn movie_req_for(tmdb_id: u64, imdb_id: &str) -> AcquireRequest {
        AcquireRequest {
            imdb_id: imdb_id.into(),
            tmdb_id,
            ..movie_req()
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

    /// A prober whose verdict is `VerifyResult::Pass` for any language requirement: one audio track
    /// with no language tag (inconclusive, never a positive violation) and no subtitle requirement.
    fn passing_prober() -> Arc<dyn crate::acquire::Prober> {
        Arc::new(CannedProber(Ok(vec![Track {
            kind: TrackKind::Audio,
            language: None,
        }])))
    }

    /// Default app: a PASSING prober (`VerifyResult::Pass`) so the staging probe gate is satisfied
    /// without reaching a real CDN. (Upgrade staging adopts only a `Pass` — an unverifiable `Accept`
    /// is rejected — so tests that exercise that rejection use `app_with_prober` with
    /// [`unverifiable_prober`].)
    fn app_with(
        scraper: Arc<dyn Scraper>,
        provider: Arc<dyn DebridProvider>,
        store: Store,
    ) -> AppState {
        app_with_prober(scraper, provider, store, passing_prober())
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
            read_activity: Arc::new(crate::read_activity::ReadActivity::without_startup_grace()),
        }
    }

    #[tokio::test]
    async fn current_show_hashes_reflects_store_and_excludes_same_id_movie() {
        // Guards the F1 fix: consolidation must see the CURRENT show membership, not a stale
        // tick-start snapshot. The helper reflects live store state and excludes a movie sharing
        // the numeric tmdb id.
        let store = mem_store();
        let series_req = |tmdb: u64| AcquireRequest {
            imdb_id: "tt9".into(),
            tmdb_id: tmdb,
            kind: MediaKind::Series,
            season: Some(1),
            episode: Some(1),
            original_language: Some("eng".into()),
            spoken_languages: Some(vec![]),
            metadata: movie_meta(),
        };
        let rec = |req: AcquireRequest| OwnedRecord {
            request: req,
            provenance: Provenance::watchlist("a"),
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![(1, 1)],
            quality: None,
        };
        store
            .put_owned("se1".into(), rec(series_req(100)))
            .await
            .unwrap();
        store
            .put_owned("se2".into(), rec(series_req(100)))
            .await
            .unwrap();
        // A MOVIE sharing tmdb 100 must NOT be folded into the show set.
        store
            .put_owned(
                "mov".into(),
                OwnedRecord {
                    request: movie_req_for(100, "tt1"),
                    ..rec(series_req(100))
                },
            )
            .await
            .unwrap();
        // A different show id.
        store
            .put_owned("other".into(), rec(series_req(200)))
            .await
            .unwrap();

        let app = app_with(
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(MockProvider::default()),
            store.clone(),
        );
        let mut got = current_show_hashes(&app, 100).await;
        got.sort();
        assert_eq!(got, vec!["se1".to_string(), "se2".to_string()]);

        // Reflects a mid-tick membership change: prune se1, stage a fresh higher-quality copy.
        store.remove_owned("se1".into()).await.unwrap();
        store
            .put_owned("se1new".into(), rec(series_req(100)))
            .await
            .unwrap();
        let mut got2 = current_show_hashes(&app, 100).await;
        got2.sort();
        assert_eq!(got2, vec!["se1new".to_string(), "se2".to_string()]);
    }

    #[test]
    fn merged_provenance_of_a_vanished_group_is_empty_not_manual() {
        // If every owned copy of a title is concurrently pruned between grouping and staging, the
        // staged upgrade must inherit EMPTY provenance (a plain mirror) — NOT manual(), which would
        // permanently mislabel an engine-staged copy as a protected manual add (run_upgrade_once
        // skips a manual group forever, and dedup never auto-removes a manual copy). merged_provenance
        // over no records folds to empty (it builds `entries: []`, not Provenance::default() ==
        // manual()) — the same fallback the movie/show upgrade paths rely on.
        let prov = merged_provenance(&[]);
        assert!(
            prov.entries.is_empty(),
            "a vanished group must fall back to EMPTY provenance, got {:?}",
            prov.entries
        );
        assert!(
            !prov.has_manual_entry(),
            "must not mislabel the staged upgrade as a protected manual add"
        );
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
                        unverified: false,
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
        let new_rec = store
            .get_owned("hnew".into())
            .await
            .expect("new release recorded owned+verified");
        assert_eq!(
            new_rec.provenance,
            Provenance::watchlist("a"),
            "the staged upgrade inherits the superseded copy's sticky provenance"
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

    /// Start-up grace: an upgrade tick run right after the process starts (a fresh tracker, nothing
    /// read yet) finds the library NOT idle for the first idle window, so nothing is staged, swapped
    /// or pruned and the round-robin cursor is left unstamped. (The scheduler therefore holds the
    /// first pass back until the grace is over — `scheduler::upgrade_first_run_delay`.)
    #[tokio::test]
    async fn upgrade_tick_defers_during_the_startup_grace() {
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
                    quality: None,
                },
            )
            .await
            .unwrap();
        let (mut app, added, deleted) = unknown_movie_app(&store, vec![remux_candidate()]);
        app.read_activity = Arc::new(crate::read_activity::ReadActivity::new());
        run_upgrade_once(&app).await;
        assert!(
            added.lock().unwrap().is_empty(),
            "nothing staged during the grace"
        );
        assert!(
            deleted.lock().unwrap().is_empty(),
            "nothing pruned during the grace"
        );
        assert!(store.get_owned("hold".into()).await.is_some());
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 27205).await,
            0,
            "deferred → cursor unstamped"
        );
    }

    #[tokio::test]
    async fn movie_upgrade_keeps_an_over_ceiling_copy_when_downgrade_flag_off() {
        // Mixed group at a 1080 ceiling: a within-ceiling WEB 1080p copy ("hold", the served copy)
        // and an over-ceiling 2160p engine copy ("h2160", acquired under a previously-higher
        // ceiling, so it carries a KNOWN quality). A cached REMUX 1080p upgrade is available.
        // best_owned_quality picks the within-ceiling copy as the baseline (the 2160p is
        // effective-score-penalised), so the REMUX is an Upgrade — and the prune-all would DELETE
        // the 2160p copy as collateral, a corrective downgrade the contract reserves for
        // ALLOW_RESOLUTION_DOWNGRADE. With the flag OFF, the 2160p copy must be KEPT.
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
                        unverified: false,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_owned(
                "h2160".into(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 8_000,
                        resolution: 2160,
                        score: 1_008_000,
                        unverified: false,
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
                    id: "t2160".into(),
                    hash: "h2160".into(),
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
        assert!(!app.config.allow_resolution_downgrade);

        let rec = store.get_owned("hold".into()).await.unwrap();
        let r = try_upgrade_movie(
            &app,
            27205,
            &["h2160".to_string(), "hold".to_string()],
            &rec,
            Duration::from_secs(0),
        )
        .await;
        assert!(r.is_ok(), "the within-ceiling upgrade proceeds");
        assert!(
            store.get_owned("hnew".into()).await.is_some(),
            "the REMUX upgrade is adopted"
        );
        assert!(
            store.get_owned("hold".into()).await.is_none(),
            "the superseded within-ceiling copy is pruned"
        );
        assert!(
            store.get_owned("h2160".into()).await.is_some(),
            "the over-ceiling copy must NOT be pruned without ALLOW_RESOLUTION_DOWNGRADE"
        );
        assert!(
            !deleted.lock().unwrap().contains(&"t2160".to_string()),
            "the over-ceiling torrent must not be deleted from the provider"
        );
    }

    #[tokio::test]
    async fn movie_upgrade_does_not_downgrade_a_served_over_ceiling_copy() {
        // Same mixed group as above, but the SERVED slot points at the over-ceiling 2160p copy
        // ("h2160") — the realistic state for an engine copy acquired under a previously-higher
        // ceiling. Adopting the within-ceiling REMUX would repoint the served slot from 2160p down
        // to 1080p, an un-opted corrective downgrade. With ALLOW_RESOLUTION_DOWNGRADE off the swap
        // must be skipped entirely (no new copy adopted, both copies kept, selection unchanged).
        let store = mem_store();
        for (hash, tier, res, score) in [
            ("hold", 3_000i64, 1080u16, 10i64),
            ("h2160", 8_000, 2160, 1_008_000),
        ] {
            store
                .put_owned(
                    hash.into(),
                    OwnedRecord {
                        request: movie_req(),
                        provenance: Provenance::watchlist("a"),
                        added_at: 1,
                        status: OwnedStatus::Verified,
                        provides: vec![],
                        quality: Some(QualitySummary {
                            cached: true,
                            source_tier: tier,
                            resolution: res,
                            score,
                            unverified: false,
                        }),
                    },
                )
                .await
                .unwrap();
        }
        // The served slot is the OVER-CEILING copy this time.
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "h2160".into(),
                    file_path: "big.mkv".into(),
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
                    id: "t2160".into(),
                    hash: "h2160".into(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
            ],
            add_magnet: Some(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            }),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        assert!(!app.config.allow_resolution_downgrade);

        let rec = store.get_owned("hold".into()).await.unwrap();
        let r = try_upgrade_movie(
            &app,
            27205,
            &["h2160".to_string(), "hold".to_string()],
            &rec,
            Duration::from_secs(0),
        )
        .await;
        assert!(
            matches!(r, Err(UpgradeSkip::NoChange(_))),
            "the swap is skipped (served copy is over-ceiling and retained), got {r:?}"
        );
        assert!(
            store.get_owned("hnew".into()).await.is_none(),
            "no within-ceiling copy is adopted"
        );
        assert!(store.get_owned("hold".into()).await.is_some());
        assert!(store.get_owned("h2160".into()).await.is_some());
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "h2160",
            "the served selection stays on the over-ceiling copy"
        );
        assert!(deleted.lock().unwrap().is_empty());
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
                unverified: false,
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
                        unverified: false,
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
            // The first get_torrents is the pre-add presence check (`add_staging_candidate`); the
            // second is the post-stage prune listing → fail it there.
            fail_get_torrents_after: Some(1),
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
                        unverified: false,
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
            Ok(vec![Torrent {
                id: "told".into(),
                hash: "hold".into(),
                status: "downloaded".into(),
                ..Default::default()
            }])
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
                        unverified: false,
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
        let ra = Arc::new(crate::read_activity::ReadActivity::without_startup_grace());
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
        let prober = passing_prober();
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
                        unverified: false,
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
            listing: vec![Torrent {
                id: "told".into(),
                hash: "hold".into(),
                status: "downloaded".into(),
                ..Default::default()
            }],
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
                        unverified: false,
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

    /// A prober whose tracks can't be read (`Unsupported` → `VerifyResult::Accept`): the release's
    /// languages are unchecked.
    fn unverifiable_prober() -> Arc<dyn crate::acquire::Prober> {
        Arc::new(CannedProber(Err(ProbeError::Unsupported)))
    }

    /// Movie swap fixture: owned `hold` (Verified, served, quality `owned_q`, torrent `told`) and a
    /// scraped `cand` that stages cached as torrent `tnew` with the single video `staged_file`.
    /// Returns the app, its store and the provider's deleted-torrent log.
    async fn movie_swap_fixture(
        owned_q: QualitySummary,
        cand: RawCandidate,
        staged_file: &str,
        prober: Arc<dyn crate::acquire::Prober>,
    ) -> (AppState, Store, Arc<std::sync::Mutex<Vec<String>>>) {
        let (app, store, deleted, _) =
            movie_swap_fixture_with(owned_q, cand, staged_file, prober, false).await;
        (app, store, deleted)
    }

    /// [`movie_swap_fixture`], also returning the provider's add log (infohashes). With
    /// `cand_in_account` the candidate's torrent is ALREADY in the provider account (listed as
    /// `tnew`, outside this title's group) and the add returns that EXISTING id, as TorBox's
    /// `createtorrent` does for a present infohash.
    async fn movie_swap_fixture_with(
        owned_q: QualitySummary,
        cand: RawCandidate,
        staged_file: &str,
        prober: Arc<dyn crate::acquire::Prober>,
        cand_in_account: bool,
    ) -> (
        AppState,
        Store,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
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
                    quality: Some(owned_q),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: "hold".into(),
                    file_path: "old.avi".into(),
                },
            )
            .await
            .unwrap();
        let cand_hash = cand.info_hash.clone();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut torrents = vec![Torrent {
            id: "told".into(),
            hash: "hold".into(),
            status: "downloaded".into(),
            ..Default::default()
        }];
        if cand_in_account {
            torrents.push(Torrent {
                id: "tnew".into(),
                hash: cand_hash.clone(),
                status: "downloaded".into(),
                ..Default::default()
            });
        }
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents,
            add_magnet: Some(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(),
                hash: cand_hash,
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: staged_file.into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            added: added.clone(),
            ..Default::default()
        });
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand],
        });
        let app = app_with_prober(scraper, provider, store.clone(), prober);
        (app, store, deleted, added)
    }

    #[tokio::test]
    async fn movie_upgrade_rejects_unverifiable_staged_candidate_without_blacklist() {
        // A strictly better cached REMUX whose tracks can't be checked (probe `Accept`) must NOT
        // replace the owned copy: an upgrade never swaps a checked copy for an unchecked one. The
        // staged torrent is dropped, but the hash is NOT blacklisted (nothing is known to be wrong
        // with it — a later probe improvement could still adopt it).
        let owned = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 10,
            unverified: false,
        };
        let (app, store, deleted) = movie_swap_fixture(
            owned,
            remux_candidate(),
            "M.2020.1080p.REMUX.mkv",
            unverifiable_prober(),
        )
        .await;
        run_upgrade_once(&app).await;

        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold",
            "an unverifiable candidate must not be swapped in"
        );
        assert!(
            store.get_owned("hnew".into()).await.is_none(),
            "an unverifiable candidate must not be recorded owned"
        );
        assert!(
            store.get_owned("hold".into()).await.is_some(),
            "the current copy is kept"
        );
        let d = deleted.lock().unwrap().clone();
        assert!(
            d.contains(&"tnew".to_string()),
            "the staged torrent must be deleted"
        );
        assert!(
            !d.contains(&"told".to_string()),
            "the current torrent must not be pruned"
        );
        assert!(
            !store
                .is_blacklisted(MediaKind::Movie, 27205, "hnew".into())
                .await,
            "an unverifiable (not wrong) release must not be blacklisted"
        );
        assert_ne!(
            store.get_upgrade_checked(&MediaType::Movie, 27205).await,
            0,
            "evaluated to completion (NoChange) → the cursor advances"
        );
    }

    #[tokio::test]
    async fn movie_upgrade_replaces_unverified_copy_with_equal_score_verified() {
        // The owned copy's tracks were never checked (flagged `unverified`); a cached release of the
        // SAME quality (same tier/resolution/score — no category jump) whose tracks PASS the probe
        // replaces it, and the unchecked copy is pruned.
        let prefs = AcquisitionConfig::default().prefs;
        let mut owned = QualitySummary::of(&release::parse(&web_1080_candidate()), &prefs);
        owned.unverified = true;
        let (app, store, deleted) = movie_swap_fixture(
            owned,
            web_1080_candidate(),
            "M.2020.1080p.WEB-DL.mkv",
            Arc::new(CannedProber(Ok(vec![Track {
                kind: TrackKind::Audio,
                language: Some("eng".into()),
            }]))),
        )
        .await;
        run_upgrade_once(&app).await;

        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hweb",
            "the unverified copy is replaced by the checked equal-quality release"
        );
        let new_rec = store
            .get_owned("hweb".into())
            .await
            .expect("replacement recorded owned");
        assert!(
            !new_rec.quality.expect("quality recorded").unverified,
            "the adopted replacement passed the probe → not flagged"
        );
        assert!(
            store.get_owned("hold".into()).await.is_none(),
            "the unchecked copy is pruned"
        );
        assert!(deleted.lock().unwrap().contains(&"told".to_string()));
    }

    #[tokio::test]
    async fn movie_upgrade_leaves_verified_copy_alone_for_equal_score_release() {
        // Control for the test above: the same equal-quality candidate is NOT an upgrade over a
        // CHECKED copy (the no-churn guard), so nothing is staged.
        let prefs = AcquisitionConfig::default().prefs;
        let owned = QualitySummary::of(&release::parse(&web_1080_candidate()), &prefs);
        assert!(!owned.unverified);
        let (app, store, deleted) = movie_swap_fixture(
            owned,
            web_1080_candidate(),
            "M.2020.1080p.WEB-DL.mkv",
            Arc::new(CannedProber(Ok(vec![]))),
        )
        .await;
        run_upgrade_once(&app).await;
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold"
        );
        assert!(
            deleted.lock().unwrap().is_empty(),
            "nothing staged or pruned"
        );
    }

    #[tokio::test]
    async fn movie_upgrade_accepts_spoken_language_audio_under_original() {
        // Nightborn's 6 Nov upgrade pass: TMDB original_language `fi`, but English is a listed spoken
        // language. Under the default AUDIO_LANGUAGE=original an `eng`-audio upgrade candidate must
        // PASS the staging probe (and be adopted), not be blacklisted BadAudio.
        let store = mem_store();
        let nightborn = AcquireRequest {
            original_language: Some("fi".into()),
            spoken_languages: Some(vec!["en".into(), "fi".into(), "es".into()]),
            ..movie_req()
        };
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: nightborn,
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                        unverified: false,
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
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            ..Default::default()
        });
        let prober: Arc<dyn crate::acquire::Prober> = Arc::new(CannedProber(Ok(vec![Track {
            kind: TrackKind::Audio,
            language: Some("eng".into()),
        }])));
        let app = app_with_prober(scraper, provider, store.clone(), prober);

        run_upgrade_once(&app).await;

        assert!(
            !store
                .is_blacklisted(MediaKind::Movie, 27205, "hnew".into())
                .await,
            "an English (spoken-language) release must not be blacklisted BadAudio"
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hnew",
            "the spoken-language upgrade must be adopted"
        );
        // The staged record inherits the title's spoken languages.
        assert_eq!(
            store
                .get_owned("hnew".into())
                .await
                .unwrap()
                .request
                .spoken_languages,
            Some(vec!["en".into(), "fi".into(), "es".into()])
        );
    }

    /// A `reqwest::Error` carrying HTTP `status`, as TMDB's `fetch_with_retry` surfaces one.
    fn http_status_error(status: u16) -> reqwest::Error {
        reqwest::Response::from(
            hyper::Response::builder()
                .status(status)
                .body(hyper::body::Bytes::new())
                .unwrap(),
        )
        .error_for_status()
        .unwrap_err()
    }

    /// A canned TMDB `details` fetcher for the language backfill that counts its calls (so a test
    /// can pin "no fetch" / "one fetch per group" without ever reaching the network).
    fn canned_details<R>(
        calls: Arc<AtomicUsize>,
        result: R,
    ) -> impl Fn(u64, MediaType) -> std::future::Ready<Result<TitleDetails, reqwest::Error>>
    where
        R: Fn() -> Result<TitleDetails, reqwest::Error>,
    {
        move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(result())
        }
    }

    /// TMDB's view of Nightborn: `fi` original, mostly English.
    fn nightborn_details() -> TitleDetails {
        TitleDetails {
            title: "Nightborn".into(),
            year: Some("2026".into()),
            original_language: Some("fi".into()),
            spoken_languages: vec!["en".into(), "fi".into(), "es".into()],
        }
    }

    /// A show episode record whose TMDB languages were never fetched (`spoken_languages: None`).
    fn legacy_episode(season: u32, episode: u32) -> OwnedRecord {
        OwnedRecord {
            request: AcquireRequest {
                kind: MediaKind::Series,
                season: Some(season),
                episode: Some(episode),
                spoken_languages: None,
                ..movie_req()
            },
            provenance: Provenance::watchlist("a"),
            added_at: 7,
            status: OwnedStatus::Verified,
            provides: vec![(season, episode)],
            quality: None,
        }
    }

    fn empty_app(store: Store) -> AppState {
        app_with(
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(MockProvider::default()),
            store,
        )
    }

    #[tokio::test]
    async fn language_backfill_covers_the_whole_group_even_when_the_representative_is_fetched() {
        // The decision is group-wide, not representative-only: here the representative (`ha`, the
        // lexicographically-first hash) was acquired after the field existed (`Some`), but the
        // group's legacy episode `hb` and account-mirror record `hc` are `None`. The show paths probe
        // with each record's OWN request, so all of them must be backfilled — with ONE fetch. A mirror
        // record (no `original_language` either) also gains TMDB's original language, so its staging
        // probe checks `original ∪ spoken`, not the spoken list alone.
        let store = mem_store();
        let mut fresh = legacy_episode(1, 1);
        fresh.request.spoken_languages = Some(vec!["en".into()]);
        store.put_owned("ha".into(), fresh).await.unwrap();
        store
            .put_owned("hb".into(), legacy_episode(1, 2))
            .await
            .unwrap();
        let mut mirror = legacy_episode(1, 3);
        mirror.request.original_language = None;
        mirror.provenance = Provenance { entries: vec![] };
        store.put_owned("hc".into(), mirror).await.unwrap();
        let app = empty_app(store.clone());
        let mut rec = store.get_owned("ha".into()).await.unwrap();
        // A hash pruned concurrently (no record) is skipped, never resurrected.
        let hashes: Vec<String> = ["ha", "hb", "hc", "hgone"].map(String::from).to_vec();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut backfill =
            LanguageBackfill::new(canned_details(calls.clone(), || Ok(nightborn_details())));

        let out = backfill
            .run(&app, 314360, &MediaType::Show, &hashes, &mut rec)
            .await;

        assert!(out.is_ok(), "{out:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one TMDB call per group");
        let want = Some(vec!["en".to_string(), "fi".to_string(), "es".to_string()]);
        let hb = store.get_owned("hb".into()).await.unwrap();
        assert_eq!(
            hb.request.spoken_languages, want,
            "legacy episode backfilled"
        );
        assert_eq!(
            hb.request.original_language.as_deref(),
            Some("eng"),
            "a recorded original language is kept"
        );
        assert_eq!(hb.request.episode, Some(2), "other fields intact");
        assert_eq!(hb.provides, vec![(1, 2)]);
        assert_eq!(hb.added_at, 7);
        let hc = store.get_owned("hc".into()).await.unwrap();
        assert_eq!(
            hc.request.spoken_languages, want,
            "mirror record backfilled"
        );
        assert_eq!(hc.request.original_language.as_deref(), Some("fi"));
        // The already-fetched representative keeps its own list, in hand and in the store.
        assert_eq!(rec.request.spoken_languages, Some(vec!["en".to_string()]));
        assert_eq!(
            store
                .get_owned("ha".into())
                .await
                .unwrap()
                .request
                .spoken_languages,
            Some(vec!["en".to_string()])
        );
        assert!(store.get_owned("hgone".into()).await.is_none());

        // Converged: every record is now `Some`, so the next visit fetches nothing.
        let out = backfill
            .run(&app, 314360, &MediaType::Show, &hashes, &mut rec)
            .await;
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "never re-fetched");
    }

    #[tokio::test]
    async fn language_backfill_fills_the_in_hand_representative() {
        // The movie path stages with the in-hand representative `rec`, so a fetch must update it too
        // (spoken list, and the original language when it was unknown).
        let store = mem_store();
        let mut legacy = legacy_episode(1, 1);
        legacy.request = AcquireRequest {
            original_language: None,
            spoken_languages: None,
            ..movie_req()
        };
        legacy.provides = vec![];
        store.put_owned("hold".into(), legacy).await.unwrap();
        let app = empty_app(store.clone());
        let mut rec = store.get_owned("hold".into()).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut backfill =
            LanguageBackfill::new(canned_details(calls.clone(), || Ok(nightborn_details())));
        let out = backfill
            .run(&app, 27205, &MediaType::Movie, &["hold".into()], &mut rec)
            .await;
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(
            rec.request.spoken_languages,
            Some(vec!["en".to_string(), "fi".to_string(), "es".to_string()])
        );
        assert_eq!(rec.request.original_language.as_deref(), Some("fi"));
        let stored = store.get_owned("hold".into()).await.unwrap().request;
        assert_eq!(stored.spoken_languages, rec.request.spoken_languages);
        assert_eq!(stored.original_language, rec.request.original_language);
    }

    #[tokio::test]
    async fn language_backfill_skips_the_fetch_when_every_record_is_fetched() {
        // `Some(..)` (even empty) on every record means TMDB was already consulted: no fetch at all.
        let store = mem_store();
        let mut a = legacy_episode(1, 1);
        a.request.spoken_languages = Some(vec![]);
        let mut b = legacy_episode(1, 2);
        b.request.spoken_languages = Some(vec!["en".into()]);
        store.put_owned("ha".into(), a).await.unwrap();
        store.put_owned("hb".into(), b).await.unwrap();
        let app = empty_app(store.clone());
        let mut rec = store.get_owned("ha".into()).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut backfill =
            LanguageBackfill::new(canned_details(calls.clone(), || Ok(nightborn_details())));
        let out = backfill
            .run(
                &app,
                314360,
                &MediaType::Show,
                &["ha".into(), "hb".into()],
                &mut rec,
            )
            .await;
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no TMDB fetch");
        assert_eq!(rec.request.spoken_languages, Some(vec![]));
    }

    #[tokio::test]
    async fn language_backfill_transient_failure_defers_without_writing() {
        // A transient TMDB failure must NOT let this visit probe with the original-only rule (that
        // would blacklist correct spoken-language candidates for the blacklist TTL): the title is
        // Deferred (cursor unchanged → retried next tick), nothing is written, and later titles
        // that also need a backfill are deferred this tick WITHOUT another (slow, retrying) fetch.
        // A title that needs no backfill is unaffected.
        let store = mem_store();
        store
            .put_owned("hb".into(), legacy_episode(1, 2))
            .await
            .unwrap();
        let mut done = legacy_episode(1, 1);
        done.request.spoken_languages = Some(vec![]);
        store.put_owned("hdone".into(), done).await.unwrap();
        let app = empty_app(store.clone());
        let mut rec = store.get_owned("hb".into()).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut backfill = LanguageBackfill::new(canned_details(calls.clone(), || {
            Err(http_status_error(502))
        }));

        let out = backfill
            .run(&app, 314360, &MediaType::Show, &["hb".into()], &mut rec)
            .await;
        assert!(matches!(out, Err(UpgradeSkip::Deferred(_))), "{out:?}");
        assert_eq!(rec.request.spoken_languages, None);
        assert_eq!(
            store
                .get_owned("hb".into())
                .await
                .unwrap()
                .request
                .spoken_languages,
            None
        );
        // A second title needing a backfill this tick: deferred without fetching again.
        let out = backfill
            .run(&app, 999, &MediaType::Show, &["hb".into()], &mut rec)
            .await;
        assert!(matches!(out, Err(UpgradeSkip::Deferred(_))), "{out:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "TMDB outage: one fetch per tick"
        );
        // A fully-fetched title still proceeds.
        let mut done = store.get_owned("hdone".into()).await.unwrap();
        let out = backfill
            .run(&app, 1000, &MediaType::Show, &["hdone".into()], &mut done)
            .await;
        assert!(out.is_ok(), "{out:?}");
    }

    #[tokio::test]
    async fn language_backfill_permanent_failure_proceeds_without_writing() {
        // A non-429 4xx (bad key, unknown id) won't fix itself by waiting: deferring would park the
        // title at the head of the round-robin forever, blocking all its upgrades. Proceed with the
        // recorded languages (the pre-backfill rule), write nothing, and keep TMDB usable for the
        // rest of the tick.
        let store = mem_store();
        store
            .put_owned("hb".into(), legacy_episode(1, 2))
            .await
            .unwrap();
        let app = empty_app(store.clone());
        let mut rec = store.get_owned("hb".into()).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut backfill = LanguageBackfill::new(canned_details(calls.clone(), || {
            Err(http_status_error(404))
        }));
        for id in [314360, 999] {
            let out = backfill
                .run(&app, id, &MediaType::Show, &["hb".into()], &mut rec)
                .await;
            assert!(out.is_ok(), "{out:?}");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a 4xx doesn't mark TMDB down"
        );
        assert_eq!(rec.request.spoken_languages, None);
        assert_eq!(
            store
                .get_owned("hb".into())
                .await
                .unwrap()
                .request
                .spoken_languages,
            None
        );
    }

    #[tokio::test]
    async fn tmdb_error_permanence_classification() {
        assert!(tmdb_error_is_permanent(&http_status_error(404)));
        assert!(tmdb_error_is_permanent(&http_status_error(401)));
        assert!(!tmdb_error_is_permanent(&http_status_error(429)));
        assert!(!tmdb_error_is_permanent(&http_status_error(502)));
        assert!(!tmdb_error_is_permanent(&http_status_error(500)));
        // No HTTP status (transport / decode failure) → transient.
        let decode = reqwest::Response::from(
            hyper::Response::builder()
                .status(200)
                .body(hyper::body::Bytes::from_static(b"not json"))
                .unwrap(),
        )
        .json::<serde_json::Value>()
        .await
        .unwrap_err();
        assert!(decode.status().is_none());
        assert!(!tmdb_error_is_permanent(&decode));
    }

    /// A Nightborn-shaped legacy movie (`fi` original, spoken languages never fetched) owned as a
    /// cached 1080p WEB copy, plus a provider/scraper offering a cached `eng`-audio REMUX upgrade.
    async fn legacy_nightborn_app(store: &Store) -> AppState {
        store
            .put_owned(
                "hold".into(),
                OwnedRecord {
                    request: AcquireRequest {
                        original_language: Some("fi".into()),
                        spoken_languages: None,
                        ..movie_req()
                    },
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 10,
                        unverified: false,
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
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            ..Default::default()
        });
        let prober: Arc<dyn crate::acquire::Prober> = Arc::new(CannedProber(Ok(vec![Track {
            kind: TrackKind::Audio,
            language: Some("eng".into()),
        }])));
        app_with_prober(scraper, provider, store.clone(), prober)
    }

    #[tokio::test]
    async fn upgrade_tick_backfills_a_legacy_title_before_its_staging_probe() {
        // Wiring: `run_upgrade_once` must backfill a legacy title BEFORE dispatch. Without it, the
        // staging probe checks `fi` alone and blacklists the English upgrade as BadAudio.
        let store = mem_store();
        let calls = Arc::new(AtomicUsize::new(0));
        let app = legacy_nightborn_app(&store).await;

        run_upgrade_once_with(
            &app,
            canned_details(calls.clone(), || Ok(nightborn_details())),
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            !store
                .is_blacklisted(MediaKind::Movie, 27205, "hnew".into())
                .await,
            "the English upgrade must not be blacklisted BadAudio"
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hnew",
            "the spoken-language upgrade must be adopted"
        );
        assert_eq!(
            store
                .get_owned("hnew".into())
                .await
                .unwrap()
                .request
                .spoken_languages,
            Some(vec!["en".into(), "fi".into(), "es".into()]),
            "the staged record inherits the backfilled languages"
        );
    }

    #[tokio::test]
    async fn upgrade_tick_defers_a_legacy_title_when_tmdb_is_unavailable() {
        // A transient TMDB failure during the backfill defers the title: nothing is staged or probed
        // (so nothing is blacklisted under the original-only rule) and its cursor stays unstamped.
        let store = mem_store();
        let calls = Arc::new(AtomicUsize::new(0));
        let app = legacy_nightborn_app(&store).await;

        run_upgrade_once_with(
            &app,
            canned_details(calls.clone(), || Err(http_status_error(503))),
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            !store
                .is_blacklisted(MediaKind::Movie, 27205, "hnew".into())
                .await
        );
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold",
            "nothing staged or swapped"
        );
        assert!(store.get_owned("hnew".into()).await.is_none());
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 27205).await,
            0,
            "deferred: cursor unchanged"
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
                        unverified: false,
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
                        unverified: false,
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
                        unverified: false,
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
                            unverified: false,
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

    /// A known resolution stays a floor in the baseline too: owning a 1080p copy with an untagged
    /// source and a 720p WEB copy, the baseline is the 1080p one, so a 720p candidate — which
    /// would "improve" the 720p WEB copy — is never adopted and the swap never prunes the 1080p
    /// copy (only the gated corrective downgrade ever trades resolution down).
    #[tokio::test]
    async fn best_owned_quality_keeps_an_unknown_copys_resolution_as_a_floor() {
        let store = mem_store();
        let rec = |resolution: u16, tier: i64, score: i64| OwnedRecord {
            request: movie_req(),
            provenance: Provenance::watchlist("a"),
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![],
            quality: Some(QualitySummary {
                cached: true,
                source_tier: tier,
                resolution,
                score,
                unverified: false,
            }),
        };
        store
            .put_owned("a720web".into(), rec(720, 3_000, 1_000_000))
            .await
            .unwrap();
        store
            .put_owned("b1080".into(), rec(1080, 0, 1))
            .await
            .unwrap();
        let app = app_with(
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(MockProvider::default()),
            store.clone(),
        );
        let best = best_owned_quality(&app, &["a720web".into(), "b1080".into()])
            .await
            .expect("a baseline exists");
        assert_eq!((best.resolution, best.source_tier), (1080, 0));
        let prefs = &app.config.acquisition.prefs;
        let cand = |resolution: u16, tier: i64| QualitySummary {
            cached: true,
            source_tier: tier,
            resolution,
            score: 2_000_000,
            unverified: false,
        };
        assert_eq!(
            release::is_target_improvement(&best, &cand(720, 6_000), prefs),
            release::Improvement::None,
            "a 720p BluRay must not prune the 1080p copy"
        );
        assert_eq!(
            release::is_target_improvement(&best, &cand(1080, 3_000), prefs),
            release::Improvement::Upgrade,
            "a known 1080p release improves on both copies"
        );
        // Among unknown copies too: a higher-scoring copy with an untagged resolution doesn't make
        // the 1080p one prunable by a 720p release.
        store
            .put_owned("cremux".into(), rec(0, 8_000, 9_000_000))
            .await
            .unwrap();
        let best = best_owned_quality(&app, &["b1080".into(), "cremux".into()])
            .await
            .expect("a baseline exists");
        assert_eq!(best.resolution, 1080);
    }

    #[tokio::test]
    async fn best_owned_quality_picks_best_copy_known_before_unknown() {
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
                unverified: false,
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

        // An unknown-quality copy (no recorded quality) is the LOWEST quality: it never becomes the
        // baseline while a known copy exists…
        let mut unknown = rec(3_000, 1_003_000);
        unknown.quality = None;
        store
            .put_owned("bunknown".into(), unknown.clone())
            .await
            .unwrap();
        let mut partial = rec(0, 9_999_999); // source untagged → unknown, even with a higher score
        partial.quality.as_mut().unwrap().source_tier = 0;
        store.put_owned("cpartial".into(), partial).await.unwrap();
        let best = best_owned_quality(
            &app,
            &[
                "aweb".into(),
                "bunknown".into(),
                "cpartial".into(),
                "zremux".into(),
            ],
        )
        .await
        .expect("a baseline exists");
        assert_eq!(
            best.source_tier, 8_000,
            "known before unknown: REMUX is the baseline"
        );
        // …and a title owned ONLY as unknown copies baselines as unknown, so any known cached
        // within-ceiling candidate is an improvement.
        let best = best_owned_quality(&app, &["bunknown".into()])
            .await
            .expect("unknown baseline");
        assert!(release::is_unknown_quality(&best, 1080));
        assert_eq!(
            release::is_target_improvement(
                &best,
                &QualitySummary {
                    cached: true,
                    source_tier: 3_000,
                    resolution: 720,
                    score: 1,
                    unverified: false,
                },
                &app.config.acquisition.prefs
            ),
            release::Improvement::Upgrade
        );
        // No owned record at all → no baseline.
        assert!(best_owned_quality(&app, &["gone".into()]).await.is_none());
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
                        unverified: false,
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
                        unverified: false,
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

    // ── corrective-downgrade tests (Task 4) ───────────────────────────────────

    /// Harness for the movie corrective-downgrade tests: a single owned copy ABOVE the P1080
    /// ceiling (2160p REMUX — as if acquired before the ceiling was lowered, or mirrored from a
    /// pre-existing library file), with the movie selection pointing at it, plus a cached
    /// within-ceiling 1080p REMUX candidate available from the scraper/provider.
    struct DowngradeHarness {
        app: AppState,
        tmdb: u64,
        owned_hashes: Vec<String>,
        rec: OwnedRecord,
        owned_2160_hash: String,
        cand_1080_hash: String,
    }

    impl DowngradeHarness {
        /// Flip `allow_resolution_downgrade` on the harness's `Config` (rebuilds the `Arc` since
        /// `Config` doesn't expose interior mutability).
        fn set_allow_downgrade(&mut self, v: bool) {
            let mut cfg = (*self.app.config).clone();
            cfg.allow_resolution_downgrade = v;
            self.app.config = Arc::new(cfg);
        }
    }

    async fn harness_with_owned_movie_2160_and_1080_candidate() -> DowngradeHarness {
        let store = mem_store();
        let owned_2160_hash = "h2160".to_string();
        store
            .put_owned(
                owned_2160_hash.clone(),
                OwnedRecord {
                    request: movie_req(),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(QualitySummary {
                        cached: true,
                        source_tier: 8_000,
                        resolution: 2160,
                        score: 1_008_000,
                        unverified: false,
                    }),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(27205),
                SelectionEntry {
                    hash: owned_2160_hash.clone(),
                    file_path: "old2160.mkv".into(),
                },
            )
            .await
            .unwrap();

        // A cached, within-ceiling 1080p REMUX candidate — `remux_candidate()` is already used
        // (and proven) elsewhere as a meaningful cached upgrade; reused here as the corrective
        // within-ceiling replacement.
        let cand = remux_candidate();
        let cand_1080_hash = cand.info_hash.clone();
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "told".into(),
                hash: owned_2160_hash.clone(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            add_magnet: Some(AddMagnetResponse {
                id: "tnew".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(),
                hash: cand_1080_hash.clone(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 20_000_000_000,
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
        let rec = store.get_owned(owned_2160_hash.clone()).await.unwrap();

        DowngradeHarness {
            app,
            tmdb: 27205,
            owned_hashes: vec![owned_2160_hash.clone()],
            rec,
            owned_2160_hash,
            cand_1080_hash,
        }
    }

    #[tokio::test]
    async fn movie_over_ceiling_downgrade_is_dry_run_by_default() {
        // Own a cached 2160p movie; ceiling = 1080; a cached 1080p candidate is scrape-available.
        // Flag OFF (default): no swap, no prune — the 2160p copy stays owned and its selection
        // unchanged.
        let h = harness_with_owned_movie_2160_and_1080_candidate().await;
        assert!(!h.app.config.allow_resolution_downgrade);
        let r = try_upgrade_movie(
            &h.app,
            h.tmdb,
            &h.owned_hashes,
            &h.rec,
            Duration::from_secs(0),
        )
        .await;
        assert!(matches!(r, Ok(()) | Err(UpgradeSkip::NoChange(_))));
        assert!(
            h.app
                .store
                .get_owned(h.owned_2160_hash.clone())
                .await
                .is_some(),
            "dry-run must not prune the over-ceiling copy"
        );
        assert_eq!(
            h.app
                .store
                .get_selection(movie_slot(h.tmdb))
                .await
                .unwrap()
                .hash,
            h.owned_2160_hash
        );
    }

    #[tokio::test]
    async fn movie_over_ceiling_downgrade_swaps_when_flag_set() {
        let mut h = harness_with_owned_movie_2160_and_1080_candidate().await;
        h.set_allow_downgrade(true);
        let r = try_upgrade_movie(
            &h.app,
            h.tmdb,
            &h.owned_hashes,
            &h.rec,
            Duration::from_secs(0),
        )
        .await;
        assert!(r.is_ok());
        assert!(
            h.app
                .store
                .get_owned(h.owned_2160_hash.clone())
                .await
                .is_none(),
            "flag on: the over-ceiling copy is pruned"
        );
        assert_eq!(
            h.app
                .store
                .get_selection(movie_slot(h.tmdb))
                .await
                .unwrap()
                .hash,
            h.cand_1080_hash
        );
    }

    // ── in-tick candidate fall-through tests (movie path) ─────────────────────

    #[tokio::test]
    async fn movie_upgrade_falls_through_pack_to_next_candidate() {
        // Own a 720p movie. Scrape returns two IN-CEILING improvements: the top-scored one (1080p
        // REMUX) resolves to a MULTI-FEATURE pack (rejected + blacklisted); the next (1080p WEB) is a
        // clean single movie. The engine must fall through and adopt the WEB release THIS tick.
        const TMDB: u64 = 550;
        let store = mem_store();
        let owned_hash = "own720".to_string();
        store
            .put_owned(
                owned_hash.clone(),
                OwnedRecord {
                    request: movie_req_for(TMDB, "tt0137523"),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(q720_web()),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(TMDB),
                SelectionEntry {
                    hash: owned_hash.clone(),
                    file_path: "old.720p.mkv".into(),
                },
            )
            .await
            .unwrap();

        // Two cached in-ceiling improvements over 720p; REMUX outscores WEB so it is tried first.
        let pack = RawCandidate {
            name: "Fight.Club.1999.1080p.BluRay.REMUX".into(),
            description: "REMUX\nRD+".into(),
            info_hash: "hpack".into(),
            file_idx: None,
            file_name: None,
        };
        let good = RawCandidate {
            name: "Fight.Club.1999.1080p.WEB".into(),
            description: "WEB\nRD+".into(),
            info_hash: "hgood".into(),
            file_idx: None,
            file_name: None,
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![pack.clone(), good.clone()],
        });

        let mut by_id = std::collections::HashMap::new();
        // "hpack" → two feature-sized videos ⇒ count_feature_videos > 1 ⇒ MoviePack reject.
        by_id.insert(
            "hpack".to_string(),
            TorrentInfo {
                id: "hpack".into(),
                hash: "hpack".into(),
                status: "downloaded".into(),
                files: vec![
                    TorrentFile {
                        id: 0,
                        path: "Fight.Club.1080p.mkv".into(),
                        bytes: 8_000_000_000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 1,
                        path: "Other.Movie.1080p.mkv".into(),
                        bytes: 8_000_000_000,
                        selected: 1,
                    },
                ],
                links: vec!["https://cdn/p0".into(), "https://cdn/p1".into()],
                ..Default::default()
            },
        );
        // "hgood" → single feature video ⇒ adopted.
        by_id.insert(
            "hgood".to_string(),
            TorrentInfo {
                id: "hgood".into(),
                hash: "hgood".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Fight.Club.1999.1080p.WEB.mkv".into(),
                    bytes: 6_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/g0".into()],
                ..Default::default()
            },
        );

        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "own720".into(),
                hash: owned_hash.clone(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            torrent_info_by_id: by_id,
            resolved_url: Some("https://cdn/g0".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        // app_with wires a passing prober (`VerifyResult::Pass`), so staging the good candidate
        // verifies.

        let owned = vec![owned_hash.clone()];
        let rec = app.store.get_owned(owned_hash.clone()).await.unwrap();
        let r = try_upgrade_movie(&app, TMDB, &owned, &rec, Duration::from_secs(0)).await;
        assert!(
            r.is_ok(),
            "fall-through should adopt the good candidate: {r:?}"
        );
        assert_eq!(
            app.store
                .get_selection(movie_slot(TMDB))
                .await
                .unwrap()
                .hash,
            "hgood",
            "selection repointed to the second-best (usable) candidate"
        );
        assert!(
            app.store
                .is_blacklisted(MediaKind::Movie, TMDB, "hpack".into())
                .await,
            "the multi-feature pack was blacklisted"
        );
        assert!(
            app.store.get_owned("own720".into()).await.is_none(),
            "old 720p copy pruned"
        );
    }

    /// Owns a 720p movie and wires a two-candidate scrape (top-scored `hpack` = 1080p REMUX, second
    /// `hgood` = 1080p WEB) with per-candidate `torrent_info_by_id`. Callers pass the resolved files
    /// for each and whether the top's info errors (transient).
    async fn movie_two_candidate_app(
        top_files: Vec<TorrentFile>,
        second_files: Vec<TorrentFile>,
        fail_top_info: bool,
    ) -> (AppState, String) {
        const TMDB: u64 = 550;
        let store = mem_store();
        let owned_hash = "own720".to_string();
        store
            .put_owned(
                owned_hash.clone(),
                OwnedRecord {
                    request: movie_req_for(TMDB, "tt0137523"),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(q720_web()),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(TMDB),
                SelectionEntry {
                    hash: owned_hash.clone(),
                    file_path: "old.720p.mkv".into(),
                },
            )
            .await
            .unwrap();

        let pack = RawCandidate {
            name: "Fight.Club.1999.1080p.BluRay.REMUX".into(),
            description: "REMUX\nRD+".into(),
            info_hash: "hpack".into(),
            file_idx: None,
            file_name: None,
        };
        let good = RawCandidate {
            name: "Fight.Club.1999.1080p.WEB".into(),
            description: "WEB\nRD+".into(),
            info_hash: "hgood".into(),
            file_idx: None,
            file_name: None,
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![pack, good],
        });

        let mut by_id = std::collections::HashMap::new();
        by_id.insert(
            "hpack".to_string(),
            TorrentInfo {
                id: "hpack".into(),
                hash: "hpack".into(),
                status: "downloaded".into(),
                files: top_files,
                links: vec!["https://cdn/p".into()],
                ..Default::default()
            },
        );
        by_id.insert(
            "hgood".to_string(),
            TorrentInfo {
                id: "hgood".into(),
                hash: "hgood".into(),
                status: "downloaded".into(),
                files: second_files,
                links: vec!["https://cdn/g".into()],
                ..Default::default()
            },
        );
        let mut fail_info_ids = std::collections::HashSet::new();
        if fail_top_info {
            fail_info_ids.insert("hpack".to_string());
        }

        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "own720".into(),
                hash: owned_hash.clone(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            torrent_info_by_id: by_id,
            fail_info_ids,
            resolved_url: Some("https://cdn/g".into()),
            deleted: Arc::new(std::sync::Mutex::new(Vec::new())),
            ..Default::default()
        });
        (app_with(scraper, provider, store), owned_hash)
    }

    fn feature(name: &str) -> TorrentFile {
        TorrentFile {
            id: 0,
            path: name.into(),
            bytes: 8_000_000_000,
            selected: 1,
        }
    }

    #[tokio::test]
    async fn movie_upgrade_exhausts_bad_candidates_and_keeps_current() {
        // BOTH candidates resolve to multi-feature packs → both rejected+blacklisted, NoChange,
        // nothing swapped.
        let (app, owned) = movie_two_candidate_app(
            vec![feature("A.1080p.mkv"), feature("B.1080p.mkv")],
            vec![feature("C.1080p.mkv"), feature("D.1080p.mkv")],
            false,
        )
        .await;
        let rec = app.store.get_owned(owned.clone()).await.unwrap();
        let r = try_upgrade_movie(
            &app,
            550,
            std::slice::from_ref(&owned),
            &rec,
            Duration::from_secs(0),
        )
        .await;
        assert!(
            matches!(r, Err(UpgradeSkip::NoChange(_))),
            "all bad → NoChange: {r:?}"
        );
        assert_eq!(
            app.store.get_selection(movie_slot(550)).await.unwrap().hash,
            owned,
            "selection unchanged"
        );
        assert!(
            app.store.get_owned(owned.clone()).await.is_some(),
            "old copy kept"
        );
    }

    #[tokio::test]
    async fn movie_upgrade_transient_on_top_candidate_stops_without_trying_rest() {
        // Top candidate's get_torrent_info errors (transient) → stage_and_verify returns Deferred →
        // the loop STOPS (returns Deferred) and never stages the good second candidate.
        let (app, owned) = movie_two_candidate_app(
            vec![feature("Fight.Club.1080p.mkv")], // (unused: hpack info errors before file inspection)
            vec![feature("Fight.Club.1999.1080p.WEB.mkv")],
            true, // hpack info errors
        )
        .await;
        let rec = app.store.get_owned(owned.clone()).await.unwrap();
        let r = try_upgrade_movie(
            &app,
            550,
            std::slice::from_ref(&owned),
            &rec,
            Duration::from_secs(0),
        )
        .await;
        assert!(
            matches!(r, Err(UpgradeSkip::Deferred(_))),
            "transient on top → Deferred: {r:?}"
        );
        assert_eq!(
            app.store.get_selection(movie_slot(550)).await.unwrap().hash,
            owned,
            "nothing adopted — the good candidate was never staged"
        );
    }

    #[tokio::test]
    async fn movie_over_ceiling_downgrade_dry_run_breaks_and_makes_no_change() {
        // Own a 2160p movie; ceiling 1080 (default prefs); ALLOW_RESOLUTION_DOWNGRADE off. Two
        // in-ceiling 1080p candidates → rank_changes rates both as Downgrade; the loop breaks on the
        // first (gated), logging one preview, and makes NO change / stages nothing.
        const TMDB: u64 = 603;
        let store = mem_store();
        let owned = "own2160".to_string();
        store
            .put_owned(
                owned.clone(),
                OwnedRecord {
                    request: movie_req_for(TMDB, "tt0133093"),
                    provenance: Provenance::watchlist("a"),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: Some(q2160_remux()),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                movie_slot(TMDB),
                SelectionEntry {
                    hash: owned.clone(),
                    file_path: "old.2160p.mkv".into(),
                },
            )
            .await
            .unwrap();
        let c1 = RawCandidate {
            name: "The.Matrix.1999.1080p.REMUX".into(),
            description: "REMUX\nRD+".into(),
            info_hash: "hc1".into(),
            file_idx: None,
            file_name: None,
        };
        let c2 = RawCandidate {
            name: "The.Matrix.1999.1080p.WEB".into(),
            description: "WEB\nRD+".into(),
            info_hash: "hc2".into(),
            file_idx: None,
            file_name: None,
        };
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "own2160".into(),
                hash: owned.clone(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            deleted: deleted.clone(),
            ..Default::default() // no torrent_info_by_id → staging never reached
        });
        let app = app_with(
            Arc::new(MockScraper {
                candidates: vec![c1, c2],
            }),
            provider,
            store.clone(),
        );
        assert!(
            !app.config.allow_resolution_downgrade,
            "flag must be off for this test"
        );
        let rec = app.store.get_owned(owned.clone()).await.unwrap();
        let r = try_upgrade_movie(
            &app,
            TMDB,
            std::slice::from_ref(&owned),
            &rec,
            Duration::from_secs(0),
        )
        .await;
        assert!(
            matches!(r, Err(UpgradeSkip::NoChange(_))),
            "gated downgrade → NoChange: {r:?}"
        );
        assert_eq!(
            app.store
                .get_selection(movie_slot(TMDB))
                .await
                .unwrap()
                .hash,
            owned,
            "unchanged"
        );
        assert!(
            deleted.lock().unwrap().is_empty(),
            "nothing staged (broke before staging)"
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
                        unverified: false,
                    },
                ),
                (
                    2,
                    crate::release::QualitySummary {
                        cached: true,
                        source_tier: 3_000,
                        resolution: 1080,
                        score: 1,
                        unverified: false,
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
                unverified: false,
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

    /// A provider call log (`add_magnet` infohashes / `delete_torrent` ids).
    type CallLog = Arc<std::sync::Mutex<Vec<String>>>;

    /// A movie app whose account lists the owned "hold" copy (`told`) and whose staging add resolves
    /// to the cached REMUX candidate (`hnew`/`tnew`). Returns the app plus the add / delete logs.
    fn unknown_movie_app(
        store: &Store,
        candidates: Vec<RawCandidate>,
    ) -> (AppState, CallLog, CallLog) {
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
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
                    path: "M.2020.1080p.REMUX.mkv".into(),
                    bytes: 30_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            added: added.clone(),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(
            Arc::new(MockScraper { candidates }),
            provider,
            store.clone(),
        );
        (app, added, deleted)
    }

    /// An owned movie copy whose quality is UNKNOWN — no recorded quality (a mirror copy whose file
    /// name was never seen), or an untagged axis (even with a higher score than the candidate) — is
    /// the lowest quality: a known cached candidate that passes the probe replaces it, and the next
    /// visit changes nothing (convergence).
    #[tokio::test]
    async fn movie_with_unknown_quality_is_upgraded_and_converges() {
        for current in [
            None,
            Some(QualitySummary {
                cached: true,
                source_tier: 0, // untagged source
                resolution: 1080,
                score: 9_999_999,
                unverified: false,
            }),
        ] {
            let store = mem_store();
            store
                .put_owned(
                    "hold".into(),
                    OwnedRecord {
                        request: movie_req(),
                        provenance: Provenance { entries: vec![] }, // pre-existing mirror copy
                        added_at: 1,
                        status: OwnedStatus::Verified,
                        provides: vec![],
                        quality: current.clone(),
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
            let (app, added, deleted) =
                unknown_movie_app(&store, vec![remux_candidate(), web_1080_candidate()]);

            run_upgrade_once(&app).await;

            assert_eq!(
                store.get_selection(movie_slot(27205)).await.unwrap().hash,
                "hnew",
                "{current:?}: swapped to the known REMUX"
            );
            let adopted = store.get_owned("hnew".into()).await.expect("adopted");
            let q = adopted.quality.expect("a known quality is recorded");
            assert!(!release::is_unknown_quality(&q, 1080));
            assert!(
                store.get_owned("hold".into()).await.is_none(),
                "unknown copy pruned"
            );
            assert!(deleted.lock().unwrap().contains(&"told".to_string()));
            assert_eq!(*added.lock().unwrap(), vec!["hnew".to_string()]);

            // Second visit: the adopted copy is known; the WEB release is no improvement on it and the
            // REMUX is owned → nothing is staged, nothing changes.
            run_upgrade_once(&app).await;
            assert_eq!(
                *added.lock().unwrap(),
                vec!["hnew".to_string()],
                "{current:?}: converged — nothing staged on the second visit"
            );
            assert_eq!(
                store.get_selection(movie_slot(27205)).await.unwrap().hash,
                "hnew"
            );
            assert!(store.get_owned("hnew".into()).await.is_some());
        }
    }

    /// The hard constraint: a copy whose FILE NAME shows a resolution above `MAX_RESOLUTION` is never
    /// treated as unknown-lowest, even when its record carries no quality (account-mirror records
    /// above the ceiling used to be stored `quality: None`). The upgrade tick re-derives the quality
    /// from the persisted identification first, so the copy stays under the corrective-downgrade
    /// rules: kept with `ALLOW_RESOLUTION_DOWNGRADE` off, corrected with it on.
    #[tokio::test]
    async fn over_ceiling_movie_without_quality_stays_under_corrective_downgrade_rules() {
        for allow_downgrade in [false, true] {
            let store = mem_store();
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
                .put_matches(vec![(
                    "told".into(),
                    TorrentInfo {
                        id: "told".into(),
                        hash: "HOLD".into(),
                        status: "downloaded".into(),
                        files: vec![TorrentFile {
                            id: 0,
                            path: "M (2020)/M.2020.2160p.UHD.BluRay.REMUX.mkv".into(),
                            bytes: 60_000_000_000,
                            selected: 1,
                        }],
                        ..Default::default()
                    },
                    movie_meta(),
                )])
                .await
                .unwrap();
            let (mut app, added, deleted) = unknown_movie_app(&store, vec![remux_candidate()]);
            let mut cfg = (*app.config).clone();
            cfg.allow_resolution_downgrade = allow_downgrade;
            app.config = Arc::new(cfg);

            run_upgrade_once(&app).await;

            if allow_downgrade {
                assert!(
                    store.get_owned("hnew".into()).await.is_some(),
                    "flag on: corrected to the within-ceiling REMUX"
                );
                assert!(store.get_owned("hold".into()).await.is_none());
            } else {
                assert!(added.lock().unwrap().is_empty(), "flag off: nothing staged");
                assert!(
                    deleted.lock().unwrap().is_empty(),
                    "flag off: nothing deleted"
                );
                let q = store
                    .get_owned("hold".into())
                    .await
                    .expect("the over-ceiling copy is kept")
                    .quality
                    .expect("its quality was re-derived from the file name");
                assert_eq!(q.resolution, 2160);
                assert!(!release::is_unknown_quality(&q, 1080));
            }
        }
    }

    #[test]
    fn season_owned_quality_includes_packs_and_ranks_unknown_lowest() {
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
        let q = season_owned_quality(&owned, 1);
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

        // A contributor with no recorded quality is UNKNOWN on both axes — the lowest — so it never
        // makes a known pack a regression…
        let mut owned2 = owned;
        owned2.retain(|(h, _)| h != "hp");
        owned2.push(("hx".to_string(), rec(vec![(1, 11)], None)));
        let q2 = season_owned_quality(&owned2, 1);
        assert_eq!(
            q2.len(),
            3,
            "the unknown contributor is represented, not a skip"
        );
        let pack = |q: QualitySummary| ConsolidationInput {
            season: 1,
            aired_episodes: vec![9, 10, 11],
            owned_episode_quality: q2.clone(),
            pack_cached: true,
            pack_episodes: vec![9, 10, 11],
            pack_quality: q,
        };
        assert!(
            consolidation_target(&pack(q1080_web())),
            "a known pack replaces it"
        );
        // …while an untagged pack axis is the lowest too: a pack with unknown resolution is a
        // regression against a known 1080p episode.
        let mut untagged_res = q1080_bluray();
        untagged_res.resolution = 0;
        assert!(!consolidation_target(&pack(untagged_res)));
    }

    /// Phase (a2)'s per-axis filter treats an unknown pack axis as the lowest: ANY known candidate
    /// passes it, while a known pack axis is still compared.
    #[test]
    fn pack_split_filter_accepts_any_known_candidate_over_an_unknown_pack_axis() {
        let cand = |desc: &str| {
            release::parse(&RawCandidate {
                name: "Torrentio".into(),
                description: format!("{desc}\nRD+"),
                info_hash: "h".into(),
                file_idx: Some(0),
                file_name: Some(format!("{desc}.mkv")),
            })
        };
        let hdtv_720 = cand("S.S01E01.720p.HDTV.x264");
        assert!(pack_split_no_regression(
            &hdtv_720,
            &QualitySummary::unknown()
        ));
        let mut untagged_res_bluray = q1080_bluray();
        untagged_res_bluray.resolution = 0;
        assert!(
            pack_split_no_regression(&cand("S.S01E01.720p.BluRay.x264"), &untagged_res_bluray),
            "the unknown resolution axis accepts any known resolution"
        );
        assert!(
            !pack_split_no_regression(&hdtv_720, &untagged_res_bluray),
            "the known tier axis is still compared"
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
    fn rank_cached_pack_candidates_dedups_repeated_info_hash() {
        use crate::config::AcquisitionConfig;
        let prefs = AcquisitionConfig::default().prefs;
        // The same infohash returned twice (Torrentio does this under differing source/title
        // strings) must appear ONCE in the ranked output — otherwise a failed+blacklisted first
        // entry would be re-added/re-probed/re-blacklisted from its duplicate.
        let dup = |desc: &str| RawCandidate {
            name: "Torrentio".into(),
            description: desc.into(),
            info_hash: "hdup".into(),
            file_idx: None,
            file_name: None,
        };
        let raws = vec![
            dup("Show.S01.1080p.WEB-DL.x265\nRD+"),
            dup("Show.S01.1080p.WEB.x265\n\u{26a1}"),
        ];
        let ranked = rank_cached_pack_candidates(&raws, &prefs, &[]);
        assert_eq!(
            ranked.len(),
            1,
            "a repeated infohash must be deduped to a single ranked entry"
        );
        assert_eq!(ranked[0].info_hash, "hdup");
    }

    /// A cached REMUX 1080p candidate with the given hash and (optional) addon-reported file name.
    fn remux_named(hash: &str, file_name: Option<&str>) -> RawCandidate {
        RawCandidate {
            info_hash: hash.into(),
            file_name: file_name.map(String::from),
            ..remux_candidate()
        }
    }

    #[tokio::test]
    async fn rank_changes_skips_known_unverifiable_containers() {
        // The upgrade pass only adopts a release whose tracks PASS the probe, and the probe can never
        // read an `.avi` — so a candidate whose addon-reported file name is a non-MKV/MP4 container
        // is dropped up front (no wasted add + cached-poll + probe). A candidate with NO file name
        // is kept: its container is unknown until staged, and the probe decides.
        let app = app_with(
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(MockProvider::default()),
            mem_store(),
        );
        let baseline = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 10,
            unverified: false,
        };
        let raws = vec![
            remux_named("havi", Some("M.2020.1080p.REMUX.avi")),
            remux_named("hnone", None),
            remux_named("hmkv", Some("M.2020.1080p.REMUX.mkv")),
            remux_named("hmp4", Some("M.2020.1080p.REMUX.mp4")),
        ];
        let ranked = rank_changes(
            &app,
            MediaKind::Movie,
            27205,
            &raws,
            &baseline,
            &[],
            &movie_req(),
        )
        .await;
        let hashes: Vec<_> = ranked.iter().map(|(r, _)| r.info_hash.as_str()).collect();
        assert!(
            !hashes.contains(&"havi"),
            "a known-unverifiable (.avi) candidate must be skipped: {hashes:?}"
        );
        for kept in ["hnone", "hmkv", "hmp4"] {
            assert!(hashes.contains(&kept), "{kept} must be ranked: {hashes:?}");
        }
    }

    /// For an UNKNOWN-quality owned copy every known cached candidate is an improvement whatever its
    /// score, so the language-aware improvement test can't drop a release acquisition would
    /// penalise for the wrong audio language. It is dropped explicitly: a release tagged only with
    /// other languages (`Rus` + 🇷🇺 on an English-original title) never replaces the user's copy of
    /// unknown quality — untagged audio tracks would let it pass the probe. Correctly-tagged and
    /// untagged releases still qualify.
    #[tokio::test]
    async fn rank_changes_drops_wrong_language_candidates_for_an_unknown_current() {
        let app = app_with(
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(MockProvider::default()),
            mem_store(),
        );
        let cand = |hash: &str, tag: &str, flag: &str| RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: format!("M.2020.1080p.BluRay.x264.{tag}\n👤 50 💾 8 GB\n{flag}\nRD+"),
            info_hash: hash.into(),
            file_idx: Some(0),
            file_name: Some(format!("M.2020.1080p.BluRay.x264.{tag}.mkv")),
        };
        let raws = vec![
            cand("hrus", "Rus", "🇷🇺"),
            cand("heng", "ENG", "🇬🇧"),
            cand("hplain", "GRP", ""),
        ];
        for baseline in [
            QualitySummary::unknown(),
            QualitySummary {
                cached: true,
                source_tier: 0,
                resolution: 1080,
                score: 9_999_999,
                unverified: false,
            },
        ] {
            let ranked = rank_changes(
                &app,
                MediaKind::Movie,
                27205,
                &raws,
                &baseline,
                &[],
                &movie_req(),
            )
            .await;
            let mut hashes: Vec<_> = ranked.iter().map(|(r, _)| r.info_hash.as_str()).collect();
            hashes.sort_unstable();
            assert_eq!(
                hashes,
                vec!["heng", "hplain"],
                "{baseline:?}: the wrong-language release is not a candidate"
            );
        }
    }

    #[test]
    fn rank_cached_pack_candidates_skips_known_unverifiable_containers() {
        // Same rule for consolidation / pack-correction ranking: a pack whose reported file is a
        // non-MKV/MP4 container can never pass the staging probe, so it isn't staged.
        let prefs = AcquisitionConfig::default().prefs;
        let raws = vec![
            remux_named("havi", Some("S.S01E01.1080p.REMUX.avi")),
            remux_named("hnone", None),
            remux_named("hmkv", Some("S.S01E01.1080p.REMUX.mkv")),
        ];
        let ranked = rank_cached_pack_candidates(&raws, &prefs, &[]);
        let hashes: Vec<_> = ranked.iter().map(|r| r.info_hash.as_str()).collect();
        assert!(!hashes.contains(&"havi"), "{hashes:?}");
        assert!(
            hashes.contains(&"hnone") && hashes.contains(&"hmkv"),
            "{hashes:?}"
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
                spoken_languages: Some(vec![]),
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
    /// The torrents the provider lists: the two scattered episodes. A candidate pack is NOT listed —
    /// it isn't in the account before staging, and upgrade staging never stages a hash that already
    /// is (`add_staging_candidate`).
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
    async fn consolidation_falls_through_rejected_pack_to_the_next_candidate() {
        // Two ranked pack candidates: the top-scored one (1080p REMUX) resolves to a MULTI-SEASON
        // pack (rejected), the next (1080p BluRay) is a clean full-season pack. Consolidation must
        // fall through IN-TICK and adopt the second — the same in-tick fall-through the movie and
        // per-episode paths have; every other consolidate test seeds only ONE candidate.
        let store = mem_store();
        seed_two_scattered_episodes(&store).await; // e1=S01E01, e2=S01E02 (q1080_web)

        // Both are 1080p HEVC cached (so the HEVC bonus can't flip the ranking); REMUX outscores
        // BluRay on source tier, so `hbad` is tried first.
        let bad = RawCandidate {
            name: "S.2019.S01.1080p.BluRay.REMUX.x265".into(),
            description: "REMUX\nRD+".into(),
            info_hash: "hbad".into(),
            file_idx: None,
            file_name: None,
        };
        let good = RawCandidate {
            name: "S.2019.S01.1080p.BluRay.x265".into(),
            description: "BluRay\nRD+".into(),
            info_hash: "hgood".into(),
            file_idx: None,
            file_name: None,
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![bad, good],
        });

        let mut by_id = std::collections::HashMap::new();
        // hbad → multi-season (S01E01, S01E02, S02E01) ⇒ rejected + deleted, fall through.
        by_id.insert(
            "hbad".to_string(),
            TorrentInfo {
                id: "hbad".into(),
                hash: "hbad".into(),
                status: "downloaded".into(),
                files: vec![
                    TorrentFile {
                        id: 0,
                        path: "S.S01E01.1080p.REMUX.mkv".into(),
                        bytes: 3_000_000_000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 1,
                        path: "S.S01E02.1080p.REMUX.mkv".into(),
                        bytes: 3_000_000_000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 2,
                        path: "S.S02E01.1080p.REMUX.mkv".into(),
                        bytes: 3_000_000_000,
                        selected: 1,
                    },
                ],
                links: vec![
                    "https://cdn/b0".into(),
                    "https://cdn/b1".into(),
                    "https://cdn/b2".into(),
                ],
                ..Default::default()
            },
        );
        // hgood → clean single-season pack (S01E01, S01E02) ⇒ adopted.
        by_id.insert(
            "hgood".to_string(),
            TorrentInfo {
                id: "hgood".into(),
                hash: "hgood".into(),
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
                links: vec!["https://cdn/g0".into(), "https://cdn/g1".into()],
                ..Default::default()
            },
        );

        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![
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
            ],
            torrent_info_by_id: by_id,
            resolved_url: Some("https://cdn/g0".into()),
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
        assert!(res.is_ok(), "consolidation completes after falling through");

        // The SECOND candidate (hgood) is adopted with the full-season provides.
        let pack = store
            .get_owned("hgood".into())
            .await
            .expect("the second candidate must be adopted");
        assert_eq!(pack.provides, vec![(1, 1), (1, 2)]);
        assert!(
            store.get_owned("hbad".into()).await.is_none(),
            "the rejected multi-season pack must NOT be recorded owned"
        );
        // Both episode slots repoint to the adopted pack, and the scattered singletons are pruned.
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            "hgood"
        );
        assert!(store.get_owned("e1".into()).await.is_none());
        let d = deleted.lock().unwrap();
        assert!(
            d.contains(&"hbad".to_string()),
            "the rejected pack is deleted"
        );
        assert!(
            d.contains(&"te1".to_string()) && d.contains(&"te2".to_string()),
            "scattered episode torrents pruned after adopting the second candidate"
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
            // The first get_torrents is the pre-add presence check (`add_staging_candidate`); the
            // second is the post-stage prune listing → fail it there.
            fail_get_torrents_after: Some(1),
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
    async fn show_consolidation_skips_staging_when_library_active() {
        // The library is active BEFORE staging, so the pre-stage idle gate defers immediately: the
        // pack is never added/selected/probed (no wasted provider work + CDN probe that a later idle
        // gate would only roll back), and per-episode content stays intact. Mirrors the movie /
        // per-episode / pack-correction paths, which all idle-check before staging.
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
            !deleted.lock().unwrap().contains(&"tpack".to_string()),
            "the pack is never staged (so never deleted) when the library is active pre-stage"
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

    #[tokio::test]
    async fn show_consolidation_skips_a_season_left_partially_repointed() {
        // Guards the recovery behaviour documented at the repoint-failure branch: after a PARTIAL
        // repoint (pack owned-record persisted, E01 repointed to the pack, E02's repoint failed so its
        // slot still points at the scattered singleton, and the deferred prune left both singletons
        // owned), the NEXT consolidation tick must NOT re-run — `already_pack` sees the persisted pack
        // covering the season and short-circuits it. So nothing is deleted (no re-consolidation churn),
        // both scattered singletons persist (recovery is the duplicate-dedup pass's job, not
        // consolidation's), and — critically — no selection slot resolves to a deleted/absent hash.
        let store = mem_store();
        // The residue a partial repoint leaves: two scattered singletons still owned + the pack owned.
        seed_two_scattered_episodes(&store).await;
        let pack = OwnedRecord {
            provides: vec![(1, 1), (1, 2)],
            ..ep_record(1, 1, q1080_bluray())
        };
        store.put_owned("hpack".into(), pack.clone()).await.unwrap();
        // E01 was repointed to the pack; E02's repoint failed, so its slot still points at the singleton.
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 1),
                SelectionEntry {
                    hash: "hpack".into(),
                    file_path: "S.S01E01.1080p.BluRay.mkv".into(),
                },
            )
            .await
            .unwrap();

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

        // The current store state (both singletons + the pack) is what this tick observes.
        let owned = vec![
            ("e1".to_string(), ep_record(1, 1, q1080_web())),
            ("e2".to_string(), ep_record(1, 2, q1080_web())),
            ("hpack".to_string(), pack),
        ];
        let group = vec!["e1".to_string(), "e2".to_string(), "hpack".to_string()];
        let res = consolidate_show_seasons(
            &app,
            SHOW_TMDB,
            &group,
            &owned,
            &[(1, 1), (1, 2)],
            Duration::from_secs(300),
        )
        .await;
        assert!(
            matches!(res, Ok(())),
            "a season already covered by a persisted pack is skipped (no re-consolidation)"
        );
        assert!(
            deleted.lock().unwrap().is_empty(),
            "the redundant scattered singletons are NOT deleted by consolidation (dedup's job)"
        );
        assert!(
            store.get_owned("e1".into()).await.is_some()
                && store.get_owned("e2".into()).await.is_some(),
            "both scattered singletons persist until the duplicate-dedup pass reclaims them"
        );
        // No slot may resolve to a deleted/absent hash: E01 → pack (present), E02 → e2 (present).
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            "hpack",
            "E01 stays repointed at the present pack"
        );
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 2))
                .await
                .unwrap()
                .hash,
            "e2",
            "E02's failed-repoint slot still points at its present scattered singleton"
        );
    }

    fn q1080_web() -> crate::release::QualitySummary {
        crate::release::QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 1,
            unverified: false,
        }
    }
    fn q1080_bluray() -> crate::release::QualitySummary {
        crate::release::QualitySummary {
            cached: true,
            source_tier: 6_000,
            resolution: 1080,
            score: 2,
            unverified: false,
        }
    }
    fn q2160_remux() -> crate::release::QualitySummary {
        crate::release::QualitySummary {
            cached: true,
            source_tier: 8_000,
            resolution: 2160,
            score: 9,
            unverified: false,
        }
    }
    fn q720_web() -> crate::release::QualitySummary {
        crate::release::QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 720,
            score: 1,
            unverified: false,
        }
    }

    // ── per-episode upgrade/downgrade + over-ceiling pack correction tests (Task 5) ──────────
    //
    // These call `upgrade_show_episodes_and_packs` directly rather than the public dispatch
    // target `try_upgrade_show`. `try_upgrade_show` always additionally invokes the TMDB-driven
    // `try_consolidate_show` (see its doc comment) — reaching a REAL `https://api.themoviedb.org`
    // call whenever the show group still has a resolvable IMDB id after this phase runs (true in
    // every scenario below, since a resolvable id is exactly what's needed to exercise real
    // scrape/upgrade logic here). Every OTHER show test in this module deliberately avoids that by
    // calling `consolidate_show_seasons` (the pure/offline half of that same split) instead of
    // `try_consolidate_show` — this mirrors that precedent so these new tests stay fully
    // deterministic and network-free too. `try_upgrade_show` itself is exercised structurally by
    // the `run_upgrade_once` dispatch (Movie/Show branch) — its own behaviour beyond "call the two
    // pieces in order" is covered by this file's existing (unchanged) consolidation tests plus the
    // cases below.

    /// A cached 1080p WEB-DL single-episode candidate for S01E01 — a meaningful upgrade over a
    /// 720p WEB singleton (same source tier, higher resolution).
    fn ep_1080_web_candidate() -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "S.S01E01.1080p.WEB-DL.x265\nRD+".into(),
            info_hash: "hep1080".into(),
            file_idx: Some(0),
            file_name: Some("S.S01E01.1080p.WEB-DL.mkv".into()),
        }
    }
    /// A cached 1080p REMUX single-episode candidate for S01E01 — the within-ceiling corrective
    /// replacement for an over-ceiling 2160p singleton.
    fn ep_1080_remux_candidate() -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "S.S01E01.1080p.BluRay.REMUX.x265\nRD+".into(),
            info_hash: "hep1080d".into(),
            file_idx: Some(0),
            file_name: Some("S.S01E01.1080p.REMUX.mkv".into()),
        }
    }

    /// Harness for the Task 5 show tests: an owned singleton episode or full-season pack, a
    /// scraped candidate (or none), and `set_allow_downgrade` to flip the corrective-downgrade
    /// flag — mirrors `DowngradeHarness` above but for the per-episode/pack-correction path.
    /// Only the fields relevant to a given scenario are populated; the rest are left empty.
    struct ShowUpgradeHarness {
        app: AppState,
        tmdb: u64,
        group_hashes: Vec<String>,
        old_720_hash: String,
        cand_1080_hash: String,
        old_2160_hash: String,
        pack_hash: String,
        /// The provider's `delete_torrent` log (torrent ids).
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
        /// The provider's `add_magnet` log (infohashes): every candidate that was staged.
        added: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ShowUpgradeHarness {
        fn set_allow_downgrade(&mut self, v: bool) {
            let mut cfg = (*self.app.config).clone();
            cfg.allow_resolution_downgrade = v;
            self.app.config = Arc::new(cfg);
        }
    }

    /// Own S01E01 as a cached 720p WEB singleton; a cached 1080p WEB-DL S01E01 is scrape-available
    /// (a meaningful upgrade — same tier, higher resolution).
    async fn show_harness_singleton_720_ep() -> ShowUpgradeHarness {
        show_harness_singleton_720_ep_with(false).await
    }

    /// [`show_harness_singleton_720_ep`]; with `cand_in_account` the candidate's torrent is ALREADY in
    /// the provider account (listed as `tep1080`, outside the show's group — e.g. identified under
    /// another title, or an unidentified manual add) and the add returns that EXISTING id, as TorBox's
    /// `createtorrent` does for a present infohash.
    async fn show_harness_singleton_720_ep_with(cand_in_account: bool) -> ShowUpgradeHarness {
        let store = mem_store();
        let old_hash = "e720".to_string();
        store
            .put_owned(old_hash.clone(), ep_record(1, 1, q720_web()))
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 1),
                SelectionEntry {
                    hash: old_hash.clone(),
                    file_path: "old720.mkv".into(),
                },
            )
            .await
            .unwrap();

        let cand = ep_1080_web_candidate();
        let cand_hash = cand.info_hash.clone();
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut torrents = vec![Torrent {
            id: "told720".into(),
            hash: old_hash.clone(),
            status: "downloaded".into(),
            ..Default::default()
        }];
        if cand_in_account {
            torrents.push(Torrent {
                id: "tep1080".into(),
                hash: cand_hash.clone(),
                status: "downloaded".into(),
                ..Default::default()
            });
        }
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents,
            add_magnet: Some(AddMagnetResponse {
                id: "tep1080".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tep1080".into(),
                hash: cand_hash.clone(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "S.S01E01.1080p.WEB-DL.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/ep1080".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/ep1080".into()),
            deleted: deleted.clone(),
            added: added.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        ShowUpgradeHarness {
            app,
            tmdb: SHOW_TMDB,
            group_hashes: vec![old_hash.clone()],
            old_720_hash: old_hash,
            cand_1080_hash: cand_hash,
            old_2160_hash: String::new(),
            pack_hash: String::new(),
            deleted,
            added,
        }
    }

    /// Own S01E01 as a cached 2160p REMUX singleton (above the P1080 default ceiling — as if
    /// acquired before the ceiling was lowered, or mirrored pre-existing); a cached within-ceiling
    /// 1080p REMUX S01E01 is scrape-available as the corrective replacement.
    async fn show_harness_singleton_2160_ep() -> ShowUpgradeHarness {
        let store = mem_store();
        let old_hash = "e2160".to_string();
        store
            .put_owned(old_hash.clone(), ep_record(1, 1, q2160_remux()))
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 1),
                SelectionEntry {
                    hash: old_hash.clone(),
                    file_path: "old2160.mkv".into(),
                },
            )
            .await
            .unwrap();

        let cand = ep_1080_remux_candidate();
        let cand_hash = cand.info_hash.clone();
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "told2160".into(),
                hash: old_hash.clone(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            add_magnet: Some(AddMagnetResponse {
                id: "tep1080d".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tep1080d".into(),
                hash: cand_hash.clone(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "S.S01E01.1080p.REMUX.mkv".into(),
                    bytes: 20_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/ep1080d".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/ep1080d".into()),
            deleted: deleted.clone(),
            added: added.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        ShowUpgradeHarness {
            app,
            tmdb: SHOW_TMDB,
            group_hashes: vec![old_hash.clone()],
            old_720_hash: String::new(),
            cand_1080_hash: cand_hash,
            old_2160_hash: old_hash,
            pack_hash: String::new(),
            deleted,
            added,
        }
    }

    /// Own a cached 2160p full-season pack (S01E01-E02, above the P1080 default ceiling); the
    /// scraper offers only an OVER-ceiling 2160p pack candidate — no within-ceiling replacement
    /// exists, so `release::score`'s hard ceiling filter excludes it entirely.
    async fn show_harness_pack_2160_no_candidate() -> ShowUpgradeHarness {
        let store = mem_store();
        let pack_hash = "pack2160".to_string();
        let rec = OwnedRecord {
            request: AcquireRequest {
                imdb_id: "tt9".into(),
                tmdb_id: SHOW_TMDB,
                kind: MediaKind::Series,
                season: Some(1),
                episode: Some(1),
                original_language: Some("eng".into()),
                spoken_languages: Some(vec![]),
                metadata: show_meta(),
            },
            provenance: Provenance::watchlist("a"),
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![(1, 1), (1, 2)],
            quality: Some(q2160_remux()),
        };
        store.put_owned(pack_hash.clone(), rec).await.unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 1),
                SelectionEntry {
                    hash: pack_hash.clone(),
                    file_path: "S.S01E01.2160p.mkv".into(),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 2),
                SelectionEntry {
                    hash: pack_hash.clone(),
                    file_path: "S.S01E02.2160p.mkv".into(),
                },
            )
            .await
            .unwrap();

        // Only an OVER-CEILING pack candidate is scrape-available — no within-ceiling replacement.
        let cand_4k = RawCandidate {
            name: "Torrentio\n2160p".into(),
            description: "S.2019.S01.2160p.BluRay.REMUX.x265\nRD+".into(),
            info_hash: "hpack4k".into(),
            file_idx: None,
            file_name: None,
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand_4k],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "tpack2160".into(),
                hash: pack_hash.clone(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            deleted: deleted.clone(),
            added: added.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        ShowUpgradeHarness {
            app,
            tmdb: SHOW_TMDB,
            group_hashes: vec![pack_hash.clone()],
            old_720_hash: String::new(),
            cand_1080_hash: String::new(),
            old_2160_hash: String::new(),
            pack_hash,
            deleted,
            added,
        }
    }

    /// Own ONE cached 2160p full-season pack (S01E01+E02, above the P1080 ceiling); the scraper
    /// offers a cached WITHIN-ceiling 1080p full-season pack whose resolved files are S01E01+E02
    /// (same episodes as the owned pack). Provider reports the staged pack as `downloaded` with the
    /// two episode videos so staging succeeds. Models the destructive success path of pack
    /// correction — mirrors `idle_show_with_cached_full_season_pack_consolidates_repoints_and_prunes`'s
    /// provider mock. `cand_1080_hash` carries the new within-ceiling pack's hash.
    async fn show_harness_pack_2160_with_1080_candidate() -> ShowUpgradeHarness {
        show_harness_pack_2160_with_1080_candidate_with(false).await
    }

    /// [`show_harness_pack_2160_with_1080_candidate`]; with `cand_in_account` the replacement pack's
    /// torrent is ALREADY in the provider account (listed as `tpack1080`, outside the show's group)
    /// and the add returns that EXISTING id (TorBox's same-id re-add).
    async fn show_harness_pack_2160_with_1080_candidate_with(
        cand_in_account: bool,
    ) -> ShowUpgradeHarness {
        let store = mem_store();
        let pack_hash = "pack2160".to_string();
        let rec = OwnedRecord {
            request: AcquireRequest {
                imdb_id: "tt9".into(),
                tmdb_id: SHOW_TMDB,
                kind: MediaKind::Series,
                season: Some(1),
                episode: Some(1),
                original_language: Some("eng".into()),
                spoken_languages: Some(vec![]),
                metadata: show_meta(),
            },
            provenance: Provenance::watchlist("a"),
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![(1, 1), (1, 2)],
            quality: Some(q2160_remux()),
        };
        store.put_owned(pack_hash.clone(), rec).await.unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 1),
                SelectionEntry {
                    hash: pack_hash.clone(),
                    file_path: "S.S01E01.2160p.mkv".into(),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 2),
                SelectionEntry {
                    hash: pack_hash.clone(),
                    file_path: "S.S01E02.2160p.mkv".into(),
                },
            )
            .await
            .unwrap();

        // A cached, within-ceiling 1080p FULL-SEASON pack covering the same episodes as the owned pack.
        let new_pack_hash = "hpack1080".to_string();
        let cand = RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "S.2019.S01.1080p.BluRay.x265\nRD+".into(),
            info_hash: new_pack_hash.clone(),
            file_idx: None,
            file_name: None,
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut torrents = vec![Torrent {
            id: "tpack2160".into(),
            hash: pack_hash.clone(),
            status: "downloaded".into(),
            ..Default::default()
        }];
        if cand_in_account {
            torrents.push(Torrent {
                id: "tpack1080".into(),
                hash: new_pack_hash.clone(),
                status: "downloaded".into(),
                ..Default::default()
            });
        }
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents,
            add_magnet: Some(AddMagnetResponse {
                id: "tpack1080".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tpack1080".into(),
                hash: new_pack_hash.clone(),
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
                links: vec!["https://cdn/np1".into(), "https://cdn/np2".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/np1".into()),
            deleted: deleted.clone(),
            added: added.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        ShowUpgradeHarness {
            app,
            tmdb: SHOW_TMDB,
            group_hashes: vec![pack_hash.clone()],
            old_720_hash: String::new(),
            cand_1080_hash: new_pack_hash,
            old_2160_hash: String::new(),
            pack_hash,
            deleted,
            added,
        }
    }

    #[tokio::test]
    async fn singleton_episode_upgrades_to_better_cached_release() {
        // Own S1E1 as a cached 720p singleton; a cached 1080p S1E1 is scrape-available; ceiling 1080.
        let h = show_harness_singleton_720_ep().await;
        upgrade_show_episodes_and_packs(&h.app, h.tmdb, &h.group_hashes, Duration::from_secs(0))
            .await;
        let sel = h
            .app
            .store
            .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
            .await
            .unwrap();
        assert_eq!(
            sel.hash, h.cand_1080_hash,
            "episode slot repointed to the 1080p release"
        );
        assert!(
            h.app
                .store
                .get_owned(h.old_720_hash.clone())
                .await
                .is_none(),
            "old singleton pruned"
        );
    }

    #[tokio::test]
    async fn singleton_episode_downgrade_respects_flag() {
        // Own S1E1 cached 2160p singleton; ceiling 1080 (default); cached 1080p available.
        let mut h = show_harness_singleton_2160_ep().await;
        // Flag OFF: kept.
        assert!(!h.app.config.allow_resolution_downgrade);
        upgrade_show_episodes_and_packs(&h.app, h.tmdb, &h.group_hashes, Duration::from_secs(0))
            .await;
        assert!(
            h.app
                .store
                .get_owned(h.old_2160_hash.clone())
                .await
                .is_some(),
            "dry-run keeps 2160p"
        );
        assert_eq!(
            h.app
                .store
                .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                .await
                .unwrap()
                .hash,
            h.old_2160_hash,
            "selection unchanged in dry-run"
        );
        // Flag ON: corrected.
        h.set_allow_downgrade(true);
        upgrade_show_episodes_and_packs(&h.app, h.tmdb, &h.group_hashes, Duration::from_secs(0))
            .await;
        assert!(
            h.app
                .store
                .get_owned(h.old_2160_hash.clone())
                .await
                .is_none(),
            "flag on prunes 2160p"
        );
        assert_eq!(
            h.app
                .store
                .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                .await
                .unwrap()
                .hash,
            h.cand_1080_hash,
            "episode slot repointed to the within-ceiling replacement"
        );
    }

    #[tokio::test]
    async fn per_episode_upgrade_does_not_downgrade_a_served_over_ceiling_slot() {
        // The duplicate state the per-episode downgrade guard protects: own a WITHIN-ceiling 720p
        // singleton for S1E1 AND an OVER-ceiling 2160p pack that also provides S1E1, with the episode
        // slot pointing at the pack. A scraped 1080p upgrade of the singleton is rated
        // `Improvement::Upgrade` (vs the 720p baseline), so absent the guard the per-episode swap
        // would repoint the slot 2160p→1080p — a corrective downgrade with ALLOW_RESOLUTION_DOWNGRADE
        // OFF. The guard must keep the slot on the pack when the flag is off, and only allow the swap
        // when it is on.
        let store = mem_store();
        let single_hash = "e720".to_string();
        let pack_hash = "pack2160".to_string();
        store
            .put_owned(single_hash.clone(), ep_record(1, 1, q720_web()))
            .await
            .unwrap();
        let pack_rec = OwnedRecord {
            request: AcquireRequest {
                imdb_id: "tt9".into(),
                tmdb_id: SHOW_TMDB,
                kind: MediaKind::Series,
                season: Some(1),
                episode: Some(1),
                original_language: Some("eng".into()),
                spoken_languages: Some(vec![]),
                metadata: show_meta(),
            },
            provenance: Provenance::watchlist("a"),
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![(1, 1), (1, 2)],
            quality: Some(q2160_remux()),
        };
        store.put_owned(pack_hash.clone(), pack_rec).await.unwrap();
        // The episode slot serves the OVER-ceiling pack, not the singleton.
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 1),
                SelectionEntry {
                    hash: pack_hash.clone(),
                    file_path: "S.S01E01.2160p.mkv".into(),
                },
            )
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 2),
                SelectionEntry {
                    hash: pack_hash.clone(),
                    file_path: "S.S01E02.2160p.mkv".into(),
                },
            )
            .await
            .unwrap();

        let cand = ep_1080_web_candidate();
        let cand_hash = cand.info_hash.clone();
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![
                Torrent {
                    id: "told720".into(),
                    hash: single_hash.clone(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
                Torrent {
                    id: "tpack2160".into(),
                    hash: pack_hash.clone(),
                    status: "downloaded".into(),
                    ..Default::default()
                },
            ],
            add_magnet: Some(AddMagnetResponse {
                id: "tep1080".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tep1080".into(),
                hash: cand_hash.clone(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "S.S01E01.1080p.WEB-DL.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/ep1080".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/ep1080".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let mut app = app_with(scraper, provider, store.clone());
        let group_hashes = vec![single_hash.clone(), pack_hash.clone()];

        // Flag OFF (default): the guard keeps the slot on the over-ceiling pack.
        assert!(!app.config.allow_resolution_downgrade);
        upgrade_show_episodes_and_packs(&app, SHOW_TMDB, &group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            pack_hash,
            "flag off: the per-episode swap must NOT repoint the slot off the served over-ceiling pack"
        );

        // Flag ON: the guard steps aside and the per-episode upgrade proceeds.
        {
            let mut cfg = (*app.config).clone();
            cfg.allow_resolution_downgrade = true;
            app.config = Arc::new(cfg);
        }
        upgrade_show_episodes_and_packs(&app, SHOW_TMDB, &group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            cand_hash,
            "flag on: the per-episode upgrade repoints the slot to the within-ceiling replacement"
        );
    }

    #[tokio::test]
    async fn over_ceiling_pack_with_no_conforming_candidate_is_kept() {
        // Own a cached 2160p full-season pack; ceiling 1080; scraper returns NO within-ceiling pack.
        let mut h = show_harness_pack_2160_no_candidate().await;
        h.set_allow_downgrade(true);
        upgrade_show_episodes_and_packs(&h.app, h.tmdb, &h.group_hashes, Duration::from_secs(0))
            .await;
        assert!(
            h.app.store.get_owned(h.pack_hash.clone()).await.is_some(),
            "sole over-ceiling pack kept when no conforming replacement exists"
        );
        assert_eq!(
            h.app
                .store
                .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                .await
                .unwrap()
                .hash,
            h.pack_hash,
            "selection unchanged — nothing to repoint to"
        );
    }

    #[tokio::test]
    async fn over_ceiling_pack_corrected_to_within_ceiling_replacement() {
        // Own a cached 2160p full-season pack (S01E01+E02); ceiling 1080; flag ON; the scraper
        // offers a cached within-ceiling 1080p full-season pack covering the SAME episodes. The
        // over-ceiling pack must be replaced: both episode slots repointed to the new pack, the old
        // pack pruned. This is the destructive SUCCESS path — the most impactful new behaviour.
        let mut h = show_harness_pack_2160_with_1080_candidate().await;
        h.set_allow_downgrade(true);
        upgrade_show_episodes_and_packs(&h.app, h.tmdb, &h.group_hashes, Duration::from_secs(0))
            .await;

        // Both episode slots repointed to the within-ceiling replacement.
        assert_eq!(
            h.app
                .store
                .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                .await
                .unwrap()
                .hash,
            h.cand_1080_hash,
            "E01 slot repointed to the within-ceiling pack"
        );
        assert_eq!(
            h.app
                .store
                .get_selection(crate::store::episode_slot(h.tmdb, 1, 2))
                .await
                .unwrap()
                .hash,
            h.cand_1080_hash,
            "E02 slot repointed to the within-ceiling pack"
        );
        // Old over-ceiling pack pruned from owned.
        assert!(
            h.app.store.get_owned(h.pack_hash.clone()).await.is_none(),
            "old over-ceiling pack pruned"
        );
        // New within-ceiling pack recorded owned, covering the full owned episode set.
        let new_rec = h
            .app
            .store
            .get_owned(h.cand_1080_hash.clone())
            .await
            .expect("within-ceiling replacement recorded owned");
        assert!(
            new_rec.provides.contains(&(1, 1)) && new_rec.provides.contains(&(1, 2)),
            "replacement provides the full owned episode set: {:?}",
            new_rec.provides
        );
    }

    /// Rebuild a harness's app around an UNVERIFIABLE prober (probe `Accept`), keeping its scraper,
    /// provider, store and downgrade flag.
    fn with_unverifiable_prober(app: &AppState) -> AppState {
        let mut rebuilt = app_with_prober(
            app.scraper.clone(),
            app.provider.clone(),
            app.store.clone(),
            unverifiable_prober(),
        );
        let mut cfg = (*rebuilt.config).clone();
        cfg.allow_resolution_downgrade = app.config.allow_resolution_downgrade;
        rebuilt.config = Arc::new(cfg);
        rebuilt
    }

    #[tokio::test]
    async fn episode_upgrade_rejects_unverifiable_staged_candidate() {
        // Per-episode staging adopts only a probe `Pass`: a better 1080p single whose tracks can't be
        // checked leaves the 720p episode in place, unblacklisted.
        let h = show_harness_singleton_720_ep().await;
        let app = with_unverifiable_prober(&h.app);
        upgrade_show_episodes_and_packs(&app, h.tmdb, &h.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            app.store
                .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                .await
                .unwrap()
                .hash,
            h.old_720_hash,
            "episode slot must not move to an unverifiable release"
        );
        assert!(app.store.get_owned(h.old_720_hash.clone()).await.is_some());
        assert!(app
            .store
            .get_owned(h.cand_1080_hash.clone())
            .await
            .is_none());
        assert!(
            !app.store
                .is_blacklisted(MediaKind::Series, h.tmdb, h.cand_1080_hash.clone())
                .await
        );
        let d = h.deleted.lock().unwrap().clone();
        assert!(
            d.contains(&"tep1080".to_string()),
            "the staged torrent must be deleted: {d:?}"
        );
        assert!(
            !d.contains(&"told720".to_string()),
            "the current torrent must not be pruned: {d:?}"
        );
    }

    #[tokio::test]
    async fn over_ceiling_pack_correction_rejects_unverifiable_replacement() {
        // Pack correction adopts only a probe `Pass`: the over-ceiling pack is kept (sole copy) when
        // the only conforming replacement's tracks can't be checked.
        let mut h = show_harness_pack_2160_with_1080_candidate().await;
        h.set_allow_downgrade(true);
        let app = with_unverifiable_prober(&h.app);
        upgrade_show_episodes_and_packs(&app, h.tmdb, &h.group_hashes, Duration::from_secs(0))
            .await;
        for e in [1, 2] {
            assert_eq!(
                app.store
                    .get_selection(crate::store::episode_slot(h.tmdb, 1, e))
                    .await
                    .unwrap()
                    .hash,
                h.pack_hash,
                "E0{e} slot must stay on the over-ceiling pack"
            );
        }
        assert!(app.store.get_owned(h.pack_hash.clone()).await.is_some());
        assert!(app
            .store
            .get_owned(h.cand_1080_hash.clone())
            .await
            .is_none());
        assert!(
            !app.store
                .is_blacklisted(MediaKind::Series, h.tmdb, h.cand_1080_hash.clone())
                .await
        );
        let d = h.deleted.lock().unwrap().clone();
        assert!(
            d.contains(&"tpack1080".to_string()),
            "the staged replacement must be deleted: {d:?}"
        );
        assert!(
            !d.contains(&"tpack2160".to_string()),
            "the over-ceiling pack must not be pruned: {d:?}"
        );
    }

    #[tokio::test]
    async fn consolidation_rejects_unverifiable_pack() {
        // Consolidation adopts only a probe `Pass`: a full-season pack whose tracks can't be checked
        // is dropped and the scattered (checked) episodes keep their selections.
        let store = mem_store();
        seed_two_scattered_episodes(&store).await;
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
        let app = app_with_prober(
            Arc::new(MockScraper {
                candidates: vec![season_pack_candidate()],
            }),
            provider,
            store.clone(),
            unverifiable_prober(),
        );
        let res = consolidate_show_seasons(
            &app,
            SHOW_TMDB,
            &group_two(),
            &owned_two(),
            &[(1, 1), (1, 2)],
            Duration::from_secs(300),
        )
        .await;
        assert!(res.is_ok(), "evaluated to completion: {res:?}");
        assert!(store.get_owned("hpack".into()).await.is_none());
        for (e, h) in [(1, "e1"), (2, "e2")] {
            assert_eq!(
                store
                    .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, e))
                    .await
                    .unwrap()
                    .hash,
                h
            );
            assert!(store.get_owned(h.into()).await.is_some(), "{h} kept");
        }
        let d = deleted.lock().unwrap().clone();
        assert!(d.contains(&"tpack".to_string()), "staged pack dropped");
        assert!(!d.contains(&"te1".to_string()) && !d.contains(&"te2".to_string()));
        assert!(
            !store
                .is_blacklisted(MediaKind::Series, SHOW_TMDB, "hpack".into())
                .await
        );
    }

    #[tokio::test]
    async fn non_idle_library_defers_show_upgrade_leaving_state_unchanged() {
        // FIX 1: a singleton episode WITH a valid better cached candidate, but the library is
        // NON-idle. The per-episode idle guard must bail and SIGNAL deferral (`true`) so
        // `try_upgrade_show` returns `Deferred` and `run_upgrade_once` leaves the round-robin cursor
        // unstamped — nothing swapped/pruned. Asserted on the offline core
        // `upgrade_show_episodes_and_packs` (returns `true` + zero store mutations) rather than
        // `try_upgrade_show`: the latter's consolidation tail makes a live TMDB call — the one
        // network dependency this file's show-test split exists to avoid — and the wrapper's
        // `if deferred { Err(Deferred) }` is trivial straight-line logic over this bool.
        let h = show_harness_singleton_720_ep().await;
        // Mark the library active; a 60s window then reports it non-idle.
        h.app.read_activity.touch("Shows/S/S01E01.mkv").await;

        let deferred = upgrade_show_episodes_and_packs(
            &h.app,
            h.tmdb,
            &h.group_hashes,
            Duration::from_secs(60),
        )
        .await;

        assert!(
            deferred,
            "a non-idle library must defer the per-episode phase (cursor left unstamped upstream)"
        );
        assert_eq!(
            h.app
                .store
                .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                .await
                .unwrap()
                .hash,
            h.old_720_hash,
            "selection unchanged while deferred"
        );
        assert!(
            h.app
                .store
                .get_owned(h.old_720_hash.clone())
                .await
                .is_some(),
            "old owned record still present (nothing pruned)"
        );
        assert!(
            h.app
                .store
                .get_owned(h.cand_1080_hash.clone())
                .await
                .is_none(),
            "candidate not staged/recorded while deferred"
        );
    }

    /// Symmetric to the movie path's `movie_with_unknown_quality_is_upgraded_and_converges`: an owned
    /// singleton episode of UNKNOWN quality — none recorded, or an untagged axis (even with a higher
    /// score than the candidate) — is the lowest quality, so a known cached candidate replaces it.
    #[tokio::test]
    async fn unknown_quality_singleton_episode_is_upgraded() {
        for current in [
            None,
            Some(QualitySummary {
                cached: true,
                source_tier: 0,
                resolution: 1080,
                score: 9_999_999,
                unverified: false,
            }),
        ] {
            let h = show_harness_singleton_720_ep().await;
            let rec = OwnedRecord {
                quality: current.clone(),
                ..ep_record(1, 1, q720_web())
            };
            h.app
                .store
                .put_owned(h.old_720_hash.clone(), rec)
                .await
                .unwrap();
            let deferred = upgrade_show_episodes_and_packs(
                &h.app,
                h.tmdb,
                &h.group_hashes,
                Duration::from_secs(0),
            )
            .await;
            assert!(!deferred);
            assert_eq!(
                h.app
                    .store
                    .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                    .await
                    .unwrap()
                    .hash,
                h.cand_1080_hash,
                "{current:?}: slot repointed to the known release"
            );
            assert!(
                h.app
                    .store
                    .get_owned(h.old_720_hash.clone())
                    .await
                    .is_none(),
                "{current:?}: unknown singleton pruned"
            );
        }
    }

    /// Phase (a) repoints the episode slot to what it adopts, so it must judge candidates against
    /// what the slot SERVES, not only the singleton's own quality: a singleton (unknown, or a known
    /// but worse one) whose episode is served by a better owned pack must not move the slot from the
    /// pack to a candidate that beats the singleton but not the pack (here 1080p WEB over a 1080p
    /// BluRay pack — a tier drop, plus an extra torrent).
    #[tokio::test]
    async fn singleton_upgrade_never_repoints_a_slot_served_by_a_better_pack() {
        for single in [None, Some(q720_web())] {
            let mut h = show_harness_singleton_720_ep().await;
            h.app
                .store
                .put_owned(
                    h.old_720_hash.clone(),
                    OwnedRecord {
                        quality: single.clone(),
                        ..ep_record(1, 1, q720_web())
                    },
                )
                .await
                .unwrap();
            let pack = "pk".to_string();
            h.app
                .store
                .put_owned(
                    pack.clone(),
                    OwnedRecord {
                        provides: vec![(1, 1), (1, 2)],
                        ..ep_record(1, 1, q1080_bluray())
                    },
                )
                .await
                .unwrap();
            h.app
                .store
                .put_selection(
                    crate::store::episode_slot(h.tmdb, 1, 1),
                    SelectionEntry {
                        hash: pack.clone(),
                        file_path: "S.S01E01.1080p.BluRay.mkv".into(),
                    },
                )
                .await
                .unwrap();
            h.group_hashes = vec![h.old_720_hash.clone(), pack.clone()];
            let deferred = upgrade_show_episodes_and_packs(
                &h.app,
                h.tmdb,
                &h.group_hashes,
                Duration::from_secs(0),
            )
            .await;
            assert!(!deferred);
            assert_eq!(
                h.app
                    .store
                    .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                    .await
                    .unwrap()
                    .hash,
                pack,
                "{single:?}: the slot stays on the better pack"
            );
            assert!(
                h.added.lock().unwrap().is_empty(),
                "{single:?}: nothing staged — the candidate is no better than what is served"
            );

            // Control: a candidate that beats the pack too (1080p REMUX) is still adopted.
            let remux = ep_1080_remux_candidate();
            let app = app_with(
                Arc::new(MockScraper {
                    candidates: vec![remux.clone()],
                }),
                h.app.provider.clone(),
                h.app.store.clone(),
            );
            assert!(
                !upgrade_show_episodes_and_packs(
                    &app,
                    h.tmdb,
                    &h.group_hashes,
                    Duration::from_secs(0)
                )
                .await
            );
            assert_eq!(
                app.store
                    .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                    .await
                    .unwrap()
                    .hash,
                remux.info_hash,
                "{single:?}: a candidate better than the served pack is adopted"
            );
        }
    }

    /// A pack with NO recorded quality serving an episode is the lowest quality too: phase (a2)
    /// splits the served episode off to a known cached single.
    #[tokio::test]
    async fn pack_served_episode_of_an_unknown_quality_pack_is_upgraded() {
        let f = pack_fixture(
            &[(1, q1080_web()), (2, q1080_web()), (3, q1080_web())],
            &[1, 2, 3, 4],
            &[4],
            q_sd_web_unverified(),
            vec![(4, single_candidate(4, "hy4", "720p", "720p.HDTV"))],
            false,
        )
        .await;
        let pack = f.store.get_owned(PACK.into()).await.unwrap();
        f.store
            .put_owned(
                PACK.into(),
                OwnedRecord {
                    quality: None,
                    ..pack
                },
            )
            .await
            .unwrap();
        let deferred = upgrade_show_episodes_and_packs(
            &f.app,
            SHOW_TMDB,
            &f.group_hashes,
            Duration::from_secs(0),
        )
        .await;
        assert!(!deferred);
        assert_eq!(
            slot_hash(&f.store, 4).await,
            "hy4",
            "E4 repointed to the known single"
        );
    }

    /// Owns S1E1 as a cached 720p singleton; the scrape's top-scored candidate resolves to a pack
    /// whose files are S01E02 only (lacks the target episode E1 → `NoChange`), and the next-best is
    /// a clean S01E01 1080p release. The per-episode loop must fall through the wrong candidate and
    /// adopt the good one within the same tick.
    #[tokio::test]
    async fn episode_upgrade_falls_through_missing_episode_to_next_candidate() {
        let store = mem_store();
        let old = "eold720".to_string();
        store
            .put_owned(old.clone(), ep_record(1, 1, q720_web()))
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 1),
                SelectionEntry {
                    hash: old.clone(),
                    file_path: "old.S01E01.720p.mkv".into(),
                },
            )
            .await
            .unwrap();

        let wrong = RawCandidate {
            name: "Show.S01E02.1080p.REMUX".into(),
            description: "REMUX\nRD+".into(),
            info_hash: "hwrong".into(),
            file_idx: None,
            file_name: None,
        }; // higher score, wrong episode
        let good = RawCandidate {
            name: "Show.S01E01.1080p.WEB".into(),
            description: "WEB\nRD+".into(),
            info_hash: "hgood".into(),
            file_idx: None,
            file_name: None,
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![wrong.clone(), good.clone()],
        });

        let mut by_id = std::collections::HashMap::new();
        by_id.insert(
            "hwrong".to_string(),
            TorrentInfo {
                id: "hwrong".into(),
                hash: "hwrong".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Show.S01E02.1080p.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/w0".into()],
                ..Default::default()
            },
        );
        by_id.insert(
            "hgood".to_string(),
            TorrentInfo {
                id: "hgood".into(),
                hash: "hgood".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Show.S01E01.1080p.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/g0".into()],
                ..Default::default()
            },
        );

        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "eold720".into(),
                hash: old.clone(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            torrent_info_by_id: by_id,
            resolved_url: Some("https://cdn/g0".into()),
            deleted: Arc::new(std::sync::Mutex::new(Vec::new())),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        let deferred = upgrade_show_episodes_and_packs(
            &app,
            SHOW_TMDB,
            std::slice::from_ref(&old),
            Duration::from_secs(0),
        )
        .await;
        assert!(!deferred, "adopting a candidate is not a deferral");
        assert_eq!(
            app.store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            "hgood",
            "episode slot repointed to the correct-episode candidate"
        );
        assert!(
            app.store.get_owned("eold720".into()).await.is_none(),
            "old singleton pruned"
        );
    }

    #[tokio::test]
    async fn per_episode_upgrade_rejects_multi_episode_pack() {
        // A per-episode upgrade must NOT adopt a single-season MULTI-episode pack that merely
        // CONTAINS the target episode: it would be recorded with provides=[(season, episode)] —
        // truncating the pack's real episode set, hiding its other episodes (re-acquired as
        // duplicate singletons) and reintroducing the season-pack churn `provides` prevents. The
        // pack is left to the consolidation path; the per-episode slot must stay unchanged.
        let store = mem_store();
        let old = "eold720".to_string();
        store
            .put_owned(old.clone(), ep_record(1, 5, q720_web()))
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 5),
                SelectionEntry {
                    hash: old.clone(),
                    file_path: "old.S01E05.720p.mkv".into(),
                },
            )
            .await
            .unwrap();

        // The ONLY candidate is a single-season 1080p pack that contains E05 AND E06 (higher score
        // than the owned 720p). Before the fix it was adopted with a truncated provides=[(1,5)].
        let pack = RawCandidate {
            name: "Show.S01.1080p.WEB".into(),
            description: "WEB\nRD+".into(),
            info_hash: "hpack".into(),
            file_idx: None,
            file_name: None,
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![pack.clone()],
        });

        let mut by_id = std::collections::HashMap::new();
        by_id.insert(
            "hpack".to_string(),
            TorrentInfo {
                id: "hpack".into(),
                hash: "hpack".into(),
                status: "downloaded".into(),
                files: vec![
                    TorrentFile {
                        id: 0,
                        path: "Show.S01E05.1080p.mkv".into(),
                        bytes: 2_000_000_000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 1,
                        path: "Show.S01E06.1080p.mkv".into(),
                        bytes: 2_000_000_000,
                        selected: 1,
                    },
                ],
                links: vec!["https://cdn/p0".into(), "https://cdn/p1".into()],
                ..Default::default()
            },
        );

        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent {
                id: "eold720".into(),
                hash: old.clone(),
                status: "downloaded".into(),
                ..Default::default()
            }],
            torrent_info_by_id: by_id,
            resolved_url: Some("https://cdn/p0".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        let deferred = upgrade_show_episodes_and_packs(
            &app,
            SHOW_TMDB,
            std::slice::from_ref(&old),
            Duration::from_secs(0),
        )
        .await;
        assert!(!deferred, "rejecting a pack is not a deferral");
        assert_eq!(
            app.store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 5))
                .await
                .unwrap()
                .hash,
            old,
            "E05 slot must stay on the singleton; a multi-episode pack is not a per-episode candidate"
        );
        assert!(
            app.store.get_owned("hpack".into()).await.is_none(),
            "multi-episode pack must not be adopted (would truncate provides to [(1,5)])"
        );
        assert!(
            app.store.get_owned("eold720".into()).await.is_some(),
            "old singleton kept — nothing better was adopted"
        );
    }

    #[tokio::test]
    async fn per_episode_upgrade_propagates_apply_upgrade_defer() {
        // F7: the per-episode loop discarded apply_upgrade's result. If the prune listing is
        // unavailable (or the library became active) AFTER staging, apply_upgrade rolls back and
        // returns Deferred — the loop must PROPAGATE that (return true → round-robin cursor left
        // unstamped, reconsidered next tick), not swallow it and log a false "swapped". Force the
        // deferral deterministically via a listing-fetch failure (fetched only after staging).
        let store = mem_store();
        let old = "e1old720".to_string();
        store
            .put_owned(old.clone(), ep_record(1, 1, q720_web()))
            .await
            .unwrap();
        store
            .put_selection(
                crate::store::episode_slot(SHOW_TMDB, 1, 1),
                SelectionEntry {
                    hash: old.clone(),
                    file_path: "old.S01E01.720p.mkv".into(),
                },
            )
            .await
            .unwrap();

        let cand = RawCandidate {
            name: "Show.S01E01.1080p.WEB".into(),
            description: "WEB\nRD+".into(),
            info_hash: "hnew1080".into(),
            file_idx: None,
            file_name: None,
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            // Call 1 = the pre-add presence check; call 2 = the post-stage prune listing → fail it
            // there, so apply_upgrade defers.
            fail_get_torrents_after: Some(1),
            add_magnet: Some(AddMagnetResponse {
                id: "tnew1080".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tnew1080".into(),
                hash: "hnew1080".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Show.S01E01.1080p.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/n0".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/n0".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        let deferred = upgrade_show_episodes_and_packs(
            &app,
            SHOW_TMDB,
            std::slice::from_ref(&old),
            Duration::from_secs(0),
        )
        .await;

        assert!(
            deferred,
            "apply_upgrade Deferred on the per-episode path must propagate (cursor left unstamped)"
        );
        assert_eq!(
            app.store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            old,
            "selection unchanged after apply_upgrade rollback"
        );
        assert!(
            app.store.get_owned("e1old720".into()).await.is_some(),
            "old singleton kept"
        );
        assert!(
            app.store.get_owned("hnew1080".into()).await.is_none(),
            "staged candidate not recorded (rolled back)"
        );
        assert!(
            deleted.lock().unwrap().contains(&"tnew1080".to_string()),
            "staged torrent dropped on rollback"
        );
    }

    #[tokio::test]
    async fn over_ceiling_pack_correction_propagates_apply_upgrade_defer() {
        // F7: correct_over_ceiling_packs discarded apply_upgrade's result the same way. On a
        // post-staging Deferred it must return true (propagate the defer, leave the cursor unstamped
        // and the over-ceiling pack intact for a retry) rather than log a false "pack corrected".
        let store = mem_store();
        let pack_hash = "pack2160".to_string();
        let rec = OwnedRecord {
            request: AcquireRequest {
                imdb_id: "tt9".into(),
                tmdb_id: SHOW_TMDB,
                kind: MediaKind::Series,
                season: Some(1),
                episode: Some(1),
                original_language: Some("eng".into()),
                spoken_languages: Some(vec![]),
                metadata: show_meta(),
            },
            provenance: Provenance::watchlist("a"),
            added_at: 1,
            status: OwnedStatus::Verified,
            provides: vec![(1, 1), (1, 2)],
            quality: Some(q2160_remux()),
        };
        store.put_owned(pack_hash.clone(), rec).await.unwrap();
        for (s, e, path) in [
            (1u32, 1u32, "S.S01E01.2160p.mkv"),
            (1, 2, "S.S01E02.2160p.mkv"),
        ] {
            store
                .put_selection(
                    crate::store::episode_slot(SHOW_TMDB, s, e),
                    SelectionEntry {
                        hash: pack_hash.clone(),
                        file_path: path.into(),
                    },
                )
                .await
                .unwrap();
        }

        let cand = RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "S.2019.S01.1080p.BluRay.x265\nRD+".into(),
            info_hash: "hpack1080".into(),
            file_idx: None,
            file_name: None,
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand],
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            // Call 1 = the pre-add presence check; call 2 = the post-stage prune listing → fail it
            // there, so apply_upgrade defers.
            fail_get_torrents_after: Some(1),
            add_magnet: Some(AddMagnetResponse {
                id: "tpack1080".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tpack1080".into(),
                hash: "hpack1080".into(),
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
                links: vec!["https://cdn/np1".into(), "https://cdn/np2".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/np1".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = {
            let mut a = app_with(scraper, provider, store.clone());
            let mut cfg = (*a.config).clone();
            cfg.allow_resolution_downgrade = true; // permit the corrective downgrade
            a.config = Arc::new(cfg);
            a
        };

        let deferred = upgrade_show_episodes_and_packs(
            &app,
            SHOW_TMDB,
            std::slice::from_ref(&pack_hash),
            Duration::from_secs(0),
        )
        .await;

        assert!(
            deferred,
            "apply_upgrade Deferred during pack correction must propagate (cursor left unstamped)"
        );
        assert!(
            app.store.get_owned(pack_hash.clone()).await.is_some(),
            "over-ceiling pack kept intact for retry (rollback)"
        );
        assert!(
            app.store.get_owned("hpack1080".into()).await.is_none(),
            "staged replacement not recorded (rolled back)"
        );
        assert_eq!(
            app.store
                .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1))
                .await
                .unwrap()
                .hash,
            pack_hash,
            "E01 slot unchanged after rollback"
        );
    }

    // ── pack-served episode upgrade tests (phase a2) ──────────────────────────────────────────

    fn served(entries: &[(u32, &str)]) -> std::collections::HashMap<(u32, u32), String> {
        entries
            .iter()
            .map(|(e, h)| ((1, *e), h.to_string()))
            .collect()
    }
    const PACK_E1_E4: [(u32, u32); 4] = [(1, 1), (1, 2), (1, 3), (1, 4)];

    #[test]
    fn pack_prunable_when_last_served_episode_moves_and_rest_covered() {
        assert!(pack_prunable_after_repoint(
            "pack",
            &PACK_E1_E4,
            (1, 4),
            &served(&[(1, "s1"), (2, "s2"), (3, "s3"), (4, "pack")]),
            &PACK_E1_E4,
        ));
    }

    #[test]
    fn pack_kept_while_it_still_serves_another_episode() {
        assert!(!pack_prunable_after_repoint(
            "pack",
            &PACK_E1_E4,
            (1, 4),
            &served(&[(1, "s1"), (2, "s2"), (3, "pack"), (4, "pack")]),
            &PACK_E1_E4,
        ));
    }

    #[test]
    fn pack_kept_when_an_unserved_episode_is_not_covered_elsewhere() {
        assert!(!pack_prunable_after_repoint(
            "pack",
            &PACK_E1_E4,
            (1, 4),
            &served(&[(1, "s1"), (3, "s3"), (4, "pack")]),
            &[(1, 1), (1, 3), (1, 4)],
        ));
    }

    #[test]
    fn pack_prune_counts_only_present_or_just_adopted_coverage() {
        let plan = |adopted: &[&str]| PackPrune {
            tmdb_id: SHOW_TMDB,
            hash: "pack".into(),
            provides: PACK_E1_E4.to_vec(),
            repointed: (1, 4),
            served_by: served(&[(1, "s1"), (2, "s2"), (3, "s3"), (4, "pack")]),
            others: (1..=3u32)
                .map(|e| (format!("s{e}"), vec![(1, e)]))
                .collect(),
            adopted: adopted.iter().map(|h| h.to_string()).collect(),
        };
        let listing = |hashes: &[&str]| -> Vec<Torrent> {
            hashes
                .iter()
                .map(|h| Torrent {
                    id: format!("t{h}"),
                    hash: h.to_string(),
                    ..Default::default()
                })
                .collect()
        };
        assert_eq!(
            plan(&[]).resolve(&listing(&["s1", "S2", "s3", "pack"])),
            vec!["pack".to_string()],
            "every covering copy present (hash match is case-insensitive) → prune"
        );
        assert!(
            plan(&[])
                .resolve(&listing(&["s1", "s3", "pack"]))
                .is_empty(),
            "s2 lapsed off the provider → the pack is the only present holder of E2 → keep"
        );
        assert_eq!(
            plan(&["S2"]).resolve(&listing(&["s1", "s3", "pack"])),
            vec!["pack".to_string()],
            "s2 adopted earlier this tick counts as present even before the listing shows it"
        );
        assert_eq!(
            PrunePlan::Hashes(vec!["x".into()]).resolve(&[]),
            vec!["x".to_string()],
            "a fixed prune list ignores the listing"
        );
    }

    /// Every `(season, episode)` an [`EpisodeScraper`] was asked for, in call order.
    type ScrapeLog = Arc<std::sync::Mutex<Vec<(Option<u32>, Option<u32>)>>>;

    /// A scraper scripted per requested episode that records every `(season, episode)` it is asked
    /// for, so a test can pin exactly which scrapes a phase issued (and how many).
    struct EpisodeScraper {
        by_episode: std::collections::HashMap<u32, Vec<RawCandidate>>,
        calls: ScrapeLog,
    }
    #[async_trait]
    impl Scraper for EpisodeScraper {
        async fn find(
            &self,
            _i: &str,
            _k: MediaKind,
            season: Option<u32>,
            episode: Option<u32>,
        ) -> Result<Vec<RawCandidate>, crate::error::AppError> {
            self.calls.lock().unwrap().push((season, episode));
            Ok(episode
                .and_then(|e| self.by_episode.get(&e).cloned())
                .unwrap_or_default())
        }
    }

    /// YAGA's pack quality: an SD (`resolution: 0`) WEB release whose tracks could never be checked
    /// (a `.avi`), so `unverified`.
    fn q_sd_web_unverified() -> QualitySummary {
        QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 0,
            score: 1,
            unverified: true,
        }
    }

    /// A cached true-singleton S01E`episode` candidate tagged `tag` (e.g. `1080p.WEB-DL`).
    fn single_candidate(episode: u32, hash: &str, res_label: &str, tag: &str) -> RawCandidate {
        RawCandidate {
            name: format!("Torrentio\n{res_label}"),
            description: format!("S.S01E{episode:02}.{tag}.x265\nRD+"),
            info_hash: hash.into(),
            file_idx: Some(0),
            file_name: Some(format!("S.S01E{episode:02}.{tag}.mkv")),
        }
    }

    const PACK: &str = "ypack";

    struct PackFixture {
        app: AppState,
        store: Store,
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
        /// Every infohash `add_magnet` was called with (i.e. every staged candidate), in order.
        added: Arc<std::sync::Mutex<Vec<String>>>,
        calls: ScrapeLog,
        group_hashes: Vec<String>,
    }

    /// The staged `TorrentInfo` for a single-file candidate: cached, one selected file named after
    /// the candidate's `file_name`, its torrent id == its infohash (the mock's per-candidate mode).
    fn single_file_info(cand: &RawCandidate) -> TorrentInfo {
        TorrentInfo {
            id: cand.info_hash.clone(),
            hash: cand.info_hash.clone(),
            status: "downloaded".into(),
            files: vec![TorrentFile {
                id: 0,
                path: cand.file_name.clone().unwrap(),
                bytes: 2_000_000_000,
                selected: 1,
            }],
            links: vec![format!("https://cdn/{}", cand.info_hash)],
            ..Default::default()
        }
    }

    /// Extra knobs for [`pack_fixture_with`] beyond the common [`pack_fixture`] shape.
    #[derive(Default)]
    struct PackOpts {
        /// Make the post-stage prune listing fail (forcing `apply_upgrade` to defer).
        fail_listing: bool,
        /// Owned hashes left OUT of the provider listing: still owned + selected, but their torrent
        /// has lapsed off the provider (a `Verified`-but-absent record).
        absent: Vec<String>,
        /// Staged `TorrentInfo` overrides keyed by their id/infohash (e.g. a candidate that resolves
        /// to a multi-episode pack rather than the default single file).
        infos: Vec<TorrentInfo>,
        /// Serve the account listing through a [`LiveListingProvider`], so it reflects every add and
        /// delete (a pruned pack leaves the listing and can be re-staged, as on a real account).
        live_listing: bool,
    }

    /// A provider whose `get_torrents` reflects `add_magnet` and `delete_torrent`, as a real account
    /// does: a staged candidate appears in the listing and a deleted torrent leaves it. (A plain
    /// [`MockProvider`] serves a static listing, so a pruned torrent stays "present" and the staging
    /// guard never lets it be re-staged.) Everything else — the add/delete logs, per-id
    /// `TorrentInfo`s, failure injection — is the wrapped mock's; `get_torrents` still calls the mock
    /// first so its listing-failure knobs keep working.
    #[derive(Debug)]
    struct LiveListingProvider {
        inner: MockProvider,
        listing: std::sync::Mutex<Vec<Torrent>>,
    }
    #[async_trait]
    impl DebridProvider for LiveListingProvider {
        fn name(&self) -> &'static str {
            "live-listing"
        }
        async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
            self.inner.get_torrents().await?;
            Ok(self.listing.lock().unwrap().clone())
        }
        async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
            self.inner.get_torrent_info(id).await
        }
        async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
            let added = self.inner.add_magnet(magnet).await?;
            let hash = magnet.rsplit("urn:btih:").next().unwrap_or("").to_string();
            let mut listing = self.listing.lock().unwrap();
            if !listing.iter().any(|t| t.id == added.id) {
                listing.push(Torrent {
                    id: added.id.clone(),
                    hash,
                    status: "downloaded".into(),
                    ..Default::default()
                });
            }
            Ok(added)
        }
        async fn select_files(&self, id: &str, file_ids: &str) -> Result<(), reqwest::Error> {
            self.inner.select_files(id, file_ids).await
        }
        async fn delete_torrent(&self, id: &str) -> Result<(), reqwest::Error> {
            self.inner.delete_torrent(id).await?;
            self.listing.lock().unwrap().retain(|t| t.id != id);
            Ok(())
        }
        async fn resolve_url(
            &self,
            loc: &crate::provider::FileLocator,
        ) -> Result<String, crate::error::AppError> {
            self.inner.resolve_url(loc).await
        }
        async fn invalidate(&self, loc: &crate::provider::FileLocator) {
            self.inner.invalidate(loc).await
        }
    }

    /// Seed a show group: singletons `s{e}` (Verified, quality per entry) for each of `singles`,
    /// each serving its own slot unless the pack serves it, plus the multi-episode pack [`PACK`]
    /// (`pack_q`) providing `pack_provides` and serving the slots in `pack_serves`. The scraper
    /// offers each `(episode, candidate)` in `offers` for that episode only; the provider lists every
    /// owned torrent (`t<hash>`, so a prune can delete it) and stages each offered candidate under its
    /// own infohash as a cached single-file torrent. `fail_listing` makes the post-stage prune
    /// listing fail (forcing `apply_upgrade` to defer).
    async fn pack_fixture(
        singles: &[(u32, QualitySummary)],
        pack_provides: &[u32],
        pack_serves: &[u32],
        pack_q: QualitySummary,
        offers: Vec<(u32, RawCandidate)>,
        fail_listing: bool,
    ) -> PackFixture {
        pack_fixture_with(
            singles,
            pack_provides,
            pack_serves,
            pack_q,
            offers,
            PackOpts {
                fail_listing,
                ..Default::default()
            },
        )
        .await
    }

    /// [`pack_fixture`] with the extra [`PackOpts`] knobs.
    async fn pack_fixture_with(
        singles: &[(u32, QualitySummary)],
        pack_provides: &[u32],
        pack_serves: &[u32],
        pack_q: QualitySummary,
        offers: Vec<(u32, RawCandidate)>,
        opts: PackOpts,
    ) -> PackFixture {
        let store = mem_store();
        let mut torrents = Vec::new();
        let mut group_hashes = Vec::new();
        for (e, q) in singles {
            let h = format!("s{e}");
            store
                .put_owned(h.clone(), ep_record(1, *e, q.clone()))
                .await
                .unwrap();
            if !pack_serves.contains(e) {
                store
                    .put_selection(
                        crate::store::episode_slot(SHOW_TMDB, 1, *e),
                        SelectionEntry {
                            hash: h.clone(),
                            file_path: format!("{h}.mkv"),
                        },
                    )
                    .await
                    .unwrap();
            }
            if !opts.absent.contains(&h) {
                torrents.push(Torrent {
                    id: format!("t{h}"),
                    hash: h.clone(),
                    status: "downloaded".into(),
                    ..Default::default()
                });
            }
            group_hashes.push(h);
        }
        let pack_rec = OwnedRecord {
            provides: pack_provides.iter().map(|e| (1, *e)).collect(),
            ..ep_record(1, pack_provides[0], pack_q)
        };
        store.put_owned(PACK.into(), pack_rec).await.unwrap();
        for e in pack_serves {
            store
                .put_selection(
                    crate::store::episode_slot(SHOW_TMDB, 1, *e),
                    SelectionEntry {
                        hash: PACK.into(),
                        file_path: format!("Yaga.S01E01-E04/Yaga.s01e{e:02}.avi"),
                    },
                )
                .await
                .unwrap();
        }
        torrents.push(Torrent {
            id: format!("t{PACK}"),
            hash: PACK.into(),
            status: "downloaded".into(),
            ..Default::default()
        });
        group_hashes.push(PACK.into());

        let mut by_episode: std::collections::HashMap<u32, Vec<RawCandidate>> =
            std::collections::HashMap::new();
        let mut by_id = std::collections::HashMap::new();
        for (e, cand) in offers {
            by_id.insert(cand.info_hash.clone(), single_file_info(&cand));
            by_episode.entry(e).or_default().push(cand);
        }
        for info in opts.infos {
            by_id.insert(info.id.clone(), info);
        }
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let scraper = Arc::new(EpisodeScraper {
            by_episode,
            calls: calls.clone(),
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = MockProvider {
            torrents,
            torrent_info_by_id: by_id,
            resolved_url: Some("https://cdn/staged".into()),
            deleted: deleted.clone(),
            added: added.clone(),
            // Call 1 = the staged candidate's pre-add presence check; later = the prune listing.
            fail_get_torrents_after: opts.fail_listing.then_some(1),
            ..Default::default()
        };
        let provider: Arc<dyn DebridProvider> = if opts.live_listing {
            Arc::new(LiveListingProvider {
                listing: std::sync::Mutex::new(mock.torrents.clone()),
                inner: mock,
            })
        } else {
            Arc::new(mock)
        };
        let app = app_with(scraper, provider, store.clone());
        PackFixture {
            app,
            store,
            deleted,
            added,
            calls,
            group_hashes,
        }
    }

    async fn slot_hash(store: &Store, episode: u32) -> String {
        store
            .get_selection(crate::store::episode_slot(SHOW_TMDB, 1, episode))
            .await
            .unwrap()
            .hash
    }

    #[tokio::test]
    async fn pack_served_episode_is_upgraded_and_superseded_pack_pruned() {
        // The YAGA shape: E1–E3 are 1080p WEB singletons serving their own slots; E4 is served by
        // an SD `.avi` pack (unverified) that also provides E1–E4. A cached 1080p single for E4 is
        // scrape-available → E4 is repointed to it and, since the pack then serves nothing and its
        // other episodes are all held by the singletons, the pack is pruned.
        let f = pack_fixture(
            &[(1, q1080_web()), (2, q1080_web()), (3, q1080_web())],
            &[1, 2, 3, 4],
            &[4],
            q_sd_web_unverified(),
            vec![(4, single_candidate(4, "hy4", "1080p", "1080p.WEB-DL"))],
            false,
        )
        .await;
        let deferred = upgrade_show_episodes_and_packs(
            &f.app,
            SHOW_TMDB,
            &f.group_hashes,
            Duration::from_secs(0),
        )
        .await;
        assert!(!deferred, "a completed swap is not a deferral");
        assert_eq!(slot_hash(&f.store, 4).await, "hy4", "E4 repointed");
        let new_rec = f
            .store
            .get_owned("hy4".into())
            .await
            .expect("staged single recorded owned");
        assert_eq!(new_rec.provides, vec![(1, 4)]);
        assert!(
            f.store.get_owned(PACK.into()).await.is_none(),
            "fully-superseded pack's owned record removed"
        );
        let d = f.deleted.lock().unwrap().clone();
        assert!(d.contains(&format!("t{PACK}")), "pack deleted: {d:?}");
        for e in 1..=3u32 {
            let h = format!("s{e}");
            assert_eq!(slot_hash(&f.store, e).await, h, "E{e} slot untouched");
            assert!(f.store.get_owned(h.clone()).await.is_some(), "{h} kept");
            assert!(!d.contains(&format!("t{h}")), "{h} not deleted");
        }
    }

    #[tokio::test]
    async fn pack_served_episode_upgrade_keeps_pack_serving_other_episodes() {
        // The pack serves E3 AND E4; only E3 (the first served episode, so the sample) has a better
        // single. E3 is repointed, E4 is still evaluated (a later episode with nothing better is
        // skipped, not a reason to stop), and the pack is KEPT because it still serves E4.
        let f = pack_fixture(
            &[(1, q1080_web()), (2, q1080_web())],
            &[1, 2, 3, 4],
            &[3, 4],
            q_sd_web_unverified(),
            vec![(3, single_candidate(3, "hy3", "1080p", "1080p.WEB-DL"))],
            false,
        )
        .await;
        let deferred = upgrade_show_episodes_and_packs(
            &f.app,
            SHOW_TMDB,
            &f.group_hashes,
            Duration::from_secs(0),
        )
        .await;
        assert!(!deferred);
        assert_eq!(slot_hash(&f.store, 3).await, "hy3", "E3 repointed");
        assert_eq!(slot_hash(&f.store, 4).await, PACK, "E4 still on the pack");
        assert!(
            f.store.get_owned(PACK.into()).await.is_some(),
            "pack still serving E4 must be kept"
        );
        assert!(
            !f.deleted.lock().unwrap().contains(&format!("t{PACK}")),
            "pack torrent not deleted"
        );
        assert!(
            f.calls.lock().unwrap().contains(&(Some(1), Some(4))),
            "E4 still scraped after the sample found an upgrade"
        );
    }

    #[tokio::test]
    async fn pack_sample_first_skips_remaining_episodes_when_nothing_better() {
        // A 1080p WEB pack serving E1–E3. The sample (E1) has only a non-improving 720p release, so
        // the rest of the pack is skipped this visit — even though E2 has a better release, it is
        // never scraped: exactly one scrape for the pack.
        let f = pack_fixture(
            &[],
            &[1, 2, 3],
            &[1, 2, 3],
            q1080_web(),
            vec![
                (1, single_candidate(1, "hlow1", "720p", "720p.WEB-DL")),
                (
                    2,
                    single_candidate(2, "hbig2", "1080p", "1080p.BluRay.REMUX"),
                ),
            ],
            false,
        )
        .await;
        let deferred = upgrade_show_episodes_and_packs(
            &f.app,
            SHOW_TMDB,
            &f.group_hashes,
            Duration::from_secs(0),
        )
        .await;
        assert!(!deferred);
        assert_eq!(
            f.calls.lock().unwrap().clone(),
            vec![(Some(1), Some(1))],
            "only the sample episode is scraped"
        );
        for e in 1..=3u32 {
            assert_eq!(slot_hash(&f.store, e).await, PACK);
        }
        assert!(
            f.deleted.lock().unwrap().is_empty(),
            "nothing staged/deleted"
        );
    }

    #[tokio::test]
    async fn over_ceiling_pack_is_left_to_pack_correction() {
        // A 2160p pack (above the default 1080 ceiling) serving E2 and E3: phase (a2) must not touch
        // it — the only scrape is pack correction's own season scrape (episode 1), never a
        // per-episode scrape for a served episode.
        let f = pack_fixture(
            &[],
            &[1, 2, 3],
            &[2, 3],
            q2160_remux(),
            vec![(2, single_candidate(2, "hy2", "1080p", "1080p.WEB-DL"))],
            false,
        )
        .await;
        upgrade_show_episodes_and_packs(&f.app, SHOW_TMDB, &f.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            f.calls.lock().unwrap().clone(),
            vec![(Some(1), Some(1))],
            "only pack correction's season scrape is issued"
        );
        assert_eq!(slot_hash(&f.store, 2).await, PACK);
        assert!(f.store.get_owned(PACK.into()).await.is_some());
    }

    #[tokio::test]
    async fn pack_served_episode_swap_defer_keeps_pack() {
        // The YAGA shape, but the post-stage prune listing fails: apply_upgrade rolls the staged
        // single back and defers BEFORE any prune — the pack (and its E4 slot) must be intact and
        // the deferral propagated (cursor left unstamped).
        let f = pack_fixture(
            &[(1, q1080_web()), (2, q1080_web()), (3, q1080_web())],
            &[1, 2, 3, 4],
            &[4],
            q_sd_web_unverified(),
            vec![(4, single_candidate(4, "hy4", "1080p", "1080p.WEB-DL"))],
            true,
        )
        .await;
        let deferred = upgrade_show_episodes_and_packs(
            &f.app,
            SHOW_TMDB,
            &f.group_hashes,
            Duration::from_secs(0),
        )
        .await;
        assert!(deferred, "a post-stage defer must propagate");
        assert_eq!(slot_hash(&f.store, 4).await, PACK, "E4 still on the pack");
        assert!(f.store.get_owned(PACK.into()).await.is_some(), "pack kept");
        assert!(
            f.store.get_owned("hy4".into()).await.is_none(),
            "staged single rolled back"
        );
        let d = f.deleted.lock().unwrap().clone();
        assert!(d.contains(&"hy4".to_string()), "staged torrent dropped");
        assert!(!d.contains(&format!("t{PACK}")), "pack not deleted");
    }

    #[tokio::test]
    async fn pack_kept_when_an_episode_is_held_only_by_a_pending_copy() {
        // The YAGA shape, but E3's only other copy is still `Pending` (it may yet be reaped as
        // dead). E4 is upgraded, but the pack — the only settled holder of E3 — must be kept.
        let f = pack_fixture(
            &[(1, q1080_web()), (2, q1080_web()), (3, q1080_web())],
            &[1, 2, 3, 4],
            &[4],
            q_sd_web_unverified(),
            vec![(4, single_candidate(4, "hy4", "1080p", "1080p.WEB-DL"))],
            false,
        )
        .await;
        let mut s3 = f.store.get_owned("s3".into()).await.unwrap();
        s3.status = OwnedStatus::Pending;
        f.store.put_owned("s3".into(), s3).await.unwrap();
        upgrade_show_episodes_and_packs(&f.app, SHOW_TMDB, &f.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(slot_hash(&f.store, 4).await, "hy4", "E4 repointed");
        assert!(
            f.store.get_owned(PACK.into()).await.is_some(),
            "pack kept: E3 is otherwise held only by a Pending copy"
        );
        assert!(!f.deleted.lock().unwrap().contains(&format!("t{PACK}")));
    }

    #[tokio::test]
    async fn pack_prune_counts_singles_upgraded_earlier_in_the_tick() {
        // E1's 720p singleton is upgraded (and pruned) by phase (a) this same tick, so E1 is now held
        // only by the NEW single — which the provider listing fetched by the E4 swap doesn't show
        // (the mock listing is static, like TorBox's lagging one right after an add). A single staged
        // earlier this tick still counts as a present holder, so the pack serving E4 is recognised as
        // fully covered once E4 moves, and pruned. (The store RE-READ itself is pinned by
        // `pack_prune_rereads_coverage_after_an_overlapping_pack_is_pruned`.)
        let f = pack_fixture(
            &[(1, q720_web()), (2, q1080_web()), (3, q1080_web())],
            &[1, 2, 3, 4],
            &[4],
            q_sd_web_unverified(),
            vec![
                (1, single_candidate(1, "hy1", "1080p", "1080p.WEB-DL")),
                (4, single_candidate(4, "hy4", "1080p", "1080p.WEB-DL")),
            ],
            false,
        )
        .await;
        upgrade_show_episodes_and_packs(&f.app, SHOW_TMDB, &f.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            slot_hash(&f.store, 1).await,
            "hy1",
            "E1 upgraded by phase (a)"
        );
        assert!(
            f.store.get_owned("s1".into()).await.is_none(),
            "old E1 pruned"
        );
        assert_eq!(slot_hash(&f.store, 4).await, "hy4", "E4 repointed");
        assert!(
            f.store.get_owned(PACK.into()).await.is_none(),
            "pack pruned: every episode it provides is held elsewhere"
        );
    }

    #[tokio::test]
    async fn pack_kept_when_a_covering_copy_has_lapsed_off_the_provider() {
        // The YAGA shape, but E2's singleton has lapsed off the provider: its owned record is still
        // `Verified` and E2's slot still names it, yet its torrent is gone from the listing — so the
        // VFS falls back to the pack for E2. E4 is still upgraded, but the pack (the only PRESENT
        // holder of E2) must be kept, not pruned on the strength of an absent record.
        let f = pack_fixture_with(
            &[(1, q1080_web()), (2, q1080_web()), (3, q1080_web())],
            &[1, 2, 3, 4],
            &[4],
            q_sd_web_unverified(),
            vec![(4, single_candidate(4, "hy4", "1080p", "1080p.WEB-DL"))],
            PackOpts {
                absent: vec!["s2".into()],
                ..Default::default()
            },
        )
        .await;
        let deferred = upgrade_show_episodes_and_packs(
            &f.app,
            SHOW_TMDB,
            &f.group_hashes,
            Duration::from_secs(0),
        )
        .await;
        assert!(!deferred);
        assert_eq!(slot_hash(&f.store, 4).await, "hy4", "E4 repointed");
        assert!(
            f.store.get_owned(PACK.into()).await.is_some(),
            "pack kept: it is the only present holder of E2"
        );
        let d = f.deleted.lock().unwrap().clone();
        assert!(!d.contains(&format!("t{PACK}")), "pack not deleted: {d:?}");
    }

    #[tokio::test]
    async fn pack_pruned_once_its_last_two_served_episodes_move_in_one_tick() {
        // The pack serves E3 AND E4 and both have a better single. E3 moves first (the pack still
        // serves E4 → kept); then E4 moves and the pack is pruned — E3 is now held by the single
        // staged moments earlier this tick, which counts as present even though the listing fetched
        // by the E4 swap doesn't show that just-added torrent (static mock listing).
        let f = pack_fixture(
            &[(1, q1080_web()), (2, q1080_web())],
            &[1, 2, 3, 4],
            &[3, 4],
            q_sd_web_unverified(),
            vec![
                (3, single_candidate(3, "hy3", "1080p", "1080p.WEB-DL")),
                (4, single_candidate(4, "hy4", "1080p", "1080p.WEB-DL")),
            ],
            false,
        )
        .await;
        upgrade_show_episodes_and_packs(&f.app, SHOW_TMDB, &f.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(slot_hash(&f.store, 3).await, "hy3", "E3 repointed");
        assert_eq!(slot_hash(&f.store, 4).await, "hy4", "E4 repointed");
        assert!(
            f.store.get_owned(PACK.into()).await.is_none(),
            "pack pruned once it serves nothing and every episode is held elsewhere"
        );
        assert!(f.deleted.lock().unwrap().contains(&format!("t{PACK}")));
    }

    #[tokio::test]
    async fn pack_prune_rereads_coverage_after_an_overlapping_pack_is_pruned() {
        // Two overlapping SD packs both provide E1–E4: P1 serves E1, P2 serves E2 (E3/E4 have no
        // managed selection). Better singles exist for E1 and E2. P1 goes first: once E1 moves, P1
        // serves nothing and P2 still holds E2–E4 → P1 is pruned. Then E2 moves off P2, and P2 must
        // be KEPT — it is now the only holder of E3/E4. A tick-start snapshot would still count the
        // already-pruned P1 as coverage and delete P2 too, losing E3/E4.
        let store = mem_store();
        for (h, e) in [("p1", 1u32), ("p2", 2)] {
            let rec = OwnedRecord {
                provides: PACK_E1_E4.to_vec(),
                ..ep_record(1, 1, q_sd_web_unverified())
            };
            store.put_owned(h.into(), rec).await.unwrap();
            store
                .put_selection(
                    crate::store::episode_slot(SHOW_TMDB, 1, e),
                    SelectionEntry {
                        hash: h.into(),
                        file_path: format!("{h}/S.s01e{e:02}.avi"),
                    },
                )
                .await
                .unwrap();
        }
        let h1 = single_candidate(1, "h1", "1080p", "1080p.WEB-DL");
        let h2 = single_candidate(2, "h2", "1080p", "1080p.WEB-DL");
        let by_id: std::collections::HashMap<String, TorrentInfo> = [&h1, &h2]
            .iter()
            .map(|c| (c.info_hash.clone(), single_file_info(c)))
            .collect();
        let scraper = Arc::new(EpisodeScraper {
            by_episode: [(1, vec![h1]), (2, vec![h2])].into_iter().collect(),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: ["p1", "p2"]
                .iter()
                .map(|h| Torrent {
                    id: format!("t{h}"),
                    hash: h.to_string(),
                    status: "downloaded".into(),
                    ..Default::default()
                })
                .collect(),
            torrent_info_by_id: by_id,
            resolved_url: Some("https://cdn/staged".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        let group = vec!["p1".to_string(), "p2".to_string()];
        upgrade_show_episodes_and_packs(&app, SHOW_TMDB, &group, Duration::from_secs(0)).await;
        assert_eq!(slot_hash(&store, 1).await, "h1", "E1 repointed");
        assert_eq!(slot_hash(&store, 2).await, "h2", "E2 repointed");
        let d = deleted.lock().unwrap().clone();
        assert!(store.get_owned("p1".into()).await.is_none(), "P1 pruned");
        assert!(d.contains(&"tp1".to_string()), "P1 deleted: {d:?}");
        assert!(
            store.get_owned("p2".into()).await.is_some(),
            "P2 kept: the only remaining holder of E3/E4"
        );
        assert!(!d.contains(&"tp2".to_string()), "P2 not deleted: {d:?}");
    }

    #[tokio::test]
    async fn multi_episode_candidate_is_staged_at_most_once_per_tick() {
        // An SD pack serves E1–E3; Torrentio lists the same BluRay season pack under every episode
        // query (an improving candidate each time). Staging it for E1 resolves to a multi-episode
        // pack, which the per-episode path can never adopt — so it must not be re-staged (add +
        // cached poll + delete) for E2 and E3 this tick.
        let season_pack = |e: u32| RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "S.S01.1080p.BluRay.x265\nRD+".into(),
            info_hash: "hseason".into(),
            file_idx: Some(e as usize - 1),
            file_name: Some(format!("S.S01E{e:02}.1080p.BluRay.x265.mkv")),
        };
        let pack_info = TorrentInfo {
            id: "hseason".into(),
            hash: "hseason".into(),
            status: "downloaded".into(),
            files: (1..=3u32)
                .map(|e| TorrentFile {
                    id: e - 1,
                    path: format!("S.S01.1080p.BluRay/S.S01E{e:02}.1080p.BluRay.x265.mkv"),
                    bytes: 2_000_000_000,
                    selected: 1,
                })
                .collect(),
            links: (1..=3u32).map(|e| format!("https://cdn/s{e}")).collect(),
            ..Default::default()
        };
        let f = pack_fixture_with(
            &[],
            &[1, 2, 3],
            &[1, 2, 3],
            q_sd_web_unverified(),
            (1..=3u32).map(|e| (e, season_pack(e))).collect(),
            PackOpts {
                infos: vec![pack_info],
                ..Default::default()
            },
        )
        .await;
        upgrade_show_episodes_and_packs(&f.app, SHOW_TMDB, &f.group_hashes, Duration::from_secs(0))
            .await;
        let d = f.deleted.lock().unwrap().clone();
        assert_eq!(
            d.iter().filter(|h| *h == "hseason").count(),
            1,
            "the season pack is staged (and dropped) once, not once per served episode: {d:?}"
        );
        for e in 1..=3u32 {
            assert_eq!(slot_hash(&f.store, e).await, PACK, "E{e} still on the pack");
        }
        assert!(f.store.get_owned(PACK.into()).await.is_some(), "pack kept");
    }

    #[tokio::test]
    async fn pack_correction_never_stages_a_single_adopted_earlier_in_the_tick() {
        // E1 is a 720p singleton with a better 1080p single available; E2–E3 come from a 2160p pack
        // above the ceiling (corrective downgrade ON). Phase (a) adopts the E1 single. Pack
        // correction then scrapes episode 1 of the season, where that same single is listed: it must
        // be recognised as already owned and never re-staged — on TorBox a re-add of an owned
        // infohash returns the SAME torrent id (as the mock does), so the coverage-miss cleanup would
        // delete the freshly-adopted single out from under its record and selection.
        let mut f = pack_fixture(
            &[(1, q720_web())],
            &[2, 3],
            &[2, 3],
            q2160_remux(),
            vec![(1, single_candidate(1, "hy1", "1080p", "1080p.WEB-DL"))],
            false,
        )
        .await;
        {
            let mut cfg = (*f.app.config).clone();
            cfg.allow_resolution_downgrade = true;
            f.app.config = Arc::new(cfg);
        }
        upgrade_show_episodes_and_packs(&f.app, SHOW_TMDB, &f.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            slot_hash(&f.store, 1).await,
            "hy1",
            "E1 upgraded by phase (a)"
        );
        assert!(f.store.get_owned("hy1".into()).await.is_some());
        let d = f.deleted.lock().unwrap().clone();
        assert!(
            !d.contains(&"hy1".to_string()),
            "the adopted single is never deleted: {d:?}"
        );
        assert_eq!(slot_hash(&f.store, 2).await, PACK);
        assert!(f.store.get_owned(PACK.into()).await.is_some());
    }

    /// A cached S01 BluRay x264 1080p season pack (hash [`PACK`]) as Torrentio lists it under episode
    /// `e`'s query (the stream points at that episode's file in the pack).
    fn bluray_x264_pack_raw(e: u32) -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "Show.S01.1080p.BluRay.x264-GRP\n👤 20 💾 4.4 GB\nRD+".into(),
            info_hash: PACK.into(),
            file_idx: Some(e as usize - 1),
            file_name: Some(format!("Show.S01E{e:02}.1080p.BluRay.x264-GRP.mkv")),
        }
    }

    /// A cached WEB-DL x265 1080p single for S01E`e` — it OUT-SCORES the BluRay x264 pack (the +5k
    /// HEVC bonus outweighs the 3k BluRay-over-WEB tier gap) at a LOWER source tier.
    fn web_x265_single_raw(e: u32) -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: format!("Show.S01E{e:02}.1080p.WEB-DL.x265-GRP\n👤 20 💾 1.1 GB\nRD+"),
            info_hash: format!("hweb{e}"),
            file_idx: Some(0),
            file_name: Some(format!("Show.S01E{e:02}.1080p.WEB-DL.x265-GRP.mkv")),
        }
    }

    /// One show visit offline: phases (a)/(a2)/(b) on the CURRENT group, then consolidation on the
    /// re-read group — exactly `try_upgrade_show`'s order, minus its live TMDB aired lookup.
    async fn show_upgrade_visit(app: &AppState, aired: &[(u32, u32)]) {
        let hashes = current_show_hashes(app, SHOW_TMDB).await;
        upgrade_show_episodes_and_packs(app, SHOW_TMDB, &hashes, Duration::from_secs(0)).await;
        let fresh = current_show_hashes(app, SHOW_TMDB).await;
        let mut owned = Vec::new();
        for h in &fresh {
            if let Some(r) = app.store.get_owned(h.clone()).await {
                owned.push((h.clone(), r));
            }
        }
        let _ = consolidate_show_seasons(
            app,
            SHOW_TMDB,
            &fresh,
            &owned,
            aired,
            Duration::from_secs(0),
        )
        .await;
    }

    /// The observable show state: owned hashes (sorted) and each episode slot's hash.
    async fn show_state(store: &Store, episodes: u32) -> (Vec<String>, Vec<String>) {
        let mut owned: Vec<String> = store
            .all_owned()
            .await
            .into_iter()
            .filter(|(_, r)| r.request.tmdb_id == SHOW_TMDB)
            .map(|(h, _)| h)
            .collect();
        owned.sort();
        let mut slots = Vec::new();
        for e in 1..=episodes {
            slots.push(slot_hash(store, e).await);
        }
        (owned, slots)
    }

    #[tokio::test]
    async fn pack_served_split_and_consolidation_converge() {
        // A season held as a cached BluRay x264 1080p pack serving every slot, while cached WEB-DL
        // x265 1080p singles exist for every episode. The singles score HIGHER (HEVC) but sit at a
        // LOWER source tier, so consolidation — which judges "no regression" on tier + resolution —
        // would re-adopt the pack over them. Phase (a2) must therefore not split the pack into them:
        // otherwise every visit splits the season (N adds, pack pruned) and consolidation folds it
        // back (pack re-added, N singles pruned), forever. The listing reflects adds and deletes, so
        // a pruned pack really could be re-staged.
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let pack_q = QualitySummary::of(&release::parse(&bluray_x264_pack_raw(1)), &prefs);
        let single_q = QualitySummary::of(&release::parse(&web_x265_single_raw(1)), &prefs);
        assert!(
            single_q.score > pack_q.score && single_q.source_tier < pack_q.source_tier,
            "fixture: the single out-scores the pack at a lower tier ({single_q:?} vs {pack_q:?})"
        );
        assert_eq!(
            release::is_target_improvement(&pack_q, &single_q, &prefs),
            release::Improvement::None,
            "a same-resolution tier drop is not an upgrade, whatever the score"
        );
        let pack_info = TorrentInfo {
            id: PACK.into(),
            hash: PACK.into(),
            status: "downloaded".into(),
            files: (1..=3u32)
                .map(|e| TorrentFile {
                    id: e - 1,
                    path: format!(
                        "Show.S01.1080p.BluRay.x264-GRP/Show.S01E{e:02}.1080p.BluRay.x264-GRP.mkv"
                    ),
                    bytes: 1_500_000_000,
                    selected: 1,
                })
                .collect(),
            links: (1..=3u32).map(|e| format!("https://cdn/p{e}")).collect(),
            ..Default::default()
        };
        let f = pack_fixture_with(
            &[],
            &[1, 2, 3],
            &[1, 2, 3],
            pack_q,
            (1..=3u32)
                .flat_map(|e| [(e, web_x265_single_raw(e)), (e, bluray_x264_pack_raw(e))])
                .collect(),
            PackOpts {
                infos: vec![pack_info],
                live_listing: true,
                ..Default::default()
            },
        )
        .await;
        let aired = [(1, 1), (1, 2), (1, 3)];

        show_upgrade_visit(&f.app, &aired).await;
        let after_first = show_state(&f.store, 3).await;
        let (adds, deletes) = (
            f.added.lock().unwrap().len(),
            f.deleted.lock().unwrap().len(),
        );
        show_upgrade_visit(&f.app, &aired).await;
        assert_eq!(
            show_state(&f.store, 3).await,
            after_first,
            "a second visit changes no owned record or slot"
        );
        assert_eq!(
            f.added.lock().unwrap()[adds..].to_vec(),
            Vec::<String>::new(),
            "a second visit stages nothing"
        );
        assert_eq!(
            f.deleted.lock().unwrap()[deletes..].to_vec(),
            Vec::<String>::new(),
            "a second visit deletes nothing"
        );
        assert_eq!(
            after_first,
            (vec![PACK.to_string()], vec![PACK.to_string(); 3]),
            "the pack is never split into lower-tier singles"
        );
        assert_eq!(
            f.added.lock().unwrap().clone(),
            Vec::<String>::new(),
            "no lower-tier single is ever staged for a pack-served slot"
        );
    }

    #[tokio::test]
    async fn unverified_pack_split_into_equal_singles_is_refolded_at_most_once() {
        // An UNVERIFIED MKV WEB-DL x265 1080p pack (its probed file's tracks couldn't be read) serving
        // every slot, with equal-tier/resolution, slightly higher-scoring singles available. Task 4's
        // unverified rule lets phase (a2) split it into the (checked) singles, and consolidation —
        // whose no-regression gate the equal pack passes — folds it back in the same visit (the
        // listing reflects adds and deletes, so the pruned pack can be re-staged). But consolidation
        // adopts only a probe `Pass` and records the pack VERIFIED, so (a2) can't split it again: the
        // later visits change nothing (no split/re-fold cycle).
        let pack_raw = |e: u32| RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "Show.S01.1080p.WEB-DL.x265-GRP\n👤 20 💾 4.4 GB\nRD+".into(),
            info_hash: PACK.into(),
            file_idx: Some(e as usize - 1),
            file_name: Some(format!("Show.S01E{e:02}.1080p.WEB-DL.x265-GRP.mkv")),
        };
        let single_raw = |e: u32| RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: format!("Show.S01E{e:02}.1080p.WEB-DL.x265-OTHER\n👤 400 💾 1.1 GB\nRD+"),
            info_hash: format!("hweb{e}"),
            file_idx: Some(0),
            file_name: Some(format!("Show.S01E{e:02}.1080p.WEB-DL.x265-OTHER.mkv")),
        };
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let pack_q = QualitySummary {
            unverified: true,
            ..QualitySummary::of(&release::parse(&pack_raw(1)), &prefs)
        };
        let single_q = QualitySummary::of(&release::parse(&single_raw(1)), &prefs);
        assert!(
            single_q.score >= pack_q.score
                && single_q.source_tier == pack_q.source_tier
                && single_q.resolution == pack_q.resolution,
            "fixture: equal tier/res, single scores >= pack ({single_q:?} vs {pack_q:?})"
        );
        let pack_info = TorrentInfo {
            id: PACK.into(),
            hash: PACK.into(),
            status: "downloaded".into(),
            files: (1..=3u32)
                .map(|e| TorrentFile {
                    id: e - 1,
                    path: format!("Show.S01/Show.S01E{e:02}.1080p.WEB-DL.x265-GRP.mkv"),
                    bytes: 1_500_000_000,
                    selected: 1,
                })
                .collect(),
            links: (1..=3u32).map(|e| format!("https://cdn/p{e}")).collect(),
            ..Default::default()
        };
        let f = pack_fixture_with(
            &[],
            &[1, 2, 3],
            &[1, 2, 3],
            pack_q,
            (1..=3u32)
                .flat_map(|e| [(e, single_raw(e)), (e, pack_raw(e))])
                .collect(),
            PackOpts {
                infos: vec![pack_info],
                live_listing: true,
                ..Default::default()
            },
        )
        .await;
        let aired = [(1, 1), (1, 2), (1, 3)];

        show_upgrade_visit(&f.app, &aired).await;
        let after_first = show_state(&f.store, 3).await;
        let singles: Vec<String> = (1..=3u32).map(|e| format!("hweb{e}")).collect();
        // Visit 1 split the pack (staging each single, pruning the pack) and then re-folded it
        // (re-staging the pack, pruning the singles).
        let mut expected_adds = singles.clone();
        expected_adds.push(PACK.to_string());
        assert_eq!(
            f.added.lock().unwrap().clone(),
            expected_adds,
            "visit 1 stages the singles, then re-stages the pack"
        );
        let mut expected_deletes = vec![format!("t{PACK}")];
        expected_deletes.extend(singles.iter().cloned());
        expected_deletes.sort();
        let mut deleted = f.deleted.lock().unwrap().clone();
        deleted.sort();
        assert_eq!(
            deleted, expected_deletes,
            "visit 1 prunes the pack, then the singles it was folded back over"
        );
        assert_eq!(
            after_first,
            (vec![PACK.to_string()], vec![PACK.to_string(); 3]),
            "the pack serves the season again after visit 1"
        );
        let (adds, deletes) = (
            f.added.lock().unwrap().len(),
            f.deleted.lock().unwrap().len(),
        );
        for visit in 2..=3 {
            show_upgrade_visit(&f.app, &aired).await;
            assert_eq!(
                show_state(&f.store, 3).await,
                after_first,
                "visit {visit} changes no owned record or slot"
            );
        }
        assert_eq!(
            f.added.lock().unwrap()[adds..].to_vec(),
            Vec::<String>::new(),
            "later visits stage nothing"
        );
        assert_eq!(
            f.deleted.lock().unwrap()[deletes..].to_vec(),
            Vec::<String>::new(),
            "later visits delete nothing"
        );
        // The re-folded pack is now a CHECKED copy, so the unverified rule can't split it again.
        let q = f
            .store
            .get_owned(PACK.into())
            .await
            .unwrap()
            .quality
            .unwrap();
        assert!(!q.unverified, "the re-folded pack is recorded verified");
    }

    /// A cached S01 WEB-DL x264 1080p season pack (hash [`PACK`]) as Torrentio lists it under episode
    /// `e`'s query — the same source tier as [`web_x265_single_raw`], below
    /// [`bluray_x264_single_raw`].
    fn web_x264_pack_raw(e: u32) -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "Show.S01.1080p.WEB-DL.x264-GRP\n👤 20 💾 12 GB\nRD+".into(),
            info_hash: PACK.into(),
            file_idx: Some(e as usize - 1),
            file_name: Some(format!("Show.S01E{e:02}.1080p.WEB-DL.x264-GRP.mkv")),
        }
    }

    /// A cached BluRay x264 1080p single for S01E`e`.
    fn bluray_x264_single_raw(e: u32) -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: format!("Show.S01E{e:02}.1080p.BluRay.x264-BD\n👤 20 💾 2 GB\nRD+"),
            info_hash: format!("hbd{e}"),
            file_idx: Some(0),
            file_name: Some(format!("Show.S01E{e:02}.1080p.BluRay.x264-BD.mkv")),
        }
    }

    #[tokio::test]
    async fn pack_split_singles_upgrade_and_consolidation_converge() {
        // All three show passes must agree on "better", or one undoes another. A season served by a
        // WEB-DL x264 1080p pack; per episode, cached BluRay x264 1080p singles (a higher tier, so
        // (a2) splits the pack into them) and WEB-DL x265 1080p singles (they out-score the BluRay
        // singles on HEVC's bonus, at a LOWER tier). Were phase (a) to swap the BluRay singles for
        // the WEB x265 ones, consolidation (tier + resolution only) would fold those back into the
        // pruned WEB pack, which (a2) splits again on the next visit: a two-visit cycle of ~N+2
        // adds, deletes and probes per visit. The listing reflects adds and deletes, so the pruned
        // pack really can be re-staged.
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let q = |c: &RawCandidate| QualitySummary::of(&release::parse(c), &prefs);
        let (pack_q, bd_q, hv_q) = (
            q(&web_x264_pack_raw(1)),
            q(&bluray_x264_single_raw(1)),
            q(&web_x265_single_raw(1)),
        );
        assert_eq!(
            release::is_target_improvement(&pack_q, &bd_q, &prefs),
            release::Improvement::Upgrade,
            "fixture: the BluRay single is an upgrade over the WEB pack"
        );
        assert!(
            hv_q.score > bd_q.score && hv_q.source_tier < bd_q.source_tier,
            "fixture: the WEB x265 single out-scores the BluRay one at a lower tier"
        );
        let pack_info = TorrentInfo {
            id: PACK.into(),
            hash: PACK.into(),
            status: "downloaded".into(),
            files: (1..=3u32)
                .map(|e| TorrentFile {
                    id: e - 1,
                    path: format!(
                        "Show.S01.1080p.WEB-DL.x264-GRP/Show.S01E{e:02}.1080p.WEB-DL.x264-GRP.mkv"
                    ),
                    bytes: 4_000_000_000,
                    selected: 1,
                })
                .collect(),
            links: (1..=3u32).map(|e| format!("https://cdn/p{e}")).collect(),
            ..Default::default()
        };
        let f = pack_fixture_with(
            &[],
            &[1, 2, 3],
            &[1, 2, 3],
            pack_q,
            (1..=3u32)
                .flat_map(|e| {
                    [
                        (e, bluray_x264_single_raw(e)),
                        (e, web_x265_single_raw(e)),
                        (e, web_x264_pack_raw(e)),
                    ]
                })
                .collect(),
            PackOpts {
                infos: vec![pack_info],
                live_listing: true,
                ..Default::default()
            },
        )
        .await;
        let aired = [(1, 1), (1, 2), (1, 3)];

        show_upgrade_visit(&f.app, &aired).await;
        let bluray: Vec<String> = (1..=3u32).map(|e| format!("hbd{e}")).collect();
        assert_eq!(
            show_state(&f.store, 3).await,
            (bluray.clone(), bluray.clone()),
            "visit 1 moves every episode to its BluRay single and prunes the WEB pack"
        );
        assert_eq!(
            f.deleted.lock().unwrap().clone(),
            vec![format!("t{PACK}")],
            "visit 1 deletes only the superseded pack"
        );
        let (adds, deletes) = (
            f.added.lock().unwrap().len(),
            f.deleted.lock().unwrap().len(),
        );
        for visit in 2..=4 {
            show_upgrade_visit(&f.app, &aired).await;
            assert_eq!(
                show_state(&f.store, 3).await,
                (bluray.clone(), bluray.clone()),
                "visit {visit} changes no owned record or slot"
            );
        }
        assert_eq!(
            f.added.lock().unwrap()[adds..].to_vec(),
            Vec::<String>::new(),
            "later visits stage nothing (neither the lower-tier singles nor the pruned pack)"
        );
        assert_eq!(
            f.deleted.lock().unwrap()[deletes..].to_vec(),
            Vec::<String>::new(),
            "later visits delete nothing"
        );
    }

    #[tokio::test]
    async fn pack_served_episode_is_not_split_into_a_lower_tier_single_at_higher_resolution() {
        // A 720p BluRay pack serving every slot; 1080p WEB-DL singles are upgrades over it by
        // resolution. Phase (a2) still splits a pack only into singles at least as good on BOTH
        // axes (consolidation's per-axis test): nothing is staged, and the sample-first bound stops
        // the pack after its first served episode.
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let pack_raw = RawCandidate {
            name: "Torrentio\n720p".into(),
            description: "Show.S01.720p.BluRay.x264-GRP\nRD+".into(),
            info_hash: PACK.into(),
            file_idx: Some(0),
            file_name: Some("Show.S01E01.720p.BluRay.x264-GRP.mkv".into()),
        };
        let pack_q = QualitySummary::of(&release::parse(&pack_raw), &prefs);
        let single = |e: u32| single_candidate(e, &format!("hw{e}"), "1080p", "1080p.WEB-DL");
        assert_eq!(
            release::is_target_improvement(
                &pack_q,
                &QualitySummary::of(&release::parse(&single(1)), &prefs),
                &prefs
            ),
            release::Improvement::Upgrade,
            "fixture: the 1080p WEB single is an upgrade over the 720p BluRay pack"
        );
        let f = pack_fixture(
            &[],
            &[1, 2, 3],
            &[1, 2, 3],
            pack_q,
            (1..=3u32).map(|e| (e, single(e))).collect(),
            false,
        )
        .await;
        upgrade_show_episodes_and_packs(&f.app, SHOW_TMDB, &f.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            f.added.lock().unwrap().clone(),
            Vec::<String>::new(),
            "no lower-tier single is staged for a pack-served slot"
        );
        for e in 1..=3u32 {
            assert_eq!(slot_hash(&f.store, e).await, PACK, "E{e} stays on the pack");
        }
        assert_eq!(
            f.calls.lock().unwrap().clone(),
            vec![(Some(1), Some(1))],
            "sample-first: one scrape for the unimprovable pack"
        );
    }

    /// A cached 1080p WEB-DL x265 single for S01E04 tagged Russian by name AND Torrentio flag — it
    /// out-scores [`eng_x264_e4_raw`] on raw prefs (HEVC), where `AUDIO_LANGUAGE=original` has no
    /// per-title language to penalise it with.
    fn rus_x265_e4_raw() -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "Yaga.S01E04.1080p.WEB-DL.x265.Rus\n👤 50 💾 2.1 GB\n🇷🇺\nRD+".into(),
            info_hash: "hrus4".into(),
            file_idx: Some(0),
            file_name: Some("Yaga.S01E04.1080p.WEB-DL.x265.Rus.mkv".into()),
        }
    }
    fn eng_x264_e4_raw() -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "Yaga.S01E04.1080p.WEB-DL.x264.ENG\n👤 5 💾 1.1 GB\n🇬🇧\nRD+".into(),
            info_hash: "heng4".into(),
            file_idx: Some(0),
            file_name: Some("Yaga.S01E04.1080p.WEB-DL.x264.ENG.mkv".into()),
        }
    }

    #[tokio::test]
    async fn pack_served_episode_stages_the_original_language_release_first() {
        // YAGA S01E04: an English-original show whose E4 is served by an SD pack. A Russian-tagged
        // single (name + 🇷🇺) and an English one both improve it; the Russian one scores higher on
        // the raw prefs (HEVC). Candidates are ORDERED with the title's resolved audio (as
        // acquisition ranks), so the English release is staged first — and, passing the probe, is
        // the one adopted; the Russian release is never staged.
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let raw = |c: &RawCandidate| QualitySummary::of(&release::parse(c), &prefs).score;
        assert!(
            raw(&rus_x265_e4_raw()) > raw(&eng_x264_e4_raw()),
            "fixture: the Russian release out-scores the English one on raw prefs"
        );
        let f = pack_fixture(
            &[(1, q1080_web()), (2, q1080_web()), (3, q1080_web())],
            &[1, 2, 3, 4],
            &[4],
            q_sd_web_unverified(),
            vec![(4, rus_x265_e4_raw()), (4, eng_x264_e4_raw())],
            false,
        )
        .await;
        upgrade_show_episodes_and_packs(&f.app, SHOW_TMDB, &f.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            f.added.lock().unwrap().clone(),
            vec!["heng4".to_string()],
            "the English release is staged first (and the Russian one never)"
        );
        assert_eq!(slot_hash(&f.store, 4).await, "heng4", "E4 moved to English");
        let stored = f.store.get_owned("heng4".into()).await.unwrap();
        assert_eq!(
            stored.quality.unwrap().score,
            raw(&eng_x264_e4_raw()),
            "the stored score stays on the raw prefs (consistent with every other stored score)"
        );
    }

    #[tokio::test]
    async fn consolidation_stages_the_original_language_pack_first() {
        // The pack ranker orders by the title's resolved audio too: of two cached BluRay 1080p
        // full-season packs that both pass the no-regression gate, the English one is staged before
        // a Russian-tagged one that scores higher on the raw prefs (HEVC).
        let rus = RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "S.2019.S01.1080p.BluRay.x265.Rus\n🇷🇺\nRD+".into(),
            info_hash: "hrus".into(),
            file_idx: None,
            file_name: None,
        };
        let eng = RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "S.2019.S01.1080p.BluRay.x264.ENG\n🇬🇧\nRD+".into(),
            info_hash: "heng".into(),
            file_idx: None,
            file_name: None,
        };
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let raw = |c: &RawCandidate| QualitySummary::of(&release::parse(c), &prefs).score;
        assert!(
            raw(&rus) > raw(&eng),
            "fixture: raw order puts Russian first"
        );
        let store = mem_store();
        seed_two_scattered_episodes(&store).await;
        let pack = |h: &str| TorrentInfo {
            id: h.into(),
            hash: h.into(),
            ..pack_info()
        };
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: show_listing(),
            torrent_info_by_id: [
                ("hrus".to_string(), pack("hrus")),
                ("heng".into(), pack("heng")),
            ]
            .into_iter()
            .collect(),
            resolved_url: Some("https://cdn/p1".into()),
            added: added.clone(),
            ..Default::default()
        });
        let app = app_with(
            Arc::new(MockScraper {
                candidates: vec![rus, eng],
            }),
            provider,
            store.clone(),
        );
        consolidate_show_seasons(
            &app,
            SHOW_TMDB,
            &group_two(),
            &owned_two(),
            &[(1, 1), (1, 2)],
            Duration::from_secs(300),
        )
        .await
        .unwrap();
        assert_eq!(
            added.lock().unwrap().clone(),
            vec!["heng".to_string()],
            "the English pack is staged first (and adopted)"
        );
        assert!(store.get_owned("heng".into()).await.is_some());
    }

    #[tokio::test]
    async fn movie_upgrade_never_adopts_a_release_acquisition_ranks_below_the_owned_copy() {
        // An English-original movie held as a 1080p WEB-DL tagged ENG. A cached 1080p BluRay tagged
        // Russian (name + 🇷🇺) is a category jump on the RAW prefs (a higher tier, a higher score),
        // but under the title's resolved audio — how acquisition ranks — its wrong-language penalty
        // puts it BELOW the owned copy. Its untagged audio tracks would pass the probe, so it must
        // never be staged. The same release tagged English is adopted (control).
        let prefs = crate::config::AcquisitionConfig::default().prefs;
        let owned_q = QualitySummary::of(
            &release::parse(&RawCandidate {
                name: "Torrentio\n1080p".into(),
                description: "M.2020.1080p.WEB-DL.x264.ENG\n👤 50 💾 4 GB\n🇬🇧\nRD+".into(),
                info_hash: "hold".into(),
                file_idx: Some(0),
                file_name: Some("M.2020.1080p.WEB-DL.x264.ENG.mkv".into()),
            }),
            &prefs,
        );
        let cand = |tag: &str, flag: &str| RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: format!("M.2020.1080p.BluRay.x264.{tag}\n👤 50 💾 8 GB\n{flag}\nRD+"),
            info_hash: "hnew".into(),
            file_idx: Some(0),
            file_name: Some(format!("M.2020.1080p.BluRay.x264.{tag}.mkv")),
        };
        let rus = cand("Rus", "🇷🇺");
        assert_eq!(
            release::is_target_improvement(
                &owned_q,
                &QualitySummary::of(&release::parse(&rus), &prefs),
                &prefs
            ),
            release::Improvement::Upgrade,
            "fixture: a category jump on the raw prefs"
        );
        let resolved = prefs.with_resolved_audio(movie_req().original_language.as_deref(), &[]);
        assert!(
            release::score(&release::parse(&rus), &resolved).unwrap() < owned_q.score,
            "fixture: acquisition ranks the Russian release below the owned copy"
        );

        let (app, store, deleted, added) = movie_swap_fixture_with(
            owned_q.clone(),
            rus,
            "M.2020.1080p.BluRay.x264.Rus.mkv",
            passing_prober(),
            false,
        )
        .await;
        run_upgrade_once(&app).await;
        assert_eq!(
            added.lock().unwrap().clone(),
            Vec::<String>::new(),
            "the wrong-language release is never staged"
        );
        assert!(deleted.lock().unwrap().is_empty(), "nothing pruned");
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold"
        );

        let (app, store, _, _) = movie_swap_fixture_with(
            owned_q,
            cand("ENG", "🇬🇧"),
            "M.2020.1080p.BluRay.x264.ENG.mkv",
            passing_prober(),
            false,
        )
        .await;
        run_upgrade_once(&app).await;
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hnew",
            "control: the English-tagged BluRay is adopted"
        );
    }

    // ── a candidate already in the provider account is never staged ───────────────────────────
    //
    // Every staging site deletes the staged torrent BY THE ID THE ADD RETURNED when it rejects the
    // candidate (here: tracks unverifiable) or rolls back. On TorBox an add of an infohash already in
    // the account returns the EXISTING torrent's id — and the rankers exclude only this title's own
    // owned hashes — so a candidate present outside the group (identified under another title, or an
    // unidentified manual add) would be deleted. Each fixture lists the candidate in the account and
    // has the mock's add return that existing id.

    #[tokio::test]
    async fn movie_upgrade_never_stages_a_candidate_already_in_the_account() {
        let owned = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 10,
            unverified: false,
        };
        let (app, store, deleted, added) = movie_swap_fixture_with(
            owned,
            remux_candidate(),
            "M.2020.1080p.REMUX.mkv",
            unverifiable_prober(),
            true,
        )
        .await;
        run_upgrade_once(&app).await;
        assert_eq!(
            added.lock().unwrap().clone(),
            Vec::<String>::new(),
            "never staged"
        );
        let d = deleted.lock().unwrap().clone();
        assert!(
            !d.contains(&"tnew".to_string()),
            "the existing torrent is never deleted: {d:?}"
        );
        assert!(!d.contains(&"told".to_string()), "current copy kept: {d:?}");
        assert_eq!(
            store.get_selection(movie_slot(27205)).await.unwrap().hash,
            "hold"
        );
        assert!(store.get_owned("hnew".into()).await.is_none());
    }

    #[tokio::test]
    async fn episode_upgrade_never_stages_a_candidate_already_in_the_account() {
        let h = show_harness_singleton_720_ep_with(true).await;
        let app = with_unverifiable_prober(&h.app);
        upgrade_show_episodes_and_packs(&app, h.tmdb, &h.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            h.added.lock().unwrap().clone(),
            Vec::<String>::new(),
            "never staged"
        );
        let d = h.deleted.lock().unwrap().clone();
        assert!(
            !d.contains(&"tep1080".to_string()),
            "the existing torrent is never deleted: {d:?}"
        );
        assert_eq!(
            app.store
                .get_selection(crate::store::episode_slot(h.tmdb, 1, 1))
                .await
                .unwrap()
                .hash,
            h.old_720_hash
        );
    }

    #[tokio::test]
    async fn pack_correction_never_stages_a_candidate_already_in_the_account() {
        let mut h = show_harness_pack_2160_with_1080_candidate_with(true).await;
        h.set_allow_downgrade(true);
        let app = with_unverifiable_prober(&h.app);
        upgrade_show_episodes_and_packs(&app, h.tmdb, &h.group_hashes, Duration::from_secs(0))
            .await;
        assert_eq!(
            h.added.lock().unwrap().clone(),
            Vec::<String>::new(),
            "never staged"
        );
        let d = h.deleted.lock().unwrap().clone();
        assert!(
            !d.contains(&"tpack1080".to_string()),
            "the existing torrent is never deleted: {d:?}"
        );
        assert!(!d.contains(&"tpack2160".to_string()), "pack kept: {d:?}");
        assert!(app.store.get_owned(h.pack_hash.clone()).await.is_some());
    }

    #[tokio::test]
    async fn consolidation_never_stages_a_pack_already_in_the_account() {
        let store = mem_store();
        seed_two_scattered_episodes(&store).await;
        let mut torrents = show_listing();
        torrents.push(Torrent {
            id: "tpack".into(),
            hash: "hpack".into(),
            status: "downloaded".into(),
            ..Default::default()
        });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents,
            add_magnet: Some(AddMagnetResponse {
                id: "tpack".into(),
                uri: String::new(),
            }),
            torrent_info: Some(pack_info()),
            resolved_url: Some("https://cdn/p1".into()),
            deleted: deleted.clone(),
            added: added.clone(),
            ..Default::default()
        });
        let app = app_with_prober(
            Arc::new(MockScraper {
                candidates: vec![season_pack_candidate()],
            }),
            provider,
            store.clone(),
            unverifiable_prober(),
        );
        let res = consolidate_show_seasons(
            &app,
            SHOW_TMDB,
            &group_two(),
            &owned_two(),
            &[(1, 1), (1, 2)],
            Duration::from_secs(300),
        )
        .await;
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(
            added.lock().unwrap().clone(),
            Vec::<String>::new(),
            "never staged"
        );
        let d = deleted.lock().unwrap().clone();
        assert!(
            !d.contains(&"tpack".to_string()),
            "the existing torrent is never deleted: {d:?}"
        );
        for h in ["e1", "e2"] {
            assert!(store.get_owned(h.into()).await.is_some(), "{h} kept");
        }
    }
}
