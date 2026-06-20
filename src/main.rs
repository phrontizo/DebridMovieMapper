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
use hyper_util::rt::{TokioIo, TokioTimer};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tracing::info;

const MAX_CONNECTIONS: usize = 256;

/// Maximum time to wait for a client to send a complete set of request headers. hyper applies this
/// to EACH request, including the next-request wait on an idle HTTP/1.1 keep-alive connection — so a
/// peer that dies uncleanly (rclone container restart, killed player, network partition) without
/// sending FIN/RST no longer pins its connection permit (one of `MAX_CONNECTIONS`) indefinitely. It
/// only bounds the HEADER phase, never response-body streaming, so long media streams are unaffected;
/// a client whose idle keep-alive is closed simply reconnects on its next request.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The log-filter directive: `RUST_LOG` when set to something non-empty, else `info` (the prior
/// default, so logging is unchanged when the var is unset). Parsing/validation of the directive is
/// left to `EnvFilter` at the call site (a malformed value falls back to `info`).
fn log_directive(rust_log: Option<String>) -> String {
    rust_log
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "info".to_string())
}

/// Debug-string classification of a `serve_connection` error that is a benign/expected client-side
/// disconnect (`IncompleteMessage` — a player/WebDAV client dropping mid-request) or our own
/// `header_read_timeout` closing an idle keep-alive connection (`HeaderTimeout`, or the textual
/// "error reading header: operation timed out"). hyper 1.x doesn't expose typed accessors for these,
/// so we match its Debug output — pinned by a unit test so a hyper upgrade that renames a variant
/// fails CI rather than silently demoting the wrong errors. (io-level kinds are handled separately,
/// by downcast.) The timeout match is scoped to the "reading header" phrase rather than a bare
/// "timed out" so a genuine upstream/CDN body timeout (whose Debug also contains "timed out") is NOT
/// misclassified as an ignorable disconnect.
fn dbg_is_ignorable_disconnect(dbg: &str) -> bool {
    dbg.contains("IncompleteMessage")
        || dbg.contains("HeaderTimeout")
        || dbg.contains("reading header")
}

/// Whether a request path is routed to the Trakt enrolment service rather than WebDAV. Matches the
/// exact `/trakt` collection plus anything under `/trakt/` — but NOT an unrelated sibling such as
/// `/traktor` (a bare `starts_with("/trakt")` would misroute it). Extracted so it is unit-testable
/// (the routing closure that uses it lives inline in `serve_connection`).
fn is_trakt_path(p: &str) -> bool {
    p == "/trakt" || p.starts_with("/trakt/")
}

/// The shared HTTP/1 server-connection builder. Extracted so a test can drive a real
/// `serve_connection` through the EXACT config production uses: `header_read_timeout` is only honoured
/// when a `Timer` is also set, and hyper otherwise **panics inside `serve_connection`** ("timeout set,
/// but no timer set") — a panic that no `cargo test` would catch unless a test actually serves a
/// connection (see `http1_builder_serves_a_connection`). `TokioTimer` comes from hyper-util's `tokio`
/// feature (already enabled for `TokioIo`).
fn http1_builder() -> http1::Builder {
    let mut b = http1::Builder::new();
    b.timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    b
}

