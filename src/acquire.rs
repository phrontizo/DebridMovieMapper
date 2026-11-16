use crate::config::QualityPrefs;
use crate::probe::{self, ProbeError, Track, Verify};
use crate::provider::{DebridProvider, FileLocator};
use crate::rd_client::TorrentInfo;
use crate::release::{self, ReleaseInfo};
use crate::scraper::{MediaKind, Scraper};
use crate::store::{AcquireRequest, OwnedRecord, OwnedStatus, Provenance, Store};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// A candidate is already owned for this title, supplies the request (for a show, the requested
    /// episode, or coverage not yet known) and is present on the provider, so nothing was added. It
    /// may itself still be `Pending` (`observe` settles it).
    Acquired(String),
    /// This candidate was added and recorded `Pending`; `observe` resolves it (title check, probe)
    /// once its files appear.
    Pending(String),
    /// No non-blacklisted candidate within the ceiling and filters could be added: none scraped or
    /// all filtered/blacklisted, every remaining candidate already in the account (untracked, owned
    /// under another title, or a present copy of the show that doesn't provide the episode), the
    /// `max_attempts` add attempts all failed, or the request names no episode for a show.
    NoAcceptableRelease,
    /// Retry later: the scraper or the provider listing was unreachable, the staging intent could
    /// not be written (nothing was added), or an added candidate couldn't be recorded (the add was
    /// deleted again, or left for the staging sweep when that delete failed).
    TemporarilyUnavailable,
}

/// The verdict of a [`TitleValidator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleCheck {
    /// The file is the requested title (and, for a show, declares the requested episode).
    Match,
    /// The file is something else.
    Mismatch,
    /// No verdict: TMDB could not be reached (an outage past the client's retry budget, or a
    /// rejected API key). The caller must not reject the release on it; check again later.
    Unavailable,
}

/// A title's TMDB languages: what `AUDIO_LANGUAGE=original` resolves against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleLanguages {
    pub original_language: Option<String>,
    pub spoken_languages: Vec<String>,
}

/// Validates an acquired file genuinely matches the requested title (reuses `identify_name`).
#[async_trait]
pub trait TitleValidator: Send + Sync {
    /// `file_path` is the file's path in its torrent, or just its name: the title is identified from
    /// the name, while the season of a file with no episode code may come from a directory.
    async fn validate(
        &self,
        file_path: &str,
        expected_tmdb_id: u64,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> TitleCheck;

    /// The title's TMDB original + spoken languages, or `None` when they can't be fetched. Used to
    /// complete a stored request recorded without them (an account-mirror record) before
    /// `observe` re-acquires from it. The default knows none.
    async fn title_languages(&self, _tmdb_id: u64, _kind: MediaKind) -> Option<TitleLanguages> {
        None
    }
}

/// Probes a cached file's tracks (seam over `probe::probe_tracks` for testability).
#[async_trait]
pub trait Prober: Send + Sync {
    async fn probe(&self, cdn_url: &str) -> Result<Vec<Track>, ProbeError>;
}

pub struct HttpProber {
    pub http: reqwest::Client,
}

#[async_trait]
impl Prober for HttpProber {
    async fn probe(&self, cdn_url: &str) -> Result<Vec<Track>, ProbeError> {
        probe::probe_tracks(&self.http, cdn_url).await
    }
}

pub struct TmdbTitleValidator {
    pub tmdb: Arc<crate::tmdb_client::TmdbClient>,
}

#[async_trait]
impl TitleValidator for TmdbTitleValidator {
    async fn validate(
        &self,
        file_path: &str,
        expected: u64,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> TitleCheck {
        // Reuse the existing identification logic; confident == resolves to the expected tmdb id.
        // identify_name needs the file(s) for its show-vs-movie heuristic; the selected file
        // (whose name carries any SxxExx pattern) is enough to drive that for a single title.
        let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
        let files = [crate::rd_client::TorrentFile {
            id: 0,
            path: file_name.to_string(),
            bytes: 0,
            selected: 1,
        }];
        let found = crate::identification::identify_name(file_name, &files, &self.tmdb).await;
        title_check(&found, file_path, expected, kind, season, episode)
    }

    async fn title_languages(&self, tmdb_id: u64, kind: MediaKind) -> Option<TitleLanguages> {
        match self.tmdb.details(tmdb_id, media_type_of(kind)).await {
            Ok(d) => Some(TitleLanguages {
                original_language: d.original_language,
                spoken_languages: d.spoken_languages,
            }),
            Err(e) => {
                debug!("tmdb {} languages lookup failed: {}", tmdb_id, e);
                None
            }
        }
    }
}

/// The TMDB media type a request of `kind` names.
fn media_type_of(kind: MediaKind) -> crate::vfs::MediaType {
    match kind {
        MediaKind::Movie => crate::vfs::MediaType::Movie,
        MediaKind::Series => crate::vfs::MediaType::Show,
    }
}

/// [`TmdbTitleValidator`]'s verdict for the identification `found` of the file at `file_path` (a
/// path in its torrent, or just a file name). A failed TMDB search, matched or not, is
/// `Unavailable`, never a `Mismatch`, so an outage can't get a correct release blacklisted. The
/// identification must carry the requested media type as well as the id: TMDB's movie and TV id
/// spaces overlap, so movie 42 and show 42 are unrelated titles.
fn title_check(
    found: &crate::identification::Identification,
    file_path: &str,
    expected: u64,
    kind: MediaKind,
    season: Option<u32>,
    episode: Option<u32>,
) -> TitleCheck {
    use crate::identification::Identification;
    let m = match found {
        Identification::Identified(m) => m,
        Identification::NoMatch => return TitleCheck::Mismatch,
        Identification::Unavailable => return TitleCheck::Unavailable,
    };
    if m.external_id.as_deref() != Some(format!("tmdb:{}", expected).as_str())
        || m.media_type != media_type_of(kind)
    {
        return TitleCheck::Mismatch;
    }
    // A show's file must declare the requested episode — a multi-episode file (e.g. `S01E01E02`)
    // requested for its second episode included, as must a file with no episode code that names it
    // by an absolute number (`codeless_episode`).
    let name = file_path.rsplit('/').next().unwrap_or(file_path);
    let episode_ok = kind != MediaKind::Series
        || matches!(
            (season, episode),
            (Some(s), Some(e)) if crate::vfs::parse_se_all(name).contains(&(s, e))
                || codeless_episode(file_path) == Some((s, e))
        );
    if episode_ok {
        TitleCheck::Match
    } else {
        TitleCheck::Mismatch
    }
}

use crate::now_unix_secs as now_secs;

/// One-line summary of a ranked candidate for debug logs (short hash + the ranking-relevant
/// signals). Contains no token/URL — safe to log.
fn release_summary(r: &ReleaseInfo) -> String {
    let short = r.info_hash.get(..8).unwrap_or(r.info_hash.as_str());
    format!(
        "{short}({} {} {:?} s={})",
        if r.cached { "cached" } else { "uncached" },
        r.resolution
            .map(|p| format!("{p}p"))
            .unwrap_or_else(|| "?".into()),
        r.source,
        r.seeders
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".into()),
    )
}

pub struct AcquisitionEngine {
    provider: Arc<dyn DebridProvider>,
    scraper: Arc<dyn Scraper>,
    validator: Arc<dyn TitleValidator>,
    prober: Arc<dyn Prober>,
    store: Store,
    prefs: QualityPrefs,
    max_attempts: u32,
    stall_timeout: Duration,
    /// How long an optimistically-added torrent may stay Pending without resolving its file list
    /// (or seeding) before `observe` reaps it as dead and re-scrapes (SP3).
    dead_timeout: Duration,
    /// torrent_id -> (last progress, when first seen at that progress) for stall detection.
    progress: Arc<Mutex<HashMap<String, (f64, Instant)>>>,
    /// hash -> deferred-probe state (count + last-probe time): bounds the initial fast re-probe
    /// burst then backs off. A transient-deferring probe is never accepted unverified.
    verify_attempts: Arc<Mutex<HashMap<String, DeferState>>>,
    /// hash -> consecutive title-validation mismatches. A TMDB outage, or any failed search, comes
    /// back `TitleCheck::Unavailable` and counts no strike; a `Mismatch` still needs two CONSECUTIVE
    /// results (≥2 scan ticks apart) before the release is rejected as WrongTitle, so a one-tick
    /// mismatch glitch (an identification that briefly resolves to another title, e.g. while TMDB's
    /// search results shift) rides out. A genuine mismatch is rejected one tick later. In-memory
    /// (best-effort).
    validate_fails: Arc<Mutex<HashMap<String, u32>>>,
    /// torrent_id -> when `get_torrent_info` FIRST started failing consecutively for a listed Pending
    /// torrent. Every other Pending arm in `observe` has a `dead_timeout` escape; the info-fetch arm
    /// keys its escape off how long info has been CONSECUTIVELY unfetchable (reset on any successful
    /// fetch) rather than the torrent's `added_at` age — so a single transient blip on a long, healthy
    /// download can't reap a good release, while a genuinely stuck listed torrent (present but its info
    /// endpoint persistently errors) is still reaped + re-scraped instead of stranding Pending forever.
    /// In-memory (best-effort), pruned to live listing ids each tick like `progress`.
    info_fail_since: Arc<Mutex<HashMap<String, Instant>>>,
}

/// Whether `rec`, an owned record of the requested title, already supplies `req`. A movie copy
/// always does. A show copy does when it lists the requested episode in `provides`, when it is still
/// `Pending` (its `provides` is settled only once `observe` verifies it, so an in-flight pack lists
/// just the episode it was added for), or when its `provides` is empty (file names `parse_se_all`
/// can't decode, e.g. absolute numbering such as `Show - 041 [1080p]`, or an account-mirror record
/// written before `parse_se_all` read the `NxM` form: the VFS still emits those files, so the
/// coverage is unknown and assumed — adding a singleton for every episode would duplicate them all).
fn supplies_request(rec: &OwnedRecord, req: &AcquireRequest) -> bool {
    match (req.kind, req.season, req.episode) {
        (MediaKind::Series, Some(s), Some(e)) => {
            rec.status == OwnedStatus::Pending
                || rec.provides.is_empty()
                || rec.provides.contains(&(s, e))
        }
        _ => true,
    }
}

/// A show request that names no single episode (an account-mirror season pack's record). Shows are
/// acquired per episode, so such a request can't be scraped.
fn requests_no_episode(req: &AcquireRequest) -> bool {
    req.kind == MediaKind::Series && (req.season.is_none() || req.episode.is_none())
}

/// The representative video for a movie request: the largest video file (the feature). Used both to
/// choose what to select and to identify the served file afterwards — the latter matters because
/// some providers (TorBox) auto-select *every* file, so "first selected" is not the video (it could
/// be a `.srt`/`.nfo`). `acquire` and `observe` must select the same file. A misleading addon file
/// hint/index would have diverged them (one selecting a sample, the other the feature → a broken
/// locator / stuck Pending), so neither passes a hint and the largest video is always taken.
fn select_target(info: &TorrentInfo) -> Option<&crate::rd_client::TorrentFile> {
    info.files
        .iter()
        .filter(|f| crate::vfs::is_video_file(&f.path))
        .max_by_key(|f| f.bytes)
}

/// Choose file ids to select for a movie candidate (see `select_target`).
fn select_file_ids(info: &TorrentInfo) -> Vec<u32> {
    select_target(info).map(|f| vec![f.id]).unwrap_or_default()
}

/// Select file ids appropriate to the request kind: a single target video for a movie (so the
/// movie-pack guard can reject multi-feature packs), or ALL video files for a series (so a season
/// pack downloads fully on providers that don't auto-select, and `provides` covers every episode).
fn select_ids_for(kind: MediaKind, info: &TorrentInfo) -> Vec<u32> {
    match kind {
        MediaKind::Movie => select_file_ids(info),
        MediaKind::Series => info
            .files
            .iter()
            .filter(|f| crate::vfs::is_video_file(&f.path))
            .map(|f| f.id)
            .collect(),
    }
}

/// Map a torrent's SELECTED video files to (season, episode, file_path) by parsing SxxExx.
/// `pub(crate)` so the upgrade engine can reuse it for consolidation.
///
/// Each episode appears once, from the FIRST file (in provider file order) that carries it: a pack
/// holding one episode twice (per-language folders, two resolutions) is title-checked and probed on
/// that file by `observe`, so its slot must serve that file and `provides` must list the episode
/// once (a single-episode torrent with two versions is not a pack).
pub(crate) fn episode_files(info: &TorrentInfo) -> Vec<(u32, u32, String)> {
    let mut seen = std::collections::HashSet::new();
    info.files
        .iter()
        .filter(|f| f.selected == 1 && crate::vfs::is_video_file(&f.path))
        .flat_map(|f| {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path);
            // A multi-episode file contributes ALL its episodes (so `provides` is complete and the
            // reconciler doesn't re-acquire a contained episode as a duplicate singleton).
            crate::vfs::parse_se_all(name)
                .into_iter()
                .map(move |(s, e)| (s, e, f.path.clone()))
        })
        .filter(|(s, e, _)| seen.insert((*s, *e)))
        .collect()
}

/// The episode a show file with no `parse_se_all` code names by a trailing ` - NN` number, the
/// common anime form (`[Group] Show - 12 (1080p) [ABCD1234].mkv`,
/// `identification::trailing_episode_number`): that number, in the season the VFS places the file in
/// (`vfs::season_of_codeless_file`: season 1 unless the name or a directory names one, so
/// `Show S2 - 05 (1080p).mkv` is S02E05). `None` for a file with an episode code or no such number.
/// An absolute number past season 1 (`Show - 30` for S02E05) therefore names no requested episode.
fn codeless_episode(path: &str) -> Option<(u32, u32)> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if !crate::vfs::parse_se_all(name).is_empty() {
        return None;
    }
    let episode = crate::identification::trailing_episode_number(name).filter(|&e| e >= 1)?;
    Some((crate::vfs::season_of_codeless_file(path, name), episode))
}

/// A single-file show release named by an absolute number: the torrent's only selected video file,
/// when it carries no episode code but names an episode by `codeless_episode`, with that episode.
/// A torrent of several such files is a pack whose numbers can't be mapped to seasons without
/// TMDB's per-season episode counts, so it gets none (and `observe` still rejects it as
/// `EpisodeMissing`).
fn codeless_single_episode(info: &TorrentInfo) -> Option<(u32, u32, String)> {
    let mut videos = info
        .files
        .iter()
        .filter(|f| f.selected == 1 && crate::vfs::is_video_file(&f.path));
    let only = videos.next()?;
    if videos.next().is_some() {
        return None;
    }
    let (s, e) = codeless_episode(&only.path)?;
    Some((s, e, only.path.clone()))
}

/// The file of `info` that supplies the episode a show request names: the first selected video file
/// carrying its code (`episode_files`), else a single-file release naming it by an absolute number
/// (`codeless_single_episode`).
fn requested_episode_file(info: &TorrentInfo, req: &AcquireRequest) -> Option<(u32, u32, String)> {
    let requested =
        |(s, e, _): &(u32, u32, String)| Some(*s) == req.season && Some(*e) == req.episode;
    episode_files(info)
        .into_iter()
        .find(requested)
        .or_else(|| codeless_single_episode(info).filter(requested))
}

/// The size floor of a feature-length video: smaller ones are samples / extras / featurettes.
/// `pub(crate)` so every movie-pack / collection check counts features against the same floor.
pub(crate) const FEATURE_MIN_BYTES: u64 = 700_000_000;

/// Count feature-sized video files ([`FEATURE_MIN_BYTES`]). A real single-movie release has exactly
/// one; more than one signals a multi-movie pack.
/// `pub(crate)` so the upgrade engine can apply the same movie-pack guard when staging.
pub(crate) fn count_feature_videos(info: &TorrentInfo) -> usize {
    info.files
        .iter()
        .filter(|f| crate::vfs::is_video_file(&f.path) && f.bytes >= FEATURE_MIN_BYTES)
        .count()
}

/// Build a FileLocator for `path` within `info` (pairs the per-file link by position among selected).
/// `pub` so integration tests can resolve a real file's CDN URL through the exact engine logic.
pub fn locator_for(info: &TorrentInfo, hash: &str, path: &str) -> FileLocator {
    let mut link_idx = 0;
    for f in &info.files {
        if f.selected == 1 {
            if f.path == path {
                return FileLocator {
                    hash: hash.to_string(),
                    torrent_id: info.id.clone(),
                    file_id: f.id,
                    file_path: path.to_string(),
                    link: info.links.get(link_idx).cloned(),
                };
            }
            link_idx += 1;
        }
    }
    FileLocator {
        hash: hash.to_string(),
        torrent_id: info.id.clone(),
        file_id: 0,
        file_path: path.to_string(),
        link: None,
    }
}

