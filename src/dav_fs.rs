use crate::provider::DebridProvider;
use crate::repair::RepairManager;
use crate::vfs::{DebridVfs, VfsNode};
use bytes::Bytes;
use dav_server::davpath::DavPath;
use dav_server::fs::*;
use futures_util::FutureExt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

/// 2 MB read-ahead buffer per open file. Exported so the WebDAV handler can be told to request
/// reads in `BUFFER_SIZE` chunks (`read_buf_size`): without that, dav-server drains each 2 MB buffer
/// via ~128 separate 16 KB `read_bytes` calls, taking the per-read locks (repair health, read
/// activity) ~128× more often than the read-ahead design intends.
pub const BUFFER_SIZE: usize = 2 * 1024 * 1024;

/// Maximum single CDN fetch to prevent unbounded memory growth (16 MB)
pub const MAX_FETCH_SIZE: usize = 16 * 1024 * 1024;

/// Parse the start byte from a `Content-Range: bytes <start>-<end>/<total>` header value.
/// `None` for an absent/unparseable value (callers then tolerate it rather than reject).
fn parse_content_range_start(v: &str) -> Option<u64> {
    v.trim()
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// The `DebridVfs::timestamps` key for `path`: its relative form with no leading/trailing `/` or
/// leading `./`, and the root (`.`) as the empty string. `open`, `read_dir` and `metadata` all key
/// through this so a file reports the same mtime whether it is listed, stat'ed or opened.
fn timestamp_key(path: &DavPath) -> String {
    let rel = path.as_rel_ospath();
    let key = rel
        .to_str()
        .unwrap_or("")
        .trim_matches('/')
        .trim_start_matches("./");
    if key == "." {
        String::new()
    } else {
        key.to_string()
    }
}

/// Resolve a `SeekFrom` against the current position `pos` and the object `size` to a new absolute
/// offset, rejecting a seek that would land before byte 0 (or overflow `i64`). Shared by
/// `ProxiedMediaFile` and `VirtualFile`'s `DavFile::seek` so the underflow guard lives in one place.
fn seek_within(seek: std::io::SeekFrom, pos: u64, size: u64) -> Result<u64, FsError> {
    let resolve = |base: u64, delta: i64| -> Result<u64, FsError> {
        Ok((base as i64)
            .checked_add(delta)
            .filter(|&n| n >= 0)
            .ok_or(FsError::GeneralFailure)? as u64)
    };
    match seek {
        std::io::SeekFrom::Start(p) => Ok(p),
        std::io::SeekFrom::Current(p) => resolve(pos, p),
        std::io::SeekFrom::End(p) => resolve(size, p),
    }
}

#[derive(Clone)]
pub struct DebridFileSystem {
    vfs: Arc<RwLock<DebridVfs>>,
    provider: Arc<dyn DebridProvider>,
    repair_manager: Arc<RepairManager>,
    http_client: reqwest::Client,
    read_activity: Arc<crate::read_activity::ReadActivity>,
    /// Size of the FIRST read-ahead fetch on each opened media file (later reads use `BUFFER_SIZE`).
    /// Defaults to `BUFFER_SIZE` (no enlargement); `with_first_read_bytes` sets it from config.
    first_read_bytes: u64,
}

impl DebridFileSystem {
    pub fn new(
        provider: Arc<dyn DebridProvider>,
        vfs: Arc<RwLock<DebridVfs>>,
        repair_manager: Arc<RepairManager>,
        http_client: reqwest::Client,
        read_activity: Arc<crate::read_activity::ReadActivity>,
    ) -> Self {
        Self {
            vfs,
            provider,
            repair_manager,
            http_client,
            read_activity,
            first_read_bytes: BUFFER_SIZE as u64,
        }
    }

    /// Set the first-read-ahead window (from `Config::cdn_first_read_bytes`). Clamped to
    /// [`BUFFER_SIZE`, `MAX_FETCH_SIZE`] so it can never undercut the normal window or exceed the
    /// per-fetch cap, regardless of the configured value.
    pub fn with_first_read_bytes(mut self, bytes: u64) -> Self {
        self.first_read_bytes = bytes.clamp(BUFFER_SIZE as u64, MAX_FETCH_SIZE as u64);
        self
    }

    /// Resolve a path to a VfsNode reference without cloning.
    fn find_node_ref<'v>(vfs: &'v DebridVfs, path: &DavPath) -> Option<&'v VfsNode> {
        let path_osstr = path.as_rel_ospath();
        let path_str = path_osstr.to_str()?;
        Self::walk_components(&vfs.root, path_str)
    }

    /// Walk `/`-separated `path_str` from `root`, returning the node it names (or `None`). A `..`
    /// component is REJECTED (returns `None`) as defense-in-depth — DavPath already normalises `..`
    /// away before `find_node_ref` sees it, but this guards any future caller that bypasses DavPath.
    /// Pure (`&str` in) so the traversal-rejection is unit-testable without constructing a DavPath.
    fn walk_components<'v>(root: &'v VfsNode, path_str: &str) -> Option<&'v VfsNode> {
        if path_str == "." || path_str.is_empty() {
            return Some(root);
        }
        let mut current = root;
        for component in path_str.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                return None;
            }
            if let VfsNode::Directory { children } = current {
                current = children.get(component)?;
            } else {
                return None;
            }
        }
        Some(current)
    }

    /// Owned-clone resolver — test-only. Production code resolves a `&VfsNode` via `find_node_ref`
    /// and clones only the small per-file payload it needs (see `open`), never the whole subtree.
    #[cfg(test)]
    fn find_node_in(vfs: &DebridVfs, path: &DavPath) -> Option<VfsNode> {
        Self::find_node_ref(vfs, path).cloned()
    }
}

impl DavFileSystem for DebridFileSystem {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        _options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let rel = path.as_rel_ospath();
            // Timestamp lookup key, shared with `metadata`/`read_dir` so an opened file reports the
            // SAME mtime it does when listed (see below — the open-file metadata used to hardcode
            // UNIX_EPOCH, confusing media-server change detection).
            let ts_key = timestamp_key(path);
            let name = rel
                .to_str()
                .and_then(|s| s.rsplit('/').next())
                .unwrap_or("")
                .to_string();
            // Resolve under the read lock and clone ONLY the small per-file payload. A directory
            // match returns Forbidden without `.cloned()`-ing the whole subtree — otherwise a GET
            // on a large folder (e.g. /Movies) deep-clones thousands of nodes just to reject it.
            enum Payload {
                Media {
                    file_size: u64,
                    locator: crate::provider::FileLocator,
                    modified_time: SystemTime,
                },
                Virtual {
                    content: Vec<u8>,
                    modified_time: SystemTime,
                },
            }
            let payload = {
                let vfs = self.vfs.read().await;
                match Self::find_node_ref(&vfs, path).ok_or(FsError::NotFound)? {
                    VfsNode::MediaFile { file_size, locator } => Payload::Media {
                        file_size: *file_size,
                        locator: locator.clone(),
                        modified_time: vfs.timestamps.get(&ts_key).copied().unwrap_or(UNIX_EPOCH),
                    },
                    VfsNode::VirtualFile { content } => Payload::Virtual {
                        content: content.clone(),
                        // Same ts_key/timestamps lookup as the MediaFile arm, so a listed and an
                        // opened NFO report the same mtime (was hardcoded UNIX_EPOCH here).
                        modified_time: vfs.timestamps.get(&ts_key).copied().unwrap_or(UNIX_EPOCH),
                    },
                    VfsNode::Directory { .. } => return Err(FsError::Forbidden),
                }
            };
            match payload {
                Payload::Media {
                    file_size,
                    locator,
                    modified_time,
                } => Ok(Box::new(ProxiedMediaFile {
                    name,
                    locator,
                    file_size,
                    repair_manager: self.repair_manager.clone(),
                    provider: self.provider.clone(),
                    http_client: self.http_client.clone(),
                    pos: 0,
                    cdn_url: None,
                    buffer: Bytes::new(),
                    buffer_start: 0,
                    read_activity: self.read_activity.clone(),
                    modified_time,
                    confirmed_read: false,
                    first_read_bytes: self.first_read_bytes,
                }) as Box<dyn DavFile>),
                Payload::Virtual {
                    content,
                    modified_time,
                } => Ok(Box::new(VirtualFile {
                    content: Bytes::from(content),
                    pos: 0,
                    modified_time,
                }) as Box<dyn DavFile>),
            }
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let vfs = self.vfs.read().await;
            let node = Self::find_node_ref(&vfs, path).ok_or(FsError::NotFound)?;
            let base = timestamp_key(path);
            if let VfsNode::Directory { children } = node {
                let mut entries: Vec<Box<dyn DavDirEntry>> = Vec::new();
                for (name, child) in children {
                    let child_path = if base.is_empty() {
                        name.clone()
                    } else {
                        format!("{}/{}", base, name)
                    };
                    let modified_time = vfs
                        .timestamps
                        .get(&child_path)
                        .copied()
                        .unwrap_or(UNIX_EPOCH);
                    entries.push(Box::new(DebridDirEntry {
                        name: name.clone(),
                        metadata: DebridMetaData::from_node(child, modified_time),
                    }));
                }
                let stream = futures_util::stream::iter(entries.into_iter().map(Ok));
                Ok(Box::pin(stream) as FsStream<Box<dyn DavDirEntry>>)
            } else {
                Err(FsError::Forbidden)
            }
        }
        .boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let vfs = self.vfs.read().await;
            let node = Self::find_node_ref(&vfs, path).ok_or(FsError::NotFound)?;
            let modified_time = vfs
                .timestamps
                .get(&timestamp_key(path))
                .copied()
                .unwrap_or(UNIX_EPOCH);
            Ok(Box::new(DebridMetaData::from_node(node, modified_time)) as Box<dyn DavMetaData>)
        }
        .boxed()
    }
}

#[derive(Debug, Clone)]
struct DebridMetaData {
    is_directory: bool,
    size: u64,
    modified_time: SystemTime,
}

