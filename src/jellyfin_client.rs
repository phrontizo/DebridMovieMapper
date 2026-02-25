use crate::vfs::{VfsChange, UpdateType};
use std::time::Duration;
use tracing::info;

const MAX_RETRIES: u32 = 10;
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// Delay before sending notification to let rclone's directory cache expire.
/// Must be longer than rclone's --dir-cache-time (default 10s in compose.yml).
const NOTIFICATION_DELAY: Duration = Duration::from_secs(15);

pub struct JellyfinClient {
    url: String,
    api_key: String,
    mount_path: String,
    http: reqwest::Client,
}

impl JellyfinClient {
    pub fn new(url: String, api_key: String, mount_path: String) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            api_key,
            mount_path: mount_path.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
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
            changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>().join(", ")
        );

        // Wait for rclone's directory cache to expire so Jellyfin sees fresh data
        // when it checks the filesystem after receiving our notification.
        tokio::time::sleep(NOTIFICATION_DELAY).await;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(RETRY_DELAY).await;
            }

            let result = self.http
                .post(&url)
                .header("X-Emby-Token", &self.api_key)
                .json(&body)
                .send()
                .await;

            match result {
                Ok(response) => {
                    if response.status().is_success() {
                        info!("Jellyfin notified successfully");
                        return;
                    }
                    let status = response.status();
                    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                        tracing::warn!(
                            "Jellyfin returned 503 (still starting?), retry {}/{}",
                            attempt + 1,
                            MAX_RETRIES
                        );
                        continue;
                    }
                    tracing::warn!(
                        "Jellyfin notification returned status {}: {}",
                        status,
                        response.text().await.unwrap_or_default()
                    );
                    return;
                }
                Err(e) if e.is_connect() => {
                    tracing::warn!(
                        "Cannot connect to Jellyfin (not started?), retry {}/{}",
                        attempt + 1,
                        MAX_RETRIES
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Failed to notify Jellyfin: {}", e);
                    return;
                }
            }
        }

        tracing::warn!("Jellyfin notification failed after {} retries", MAX_RETRIES);
    }

    /// Try to create a JellyfinClient from environment variables.
    /// Returns None if any of the required env vars are missing.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("JELLYFIN_URL").ok()?;
        let api_key = std::env::var("JELLYFIN_API_KEY").ok()?;
        let mount_path = std::env::var("JELLYFIN_RCLONE_MOUNT_PATH").ok()?;

        if url.is_empty() || api_key.is_empty() || mount_path.is_empty() {
            return None;
        }

        Some(Self::new(url, api_key, mount_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_body_single_created() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "test-key".to_string(),
            "/mnt/debrid".to_string(),
        );
        let changes = vec![VfsChange {
            path: "Shows/Breaking Bad/Season 03".to_string(),
            update_type: UpdateType::Created,
        }];
        let body = client.build_request_body(&changes);
        let updates = body["Updates"].as_array().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["Path"], "/mnt/debrid/Shows/Breaking Bad/Season 03");
        assert_eq!(updates[0]["UpdateType"], "Created");
    }

    #[test]
    fn build_request_body_multiple_changes() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "test-key".to_string(),
            "/mnt/debrid".to_string(),
        );
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
        assert_eq!(updates[2]["Path"], "/mnt/debrid/Shows/Breaking Bad/Season 01");
    }

    #[test]
    fn build_request_body_trims_trailing_slashes() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096/".to_string(),
            "test-key".to_string(),
            "/mnt/debrid/".to_string(),
        );
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
        );
        let body = client.build_request_body(&[]);
        let updates = body["Updates"].as_array().unwrap();
        assert!(updates.is_empty());
    }
}
