//! Local-network Trakt enrolment routes, served on the existing WebDAV listener.
//!
//! Trust model: these routes carry **no authentication** — the design assumes the WebDAV
//! port is only reachable on a trusted local network (same as the media itself). They let an
//! operator link a Trakt account via OAuth device-flow, list linked accounts, and refresh or
//! remove them. Tokens are keyed by the user's Trakt *slug* (stable per-user key).
//!
//! Three browser-side guards back that model up: a `Host` allowlist (only local-network names,
//! plus `TRAKT_ENROL_ALLOWED_HOSTS`) refuses DNS-rebinding pages, a same-origin check refuses
//! cross-origin form POSTs, and the pages refuse to be framed (`frame-ancestors 'none'`,
//! `X-Frame-Options: DENY`), so a click on another site can't drive them (clickjacking).
//!
//! Responses are built as `dav_server::body::Body` (via `Body::from(String)`) so `main`'s
//! per-connection service can return either these or `DavHandler::handle`'s response uniformly.

use crate::error::AppError;
use crate::store::{Store, TraktTokens};
use crate::trakt_client::{DeviceCode, DeviceTokenPoll, TraktClient};
use dav_server::body::Body;
use hyper::{Method, Response, StatusCode};
use std::sync::Arc;
use tracing::{info, warn};

/// Handles the local-network Trakt enrolment routes. Constructed only when Trakt is enabled.
#[derive(Clone)]
pub struct EnrolmentService {
    trakt: Arc<dyn TraktClient>,
    store: Store,
    /// Caps concurrent device-flow enrolment polls. The `/trakt/enrol` route carries no auth, so
    /// without this a single client could spawn unbounded long-lived (~600s) background poll tasks —
    /// exhausting host resources and hammering Trakt's API under our `client_id`. A small bound (not
    /// one) lets a household link several accounts back-to-back without the next `/trakt/enrol`
    /// 429-ing while an earlier code is still pending approval, while still keeping the exposure
    /// tightly bounded.
    inflight: Arc<tokio::sync::Semaphore>,
    /// Extra hostnames (lowercase, no port) accepted in the `Host` header on top of the built-in
    /// local-network names — from `TRAKT_ENROL_ALLOWED_HOSTS`, e.g. a reverse proxy's FQDN.
    allowed_hosts: Vec<String>,
    /// Slugs whose Refresh click's refresh is still running. That refresh outlives the click (see
    /// the refresh route), so without this every further click while Trakt throttles would queue
    /// one more refresh behind the shared rate limiter, pushing the background sync back further.
    refreshing: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// A slug's claim on `EnrolmentService::refreshing`, released on drop (a panicking refresh too).
struct RefreshClaim {
    refreshing: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    slug: String,
}

impl RefreshClaim {
    /// `None` while a refresh of `slug` is already running.
    fn take(
        refreshing: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
        slug: &str,
    ) -> Option<Self> {
        let mut set = refreshing.lock().unwrap_or_else(|e| e.into_inner());
        set.insert(slug.to_string()).then(|| RefreshClaim {
            refreshing: refreshing.clone(),
            slug: slug.to_string(),
        })
    }
}

impl Drop for RefreshClaim {
    fn drop(&mut self) {
        self.refreshing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.slug);
    }
}

/// Max simultaneous pending device-flow enrolments (each holds a slot for the ~600s poll window).
const MAX_CONCURRENT_ENROLMENTS: usize = 3;

/// How long a Refresh or Enrol click waits on Trakt before the page answers. Both calls share the
/// Trakt client's rate limiter with the background sync, so while a throttled sync has parked it
/// a 429 back-off (up to `ratelimit::MAX_RETRY_AFTER_SECS` per attempt) would otherwise hold the
/// browser, and its connection permit, for minutes.
const TRAKT_CLICK_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

impl EnrolmentService {
    pub fn new(trakt: Arc<dyn TraktClient>, store: Store) -> Self {
        Self {
            trakt,
            store,
            inflight: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_ENROLMENTS)),
            allowed_hosts: Vec::new(),
            refreshing: Default::default(),
        }
    }

    /// Also accept these hostnames (lowercase, no port; see `config::parse_allowed_hosts`) in the
    /// `Host` header, on top of the built-in local-network names.
    pub fn with_allowed_hosts(mut self, hosts: Vec<String>) -> Self {
        self.allowed_hosts = hosts;
        self
    }

    /// Route a `/trakt*` request by method + path. Unknown `/trakt` paths return 404.
    // Generic over the body type — `handle` inspects only method/path/headers, never the body — so
    // the router (incl. the CSRF guard, 404 fallback, and slug decoding) is unit-testable with a
    // lightweight `Request<()>` rather than a real streaming `Incoming` body.
    pub async fn handle<B>(&self, req: hyper::Request<B>) -> Response<Body> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // DNS-rebinding guard, for every method (the GET page lists the account slugs). A page on
        // an attacker's domain whose name is rebound to this server's IP is same-origin as far as
        // the browser knows: its Origin matches its Host, so the CSRF guard below passes it. Its
        // Host still names the attacker's domain, though, so answer only for local-network names
        // the attacker can't point at us. A request with no Host header isn't from a browser.
        if let Some(host) = req.headers().get(hyper::header::HOST) {
            let allowed = host
                .to_str()
                .is_ok_and(|h| host_is_allowed(h, &self.allowed_hosts));
            if !allowed {
                warn!(
                    "Rejecting {} {} for Host {:?}: not a local-network name \
                     (add it to TRAKT_ENROL_ALLOWED_HOSTS to allow it)",
                    method, path, host
                );
                return html(
                    StatusCode::MISDIRECTED_REQUEST,
                    "<h1>421 Misdirected Request</h1><p>This host name is not accepted. \
                     Add it to TRAKT_ENROL_ALLOWED_HOSTS to use it.</p>"
                        .to_string(),
                );
            }
        }

        // CSRF guard: the state-changing POST routes are unauthenticated, so the trusted-LAN
        // model alone does not stop a drive-by cross-origin form POST from a page a LAN browser
        // visits. Reject any POST a browser flags as cross-origin (non-browser clients send
        // neither header and are allowed, consistent with the trusted-LAN model).
        if method == Method::POST && is_cross_origin(&req) {
            // Log the values the check compared, so an operator can see a Host that a reverse
            // proxy rewrote. None is a secret, and `HeaderValue`'s Debug escapes control bytes.
            let headers = req.headers();
            warn!(
                "Rejecting cross-origin POST to {} (CSRF guard): Origin {:?}, Host {:?}, \
                 Sec-Fetch-Site {:?} (behind a reverse proxy, forward the browser's Host header \
                 unchanged)",
                path,
                headers.get(hyper::header::ORIGIN),
                headers.get(hyper::header::HOST),
                headers.get("sec-fetch-site"),
            );
            return html(
                StatusCode::FORBIDDEN,
                "<h1>403 Forbidden</h1><p>Cross-origin request rejected.</p>".to_string(),
            );
        }

        match (&method, segments.as_slice()) {
            (&Method::POST, ["trakt", "enrol"]) => self.start_enrolment().await,
            (&Method::GET, ["trakt"]) | (&Method::GET, ["trakt", "accounts"]) => {
                self.accounts_page().await
            }
            (&Method::POST, ["trakt", "accounts", slug, "refresh"]) => {
                // Decode the percent-encoded path segment back to the raw stored slug (see
                // `decode_path_segment`) — otherwise a fallback slug with a space/special byte
                // never matches the stored key and the button silently no-ops.
                let slug = decode_path_segment(slug);
                let Some(claim) = RefreshClaim::take(&self.refreshing, &slug) else {
                    info!(
                        "Trakt token refresh for '{}' is already running; not starting another",
                        slug
                    );
                    return redirect("/trakt/accounts");
                };
                // The refresh runs in its own task, so a browser that gives up and closes the
                // connection (dropping this future) can't cut it off after Trakt has processed the
                // grant and lose the rotated single-use refresh token. That is also why the
                // refresh itself is never put under a timeout. The click waits for it at most
                // `TRAKT_CLICK_WAIT`. A refresh held up longer by the shared limiter's 429
                // back-off finishes in the background, and the accounts page shows the outcome
                // once reloaded.
                let svc = self.clone();
                let task_slug = slug.clone();
                let refresh = tokio::spawn(async move {
                    let _claim = claim;
                    if let Err(e) = svc.refresh_account(&task_slug).await {
                        // refresh_account already logs a Trakt-refresh failure (flagging the
                        // account only for a non-transient one); the only error surfaced here is a
                        // token-store write failure, which would otherwise be lost silently
                        // (unlike the `remove` arm below).
                        warn!(
                            "Trakt token refresh persist failed for '{}': {}",
                            task_slug, e
                        );
                    }
                });
                match tokio::time::timeout(TRAKT_CLICK_WAIT, refresh).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!("Trakt token refresh for '{}' panicked: {}", slug, e),
                    // Dropping the JoinHandle detaches the task; it is not aborted.
                    Err(_) => info!(
                        "Trakt token refresh for '{}' is still waiting on Trakt after {}s \
                         (rate-limited?); it carries on in the background",
                        slug,
                        TRAKT_CLICK_WAIT.as_secs()
                    ),
                }
                redirect("/trakt/accounts")
            }
            // Removing is two-step: the accounts page's Remove button GETs this confirmation page
            // (no state change), whose own form POSTs below. Removing an account drops its wanted
            // rows, and the next Trakt cycle then deletes the content only it wanted.
            (&Method::GET, ["trakt", "accounts", slug, "remove"]) => {
                let slug = decode_path_segment(slug);
                match self.store.get_trakt_tokens(slug.clone()).await {
                    Some(tok) => html(StatusCode::OK, confirm_remove_html(&slug, &tok.username)),
                    None => html(
                        StatusCode::NOT_FOUND,
                        "<h1>404 Not Found</h1><p>No such account. <a href=\"/trakt/accounts\">Back to accounts</a></p>"
                            .to_string(),
                    ),
                }
            }
            (&Method::POST, ["trakt", "accounts", slug, "remove"]) => {
                let slug = decode_path_segment(slug);
                if let Err(e) = self.remove_account(&slug).await {
                    warn!("Trakt token removal failed for '{}': {}", slug, e);
                }
                redirect("/trakt/accounts")
            }
            _ => html(StatusCode::NOT_FOUND, "<h1>404 Not Found</h1>".to_string()),
        }
    }

    /// Begin device-flow enrolment: fetch a device code, spawn the background poll that
    /// completes the link, and return HTML showing the user_code + verification URL. A device code
    /// that takes longer than `TRAKT_CLICK_WAIT` to arrive is given up on with a 503.
    pub(crate) async fn start_enrolment(&self) -> Response<Body> {
        // Admit only a small bounded number of in-flight enrolments (the route is unauthenticated).
        // The owned permit is moved into the background poll below and released when it
        // finishes/expires.
        let Ok(permit) = self.inflight.clone().try_acquire_owned() else {
            return html(
                StatusCode::TOO_MANY_REQUESTS,
                "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Enrolment in progress</title></head>\
                 <body><h1>Too many enrolments already in progress</h1>\
                 <p>Finish approving a pending device code on trakt.tv, or wait for one to expire, then try again.</p>\
                 <p><a href=\"/trakt/accounts\">Back to accounts</a></p></body></html>"
                    .to_string(),
            );
        };
        // Unlike the Refresh click's refresh, this request may be cut off: an unused device code
        // simply expires.
        let dc = match tokio::time::timeout(TRAKT_CLICK_WAIT, self.trakt.device_code()).await {
            Ok(Ok(dc)) => dc,
            Ok(Err(e)) => {
                // `permit` drops here, releasing the slot for the next attempt.
                return html(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Enrolment error</title></head>\
                         <body><h1>Could not start enrolment</h1><p>{}</p></body></html>",
                        esc(&e.to_string())
                    ),
                );
            }
            Err(_) => {
                warn!(
                    "Trakt device-code request still unanswered after {}s (rate-limited?); \
                     answering 503",
                    TRAKT_CLICK_WAIT.as_secs()
                );
                // `permit` drops here too.
                return html(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Enrolment error</title></head>\
                     <body><h1>Could not start enrolment</h1>\
                     <p>Trakt is slow to answer (probably rate-limiting); try again shortly.</p>\
                     <p><a href=\"/trakt/accounts\">Back to accounts</a></p></body></html>"
                        .to_string(),
                );
            }
        };
        // Spawn the device-flow completion in the background; the operator approves the code on
        // trakt.tv and this poll persists the tokens once authorised.
        let trakt = self.trakt.clone();
        let store = self.store.clone();
        let device_code = dc.device_code.clone();
        let interval = dc.interval;
        let expires_in = dc.expires_in;
        tokio::spawn(async move {
            let _permit = permit; // held for the lifetime of the poll → frees the slot on completion/expiry
            match poll_to_completion(&trakt, &store, device_code, interval, expires_in).await {
                Ok(slug) => info!("Trakt enrolment completed for '{}'", slug),
                Err(e) => warn!("Trakt enrolment did not complete: {}", e),
            }
        });
        html(StatusCode::OK, enrol_html(&dc))
    }

    /// Render the list of linked accounts.
    pub(crate) async fn accounts_page(&self) -> Response<Body> {
        let accounts = self.store.all_trakt_tokens().await;
        html(StatusCode::OK, accounts_html(&accounts))
    }

    /// Refresh one account's tokens. On success, rebuild + persist with the re-enrolment flag
    /// cleared. A transient failure (`trakt_client::refresh_error_is_transient`: a 429, a 5xx, a
    /// transport error, an undecodable answer) is logged and leaves the record alone. Any other
    /// failure sets the flag (compare-and-swap, only while the stored refresh token is still the
    /// one tried) so the accounts page surfaces it.
    pub(crate) async fn refresh_account(&self, slug: &str) -> Result<(), AppError> {
        let Some(tokens) = self.store.get_trakt_tokens(slug.to_string()).await else {
            return Ok(()); // nothing stored under this slug — no-op
        };
        let expected = tokens.refresh.clone();
        let outcome = self.trakt.refresh(&tokens.refresh).await;
        // Apply the result via a store-level compare-and-swap keyed on the refresh token we tried, so
        // the whole read → compare → write is atomic. This is NOT merely cosmetic: a plain re-read +
        // write races a concurrent refresh (a second /refresh POST, or the background sync) — if that
        // other refresh succeeds and rotates the single-use token AFTER our re-read but BEFORE our
        // write, we would clobber its fresh token with our stale one plus a spurious
        // `needs_reenrolment`, permanently breaking the account. The CAS writes only while the stored
        // refresh still equals `expected` (nobody rotated it), so the other refresh always wins.
        match outcome {
            Ok(fresh) => {
                self.store
                    .update_trakt_tokens_if_refresh_matches(
                        slug.to_string(),
                        expected,
                        move |current| {
                            current.access = fresh.access_token;
                            if !fresh.refresh_token.is_empty() {
                                current.refresh = fresh.refresh_token;
                            }
                            current.expires_at =
                                token_expires_at(fresh.created_at, fresh.expires_in);
                            current.needs_reenrolment = false;
                        },
                    )
                    .await?;
                Ok(())
            }
            Err(e) if crate::trakt_client::refresh_error_is_transient(&e) => {
                // A rate-limit (persistent 429), a 5xx or a transport failure is transient, NOT an
                // auth failure. Don't flag `needs_reenrolment`: it is an accounts-page marker
                // (background sync keeps syncing a flagged account and clears it on the next
                // success), and a spurious one sends the operator off to re-enrol a healthy
                // account. Leave the record intact. If Trakt did process the grant before the 5xx
                // and rotate the token, the next refresh gets a 4xx `invalid_grant` and flags then.
                info!(
                    "Trakt refresh for '{}' failed transiently ({}) — not flagging, will retry next cycle",
                    slug, e
                );
                Ok(())
            }
            Err(e) => {
                // Only flag re-enrolment if the stored refresh is STILL the one we failed against. If
                // a concurrent refresh already rotated it, Trakt invalidated our token on that other
                // success, so THIS failure is a false alarm — the CAS skips the write (returns false)
                // and we leave the good record alone. This prevents a spurious `needs_reenrolment`
                // marker telling the operator to re-enrol a healthy account.
                let flagged = self
                    .store
                    .update_trakt_tokens_if_refresh_matches(slug.to_string(), expected, |current| {
                        current.needs_reenrolment = true;
                    })
                    .await?;
                if flagged {
                    warn!("Trakt refresh failed for '{}': {}", slug, e);
                } else {
                    info!(
                        "Trakt refresh for '{}' failed on a since-rotated token (concurrent refresh) or a removed account — not flagging",
                        slug
                    );
                }
                Ok(())
            }
        }
    }

    /// Remove (unlink) one account's stored tokens AND its materialised `wanted` rows.
    ///
    /// Deleting only the tokens would orphan the account's `wanted` rows permanently: the sole
    /// pruner of `wanted` (`tasks::sync_trakt_user`) only ever processes accounts that still have
    /// tokens, so a removed slug's rows are never revisited. Left behind, they keep the reconciler
    /// acquiring the title under the removed user's (now frozen) provenance and perpetually satisfy
    /// `wants()`, so Trigger-B cleanup can never fire. Prune them here as part of the unlink.
    /// The store drops the token row and the wanted rows in one write transaction, so a failure
    /// part-way can't leave orphaned rows behind.
    pub(crate) async fn remove_account(&self, slug: &str) -> Result<(), AppError> {
        self.store.remove_trakt_account(slug.to_string()).await
    }
}

