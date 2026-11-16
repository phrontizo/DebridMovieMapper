use crate::vfs::{UpdateType, VfsChange};
use reqwest::header::HeaderValue;
use std::time::Duration;
use tracing::info;

// Total number of send attempts (the loop is `0..MAX_SEND_ATTEMPTS`), NOT 1 + N retries — i.e. 10
// sends, matching the "attempt N/10" log text.
const MAX_SEND_ATTEMPTS: u32 = 10;
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// Max number of changed paths rendered into the notify log line. On a first-run / large-delta scan
/// the change set can be thousands of entries; the full count is always logged, but only a sample of
/// the paths (the rest collapse to "… and N more") to keep the line from bloating the log.
const LOG_SAMPLE_MAX: usize = 20;
/// Delay before sending notification to let rclone's directory cache expire.
/// Must be longer than rclone's --dir-cache-time (default 10s in compose.yml).
const NOTIFICATION_DELAY: Duration = Duration::from_secs(15);

/// What to do with a Jellyfin notification response, by HTTP status.
#[derive(Debug, PartialEq, Eq)]
enum NotifyOutcome {
    /// 2xx — notification accepted; stop.
    Done,
    /// Transient server error (502/503/504) or 429 rate-limit — retry.
    Retry,
    /// Any other status (4xx, 500, …) — a retry won't help; stop.
    GiveUp,
}

/// Classify a Jellyfin notification response status. Note 500 is NOT retried (it usually
/// signals a genuine error), whereas 502/503/504 are transient and worth retrying. 429
/// (Too Many Requests) is the canonical "back off and retry" signal, so it is retried too —
/// giving up on it would silently drop a library-update notification a short delay would deliver.
fn classify_status(status: reqwest::StatusCode) -> NotifyOutcome {
    if status.is_success() {
        NotifyOutcome::Done
    } else if matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    ) {
        NotifyOutcome::Retry
    } else {
        NotifyOutcome::GiveUp
    }
}

