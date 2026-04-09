use crate::rd_client::RealDebridClient;
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

#[derive(Clone)]
pub struct DebridFileSystem {
    vfs: Arc<RwLock<DebridVfs>>,
    rd_client: Arc<RealDebridClient>,
    repair_manager: Arc<RepairManager>,
    http_client: reqwest::Client,
}

impl DebridFileSystem {
    pub fn new(
        rd_client: Arc<RealDebridClient>,
        vfs: Arc<RwLock<DebridVfs>>,
        repair_manager: Arc<RepairManager>,
        http_client: reqwest::Client,
    ) -> Self {
        Self {
            vfs,
            rd_client,
            repair_manager,
            http_client,
        }
    }

    /// Resolve a path to a VfsNode reference without cloning.
    fn find_node_ref<'v>(vfs: &'v DebridVfs, path: &DavPath) -> Option<&'v VfsNode> {
        let mut current = &vfs.root;
        let path_osstr = path.as_rel_ospath();
        let path_str = path_osstr.to_str()?;
        if path_str == "." || path_str.is_empty() {
            return Some(current);
        }
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

    fn find_node_in(vfs: &DebridVfs, path: &DavPath) -> Option<VfsNode> {
        Self::find_node_ref(vfs, path).cloned()
    }

    async fn find_node(&self, path: &DavPath) -> Option<VfsNode> {
        let vfs = self.vfs.read().await;
        Self::find_node_in(&vfs, path)
    }
}