/// Poll the device-token endpoint until Authorized/terminal; on success fetch the user's slug
/// and persist the tokens. Returns the stored slug. Polling stops `expires_in_secs` after the call,
/// by the clock, so it never outlives the device code. That includes the time a `poll_token` spends
/// held up by the shared Trakt rate limiter (minutes per attempt under a 429 back-off): a poll
/// still waiting at the deadline is cut off, since a code past its expiry can't be authorised.
///
/// Also used directly by the interactive live integration test
/// (`tests/trakt_integration_test.rs`) to drive the device flow end-to-end — that test calls
/// this function after printing the verification URL so a human can approve it in-browser.
pub async fn poll_to_completion(
    trakt: &Arc<dyn TraktClient>,
    store: &Store,
    device_code: String,
    interval_secs: u64,
    expires_in_secs: u64,
) -> Result<String, AppError> {
    let interval = interval_secs.max(1);
    // `expires_in` is Trakt's number: an absurd one must not overflow the `Instant` and panic.
    let deadline = {
        let now = tokio::time::Instant::now();
        now.checked_add(std::time::Duration::from_secs(expires_in_secs))
            .unwrap_or_else(|| now + std::time::Duration::from_secs(u64::from(u32::MAX)))
    };
    let expired = || AppError::Config("Trakt device code expired before authorisation".to_string());
    // A transient poll error (dropped connection/timeout) must not abort the whole enrolment after a
    // single blip — retry it like `Pending`. Bounded by both the deadline AND a consecutive-error
    // cap, so a genuinely persistent error (e.g. a bad client id) still fails reasonably fast rather
    // than spinning until expiry. The counter resets on any successful poll.
    const MAX_CONSECUTIVE_POLL_ERRORS: u32 = 5;
    let mut consecutive_errors: u32 = 0;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let poll = match tokio::time::timeout(remaining, trakt.poll_token(&device_code)).await {
            Err(_) => return Err(expired()),
            Ok(Ok(p)) => {
                consecutive_errors = 0;
                p
            }
            Ok(Err(e)) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_POLL_ERRORS
                    || tokio::time::Instant::now() >= deadline
                {
                    return Err(e);
                }
                warn!(
                    "Trakt poll_token transient error {}/{} (retrying): {}",
                    consecutive_errors, MAX_CONSECUTIVE_POLL_ERRORS, e
                );
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                continue;
            }
        };
        match poll {
            DeviceTokenPoll::Authorized(tok) => {
                // The grant succeeded and Trakt has issued (and rotated to) this token pair. Retry
                // `me()` (the slug lookup) several times on the poll cadence before giving up, so a
                // brief Trakt hiccup right after approval (~20s at a 5s cadence) doesn't discard a
                // valid grant and force the operator to redo the whole device-flow approval.
                const MAX_ME_ATTEMPTS: u32 = 5;
                let mut last_err: Option<AppError> = None;
                for attempt in 1..=MAX_ME_ATTEMPTS {
                    match trakt.me(&tok.access_token).await {
                        // Neither a slug nor a username: storing under "" would leave an account
                        // the page can't refresh or remove (its `/trakt/accounts//…` URLs match no
                        // route). Treat the incoherent answer like a failed `me()` attempt.
                        Ok(user) if user.slug.is_empty() => {
                            warn!(
                                "Trakt me() after authorisation returned no slug or username \
                                 (attempt {}/{})",
                                attempt, MAX_ME_ATTEMPTS
                            );
                            last_err = Some(AppError::Upstream(
                                "Trakt /users/me returned no slug or username".to_string(),
                            ));
                            if attempt < MAX_ME_ATTEMPTS {
                                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                            }
                        }
                        Ok(user) => {
                            let tokens = TraktTokens {
                                access: tok.access_token,
                                refresh: tok.refresh_token,
                                expires_at: token_expires_at(tok.created_at, tok.expires_in),
                                username: user.username,
                                needs_reenrolment: false,
                            };
                            store.put_trakt_tokens(user.slug.clone(), tokens).await?;
                            return Ok(user.slug);
                        }
                        Err(e) => {
                            warn!(
                                "Trakt me() after authorisation failed (attempt {}/{}): {}",
                                attempt, MAX_ME_ATTEMPTS, e
                            );
                            last_err = Some(e);
                            if attempt < MAX_ME_ATTEMPTS {
                                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                            }
                        }
                    }
                }
                return Err(last_err.unwrap_or_else(|| {
                    AppError::Config("Trakt me() failed after authorisation".to_string())
                }));
            }
            DeviceTokenPoll::Pending => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(expired());
                }
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
            }
            DeviceTokenPoll::Denied => {
                return Err(AppError::Config("Trakt enrolment was denied".to_string()))
            }
            DeviceTokenPoll::Expired => {
                return Err(AppError::Config("Trakt device code expired".to_string()))
            }
            DeviceTokenPoll::Rejected(code) => {
                let hint = match code {
                    401 | 403 => " (check TRAKT_CLIENT_ID / TRAKT_CLIENT_SECRET)",
                    404 => " (device code invalid: start a new enrolment)",
                    409 => " (device code already used: start a new enrolment)",
                    _ => "",
                };
                return Err(AppError::Upstream(format!(
                    "Trakt device-token poll returned HTTP {code}{hint}"
                )));
            }
        }
    }
}

