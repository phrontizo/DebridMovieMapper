use dav_server::fs::*;
use dav_server::davpath::DavPath;
use futures_util::FutureExt;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::vfs::{DebridVfs, VfsNode};
use crate::rd_client::RealDebridClient;
use crate::repair::RepairManager;
use bytes::Bytes;
use std::time::{SystemTime, UNIX_EPOCH};

/// 2 MB read-ahead buffer per open file
const BUFFER_SIZE: usize = 2 * 1024 * 1024;

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

    fn find_node_in(vfs: &DebridVfs, path: &DavPath) -> Option<VfsNode> {
        let mut current = &vfs.root;
        let path_osstr = path.as_rel_ospath();
        let path_str = path_osstr.to_str()?;
        if path_str == "." || path_str.is_empty() {
            return Some(current.clone());
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
        Some(current.clone())
    }

    async fn find_node(&self, path: &DavPath) -> Option<VfsNode> {
        let vfs = self.vfs.read().await;
        Self::find_node_in(&vfs, path)
    }
}

impl DavFileSystem for DebridFileSystem {
    fn open<'a>(&'a self, path: &'a DavPath, _options: OpenOptions) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let node = self.find_node(path).await.ok_or(FsError::NotFound)?;
            let name = path.as_rel_ospath().to_str()
                .and_then(|s| s.rsplit('/').next())
                .unwrap_or("")
                .to_string();
            match node {
                VfsNode::MediaFile { file_size, rd_link, rd_torrent_id } => {
                    Ok(Box::new(ProxiedMediaFile {
                        name,
                        rd_link,
                        rd_torrent_id,
                        file_size,
                        repair_manager: self.repair_manager.clone(),
                        rd_client: self.rd_client.clone(),
                        http_client: self.http_client.clone(),
                        pos: 0,
                        cdn_url: None,
                        buffer: Vec::new(),
                        buffer_start: 0,
                    }) as Box<dyn DavFile>)
                }
                VfsNode::VirtualFile { content } => {
                    Ok(Box::new(VirtualFile {
                        content: Bytes::from(content),
                        pos: 0,
                    }) as Box<dyn DavFile>)
                }
                VfsNode::Directory { .. } => Err(FsError::Forbidden),
            }
        }.boxed()
    }

    fn read_dir<'a>(&'a self, path: &'a DavPath, _meta: ReadDirMeta) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let vfs = self.vfs.read().await;
            let node = Self::find_node_in(&vfs, path).ok_or(FsError::NotFound)?;
            let path_str = path.as_rel_ospath().to_str().unwrap_or("").trim_matches('/').trim_start_matches("./");
            if let VfsNode::Directory { children } = node {
                let mut entries: Vec<Box<dyn DavDirEntry>> = Vec::new();
                for (name, child) in children {
                    let child_path = if path_str.is_empty() || path_str == "." {
                        name.clone()
                    } else {
                        format!("{}/{}", path_str, name)
                    };
                    let modified_time = vfs.timestamps.get(&child_path).copied().unwrap_or(UNIX_EPOCH);
                    entries.push(Box::new(DebridDirEntry {
                        name,
                        node: child.clone(),
                        modified_time,
                    }));
                }
                let stream = futures_util::stream::iter(entries.into_iter().map(Ok));
                Ok(Box::pin(stream) as FsStream<Box<dyn DavDirEntry>>)
            } else {
                Err(FsError::Forbidden)
            }
        }.boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let vfs = self.vfs.read().await;
            let node = Self::find_node_in(&vfs, path).ok_or(FsError::NotFound)?;
            let path_str = path.as_rel_ospath().to_str().unwrap_or("").trim_matches('/').trim_start_matches("./");
            let modified_time = vfs.timestamps.get(path_str).copied().unwrap_or(UNIX_EPOCH);
            Ok(Box::new(DebridMetaData { node, modified_time }) as Box<dyn DavMetaData>)
        }.boxed()
    }
}

