use dav_server::fs::*;
use dav_server::davpath::DavPath;
use futures_util::FutureExt;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::vfs::{DebridVfs, VfsNode};
use crate::rd_client::RealDebridClient;
use crate::repair::RepairManager;
use bytes::Bytes;
use std::time::{SystemTime, Duration};

#[derive(Clone)]
pub struct DebridFileSystem {
    pub vfs: Arc<RwLock<DebridVfs>>,
    pub rd_client: Arc<RealDebridClient>,
    pub repair_manager: Arc<RepairManager>,
}

impl DebridFileSystem {
    pub fn new(rd_client: Arc<RealDebridClient>, vfs: Arc<RwLock<DebridVfs>>, repair_manager: Arc<RepairManager>) -> Self {
        Self {
            vfs,
            rd_client,
            repair_manager,
        }
    }

    async fn find_node(&self, path: &DavPath) -> Option<VfsNode> {
        let vfs = self.vfs.read().await;
        let mut current = &vfs.root;

        let path_osstr = path.as_rel_ospath();
        let path_str = path_osstr.to_str().unwrap();
        if path_str == "." || path_str.is_empty() {
            return Some(current.clone());
        }

        for component in path_str.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            if let VfsNode::Directory { children, .. } = current {
                if let Some(next) = children.get(component) {
                    current = next;
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        Some(current.clone())
    }
}

impl DavFileSystem for DebridFileSystem {
    fn open<'a>(&'a self, path: &'a DavPath, _options: OpenOptions) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let node = self.find_node(path).await.ok_or(FsError::NotFound)?;
            match node {
                VfsNode::StrmFile { name, rd_link, rd_torrent_id } => {
                    Ok(Box::new(StrmFile {
                        name,
                        rd_link,
                        rd_torrent_id,
                        rd_client: self.rd_client.clone(),
                        repair_manager: self.repair_manager.clone(),
                        pos: 0,
                        content_cache: None,
                    }) as Box<dyn DavFile>)
                }
                VfsNode::VirtualFile { name, content } => {
                    Ok(Box::new(VirtualFile {
                        name,
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
            let node = self.find_node(path).await.ok_or(FsError::NotFound)?;
            if let VfsNode::Directory { children, .. } = node {
                let mut entries: Vec<Box<dyn DavDirEntry>> = Vec::new();
                for (name, child) in children {
                    entries.push(Box::new(DebridDirEntry {
                        name,
                        node: child.clone(),
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
            let node = self.find_node(path).await.ok_or(FsError::NotFound)?;
            Ok(Box::new(DebridMetaData { node }) as Box<dyn DavMetaData>)
        }.boxed()
    }
}

#[derive(Debug, Clone)]
struct DebridMetaData {
    node: VfsNode,
}

impl DavMetaData for DebridMetaData {
    fn len(&self) -> u64 {
        match &self.node {
            VfsNode::StrmFile { .. } => 200, // Approximate STRM file size (RD URLs are ~150-200 bytes)
            VfsNode::VirtualFile { content, .. } => content.len() as u64,
            VfsNode::Directory { .. } => 0,
        }
    }
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(SystemTime::now())
    }
    fn is_dir(&self) -> bool {
        matches!(self.node, VfsNode::Directory { .. })
    }
}

struct DebridDirEntry {
    name: String,
    node: VfsNode,
}

impl DavDirEntry for DebridDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.as_bytes().to_vec()
    }
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            Ok(Box::new(DebridMetaData { node: self.node.clone() }) as Box<dyn DavMetaData>)
        }.boxed()
    }
}

/// A STRM file that dynamically generates its content (the RD download URL) when read
#[derive(Debug)]
struct StrmFile {
    name: String,
    rd_link: String, // The torrents link that needs unrestricting
    rd_torrent_id: String,
    rd_client: Arc<RealDebridClient>,
    repair_manager: Arc<RepairManager>,
    pos: u64,
    content_cache: Option<Bytes>,
}

impl DavFile for StrmFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            // Generate content if not cached to get accurate size
            if self.content_cache.is_none() {
                match self.generate_content().await {
                    Ok(content) => {
                        self.content_cache = Some(content);
                    }
                    Err(e) => {
                        tracing::error!("Failed to generate STRM content for {}: {:?}", self.name, e);
                        // Return minimal metadata on error
                        return Ok(Box::new(DebridMetaData {
                            node: VfsNode::VirtualFile {
                                name: self.name.clone(),
                                content: vec![],
                            }
                        }) as Box<dyn DavMetaData>);
                    }
                }
            }

            let size = self.content_cache.as_ref().map(|c| c.len() as u64).unwrap_or(0);
            Ok(Box::new(DebridMetaData {
                node: VfsNode::VirtualFile {
                    name: self.name.clone(),
                    content: vec![0; size as usize], // Dummy content for size
                }
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
            // Generate content if not already cached
            if self.content_cache.is_none() {
                match self.generate_content().await {
                    Ok(content) => {
                        self.content_cache = Some(content);
                    }
                    Err(e) => {
                        tracing::error!("Failed to generate STRM content for {}: {:?}", self.name, e);
                        return Err(FsError::GeneralFailure);
                    }
                }
            }

            let content = self.content_cache.as_ref().unwrap();

            if self.pos >= content.len() as u64 {
                return Ok(Bytes::new());
            }

            let start = self.pos as usize;
            let end = std::cmp::min(start + len, content.len());
            let data = content.slice(start..end);

            self.pos += data.len() as u64;
            Ok(data)
        }.boxed()
    }

    fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let new_pos = match pos {
                std::io::SeekFrom::Start(p) => p,
                std::io::SeekFrom::Current(p) => (self.pos as i64 + p) as u64,
                std::io::SeekFrom::End(p) => {
                    // Need to know content size
                    if self.content_cache.is_none() {
                        match self.generate_content().await {
                            Ok(content) => {
                                self.content_cache = Some(content);
                            }
                            Err(_) => return Err(FsError::GeneralFailure),
                        }
                    }
                    let size = self.content_cache.as_ref().unwrap().len() as i64;
                    (size + p) as u64
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

impl StrmFile {
    /// Generate the STRM file content by unrestricting the RD link
    /// Returns the direct download URL that Jellyfin will stream from
    async fn generate_content(&self) -> Result<Bytes, Box<dyn std::error::Error + Send + Sync>> {
        // Check if torrent is under repair - if so, return error
        if self.repair_manager.should_hide_torrent(&self.rd_torrent_id).await {
            return Err("Torrent is under repair".into());
        }

        // Unrestrict the link to get the direct download URL
        match self.rd_client.unrestrict_link(&self.rd_link).await {
            Ok(response) => {
                // The STRM file contains just the direct download URL
                let url = response.download;
                tracing::debug!("Generated STRM content for {}: {}", self.name, url);
                Ok(Bytes::from(format!("{}\n", url)))
            }
            Err(e) => {
                let error_str = e.to_string();

                // Check if it's a 503 error - mark for repair
                if error_str.contains("503") || error_str.contains("Service Unavailable") {
                    tracing::warn!("503 error unrestricting {} - marking for repair", self.rd_link);
                    self.repair_manager.mark_broken(&self.rd_torrent_id, &self.rd_link).await;

                    // Trigger immediate repair in background
                    let repair_mgr = self.repair_manager.clone();
                    let rd_client = self.rd_client.clone();
                    let torrent_id = self.rd_torrent_id.clone();

                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        match rd_client.get_torrent_info(&torrent_id).await {
                            Ok(torrent_info) => {
                                if let Err(e) = repair_mgr.repair_torrent(&torrent_info).await {
                                    tracing::debug!("Immediate repair skipped for {}: {}", torrent_id, e);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to fetch torrent info for repair of {}: {}", torrent_id, e);
                            }
                        }
                    });
                }

                Err(e.into())
            }
        }
    }
}

/// Simple virtual file (NFO files, etc)
#[derive(Debug)]
struct VirtualFile {
    name: String,
    content: Bytes,
    pos: u64,
}

impl DavFile for VirtualFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let node = VfsNode::VirtualFile {
            name: self.name.clone(),
            content: self.content.to_vec(),
        };
        async move {
            Ok(Box::new(DebridMetaData { node }) as Box<dyn DavMetaData>)
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
                std::io::SeekFrom::Current(p) => (self.pos as i64 + p) as u64,
                std::io::SeekFrom::End(p) => (self.content.len() as i64 + p) as u64,
            };
            self.pos = new_pos;
            Ok(new_pos)
        }.boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move { Ok(()) }.boxed()
    }
}
