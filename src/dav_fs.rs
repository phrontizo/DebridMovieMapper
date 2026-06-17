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

/// 2 MB read-ahead buffer per open file
const BUFFER_SIZE: usize = 2 * 1024 * 1024;

/// Maximum single CDN fetch to prevent unbounded memory growth (16 MB)
const MAX_FETCH_SIZE: usize = 16 * 1024 * 1024;

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

#[derive(Clone)]
pub struct DebridFileSystem {
    vfs: Arc<RwLock<DebridVfs>>,
    rd_client: Arc<dyn DebridProvider>,
    repair_manager: Arc<RepairManager>,
    http_client: reqwest::Client,
    read_activity: Arc<crate::read_activity::ReadActivity>,
}

impl DebridFileSystem {
    pub fn new(
        rd_client: Arc<dyn DebridProvider>,
        vfs: Arc<RwLock<DebridVfs>>,
        repair_manager: Arc<RepairManager>,
        http_client: reqwest::Client,
        read_activity: Arc<crate::read_activity::ReadActivity>,
    ) -> Self {
        Self {
            vfs,
            rd_client,
            repair_manager,
            http_client,
            read_activity,
        }
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
            let vfs_path = rel.to_string_lossy().into_owned();
            // Timestamp lookup key, normalised exactly like the `metadata`/`read_dir` paths so an
            // opened file reports the SAME mtime it does when listed (see below — the open-file
            // metadata used to hardcode UNIX_EPOCH, confusing media-server change detection).
            let ts_key = rel
                .to_str()
                .unwrap_or("")
                .trim_matches('/')
                .trim_start_matches("./")
                .to_string();
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
                Virtual(Vec<u8>),
            }
            let payload = {
                let vfs = self.vfs.read().await;
                match Self::find_node_ref(&vfs, path).ok_or(FsError::NotFound)? {
                    VfsNode::MediaFile { file_size, locator } => Payload::Media {
                        file_size: *file_size,
                        locator: locator.clone(),
                        modified_time: vfs.timestamps.get(&ts_key).copied().unwrap_or(UNIX_EPOCH),
                    },
                    VfsNode::VirtualFile { content } => Payload::Virtual(content.clone()),
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
                    rd_client: self.rd_client.clone(),
                    http_client: self.http_client.clone(),
                    pos: 0,
                    cdn_url: None,
                    buffer: Bytes::new(),
                    buffer_start: 0,
                    read_activity: self.read_activity.clone(),
                    vfs_path,
                    modified_time,
                    confirmed_read: false,
                }) as Box<dyn DavFile>),
                Payload::Virtual(content) => Ok(Box::new(VirtualFile {
                    content: Bytes::from(content),
                    pos: 0,
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
            let path_str = path
                .as_rel_ospath()
                .to_str()
                .unwrap_or("")
                .trim_matches('/')
                .trim_start_matches("./");
            if let VfsNode::Directory { children } = node {
                let mut entries: Vec<Box<dyn DavDirEntry>> = Vec::new();
                for (name, child) in children {
                    let child_path = if path_str.is_empty() || path_str == "." {
                        name.clone()
                    } else {
                        format!("{}/{}", path_str, name)
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
            let path_str = path
                .as_rel_ospath()
                .to_str()
                .unwrap_or("")
                .trim_matches('/')
                .trim_start_matches("./");
            let modified_time = vfs.timestamps.get(path_str).copied().unwrap_or(UNIX_EPOCH);
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

/// A media file that lazily unrestricts its RD link and proxies CDN bytes.
/// The CDN URL is cached per open instance. Reads use a 2 MB read-ahead buffer.
struct ProxiedMediaFile {
    name: String,
    locator: crate::provider::FileLocator,
    file_size: u64,
    repair_manager: Arc<RepairManager>,
    rd_client: Arc<dyn DebridProvider>,
    http_client: reqwest::Client,
    pos: u64,
    cdn_url: Option<String>,
    buffer: Bytes,
    buffer_start: u64,
    read_activity: Arc<crate::read_activity::ReadActivity>,
    vfs_path: String,
    /// Real per-torrent mtime (from the VFS `timestamps`), captured at open so the opened-file
    /// metadata matches what `read_dir`/`metadata` report for the same path.
    modified_time: SystemTime,
    /// Set once the first CDN fetch succeeds, so the repair-budget reset (`note_read_success`) fires
    /// at most once per opened file rather than per chunk.
    confirmed_read: bool,
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

        match self.rd_client.resolve_url(&self.locator).await {
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
                self.rd_client.invalidate(&old_locator).await;
                match self.rd_client.resolve_url(&self.locator).await {
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
            Err(reason) => {
                // "not cached" is the NORMAL lapsed-cache outcome: the torrent is being re-downloaded
                // and the scan loop will surface it once ready — logging that at error! on every
                // cooldown-spaced read of a lapsed file is wrong-level noise. Reserve warn! for an
                // actual repair failure (rate-limited / max-attempts / info-fetch error).
                if reason.contains("not cached") {
                    tracing::info!(
                        "Instant repair for {}: {} — leaving to download",
                        self.name,
                        reason
                    );
                } else {
                    tracing::warn!("Instant repair failed for {}: {}", self.name, reason);
                }
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
        let buffer_end = self.buffer_start + self.buffer.len() as u64;

        // Buffer hit — serve directly without any network I/O.
        if pos >= self.buffer_start && pos < buffer_end {
            let offset = (pos - self.buffer_start) as usize;
            let available = self.buffer.len() - offset;
            let to_read = std::cmp::min(len, available);
            let data = self.buffer.slice(offset..offset + to_read);
            self.pos += to_read as u64;
            return Ok(data);
        }

        // Buffer miss — fetch from CDN (with retry on expired URL).
        let fetch_size = len.clamp(BUFFER_SIZE, MAX_FETCH_SIZE) as u64;
        let range_end = std::cmp::min(pos + fetch_size - 1, self.file_size - 1);
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

    /// Fetch a byte range from the CDN with one retry per error type:
    ///
    /// - **Connection error** (TCP failure, timeout): retry immediately with the
    ///   same CDN URL — the URL is still valid, the error is transient.  The cache
    ///   is intentionally left intact so concurrent readers do not all re-call
    ///   `resolve_url` through the rate-limiter at once.
    ///
    /// - **HTTP error** (non-2xx/206): the CDN URL has likely expired (~1 h TTL).
    ///   Clear `cdn_url` and invalidate the unrestrict cache, then retry to get a
    ///   fresh URL.  `resolve_cdn_url` will call `resolve_url`, which blocks on
    ///   the adaptive rate-limiter (up to `MAX_INTERVAL_MS` / 2 s under 429 storm).
    async fn fetch_cdn_range(&mut self, pos: u64, range_end: u64) -> Result<Bytes, FsError> {
        // Three attempts: original url, one cheap fresh-url retry, then (only if a 5xx persisted) one
        // post-instant-repair fetch. `repaired` caps the repair escalation at once per call.
        let mut repaired = false;
        for attempt in 0..3u8 {
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
                && resp
                    .content_length()
                    .is_some_and(|cl| cl > MAX_FETCH_SIZE as u64);
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
                self.rd_client.invalidate(&self.locator).await;
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
            let mut body = bytes::BytesMut::with_capacity(want_usize);
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        body.extend_from_slice(&chunk);
                        // Stop once the requested window is satisfied and drop the rest of the
                        // connection: a Range-ignoring 200 (TorBox) streams the WHOLE multi-GB object
                        // from byte 0, so we must read only `want` bytes rather than the entire file
                        // (mirrors `probe::read_body`). `want` <= MAX_FETCH_SIZE, so this also bounds
                        // the allocation; the check below is a hard backstop.
                        if body.len() >= want_usize {
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
                            self.rd_client.invalidate(&self.locator).await;
                            return Err(FsError::GeneralFailure);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        // Strip the signed CDN URL from the error Display (see above).
                        tracing::warn!(
                            "CDN body read failed for {}: {}",
                            self.name,
                            e.without_url()
                        );
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
                self.rd_client.invalidate(&self.locator).await;
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
            self.read_activity.touch(&self.vfs_path).await;
            self.fetch_bytes(len).await
        }
        .boxed()
    }

    fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let new_pos = match pos {
                std::io::SeekFrom::Start(p) => p,
                std::io::SeekFrom::Current(p) => {
                    let base = self.pos as i64;
                    let result = base
                        .checked_add(p)
                        .filter(|&n| n >= 0)
                        .ok_or(FsError::GeneralFailure)?;
                    result as u64
                }
                std::io::SeekFrom::End(p) => {
                    let size = self.file_size as i64;
                    let result = size
                        .checked_add(p)
                        .filter(|&n| n >= 0)
                        .ok_or(FsError::GeneralFailure)?;
                    result as u64
                }
            };
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

    /// Compile-time check: ProxiedMediaFile has all expected fields.
    /// Fails to compile if any field is removed or renamed.
    #[allow(dead_code)]
    fn _assert_proxied_media_file_has_on_demand_fields(
        repair_manager: Arc<RepairManager>,
        rd_client: Arc<dyn crate::provider::DebridProvider>,
    ) {
        use crate::provider::FileLocator;
        let _ = ProxiedMediaFile {
            name: String::new(),
            locator: FileLocator {
                ..Default::default()
            },
            file_size: 0,
            repair_manager,
            rd_client,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: None,
            buffer: Bytes::new(),
            buffer_start: 0,
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
            vfs_path: String::new(),
            modified_time: SystemTime::UNIX_EPOCH,
            confirmed_read: false,
        };
    }

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
}

impl DavFile for VirtualFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = DebridMetaData {
            is_directory: false,
            size: self.content.len() as u64,
            modified_time: SystemTime::UNIX_EPOCH,
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
            let end = std::cmp::min(start + len, self.content.len());
            let data = self.content.slice(start..end);

            self.pos += data.len() as u64;
            Ok(data)
        }
        .boxed()
    }

    fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let new_pos = match pos {
                std::io::SeekFrom::Start(p) => p,
                std::io::SeekFrom::Current(p) => {
                    let base = self.pos as i64;
                    let result = base
                        .checked_add(p)
                        .filter(|&n| n >= 0)
                        .ok_or(FsError::GeneralFailure)?;
                    result as u64
                }
                std::io::SeekFrom::End(p) => {
                    let size = self.content.len() as i64;
                    let result = size
                        .checked_add(p)
                        .filter(|&n| n >= 0)
                        .ok_or(FsError::GeneralFailure)?;
                    result as u64
                }
            };
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

    #[test]
    fn debrid_filesystem_accepts_trait_object() {
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let vfs = Arc::new(RwLock::new(DebridVfs::new()));
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let http = reqwest::Client::new();
        let ra = Arc::new(crate::read_activity::ReadActivity::new());
        let _fs = DebridFileSystem::new(provider, vfs, repair, http, ra);
    }

    #[test]
    fn proxied_media_file_holds_locator() {
        use crate::provider::FileLocator;
        let loc = FileLocator {
            hash: "h".to_string(),
            torrent_id: "t".to_string(),
            file_id: 3,
            file_path: "Movie/Movie.mkv".to_string(),
            link: Some("https://rd/x".to_string()),
        };
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolved_url: Some("https://cdn/movie".to_string()),
            ..Default::default()
        });
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let f = ProxiedMediaFile {
            name: "Movie.mkv".to_string(),
            locator: loc.clone(),
            file_size: 10,
            repair_manager: repair,
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: None,
            buffer: bytes::Bytes::new(),
            buffer_start: 0,
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
            vfs_path: String::new(),
            modified_time: SystemTime::UNIX_EPOCH,
            confirmed_read: false,
        };
        assert_eq!(f.locator.file_id, 3);
    }

    #[tokio::test]
    async fn fetch_bytes_rejects_range_ignoring_200_after_seek() {
        use crate::provider::FileLocator;
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

        let url = format!("http://{}/", addr);
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolved_url: Some(url.clone()),
            ..Default::default()
        });
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let mut f = ProxiedMediaFile {
            name: "Movie.mkv".to_string(),
            locator: FileLocator::default(),
            file_size: 1000,
            repair_manager: repair,
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos: 500, // a seek to a non-zero offset
            cdn_url: Some(url),
            buffer: bytes::Bytes::new(),
            buffer_start: 0,
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
            vfs_path: String::new(),
            modified_time: SystemTime::UNIX_EPOCH,
            confirmed_read: false,
        };

        // The 200 carries the file from offset 0, not from 500 — serving it would hand the
        // reader the wrong bytes. It must error instead.
        let result = f.fetch_bytes(8).await;
        assert!(
            result.is_err(),
            "a Range-ignoring 200 at a non-zero offset must be rejected, not mis-served"
        );
    }

    /// Spawn a local server that replies `200 OK` with the given Content-Length header and
    /// body, ignoring the Range header. Returns the base URL.
    async fn spawn_range_ignoring_200(content_length: u64, body: &'static [u8]) -> String {
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
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    content_length
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{}/", addr)
    }

    fn proxied_for(url: String, pos: u64, file_size: u64) -> ProxiedMediaFile {
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolved_url: Some(url.clone()),
            ..Default::default()
        });
        let repair = Arc::new(RepairManager::new(provider.clone()));
        ProxiedMediaFile {
            name: "Movie.mkv".to_string(),
            locator: crate::provider::FileLocator::default(),
            file_size,
            repair_manager: repair,
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos,
            cdn_url: Some(url),
            buffer: bytes::Bytes::new(),
            buffer_start: 0,
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
            vfs_path: String::new(),
            modified_time: SystemTime::UNIX_EPOCH,
            confirmed_read: false,
        }
    }

    #[tokio::test]
    async fn fetch_bytes_rejects_whole_object_200_at_offset_zero() {
        // A 200 whose Content-Length exceeds the fetch window (whole multi-GB object) must
        // be refused even at offset 0, rather than buffered into memory.
        let url = spawn_range_ignoring_200(999_999_999, b"only-headers-matter").await;
        let mut f = proxied_for(url, 0, 5_000_000_000);
        let result = f.fetch_bytes(8).await;
        assert!(
            result.is_err(),
            "a whole-object 200 larger than the fetch window must be rejected"
        );
    }

    #[tokio::test]
    async fn fetch_bytes_accepts_small_200_at_offset_zero() {
        // A small whole-object 200 at offset 0 (Content-Length within the fetch window) is
        // legitimate (the bytes start at 0 = pos) and must be served.
        let body: &[u8] = b"ZEROBYTES1234567"; // 16 bytes
        let url = spawn_range_ignoring_200(body.len() as u64, body).await;
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
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let f = ProxiedMediaFile {
            name: "Movie.mkv".to_string(),
            locator: crate::provider::FileLocator::default(),
            file_size,
            repair_manager: repair,
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos,
            cdn_url: Some(url),
            buffer: bytes::Bytes::new(),
            buffer_start: 0,
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
            vfs_path: String::new(),
            modified_time: SystemTime::UNIX_EPOCH,
            confirmed_read: false,
        };
        (f, counter)
    }

    #[tokio::test]
    async fn fetch_cdn_range_invalidates_on_http_error() {
        // A 4xx/5xx means the CDN URL has expired — drop the cached resolution so a fresh
        // URL is fetched on retry.
        let url = spawn_always_status("403 Forbidden").await;
        let (mut f, invalidate_calls) = proxied_with_counter(url, 0, 1000);
        let _ = f.fetch_bytes(8).await; // errors after retries
        assert!(
            invalidate_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "an HTTP error must invalidate the cached resolution"
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
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut f = ProxiedMediaFile {
            name: "M.mkv".into(),
            locator: crate::provider::FileLocator::default(),
            file_size: 42,
            repair_manager: repair,
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: None,
            buffer: bytes::Bytes::new(),
            buffer_start: 0,
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
            vfs_path: String::new(),
            modified_time: ts,
            confirmed_read: false,
        };
        let md = f.metadata().await.unwrap();
        assert_eq!(
            md.modified().unwrap(),
            ts,
            "opened-file mtime is the plumbed value"
        );
        assert_eq!(md.len(), 42);
    }

    #[tokio::test]
    async fn read_bytes_stamps_read_activity() {
        use crate::read_activity::ReadActivity;
        let ra = Arc::new(ReadActivity::new());
        let provider: Arc<dyn DebridProvider> = Arc::new(crate::provider::MockProvider {
            resolved_url: Some("http://127.0.0.1:0/none".into()),
            ..Default::default()
        });
        let mut f = ProxiedMediaFile {
            name: "x.mkv".into(),
            locator: crate::provider::FileLocator {
                torrent_id: "t".into(),
                ..Default::default()
            },
            file_size: 10,
            repair_manager: Arc::new(RepairManager::new(provider.clone())),
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: None,
            buffer: Bytes::new(),
            buffer_start: 0,
            read_activity: ra.clone(),
            vfs_path: "Movies/X/x.mkv".into(),
            modified_time: SystemTime::UNIX_EPOCH,
            confirmed_read: false,
        };
        // The CDN fetch will fail (unroutable URL), but the stamp happens before the fetch.
        let _ = f.read_bytes(4).await;
        assert!(
            !ra.is_idle("Movies/X/x.mkv", std::time::Duration::from_secs(300))
                .await
        );
    }

    #[tokio::test]
    async fn resolve_cdn_url_repairs_and_swaps_locator_on_unavailable() {
        // The headline repair-on-playback flow: resolving the broken torrent returns
        // Unavailable, instant repair produces a fresh locator, the file swaps to it,
        // invalidates the old resolution, and re-resolves to a working URL.
        use crate::provider::FileLocator;
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
        let provider: Arc<dyn DebridProvider> = Arc::new(mock);
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let mut f = ProxiedMediaFile {
            name: "Movie.mkv".to_string(),
            locator: FileLocator {
                hash: "H".to_string(),
                torrent_id: "old_tid".to_string(),
                file_id: 1,
                file_path: "/Movie.mkv".to_string(),
                link: Some("https://rd/oldlink".to_string()),
            },
            file_size: 1000,
            repair_manager: repair,
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: None,
            buffer: bytes::Bytes::new(),
            buffer_start: 0,
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
            vfs_path: String::new(),
            modified_time: SystemTime::UNIX_EPOCH,
            confirmed_read: false,
        };

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
        // the provider, not a stale URL. After the cheap fresh-URL retry still 5xxs, the byte-fetch
        // path must escalate to instant repair (re-add by hash) — swapping the locator — rather than
        // failing forever and letting the player retry-storm the 500.
        use crate::provider::FileLocator;
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
        let provider: Arc<dyn DebridProvider> = Arc::new(mock);
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let mut f = ProxiedMediaFile {
            name: "Movie.mkv".to_string(),
            locator: FileLocator {
                hash: "H".to_string(),
                torrent_id: "old_tid".to_string(),
                file_id: 1,
                file_path: "/Movie.mkv".to_string(),
                link: Some("https://rd/oldlink".to_string()),
            },
            file_size: 1000,
            repair_manager: repair,
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: Some(url),
            buffer: bytes::Bytes::new(),
            buffer_start: 0,
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
            vfs_path: String::new(),
            modified_time: SystemTime::UNIX_EPOCH,
            confirmed_read: false,
        };

        // The read still fails (the repaired CDN also 500s), but repair must have been attempted —
        // proven by the locator swapping to the repaired torrent.
        let _ = f.fetch_bytes(8).await;
        assert_eq!(
            f.locator.torrent_id, "new_tid",
            "a persistent CDN 5xx on the bytes must escalate to instant repair (locator swapped)"
        );
    }
}
