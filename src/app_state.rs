use crate::config::Config;
use crate::jellyfin_client::JellyfinClient;
use crate::provider::DebridProvider;
use crate::repair::RepairManager;
use crate::store::Store;
use crate::tmdb_client::TmdbClient;
use crate::vfs::DebridVfs;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared application state, constructed once at startup and cloned (cheaply —
/// every field is an `Arc`/handle) into the background scan task. Future phases
/// (scheduler, web UI) hang their handles off this struct.
#[derive(Clone)]
pub struct AppState {
    pub provider: Arc<dyn DebridProvider>,
    pub tmdb_client: Arc<TmdbClient>,
    pub vfs: Arc<RwLock<DebridVfs>>,
    pub store: Store,
    pub repair_manager: Arc<RepairManager>,
    pub config: Arc<Config>,
    pub jellyfin_client: Option<Arc<JellyfinClient>>,
    pub http_client: reqwest::Client,
}