impl DebridMetaData {
    fn from_node(node: &VfsNode, modified_time: SystemTime) -> Self {
        let (is_directory, size) = match node {
            VfsNode::MediaFile { file_size, .. } => (false, *file_size),
            VfsNode::VirtualFile { content, .. } => (false, content.len() as u64),
            VfsNode::Directory { .. } => (true, 0),
        };
        Self {
            is_directory,
            size,
            modified_time,
        }
    }
}

impl DavMetaData for DebridMetaData {
    fn len(&self) -> u64 {
        self.size
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified_time)
    }
    fn is_dir(&self) -> bool {
        self.is_directory
    }
}

struct DebridDirEntry {
    name: String,
    metadata: DebridMetaData,
}

impl DavDirEntry for DebridDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.metadata.clone();
        async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) }.boxed()
    }
}

/// A media file that lazily resolves its `FileLocator` to a CDN URL via the active provider and
/// proxies the CDN bytes (provider-neutral — Real-Debrid unrestricts a per-file link, TorBox
/// resolves by `(torrent_id, file_id)`). The CDN URL is cached per open instance. Warm-buffer reads
/// use a 2 MB read-ahead buffer; the first fetch (empty buffer) may be larger — see `first_read_bytes`.
struct ProxiedMediaFile {
    name: String,
    locator: crate::provider::FileLocator,
    file_size: u64,
    repair_manager: Arc<RepairManager>,
    provider: Arc<dyn DebridProvider>,
    http_client: reqwest::Client,
    pos: u64,
    cdn_url: Option<String>,
    buffer: Bytes,
    buffer_start: u64,
    read_activity: Arc<crate::read_activity::ReadActivity>,
    /// Real per-torrent mtime (from the VFS `timestamps`), captured at open so the opened-file
    /// metadata matches what `read_dir`/`metadata` report for the same path.
    modified_time: SystemTime,
    /// Set once the first CDN fetch succeeds, so the repair-budget reset (`note_read_success`) fires
    /// at most once per opened file rather than per chunk.
    confirmed_read: bool,
    /// Size of the enlarged read-ahead fetch used whenever the read-ahead buffer is empty — the FIRST
    /// fetch of an opened file (front-loading the container header/early index so the open/ffprobe
    /// burst needs fewer cold CDN round-trips) and again after an instant-repair buffer reset (which
    /// re-primes the read-ahead from the current position). Later (warm-buffer) fetches use
    /// `BUFFER_SIZE`.
    first_read_bytes: u64,
}

// `DavFile` requires `Debug` as a supertrait, but this struct holds two capability values in the
// clear — `locator.link` (the RD restricted link) and `cdn_url` (the signed CDN download URL) — so
// an auto-derived Debug would let a stray `{:?}` (e.g. a future dav-server trace, or our own
// logging) leak them. Redact like `RealDebridClient`/`TorBoxClient`: print only the non-sensitive
// identity (name + hash), never the link or the resolved CDN URL.
impl std::fmt::Debug for ProxiedMediaFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxiedMediaFile")
            .field("name", &self.name)
            .field("hash", &self.locator.hash)
            .field("file_size", &self.file_size)
            .field("pos", &self.pos)
            // locator.link, cdn_url, and buffer are deliberately omitted (sensitive / large).
            .finish_non_exhaustive()
    }
}

impl ProxiedMediaFile {
    /// Lazily resolve the CDN download URL, caching the result.
    async fn resolve_cdn_url(&mut self) -> Result<String, FsError> {
        if let Some(ref url) = self.cdn_url {
            return Ok(url.clone());
        }

        match self.provider.resolve_url(&self.locator).await {
            Ok(url) => {
                // Never log the URL itself (it's an unrestricted, sensitive CDN link). `trace`, not
                // `debug` — this fires on EVERY file open (the resolution itself is cached), so a
                // player/Jellyfin scan re-opening files would otherwise flood the debug log.
                tracing::trace!(
                    "dav: resolved {} (hash {}) → CDN url",
                    self.name,
                    self.locator.hash
                );
                self.cdn_url = Some(url.clone());
                Ok(url)
            }
            Err(crate::error::AppError::Unavailable) => {
                tracing::warn!(
                    "Resolve unavailable for {} — attempting instant repair",
                    self.name
                );
                self.attempt_instant_repair().await
            }
            Err(e) => {
                tracing::warn!("Resolve failed for {} (not repairing): {}", self.name, e);
                Err(FsError::GeneralFailure)
            }
        }
    }

    /// Re-add the torrent by hash and, if a cached replacement is found, swap to the repaired
    /// locator and return a fresh CDN url. Drives both the resolve-time `Unavailable` path and the
    /// byte-fetch persistent-5xx path (a server error on the bytes means the file is broken on the
    /// provider, not merely an expired URL). The repair state machine's cooldown / attempt cap
    /// keeps repeated reads of the same broken file from storming re-adds.
    async fn attempt_instant_repair(&mut self) -> Result<String, FsError> {
        match self.repair_manager.try_instant_repair(&self.locator).await {
            Ok(new_locator) => {
                tracing::info!(
                    "Instant repair succeeded for {} — new torrent {}",
                    self.name,
                    new_locator.torrent_id
                );
                let old_locator = std::mem::replace(&mut self.locator, new_locator);
                self.buffer = Bytes::new();
                self.buffer_start = 0;
                self.cdn_url = None;
                // The repaired torrent has a different id — let the next confirmed read reset ITS
                // repair budget via `note_read_success` (the old `confirmed_read` referred to the
                // superseded torrent).
                self.confirmed_read = false;
                self.provider.invalidate(&old_locator).await;
                match self.provider.resolve_url(&self.locator).await {
                    Ok(url) => {
                        self.cdn_url = Some(url.clone());
                        Ok(url)
                    }
                    Err(e2) => {
                        tracing::error!(
                            "Failed to resolve repaired locator for {}: {}",
                            self.name,
                            e2
                        );
                        Err(FsError::GeneralFailure)
                    }
                }
            }
            Err(
                reason @ (crate::repair::RepairFailure::NotCached(_)
                | crate::repair::RepairFailure::Deferred(_)),
            ) => {
                // NotCached is the NORMAL lapsed-cache outcome (the torrent is being re-downloaded
                // and the scan loop will surface it once ready); Deferred is a transient settle blip
                // that a later read retries. Logging either at warn! on every cooldown-spaced read
                // is wrong-level noise. (Matched on the typed variant rather than a substring of the
                // message, so re-wording can't reclassify it.)
                tracing::info!(
                    "Instant repair for {}: {} — leaving to download",
                    self.name,
                    reason
                );
                Err(FsError::GeneralFailure)
            }
            Err(reason) => {
                // An actual repair failure (rate-limited / max-attempts / info-fetch error).
                tracing::warn!("Instant repair failed for {}: {}", self.name, reason);
                Err(FsError::GeneralFailure)
            }
        }
    }

    /// Fetch bytes from CDN, using the read-ahead buffer.
    async fn fetch_bytes(&mut self, len: usize) -> Result<Bytes, FsError> {
        // A zero-length read needs no bytes — return early before the buffer-miss path forces a
        // BUFFER_SIZE (2 MB) ranged GET that would just be discarded (`to_read = min(0, …) = 0`).
        if len == 0 || self.pos >= self.file_size {
            return Ok(Bytes::new());
        }

        let pos = self.pos;
        // Saturating add (consistent with the deliberate saturating range math below): a pathological
        // `buffer_start` near `u64::MAX` from a garbage provider `bytes` field must not overflow.
        let buffer_end = self.buffer_start.saturating_add(self.buffer.len() as u64);

        // Buffer hit — serve directly without any network I/O.
        if pos >= self.buffer_start && pos < buffer_end {
            let offset = (pos - self.buffer_start) as usize;
            let available = self.buffer.len() - offset;
            let to_read = std::cmp::min(len, available);
            let data = self.buffer.slice(offset..offset + to_read);
            self.pos += to_read as u64;
            return Ok(data);
        }

        // Buffer miss — fetch from CDN (with retry on expired URL). Whenever the read-ahead buffer is
        // empty — the FIRST fetch of an opened file, and again after an instant-repair buffer reset —
        // use the larger `first_read_bytes` window so a player's open/ffprobe burst (header + early
        // index) is satisfied from one cold fetch instead of several; later (warm-buffer) fetches use
        // the normal `BUFFER_SIZE` read-ahead. `first_read_bytes` is already clamped to
        // [BUFFER_SIZE, MAX_FETCH_SIZE]; clamp again defensively.
        let min_window = if self.buffer.is_empty() {
            self.first_read_bytes
                .clamp(BUFFER_SIZE as u64, MAX_FETCH_SIZE as u64)
        } else {
            BUFFER_SIZE as u64
        };
        let fetch_size = (len as u64).max(min_window).min(MAX_FETCH_SIZE as u64);
        // Saturating range math (mirrors `fetch_cdn_range`'s `want` and `VirtualFile::read_bytes`): a
        // pathological `file_size`/`pos` from a garbage provider `bytes` field can't overflow the add
        // before the `min` clamps it. Bounded by `file_size - 1` (also saturating).
        let range_end = std::cmp::min(
            pos.saturating_add(fetch_size).saturating_sub(1),
            self.file_size.saturating_sub(1),
        );
        let body = self.fetch_cdn_range(pos, range_end).await?;

        // A successful CDN fetch confirms the file genuinely works — reset its repair budget once per
        // open (so the 3-attempt cap bounds only consecutive failed repairs, not separate incidents
        // over the deployment's lifetime). `note_read_success` fast-paths a clean torrent, so this is
        // cheap; doing it once (guarded by `confirmed_read`) avoids per-chunk work.
        if !self.confirmed_read {
            self.confirmed_read = true;
            self.repair_manager
                .note_read_success(&self.locator.torrent_id)
                .await;
        }

        self.buffer = body;
        self.buffer_start = pos;

        let to_read = std::cmp::min(len, self.buffer.len());
        let data = self.buffer.slice(..to_read);
        self.pos += to_read as u64;
        Ok(data)
    }