/// Docker liveness probe. Actually exercises the request/handler path (a minimal HTTP request),
/// NOT just `accept()`: the accept loop completes the TCP handshake BEFORE the connection-permit
/// check, so a bare `connect()` succeeds even when the pool is exhausted and the handler immediately
/// drops the stream — which would report a wedged, request-rejecting container as healthy and never
/// get it restarted. Requiring an HTTP status line back catches that. HTTP/1.0 so the server closes
/// the connection after responding (no keep-alive idle to wait on).
fn healthcheck(port: u16) -> bool {
    use std::io::{Read, Write};
    let Ok(addr) = format!("127.0.0.1:{port}").parse::<std::net::SocketAddr>() else {
        return false;
    };
    let timeout = std::time::Duration::from_secs(5);
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    // OPTIONS is cheap and WebDAV servers always answer it; any HTTP status line proves the handler
    // path is alive (even a 404/405 starts with "HTTP/").
    if stream
        .write_all(b"OPTIONS / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(n) => buf[..n].starts_with(b"HTTP/"),
        Err(_) => false,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env FIRST so the healthcheck resolves PORT identically to the server (which loads
    // .env then trim-parses PORT via Config). Without this, a PORT set only in .env — or one with
    // surrounding whitespace from quoted compose YAML — made the healthcheck probe 8080 while the
    // server listened elsewhere, reporting a healthy container as unhealthy.
    dotenvy::dotenv().ok();

    // Healthcheck mode: exercise the request/handler path, then exit.
    if std::env::args().any(|a| a == "--healthcheck") {
        // Resolve PORT identically to the server (Config also rejects 0 → 8080), so the probe never
        // targets a different port than the one the server actually bound.
        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .filter(|&p| p != 0)
            .unwrap_or(8080);
        std::process::exit(if healthcheck(port) { 0 } else { 1 });
    }

    // Honour RUST_LOG (e.g. `RUST_LOG=debridmoviemapper=debug`); default to INFO when unset so
    // behaviour is unchanged. A malformed directive falls back to INFO rather than crashing startup —
    // but is surfaced (after the subscriber is initialised) so an operator's typo isn't silent.
    let directive = log_directive(std::env::var("RUST_LOG").ok());
    let (filter, directive_error) = match tracing_subscriber::EnvFilter::try_new(&directive) {
        Ok(f) => (f, None),
        Err(e) => (tracing_subscriber::EnvFilter::new("info"), Some(e)),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();
    if let Some(e) = directive_error {
        tracing::warn!("Invalid RUST_LOG directive {directive:?} ({e}); falling back to info");
    }

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
        // Request reads in whole-buffer (2 MB) chunks rather than dav-server's 16 KB default, so each
        // 2 MB read-ahead is drained in one `read_bytes` call instead of ~128 — collapsing the
        // per-read lock/async overhead (repair health + read-activity) by the same factor under the
        // many concurrent rclone readers. `fetch_bytes` already clamps the fetch to BUFFER_SIZE, so
        // sizing is unchanged; a buffer hit just returns more of the existing refcounted slice.
        .read_buf_size(debridmoviemapper::dav_fs::BUFFER_SIZE)
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
    // Bounds the number of concurrent fire-and-forget 503-rejecter tasks (below) so a reconnect
    // storm against an already-saturated pool can't spawn unbounded short-lived tasks — the very
    // amplification the connection cap exists to prevent. When this is also exhausted, the excess
    // connection is simply dropped (closed) without a 503. The bound is set to MAX_CONNECTIONS so the
    // 503-rejecter pool is at least as large as the connection pool: that preserves the
    // healthcheck-503 guarantee (a transiently-saturated server still answers the `--healthcheck`
    // probe with 503 rather than a bare drop → Docker keeps it alive) for any realistic load, while
    // still capping the absolute worst case — each rejecter is a tiny ≤1s task, so this ceiling is
    // cheap and is only approached under a pathological multi-hundred-connection burst.
    const MAX_REJECTERS: usize = MAX_CONNECTIONS;
    let reject_semaphore = Arc::new(Semaphore::new(MAX_REJECTERS));

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

    // A minimal 503 sent to a connection rejected for pool saturation. Sending a real HTTP response
    // (rather than a bare TCP drop) means a transiently-saturated-but-alive server still answers the
    // `--healthcheck` probe's `OPTIONS /` with an `HTTP/` status line — so it reports HEALTHY (busy,
    // not wedged) instead of being restarted by Docker, which would kill every in-flight stream.
    const REJECT_503: &[u8] =
        b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
    // Throttle the saturation WARN so a reconnect storm can't spam the log (the accept()-error path
    // backs off; this path must not block legitimate accepts, so it rate-limits the log instead).
    let mut last_reject_log: Option<std::time::Instant> = None;

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
                        let now = std::time::Instant::now();
                        if last_reject_log
                            .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_secs(1))
                        {
                            tracing::warn!(
                                "Max connections ({}) reached — rejecting with 503",
                                MAX_CONNECTIONS
                            );
                            last_reject_log = Some(now);
                        }
                        // Fire-and-forget a 503 (bounded by a short write timeout so a slow/dead peer
                        // can't stall the accept loop), but only if a rejecter slot is free — so the
                        // rejecter tasks themselves stay bounded under a storm. No connection permit is
                        // held; this is a tiny one-shot write. If no rejecter slot is free, drop the
                        // connection (close on `stream` going out of scope).
                        if let Ok(reject_permit) = reject_semaphore.clone().try_acquire_owned() {
                            tokio::task::spawn(async move {
                                let _reject_permit = reject_permit; // released when the write finishes
                                use tokio::io::AsyncWriteExt;
                                let mut stream = stream;
                                let _ = tokio::time::timeout(
                                    std::time::Duration::from_secs(1),
                                    async {
                                        let _ = stream.write_all(REJECT_503).await;
                                        let _ = stream.shutdown().await;
                                    },
                                )
                                .await;
                            });
                        }
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let dav_handler = dav_handler.clone();
                let enrolment = enrolment.clone();

                tokio::task::spawn(async move {
                    let _permit = permit; // Hold permit until connection closes
                    if let Err(err) = http1_builder()
                        .serve_connection(
                            io,
                            service_fn(move |req: Request<hyper::body::Incoming>| {
                                let dav_handler = dav_handler.clone();
                                let enrolment = enrolment.clone();
                                async move {
                                    // Route the local-network Trakt enrolment paths to the
                                    // enrolment service; everything else is WebDAV. Both arms
                                    // produce a `Response<dav_server::body::Body>`.
                                    if is_trakt_path(req.uri().path()) {
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
                        // Benign client-side disconnects + our own idle keep-alive header timeout:
                        // typed io-kind inspection first, then a documented Debug-string fallback for
                        // hyper variants without typed accessors.
                        let io_benign = err
                            .source()
                            .and_then(|s| s.downcast_ref::<std::io::Error>())
                            .is_some_and(|io_err| {
                                matches!(
                                    io_err.kind(),
                                    std::io::ErrorKind::ConnectionReset
                                        | std::io::ErrorKind::BrokenPipe
                                        | std::io::ErrorKind::TimedOut
                                        | std::io::ErrorKind::UnexpectedEof
                                )
                            });
                        let dbg = format!("{:?}", err);
                        if io_benign || dbg_is_ignorable_disconnect(&dbg) {
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

    // Signal the background tasks to stop and wait for them to finish.
    //
    // NOTE: in-flight WebDAV connection tasks are detached (`tokio::task::spawn` above, each holding
    // a connection permit) and are NOT drained here — when `main` returns the runtime drops and those
    // tasks are aborted, so any active stream is cut mid-transfer. This is the intended behaviour for a
    // media proxy under `docker stop`: a player simply re-requests the byte range on restart, and a
    // bounded-grace drain would only delay shutdown for connections that are about to be re-opened
    // anyway. We DO drain the background scheduler (account sync / acquisition / Trakt / upgrade), whose
    // mid-flight provider mutations are worth finishing cleanly.
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
    use super::{
        dbg_is_ignorable_disconnect, healthcheck, http1_builder, is_trakt_path, log_directive,
    };

    #[test]
    fn is_trakt_path_matches_enrolment_routes_only() {
        assert!(is_trakt_path("/trakt"));
        assert!(is_trakt_path("/trakt/"));
        assert!(is_trakt_path("/trakt/accounts"));
        assert!(is_trakt_path("/trakt/enrol"));
        // Must NOT misroute WebDAV paths, including a sibling that merely shares the prefix.
        assert!(!is_trakt_path("/"));
        assert!(!is_trakt_path("/Movies/x.mkv"));
        assert!(!is_trakt_path("/traktor"));
        assert!(!is_trakt_path("/trakt-backup"));
    }

    #[tokio::test]
    async fn http1_builder_serves_a_connection() {
        // Regression: `header_read_timeout` PANICS inside serve_connection unless a `Timer` is also
        // set. Drive a real connection through the SAME builder production uses and assert a response
        // comes back — without the `.timer(...)` this fails (the served task panics, client reads 0
        // bytes), which no other test catches.
        use hyper::service::service_fn;
        use hyper::{Request, Response};
        use hyper_util::rt::TokioIo;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            let _ = http1_builder()
                .serve_connection(
                    io,
                    service_fn(|_req: Request<hyper::body::Incoming>| async {
                        Ok::<_, hyper::Error>(Response::new(dav_server::body::Body::from(
                            "ok".to_string(),
                        )))
                    }),
                )
                .await;
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        client.read_to_end(&mut buf).await.unwrap();
        assert!(
            buf.starts_with(b"HTTP/"),
            "expected an HTTP response (builder must not panic), got: {:?}",
            String::from_utf8_lossy(&buf)
        );
        let _ = server.await;
    }

    #[test]
    fn healthcheck_requires_an_http_response() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // A server that replies with an HTTP status line → healthy.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 128];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        assert!(healthcheck(port), "an HTTP response must report healthy");
        let _ = h.join();

        // A server that accepts but replies with non-HTTP bytes (e.g. an exhausted pool dropping the
        // stream after a bare accept) → unhealthy. A bare TCP connect would have falsely passed.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 128];
                let _ = s.read(&mut buf);
                // Drop without writing an HTTP response (closes the connection).
            }
        });
        assert!(
            !healthcheck(port),
            "a dropped/non-HTTP connection must report unhealthy"
        );
        let _ = h.join();

        // Nothing listening → unhealthy.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(
            !healthcheck(dead_port),
            "a closed port must report unhealthy"
        );
    }

    #[test]
    fn ignorable_disconnect_matches_expected_hyper_debug_strings() {
        // Pin the hyper Debug substrings we classify as benign. If a hyper upgrade renames a variant,
        // these assertions fail in CI instead of the connection-error log silently regressing.
        assert!(dbg_is_ignorable_disconnect(
            "hyper::Error(IncompleteMessage)"
        ));
        assert!(dbg_is_ignorable_disconnect("Error { kind: HeaderTimeout }"));
        assert!(dbg_is_ignorable_disconnect(
            "error reading header: operation timed out"
        ));
        // A genuine server-side body error must NOT be classified as an ignorable disconnect —
        // including a CDN body timeout, whose Debug also contains "timed out" but not "reading header".
        assert!(!dbg_is_ignorable_disconnect("Error { kind: User(Body) }"));
        assert!(!dbg_is_ignorable_disconnect(
            "Error { kind: User(Body), source: \"operation timed out\" }"
        ));
        assert!(!dbg_is_ignorable_disconnect("some unexpected error"));
    }

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
