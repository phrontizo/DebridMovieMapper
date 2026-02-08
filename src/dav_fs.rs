use dav_server::fs::*;
use dav_server::davpath::DavPath;
use futures_util::FutureExt;
use std::sync::Arc;
use tokio::sync::RwLock;
use crate::vfs::{DebridVfs, VfsNode};
use crate::rd_client::RealDebridClient;
use std::io::SeekFrom;
use bytes::Bytes;
use reqwest::StatusCode;
use std::time::{SystemTime, Duration};
use rand::Rng;

#[derive(Clone)]
pub struct DebridFileSystem {
    pub vfs: Arc<RwLock<DebridVfs>>,
    pub rd_client: Arc<RealDebridClient>,
    pub link_cache: Arc<RwLock<std::collections::HashMap<String, String>>>,
    pub client: reqwest::Client,
    pub download_semaphore: Arc<tokio::sync::Semaphore>,
}

impl DebridFileSystem {
    pub fn new(rd_client: Arc<RealDebridClient>, vfs: Arc<RwLock<DebridVfs>>) -> Self {
        Self {
            vfs,
            rd_client,
            link_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
            client: reqwest::Client::builder()
                .user_agent("DebridMovieMapper/0.1.0")
                .timeout(Duration::from_secs(60))
                .pool_idle_timeout(Duration::from_secs(10))
                .pool_max_idle_per_host(8)
                .tcp_nodelay(true)
                .http1_only()
                .build()
                .unwrap(),
            download_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
        }
    }

