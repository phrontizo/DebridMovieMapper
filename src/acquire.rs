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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::warn;

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
            matches!(
                (season, episode, parse_se(file_name)),
                (Some(s), Some(e), Some((fs, fe))) if fs == s && fe == e
            )
        } else {
            true
        }
    }
}

fn parse_se(name: &str) -> Option<(u32, u32)> {
    use regex::Regex;
    use std::sync::LazyLock;
    static SE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)s(\d{1,2})e(\d{1,3})").unwrap());
    let c = SE.captures(name)?;
    Some((
        c.get(1)?.as_str().parse().ok()?,
        c.get(2)?.as_str().parse().ok()?,
    ))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    /// hash -> consecutive deferred-probe count, to bound re-probing of stuck-Pending torrents.
    verify_attempts: Arc<Mutex<HashMap<String, u32>>>,
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
fn select_file_ids(info: &TorrentInfo, file_hint: Option<&str>, file_idx: Option<usize>) -> Vec<u32> {
    select_target(info, file_hint, file_idx)
        .map(|f| vec![f.id])
        .unwrap_or_default()
}

/// Select file ids appropriate to the request kind: a single target video for a movie (so the
/// movie-pack guard can reject multi-feature packs), or ALL video files for a series (so a season
/// pack downloads fully on providers that don't auto-select, and `provides` covers every episode).
fn select_ids_for(kind: MediaKind, info: &TorrentInfo, hint: Option<&str>, idx: Option<usize>) -> Vec<u32> {
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
        .filter_map(|f| {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path);
            parse_se(name).map(|(s, e)| (s, e, f.path.clone()))
        })
        .collect()
}

/// Count feature-sized video files. A real single-movie release has exactly one; more than one
/// signals a multi-movie pack. The size floor excludes samples / extras / featurettes.
fn count_feature_videos(info: &TorrentInfo) -> usize {
    const FEATURE_MIN_BYTES: u64 = 700_000_000;
    info.files
        .iter()
        .filter(|f| crate::vfs::is_video_file(&f.path) && f.bytes >= FEATURE_MIN_BYTES)
        .count()
}