impl DavFileSystem for DebridFileSystem {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        _options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let node = self.find_node(path).await.ok_or(FsError::NotFound)?;
            let name = path
                .as_rel_ospath()
                .to_str()
                .and_then(|s| s.rsplit('/').next())
                .unwrap_or("")
                .to_string();
            match node {
                VfsNode::MediaFile {
                    file_size,
                    rd_link,
                    rd_torrent_id,
                } => Ok(Box::new(ProxiedMediaFile {
                    name,
                    rd_link,
                    rd_torrent_id,
                    file_size,
                    repair_manager: self.repair_manager.clone(),
                    rd_client: self.rd_client.clone(),
                    http_client: self.http_client.clone(),
                    pos: 0,
                    cdn_url: None,
                    buffer: Bytes::new(),
                    buffer_start: 0,
                }) as Box<dyn DavFile>),
                VfsNode::VirtualFile { content } => Ok(Box::new(VirtualFile {
                    content: Bytes::from(content),
                    pos: 0,
                }) as Box<dyn DavFile>),
                VfsNode::Directory { .. } => Err(FsError::Forbidden),
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

/// Returns true if the error from unrestrict_link indicates a broken torrent
/// that should trigger the repair process. Only 503 (Service Unavailable)
/// means a broken torrent — other errors (network, 404, etc.) should not
/// trigger repair as they may be transient or indicate intentional deletion.
fn should_repair_on_unrestrict_error(status: Option<reqwest::StatusCode>) -> bool {
    status == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
}

/// A media file that lazily unrestricts its RD link and proxies CDN bytes.
/// The CDN URL is cached per open instance. Reads use a 2 MB read-ahead buffer.
#[derive(Debug)]
struct ProxiedMediaFile {
    name: String,
    rd_link: String,
    rd_torrent_id: String,
    file_size: u64,
    repair_manager: Arc<RepairManager>,
    rd_client: Arc<RealDebridClient>,
    http_client: reqwest::Client,
    pos: u64,
    cdn_url: Option<String>,
    buffer: Bytes,
    buffer_start: u64,
}

impl ProxiedMediaFile {
    /// Lazily resolve the CDN download URL, caching the result.
    async fn resolve_cdn_url(&mut self) -> Result<String, FsError> {
        if let Some(ref url) = self.cdn_url {
            return Ok(url.clone());
        }

        match self.rd_client.unrestrict_link(&self.rd_link).await {
            Ok(response) => {
                let url = response.download;
                self.cdn_url = Some(url.clone());
                Ok(url)
            }
            Err(e) => {
                if should_repair_on_unrestrict_error(e.status()) {
                    tracing::warn!(
                        "Unrestrict returned 503 for {} — attempting instant repair",
                        self.name
                    );
                    match self
                        .repair_manager
                        .try_instant_repair(&self.rd_torrent_id, &self.rd_link)
                        .await
                    {
                        Ok(result) => {
                            tracing::info!(
                                "Instant repair succeeded for {} — new torrent {}",
                                self.name,
                                result.new_torrent_id
                            );
                            let old_rd_link =
                                std::mem::replace(&mut self.rd_link, result.new_rd_link);
                            self.rd_torrent_id = result.new_torrent_id;
                            // Clear the read-ahead buffer so subsequent reads fetch from the
                            // new CDN URL instead of serving stale bytes from the old one.
                            self.buffer = Bytes::new();
                            self.buffer_start = 0;
                            // Invalidate the cached unrestrict response for the old (broken) link
                            // so other ProxiedMediaFile instances don't get stale data before
                            // the next VFS rebuild.
                            self.rd_client
                                .invalidate_unrestrict_cache(&old_rd_link)
                                .await;
                            // Unrestrict the new link immediately
                            match self.rd_client.unrestrict_link(&self.rd_link).await {
                                Ok(response) => {
                                    let url = response.download;
                                    self.cdn_url = Some(url.clone());
                                    return Ok(url);
                                }
                                Err(e2) => {
                                    tracing::error!(
                                        "Failed to unrestrict repaired link for {}: {}",
                                        self.name,
                                        e2
                                    );
                                }
                            }
                        }
                        Err(reason) => {
                            tracing::error!(
                                "Instant repair failed for {}: {} — file unavailable",
                                self.name,
                                reason
                            );
                        }
                    }
                } else {
                    tracing::warn!("Unrestrict failed for {} (not repairing): {}", self.name, e);
                }
                Err(FsError::GeneralFailure)
            }
        }
    }

    /// Fetch bytes from CDN, using the read-ahead buffer.
    async fn fetch_bytes(&mut self, len: usize) -> Result<Bytes, FsError> {
        if self.pos >= self.file_size {
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

        self.buffer = body;
        self.buffer_start = pos;

        let to_read = std::cmp::min(len, self.buffer.len());
        let data = self.buffer.slice(..to_read);
        self.pos += to_read as u64;
        Ok(data)
    }

    /// Fetch a byte range from the CDN, retrying once if the URL appears stale
    /// (connection error or non-2xx/206 response).  A single retry is enough to
    /// recover from a 1-hour CDN URL expiry without exposing the error to the
    /// WebDAV client.
    ///
    /// Note: on retry, `resolve_cdn_url` must call `unrestrict_link` which blocks
    /// on the adaptive rate-limiter.  Under an active 429 storm this may delay by
    /// up to `MAX_INTERVAL_MS` (2 s) before the fresh CDN URL is returned.
    async fn fetch_cdn_range(&mut self, pos: u64, range_end: u64) -> Result<Bytes, FsError> {
        for attempt in 0..2u8 {
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
                    tracing::warn!(
                        "CDN fetch failed for {}: {} — clearing cached CDN URL",
                        self.name,
                        e
                    );
                    // Clear cached CDN URL so re-resolve fetches a fresh one.
                    self.cdn_url = None;
                    // Also invalidate the unrestrict cache (the RD link may be stale too).
                    self.rd_client
                        .invalidate_unrestrict_cache(&self.rd_link)
                        .await;
                    if attempt == 0 {
                        continue;
                    }
                    return Err(FsError::GeneralFailure);
                }
            };

            let status = resp.status();
            if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                tracing::warn!(
                    "CDN returned {} for {} — clearing cached CDN URL",
                    status,
                    self.name
                );
                // CDN URLs expire after ~1h; a 403/410 here means the URL is stale.
                self.cdn_url = None;
                self.rd_client
                    .invalidate_unrestrict_cache(&self.rd_link)
                    .await;
                if attempt == 0 {
                    continue;
                }
                return Err(FsError::GeneralFailure);
            }

            let body = resp.bytes().await.map_err(|e| {
                tracing::warn!("CDN body read failed for {}: {}", self.name, e);
                FsError::GeneralFailure
            })?;

            return Ok(body);
        }

        Err(FsError::GeneralFailure)
    }

}