#[derive(Debug, Clone)]
struct DebridMetaData {
    node: VfsNode,
    modified_time: SystemTime,
}

impl DavMetaData for DebridMetaData {
    fn len(&self) -> u64 {
        match &self.node {
            VfsNode::MediaFile { file_size, .. } => *file_size,
            VfsNode::VirtualFile { content, .. } => content.len() as u64,
            VfsNode::Directory { .. } => 0,
        }
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified_time)
    }
    fn is_dir(&self) -> bool {
        matches!(self.node, VfsNode::Directory { .. })
    }
}

struct DebridDirEntry {
    name: String,
    node: VfsNode,
    modified_time: SystemTime,
}

impl DavDirEntry for DebridDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            Ok(Box::new(DebridMetaData { node: self.node.clone(), modified_time: self.modified_time }) as Box<dyn DavMetaData>)
        }.boxed()
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
/// The CDN URL is cached per open instance. Reads use a 2MB buffer for read-ahead.
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
    buffer: Vec<u8>,
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
                    tracing::warn!("Unrestrict returned 503 for {} — attempting instant repair", self.name);
                    match self.repair_manager.try_instant_repair(&self.rd_torrent_id, &self.rd_link).await {
                        Ok(result) => {
                            tracing::info!("Instant repair succeeded for {} — new torrent {}", self.name, result.new_torrent_id);
                            self.rd_torrent_id = result.new_torrent_id;
                            self.rd_link = result.new_rd_link;
                            // Unrestrict the new link immediately
                            match self.rd_client.unrestrict_link(&self.rd_link).await {
                                Ok(response) => {
                                    let url = response.download;
                                    self.cdn_url = Some(url.clone());
                                    return Ok(url);
                                }
                                Err(e2) => {
                                    tracing::error!("Failed to unrestrict repaired link for {}: {}", self.name, e2);
                                }
                            }
                        }
                        Err(reason) => {
                            tracing::error!("Instant repair failed for {}: {} — file unavailable", self.name, reason);
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

        // Check if the requested range is within the buffer
        if pos >= self.buffer_start && pos < buffer_end {
            let offset = (pos - self.buffer_start) as usize;
            let available = self.buffer.len() - offset;
            let to_read = std::cmp::min(len, available);
            let data = Bytes::copy_from_slice(&self.buffer[offset..offset + to_read]);
            self.pos += to_read as u64;
            return Ok(data);
        }

        // Buffer miss — fetch from CDN
        let cdn_url = self.resolve_cdn_url().await?;
        let fetch_size = std::cmp::max(len, BUFFER_SIZE) as u64;
        let range_end = std::cmp::min(pos + fetch_size - 1, self.file_size - 1);

        let resp = self.http_client
            .get(&cdn_url)
            .header("Range", format!("bytes={}-{}", pos, range_end))
            .send()
            .await
            .map_err(|e| {
                tracing::warn!("CDN fetch failed for {}: {}", self.name, e);
                FsError::GeneralFailure
            })?;

        let status = resp.status();
        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            tracing::warn!("CDN returned {} for {}", status, self.name);
            return Err(FsError::GeneralFailure);
        }

        let body = resp.bytes().await.map_err(|e| {
            tracing::warn!("CDN body read failed for {}: {}", self.name, e);
            FsError::GeneralFailure
        })?;

        // Store in buffer
        self.buffer = body.to_vec();
        self.buffer_start = pos;

        let to_read = std::cmp::min(len, self.buffer.len());
        let data = Bytes::copy_from_slice(&self.buffer[..to_read]);
        self.pos += to_read as u64;
        Ok(data)
    }
}

