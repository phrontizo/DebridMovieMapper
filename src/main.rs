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
use tokio::net::{TcpListener, TcpStream};
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

/// TCP keepalive applied to every accepted connection. `HEADER_READ_TIMEOUT` only bounds the header
/// phase, NOT response-body streaming — so a peer that dies uncleanly (half-open TCP, no FIN/RST:
/// network partition, killed container, yanked cable) while its connection is IDLE (between range
/// requests, or holding an idle keep-alive slot) would otherwise pin its `MAX_CONNECTIONS` permit
/// until the OS retransmission timeout fires (many minutes), and enough such dead connections exhaust
/// the pool. Keepalive makes the kernel probe the idle socket and tear the dead peer down in
/// ~idle + probes×interval (~90s here) instead, reclaiming the permit. (A peer that dies WHILE we are
/// actively writing body bytes is instead reclaimed by the retransmission timer — Linux only sends
/// keepalive probes on a socket with no unacknowledged data in flight — but this proxy sits idle
/// between its many small range requests, so keepalive covers the dominant case.) Active streams are
/// unaffected: a live peer answers the probes / has traffic.
const TCP_KEEPALIVE_IDLE: std::time::Duration = std::time::Duration::from_secs(60);
const TCP_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const TCP_KEEPALIVE_RETRIES: u32 = 3;

/// Enable bounded TCP keepalive on an accepted stream so a dead peer that goes idle (between requests
/// or on a half-open keep-alive slot) can't pin its connection permit until the OS retransmission
/// timeout. Best-effort: a failure to set the option (unusual) is logged at debug and the connection
/// proceeds without keepalive rather than being dropped.
fn set_tcp_keepalive(stream: &TcpStream) {
    let ka = socket2::TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_IDLE)
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES);
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_keepalive(&ka) {
        tracing::debug!("failed to set TCP keepalive on accepted connection: {e}");
    }
}

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
/// so we match its Debug output. `real_header_read_timeout_is_classified_ignorable` drives a LIVE
/// timeout through `serve_connection` and asserts production's classification
/// (`is_benign_connection_error`) still treats it as benign, so a hyper upgrade that changes the
/// timeout error's shape fails CI rather than silently demoting the wrong errors;
/// `ignorable_disconnect_matches_expected_hyper_debug_strings` separately pins this function's
/// accepted/rejected substrings. (io-level kinds are handled separately, by downcast, in
/// `is_benign_connection_error`.) The timeout match is scoped to the "reading header" phrase rather
/// than a bare "timed out" so a genuine upstream/CDN body timeout (whose Debug also contains "timed
/// out") is NOT misclassified as an ignorable disconnect.
fn dbg_is_ignorable_disconnect(dbg: &str) -> bool {
    dbg.contains("IncompleteMessage")
        || dbg.contains("HeaderTimeout")
        || dbg.contains("reading header")
}

/// Whether a `serve_connection` error is a benign client-side disconnect or our own idle keep-alive
/// `header_read_timeout`, so the accept loop drops it silently instead of logging it. Typed io-kind
/// inspection of the error's source first (reset / broken pipe / timed out / unexpected EOF), then
/// the Debug-string fallback (`dbg_is_ignorable_disconnect`) for hyper variants without typed
/// accessors. The accept loop and `real_header_read_timeout_is_classified_ignorable` both call it.
fn is_benign_connection_error(err: &hyper::Error) -> bool {
    use std::error::Error;
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
    io_benign || dbg_is_ignorable_disconnect(&format!("{err:?}"))
}

/// Whether a request path is routed to the Trakt enrolment service rather than WebDAV. Matches the
/// exact `/trakt` collection plus anything under `/trakt/` — but NOT an unrelated sibling such as
/// `/traktor` (a bare `starts_with("/trakt")` would misroute it). Extracted so it is unit-testable;
/// `route` uses it.
fn is_trakt_path(p: &str) -> bool {
    p == "/trakt" || p.starts_with("/trakt/")
}