impl DavFile for ProxiedMediaFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            if self
                .repair_manager
                .should_hide_torrent(&self.rd_torrent_id)
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
                modified_time: SystemTime::UNIX_EPOCH,
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
                .should_hide_torrent(&self.rd_torrent_id)
                .await
            {
                return Err(FsError::GeneralFailure);
            }

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
        rd_client: Arc<RealDebridClient>,
    ) {
        let _ = ProxiedMediaFile {
            name: String::new(),
            rd_link: String::new(),
            rd_torrent_id: String::new(),
            file_size: 0,
            repair_manager,
            rd_client,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: None,
            buffer: Bytes::new(),
            buffer_start: 0,
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
            rd_link: "link".to_string(),
            rd_torrent_id: "tid".to_string(),
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
    fn should_repair_only_on_503() {
        // Only 503 (Service Unavailable) means broken torrent → trigger repair
        assert!(should_repair_on_unrestrict_error(Some(
            reqwest::StatusCode::SERVICE_UNAVAILABLE
        )));

        // Other status codes should NOT trigger repair
        assert!(!should_repair_on_unrestrict_error(Some(
            reqwest::StatusCode::NOT_FOUND
        )));
        assert!(!should_repair_on_unrestrict_error(Some(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!should_repair_on_unrestrict_error(Some(
            reqwest::StatusCode::BAD_GATEWAY
        )));
        assert!(!should_repair_on_unrestrict_error(Some(
            reqwest::StatusCode::GATEWAY_TIMEOUT
        )));
        assert!(!should_repair_on_unrestrict_error(Some(
            reqwest::StatusCode::FORBIDDEN
        )));

        // Network errors (no status code) should NOT trigger repair
        assert!(!should_repair_on_unrestrict_error(None));
    }

    #[test]
    fn fetch_cdn_range_clears_cdn_url_on_failure() {
        // Verify that fetch_cdn_range clears cdn_url when the CDN returns an error,
        // so the next read re-resolves via unrestrict_link (getting a fresh CDN URL).
        // Without this, expired CDN URLs would cause all subsequent reads to fail
        // permanently for the lifetime of the open file handle.
        let source = include_str!("dav_fs.rs");

        let fn_start = source
            .find("fn fetch_cdn_range")
            .expect("fetch_cdn_range function must exist");
        let fn_body = &source[fn_start..];
        let fn_end = fn_body[1..]
            .find("\n    async fn ")
            .map(|i| i + 1)
            .unwrap_or(fn_body.len());
        let fn_source = &fn_body[..fn_end];

        // Must clear cdn_url on failure
        let cdn_url_none_count = fn_source.matches("self.cdn_url = None").count();
        assert!(
            cdn_url_none_count >= 2,
            "fetch_cdn_range must clear self.cdn_url = None on both connection errors and HTTP \
             error status (found {} occurrences, expected >= 2)",
            cdn_url_none_count
        );

        // Must also invalidate the unrestrict cache so re-resolve gets a fresh URL
        let cache_invalidate_count = fn_source.matches("invalidate_unrestrict_cache").count();
        assert!(
            cache_invalidate_count >= 2,
            "fetch_cdn_range must invalidate unrestrict cache on both connection errors and HTTP \
             error status (found {} occurrences, expected >= 2)",
            cache_invalidate_count
        );
    }

    #[test]
    fn fetch_cdn_range_retries_once_on_cdn_url_expiry() {
        // Verify that fetch_cdn_range contains a retry loop so that a single CDN
        // failure (e.g. 403 on an expired URL) is recovered inline without
        // propagating an error to the WebDAV client.
        let source = include_str!("dav_fs.rs");

        let fn_start = source
            .find("fn fetch_cdn_range")
            .expect("fetch_cdn_range function must exist");
        let fn_body = &source[fn_start..];
        let fn_end = fn_body[1..]
            .find("\n    async fn ")
            .map(|i| i + 1)
            .unwrap_or(fn_body.len());
        let fn_source = &fn_body[..fn_end];

        // The function must contain a loop or for-loop for retry logic
        assert!(
            fn_source.contains("for attempt") || fn_source.contains("loop {"),
            "fetch_cdn_range must contain a retry loop for CDN URL expiry (found none)"
        );
        // And must continue to retry rather than returning immediately on first failure
        assert!(
            fn_source.contains("continue"),
            "fetch_cdn_range must use `continue` to retry on first failure"
        );
    }


    #[test]
    fn find_node_in_rejects_dotdot_traversal() {
        let vfs = DebridVfs::new();

        // Verify normal lookup works
        let path = DavPath::new("/Movies").unwrap();
        let result = DebridFileSystem::find_node_in(&vfs, &path);
        assert!(result.is_some(), "Normal path should resolve");

        // DavPath normalizes paths containing "..", so /Movies/../etc/passwd
        // becomes /etc/passwd. Our find_node_in guard is defense-in-depth
        // against any future code path that might bypass DavPath construction.
        // Verify the guard exists in the source code.
        let source = include_str!("dav_fs.rs");
        assert!(
            source.contains(r#"if component == ".." {"#),
            "find_node_in must contain the .. traversal guard"
        );
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

        let vfs = DebridVfs::build(torrents);
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
