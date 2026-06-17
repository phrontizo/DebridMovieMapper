use dav_server::DavHandler;
use debridmoviemapper::app_state::AppState;
use debridmoviemapper::config::Config;
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::enrolment::EnrolmentService;
use debridmoviemapper::provider::{DebridProvider, ProviderKind};
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::repair::RepairManager;
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

/// The log-filter directive: `RUST_LOG` when set to something non-empty, else `info` (the prior
/// default, so logging is unchanged when the var is unset). Parsing/validation of the directive is
/// left to `EnvFilter` at the call site (a malformed value falls back to `info`).
fn log_directive(rust_log: Option<String>) -> String {
    rust_log
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "info".to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env FIRST so the healthcheck resolves PORT identically to the server (which loads
    // .env then trim-parses PORT via Config). Without this, a PORT set only in .env — or one with
    // surrounding whitespace from quoted compose YAML — made the healthcheck probe 8080 while the
    // server listened elsewhere, reporting a healthy container as unhealthy.
    dotenvy::dotenv().ok();

    // Healthcheck mode: verify the WebDAV server is listening, then exit.
    if std::env::args().any(|a| a == "--healthcheck") {
        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(8080);
        let ok = format!("127.0.0.1:{}", port)
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|addr| {
                std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(5))
                    .is_ok()
            })
            .unwrap_or(false);
        std::process::exit(if ok { 0 } else { 1 });
    }

    // Honour RUST_LOG (e.g. `RUST_LOG=debridmoviemapper=debug`); default to INFO when unset so
    // behaviour is unchanged. A malformed directive falls back to INFO rather than crashing startup.
    let directive = log_directive(std::env::var("RUST_LOG").ok());
    let filter = tracing_subscriber::EnvFilter::try_new(&directive)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

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

    // Surface a build failure as a startup error (consistent with the TMDB/provider/scraper clients)
    // rather than panicking — reqwest's builder essentially never fails, but `?` keeps startup
    // failures uniform and avoids an `expect` panic.
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to build CDN HTTP client: {e}"))?;

    // The scraper gets its OWN client so an optional proxy applies to Torrentio/addon traffic only
    // — never the CDN media reads, TMDB, or provider APIs (which keep the direct `http_client`).
    let scraper_http = debridmoviemapper::scraper::build_http_client(
        config.acquisition.scraper_proxy_url.as_deref(),
    )?;
    if config.acquisition.scraper_proxy_url.is_some() {
        // Don't log the URL — it may contain proxy credentials.
        info!("Scraper requests routed through the configured HTTP proxy");
    }
    let scraper: Arc<dyn debridmoviemapper::scraper::Scraper> =
        Arc::new(debridmoviemapper::scraper::TorrentioScraper::new(
            config.acquisition.scraper_addon_url.clone(),
            config.provider_kind,
            &config.provider_token,
            scraper_http,
        ));
    let validator: Arc<dyn debridmoviemapper::acquire::TitleValidator> =
        Arc::new(debridmoviemapper::acquire::TmdbTitleValidator {
            tmdb: tmdb_client.clone(),
        });
    let prober: Arc<dyn debridmoviemapper::acquire::Prober> =
        Arc::new(debridmoviemapper::acquire::HttpProber {
            http: http_client.clone(),
        });
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
            // SIGTERM registration essentially never fails, but a panic here would bypass the
            // graceful-shutdown machinery entirely. On the off chance it fails, log and fall back to
            // SIGINT-only handling rather than tearing down the process.
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    tokio::select! {
                        _ = ctrl_c => info!("Received SIGINT, shutting down..."),
                        _ = sigterm.recv() => info!("Received SIGTERM, shutting down..."),
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to register SIGTERM handler ({e}); falling back to SIGINT only"
                    );
                    ctrl_c.await.ok();
                    info!("Received SIGINT, shutting down...");
                }
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
                let (stream, _addr) = match result {
                    Ok(pair) => pair,
                    Err(e) => {
                        // A transient accept() error — fd exhaustion (EMFILE/ENFILE), a peer
                        // that aborted before we accepted (ECONNABORTED), or EINTR — must NOT
                        // tear down the whole listener (which would also skip graceful
                        // shutdown below). Log, briefly back off so we don't hot-spin while
                        // the process is out of descriptors, and keep accepting.
                        tracing::warn!("accept() failed: {e} — continuing to accept");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
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
                        let dbg = format!("{:?}", err);
                        if dbg.contains("IncompleteMessage") {
                            return;
                        }
                        // A `User(Body)` error is our response-body stream failing (e.g. a CDN fetch
                        // returning `GeneralFailure`). The body-producing layer (`dav_fs`) already
                        // logs the concrete cause at WARN, so logging it again here at ERROR is just
                        // duplicate noise — and a broken file the player retries would spam it. Demote.
                        if dbg.contains("User(Body)") {
                            tracing::debug!("Connection body error (cause logged upstream): {}", dbg);
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

#[cfg(test)]
mod tests {
    use super::log_directive;

    #[test]
    fn log_directive_defaults_to_info_and_honours_rust_log() {
        // Unset / blank → the prior INFO default (behaviour unchanged when RUST_LOG isn't set).
        assert_eq!(log_directive(None), "info");
        assert_eq!(log_directive(Some(String::new())), "info");
        assert_eq!(log_directive(Some("   ".into())), "info");
        // Set → used verbatim (trimmed), so a scoped or plain directive both work.
        assert_eq!(log_directive(Some("debug".into())), "debug");
        assert_eq!(
            log_directive(Some("  debridmoviemapper=debug  ".into())),
            "debridmoviemapper=debug"
        );
    }

    #[test]
    fn log_directive_is_a_valid_env_filter() {
        // The default and a scoped directive must both parse as EnvFilter (no startup crash).
        for d in [
            "info",
            "debridmoviemapper=debug",
            "debridmoviemapper::acquire=debug,info",
        ] {
            assert!(tracing_subscriber::EnvFilter::try_new(d).is_ok(), "{d}");
        }
    }
}