/// Render a bounded sample of changed paths for the notify log line: up to `max` paths joined with
/// ", ", plus a "… and N more" tail when the change set is larger. Keeps a large first-run delta
/// from producing a multi-KB log line (the full count is logged separately).
fn sample_change_paths(changes: &[VfsChange], max: usize) -> String {
    let shown = changes
        .iter()
        .take(max)
        .map(|c| c.path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if changes.len() > max {
        format!("{} … and {} more", shown, changes.len() - max)
    } else {
        shown
    }
}

/// `e` and every error in its `source()` chain, joined with ": ". A reqwest error's Display names
/// only its kind and URL ("error sending request for url (…)"), never the cause, so a mistyped
/// host (DNS), a refused port and a TLS certificate error would all read the same. This is safe to
/// log: the URL is the non-secret Jellyfin base, and the API key is sent only in the Authorization
/// header, which no error carries.
fn error_chain(e: &dyn std::error::Error) -> String {
    std::iter::successors(Some(e), |e| e.source())
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(": ")
}

pub struct JellyfinClient {
    url: String,
    /// Pre-built `Authorization: MediaBrowser Token="<key>"` value, marked sensitive so the key is
    /// never printed in debug output. Jellyfin 12 disables the legacy `X-Emby-Token` header (and
    /// the `api_key` query parameter) by default; this scheme also works on Jellyfin 10.x.
    auth_header: HeaderValue,
    mount_path: String,
    http: reqwest::Client,
}

impl JellyfinClient {
    pub fn new(url: String, api_key: String, mount_path: String) -> Result<Self, String> {
        let url = url.trim_end_matches('/').to_string();
        // Validate the URL parses and carries an http(s) scheme NOW, at construction. A typo (missing
        // scheme like `jellyfin:8096`, or `htp://…`) otherwise builds a client that logs "enabled" at
        // startup but then fails EVERY `notify_changes` — 15s of dead sleep + a confusing per-scan
        // "Failed to notify Jellyfin" warning — for the deployment's life. Fail loudly at startup
        // instead (from_env logs a clear "Failed to create Jellyfin client"), like the API-key check.
        let parsed = reqwest::Url::parse(&url)
            .map_err(|e| format!("JELLYFIN_URL is not a valid URL ({url:?}): {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "JELLYFIN_URL must be an http(s) URL, got scheme {:?} in {url:?}",
                parsed.scheme()
            ));
        }
        // The key is sent as the quoted `Token` parameter of `Authorization: MediaBrowser …`, so a
        // character that would end or split that parameter is refused (Jellyfin keys are hex).
        if api_key.contains(['"', '\\', ',']) {
            return Err(
                "Jellyfin API key must not contain a double quote, backslash or comma".to_string(),
            );
        }
        let mut auth_header = HeaderValue::from_str(&format!("MediaBrowser Token=\"{api_key}\""))
            .map_err(|e| {
            format!(
                "Jellyfin API key contains invalid HTTP header characters: {}",
                e
            )
        })?;
        auth_header.set_sensitive(true);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("Failed to build Jellyfin HTTP client: {}", e))?;
        Ok(Self {
            url,
            auth_header,
            mount_path: mount_path.trim_end_matches('/').to_string(),
            http,
        })
    }

    fn build_request_body(&self, changes: &[VfsChange]) -> serde_json::Value {
        let updates: Vec<serde_json::Value> = changes
            .iter()
            .map(|change| {
                let full_path = format!("{}/{}", self.mount_path, change.path);
                let update_type = match change.update_type {
                    UpdateType::Created => "Created",
                    UpdateType::Modified => "Modified",
                    UpdateType::Deleted => "Deleted",
                };
                serde_json::json!({
                    "Path": full_path,
                    "UpdateType": update_type
                })
            })
            .collect();

        serde_json::json!({ "Updates": updates })
    }

    pub async fn notify_changes(&self, changes: &[VfsChange]) {
        if changes.is_empty() {
            return;
        }

        let body = self.build_request_body(changes);
        let url = format!("{}/Library/Media/Updated", self.url);

        info!(
            "Notifying Jellyfin of {} change(s) in {}s: {}",
            changes.len(),
            NOTIFICATION_DELAY.as_secs(),
            sample_change_paths(changes, LOG_SAMPLE_MAX)
        );

        // Wait for rclone's directory cache to expire so Jellyfin sees fresh data
        // when it checks the filesystem after receiving our notification.
        tokio::time::sleep(NOTIFICATION_DELAY).await;

        // Why the last retried attempt failed, for the give-up line.
        let mut last_failure = String::new();
        for attempt in 0..MAX_SEND_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }

            let result = self
                .http
                .post(&url)
                .header(reqwest::header::AUTHORIZATION, self.auth_header.clone())
                .json(&body)
                .send()
                .await;

            match result {
                Ok(response) => {
                    let status = response.status();
                    match classify_status(status) {
                        NotifyOutcome::Done => {
                            info!("Jellyfin notified successfully");
                            return;
                        }
                        NotifyOutcome::Retry => {
                            tracing::warn!(
                                "Jellyfin returned {} (transient), attempt {}/{}",
                                status,
                                attempt + 1,
                                MAX_SEND_ATTEMPTS
                            );
                            last_failure = format!("Jellyfin returned {status}");
                            continue;
                        }
                        NotifyOutcome::GiveUp => {
                            let body = response.text().await.unwrap_or_default();
                            tracing::warn!(
                                "Jellyfin notification returned status {}: {:.200}",
                                status,
                                body
                            );
                            return;
                        }
                    }
                }
                Err(e) if e.is_connect() || e.is_timeout() || e.is_request() => {
                    // Connect failures (not started yet), timeouts (slow/overloaded Jellyfin) and a
                    // connection that drops after the request was sent (Jellyfin restarting
                    // mid-request: a reset, or closed before any response) are all transient, so
                    // retry; a repeated POST /Library/Media/Updated is harmless. Any other error
                    // (e.g. building the request) gives up. In reqwest 0.12 every error from
                    // sending the request, connect failures and the client timeout included, is a
                    // request error, so `is_request()` alone matches all three. The explicit checks
                    // choose the log label, and keep connect and timeout errors retried should a
                    // later reqwest classify them differently
                    // (`reqwest_connect_and_timeout_errors_are_request_errors` pins today's).
                    // The log names the cause chain, which tells a DNS failure, a refused port and
                    // a TLS error apart.
                    last_failure = error_chain(&e);
                    tracing::warn!(
                        "Transient Jellyfin error ({}: {}), attempt {}/{}",
                        if e.is_timeout() {
                            "timeout"
                        } else if e.is_connect() {
                            "cannot connect"
                        } else {
                            "request failed"
                        },
                        last_failure,
                        attempt + 1,
                        MAX_SEND_ATTEMPTS
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Failed to notify Jellyfin: {}", error_chain(&e));
                    return;
                }
            }
        }

        tracing::warn!(
            "Jellyfin notification failed after {} attempts (last: {})",
            MAX_SEND_ATTEMPTS,
            last_failure
        );
    }

    /// Try to create a JellyfinClient from environment variables.
    /// Returns None if any of the required env vars are missing or invalid.
    pub fn from_env() -> Option<Self> {
        Self::from_values(
            std::env::var("JELLYFIN_URL").ok(),
            std::env::var("JELLYFIN_API_KEY").ok(),
            std::env::var("JELLYFIN_RCLONE_MOUNT_PATH").ok(),
        )
    }

    /// `from_env` on already-read values (`None` = the variable is unset), so the handling can be
    /// tested without mutating the process environment.
    fn from_values(
        url: Option<String>,
        api_key: Option<String>,
        mount_path: Option<String>,
    ) -> Option<Self> {
        // Trim and treat blank-as-unset for every value, matching `Config`'s env handling — a
        // whitespace-only value (e.g. from quoted compose YAML) must not build a client that POSTs
        // to a malformed URL at runtime.
        let url = url?.trim().to_string();
        let api_key = api_key?.trim().to_string();
        let mount_path = mount_path?.trim().to_string();

        if url.is_empty() || api_key.is_empty() || mount_path.is_empty() {
            return None;
        }

        match Self::new(url, api_key, mount_path) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!("Failed to create Jellyfin client: {}", e);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_loopback::{serve, Loopback, Step};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    #[test]
    fn classify_status_distinguishes_done_retry_giveup() {
        use reqwest::StatusCode;
        // 2xx → Done
        assert_eq!(classify_status(StatusCode::OK), NotifyOutcome::Done);
        assert_eq!(classify_status(StatusCode::NO_CONTENT), NotifyOutcome::Done);
        // Transient 5xx → Retry
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE),
            NotifyOutcome::Retry
        );
        // 429 Too Many Requests is the canonical "back off and retry" signal → Retry
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            NotifyOutcome::Retry
        );
        assert_eq!(
            classify_status(StatusCode::BAD_GATEWAY),
            NotifyOutcome::Retry
        );
        assert_eq!(
            classify_status(StatusCode::GATEWAY_TIMEOUT),
            NotifyOutcome::Retry
        );
        // 500 and 4xx are NOT retried → GiveUp
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            NotifyOutcome::GiveUp
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND),
            NotifyOutcome::GiveUp
        );
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED),
            NotifyOutcome::GiveUp
        );
    }

    #[test]
    fn sample_change_paths_truncates_large_change_sets() {
        let mk = |n: usize| -> Vec<VfsChange> {
            (0..n)
                .map(|i| VfsChange {
                    path: format!("Movies/Film {i}"),
                    update_type: UpdateType::Created,
                })
                .collect()
        };
        // Under the cap: all paths, no tail.
        let s = sample_change_paths(&mk(3), 20);
        assert_eq!(s, "Movies/Film 0, Movies/Film 1, Movies/Film 2");
        assert!(!s.contains("more"));
        // Exactly at the cap: still no tail.
        assert!(!sample_change_paths(&mk(20), 20).contains("more"));
        // Over the cap: sample + "… and N more".
        let big = sample_change_paths(&mk(1000), 20);
        assert!(big.contains("Movies/Film 0"), "must show the first sample");
        assert!(big.contains("Movies/Film 19"), "must show up to the cap");
        assert!(!big.contains("Movies/Film 20"), "must not exceed the cap");
        assert!(
            big.ends_with("… and 980 more"),
            "tail must count the remainder: {big}"
        );
        // Empty set → empty string.
        assert_eq!(sample_change_paths(&[], 20), "");
    }

    #[test]
    fn build_request_body_single_created() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "test-key".to_string(),
            "/mnt/debrid".to_string(),
        )
        .unwrap();
        let changes = vec![VfsChange {
            path: "Shows/Breaking Bad/Season 03".to_string(),
            update_type: UpdateType::Created,
        }];
        let body = client.build_request_body(&changes);
        let updates = body["Updates"].as_array().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates[0]["Path"],
            "/mnt/debrid/Shows/Breaking Bad/Season 03"
        );
        assert_eq!(updates[0]["UpdateType"], "Created");
    }

    #[test]
    fn build_request_body_multiple_changes() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "test-key".to_string(),
            "/mnt/debrid".to_string(),
        )
        .unwrap();
        let changes = vec![
            VfsChange {
                path: "Movies/Old Movie".to_string(),
                update_type: UpdateType::Deleted,
            },
            VfsChange {
                path: "Movies/New Movie".to_string(),
                update_type: UpdateType::Created,
            },
            VfsChange {
                path: "Shows/Breaking Bad/Season 01".to_string(),
                update_type: UpdateType::Modified,
            },
        ];
        let body = client.build_request_body(&changes);
        let updates = body["Updates"].as_array().unwrap();
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0]["UpdateType"], "Deleted");
        assert_eq!(updates[1]["UpdateType"], "Created");
        assert_eq!(
            updates[2]["Path"],
            "/mnt/debrid/Shows/Breaking Bad/Season 01"
        );
    }

    #[test]
    fn build_request_body_trims_trailing_slashes() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096/".to_string(),
            "test-key".to_string(),
            "/mnt/debrid/".to_string(),
        )
        .unwrap();
        let changes = vec![VfsChange {
            path: "Movies/Test".to_string(),
            update_type: UpdateType::Created,
        }];
        let body = client.build_request_body(&changes);
        let updates = body["Updates"].as_array().unwrap();
        assert_eq!(updates[0]["Path"], "/mnt/debrid/Movies/Test");
    }

    #[test]
    fn build_request_body_empty_has_no_updates() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "test-key".to_string(),
            "/mnt/debrid".to_string(),
        )
        .unwrap();
        let body = client.build_request_body(&[]);
        let updates = body["Updates"].as_array().unwrap();
        assert!(updates.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn notify_changes_with_no_changes_sends_nothing_and_does_not_wait() {
        // An empty change set must return at once: no request, and no NOTIFICATION_DELAY sleep
        // (under a paused clock a sleep would auto-advance time by the full delay).
        use std::sync::atomic::Ordering;
        let (url, counter) = spawn_counting_server(500, 0).await;
        let client = JellyfinClient::new(url, "key".to_string(), "/media".to_string()).unwrap();
        let start = tokio::time::Instant::now();
        client.notify_changes(&[]).await;
        assert!(
            start.elapsed() < NOTIFICATION_DELAY,
            "an empty change set must not wait out the notification delay"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "an empty change set sends no request"
        );
    }

    #[test]
    fn new_rejects_an_api_key_that_would_break_the_quoted_auth_param() {
        // The key goes inside `Token="…"` in the Authorization header; a quote, backslash or comma
        // would end or split that parameter, so such a key is refused at construction.
        for bad in ["ab\"cd", "ab\\cd", "ab,cd"] {
            assert!(
                JellyfinClient::new(
                    "http://jellyfin:8096".to_string(),
                    bad.to_string(),
                    "/media".to_string()
                )
                .is_err(),
                "API key {bad:?} must be rejected"
            );
        }
    }

    /// Loopback that answers every request 200 and records each request, so a test can check
    /// what the client sent ([`Loopback::heads`]).
    async fn spawn_capturing_server() -> Loopback {
        serve(Vec::new(), Some(Step::status(200))).await
    }

    #[tokio::test(start_paused = true)]
    async fn notify_changes_authenticates_with_the_mediabrowser_authorization_header() {
        // Jellyfin 12 disables the legacy X-Emby-Token header by default; the supported form is
        // `Authorization: MediaBrowser Token="<key>"` (which Jellyfin 10.x accepts too).
        let server = spawn_capturing_server().await;
        let client = JellyfinClient::new(
            server.base.clone(),
            "test-key".to_string(),
            "/media".to_string(),
        )
        .unwrap();
        client.notify_changes(&one_change()).await;
        let heads = server.heads();
        assert_eq!(heads.len(), 1, "one notification request");
        let headers: Vec<(String, String)> = heads[0]
            .split("\r\n")
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let auth: Vec<&str> = headers
            .iter()
            .filter(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(
            auth,
            vec!["MediaBrowser Token=\"test-key\""],
            "request head: {}",
            heads[0]
        );
        assert!(
            !headers.iter().any(|(name, _)| name == "x-emby-token"),
            "the deprecated X-Emby-Token header must not be sent: {}",
            heads[0]
        );
    }

    #[test]
    fn new_returns_error_for_invalid_api_key() {
        // HeaderValue rejects control characters (e.g. newlines)
        let result = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "bad\nkey".to_string(),
            "/mnt/debrid".to_string(),
        );
        assert!(
            result.is_err(),
            "API key with control characters should return Err"
        );
    }

    #[test]
    fn new_succeeds_for_valid_api_key() {
        let result = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "abc123def456".to_string(),
            "/mnt/debrid".to_string(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn new_rejects_a_malformed_or_schemeless_url() {
        // A URL typo must fail at construction, not silently break every notify (a 15s dead sleep +
        // per-scan warning) despite a "Jellyfin notification enabled" startup log.
        for bad in [
            "jellyfin:8096",
            "htp://jellyfin:8096",
            "not a url",
            "",
            "/media",
        ] {
            assert!(
                JellyfinClient::new(bad.to_string(), "k".to_string(), "/media".to_string())
                    .is_err(),
                "malformed/schemeless JELLYFIN_URL {bad:?} must be rejected"
            );
        }
        // https and a trailing slash are fine (the slash is trimmed).
        assert!(JellyfinClient::new(
            "https://jellyfin.local:8920/".to_string(),
            "k".to_string(),
            "/media".to_string()
        )
        .is_ok());
    }

    #[test]
    fn from_values_trims_values_and_treats_blank_as_unset() {
        // `from_env` delegates to `from_values`, so this covers the env handling without touching
        // the process environment (which other tests in this binary share).
        let some = |s: &str| Some(s.to_string());

        // Whitespace-padded values must be trimmed (and not build a malformed-URL client).
        let client = JellyfinClient::from_values(
            some("  http://jellyfin:8096  "),
            some("  abc123  "),
            some("  /media  "),
        )
        .expect("trimmed config builds a client");
        assert_eq!(client.url, "http://jellyfin:8096");
        assert_eq!(client.mount_path, "/media");

        // A whitespace-only or missing value is treated as unset → None.
        assert!(
            JellyfinClient::from_values(some("   "), some("abc123"), some("/media")).is_none(),
            "a whitespace-only JELLYFIN_URL must be treated as unset"
        );
        assert!(
            JellyfinClient::from_values(some("http://jellyfin:8096"), some(" "), some("/media"))
                .is_none(),
            "a whitespace-only JELLYFIN_API_KEY must be treated as unset"
        );
        assert!(
            JellyfinClient::from_values(some("http://jellyfin:8096"), some("abc123"), None)
                .is_none(),
            "an unset JELLYFIN_RCLONE_MOUNT_PATH disables the client"
        );
        // An invalid value (here a schemeless URL) disables the client rather than panicking.
        assert!(
            JellyfinClient::from_values(some("jellyfin:8096"), some("abc123"), some("/media"))
                .is_none(),
            "an invalid JELLYFIN_URL disables the client"
        );
    }

    /// Loopback that replies `fail_status` (empty body) on the first `fail_n` connections, then 200,
    /// counting every request.
    async fn spawn_counting_server(fail_status: u16, fail_n: usize) -> (String, Arc<AtomicUsize>) {
        let server = serve(
            vec![Step::status(fail_status); fail_n],
            Some(Step::status(200)),
        )
        .await;
        (server.base, server.connections)
    }

    fn one_change() -> Vec<VfsChange> {
        vec![VfsChange {
            path: "Movies/Example (2020)".to_string(),
            update_type: UpdateType::Created,
        }]
    }

    #[tokio::test(start_paused = true)]
    async fn notify_changes_retries_a_transient_503_then_succeeds() {
        // The send/retry loop is the only nontrivial runtime logic here. A transient 503 must be
        // retried and recover on the next 200 — two requests total. `start_paused` auto-advances the
        // NOTIFICATION_DELAY + RETRY_DELAY sleeps so the test is instant.
        use std::sync::atomic::Ordering;
        let (url, counter) = spawn_counting_server(503, 1).await;
        let client = JellyfinClient::new(url, "key".to_string(), "/media".to_string()).unwrap();
        client.notify_changes(&one_change()).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "a transient 503 then a 200 = two requests"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn notify_changes_gives_up_on_a_500_without_retrying() {
        // A 500 is GiveUp (a retry won't help): exactly one request, no retry storm.
        use std::sync::atomic::Ordering;
        let (url, counter) = spawn_counting_server(500, 100).await;
        let client = JellyfinClient::new(url, "key".to_string(), "/media".to_string()).unwrap();
        client.notify_changes(&one_change()).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "a 500 is not retried — exactly one request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn notify_changes_stops_after_max_send_attempts_on_a_persistent_503() {
        // A persistently-transient upstream (always 503) must be retried EXACTLY MAX_SEND_ATTEMPTS
        // times and then stop — guarding the loop bound against an off-by-one or an early return in
        // the Retry arm that would change the total request count.
        use std::sync::atomic::Ordering;
        let (url, counter) = spawn_counting_server(503, 1000).await;
        let client = JellyfinClient::new(url, "key".to_string(), "/media".to_string()).unwrap();
        client.notify_changes(&one_change()).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            MAX_SEND_ATTEMPTS as usize,
            "a persistent transient error is retried exactly MAX_SEND_ATTEMPTS times"
        );
    }

    /// Loopback that, on the first `hang_n` connections, reads the request then holds the connection
    /// open WITHOUT responding (the client times out → a transient transport error) before serving a
    /// 200 on the next connection. Counts every accepted connection.
    async fn spawn_timeout_then_200(hang_n: usize) -> (String, Arc<AtomicUsize>) {
        let server = serve(vec![Step::HangUntilClosed; hang_n], Some(Step::status(200))).await;
        (server.base, server.connections)
    }

    #[tokio::test(start_paused = true)]
    async fn notify_changes_retries_a_transient_timeout_then_succeeds() {
        // A client timeout (Jellyfin slow/overloaded) is transient, so the notification is retried
        // and recovers on the next 200. Each scan tick sends its change set once, so a notification
        // given up on is lost. `start_paused` auto-advances the reqwest request timeout so it is
        // instant.
        use std::sync::atomic::Ordering;
        let (url, counter) = spawn_timeout_then_200(1).await;
        let client = JellyfinClient::new(url, "key".to_string(), "/media".to_string()).unwrap();
        client.notify_changes(&one_change()).await;
        // ≥2 connections proves the timeout was RETRIED past the hang (the exact count is left loose:
        // reqwest's connect/pool behaviour under a paused clock can add a connection). Giving up on
        // a timeout would stop at the first hang = 1.
        assert!(
            counter.load(Ordering::SeqCst) >= 2,
            "a transient timeout must be retried (>=2 connections), got {}",
            counter.load(Ordering::SeqCst)
        );
    }

    /// `notify_changes`' retry guard relies on reqwest 0.12 reporting connect failures and the
    /// client timeout as request errors too (`is_request()`), with `is_connect()` / `is_timeout()`
    /// only picking the log label. A reqwest bump that changes either classification shows up here.
    #[tokio::test(start_paused = true)]
    async fn reqwest_connect_and_timeout_errors_are_request_errors() {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();

        let (url, _) = spawn_timeout_then_200(1).await;
        let e = http.post(&url).send().await.expect_err("the server hangs");
        assert!(e.is_timeout(), "{e:?}");
        assert!(e.is_request(), "a timeout is a request error: {e:?}");

        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);
        let e = http
            .post(format!("http://{addr}"))
            .send()
            .await
            .expect_err("nothing listens");
        assert!(e.is_connect(), "{e:?}");
        assert!(
            e.is_request(),
            "a connect failure is a request error: {e:?}"
        );
    }

    /// A transient send error is logged with its cause. reqwest's Display names only the error kind
    /// and the URL, so a mistyped host, a refused port and a TLS failure would all read "cannot
    /// connect". The give-up line names the last cause too.
    #[tokio::test(start_paused = true)]
    async fn notify_changes_logs_the_cause_of_a_transient_send_error() {
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        // The innermost cause reqwest reports when nothing listens (e.g. "Connection refused (os
        // error 111)"), read from reqwest itself so the test doesn't depend on the OS's wording.
        let probe = reqwest::Client::new()
            .post(&url)
            .send()
            .await
            .expect_err("nothing listens");
        let cause = std::iter::successors(Some(&probe as &dyn std::error::Error), |e| e.source())
            .last()
            .unwrap()
            .to_string();
        let client = JellyfinClient::new(url, "key".to_string(), "/media".to_string()).unwrap();
        let ((), logs) = crate::test_log::capture_async(
            tracing::Level::WARN,
            client.notify_changes(&one_change()),
        )
        .await;
        let attempts: Vec<&str> = logs
            .lines()
            .filter(|l| l.contains("Transient Jellyfin error"))
            .collect();
        assert_eq!(attempts.len(), MAX_SEND_ATTEMPTS as usize, "{logs}");
        assert!(
            attempts.iter().all(|l| l.contains(&cause)),
            "every attempt must name the cause {cause:?}:\n{logs}"
        );
        let give_up = logs
            .lines()
            .find(|l| l.contains("Jellyfin notification failed after"))
            .unwrap_or_else(|| panic!("no give-up line:\n{logs}"));
        assert!(
            give_up.contains(&cause),
            "the give-up line must name the last cause {cause:?}: {give_up}"
        );
    }

    /// Loopback that, on the first `drop_n` connections, reads the request then closes the socket
    /// without answering (Jellyfin restarting mid-request), then serves 200. Counts every accepted
    /// connection.
    async fn spawn_drop_then_200(drop_n: usize) -> (String, Arc<AtomicUsize>) {
        // Closing without a response ends the connection before any response arrives: a request
        // error that is neither a connect error nor a timeout.
        let server = serve(vec![Step::DropAfterRead; drop_n], Some(Step::status(200))).await;
        (server.base, server.connections)
    }

    #[tokio::test(start_paused = true)]
    async fn notify_changes_retries_a_connection_dropped_mid_request() {
        // Jellyfin restarting after it accepted the connection and read the request closes the
        // connection with no response. Each scan tick sends its change set once, so only a retry
        // delivers it.
        use std::sync::atomic::Ordering;
        let (url, counter) = spawn_drop_then_200(1).await;
        let client = JellyfinClient::new(url, "key".to_string(), "/media".to_string()).unwrap();
        client.notify_changes(&one_change()).await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "a dropped connection then a 200 = two requests"
        );
    }
}