/// Compute a token's absolute expiry (epoch secs). Trakt anchors `expires_in` on `created_at`;
/// if Trakt ever omits `created_at` (parsed as `0` by `u64_field`), fall back to the wall clock
/// so a brand-new token is not stamped as having expired in 1970 — which would otherwise read as
/// permanently expired and drive perpetual refresh churn. `pub(crate)` so the background
/// `tasks::sync_trakt_user` refresh path shares the exact same fallback (not a divergent inline calc).
pub(crate) fn token_expires_at(created_at: u64, expires_in: u64) -> u64 {
    let anchor = if created_at == 0 {
        crate::now_unix_secs()
    } else {
        created_at
    };
    anchor.saturating_add(expires_in)
}

// --- Pure HTML builders ---------------------------------------------------------------------

/// Best-effort same-origin guard for the unauthenticated, state-changing POST routes. Modern
/// browsers attach `Sec-Fetch-Site` and `Origin` to form submissions, so a cross-origin drive-by
/// POST is detectable: reject when `Sec-Fetch-Site` is `cross-site`, or when `Origin`'s host:port
/// does not match the request `Host`. A request with neither header (curl, tests, very old
/// browsers) is treated as same-origin — acceptable under the trusted-LAN model.
fn is_cross_origin<B>(req: &hyper::Request<B>) -> bool {
    let headers = req.headers();
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        // Per the Fetch spec, Sec-Fetch-Site is only ever `same-origin`/`same-site`/`cross-site`/
        // `none`; `cross-site` is the drive-by case. (A `same-site` sibling-subdomain POST is still
        // caught below by the Origin↔Host mismatch, which browsers always attach to a POST.)
        if site.eq_ignore_ascii_case("cross-site") {
            return true;
        }
    }
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        // A PRESENT Origin must parse to a host[:port] that matches the Host header. An
        // unparseable/`null` Origin (sandboxed iframe, some redirect/privacy contexts — no
        // `scheme://host`) is NOT same-origin, so treat it as cross-origin rather than letting it
        // fall through as allowed. (A request with NO Origin header at all — curl/tests/old browsers
        // — is still treated as same-origin under the documented trusted-LAN model.)
        let host = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let same_origin = origin
            .split_once("://")
            .is_some_and(|(scheme, origin_host)| {
                !origin_host.is_empty()
                    && !host.is_empty()
                    // Normalise the default port for the scheme so a same-origin POST that carries the
                    // default port on one side but omits it on the other (e.g. a reverse proxy that
                    // forwards the browser's Host with an explicit `:443`) isn't falsely rejected as
                    // CSRF. Only the default port is evened out: a proxy that forwards another name,
                    // or drops a non-default port, fails the check, so it has to pass the browser's
                    // Host through.
                    && strip_default_port(scheme, origin_host)
                        .eq_ignore_ascii_case(strip_default_port(scheme, host))
            });
        if !same_origin {
            return true;
        }
    }
    false
}

/// Whether the enrolment routes answer for a request whose `Host` header is `host`: an IP literal,
/// `localhost` or a `*.localhost` name, a single-label name (`mediabox`, a docker service name), a name
/// under a suffix that public DNS never delegates (`.local`, `.lan`, `.internal`, `.home.arpa`), or
/// one of `extra` (lowercase, no port). A DNS-rebinding attack needs a name the attacker controls in
/// public DNS, which none of these are. Anything malformed (bad port, unclosed bracket, characters a
/// hostname can't contain) is refused.
fn host_is_allowed(host: &str, extra: &[String]) -> bool {
    let Some((name, is_ip_literal)) = hostname_of(host) else {
        return false;
    };
    if is_ip_literal || name.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    if !name.split('.').all(label_ok) {
        return false;
    }
    if extra.contains(&name) || !name.contains('.') {
        return true;
    }
    const LOCAL_SUFFIXES: [&str; 5] = ["localhost", "local", "lan", "internal", "home.arpa"];
    LOCAL_SUFFIXES.iter().any(|suffix| {
        name.strip_suffix(suffix)
            .is_some_and(|rest| rest.is_empty() || rest.ends_with('.'))
    })
}

/// Split a `Host` header value into its lowercase hostname (port, IPv6 brackets, an IPv6 zone id
/// and a trailing root dot removed) and whether it was a bracketed IPv6 literal. `None` when it is
/// malformed: empty, an unclosed bracket, or a port that isn't all digits.
fn hostname_of(host: &str) -> Option<(String, bool)> {
    let host = host.trim();
    let port_ok = |port: &str| port.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = host.strip_prefix('[') {
        let (addr, after) = rest.split_once(']')?;
        if !(after.is_empty() || after.strip_prefix(':').is_some_and(port_ok)) {
            return None;
        }
        // `[fe80::1%25eth0]`: the zone id is not part of the address.
        let addr = addr.split('%').next().unwrap_or(addr);
        return addr
            .parse::<std::net::Ipv6Addr>()
            .is_ok()
            .then(|| (addr.to_string(), true));
    }
    let name = match host.split_once(':') {
        Some((name, port)) if port_ok(port) => name,
        Some(_) => return None,
        None => host,
    };
    let name = name.strip_suffix('.').unwrap_or(name);
    (!name.is_empty()).then(|| (name.to_ascii_lowercase(), false))
}

/// Strip the DEFAULT port for `scheme` (`:80` for http, `:443` for https) from a `host[:port]`, so a
/// host with the explicit default port compares equal to the same host without it. A non-default port
/// (or a non-http(s) scheme) is left intact.
fn strip_default_port<'a>(scheme: &str, host_port: &'a str) -> &'a str {
    let default = if scheme.eq_ignore_ascii_case("http") {
        ":80"
    } else if scheme.eq_ignore_ascii_case("https") {
        ":443"
    } else {
        return host_port;
    };
    host_port.strip_suffix(default).unwrap_or(host_port)
}

/// HTML-escape `s` for safe use in element text and BOTH double- and single-quoted attributes.
/// Covers `&`, `<`, `>`, `"`, and `'` (`&` first to avoid double-escaping). Every attribute this
/// module renders is currently double-quoted, so `'` escaping is defence-in-depth against a future
/// single-quoted attribute becoming a break-out vector.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Percent-encode a single URL path segment so a slug can never break out of the
/// `/trakt/accounts/<slug>/…` route. Real Trakt slugs are `[a-z0-9_-]`, so this is a no-op for them;
/// it is defence-in-depth against a path-significant character (`/`, `?`, `#`, space, …) arriving in
/// a malformed slug. Encodes everything outside the RFC 3986 unreserved set — which ALSO neutralises
/// HTML metacharacters (`"`→`%22`, `<`→`%3C`, `&`→`%26`, …), so the encoded value is safe interpolated
/// directly inside the double-quoted `action="…"` attribute WITHOUT a separate `esc()`. `esc()` is
/// applied to the slug's TEXT rendering (`(slug: …)`), not to this action URL.
fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-decode a single URL path segment — the exact inverse of `encode_path_segment`, so a
/// slug rendered into the `/trakt/accounts/<slug>/…` action URL round-trips back to its stored
/// key. Without this, a username-fallback slug carrying a non-unreserved byte (a space, `.`
/// exempt — e.g. `"Bob Smith"` → encoded `"Bob%20Smith"`) would arrive at the refresh/remove
/// handler still percent-encoded and never match the token stored under the raw slug, silently
/// no-op-ing the button. A malformed `%` escape is passed through literally; the decoded bytes
/// are interpreted as UTF-8 (lossy) so a non-ASCII slug still round-trips.
fn decode_path_segment(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The enrolment instructions page (shows the device user_code + verification URL).
fn enrol_html(dc: &DeviceCode) -> String {
    // Defensive: only render a clickable link for an http(s) URL. The value is from Trakt's
    // authenticated API (always https://trakt.tv/activate), but escaping alone would not stop a
    // `javascript:`/`data:` href from executing on click if that ever changed — so a non-web
    // value is shown as escaped text only.
    let url = &dc.verification_url;
    let link = if url.starts_with("https://") || url.starts_with("http://") {
        format!("<a href=\"{u}\">{u}</a>", u = esc(url))
    } else {
        esc(url)
    };
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Link a Trakt account</title></head>\
         <body><h1>Link a Trakt account</h1>\
         <ol><li>Open {link}</li>\
         <li>Enter this code: <strong>{code}</strong></li>\
         <li>Approve the request on Trakt.</li></ol>\
         <p>This device finishes linking automatically once you approve it. \
         Then return to <a href=\"/trakt/accounts\">the accounts page</a>.</p></body></html>",
        link = link,
        code = esc(&dc.user_code),
    )
}

/// Shown next to a flagged account. The flag is set by a failed sync (any read failure, even a
/// transient one, or a refresh failure that `trakt_client::refresh_error_is_transient` doesn't
/// excuse) and cleared by the next successful one, so it says what to do if it persists — and
/// that Remove is the wrong fix: it drops the account's wanted rows, so the next Trakt cycle
/// deletes the content only it wanted.
const REENROL_HINT: &str = " <strong>&#9888; the last sync failed</strong> \
     (this clears by itself after the next successful sync). If it persists, click \
     <em>Enrol a new account</em> and approve as the same Trakt user: that replaces the \
     account's tokens in place. Do not Remove the account first.";

