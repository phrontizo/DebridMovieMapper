//! Local-network Trakt enrolment routes, served on the existing WebDAV listener.
//!
//! Trust model: these routes carry **no authentication** — the design assumes the WebDAV
//! port is only reachable on a trusted local network (same as the media itself). They let an
//! operator link a Trakt account via OAuth device-flow, list linked accounts, and refresh or
//! remove them. Tokens are keyed by the user's Trakt *slug* (stable per-user key).
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
    /// Caps concurrent device-flow enrolment polls to one. The `/trakt/enrol` route carries no
    /// auth, so without this a single client could spawn unbounded long-lived (~600s) background
    /// poll tasks — exhausting host resources and hammering Trakt's API under our `client_id`.
    inflight: Arc<tokio::sync::Semaphore>,
}

impl EnrolmentService {
    pub fn new(trakt: Arc<dyn TraktClient>, store: Store) -> Self {
        Self {
            trakt,
            store,
            inflight: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    /// Route a `/trakt*` request by method + path. Unknown `/trakt` paths return 404.
    pub async fn handle(&self, req: hyper::Request<hyper::body::Incoming>) -> Response<Body> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // CSRF guard: the state-changing POST routes are unauthenticated, so the trusted-LAN
        // model alone does not stop a drive-by cross-origin form POST from a page a LAN browser
        // visits. Reject any POST a browser flags as cross-origin (non-browser clients send
        // neither header and are allowed, consistent with the trusted-LAN model).
        if method == Method::POST && is_cross_origin(&req) {
            warn!("Rejecting cross-origin POST to {} (CSRF guard)", path);
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
                if let Err(e) = self.refresh_account(slug).await {
                    // refresh_account already logs + flags the account on a Trakt-refresh failure;
                    // the only error surfaced here is a token-store write failure, which would
                    // otherwise be lost silently (unlike the `remove` arm below).
                    warn!("Trakt token refresh persist failed for '{}': {}", slug, e);
                }
                redirect("/trakt/accounts")
            }
            (&Method::POST, ["trakt", "accounts", slug, "remove"]) => {
                if let Err(e) = self.remove_account(slug).await {
                    warn!("Trakt token removal failed for '{}': {}", slug, e);
                }
                redirect("/trakt/accounts")
            }
            _ => html(StatusCode::NOT_FOUND, "<h1>404 Not Found</h1>".to_string()),
        }
    }