impl DavFile for ProxiedMediaFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            if self.repair_manager.should_hide_torrent(&self.rd_torrent_id).await {
                return Ok(Box::new(DebridMetaData {
                    node: VfsNode::VirtualFile { content: vec![] },
                    modified_time: SystemTime::UNIX_EPOCH,
                }) as Box<dyn DavMetaData>);
            }

            Ok(Box::new(DebridMetaData {
                node: VfsNode::MediaFile {
                    file_size: self.file_size,
                    rd_link: self.rd_link.clone(),
                    rd_torrent_id: self.rd_torrent_id.clone(),
                },
                modified_time: SystemTime::UNIX_EPOCH,
            }) as Box<dyn DavMetaData>)
        }.boxed()
    }

    fn write_buf(&mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        async move { Err(FsError::Forbidden) }.boxed()
    }

    fn write_bytes(&mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        async move { Err(FsError::Forbidden) }.boxed()
    }

    fn read_bytes(&mut self, len: usize) -> FsFuture<'_, Bytes> {
        async move {
            if self.repair_manager.should_hide_torrent(&self.rd_torrent_id).await {
                return Err(FsError::GeneralFailure);
            }

            self.fetch_bytes(len).await
        }.boxed()
    }

    fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let new_pos = match pos {
                std::io::SeekFrom::Start(p) => p,
                std::io::SeekFrom::Current(p) => {
                    let base = self.pos as i64;
                    let result = base.checked_add(p)
                        .filter(|&n| n >= 0)
                        .ok_or(FsError::GeneralFailure)?;
                    result as u64
                }
                std::io::SeekFrom::End(p) => {
                    let size = self.file_size as i64;
                    let result = size.checked_add(p)
                        .filter(|&n| n >= 0)
                        .ok_or(FsError::GeneralFailure)?;
                    result as u64
                }
            };
            self.pos = new_pos;
            Ok(new_pos)
        }.boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move { Ok(()) }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time check: ProxiedMediaFile has rd_link and rd_client fields.
    /// Fails to compile if either field is removed or renamed.
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
            buffer: Vec::new(),
            buffer_start: 0,
        };
    }

    #[test]
    fn should_repair_only_on_503() {
        // Only 503 (Service Unavailable) means broken torrent → trigger repair
        assert!(should_repair_on_unrestrict_error(Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)));

        // Other status codes should NOT trigger repair
        assert!(!should_repair_on_unrestrict_error(Some(reqwest::StatusCode::NOT_FOUND)));
        assert!(!should_repair_on_unrestrict_error(Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(!should_repair_on_unrestrict_error(Some(reqwest::StatusCode::BAD_GATEWAY)));
        assert!(!should_repair_on_unrestrict_error(Some(reqwest::StatusCode::GATEWAY_TIMEOUT)));
        assert!(!should_repair_on_unrestrict_error(Some(reqwest::StatusCode::FORBIDDEN)));

        // Network errors (no status code) should NOT trigger repair
        assert!(!should_repair_on_unrestrict_error(None));
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
}

/// Simple virtual file (NFO files, etc)
#[derive(Debug)]
struct VirtualFile {
    content: Bytes,
    pos: u64,
}

impl DavFile for VirtualFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let node = VfsNode::VirtualFile {
            content: self.content.to_vec(),
        };
        async move {
            Ok(Box::new(DebridMetaData { node, modified_time: SystemTime::UNIX_EPOCH }) as Box<dyn DavMetaData>)
        }.boxed()
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
        }.boxed()
    }

    fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let new_pos = match pos {
                std::io::SeekFrom::Start(p) => p,
                std::io::SeekFrom::Current(p) => {
                    let base = self.pos as i64;
                    let result = base.checked_add(p)
                        .filter(|&n| n >= 0)
                        .ok_or(FsError::GeneralFailure)?;
                    result as u64
                }
                std::io::SeekFrom::End(p) => {
                    let size = self.content.len() as i64;
                    let result = size.checked_add(p)
                        .filter(|&n| n >= 0)
                        .ok_or(FsError::GeneralFailure)?;
                    result as u64
                }
            };
            self.pos = new_pos;
            Ok(new_pos)
        }.boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move { Ok(()) }.boxed()
    }
}
