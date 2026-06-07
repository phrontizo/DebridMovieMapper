use crate::error::AppError;
use crate::provider::DebridProvider;
use crate::rd_client::TorrentInfo;
use std::time::Duration;

/// Shared add→select primitive used by both repair (same-hash) and acquisition (candidate).
/// Adds the magnet for `hash`, waits `settle`, fetches info, asks `select` which file ids to
/// select, selects them, and returns `(new_torrent_id, post_add_info)`. On any failure after
/// the add, the leaked torrent is deleted before returning `Err` (so nothing leaks). The caller
/// decides cached-vs-not by re-fetching final info (mirrors the existing repair flow).
pub async fn materialise(
    provider: &dyn DebridProvider,
    hash: &str,
    settle: Duration,
    select: impl Fn(&TorrentInfo) -> Vec<u32>,
) -> Result<(String, TorrentInfo), AppError> {
    let magnet = format!("magnet:?xt=urn:btih:{}", hash);
    let added = provider.add_magnet(&magnet).await.map_err(AppError::Http)?;
    let new_id = added.id;

    if !settle.is_zero() {
        tokio::time::sleep(settle).await;
    }

    let info = match provider.get_torrent_info(&new_id).await {
        Ok(i) => i,
        Err(e) => {
            let _ = provider.delete_torrent(&new_id).await;
            return Err(AppError::Http(e));
        }
    };

    let ids = select(&info);
    if ids.is_empty() {
        let _ = provider.delete_torrent(&new_id).await;
        return Err(AppError::Repair(format!(
            "no matching files to select in torrent {}",
            new_id
        )));
    }
    let ids_str = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    if let Err(e) = provider.select_files(&new_id, &ids_str).await {
        let _ = provider.delete_torrent(&new_id).await;
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
            add_magnet: Some(AddMagnetResponse { id: "new".into(), uri: String::new() }),
            torrent_info: Some(TorrentInfo {
                id: "new".into(),
                hash: "H".into(),
                status: status.into(),
                files: vec![TorrentFile { id: 7, path: "/Movie.mkv".into(), bytes: 10, selected: 0 }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn materialise_selects_via_closure_and_returns_info() {
        let provider: Arc<dyn DebridProvider> = Arc::new(mock("downloaded"));
        let (id, info) = materialise(&*provider, "H", Duration::from_millis(0), |info| {
            info.files.iter().filter(|f| f.path.ends_with(".mkv")).map(|f| f.id).collect()
        })
        .await
        .expect("materialise");
        assert_eq!(id, "new");
        assert_eq!(info.id, "new");
    }

    #[tokio::test]
    async fn materialise_errors_when_selector_matches_nothing() {
        let provider: Arc<dyn DebridProvider> = Arc::new(mock("downloaded"));
        let r = materialise(&*provider, "H", Duration::from_millis(0), |_| Vec::<u32>::new()).await;
        assert!(r.is_err(), "no selected files must be an error");
    }
}