/// The accounts list page: each linked account with username + slug, a sync-failure marker (with
/// what to do about it) when flagged, per-account refresh/remove buttons (Remove leads to a
/// confirmation page), and a form to enrol a new account.
fn accounts_html(accounts: &[(String, TraktTokens)]) -> String {
    let mut rows = String::new();
    if accounts.is_empty() {
        rows.push_str("<p>No Trakt accounts are linked yet.</p>");
    } else {
        rows.push_str("<ul>");
        for (slug, tok) in accounts {
            let marker = if tok.needs_reenrolment {
                REENROL_HINT
            } else {
                ""
            };
            rows.push_str(&format!(
                "<li><strong>{user}</strong> (slug: {slug}){marker} \
                 <form method=\"post\" action=\"/trakt/accounts/{slug_path}/refresh\" style=\"display:inline\">\
                 <button type=\"submit\">Refresh</button></form> \
                 <form method=\"get\" action=\"/trakt/accounts/{slug_path}/remove\" style=\"display:inline\">\
                 <button type=\"submit\">Remove</button></form></li>",
                user = esc(&tok.username),
                slug = esc(slug),
                slug_path = encode_path_segment(slug),
                marker = marker,
            ));
        }
        rows.push_str("</ul>");
    }
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Trakt accounts</title></head>\
         <body><h1>Trakt accounts</h1>{rows}\
         <hr><form method=\"post\" action=\"/trakt/enrol\">\
         <button type=\"submit\">Enrol a new account</button></form></body></html>",
        rows = rows,
    )
}

/// The confirmation page the accounts page's Remove button leads to. Removing is destructive
/// beyond the tokens: the account's wanted rows go with it, and the next Trakt sync then deletes
/// the content its watchlist acquired that no other linked account wants.
fn confirm_remove_html(slug: &str, username: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Remove Trakt account</title></head>\
         <body><h1>Remove {user} (slug: {slug})?</h1>\
         <p>On the next Trakt sync, content acquired from this account's watchlist that no other \
         linked account wants will be deleted. Removing the last linked account deletes all \
         watchlist-acquired content.</p>\
         <p>To re-authorise this account instead, go back and click <em>Enrol a new account</em>, \
         then approve as the same Trakt user: that replaces its tokens in place and deletes \
         nothing.</p>\
         <form method=\"post\" action=\"/trakt/accounts/{slug_path}/remove\">\
         <button type=\"submit\">Remove the account</button></form>\
         <p><a href=\"/trakt/accounts\">Cancel</a></p></body></html>",
        user = esc(username),
        slug = esc(slug),
        slug_path = encode_path_segment(slug),
    )
}

// --- Response helpers -----------------------------------------------------------------------

/// Build an HTML response of the WebDAV body type. Output is already `esc`/`encode_path_segment`-safe;
/// the hardening headers are defence-in-depth: `nosniff` stops a browser MIME-sniffing the page as
/// something executable, and a `default-src 'none'` CSP (the pages use no scripts) neutralises any
/// future escaping regression on these unauthenticated LAN pages. `frame-ancestors 'none'` (it
/// does not fall back to `default-src`) plus `X-Frame-Options: DENY` for browsers without CSP 2
/// refuse framing: a page on another site could otherwise overlay a framed Remove confirmation
/// with its own button, and the click would submit a same-origin POST the CSRF guard lets through.
fn html(status: StatusCode, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(hyper::header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            hyper::header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
        )
        .header(hyper::header::X_FRAME_OPTIONS, "DENY")
        .body(Body::from(body))
        .expect("building a static HTML response cannot fail")
}