/// Route one request: the local-network Trakt enrolment paths (`is_trakt_path`) to `enrolment`, or
/// a 404 when enrolment is off (Trakt is not configured), and everything else to WebDAV. Generic
/// over the request body, as both handlers are, so a test can route a `Request<Body>`.
async fn route<B>(
    req: Request<B>,
    dav: &DavHandler,
    enrolment: Option<&EnrolmentService>,
) -> hyper::Response<dav_server::body::Body>
where
    B: hyper::body::Body,
    B::Data: bytes::Buf + Send + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    if !is_trakt_path(req.uri().path()) {
        return dav.handle(req).await;
    }
    match enrolment {
        Some(enrolment) => enrolment.handle(req).await,
        None => {
            // Built infallibly (Response::new + status_mut), so the per-request path carries no
            // panic site.
            let mut resp = hyper::Response::new(dav_server::body::Body::from(
                "Trakt enrolment is not enabled".to_string(),
            ));
            *resp.status_mut() = hyper::StatusCode::NOT_FOUND;
            resp
        }
    }
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

/// Docker `HEALTHCHECK` probe (`--healthcheck`). Sends a real `OPTIONS /` request and requires an
/// HTTP status line back within 5s, rather than a bare `connect()`: the kernel completes the TCP
/// handshake on its own, so a connect succeeds even when the accept loop or the handler path is dead
/// or hung. Any status line counts as healthy, including the 503 the accept loop sends when the
/// connection pool is full: a saturated pool is busy, not wedged, and anything that acts on health
/// (Swarm, Kubernetes, an autoheal sidecar) would otherwise restart it and kill every stream. Only
/// when the 503-rejecter pool is exhausted too (the connection is dropped) does a full pool read
/// unhealthy. Plain Docker / Compose never restart an unhealthy container (`restart:` policies act
/// only on exit); with the shipped compose.yml the status only gates `depends_on: service_healthy`
/// at start-up, so a wedged server stays wedged until someone intervenes. `HEADER_READ_TIMEOUT` and
/// TCP keepalive (`set_tcp_keepalive`) are what bound permit leaks. HTTP/1.0 so the server closes
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
    // Read until we have at least the 5-byte "HTTP/" status-line prefix. A single read() can
    // deliver fewer bytes than were sent, so requiring the prefix in the FIRST chunk could mark a
    // healthy server unhealthy on a short read. The read timeout set above bounds the loop; an
    // error/timeout or EOF-before-5-bytes reports unhealthy.
    let mut buf = [0u8; 64];
    let mut filled = 0;
    while filled < 5 {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break, // EOF before the status line → not a live HTTP responder
            Ok(n) => filled += n,
            Err(_) => return false, // timeout / connection error
        }
    }
    buf[..filled].starts_with(b"HTTP/")
}

/// Bounds the number of concurrent fire-and-forget 503-rejecter tasks (see `serve`) so a reconnect
/// storm against an already-saturated pool can't spawn unbounded short-lived tasks — the very
/// amplification the connection cap exists to prevent. When this is also exhausted, the excess
/// connection is simply dropped (closed) without a 503. The bound is set to MAX_CONNECTIONS so the
/// 503-rejecter pool is at least as large as the connection pool: that preserves the
/// healthcheck-503 guarantee (a transiently-saturated server still answers the `--healthcheck`
/// probe with 503 rather than a bare drop → it reads HEALTHY) for any realistic load, while
/// still capping the absolute worst case — each rejecter is a tiny ≤1s task, so this ceiling is
/// cheap and is only approached under a pathological multi-hundred-connection burst.
const MAX_REJECTERS: usize = MAX_CONNECTIONS;

/// A minimal 503 sent to a connection rejected for pool saturation. Sending a real HTTP response
/// (rather than a bare TCP drop) means a transiently-saturated-but-alive server still answers the
/// `--healthcheck` probe's `OPTIONS /` with an `HTTP/` status line — so it reports HEALTHY (busy,
/// not wedged) rather than UNHEALTHY, which an orchestrator acting on health would answer with a
/// restart that kills every in-flight stream. (Plain Docker / Compose don't restart an unhealthy
/// container; see `healthcheck`.)
const REJECT_503: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";

