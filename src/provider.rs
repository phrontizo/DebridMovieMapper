use crate::rd_client::{AddMagnetResponse, Torrent, TorrentInfo, UnrestrictResponse};

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
    async fn unrestrict_link(&self, link: &str) -> Result<UnrestrictResponse, reqwest::Error>;
    async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error>;
    async fn select_files(&self, torrent_id: &str, file_ids: &str) -> Result<(), reqwest::Error>;
    async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error>;

    /// Remove a single cached resolution (RD: the unrestrict cache entry for `link`).
    async fn invalidate_unrestrict_cache(&self, link: &str);
    /// Evict expired cached resolutions.
    async fn evict_expired_cache(&self);
}

/// Test-only in-memory provider. Returns configured canned values; unconfigured
/// methods return `Default`s or are no-ops. Not compiled into release builds.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MockProvider {
    pub torrents: Vec<Torrent>,
    pub torrent_info: Option<TorrentInfo>,
    pub unrestrict: Option<UnrestrictResponse>,
    pub add_magnet: Option<AddMagnetResponse>,
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
    async fn unrestrict_link(&self, _link: &str) -> Result<UnrestrictResponse, reqwest::Error> {
        Ok(self.unrestrict.clone().unwrap_or_default())
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
    async fn invalidate_unrestrict_cache(&self, _link: &str) {}
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
        provider.invalidate_unrestrict_cache("x").await;
        provider.evict_expired_cache().await;
    }
}
