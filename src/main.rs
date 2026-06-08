use dav_server::DavHandler;
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::config::Config;
use debridmoviemapper::provider::{DebridProvider, ProviderKind};
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::repair::RepairManager;
use debridmoviemapper::app_state::AppState;
use debridmoviemapper::tasks::ScanConfig;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::torbox_client::TorBoxClient;
use debridmoviemapper::vfs::DebridVfs;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tracing::info;

const MAX_CONNECTIONS: usize = 256;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Healthcheck mode: verify the WebDAV server is listening, then exit
    if std::env::args().any(|a| a == "--healthcheck") {
        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080);
        std::process::exit(
            if std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
                0
            } else {
                1
            },
        );
    }

    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let config = Config::from_env().unwrap_or_else(|e| {
        eprintln!("Configuration error: {}", e);
        std::process::exit(1);
    });

    // Construct the selected provider from the chosen token. Either client surfaces
    // a clear configuration error here (via `?`) rather than tripping a later panic.
    let provider: Arc<dyn DebridProvider> = match config.provider_kind {
        ProviderKind::RealDebrid => Arc::new(RealDebridClient::new(config.provider_token.clone())?),
        ProviderKind::TorBox => Arc::new(TorBoxClient::new(config.provider_token.clone())?),
    };

    info!("Scan interval: {}s", config.scan_interval_secs);

    let tmdb_client = Arc::new(TmdbClient::new(config.tmdb_api_key.clone())?);
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));
    let repair_manager = Arc::new(RepairManager::new(provider.clone()));

    let jellyfin_client =
        debridmoviemapper::jellyfin_client::JellyfinClient::from_env().map(Arc::new);

    if jellyfin_client.is_some() {
        info!("Jellyfin notification enabled");
    } else {
        info!("Jellyfin notification disabled (set JELLYFIN_URL, JELLYFIN_API_KEY, JELLYFIN_RCLONE_MOUNT_PATH to enable)");
    }

    // Open the metadata cache. Store::open never fails on an incompatible/corrupt
    // database: it moves the old file aside (<db_path>.corrupt) and recreates it.
    let store = debridmoviemapper::store::Store::open(&config.db_path)?;

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build CDN HTTP client");

    let scraper: Arc<dyn debridmoviemapper::scraper::Scraper> =
        Arc::new(debridmoviemapper::scraper::TorrentioScraper::new(
            config.acquisition.scraper_addon_url.clone(),
            config.provider_kind,
            &config.provider_token,
            http_client.clone(),
        ));
    let validator: Arc<dyn debridmoviemapper::acquire::TitleValidator> =
        Arc::new(debridmoviemapper::acquire::TmdbTitleValidator { tmdb: tmdb_client.clone() });
    let prober: Arc<dyn debridmoviemapper::acquire::Prober> =
        Arc::new(debridmoviemapper::acquire::HttpProber { http: http_client.clone() });
    let engine = Arc::new(debridmoviemapper::acquire::AcquisitionEngine::new(
        provider.clone(),
        scraper.clone(),
        validator,
        prober,
        store.clone(),
        config.acquisition.prefs.clone(),
        config.acquisition.max_acquire_attempts,
        std::time::Duration::from_secs(config.acquisition.stall_timeout_secs),
    ));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let app_state = AppState {
        provider: provider.clone(),
        tmdb_client: tmdb_client.clone(),
        vfs: vfs.clone(),
        store: store.clone(),
        repair_manager: repair_manager.clone(),
        config: Arc::new(config),
        jellyfin_client,
        http_client: http_client.clone(),
        scraper: scraper.clone(),
        engine: engine.clone(),
    };

    // TEMPORARY (SP1) dev/verification trigger — remove once SP2 (Trakt) / SP4 (ad-hoc add) exist.
    // Usage: --acquire <movie|series> <imdb-or-tmdb-id> [season episode]
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--acquire") {
        let kind_s = args.get(pos + 1).cloned().unwrap_or_default();
        let id_s = args.get(pos + 2).cloned().unwrap_or_default();
        let season = args.get(pos + 3).and_then(|s| s.parse::<u32>().ok());
        let episode = args.get(pos + 4).and_then(|s| s.parse::<u32>().ok());
        let kind = match kind_s.as_str() {
            "movie" => debridmoviemapper::scraper::MediaKind::Movie,
            "series" => debridmoviemapper::scraper::MediaKind::Series,
            other => {
                eprintln!("--acquire: kind must be 'movie' or 'series', got '{}'", other);
                std::process::exit(2);
            }
        };
        let media_type = match kind {
            debridmoviemapper::scraper::MediaKind::Movie => debridmoviemapper::vfs::MediaType::Movie,
            debridmoviemapper::scraper::MediaKind::Series => debridmoviemapper::vfs::MediaType::Show,
        };
        let (imdb_id, tmdb_id) = if id_s.starts_with("tt") {
            match tmdb_client.find_by_imdb(&id_s).await {
                Ok(Some((tid, _))) => (id_s.clone(), tid),
                _ => { eprintln!("--acquire: could not resolve IMDB id {}", id_s); std::process::exit(2); }
            }
        } else {
            let tid: u64 = id_s.parse().unwrap_or_else(|_| { eprintln!("--acquire: invalid id {}", id_s); std::process::exit(2); });
            match tmdb_client.external_imdb_id(tid, media_type.clone()).await {
                Ok(Some(imdb)) => (imdb, tid),
                _ => { eprintln!("--acquire: could not resolve IMDB id for tmdb {}", tid); std::process::exit(2); }
            }
        };
        let (title, year, original_language) = match tmdb_client.details(tmdb_id, media_type.clone()).await {
            Ok(d) => d,
            Err(e) => {
                eprintln!("--acquire: could not fetch TMDB details for {}: {}", tmdb_id, e);
                std::process::exit(2);
            }
        };
        let req = debridmoviemapper::store::AcquireRequest {
            imdb_id,
            tmdb_id,
            kind,
            season,
            episode,
            original_language,
            metadata: debridmoviemapper::vfs::MediaMetadata {
                title,
                year,
                media_type,
                external_id: Some(format!("tmdb:{}", tmdb_id)),
            },
        };
        let outcome = engine.acquire(req, debridmoviemapper::store::Provenance::manual()).await;
        println!("--acquire outcome: {:?}", outcome);
        return Ok(());
    }

    let scan_handle = tokio::spawn(debridmoviemapper::tasks::run_scan_loop(
        ScanConfig {
            app: app_state.clone(),
        },
        shutdown_rx,
    ));

    let dav_fs = DebridFileSystem::new(
        app_state.provider.clone(),
        app_state.vfs.clone(),
        app_state.repair_manager.clone(),
        app_state.http_client.clone(),
    );
    let dav_handler = DavHandler::builder()
        .filesystem(Box::new(dav_fs))
        .locksystem(dav_server::fakels::FakeLs::new())
        .build_handler();

    let addr = SocketAddr::from(([0, 0, 0, 0], app_state.config.port));
    let listener = TcpListener::bind(addr).await?;
    info!("WebDAV server listening on http://{}", addr);

    let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));

    // Unified shutdown future: triggers on SIGINT (ctrl+c) or SIGTERM (Docker stop)
    let shutdown_signal = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("Failed to register SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => info!("Received SIGINT, shutting down..."),
                _ = sigterm.recv() => info!("Received SIGTERM, shutting down..."),
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.ok();
            info!("Received SIGINT, shutting down...");
        }
    };
    tokio::pin!(shutdown_signal);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _addr) = result?;
                let permit = match semaphore.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!("Max connections ({}) reached, rejecting", MAX_CONNECTIONS);
                        drop(stream);
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let dav_handler = dav_handler.clone();

                tokio::task::spawn(async move {
                    let _permit = permit; // Hold permit until connection closes
                    if let Err(err) = http1::Builder::new()
                        .serve_connection(
                            io,
                            service_fn(move |req: Request<hyper::body::Incoming>| {
                                let dav_handler = dav_handler.clone();
                                async move { Ok::<_, hyper::Error>(dav_handler.handle(req).await) }
                            }),
                        )
                        .await
                    {
                        use std::error::Error;
                        if let Some(io_err) =
                            err.source().and_then(|s| s.downcast_ref::<std::io::Error>())
                        {
                            if matches!(
                                io_err.kind(),
                                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                            ) {
                                return;
                            }
                        }
                        // hyper 1.x does not expose is_incomplete_message() — use string check
                        // This handles clients that disconnect mid-request (common with WebDAV)
                        if format!("{:?}", err).contains("IncompleteMessage") {
                            return;
                        }
                        tracing::error!("Error serving connection: {:?}", err);
                    }
                });
            }
            _ = &mut shutdown_signal => {
                break;
            }
        }
    }

    // Signal the scan loop to stop and wait for it to finish
    let _ = shutdown_tx.send(true);
    info!("Waiting for scan task to finish...");
    let _ = scan_handle.await;

    info!("Shutdown complete.");
    Ok(())
}
