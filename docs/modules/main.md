# `main.rs`

Initializes shared state, spawns the scheduler (`scheduler::run`, which runs the scan loop + the Trakt/episode/upgrade jobs), starts the WebDAV server on `PORT` (default 8080), listening on every interface (`0.0.0.0`; there is no bind-address setting).
Before anything else, `--healthcheck` included, it loads `.env` through dotenvy.
If a `.env` exists but can't be loaded (a line dotenvy can't parse, such as an unquoted value containing a space, or a read error), it prints a `Configuration error` to stderr and exits 1 (`config::load_dotenv`, whose `dotenv_failure` builds the message). dotenvy stops at the first bad line, so every later setting would otherwise silently take its default.
The message never repeats the line, which may hold a token.
A missing `.env` is fine.
The accept loop is `serve(listener, max_connections, max_rejecters, handler, shutdown)`.
It holds the connection semaphore, the 503 rejecter (capped by `MAX_REJECTERS`) and the keepalive setup.
`route` sends `/trakt*` to the enrolment service (a 404 when Trakt is off) and everything else to WebDAV.
Loopback unit tests cover both, including the healthcheck contract: with the pool full the server answers 503 and reads healthy, and a dropped connection reads unhealthy.
Connections are bounded by a `MAX_CONNECTIONS` (256) semaphore, and `http1::Builder` sets a `header_read_timeout` (60s) so an idle/dead keep-alive connection can't pin its permit indefinitely (a dead peer would otherwise leak permits until the pool exhausts and the service silently rejects all new connections).
The `header_read_timeout` only bounds the HEADER phase, so a peer that dies uncleanly while its connection is **idle** (between range requests, or on a half-open keep-alive slot; no FIN/RST) is reclaimed by **TCP keepalive** (`set_tcp_keepalive`: 60s idle + 3×10s probes ≈ 90s) applied to every accepted socket — otherwise it would pin its permit until the OS retransmission timeout (minutes).
(A peer that dies *while* body bytes are being written is reclaimed by the retransmission timer instead — Linux only probes a socket with no unacknowledged data in flight — but the proxy sits idle between its many small range requests, so keepalive covers the dominant case.)
Active streams are unaffected (a live peer answers the probes).
`--healthcheck` mode issues a real `OPTIONS /` HTTP request (not a bare TCP connect) and requires an HTTP status line within 5s, so a dead or hung accept/handler path reports UNHEALTHY.
A saturated connection pool answers 503 and so reads HEALTHY by design (busy, not wedged; an orchestrator acting on health, such as Swarm, Kubernetes or an autoheal sidecar, would otherwise restart it and kill every stream); only when the 503-rejecter pool is also exhausted (the connection is dropped) does it read unhealthy.
Plain Docker/Compose never restart an unhealthy container (`restart:` policies act only on exit): with the shipped compose.yml the health status only gates `depends_on: service_healthy` at start-up, so a wedged server stays wedged until someone intervenes.
`header_read_timeout` plus TCP keepalive are what bound permit leaks.
Benign connection errors (client disconnects, our own header-read timeout) are classified by `is_benign_connection_error` (io-kind source check, then the `dbg_is_ignorable_disconnect` Debug-string fallback) and dropped silently; `real_header_read_timeout_is_classified_ignorable` drives a real hyper timeout through that same function.
It resolves `PORT` via the shared `config::resolve_port` (same logic as the server) so the probe can never target the wrong port.
On shutdown the wait for the scheduler is bounded by an 8s `tokio::time::timeout`, so a scan loop parked in a slow provider call or rate-limiter backpressure can't push graceful shutdown past `docker stop`'s ~10s grace into a SIGKILL.
The periodic Trakt/episode/upgrade jobs are not drained: `scheduler::periodic` cancels them at the await point they are parked on as soon as shutdown is signalled.
An upgrade candidate cut off mid-staging keeps its staging intent open, so the account mirror never adopts it and the scan loop's staging sweep deletes it (`upgrade::sweep_staging_intents`).
