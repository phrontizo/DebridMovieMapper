use dav_server::DavHandler;
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::repair::RepairManager;
use debridmoviemapper::tasks::{ScanConfig, MATCHES_TABLE};
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::DebridVfs;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use redb::Database;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tracing::{info, warn};

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

    let api_token = std::env::var("RD_API_TOKEN")
        .expect("RD_API_TOKEN must be set")
        .trim()
        .to_string();
    let tmdb_api_key = std::env::var("TMDB_API_KEY")
        .expect("TMDB_API_KEY must be set")
        .trim()
        .to_string();
    let scan_interval_secs = std::env::var("SCAN_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60)
        .max(10); // Enforce minimum 10s to prevent hammering the Real-Debrid API

    info!("Scan interval: {}s", scan_interval_secs);

    let rd_client = Arc::new(RealDebridClient::new(api_token)?);
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));

    let jellyfin_client =
        debridmoviemapper::jellyfin_client::JellyfinClient::from_env().map(Arc::new);

    if jellyfin_client.is_some() {
        info!("Jellyfin notification enabled");
    } else {
        info!("Jellyfin notification disabled (set JELLYFIN_URL, JELLYFIN_API_KEY, JELLYFIN_RCLONE_MOUNT_PATH to enable)");
    }

    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "metadata.db".to_string());
    let db = Arc::new(Database::create(&db_path).expect("Failed to open database"));

    // Ensure table exists on fresh databases
    {
        let write_txn = db.begin_write().expect("Failed to begin write transaction");
        write_txn
            .open_table(MATCHES_TABLE)
            .expect("Failed to create matches table");
        write_txn.commit().expect("Failed to commit table creation");
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let scan_handle = tokio::spawn(debridmoviemapper::tasks::run_scan_loop(
        ScanConfig {
            rd_client: rd_client.clone(),
            tmdb_client: tmdb_client.clone(),
            vfs: vfs.clone(),
            db: db.clone(),
            repair_manager: repair_manager.clone(),
            interval_secs: scan_interval_secs,
            jellyfin_client,
        },
        shutdown_rx,
    ));

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build CDN HTTP client");
    let dav_fs = DebridFileSystem::new(
        rd_client.clone(),
        vfs.clone(),
        repair_manager.clone(),
        http_client,
    );
    let dav_handler = DavHandler::builder()
        .filesystem(Box::new(dav_fs))
        .locksystem(dav_server::fakels::FakeLs::new())
        .build_handler();

    let port: u16 = match std::env::var("PORT") {
        Ok(s) => s.parse().unwrap_or_else(|_| {
            warn!("Invalid PORT value '{}', falling back to 8080", s);
            8080
        }),
        Err(_) => 8080,
    };
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
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