/// Build a FileLocator for `path` within `info` (pairs the per-file link by position among selected).
fn locator_for(info: &TorrentInfo, hash: &str, path: &str) -> FileLocator {
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

enum VerifyResult {
    Pass,
    Accept,
    Reject(&'static str),
    Defer,
}

/// Max consecutive deferred probes for a downloaded-but-Pending torrent before `observe`
/// stops re-probing it and accepts it unverified (bounds transient-CDN probe retries).
const MAX_VERIFY_ATTEMPTS: u32 = 5;

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
        let mut parsed: Vec<ReleaseInfo> = Vec::new();
        for c in &candidates {
            let r = release::parse(c);
            if self.store.is_blacklisted(req.tmdb_id, r.info_hash.clone()).await {
                continue;
            }
            parsed.push(r);
        }
        let ranked = release::rank(parsed, &self.prefs);

        for cand in ranked.into_iter().take(self.max_attempts as usize) {
            if self.store.get_owned(cand.info_hash.clone()).await.is_some() {
                return AcquireOutcome::Acquired(cand.info_hash.clone()); // idempotent
            }
            let magnet = format!("magnet:?xt=urn:btih:{}", cand.info_hash);
            let added = match self.provider.add_magnet(&magnet).await {
                Ok(a) => a,
                Err(e) => {
                    warn!("add_magnet failed for {}: {} — trying next", cand.info_hash, e);
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
            if let Ok(info) = self.provider.get_torrent_info(&added.id).await {
                let ids = select_ids_for(req.kind, &info, cand.file_name.as_deref(), cand.file_idx);
                if !ids.is_empty() {
                    let csv = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
                    let _ = self.provider.select_files(&added.id, &csv).await;
                }
            }
            return AcquireOutcome::Pending(cand.info_hash);
        }
        AcquireOutcome::NoAcceptableRelease
    }

    async fn verify_file(&self, locator: &FileLocator, req: &AcquireRequest) -> VerifyResult {
        let url = match self.provider.resolve_url(locator).await {
            Ok(u) => u,
            Err(_) => return VerifyResult::Defer, // can't reach it now; observe retries
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
            Err(ProbeError::Transient) => VerifyResult::Defer,
        }
    }

    /// Called each scan tick with the current torrent list. Probes completed Pending owned
    /// torrents, and re-acquires owned torrents that have stalled/died/failed verification.
    pub async fn observe(&self, torrents: &[crate::rd_client::Torrent]) {
        let owned = self.store.all_owned().await;
        // Key by lowercased provider hash so it matches the lowercased candidate hashes we store.
        let by_hash: HashMap<String, &crate::rd_client::Torrent> = torrents
            .iter()
            .map(|t| (t.hash.to_ascii_lowercase(), t))
            .collect();

        for (hash, rec) in &owned {
            let Some(t) = by_hash.get(hash.as_str()).copied() else {
                continue; // not in the current account listing; leave it
            };
            let dead = matches!(t.status.as_str(), "magnet_error" | "dead" | "error" | "virus");
            if dead {
                self.fail_and_reacquire(hash, &t.id, &rec.request, "Dead", &rec.provenance).await;
                continue;
            }
            if t.status == "downloaded" {
                if rec.status == OwnedStatus::Pending {
                    self.verify_pending(hash, &t.id, &rec.request, &rec.provenance).await;
                }
                self.progress.lock().await.remove(&t.id);
                continue;
            }
            // still downloading — stall check
            if self.is_stalled(&t.id, t.progress).await {
                self.fail_and_reacquire(hash, &t.id, &rec.request, "Stalled", &rec.provenance).await;
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

    async fn verify_pending(
        &self,
        hash: &str,
        torrent_id: &str,
        req: &AcquireRequest,
        provenance: &Provenance,
    ) {
        // Bound re-probing: once we've deferred MAX_VERIFY_ATTEMPTS times, stop probing.
        if self.verify_attempts.lock().await.get(hash).copied().unwrap_or(0) >= MAX_VERIFY_ATTEMPTS {
            return;
        }
        let info = match self.provider.get_torrent_info(torrent_id).await {
            Ok(i) => i,
            Err(_) => return,
        };
        // No candidate hint here (we work from the owned record), so fall back to the largest video.
        let Some(path) = select_target(&info, None, None).map(|f| f.path.clone()) else {
            return;
        };
        let locator = locator_for(&info, hash, &path);
        match self.verify_file(&locator, req).await {
            VerifyResult::Pass | VerifyResult::Accept => {
                let _ = self
                    .store
                    .set_owned_status(hash.to_string(), OwnedStatus::Verified)
                    .await;
                self.verify_attempts.lock().await.remove(hash);
            }
            VerifyResult::Defer => {
                let n = {
                    let mut m = self.verify_attempts.lock().await;
                    let n = m.entry(hash.to_string()).or_insert(0);
                    *n += 1;
                    *n
                };
                if n >= MAX_VERIFY_ATTEMPTS {
                    warn!(
                        "giving up verifying {} after {} deferred probes; accepting unverified",
                        hash, n
                    );
                    let _ = self
                        .store
                        .set_owned_status(hash.to_string(), OwnedStatus::Verified)
                        .await;
                }
            }
            VerifyResult::Reject(reason) => {
                self.verify_attempts.lock().await.remove(hash);
                self.fail_and_reacquire(hash, torrent_id, req, reason, provenance).await;
            }
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
        warn!("owned torrent {} failed ({}) — blacklist + re-acquire", hash, reason);
        let _ = self
            .store
            .blacklist_add(req.tmdb_id, hash.to_string(), reason, now_secs())
            .await;
        let _ = self.store.remove_owned(hash.to_string()).await;
        let _ = self.store.remove_authoritative(hash.to_string()).await;
        let _ = self.provider.delete_torrent(torrent_id).await;
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
        async fn validate(&self, _f: &str, _t: u64, _k: MediaKind, _s: Option<u32>, _e: Option<u32>) -> bool {
            self.0
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
        AcquisitionEngine::new(provider, scraper, validator, prober, store, prefs(), 5, Duration::from_secs(1800), Duration::from_secs(600))
    }

    #[tokio::test]
    async fn acquire_records_pending_and_quality_optimistically() {
        let st = store();
        let scraper = Arc::new(MockScraper { candidates: vec![cand("h1", true)] });
        let eng = engine(
            provider_returning("downloaded", "h1"),
            scraper,
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let out = eng.acquire(req(), Provenance::watchlist("alice")).await;
        assert_eq!(out, AcquireOutcome::Pending("h1".into()), "acquire is optimistic: always Pending");
        let rec = st.get_owned("h1".into()).await.unwrap();
        assert_eq!(rec.status, OwnedStatus::Pending);
        assert_eq!(rec.provenance, Provenance::watchlist("alice"));
        assert!(rec.quality.unwrap().cached, "cached candidate's quality recorded");
        assert_eq!(
            st.authoritative_meta("h1".into()).await.unwrap().external_id.as_deref(),
            Some("tmdb:27205")
        );
    }

    #[tokio::test]
    async fn acquire_idempotent_when_already_owned() {
        let st = store();
        st.put_owned("h1".into(), OwnedRecord {
            request: req(), provenance: Provenance::manual(), added_at: 1,
            status: OwnedStatus::Verified, provides: vec![], quality: None,
        }).await.unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![cand("h1", true)] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert_eq!(eng.acquire(req(), Provenance::manual()).await, AcquireOutcome::Acquired("h1".into()));
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
        assert_eq!(eng.acquire(req(), Provenance::manual()).await, AcquireOutcome::NoAcceptableRelease);
    }

    #[tokio::test]
    async fn observe_caps_deferred_probes_and_accepts() {
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
        let scraper = Arc::new(MockScraper { candidates: vec![] });
        let prober = Arc::new(CannedProber(Err(ProbeError::Transient))); // always defers
        let eng = engine(
            provider_returning("downloaded", "h1"),
            scraper,
            Arc::new(OkValidator(true)),
            prober,
            st.clone(),
        );
        let torrents = vec![crate::rd_client::Torrent {
            id: "tid_h1".into(),
            hash: "h1".into(),
            status: "downloaded".into(),
            progress: 100.0,
            ..Default::default()
        }];
        for _ in 0..MAX_VERIFY_ATTEMPTS {
            eng.observe(&torrents).await;
        }
        assert_eq!(
            st.get_owned("h1".into()).await.unwrap().status,
            OwnedStatus::Verified,
            "should accept unverified after MAX deferred probes"
        );
    }
}
