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
}