    async fn find_node(&self, path: &DavPath) -> Option<VfsNode> {
        let vfs = self.vfs.read().await;
        let mut current = &vfs.root;
        
        let path_osstr = path.as_rel_ospath();
        let path_str = path_osstr.to_str().unwrap();
        if path_str == "." || path_str == "" {
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
            if let VfsNode::File { name, size, rd_link, .. } = node {
                Ok(Box::new(DebridFile {
                    name,
                    size,
                    rd_link,
                    rd_client: self.rd_client.clone(),
                    link_cache: self.link_cache.clone(),
                    client: self.client.clone(),
                    download_semaphore: self.download_semaphore.clone(),
                    pos: 0,
                    buffer: Bytes::new(),
                    buffer_offset: 0,
                }) as Box<dyn DavFile>)
            } else {
                Err(FsError::Forbidden)
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
            VfsNode::File { size, .. } => *size,
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

#[derive(Debug)]
struct DebridFile {
    name: String,
    size: u64,
    rd_link: String,
    rd_client: Arc<RealDebridClient>,
    link_cache: Arc<RwLock<std::collections::HashMap<String, String>>>,
    client: reqwest::Client,
    download_semaphore: Arc<tokio::sync::Semaphore>,
    pos: u64,
    buffer: Bytes,
    buffer_offset: u64,
}

impl DavFile for DebridFile {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let node = VfsNode::File {
            name: self.name.clone(),
            size: self.size,
            rd_torrent_id: "".to_string(),
            rd_link: self.rd_link.clone(),
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
        let offset = self.pos;
        let rd_link = self.rd_link.clone();
        let rd_client = self.rd_client.clone();
        let link_cache = self.link_cache.clone();
        let client = self.client.clone();
        let semaphore = self.download_semaphore.clone();

        async move {
            // 1. Check if we can serve from buffer
            if offset >= self.buffer_offset && offset < self.buffer_offset + self.buffer.len() as u64 {
                let start_in_buf = (offset - self.buffer_offset) as usize;
                let available = self.buffer.len() - start_in_buf;
                if available > 0 {
                    let to_read = std::cmp::min(len, available);
                    let data = self.buffer.slice(start_in_buf..start_in_buf + to_read);
                    self.pos += to_read as u64;
                    tracing::trace!("Buffer hit for {}: {} bytes at offset {}", self.name, to_read, offset);
                    return Ok(data);
                }
            }

            tracing::debug!("Buffer miss for {}: requesting {} bytes at offset {}", self.name, len, offset);
            // 2. Buffer miss, fetch from network
            let mut use_cache = true;
            let mut attempts = 0;
            let max_attempts = 15;
            let fetch_size = std::cmp::max(len, 4 * 1024 * 1024); // 4MB read-ahead

            loop {
                attempts += 1;
                let _permit = semaphore.acquire().await.map_err(|_| FsError::GeneralFailure)?;
                let download_link = if use_cache {
                    let cache = link_cache.read().await;
                    cache.get(&rd_link).cloned()
                } else {
                    None
                };

                let download_link = if let Some(link) = download_link {
                    link
                } else {
                    let resp = rd_client.unrestrict_link(&rd_link).await.map_err(|e| {
                        tracing::error!("Failed to unrestrict link {}: {}", rd_link, e);
                        FsError::GeneralFailure
                    })?;
                    let mut cache = link_cache.write().await;
                    cache.insert(rd_link.clone(), resp.download.clone());
                    resp.download
                };

                let mut end = offset + fetch_size as u64 - 1;
                if end >= self.size {
                    end = self.size.saturating_sub(1);
                }
                if end < offset {
                    return Ok(Bytes::new());
                }

                let range = format!("bytes={}-{}", offset, end);
                tracing::debug!("Requesting range {} for {}", range, self.name);
                let start_time = std::time::Instant::now();
                let resp = client.get(&download_link)
                    .header("Range", &range)
                    .send()
                    .await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let bytes = r.bytes().await.map_err(|e| {
                            tracing::error!("Failed to read body from {} (attempt {}): {}", download_link, attempts, e);
                            e
                        });
                        
                        let bytes = match bytes {
                            Ok(b) => b,
                            Err(e) if attempts < max_attempts => {
                                tracing::warn!("Body read failed, retrying (attempt {}/{}): {:?}", attempts, max_attempts, e);
                                use_cache = false;
                                {
                                    let mut cache = link_cache.write().await;
                                    cache.remove(&rd_link);
                                }
                                let delay = Duration::from_millis(500 * attempts as u64 + rand::thread_rng().gen_range(0..500));
                                tokio::time::sleep(delay).await;
                                continue;
                            }
                            Err(_) => return Err(FsError::GeneralFailure),
                        };
                        
                        let duration = start_time.elapsed();
                        if duration.as_millis() > 5000 {
                            tracing::warn!("Slow read: {} bytes in {:?} from {}", bytes.len(), duration, download_link);
                        }
                        
                        // Update buffer
                        self.buffer = bytes.clone();
                        self.buffer_offset = offset;
                        
                        let to_read = std::cmp::min(len, bytes.len());
                        let result = bytes.slice(0..to_read);
                        
                        if result.len() < len && offset + (result.len() as u64) < self.size {
                            tracing::warn!("Short read: got {} bytes, wanted {} for {} at offset {}", result.len(), len, self.name, offset);
                        }

                        self.pos += result.len() as u64;
                        return Ok(result);
                    }
                    Ok(r) if attempts < max_attempts && (r.status() == StatusCode::FORBIDDEN || r.status() == StatusCode::NOT_FOUND || r.status() == StatusCode::UNAUTHORIZED) => {
                        tracing::warn!("Link expired or invalid (status {}), retrying (attempt {}/{})...", r.status(), attempts, max_attempts);
                        use_cache = false;
                        let mut cache = link_cache.write().await;
                        cache.remove(&rd_link);
                        let delay = Duration::from_millis(100 + rand::thread_rng().gen_range(0..200));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    Ok(r) if attempts < max_attempts => {
                        tracing::warn!("Download server returned error {}, retrying (attempt {}/{})...", r.status(), attempts, max_attempts);
                        use_cache = false;
                        let delay = Duration::from_millis(200 * attempts as u64 + rand::thread_rng().gen_range(0..200));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    Ok(r) => {
                        tracing::error!("Download server returned error {} for {} (Range: {}) after {} attempts", r.status(), download_link, range, attempts);
                        return Err(FsError::GeneralFailure);
                    }
                    Err(e) if attempts < max_attempts => {
                        tracing::warn!("Failed to request range, retrying (attempt {}/{}): {:?}", attempts, max_attempts, e);
                        use_cache = false;
                        {
                            let mut cache = link_cache.write().await;
                            cache.remove(&rd_link);
                        }
                        // Progressive backoff: 1s, 2s, 3s... + jitter
                        let delay = Duration::from_millis(1000 * attempts as u64 + rand::thread_rng().gen_range(0..1000));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    Err(e) => {
                        tracing::error!("Failed to request range {} from {} after {} attempts: {:?}", range, download_link, attempts, e);
                        return Err(FsError::GeneralFailure);
                    }
                }
            }
        }.boxed()
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(p) => (self.pos as i64 + p) as u64,
            SeekFrom::End(p) => (self.size as i64 + p) as u64,
        };
        self.pos = new_pos;
        async move { Ok(new_pos) }.boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        async move { Ok(()) }.boxed()
    }
}
