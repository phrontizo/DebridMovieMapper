use crate::error::AppError;
use crate::provider::DebridProvider;
use crate::rd_client::TorrentInfo;
use std::time::{Duration, Instant};

/// Add→poll→select primitive used by the instant-repair path (same-hash re-add). (The SP3
/// acquisition engine no longer calls this — it adds optimistically in `acquire` and resolves the
/// verdict later in `observe`; only `repair.rs` materialises.)
/// Adds the magnet for `hash`, then **polls** for the file list — a freshly-added uncached torrent
/// can take several seconds to resolve its metadata, so every `settle` it re-fetches info and asks
/// `select` which file ids to select, retrying until `select` returns something or `max_wait`
/// elapses. `settle == 0` means a single immediate attempt (no polling). On success it selects the
/// files and returns `(new_torrent_id, post_add_info)`. On any failure after the add — including the
/// file list never resolving within `max_wait` — the leaked torrent is deleted before returning
/// `Err` (so nothing leaks). The caller decides cached-vs-not by re-fetching final info.
///
/// `protect_id`: a torrent id that must NEVER be deleted by the failure cleanup. This guards the
/// same-hash re-add on providers (TorBox) whose `add_magnet` recovers the EXISTING torrent by
/// infohash and returns its **same** id — there the "added" torrent IS the real torrent under
/// repair, so deleting it on a transient failure (e.g. a re-activating torrent that briefly reports
/// `files: null`) would be permanent data loss. Real-Debrid mints a fresh id, so its leaked-torrent
/// cleanup is unaffected (`new_id != protect_id`). Pass `None` to always clean up.
pub async fn materialise(
    provider: &dyn DebridProvider,
    hash: &str,
    settle: Duration,
    max_wait: Duration,
    protect_id: Option<&str>,
    select: impl Fn(&TorrentInfo) -> Vec<u32>,
) -> Result<(String, TorrentInfo), AppError> {
    let magnet = format!("magnet:?xt=urn:btih:{}", hash);
    let added = provider.add_magnet(&magnet).await.map_err(AppError::Http)?;
    let new_id = added.id;
    // Skip the leaked-torrent delete when the add recovered the protected (real) torrent.
    let cleanup_ok = protect_id != Some(new_id.as_str());

    let deadline = Instant::now() + max_wait;
    let (info, ids) = loop {
        if !settle.is_zero() {
            tokio::time::sleep(settle).await;
        }
        let info = match provider.get_torrent_info(&new_id).await {
            Ok(i) => i,
            Err(e) => {
                if cleanup_ok {
                    let _ = provider.delete_torrent(&new_id).await;
                }
                return Err(AppError::Http(e));
            }
        };
        let ids = select(&info);
        // Stop on a usable file list; on a single-shot request (settle == 0); or at the deadline.
        if !ids.is_empty() || settle.is_zero() || Instant::now() >= deadline {
            break (info, ids);
        }
    };

    if ids.is_empty() {
        if cleanup_ok {
            let _ = provider.delete_torrent(&new_id).await;
        }
        return Err(AppError::Repair(format!(
            "no matching files to select in torrent {}",
            new_id
        )));
    }
    let ids_str = ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if let Err(e) = provider.select_files(&new_id, &ids_str).await {
        if cleanup_ok {
            let _ = provider.delete_torrent(&new_id).await;
        }
        return Err(AppError::Http(e));
    }
    Ok((new_id, info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::rd_client::{AddMagnetResponse, TorrentFile};
    use std::sync::Arc;

    fn mock(status: &str) -> MockProvider {
        MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "new".into(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new".into(),
                hash: "H".into(),
                status: status.into(),
                files: vec![TorrentFile {
                    id: 7,
                    path: "/Movie.mkv".into(),
                    bytes: 10,
                    selected: 0,
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn materialise_selects_via_closure_and_returns_info() {
        let provider: Arc<dyn DebridProvider> = Arc::new(mock("downloaded"));
        let (id, info) = materialise(
            &*provider,
            "H",
            Duration::from_millis(0),
            Duration::from_millis(0),
            None,
            |info| {
                info.files
                    .iter()
                    .filter(|f| f.path.ends_with(".mkv"))
                    .map(|f| f.id)
                    .collect()
            },
        )
        .await
        .expect("materialise");
        assert_eq!(id, "new");
        assert_eq!(info.id, "new");
    }

    #[tokio::test]
    async fn materialise_errors_when_selector_matches_nothing() {
        let provider: Arc<dyn DebridProvider> = Arc::new(mock("downloaded"));
        let r = materialise(
            &*provider,
            "H",
            Duration::from_millis(0),
            Duration::from_millis(0),
            None,
            |_| Vec::<u32>::new(),
        )
        .await;
        assert!(r.is_err(), "no selected files must be an error");
    }

    /// Provider whose file list is empty until `resolve_after` calls to `get_torrent_info`, then
    /// returns a video file — simulates an uncached torrent whose metadata resolves after a delay.
    #[derive(Debug)]
    struct PollProvider {
        calls: std::sync::atomic::AtomicUsize,
        resolve_after: usize,
    }
    #[async_trait::async_trait]
    impl DebridProvider for PollProvider {
        fn name(&self) -> &'static str {
            "poll"
        }
        async fn get_torrents(&self) -> Result<Vec<crate::rd_client::Torrent>, reqwest::Error> {
            Ok(vec![])
        }
        async fn get_torrent_info(&self, _id: &str) -> Result<TorrentInfo, reqwest::Error> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let files = if n >= self.resolve_after {
                vec![TorrentFile {
                    id: 7,
                    path: "/Movie.mkv".into(),
                    bytes: 10,
                    selected: 0,
                }]
            } else {
                vec![]
            };
            Ok(TorrentInfo {
                id: "new".into(),
                hash: "H".into(),
                status: "downloading".into(),
                files,
                ..Default::default()
            })
        }
        async fn add_magnet(&self, _m: &str) -> Result<AddMagnetResponse, reqwest::Error> {
            Ok(AddMagnetResponse {
                id: "new".into(),
                uri: String::new(),
            })
        }
        async fn select_files(&self, _t: &str, _f: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn delete_torrent(&self, _t: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn resolve_url(
            &self,
            _l: &crate::provider::FileLocator,
        ) -> Result<String, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn invalidate(&self, _l: &crate::provider::FileLocator) {}
        async fn evict_expired_cache(&self) {}
    }

    fn mkv_selector(info: &TorrentInfo) -> Vec<u32> {
        info.files
            .iter()
            .filter(|f| f.path.ends_with(".mkv"))
            .map(|f| f.id)
            .collect()
    }

    #[tokio::test]
    async fn materialise_polls_until_file_list_resolves() {
        // Empty on the first check, file present on the second → materialise should keep polling.
        let provider: Arc<dyn DebridProvider> = Arc::new(PollProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            resolve_after: 1,
        });
        let (id, info) = materialise(
            &*provider,
            "H",
            Duration::from_millis(5),
            Duration::from_secs(2),
            None,
            mkv_selector,
        )
        .await
        .expect("should resolve on a later poll");
        assert_eq!(id, "new");
        assert_eq!(info.files.len(), 1);
    }

    #[tokio::test]
    async fn materialise_times_out_when_file_list_never_resolves() {
        let provider: Arc<dyn DebridProvider> = Arc::new(PollProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            resolve_after: 9999,
        });
        let r = materialise(
            &*provider,
            "H",
            Duration::from_millis(5),
            Duration::from_millis(40),
            None,
            mkv_selector,
        )
        .await;
        assert!(r.is_err(), "must error after max_wait with no file list");
    }

    /// Provider that records delete_torrent calls — to prove `protect_id` suppresses the cleanup
    /// delete when the (same-id) re-add returned the protected torrent.
    #[derive(Debug, Default)]
    struct DeleteRecordingProvider {
        deleted: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl DebridProvider for DeleteRecordingProvider {
        fn name(&self) -> &'static str {
            "rec"
        }
        async fn get_torrents(&self) -> Result<Vec<crate::rd_client::Torrent>, reqwest::Error> {
            Ok(vec![])
        }
        async fn get_torrent_info(&self, _id: &str) -> Result<TorrentInfo, reqwest::Error> {
            // No files → selector matches nothing → materialise hits the cleanup-delete path.
            Ok(TorrentInfo {
                id: "tid".into(),
                hash: "H".into(),
                status: "downloading".into(),
                ..Default::default()
            })
        }
        async fn add_magnet(&self, _m: &str) -> Result<AddMagnetResponse, reqwest::Error> {
            Ok(AddMagnetResponse {
                id: "tid".into(), // same id as protect_id — TorBox behaviour
                uri: String::new(),
            })
        }
        async fn select_files(&self, _t: &str, _f: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn delete_torrent(&self, t: &str) -> Result<(), reqwest::Error> {
            self.deleted.lock().unwrap().push(t.to_string());
            Ok(())
        }
        async fn resolve_url(
            &self,
            _l: &crate::provider::FileLocator,
        ) -> Result<String, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn invalidate(&self, _l: &crate::provider::FileLocator) {}
        async fn evict_expired_cache(&self) {}
    }

    #[tokio::test]
    async fn materialise_protect_id_suppresses_cleanup_delete_on_failure() {
        // Same-id re-add (new == protect_id) with no matching files: materialise must NOT delete the
        // protected (real) torrent — that would be data loss on TorBox.
        let provider = Arc::new(DeleteRecordingProvider::default());
        let p: Arc<dyn DebridProvider> = provider.clone();
        let r = materialise(
            &*p,
            "H",
            Duration::from_millis(0),
            Duration::from_millis(0),
            Some("tid"),
            |_| Vec::<u32>::new(),
        )
        .await;
        assert!(r.is_err(), "no matching files → Err");
        assert!(
            provider.deleted.lock().unwrap().is_empty(),
            "protect_id must suppress the cleanup delete of the real torrent, got: {:?}",
            provider.deleted.lock().unwrap()
        );
    }
}
