use dav_server::DavHandler;
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::config::Config;
use debridmoviemapper::enrolment::EnrolmentService;
use debridmoviemapper::provider::{DebridProvider, ProviderKind};
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::repair::RepairManager;
use debridmoviemapper::app_state::AppState;
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
        std::time::Duration::from_secs(config.acquisition.acquire_dead_timeout_secs),
    ));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Construct the Trakt client only when Trakt sync is configured. Read `config.trakt`
    // here, before `config` is moved into the `AppState` (`config: Arc::new(config)`).
    let trakt_client: Option<Arc<dyn debridmoviemapper::trakt_client::TraktClient>> =
        config.trakt.as_ref().map(|t| {
            Arc::new(debridmoviemapper::trakt_client::TraktClientImpl::new(
                t.client_id.clone(),
                t.client_secret.clone(),
                http_client.clone(),
            )) as Arc<dyn debridmoviemapper::trakt_client::TraktClient>
        });

    let read_activity = Arc::new(debridmoviemapper::read_activity::ReadActivity::new());

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
        trakt_client,
        read_activity: read_activity.clone(),
    };

    let scheduler_handle = tokio::spawn(debridmoviemapper::scheduler::run(
        app_state.clone(),
        shutdown_rx,
    ));

    let dav_fs = DebridFileSystem::new(
        app_state.provider.clone(),
        app_state.vfs.clone(),
        app_state.repair_manager.clone(),
        app_state.http_client.clone(),
        app_state.read_activity.clone(),
    );
    let dav_handler = DavHandler::builder()
        .filesystem(Box::new(dav_fs))
        .locksystem(dav_server::fakels::FakeLs::new())
        .build_handler();

    // Local-network Trakt enrolment routes (no auth — trusted LAN), present only when Trakt is
    // configured. Served on the same listener; `/trakt*` requests are routed here below.
    let enrolment: Option<Arc<EnrolmentService>> = app_state
        .trakt_client
        .clone()
        .map(|t| Arc::new(EnrolmentService::new(t, app_state.store.clone())));
    if enrolment.is_some() {
        info!("Trakt enrolment page available at /trakt/accounts");
    }

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
                let enrolment = enrolment.clone();

                tokio::task::spawn(async move {
                    let _permit = permit; // Hold permit until connection closes
                    if let Err(err) = http1::Builder::new()
                        .serve_connection(
                            io,
                            service_fn(move |req: Request<hyper::body::Incoming>| {
                                let dav_handler = dav_handler.clone();
                                let enrolment = enrolment.clone();
                                async move {
                                    // Route the local-network Trakt enrolment paths to the
                                    // enrolment service; everything else is WebDAV. Both arms
                                    // produce a `Response<dav_server::body::Body>`.
                                    let p = req.uri().path();
                                    if p == "/trakt" || p.starts_with("/trakt/") {
                                        match &enrolment {
                                            Some(enr) => Ok::<_, hyper::Error>(enr.handle(req).await),
                                            None => Ok::<_, hyper::Error>(
                                                hyper::Response::builder()
                                                    .status(hyper::StatusCode::NOT_FOUND)
                                                    .body(dav_server::body::Body::from(
                                                        "Trakt enrolment is not enabled".to_string(),
                                                    ))
                                                    .expect("static 404 response"),
                                            ),
                                        }
                                    } else {
                                        Ok::<_, hyper::Error>(dav_handler.handle(req).await)
                                    }
                                }
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

    // Signal the background tasks to stop and wait for them to finish
    let _ = shutdown_tx.send(true);
    info!("Waiting for background tasks to finish...");
    if let Err(e) = scheduler_handle.await {
        tracing::error!("Scheduler task ended abnormally: {:?}", e);
    }

    info!("Shutdown complete.");
    Ok(())
}
