use crate::error::AppError;
use crate::rd_client::{AddMagnetResponse, Torrent, TorrentInfo};

/// Build a synthetic Bad Gateway `reqwest::Error` for cases where we must return a `reqwest::Error`
/// but have no live error response (exhausted retries that only produced deserialisation failures, or
/// an input we refuse to act on). Shared by both provider clients so the fallible construction lives
/// in exactly one place.
pub(crate) fn synthetic_bad_gateway(detail: &'static [u8]) -> reqwest::Error {
    reqwest::Response::from(
        hyper::Response::builder()
            .status(reqwest::StatusCode::BAD_GATEWAY)
            .body(hyper::body::Bytes::from_static(detail))
            .expect("static BAD_GATEWAY response always builds"),
    )
    .error_for_status()
    .expect_err("BAD_GATEWAY always yields an error status")
}

/// A 404 `reqwest::Error`, as TorBox's `mylist?id=` sometimes answers for a torrent that exists.
/// Shared by the tests that script such a lookup.
#[cfg(test)]
pub(crate) fn not_found_error() -> reqwest::Error {
    reqwest::Response::from(
        hyper::Response::builder()
            .status(reqwest::StatusCode::NOT_FOUND)
            .body(hyper::body::Bytes::new())
            .expect("an empty 404 response always builds"),
    )
    .error_for_status()
    .expect_err("404 is an error status")
}

/// Per-request timeout for the synchronous on-read resolve calls (RD `unrestrict/link`, TorBox
/// `requestdl`), overriding the clients' 60 s default. Both answer with a small JSON body in well
/// under a second, so this only bites an endpoint that accepts the connection and then hangs — which
/// would otherwise hold the WebDAV read for 3 attempts × 60 s.
pub(crate) const READ_PATH_REQUEST_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(15);

/// How many times a paginated provider listing is attempted while concurrent deletes keep shifting
/// its pages (see [`DeleteTracker`]) before it fails as an `Err` (callers skip the tick).
pub(crate) const MAX_LISTING_ATTEMPTS: u32 = 3;

/// Counts this process's torrent deletes so a multi-page provider listing can tell whether one may
/// have landed while it was paging. Both providers page by position (RD `page`, TorBox `offset`), so
/// deleting an item on an already-fetched page shifts every later item back by one and the first
/// item of the next page is never returned — an `Ok` listing silently missing a live torrent, which
/// downstream reads as lapsed (re-acquire), absent (a staging presence check) or gone (a mirror
/// reap). The upgrade, reconcile and repair paths delete while other jobs list, so the clients
/// re-list when this reports a possible overlap. Deletes made outside this process (the provider's
/// web UI) can't be seen; this only covers our own.
#[derive(Debug, Default)]
pub(crate) struct DeleteTracker {
    started: std::sync::atomic::AtomicU64,
    in_flight: std::sync::atomic::AtomicUsize,
}

/// Marks one delete request as in flight; dropping it (on completion or cancellation) clears that.
pub(crate) struct DeleteInFlight<'a>(&'a DeleteTracker);

impl Drop for DeleteInFlight<'_> {
    fn drop(&mut self) {
        self.0
            .in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The tracker's state when a listing started; see [`DeleteTracker::changed_since`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteSnapshot {
    started: u64,
    in_flight: bool,
}

impl DeleteTracker {
    /// Call before sending a delete and hold the guard until the request is done.
    pub(crate) fn begin(&self) -> DeleteInFlight<'_> {
        use std::sync::atomic::Ordering::SeqCst;
        self.in_flight.fetch_add(1, SeqCst);
        self.started.fetch_add(1, SeqCst);
        DeleteInFlight(self)
    }

    /// Take before fetching the first page.
    pub(crate) fn snapshot(&self) -> DeleteSnapshot {
        use std::sync::atomic::Ordering::SeqCst;
        DeleteSnapshot {
            started: self.started.load(SeqCst),
            in_flight: self.in_flight.load(SeqCst) > 0,
        }
    }

    /// Whether a delete may have taken effect since `since`: one started after it, or one was
    /// already in flight then (and so may have landed mid-listing).
    pub(crate) fn changed_since(&self, since: DeleteSnapshot) -> bool {
        since.in_flight || self.started.load(std::sync::atomic::Ordering::SeqCst) != since.started
    }
}

