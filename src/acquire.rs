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
    /// Confirmed: cached + verified (or accepted-with-warning).
    Acquired(String),
    /// Added; downloading or probe deferred — `observe` finishes it.
    Pending(String),
    /// No candidate passed (all blacklisted / above ceiling / wrong-title / failed).
    NoAcceptableRelease,
    /// Scraper unreachable; retry later.
    TemporarilyUnavailable,
}

/// Validates an acquired file genuinely matches the requested title (reuses `identify_name`).
#[async_trait]
pub trait TitleValidator: Send + Sync {
    async fn validate(
        &self,
        file_name: &str,
        expected_tmdb_id: u64,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> bool;
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
        file_name: &str,
        expected: u64,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> bool {
        // Reuse the existing identification logic; confident == resolves to the expected tmdb id.
        // identify_name needs the file(s) for its show-vs-movie heuristic; the selected file
        // (whose name carries any SxxExx pattern) is enough to drive that for a single title.
        let files = [crate::rd_client::TorrentFile {
            id: 0,
            path: file_name.to_string(),
            bytes: 0,
            selected: 1,
        }];
        let meta = crate::identification::identify_name(file_name, &files, &self.tmdb).await;
        let id_ok = matches!(
            &meta,
            Some(m) if m.external_id.as_deref() == Some(format!("tmdb:{}", expected).as_str())
        );
        if !id_ok {
            return false;
        }
        if kind == MediaKind::Series {
            // Accept the file if it declares the requested episode — including a multi-episode file
            // (e.g. `S01E01E02`) requested for its second episode.
            matches!(
                (season, episode),
                (Some(s), Some(e)) if parse_se_all(file_name).contains(&(s, e))
            )
        } else {
            true
        }
    }
}

/// Parse ALL `(season, episode)` pairs a filename declares. Handles a single `SxxEyy`, a contiguous
/// MULTI-episode file (`S01E01E02`, `S01E01-E02`, `S01E01.E02`), and the dash-concatenated absolute
/// code (`Show - 409 - Title` → `[(4, 9)]`). Returns an empty `Vec` when no episode code is found.
///
/// Multi-episode handling matters for `provides`: a double-episode file reporting only its FIRST
/// episode leaves the second looking un-owned, so `monitor_episodes` re-acquires it as a duplicate
/// singleton — the same failure the dash-code fallback prevents for packs, in a different form.
///
/// KNOWN LIMITATION — a RANGE form spanning >2 episodes (`S01E01-E03`) records only the explicit
/// endpoints `[(1,1),(1,3)]`, dropping the middle `(1,2)` (so it may re-acquire the middle as a
/// duplicate). The common 2-episode forms (`S01E01E02`, `S01E01-E02`) and explicit lists
/// (`S01E01E02E03`) parse fully; range-named SINGLE files spanning 3+ episodes are uncommon (such
/// content is usually a season pack of separate per-episode files), so this is an accepted edge.
fn parse_se_all(name: &str) -> Vec<(u32, u32)> {
    use regex::Regex;
    use std::sync::LazyLock;
    // Primary: SxxEyy, plus any contiguous `Eyy` continuations in the SAME season. The continuation
    // REQUIRES an explicit `e` before each number so a resolution/codec token can't be mis-parsed as
    // an episode (`S01E01.1080p` → the `.1080` has no `e`, so it is NOT a continuation).
    static SE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)s(\d{1,2})e(\d{1,3})((?:[-_. ]?e\d{1,3})*)").unwrap());
    if let Some(c) = SE.captures(name) {
        if let (Some(s), Some(e1)) = (
            c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()),
            c.get(2).and_then(|m| m.as_str().parse::<u32>().ok()),
        ) {
            let mut eps = vec![(s, e1)];
            if let Some(rest) = c.get(3).map(|m| m.as_str()) {
                static EP: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r"(?i)e(\d{1,3})").unwrap());
                for cap in EP.captures_iter(rest) {
                    if let Some(e) = cap.get(1).and_then(|m| m.as_str().parse::<u32>().ok()) {
                        let pair = (s, e);
                        if !eps.contains(&pair) {
                            eps.push(pair);
                        }
                    }
                }
            }
            return eps;
        }
        // The `\d{1,2}`/`\d{1,3}` captures always parse to `u32`, so the `if let` above always
        // returns when `SE` matches; reaching here would require that to change, in which case
        // falling through to the dash-code fallback (which won't match an SxxExx name) is correct.
    }
    // Fallback: the dash-delimited CONCATENATED SxxEyy code some packs use instead of "S04E09",
    // e.g. "Rick and Morty - 409 - Childrick of Mort.mkv" → S04E09 (last two digits = episode,
    // leading digit = season). Without this, such packs derive an empty `provides`, so the
    // reconciler thinks no episodes are owned and re-acquires the whole show as per-episode torrents
    // (permanent duplicates). EXACTLY 3 digits (season 1–9): this deliberately excludes 4-digit
    // tokens, which collide with years ("Show - 2024 - Title", date-named daily episodes
    // "... - 2021 - 03 - 14") and would mis-decode to a phantom (20, NN) episode. A show with ≥10
    // seasons using concatenated coding is rare and ambiguous with years anyway — rely on `SxxExx`
    // for those. The " - NNN - " (space-dash-space) anchor avoids resolution false positives.
    //
    // KNOWN LIMITATION — absolute (anime) numbering: a 3-digit token here is decoded as concatenated
    // SxxEyy, but absolute episode numbers are indistinguishable from it ("One Piece - 409 - …" is
    // absolute ep 409, not S4E9). For such releases this records a phantom (4,9) as `provides`,
    // which can suppress re-acquisition of the real S4E9 and write a wrong VFS selection slot. This
    // is an accepted trade-off: the common concatenated-pack case (Western shows) is far more
    // prevalent, and the prior behaviour (empty `provides` → whole-show re-acquire churn) was worse.
    static DASH_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s-\s(\d{3})\s-\s").unwrap());
    if let Some(c) = DASH_CODE.captures(name) {
        if let Some(code) = c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()) {
            let (season, episode) = (code / 100, code % 100);
            // Reject an implausible decode (episode 00) so a stray number can't masquerade.
            if season >= 1 && episode >= 1 {
                return vec![(season, episode)];
            }
        }
    }
    Vec::new()
}