/// The verdict of probing a cached file's tracks against the language requirement.
/// `pub` so the upgrade/consolidation engine can apply the same probe gate as acquisition.
pub enum VerifyResult {
    Pass,
    Accept,
    Reject(&'static str),
    Defer,
}

/// Number of consecutive deferred (transient-CDN) probes for a downloaded-but-Pending torrent
/// before `observe` stops re-probing it every scan tick and switches to the `VERIFY_BACKOFF`
/// cadence. The release is NEVER accepted unverified — accepting could freeze a wrong-language
/// release into the library; see the `VerifyResult::Defer` arm in `observe`.
const MAX_VERIFY_ATTEMPTS: u32 = 5;

/// Back-off between deferred re-probes once the initial fast burst (`MAX_VERIFY_ATTEMPTS`) is
/// spent, so a momentarily-unprobeable torrent isn't re-fetched (4 MB CDN read) every scan tick.
const VERIFY_BACKOFF: Duration = Duration::from_secs(3600);

/// Hard ceiling (anchored on the persisted `added_at`) on how long a downloaded-but-unprobeable
/// torrent may stay Pending before `observe` treats it as effectively broken and replaces it
/// (blacklist + re-acquire). Long enough to ride out an extended provider/CDN outage without
/// churning good releases, short enough that a genuinely-dead release is eventually swapped out.
/// It is only ever applied to a probe that has just deferred again, so a release is never replaced
/// on the strength of an old probe.
const VERIFY_DEADLINE_SECS: u64 = 24 * 3600;

/// How long a release must itself have been deferring before the verify deadline may replace it.
/// `added_at` is when the torrent was ADDED, so an uncached release that took more than
/// `VERIFY_DEADLINE_SECS` to download would otherwise be replaced after its first
/// `MAX_VERIFY_ATTEMPTS` re-probes (a few minutes of CDN trouble) — a day's download thrown away.
/// With the hourly back-off, the floor buys it at least one back-off re-probe after the fast burst;
/// it is replaced by the first re-probe that still defers once the floor has passed (about two
/// hours after its first deferral). In-memory like the rest of `DeferState`, so a restart starts
/// the floor (and the burst) over.
const VERIFY_DEADLINE_MIN_DEFER: Duration = Duration::from_secs(2 * 3600);

/// Per-hash deferred-probe state for transient (CDN-unavailable) probes: how many times we've
/// probed-and-deferred (to bound the fast burst), when we last probed (to back off afterwards) and
/// when it first deferred (for `VERIFY_DEADLINE_MIN_DEFER`), both in epoch seconds. In-memory only —
/// reset on restart; the hard deadline is anchored on the persisted `added_at`.
#[derive(Clone, Copy)]
struct DeferState {
    attempts: u32,
    last_probe_at: u64,
    first_defer_at: u64,
}

impl AcquisitionEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn DebridProvider>,
        scraper: Arc<dyn Scraper>,
        validator: Arc<dyn TitleValidator>,
        prober: Arc<dyn Prober>,
        store: Store,
        prefs: QualityPrefs,
        max_attempts: u32,
        stall_timeout: Duration,
        dead_timeout: Duration,
    ) -> Self {
        Self {
            provider,
            scraper,
            validator,
            prober,
            store,
            prefs,
            max_attempts,
            stall_timeout,
            dead_timeout,
            progress: Arc::new(Mutex::new(HashMap::new())),
            verify_attempts: Arc::new(Mutex::new(HashMap::new())),
            validate_fails: Arc::new(Mutex::new(HashMap::new())),
            info_fail_since: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Optimistically acquire `req`: scrape, rank, add the best non-blacklisted candidate, record
    /// it `Pending`, and return — WITHOUT synchronously selecting/validating/probing (that is
    /// `observe`'s job once the torrent's files resolve). A slow-to-seed release is therefore no
    /// longer judged or deleted prematurely. `provenance` is recorded and preserved across
    /// `observe`'s re-acquire (sticky).
    pub async fn acquire(&self, req: AcquireRequest, provenance: Provenance) -> AcquireOutcome {
        // A show is acquired one episode at a time. A request naming none (an account-mirror season
        // pack's record) would scrape S1E1 — the scraper's default — whatever season it held, and
        // every candidate added could only end in EpisodeMissing → blacklist → delete → next.
        if requests_no_episode(&req) {
            warn!(
                "acquire: tmdb {} — show request names no episode; not scraping",
                req.tmdb_id
            );
            return AcquireOutcome::NoAcceptableRelease;
        }
        let candidates = match self
            .scraper
            .find(&req.imdb_id, req.kind, req.season, req.episode)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!("scrape failed for {}: {}", req.imdb_id, e);
                return AcquireOutcome::TemporarilyUnavailable;
            }
        };
        debug!(
            "acquire: tmdb {} ({}) — scraped {} candidates",
            req.tmdb_id,
            req.imdb_id,
            candidates.len()
        );
        // Fetch the per-title blacklist ONCE and test membership in memory, rather than one awaited
        // redb read per candidate (Torrentio commonly returns dozens of streams per title).
        let blacklist = self
            .store
            .blacklisted_hashes_for(req.kind, req.tmdb_id)
            .await;
        let mut parsed: Vec<ReleaseInfo> = Vec::new();
        let mut blacklisted = 0usize;
        for c in &candidates {
            let r = release::parse(c);
            if blacklist.contains(&r.info_hash.to_ascii_lowercase()) {
                blacklisted += 1;
                continue;
            }
            parsed.push(r);
        }
        // Resolve `AUDIO_LANGUAGE=original` (the default) against this title's original + spoken
        // languages so a wrong-language release is down-ranked here, not just rejected later at probe
        // time (which would burn an acquire cycle). Matches `probe::verify`'s resolution of `Original`.
        let prefs = self.prefs.with_resolved_audio(
            req.original_language.as_deref(),
            req.spoken_languages.as_deref().unwrap_or_default(),
        );
        let ranked = release::rank(parsed, &prefs);
        debug!(
            "acquire: tmdb {} — {} ranked after filters ({} blacklisted); top: [{}]",
            req.tmdb_id,
            ranked.len(),
            blacklisted,
            ranked
                .iter()
                .take(3)
                .map(release_summary)
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Live provider listing (lazily fetched once: at the first candidate owned for this title,
        // or before the first add). Outer Option = "fetched?", inner = "fetch succeeded?". Used to
        // distinguish "already acquired" from a lapsed/externally-deleted owned record that must be
        // RE-added, and to keep untracked torrents already in the account out of the add loop.
        let mut present: Option<Option<std::collections::HashSet<String>>> = None;

        // Order-independent idempotency pre-scan over EVERY ranked candidate (owned lookups only;
        // adds stay limited to `max_attempts` add attempts below, and candidates skipped there as
        // already in the account don't count): if ANY is already owned for THIS title, supplies the
        // request (`supplies_request`: for a show, the requested episode, or unknown/unsettled
        // coverage) and is confirmed present on the provider, the request is already acquired —
        // short-circuit regardless of rank order. Without this, a higher-ranked NON-owned candidate
        // would be `add_magnet`'d (a DUPLICATE torrent) even though a lower-ranked owned+present
        // candidate exists. A present owned copy of the show that does NOT provide the episode (a
        // pack with only some files selected) is no short-circuit — the episode would never be
        // added — and the add loop never re-adds it either. A transient listing failure (inner
        // `None`) preserves the old idempotent behaviour (treat an owned record as present) rather
        // than re-adding on a listing blip.
        //
        // Cache each candidate's owner (`(kind, tmdb_id)` of its owned record, `None` = not owned)
        // as we read it so the add loop below need not re-issue the same `get_owned` (a
        // spawn_blocking redb read): the pre-scan visits every candidate before the add loop runs
        // unless it short-circuits first, so the cache is complete by the time the add loop reads it.
        let this_title = (req.kind, req.tmdb_id);
        let mut owner: std::collections::HashMap<String, Option<(MediaKind, u64)>> =
            std::collections::HashMap::new();
        for cand in &ranked {
            let rec = self.store.get_owned(cand.info_hash.clone()).await;
            let rec_owner = rec.as_ref().map(|r| (r.request.kind, r.request.tmdb_id));
            owner.insert(cand.info_hash.clone(), rec_owner);
            if rec_owner != Some(this_title) || !rec.is_some_and(|r| supplies_request(&r, &req)) {
                continue;
            }
            if present.is_none() {
                present = Some(self.present_hashes().await);
            }
            let owned_present = match present.as_ref().expect("just fetched") {
                Some(p) => p.contains(&cand.info_hash.to_ascii_lowercase()),
                None => true, // listing unavailable → keep the old idempotent behaviour
            };
            if owned_present {
                debug!(
                    "acquire: tmdb {} — {} already owned + present (idempotent, no re-add)",
                    req.tmdb_id, cand.info_hash
                );
                return AcquireOutcome::Acquired(cand.info_hash.clone());
            }
        }

        if ranked.is_empty() {
            debug!(
                "acquire: tmdb {} — no acceptable release ({} scraped, none passed filters)",
                req.tmdb_id,
                candidates.len()
            );
            return AcquireOutcome::NoAcceptableRelease;
        }
        // A candidate already in the account that this title doesn't own must never be added: on
        // TorBox an add of a present infohash returns the EXISTING torrent's id, so the engine would
        // adopt a torrent it never added (a still-downloading manual add, an unidentified torrent)
        // as its own Pending record — and delete it on any rejection, or remove it later as an
        // engine copy. So the listing must be known before the first add: if it can't be fetched,
        // add nothing and retry later (mirrors the upgrade engine's `add_staging_candidate`).
        if present.is_none() {
            present = Some(self.present_hashes().await);
        }
        let Some(Some(present_set)) = present.as_ref() else {
            warn!(
                "acquire: tmdb {} — provider listing unavailable; not adding a candidate this time",
                req.tmdb_id
            );
            return AcquireOutcome::TemporarilyUnavailable;
        };

        // Add the best eligible candidate, trying the next one only when the add itself fails, for
        // at most `max_attempts` adds.
        let mut attempts = 0u32;
        for cand in ranked {
            let hash_lc = cand.info_hash.to_ascii_lowercase();
            match owner.get(&cand.info_hash).copied().flatten() {
                // Owned for this title and present, yet not a short-circuit in the pre-scan: a copy
                // of the show that doesn't provide the requested episode. Re-adding it would add
                // nothing (on TorBox it would adopt the torrent over its record, and `observe` would
                // then delete it as EpisodeMissing), so try the next candidate.
                Some(o) if o == this_title && present_set.contains(&hash_lc) => {
                    debug!(
                        "acquire: tmdb {} — owned {} is present but doesn't provide S{:02}E{:02}; not re-adding it",
                        req.tmdb_id,
                        cand.info_hash,
                        req.season.unwrap_or_default(),
                        req.episode.unwrap_or_default()
                    );
                    continue;
                }
                // Owned for this title but not present (the pre-scan returned on a present one that
                // supplies the request): the title lapsed (cache lost / externally deleted), the
                // documented re-acquire case.
                Some(o) if o == this_title => {
                    debug!(
                        "acquire: tmdb {} — owned {} is absent (lapsed) — re-adding",
                        req.tmdb_id, cand.info_hash
                    );
                }
                // Owned under ANOTHER title: adding it would overwrite that title's owned record (and
                // on TorBox adopt its torrent). Most likely one of the two is misidentified.
                Some((kind, other)) => {
                    warn!(
                        "acquire: tmdb {} — candidate {} is owned under {:?} tmdb {} (possible misidentification); skipping it",
                        req.tmdb_id, cand.info_hash, kind, other
                    );
                    continue;
                }
                None if present_set.contains(&hash_lc) => {
                    debug!(
                        "acquire: tmdb {} — {} is already in the provider account but not tracked; not adding it",
                        req.tmdb_id, cand.info_hash
                    );
                    continue;
                }
                None => {}
            }
            if attempts >= self.max_attempts {
                break;
            }
            attempts += 1;
            debug!(
                "acquire: tmdb {} — adding {}",
                req.tmdb_id,
                release_summary(&cand)
            );
            // Open a staging intent BEFORE the add, as the upgrade engine's `add_staging_candidate`
            // does: from the add until the Pending record below is written the account holds a
            // torrent no owned record names, and an acquire cut off in between (a dropped job on
            // shutdown, a caught panic, an OOM kill) must leave a trace, or the account mirror would
            // adopt the never-checked release as a `Verified` copy. The scan loop's staging sweep
            // deletes the torrent of an intent left open. Nothing is added without it.
            if let Err(e) = self
                .store
                .put_staging_intent(cand.info_hash.clone(), now_secs())
                .await
            {
                warn!(
                    "acquire: tmdb {} — staging intent write failed for {} ({}); not adding, retrying later",
                    req.tmdb_id, cand.info_hash, e
                );
                return AcquireOutcome::TemporarilyUnavailable;
            }
            let magnet = format!("magnet:?xt=urn:btih:{}", cand.info_hash);
            let added = match self.provider.add_magnet(&magnet).await {
                Ok(a) => a,
                Err(e) => {
                    // Nothing was added: close the intent, or a later sweep would delete a torrent of
                    // this hash the user adds by hand.
                    let _ = self
                        .store
                        .clear_staging_intent(cand.info_hash.clone())
                        .await;
                    warn!(
                        "add_magnet failed for {}: {} — trying next",
                        cand.info_hash, e
                    );
                    continue;
                }
            };
            if let Err(e) = self
                .store
                .set_staging_torrent_id(cand.info_hash.clone(), added.id.clone())
                .await
            {
                // The sweep still finds the torrent by its hash in the listing.
                warn!(
                    "acquire: failed to record added torrent id {} for {}: {}",
                    added.id, cand.info_hash, e
                );
            }
            // Record Pending immediately (the verdict belongs to observe).
            let provides = match (req.kind, req.season, req.episode) {
                (MediaKind::Series, Some(s), Some(e)) => vec![(s, e)],
                _ => vec![],
            };
            // If either write fails, roll the add back (as the upgrade engine's `record_staged`
            // does): an untracked torrent would be skipped by every later acquire as already in the
            // account, and the account mirror would adopt it with no provenance (never removed by
            // the lifecycle, never title-checked or probed). The hash was absent from the listing
            // before the add, so `added.id` is ours to delete, on TorBox too.
            let recorded = match self
                .store
                .put_owned(
                    cand.info_hash.clone(),
                    OwnedRecord {
                        request: req.clone(),
                        provenance: provenance.clone(),
                        added_at: now_secs(),
                        status: OwnedStatus::Pending,
                        provides,
                        quality: Some(release::QualitySummary::of(&cand, &self.prefs)),
                    },
                )
                .await
            {
                Err(e) => Err(("owned-record", e)),
                Ok(()) => match self
                    .store
                    .put_authoritative(cand.info_hash.clone(), req.metadata.clone())
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ = self.store.remove_owned(cand.info_hash.clone()).await;
                        Err(("authoritative", e))
                    }
                },
            };
            if let Err((what, e)) = recorded {
                warn!(
                    "acquire: tmdb {} — added {} but its {} write failed ({}); deleting the add, retrying later",
                    req.tmdb_id, cand.info_hash, what, e
                );
                // Close the intent only once the torrent is gone: a failed delete keeps it, so the
                // account mirror leaves the unchecked torrent alone and the staging sweep deletes it
                // (as the upgrade engine's `discard_staged` does).
                match self.provider.delete_torrent(&added.id).await {
                    Ok(()) => {
                        let _ = self
                            .store
                            .clear_staging_intent(cand.info_hash.clone())
                            .await;
                    }
                    Err(e) => warn!(
                        "acquire: deleting the unrecorded add {} ({}) failed: {}; the staging sweep retries it",
                        cand.info_hash, added.id, e
                    ),
                }
                return AcquireOutcome::TemporarilyUnavailable;
            }
            // Recorded `Pending`: the intent is done. A failed clear is harmless — the sweep closes an
            // intent whose hash is owned without deleting anything.
            let _ = self
                .store
                .clear_staging_intent(cand.info_hash.clone())
                .await;
            // Best-effort: if the file list is already present (cached), select now so it is
            // immediately resolvable; otherwise observe selects once metadata resolves.
            //
            // Select WITHOUT the addon hint (None, None) so the chosen file matches what `observe`
            // later records/probes (`select_target` = the largest video). A valid
            // single-feature movie's feature IS the largest (the pack-guard rejects multi-feature),
            // so this is correct; and it keeps acquire and observe on the same file when a misleading
            // hint points at a non-largest file (e.g. a sample) — acquire would select that while
            // observe probes the largest, leaving the largest unselected → a broken locator → a stuck
            // Pending.
            if let Ok(info) = self.provider.get_torrent_info(&added.id).await {
                let ids = select_ids_for(req.kind, &info);
                if !ids.is_empty() {
                    let csv = ids
                        .iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    let _ = self.provider.select_files(&added.id, &csv).await;
                }
            }
            // A DIFFERENT owned hash for this exact slot may have lapsed off the provider (we just
            // added a better/replacement release above it). Reap its orphaned records — but only
            // against a non-empty listing, so a spurious empty one can't reap a still-owned record.
            if !present_set.is_empty() {
                self.reap_lapsed_superseded(&req, &cand.info_hash, present_set)
                    .await;
            }
            return AcquireOutcome::Pending(cand.info_hash);
        }
        debug!(
            "acquire: tmdb {} — no acceptable release ({} scraped, none could be added)",
            req.tmdb_id,
            candidates.len()
        );
        AcquireOutcome::NoAcceptableRelease
    }

    /// The provider listing's info hashes, lowercased; `None` when the listing can't be fetched.
    async fn present_hashes(&self) -> Option<std::collections::HashSet<String>> {
        self.provider.get_torrents().await.ok().map(|ts| {
            ts.iter()
                .map(|t| t.hash.to_ascii_lowercase())
                .collect::<std::collections::HashSet<String>>()
        })
    }

    /// Validate a file name against an expected title (used by the upgrade engine before staging).
    pub async fn validate_title(
        &self,
        file_name: &str,
        tmdb_id: u64,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> TitleCheck {
        self.validator
            .validate(file_name, tmdb_id, kind, season, episode)
            .await
    }

    /// Probe a specific selected file (by path) within `info` and return the verify verdict.
    /// Used by the upgrade/consolidation engine to apply the same probe gate as acquisition.
    pub async fn probe_file(
        &self,
        info: &TorrentInfo,
        hash: &str,
        file_path: &str,
        req: &AcquireRequest,
    ) -> VerifyResult {
        let locator = locator_for(info, hash, file_path);
        self.verify_file(&locator, req).await
    }

    async fn verify_file(&self, locator: &FileLocator, req: &AcquireRequest) -> VerifyResult {
        let url = match self.provider.resolve_url(locator).await {
            Ok(u) => u,
            Err(e) => {
                // Distinguish a resolve failure (provider/requestdl) from a probe fetch failure —
                // both defer, but only this branch means we never even got a CDN url.
                debug!(
                    "verify: resolve_url failed for tmdb {} hash {} (torrent_id={} file_id={}): {} — defer",
                    req.tmdb_id, locator.hash, locator.torrent_id, locator.file_id, e
                );
                return VerifyResult::Defer;
            }
        };
        let langreq = probe::LangReq {
            audio: self.prefs.audio.clone(),
            subtitle: self.prefs.subtitle.clone(),
            original_language: req.original_language.clone(),
            spoken_languages: req.spoken_languages.clone().unwrap_or_default(),
        };
        match self.prober.probe(&url).await {
            Ok(tracks) => match probe::verify(&tracks, &langreq) {
                Verify::Pass => VerifyResult::Pass,
                Verify::FailAudio => VerifyResult::Reject("BadAudio"),
                Verify::FailSubtitle => VerifyResult::Reject("BadSubtitle"),
                Verify::Inconclusive => VerifyResult::Accept,
            },
            Err(ProbeError::Corrupt) => VerifyResult::Reject("Corrupt"),
            Err(ProbeError::Unsupported) | Err(ProbeError::TracksNotFound) => VerifyResult::Accept,
            Err(ProbeError::Transient) => {
                debug!(
                    "verify: probe transient for tmdb {} hash {} (file_id={}) — fetched url ok but the probe window read failed; defer",
                    req.tmdb_id, locator.hash, locator.file_id
                );
                // The URL may be the bad part (stale, or a 403/404/410): drop the provider's cached
                // resolution so the next attempt resolves a fresh one instead of re-fetching the same
                // URL for the hours the provider caches it.
                self.provider.invalidate(locator).await;
                VerifyResult::Defer
            }
        }
    }

    /// Write `provides` and the per-slot `selection` entries for a now-Verified owned torrent,
    /// and update its status. For a movie: one `movie_slot` entry for the selected file. For a
    /// show: `episode_slot` entries for every SE-mapped selected file, and `provides` is that set.
    /// `unverified` is stamped on the record's quality snapshot so the upgrade pass can later replace
    /// an unchecked release with a checked one: `true` for an `Accept` (the tracks couldn't be
    /// checked) or for a `Pass` that checked no audio language ([`Self::audio_unchecked`]),
    /// otherwise `false`. Returns whether the record was verified: `false`, with nothing written,
    /// when it is gone (removed while the probe ran) or the write failed (the next tick verifies it
    /// again).
    async fn record_verified(
        &self,
        hash: &str,
        req: &AcquireRequest,
        info: &TorrentInfo,
        selected_path: &str,
        unverified: bool,
    ) -> bool {
        // Compute the SE-mapped episode set for a series (this hash's `provides` — the season-pack
        // churn fix); a movie supplies none. A single-file release with no episode code that names
        // the requested episode by an absolute number (`codeless_single_episode`, how `observe`
        // found it) supplies that episode, so `monitor_episodes` sees it covered.
        let eps: Vec<(u32, u32, String)> = match req.kind {
            MediaKind::Movie => Vec::new(),
            MediaKind::Series => {
                let eps = episode_files(info);
                if eps.is_empty() {
                    codeless_single_episode(info)
                        .filter(|(s, e, path)| {
                            Some(*s) == req.season
                                && Some(*e) == req.episode
                                && path == selected_path
                        })
                        .into_iter()
                        .collect()
                } else {
                    eps
                }
            }
        };

        // One atomic read-modify-write FIRST, so status (+ series `provides`) and the cached-quality
        // refresh commit together, a record removed concurrently is never written back, AND the
        // slot-overwrite gate below sees this hash's REFRESHED quality. CRITICAL: a torrent reaching
        // this point is `downloaded` (present on the provider), so it IS cached — refresh the
        // acquire-time `quality` snapshot (an UNCACHED candidate recorded `cached: false`), else the
        // upgrade engine's "`!current.cached` → any cached candidate is an upgrade" rule would later
        // replace this working copy with a LOWER-quality cached release and delete it.
        let provides =
            (req.kind == MediaKind::Series).then(|| eps.iter().map(|(s, e, _)| (*s, *e)).collect());
        let new_quality = match self
            .store
            .mark_verified_if_present(hash.to_string(), provides, unverified)
            .await
        {
            Ok(Some(q)) => q,
            Ok(None) => {
                // Record vanished (removed concurrently while the probe ran, e.g. by
                // `execute_remove`, which deleted its torrent): write no selection, or a slot would
                // point at a hash nothing owns.
                debug!(
                    "observe: tmdb {} hash {} record vanished before it was verified; not writing selection",
                    req.tmdb_id, hash
                );
                return false;
            }
            Err(e) => {
                // Still Pending: the next tick verifies it again.
                warn!(
                    "observe: tmdb {} hash {} could not be marked verified ({}); retrying next tick",
                    req.tmdb_id, hash, e
                );
                return false;
            }
        };

        // Write the selection slot(s). For a MOVIE the acquire path is authoritative for the single
        // title slot (re-acquire repoints to the fresh copy), so write unconditionally. For a SERIES
        // pack, DON'T let a pack take a sibling-episode slot that already serves a different,
        // BETTER release: it would serve a worse file, and the scan loop's selection repoint would
        // move the slot straight back. `provides` (set above) already covers the whole pack
        // regardless of which slots we (re)point, so re-acquire churn is unaffected.
        let Some(id) = crate::vfs::tmdb_id_of(&req.metadata) else {
            return true;
        };
        match req.kind {
            MediaKind::Movie => {
                let _ = self
                    .store
                    .put_selection(
                        crate::store::movie_slot(id),
                        crate::store::SelectionEntry {
                            hash: hash.to_string(),
                            file_path: selected_path.to_string(),
                        },
                    )
                    .await;
            }
            MediaKind::Series => {
                for (s, e, path) in &eps {
                    let slot = crate::store::episode_slot(id, *s, *e);
                    let overwrite = match self.store.get_selection(slot.clone()).await {
                        // Empty slot → take it.
                        None => true,
                        // Already ours → idempotent re-point (e.g. file_path refresh).
                        Some(existing) if existing.hash.eq_ignore_ascii_case(hash) => true,
                        Some(existing) => match self.store.get_owned(existing.hash.clone()).await {
                            // The slot points at a release we no longer own (stale) → repoint to
                            // this present pack, which supplies the episode.
                            None => true,
                            // A different, still-owned release holds this episode: overwrite only
                            // when this pack is NOT a downgrade, judged as the scan loop's
                            // selection repoint judges a slot (`selection::takes_slot_from`), so
                            // that pass never moves the slot back: the rank of the file each copy
                            // serves (resolution, then source tier, then checked before unchecked;
                            // an untagged axis is the lowest), a checked copy giving way to an
                            // unchecked one only for a higher resolution; at equal rank the score
                            // decides. With no quality on either side nothing is provable → keep.
                            Some(old) => {
                                (new_quality.is_some() || old.quality.is_some())
                                    && crate::selection::takes_slot_from(
                                        (new_quality.as_ref(), path),
                                        (old.quality.as_ref(), &existing.file_path),
                                    )
                            }
                        },
                    };
                    if overwrite {
                        let _ = self
                            .store
                            .put_selection(
                                slot,
                                crate::store::SelectionEntry {
                                    hash: hash.to_string(),
                                    file_path: path.clone(),
                                },
                            )
                            .await;
                    }
                }
            }
        }
        true
    }

    /// Called each scan tick with the current torrent list. Resolves optimistically-added Pending
    /// torrents (select files → pack-guard → title-validation → probe → Verified + provides +
    /// selection), reaps genuinely-dead/never-resolving ones after `dead_timeout`, and recovers by
    /// re-scraping. No scraping on the happy path. Returns the lowercased hashes whose listed
    /// torrents it deleted, so the caller can drop them from this tick's listing before anything
    /// shows or adopts them (`torrents` is the listing from before the deletes).
    pub async fn observe(
        &self,
        torrents: &[crate::rd_client::Torrent],
    ) -> std::collections::HashSet<String> {
        let mut deleted = std::collections::HashSet::new();
        let owned = self.store.all_owned().await;
        // Key by lowercased provider hash so it matches the lowercased candidate hashes we store.
        // When the listing contains DUPLICATE entries for a hash (a repair leak or an external
        // re-add — exactly what the scan loop's dedup deletes), resolve each hash to its BEST entry
        // by HEALTH: a `downloaded` torrent beats a still-downloading/queued one, which beats a
        // dead/error one — and among equal health keep the newest (the provider lists newest-first, so
        // the first-seen). Ranking by health (not just "downloaded vs not") is essential: otherwise a
        // newest `error`/`dead` duplicate masks an older but LIVE `downloading` one, and `observe`
        // then reaps+blacklists the hash (`fail_and_reacquire`), abandoning the in-flight download.
        fn dup_rank(status: &str) -> u8 {
            match status {
                "downloaded" => 2,
                "magnet_error" | "dead" | "error" | "virus" => 0,
                _ => 1, // downloading / queued / waiting_files_selection / …
            }
        }
        let mut by_hash: HashMap<String, &crate::rd_client::Torrent> = HashMap::new();
        // Every listing id of each hash: a rejected hash is deleted under all of them (a retried
        // add or a manual re-add can list it twice), not only under the entry `by_hash` keeps, which
        // would leave the other copy in the account untracked (blacklisted, so never mirrored).
        let mut ids_by_hash: HashMap<String, Vec<&str>> = HashMap::new();
        for t in torrents {
            let key = t.hash.to_ascii_lowercase();
            ids_by_hash
                .entry(key.clone())
                .or_default()
                .push(t.id.as_str());
            match by_hash.get(&key) {
                // Keep the existing entry only when it ranks at least as healthy (first-seen = newest
                // wins ties); a strictly-healthier incoming entry replaces it.
                Some(existing) if dup_rank(&existing.status) >= dup_rank(&t.status) => {}
                _ => {
                    by_hash.insert(key, t);
                }
            }
        }

        // A completely empty listing is untrustworthy as a reap signal: a spurious empty-but-OK
        // provider response (eventual consistency) would otherwise mark EVERY in-flight Pending
        // "absent" and, past the dead-timeout, blacklist + reap it. Treat empty like a failed listing
        // for reaping (the reconciler already skips a *failed* get_torrents tick for the same reason).
        let listing_empty = torrents.is_empty();
        // Set once a title check this tick finds TMDB unreachable (see the validation step).
        let mut tmdb_down = false;
        for (hash, rec) in &owned {
            let Some(t) = by_hash.get(hash.as_str()).copied() else {
                // Not in the listing. A Pending torrent that never registered/resolved is dead
                // once it has been waiting longer than the dead-timeout — but only trust "absent"
                // when the listing isn't wholesale empty.
                if rec.status == OwnedStatus::Pending {
                    let age = now_secs().saturating_sub(rec.added_at);
                    if age > self.dead_timeout.as_secs() && !listing_empty {
                        self.reject(
                            hash,
                            &[],
                            &rec.request,
                            "NeverResolved",
                            &rec.provenance,
                            &mut deleted,
                        )
                        .await;
                    } else {
                        debug!(
                            "observe: tmdb {} hash {} absent from listing ({}s/{}s before reap, listing_empty={}) — waiting",
                            rec.request.tmdb_id, hash, age, self.dead_timeout.as_secs(), listing_empty
                        );
                    }
                }
                continue;
            };
            let ids: &[&str] = ids_by_hash
                .get(hash.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default();
            if matches!(
                t.status.as_str(),
                "magnet_error" | "dead" | "error" | "virus"
            ) {
                // A record with no IMDB id (e.g. a mirror torrent whose id didn't resolve) can't be
                // re-scraped, so don't delete-without-replace — leave it for on-read repair (dav_fs).
                if rec.request.imdb_id.trim().is_empty() {
                    debug!(
                        "observe: tmdb {} hash {} provider status={:?} but no IMDB id to re-scrape — leaving in place",
                        rec.request.tmdb_id, hash, t.status
                    );
                    continue;
                }
                debug!(
                    "observe: tmdb {} hash {} provider status={:?} — dead",
                    rec.request.tmdb_id, hash, t.status
                );
                self.reject(
                    hash,
                    ids,
                    &rec.request,
                    "Dead",
                    &rec.provenance,
                    &mut deleted,
                )
                .await;
                continue;
            }
            if rec.status == OwnedStatus::Verified {
                self.progress.lock().await.remove(&t.id);
                continue;
            }
            // Pending: fetch info to inspect files.
            let info = match self.provider.get_torrent_info(&t.id).await {
                Ok(i) => {
                    // Info resolved — clear any consecutive-failure timer for this id.
                    self.info_fail_since.lock().await.remove(&t.id);
                    i
                }
                Err(e) => {
                    // A listed torrent whose info endpoint keeps erroring is genuinely stuck. Reap it
                    // once the failures have PERSISTED past the dead-timeout (anchored on the first
                    // consecutive failure, not the record's age), so a single transient blip on a
                    // healthy long download rides out but a stuck one doesn't strand Pending forever.
                    let first = {
                        let mut map = self.info_fail_since.lock().await;
                        *map.entry(t.id.clone()).or_insert_with(Instant::now)
                    };
                    if !listing_empty && first.elapsed() >= self.dead_timeout {
                        self.reject(
                            hash,
                            ids,
                            &rec.request,
                            "InfoUnfetchable",
                            &rec.provenance,
                            &mut deleted,
                        )
                        .await;
                    } else {
                        debug!(
                            "observe: hash {} get_torrent_info({}) failed: {} — retry next tick ({}s unfetchable)",
                            hash, t.id, e, first.elapsed().as_secs()
                        );
                    }
                    continue;
                }
            };
            let has_files = info
                .files
                .iter()
                .any(|f| crate::vfs::is_video_file(&f.path));
            if !has_files {
                let age = now_secs().saturating_sub(rec.added_at);
                if age > self.dead_timeout.as_secs() {
                    self.reject(
                        hash,
                        ids,
                        &rec.request,
                        "NeverResolved",
                        &rec.provenance,
                        &mut deleted,
                    )
                    .await;
                } else {
                    debug!(
                        "observe: hash {} no video files yet ({}s/{}s before reap) — waiting",
                        hash,
                        age,
                        self.dead_timeout.as_secs()
                    );
                }
                continue;
            }
            // Ensure something is selected so it downloads (RD: nothing downloads until selected).
            let none_selected = info.files.iter().all(|f| f.selected != 1);
            if none_selected {
                // If selection never takes effect across the whole dead-timeout window, the hash is
                // genuinely stuck (the provider lists files but won't honour select_files for it) —
                // reap + re-acquire rather than retrying selection forever, consistent with the
                // !has_files / missing-selected-path branches. Checked FIRST so the give-up tick
                // doesn't waste a select_files round-trip on a hash we're about to reap. A provider
                // OUTAGE instead fails the get_torrent_info above and skips this branch, so this only
                // fires on a stuck hash.
                if now_secs().saturating_sub(rec.added_at) > self.dead_timeout.as_secs() {
                    self.reject(
                        hash,
                        ids,
                        &rec.request,
                        "SelectStuck",
                        &rec.provenance,
                        &mut deleted,
                    )
                    .await;
                    continue;
                }
                // No candidate hint preserved in OwnedRecord; use the kind-appropriate fallback (largest video for movies, all videos for series).
                let ids = select_ids_for(rec.request.kind, &info);
                debug!(
                    "observe: hash {} nothing selected yet — selecting {} file(s)",
                    hash,
                    ids.len()
                );
                if !ids.is_empty() {
                    let csv = ids
                        .iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    let _ = self.provider.select_files(&t.id, &csv).await;
                }
                continue; // re-inspect next tick after selection settles
            }
            // Movie-pack guard (deferred from acquire).
            if rec.request.kind == MediaKind::Movie && count_feature_videos(&info) > 1 {
                self.reject(
                    hash,
                    ids,
                    &rec.request,
                    "MoviePack",
                    &rec.provenance,
                    &mut deleted,
                )
                .await;
                continue;
            }
            // Choose the representative file to validate + probe. Movie: the feature video.
            // Series: the file matching the REQUESTED episode (validating a pack's largest file
            // against the requested (s,e) would misfire — a pack holds many episodes), or a
            // single-file release that names it by an absolute number.
            let selected_path = match rec.request.kind {
                MediaKind::Movie => select_target(&info).map(|f| f.path.clone()),
                MediaKind::Series => requested_episode_file(&info, &rec.request).map(|(_, _, p)| p),
            };
            let Some(selected_path) = selected_path else {
                // The requested episode isn't present (or no video resolved). Past the dead-timeout,
                // treat it as a wrong/incomplete pick and re-acquire; otherwise wait (metadata may
                // still be settling).
                if now_secs().saturating_sub(rec.added_at) > self.dead_timeout.as_secs() {
                    self.reject(
                        hash,
                        ids,
                        &rec.request,
                        "EpisodeMissing",
                        &rec.provenance,
                        &mut deleted,
                    )
                    .await;
                } else {
                    debug!(
                        "observe: tmdb {} hash {} target file (s={:?} e={:?}) not present yet — waiting",
                        rec.request.tmdb_id, hash, rec.request.season, rec.request.episode
                    );
                }
                continue;
            };
            if t.status != "downloaded" {
                // Still downloading — stall check only (uses stall_timeout as the no-progress
                // ceiling). Defer title-validation + probe until the torrent is downloaded so we
                // don't burn a TMDB lookup (and a 4 MB CDN probe) on EVERY scan tick for the whole
                // download. A wrong-title torrent is still caught — just once it finishes.
                if self.is_stalled(&t.id, t.progress).await {
                    self.reject(
                        hash,
                        ids,
                        &rec.request,
                        "Stalled",
                        &rec.provenance,
                        &mut deleted,
                    )
                    .await;
                } else {
                    debug!(
                        "observe: tmdb {} hash {} downloading (status={:?}, progress={:.0}%) — waiting",
                        rec.request.tmdb_id, hash, t.status, t.progress
                    );
                }
                continue;
            }
            // Deferred-probe back-off. A downloaded torrent whose probe keeps coming back Transient
            // (CDN momentarily unavailable) is re-probed every tick for the first
            // MAX_VERIFY_ATTEMPTS (to catch a quick blip), then only on a back-off (so we don't
            // re-fetch 4 MB every tick). It is NEVER accepted unverified; the hard deadline that
            // replaces it is applied in the `VerifyResult::Defer` arm below, on a re-probe that has
            // just deferred again, so a CDN that recovered since the last probe is given its chance.
            let defer_state = self.verify_attempts.lock().await.get(hash).copied();
            if let Some(st) = defer_state.filter(|st| st.attempts >= MAX_VERIFY_ATTEMPTS) {
                if now_secs().saturating_sub(st.last_probe_at) < VERIFY_BACKOFF.as_secs() {
                    debug!(
                        "observe: tmdb {} hash {} probe still deferring (attempts={}) — backing off",
                        rec.request.tmdb_id, hash, st.attempts
                    );
                    continue; // within the back-off window — wait before re-probing
                }
            }
            // Downloaded → validate the title (ONCE), then probe and finalise.
            let file_name = selected_path
                .rsplit('/')
                .next()
                .unwrap_or(&selected_path)
                .to_string();
            // Validate ONLY on the first downloaded tick (`defer_state.is_none()`). The file name is
            // invariant, so re-validating adds nothing — but `validate` makes an UNCACHED live TMDB
            // call, and a glitch that reads as a mismatch would `fail_and_reacquire` (blacklist +
            // delete) an already-validated, correct, cached release. A probe only runs after
            // validation passes, so a recorded `defer_state` already implies a prior pass.
            if defer_state.is_none() {
                // Once one check this tick found TMDB unreachable, don't spend another full retry
                // cycle on each remaining record: they wait for the next tick too.
                let check = if tmdb_down {
                    TitleCheck::Unavailable
                } else {
                    // The whole path: a file with no episode code may take its season from a
                    // directory, as the VFS places it.
                    self.validator
                        .validate(
                            &selected_path,
                            rec.request.tmdb_id,
                            rec.request.kind,
                            rec.request.season,
                            rec.request.episode,
                        )
                        .await
                };
                if check == TitleCheck::Match {
                    // Passed → clear any prior mismatch count and fall through to the probe.
                    self.validate_fails.lock().await.remove(hash);
                } else if check == TitleCheck::Unavailable {
                    // No verdict (TMDB outage / rejected key): no strike, nothing blacklisted. The
                    // record stays Pending and is checked again next tick, however long TMDB is down.
                    tmdb_down = true;
                    debug!(
                        "observe: tmdb {} hash {} title check unavailable (TMDB unreachable) — re-checking next tick",
                        rec.request.tmdb_id, hash
                    );
                    continue;
                } else {
                    // Only blacklist+delete after TWO CONSECUTIVE mismatches (a single-tick glitch —
                    // incl. the first observe after a restart — rides out); a genuine WrongTitle is
                    // rejected on the next tick. The record stays Pending meanwhile (a title with no
                    // managed slot still shows it, through the VFS fallback; the scan loop drops a
                    // torrent from the listing in the tick `observe` deletes it). The strike count
                    // is kept until the delete succeeds, so a failed one is retried next tick.
                    let fails = {
                        let mut m = self.validate_fails.lock().await;
                        let c = m.entry(hash.to_string()).or_insert(0);
                        *c += 1;
                        *c
                    };
                    if fails >= 2 {
                        if self
                            .reject(
                                hash,
                                ids,
                                &rec.request,
                                "WrongTitle",
                                &rec.provenance,
                                &mut deleted,
                            )
                            .await
                        {
                            self.validate_fails.lock().await.remove(hash);
                        }
                    } else {
                        debug!(
                            "observe: tmdb {} hash {} title-validation mismatch ({}/2) — re-checking next tick",
                            rec.request.tmdb_id, hash, fails
                        );
                    }
                    continue;
                }
            }
            let locator = locator_for(&info, hash, &selected_path);
            match self.verify_file(&locator, &rec.request).await {
                verdict @ (VerifyResult::Pass | VerifyResult::Accept) => {
                    // Acquisition policy is unchanged — an unverifiable release (`Accept`: unreadable
                    // container / no tracks) is still accepted when nothing better was found — but it
                    // is FLAGGED so the upgrade pass replaces it with a checked copy. So is a `Pass`
                    // that had no audio language to check (a re-acquire whose title languages could
                    // not be looked up, see `fill_missing_languages`).
                    let unverified = matches!(verdict, VerifyResult::Accept)
                        || self.audio_unchecked(&rec.request);
                    if self
                        .record_verified(hash, &rec.request, &info, &selected_path, unverified)
                        .await
                    {
                        info!(
                            "observe: tmdb {} hash {} verified ({}{})",
                            rec.request.tmdb_id,
                            hash,
                            file_name,
                            if unverified { ", tracks unchecked" } else { "" }
                        );
                        self.verify_attempts.lock().await.remove(hash);
                        self.progress.lock().await.remove(&t.id);
                    }
                }
                VerifyResult::Defer => {
                    // Record the deferred attempt + when we probed (drives the back-off gate above).
                    // Crucially we do NOT accept-unverified: the record stays Pending (so the upgrade
                    // engine's Verified gate skips it and the VFS keeps the prior selection), is
                    // re-probed on the back-off, and is replaced only by a re-probe that still defers
                    // past the deadline. This prevents freezing a wrong-language release into the
                    // library.
                    let now = now_secs();
                    let mut m = self.verify_attempts.lock().await;
                    let st = m.entry(hash.to_string()).or_insert(DeferState {
                        attempts: 0,
                        last_probe_at: now,
                        first_defer_at: now,
                    });
                    st.attempts += 1;
                    st.last_probe_at = now;
                    let (attempts, first_defer_at) = (st.attempts, st.first_defer_at);
                    drop(m);
                    // Hard deadline: still unfetchable past `VERIFY_DEADLINE_SECS` since it was added,
                    // and deferring for at least `VERIFY_DEADLINE_MIN_DEFER` itself → effectively
                    // broken; replace it (blacklist + re-acquire), bounded by the blacklist so it
                    // can't churn.
                    if attempts >= MAX_VERIFY_ATTEMPTS
                        && now.saturating_sub(rec.added_at) > VERIFY_DEADLINE_SECS
                        && now.saturating_sub(first_defer_at) >= VERIFY_DEADLINE_MIN_DEFER.as_secs()
                    {
                        self.reject(
                            hash,
                            ids,
                            &rec.request,
                            "ProbeUnfetchable",
                            &rec.provenance,
                            &mut deleted,
                        )
                        .await;
                    } else {
                        debug!(
                            "observe: tmdb {} hash {} probe deferred (attempt {}) — CDN transient, will re-probe",
                            rec.request.tmdb_id, hash, attempts
                        );
                    }
                }
                VerifyResult::Reject(reason) => {
                    self.reject(
                        hash,
                        ids,
                        &rec.request,
                        reason,
                        &rec.provenance,
                        &mut deleted,
                    )
                    .await;
                }
            }
        }

        // Bound the in-memory maps to live torrents / owned hashes (avoid unbounded growth
        // when torrents disappear from the listing).
        let live_ids: std::collections::HashSet<&str> =
            torrents.iter().map(|t| t.id.as_str()).collect();
        self.progress
            .lock()
            .await
            .retain(|tid, _| live_ids.contains(tid.as_str()));
        // Same id-keyed bound for the info-fetch failure timer (dropped when a torrent leaves the
        // listing, so it can't grow unbounded as ids churn).
        self.info_fail_since
            .lock()
            .await
            .retain(|tid, _| live_ids.contains(tid.as_str()));
        let owned_hashes: std::collections::HashSet<&str> =
            owned.iter().map(|(h, _)| h.as_str()).collect();
        self.verify_attempts
            .lock()
            .await
            .retain(|h, _| owned_hashes.contains(h.as_str()));
        // Bound `validate_fails` to owned hashes too — a stale per-hash count from a prior lifecycle
        // would otherwise let a single transient validate blip on a re-acquired hash trip the
        // two-strikes reap on its first tick (see `observe_prunes_stale_validate_fails_for_unowned_hash`).
        self.validate_fails
            .lock()
            .await
            .retain(|h, _| owned_hashes.contains(h.as_str()));
        deleted
    }

    async fn is_stalled(&self, torrent_id: &str, progress: f64) -> bool {
        let mut map = self.progress.lock().await;
        let entry = map
            .entry(torrent_id.to_string())
            .or_insert((progress, Instant::now()));
        if (progress - entry.0).abs() > f64::EPSILON {
            *entry = (progress, Instant::now()); // progressed — reset
            false
        } else {
            entry.1.elapsed() >= self.stall_timeout
        }
    }

    /// [`Self::fail_and_reacquire`] for `observe`, noting in `deleted` a hash whose listed torrents
    /// it deleted. Returns whether the rejection went through (every delete succeeded).
    async fn reject(
        &self,
        hash: &str,
        torrent_ids: &[&str],
        req: &AcquireRequest,
        reason: &str,
        provenance: &Provenance,
        deleted: &mut std::collections::HashSet<String>,
    ) -> bool {
        let removed = self
            .fail_and_reacquire(hash, torrent_ids, req, reason, provenance)
            .await;
        if removed && !torrent_ids.is_empty() {
            deleted.insert(hash.to_ascii_lowercase());
        }
        removed
    }

    /// Reject the owned `hash`: delete it from the provider under every listing id in
    /// `torrent_ids` (empty = absent from the listing, nothing to delete), then blacklist it, drop
    /// its records and re-acquire `req` with the failed record's (sticky) `provenance`. Returns
    /// `false` when a delete failed: then nothing else happens, and the state that led to the
    /// rejection (the stall timer, the info-failure timer, the probe back-off) is kept, so the next
    /// `observe` that sees the hash in that state rejects it again and retries the delete.
    async fn fail_and_reacquire(
        &self,
        hash: &str,
        torrent_ids: &[&str],
        req: &AcquireRequest,
        reason: &str,
        provenance: &Provenance,
    ) -> bool {
        warn!(
            "owned torrent {} failed ({}) — blacklist + re-acquire",
            hash, reason
        );
        // Delete the provider torrent BEFORE dropping the store records (the `execute_remove`
        // ordering), under every listing id of the hash (a retried add or a manual re-add can list
        // it twice). If any delete fails, KEEP the records so a later `observe` retries the delete
        // rather than leaving a present-but-untracked torrent behind; the kept record is what stops
        // `record_mirror_owned` re-adopting it (the mirror never adopts an owned hash).
        let mut removed = true;
        for id in torrent_ids {
            if let Err(e) = self.provider.delete_torrent(id).await {
                warn!(
                    "observe: delete_torrent {} ({}) failed: {}; the delete is retried on a later tick",
                    id, hash, e
                );
                removed = false;
            }
        }
        if !removed {
            return false;
        }
        for id in torrent_ids {
            self.progress.lock().await.remove(*id);
            self.info_fail_since.lock().await.remove(*id);
        }
        self.verify_attempts.lock().await.remove(hash);
        // Blacklist only once the torrent is gone: the entry records a completed removal, and until
        // then the kept record (not the blacklist) is what stops the mirror re-adopting the torrent.
        // A kept `Pending` record may also still settle `Verified` (a stalled download that resumes,
        // a probe that recovers); for an empty-provenance one (a dead mirror movie's re-acquire
        // passes its empty provenance on) a non-"Dead" entry written before the failed delete would
        // let `record_mirror_owned`'s phantom reaper drop that present, playing copy.
        let _ = self
            .store
            .blacklist_add(req.kind, req.tmdb_id, hash.to_string(), reason, now_secs())
            .await;
        // Drop the owned record, its authoritative id and every selection slot naming the hash (so
        // the VFS stops showing the dead release), in one transaction: a failure leaves all three
        // in place, never a slot serving a forgotten record. The torrent is already gone, so a kept
        // record is reaped again on a later tick.
        if let Err(e) = self.store.forget_owned(hash.to_string()).await {
            warn!("observe: dropping the records of reaped {hash} failed: {e}");
        }
        // Re-acquire ONLY once the dead record was actually cleared (a failed delete returned above
        // with the record kept for the retry): adding a replacement then would leave the dead hash
        // present alongside a second torrent for the title, re-scraped on every stuck tick until the
        // delete succeeds. Sticky provenance: re-acquire preserves the failed record's origin
        // (Trigger B correctness).
        //
        // SCOPE for a multi-episode pack: `req` carries only the single originally-requested
        // (season, episode), so this immediately restores that ONE episode. The pack's OTHER
        // episodes (whose selection slots were cleared above) are left un-selected here — but the
        // pack is only reaped when the provider actually lost it, so those episodes were already
        // gone. `monitor_episodes` re-acquires the full missing set (owned-episode coverage via the
        // `provides` union) on its next cycle, so the gap is bounded latency, not lost content. A
        // show record naming no episode (an account-mirror season pack) is left to it entirely:
        // there is no single episode to restore (see `acquire`).
        if requests_no_episode(req) {
            debug!(
                "observe: tmdb {} reaped {} names no episode; leaving re-acquisition to monitor_episodes",
                req.tmdb_id, hash
            );
            return true;
        }
        let mut req = req.clone();
        self.fill_missing_languages(&mut req).await;
        let _ = self.acquire(req, provenance.clone()).await; // promotes the next candidate (bad hash now blacklisted)
        true
    }

    /// Complete a stored request recorded without the title's TMDB languages (an account-mirror
    /// record: `spoken_languages` is `None` until the upgrade pass backfills it) before it is
    /// re-acquired. Under `AUDIO_LANGUAGE=original` ranking resolves the audio requirement against
    /// them and `observe` probes against them; with none, a dub could rank first and pass a probe
    /// that checks no language. The languages are persisted on the replacement's record. A failed
    /// lookup leaves the request as it is: `observe` then records the replacement's probe `Pass` as
    /// unchecked (see [`Self::audio_unchecked`]), so the upgrade pass replaces it with a checked copy.
    async fn fill_missing_languages(&self, req: &mut AcquireRequest) {
        if self.prefs.audio != crate::config::AudioReq::Original || req.spoken_languages.is_some() {
            return;
        }
        match self.validator.title_languages(req.tmdb_id, req.kind).await {
            Some(langs) => {
                debug!(
                    "observe: tmdb {} languages looked up for the re-acquire (original {:?}, spoken {:?})",
                    req.tmdb_id, langs.original_language, langs.spoken_languages
                );
                if req.original_language.is_none() {
                    req.original_language = langs.original_language;
                }
                req.spoken_languages = Some(langs.spoken_languages);
            }
            None => debug!(
                "observe: tmdb {} languages unknown; re-acquiring with the recorded ones",
                req.tmdb_id
            ),
        }
    }

    /// Whether a probe of a file for `req` checked no audio language at all: under
    /// `AUDIO_LANGUAGE=original`, with the title's languages never fetched and no original language
    /// recorded, the wanted set is empty and `probe::verify` skips the audio check, so its `Pass`
    /// proves nothing about the audio. The upgrade engine asks the same before it stages or adopts
    /// anything (`upgrade::staging_probe`).
    pub(crate) fn audio_unchecked(&self, req: &AcquireRequest) -> bool {
        self.prefs.audio == crate::config::AudioReq::Original
            && req.spoken_languages.is_none()
            && crate::config::original_audio_languages(req.original_language.as_deref(), &[])
                .is_empty()
    }

    /// After adding a NEW hash for `req`, reap a DIFFERENT owned record for this EXACT slot that has
    /// lapsed off the provider (absent from `present`). The reaped record is any NON-manual record
    /// for the exact slot — engine-provenance OR an empty-provenance account-mirror copy (the guard
    /// below is `!has_manual_entry()`, not empty-only). `dedup`/`observe` ignore an absent hash, and
    /// the only other absent-record reaper (`reap_absent_mirror_owned`) is empty-provenance-only and
    /// daily — so an engine-provenance lapse would otherwise linger as a `Verified`-but-absent zombie
    /// until the daily upgrade or Trigger-B, while a mirror lapse is merely cleaned sooner here than
    /// on the next daily prune. Store-only — the torrent is already gone, so there is no provider delete and no
    /// stream to interrupt (hence no idle-gate). Conservative to avoid ever reaping a live record:
    /// only an EXACT slot supersede (a same-tmdb movie, or a singleton episode == the requested one)
    /// and only a NON-manual record (invariant M-1) is reaped; a pack covering other episodes, any
    /// still-present copy, and `new_hash` itself are all kept. `present` must be a confirmed,
    /// non-empty listing (the caller guards off an unknown/empty one, like `observe`).
    async fn reap_lapsed_superseded(
        &self,
        req: &AcquireRequest,
        new_hash: &str,
        present: &std::collections::HashSet<String>,
    ) {
        for (hash, rec) in self.store.all_owned().await {
            if hash.eq_ignore_ascii_case(new_hash) {
                continue;
            }
            if rec.request.tmdb_id != req.tmdb_id || rec.request.kind != req.kind {
                continue;
            }
            if rec.provenance.has_manual_entry() {
                continue; // never auto-remove manual content (M-1)
            }
            let same_slot = match (req.kind, req.season, req.episode) {
                (MediaKind::Series, Some(s), Some(e)) => {
                    rec.provides.len() == 1 && rec.provides[0] == (s, e)
                }
                (MediaKind::Movie, _, _) => rec.provides.is_empty(),
                _ => false,
            };
            if !same_slot {
                continue;
            }
            if present.contains(&hash.to_ascii_lowercase()) {
                continue; // still present on the provider → keep
            }
            debug!(
                "acquire: tmdb {} — reaping lapsed superseded owned {} (replaced by {})",
                req.tmdb_id, hash, new_hash
            );
            // One transaction for the record, its authoritative id and its slots (`forget_owned`).
            if let Err(e) = self.store.forget_owned(hash.clone()).await {
                warn!(
                    "acquire: tmdb {} — dropping the records of lapsed {} failed: {}",
                    req.tmdb_id, hash, e
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AudioReq, SubReq};
    use crate::provider::MockProvider;
    use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo as TI};
    use crate::release::RawCandidate;
    use crate::scraper::MockScraper;
    use crate::store::Provenance;
    use crate::vfs::{MediaMetadata, MediaType};

    #[test]
    fn select_target_picks_largest_so_acquire_and_observe_agree() {
        // `acquire` and `observe` must select the SAME movie file. `select_target` takes no hint
        // and picks the largest video (the feature), so both paths agree by construction. A
        // hint/index-driven selector (the old behaviour) could pick a smaller SAMPLE, leaving the
        // largest — which observe records/probes — unselected → a broken locator → a stuck Pending.
        let info = TI {
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "Movie.2020.SAMPLE.mkv".into(),
                    bytes: 50_000_000,
                    selected: 1,
                },
                TorrentFile {
                    id: 2,
                    path: "Movie.2020.1080p.x265.mkv".into(),
                    bytes: 8_000_000_000,
                    selected: 1,
                },
            ],
            ..Default::default()
        };
        // The feature (largest video) is always chosen — never the smaller sample.
        assert_eq!(select_target(&info).unwrap().id, 2);
    }

    #[test]
    fn count_feature_videos_excludes_sub_floor_sample() {
        // The ~700MB `FEATURE_MIN_BYTES` floor distinguishes a real feature from a bundled sample.
        // The sub-floor file here is a VALID video (a neutral name, so `is_video_file` accepts it —
        // a name like "sample.mkv" would be rejected by `is_video_file` regardless of size, which
        // would NOT exercise the floor): it is excluded purely because it falls below the byte floor,
        // so a feature + a small clip reads as ONE feature (not a multi-movie pack).
        let info = TI {
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "Movie.2020.1080p.BluRay.mkv".into(),
                    bytes: 8_000_000_000, // > 700MB → a feature
                    selected: 1,
                },
                TorrentFile {
                    id: 2,
                    path: "Movie.2020.720p.WEBRip.mkv".into(),
                    bytes: 50_000_000, // < 700MB → below the floor, excluded
                    selected: 1,
                },
            ],
            ..Default::default()
        };
        assert_eq!(count_feature_videos(&info), 1);
    }

    #[test]
    fn count_feature_videos_counts_two_features_as_a_pack() {
        // Two >=floor videos = a multi-movie pack → the guard must see count == 2 so a movie request
        // never silently acquires a bundle of features (the reject gated on this floor in both
        // `acquire` and `upgrade`).
        let info = TI {
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "Movie.A.2020.1080p.mkv".into(),
                    bytes: 4_000_000_000, // > 700MB → a feature
                    selected: 1,
                },
                TorrentFile {
                    id: 2,
                    path: "Movie.B.2021.1080p.mkv".into(),
                    bytes: 5_000_000_000, // > 700MB → a second feature
                    selected: 1,
                },
            ],
            ..Default::default()
        };
        assert_eq!(count_feature_videos(&info), 2);
    }

    #[test]
    fn episode_files_extracts_dash_concatenated_pack() {
        // A pre-existing absolute/concatenated-coded season pack must report its episodes (so the
        // reconciler doesn't re-acquire them as per-episode duplicates).
        let info = TI {
            hash: "h".into(),
            files: vec![
                TorrentFile {
                    id: 0,
                    path: "Rick and Morty - 409 - Childrick of Mort.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                },
                TorrentFile {
                    id: 1,
                    path: "Rick and Morty - 410 - Star Mort.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                },
            ],
            ..Default::default()
        };
        let mut eps: Vec<(u32, u32)> = episode_files(&info)
            .into_iter()
            .map(|(s, e, _)| (s, e))
            .collect();
        eps.sort_unstable();
        assert_eq!(eps, vec![(4, 9), (4, 10)]);
    }

    #[test]
    fn episode_files_reports_all_episodes_of_a_multi_episode_file() {
        // A single file covering two episodes must report BOTH (and both point to that file), so the
        // reconciler sees the second episode as owned instead of re-acquiring it as a duplicate.
        let info = TI {
            hash: "h".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Show.S01E01E02.1080p.mkv".into(),
                bytes: 2_000_000_000,
                selected: 1,
            }],
            ..Default::default()
        };
        let eps = episode_files(&info);
        assert_eq!(
            eps,
            vec![
                (1, 1, "Show.S01E01E02.1080p.mkv".to_string()),
                (1, 2, "Show.S01E01E02.1080p.mkv".to_string()),
            ]
        );
    }

    #[test]
    fn episode_files_keeps_the_first_file_of_each_episode() {
        // A pack holding one episode twice (language folders, two resolutions) yields it once, from
        // its first file: the one `observe` title-checks and probes. A multi-episode file still
        // yields every episode it carries.
        let file = |id, path: &str| TorrentFile {
            id,
            path: path.into(),
            bytes: 2_000_000_000,
            selected: 1,
        };
        let info = TI {
            hash: "h".into(),
            files: vec![
                file(0, "A/Show.S01E01.mkv"),
                file(1, "B/Show.S01E01.mkv"),
                file(2, "A/Show.S01E02E03.mkv"),
                file(3, "B/Show.S01E03.mkv"),
            ],
            ..Default::default()
        };
        assert_eq!(
            episode_files(&info),
            vec![
                (1, 1, "A/Show.S01E01.mkv".to_string()),
                (1, 2, "A/Show.S01E02E03.mkv".to_string()),
                (1, 3, "A/Show.S01E02E03.mkv".to_string()),
            ]
        );
    }

    #[test]
    fn locator_for_pairs_link_by_selected_position() {
        // The per-file CDN link is paired by position among the SELECTED files only — an unselected
        // file must NOT advance the link index, or playback resolves the wrong file's bytes.
        let info = TI {
            id: "tid".into(),
            hash: "h".into(),
            files: vec![
                TorrentFile {
                    id: 9,
                    path: "Unselected.mkv".into(),
                    bytes: 1,
                    selected: 0,
                },
                TorrentFile {
                    id: 5,
                    path: "A.mkv".into(),
                    bytes: 1,
                    selected: 1,
                },
                TorrentFile {
                    id: 6,
                    path: "B.mkv".into(),
                    bytes: 1,
                    selected: 1,
                },
            ],
            links: vec!["urlA".into(), "urlB".into()],
            ..Default::default()
        };
        // A is selected-position 0 (the unselected file does not consume a link slot).
        let a = locator_for(&info, "h", "A.mkv");
        assert_eq!(a.file_id, 5);
        assert_eq!(a.link.as_deref(), Some("urlA"));
        // B is selected-position 1, NOT 2 — only selected files advance the index.
        let b = locator_for(&info, "h", "B.mkv");
        assert_eq!(b.file_id, 6);
        assert_eq!(b.link.as_deref(), Some("urlB"));
        // A path that matches nothing → a safe default (file_id 0, no link).
        let missing = locator_for(&info, "h", "Nope.mkv");
        assert_eq!(missing.file_id, 0);
        assert_eq!(missing.link, None);
        // A provider with no per-file links (TorBox) → link None, but the file_id is still correct.
        let no_links = TI {
            links: vec![],
            ..info.clone()
        };
        let tb = locator_for(&no_links, "h", "B.mkv");
        assert_eq!(tb.file_id, 6);
        assert_eq!(tb.link, None);
    }

    fn store() -> Store {
        Store::in_memory()
    }
    fn prefs() -> QualityPrefs {
        QualityPrefs {
            max_resolution: crate::config::MaxResolution::P1080,
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            prefer_hevc: true,
            prefer_hdr: false,
        }
    }
    fn meta() -> MediaMetadata {
        MediaMetadata {
            title: "Movie".into(),
            year: Some("2023".into()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:27205".into()),
        }
    }
    fn req() -> AcquireRequest {
        AcquireRequest {
            imdb_id: "tt1".into(),
            tmdb_id: 27205,
            kind: MediaKind::Movie,
            season: None,
            episode: None,
            original_language: Some("eng".into()),
            spoken_languages: Some(vec![]),
            metadata: meta(),
        }
    }
    fn cand(hash: &str, cached: bool) -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: format!("Movie.2023.1080p.x265{}", if cached { "\nRD+" } else { "" }),
            info_hash: hash.into(),
            file_idx: Some(0),
            file_name: Some("Movie.2023.1080p.x265.mkv".into()),
        }
    }
    /// Like [`provider_returning`] for a FRESH add: the account listing does not hold the hash yet
    /// (`acquire` never adds a hash already in the account that the title doesn't own).
    fn provider_adding(status: &str, hash: &str) -> Arc<dyn DebridProvider> {
        Arc::new(MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: format!("tid_{hash}"),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: format!("tid_{hash}"),
                hash: hash.into(),
                status: status.into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Movie.2023.1080p.x265.mkv".into(),
                    bytes: 10,
                    selected: 1,
                }],
                links: vec!["https://cdn/file".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/file".into()),
            ..Default::default()
        })
    }
    fn provider_returning(status: &str, hash: &str) -> Arc<dyn DebridProvider> {
        Arc::new(MockProvider {
            // List the torrent so `get_torrents()` reports the hash as PRESENT — an owned+present
            // title short-circuits as idempotent, whereas an owned-but-absent (lapsed) one re-adds.
            torrents: vec![torrent(&format!("tid_{hash}"), hash, status, 100.0)],
            add_magnet: Some(AddMagnetResponse {
                id: format!("tid_{hash}"),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: format!("tid_{hash}"),
                hash: hash.into(),
                status: status.into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Movie.2023.1080p.x265.mkv".into(),
                    bytes: 10,
                    selected: 1,
                }],
                links: vec!["https://cdn/file".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/file".into()),
            ..Default::default()
        })
    }

    /// `true` → `Match`, `false` → `Mismatch`.
    struct OkValidator(bool);
    #[async_trait]
    impl TitleValidator for OkValidator {
        async fn validate(
            &self,
            _f: &str,
            _t: u64,
            _k: MediaKind,
            _s: Option<u32>,
            _e: Option<u32>,
        ) -> TitleCheck {
            if self.0 {
                TitleCheck::Match
            } else {
                TitleCheck::Mismatch
            }
        }
    }
    /// Validator that returns `Mismatch` for its first `fail_first` calls, then `Match` — a one-tick
    /// mismatch glitch, then recovery.
    struct FailThenPassValidator {
        fail_first: u32,
        calls: std::sync::atomic::AtomicU32,
    }
    #[async_trait]
    impl TitleValidator for FailThenPassValidator {
        async fn validate(
            &self,
            _f: &str,
            _t: u64,
            _k: MediaKind,
            _s: Option<u32>,
            _e: Option<u32>,
        ) -> TitleCheck {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= self.fail_first {
                TitleCheck::Match
            } else {
                TitleCheck::Mismatch
            }
        }
    }
    /// Validator for a TMDB outage: every check is `Unavailable`. Counts its calls.
    #[derive(Default)]
    struct UnavailableValidator {
        calls: std::sync::atomic::AtomicU32,
    }
    #[async_trait]
    impl TitleValidator for UnavailableValidator {
        async fn validate(
            &self,
            _f: &str,
            _t: u64,
            _k: MediaKind,
            _s: Option<u32>,
            _e: Option<u32>,
        ) -> TitleCheck {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            TitleCheck::Unavailable
        }
    }
    struct CannedProber(Result<Vec<Track>, ProbeError>);
    #[async_trait]
    impl Prober for CannedProber {
        async fn probe(&self, _url: &str) -> Result<Vec<Track>, ProbeError> {
            self.0.clone()
        }
    }
    /// Records every URL it probes, then passes.
    #[derive(Default)]
    struct RecordingProber {
        urls: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Prober for RecordingProber {
        async fn probe(&self, url: &str) -> Result<Vec<Track>, ProbeError> {
            self.urls.lock().unwrap().push(url.to_string());
            Ok(vec![])
        }
    }
    /// Hash-aware provider for end-to-end re-acquire tests: `add_magnet` derives the torrent id from
    /// the magnet's infohash and `get_torrent_info` returns a downloaded single-file movie for that
    /// hash — so a blacklist→re-scrape→add-next-candidate round-trip resolves a DIFFERENT hash than
    /// the original (unlike `provider_returning`, which is pinned to one hash).
    #[derive(Debug, Default)]
    struct HashAwareProvider {
        deleted: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait]
    impl DebridProvider for HashAwareProvider {
        fn name(&self) -> &'static str {
            "hashaware"
        }
        async fn get_torrents(&self) -> Result<Vec<crate::rd_client::Torrent>, reqwest::Error> {
            Ok(vec![])
        }
        async fn get_torrent_info(&self, id: &str) -> Result<TI, reqwest::Error> {
            let hash = id.strip_prefix("tid_").unwrap_or(id).to_string();
            Ok(TI {
                id: id.to_string(),
                hash,
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Movie.2023.1080p.x265.mkv".into(),
                    bytes: 10,
                    selected: 1,
                }],
                links: vec!["https://cdn/file".into()],
                ..Default::default()
            })
        }
        async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
            let hash = magnet.rsplit("btih:").next().unwrap_or("").to_string();
            Ok(AddMagnetResponse {
                id: format!("tid_{hash}"),
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
            Ok("https://cdn/file".into())
        }
        async fn invalidate(&self, _l: &crate::provider::FileLocator) {}
    }

    fn engine(
        provider: Arc<dyn DebridProvider>,
        scraper: Arc<dyn Scraper>,
        validator: Arc<dyn TitleValidator>,
        prober: Arc<dyn Prober>,
        store: Store,
    ) -> AcquisitionEngine {
        AcquisitionEngine::new(
            provider,
            scraper,
            validator,
            prober,
            store,
            prefs(),
            5,
            Duration::from_secs(1800),
            Duration::from_secs(600),
        )
    }

    fn engine_dead(
        provider: Arc<dyn DebridProvider>,
        scraper: Arc<dyn Scraper>,
        validator: Arc<dyn TitleValidator>,
        prober: Arc<dyn Prober>,
        store: Store,
        dead_secs: u64,
    ) -> AcquisitionEngine {
        AcquisitionEngine::new(
            provider,
            scraper,
            validator,
            prober,
            store,
            prefs(),
            5,
            Duration::from_secs(1800),
            Duration::from_secs(dead_secs),
        )
    }

    fn torrent(id: &str, hash: &str, status: &str, progress: f64) -> crate::rd_client::Torrent {
        crate::rd_client::Torrent {
            id: id.into(),
            hash: hash.into(),
            status: status.into(),
            progress,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn acquire_records_pending_and_quality_optimistically() {
        let st = store();
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand("h1", true)],
        });
        let eng = engine(
            provider_adding("downloaded", "h1"),
            scraper,
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let out = eng.acquire(req(), Provenance::watchlist("alice")).await;
        assert_eq!(
            out,
            AcquireOutcome::Pending("h1".into()),
            "acquire is optimistic: always Pending"
        );
        let rec = st.get_owned("h1".into()).await.unwrap();
        assert_eq!(rec.status, OwnedStatus::Pending);
        assert_eq!(rec.provenance, Provenance::watchlist("alice"));
        assert!(
            rec.quality.unwrap().cached,
            "cached candidate's quality recorded"
        );
        assert!(
            st.all_staging_intents().await.is_empty(),
            "a recorded add leaves no staging intent"
        );
        assert_eq!(
            st.authoritative_meta("h1".into())
                .await
                .unwrap()
                .external_id
                .as_deref(),
            Some("tmdb:27205")
        );
    }

    /// Two cached candidates: `h1` (REMUX, ranked first) and `h2` (WEB).
    fn two_ranked_candidates() -> Vec<RawCandidate> {
        vec![
            RawCandidate {
                name: "Torrentio\n1080p".into(),
                description: "Movie.2023.1080p.REMUX.x265\nRD+".into(),
                info_hash: "h1".into(),
                file_idx: Some(0),
                file_name: Some("Movie.2023.1080p.REMUX.mkv".into()),
            },
            RawCandidate {
                name: "Torrentio\n1080p".into(),
                description: "Movie.2023.1080p.WEB.x265\nRD+".into(),
                info_hash: "h2".into(),
                file_idx: Some(0),
                file_name: Some("Movie.2023.1080p.WEB.mkv".into()),
            },
        ]
    }

    #[tokio::test]
    async fn acquire_never_adds_an_untracked_torrent_already_in_the_account() {
        // The top candidate is already in the account but untracked (a still-downloading manual
        // add, an unidentified torrent). On TorBox an add of it returns the EXISTING torrent's id,
        // so the engine would adopt — and on any rejection delete — a torrent it never added.
        let st = store();
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_user", "h1", "downloading", 40.0)],
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h2".into(),
                uri: String::new(),
            }),
            added: added.clone(),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: two_ranked_candidates(),
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let out = eng.acquire(req(), Provenance::watchlist("alice")).await;
        assert_eq!(out, AcquireOutcome::Pending("h2".into()));
        assert_eq!(
            *added.lock().unwrap(),
            vec!["h2".to_string()],
            "the untracked torrent's hash is never added"
        );
        assert!(st.get_owned("h1".into()).await.is_none());
    }

    #[tokio::test]
    async fn acquire_adds_nothing_when_the_listing_is_unavailable() {
        // Without the listing, presence can't be ruled out, so nothing is added (retry later).
        let st = store();
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            fail_get_torrents: true,
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h1".into(),
                uri: String::new(),
            }),
            added: added.clone(),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: two_ranked_candidates(),
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let out = eng.acquire(req(), Provenance::watchlist("alice")).await;
        assert_eq!(out, AcquireOutcome::TemporarilyUnavailable);
        assert!(added.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn acquire_skips_a_candidate_owned_under_another_title() {
        // The top candidate is owned + present under a DIFFERENT tmdb id (a misidentified mirror
        // copy, say). It must not count as this title being acquired, nor be added over that title's
        // record: the next candidate is added instead.
        let st = store();
        let other = AcquireRequest {
            tmdb_id: 999,
            metadata: MediaMetadata {
                external_id: Some("tmdb:999".into()),
                ..meta()
            },
            ..req()
        };
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: other,
                provenance: Provenance {
                    entries: Vec::new(),
                },
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_h1", "h1", "downloaded", 100.0)],
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h2".into(),
                uri: String::new(),
            }),
            added: added.clone(),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: two_ranked_candidates(),
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let out = eng.acquire(req(), Provenance::watchlist("alice")).await;
        assert_eq!(out, AcquireOutcome::Pending("h2".into()));
        assert_eq!(*added.lock().unwrap(), vec!["h2".to_string()]);
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().request.tmdb_id,
            999,
            "the other title's record is untouched"
        );
    }

    #[tokio::test]
    async fn acquire_is_idempotent_on_an_owned_copy_ranked_below_the_add_attempts() {
        // With MAX_ACQUIRE_ATTEMPTS=1 only the top candidate may be added, but an owned + present
        // copy ranked second still means the title is acquired: nothing is added.
        let st = store();
        st.put_owned(
            "h2".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("alice"),
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_h2", "h2", "downloaded", 100.0)],
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h1".into(),
                uri: String::new(),
            }),
            added: added.clone(),
            ..Default::default()
        });
        let eng = AcquisitionEngine::new(
            provider,
            Arc::new(MockScraper {
                candidates: two_ranked_candidates(),
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            prefs(),
            1,
            Duration::from_secs(1800),
            Duration::from_secs(600),
        );
        let out = eng.acquire(req(), Provenance::watchlist("alice")).await;
        assert_eq!(out, AcquireOutcome::Acquired("h2".into()));
        assert!(added.lock().unwrap().is_empty(), "no duplicate is added");
    }

    #[tokio::test]
    async fn acquire_rolls_back_an_add_whose_owned_record_cannot_be_written() {
        // The add succeeded but the Pending record couldn't be written: the torrent would sit in the
        // account untracked — skipped by every later acquire as "already in the account", then
        // adopted by the account mirror with no provenance (never removed, never title-checked or
        // probed). Delete it (it was absent before the add, so the id is ours) and retry later.
        // The staging intent and its torrent id are written; every write after them fails.
        for fail_delete in [false, true] {
            let (st, switch) = crate::store::fail_writes::fail_after_arm_store();
            let per_commit = crate::store::fail_writes::syncs_per_commit(&st, &switch).await;
            let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
            let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
                add_magnet: Some(AddMagnetResponse {
                    id: "tid_h1".into(),
                    uri: String::new(),
                }),
                deleted: deleted.clone(),
                fail_delete,
                ..Default::default()
            });
            let eng = engine(
                provider,
                Arc::new(MockScraper {
                    candidates: vec![cand("h1", true)],
                }),
                Arc::new(OkValidator(true)),
                Arc::new(CannedProber(Ok(vec![]))),
                st,
            );
            switch.arm_after(2, per_commit);
            let out = eng.acquire(req(), Provenance::watchlist("alice")).await;
            assert_eq!(out, AcquireOutcome::TemporarilyUnavailable);
            assert_eq!(
                *deleted.lock().unwrap(),
                vec!["tid_h1".to_string()],
                "the untracked add is deleted"
            );
            switch.disarm();
            let st = switch.reopen();
            assert!(st.get_owned("h1".into()).await.is_none());
            if fail_delete {
                // The torrent is still there, unchecked: its intent stays open, so the account
                // mirror leaves it alone and the staging sweep deletes it.
                assert_eq!(
                    st.all_staging_intents().await,
                    vec![(
                        "h1".to_string(),
                        crate::store::StagingIntent {
                            torrent_id: Some("tid_h1".into()),
                            started_at: st.all_staging_intents().await[0].1.started_at,
                        }
                    )]
                );
            }
        }
    }

    /// A provider whose `add_magnet` never answers (the request is in flight when the job is cut
    /// off); its listing is empty.
    #[derive(Debug, Default)]
    struct HangingAdd;
    #[async_trait]
    impl DebridProvider for HangingAdd {
        fn name(&self) -> &'static str {
            "hanging-add"
        }
        async fn get_torrents(&self) -> Result<Vec<crate::rd_client::Torrent>, reqwest::Error> {
            Ok(vec![])
        }
        async fn get_torrent_info(&self, _id: &str) -> Result<TI, reqwest::Error> {
            Ok(TI::default())
        }
        async fn add_magnet(&self, _magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
            std::future::pending().await
        }
        async fn select_files(&self, _t: &str, _f: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn delete_torrent(&self, _t: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn resolve_url(
            &self,
            _l: &crate::provider::FileLocator,
        ) -> Result<String, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn invalidate(&self, _l: &crate::provider::FileLocator) {}
    }

    /// An acquire cut off while its add is in flight (the job dropped on shutdown, a caught panic)
    /// leaves a staging intent, so the account mirror never adopts the unchecked torrent if the add
    /// landed, and the staging sweep deletes it once it is listed.
    #[tokio::test]
    async fn an_acquire_cut_off_mid_add_leaves_an_intent_the_sweep_resolves() {
        let st = store();
        let eng = engine(
            Arc::new(HangingAdd),
            Arc::new(MockScraper {
                candidates: vec![cand("h1", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let cut = tokio::time::timeout(
            Duration::from_millis(300),
            eng.acquire(req(), Provenance::watchlist("alice")),
        )
        .await;
        assert!(cut.is_err(), "the add never answers, so acquire is cut off");
        assert!(st.get_owned("h1".into()).await.is_none());
        let intents = st.all_staging_intents().await;
        assert_eq!(intents.len(), 1, "{intents:?}");
        assert_eq!(intents[0].0, "h1");
        assert_eq!(intents[0].1.torrent_id, None);

        // The add did land: the mirror leaves it alone, and the next start's sweep deletes it.
        let listing = vec![torrent("tid_h1", "h1", "downloaded", 100.0)];
        let tmdb = crate::tmdb_client::TmdbClient::offline().await;
        let info = downloaded_movie("tid_h1", "h1");
        crate::tasks::record_mirror_owned(&st, &tmdb, &prefs(), &[(info, meta())]).await;
        assert!(st.get_owned("h1".into()).await.is_none(), "not mirrored");
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = MockProvider {
            deleted: deleted.clone(),
            ..Default::default()
        };
        // The next start: this process's note of the intent is gone.
        st.forget_local_staging_intents();
        let swept =
            crate::upgrade::sweep_staging_intents(&st, &provider, &listing, now_secs() + 1).await;
        assert_eq!(
            swept.deleted,
            std::collections::HashSet::from(["h1".to_string()])
        );
        assert_eq!(*deleted.lock().unwrap(), vec!["tid_h1".to_string()]);
        assert!(st.all_staging_intents().await.is_empty());
    }

    #[tokio::test]
    async fn acquire_tries_next_candidate_when_add_magnet_fails() {
        // The top-ranked candidate's add_magnet fails (a provider hiccup / "already queued"); acquire
        // must fall through to the next-best candidate rather than giving up (the add-fail→continue
        // path at the top of the candidate loop).
        let st = store();
        let h1 = RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "Movie.2023.1080p.REMUX.x265\nRD+".into(), // REMUX tier → ranked FIRST
            info_hash: "h1".into(),
            file_idx: Some(0),
            file_name: Some("Movie.2023.1080p.REMUX.mkv".into()),
        };
        let h2 = RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "Movie.2023.1080p.WEB.x265\nRD+".into(),
            info_hash: "h2".into(),
            file_idx: Some(0),
            file_name: Some("Movie.2023.1080p.WEB.mkv".into()),
        };
        let scraper = Arc::new(MockScraper {
            candidates: vec![h1, h2],
        });
        let mut info_by_id = std::collections::HashMap::new();
        info_by_id.insert(
            "h2".to_string(),
            TI {
                id: "h2".into(),
                hash: "h2".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Movie.2023.1080p.WEB.mkv".into(),
                    bytes: 10,
                    selected: 1,
                }],
                links: vec!["https://cdn/file".into()],
                ..Default::default()
            },
        );
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            fail_add_magnet_hashes: ["h1".to_string()].into_iter().collect(),
            torrent_info_by_id: info_by_id,
            ..Default::default()
        });
        let eng = engine(
            provider,
            scraper,
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let out = eng.acquire(req(), Provenance::watchlist("alice")).await;
        assert_eq!(
            out,
            AcquireOutcome::Pending("h2".into()),
            "falls through to the next candidate after the top candidate's add failure"
        );
        assert!(
            st.get_owned("h2".into()).await.is_some(),
            "the second candidate is recorded Pending"
        );
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "the failed candidate is not recorded owned"
        );
        assert!(
            st.all_staging_intents().await.is_empty(),
            "a failed add closes its staging intent, a recorded one too"
        );
    }

    #[tokio::test]
    async fn acquire_idempotent_when_already_owned_and_present() {
        // Owned AND present in the provider listing (provider_returning lists the hash) → idempotent.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper {
                candidates: vec![cand("h1", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert_eq!(
            eng.acquire(req(), Provenance::manual()).await,
            AcquireOutcome::Acquired("h1".into())
        );
        // Idempotent means the existing record is UNTOUCHED — a spurious re-add would overwrite it
        // (reset status to Pending, bump added_at, drop the manual provenance). Assert it's intact.
        let rec = st.get_owned("h1".into()).await.expect("record kept");
        assert_eq!(
            rec.status,
            OwnedStatus::Verified,
            "status must not be reset"
        );
        assert_eq!(rec.added_at, 1, "added_at must not be bumped (no re-add)");
        assert!(
            rec.provenance.has_manual_entry(),
            "the manual provenance must be preserved"
        );
    }

    #[tokio::test]
    async fn acquire_short_circuits_on_a_lower_ranked_owned_present_candidate() {
        // ACQ1 regression: a higher-ranked NON-owned candidate is scraped FIRST, but a lower-ranked
        // candidate is already owned AND present. acquire must short-circuit as Acquired(owned)
        // regardless of rank order — NOT add_magnet the higher-ranked one as a DUPLICATE.
        let st = store();
        st.put_owned(
            "h_owned".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        // h_new ranks higher (BluRay source tier > the default's unknown tier); the provider lists
        // ONLY h_owned as present, not h_new.
        let h_new = RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "Movie.2023.1080p.BluRay.x265\nRD+".into(),
            info_hash: "h_new".into(),
            file_idx: Some(0),
            file_name: Some("Movie.2023.1080p.BluRay.x265.mkv".into()),
        };
        let eng = engine(
            provider_returning("downloaded", "h_owned"),
            Arc::new(MockScraper {
                candidates: vec![h_new, cand("h_owned", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert_eq!(
            eng.acquire(req(), Provenance::manual()).await,
            AcquireOutcome::Acquired("h_owned".into()),
            "must short-circuit on the owned+present candidate, not add the higher-ranked h_new"
        );
        let rec = st.get_owned("h_owned".into()).await.expect("record kept");
        assert_eq!(rec.added_at, 1, "no spurious re-add of the owned record");
    }

    #[tokio::test]
    async fn acquire_reacquires_owned_but_absent_title() {
        // Owned (Verified) record exists, but the torrent is NOT in the provider listing — it lapsed
        // (cache lost / externally deleted). acquire must RE-add it (Pending), not short-circuit as
        // already-acquired (the documented availability re-acquire). Regression guard.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("alice"),
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        // Provider lists NOTHING (h1 absent), but add_magnet/info succeed.
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h1".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: "tid_h1".into(),
                hash: "h1".into(),
                status: "downloaded".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("h1", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert_eq!(
            eng.acquire(req(), Provenance::watchlist("alice")).await,
            AcquireOutcome::Pending("h1".into()),
            "an owned-but-absent title must be re-acquired, not treated as already-acquired"
        );
    }

    #[tokio::test]
    async fn acquire_reaps_a_lapsed_superseded_owned_record() {
        // A watchlist-owned copy hB has lapsed off the provider (absent), while the scrape now tops
        // with a BETTER, un-owned release hA. acquire adds hA (Pending) and must reap hB's
        // now-orphaned record — otherwise it lingers as a Verified-but-absent zombie
        // (`reap_absent_mirror_owned` cleans only empty-provenance mirrors).
        let st = store();
        st.put_owned(
            "hB".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("alice"),
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        // Listing is non-empty (an unrelated torrent) but hB is absent; add/info succeed for hA.
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_other", "other", "downloaded", 100.0)],
            add_magnet: Some(AddMagnetResponse {
                id: "tid_hA".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: "tid_hA".into(),
                hash: "hA".into(),
                status: "downloaded".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        // hA is 1080p (ranks above the 720p hB), both cached — so hA is added and hB is the lapsed
        // owned copy that must be reaped.
        let hb_720 = RawCandidate {
            name: "Torrentio\n720p".into(),
            description: "Movie.2023.720p.x265\nRD+".into(),
            info_hash: "hB".into(),
            file_idx: Some(0),
            file_name: Some("Movie.2023.720p.x265.mkv".into()),
        };
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("hA", true), hb_720],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert_eq!(
            eng.acquire(req(), Provenance::watchlist("alice")).await,
            AcquireOutcome::Pending("hA".into()),
        );
        assert!(
            st.get_owned("hB".into()).await.is_none(),
            "the lapsed superseded owned record must be reaped"
        );
        assert!(
            st.get_owned("hA".into()).await.is_some(),
            "the newly added hash must be recorded Pending"
        );
    }

    #[tokio::test]
    async fn acquire_reaps_a_lapsed_superseded_episode_singleton() {
        // Series analogue of the movie reap: after adding a NEW hash hA for the S01E01 request, a
        // DIFFERENT owned record that is a SINGLETON for that EXACT (season, episode) and has lapsed
        // off the provider (absent) is reaped — while records that are NOT an exact-slot supersede are
        // KEPT: a singleton for a DIFFERENT episode, a PACK covering it plus others, and a
        // still-PRESENT singleton for the same episode.
        let st = store();
        let series_req = AcquireRequest {
            imdb_id: "tt1".into(),
            tmdb_id: 27205,
            kind: MediaKind::Series,
            season: Some(1),
            episode: Some(1),
            original_language: Some("eng".into()),
            spoken_languages: Some(vec![]),
            metadata: MediaMetadata {
                title: "Show".into(),
                year: Some("2023".into()),
                media_type: MediaType::Show,
                external_id: Some("tmdb:27205".into()),
            },
        };
        // hB — lapsed singleton for the requested (1,1): the record that must be reaped.
        st.put_owned(
            "hB".into(),
            OwnedRecord {
                request: series_req.clone(),
                provenance: Provenance::watchlist("alice"),
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![(1, 1)],
                quality: None,
            },
        )
        .await
        .unwrap();
        // hOther — lapsed singleton for a DIFFERENT episode (1,2): wrong slot → KEPT.
        st.put_owned(
            "hOther".into(),
            OwnedRecord {
                request: series_req.clone(),
                provenance: Provenance::watchlist("alice"),
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![(1, 2)],
                quality: None,
            },
        )
        .await
        .unwrap();
        // hPack — lapsed PACK covering (1,1) plus another episode: not a singleton → KEPT.
        st.put_owned(
            "hPack".into(),
            OwnedRecord {
                request: series_req.clone(),
                provenance: Provenance::watchlist("alice"),
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![(1, 1), (1, 2)],
                quality: None,
            },
        )
        .await
        .unwrap();
        // hPresent — singleton for the requested (1,1) but STILL PRESENT on the provider → KEPT.
        st.put_owned(
            "hPresent".into(),
            OwnedRecord {
                request: series_req.clone(),
                provenance: Provenance::watchlist("alice"),
                added_at: 1,
                status: OwnedStatus::Verified,
                provides: vec![(1, 1)],
                quality: None,
            },
        )
        .await
        .unwrap();
        // Listing is non-empty and contains hPresent (so it reads as still present); hB/hOther/hPack
        // are absent. add/info succeed for the newly-added hA.
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_hPresent", "hPresent", "downloaded", 100.0)],
            add_magnet: Some(AddMagnetResponse {
                id: "tid_hA".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: "tid_hA".into(),
                hash: "hA".into(),
                status: "downloaded".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        // hA is 1080p (ranks above the 720p hB), both cached — so hA is added and hB is the lapsed
        // singleton that must be reaped. hB is also the (owned but absent) candidate that triggers the
        // pre-scan's present-listing fetch, exactly as in the movie case.
        let hb_720 = RawCandidate {
            name: "Torrentio\n720p".into(),
            description: "Show.S01E01.720p.x265\nRD+".into(),
            info_hash: "hB".into(),
            file_idx: Some(0),
            file_name: Some("Show.S01E01.720p.x265.mkv".into()),
        };
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("hA", true), hb_720],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert_eq!(
            eng.acquire(series_req.clone(), Provenance::watchlist("alice"))
                .await,
            AcquireOutcome::Pending("hA".into()),
        );
        assert!(
            st.get_owned("hB".into()).await.is_none(),
            "the lapsed singleton for the exact requested episode must be reaped"
        );
        assert!(
            st.get_owned("hOther".into()).await.is_some(),
            "a singleton for a DIFFERENT episode must be kept"
        );
        assert!(
            st.get_owned("hPack".into()).await.is_some(),
            "a pack covering the episode plus others must be kept"
        );
        assert!(
            st.get_owned("hPresent".into()).await.is_some(),
            "a still-present singleton for the same episode must be kept"
        );
        assert!(
            st.get_owned("hA".into()).await.is_some(),
            "the newly added hash must be recorded Pending"
        );
    }

    /// A show request for S01E`episode` of tmdb 27205.
    fn series_req_for(episode: u32) -> AcquireRequest {
        AcquireRequest {
            imdb_id: "tt1".into(),
            tmdb_id: 27205,
            kind: MediaKind::Series,
            season: Some(1),
            episode: Some(episode),
            original_language: Some("eng".into()),
            spoken_languages: Some(vec![]),
            metadata: MediaMetadata {
                title: "Show".into(),
                year: Some("2023".into()),
                media_type: MediaType::Show,
                external_id: Some("tmdb:27205".into()),
            },
        }
    }

    /// Acquire S01E06 when the show's owned season pack `hpack` (with the given status and
    /// `provides`) is present and ranked first (1080p), with a 720p singleton `hs` ranked second.
    /// Returns the outcome, the hashes added, and the store.
    async fn acquire_e06_with_owned_pack(
        status: OwnedStatus,
        provides: Vec<(u32, u32)>,
    ) -> (AcquireOutcome, Vec<String>, Store) {
        let st = store();
        st.put_owned(
            "hpack".into(),
            OwnedRecord {
                request: series_req_for(1),
                provenance: Provenance {
                    entries: Vec::new(),
                },
                added_at: 1,
                status,
                provides,
                quality: None,
            },
        )
        .await
        .unwrap();
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_hpack", "hpack", "downloaded", 100.0)],
            add_magnet: Some(AddMagnetResponse {
                id: "tid_hs".into(),
                uri: String::new(),
            }),
            added: added.clone(),
            ..Default::default()
        });
        let pack = RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: "Show.S01.1080p.BluRay.x265\nRD+".into(),
            info_hash: "hpack".into(),
            file_idx: Some(5),
            file_name: Some("Show.S01E06.1080p.BluRay.x265.mkv".into()),
        };
        let single = RawCandidate {
            name: "Torrentio\n720p".into(),
            description: "Show.S01E06.720p.WEB.x265\nRD+".into(),
            info_hash: "hs".into(),
            file_idx: Some(0),
            file_name: Some("Show.S01E06.720p.WEB.x265.mkv".into()),
        };
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![pack, single],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let out = eng
            .acquire(series_req_for(6), Provenance::watchlist("alice"))
            .await;
        let added = added.lock().unwrap().clone();
        (out, added, st)
    }

    #[tokio::test]
    async fn acquire_adds_an_episode_an_owned_present_pack_does_not_provide() {
        // A present, settled pack of the show that doesn't provide S01E06 (a Real-Debrid pack with
        // only some files selected) is not this episode being acquired. It must not short-circuit
        // as Acquired — the episode would never be added — nor be re-added (on TorBox that adopts
        // the pack over its record); the next candidate is added instead.
        let (out, added, st) = acquire_e06_with_owned_pack(
            OwnedStatus::Verified,
            vec![(1, 1), (1, 2), (1, 3), (1, 4), (1, 5)],
        )
        .await;
        assert_eq!(out, AcquireOutcome::Pending("hs".into()));
        assert_eq!(added, vec!["hs".to_string()], "the pack is never re-added");
        let pack = st.get_owned("hpack".into()).await.expect("pack kept");
        assert_eq!(pack.status, OwnedStatus::Verified);
        assert_eq!(pack.added_at, 1, "the pack's record is untouched");
        assert_eq!(pack.provides.len(), 5);
        assert_eq!(pack.request.episode, Some(1));
    }

    #[tokio::test]
    async fn acquire_is_idempotent_on_a_present_pending_pack_of_the_show() {
        // A Pending pack's `provides` isn't settled yet (it lists only the episode it was added
        // for), so it still counts as acquiring the requested episode: nothing is added.
        let (out, added, _) = acquire_e06_with_owned_pack(OwnedStatus::Pending, vec![(1, 1)]).await;
        assert_eq!(out, AcquireOutcome::Acquired("hpack".into()));
        assert!(added.is_empty());
    }

    #[tokio::test]
    async fn acquire_is_idempotent_on_a_present_pack_with_unknown_coverage() {
        // A settled pack whose file names decode no episode (`provides` empty) may well hold the
        // episode — the VFS still emits its files — so it stays conservative: nothing is added.
        let (out, added, _) = acquire_e06_with_owned_pack(OwnedStatus::Verified, vec![]).await;
        assert_eq!(out, AcquireOutcome::Acquired("hpack".into()));
        assert!(added.is_empty());
    }

    #[tokio::test]
    async fn acquire_returns_temporarily_unavailable_on_scrape_error() {
        // A scraper error (a retryable addon outage) must surface as TemporarilyUnavailable so the
        // reconciler retries next tick, NOT NoAcceptableRelease. MockScraper always succeeds, so this
        // uses a dedicated failing scraper.
        struct FailScraper;
        #[async_trait::async_trait]
        impl Scraper for FailScraper {
            async fn find(
                &self,
                _imdb_id: &str,
                _kind: crate::scraper::MediaKind,
                _season: Option<u32>,
                _episode: Option<u32>,
            ) -> Result<Vec<RawCandidate>, crate::error::AppError> {
                Err(crate::error::AppError::Task("scrape boom".into()))
            }
        }
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(FailScraper),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            store(),
        );
        assert_eq!(
            eng.acquire(req(), Provenance::manual()).await,
            AcquireOutcome::TemporarilyUnavailable
        );
    }

    #[tokio::test]
    async fn acquire_idempotent_when_listing_unavailable() {
        // Owned record exists AND a ranked candidate matches it, but the provider listing FAILS
        // (get_torrents errors → inner `None`). The engine must preserve the old idempotent behaviour
        // (treat the owned record as present) and short-circuit as Acquired WITHOUT re-adding, rather
        // than duplicating the torrent on a transient listing blip. Exercises the whole point of the
        // `Option<Option<HashSet>>` "fetched? / succeeded?" shape.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 7,
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        // Listing fails; add_magnet is left None so a wrongful re-add would NOT yield Acquired("h1").
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            fail_get_torrents: true,
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("h1", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert_eq!(
            eng.acquire(req(), Provenance::manual()).await,
            AcquireOutcome::Acquired("h1".into()),
            "a listing blip must preserve idempotency (treat owned as present), not re-add"
        );
        let rec = st.get_owned("h1".into()).await.expect("record kept");
        assert_eq!(rec.added_at, 7, "no spurious re-add on a listing blip");
        assert!(
            rec.provenance.has_manual_entry(),
            "the manual provenance must be preserved"
        );
    }

    #[tokio::test]
    async fn acquire_no_candidates_is_no_acceptable() {
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            store(),
        );
        assert_eq!(
            eng.acquire(req(), Provenance::manual()).await,
            AcquireOutcome::NoAcceptableRelease
        );
    }

    #[tokio::test]
    async fn observe_deferred_probes_never_accept_unverified() {
        // A recently-added torrent whose probe keeps deferring (CDN momentarily unavailable) must
        // NEVER be marked Verified — accepting it could freeze a wrong-language release into the
        // library. Within the deadline it stays Pending (re-probed on a back-off), full stop.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(), // recent ⇒ well within the verify deadline
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let prober = Arc::new(CannedProber(Err(ProbeError::Transient))); // always defers
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            prober,
            st.clone(),
        );
        let torrents = vec![torrent("tid_h1", "h1", "downloaded", 100.0)];
        for _ in 0..(MAX_VERIFY_ATTEMPTS + 5) {
            eng.observe(&torrents).await;
        }
        let rec = st.get_owned("h1".into()).await.expect("still owned");
        assert_eq!(
            rec.status,
            OwnedStatus::Pending,
            "a perpetually-deferring probe must stay Pending, never accepted unverified"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "and not yet replaced — it is still within the verify deadline"
        );
    }

    #[tokio::test]
    async fn observe_unfetchable_past_deadline_replaces() {
        // A torrent added past the verify deadline whose probe has kept deferring is treated as
        // effectively broken: blacklisted + re-acquired (not accepted, not left forever) — but only
        // once it has been deferring for a while itself. A download that took longer than the
        // deadline must not lose its whole hourly re-probe window to a few minutes of CDN trouble.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 1, // epoch ⇒ far past the verify deadline
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let prober = Arc::new(CannedProber(Err(ProbeError::Transient))); // always defers
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }), // no replacement ⇒ hash blacklisted, record removed
            Arc::new(OkValidator(true)),
            prober,
            st.clone(),
        );
        let torrents = vec![torrent("tid_h1", "h1", "downloaded", 100.0)];
        // Fast burst first (probes every tick while attempts < MAX). The record is past the deadline
        // by `added_at`, but it has only just started deferring: it is kept for the back-off.
        for _ in 0..(MAX_VERIFY_ATTEMPTS + 1) {
            eng.observe(&torrents).await;
        }
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "a release that has only just started deferring keeps its re-probe window"
        );
        assert!(st.get_owned("h1".into()).await.is_some());
        // Once it has been deferring for longer than the floor, the next back-off re-probe that
        // still defers replaces it.
        age_defer_state(&eng, "h1").await;
        eng.observe(&torrents).await;
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "an unfetchable release past the deadline should be blacklisted"
        );
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "and its owned record replaced (removed pending re-acquire)"
        );
    }

    /// Move `hash`'s deferred-probe state back so it has been deferring for longer than
    /// `VERIFY_DEADLINE_MIN_DEFER` and its last probe is past `VERIFY_BACKOFF`.
    async fn age_defer_state(eng: &AcquisitionEngine, hash: &str) {
        let mut m = eng.verify_attempts.lock().await;
        let st = m.get_mut(hash).expect("deferring");
        st.first_defer_at -= VERIFY_DEADLINE_MIN_DEFER.as_secs() + 1;
        st.last_probe_at -= VERIFY_BACKOFF.as_secs() + 1;
    }

    #[tokio::test]
    async fn observe_past_the_verify_deadline_reprobes_before_replacing() {
        // Past both deadlines the release is still probed once more before it is replaced: a CDN
        // that recovered since the last (hourly) probe gets the release verified, not blacklisted.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 1, // epoch ⇒ far past the verify deadline
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![Track {
                kind: crate::probe::TrackKind::Audio,
                language: Some("eng".into()),
            }]))),
            st.clone(),
        );
        // The fast burst is spent, it has deferred for longer than the floor, and the back-off since
        // its last probe has passed.
        let now = now_secs();
        eng.verify_attempts.lock().await.insert(
            "h1".into(),
            DeferState {
                attempts: MAX_VERIFY_ATTEMPTS,
                last_probe_at: now,
                first_defer_at: now,
            },
        );
        age_defer_state(&eng, "h1").await;
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "a release whose re-probe now passes is not blacklisted"
        );
        assert_eq!(
            st.get_owned("h1".into()).await.expect("kept").status,
            OwnedStatus::Verified
        );
    }

    #[tokio::test]
    async fn a_transient_probe_drops_the_cached_cdn_url() {
        // probe_tracks' contract: on `Transient` the caller re-resolves. The provider caches resolved
        // URLs for hours, so without dropping the entry every re-probe would fetch the same bad URL.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let invalidated = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrent_info: Some(TI {
                id: "tid_h1".into(),
                hash: "h1".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Movie.2023.1080p.x265.mkv".into(),
                    bytes: 10,
                    selected: 1,
                }],
                links: vec!["https://cdn/file".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/file".into()),
            invalidate_calls: invalidated.clone(),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Err(ProbeError::Transient))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert_eq!(
            invalidated.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the deferred probe's URL is dropped so the next attempt resolves a fresh one"
        );
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().status,
            OwnedStatus::Pending
        );
    }

    #[tokio::test]
    async fn observe_verifies_pending_cached_and_writes_selection() {
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![Track {
                kind: crate::probe::TrackKind::Audio,
                language: Some("eng".into()),
            }]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().status,
            OwnedStatus::Verified
        );
        // movie selection slot written for tmdb 27205.
        assert_eq!(
            st.get_selection(crate::store::movie_slot(27205))
                .await
                .unwrap()
                .hash,
            "h1"
        );
    }

    #[tokio::test]
    async fn observe_marks_uncached_acquired_quality_as_cached_on_verify() {
        // Regression: a movie acquired while UNCACHED records `quality.cached = false`. Once it
        // downloads and `observe` verifies it, the record is present/cached on the provider, so the
        // snapshot MUST be refreshed — otherwise the upgrade engine's `!current.cached → any cached
        // candidate is an upgrade` rule would later prune this working copy for a lower-quality one.
        let st = store();
        let uncached_quality = crate::release::QualitySummary {
            cached: false,
            source_tier: 3_000,
            resolution: 1080,
            score: 111_000, // an uncached score (no CACHED_BONUS)
            unverified: false,
        };
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: Some(uncached_quality),
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![Track {
                kind: crate::probe::TrackKind::Audio,
                language: Some("eng".into()),
            }]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        let rec = st.get_owned("h1".into()).await.unwrap();
        assert_eq!(rec.status, OwnedStatus::Verified);
        let q = rec.quality.expect("quality preserved");
        assert!(
            q.cached,
            "verified (downloaded) record must be marked cached"
        );
        assert_eq!(
            q.score,
            111_000 + crate::release::CACHED_BONUS,
            "score must gain the cached bonus so the upgrade comparison stays consistent"
        );
    }

    #[tokio::test]
    async fn observe_wrong_title_blacklists_and_reacquires() {
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        // Scraper returns nothing, so re-acquire finds no replacement; the dead hash is blacklisted.
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(false)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        // First tick: validation fails ONCE — not yet blacklisted (could be a transient TMDB blip).
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "a single validation failure must not blacklist (transient-TMDB tolerance)"
        );
        assert!(st.get_owned("h1".into()).await.is_some());
        // Second consecutive failure → genuine WrongTitle → blacklist + reap.
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await
        );
        assert!(st.get_owned("h1".into()).await.is_none());
    }

    #[tokio::test]
    async fn observe_one_tick_mismatch_recovers_without_blacklist() {
        // The title check reads as a Mismatch on the first tick (a one-tick glitch; a TMDB outage
        // would be `Unavailable` and count no strike) then matches on the second. The record must
        // NOT be blacklisted — it recovers and verifies. This is the restart window (in-memory defer
        // state lost → re-validate) made safe by the two-consecutive-mismatches rule.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(FailThenPassValidator {
                fail_first: 1,
                calls: std::sync::atomic::AtomicU32::new(0),
            }),
            Arc::new(CannedProber(Ok(vec![]))), // probe Accepts (unknown tracks) → Verified
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await; // tick 1: validate fails (1/2)
        assert!(
            st.get_owned("h1".into()).await.is_some(),
            "a transient validate failure must not reap the record"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await; // tick 2: validate passes → verified
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().status,
            OwnedStatus::Verified,
            "once validation recovers, the record verifies (not blacklisted)"
        );
    }

    #[tokio::test]
    async fn observe_waits_out_a_tmdb_outage_without_rejecting_the_release() {
        // TMDB stays unreachable for many ticks (an outage, or a rejected API key): the title check
        // has no verdict, so the correct release must stay Pending — never blacklisted, never
        // deleted, never replaced — and verify once TMDB answers again.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("alice"),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_h1", "h1", "downloaded", 100.0)],
            torrent_info: Some(TI {
                id: "tid_h1".into(),
                hash: "h1".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Movie.2023.1080p.x265.mkv".into(),
                    bytes: 10,
                    selected: 1,
                }],
                links: vec!["https://cdn/file".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/file".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let validator = Arc::new(UnavailableValidator::default());
        let eng = engine(
            provider.clone(),
            Arc::new(MockScraper {
                candidates: vec![cand("h2", true)],
            }),
            validator.clone(),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        for _ in 0..5 {
            eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
                .await;
        }
        assert_eq!(
            validator.calls.load(std::sync::atomic::Ordering::SeqCst),
            5,
            "the title is checked again every tick"
        );
        assert!(
            !st.is_blacklisted(MediaKind::Movie, 27205, "h1".into())
                .await,
            "an unavailable title check must never blacklist the release"
        );
        assert!(
            deleted.lock().unwrap().is_empty(),
            "an unavailable title check must never delete the release"
        );
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().status,
            OwnedStatus::Pending,
            "the release waits, Pending, for TMDB to answer"
        );
        assert!(
            st.get_owned("h2".into()).await.is_none(),
            "no replacement is acquired"
        );
        // TMDB answers again: the same release verifies.
        let eng = engine(
            provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().status,
            OwnedStatus::Verified
        );
    }

    #[tokio::test]
    async fn observe_checks_titles_against_an_unreachable_tmdb_once_per_tick() {
        // Each check against an unreachable TMDB costs a full retry cycle, so after the first one
        // comes back Unavailable the other Pending records wait for the next tick unchecked.
        let st = store();
        for h in ["h1", "h2"] {
            st.put_owned(
                h.into(),
                OwnedRecord {
                    request: req(),
                    provenance: Provenance::watchlist("alice"),
                    added_at: now_secs(),
                    status: OwnedStatus::Pending,
                    provides: vec![],
                    quality: None,
                },
            )
            .await
            .unwrap();
        }
        let validator = Arc::new(UnavailableValidator::default());
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            validator.clone(),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[
            torrent("tid_h1", "h1", "downloaded", 100.0),
            torrent("tid_h1", "h2", "downloaded", 100.0),
        ])
        .await;
        assert_eq!(
            validator.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one unavailable check per tick"
        );
        for h in ["h1", "h2"] {
            assert_eq!(
                st.get_owned(h.into()).await.unwrap().status,
                OwnedStatus::Pending
            );
        }
    }

    #[test]
    fn title_check_maps_an_unavailable_identification_to_no_verdict() {
        use crate::identification::Identification;
        let identified = |id: u64| {
            Identification::Identified(MediaMetadata {
                title: "Movie".into(),
                year: Some("2023".into()),
                media_type: MediaType::Movie,
                external_id: Some(format!("tmdb:{id}")),
            })
        };
        let movie = |found: &Identification| {
            title_check(found, "Movie.2023.mkv", 27205, MediaKind::Movie, None, None)
        };
        assert_eq!(movie(&identified(27205)), TitleCheck::Match);
        assert_eq!(movie(&identified(1)), TitleCheck::Mismatch);
        assert_eq!(movie(&Identification::NoMatch), TitleCheck::Mismatch);
        assert_eq!(
            movie(&Identification::Unavailable),
            TitleCheck::Unavailable,
            "a TMDB failure is no verdict, never a mismatch"
        );
        // A show's file must declare the requested episode.
        let show_identified = Identification::Identified(MediaMetadata {
            title: "Show".into(),
            year: Some("2023".into()),
            media_type: MediaType::Show,
            external_id: Some("tmdb:27205".into()),
        });
        let show = |e: u32| {
            title_check(
                &show_identified,
                "Show.S01E01E02.mkv",
                27205,
                MediaKind::Series,
                Some(1),
                Some(e),
            )
        };
        assert_eq!(show(2), TitleCheck::Match);
        assert_eq!(show(3), TitleCheck::Mismatch);
        // ...in any form `parse_se_all` reads, the `NxM` one included.
        assert_eq!(
            title_check(
                &show_identified,
                "Show - 2x05 - Title.mkv",
                27205,
                MediaKind::Series,
                Some(2),
                Some(5),
            ),
            TitleCheck::Match
        );
    }

    #[test]
    fn title_check_requires_the_requested_media_type() {
        // TMDB's movie and TV id spaces overlap: a file identified as the show that carries the
        // requested movie's numeric id (or the reverse) is another title.
        use crate::identification::Identification;
        let identified = |media_type: MediaType| {
            Identification::Identified(MediaMetadata {
                title: "Title".into(),
                year: Some("2023".into()),
                media_type,
                external_id: Some("tmdb:42".into()),
            })
        };
        let series = |found: &Identification| {
            title_check(
                found,
                "Show.S01E01.mkv",
                42,
                MediaKind::Series,
                Some(1),
                Some(1),
            )
        };
        let movie = |found: &Identification| {
            title_check(found, "Movie.2023.mkv", 42, MediaKind::Movie, None, None)
        };
        assert_eq!(series(&identified(MediaType::Movie)), TitleCheck::Mismatch);
        assert_eq!(movie(&identified(MediaType::Show)), TitleCheck::Mismatch);
        assert_eq!(series(&identified(MediaType::Show)), TitleCheck::Match);
        assert_eq!(movie(&identified(MediaType::Movie)), TitleCheck::Match);
    }

    #[test]
    fn title_check_reads_an_absolute_numbered_episode_file() {
        // The common anime form names the episode by a trailing ` - NN` number and no code: it is
        // that episode of the season the VFS places the file in (season 1 unless the name or a
        // directory names one).
        use crate::identification::Identification;
        let found = Identification::Identified(MediaMetadata {
            title: "Show".into(),
            year: None,
            media_type: MediaType::Show,
            external_id: Some("tmdb:42".into()),
        });
        let check = |path: &str, s: u32, e: u32| {
            title_check(&found, path, 42, MediaKind::Series, Some(s), Some(e))
        };
        assert_eq!(check("Show - 12 (1080p).mkv", 1, 12), TitleCheck::Match);
        assert_eq!(
            check(
                "[Group] Show - 12 (1080p) [ABCD1234]/Show - 12 (1080p).mkv",
                1,
                12
            ),
            TitleCheck::Match
        );
        assert_eq!(check("Show - 12 (1080p).mkv", 1, 13), TitleCheck::Mismatch);
        assert_eq!(check("Show S2 - 05 (1080p).mkv", 2, 5), TitleCheck::Match);
        assert_eq!(
            check("Show S2/Show - 05 (1080p).mkv", 2, 5),
            TitleCheck::Match
        );
        assert_eq!(check("Show - 05 (1080p).mkv", 2, 5), TitleCheck::Mismatch);
        // A number followed by a bracketed release year is no episode number.
        assert_eq!(check("Show - 12 (2008).mkv", 1, 12), TitleCheck::Mismatch);
        // A file with an episode code is read by its code alone.
        assert_eq!(
            check("Show S01E05 - 12 (1080p).mkv", 1, 12),
            TitleCheck::Mismatch
        );
    }

    #[tokio::test]
    async fn observe_prunes_stale_validate_fails_for_unowned_hash() {
        // The per-hash `validate_fails` counter (the transient-TMDB two-strikes tolerance) must be
        // bounded to currently-owned hashes each tick, exactly like `verify_attempts`/`progress`.
        // Otherwise a stale `=1` entry from a prior lifecycle of a hash survives after the title is
        // removed (Trigger-B / dedup / lapse) and, on a later re-acquire of the SAME release, a
        // single transient validate blip bumps it straight to 2 → wrongly blacklists + reaps a
        // correct cached release on its FIRST tick, defeating the documented one-blip tolerance.
        let st = store();
        let eng = engine(
            provider_returning("downloaded", "other"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        // A lingering counter for a hash that is no longer owned (its record was removed).
        eng.validate_fails.lock().await.insert("ghost".into(), 1);
        // A non-empty listing that doesn't include "ghost"; the store has no owned records.
        eng.observe(&[torrent("tid_other", "other", "downloaded", 100.0)])
            .await;
        assert!(
            !eng.validate_fails.lock().await.contains_key("ghost"),
            "a validate_fails entry for an unowned hash must be pruned each observe tick"
        );
    }

    /// A show request naming no episode — the shape of an account-mirror series record (and of an
    /// over-ceiling pack replacement built from one).
    fn episodeless_series_req() -> AcquireRequest {
        AcquireRequest {
            kind: MediaKind::Series,
            season: None,
            episode: None,
            metadata: MediaMetadata {
                media_type: MediaType::Show,
                ..meta()
            },
            ..req()
        }
    }

    #[tokio::test]
    async fn observe_reaps_a_dead_episodeless_series_record_without_reacquiring() {
        // A dead mirror season pack (series record with no requested episode) is reaped, but must NOT
        // re-acquire: the scraper would default the missing episode to S1E1, and every candidate it
        // added could only end in EpisodeMissing → blacklist → delete → next. `monitor_episodes`
        // re-acquires a wanted show's missing episodes through the `provides` union instead.
        let st = store();
        st.put_owned(
            "hpack".into(),
            OwnedRecord {
                request: episodeless_series_req(),
                provenance: Provenance {
                    entries: Vec::new(),
                },
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: vec![(2, 1), (2, 2)],
                quality: None,
            },
        )
        .await
        .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_pack", "hpack", "dead", 0.0)],
            deleted: deleted.clone(),
            added: added.clone(),
            add_magnet: Some(AddMagnetResponse {
                id: "tid_new".into(),
                uri: String::new(),
            }),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("hs1e1", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_pack", "hpack", "dead", 0.0)])
            .await;
        assert!(
            st.get_owned("hpack".into()).await.is_none(),
            "the dead record is reaped"
        );
        assert!(
            st.is_blacklisted(MediaKind::Series, 27205, "hpack".into())
                .await
        );
        assert_eq!(*deleted.lock().unwrap(), vec!["tid_pack".to_string()]);
        assert!(
            added.lock().unwrap().is_empty(),
            "nothing is re-acquired for a record that names no episode"
        );
        assert!(st.get_owned("hs1e1".into()).await.is_none());
    }

    #[tokio::test]
    async fn acquire_refuses_a_series_request_without_an_episode() {
        // Defence in depth: no caller may turn an episode-less show request into an S1E1 scrape.
        let st = store();
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            added: added.clone(),
            add_magnet: Some(AddMagnetResponse {
                id: "tid_new".into(),
                uri: String::new(),
            }),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("hs1e1", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let out = eng
            .acquire(episodeless_series_req(), Provenance::watchlist("alice"))
            .await;
        assert_eq!(out, AcquireOutcome::NoAcceptableRelease);
        assert!(added.lock().unwrap().is_empty());
        assert!(st.get_owned("hs1e1".into()).await.is_none());
    }

    #[tokio::test]
    async fn observe_dead_status_with_no_imdb_is_left_in_place() {
        // Content-loss guard (A19): a dead/error-status torrent whose record has NO imdb id (a mirror
        // torrent that can't be re-scraped) must be LEFT IN PLACE — deleting it would lose
        // un-rescrapable user content. Leave it for on-read repair instead.
        let st = store();
        let mut r = req();
        r.imdb_id = String::new(); // mirror torrent, no imdb id
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: r,
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_h1", "h1", "dead", 0.0)],
            deleted: deleted.clone(),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "dead", 0.0)]).await;
        assert!(
            st.get_owned("h1".into()).await.is_some(),
            "a dead torrent with no imdb id must be left in place (content-loss guard)"
        );
        assert!(
            deleted.lock().unwrap().is_empty(),
            "must not delete an un-rescrapable record"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await
        );
    }

    #[tokio::test]
    async fn observe_dead_status_with_imdb_reaps_and_blacklists() {
        // Counterpart to the content-loss guard: a dead torrent WITH an imdb id (re-scrapable) is
        // reaped — deleted + blacklisted + re-acquired (no replacement here → just removed).
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(), // imdb_id = "tt1"
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_h1", "h1", "dead", 0.0)],
            deleted: deleted.clone(),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper { candidates: vec![] }), // no replacement
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "dead", 0.0)]).await;
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "a dead torrent WITH an imdb id is reaped"
        );
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await
        );
        assert_eq!(*deleted.lock().unwrap(), vec!["tid_h1".to_string()]);
    }

    #[tokio::test]
    async fn observe_dead_delete_failure_keeps_record_and_does_not_reacquire() {
        // When the provider delete FAILS transiently, fail_and_reacquire deliberately KEEPS the
        // owned/authoritative/selection records so a later tick retries the delete. It must NOT
        // also re-acquire a replacement in the same tick — doing so leaves the dead hash present
        // AND a second torrent for the title, and re-scrapes on every subsequent stuck tick until
        // the delete finally succeeds. The re-acquire is gated on the delete having succeeded.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(), // imdb tt1 → re-scrapable
                provenance: Provenance::watchlist("a"),
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_h1", "h1", "dead", 0.0)],
            deleted: deleted.clone(),
            fail_delete: true, // the delete transiently fails → records must be kept, no re-acquire
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h2".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: "tid_h2".into(),
                hash: "h2".into(),
                status: "downloading".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Movie.2023.1080p.x265.mkv".into(),
                    bytes: 10,
                    selected: 1,
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        // The scraper offers a cached replacement h2 — if re-acquire wrongly fired, h2 would become
        // an owned Pending record.
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("h2", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "dead", 0.0)]).await;

        assert_eq!(
            *deleted.lock().unwrap(),
            vec!["tid_h1".to_string()],
            "the delete was attempted"
        );
        assert!(
            st.get_owned("h1".into()).await.is_some(),
            "a failed delete keeps the owned record for a later retry tick"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "the hash is blacklisted only once its delete succeeds (the kept record stops re-adoption)"
        );
        assert!(
            st.get_owned("h2".into()).await.is_none(),
            "no replacement is acquired while the dead torrent's delete is still failing"
        );
    }

    /// An account-mirror movie record of `req()`'s film: empty provenance, `Verified`, and (unless
    /// the caller sets them) no TMDB languages ever fetched — the mirror writes
    /// `original_language`/`spoken_languages` `None`, and only the upgrade pass backfills them.
    fn mirror_movie_record(request: AcquireRequest) -> OwnedRecord {
        OwnedRecord {
            request,
            provenance: Provenance { entries: vec![] },
            added_at: now_secs(),
            status: OwnedStatus::Verified,
            provides: vec![],
            quality: None,
        }
    }

    /// A downloaded single-file movie torrent `id` for `hash`.
    fn downloaded_movie(id: &str, hash: &str) -> TI {
        TI {
            id: id.into(),
            hash: hash.into(),
            status: "downloaded".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Movie.2023.1080p.x265.mkv".into(),
                bytes: 10,
                selected: 1,
            }],
            links: vec!["https://cdn/file".into()],
            ..Default::default()
        }
    }

    /// A Russian-dub REMUX (ranked first under the raw `Original` prefs, which penalise no
    /// language) and an English WEB release of `req()`'s film, both cached.
    fn dub_and_english_candidates() -> Vec<RawCandidate> {
        let mk = |hash: &str, desc: &str| RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: format!("{desc}\nRD+"),
            info_hash: hash.into(),
            file_idx: Some(0),
            file_name: Some(format!("{desc}.mkv")),
        };
        vec![
            mk("hrus", "Movie.2023.1080p.BluRay.REMUX.x265.RUS"),
            mk("heng", "Movie.2023.1080p.WEB.x265.English"),
        ]
    }

    /// A provider listing the dead mirror torrent `tid_h1` and resolving each added candidate of
    /// [`dub_and_english_candidates`] to a downloaded movie (its id = its hash).
    fn provider_with_dead_h1(
        added: Arc<std::sync::Mutex<Vec<String>>>,
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Arc<dyn DebridProvider> {
        Arc::new(MockProvider {
            torrents: vec![torrent("tid_h1", "h1", "dead", 0.0)],
            torrent_info_by_id: ["hrus", "heng"]
                .into_iter()
                .map(|h| (h.to_string(), downloaded_movie(h, h)))
                .collect(),
            resolved_url: Some("https://cdn/file".into()),
            added,
            deleted,
            ..Default::default()
        })
    }

    /// Matches every title and reports `langs` as its TMDB languages; counts the lookups.
    struct LangValidator {
        langs: Option<TitleLanguages>,
        lookups: std::sync::atomic::AtomicU32,
    }
    #[async_trait]
    impl TitleValidator for LangValidator {
        async fn validate(
            &self,
            _f: &str,
            _t: u64,
            _k: MediaKind,
            _s: Option<u32>,
            _e: Option<u32>,
        ) -> TitleCheck {
            TitleCheck::Match
        }
        async fn title_languages(&self, _tmdb_id: u64, _kind: MediaKind) -> Option<TitleLanguages> {
            self.lookups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.langs.clone()
        }
    }

    #[tokio::test]
    async fn dead_mirror_movie_is_replaced_under_its_title_languages() {
        // A dead account-mirror movie is re-acquired from its stored request, which the mirror wrote
        // with no TMDB languages. Ranked with those, `AUDIO_LANGUAGE=original` stays unresolved and
        // penalises nothing, so the Russian dub would be added — and later pass a probe that has no
        // language to check. The title's languages are looked up first, so the English release
        // wins and the replacement record carries them.
        let raw = release::rank(
            dub_and_english_candidates()
                .iter()
                .map(release::parse)
                .collect(),
            &prefs(),
        );
        assert_eq!(raw[0].info_hash, "hrus", "premise: the dub ranks first raw");

        let st = store();
        let no_langs = AcquireRequest {
            original_language: None,
            spoken_languages: None,
            ..req()
        };
        st.put_owned("h1".into(), mirror_movie_record(no_langs))
            .await
            .unwrap();
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let validator = Arc::new(LangValidator {
            langs: Some(TitleLanguages {
                original_language: Some("en".into()),
                spoken_languages: vec!["en".into()],
            }),
            lookups: Default::default(),
        });
        let eng = engine(
            provider_with_dead_h1(added.clone(), deleted.clone()),
            Arc::new(MockScraper {
                candidates: dub_and_english_candidates(),
            }),
            validator.clone(),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "dead", 0.0)]).await;

        assert_eq!(*deleted.lock().unwrap(), vec!["tid_h1".to_string()]);
        assert_eq!(
            *added.lock().unwrap(),
            vec!["heng".to_string()],
            "ranked under the title's languages, the English release is added, not the dub"
        );
        let rec = st
            .get_owned("heng".into())
            .await
            .expect("replacement owned");
        assert_eq!(rec.status, OwnedStatus::Pending);
        assert_eq!(rec.request.original_language.as_deref(), Some("en"));
        assert_eq!(
            rec.request.spoken_languages,
            Some(vec!["en".to_string()]),
            "the languages are persisted, so observe's probe checks the audio"
        );
        assert_eq!(
            validator.lookups.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn dead_mirror_replacement_with_unknown_languages_is_flagged_unverified() {
        // Backstop: when the title's languages can't be looked up, the replacement is ranked and
        // probed with no audio requirement at all. Its probe `Pass` checked no audio, so it must be
        // recorded as unchecked (`unverified`), for the upgrade pass to replace with a checked copy.
        let st = store();
        let no_langs = AcquireRequest {
            original_language: None,
            spoken_languages: None,
            ..req()
        };
        st.put_owned("h1".into(), mirror_movie_record(no_langs))
            .await
            .unwrap();
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let eng = engine(
            provider_with_dead_h1(added.clone(), deleted.clone()),
            Arc::new(MockScraper {
                candidates: dub_and_english_candidates(),
            }),
            Arc::new(LangValidator {
                langs: None, // the lookup failed
                lookups: Default::default(),
            }),
            Arc::new(CannedProber(Ok(vec![Track {
                kind: crate::probe::TrackKind::Audio,
                language: Some("rus".into()),
            }]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "dead", 0.0)]).await;
        assert_eq!(*added.lock().unwrap(), vec!["hrus".to_string()]);

        eng.observe(&[torrent("hrus", "hrus", "downloaded", 100.0)])
            .await;
        let rec = st
            .get_owned("hrus".into())
            .await
            .expect("replacement owned");
        assert_eq!(rec.status, OwnedStatus::Verified);
        assert!(
            rec.quality.expect("acquire-time quality").unverified,
            "a Pass that checked no audio language must be recorded unverified"
        );
    }

    #[tokio::test]
    async fn observe_pass_with_known_languages_is_not_flagged_by_the_backstop() {
        // The backstop only fires when no language could be checked: a request with its languages
        // looked up (even an empty spoken list) keeps a `Pass` checked.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: AcquireRequest {
                    original_language: None,
                    spoken_languages: Some(vec!["en".into()]),
                    ..req()
                },
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: Some(crate::release::QualitySummary {
                    cached: true,
                    source_tier: 3_000,
                    resolution: 1080,
                    score: 1_111_000,
                    unverified: false,
                }),
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![Track {
                kind: crate::probe::TrackKind::Audio,
                language: Some("eng".into()),
            }]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        let rec = st.get_owned("h1".into()).await.expect("record kept");
        assert_eq!(rec.status, OwnedStatus::Verified);
        assert!(!rec.quality.expect("quality kept").unverified);
    }

    #[tokio::test]
    async fn dead_mirror_delete_failure_survives_the_mirror_pass_and_is_retried() {
        // The delete of a dead mirror torrent fails: the records are kept for the next tick's
        // retry, and the hash is blacklisted only once the delete succeeds. The same scan tick's
        // mirror pass must leave the kept record alone (its phantom reaper ignores "Dead" entries
        // and Pending records anyway), else the delete is never retried and the dead movie never
        // replaced.
        let st = store();
        st.put_owned("h1".into(), mirror_movie_record(req()))
            .await
            .unwrap();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let failing: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_h1", "h1", "dead", 0.0)],
            deleted: deleted.clone(),
            fail_delete: true,
            ..Default::default()
        });
        let scraper = Arc::new(MockScraper {
            candidates: vec![cand("h2", true)],
        });
        let eng = engine(
            failing,
            scraper.clone(),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "dead", 0.0)]).await;
        let tmdb = crate::tmdb_client::TmdbClient::offline().await;
        crate::tasks::record_mirror_owned(&st, &tmdb, &prefs(), &[]).await;
        assert_eq!(*deleted.lock().unwrap(), vec!["tid_h1".to_string()]);
        assert!(
            st.get_owned("h1".into()).await.is_some(),
            "the kept record survives the mirror pass"
        );
        assert!(
            !st.is_blacklisted(MediaKind::Movie, 27205, "h1".into())
                .await,
            "not blacklisted while the delete is pending"
        );

        // Next tick: the delete succeeds → blacklisted, removed and replaced.
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let working: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_h1", "h1", "dead", 0.0)],
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h2".into(),
                uri: String::new(),
            }),
            deleted: deleted.clone(),
            added: added.clone(),
            ..Default::default()
        });
        let eng = engine(
            working,
            scraper,
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "dead", 0.0)]).await;
        assert_eq!(*deleted.lock().unwrap(), vec!["tid_h1".to_string()]);
        assert!(st.get_owned("h1".into()).await.is_none());
        assert!(
            st.is_blacklisted(MediaKind::Movie, 27205, "h1".into())
                .await
        );
        assert_eq!(*added.lock().unwrap(), vec!["h2".to_string()]);
        assert_eq!(
            st.get_owned("h2".into()).await.map(|r| r.status),
            Some(OwnedStatus::Pending)
        );
    }

    #[tokio::test]
    async fn a_rejected_release_is_deleted_under_every_listing_id_of_its_hash() {
        // The listing holds two torrents of the rejected hash (a retried add, a manual re-add).
        // Deleting only the entry observe picked would leave the other in the account, untracked
        // and blacklisted (never mirrored), still served by the VFS beside its replacement.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("a"),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let listing = vec![
            torrent("t1", "h1", "downloaded", 100.0),
            torrent("t2", "H1", "downloaded", 100.0),
        ];
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: listing.clone(),
            torrent_info: Some(downloaded_movie("t1", "h1")),
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h2".into(),
                uri: String::new(),
            }),
            resolved_url: Some("https://cdn/file".into()),
            deleted: deleted.clone(),
            added: added.clone(),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("h2", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Err(ProbeError::Corrupt))),
            st.clone(),
        );
        eng.observe(&listing).await;
        let mut gone = deleted.lock().unwrap().clone();
        gone.sort();
        assert_eq!(gone, vec!["t1".to_string(), "t2".to_string()]);
        assert!(st.get_owned("h1".into()).await.is_none());
        assert_eq!(*added.lock().unwrap(), vec!["h2".to_string()]);
    }

    #[tokio::test]
    async fn a_failed_delete_of_a_duplicate_keeps_the_records() {
        // Every listing id of the hash must go before its records are dropped: a failed delete (every
        // one fails here; `a_partly_failed_delete_retries_only_what_is_left` covers one of two)
        // keeps them (and the hash unblacklisted) for the next tick's retry, and adds no replacement.
        let st = store();
        st.put_owned("h1".into(), mirror_movie_record(req()))
            .await
            .unwrap();
        let listing = vec![
            torrent("t1", "h1", "dead", 0.0),
            torrent("t2", "h1", "dead", 0.0),
        ];
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: listing.clone(),
            fail_delete: true,
            deleted: deleted.clone(),
            added: added.clone(),
            ..Default::default()
        });
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("h2", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let gone = eng.observe(&listing).await;
        let mut tried = deleted.lock().unwrap().clone();
        tried.sort();
        assert_eq!(tried, vec!["t1".to_string(), "t2".to_string()]);
        assert!(st.get_owned("h1".into()).await.is_some());
        assert!(
            !st.is_blacklisted(MediaKind::Movie, 27205, "h1".into())
                .await
        );
        assert!(added.lock().unwrap().is_empty());
        assert!(gone.is_empty(), "nothing was deleted: {gone:?}");
    }

    /// A [`MockProvider`] whose `delete_torrent` fails for the ids in `fail_ids` (changeable between
    /// ticks), recording every attempted id in `deleted`.
    #[derive(Debug)]
    struct FailSomeDeletes {
        inner: MockProvider,
        fail_ids: std::sync::Mutex<std::collections::HashSet<String>>,
        deleted: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl FailSomeDeletes {
        fn new(inner: MockProvider, fail_ids: &[&str]) -> Self {
            Self {
                inner,
                fail_ids: std::sync::Mutex::new(fail_ids.iter().map(|s| s.to_string()).collect()),
                deleted: Arc::default(),
            }
        }
        fn heal(&self) {
            self.fail_ids.lock().unwrap().clear();
        }
        fn take_deleted(&self) -> Vec<String> {
            let mut d = std::mem::take(&mut *self.deleted.lock().unwrap());
            d.sort();
            d
        }
    }
    #[async_trait]
    impl DebridProvider for FailSomeDeletes {
        fn name(&self) -> &'static str {
            "fail-some-deletes"
        }
        async fn get_torrents(&self) -> Result<Vec<crate::rd_client::Torrent>, reqwest::Error> {
            self.inner.get_torrents().await
        }
        async fn get_torrent_info(&self, id: &str) -> Result<TI, reqwest::Error> {
            self.inner.get_torrent_info(id).await
        }
        async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
            self.inner.add_magnet(magnet).await
        }
        async fn select_files(&self, t: &str, f: &str) -> Result<(), reqwest::Error> {
            self.inner.select_files(t, f).await
        }
        async fn delete_torrent(&self, id: &str) -> Result<(), reqwest::Error> {
            self.deleted.lock().unwrap().push(id.to_string());
            if self.fail_ids.lock().unwrap().contains(id) {
                return Err(crate::provider::synthetic_bad_gateway(b"delete failed"));
            }
            Ok(())
        }
        async fn resolve_url(
            &self,
            l: &crate::provider::FileLocator,
        ) -> Result<String, crate::error::AppError> {
            self.inner.resolve_url(l).await
        }
        async fn invalidate(&self, l: &crate::provider::FileLocator) {
            self.inner.invalidate(l).await
        }
    }

    /// One of a dead hash's two listing ids fails to delete: the records stay, the hash is not
    /// blacklisted and nothing is added. The next tick lists only the remaining id, deletes it, then
    /// blacklists the hash and re-acquires.
    #[tokio::test]
    async fn a_partly_failed_delete_retries_only_what_is_left() {
        let st = store();
        st.put_owned("h1".into(), mirror_movie_record(req()))
            .await
            .unwrap();
        let added = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(FailSomeDeletes::new(
            MockProvider {
                add_magnet: Some(AddMagnetResponse {
                    id: "tid_h2".into(),
                    uri: String::new(),
                }),
                added: added.clone(),
                ..Default::default()
            },
            &["t2"],
        ));
        let eng = engine(
            provider.clone(),
            Arc::new(MockScraper {
                candidates: vec![cand("h2", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let gone = eng
            .observe(&[
                torrent("t1", "h1", "dead", 0.0),
                torrent("t2", "h1", "dead", 0.0),
            ])
            .await;
        assert_eq!(provider.take_deleted(), vec!["t1", "t2"]);
        assert!(gone.is_empty(), "{gone:?}");
        assert!(st.get_owned("h1".into()).await.is_some());
        assert!(
            !st.is_blacklisted(MediaKind::Movie, 27205, "h1".into())
                .await
        );
        assert!(added.lock().unwrap().is_empty());

        provider.heal();
        let gone = eng.observe(&[torrent("t2", "h1", "dead", 0.0)]).await;
        assert_eq!(provider.take_deleted(), vec!["t2"]);
        assert_eq!(gone, std::collections::HashSet::from(["h1".to_string()]));
        assert!(st.get_owned("h1".into()).await.is_none());
        assert!(
            st.is_blacklisted(MediaKind::Movie, 27205, "h1".into())
                .await
        );
        assert_eq!(*added.lock().unwrap(), vec!["h2".to_string()]);
    }

    /// A stalled download whose delete failed is rejected again on the very next tick: the stall
    /// timer that tripped is kept, not restarted (which would wait out another `STALL_TIMEOUT_SECS`
    /// with the dead download still in the account).
    #[tokio::test]
    async fn a_stalled_download_whose_delete_failed_is_retried_next_tick() {
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("a"),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let provider = Arc::new(FailSomeDeletes::new(
            MockProvider {
                torrent_info: Some(TI {
                    id: "tid_h1".into(),
                    hash: "h1".into(),
                    status: "downloading".into(),
                    files: vec![TorrentFile {
                        id: 0,
                        path: "Movie.2023.1080p.x265.mkv".into(),
                        bytes: 10,
                        selected: 1,
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
            &["tid_h1"],
        ));
        let eng = AcquisitionEngine::new(
            provider.clone(),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            prefs(),
            5,
            Duration::from_millis(300),
            Duration::from_secs(600),
        );
        let listing = [torrent("tid_h1", "h1", "downloading", 10.0)];
        eng.observe(&listing).await; // starts the stall timer
        tokio::time::sleep(Duration::from_millis(400)).await;
        eng.observe(&listing).await; // stalled: the delete fails
        assert_eq!(provider.take_deleted(), vec!["tid_h1"]);
        assert!(st.get_owned("h1".into()).await.is_some());

        provider.heal();
        let gone = eng.observe(&listing).await;
        assert_eq!(provider.take_deleted(), vec!["tid_h1"], "retried at once");
        assert_eq!(gone, std::collections::HashSet::from(["h1".to_string()]));
        assert!(st.get_owned("h1".into()).await.is_none());
        assert!(
            st.is_blacklisted(MediaKind::Movie, 27205, "h1".into())
                .await
        );
    }

    /// `observe` reports the hashes whose listed torrents it deleted (a probe reject, a second
    /// WrongTitle strike), so the scan loop drops them from the listing it shows that tick.
    #[tokio::test]
    async fn observe_returns_the_hashes_it_deleted() {
        let pending = OwnedRecord {
            request: req(),
            provenance: Provenance::watchlist("a"),
            added_at: now_secs(),
            status: OwnedStatus::Pending,
            provides: vec![],
            quality: None,
        };
        let provider = || -> Arc<dyn DebridProvider> {
            Arc::new(MockProvider {
                torrent_info: Some(downloaded_movie("t1", "h1")),
                resolved_url: Some("https://cdn/file".into()),
                ..Default::default()
            })
        };
        let listing = [torrent("t1", "H1", "downloaded", 100.0)];

        // A probe reject.
        let st = store();
        st.put_owned("h1".into(), pending.clone()).await.unwrap();
        let eng = engine(
            provider(),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Err(ProbeError::Corrupt))),
            st.clone(),
        );
        assert_eq!(
            eng.observe(&listing).await,
            std::collections::HashSet::from(["h1".to_string()])
        );

        // WrongTitle: nothing on the first strike, the hash on the second.
        let st = store();
        st.put_owned("h1".into(), pending).await.unwrap();
        let eng = engine(
            provider(),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(false)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert!(eng.observe(&listing).await.is_empty());
        assert_eq!(
            eng.observe(&listing).await,
            std::collections::HashSet::from(["h1".to_string()])
        );
    }

    /// A second WrongTitle strike whose delete fails keeps the strike, so the next tick (one more
    /// mismatch) retries the delete instead of starting the two strikes over.
    #[tokio::test]
    async fn a_wrong_title_whose_delete_failed_keeps_its_strike() {
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("a"),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let provider = Arc::new(FailSomeDeletes::new(
            MockProvider {
                torrent_info: Some(downloaded_movie("t1", "h1")),
                ..Default::default()
            },
            &["t1"],
        ));
        let eng = engine(
            provider.clone(),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(false)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let listing = [torrent("t1", "h1", "downloaded", 100.0)];
        eng.observe(&listing).await; // strike 1
        eng.observe(&listing).await; // strike 2: the delete fails
        assert_eq!(provider.take_deleted(), vec!["t1"]);
        assert!(st.get_owned("h1".into()).await.is_some());
        provider.heal();
        assert_eq!(
            eng.observe(&listing).await,
            std::collections::HashSet::from(["h1".to_string()])
        );
        assert_eq!(provider.take_deleted(), vec!["t1"]);
        assert!(st.get_owned("h1".into()).await.is_none());
    }

    /// Removes `hash`'s owned record while probing it (a concurrent `execute_remove`), then passes.
    struct RemovingProber {
        store: Store,
        hash: &'static str,
    }
    #[async_trait]
    impl Prober for RemovingProber {
        async fn probe(&self, _url: &str) -> Result<Vec<Track>, ProbeError> {
            self.store.remove_owned(self.hash.into()).await.unwrap();
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn a_record_removed_mid_probe_gets_no_selection() {
        // The record vanished while its probe ran: pointing the title's slot at a hash nothing owns
        // (whose torrent was just deleted) would hide the title behind a stale slot nothing repoints.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("a"),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(RemovingProber {
                store: st.clone(),
                hash: "h1",
            }),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert!(st.get_owned("h1".into()).await.is_none(), "not resurrected");
        assert!(
            st.get_selection(crate::store::movie_slot(27205))
                .await
                .is_none(),
            "no slot written for a vanished record"
        );
    }

    #[tokio::test]
    async fn observe_prefers_downloaded_duplicate_over_stale_error_entry() {
        // A hash with a healthy newest `downloaded` entry PLUS an older `error` duplicate (a repair
        // leak / external re-add). observe must resolve the hash to the downloaded copy and keep the
        // Verified record — NOT fail_and_reacquire off the stale error duplicate. (A naive
        // last-write-wins by_hash resolved to the oldest/errored entry and wrongly reaped it.)
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("a"),
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        // Provider lists newest-first: newest downloaded, then an older errored duplicate.
        eng.observe(&[
            torrent("tid_new", "h1", "downloaded", 100.0),
            torrent("tid_old", "h1", "error", 0.0),
        ])
        .await;
        assert!(
            st.get_owned("h1".into()).await.is_some(),
            "the Verified record must survive (resolved to the downloaded duplicate)"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "a title with a healthy downloaded copy must not be blacklisted"
        );
    }

    #[tokio::test]
    async fn observe_keeps_live_downloading_duplicate_over_newest_error_entry() {
        // A hash listed twice: a NEWEST error/dead entry plus an OLDER still-downloading one (a repair
        // leak / external re-add). observe must resolve to the LIVE download (rank by HEALTH, not just
        // downloaded-vs-not) and keep it Pending — NOT reap+blacklist off the dead duplicate, which
        // would abandon the in-flight download.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("a"),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloading", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        // Provider lists newest-first: a newest errored duplicate, then the older live download.
        eng.observe(&[
            torrent("tid_err", "h1", "error", 0.0),
            torrent("tid_dl", "h1", "downloading", 42.0),
        ])
        .await;
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().status,
            OwnedStatus::Pending,
            "the live download must survive (resolved to the downloading duplicate)"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "a live in-flight download must not be reaped/blacklisted off a dead duplicate"
        );
    }

    #[tokio::test]
    async fn observe_reaps_never_resolved_after_dead_timeout() {
        let st = store();
        // added_at far in the past; dead-timeout = 0 ⇒ immediately past it. Genuinely absent: the
        // listing is NON-empty (other torrents present) but doesn't contain h1.
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 1,
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine_dead(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            0,
        );
        // Non-empty listing lacking h1 ⇒ h1 is genuinely absent ⇒ reaped past the dead-timeout.
        eng.observe(&[torrent("other_id", "otherhash", "downloaded", 100.0)])
            .await;
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "never-resolved Pending is reaped"
        );
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await
        );
    }

    #[tokio::test]
    async fn observe_does_not_reap_pending_on_empty_listing() {
        // A spurious empty-but-OK provider listing must NOT reap an in-flight Pending acquisition,
        // even past the dead-timeout — an empty listing is an untrustworthy "absent" signal (provider
        // eventual consistency), so the record is kept for a later tick with a real listing.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 1,
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine_dead(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            0, // dead-timeout 0 ⇒ would reap immediately IF empty were trusted
        );
        eng.observe(&[]).await; // empty listing
        assert!(
            st.get_owned("h1".into()).await.is_some(),
            "an empty listing must NOT reap an in-flight Pending"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "an empty listing must NOT blacklist an in-flight Pending"
        );
    }

    #[tokio::test]
    async fn observe_reaps_stuck_unselected_after_dead_timeout() {
        // Files are present but the provider never honours selection (selected stays 0). Past the
        // dead-timeout this genuinely-stuck hash must be reaped + blacklisted, not retried forever.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 1,
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![torrent("tid_h1", "h1", "downloaded", 100.0)],
            torrent_info: Some(TI {
                id: "tid_h1".into(),
                hash: "h1".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Movie.2023.1080p.x265.mkv".into(),
                    bytes: 10,
                    selected: 0, // never selected
                }],
                ..Default::default()
            }),
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h1".into(),
                uri: String::new(),
            }),
            ..Default::default()
        });
        let eng = engine_dead(
            provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            0,
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "stuck-unselected Pending is reaped past the dead-timeout"
        );
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await
        );
    }

    #[tokio::test]
    async fn observe_reaps_pending_when_info_persistently_unfetchable() {
        // A LISTED Pending torrent whose `get_torrent_info` keeps failing (it is present in the
        // listing, but its per-torrent info endpoint persistently errors) must eventually be reaped +
        // blacklisted. Every other Pending arm has a dead-timeout escape; without one here the record
        // is stranded Pending forever (and `acquire`'s owned-AND-present idempotency short-circuits any
        // reconciler retry). dead_timeout=0 ⇒ the very first failing tick is already past the deadline.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 1,
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            fail_info_ids: std::collections::HashSet::from(["tid_h1".to_string()]),
            ..Default::default()
        });
        let eng = engine_dead(
            provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            0,
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloading", 50.0)])
            .await;
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "a persistently-info-unfetchable Pending torrent past the dead-timeout is reaped"
        );
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "and its hash is blacklisted"
        );
    }

    #[tokio::test]
    async fn observe_does_not_reap_pending_on_transient_info_failure() {
        // A SINGLE transient `get_torrent_info` failure on a listed Pending torrent must NOT reap it,
        // even when the record is ancient (a legitimately-long download). The info-fetch reap keys off
        // how long info has been CONSECUTIVELY unfetchable, not the torrent's `added_at` age — so a
        // one-tick blip on an otherwise-healthy download can never blacklist a good release.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: 1, // ancient — a naive `age > dead_timeout` check would wrongly reap.
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            fail_info_ids: std::collections::HashSet::from(["tid_h1".to_string()]),
            ..Default::default()
        });
        // Generous dead-timeout: one failing tick's elapsed (~0s) is far under it, so no reap.
        let eng = engine_dead(
            provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            3600,
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloading", 50.0)])
            .await;
        assert!(
            st.get_owned("h1".into()).await.is_some(),
            "a single transient info failure must NOT reap a Pending torrent"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "a single transient info failure must NOT blacklist a Pending torrent"
        );
    }

    #[tokio::test]
    async fn observe_leaves_downloading_with_recent_progress_pending() {
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine_dead(
            provider_returning("downloading", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            600,
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloading", 12.0)])
            .await;
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().status,
            OwnedStatus::Pending,
            "slow-seed not judged early"
        );
    }

    #[tokio::test]
    async fn observe_reaps_a_stalled_download() {
        // A `downloading` torrent with no progress for `stall_timeout` must be reaped + blacklisted
        // (the "Stalled" path), else a stuck download strands a Pending record forever. Uses
        // stall_timeout=0 so the no-progress tick trips immediately.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = AcquisitionEngine::new(
            provider_returning("downloading", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            prefs(),
            5,
            Duration::ZERO, // stall_timeout=0 → any no-progress tick is stalled
            Duration::from_secs(600),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloading", 10.0)])
            .await;
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "a stalled download must be reaped"
        );
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "a stalled hash must be blacklisted"
        );
    }

    #[tokio::test]
    async fn is_stalled_resets_when_progress_advances() {
        // The stall timer must RESET whenever download progress ADVANCES across ticks — a moving
        // download is never "stalled", even under the strictest possible timeout. CLAUDE.md flags the
        // "resets only on a progress change" contract: the past TorBox bug was a constant 0.0% that
        // never advanced and so was falsely reaped; here we prove the reset arm fires on real movement.
        // stall_timeout=ZERO is the strictest case — a NON-advancing tick is always "stalled"
        // (elapsed >= 0), so the ONLY thing that can return `false` is the progress-advanced reset.
        let eng = AcquisitionEngine::new(
            provider_returning("downloading", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            store(),
            prefs(),
            5,
            Duration::ZERO,
            Duration::from_secs(600),
        );
        // First sighting at 10% primes the map; with the zero timeout a no-movement tick reads stalled.
        assert!(
            eng.is_stalled("tid", 10.0).await,
            "a no-progress tick is stalled at timeout 0"
        );
        // Progress ADVANCED 10% → 20%: the reset arm fires, so it is NOT stalled even though the zero
        // timeout would otherwise trip. This is the bug-class guard — an actively-moving download survives.
        assert!(
            !eng.is_stalled("tid", 20.0).await,
            "an advancing download must NOT be stalled (reset fires)"
        );
        // Progress now HOLDS at 20%: no advance → stalled again, confirming the reset protects only an
        // actively-moving download (not a one-shot exemption that sticks after movement stops).
        assert!(
            eng.is_stalled("tid", 20.0).await,
            "held progress is stalled again once movement stops"
        );
    }

    #[tokio::test]
    async fn observe_corrupt_probe_blacklists_and_reacquires() {
        // A definitive corrupt-probe verdict reaps + blacklists on the first downloaded tick (no
        // two-strikes — unlike the title validator, a Corrupt structure is unambiguous). This is the
        // gate that keeps a broken file out of playback.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Err(ProbeError::Corrupt))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "a corrupt probe must reap the record"
        );
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await
        );
    }

    #[tokio::test]
    async fn observe_wrong_audio_probe_blacklists_and_reacquires() {
        // A wrong-audio-language probe (required = original "eng", file carries only "fra") must be
        // rejected → reaped + blacklisted, so a wrong-language release can't freeze into the library.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(), // original_language = Some("eng"); prefs.audio = Original
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![Track {
                kind: crate::probe::TrackKind::Audio,
                language: Some("fra".into()),
            }]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "a wrong-audio probe must reap the record"
        );
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await
        );
    }

    #[tokio::test]
    async fn observe_verifies_spoken_language_audio_under_original() {
        // A `fi`-original film: TMDB original_language `fi`, spoken [en, fi]; the release's audio is
        // `eng`.
        // Under the default `AUDIO_LANGUAGE=original` an English track is acceptable (it is one of
        // the title's spoken languages) → Verified, NOT reaped/blacklisted as BadAudio.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: AcquireRequest {
                    original_language: Some("fi".into()),
                    spoken_languages: Some(vec!["en".into(), "fi".into()]),
                    ..req()
                },
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![Track {
                kind: crate::probe::TrackKind::Audio,
                language: Some("eng".into()),
            }]))),
            st.clone(),
        );
        assert_eq!(eng.prefs.audio, AudioReq::Original);
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert_eq!(
            st.get_owned("h1".into()).await.map(|r| r.status),
            Some(OwnedStatus::Verified),
            "a spoken-language audio track must verify under AUDIO_LANGUAGE=original"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await
        );
    }

    /// A Pending owned record carrying an acquire-time quality snapshot (so `record_verified` has a
    /// `quality` to flag), for the unverified-flag observe tests.
    async fn seed_pending_with_quality(st: &Store) {
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: Some(crate::release::QualitySummary {
                    cached: true,
                    source_tier: 3_000,
                    resolution: 1080,
                    score: 1_111_000,
                    unverified: false,
                }),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn observe_flags_unverifiable_accept() {
        // A release whose tracks can't be read (an `.avi`, an unreadable MP4 `moov`) is still
        // accepted — acquisition policy is unchanged — but its quality is flagged `unverified` so the
        // upgrade pass later replaces it with a checked copy.
        let st = store();
        seed_pending_with_quality(&st).await;
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Err(ProbeError::Unsupported))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        let rec = st.get_owned("h1".into()).await.expect("record kept");
        assert_eq!(rec.status, OwnedStatus::Verified, "Accept still verifies");
        assert!(
            rec.quality.expect("quality kept").unverified,
            "an unverifiable Accept must be flagged unverified"
        );
    }

    #[tokio::test]
    async fn observe_pass_is_not_flagged() {
        let st = store();
        seed_pending_with_quality(&st).await;
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![Track {
                kind: crate::probe::TrackKind::Audio,
                language: Some("eng".into()),
            }]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        let rec = st.get_owned("h1".into()).await.expect("record kept");
        assert_eq!(rec.status, OwnedStatus::Verified);
        assert!(
            !rec.quality.expect("quality kept").unverified,
            "a checked (Pass) release must not be flagged"
        );
    }

    #[tokio::test]
    async fn acquire_ranks_a_spoken_language_release_above_a_foreign_dub() {
        // Ranking resolves `Original` against original ∪ spoken: for a `fi`-original title that lists
        // English as spoken, an English-tagged release must not take the wrong-language penalty, so
        // it outranks an otherwise-better (HEVC) French dub. Before, the English release took the
        // same wrong-language penalty as the dub, so the dub's HEVC bonus won.
        let mk = |hash: &str, desc: &str| RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: format!("{desc}\nRD+"),
            info_hash: hash.into(),
            file_idx: Some(0),
            file_name: Some(format!("{desc}.mkv")),
        };
        let st = store();
        let eng = engine(
            provider_adding("downloaded", "heng"),
            Arc::new(MockScraper {
                candidates: vec![
                    mk("hfre", "Film.2026.1080p.WEB.x265.French"),
                    mk("heng", "Film.2026.1080p.WEB.x264.English"),
                ],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let request = AcquireRequest {
            original_language: Some("fi".into()),
            spoken_languages: Some(vec!["en".into(), "fi".into()]),
            ..req()
        };
        let out = eng.acquire(request, Provenance::manual()).await;
        let hash = match out {
            AcquireOutcome::Pending(h) | AcquireOutcome::Acquired(h) => h,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert_eq!(
            hash, "heng",
            "the spoken-language (English) release must win"
        );
    }

    #[tokio::test]
    async fn acquire_skips_a_blacklisted_candidate_and_picks_the_next() {
        // `acquire` must filter a known-bad hash from the candidate set and add the next-best instead,
        // not re-add the blacklisted hash on every reconcile tick.
        let st = store();
        st.blacklist_add(
            crate::scraper::MediaKind::Movie,
            27205,
            "h1".into(),
            "WrongTitle",
            now_secs(),
        )
        .await
        .unwrap();
        let eng = engine(
            provider_adding("downloaded", "h2"),
            Arc::new(MockScraper {
                candidates: vec![cand("h1", true), cand("h2", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let outcome = eng.acquire(req(), Provenance::manual()).await;
        assert_eq!(
            outcome,
            AcquireOutcome::Pending("h2".into()),
            "the blacklisted h1 is skipped; h2 is added"
        );
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "a blacklisted hash must never be added"
        );
        assert!(st.get_owned("h2".into()).await.is_some());
    }

    #[tokio::test]
    async fn acquire_returns_no_acceptable_release_when_all_candidates_blacklisted() {
        let st = store();
        for h in ["h1", "h2"] {
            st.blacklist_add(
                crate::scraper::MediaKind::Movie,
                27205,
                h.into(),
                "WrongTitle",
                now_secs(),
            )
            .await
            .unwrap();
        }
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper {
                candidates: vec![cand("h1", true), cand("h2", true)],
            }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert_eq!(
            eng.acquire(req(), Provenance::manual()).await,
            AcquireOutcome::NoAcceptableRelease,
            "all candidates blacklisted → nothing acquirable"
        );
    }

    #[tokio::test]
    async fn observe_wrong_title_reacquires_the_next_candidate_end_to_end() {
        // The self-healing round-trip: h1 fails title-validation (two strikes) → blacklist + delete +
        // re-scrape → the NEXT candidate (h2) is added as Pending. Proves the recovery promotes a
        // DIFFERENT hash (not a re-add of the just-blacklisted one) end to end.
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(HashAwareProvider::default());
        let eng = engine(
            provider,
            Arc::new(MockScraper {
                candidates: vec![cand("h1", true), cand("h2", true)],
            }),
            Arc::new(OkValidator(false)), // h1 fails title-validation
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        // Tick 1: one validation failure — not yet reaped (transient-TMDB tolerance).
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().status,
            OwnedStatus::Pending
        );
        // Tick 2: second consecutive failure → reap h1 + re-acquire the next candidate.
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert!(st.get_owned("h1".into()).await.is_none(), "h1 reaped");
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "h1 blacklisted"
        );
        let h2 = st
            .get_owned("h2".into())
            .await
            .expect("the next candidate (h2) must be acquired");
        assert_eq!(
            h2.status,
            OwnedStatus::Pending,
            "h2 is recorded Pending (its verdict is deferred to a later observe)"
        );
    }

    #[tokio::test]
    async fn observe_series_reaps_pack_missing_requested_episode() {
        // A downloaded series torrent that does NOT contain the requested (season, episode) must,
        // past the dead-timeout, be reaped + blacklisted as "EpisodeMissing" (a wrong/incomplete
        // pack must be replaced, not stuck forever).
        let st = store();
        let series_req = AcquireRequest {
            imdb_id: "tt1".into(),
            tmdb_id: 27205,
            kind: MediaKind::Series,
            season: Some(1),
            episode: Some(1),
            original_language: Some("eng".into()),
            spoken_languages: Some(vec![]),
            metadata: MediaMetadata {
                title: "Show".into(),
                year: Some("2023".into()),
                media_type: MediaType::Show,
                external_id: Some("tmdb:27205".into()),
            },
        };
        st.put_owned(
            "hp".into(),
            OwnedRecord {
                request: series_req.clone(),
                provenance: Provenance::manual(),
                added_at: now_secs().saturating_sub(10), // older than the (0s) dead-timeout
                status: OwnedStatus::Pending,
                provides: vec![(1, 1)],
                quality: None,
            },
        )
        .await
        .unwrap();
        // The pack contains only S01E05 — the requested S01E01 is absent.
        let wrong_pack: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid_hp".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: "tid_hp".into(),
                hash: "hp".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Show.S01E05.1080p.mkv".into(),
                    bytes: 1_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/e05".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/e05".into()),
            ..Default::default()
        });
        let eng = engine_dead(
            wrong_pack,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            0,
        );
        eng.observe(&[torrent("tid_hp", "hp", "downloaded", 100.0)])
            .await;
        assert!(
            st.get_owned("hp".into()).await.is_none(),
            "a pack missing the requested episode must be reaped past the dead-timeout"
        );
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Series, 27205, "hp".into())
                .await,
            "the wrong-pack hash must be blacklisted"
        );
    }

    #[tokio::test]
    async fn observe_does_not_reap_episode_missing_within_dead_timeout() {
        // Same wrong-pack setup as observe_series_reaps_pack_missing_requested_episode, but the record
        // is fresh (added just now) and the engine's dead-timeout is the default 600s — so the
        // requested episode being absent is still WITHIN the window: observe must WAIT (metadata may
        // still be settling), NOT reap or blacklist. Guards the else-branch of the EpisodeMissing arm.
        let st = store();
        let series_req = AcquireRequest {
            imdb_id: "tt1".into(),
            tmdb_id: 27205,
            kind: MediaKind::Series,
            season: Some(1),
            episode: Some(1),
            original_language: Some("eng".into()),
            spoken_languages: Some(vec![]),
            metadata: MediaMetadata {
                title: "Show".into(),
                year: Some("2023".into()),
                media_type: MediaType::Show,
                external_id: Some("tmdb:27205".into()),
            },
        };
        st.put_owned(
            "hp".into(),
            OwnedRecord {
                request: series_req.clone(),
                provenance: Provenance::manual(),
                added_at: now_secs(), // fresh — well within the 600s dead-timeout
                status: OwnedStatus::Pending,
                provides: vec![(1, 1)],
                quality: None,
            },
        )
        .await
        .unwrap();
        // The pack contains only S01E05 — the requested S01E01 is absent.
        let wrong_pack: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid_hp".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: "tid_hp".into(),
                hash: "hp".into(),
                status: "downloaded".into(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "Show.S01E05.1080p.mkv".into(),
                    bytes: 1_000_000_000,
                    selected: 1,
                }],
                links: vec!["https://cdn/e05".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/e05".into()),
            ..Default::default()
        });
        // Default engine → 600s dead-timeout (NOT engine_dead(.., 0)).
        let eng = engine(
            wrong_pack,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_hp", "hp", "downloaded", 100.0)])
            .await;
        let rec = st
            .get_owned("hp".into())
            .await
            .expect("record must be kept while within the dead-timeout — wait, don't reap early");
        assert_eq!(
            rec.status,
            OwnedStatus::Pending,
            "the record must stay Pending (not reaped/finalised) while waiting"
        );
        assert!(
            !st.is_blacklisted(crate::scraper::MediaKind::Series, 27205, "hp".into())
                .await,
            "the hash must NOT be blacklisted while still within the dead-timeout"
        );
    }

    #[tokio::test]
    async fn observe_movie_pack_guard_rejects_and_reacquires() {
        let st = store();
        st.put_owned(
            "h1".into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        // Provider returns a TI with TWO feature-sized video files — a multi-movie pack.
        let pack_provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid_h1".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: "tid_h1".into(),
                hash: "h1".into(),
                status: "downloaded".into(),
                files: vec![
                    TorrentFile {
                        id: 0,
                        path: "Movie.A.2020.1080p.mkv".into(),
                        bytes: 2_000_000_000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 1,
                        path: "Movie.B.2019.1080p.mkv".into(),
                        bytes: 2_000_000_000,
                        selected: 1,
                    },
                ],
                links: vec!["https://cdn/a".into(), "https://cdn/b".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/a".into()),
            ..Default::default()
        });
        // Scraper returns no replacement candidates — blacklist is the only outcome.
        let eng = engine(
            pack_provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)])
            .await;
        assert!(
            st.get_owned("h1".into()).await.is_none(),
            "movie-pack record must be removed"
        );
        assert!(
            st.is_blacklisted(crate::scraper::MediaKind::Movie, 27205, "h1".into())
                .await,
            "movie-pack hash must be blacklisted"
        );
    }

    /// A torrent holding the requested episode twice (two language folders) is probed on its first
    /// copy, and that is the file its slot serves: never the second, unchecked one. `provides` lists
    /// the episode once, so a single-episode torrent is not mistaken for a pack.
    #[tokio::test]
    async fn observe_slots_the_episode_file_it_probed_when_a_pack_holds_it_twice() {
        let st = store();
        st.put_owned(
            "hp".into(),
            OwnedRecord {
                request: series_req_for(1),
                provenance: Provenance::watchlist("alice"),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![(1, 1)],
                quality: None,
            },
        )
        .await
        .unwrap();
        let file = |id, path: &str| TorrentFile {
            id,
            path: path.into(),
            bytes: 1_000_000_000,
            selected: 1,
        };
        // Each file resolves to its own link, so the probed URL names the probed file.
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolve_by_link: true,
            torrent_info: Some(TI {
                id: "tid_hp".into(),
                hash: "hp".into(),
                status: "downloaded".into(),
                files: vec![file(0, "A/Show.S01E01.mkv"), file(1, "B/Show.S01E01.mkv")],
                links: vec!["https://cdn/a".into(), "https://cdn/b".into()],
                ..Default::default()
            }),
            ..Default::default()
        });
        let prober = Arc::new(RecordingProber::default());
        let eng = engine(
            provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            prober.clone(),
            st.clone(),
        );
        eng.observe(&[torrent("tid_hp", "hp", "downloaded", 100.0)])
            .await;
        let rec = st.get_owned("hp".into()).await.unwrap();
        assert_eq!(rec.status, OwnedStatus::Verified);
        assert_eq!(rec.provides, vec![(1, 1)]);
        assert_eq!(
            *prober.urls.lock().unwrap(),
            vec!["https://cdn/a".to_string()],
            "observe probes the first copy"
        );
        let tmdb = crate::vfs::tmdb_id_of(&rec.request.metadata).unwrap();
        assert_eq!(
            st.get_selection(crate::store::episode_slot(tmdb, 1, 1))
                .await
                .unwrap()
                .file_path,
            "A/Show.S01E01.mkv",
            "the slot serves the file observe probed"
        );
    }

    /// A pack whose files carry `NxM` codes (`Show - 2x05 - Title.mkv`) supplies the episode it was
    /// added for: `observe` verifies it with every episode it holds and slots the requested one,
    /// rather than rejecting a correct release as EpisodeMissing once the dead timeout has passed.
    #[tokio::test]
    async fn observe_verifies_a_pack_whose_files_use_nxm_episode_codes() {
        let st = store();
        let mut request = series_req_for(5);
        request.season = Some(2);
        st.put_owned(
            "hp".into(),
            OwnedRecord {
                request,
                provenance: Provenance::watchlist("alice"),
                // Past the 600 s dead timeout, where a file-less episode is rejected.
                added_at: now_secs() - 3_600,
                status: OwnedStatus::Pending,
                provides: vec![(2, 5)],
                quality: None,
            },
        )
        .await
        .unwrap();
        let file = |id, path: &str| TorrentFile {
            id,
            path: path.into(),
            bytes: 1_000_000_000,
            selected: 1,
        };
        let provider = Arc::new(MockProvider {
            resolve_by_link: true,
            torrent_info: Some(TI {
                id: "tid_hp".into(),
                hash: "hp".into(),
                status: "downloaded".into(),
                files: vec![
                    file(0, "Show S02/Show - 2x04 - First.mkv"),
                    file(1, "Show S02/Show - 2x05 - Second.mkv"),
                    file(2, "Show S02/Show - 2x06 - Third.mkv"),
                ],
                links: vec![
                    "https://cdn/e04".into(),
                    "https://cdn/e05".into(),
                    "https://cdn/e06".into(),
                ],
                ..Default::default()
            }),
            ..Default::default()
        });
        let prober = Arc::new(RecordingProber::default());
        let eng = engine(
            provider.clone(),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            prober.clone(),
            st.clone(),
        );
        eng.observe(&[torrent("tid_hp", "hp", "downloaded", 100.0)])
            .await;
        let rec = st
            .get_owned("hp".into())
            .await
            .expect("the pack is kept, not rejected as EpisodeMissing");
        assert_eq!(rec.status, OwnedStatus::Verified);
        assert_eq!(rec.provides, vec![(2, 4), (2, 5), (2, 6)]);
        assert!(provider.deleted.lock().unwrap().is_empty());
        assert_eq!(
            *prober.urls.lock().unwrap(),
            vec!["https://cdn/e05".to_string()],
            "the requested episode's file is probed"
        );
        let tmdb = crate::vfs::tmdb_id_of(&rec.request.metadata).unwrap();
        assert_eq!(
            st.get_selection(crate::store::episode_slot(tmdb, 2, 5))
                .await
                .unwrap()
                .file_path,
            "Show S02/Show - 2x05 - Second.mkv"
        );
    }

    /// Run one `observe` tick over a downloaded show torrent `hp` holding `paths`, owned `Pending`
    /// for S`season`E`episode` and added an hour ago (past the 600 s dead timeout, where a torrent
    /// lacking the episode is rejected). Returns the store and the provider.
    async fn observe_show_torrent(
        paths: &[&str],
        season: u32,
        episode: u32,
    ) -> (Store, Arc<MockProvider>) {
        let st = store();
        let mut request = series_req_for(episode);
        request.season = Some(season);
        st.put_owned(
            "hp".into(),
            OwnedRecord {
                request,
                provenance: Provenance::watchlist("alice"),
                added_at: now_secs() - 3_600,
                status: OwnedStatus::Pending,
                provides: vec![(season, episode)],
                quality: None,
            },
        )
        .await
        .unwrap();
        let provider = Arc::new(MockProvider {
            resolve_by_link: true,
            torrent_info: Some(TI {
                id: "tid_hp".into(),
                hash: "hp".into(),
                status: "downloaded".into(),
                files: paths
                    .iter()
                    .enumerate()
                    .map(|(id, path)| TorrentFile {
                        id: id as u32,
                        path: (*path).into(),
                        bytes: 1_000_000_000,
                        selected: 1,
                    })
                    .collect(),
                links: (0..paths.len())
                    .map(|i| format!("https://cdn/f{i}"))
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        });
        let eng = engine(
            provider.clone(),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(RecordingProber::default()),
            st.clone(),
        );
        eng.observe(&[torrent("tid_hp", "hp", "downloaded", 100.0)])
            .await;
        (st, provider)
    }

    /// A single-file anime release named by its absolute number (`[Group] Show - 12 (1080p)`, no
    /// episode code) supplies the episode it names in season 1, where the VFS places it: `observe`
    /// verifies it and slots it, rather than rejecting it as EpisodeMissing once the dead timeout has
    /// passed (and blacklisting a correct release for 30 days).
    #[tokio::test]
    async fn observe_verifies_a_single_absolute_numbered_episode_file() {
        let path = "[Group] Show - 12 (1080p) [ABCD1234].mkv";
        let (st, provider) = observe_show_torrent(&[path], 1, 12).await;
        let rec = st
            .get_owned("hp".into())
            .await
            .expect("the release is kept, not rejected as EpisodeMissing");
        assert_eq!(rec.status, OwnedStatus::Verified);
        assert_eq!(rec.provides, vec![(1, 12)]);
        assert!(provider.deleted.lock().unwrap().is_empty());
        assert!(
            !st.is_blacklisted(MediaKind::Series, 27205, "hp".into())
                .await
        );
        assert_eq!(
            st.get_selection(crate::store::episode_slot(27205, 1, 12))
                .await
                .unwrap()
                .file_path,
            path
        );
    }

    /// The fallback is narrow: a number naming another episode, and a multi-file torrent of
    /// code-less files (whose absolute numbers can't be mapped past season 1 without TMDB's
    /// per-season episode counts), are still rejected as EpisodeMissing.
    #[tokio::test]
    async fn observe_rejects_an_absolute_numbered_file_for_another_episode_or_in_a_pack() {
        for (paths, episode) in [
            (vec!["[Group] Show - 12 (1080p) [ABCD1234].mkv"], 13),
            (
                vec![
                    "Pack/[Group] Show - 12 (1080p).mkv",
                    "Pack/[Group] Show - 13 (1080p).mkv",
                ],
                12,
            ),
        ] {
            let (st, provider) = observe_show_torrent(&paths, 1, episode).await;
            assert!(
                st.get_owned("hp".into()).await.is_none(),
                "{paths:?} requested as E{episode} is rejected"
            );
            assert!(
                st.is_blacklisted(MediaKind::Series, 27205, "hp".into())
                    .await
            );
            assert_eq!(
                *provider.deleted.lock().unwrap(),
                vec!["tid_hp".to_string()]
            );
        }
    }

    /// A keyword in the episode title after the code (`Bonus Round`) doesn't make the file an extras
    /// clip, so the episode is found and verified instead of being reaped as NeverResolved.
    #[tokio::test]
    async fn observe_verifies_an_episode_whose_title_holds_an_extras_keyword() {
        let path = "Show - S02E05 - Bonus Round.mkv";
        let (st, provider) = observe_show_torrent(&[path], 2, 5).await;
        let rec = st
            .get_owned("hp".into())
            .await
            .expect("the release is kept");
        assert_eq!(rec.status, OwnedStatus::Verified);
        assert_eq!(rec.provides, vec![(2, 5)]);
        assert!(provider.deleted.lock().unwrap().is_empty());
        assert_eq!(
            st.get_selection(crate::store::episode_slot(27205, 2, 5))
                .await
                .unwrap()
                .file_path,
            path
        );
    }

    #[tokio::test]
    async fn observe_series_pack_writes_provides_and_episode_selection() {
        let st = store();
        let series_req = AcquireRequest {
            imdb_id: "tt1".into(),
            tmdb_id: 27205,
            kind: MediaKind::Series,
            season: Some(1),
            episode: Some(1),
            original_language: Some("eng".into()),
            spoken_languages: Some(vec![]),
            metadata: MediaMetadata {
                title: "Show".into(),
                year: Some("2023".into()),
                media_type: MediaType::Show,
                external_id: Some("tmdb:27205".into()),
            },
        };
        st.put_owned(
            "hp".into(),
            OwnedRecord {
                request: series_req.clone(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![(1, 1)],
                quality: None,
            },
        )
        .await
        .unwrap();
        // Provider returns a season-pack TI: S01E01 + S01E02, both selected.
        let series_provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid_hp".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: "tid_hp".into(),
                hash: "hp".into(),
                status: "downloaded".into(),
                files: vec![
                    TorrentFile {
                        id: 0,
                        path: "Show.S01E01.1080p.mkv".into(),
                        bytes: 1_000_000_000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 1,
                        path: "Show.S01E02.1080p.mkv".into(),
                        bytes: 1_000_000_000,
                        selected: 1,
                    },
                ],
                links: vec!["https://cdn/e01".into(), "https://cdn/e02".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/e01".into()),
            ..Default::default()
        });
        let eng = engine(
            series_provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_hp", "hp", "downloaded", 100.0)])
            .await;
        let rec = st.get_owned("hp".into()).await.unwrap();
        assert_eq!(
            rec.status,
            OwnedStatus::Verified,
            "series pack must be Verified after observe"
        );
        let mut provides = rec.provides.clone();
        provides.sort();
        assert_eq!(
            provides,
            vec![(1u32, 1u32), (1u32, 2u32)],
            "provides must cover both episodes"
        );
        assert_eq!(
            st.get_selection(crate::store::episode_slot(27205, 1, 1))
                .await
                .unwrap()
                .hash,
            "hp",
            "selection slot for S01E01 must point to hp"
        );
        assert_eq!(
            st.get_selection(crate::store::episode_slot(27205, 1, 2))
                .await
                .unwrap()
                .hash,
            "hp",
            "selection slot for S01E02 must point to hp"
        );
    }

    // Shared setup for the pack-selection-gate tests: an E01 single ("hsingle") at `single_score`
    // and a pending pack ("hpack", E01+E02) at `pack_score`. Runs observe and returns the store.
    async fn run_pack_gate(single_score: i64, pack_score: i64) -> Store {
        let q = |score: i64| {
            Some(crate::release::QualitySummary {
                cached: true,
                source_tier: 8_000,
                resolution: 1080,
                score,
                unverified: false,
            })
        };
        run_pack_gate_q(q(single_score), q(pack_score)).await
    }

    /// [`run_pack_gate`] with explicit (possibly unknown) qualities for the single and the pack. The
    /// pack's probe passes, so it is recorded checked.
    async fn run_pack_gate_q(
        single_q: Option<crate::release::QualitySummary>,
        pack_q: Option<crate::release::QualitySummary>,
    ) -> Store {
        run_pack_gate_probed(single_q, pack_q, true).await
    }

    /// [`run_pack_gate_q`], the pack's probe passing (`checked`) or finding no tracks (an `Accept`,
    /// so the pack is recorded unchecked).
    async fn run_pack_gate_probed(
        single_q: Option<crate::release::QualitySummary>,
        pack_q: Option<crate::release::QualitySummary>,
        checked: bool,
    ) -> Store {
        let st = store();
        let base = AcquireRequest {
            imdb_id: "tt1".into(),
            tmdb_id: 27205,
            kind: MediaKind::Series,
            season: Some(1),
            episode: Some(2),
            original_language: Some("eng".into()),
            spoken_languages: Some(vec![]),
            metadata: MediaMetadata {
                title: "Show".into(),
                year: Some("2023".into()),
                media_type: MediaType::Show,
                external_id: Some("tmdb:27205".into()),
            },
        };
        st.put_owned(
            "hsingle".into(),
            OwnedRecord {
                request: AcquireRequest {
                    episode: Some(1),
                    ..base.clone()
                },
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: vec![(1, 1)],
                quality: single_q,
            },
        )
        .await
        .unwrap();
        st.put_owned(
            "hpack".into(),
            OwnedRecord {
                request: base.clone(),
                provenance: Provenance::manual(),
                added_at: now_secs(),
                status: OwnedStatus::Pending,
                provides: vec![(1, 2)],
                quality: pack_q,
            },
        )
        .await
        .unwrap();
        st.put_selection(
            crate::store::episode_slot(27205, 1, 1),
            crate::store::SelectionEntry {
                hash: "hsingle".into(),
                file_path: "E01.single.mkv".into(),
            },
        )
        .await
        .unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid_hpack".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TI {
                id: "tid_hpack".into(),
                hash: "hpack".into(),
                status: "downloaded".into(),
                files: vec![
                    TorrentFile {
                        id: 0,
                        path: "Show.S01E01.mkv".into(),
                        bytes: 1_000_000_000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 1,
                        path: "Show.S01E02.mkv".into(),
                        bytes: 1_000_000_000,
                        selected: 1,
                    },
                ],
                links: vec!["https://cdn/e01".into(), "https://cdn/e02".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/e01".into()),
            ..Default::default()
        });
        let tracks = if checked {
            vec![Track {
                kind: crate::probe::TrackKind::Audio,
                language: Some("eng".into()),
            }]
        } else {
            vec![]
        };
        let eng = engine(
            provider,
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(tracks))),
            st.clone(),
        );
        eng.observe(&[
            torrent("tid_hpack", "hpack", "downloaded", 100.0),
            torrent("tid_hsingle", "hsingle", "downloaded", 100.0),
        ])
        .await;
        st
    }

    /// The gate ranks like the scan loop's selection repoint (quality first, the score only within
    /// equal quality), so that pass never moves the slot straight back: a 1080p BluRay x265 pack
    /// whose HEVC bonus out-scores a 1080p REMUX x264 single, or an unchecked pack of the single's
    /// quality, never takes the single's slot.
    #[tokio::test]
    async fn observe_pack_slot_gate_ranks_quality_before_score() {
        let q = |tier: i64, score: i64| {
            Some(crate::release::QualitySummary {
                cached: true,
                source_tier: tier,
                resolution: 1080,
                score,
                unverified: false,
            })
        };
        let slot = |st: Store| async move {
            st.get_selection(crate::store::episode_slot(27205, 1, 1))
                .await
                .unwrap()
                .hash
        };
        let st = run_pack_gate_q(q(8_000, 1_000_000), q(6_000, 1_100_000)).await;
        assert_eq!(slot(st.clone()).await, "hsingle");
        assert_eq!(
            st.get_selection(crate::store::episode_slot(27205, 1, 2))
                .await
                .unwrap()
                .hash,
            "hpack",
            "the pack still claims the empty E02 slot"
        );
        let st = run_pack_gate_probed(q(8_000, 1_000_000), q(8_000, 2_000_000), false).await;
        assert_eq!(slot(st.clone()).await, "hsingle", "an unchecked pack");
        // Nothing for the repoint planner to move back.
        let owned = st.all_owned().await;
        let selection = st.all_selection().await;
        let single = TI {
            hash: "hsingle".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "E01.single.mkv".into(),
                bytes: 1_000_000_000,
                selected: 1,
            }],
            ..Default::default()
        };
        let pack = TI {
            hash: "hpack".into(),
            files: vec![
                TorrentFile {
                    id: 0,
                    path: "Show.S01E01.mkv".into(),
                    bytes: 1_000_000_000,
                    selected: 1,
                },
                TorrentFile {
                    id: 1,
                    path: "Show.S01E02.mkv".into(),
                    bytes: 1_000_000_000,
                    selected: 1,
                },
            ],
            ..Default::default()
        };
        let plan = crate::selection::plan_selection_repoints(
            &owned,
            &selection,
            &[&single, &pack],
            crate::selection::RepointRules {
                max_resolution: 1080,
                allow_resolution_downgrade: false,
            },
            &prefs(),
        );
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[tokio::test]
    async fn observe_pack_does_not_clobber_a_better_existing_episode_selection() {
        // A LOWER-quality season pack must claim the empty E02 slot but NOT steal E01 from a better
        // single — that would serve a worse file and let dedup delete the better copy.
        let st = run_pack_gate(1_000_000, 100).await;
        assert_eq!(
            st.get_selection(crate::store::episode_slot(27205, 1, 1))
                .await
                .unwrap()
                .hash,
            "hsingle",
            "the better E01 single must NOT be clobbered by the lower-quality pack"
        );
        assert_eq!(
            st.get_selection(crate::store::episode_slot(27205, 1, 2))
                .await
                .unwrap()
                .hash,
            "hpack",
            "the pack still claims the empty E02 slot"
        );
        let mut provides = st.get_owned("hpack".into()).await.unwrap().provides;
        provides.sort();
        assert_eq!(
            provides,
            vec![(1u32, 1u32), (1u32, 2u32)],
            "provides still covers both episodes (no re-acquire churn)"
        );
    }

    #[tokio::test]
    async fn observe_pack_overwrites_a_worse_existing_episode_selection() {
        // The symmetric direction: a pack that is NOT a downgrade (>= the single) DOES repoint E01,
        // so a genuine consolidation/quality improvement still takes effect.
        let st = run_pack_gate(100, 1_000_000).await;
        assert_eq!(
            st.get_selection(crate::store::episode_slot(27205, 1, 1))
                .await
                .unwrap()
                .hash,
            "hpack",
            "a better-or-equal pack repoints E01"
        );
    }

    /// Unknown quality is the lowest: a KNOWN pack takes a sibling slot held by an unknown-quality
    /// single (no recorded quality, or an untagged axis — whatever its score), while an unknown pack
    /// never steals a slot from a known single.
    #[tokio::test]
    async fn observe_pack_slot_gate_ranks_unknown_quality_lowest() {
        let q = |res: u16, tier: i64, score: i64| {
            Some(crate::release::QualitySummary {
                cached: true,
                source_tier: tier,
                resolution: res,
                score,
                unverified: false,
            })
        };
        let slot = |st: Store| async move {
            st.get_selection(crate::store::episode_slot(27205, 1, 1))
                .await
                .unwrap()
                .hash
        };
        for single in [None, q(720, 0, 9_000_000), q(0, 8_000, 9_000_000)] {
            let st = run_pack_gate_q(single.clone(), q(720, 3_000, 1_075_000)).await;
            assert_eq!(
                slot(st).await,
                "hpack",
                "{single:?}: the known pack takes the slot"
            );
        }
        // A known resolution stays a floor: a 720p pack never takes the slot of a 1080p single
        // whose source is untagged.
        let st = run_pack_gate_q(q(1080, 0, 1), q(720, 3_000, 1_075_000)).await;
        assert_eq!(slot(st).await, "hsingle");
        for pack in [None, q(720, 0, 9_000_000), q(0, 8_000, 9_000_000)] {
            let st = run_pack_gate_q(q(720, 3_000, 1_075_000), pack.clone()).await;
            assert_eq!(
                slot(st).await,
                "hsingle",
                "{pack:?}: an unknown pack never steals it at the same or a lower resolution"
            );
        }
        // Resolution comes first, as dedup ranks copies: a 1080p pack whose source is untagged
        // outranks a 720p single.
        let st = run_pack_gate_q(q(720, 3_000, 1_075_000), q(1080, 0, 1)).await;
        assert_eq!(slot(st).await, "hpack");
    }

    /// Seed `hash` as an owned movie record of [`req`] with its authoritative id and the film's
    /// selection slot, plus an unrelated title's slot that must survive.
    async fn seed_owned_with_slot(st: &Store, hash: &str, status: OwnedStatus) {
        st.put_owned(
            hash.into(),
            OwnedRecord {
                request: req(),
                provenance: Provenance::watchlist("alice"),
                added_at: 1,
                status,
                provides: vec![],
                quality: None,
            },
        )
        .await
        .unwrap();
        st.put_authoritative(hash.into(), meta()).await.unwrap();
        for (slot, h) in [
            (crate::store::movie_slot(27205), hash),
            (crate::store::movie_slot(4242), "other"),
        ] {
            st.put_selection(
                slot,
                crate::store::SelectionEntry {
                    hash: h.into(),
                    file_path: format!("{h}.mkv"),
                },
            )
            .await
            .unwrap();
        }
    }

    /// Assert `hash`'s owned record, authoritative id and slot are all gone, and the other title's
    /// slot kept.
    async fn assert_forgotten(st: &Store, hash: &str) {
        assert!(st.get_owned(hash.into()).await.is_none(), "owned row");
        assert!(
            st.authoritative_meta(hash.into()).await.is_none(),
            "authoritative id"
        );
        assert!(
            st.get_selection(crate::store::movie_slot(27205))
                .await
                .is_none(),
            "the film's slot"
        );
        assert_eq!(
            st.get_selection(crate::store::movie_slot(4242))
                .await
                .map(|e| e.hash),
            Some("other".to_string()),
            "another title's slot is untouched"
        );
    }

    /// A reaped torrent's owned row, authoritative id and slots go in ONE commit after the
    /// blacklist entry, so a write failure can't leave a slot serving a forgotten record (or an
    /// authoritative id for one): with only two commits let through, all three are gone.
    #[tokio::test]
    async fn fail_and_reacquire_forgets_the_record_in_one_commit() {
        let (st, switch) = crate::store::fail_writes::fail_after_arm_store();
        let per_commit = crate::store::fail_writes::syncs_per_commit(&st, &switch).await;
        seed_owned_with_slot(&st, "h1", OwnedStatus::Pending).await;
        let eng = engine(
            Arc::new(MockProvider::default()),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st,
        );
        switch.arm_after(2, per_commit); // the blacklist entry, then the forget
        assert!(
            eng.fail_and_reacquire(
                "h1",
                &["tid_h1"],
                &req(),
                "Dead",
                &Provenance::watchlist("alice"),
            )
            .await
        );
        switch.disarm();
        let st = switch.reopen();
        assert!(st.blacklist_newest_at_by_hash(&[]).await.contains_key("h1"));
        assert_forgotten(&st, "h1").await;
    }

    /// The lapsed record `acquire` reaps for the same slot is forgotten in one commit too.
    #[tokio::test]
    async fn reap_lapsed_superseded_forgets_the_record_in_one_commit() {
        let (st, switch) = crate::store::fail_writes::fail_after_arm_store();
        let per_commit = crate::store::fail_writes::syncs_per_commit(&st, &switch).await;
        seed_owned_with_slot(&st, "hold", OwnedStatus::Verified).await;
        let eng = engine(
            Arc::new(MockProvider::default()),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st,
        );
        switch.arm_after(1, per_commit);
        let present: std::collections::HashSet<String> = ["hnew".to_string()].into();
        eng.reap_lapsed_superseded(&req(), "hnew", &present).await;
        switch.disarm();
        assert_forgotten(&switch.reopen(), "hold").await;
    }
}