    /// Fetch a byte range from the CDN with up to 3 attempts, branching by failure kind (each arm's
    /// full rationale is in the inline comments):
    ///
    /// - **Connection error** (TCP failure, timeout), while sending the request OR reading the body:
    ///   retry once on the SAME url — the URL is still valid, the error is transient (a partial body
    ///   is discarded). `cdn_url`/the resolution are intentionally left intact so many concurrent
    ///   readers don't all re-resolve through the rate-limiter at once (a stampede).
    ///
    /// - **Persistent 5xx**: NOT a stale URL — the file is (transiently) broken on the provider, so
    ///   again do NOT invalidate. Retry the same url; once a retry still 5xxs, escalate to instant
    ///   repair (re-add by hash) ONCE — a cached replacement fixes playback inline, else the read
    ///   fails (rather than the player retry-storming a 500 forever).
    ///
    /// - **Genuinely stale / non-compliant** (expired 403/410, a Range-ignoring 200 on a seek, an
    ///   oversized body, or a Content-Range mismatch): drop `cdn_url` + invalidate the resolution so
    ///   the next attempt re-resolves a fresh URL.
    async fn fetch_cdn_range(&mut self, pos: u64, range_end: u64) -> Result<Bytes, FsError> {
        // Three attempts: the original url, one retry (the same url after a connection error or a
        // 5xx; a freshly re-resolved url after a stale 4xx / non-compliant response), then (only if a
        // 5xx persisted) one post-instant-repair fetch. `repaired` caps the repair escalation at once
        // per call.
        let mut repaired = false;
        'attempts: for attempt in 0..3u8 {
            let cdn_url = self.resolve_cdn_url().await?;

            let resp = match self
                .http_client
                .get(&cdn_url)
                .header("Range", format!("bytes={}-{}", pos, range_end))
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    // Connection error (TCP failure, timeout) — the CDN URL itself is
                    // still valid; this is a transient network issue.  Do NOT clear
                    // cdn_url or invalidate the unrestrict cache: with many concurrent
                    // ProxiedMediaFile instances (one per rclone read-ahead), all
                    // invalidating simultaneously forces every caller through the
                    // rate-limited resolve_url path and serialises them through the
                    // adaptive rate-limiter (~90 callers × 700 ms ≈ 63 s hang).
                    // `e.without_url()` strips the signed CDN URL from the error's Display —
                    // `reqwest::Error` embeds the request URL otherwise, leaking the sensitive
                    // capability link into the logs (matches the scrubbing probe.rs already does).
                    tracing::warn!(
                        "CDN fetch failed for {}: {} — retrying with same URL",
                        self.name,
                        e.without_url()
                    );
                    if attempt == 0 {
                        continue;
                    }
                    return Err(FsError::GeneralFailure);
                }
            };

            let status = resp.status();
            // Read once: reused for the oversized check below AND the keep-alive drain decision.
            let content_length = resp.content_length();
            // A ranged request should yield 206 Partial Content. A plain 200 means the CDN
            // ignored the Range header and is returning the whole object starting at byte 0.
            // That is only safe to buffer when we asked for offset 0 — for any seek (pos > 0)
            // it would buffer bytes [0, n) at `buffer_start = pos`, silently serving the wrong
            // bytes. Treat such a response as unusable (like an expired URL) rather than
            // corrupting the read.
            let acceptable = status == reqwest::StatusCode::PARTIAL_CONTENT
                || (status == reqwest::StatusCode::OK && pos == 0);
            // Reject by advertised Content-Length as a cheap early-out for a 206 whose Content-Length
            // exceeds the cap (a non-compliant CDN echoing a huge body as "partial"). A 200 is only
            // `acceptable` at pos==0 — a Range-ignoring CDN (e.g. TorBox) streaming the WHOLE object
            // from byte 0, so a full-file Content-Length far above the window is EXPECTED, not
            // oversized; the window-bounded read below stops at `want` bytes and drops the connection
            // (mirroring `probe::read_body`). The bounded read is the hard guarantee regardless —
            // Content-Length is attacker-controlled and may lie or be absent.
            let oversized = status == reqwest::StatusCode::PARTIAL_CONTENT
                && content_length.is_some_and(|cl| cl > MAX_FETCH_SIZE as u64);
            // For a 206, defensively confirm the CDN returned the range we asked for: a non-compliant
            // CDN that answers 206 with a DIFFERENT start would otherwise be buffered at
            // `buffer_start = pos`, silently serving wrong bytes. An absent/unparseable Content-Range
            // is tolerated (the byte cap + emptiness checks still apply); only a parseable start that
            // disagrees with `pos` is rejected (treated like an expired URL).
            let range_mismatch = status == reqwest::StatusCode::PARTIAL_CONTENT
                && resp
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range_start)
                    .is_some_and(|start| start != pos);
            if !acceptable || oversized || range_mismatch {
                tracing::warn!(
                    "CDN returned {} for {} at offset {}",
                    status,
                    self.name,
                    pos
                );
                // A persistent server error (5xx) is NOT a stale URL — the file is (transiently)
                // broken on the provider, not the signed URL. Do NOT clear `cdn_url` / invalidate the
                // resolution for a 5xx: with many concurrent readers (one ProxiedMediaFile per rclone
                // read-ahead) all invalidating at once, the next reads would serialise through the
                // rate-limited resolve path (~63s hang) — the exact stampede the connection-error arm
                // above avoids. Retry the SAME url; once a retry (attempt > 0) still 5xxs, escalate to
                // instant repair (re-add by hash) ONCE — a cached replacement fixes playback inline,
                // else the read fails (instead of the player retry-storming a 500 forever). The repair
                // cooldown bounds re-add attempts. (`oversized`/`range_mismatch` require a 2xx, so they
                // never reach this `is_server_error` arm.)
                if status.is_server_error() {
                    if attempt > 0 && !repaired {
                        repaired = true;
                        if self.attempt_instant_repair().await.is_ok() {
                            continue; // fetch again against the repaired torrent's fresh url
                        }
                        return Err(FsError::GeneralFailure);
                    }
                    if attempt == 0 {
                        continue; // retry the SAME url (no invalidate) — a transient origin blip
                    }
                    return Err(FsError::GeneralFailure);
                }
                // Genuinely stale/non-compliant (expired 403/410, a Range-ignoring 200 on a seek, an
                // oversized body, or a range mismatch): drop the cached URL + resolution so the next
                // attempt re-resolves a fresh one.
                self.cdn_url = None;
                self.provider.invalidate(&self.locator).await;
                if attempt == 0 {
                    continue;
                }
                return Err(FsError::GeneralFailure);
            }

            // Bounded read: accumulate at most MAX_FETCH_SIZE bytes so a CDN that lies about (or
            // omits) Content-Length cannot drive an unbounded allocation. Exceeding the cap is
            // treated as an unusable response (drop the URL, retry/fail like an expired one).
            let mut resp = resp;
            // Preallocate the requested window (capped at the hard byte ceiling) so the read-ahead
            // accumulation doesn't repeatedly reallocate-and-copy as chunks arrive. The bounded
            // loop below still enforces MAX_FETCH_SIZE regardless of what the CDN actually sends.
            let want = range_end.saturating_sub(pos).saturating_add(1);
            let want_usize = want.min(MAX_FETCH_SIZE as u64) as usize;
            // Keep-alive reuse: a bounded 206 (Content-Length present and <= the window we asked for)
            // has a body of exactly that length, so reading it to its natural end (`Ok(None)`) lets
            // reqwest return the connection to the pool — the NEXT window's GET then reuses the same
            // TCP+TLS connection instead of re-handshaking per 2 MB read. We must NOT do this for a
            // Range-ignoring 200 (TorBox streaming the WHOLE multi-GB object from byte 0): there we
            // still read only `want` bytes then drop the connection (we can't drain gigabytes). A 206
            // whose Content-Length is absent or exceeds the window is treated as the latter (drop).
            let drainable = status == reqwest::StatusCode::PARTIAL_CONTENT
                && content_length.is_some_and(|cl| cl <= want_usize as u64);
            let mut body = bytes::BytesMut::with_capacity(want_usize);
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        body.extend_from_slice(&chunk);
                        // For a non-drainable response (Range-ignoring 200, or a 206 lacking a bounded
                        // Content-Length), stop once the requested window is satisfied and drop the
                        // rest of the connection — a 200 streams the WHOLE multi-GB object from byte 0,
                        // so we must read only `want` bytes (mirrors `probe::read_body`). A drainable
                        // 206 instead falls through to `Ok(None)` (its body == Content-Length <= want),
                        // so the socket is released to the keep-alive pool. `want` <= MAX_FETCH_SIZE, so
                        // this also bounds the allocation; the check below is a hard backstop.
                        if !drainable && body.len() >= want_usize {
                            body.truncate(want_usize);
                            break;
                        }
                        if body.len() > MAX_FETCH_SIZE {
                            tracing::warn!(
                                "CDN body for {} exceeded {} bytes — clearing cached CDN URL",
                                self.name,
                                MAX_FETCH_SIZE
                            );
                            self.cdn_url = None;
                            self.provider.invalidate(&self.locator).await;
                            return Err(FsError::GeneralFailure);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        // The connection died after the headers (a mid-body reset, a pooled
                        // keep-alive connection closing, the client timeout firing during the body):
                        // the same transient failure as the send-error arm above, so retry once on
                        // the SAME url without invalidating, discarding the partial body. Strip the
                        // signed CDN URL from the error Display (see above).
                        tracing::warn!(
                            "CDN body read failed for {}: {}",
                            self.name,
                            e.without_url()
                        );
                        if attempt == 0 {
                            continue 'attempts;
                        }
                        return Err(FsError::GeneralFailure);
                    }
                }
            }

            // A 206/200 that streamed ZERO bytes for a non-empty requested range (range_end >= pos
            // always holds here) is non-compliant: returning it would surface as a premature
            // mid-file EOF and silently truncate playback. Treat it like an expired URL — drop it,
            // retry once with a fresh resolution, then fail rather than truncate.
            if body.is_empty() {
                tracing::warn!(
                    "CDN returned an empty body for {} at offset {} — clearing cached CDN URL",
                    self.name,
                    pos
                );
                self.cdn_url = None;
                self.provider.invalidate(&self.locator).await;
                if attempt == 0 {
                    continue;
                }
                return Err(FsError::GeneralFailure);
            }

            return Ok(body.freeze());
        }

        Err(FsError::GeneralFailure)
    }
}