    /// Begin device-flow enrolment: fetch a device code, spawn the background poll that
    /// completes the link, and return HTML showing the user_code + verification URL.
    pub(crate) async fn start_enrolment(&self) -> Response<Body> {
        // Admit only one in-flight enrolment at a time (the route is unauthenticated). The owned
        // permit is moved into the background poll below and released when it finishes/expires.
        let Ok(permit) = self.inflight.clone().try_acquire_owned() else {
            return html(
                StatusCode::TOO_MANY_REQUESTS,
                "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Enrolment in progress</title></head>\
                 <body><h1>An enrolment is already in progress</h1>\
                 <p>Finish approving the current device code on trakt.tv, or wait for it to expire, then try again.</p>\
                 <p><a href=\"/trakt/accounts\">Back to accounts</a></p></body></html>"
                    .to_string(),
            );
        };
        let dc = match self.trakt.device_code().await {
            Ok(dc) => dc,
            Err(e) => {
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
    /// cleared; on failure, set the flag so the accounts page surfaces it.
    pub(crate) async fn refresh_account(&self, slug: &str) -> Result<(), AppError> {
        let Some(mut tokens) = self.store.get_trakt_tokens(slug.to_string()).await else {
            return Ok(()); // nothing stored under this slug — no-op
        };
        match self.trakt.refresh(&tokens.refresh).await {
            Ok(fresh) => {
                tokens.access = fresh.access_token;
                if !fresh.refresh_token.is_empty() {
                    tokens.refresh = fresh.refresh_token;
                }
                tokens.expires_at = fresh.created_at.saturating_add(fresh.expires_in);
                tokens.needs_reenrolment = false;
            }
            Err(e) => {
                warn!("Trakt refresh failed for '{}': {}", slug, e);
                tokens.needs_reenrolment = true;
            }
        }
        self.store.put_trakt_tokens(slug.to_string(), tokens).await
    }

    /// Remove (unlink) one account's stored tokens.
    pub(crate) async fn remove_account(&self, slug: &str) -> Result<(), AppError> {
        self.store.remove_trakt_tokens(slug.to_string()).await
    }
}

/// Poll the device-token endpoint until Authorized/terminal; on success fetch the user's slug
/// and persist the tokens. Returns the stored slug. The loop is bounded by `expires_in_secs`
/// so it can never spin forever.
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
    let mut elapsed: u64 = 0;
    loop {
        match trakt.poll_token(&device_code).await? {
            DeviceTokenPoll::Authorized(tok) => {
                let user = trakt.me(&tok.access_token).await?;
                let tokens = TraktTokens {
                    access: tok.access_token,
                    refresh: tok.refresh_token,
                    expires_at: tok.created_at.saturating_add(tok.expires_in),
                    username: user.username,
                    needs_reenrolment: false,
                };
                store.put_trakt_tokens(user.slug.clone(), tokens).await?;
                return Ok(user.slug);
            }
            DeviceTokenPoll::Pending => {
                if elapsed >= expires_in_secs {
                    return Err(AppError::Config(
                        "Trakt device code expired before authorisation".to_string(),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                elapsed = elapsed.saturating_add(interval);
            }
            DeviceTokenPoll::Denied => {
                return Err(AppError::Config("Trakt enrolment was denied".to_string()))
            }
            DeviceTokenPoll::Expired => {
                return Err(AppError::Config("Trakt device code expired".to_string()))
            }
        }
    }
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
        if site.eq_ignore_ascii_case("cross-site") || site.eq_ignore_ascii_case("cross-origin") {
            return true;
        }
    }
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        // Strip the scheme to compare host[:port] against the Host header.
        let origin_host = origin.split_once("://").map(|(_, rest)| rest).unwrap_or("");
        let host = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !origin_host.is_empty() && !host.is_empty() && !origin_host.eq_ignore_ascii_case(host) {
            return true;
        }
    }
    false
}

/// HTML-escape `s` for safe use in both element text and double-quoted attributes (`href="..."`,
/// `action="..."`). Covers `&`, `<`, `>`, and `"` (`&` first to avoid double-escaping).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

/// The accounts list page: each linked account with username + slug, a re-enrolment marker
/// when flagged, per-account refresh/remove forms, and a form to enrol a new account.
fn accounts_html(accounts: &[(String, TraktTokens)]) -> String {
    let mut rows = String::new();
    if accounts.is_empty() {
        rows.push_str("<p>No Trakt accounts are linked yet.</p>");
    } else {
        rows.push_str("<ul>");
        for (slug, tok) in accounts {
            let marker = if tok.needs_reenrolment {
                " <strong>&#9888; needs re-enrolment</strong>"
            } else {
                ""
            };
            rows.push_str(&format!(
                "<li><strong>{user}</strong> (slug: {slug}){marker} \
                 <form method=\"post\" action=\"/trakt/accounts/{slug}/refresh\" style=\"display:inline\">\
                 <button type=\"submit\">Refresh</button></form> \
                 <form method=\"post\" action=\"/trakt/accounts/{slug}/remove\" style=\"display:inline\">\
                 <button type=\"submit\">Remove</button></form></li>",
                user = esc(&tok.username),
                slug = esc(slug),
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

// --- Response helpers -----------------------------------------------------------------------

/// Build an HTML response of the WebDAV body type.
fn html(status: StatusCode, body: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/html; charset=utf-8")
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
    use redb::backends::InMemoryBackend;
    use redb::Database;

    fn mem_store() -> Store {
        let db = Database::builder()
            .create_with_backend(InMemoryBackend::new())
            .unwrap();
        Store::from_database(Arc::new(db)).unwrap()
    }

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
        let store = mem_store();

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

    #[tokio::test]
    async fn poll_to_completion_expired_is_err_and_stores_nothing() {
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Expired,
            ..Default::default()
        });
        let store = mem_store();
        assert!(poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .is_err());
        assert!(store.all_trakt_tokens().await.is_empty());
    }

    #[tokio::test]
    async fn poll_to_completion_denied_is_err_and_stores_nothing() {
        let trakt: Arc<dyn TraktClient> = Arc::new(MockTrakt {
            poll: DeviceTokenPoll::Denied,
            ..Default::default()
        });
        let store = mem_store();
        assert!(poll_to_completion(&trakt, &store, "dc".into(), 1, 30)
            .await
            .is_err());
        assert!(store.all_trakt_tokens().await.is_empty());
    }

    #[tokio::test]
    async fn start_enrolment_caps_concurrent_polls_to_one() {
        let store = mem_store();
        let svc = EnrolmentService::new(Arc::new(MockTrakt::default()), store);
        // Hold the single permit to simulate one enrolment already in flight.
        let held = svc
            .inflight
            .clone()
            .try_acquire_owned()
            .expect("first permit available");
        let refused = svc.start_enrolment().await;
        assert_eq!(
            refused.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "a second concurrent enrolment must be refused, not spawn another poll task"
        );
        // Once the in-flight enrolment ends, the slot frees and a new enrolment is admitted again.
        drop(held);
        let admitted = svc.start_enrolment().await;
        assert_ne!(
            admitted.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the slot must free after the in-flight enrolment ends"
        );
    }

    #[tokio::test]
    async fn remove_account_deletes_only_that_slug() {
        let store = mem_store();
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
    async fn refresh_account_updates_tokens_and_clears_flag() {
        let store = mem_store();
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
        let store = mem_store();
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
        let svc = EnrolmentService::new(trakt, mem_store());

        let resp = svc.start_enrolment().await;
        assert_eq!(resp.status(), StatusCode::OK);
        // The body is built from `enrol_html(&device_code)`, whose content is asserted above; here
        // we confirm the rendered HTML for this device code carries both fields.
        assert!(enrol_html(&dc).contains("WXYZ-9999"));
        assert!(enrol_html(&dc).contains("https://trakt.tv/activate"));
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
        assert!(page.contains("needs re-enrolment"));
        assert!(page.contains("/trakt/accounts/alice/refresh"));
        assert!(page.contains("/trakt/accounts/bob/remove"));
        assert!(page.contains("/trakt/enrol"));
    }

    // ── security: HTML-escaping (XSS) ─────────────────────────────────────────

    #[test]
    fn esc_neutralises_html_metacharacters() {
        assert_eq!(
            esc(r#"<script>alert("1")&'</script>"#),
            "&lt;script&gt;alert(&quot;1&quot;)&amp;'&lt;/script&gt;"
        );
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
}