/// The accept loop: serve every connection on `listener` with `handler` until `shutdown`
/// completes. At most `max_connections` are served at once (production: `MAX_CONNECTIONS`), each
/// holding a permit until it closes. A connection beyond that is answered `REJECT_503` by a
/// short-lived task, at most `max_rejecters` of them at a time (`MAX_REJECTERS`), and dropped
/// without an answer once those are busy too. The healthcheck's contract rests on this: a full
/// pool still reads HEALTHY, and only an exhausted rejecter pool reads unhealthy. Connections
/// still open when it returns are not drained (see `main`).
async fn serve<H, Fut>(
    listener: TcpListener,
    max_connections: usize,
    max_rejecters: usize,
    handler: H,
    shutdown: impl std::future::Future<Output = ()>,
) where
    H: Fn(Request<hyper::body::Incoming>) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = hyper::Response<dav_server::body::Body>> + Send + 'static,
{
    let semaphore = Arc::new(Semaphore::new(max_connections));
    let reject_semaphore = Arc::new(Semaphore::new(max_rejecters));
    // Throttle the saturation WARN so a reconnect storm can't spam the log (the accept()-error path
    // backs off; this path must not block legitimate accepts, so it rate-limits the log instead).
    let mut last_reject_log: Option<std::time::Instant> = None;
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _addr) = match result {
                    Ok(pair) => {
                        // Bound how long a mid-stream dead peer can pin its permit (see
                        // set_tcp_keepalive) — applied before the permit is acquired so it also
                        // covers a connection that then races into the 503-reject path.
                        set_tcp_keepalive(&pair.0);
                        pair
                    }
                    Err(e) => {
                        // A transient accept() error — fd exhaustion (EMFILE/ENFILE), a peer
                        // that aborted before we accepted (ECONNABORTED), or EINTR — must NOT
                        // tear down the whole listener (which would also skip graceful
                        // shutdown in `main`). Log, briefly back off so we don't hot-spin while
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
                                max_connections
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
                let handler = handler.clone();

                tokio::task::spawn(async move {
                    let _permit = permit; // Hold permit until connection closes
                    if let Err(err) = http1_builder()
                        .serve_connection(
                            io,
                            service_fn(move |req: Request<hyper::body::Incoming>| {
                                let response = handler(req);
                                async move { Ok::<_, hyper::Error>(response.await) }
                            }),
                        )
                        .await
                    {
                        // Benign client-side disconnects + our own idle keep-alive header timeout.
                        if is_benign_connection_error(&err) {
                            return;
                        }
                        let dbg = format!("{:?}", err);
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
            _ = &mut shutdown => {
                break;
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env FIRST so the healthcheck resolves PORT identically to the server (which loads
    // .env then trim-parses PORT via Config). Without this, a PORT set only in .env — or one with
    // surrounding whitespace from quoted compose YAML — made the healthcheck probe 8080 while the
    // server listened elsewhere, reporting a healthy container as unhealthy.
    // A .env that exists but can't be loaded stops startup, the healthcheck included: dotenvy would
    // have applied only the lines before the bad one (`config::load_dotenv`). A missing .env is
    // fine. eprintln!, as the tracing subscriber isn't up yet.
    if let Err(message) = debridmoviemapper::config::load_dotenv() {
        eprintln!("{message}");
        std::process::exit(1);
    }

    // Healthcheck mode: exercise the request/handler path, then exit.
    if std::env::args().any(|a| a == "--healthcheck") {
        // Resolve PORT identically to the server via the shared `config::resolve_port` (rejects 0 →
        // 8080), so the probe never targets a different port than the one the server actually bound.
        let port = debridmoviemapper::config::resolve_port(std::env::var("PORT").ok().as_deref()).0;
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

    // Open the metadata store. A genuinely corrupt DB is moved aside (<db_path>.corrupt[.N]) and
    // recreated; an intact-but-unusable one (newer schema, older redb format, locked by another
    // instance, I/O/permission error) fails startup here rather than being discarded.
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
    let scraper: Arc<dyn debridmoviemapper::scraper::Scraper> = Arc::new(
        debridmoviemapper::scraper::TorrentioScraper::new(
            config.acquisition.scraper_addon_url.clone(),
            config.provider_kind,
            &config.provider_token,
            scraper_http,
        )
        .with_min_interval(std::time::Duration::from_millis(
            config.acquisition.scraper_min_interval_ms,
        )),
    );
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
    )
    .with_first_read_bytes(app_state.config.cdn_first_read_bytes);
    let dav_handler = DavHandler::builder()
        .filesystem(Box::new(dav_fs))
        .locksystem(dav_server::fakels::FakeLs::new())
        // Request reads in whole-buffer (2 MB) chunks rather than dav-server's 16 KB default, so each
        // 2 MB read-ahead is drained in one `read_bytes` call instead of ~128 — collapsing the
        // per-read lock/async overhead (repair health + read-activity) by the same factor under the
        // many concurrent rclone readers. `fetch_bytes` sizes each CDN fetch independently of the
        // requested `len` (BUFFER_SIZE on a warm miss, `first_read_bytes`/CDN_FIRST_READ_MB on a cold
        // one), so this only changes how many `read_bytes` calls drain a buffer; a buffer hit just
        // returns more of the existing refcounted slice.
        .read_buf_size(debridmoviemapper::dav_fs::BUFFER_SIZE)
        .build_handler();

    // Local-network Trakt enrolment routes (no auth — trusted LAN), present only when Trakt is
    // configured. Served on the same listener; `/trakt*` requests are routed here below.
    let enrol_allowed_hosts = app_state
        .config
        .trakt
        .as_ref()
        .map(|t| t.enrol_allowed_hosts.clone())
        .unwrap_or_default();
    let enrolment: Option<Arc<EnrolmentService>> = app_state.trakt_client.clone().map(|t| {
        Arc::new(
            EnrolmentService::new(t, app_state.store.clone())
                .with_allowed_hosts(enrol_allowed_hosts),
        )
    });
    if enrolment.is_some() {
        info!("Trakt enrolment page available at /trakt/accounts");
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], app_state.config.port));
    let listener = TcpListener::bind(addr).await?;
    info!("WebDAV server listening on http://{}", addr);

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

    // Every request is routed by `route`: the `/trakt*` paths to the enrolment service (or a 404
    // when Trakt isn't configured), everything else to WebDAV.
    let handler = move |req: Request<hyper::body::Incoming>| {
        let dav_handler = dav_handler.clone();
        let enrolment = enrolment.clone();
        async move { route(req, &dav_handler, enrolment.as_deref()).await }
    };
    serve(
        listener,
        MAX_CONNECTIONS,
        MAX_REJECTERS,
        handler,
        shutdown_signal,
    )
    .await;

    // Signal the background tasks to stop and wait for them to finish.
    //
    // NOTE: in-flight WebDAV connection tasks are detached (`tokio::task::spawn` in `serve`, each
    // holding a connection permit) and are NOT drained here — when `main` returns the runtime drops and those
    // tasks are aborted, so any active stream is cut mid-transfer. This is the intended behaviour for a
    // media proxy under `docker stop`: a player simply re-requests the byte range on restart, and a
    // bounded-grace drain would only delay shutdown for connections that are about to be re-opened
    // anyway. We DO wait for the background scheduler to stop, but its jobs are not drained: the
    // periodic Trakt / episode / upgrade jobs are cancelled at the await point they are parked on as
    // soon as the shutdown signal flips (`scheduler::periodic`), and only the scan loop runs on to
    // its own next shutdown check. An upgrade candidate cut off mid-staging, or an acquisition cut
    // off between its add and its `Pending` record, keeps its staging intent, so the next start's
    // staging sweep deletes it (`upgrade::sweep_staging_intents`).
    let _ = shutdown_tx.send(true);
    info!("Waiting for background tasks to finish...");
    // Bound the wait: the scan loop parked in a slow provider call (the provider clients' 60 s request
    // timeout) or the shared rate limiter's backpressure could otherwise delay shutdown past `docker
    // stop`'s ~10s grace → SIGKILL. The scheduler's operations are idempotent, so a task cut here is
    // redone next start (a cut staging or acquisition is swept, as above). Dropping the JoinHandle on
    // timeout detaches the task; the runtime aborts it as `main` returns.
    const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(8);
    match tokio::time::timeout(SHUTDOWN_GRACE, scheduler_handle).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::error!("Scheduler task ended abnormally: {:?}", e),
        Err(_) => tracing::warn!(
            "Scheduler drain exceeded {}s; forcing shutdown",
            SHUTDOWN_GRACE.as_secs()
        ),
    }

    info!("Shutdown complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        dbg_is_ignorable_disconnect, healthcheck, http1_builder, is_benign_connection_error,
        is_trakt_path, log_directive, route, serve, set_tcp_keepalive,
    };
    use dav_server::DavHandler;
    use hyper::Request;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn set_tcp_keepalive_enables_keepalive_on_accepted_socket() {
        // The accept-loop applies keepalive so a mid-stream dead peer can't pin its permit until the
        // OS retransmission timeout. Verify SO_KEEPALIVE is actually enabled on the accepted socket.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        set_tcp_keepalive(&server);
        assert!(
            socket2::SockRef::from(&server).keepalive().unwrap(),
            "SO_KEEPALIVE must be enabled on the accepted connection"
        );
        drop(client);
    }

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
    fn healthcheck_accumulates_a_split_status_line() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        // MAIN1: the "HTTP/" prefix arrives split across multiple reads — "HT" then, after a delay,
        // the rest. The accumulation loop must gather ≥5 bytes and report healthy, NOT mark a healthy
        // server unhealthy because the first read() delivered fewer than 5 bytes.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 128];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HT");
                let _ = s.flush();
                std::thread::sleep(std::time::Duration::from_millis(50));
                let _ = s.write_all(b"TP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        assert!(
            healthcheck(port),
            "a status line split across reads must still report healthy"
        );
        let _ = h.join();
    }

    #[tokio::test]
    async fn real_header_read_timeout_is_classified_ignorable() {
        // Drive a REAL header-read timeout through serve_connection (client connects but never sends
        // request headers) and assert production's classification treats the resulting hyper error as
        // a benign disconnect. Unlike the hardcoded-string test below, this constructs an actual hyper
        // error, so a hyper upgrade that changes the timeout error's shape (Debug string AND io-kind
        // source) fails CI here rather than silently regressing the connection-error log.
        use hyper::service::service_fn;
        use hyper::{Request, Response};
        use hyper_util::rt::{TokioIo, TokioTimer};
        use tokio::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            let mut b = hyper::server::conn::http1::Builder::new();
            b.timer(TokioTimer::new())
                .header_read_timeout(std::time::Duration::from_millis(50));
            b.serve_connection(
                io,
                service_fn(|_req: Request<hyper::body::Incoming>| async {
                    Ok::<_, hyper::Error>(Response::new(dav_server::body::Body::from(
                        "ok".to_string(),
                    )))
                }),
            )
            .await
        });

        // Connect but send NOTHING, holding the socket open so the header-read timeout fires.
        let _client = TcpStream::connect(addr).await.unwrap();
        let err = server
            .await
            .unwrap()
            .expect_err("a client that never sends headers must trigger the header-read timeout");
        // The classifier the serve_connection error arm calls, so an edit to production's
        // classification is tested too.
        assert!(
            is_benign_connection_error(&err),
            "a real hyper header-read timeout must classify as a benign disconnect, got: {err:?}"
        );
    }

    #[test]
    fn ignorable_disconnect_matches_expected_hyper_debug_strings() {
        // Pin the classifier's accepted/rejected SUBSTRINGS (guarding an accidental edit to
        // dbg_is_ignorable_disconnect). The real-error coverage — that a live hyper timeout still
        // classifies as benign — is in `real_header_read_timeout_is_classified_ignorable` above.
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

    /// A running accept loop (`serve`) on a loopback port. Its handler answers `/hold` only once
    /// `release` is notified (signalling `entered` when the request arrives), so a test can keep a
    /// connection, and its permit, busy; any other path is answered `200` at once.
    struct TestServer {
        port: u16,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        shutdown: tokio::sync::oneshot::Sender<()>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn start_server(max_connections: usize, max_rejecters: usize) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let handler = {
            let (entered, release) = (entered.clone(), release.clone());
            move |req: Request<hyper::body::Incoming>| {
                let (entered, release) = (entered.clone(), release.clone());
                async move {
                    if req.uri().path() == "/hold" {
                        entered.notify_one();
                        release.notified().await;
                    }
                    hyper::Response::new(dav_server::body::Body::from("ok".to_string()))
                }
            }
        };
        let (shutdown, stop) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(serve(
            listener,
            max_connections,
            max_rejecters,
            handler,
            async {
                let _ = stop.await;
            },
        ));
        TestServer {
            port,
            entered,
            release,
            shutdown,
            task,
        }
    }

    impl TestServer {
        /// Hold the server's only connection permit: a `/hold` request whose answer the returned
        /// task yields once `release` is notified.
        async fn hold(&self) -> tokio::task::JoinHandle<String> {
            let held = tokio::spawn(exchange(self.port, "/hold"));
            tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
                .await
                .expect("the held request should reach the handler");
            held
        }

        /// Signal shutdown and check that the accept loop returns.
        async fn stop(self) {
            let _ = self.shutdown.send(());
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .expect("serve should return once shutdown completes")
                .unwrap();
        }
    }

    /// Send one request and read whatever arrives before the server closes (or resets) the
    /// connection: a full response, a bare 503, or nothing when the connection is dropped.
    async fn exchange(port: u16, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let _ = stream.write_all(request.as_bytes()).await;
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut buf)).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Whether a request is answered with a status line starting `status` within about a second.
    /// A permit is released a moment after its connection closes, so the next connection can
    /// arrive first and be rejected; this retries until it doesn't.
    async fn answered_eventually(port: u16, status: &str) -> bool {
        for _ in 0..50 {
            if exchange(port, "/").await.starts_with(status) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_full_connection_pool_answers_503_and_reads_healthy() {
        // The healthcheck's contract: a saturated pool is busy, not wedged, so the accept loop
        // answers the probe's connection with a 503 status line and it reads HEALTHY.
        let server = start_server(1, 1).await;
        let port = server.port;
        let held = server.hold().await;
        assert!(
            tokio::task::spawn_blocking(move || healthcheck(port))
                .await
                .unwrap(),
            "a full pool must still answer the healthcheck"
        );
        // The rejecter holds no connection permit and gives its own back once it has written, so
        // with one rejecter slot every later excess connection still gets its 503.
        for _ in 0..3 {
            assert!(
                answered_eventually(port, "HTTP/1.1 503").await,
                "an excess connection must be answered 503"
            );
        }
        // Releasing the held connection frees its permit, and the next connection is served.
        server.release.notify_one();
        let answer = held.await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer:?}");
        assert!(
            answered_eventually(port, "HTTP/1.1 200").await,
            "a connection must be served once a permit is free"
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn a_full_pool_with_no_free_rejecter_drops_the_connection_and_reads_unhealthy() {
        let server = start_server(1, 0).await;
        let port = server.port;
        let held = server.hold().await;
        let excess = exchange(port, "/").await;
        assert!(
            excess.is_empty(),
            "with no rejecter slot free the connection is dropped: {excess:?}"
        );
        assert!(
            !tokio::task::spawn_blocking(move || healthcheck(port))
                .await
                .unwrap(),
            "a dropped connection must read unhealthy"
        );
        // The loop survives the drops and serves again once the permit is free.
        server.release.notify_one();
        assert!(held.await.unwrap().starts_with("HTTP/1.1 200"));
        assert!(answered_eventually(port, "HTTP/1.1 200").await);
        server.stop().await;
    }

    #[tokio::test]
    async fn route_answers_404_on_the_trakt_paths_when_enrolment_is_off() {
        // A DavHandler with no filesystem answers every non-OPTIONS request 405, so anything but
        // a 404 shows the request reached WebDAV.
        let dav = DavHandler::builder().build_handler();
        let request = |path: &str| {
            Request::builder()
                .uri(path)
                .body(dav_server::body::Body::empty())
                .unwrap()
        };
        for path in ["/trakt", "/trakt/accounts"] {
            let resp = route(request(path), &dav, None).await;
            assert_eq!(resp.status(), hyper::StatusCode::NOT_FOUND, "{path}");
        }
        for path in ["/", "/Movies/x.mkv", "/traktor"] {
            let resp = route(request(path), &dav, None).await;
            assert_ne!(resp.status(), hyper::StatusCode::NOT_FOUND, "{path}");
        }
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