/// Build a 303 redirect (POST-redirect-GET) to `location`.
fn redirect(location: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(hyper::header::LOCATION, location)
        .body(Body::from(String::new()))
        .expect("building a static redirect response cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trakt_client::{MockTrakt, TraktTokenResponse, TraktUser};

    fn tokens_fixture(access: &str, username: &str) -> TraktTokens {
        TraktTokens {
            access: access.to_string(),
            refresh: "RT".to_string(),
            expires_at: 9_999_999_999,
            username: username.to_string(),
            needs_reenrolment: false,
        }
    }

    #[tokio::test]
    async fn poll_to_completion_stores_tokens_on_authorized() {
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Authorized(TraktTokenResponse {
                access_token: "AT".into(),
                refresh_token: "RT".into(),
                expires_in: 50,
                created_at: 1_000,
            }),
            user: TraktUser {
                slug: "alice".into(),
                username: "Alice".into(),
            },
            ..Default::default()
        });
        let store = Store::in_memory();

        // Authorized on the first poll → no real sleep occurs.
        let slug = poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .unwrap();
        assert_eq!(slug, "alice");

        let tok = store
            .get_trakt_tokens("alice".into())
            .await
            .expect("tokens stored");
        assert_eq!(tok.access, "AT");
        assert_eq!(tok.refresh, "RT");
        assert_eq!(tok.username, "Alice");
        assert!(!tok.needs_reenrolment);
        assert_eq!(tok.expires_at, 1_050); // created_at + expires_in
    }

    #[tokio::test(start_paused = true)]
    async fn poll_to_completion_retries_transient_poll_errors() {
        // Two transient poll errors then Authorized → enrolment must succeed, not abort on the blip.
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Authorized(TraktTokenResponse {
                access_token: "AT".into(),
                refresh_token: "RT".into(),
                expires_in: 50,
                created_at: 1_000,
            }),
            user: TraktUser {
                slug: "bob".into(),
                username: "Bob".into(),
            },
            poll_errors_before_ok: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(2)),
            ..Default::default()
        });
        let store = Store::in_memory();
        let slug = poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .unwrap();
        assert_eq!(slug, "bob");
        assert!(store.get_trakt_tokens("bob".into()).await.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn poll_to_completion_gives_up_after_too_many_consecutive_errors() {
        // Persistent poll errors (more than the consecutive cap) must fail rather than spin to expiry.
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll_errors_before_ok: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(100)),
            ..Default::default()
        });
        let store = Store::in_memory();
        assert!(poll_to_completion(&trakt, &store, "dc".into(), 1, 300)
            .await
            .is_err());
        assert!(store.all_trakt_tokens().await.is_empty());
    }

    /// A transient `me()` failure right after Trakt authorised the device code must not throw away
    /// the grant the operator just approved: `me()` is retried and the tokens are stored once it
    /// answers.
    #[tokio::test(start_paused = true)]
    async fn poll_to_completion_retries_a_transient_me_failure_and_keeps_the_grant() {
        let me_errors = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(2));
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Authorized(TraktTokenResponse {
                access_token: "AT".into(),
                refresh_token: "RT".into(),
                expires_in: 50,
                created_at: 1_000,
            }),
            user: TraktUser {
                slug: "carol".into(),
                username: "Carol".into(),
            },
            me_errors_before_ok: me_errors.clone(),
            ..Default::default()
        });
        let store = Store::in_memory();
        let slug = poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .expect("a transient me() failure is retried");
        assert_eq!(slug, "carol");
        assert_eq!(me_errors.load(std::sync::atomic::Ordering::SeqCst), 0);
        let tok = store
            .get_trakt_tokens("carol".into())
            .await
            .expect("tokens stored");
        assert_eq!(tok.access, "AT");
        assert_eq!(tok.refresh, "RT");
    }

    /// A `me()` that keeps failing after authorisation gives up after its bounded retries (all of
    /// them used, not abandoned on the first error) and stores nothing.
    #[tokio::test(start_paused = true)]
    async fn poll_to_completion_gives_up_when_me_keeps_failing_and_stores_nothing() {
        // MAX_ME_ATTEMPTS (5) failures: every attempt fails.
        let me_errors = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(5));
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Authorized(TraktTokenResponse {
                access_token: "AT".into(),
                ..Default::default()
            }),
            user: TraktUser {
                slug: "carol".into(),
                username: "Carol".into(),
            },
            me_errors_before_ok: me_errors.clone(),
            ..Default::default()
        });
        let store = Store::in_memory();
        assert!(poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .is_err());
        assert_eq!(
            me_errors.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "every me() attempt was made before giving up"
        );
        assert!(store.all_trakt_tokens().await.is_empty());
    }

    /// A `/users/me` answer with neither a slug nor a username would store the tokens under "",
    /// whose Refresh/Remove URLs (`/trakt/accounts//…`) the router can't match. Treat it like a
    /// failed `me()`: retry, then fail without storing anything.
    #[tokio::test(start_paused = true)]
    async fn poll_to_completion_rejects_an_empty_slug() {
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Authorized(TraktTokenResponse {
                access_token: "AT".into(),
                ..Default::default()
            }),
            user: TraktUser::default(),
            ..Default::default()
        });
        let store = Store::in_memory();
        assert!(poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .is_err());
        assert!(store.all_trakt_tokens().await.is_empty());
    }

    #[tokio::test]
    async fn poll_to_completion_expired_is_err_and_stores_nothing() {
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Expired,
            ..Default::default()
        });
        let store = Store::in_memory();
        assert!(poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .is_err());
        assert!(store.all_trakt_tokens().await.is_empty());
    }

    /// A 401 on the token poll (a wrong client secret) is reported with its status and a hint,
    /// not as an expired code.
    #[tokio::test]
    async fn poll_to_completion_rejected_names_the_status_and_stores_nothing() {
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Rejected(401),
            ..Default::default()
        });
        let store = Store::in_memory();
        let err = poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
        assert!(err.contains("TRAKT_CLIENT_SECRET"), "{err}");
        assert!(!err.contains("expired"), "{err}");
        assert!(store.all_trakt_tokens().await.is_empty());
    }

    /// A 404 (invalid device code) or 409 (code already used: e.g. the re-poll after a 200 whose
    /// token body failed to read) names what happened and says to start over, never "expired".
    #[tokio::test]
    async fn poll_to_completion_invalid_or_used_code_says_so_and_stores_nothing() {
        for (code, what) in [(404, "invalid"), (409, "already used")] {
            let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
                poll: DeviceTokenPoll::Rejected(code),
                ..Default::default()
            });
            let store = Store::in_memory();
            let err = poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(&code.to_string()), "{err}");
            assert!(err.contains(what), "{err}");
            assert!(err.contains("start a new enrolment"), "{err}");
            assert!(!err.contains("expired"), "{err}");
            assert!(!err.contains("TRAKT_CLIENT_SECRET"), "{err}");
            assert!(store.all_trakt_tokens().await.is_empty());
        }
    }

    /// `expires_in` comes from Trakt: an absurd value must not overflow the poll's deadline.
    #[tokio::test]
    async fn poll_to_completion_takes_an_absurd_expires_in_without_panicking() {
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Denied,
            ..Default::default()
        });
        let err = poll_to_completion(&trakt, &Store::in_memory(), "dc".into(), 1, u64::MAX)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("denied"), "{err}");
    }

    #[tokio::test]
    async fn poll_to_completion_denied_is_err_and_stores_nothing() {
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Denied,
            ..Default::default()
        });
        let store = Store::in_memory();
        assert!(poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .is_err());
        assert!(store.all_trakt_tokens().await.is_empty());
    }

    #[tokio::test]
    async fn start_enrolment_caps_concurrent_polls_to_the_bound() {
        let store = Store::in_memory();
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), store);
        // Exhaust the pool: hold all MAX_CONCURRENT_ENROLMENTS permits to simulate that many
        // enrolments already in flight.
        let held: Vec<_> = (0..MAX_CONCURRENT_ENROLMENTS)
            .map(|_| {
                svc.inflight
                    .clone()
                    .try_acquire_owned()
                    .expect("permit available up to the bound")
            })
            .collect();
        let refused = svc.start_enrolment().await;
        assert_eq!(
            refused.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "an enrolment beyond the bound must be refused, not spawn another poll task"
        );
        // Once an in-flight enrolment ends, a slot frees and a new enrolment is admitted again.
        drop(held);
        let admitted = svc.start_enrolment().await;
        assert_ne!(
            admitted.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "a slot must free after an in-flight enrolment ends"
        );
    }

    #[tokio::test]
    async fn remove_account_deletes_only_that_slug() {
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("AT", "Alice"))
            .await
            .unwrap();
        store
            .put_trakt_tokens("bob".into(), tokens_fixture("BT", "Bob"))
            .await
            .unwrap();
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), store.clone());

        svc.remove_account("alice").await.unwrap();

        assert!(store.get_trakt_tokens("alice".into()).await.is_none());
        assert!(store.get_trakt_tokens("bob".into()).await.is_some());
    }

    #[tokio::test]
    async fn remove_account_also_deletes_that_users_wanted_rows() {
        // Removing an account must not leave its `wanted` rows behind: the only pruner of `wanted`
        // (`sync_trakt_user`) iterates accounts that still have tokens, so an orphaned row would
        // otherwise keep the title acquired forever (and block Trigger-B cleanup) — nobody would
        // ever process the removed slug again.
        use crate::store::{WantedRecord, WantedSources, WatchedState};
        use crate::vfs::MediaType;
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("AT", "Alice"))
            .await
            .unwrap();
        store
            .put_trakt_tokens("bob".into(), tokens_fixture("BT", "Bob"))
            .await
            .unwrap();
        let wanted = |user: &str, tmdb_id: u64| WantedRecord {
            user: user.to_string(),
            tmdb_id,
            media_type: MediaType::Movie,
            sources: WantedSources {
                watchlist: true,
                in_progress: false,
            },
            watched_state: WatchedState::Movie { watched: false },
            show_status: None,
            imdb_id: None,
        };
        store.put_wanted(wanted("alice", 11)).await.unwrap();
        store.put_wanted(wanted("alice", 22)).await.unwrap();
        store.put_wanted(wanted("bob", 33)).await.unwrap();
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), store.clone());

        svc.remove_account("alice").await.unwrap();

        let remaining: Vec<_> = store.all_wanted().await;
        assert!(
            remaining.iter().all(|r| r.user != "alice"),
            "alice's wanted rows must be deleted with her account, got {remaining:?}"
        );
        assert!(
            remaining.iter().any(|r| r.user == "bob" && r.tmdb_id == 33),
            "bob's wanted rows must be untouched"
        );
    }

    #[tokio::test]
    async fn refresh_account_updates_tokens_and_clears_flag() {
        let store = Store::in_memory();
        let mut stale = tokens_fixture("OLD", "Alice");
        stale.needs_reenrolment = true;
        store.put_trakt_tokens("alice".into(), stale).await.unwrap();
        let trakt = Arc::new(MockTrakt {
            token: TraktTokenResponse {
                access_token: "FRESH".into(),
                refresh_token: "NEWREF".into(),
                expires_in: 100,
                created_at: 2_000,
            },
            ..Default::default()
        });
        let svc = EnrolmentService::new(trakt, store.clone());

        svc.refresh_account("alice").await.unwrap();

        let tok = store.get_trakt_tokens("alice".into()).await.unwrap();
        assert_eq!(tok.access, "FRESH");
        assert_eq!(tok.refresh, "NEWREF");
        assert_eq!(tok.expires_at, 2_100);
        assert!(!tok.needs_reenrolment);
    }

    #[tokio::test]
    async fn refresh_account_flags_on_error() {
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("OLD", "Alice"))
            .await
            .unwrap();
        let trakt = Arc::new(MockTrakt {
            fail_refresh: true,
            ..Default::default()
        });
        let svc = EnrolmentService::new(trakt, store.clone());

        svc.refresh_account("alice").await.unwrap();

        let tok = store.get_trakt_tokens("alice".into()).await.unwrap();
        assert!(
            tok.needs_reenrolment,
            "a refresh failure must flag the account"
        );
        assert_eq!(tok.refresh, "RT", "the refresh token must be preserved");
    }

    /// A 5xx (or transport failure) on refresh is transient: the account is not flagged.
    #[tokio::test]
    async fn refresh_account_does_not_flag_a_transient_failure() {
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("OLD", "Alice"))
            .await
            .unwrap();
        let trakt = Arc::new(MockTrakt {
            transient_refresh: true,
            ..Default::default()
        });
        let svc = EnrolmentService::new(trakt, store.clone());

        svc.refresh_account("alice").await.unwrap();

        let tok = store.get_trakt_tokens("alice".into()).await.unwrap();
        assert!(!tok.needs_reenrolment, "a 503 must not flag the account");
        assert_eq!(tok.refresh, "RT", "the record is left intact");
    }

    #[tokio::test]
    async fn refresh_account_does_not_resurrect_concurrently_removed_account() {
        use crate::trakt_client::{
            DeviceCode, DeviceTokenPoll, ShowProgress, TraktItem, WatchedData,
        };
        // A TraktClient whose `refresh` removes the account mid-call — deterministically simulating a
        // concurrent `/remove` (another connection, or the background sync) during the refresh
        // network window. refresh_account must NOT resurrect the removed account.
        struct RemovingTrakt {
            store: Store,
        }
        #[async_trait::async_trait]
        impl TraktClient for RemovingTrakt {
            async fn refresh(&self, _rt: &str) -> Result<TraktTokenResponse, AppError> {
                self.store
                    .remove_trakt_tokens("alice".into())
                    .await
                    .unwrap();
                Ok(TraktTokenResponse {
                    access_token: "FRESH".into(),
                    refresh_token: "NEWREF".into(),
                    expires_in: 100,
                    created_at: 2_000,
                })
            }
            async fn device_code(&self) -> Result<DeviceCode, AppError> {
                unreachable!()
            }
            async fn poll_token(&self, _d: &str) -> Result<DeviceTokenPoll, AppError> {
                unreachable!()
            }
            async fn me(&self, _a: &str) -> Result<TraktUser, AppError> {
                unreachable!()
            }
            async fn watchlist(&self, _a: &str) -> Result<Vec<TraktItem>, AppError> {
                unreachable!()
            }
            async fn in_progress(&self, _a: &str) -> Result<Vec<TraktItem>, AppError> {
                unreachable!()
            }
            async fn watched(&self, _a: &str) -> Result<WatchedData, AppError> {
                unreachable!()
            }
            async fn show_progress(&self, _a: &str, _t: u64) -> Result<ShowProgress, AppError> {
                unreachable!()
            }
        }
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("OLD", "Alice"))
            .await
            .unwrap();
        let trakt: Arc<dyn TraktClient> = Arc::new(RemovingTrakt {
            store: store.clone(),
        });
        let svc = EnrolmentService::new(trakt, store.clone());

        svc.refresh_account("alice").await.unwrap();

        assert!(
            store.get_trakt_tokens("alice".into()).await.is_none(),
            "an account removed during the refresh window must NOT be resurrected"
        );
    }

    #[test]
    fn enrol_html_contains_code_and_url() {
        let dc = DeviceCode {
            device_code: "dc".into(),
            user_code: "ABCD-1234".into(),
            verification_url: "https://trakt.tv/activate".into(),
            interval: 5,
            expires_in: 600,
        };
        let html = enrol_html(&dc);
        assert!(html.contains("ABCD-1234"), "user_code must appear");
        assert!(
            html.contains("https://trakt.tv/activate"),
            "verification_url must appear"
        );
    }

    #[tokio::test]
    async fn start_enrolment_returns_code_and_url_from_device_code() {
        let dc = DeviceCode {
            device_code: "dc".into(),
            user_code: "WXYZ-9999".into(),
            verification_url: "https://trakt.tv/activate".into(),
            interval: 5,
            expires_in: 600,
        };
        let trakt = Arc::new(MockTrakt {
            device_code: dc.clone(),
            // Authorized so the spawned background poll completes immediately (no sleep / no hang).
            poll: DeviceTokenPoll::Authorized(TraktTokenResponse {
                access_token: "AT".into(),
                ..Default::default()
            }),
            user: TraktUser {
                slug: "alice".into(),
                username: "Alice".into(),
            },
            ..Default::default()
        });
        let svc = EnrolmentService::new(trakt, Store::in_memory());

        let resp = svc.start_enrolment().await;
        assert_eq!(resp.status(), StatusCode::OK);
        let page = body_text(resp).await;
        assert!(page.contains("WXYZ-9999"), "{page}");
        assert!(page.contains("https://trakt.tv/activate"), "{page}");
    }

    #[tokio::test]
    async fn accounts_html_lists_accounts_and_marks_reenrolment() {
        let mut flagged = tokens_fixture("BT", "Bob");
        flagged.needs_reenrolment = true;
        let accounts = vec![
            ("alice".to_string(), tokens_fixture("AT", "Alice")),
            ("bob".to_string(), flagged),
        ];
        let page = accounts_html(&accounts);
        assert!(page.contains("Alice"));
        assert!(page.contains("alice"));
        assert!(page.contains("Bob"));
        assert!(page.contains("the last sync failed"));
        assert!(
            page.contains("Do not Remove the account first"),
            "the flagged account carries the re-enrol guidance"
        );
        assert_eq!(
            page.matches("the last sync failed").count(),
            1,
            "only the flagged account is marked"
        );
        assert!(page.contains("/trakt/accounts/alice/refresh"));
        assert!(page.contains("/trakt/accounts/bob/remove"));
        assert!(
            page.contains("<form method=\"get\" action=\"/trakt/accounts/bob/remove\""),
            "Remove leads to the confirmation page instead of removing"
        );
        assert!(!page.contains("method=\"post\" action=\"/trakt/accounts/bob/remove\""));
        assert!(page.contains("/trakt/enrol"));
    }

    async fn body_text(r: Response<Body>) -> String {
        use futures_util::StreamExt;
        let mut body = r.into_body();
        let mut out = Vec::new();
        while let Some(chunk) = body.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    /// GET .../remove only renders the confirmation page: it changes nothing, warns what removal
    /// deletes, and carries the POST form that does remove. An unknown slug is a 404.
    #[tokio::test]
    async fn remove_confirmation_page_warns_and_changes_nothing() {
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("AT", "Alice"))
            .await
            .unwrap();
        store
            .put_wanted(crate::store::WantedRecord {
                user: "alice".into(),
                tmdb_id: 33,
                media_type: crate::vfs::MediaType::Movie,
                sources: crate::store::WantedSources {
                    watchlist: true,
                    in_progress: false,
                },
                watched_state: crate::store::WatchedState::Movie { watched: false },
                show_status: None,
                imdb_id: None,
            })
            .await
            .unwrap();
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), store.clone());

        let r = svc
            .handle(routed_req(
                Method::GET,
                "http://host.local/trakt/accounts/alice/remove",
                true,
            ))
            .await;
        assert_eq!(r.status(), StatusCode::OK);
        let page = body_text(r).await;
        assert!(page.contains("will be deleted"), "{page}");
        assert!(page.contains("Removing the last linked account"), "{page}");
        assert!(page.contains("Enrol a new account"), "{page}");
        assert!(
            page.contains("<form method=\"post\" action=\"/trakt/accounts/alice/remove\">"),
            "{page}"
        );
        assert!(store.get_trakt_tokens("alice".into()).await.is_some());
        assert_eq!(
            store.all_wanted().await.len(),
            1,
            "the confirmation page must not drop wanted rows"
        );

        let r = svc
            .handle(routed_req(
                Method::GET,
                "http://host.local/trakt/accounts/nobody/remove",
                true,
            ))
            .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    /// Every enrolment page refuses to be framed, so a page on another site can't overlay its own
    /// bait button on a framed Remove confirmation and have the click submit it (clickjacking): a
    /// submit from inside the frame is same-origin, so the CSRF guard alone would let it through.
    #[tokio::test]
    async fn enrolment_pages_forbid_framing() {
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("AT", "Alice"))
            .await
            .unwrap();
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), store);
        for uri in [
            "http://host.local/trakt/accounts",
            "http://host.local/trakt/accounts/alice/remove",
        ] {
            let r = svc.handle(routed_req(Method::GET, uri, true)).await;
            assert_eq!(r.status(), StatusCode::OK, "{uri}");
            let csp = r
                .headers()
                .get(hyper::header::CONTENT_SECURITY_POLICY)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            assert!(csp.contains("frame-ancestors 'none'"), "{uri}: {csp}");
            assert_eq!(
                r.headers()
                    .get(hyper::header::X_FRAME_OPTIONS)
                    .and_then(|v| v.to_str().ok()),
                Some("DENY"),
                "{uri}"
            );
        }
    }

    #[test]
    fn confirm_remove_html_escapes_and_encodes_the_slug() {
        let page = confirm_remove_html("a\" <x", "<script>alert(1)</script>");
        assert!(!page.contains("<script>alert(1)</script>"));
        assert!(page.contains("&lt;script&gt;"));
        assert!(page.contains("/trakt/accounts/a%22%20%3Cx/remove"));
        assert!(!page.contains("a\" <x"));
    }

    // ── security: HTML-escaping (XSS) ─────────────────────────────────────────

    #[test]
    fn esc_neutralises_html_metacharacters() {
        assert_eq!(
            esc(r#"<script>alert("1")&'</script>"#),
            "&lt;script&gt;alert(&quot;1&quot;)&amp;&#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn encode_path_segment_passes_real_slugs_and_encodes_path_specials() {
        // Typical Trakt slugs use only unreserved characters → unchanged (the route still matches).
        assert_eq!(encode_path_segment("jane-doe"), "jane-doe");
        assert_eq!(encode_path_segment("user_123.x~y"), "user_123.x~y");
        // A path-significant character can't break out of the /trakt/accounts/<slug>/… route.
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
        assert_eq!(encode_path_segment("a b?c#d"), "a%20b%3Fc%23d");
    }

    #[test]
    fn token_expires_at_falls_back_to_now_when_created_at_missing() {
        // Normal path: expiry is anchored on Trakt's created_at.
        assert_eq!(token_expires_at(1_000_000, 7_776_000), 8_776_000);
        // created_at == 0 (Trakt omitted the field) must NOT stamp expiry near the 1970 epoch —
        // it anchors on the wall clock, so the token stays valid for ~expires_in from now.
        let now = crate::now_unix_secs();
        let got = token_expires_at(0, 7_776_000);
        assert!(
            got >= now + 7_776_000 && got <= now + 7_776_000 + 5,
            "expected ~now+expires_in, got {got} (now={now})"
        );
    }

    #[test]
    fn decode_path_segment_inverts_encode() {
        // Every slug the router might receive must round-trip back to its stored key, otherwise
        // the refresh/remove buttons silently no-op for a fallback slug (see the space case).
        for slug in [
            "jane-doe",
            "user_123.x~y",
            "Bob Smith",
            "a/b",
            "a b?c#d",
            "naïve.usér",
            "100%done",
        ] {
            assert_eq!(decode_path_segment(&encode_path_segment(slug)), slug);
        }
        // A malformed `%` escape is passed through literally rather than dropped or panicking.
        assert_eq!(decode_path_segment("ab%"), "ab%");
        assert_eq!(decode_path_segment("ab%4"), "ab%4");
        assert_eq!(decode_path_segment("ab%zz"), "ab%zz");
    }

    #[test]
    fn accounts_html_escapes_account_fields() {
        // A hostile username/slug (e.g. from a compromised Trakt response) must not break out of
        // the HTML — assert the raw markup never appears and the escaped form does.
        let accounts = vec![(
            "ev<il".to_string(),
            tokens_fixture("AT", "<script>alert(1)</script>"),
        )];
        let page = accounts_html(&accounts);
        assert!(!page.contains("<script>alert(1)</script>"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(page.contains("ev&lt;il"));
    }

    #[test]
    fn accounts_html_action_url_is_breakout_safe() {
        // The slug is percent-encoded (NOT merely esc'd) inside the double-quoted action="…" URL, so
        // a break-out attempt (`"`, space, `<`) is neutralised as `%22`/`%20`/`%3C` rather than
        // injecting an attribute. Pins that encode_path_segment (not esc) is what makes the URL safe.
        let accounts = vec![(
            "a\" onmouseover=alert(1) <x".to_string(),
            tokens_fixture("AT", "u"),
        )];
        let page = accounts_html(&accounts);
        assert!(
            page.contains("%22"),
            "the double-quote must be percent-encoded in the action URL"
        );
        assert!(
            page.contains("%20"),
            "spaces must be percent-encoded in the action URL"
        );
        assert!(
            page.contains("%3C") || page.contains("%3c"),
            "`<` must be percent-encoded"
        );
        // The raw break-out sequence must appear nowhere (the text rendering esc's the quote to &quot;).
        assert!(!page.contains("a\" onmouseover"));
    }

    #[test]
    fn enrol_html_escapes_and_guards_non_web_verification_url() {
        let dc = DeviceCode {
            device_code: "dc".into(),
            user_code: "<b>X</b>".into(),
            verification_url: "javascript:alert(1)".into(),
            interval: 5,
            expires_in: 600,
        };
        let html = enrol_html(&dc);
        // The user_code is escaped...
        assert!(html.contains("&lt;b&gt;X&lt;/b&gt;"));
        // ...and a non-http(s) verification URL is NEVER rendered as a clickable href (it is shown
        // as escaped text only, so the `javascript:` scheme cannot execute on click).
        assert!(!html.contains("href=\"javascript:"));
        assert!(html.contains("javascript:alert(1)")); // present, but as plain text
    }

    // ── security: CSRF same-origin guard ──────────────────────────────────────

    fn post_req(headers: &[(&str, &str)]) -> hyper::Request<()> {
        let mut b = hyper::Request::builder()
            .method(Method::POST)
            .uri("http://host.local/trakt/enrol");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(()).unwrap()
    }

    #[test]
    fn cross_origin_post_is_rejected() {
        // Sec-Fetch-Site: cross-site → cross-origin.
        let req = post_req(&[("host", "host.local"), ("sec-fetch-site", "cross-site")]);
        assert!(is_cross_origin(&req));
        // Mismatched Origin host → cross-origin.
        let req = post_req(&[("host", "host.local"), ("origin", "https://evil.example")]);
        assert!(is_cross_origin(&req));
        // A present-but-unparseable `Origin: null` (sandboxed iframe / privacy context) is NOT
        // same-origin → must be treated as cross-origin, not allowed to fall through.
        let req = post_req(&[("host", "host.local"), ("origin", "null")]);
        assert!(is_cross_origin(&req));
    }

    #[test]
    fn same_origin_and_headerless_post_allowed() {
        // Same Origin host:port as Host → same-origin.
        let req = post_req(&[("host", "host.local"), ("origin", "http://host.local")]);
        assert!(!is_cross_origin(&req));
        // Sec-Fetch-Site: same-origin → allowed.
        let req = post_req(&[("host", "host.local"), ("sec-fetch-site", "same-origin")]);
        assert!(!is_cross_origin(&req));
        // No CSRF-relevant headers (curl/tests) → allowed under the trusted-LAN model.
        let req = post_req(&[("host", "host.local")]);
        assert!(!is_cross_origin(&req));
    }

    #[test]
    fn same_origin_default_port_normalisation() {
        // A same-origin POST where one side carries the scheme's DEFAULT port and the other omits it
        // (e.g. a reverse proxy rewriting Host but not Origin) must NOT be rejected as cross-origin.
        let req = post_req(&[("host", "host.local"), ("origin", "http://host.local:80")]);
        assert!(!is_cross_origin(&req));
        let req = post_req(&[("host", "host.local:80"), ("origin", "http://host.local")]);
        assert!(!is_cross_origin(&req));
        let req = post_req(&[("host", "host.local"), ("origin", "https://host.local:443")]);
        assert!(!is_cross_origin(&req));
        // A NON-default port that genuinely differs is still cross-origin.
        let req = post_req(&[("host", "host.local"), ("origin", "http://host.local:8443")]);
        assert!(is_cross_origin(&req));
        // The http default (:80) must not be stripped from an https origin (scheme mismatch case).
        let req = post_req(&[("host", "host.local"), ("origin", "https://host.local:80")]);
        assert!(is_cross_origin(&req));
    }

    #[test]
    fn reverse_proxy_must_forward_the_browsers_host() {
        // The guard compares the browser's Origin with the Host the service receives, so a reverse
        // proxy has to pass the browser's Host through unchanged, port included (README: nginx
        // `proxy_set_header Host $http_host;`).
        let origin = "https://media.example.com";
        // nginx's default (`$proxy_host`) sends the upstream's own name: every POST is refused.
        let req = post_req(&[("host", "debridmoviemapper:8080"), ("origin", origin)]);
        assert!(is_cross_origin(&req));
        // `$host` drops a non-default port that the Origin carries: refused too.
        let req = post_req(&[
            ("host", "media.example.com"),
            ("origin", "https://media.example.com:8443"),
        ]);
        assert!(is_cross_origin(&req));
        // The browser's Host forwarded unchanged passes, with or without the default port.
        for host in ["media.example.com", "media.example.com:443"] {
            let req = post_req(&[("host", host), ("origin", origin)]);
            assert!(!is_cross_origin(&req), "Host {host} must pass");
        }
    }

    #[tokio::test]
    async fn csrf_rejection_log_names_origin_host_and_the_proxy_hint() {
        // Behind a reverse proxy that rewrites Host, the 403 is otherwise opaque: the warn line has
        // to show what the service compared, so the operator can see the proxy changed the Host.
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), Store::in_memory());
        let req = hyper::Request::builder()
            .method(Method::POST)
            .uri("/trakt/enrol")
            .header("host", "debridmoviemapper:8080")
            .header("origin", "https://media.example.com")
            .header("sec-fetch-site", "same-origin")
            .body(())
            .unwrap();
        let (r, logs) = crate::test_log::capture_async(tracing::Level::WARN, svc.handle(req)).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        for want in [
            "\"https://media.example.com\"",
            "\"debridmoviemapper:8080\"",
            "\"same-origin\"",
            "forward the browser's Host header unchanged",
        ] {
            assert!(logs.contains(want), "missing {want:?} in: {logs}");
        }
    }

    fn routed_req(method: Method, uri: &str, same_origin: bool) -> hyper::Request<()> {
        let origin = if same_origin {
            "http://host.local"
        } else {
            "https://evil.example"
        };
        hyper::Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "host.local")
            .header("origin", origin)
            .body(())
            .unwrap()
    }

    #[tokio::test]
    async fn handle_routes_and_enforces_csrf_and_decodes_slug() {
        // Exercises the router end-to-end: the CSRF guard precedes routing, each method+path arm
        // dispatches, an unknown path 404s, and a percent-encoded slug is decoded before dispatch.
        let store = Store::in_memory();
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), store.clone());

        // Cross-origin POST → 403 (guard fires before the route match).
        let r = svc
            .handle(routed_req(
                Method::POST,
                "http://host.local/trakt/enrol",
                false,
            ))
            .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Same-origin POST /trakt/enrol → start_enrolment (MockTrakt device_code Ok → 200).
        let r = svc
            .handle(routed_req(
                Method::POST,
                "http://host.local/trakt/enrol",
                true,
            ))
            .await;
        assert_eq!(r.status(), StatusCode::OK);

        // GET /trakt and /trakt/accounts → the accounts page.
        for uri in [
            "http://host.local/trakt",
            "http://host.local/trakt/accounts",
        ] {
            let r = svc.handle(routed_req(Method::GET, uri, true)).await;
            assert_eq!(r.status(), StatusCode::OK);
        }

        // Unknown /trakt path → 404.
        let r = svc
            .handle(routed_req(
                Method::GET,
                "http://host.local/trakt/nope",
                true,
            ))
            .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        // A percent-encoded slug is decoded before dispatch: remove the account keyed "a b".
        store
            .put_trakt_tokens("a b".into(), tokens_fixture("AT", "Alice"))
            .await
            .unwrap();
        let r = svc
            .handle(routed_req(
                Method::POST,
                "http://host.local/trakt/accounts/a%20b/remove",
                true,
            ))
            .await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert!(
            store.get_trakt_tokens("a b".into()).await.is_none(),
            "the percent-encoded slug must decode to 'a b' and reach remove_account"
        );
    }

    // ── security: Host allowlist (DNS-rebinding guard) ────────────────────────

    /// A request whose Host AND Origin both name `host` — what a browser sends from a page served by
    /// that host, including a DNS-rebinding page whose name now resolves to this server.
    fn req_for_host(method: Method, path: &str, host: &str) -> hyper::Request<()> {
        hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", host)
            .header("origin", format!("http://{host}"))
            .body(())
            .unwrap()
    }

    #[tokio::test]
    async fn rebinding_host_is_rejected_on_every_route() {
        // DNS rebinding: an attacker page on rebind.attacker.example:8080, its name rebound to this
        // server's IP, sends Host and Origin that match, so the CSRF guard passes. The Host
        // allowlist must refuse it before any route runs, reads or writes included.
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("AT", "Alice"))
            .await
            .unwrap();
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), store.clone());
        let host = "rebind.attacker.example:8080";
        for (method, path) in [
            (Method::GET, "/trakt/accounts"),
            (Method::GET, "/trakt"),
            (Method::POST, "/trakt/accounts/alice/remove"),
            (Method::POST, "/trakt/accounts/alice/refresh"),
            (Method::POST, "/trakt/enrol"),
        ] {
            let r = svc.handle(req_for_host(method.clone(), path, host)).await;
            assert_eq!(
                r.status(),
                StatusCode::MISDIRECTED_REQUEST,
                "{method} {path} for a rebinding Host must be refused"
            );
        }
        assert!(
            store.get_trakt_tokens("alice".into()).await.is_some(),
            "a refused remove must not unlink the account"
        );
        assert_eq!(
            svc.inflight.available_permits(),
            MAX_CONCURRENT_ENROLMENTS,
            "a refused enrol must not start a device-flow poll"
        );
    }

    #[tokio::test]
    async fn local_network_hosts_are_accepted() {
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), Store::in_memory());
        for host in [
            "192.168.1.10:8080",
            "127.0.0.1",
            "[::1]:8080",
            "[fe80::1%25eth0]:8080",
            "localhost:8080",
            "LOCALHOST",
            "app.localhost",
            "mediabox:8080",
            "debridmoviemapper:8080",
            "host.local",
            "nas.lan",
            "box.home.arpa",
            "svc.internal:8080",
            "host.local.",
        ] {
            let r = svc
                .handle(req_for_host(Method::GET, "/trakt/accounts", host))
                .await;
            assert_eq!(r.status(), StatusCode::OK, "Host {host} must be accepted");
        }
        // No Host header at all (a non-browser client): accepted, like the CSRF guard's
        // header-less case.
        let r = svc
            .handle(
                hyper::Request::builder()
                    .method(Method::GET)
                    .uri("/trakt/accounts")
                    .body(())
                    .unwrap(),
            )
            .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn configured_extra_host_is_accepted_case_insensitively() {
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), Store::in_memory())
            .with_allowed_hosts(vec!["media.example.com".to_string()]);
        for host in ["media.example.com", "Media.Example.COM:443"] {
            let r = svc
                .handle(req_for_host(Method::GET, "/trakt/accounts", host))
                .await;
            assert_eq!(r.status(), StatusCode::OK, "allowlisted Host {host}");
        }
        // Another public name is still refused.
        let r = svc
            .handle(req_for_host(
                Method::GET,
                "/trakt/accounts",
                "other.example.com",
            ))
            .await;
        assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);
    }

    #[test]
    fn host_allowlist_rejects_lookalikes_and_malformed_hosts() {
        for host in [
            "rebind.attacker.example",
            "localhost.attacker.example",
            "host.local.attacker.example",
            "127.0.0.1.nip.io",
            "evil.example:8080",
            "host.local:abc",
            "[::1",
            "[::1]x",
            "a b",
            "",
        ] {
            assert!(!host_is_allowed(host, &[]), "{host:?} must be refused");
        }
    }

    /// A TraktClient whose `refresh` simulates a CONCURRENT refresh: it rotates the stored token's
    /// refresh string (as if another /refresh just succeeded and Trakt invalidated ours), then fails.
    struct RotatingThenFailingRefresh {
        store: Store,
    }
    #[async_trait::async_trait]
    impl TraktClient for RotatingThenFailingRefresh {
        async fn device_code(&self) -> Result<DeviceCode, AppError> {
            Err(AppError::Unavailable)
        }
        async fn poll_token(&self, _dc: &str) -> Result<DeviceTokenPoll, AppError> {
            Err(AppError::Unavailable)
        }
        async fn refresh(&self, _refresh_token: &str) -> Result<TraktTokenResponse, AppError> {
            // Concurrent refresh rotated the stored token during our network window.
            if let Some(mut t) = self.store.get_trakt_tokens("alice".into()).await {
                t.refresh = "ROTATED".into();
                let _ = self.store.put_trakt_tokens("alice".into(), t).await;
            }
            Err(AppError::Unavailable) // our refresh against the now-stale token fails
        }
        async fn me(&self, _at: &str) -> Result<TraktUser, AppError> {
            Err(AppError::Unavailable)
        }
        async fn watchlist(
            &self,
            _at: &str,
        ) -> Result<Vec<crate::trakt_client::TraktItem>, AppError> {
            Err(AppError::Unavailable)
        }
        async fn in_progress(
            &self,
            _at: &str,
        ) -> Result<Vec<crate::trakt_client::TraktItem>, AppError> {
            Err(AppError::Unavailable)
        }
        async fn watched(&self, _at: &str) -> Result<crate::trakt_client::WatchedData, AppError> {
            Err(AppError::Unavailable)
        }
        async fn show_progress(
            &self,
            _at: &str,
            _trakt_id: u64,
        ) -> Result<crate::trakt_client::ShowProgress, AppError> {
            Err(AppError::Unavailable)
        }
    }

    #[tokio::test]
    async fn refresh_failure_on_a_concurrently_rotated_token_does_not_flag_reenrolment() {
        // A second concurrent /refresh rotates the stored token; our refresh then fails against the
        // now-stale token. That false-alarm failure must NOT set needs_reenrolment over the freshly
        // good record (the accounts page would then ask the operator to re-enrol a healthy account).
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("AT", "Alice"))
            .await
            .unwrap(); // refresh = "RT"
        let svc = EnrolmentService::new(
            Arc::new(RotatingThenFailingRefresh {
                store: store.clone(),
            }),
            store.clone(),
        );
        svc.refresh_account("alice").await.unwrap();
        let tok = store.get_trakt_tokens("alice".into()).await.unwrap();
        assert_eq!(
            tok.refresh, "ROTATED",
            "the concurrently-rotated token must survive"
        );
        assert!(
            !tok.needs_reenrolment,
            "a stale-token refresh failure must NOT flag re-enrolment on a healthy account"
        );
    }

    #[tokio::test]
    async fn refresh_failure_on_the_current_token_does_flag_reenrolment() {
        // The genuine-failure counterpart: refresh fails against the CURRENT token (no concurrent
        // rotation) → the account IS flagged for re-enrolment.
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("AT", "Alice"))
            .await
            .unwrap();
        let svc = EnrolmentService::new(
            Arc::new(MockTrakt {
                fail_refresh: true,
                ..Default::default()
            }),
            store.clone(),
        );
        svc.refresh_account("alice").await.unwrap();
        let tok = store.get_trakt_tokens("alice".into()).await.unwrap();
        assert!(
            tok.needs_reenrolment,
            "a genuine refresh failure must flag re-enrolment"
        );
    }

    /// A TraktClient whose `refresh` always reports a rate-limit (persistent 429).
    struct RateLimitedRefresh;
    #[async_trait::async_trait]
    impl TraktClient for RateLimitedRefresh {
        async fn device_code(&self) -> Result<DeviceCode, AppError> {
            Err(AppError::Unavailable)
        }
        async fn poll_token(&self, _dc: &str) -> Result<DeviceTokenPoll, AppError> {
            Err(AppError::Unavailable)
        }
        async fn refresh(&self, _refresh_token: &str) -> Result<TraktTokenResponse, AppError> {
            Err(AppError::RateLimited)
        }
        async fn me(&self, _at: &str) -> Result<TraktUser, AppError> {
            Err(AppError::Unavailable)
        }
        async fn watchlist(
            &self,
            _at: &str,
        ) -> Result<Vec<crate::trakt_client::TraktItem>, AppError> {
            Err(AppError::Unavailable)
        }
        async fn in_progress(
            &self,
            _at: &str,
        ) -> Result<Vec<crate::trakt_client::TraktItem>, AppError> {
            Err(AppError::Unavailable)
        }
        async fn watched(&self, _at: &str) -> Result<crate::trakt_client::WatchedData, AppError> {
            Err(AppError::Unavailable)
        }
        async fn show_progress(
            &self,
            _at: &str,
            _trakt_id: u64,
        ) -> Result<crate::trakt_client::ShowProgress, AppError> {
            Err(AppError::Unavailable)
        }
    }

    #[tokio::test]
    async fn refresh_rate_limit_does_not_flag_reenrolment() {
        // A persistent 429 during refresh is transient throttling, not an auth failure — the single-
        // use grant was never processed. It must NOT flag re-enrolment (a spurious accounts-page
        // marker prompting a needless re-enrolment), unlike the genuine-failure case above.
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("AT", "Alice"))
            .await
            .unwrap();
        let svc = EnrolmentService::new(Arc::new(RateLimitedRefresh), store.clone());
        svc.refresh_account("alice").await.unwrap();
        let tok = store.get_trakt_tokens("alice".into()).await.unwrap();
        assert!(
            !tok.needs_reenrolment,
            "a rate-limited refresh must NOT flag re-enrolment"
        );
    }

    /// A `MockTrakt` whose `device_code`, `poll_token` and `refresh` take `delay` to answer, as
    /// they do while the shared Trakt rate limiter waits out a 429 back-off (up to minutes per
    /// attempt).
    struct SlowTrakt {
        inner: MockTrakt,
        delay: std::time::Duration,
        refresh_calls: std::sync::atomic::AtomicUsize,
        poll_calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl TraktClient for SlowTrakt {
        async fn device_code(&self) -> Result<DeviceCode, AppError> {
            tokio::time::sleep(self.delay).await;
            self.inner.device_code().await
        }
        async fn poll_token(&self, dc: &str) -> Result<DeviceTokenPoll, AppError> {
            self.poll_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.inner.poll_token(dc).await
        }
        async fn refresh(&self, refresh_token: &str) -> Result<TraktTokenResponse, AppError> {
            self.refresh_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.inner.refresh(refresh_token).await
        }
        async fn me(&self, at: &str) -> Result<TraktUser, AppError> {
            self.inner.me(at).await
        }
        async fn watchlist(
            &self,
            at: &str,
        ) -> Result<Vec<crate::trakt_client::TraktItem>, AppError> {
            self.inner.watchlist(at).await
        }
        async fn in_progress(
            &self,
            at: &str,
        ) -> Result<Vec<crate::trakt_client::TraktItem>, AppError> {
            self.inner.in_progress(at).await
        }
        async fn watched(&self, at: &str) -> Result<crate::trakt_client::WatchedData, AppError> {
            self.inner.watched(at).await
        }
        async fn show_progress(
            &self,
            at: &str,
            trakt_id: u64,
        ) -> Result<crate::trakt_client::ShowProgress, AppError> {
            self.inner.show_progress(at, trakt_id).await
        }
    }

    fn slow_refreshing_trakt(delay: std::time::Duration) -> Arc<SlowTrakt> {
        Arc::new(SlowTrakt {
            inner: MockTrakt {
                token: TraktTokenResponse {
                    access_token: "FRESH".into(),
                    refresh_token: "NEWREF".into(),
                    expires_in: 100,
                    created_at: 2_000,
                },
                ..Default::default()
            },
            delay,
            refresh_calls: Default::default(),
            poll_calls: Default::default(),
        })
    }

    async fn refresh_click(svc: &EnrolmentService) -> Response<Body> {
        svc.handle(routed_req(
            Method::POST,
            "http://host.local/trakt/accounts/alice/refresh",
            true,
        ))
        .await
    }

    /// Clicking Refresh again while an earlier click's refresh is still waiting on Trakt starts no
    /// second refresh (each would queue one more request behind the throttled limiter, delaying the
    /// background sync further). Once it has finished, a click refreshes again.
    #[tokio::test(start_paused = true)]
    async fn refresh_click_does_not_start_a_second_refresh_while_one_is_running() {
        use std::sync::atomic::Ordering;
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("OLD", "Alice"))
            .await
            .unwrap();
        let slow = std::time::Duration::from_secs(300);
        let trakt = slow_refreshing_trakt(slow);
        let svc = EnrolmentService::new(trakt.clone(), store.clone());

        assert_eq!(refresh_click(&svc).await.status(), StatusCode::SEE_OTHER);
        let started = tokio::time::Instant::now();
        assert_eq!(refresh_click(&svc).await.status(), StatusCode::SEE_OTHER);
        assert!(
            started.elapsed() < TRAKT_CLICK_WAIT,
            "a click while a refresh is running answers at once, took {:?}",
            started.elapsed()
        );
        assert_eq!(trakt.refresh_calls.load(Ordering::SeqCst), 1);

        tokio::time::sleep(slow).await;
        for _ in 0..50 {
            if store.get_trakt_tokens("alice".into()).await.unwrap().access == "FRESH" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert_eq!(
            store.get_trakt_tokens("alice".into()).await.unwrap().access,
            "FRESH"
        );
        assert_eq!(refresh_click(&svc).await.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            trakt.refresh_calls.load(Ordering::SeqCst),
            2,
            "a click after the refresh finished refreshes again"
        );
    }

    /// A Refresh click whose refresh answers within the bound redirects once the new tokens are
    /// stored, as before.
    #[tokio::test(start_paused = true)]
    async fn refresh_click_waits_for_a_prompt_refresh() {
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("OLD", "Alice"))
            .await
            .unwrap();
        let svc = EnrolmentService::new(
            slow_refreshing_trakt(std::time::Duration::from_secs(1)),
            store.clone(),
        );
        let r = svc
            .handle(routed_req(
                Method::POST,
                "http://host.local/trakt/accounts/alice/refresh",
                true,
            ))
            .await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let tok = store.get_trakt_tokens("alice".into()).await.unwrap();
        assert_eq!(tok.access, "FRESH", "stored before the redirect");
        assert_eq!(tok.refresh, "NEWREF");
    }

    /// While the Trakt limiter waits out a 429 back-off, a Refresh click must not hang the browser
    /// for minutes: it redirects after `TRAKT_CLICK_WAIT`, and the refresh runs on to completion
    /// (cancelling it after Trakt processed the grant would lose the rotated refresh token).
    #[tokio::test(start_paused = true)]
    async fn refresh_click_answers_while_a_throttled_refresh_finishes_in_the_background() {
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_fixture("OLD", "Alice"))
            .await
            .unwrap();
        let slow = std::time::Duration::from_secs(300);
        let svc = EnrolmentService::new(slow_refreshing_trakt(slow), store.clone());
        let started = tokio::time::Instant::now();
        let r = svc
            .handle(routed_req(
                Method::POST,
                "http://host.local/trakt/accounts/alice/refresh",
                true,
            ))
            .await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert!(
            started.elapsed() <= TRAKT_CLICK_WAIT,
            "the click must be answered within the bound, took {:?}",
            started.elapsed()
        );
        assert_eq!(
            store.get_trakt_tokens("alice".into()).await.unwrap().access,
            "OLD",
            "the refresh is still waiting on Trakt"
        );

        tokio::time::sleep(slow).await;
        let mut tok = store.get_trakt_tokens("alice".into()).await.unwrap();
        for _ in 0..50 {
            if tok.access == "FRESH" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            tok = store.get_trakt_tokens("alice".into()).await.unwrap();
        }
        assert_eq!(
            tok.access, "FRESH",
            "the refresh completed after the redirect"
        );
        assert_eq!(tok.refresh, "NEWREF");
    }

    /// An Enrol click whose device-code request is held up by the Trakt limiter answers 503 after
    /// `TRAKT_CLICK_WAIT` instead of hanging, and frees its enrolment slot.
    #[tokio::test(start_paused = true)]
    async fn enrol_click_answers_503_when_the_device_code_is_held_up() {
        let svc = EnrolmentService::new(
            Arc::new(SlowTrakt {
                inner: MockTrakt::default(),
                delay: std::time::Duration::from_secs(300),
                refresh_calls: Default::default(),
                poll_calls: Default::default(),
            }),
            Store::in_memory(),
        );
        let started = tokio::time::Instant::now();
        let r = svc
            .handle(routed_req(
                Method::POST,
                "http://host.local/trakt/enrol",
                true,
            ))
            .await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            started.elapsed() <= TRAKT_CLICK_WAIT,
            "the click must be answered within the bound, took {:?}",
            started.elapsed()
        );
        assert!(body_text(r).await.contains("try again shortly"));
        assert_eq!(
            svc.inflight.available_permits(),
            MAX_CONCURRENT_ENROLMENTS,
            "a timed-out enrol must not hold an enrolment slot"
        );
    }

    /// While the Trakt limiter waits out a 429 back-off, every device-token poll takes minutes and
    /// comes back `Pending` (a final 429 maps to it). The background poll must still end when the
    /// device code expires by the clock, freeing its enrolment slot. Counting only the sleeps
    /// between polls let it run for hours.
    #[tokio::test(start_paused = true)]
    async fn poll_to_completion_ends_at_the_codes_expiry_while_polls_are_throttled() {
        use std::sync::atomic::Ordering;
        let slow = Arc::new(SlowTrakt {
            inner: MockTrakt::default(), // poll: Pending
            delay: std::time::Duration::from_secs(60),
            refresh_calls: Default::default(),
            poll_calls: Default::default(),
        });
        let trakt: Arc<dyn TraktClient> = slow.clone();
        let store = Store::in_memory();
        let started = tokio::time::Instant::now();
        let err = poll_to_completion(&trakt, &store, "dc".into(), 5, 600)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("expired"), "{err}");
        assert!(
            started.elapsed() <= std::time::Duration::from_secs(600 + 5),
            "the poll must end at the code's expiry, took {:?}",
            started.elapsed()
        );
        // One poll per 65 s (60 s held up + the 5 s interval) fits ten into 600 s; counting only
        // the sleeps would allow 121.
        let polls = slow.poll_calls.load(Ordering::SeqCst);
        assert!(polls <= 11, "{polls} polls");
        assert!(store.all_trakt_tokens().await.is_empty());
    }
}
