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

pub struct JellyfinClient {
    url: String,
    /// Pre-built sensitive header value so the key is never printed in debug output.
    api_key_header: HeaderValue,
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
        let mut api_key_header = HeaderValue::from_str(&api_key).map_err(|e| {
            format!(
                "Jellyfin API key contains invalid HTTP header characters: {}",
                e
            )
        })?;
        api_key_header.set_sensitive(true);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("Failed to build Jellyfin HTTP client: {}", e))?;
        Ok(Self {
            url,
            api_key_header,
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

        for attempt in 0..MAX_SEND_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }

            let result = self
                .http
                .post(&url)
                .header("X-Emby-Token", self.api_key_header.clone())
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
                Err(e) if e.is_connect() || e.is_timeout() => {
                    // Connect failures (not started yet) AND timeouts (slow/overloaded Jellyfin)
                    // are both transient — retry. Only genuinely non-transient errors give up.
                    tracing::warn!(
                        "Transient Jellyfin error ({}), attempt {}/{}",
                        if e.is_timeout() {
                            "timeout"
                        } else {
                            "cannot connect"
                        },
                        attempt + 1,
                        MAX_SEND_ATTEMPTS
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Failed to notify Jellyfin: {}", e);
                    return;
                }
            }
        }

        tracing::warn!(
            "Jellyfin notification failed after {} attempts",
            MAX_SEND_ATTEMPTS
        );
    }

    /// Try to create a JellyfinClient from environment variables.
    /// Returns None if any of the required env vars are missing or invalid.
    pub fn from_env() -> Option<Self> {
        // Trim and treat blank-as-unset for every value, matching `Config`'s env handling — a
        // whitespace-only value (e.g. from quoted compose YAML) must not build a client that POSTs
        // to a malformed URL at runtime.
        let url = std::env::var("JELLYFIN_URL").ok()?.trim().to_string();
        let api_key = std::env::var("JELLYFIN_API_KEY").ok()?.trim().to_string();
        let mount_path = std::env::var("JELLYFIN_RCLONE_MOUNT_PATH")
            .ok()?
            .trim()
            .to_string();

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
    fn notify_changes_skips_empty() {
        // Just verify it doesn't panic on empty input
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
    fn from_env_trims_values_and_treats_blank_as_unset() {
        // Use uniquely-scoped restore so this test doesn't disturb a real environment if one is set.
        let restore = |k: &str, v: Option<String>| match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        };
        let prev = (
            std::env::var("JELLYFIN_URL").ok(),
            std::env::var("JELLYFIN_API_KEY").ok(),
            std::env::var("JELLYFIN_RCLONE_MOUNT_PATH").ok(),
        );

        // Whitespace-padded values must be trimmed (and not build a malformed-URL client).
        std::env::set_var("JELLYFIN_URL", "  http://jellyfin:8096  ");
        std::env::set_var("JELLYFIN_API_KEY", "  abc123  ");
        std::env::set_var("JELLYFIN_RCLONE_MOUNT_PATH", "  /media  ");
        let client = JellyfinClient::from_env().expect("trimmed config builds a client");
        assert_eq!(client.url, "http://jellyfin:8096");
        assert_eq!(client.mount_path, "/media");

        // A whitespace-only value is treated as unset → None.
        std::env::set_var("JELLYFIN_URL", "   ");
        assert!(
            JellyfinClient::from_env().is_none(),
            "a whitespace-only JELLYFIN_URL must be treated as unset"
        );

        restore("JELLYFIN_URL", prev.0);
        restore("JELLYFIN_API_KEY", prev.1);
        restore("JELLYFIN_RCLONE_MOUNT_PATH", prev.2);
    }

    /// Loopback that replies `fail_status` (empty body) on the first `fail_n` connections, then 200,
    /// counting every request.
    async fn spawn_counting_server(
        fail_status: u16,
        fail_n: usize,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let counter = Arc::new(AtomicUsize::new(0));
        let c2 = counter.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut seen = 0usize;
            while let Ok((mut sock, _)) = listener.accept().await {
                c2.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let status = if seen < fail_n {
                    seen += 1;
                    fail_status
                } else {
                    200
                };
                let head = format!(
                    "HTTP/1.1 {} STATUS\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    status
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}", addr), counter)
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
    async fn notify_changes_retries_forever_up_to_the_attempt_cap() {
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
    async fn spawn_timeout_then_200(
        hang_n: usize,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let counter = Arc::new(AtomicUsize::new(0));
        let c2 = counter.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut seen = 0usize;
            while let Ok((mut sock, _)) = listener.accept().await {
                c2.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                if seen < hang_n {
                    seen += 1;
                    // Never respond: block reading until the client times out and drops the socket
                    // (read then returns EOF), so this connection produces an is_timeout() error.
                    let mut sink = [0u8; 1];
                    let _ = sock.read(&mut sink).await;
                    continue;
                }
                let head = "HTTP/1.1 200 STATUS\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}", addr), counter)
    }

    #[tokio::test(start_paused = true)]
    async fn notify_changes_retries_a_transient_timeout_then_succeeds() {
        // A transport-level timeout (Jellyfin slow/overloaded) hits the is_connect()||is_timeout()
        // retry branch — the reason that branch exists — and must recover on the next 200. Guards
        // against a regression narrowing that guard (which would silently drop notifications during a
        // Jellyfin restart). `start_paused` auto-advances the reqwest request timeout so it is instant.
        use std::sync::atomic::Ordering;
        let (url, counter) = spawn_timeout_then_200(1).await;
        let client = JellyfinClient::new(url, "key".to_string(), "/media".to_string()).unwrap();
        client.notify_changes(&one_change()).await;
        // ≥2 connections proves the timeout was RETRIED past the hang (the exact count is left loose:
        // reqwest's connect/pool behaviour under a paused clock can add a connection). A regression
        // that narrowed the is_connect()||is_timeout() guard would give up after the first hang = 1.
        assert!(
            counter.load(Ordering::SeqCst) >= 2,
            "a transient timeout must be retried (>=2 connections), got {}",
            counter.load(Ordering::SeqCst)
        );
    }
}