use crate::now_unix_secs as now_secs;

/// Extract the numeric tmdb id from `MediaMetadata.external_id` (`"tmdb:1396"`).
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
    /// hash -> consecutive title-validation failures. `validate` makes a live TMDB call and returns
    /// `false` on a transient TMDB outage (indistinguishable from a genuine title mismatch), so a
    /// single failure must NOT immediately blacklist+delete a possibly-correct cached release — we
    /// require two CONSECUTIVE failures (≥2 scan ticks apart) before treating it as a real WrongTitle.
    /// A single-tick TMDB blip (incl. the first observe after a restart, when this map is empty) thus
    /// rides out; a genuine mismatch is still rejected one tick later. In-memory (best-effort).
    validate_fails: Arc<Mutex<HashMap<String, u32>>>,
}

/// The single target media file for a candidate: the addon's named/index file, else the largest
/// video. Used both to choose what to select and to identify the served file afterwards — the
/// latter matters because some providers (TorBox) auto-select *every* file, so "first selected"
/// is not the video (it could be a `.srt`/`.nfo`).
fn select_target<'a>(
    info: &'a TorrentInfo,
    file_hint: Option<&str>,
    file_idx: Option<usize>,
) -> Option<&'a crate::rd_client::TorrentFile> {
    if let Some(hint) = file_hint {
        let hint_base = hint.rsplit('/').next().unwrap_or(hint);
        if let Some(f) = info
            .files
            .iter()
            .find(|f| f.path.rsplit('/').next().unwrap_or(&f.path) == hint_base)
        {
            return Some(f);
        }
    }
    if let Some(idx) = file_idx {
        if let Some(f) = info.files.get(idx) {
            return Some(f);
        }
    }
    info.files
        .iter()
        .filter(|f| crate::vfs::is_video_file(&f.path))
        .max_by_key(|f| f.bytes)
}

