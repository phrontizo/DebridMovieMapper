use std::sync::Arc;
use std::net::SocketAddr;
use tokio::sync::{RwLock, Semaphore};
use tokio::net::TcpListener;
use tracing::info;
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::vfs::DebridVfs;
use debridmoviemapper::dav_fs::DebridFileSystem;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::repair::RepairManager;
use dav_server::DavHandler;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use redb::{Database, TableDefinition};

const MAX_CONNECTIONS: usize = 256;
const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        .unwrap_or(60);

    info!("Scan interval: {}s", scan_interval_secs);

    let rd_client = Arc::new(RealDebridClient::new(api_token)?);
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));

    let jellyfin_client = debridmoviemapper::jellyfin_client::JellyfinClient::from_env()
        .map(Arc::new);

    if jellyfin_client.is_some() {
        info!("Jellyfin notification enabled");
    } else {
        info!("Jellyfin notification disabled (set JELLYFIN_URL, JELLYFIN_API_KEY, JELLYFIN_RCLONE_MOUNT_PATH to enable)");
    }

    let db = Arc::new(Database::create("metadata.db").expect("Failed to open database"));

    // Ensure table exists on fresh databases
    {
        let write_txn = db.begin_write().expect("Failed to begin write transaction");
        write_txn.open_table(MATCHES_TABLE).expect("Failed to create matches table");
        write_txn.commit().expect("Failed to commit table creation");
    }

    tokio::spawn(debridmoviemapper::tasks::run_scan_loop(
        rd_client.clone(),
        tmdb_client.clone(),
        vfs.clone(),
        db.clone(),
        repair_manager.clone(),
        scan_interval_secs,
        jellyfin_client,
    ));

    let dav_fs = DebridFileSystem::new(rd_client.clone(), vfs.clone(), repair_manager.clone());
    let dav_handler = DavHandler::builder()
        .filesystem(Box::new(dav_fs))
        .locksystem(dav_server::fakels::FakeLs::new())
        .build_handler();

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = TcpListener::bind(addr).await?;
    info!("WebDAV server listening on http://{}", addr);

    let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));

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
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal, stopping...");
                break;
            }
        }
    }

    info!("Shutdown complete.");
    Ok(())
}