/// The set of transient 5xx statuses worth retrying — 500/502/503/504. The DETERMINISTIC 5xx (501
/// Not Implemented / 505 HTTP Version Not Supported) are excluded, since a retry can't fix them.
/// Shared by both provider clients so the retryable set can't silently diverge between them.
pub(crate) fn is_retryable_5xx(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    )
}

/// Identifies a single media file for resolution. Stable identity is
/// `(hash, file_path)`; `torrent_id`/`file_id`/`link` are re-derivable (e.g. after
/// a re-acquire). `link` is the provider's per-file restricted link when it has
/// one (Real-Debrid); `None` for providers that resolve by `(torrent_id, file_id)`
/// (TorBox).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct FileLocator {
    pub hash: String,
    pub torrent_id: String,
    pub file_id: u32,
    pub file_path: String,
    pub link: Option<String>,
}

// Hand-written so a stray `{:?}` (e.g. inside a `Debug`-deriving struct or an error log) can never
// leak the Real-Debrid restricted-capability `link`; it is printed as a presence flag only. This
// matches the redaction both provider clients apply in their own `Debug` impls.
impl std::fmt::Debug for FileLocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileLocator")
            .field("hash", &self.hash)
            .field("torrent_id", &self.torrent_id)
            .field("file_id", &self.file_id)
            .field("file_path", &self.file_path)
            .field("link", &self.link.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Abstraction over a debrid provider. Real-Debrid and TorBox both implement it; exactly one is
/// active per deployment (chosen at startup by `choose_provider`).
///
/// Beyond the basic torrent operations it exposes the provider-neutral file-resolution surface
/// (`resolve_url`/`invalidate`) the VFS and repair paths depend on, so no component needs to know
/// which concrete client is behind the `Arc<dyn DebridProvider>`.
///
/// `Debug` is a supertrait so an `Arc<dyn DebridProvider>` can live inside `Debug`-deriving
/// structs like `RepairManager`.
#[async_trait::async_trait]
pub trait DebridProvider: Send + Sync + std::fmt::Debug {
    /// Stable, human-readable provider identifier (e.g. "real-debrid").
    fn name(&self) -> &'static str;

    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error>;
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error>;
    async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error>;
    async fn select_files(&self, torrent_id: &str, file_ids: &str) -> Result<(), reqwest::Error>;
    async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error>;

    /// Resolve a file to a streamable CDN URL. Returns `AppError::Unavailable` when the
    /// file's bytes are not currently available (any 5xx, a transport-level failure, OR a
    /// stale/expired-link 4xx such as 404 — all recoverable by a re-add-by-hash) so callers can
    /// trigger re-acquire/repair. A permanent auth 4xx (401/403) and throttling (429) stay a hard
    /// `AppError::Http`: neither is fixed by a re-add, so the read fails without repair.
    async fn resolve_url(&self, loc: &FileLocator) -> Result<String, crate::error::AppError>;

    /// Drop any cached resolution for `loc` (RD: the unrestrict-cache entry for its link).
    async fn invalidate(&self, loc: &FileLocator);
}

/// `get_torrent_info`, retrying a 404 after each wait in `waits` and returning the last 404 once they
/// are used up. Any other error is returned at once: the provider clients already retry transport
/// errors, 429s and transient 5xx.
///
/// TorBox's `mylist?id=` sometimes answers 404 ("No torrents found for this user") for a torrent that
/// exists, including one `createtorrent` has just returned. On Real-Debrid a 404 means the torrent is
/// gone, so a caller on a latency-sensitive path keeps `waits` short.
///
/// Public because the live `integration_test` looks torrents up through it too.
pub async fn torrent_info_retrying_not_found(
    provider: &dyn DebridProvider,
    id: &str,
    waits: &[std::time::Duration],
) -> Result<TorrentInfo, reqwest::Error> {
    let mut waits = waits.iter();
    loop {
        match provider.get_torrent_info(id).await {
            Err(e) if e.status() == Some(reqwest::StatusCode::NOT_FOUND) => {
                let Some(wait) = waits.next() else {
                    return Err(e);
                };
                tracing::debug!(
                    "torrent {} not found; looking again in {}ms",
                    id,
                    wait.as_millis()
                );
                tokio::time::sleep(*wait).await;
            }
            result => return result,
        }
    }
}

/// Which provider the service should run against this deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    RealDebrid,
    TorBox,
}