/// Choose file ids to select for a candidate (see `select_target`).
fn select_file_ids(
    info: &TorrentInfo,
    file_hint: Option<&str>,
    file_idx: Option<usize>,
) -> Vec<u32> {
    select_target(info, file_hint, file_idx)
        .map(|f| vec![f.id])
        .unwrap_or_default()
}

/// Select file ids appropriate to the request kind: a single target video for a movie (so the
/// movie-pack guard can reject multi-feature packs), or ALL video files for a series (so a season
/// pack downloads fully on providers that don't auto-select, and `provides` covers every episode).
fn select_ids_for(
    kind: MediaKind,
    info: &TorrentInfo,
    hint: Option<&str>,
    idx: Option<usize>,
) -> Vec<u32> {
    match kind {
        MediaKind::Movie => select_file_ids(info, hint, idx),
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
pub(crate) fn episode_files(info: &TorrentInfo) -> Vec<(u32, u32, String)> {
    info.files
        .iter()
        .filter(|f| f.selected == 1 && crate::vfs::is_video_file(&f.path))
        .flat_map(|f| {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path);
            // A multi-episode file contributes ALL its episodes (so `provides` is complete and the
            // reconciler doesn't re-acquire a contained episode as a duplicate singleton).
            parse_se_all(name)
                .into_iter()
                .map(move |(s, e)| (s, e, f.path.clone()))
        })
        .collect()
}

/// Count feature-sized video files. A real single-movie release has exactly one; more than one
/// signals a multi-movie pack. The size floor excludes samples / extras / featurettes.
/// `pub(crate)` so the upgrade engine can apply the same movie-pack guard when staging.
pub(crate) fn count_feature_videos(info: &TorrentInfo) -> usize {
    const FEATURE_MIN_BYTES: u64 = 700_000_000;
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
const VERIFY_DEADLINE_SECS: u64 = 24 * 3600;

/// Per-hash deferred-probe state for transient (CDN-unavailable) probes: how many times we've
/// probed-and-deferred (to bound the fast burst) and when we last probed (to back off afterwards).
/// In-memory only — reset on restart; the hard deadline is anchored on the persisted `added_at`,
/// so a restart can't extend a release's grace period without bound.
#[derive(Clone, Copy)]
struct DeferState {
    attempts: u32,
    last_probe: Instant,
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
        }
    }

    /// Optimistically acquire `req`: scrape, rank, add the best non-blacklisted candidate, record
    /// it `Pending`, and return — WITHOUT synchronously selecting/validating/probing (that is
    /// `observe`'s job once the torrent's files resolve). A slow-to-seed release is therefore no
    /// longer judged or deleted prematurely. `provenance` is recorded and preserved across
    /// `observe`'s re-acquire (sticky).
    pub async fn acquire(&self, req: AcquireRequest, provenance: Provenance) -> AcquireOutcome {
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
        let ranked = release::rank(parsed, &self.prefs);
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

        // Live provider listing (lazily fetched once, only when an owned candidate is hit). Outer
        // Option = "fetched?", inner = "fetch succeeded?". Used to distinguish "already acquired"
        // from a lapsed/externally-deleted owned record that must be RE-added.
        let mut present: Option<Option<std::collections::HashSet<String>>> = None;
        for cand in ranked.into_iter().take(self.max_attempts as usize) {
            if self.store.get_owned(cand.info_hash.clone()).await.is_some() {
                if present.is_none() {
                    present = Some(self.provider.get_torrents().await.ok().map(|ts| {
                        ts.iter()
                            .map(|t| t.hash.to_ascii_lowercase())
                            .collect::<std::collections::HashSet<String>>()
                    }));
                }
                // Short-circuit as idempotent "already acquired" ONLY when the torrent is confirmed
                // PRESENT on the provider. If the listing is unavailable, preserve the old idempotent
                // behaviour (avoid spurious re-adds on a transient listing failure). If confirmed
                // ABSENT, fall through to re-add — the title lapsed (cache lost / externally deleted),
                // which is exactly the documented availability-re-acquire case.
                let confirmed_absent = match present.as_ref().expect("just fetched") {
                    Some(p) => !p.contains(&cand.info_hash.to_ascii_lowercase()),
                    None => false,
                };
                if !confirmed_absent {
                    debug!(
                        "acquire: tmdb {} — {} already owned + present (idempotent, no re-add)",
                        req.tmdb_id, cand.info_hash
                    );
                    return AcquireOutcome::Acquired(cand.info_hash.clone());
                }
                debug!(
                    "acquire: tmdb {} — owned {} is absent (lapsed) — re-adding",
                    req.tmdb_id, cand.info_hash
                );
            }
            debug!(
                "acquire: tmdb {} — adding {}",
                req.tmdb_id,
                release_summary(&cand)
            );
            let magnet = format!("magnet:?xt=urn:btih:{}", cand.info_hash);
            let added = match self.provider.add_magnet(&magnet).await {
                Ok(a) => a,
                Err(e) => {
                    warn!(
                        "add_magnet failed for {}: {} — trying next",
                        cand.info_hash, e
                    );
                    continue;
                }
            };
            // Record Pending immediately (the verdict belongs to observe).
            let provides = match (req.kind, req.season, req.episode) {
                (MediaKind::Series, Some(s), Some(e)) => vec![(s, e)],
                _ => vec![],
            };
            let _ = self
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
                .await;
            let _ = self
                .store
                .put_authoritative(cand.info_hash.clone(), req.metadata.clone())
                .await;
            // Best-effort: if the file list is already present (cached), select now so it is
            // immediately resolvable; otherwise observe selects once metadata resolves.
            //
            // Select WITHOUT the addon hint (None, None) so the chosen file matches what `observe`
            // later records/probes (`select_target(.., None, None)` = the largest video). A valid
            // single-feature movie's feature IS the largest (the pack-guard rejects multi-feature),
            // so this is correct; and it avoids the B10 mismatch where a misleading hint points at a
            // non-largest file (e.g. a sample) — acquire would select that while observe probes the
            // largest, leaving the largest unselected → a broken locator → a stuck Pending.
            if let Ok(info) = self.provider.get_torrent_info(&added.id).await {
                let ids = select_ids_for(req.kind, &info, None, None);
                if !ids.is_empty() {
                    let csv = ids
                        .iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    let _ = self.provider.select_files(&added.id, &csv).await;
                }
            }
            return AcquireOutcome::Pending(cand.info_hash);
        }
        debug!(
            "acquire: tmdb {} — no acceptable release ({} scraped, none passed filters/attempts)",
            req.tmdb_id,
            candidates.len()
        );
        AcquireOutcome::NoAcceptableRelease
    }

    /// Validate a file name against an expected title (used by the upgrade engine before staging).
    pub async fn validate_title(
        &self,
        file_name: &str,
        tmdb_id: u64,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> bool {
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
                VerifyResult::Defer
            }
        }
    }

    /// Write `provides` and the per-slot `selection` entries for a now-Verified owned torrent,
    /// and update its status. For a movie: one `movie_slot` entry for the selected file. For a
    /// show: `episode_slot` entries for every SE-mapped selected file, and `provides` is that set.
    async fn record_verified(
        &self,
        hash: &str,
        req: &AcquireRequest,
        info: &TorrentInfo,
        selected_path: &str,
    ) {
        match req.kind {
            MediaKind::Movie => {
                if let Some(id) = crate::vfs::tmdb_id_of(&req.metadata) {
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
            }
            MediaKind::Series => {
                let eps = episode_files(info);
                if let Some(id) = crate::vfs::tmdb_id_of(&req.metadata) {
                    for (s, e, path) in &eps {
                        let _ = self
                            .store
                            .put_selection(
                                crate::store::episode_slot(id, *s, *e),
                                crate::store::SelectionEntry {
                                    hash: hash.to_string(),
                                    file_path: path.clone(),
                                },
                            )
                            .await;
                    }
                }
                // Persist provides = the SE-mapped episode set (the churn fix).
                // Write provides + Verified in one put_owned so there is no intermediate
                // Pending+provides failure window.
                if let Some(mut rec) = self.store.get_owned(hash.to_string()).await {
                    rec.provides = eps.iter().map(|(s, e, _)| (*s, *e)).collect();
                    rec.status = OwnedStatus::Verified;
                    let _ = self.store.put_owned(hash.to_string(), rec).await;
                }
                return;
            }
        }
        let _ = self
            .store
            .set_owned_status(hash.to_string(), OwnedStatus::Verified)
            .await;
    }

    /// Called each scan tick with the current torrent list. Resolves optimistically-added Pending
    /// torrents (select files → pack-guard → title-validation → probe → Verified + provides +
    /// selection), reaps genuinely-dead/never-resolving ones after `dead_timeout`, and recovers by
    /// re-scraping. No scraping on the happy path.
    pub async fn observe(&self, torrents: &[crate::rd_client::Torrent]) {
        let owned = self.store.all_owned().await;
        // Key by lowercased provider hash so it matches the lowercased candidate hashes we store.
        // When the listing contains DUPLICATE entries for a hash (a repair leak or an external
        // re-add — exactly what the scan loop's dedup deletes), resolve each hash to its BEST entry:
        // prefer a `downloaded` torrent, and among equals keep the newest (the provider lists
        // newest-first, so the first-seen). A naive `.collect()` is last-write-wins and would resolve
        // to the OLDEST duplicate — which may be a stale `error`/`dead` entry the scan loop is about to
        // delete, wrongly driving `fail_and_reacquire` (blacklist + re-scrape) on a title that in fact
        // has a healthy downloaded copy.
        let mut by_hash: HashMap<String, &crate::rd_client::Torrent> = HashMap::new();
        for t in torrents {
            let key = t.hash.to_ascii_lowercase();
            match by_hash.get(&key) {
                Some(existing) if existing.status == "downloaded" => {} // best already kept
                Some(_) if t.status != "downloaded" => {} // both non-downloaded: keep the newer
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
        for (hash, rec) in &owned {
            let Some(t) = by_hash.get(hash.as_str()).copied() else {
                // Not in the listing. A Pending torrent that never registered/resolved is dead
                // once it has been waiting longer than the dead-timeout — but only trust "absent"
                // when the listing isn't wholesale empty.
                if rec.status == OwnedStatus::Pending {
                    let age = now_secs().saturating_sub(rec.added_at);
                    if age > self.dead_timeout.as_secs() && !listing_empty {
                        self.fail_and_reacquire(
                            hash,
                            "",
                            &rec.request,
                            "NeverResolved",
                            &rec.provenance,
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
                self.fail_and_reacquire(hash, &t.id, &rec.request, "Dead", &rec.provenance)
                    .await;
                continue;
            }
            if rec.status == OwnedStatus::Verified {
                self.progress.lock().await.remove(&t.id);
                continue;
            }
            // Pending: fetch info to inspect files.
            let info = match self.provider.get_torrent_info(&t.id).await {
                Ok(i) => i,
                Err(e) => {
                    debug!(
                        "observe: hash {} get_torrent_info({}) failed: {} — retry next tick",
                        hash, t.id, e
                    );
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
                    self.fail_and_reacquire(
                        hash,
                        &t.id,
                        &rec.request,
                        "NeverResolved",
                        &rec.provenance,
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
                // No candidate hint preserved in OwnedRecord; use the kind-appropriate fallback (largest video for movies, all videos for series).
                let ids = select_ids_for(rec.request.kind, &info, None, None);
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
                // If selection never takes effect across the whole dead-timeout window, the hash is
                // genuinely stuck (the provider lists files but won't honour select_files for it) —
                // reap + re-acquire rather than retrying selection forever, consistent with the
                // !has_files / missing-selected-path branches. A provider OUTAGE instead fails the
                // get_torrent_info above and skips this branch, so this only fires on a stuck hash.
                if now_secs().saturating_sub(rec.added_at) > self.dead_timeout.as_secs() {
                    self.fail_and_reacquire(
                        hash,
                        &t.id,
                        &rec.request,
                        "SelectStuck",
                        &rec.provenance,
                    )
                    .await;
                }
                continue; // re-inspect next tick after selection settles
            }
            // Movie-pack guard (deferred from acquire).
            if rec.request.kind == MediaKind::Movie && count_feature_videos(&info) > 1 {
                self.fail_and_reacquire(hash, &t.id, &rec.request, "MoviePack", &rec.provenance)
                    .await;
                continue;
            }
            // Choose the representative file to validate + probe. Movie: the feature video.
            // Series: the file matching the REQUESTED episode (validating a pack's largest file
            // against the requested (s,e) would misfire — a pack holds many episodes).
            let selected_path = match rec.request.kind {
                MediaKind::Movie => select_target(&info, None, None).map(|f| f.path.clone()),
                MediaKind::Series => episode_files(&info)
                    .into_iter()
                    .find(|(s, e, _)| {
                        Some(*s) == rec.request.season && Some(*e) == rec.request.episode
                    })
                    .map(|(_, _, p)| p),
            };
            let Some(selected_path) = selected_path else {
                // The requested episode isn't present (or no video resolved). Past the dead-timeout,
                // treat it as a wrong/incomplete pick and re-acquire; otherwise wait (metadata may
                // still be settling).
                if now_secs().saturating_sub(rec.added_at) > self.dead_timeout.as_secs() {
                    self.fail_and_reacquire(
                        hash,
                        &t.id,
                        &rec.request,
                        "EpisodeMissing",
                        &rec.provenance,
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
                    self.fail_and_reacquire(hash, &t.id, &rec.request, "Stalled", &rec.provenance)
                        .await;
                } else {
                    debug!(
                        "observe: tmdb {} hash {} downloading (status={:?}, progress={:.0}%) — waiting",
                        rec.request.tmdb_id, hash, t.status, t.progress
                    );
                }
                continue;
            }
            // Deferred-probe back-off + hard deadline. A downloaded torrent whose probe keeps
            // coming back Transient (CDN momentarily unavailable) is re-probed every tick for the
            // first MAX_VERIFY_ATTEMPTS (to catch a quick blip), then only on a back-off (so we
            // don't re-fetch 4 MB every tick). It is NEVER accepted unverified; if it stays
            // unfetchable past the deadline it is treated as effectively broken and replaced
            // (blacklist + re-acquire), bounded by the blacklist so it can't churn.
            let defer_state = self.verify_attempts.lock().await.get(hash).copied();
            if defer_state.is_some_and(|st| st.attempts >= MAX_VERIFY_ATTEMPTS) {
                if now_secs().saturating_sub(rec.added_at) > VERIFY_DEADLINE_SECS {
                    self.fail_and_reacquire(
                        hash,
                        &t.id,
                        &rec.request,
                        "ProbeUnfetchable",
                        &rec.provenance,
                    )
                    .await;
                    continue;
                }
                if defer_state.is_some_and(|st| st.last_probe.elapsed() < VERIFY_BACKOFF) {
                    debug!(
                        "observe: tmdb {} hash {} probe still deferring (attempts={}) — backing off",
                        rec.request.tmdb_id,
                        hash,
                        defer_state.map(|s| s.attempts).unwrap_or(0)
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
            // call, and a transient TMDB failure makes it return `false`, which would `fail_and_reacquire`
            // (blacklist + delete) an already-validated, correct, cached release. A probe only runs
            // after validation passes, so a recorded `defer_state` already implies a prior pass.
            if defer_state.is_none() {
                let ok = self
                    .validator
                    .validate(
                        &file_name,
                        rec.request.tmdb_id,
                        rec.request.kind,
                        rec.request.season,
                        rec.request.episode,
                    )
                    .await;
                if ok {
                    // Passed → clear any prior transient-failure count and fall through to the probe.
                    self.validate_fails.lock().await.remove(hash);
                } else {
                    // `validate` can't distinguish a transient TMDB outage from a real mismatch, so
                    // only blacklist+delete after TWO CONSECUTIVE failures (a single-tick blip — incl.
                    // the first observe after a restart — rides out); a genuine WrongTitle is rejected
                    // on the next tick. The record stays Pending (invisible — no selection written yet)
                    // meanwhile, so the one-tick delay is harmless.
                    let fails = {
                        let mut m = self.validate_fails.lock().await;
                        let c = m.entry(hash.to_string()).or_insert(0);
                        *c += 1;
                        *c
                    };
                    if fails >= 2 {
                        self.validate_fails.lock().await.remove(hash);
                        self.fail_and_reacquire(
                            hash,
                            &t.id,
                            &rec.request,
                            "WrongTitle",
                            &rec.provenance,
                        )
                        .await;
                    } else {
                        debug!(
                            "observe: tmdb {} hash {} title-validation failed ({}/2) — re-checking next tick (transient TMDB?)",
                            rec.request.tmdb_id, hash, fails
                        );
                    }
                    continue;
                }
            }
            let locator = locator_for(&info, hash, &selected_path);
            match self.verify_file(&locator, &rec.request).await {
                VerifyResult::Pass | VerifyResult::Accept => {
                    info!(
                        "observe: tmdb {} hash {} verified ({}) — recording selection",
                        rec.request.tmdb_id, hash, file_name
                    );
                    self.record_verified(hash, &rec.request, &info, &selected_path)
                        .await;
                    self.verify_attempts.lock().await.remove(hash);
                    self.progress.lock().await.remove(&t.id);
                }
                VerifyResult::Defer => {
                    // Record the deferred attempt + when we probed (drives the back-off/deadline
                    // gate above). Crucially we do NOT accept-unverified: the record stays Pending
                    // (so the upgrade engine's Verified gate skips it and the VFS keeps the prior
                    // selection), is re-probed on the back-off, and is replaced only past the
                    // deadline. This prevents freezing a wrong-language release into the library.
                    let mut m = self.verify_attempts.lock().await;
                    let st = m.entry(hash.to_string()).or_insert(DeferState {
                        attempts: 0,
                        last_probe: Instant::now(),
                    });
                    st.attempts += 1;
                    st.last_probe = Instant::now();
                    let attempts = st.attempts;
                    drop(m);
                    debug!(
                        "observe: tmdb {} hash {} probe deferred (attempt {}) — CDN transient, will re-probe",
                        rec.request.tmdb_id, hash, attempts
                    );
                }
                VerifyResult::Reject(reason) => {
                    self.fail_and_reacquire(hash, &t.id, &rec.request, reason, &rec.provenance)
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

    async fn fail_and_reacquire(
        &self,
        hash: &str,
        torrent_id: &str,
        req: &AcquireRequest,
        reason: &str,
        provenance: &Provenance,
    ) {
        warn!(
            "owned torrent {} failed ({}) — blacklist + re-acquire",
            hash, reason
        );
        let _ = self
            .store
            .blacklist_add(req.kind, req.tmdb_id, hash.to_string(), reason, now_secs())
            .await;
        // Delete the provider torrent BEFORE dropping the store records (the `execute_remove`
        // ordering). On a transient delete failure, KEEP the records so the next `observe` tick
        // retries the delete rather than leaving a present-but-untracked torrent that
        // `record_mirror_owned` would re-adopt as a duplicate. An empty `torrent_id` = genuinely
        // absent (nothing to delete). The blacklist above is also a hash-scoped backstop against
        // re-adoption.
        let removed =
            torrent_id.is_empty() || self.provider.delete_torrent(torrent_id).await.is_ok();
        if removed {
            let _ = self.store.remove_owned(hash.to_string()).await;
            let _ = self.store.remove_authoritative(hash.to_string()).await;
            // Drop any selection slots this hash represented so the VFS stops showing the dead release.
            for (slot, entry) in self.store.all_selection().await {
                if entry.hash.eq_ignore_ascii_case(hash) {
                    let _ = self.store.remove_selection(slot).await;
                }
            }
        }
        self.progress.lock().await.remove(torrent_id);
        self.verify_attempts.lock().await.remove(hash);
        // Sticky provenance: re-acquire preserves the failed record's origin (Trigger B correctness).
        let _ = self.acquire(req.clone(), provenance.clone()).await; // promotes the next candidate (bad hash now blacklisted)
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
    use redb::backends::InMemoryBackend;

    #[test]
    fn parse_se_handles_sxxexx_and_dash_concatenated_codes() {
        // Standard SxxExx → a single-element vec.
        assert_eq!(parse_se_all("Show.S04E09.1080p.mkv"), vec![(4, 9)]);
        assert_eq!(parse_se_all("Show S1E5.mkv"), vec![(1, 5)]);
        // Dash-concatenated 3-digit form ("409" = S04E09).
        assert_eq!(
            parse_se_all("Rick and Morty - 409 - Childrick of Mort.mkv"),
            vec![(4, 9)]
        );
        // Must NOT misread a resolution or year as an episode code.
        assert!(parse_se_all("Movie.2009.1080p.BluRay.mkv").is_empty());
        assert!(parse_se_all("Movie (1999).mkv").is_empty());
        // A 4-digit dash-delimited YEAR must NOT decode (would have been a phantom S20E24).
        assert!(parse_se_all("Show - 2024 - Title.mkv").is_empty());
        // A date-named daily-show file must NOT decode (no 3-digit dash token).
        assert!(parse_se_all("The Daily Show - 2021 - 03 - 14.mkv").is_empty());
        // Implausible decode rejected (episode 00).
        assert!(parse_se_all("Show - 400 - Title.mkv").is_empty());
        // No episode marker at all.
        assert!(parse_se_all("Just A Movie.mkv").is_empty());
    }

    #[test]
    fn parse_se_all_handles_multi_episode_files() {
        // Contiguous multi-episode files contribute ALL their episodes (so `provides` is complete and
        // the reconciler doesn't re-acquire a contained episode as a duplicate singleton).
        assert_eq!(
            parse_se_all("Show.S01E01E02.1080p.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(parse_se_all("Show.S01E01-E02.mkv"), vec![(1, 1), (1, 2)]);
        assert_eq!(
            parse_se_all("Show S02E03E04E05.mkv"),
            vec![(2, 3), (2, 4), (2, 5)]
        );
        // A resolution/codec token after the episode must NOT be parsed as a further episode (the
        // continuation requires an explicit `e` before each number).
        assert_eq!(parse_se_all("Show.S01E01.1080p.x265.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("Show.S01E07.720p.WEB.mkv"), vec![(1, 7)]);
    }

    #[test]
    fn select_target_no_hint_picks_largest_so_acquire_and_observe_agree() {
        // B10: `acquire` and `observe` must select the SAME movie file. Both now call
        // `select_target(.., None, None)` → the largest video (the feature). A misleading addon hint
        // pointing at a smaller sample WOULD have picked it (the old acquire behaviour), leaving the
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
        // No hint (what BOTH acquire and observe now use) → the feature (largest video).
        assert_eq!(select_target(&info, None, None).unwrap().id, 2);
        // A hint at the sample would have picked it — the divergence the fix removes.
        assert_eq!(
            select_target(&info, Some("Movie.2020.SAMPLE.mkv"), None)
                .unwrap()
                .id,
            1
        );
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
        Store::from_database(Arc::new(
            redb::Database::builder()
                .create_with_backend(InMemoryBackend::new())
                .unwrap(),
        ))
        .unwrap()
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
        ) -> bool {
            self.0
        }
    }
    /// Validator that returns `false` for its first `fail_first` calls, then `true` — simulates a
    /// transient TMDB outage that recovers (B5).
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
        ) -> bool {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            n >= self.fail_first
        }
    }
    struct CannedProber(Result<Vec<Track>, ProbeError>);
    #[async_trait]
    impl Prober for CannedProber {
        async fn probe(&self, _url: &str) -> Result<Vec<Track>, ProbeError> {
            self.0.clone()
        }
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
            provider_returning("downloaded", "h1"),
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
        assert_eq!(
            st.authoritative_meta("h1".into())
                .await
                .unwrap()
                .external_id
                .as_deref(),
            Some("tmdb:27205")
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
        // A torrent whose probe has been deferring since long before the verify deadline is treated
        // as effectively broken: blacklisted + re-acquired (not accepted, not left forever).
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
        // Fast burst first (probes every tick while attempts < MAX), then the deadline fires.
        for _ in 0..(MAX_VERIFY_ATTEMPTS + 1) {
            eng.observe(&torrents).await;
        }
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
    async fn observe_transient_validate_failure_recovers_without_blacklist() {
        // B5: validation fails on the first tick (transient TMDB outage) then passes on the second.
        // The record must NOT be blacklisted — it recovers and verifies. This is the restart window
        // (in-memory defer state lost → re-validate) made safe by the two-consecutive-failures rule.
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
            provider_returning("downloaded", "h2"),
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
}
