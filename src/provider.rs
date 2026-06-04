use crate::error::AppError;
use crate::rd_client::{AddMagnetResponse, Torrent, TorrentInfo};

/// Identifies a single media file for resolution. Stable identity is
/// `(hash, file_path)`; `torrent_id`/`file_id`/`link` are re-derivable (e.g. after
/// a re-acquire). `link` is the provider's per-file restricted link when it has
/// one (Real-Debrid); `None` for providers that resolve by `(torrent_id, file_id)`
/// (TorBox).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileLocator {
    pub hash: String,
    pub torrent_id: String,
    pub file_id: u32,
    pub file_path: String,
    pub link: Option<String>,
}

/// Abstraction over a debrid provider (Real-Debrid today, TorBox in a later phase).
///
/// The method set mirrors the Real-Debrid operations the codebase calls today so
/// this phase is a pure refactor. It is widened/reshaped in later phases.
///
/// `Debug` is a supertrait so a future `Arc<dyn DebridProvider>` can live inside
/// `Debug`-deriving structs like `RepairManager` (migrated in a later phase).
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
    /// file's bytes are not currently available (RD: 503 on a broken torrent) so callers
    /// can trigger re-acquire/repair.
    async fn resolve_url(&self, loc: &FileLocator) -> Result<String, crate::error::AppError>;

    /// Drop any cached resolution for `loc` (RD: the unrestrict-cache entry for its link).
    async fn invalidate(&self, loc: &FileLocator);

    /// Evict expired cached resolutions.
    async fn evict_expired_cache(&self);
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
}

#[cfg(test)]
#[async_trait::async_trait]
impl DebridProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        Ok(self.torrents.clone())
    }
    async fn get_torrent_info(&self, _id: &str) -> Result<TorrentInfo, reqwest::Error> {
        Ok(self.torrent_info.clone().unwrap_or_default())
    }
    async fn add_magnet(&self, _magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        Ok(self.add_magnet.clone().unwrap_or_default())
    }
    async fn select_files(&self, _torrent_id: &str, _file_ids: &str) -> Result<(), reqwest::Error> {
        Ok(())
    }
    async fn delete_torrent(&self, _torrent_id: &str) -> Result<(), reqwest::Error> {
        Ok(())
    }
    async fn resolve_url(&self, _loc: &FileLocator) -> Result<String, crate::error::AppError> {
        match &self.resolved_url {
            Some(u) => Ok(u.clone()),
            None => Err(crate::error::AppError::Unavailable),
        }
    }
    async fn invalidate(&self, _loc: &FileLocator) {}
    async fn evict_expired_cache(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rd_client::RealDebridClient;
    use std::sync::Arc;

    #[test]
    fn real_debrid_client_is_a_debrid_provider() {
        let client = RealDebridClient::new("fake-token".to_string()).unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(client);
        assert_eq!(provider.name(), "real-debrid");
    }

    #[tokio::test]
    async fn mock_provider_returns_canned_values() {
        let mock = MockProvider {
            torrents: vec![Torrent {
                id: "t1".to_string(),
                ..Default::default()
            }],
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
        provider.evict_expired_cache().await;
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
    fn choose_provider_rd_only() {
        let (kind, token) =
            choose_provider(Some("rd-token".to_string()), None).unwrap();
        assert_eq!(kind, ProviderKind::RealDebrid);
        assert_eq!(token, "rd-token");
    }

    #[test]
    fn choose_provider_torbox_only() {
        let (kind, token) =
            choose_provider(None, Some("tb-token".to_string())).unwrap();
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
        let (kind, _) =
            choose_provider(Some("   ".to_string()), Some("tb".to_string())).unwrap();
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
        assert_eq!(provider.resolve_url(&loc).await.unwrap(), "https://cdn/file");
        provider.invalidate(&loc).await; // no-op, must not panic
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