/// Decide the active provider from the two optional tokens. Exactly one must be
/// set (non-blank). Both-set or neither-set is a configuration error.
pub fn choose_provider(
    rd_token: Option<String>,
    torbox_token: Option<String>,
) -> Result<(ProviderKind, String), AppError> {
    let rd = rd_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tb = torbox_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match (rd, tb) {
        (Some(_), Some(_)) => Err(AppError::Config(
            "Set only one of RD_API_TOKEN or TORBOX_API_KEY, not both".to_string(),
        )),
        (Some(token), None) => Ok((ProviderKind::RealDebrid, token)),
        (None, Some(token)) => Ok((ProviderKind::TorBox, token)),
        (None, None) => Err(AppError::Config(
            "Set one of RD_API_TOKEN or TORBOX_API_KEY".to_string(),
        )),
    }
}

/// Test-only in-memory provider. Returns configured canned values; unconfigured
/// methods return `Default`s or are no-ops. Not compiled into release builds.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MockProvider {
    pub torrents: Vec<Torrent>,
    pub torrent_info: Option<TorrentInfo>,
    pub add_magnet: Option<AddMagnetResponse>,
    pub resolved_url: Option<String>,
    /// Counts `invalidate` calls so tests can assert whether the cached resolution was dropped.
    pub invalidate_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Torrent ids whose `resolve_url` should return `Unavailable` (simulates a broken torrent),
    /// letting tests exercise the repair-on-`Unavailable` path while other ids resolve normally.
    pub unavailable_torrent_ids: std::collections::HashSet<String>,
    /// Records every `delete_torrent` id so tests can assert that leaked torrents are cleaned up.
    pub deleted: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Records the infohash of every `add_magnet` call (in call order), so tests can assert exactly
    /// which candidates were staged — and that one was never added.
    pub added: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// When `true`, `get_torrents` returns a `reqwest::Error` so tests can exercise the
    /// early-return guard in `reconcile_wanted` / `monitor_episodes`.
    pub fail_get_torrents: bool,
    /// When `Some(n)`, the first `n` `get_torrents` calls succeed and every later one fails (as with
    /// `fail_get_torrents`), so a test can fail a LATER listing — e.g. the upgrade engine's post-stage
    /// prune listing — while an earlier one (its pre-add presence check) succeeds.
    pub fail_get_torrents_after: Option<usize>,
    /// Counts `get_torrents` calls (drives `fail_get_torrents_after`).
    pub get_torrents_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// When `true`, `delete_torrent` returns a `reqwest::Error` (still recording the attempted id)
    /// so tests can exercise the keep-the-owned-record-for-retry path in `execute_remove`.
    pub fail_delete: bool,
    /// Per-torrent-id `TorrentInfo` responses. When non-empty, `add_magnet` derives the returned id
    /// from the magnet's infohash and `get_torrent_info` looks the id up here (falling back to the
    /// single `torrent_info`). Lets a test stage several candidates that each resolve differently —
    /// required to exercise the upgrade engine's in-tick candidate fall-through.
    pub torrent_info_by_id: std::collections::HashMap<String, TorrentInfo>,
    /// Torrent ids whose `get_torrent_info` returns a `reqwest::Error` (a transient failure), so a
    /// test can drive `stage_and_verify`'s `Deferred` classification for one specific staged candidate.
    pub fail_info_ids: std::collections::HashSet<String>,
    /// Infohashes whose `add_magnet` returns a `reqwest::Error`, so a test can drive the acquisition
    /// engine's "add failed → try the next candidate" fall-through for one specific candidate.
    pub fail_add_magnet_hashes: std::collections::HashSet<String>,
    /// Per-torrent-id resolved URL. When a locator's `torrent_id` is present here, `resolve_url`
    /// returns that URL instead of the flat `resolved_url` — letting a test resolve the ORIGINAL and
    /// the REPAIRED torrent to different CDN endpoints (e.g. the original 5xxs, the repaired serves).
    pub resolved_url_by_torrent_id: std::collections::HashMap<String, String>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl DebridProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        let call = self
            .get_torrents_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_get_torrents || self.fail_get_torrents_after.is_some_and(|ok| call >= ok) {
            // Drive an immediate URL-parse failure to produce a reqwest::Error without any I/O.
            return reqwest::Client::new().get("").send().await.map(|_| vec![]);
        }
        Ok(self.torrents.clone())
    }
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        if self.fail_info_ids.contains(id) {
            // Drive an immediate URL-parse failure to produce a reqwest::Error without any I/O
            // (mirrors the fail_get_torrents path), so stage_and_verify classifies it as Deferred.
            return reqwest::Client::new()
                .get("")
                .send()
                .await
                .map(|_| TorrentInfo::default());
        }
        if let Some(ti) = self.torrent_info_by_id.get(id) {
            return Ok(ti.clone());
        }
        Ok(self.torrent_info.clone().unwrap_or_default())
    }
    async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        let hash = magnet.rsplit("urn:btih:").next().unwrap_or("").to_string();
        self.added.lock().unwrap().push(hash.clone());
        if self.fail_add_magnet_hashes.contains(&hash) {
            // Drive an immediate URL-parse failure to produce a reqwest::Error without any I/O,
            // so the acquisition loop takes its "add failed → try next candidate" branch.
            return reqwest::Client::new()
                .get("")
                .send()
                .await
                .map(|_| AddMagnetResponse::default());
        }
        if !self.torrent_info_by_id.is_empty() {
            // Per-candidate mode: id = the magnet's infohash, so each staged candidate resolves to
            // its own `torrent_info_by_id` entry.
            return Ok(AddMagnetResponse {
                id: hash,
                uri: String::new(),
            });
        }
        Ok(self.add_magnet.clone().unwrap_or_default())
    }
    async fn select_files(&self, _torrent_id: &str, _file_ids: &str) -> Result<(), reqwest::Error> {
        Ok(())
    }
    async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
        self.deleted.lock().unwrap().push(torrent_id.to_string());
        if self.fail_delete {
            // Drive an immediate URL-parse failure to produce a reqwest::Error without any I/O.
            return reqwest::Client::new().get("").send().await.map(|_| ());
        }
        Ok(())
    }
    async fn resolve_url(&self, loc: &FileLocator) -> Result<String, crate::error::AppError> {
        if self.unavailable_torrent_ids.contains(&loc.torrent_id) {
            return Err(crate::error::AppError::Unavailable);
        }
        if let Some(u) = self.resolved_url_by_torrent_id.get(&loc.torrent_id) {
            return Ok(u.clone());
        }
        match &self.resolved_url {
            Some(u) => Ok(u.clone()),
            None => Err(crate::error::AppError::Unavailable),
        }
    }
    async fn invalidate(&self, _loc: &FileLocator) {
        self.invalidate_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rd_client::RealDebridClient;
    use std::sync::Arc;

    #[test]
    fn delete_tracker_flags_deletes_that_overlap_a_listing() {
        let t = DeleteTracker::default();
        // Nothing happened: an unchanged tracker lets the listing stand.
        let quiet = t.snapshot();
        assert!(!t.changed_since(quiet));

        // A delete that starts (and even finishes) after the snapshot is flagged.
        let before = t.snapshot();
        drop(t.begin());
        assert!(t.changed_since(before));

        // A delete already in flight when the listing starts may land mid-listing, so it is flagged
        // even though no new delete starts afterwards.
        let in_flight = t.begin();
        let during = t.snapshot();
        assert!(t.changed_since(during));
        drop(in_flight);

        // Once it has finished, a fresh snapshot is clean again.
        let after = t.snapshot();
        assert!(!t.changed_since(after));
    }

    /// Answers the first `misses` lookups with a 404 (or, with `fail_other`, every lookup with a
    /// non-404 error), then with a default `TorrentInfo`; `calls` counts lookups.
    #[derive(Debug, Default)]
    struct NotFoundAtFirst {
        misses: usize,
        fail_other: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl DebridProvider for NotFoundAtFirst {
        fn name(&self) -> &'static str {
            "not-found-at-first"
        }
        async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
            Ok(vec![])
        }
        async fn get_torrent_info(&self, _id: &str) -> Result<TorrentInfo, reqwest::Error> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_other {
                return Err(synthetic_bad_gateway(b"down"));
            }
            if n < self.misses {
                return Err(not_found_error());
            }
            Ok(TorrentInfo::default())
        }
        async fn add_magnet(&self, _m: &str) -> Result<AddMagnetResponse, reqwest::Error> {
            Ok(AddMagnetResponse::default())
        }
        async fn select_files(&self, _t: &str, _f: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn delete_torrent(&self, _t: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn resolve_url(&self, _l: &FileLocator) -> Result<String, AppError> {
            Err(AppError::Unavailable)
        }
        async fn invalidate(&self, _l: &FileLocator) {}
    }

    const WAITS: [std::time::Duration; 2] = [
        std::time::Duration::from_millis(500),
        std::time::Duration::from_secs(1),
    ];

    #[tokio::test(start_paused = true)]
    async fn torrent_info_lookup_retries_a_404_after_each_wait() {
        let p = NotFoundAtFirst {
            misses: 2,
            ..Default::default()
        };
        let start = tokio::time::Instant::now();

        assert!(torrent_info_retrying_not_found(&p, "t", &WAITS)
            .await
            .is_ok());

        assert_eq!(p.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(start.elapsed(), std::time::Duration::from_millis(1_500));
    }

    #[tokio::test(start_paused = true)]
    async fn torrent_info_lookup_returns_the_404_once_the_waits_are_used_up() {
        let p = NotFoundAtFirst {
            misses: usize::MAX,
            ..Default::default()
        };

        let err = torrent_info_retrying_not_found(&p, "t", &WAITS)
            .await
            .unwrap_err();

        assert_eq!(err.status(), Some(reqwest::StatusCode::NOT_FOUND));
        assert_eq!(p.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// The clients already retry transport errors, 429s and 5xx, so only a 404 is retried here.
    #[tokio::test(start_paused = true)]
    async fn torrent_info_lookup_does_not_retry_other_errors() {
        let p = NotFoundAtFirst {
            fail_other: true,
            ..Default::default()
        };
        let start = tokio::time::Instant::now();

        assert!(torrent_info_retrying_not_found(&p, "t", &WAITS)
            .await
            .is_err());

        assert_eq!(p.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    }

    #[test]
    fn real_debrid_client_is_a_debrid_provider() {
        let client = RealDebridClient::new("fake-token".to_string()).unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(client);
        assert_eq!(provider.name(), "real-debrid");
    }

    #[tokio::test]
    async fn mock_provider_returns_canned_values() {
        let invalidate_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock = MockProvider {
            torrents: vec![Torrent {
                id: "t1".to_string(),
                ..Default::default()
            }],
            invalidate_calls: invalidate_calls.clone(),
            ..Default::default()
        };
        let provider: Arc<dyn DebridProvider> = Arc::new(mock);
        assert_eq!(provider.name(), "mock");
        let torrents = provider.get_torrents().await.unwrap();
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].id, "t1");
        // Methods with no canned value return defaults / no-ops.
        assert_eq!(provider.get_torrent_info("x").await.unwrap().id, "");
        provider.invalidate(&FileLocator::default()).await;
        assert_eq!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "invalidate must record the call (not a silent no-op)"
        );
    }

    #[test]
    fn file_locator_constructs_and_clones() {
        let loc = FileLocator {
            hash: "abc".to_string(),
            torrent_id: "t1".to_string(),
            file_id: 10,
            file_path: "Movie/Movie.mkv".to_string(),
            link: Some("https://rd/restricted".to_string()),
        };
        let cloned = loc.clone();
        assert_eq!(cloned, loc);
        assert_eq!(cloned.file_id, 10);
        assert_eq!(cloned.link.as_deref(), Some("https://rd/restricted"));
    }

    #[test]
    fn file_locator_debug_redacts_the_restricted_link() {
        let loc = FileLocator {
            hash: "abc".to_string(),
            torrent_id: "t1".to_string(),
            file_id: 10,
            file_path: "Movie/Movie.mkv".to_string(),
            link: Some("https://rd/restricted-capability-secret".to_string()),
        };
        let dbg = format!("{loc:?}");
        assert!(
            !dbg.contains("restricted-capability-secret"),
            "the restricted RD link must never appear in Debug output: {dbg}"
        );
        assert!(dbg.contains("<redacted>"));
        // A None link is shown as None, not as a redaction marker.
        let none = FileLocator { link: None, ..loc };
        assert!(format!("{none:?}").contains("link: None"));
    }

    #[test]
    fn choose_provider_rd_only() {
        let (kind, token) = choose_provider(Some("rd-token".to_string()), None).unwrap();
        assert_eq!(kind, ProviderKind::RealDebrid);
        assert_eq!(token, "rd-token");
    }

    #[test]
    fn choose_provider_torbox_only() {
        let (kind, token) = choose_provider(None, Some("tb-token".to_string())).unwrap();
        assert_eq!(kind, ProviderKind::TorBox);
        assert_eq!(token, "tb-token");
    }

    #[test]
    fn choose_provider_both_set_is_error() {
        let err = choose_provider(Some("a".to_string()), Some("b".to_string()));
        assert!(err.is_err());
    }

    #[test]
    fn choose_provider_neither_set_is_error() {
        assert!(choose_provider(None, None).is_err());
    }

    #[test]
    fn choose_provider_treats_blank_token_as_unset() {
        // Whitespace-only RD token + real TorBox token → TorBox, not "both set".
        let (kind, _) = choose_provider(Some("   ".to_string()), Some("tb".to_string())).unwrap();
        assert_eq!(kind, ProviderKind::TorBox);
    }

    #[tokio::test]
    async fn mock_resolve_url_returns_configured_value() {
        let mock = MockProvider {
            resolved_url: Some("https://cdn/file".to_string()),
            ..Default::default()
        };
        let provider: Arc<dyn DebridProvider> = Arc::new(mock);
        let loc = FileLocator {
            hash: "h".to_string(),
            torrent_id: "t".to_string(),
            file_id: 1,
            file_path: "f.mkv".to_string(),
            link: None,
        };
        assert_eq!(
            provider.resolve_url(&loc).await.unwrap(),
            "https://cdn/file"
        );
        provider.invalidate(&loc).await; // records the call; must not panic
    }

    #[tokio::test]
    async fn mock_resolve_url_unavailable_by_default() {
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let loc = FileLocator::default();
        assert!(matches!(
            provider.resolve_url(&loc).await,
            Err(crate::error::AppError::Unavailable)
        ));
    }
}