impl DavFile for ProxiedMediaFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            if self
                .repair_manager
                .should_hide_torrent(&self.locator.torrent_id)
                .await
            {
                return Ok(Box::new(DebridMetaData {
                    is_directory: false,
                    size: 0,
                    modified_time: SystemTime::UNIX_EPOCH,
                }) as Box<dyn DavMetaData>);
            }

            Ok(Box::new(DebridMetaData {
                is_directory: false,
                size: self.file_size,
                modified_time: self.modified_time,
            }) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn write_buf(&mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        async move { Err(FsError::Forbidden) }.boxed()
    }

    fn write_bytes(&mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        async move { Err(FsError::Forbidden) }.boxed()
    }

    fn read_bytes(&mut self, len: usize) -> FsFuture<'_, Bytes> {
        async move {
            if self
                .repair_manager
                .should_hide_torrent(&self.locator.torrent_id)
                .await
            {
                return Err(FsError::GeneralFailure);
            }
            // Stamp the TORRENT, not the path: the upgrade engine asks whether the torrents a swap
            // would delete or take an episode away from are being read.
            self.read_activity.touch(&self.locator.hash).await;
            self.fetch_bytes(len).await
        }
        .boxed()
    }

    fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let new_pos = seek_within(pos, self.pos, self.file_size)?;
            self.pos = new_pos;
            Ok(new_pos)
        }
        .boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move { Ok(()) }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debrid_metadata_from_node_extracts_correct_fields() {
        let modified = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);

        // Directory
        let dir_node = VfsNode::Directory {
            children: std::collections::BTreeMap::new(),
        };
        let meta = DebridMetaData::from_node(&dir_node, modified);
        assert!(meta.is_directory);
        assert_eq!(meta.size, 0);
        assert_eq!(meta.modified_time, modified);

        // MediaFile
        let media_node = VfsNode::MediaFile {
            file_size: 42000,
            locator: crate::provider::FileLocator {
                link: Some("link".to_string()),
                torrent_id: "tid".to_string(),
                ..Default::default()
            },
        };
        let meta = DebridMetaData::from_node(&media_node, modified);
        assert!(!meta.is_directory);
        assert_eq!(meta.size, 42000);

        // VirtualFile
        let vf_node = VfsNode::VirtualFile {
            content: vec![1, 2, 3, 4, 5],
        };
        let meta = DebridMetaData::from_node(&vf_node, modified);
        assert!(!meta.is_directory);
        assert_eq!(meta.size, 5);
    }

    #[test]
    fn find_node_in_rejects_dotdot_traversal() {
        let vfs = DebridVfs::new();

        // Normal lookup works (DebridVfs::new seeds a "Movies" directory).
        assert!(
            DebridFileSystem::walk_components(&vfs.root, "Movies").is_some(),
            "a normal path must resolve"
        );
        assert!(
            DebridFileSystem::walk_components(&vfs.root, "").is_some(),
            "the empty (root) path resolves to root"
        );
        // The `..` traversal guard, tested BEHAVIOURALLY (not by grepping the source): DavPath
        // normalises `..` away before find_node_ref, but this guards any future caller that bypasses
        // it — a `..` component anywhere in the path must return None.
        assert!(
            DebridFileSystem::walk_components(&vfs.root, "Movies/../etc").is_none(),
            "a `..` component must be rejected"
        );
        assert!(DebridFileSystem::walk_components(&vfs.root, "..").is_none());
        assert!(DebridFileSystem::walk_components(&vfs.root, "../secret").is_none());
    }

    #[test]
    fn find_node_in_resolves_root() {
        let vfs = DebridVfs::new();
        // Root path "/" maps to rel_ospath "." or ""
        let path = DavPath::new("/").unwrap();
        let result = DebridFileSystem::find_node_in(&vfs, &path);
        assert!(result.is_some(), "Root path should resolve");
        assert!(matches!(result, Some(VfsNode::Directory { .. })));
    }

    #[test]
    fn find_node_in_returns_none_for_missing_path() {
        let vfs = DebridVfs::new();
        let path = DavPath::new("/NonExistent/Folder").unwrap();
        let result = DebridFileSystem::find_node_in(&vfs, &path);
        assert!(result.is_none(), "Missing path should return None");
    }

    #[test]
    fn find_node_in_resolves_deep_path() {
        // Build a VFS with a known movie file and verify it can be found
        let torrents = vec![(
            crate::rd_client::TorrentInfo {
                id: "t1".to_string(),
                filename: "Test.mkv".to_string(),
                original_filename: "Test.mkv".to_string(),
                hash: "h".to_string(),
                bytes: 1000,
                original_bytes: 1000,
                host: "h".to_string(),
                split: 1,
                progress: 100.0,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![crate::rd_client::TorrentFile {
                    id: 1,
                    path: "/Test.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["http://link".to_string()],
                ended: None,
            },
            crate::vfs::MediaMetadata {
                title: "Test Movie".to_string(),
                year: None,
                media_type: crate::vfs::MediaType::Movie,
                external_id: Some("tmdb:123".to_string()),
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
        let path = DavPath::new("/Movies/Test Movie [tmdbid-123]/Test.mkv").unwrap();
        let result = DebridFileSystem::find_node_in(&vfs, &path);
        assert!(result.is_some(), "Deep path to media file should resolve");
        assert!(matches!(result, Some(VfsNode::MediaFile { .. })));
    }

    #[tokio::test]
    async fn virtual_file_seek_and_read() {
        use dav_server::fs::DavFile;

        let content = b"Hello, World! This is test content.";
        let mut file = VirtualFile {
            content: Bytes::from(&content[..]),
            pos: 0,
            modified_time: SystemTime::UNIX_EPOCH,
        };

        // Read first 5 bytes
        let data = file.read_bytes(5).await.unwrap();
        assert_eq!(&data[..], b"Hello");
        assert_eq!(file.pos, 5);

        // Seek to position 7
        let pos = file.seek(std::io::SeekFrom::Start(7)).await.unwrap();
        assert_eq!(pos, 7);

        // Read from new position
        let data = file.read_bytes(6).await.unwrap();
        assert_eq!(&data[..], b"World!");
        assert_eq!(file.pos, 13);

        // Seek from current (+2)
        let pos = file.seek(std::io::SeekFrom::Current(2)).await.unwrap();
        assert_eq!(pos, 15);

        // Seek from end (-8)
        let pos = file.seek(std::io::SeekFrom::End(-8)).await.unwrap();
        assert_eq!(pos, (content.len() - 8) as u64);

        // Read past end returns empty
        file.seek(std::io::SeekFrom::Start(content.len() as u64))
            .await
            .unwrap();
        let data = file.read_bytes(10).await.unwrap();
        assert!(data.is_empty(), "Reading at EOF should return empty bytes");
    }

    #[tokio::test]
    async fn virtual_file_seek_negative_current_errors() {
        use dav_server::fs::DavFile;

        let mut file = VirtualFile {
            content: Bytes::from(&b"Hello"[..]),
            pos: 2,
            modified_time: SystemTime::UNIX_EPOCH,
        };

        // SeekFrom::Current(-10) from pos=2 should be negative => error
        let result = file.seek(std::io::SeekFrom::Current(-10)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn virtual_file_seek_negative_end_errors() {
        use dav_server::fs::DavFile;

        let mut file = VirtualFile {
            content: Bytes::from(&b"Hi"[..]),
            pos: 0,
            modified_time: SystemTime::UNIX_EPOCH,
        };

        // SeekFrom::End(-10) on 2-byte file would be negative => error
        let result = file.seek(std::io::SeekFrom::End(-10)).await;
        assert!(result.is_err());
    }
}

/// Simple virtual file (NFO files, etc)
#[derive(Debug)]
struct VirtualFile {
    content: Bytes,
    pos: u64,
    /// The real per-torrent mtime for this NFO, plumbed through `open` from `vfs.timestamps` — the
    /// SAME value the file reports when listed via `read_dir`/`metadata`. Hardcoding `UNIX_EPOCH`
    /// here (as it once did) made an opened NFO report a 1970 mtime while its directory listing
    /// reported the real one, confusing media-server change detection — the identical bug the
    /// `MediaFile` open path was already fixed for.
    modified_time: SystemTime,
}

impl DavFile for VirtualFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = DebridMetaData {
            is_directory: false,
            size: self.content.len() as u64,
            modified_time: self.modified_time,
        };
        async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) }.boxed()
    }

    fn write_buf(&mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        async move { Err(FsError::Forbidden) }.boxed()
    }

    fn write_bytes(&mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        async move { Err(FsError::Forbidden) }.boxed()
    }

    fn read_bytes(&mut self, len: usize) -> FsFuture<'_, Bytes> {
        async move {
            if self.pos >= self.content.len() as u64 {
                return Ok(Bytes::new());
            }

            let start = self.pos as usize;
            // `saturating_add` so a pathological `len` near `usize::MAX` can't overflow the add
            // before the `min` clamps it (mirrors `ProxiedMediaFile`'s saturating range math).
            let end = std::cmp::min(start.saturating_add(len), self.content.len());
            let data = self.content.slice(start..end);

            self.pos += data.len() as u64;
            Ok(data)
        }
        .boxed()
    }

    fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let new_pos = seek_within(pos, self.pos, self.content.len() as u64)?;
            self.pos = new_pos;
            Ok(new_pos)
        }
        .boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move { Ok(()) }.boxed()
    }
}

