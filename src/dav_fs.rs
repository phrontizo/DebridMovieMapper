use dav_server::fs::*;
use dav_server::davpath::DavPath;
use futures_util::FutureExt;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::vfs::{DebridVfs, VfsNode};
use crate::rd_client::RealDebridClient;
use crate::repair::RepairManager;
use bytes::Bytes;
use std::time::SystemTime;

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
                VfsNode::StrmFile { name, strm_content, rd_torrent_id, .. } => {
                    Ok(Box::new(StrmFile {
                        name,
                        content: Bytes::from(strm_content),
                        rd_torrent_id,
                        repair_manager: self.repair_manager.clone(),
                        pos: 0,
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
            VfsNode::StrmFile { strm_content, .. } => strm_content.len() as u64,
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

/// A STRM file that contains pre-generated content (the RD download URL)
#[derive(Debug)]
struct StrmFile {
    name: String,
    content: Bytes, // The actual STRM file content (URL + newline)
    rd_torrent_id: String,
    repair_manager: Arc<RepairManager>,
    pos: u64,
}

impl DavFile for StrmFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            // Check if torrent is under repair
            if self.repair_manager.should_hide_torrent(&self.rd_torrent_id).await {
                // Return minimal metadata to indicate file is unavailable
                return Ok(Box::new(DebridMetaData {
                    node: VfsNode::VirtualFile {
                        name: self.name.clone(),
                        content: vec![],
                    }
                }) as Box<dyn DavMetaData>);
            }

            // Return metadata with actual content size
            Ok(Box::new(DebridMetaData {
                node: VfsNode::StrmFile {
                    name: self.name.clone(),
                    strm_content: self.content.to_vec(),
                    rd_link: String::new(), // Not needed for metadata
                    rd_torrent_id: self.rd_torrent_id.clone(),
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
            // Check if torrent is under repair
            if self.repair_manager.should_hide_torrent(&self.rd_torrent_id).await {
                return Err(FsError::GeneralFailure);
            }

            // Read from stored content
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
                std::io::SeekFrom::End(p) => {
                    let size = self.content.len() as i64;
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