#[cfg(test)]
mod provider_abstraction_tests {
    use super::*;
    use crate::provider::{DebridProvider, MockProvider};
    use crate::repair::RepairManager;
    use crate::vfs::DebridVfs;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// A filesystem over one movie, `Movies/Test Movie [tmdbid-123]/{Test.mkv, movie.nfo}`, whose
    /// torrent was added 2023-01-01 and whose media file is 1000 bytes.
    fn one_movie_fs() -> DebridFileSystem {
        let torrents = vec![(
            crate::rd_client::TorrentInfo {
                id: "t1".to_string(),
                filename: "Test.mkv".to_string(),
                original_filename: "Test.mkv".to_string(),
                hash: "h".to_string(),
                bytes: 1000,
                original_bytes: 1000,
                host: "h".to_string(),
                split: 1,
                progress: 100.0,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![crate::rd_client::TorrentFile {
                    id: 1,
                    path: "/Test.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["http://link".to_string()],
                ended: None,
            },
            crate::vfs::MediaMetadata {
                title: "Test Movie".to_string(),
                year: None,
                media_type: crate::vfs::MediaType::Movie,
                external_id: Some("tmdb:123".to_string()),
            },
        )];
        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let ra = Arc::new(crate::read_activity::ReadActivity::without_startup_grace());
        DebridFileSystem::new(
            provider,
            Arc::new(RwLock::new(vfs)),
            repair,
            reqwest::Client::new(),
            ra,
        )
    }

    #[tokio::test]
    async fn listed_entries_report_the_real_mtime_and_size() {
        // A PROPFIND listing (`read_dir`) must key each child's timestamp exactly as the VFS stores
        // it; a mismatch would date every listed item 1970 and defeat media-server change detection.
        use dav_server::fs::{DavFileSystem, ReadDirMeta};
        use futures_util::StreamExt;
        let fs = one_movie_fs();
        let added = crate::vfs::parse_rd_date("2023-01-01");
        assert_ne!(added, SystemTime::UNIX_EPOCH);

        let folder = DavPath::new("/Movies/Test Movie [tmdbid-123]").unwrap();
        let mut entries = DavFileSystem::read_dir(&fs, &folder, ReadDirMeta::Data)
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Some(entry) = entries.next().await {
            let entry = entry.unwrap();
            let name = String::from_utf8(entry.name()).unwrap();
            let md = entry.metadata().await.unwrap();
            assert_eq!(md.modified().unwrap(), added, "{name}: listed mtime");
            if name == "Test.mkv" {
                assert_eq!(md.len(), 1000);
            }
            seen.push(name);
        }
        seen.sort();
        assert_eq!(seen, vec!["Test.mkv", "movie.nfo"]);

        // The listing of a section keys its child folders the same way.
        let movies = DavPath::new("/Movies").unwrap();
        let mut entries = DavFileSystem::read_dir(&fs, &movies, ReadDirMeta::Data)
            .await
            .unwrap();
        let entry = entries.next().await.unwrap().unwrap();
        assert_eq!(entry.metadata().await.unwrap().modified().unwrap(), added);
    }

    #[tokio::test]
    async fn stat_reports_the_real_mtime_for_folder_media_and_nfo() {
        use dav_server::fs::DavFileSystem;
        let fs = one_movie_fs();
        let added = crate::vfs::parse_rd_date("2023-01-01");
        for (path, is_dir) in [
            ("/Movies/Test Movie [tmdbid-123]", true),
            ("/Movies/Test Movie [tmdbid-123]/", true),
            ("/Movies/Test Movie [tmdbid-123]/Test.mkv", false),
            ("/Movies/Test Movie [tmdbid-123]/movie.nfo", false),
        ] {
            let md = DavFileSystem::metadata(&fs, &DavPath::new(path).unwrap())
                .await
                .unwrap();
            assert_eq!(md.modified().unwrap(), added, "{path}: stat mtime");
            assert_eq!(md.is_dir(), is_dir, "{path}");
        }
    }

    #[tokio::test]
    async fn opened_nfo_reports_the_real_mtime_not_unix_epoch() {
        use dav_server::fs::{DavFileSystem, OpenOptions};
        let fs = one_movie_fs();

        // Open the generated NFO and the media file; both must report the SAME real mtime
        // (derived from `added: 2023-01-01`), NOT the old hardcoded UNIX_EPOCH for the NFO.
        let nfo_path = DavPath::new("/Movies/Test Movie [tmdbid-123]/movie.nfo").unwrap();
        let mut nfo = DavFileSystem::open(&fs, &nfo_path, OpenOptions::default())
            .await
            .unwrap();
        let nfo_mtime = nfo.metadata().await.unwrap().modified().unwrap();
        assert_ne!(
            nfo_mtime,
            SystemTime::UNIX_EPOCH,
            "an opened NFO must report its real mtime, not UNIX_EPOCH"
        );

        let media_path = DavPath::new("/Movies/Test Movie [tmdbid-123]/Test.mkv").unwrap();
        let mut media = DavFileSystem::open(&fs, &media_path, OpenOptions::default())
            .await
            .unwrap();
        let media_mtime = media.metadata().await.unwrap().modified().unwrap();
        assert_eq!(
            nfo_mtime, media_mtime,
            "the opened NFO and its media file must report the same mtime"
        );
    }

    #[tokio::test]
    async fn fetch_bytes_rejects_range_ignoring_200_after_seek() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A CDN that ignores the Range header and always replies 200 OK with the
        // object from byte 0. Serves a couple of connections (the retry attempt too).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = b"BYTES-FROM-ZERO";
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
            }
        });

        // A seek to a non-zero offset.
        let mut f = proxied_for(format!("http://{}/", addr), 500, 1000);

        // The 200 carries the file from offset 0, not from 500 — serving it would hand the
        // reader the wrong bytes. It must error instead.
        let result = f.fetch_bytes(8).await;
        assert!(
            result.is_err(),
            "a Range-ignoring 200 at a non-zero offset must be rejected, not mis-served"
        );
    }

    /// Spawn a local server that replies `200 OK` with the given Content-Length header and
    /// body, ignoring the Range header, then closes the connection. Returns the base URL and a
    /// count of accepted connections (one request each).
    async fn spawn_range_ignoring_200(
        content_length: u64,
        body: &'static [u8],
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = requests.clone();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    content_length
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}/", addr), requests)
    }

    /// The one place tests build a `ProxiedMediaFile`: `Movie.mkv`, `file_size` bytes, at position
    /// 0 with no resolved CDN url and an empty read-ahead buffer, a repair manager over `provider`,
    /// read activity without the start-up grace, a `UNIX_EPOCH` mtime and the default first-read
    /// window. Tests override fields on the returned value.
    fn proxied(
        provider: Arc<dyn DebridProvider>,
        locator: crate::provider::FileLocator,
        file_size: u64,
    ) -> ProxiedMediaFile {
        ProxiedMediaFile {
            name: "Movie.mkv".to_string(),
            locator,
            file_size,
            repair_manager: Arc::new(RepairManager::new(provider.clone())),
            provider,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: None,
            buffer: bytes::Bytes::new(),
            buffer_start: 0,
            read_activity: Arc::new(crate::read_activity::ReadActivity::without_startup_grace()),
            modified_time: SystemTime::UNIX_EPOCH,
            confirmed_read: false,
            first_read_bytes: BUFFER_SIZE as u64,
        }
    }

    /// A `ProxiedMediaFile` at `pos` whose url `url` is already resolved (and is what the mock
    /// provider re-resolves to).
    fn proxied_for(url: String, pos: u64, file_size: u64) -> ProxiedMediaFile {
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolved_url: Some(url.clone()),
            ..Default::default()
        });
        let mut f = proxied(provider, crate::provider::FileLocator::default(), file_size);
        f.pos = pos;
        f.cdn_url = Some(url);
        f
    }

    #[test]
    fn seek_within_resolves_all_variants_and_rejects_underflow() {
        use std::io::SeekFrom;
        // Start is absolute (may exceed size — the read is clamped elsewhere).
        assert_eq!(seek_within(SeekFrom::Start(42), 10, 100).unwrap(), 42);
        // Current/End add to pos/size respectively.
        assert_eq!(seek_within(SeekFrom::Current(5), 10, 100).unwrap(), 15);
        assert_eq!(seek_within(SeekFrom::Current(-4), 10, 100).unwrap(), 6);
        assert_eq!(seek_within(SeekFrom::End(-1), 10, 100).unwrap(), 99);
        assert_eq!(seek_within(SeekFrom::End(0), 10, 100).unwrap(), 100);
        // Seeking before byte 0 is rejected, not wrapped.
        assert!(seek_within(SeekFrom::Current(-11), 10, 100).is_err());
        assert!(seek_within(SeekFrom::End(-101), 10, 100).is_err());
        // Overflow of the i64 add is rejected rather than panicking.
        assert!(seek_within(SeekFrom::Current(i64::MAX), i64::MAX as u64, 100).is_err());
    }

    #[tokio::test]
    async fn fetch_bytes_fails_a_200_whose_body_ends_before_the_window() {
        // A whole-object 200 at offset 0 is ACCEPTED, even with a multi-GB Content-Length (TorBox
        // streams the whole object; see
        // `fetch_cdn_range_accepts_range_ignoring_200_with_oversized_content_length_at_offset_0`).
        // But a body that ends short of both its Content-Length and the window (the connection
        // closes mid-body) must fail after one same-url retry, never serve truncated bytes.
        let (url, requests) = spawn_range_ignoring_200(999_999_999, b"only-headers-matter").await;
        let mut f = proxied_for(url, 0, 5_000_000_000);
        let result = f.fetch_bytes(8).await;
        assert!(
            result.is_err(),
            "a 200 whose body is cut short must not be served"
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the body read error is retried once"
        );
    }

    #[tokio::test]
    async fn fetch_bytes_accepts_small_200_at_offset_zero() {
        // A small whole-object 200 at offset 0 (Content-Length within the fetch window) is
        // legitimate (the bytes start at 0 = pos) and must be served.
        let body: &[u8] = b"ZEROBYTES1234567"; // 16 bytes
        let (url, _) = spawn_range_ignoring_200(body.len() as u64, body).await;
        let mut f = proxied_for(url, 0, body.len() as u64);
        let data = f
            .fetch_bytes(8)
            .await
            .expect("small 200 at offset 0 is valid");
        assert_eq!(&data[..], b"ZEROBYTE");
    }

    // --- Behavioural replacements for the former source-string fetch_cdn_range tests ---

    /// Server that always replies with `status` (a status line like "403 Forbidden") and no body.
    async fn spawn_always_status(status: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let head = format!("HTTP/1.1 {}\r\nContent-Length: 0\r\n\r\n", status);
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{}/", addr)
    }

    /// Server whose first response is `first_status` (no body) and whose later responses are
    /// `206 Partial Content` with `body` — simulates an expired URL recovering on retry.
    async fn spawn_first_status_then_206(
        first_status: &'static str,
        body: &'static [u8],
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut first = true;
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                if first {
                    first = false;
                    let head = format!("HTTP/1.1 {}\r\nContent-Length: 0\r\n\r\n", first_status);
                    let _ = sock.write_all(head.as_bytes()).await;
                } else {
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                }
                let _ = sock.flush().await;
            }
        });
        format!("http://{}/", addr)
    }

    /// Server that accepts then immediately closes each connection, producing a transport
    /// (connection) error on the client rather than an HTTP status.
    async fn spawn_connection_closing() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..4 {
                match listener.accept().await {
                    Ok((sock, _)) => drop(sock),
                    Err(_) => break,
                }
            }
        });
        format!("http://{}/", addr)
    }

    /// A single `206 Partial Content` carrying a `Content-Range` but NO `Content-Length` (a
    /// chunked/streaming CDN). The body end is signalled by connection close, so the client's read
    /// must be window-bounded rather than relying on Content-Length.
    async fn spawn_206_no_content_length(body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nConnection: close\r\n\r\n",
                    body.len() - 1,
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
                // No Content-Length: dropping the socket (Connection: close) signals the body end.
            }
        });
        format!("http://{}/", addr)
    }

    /// Build a ProxiedMediaFile plus a handle to the provider's invalidate-call counter.
    fn proxied_with_counter(
        url: String,
        pos: u64,
        file_size: u64,
    ) -> (
        ProxiedMediaFile,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolved_url: Some(url.clone()),
            invalidate_calls: counter.clone(),
            ..Default::default()
        });
        let mut f = proxied(provider, crate::provider::FileLocator::default(), file_size);
        f.pos = pos;
        f.cdn_url = Some(url);
        (f, counter)
    }

    #[tokio::test]
    async fn fetch_cdn_range_invalidates_on_http_error() {
        // A stale 4xx (403/410) means the CDN URL has expired — drop the cached resolution so a
        // fresh URL is fetched on retry. (A 5xx is NOT treated as stale and does not invalidate;
        // see `fetch_cdn_range_transient_5xx_retries_same_url_without_invalidating`.)
        let url = spawn_always_status("403 Forbidden").await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, 1000);
        let _ = f.fetch_bytes(8).await; // errors after retries
        assert!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "an HTTP error must invalidate the cached resolution"
        );
    }

    /// Always replies `206 Partial Content` advertising a Content-Length far above the fetch window
    /// (a non-compliant CDN echoing a huge body as "partial"). Content-Range start = 0 so ONLY the
    /// oversized guard fires, not the range-mismatch guard.
    async fn spawn_always_oversized_206(content_length: u64) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    content_length - 1,
                    content_length,
                    content_length
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{}/", addr)
    }

    #[tokio::test]
    async fn fetch_cdn_range_rejects_and_invalidates_on_oversized_206() {
        // A 206 whose advertised Content-Length exceeds the 16 MB fetch cap (a non-compliant CDN
        // echoing a huge body as "partial") must be rejected as unusable and the cached URL dropped
        // for re-resolution — the 206 analogue of the whole-object-200 rejection.
        let url = spawn_always_oversized_206(MAX_FETCH_SIZE as u64 + 1_000_000).await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, 5_000_000_000);
        let result = f.fetch_bytes(8).await;
        assert!(result.is_err(), "an oversized 206 must be rejected");
        assert!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "an oversized 206 must invalidate the cached resolution for re-resolution"
        );
    }

    #[tokio::test]
    async fn fetch_cdn_range_does_not_invalidate_on_connection_error() {
        // A transient TCP failure must NOT invalidate — the URL is still valid, and many
        // readers invalidating at once would stampede the rate-limited resolve path.
        let url = spawn_connection_closing().await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, 1000);
        let _ = f.fetch_bytes(8).await; // errors after retries
        assert_eq!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a connection error must not invalidate the cached resolution"
        );
    }

    #[tokio::test]
    async fn fetch_cdn_range_recovers_from_expired_url() {
        // First request fails with 403 (expired URL); the retry fetches a fresh 206 and the
        // read succeeds inline without surfacing an error to the client.
        let body: &[u8] = b"FRESHBYTES123456"; // 16 bytes
        let url = spawn_first_status_then_206("403 Forbidden", body).await;
        let mut f = proxied_for(url, 0, body.len() as u64);
        let data = f
            .fetch_bytes(8)
            .await
            .expect("a 403 then 206 must recover and serve bytes");
        assert_eq!(&data[..], b"FRESHBYT");
    }

    #[tokio::test]
    async fn fetch_cdn_range_handles_206_without_content_length() {
        // A 206 that omits Content-Length (chunked/streaming CDN) falls to the non-drainable,
        // window-bounded read path (`drainable` needs a Content-Length <= the window). The requested
        // window must still be served correctly, with the body end signalled by connection close.
        let body: &[u8] = b"NOCLENGTH1234567"; // 16 bytes
        let url = spawn_206_no_content_length(body).await;
        let mut f = proxied_for(url, 0, body.len() as u64);
        let data = f
            .fetch_bytes(8)
            .await
            .expect("a 206 without Content-Length must still serve the window");
        assert_eq!(&data[..], b"NOCLENGT");
    }

    #[tokio::test]
    async fn fetch_cdn_range_transient_5xx_retries_same_url_without_invalidating() {
        // A transient origin 5xx is NOT a stale URL: the byte-fetch must retry the SAME url rather
        // than clear the cached resolution, so many concurrent readers (one per rclone read-ahead)
        // don't all stampede the rate-limited resolve path. First request 500, retry 206 → recovers,
        // and invalidate is never called (contrast `fetch_cdn_range_invalidates_on_http_error`'s 403).
        let body: &[u8] = b"FRESHBYTES123456"; // 16 bytes
        let url = spawn_first_status_then_206("500 Internal Server Error", body).await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, body.len() as u64);
        let data = f
            .fetch_bytes(8)
            .await
            .expect("a transient 500 then 206 must recover and serve bytes");
        assert_eq!(&data[..], b"FRESHBYT");
        assert_eq!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a transient 5xx must NOT invalidate the cached resolution (avoids the resolve stampede)"
        );
    }

    /// Server that replies `200 OK` (ignoring the Range header) advertising a HUGE Content-Length
    /// — far above MAX_FETCH_SIZE — but only writes `body` bytes. This is exactly TorBox's CDN
    /// behaviour on a large file: it ignores `bytes=0-N` and streams the WHOLE object from byte 0,
    /// so the advertised length is the multi-GB file size while the client only needs the first
    /// window. A correct client reads its window and drops the connection.
    async fn spawn_range_ignoring_200_huge_cl(body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                // Advertise a 5 GB body (>> MAX_FETCH_SIZE) but only send the window's worth.
                let head = "HTTP/1.1 200 OK\r\nContent-Length: 5000000000\r\n\r\n".to_string();
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
                // Keep the connection open briefly so the client can read its window and drop it.
                let _ = sock.read(&mut buf).await;
            }
        });
        format!("http://{}/", addr)
    }

    #[tokio::test]
    async fn fetch_cdn_range_accepts_range_ignoring_200_with_oversized_content_length_at_offset_0()
    {
        // TorBox's CDN ignores the Range header and replies `200 OK` streaming the WHOLE (multi-GB)
        // object from byte 0, advertising the full file size as Content-Length. At pos==0 that is a
        // VALID response — the client must read its window and drop the connection, NOT reject it as
        // "oversized" (which broke first-byte playback of every TorBox file > MAX_FETCH_SIZE).
        let body: &[u8] = b"TORBOX200BYTES16"; // 16 bytes
        let url = spawn_range_ignoring_200_huge_cl(body).await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, body.len() as u64);
        let data = f
            .fetch_bytes(8)
            .await
            .expect("a Range-ignoring 200 at offset 0 must serve its window, not be rejected");
        assert_eq!(&data[..], b"TORBOX20");
        assert_eq!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a valid Range-ignoring 200 must not invalidate the cached resolution"
        );
    }

    #[tokio::test]
    async fn fetch_cdn_range_truncates_an_oversending_200_to_the_window() {
        // The DoS backstop: at pos==0 a Range-ignoring 200 that physically streams MORE than the
        // fetch window must be read only up to the window and truncated (connection dropped), never
        // buffered whole. The 200 declares a 5 GB Content-Length and here actually sends 36 bytes,
        // exceeding the 10-byte window — the read must stop and keep only 10.
        let body: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"; // 36 bytes > the 10-byte window
        let url = spawn_range_ignoring_200_huge_cl(body).await;
        let (mut f, _inv) = proxied_with_counter(url, 0, 10); // file_size 10 → 10-byte window
        let data = f
            .fetch_bytes(8)
            .await
            .expect("a window-bounded oversending 200 at offset 0 must serve its window");
        assert_eq!(&data[..], &body[..8]);
        assert_eq!(
            f.buffer.len(),
            10,
            "the over-sent 200 must be truncated to the 10-byte window, not buffered whole"
        );
    }

    #[test]
    fn parse_content_range_start_extracts_start() {
        assert_eq!(parse_content_range_start("bytes 100-199/100000"), Some(100));
        assert_eq!(parse_content_range_start("bytes 0-9/*"), Some(0));
        assert_eq!(parse_content_range_start("nonsense"), None);
        assert_eq!(parse_content_range_start("bytes abc-9/100"), None);
    }

    /// Server that always answers `206` claiming it started at byte `start`, regardless of the
    /// requested Range — used to exercise the Content-Range mismatch guard.
    async fn spawn_206_claiming_start(start: u64, body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let end = start as usize + body.len() - 1;
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/100000\r\nContent-Length: {}\r\n\r\n",
                    start, end, body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{}/", addr)
    }

    #[tokio::test]
    async fn fetch_cdn_range_rejects_206_with_mismatched_content_range() {
        // We request pos=100 but the CDN answers 206 claiming it started at byte 0. Buffering it at
        // buffer_start=100 would silently serve the wrong bytes — the read must reject (invalidate).
        let url = spawn_206_claiming_start(0, b"WRONGBYTES").await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 100, 100_000);
        let r = f.fetch_bytes(8).await;
        assert!(
            r.is_err(),
            "a 206 whose Content-Range start disagrees with the request must not be served"
        );
        assert!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "a mismatched 206 must invalidate the cached resolution"
        );
    }

    #[tokio::test]
    async fn fetch_bytes_zero_len_returns_empty_without_network() {
        // A zero-length read must short-circuit before any CDN fetch (the server here would error).
        let url = spawn_connection_closing().await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, 1000);
        let out = f.fetch_bytes(0).await.expect("zero-length read is Ok");
        assert!(out.is_empty());
        assert_eq!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a zero-length read must not touch the network"
        );
    }

    #[tokio::test]
    async fn metadata_reports_plumbed_mtime() {
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut f = proxied(provider, crate::provider::FileLocator::default(), 42);
        f.modified_time = ts;
        let md = f.metadata().await.unwrap();
        assert_eq!(
            md.modified().unwrap(),
            ts,
            "opened-file mtime is the plumbed value"
        );
        assert_eq!(md.len(), 42);
    }

    #[tokio::test]
    async fn metadata_of_a_hidden_torrent_reports_size_zero() {
        // A Broken/hidden torrent's open file reports an empty, epoch-dated file so a player never
        // reads it while its replacement downloads.
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let locator = crate::provider::FileLocator {
            torrent_id: "t".into(),
            ..Default::default()
        };
        let mut f = proxied(provider, locator, 42);
        f.modified_time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        f.repair_manager.mark_broken("t").await;
        let md = f.metadata().await.unwrap();
        assert_eq!(md.len(), 0);
        assert_eq!(md.modified().unwrap(), SystemTime::UNIX_EPOCH);
        assert!(!md.is_dir());
    }

    #[tokio::test]
    async fn read_bytes_stamps_read_activity() {
        use crate::read_activity::ReadActivity;
        let ra = Arc::new(ReadActivity::without_startup_grace());
        let provider: Arc<dyn DebridProvider> = Arc::new(crate::provider::MockProvider {
            resolved_url: Some("http://127.0.0.1:0/none".into()),
            ..Default::default()
        });
        let locator = crate::provider::FileLocator {
            hash: "abc123".into(),
            torrent_id: "t".into(),
            ..Default::default()
        };
        let mut f = proxied(provider, locator, 10);
        f.read_activity = ra.clone();
        // The CDN fetch will fail (unroutable URL), but the stamp happens before the fetch.
        let _ = f.read_bytes(4).await;
        let window = std::time::Duration::from_secs(300);
        assert!(
            !ra.torrents_idle(["abc123"], window).await,
            "the read is recorded against the file's torrent"
        );
        assert!(ra.torrents_idle(["other"], window).await);
    }

    /// The locator of the broken movie file the instant-repair tests start from (torrent `old_tid`;
    /// the mock's repaired torrent is `new_tid`).
    fn broken_movie_locator() -> crate::provider::FileLocator {
        crate::provider::FileLocator {
            hash: "H".to_string(),
            torrent_id: "old_tid".to_string(),
            file_id: 1,
            file_path: "/Movie.mkv".to_string(),
            link: Some("https://rd/oldlink".to_string()),
        }
    }

    #[tokio::test]
    async fn resolve_cdn_url_repairs_and_swaps_locator_on_unavailable() {
        // The headline repair-on-playback flow: resolving the broken torrent returns
        // Unavailable, instant repair produces a fresh locator, the file swaps to it,
        // invalidates the old resolution, and re-resolves to a working URL.
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        let invalidate_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mock = MockProvider {
            // Only the original (broken) torrent resolves to Unavailable; the repaired one resolves.
            unavailable_torrent_ids: ["old_tid".to_string()].into_iter().collect(),
            resolved_url: Some("https://cdn/fresh".to_string()),
            add_magnet: Some(AddMagnetResponse {
                id: "new_tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new_tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["https://rd/newlink".to_string()],
                ..Default::default()
            }),
            invalidate_calls: invalidate_calls.clone(),
            ..Default::default()
        };
        let mut f = proxied(Arc::new(mock), broken_movie_locator(), 1000);

        let url = f
            .resolve_cdn_url()
            .await
            .expect("repair + swap must yield a fresh CDN URL");
        assert_eq!(url, "https://cdn/fresh");
        // The file now points at the repaired torrent...
        assert_eq!(f.locator.torrent_id, "new_tid");
        // ...and the stale resolution for the old locator was invalidated.
        assert!(invalidate_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn fetch_cdn_range_persistent_5xx_escalates_to_instant_repair() {
        // A file that resolves fine but whose CDN persistently returns 500 on the bytes is broken on
        // the provider, not a stale URL. After one same-URL retry (no re-resolve) still 5xxs, the
        // byte-fetch path must escalate to instant repair (re-add by hash) — swapping the locator —
        // rather than failing forever and letting the player retry-storm the 500.
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // The CDN always 500s (both the original and the re-resolved/repaired URL point here).
        let url = spawn_always_status("500 Internal Server Error").await;
        let mock = MockProvider {
            // resolve_url succeeds (NOT Unavailable) — we exercise the byte-fetch 5xx path.
            resolved_url: Some(url.clone()),
            add_magnet: Some(AddMagnetResponse {
                id: "new_tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new_tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["https://rd/newlink".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut f = proxied(Arc::new(mock), broken_movie_locator(), 1000);
        f.cdn_url = Some(url);

        // The read still fails (the repaired CDN also 500s), but repair must have been attempted —
        // proven by the locator swapping to the repaired torrent.
        let _ = f.fetch_bytes(8).await;
        assert_eq!(
            f.locator.torrent_id, "new_tid",
            "a persistent CDN 5xx on the bytes must escalate to instant repair (locator swapped)"
        );
    }

    #[tokio::test]
    async fn fetch_cdn_range_persistent_5xx_instant_repair_recovers_and_serves_bytes() {
        // The HAPPY path of the persistent-5xx escalation: instant repair yields a working (repaired)
        // URL and the byte read then COMPLETES inline. The escalation test above proves repair is
        // attempted; this proves the post-repair re-fetch (the `continue` after attempt_instant_repair)
        // actually recovers — so a persistent 500 self-heals instead of retry-storming the player.
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};
        let body: &[u8] = b"REPAIREDBYTES!!!"; // 16 bytes
        let url_500 = spawn_always_status("500 Internal Server Error").await;
        let url_206 = spawn_206_no_content_length(body).await;
        // The ORIGINAL torrent resolves to the 500 CDN; the REPAIRED torrent resolves to the 206 CDN.
        let mut by_tid = std::collections::HashMap::new();
        by_tid.insert("old_tid".to_string(), url_500.clone());
        by_tid.insert("new_tid".to_string(), url_206);
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolved_url_by_torrent_id: by_tid,
            add_magnet: Some(AddMagnetResponse {
                id: "new_tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new_tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["https://rd/newlink".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut f = proxied(provider, broken_movie_locator(), body.len() as u64);
        f.cdn_url = Some(url_500);
        let data = f
            .fetch_bytes(8)
            .await
            .expect("persistent 5xx → repair → repaired 206 must serve bytes");
        assert_eq!(&data[..], b"REPAIRED");
        assert_eq!(
            f.locator.torrent_id, "new_tid",
            "the locator must have swapped to the repaired torrent"
        );
    }

    /// Server that DROPS the first accepted connection (a transport error on the client) then serves a
    /// valid `206` on the next — to exercise the attempt-0 connection-error retry-then-recover branch.
    async fn spawn_close_then_206(body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut first = true;
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                if first {
                    first = false;
                    drop(sock); // close immediately → connection error on the client
                    continue;
                }
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{}/", addr)
    }

    #[tokio::test]
    async fn fetch_cdn_range_recovers_from_a_transient_connection_error() {
        // A transient TCP failure on attempt 0 (connection dropped) must be retried on the SAME url and
        // recover on the next 206 — without invalidating the resolution (the URL is still valid). The
        // existing connection-error test only covers the terminal-fail case; this covers recovery.
        let body: &[u8] = b"AFTERCLOSE123456"; // 16 bytes
        let url = spawn_close_then_206(body).await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, body.len() as u64);
        let data = f
            .fetch_bytes(8)
            .await
            .expect("a dropped connection on attempt 0 then a 206 must recover");
        assert_eq!(&data[..], b"AFTERCLO");
        assert_eq!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a transient connection error must NOT invalidate the cached resolution"
        );
    }

    /// Server whose first `truncated` connections answer `206` declaring `Content-Length: body.len()`
    /// but write only half the body before closing (a mid-body reset), and whose later connections
    /// send the full `206`. Counts accepted connections (one request each).
    async fn spawn_truncating_206(
        body: &'static [u8],
        truncated: usize,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let n = seen.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let sent = if n < truncated {
                    &body[..body.len() / 2]
                } else {
                    body
                };
                let _ = sock.write_all(sent).await;
                let _ = sock.flush().await;
                // Dropping the socket closes it; for a truncated reply that is a mid-body reset.
            }
        });
        (format!("http://{}/", addr), requests)
    }

    #[tokio::test]
    async fn fetch_cdn_range_retries_a_body_read_error_on_the_same_url() {
        // A connection that dies after the headers (a mid-body reset, a pooled connection closing,
        // the client timeout firing during the body) is the same transient failure as a send
        // error: retry once on the SAME url, without invalidating, and serve the bytes.
        let body: &[u8] = b"MIDBODYRESET1234"; // 16 bytes
        let (url, requests) = spawn_truncating_206(body, 1).await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, body.len() as u64);
        let data = f
            .fetch_bytes(8)
            .await
            .expect("a truncated body on attempt 0 then a full 206 must recover");
        assert_eq!(&data[..], b"MIDBODYR");
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a body read error must NOT invalidate the cached resolution"
        );
    }

    #[tokio::test]
    async fn fetch_cdn_range_fails_after_a_second_body_read_error() {
        // Every response is cut short: the read is retried exactly once, then fails rather than
        // serving truncated bytes, and the resolution is left alone.
        let body: &[u8] = b"MIDBODYRESET1234"; // 16 bytes
        let (url, requests) = spawn_truncating_206(body, usize::MAX).await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, body.len() as u64);
        assert!(f.fetch_bytes(8).await.is_err());
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one retry, then fail"
        );
        assert_eq!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn fetch_bytes_serves_from_read_ahead_buffer_without_network() {
        // A read fully inside the 2 MB read-ahead buffer must be served from it — no CDN fetch, no
        // invalidate — with correct offset math. This core buffer-hit branch had no coverage (every
        // other test starts with an empty buffer).
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolved_url: None, // any network attempt would surface (no URL to fetch)
            invalidate_calls: counter.clone(),
            ..Default::default()
        });
        let mut f = proxied(provider, crate::provider::FileLocator::default(), 10_000);
        f.pos = 104;
        f.buffer = bytes::Bytes::from_static(b"ABCDEFGHIJ"); // [100, 110)
        f.buffer_start = 100;
        let data = f
            .fetch_bytes(4)
            .await
            .expect("a buffer hit must serve bytes");
        assert_eq!(
            &data[..],
            b"EFGH",
            "served from buffer at offset (104-100)=4"
        );
        assert_eq!(f.pos, 108, "pos advances by the bytes served");
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a buffer hit must not touch the network / invalidate"
        );
    }

    #[tokio::test]
    async fn read_bytes_short_circuits_on_a_hidden_torrent_without_io_or_stamp() {
        // A Broken/hidden torrent must short-circuit read_bytes BEFORE any I/O — so it can never
        // re-trigger repair (which would trap it until restart) and never even stamps read-activity.
        use crate::read_activity::ReadActivity;
        let ra = Arc::new(ReadActivity::without_startup_grace());
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolved_url: Some("http://127.0.0.1:0/none".into()),
            invalidate_calls: counter.clone(),
            ..Default::default()
        });
        let locator = crate::provider::FileLocator {
            hash: "abc123".into(),
            torrent_id: "t".into(),
            ..Default::default()
        };
        let mut f = proxied(provider, locator, 10);
        f.read_activity = ra.clone();
        f.repair_manager.mark_broken("t").await; // hide the torrent
        assert!(
            f.read_bytes(4).await.is_err(),
            "a hidden torrent's read must fail fast"
        );
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no I/O (no invalidate) on a hidden torrent"
        );
        assert!(
            ra.all_idle(std::time::Duration::from_secs(300)).await,
            "read-activity must NOT be stamped for a short-circuited read"
        );
    }

    #[tokio::test]
    async fn fetch_cdn_range_rejects_an_empty_body_on_an_acceptable_response() {
        // A 206/offset-0-200 that streams ZERO bytes for a non-empty range must be rejected (drop the
        // URL, retry, then fail) rather than surfaced as a premature mid-file EOF that silently
        // truncates playback. `spawn_always_status` replies 206 with Content-Length: 0.
        let url = spawn_always_status("206 Partial Content").await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, 1000);
        assert!(
            f.fetch_bytes(8).await.is_err(),
            "an empty body for a non-empty range must not be served"
        );
        assert!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "an empty-body response must invalidate the cached resolution"
        );
    }

    #[tokio::test]
    async fn proxied_media_file_seek_resolves_offsets_and_guards_underflow() {
        // ProxiedMediaFile has its OWN seek impl (separate from VirtualFile's): Start/Current/End and
        // the checked_add underflow guards on Current/End must all be exercised here.
        use std::io::SeekFrom;
        let (mut f, _) = proxied_with_counter("http://x/".to_string(), 0, 1000);
        assert_eq!(f.seek(SeekFrom::Start(500)).await.unwrap(), 500);
        assert_eq!(f.seek(SeekFrom::Current(10)).await.unwrap(), 510);
        assert_eq!(f.seek(SeekFrom::End(-8)).await.unwrap(), 992); // file_size - 8
        assert!(
            f.seek(SeekFrom::Current(-100_000)).await.is_err(),
            "a Current seek before byte 0 must error, not underflow"
        );
        assert!(
            f.seek(SeekFrom::End(-100_000)).await.is_err(),
            "an End seek before byte 0 must error, not underflow"
        );
    }

    // --- Option A: keep-alive connection reuse + larger first read-ahead window ---

    /// A keep-alive HTTP/1.1 server that answers each ranged GET with a compliant `206` (correct
    /// `Content-Range`/`Content-Length` and exactly the requested bytes), serving MULTIPLE requests
    /// on the SAME connection. Records every accepted TCP connection (to prove reuse) and every
    /// requested `(start, end)` range (to prove window sizing).
    #[allow(clippy::type_complexity)]
    async fn spawn_keepalive_206(
        total: u64,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::Mutex<Vec<(u64, u64)>>>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let conns = Arc::new(AtomicUsize::new(0));
        let ranges: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let (c2, r2) = (conns.clone(), ranges.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                c2.fetch_add(1, Ordering::SeqCst);
                let r3 = r2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    loop {
                        let n = match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let (start, end) = req
                            .lines()
                            .find_map(|l| {
                                let v = l.trim();
                                let v = v
                                    .strip_prefix("Range:")
                                    .or_else(|| v.strip_prefix("range:"))?
                                    .trim()
                                    .strip_prefix("bytes=")?;
                                let mut it = v.split('-');
                                let s: u64 = it.next()?.trim().parse().ok()?;
                                let e: u64 = it.next()?.trim().parse().ok()?;
                                Some((s, e))
                            })
                            .unwrap_or((0, 0));
                        r3.lock().unwrap().push((start, end));
                        let len = end - start + 1;
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\n\r\n",
                            start, end, total, len
                        );
                        if sock.write_all(head.as_bytes()).await.is_err() {
                            break;
                        }
                        if sock.write_all(&vec![b'A'; len as usize]).await.is_err() {
                            break;
                        }
                        let _ = sock.flush().await;
                    }
                });
            }
        });
        (format!("http://{}/", addr), conns, ranges)
    }

    #[tokio::test]
    async fn bounded_206_reuses_pooled_connection_across_windows() {
        use std::sync::atomic::Ordering;
        // A 100 MB virtual file; two sequential 2 MB window fetches at different offsets.
        let total = 100 * 1024 * 1024u64;
        let (url, conns, _ranges) = spawn_keepalive_206(total).await;
        let mut f = proxied_for(url, 0, total);

        let a = f
            .fetch_cdn_range(0, BUFFER_SIZE as u64 - 1)
            .await
            .expect("first window");
        assert_eq!(a.len(), BUFFER_SIZE);
        let b = f
            .fetch_cdn_range(BUFFER_SIZE as u64, 2 * BUFFER_SIZE as u64 - 1)
            .await
            .expect("second window");
        assert_eq!(b.len(), BUFFER_SIZE);

        // The bounded-206 drain reads each body to its natural end, so reqwest returns the socket to
        // the keep-alive pool and the second window reuses it. WITHOUT the drain fix the first fetch
        // would break mid-body, close the connection, and force a second TCP+TLS handshake → 2 conns.
        assert_eq!(
            conns.load(Ordering::SeqCst),
            1,
            "second window must reuse the pooled connection, not re-handshake"
        );
    }

    #[tokio::test]
    async fn first_read_uses_larger_window_then_buffer_size() {
        let total = 100 * 1024 * 1024u64;
        let (url, _conns, ranges) = spawn_keepalive_206(total).await;
        let mut f = proxied_for(url, 0, total);
        f.first_read_bytes = 8 * 1024 * 1024; // simulate CDN_FIRST_READ_MB=8

        // Cold first read (buffer empty) → one 8 MB window.
        let _ = f.fetch_bytes(8).await.expect("cold read");
        // Advance past the 8 MB buffer to force a warm-miss fetch.
        f.pos = 8 * 1024 * 1024;
        let _ = f.fetch_bytes(8).await.expect("warm-miss read");

        let r = ranges.lock().unwrap().clone();
        assert_eq!(r.len(), 2, "exactly two CDN fetches, got {r:?}");
        assert_eq!(
            r[0],
            (0, 8 * 1024 * 1024 - 1),
            "cold first read must use the larger first_read_bytes (8 MB) window"
        );
        assert_eq!(
            r[1].1 - r[1].0 + 1,
            BUFFER_SIZE as u64,
            "subsequent reads must fall back to the BUFFER_SIZE window"
        );
    }

    #[tokio::test]
    async fn default_first_read_is_buffer_size() {
        // With the default field value (no enlargement), the cold first read uses BUFFER_SIZE — so a
        // deployment that doesn't set CDN_FIRST_READ_MB behaves exactly as before.
        let total = 100 * 1024 * 1024u64;
        let (url, _conns, ranges) = spawn_keepalive_206(total).await;
        let mut f = proxied_for(url, 0, total); // proxied_for defaults first_read_bytes = BUFFER_SIZE
        let _ = f.fetch_bytes(8).await.expect("cold read");
        let r = ranges.lock().unwrap().clone();
        assert_eq!(r[0], (0, BUFFER_SIZE as u64 - 1));
    }
}
