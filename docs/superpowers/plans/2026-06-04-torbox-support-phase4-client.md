# TorBox Support — Phase 4: TorBox Client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement `DebridProvider` for TorBox in a new `src/torbox_client.rs`, against the live-verified TorBox API, and wire it into startup selection so `TORBOX_API_KEY` actually runs the service against TorBox.

**Architecture:** `TorBoxClient` holds a `reqwest::Client` (default `Authorization: Bearer` header), the API key (for `requestdl`'s `token` query param), a shared `AdaptiveRateLimiter`, and a resolve-URL cache (keyed by `(torrent_id, file_id)`, ~3h TTL — TorBox download links expire in ~3h). Raw TorBox JSON types (`Tb*`) are deserialized from the `{success, detail, data}` envelope and mapped to the canonical `Torrent`/`TorrentInfo`/`TorrentFile` by pure functions (unit-tested from captured JSON). TorBox resolves a file by `(torrent_id, file_id)` via `requestdl` (no per-file link → `FileLocator.link = None`); `select_files` is a no-op (TorBox auto-selects); status is normalised so `download_finished → "downloaded"` (so owned-but-uncached/Inactive items still appear in the library and re-acquire on playback).

**Tech Stack:** Rust 2021, `async-trait`, `tokio`, `reqwest` (multipart needed — see Task 0), `serde`.

## Verified TorBox API (from live capture)
- Base `https://api.torbox.app/v1/api`; auth header `Authorization: Bearer <key>`. Envelope: `{ "success": bool, "detail": str, "data": ..., "error": str? }`.
- `GET /torrents/mylist?bypass_cache=true` → `data` = **array** of torrents.
- `GET /torrents/mylist?id=<int>&bypass_cache=true` → `data` = **single object** torrent.
- Torrent fields: `id` (int), `hash`, `name`, `size`, `download_finished` (bool), `download_present` (bool), `cached` (bool), `download_state` (str), `files` (array). File fields: `id` (int, arbitrary), `name` (full path e.g. `"Sintel/Sintel.mp4"`), `short_name`, `size`, `mimetype`.
- `POST /torrents/createtorrent` (multipart, field `magnet`) → `data` = `{ hash, torrent_id (int), auth_id }`. Cached magnet returns instantly.
- `GET /torrents/requestdl?token=<key>&torrent_id=<int>&file_id=<int>` → `data` = URL **string** on success; on failure `success:false, data:null, error:"..."`.
- `POST /torrents/controltorrent` (JSON `{ "torrent_id": <int>, "operation": "delete" }`) → `{success, detail}`.

---

## Scope & non-goals
In scope: `src/torbox_client.rs` (full `DebridProvider`), extracting `AdaptiveRateLimiter` to share it, `main.rs` wiring, unit tests, and a live-verification pass. Out of scope: the formal TorBox integration tests and the cross-provider add/appears/delete/disappears lifecycle test (**Phase 5**), and docs polish (Phase 5; a short note is added here).

**Definition of done:** `cargo build`/`clippy` warning-free; `cargo test` green; running with `TORBOX_API_KEY` set starts the service against TorBox; a manual live pass confirms `get_torrents`/`get_torrent_info`/`resolve_url`/`add_magnet`/`delete_torrent` work end-to-end.

## File structure
| File | Change |
|------|--------|
| `Cargo.toml` | enable reqwest `multipart` feature |
| `src/ratelimit.rs` *(new)* | `AdaptiveRateLimiter` moved here (was private in `rd_client.rs`) + its tests |
| `src/rd_client.rs` | use `crate::ratelimit::AdaptiveRateLimiter` |
| `src/torbox_client.rs` *(new)* | `TorBoxClient` + raw types + mapping + `impl DebridProvider` |
| `src/mapper.rs` | declare `ratelimit` + `torbox_client` modules |
| `src/main.rs` | `ProviderKind::TorBox` → construct `TorBoxClient` |

---

## Task 0: Enable reqwest multipart

**Files:** `Cargo.toml`

- [ ] **Step 1:** In `Cargo.toml`, change the `reqwest` dependency features to include `"multipart"`:
  ```toml
  reqwest = { version = "0.12", features = ["json", "rustls-tls", "multipart"], default-features = false }
  ```
- [ ] **Step 2:** `cargo build` (must succeed).
- [ ] **Step 3:** Commit:
  ```bash
  git add Cargo.toml Cargo.lock
  git commit -m "build: enable reqwest multipart for TorBox createtorrent"
  ```

---

## Task 1: Extract `AdaptiveRateLimiter` into `src/ratelimit.rs`

Move the limiter so both clients share it (DRY). Pure mechanical move; behaviour unchanged.

**Files:** `src/ratelimit.rs` *(new)*, `src/rd_client.rs`, `src/mapper.rs`

- [ ] **Step 1:** Create `src/ratelimit.rs`. MOVE from `src/rd_client.rs` into it: the constants `MIN_INTERVAL_MS`, `MAX_INTERVAL_MS`, `RECOVERY_MS`, `MAX_RETRY_AFTER_SECS`; the `RateLimiterState` struct; the `AdaptiveRateLimiter` struct, its `Debug` impl, and its `impl` block. Make `AdaptiveRateLimiter` and its methods `pub`/`pub(crate)` as needed (the methods `wait_for_token`, `record_success`, `record_throttle` must be reachable from `rd_client` and `torbox_client` — make them `pub`). Add `use std::time::Duration; use tokio;` imports as required. MOVE the limiter unit tests (`adaptive_limiter_*`, `retry_after_cap_constant`) from `rd_client.rs`'s `mod tests` into a `#[cfg(test)] mod tests` in `ratelimit.rs` (they reference `MIN_INTERVAL_MS`/`MAX_INTERVAL_MS` and the limiter's private `state` — keep them in the same module so they still compile).

- [ ] **Step 2:** In `src/mapper.rs`, add `pub mod ratelimit;` (alphabetical — before `rd_client`).

- [ ] **Step 3:** In `src/rd_client.rs`, delete the moved items and add `use crate::ratelimit::AdaptiveRateLimiter;`. `RealDebridClient` still has `rate_limiter: Arc<AdaptiveRateLimiter>` and calls `self.rate_limiter.wait_for_token()/record_success()/record_throttle()` — these now resolve to the `pub` methods. Keep `MAX_RETRY_AFTER_SECS` usage in `rd_client`'s `wait_for_retry` working: if `wait_for_retry`/`fetch_with_retry` reference `MAX_RETRY_AFTER_SECS`, import it (`use crate::ratelimit::MAX_RETRY_AFTER_SECS;` — make that constant `pub`).

- [ ] **Step 4:** `cargo build` (warning-free) and `cargo test --lib` (all green — the limiter tests now run under `ratelimit::tests`).

- [ ] **Step 5:** Commit:
  ```bash
  git add src/ratelimit.rs src/rd_client.rs src/mapper.rs
  git commit -m "refactor: extract AdaptiveRateLimiter into shared ratelimit module"
  ```

---

## Task 2: TorBox raw types + pure mapping functions

The novel logic. Pure functions are fully unit-testable from the captured JSON; the HTTP methods (Task 3) call them.

**Files:** `src/torbox_client.rs` *(new)*, `src/mapper.rs`

- [ ] **Step 1: Write the failing test**

Create `src/torbox_client.rs` with the raw types, the mapping functions, and this test module:

```rust
use crate::rd_client::{Torrent, TorrentFile, TorrentInfo};
use serde::Deserialize;

const TORBOX_BASE: &str = "https://api.torbox.app/v1/api";

/// TorBox `{success, detail, data}` response envelope.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    detail: Option<String>,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct TbFile {
    #[serde(default)]
    id: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct TbTorrent {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    hash: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    download_finished: bool,
    #[serde(default)]
    download_state: String,
    #[serde(default)]
    files: Vec<TbFile>,
}

#[derive(Debug, Deserialize)]
struct TbCreate {
    #[serde(default)]
    torrent_id: i64,
    #[serde(default)]
    hash: String,
}

/// Normalised status: a finished download (cached OR Inactive/uncached but owned) maps to
/// "downloaded" so it appears in the library and re-acquires on playback; otherwise the raw
/// download_state (e.g. "downloading") is kept so the scan loop excludes not-yet-ready items.
fn tb_status(t: &TbTorrent) -> String {
    if t.download_finished {
        "downloaded".to_string()
    } else {
        t.download_state.clone()
    }
}

fn to_torrent_file(f: &TbFile) -> TorrentFile {
    TorrentFile {
        id: f.id,
        path: f.name.clone(),
        bytes: f.size,
        selected: 1,
    }
}

/// Map a TorBox torrent to the lightweight canonical `Torrent` (no files).
fn to_torrent(t: &TbTorrent) -> Torrent {
    Torrent {
        id: t.id.to_string(),
        filename: t.name.clone(),
        hash: t.hash.clone(),
        bytes: t.size,
        status: tb_status(t),
        added: String::new(),
        links: Vec::new(),
        ended: None,
        ..Default::default()
    }
}

/// Map a TorBox torrent to the full canonical `TorrentInfo` (with files; no per-file links).
fn to_torrent_info(t: &TbTorrent) -> TorrentInfo {
    TorrentInfo {
        id: t.id.to_string(),
        filename: t.name.clone(),
        hash: t.hash.clone(),
        bytes: t.size,
        status: tb_status(t),
        added: String::new(),
        files: t.files.iter().map(to_torrent_file).collect(),
        links: Vec::new(),
        ended: None,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real shapes captured live from the TorBox API (Sintel).
    const MYLIST_ITEM: &str = r#"{
        "id": 35821241, "hash": "08ada5a7a6183aae1e09d831df6748d566095a10",
        "name": "Sintel", "size": 129302391,
        "download_finished": true, "download_present": true, "cached": true,
        "download_state": "cached",
        "files": [
            {"id": 10, "name": "Sintel/Sintel.mp4", "short_name": "Sintel.mp4", "size": 129241752, "mimetype": "video/mp4"},
            {"id": 4, "name": "Sintel/poster.jpg", "short_name": "poster.jpg", "size": 46115, "mimetype": "image/jpeg"}
        ]
    }"#;

    #[test]
    fn maps_mylist_item_to_torrent_info() {
        let t: TbTorrent = serde_json::from_str(MYLIST_ITEM).unwrap();
        let info = to_torrent_info(&t);
        assert_eq!(info.id, "35821241");
        assert_eq!(info.hash, "08ada5a7a6183aae1e09d831df6748d566095a10");
        assert_eq!(info.status, "downloaded"); // download_finished → downloaded
        assert_eq!(info.bytes, 129302391);
        assert!(info.links.is_empty()); // TorBox has no per-file link array
        assert_eq!(info.files.len(), 2);
        let mp4 = info.files.iter().find(|f| f.path.ends_with(".mp4")).unwrap();
        assert_eq!(mp4.id, 10);
        assert_eq!(mp4.path, "Sintel/Sintel.mp4");
        assert_eq!(mp4.selected, 1);
    }

    #[test]
    fn maps_to_lightweight_torrent() {
        let t: TbTorrent = serde_json::from_str(MYLIST_ITEM).unwrap();
        let lt = to_torrent(&t);
        assert_eq!(lt.id, "35821241");
        assert_eq!(lt.status, "downloaded");
        assert!(lt.links.is_empty());
    }

    #[test]
    fn unfinished_torrent_keeps_raw_state() {
        let json = r#"{"id":1,"hash":"h","name":"x","size":0,"download_finished":false,"download_state":"downloading","files":[]}"#;
        let t: TbTorrent = serde_json::from_str(json).unwrap();
        assert_eq!(tb_status(&t), "downloading");
    }

    #[test]
    fn envelope_parses_array_and_object() {
        let arr: Envelope<Vec<TbTorrent>> =
            serde_json::from_str(&format!(r#"{{"success":true,"detail":"ok","data":[{}]}}"#, MYLIST_ITEM)).unwrap();
        assert!(arr.success);
        assert_eq!(arr.data.unwrap().len(), 1);
        let obj: Envelope<TbTorrent> =
            serde_json::from_str(&format!(r#"{{"success":true,"data":{}}}"#, MYLIST_ITEM)).unwrap();
        assert_eq!(obj.data.unwrap().id, 35821241);
        let fail: Envelope<String> =
            serde_json::from_str(r#"{"success":false,"detail":"err","data":null}"#).unwrap();
        assert!(!fail.success);
        assert!(fail.data.is_none());
    }
}
```

Add `pub mod torbox_client;` to `src/mapper.rs` (alphabetical — after `tmdb_client`? No: after `tasks`, before `tmdb_client` is wrong alphabetically; place `torbox_client` after `tmdb_client`). Use correct alphabetical order: `... tasks; tmdb_client; torbox_client; vfs;`.

- [ ] **Step 2:** Run `cargo test --lib torbox_client` → the four mapping/envelope tests should PASS once the file compiles. (If `Torrent`/`TorrentInfo`/`TorrentFile` need more `..Default::default()` fields, they already derive `Default` — these struct literals rely on it.)

- [ ] **Step 3:** `cargo build` warning-free. NOTE: at this point the HTTP client struct doesn't exist yet and `DebridProvider` is not implemented — that's fine, this task is the pure mapping layer only. The `Envelope`/`TbCreate`/`download_present`/`cached` items may be unused until Task 3; add `#[allow(dead_code)]` to any item that triggers an unused warning, OR (preferred) leave them out until Task 3 if they warn. Keep the build warning-free.

- [ ] **Step 4:** Commit:
  ```bash
  git add src/torbox_client.rs src/mapper.rs
  git commit -m "feat: TorBox raw types and pure canonical mapping functions"
  ```

---

## Task 3: `TorBoxClient` HTTP methods (inherent)

Add the client struct, constructor, request helper, resolve cache, and inherent async methods that call the API and the Task-2 mappers. Mirror `RealDebridClient`'s structure (HTTP client with default auth header, `AdaptiveRateLimiter`, a cache) — read `src/rd_client.rs` for the pattern.

**Files:** `src/torbox_client.rs`

- [ ] **Step 1: Write failing tests** (cache behaviour — no network)

Add to `torbox_client.rs`'s `mod tests`:

```rust
#[tokio::test]
async fn resolve_cache_invalidate_and_evict() {
    let client = TorBoxClient::new("fake".to_string()).unwrap();
    let loc = crate::provider::FileLocator {
        torrent_id: "1".to_string(),
        file_id: 10,
        ..Default::default()
    };
    client.cache_put(&loc, "https://cdn/x".to_string()).await;
    assert_eq!(client.cache_get(&loc).await.as_deref(), Some("https://cdn/x"));
    client.invalidate_locator(&loc).await;
    assert!(client.cache_get(&loc).await.is_none());
}

#[test]
fn torbox_client_constructs() {
    let c = TorBoxClient::new("fake".to_string()).unwrap();
    assert_eq!(c.provider_name(), "torbox");
}
```

- [ ] **Step 2:** Run → FAIL (no `TorBoxClient`).

- [ ] **Step 3: Implement the client.** Add to `torbox_client.rs` (above `mod tests`). Add imports at the top: `use crate::error::AppError; use crate::provider::FileLocator; use crate::ratelimit::AdaptiveRateLimiter; use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION}; use std::collections::HashMap; use std::sync::Arc; use std::time::{Duration, Instant}; use tokio::sync::RwLock; use tracing::{info, warn};`

```rust
const RESOLVE_CACHE_TTL: Duration = Duration::from_secs(3 * 3600); // TorBox links last ~3h

#[derive(Clone)]
struct CachedUrl {
    url: String,
    at: Instant,
}

#[derive(Debug)]
pub struct TorBoxClient {
    client: reqwest::Client,
    api_key: String,
    rate_limiter: Arc<AdaptiveRateLimiter>,
    resolve_cache: Arc<RwLock<HashMap<(String, u32), CachedUrl>>>,
}

impl std::fmt::Debug for CachedUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedUrl").finish()
    }
}

impl TorBoxClient {
    pub fn new(api_key: String) -> Result<Self, AppError> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", api_key))
            .map_err(|e| AppError::Config(format!("Invalid TorBox API key for header: {}", e)))?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(format!("DebridMovieMapper/{}", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| AppError::Config(format!("Failed to build TorBox HTTP client: {}", e)))?;
        Ok(Self {
            client,
            api_key,
            rate_limiter: Arc::new(AdaptiveRateLimiter::new()),
            resolve_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn provider_name(&self) -> &'static str {
        "torbox"
    }

    async fn cache_get(&self, loc: &FileLocator) -> Option<String> {
        let key = (loc.torrent_id.clone(), loc.file_id);
        let cache = self.resolve_cache.read().await;
        cache.get(&key).and_then(|c| {
            if c.at.elapsed() < RESOLVE_CACHE_TTL {
                Some(c.url.clone())
            } else {
                None
            }
        })
    }

    async fn cache_put(&self, loc: &FileLocator, url: String) {
        let key = (loc.torrent_id.clone(), loc.file_id);
        self.resolve_cache
            .write()
            .await
            .insert(key, CachedUrl { url, at: Instant::now() });
    }

    async fn invalidate_locator(&self, loc: &FileLocator) {
        let key = (loc.torrent_id.clone(), loc.file_id);
        self.resolve_cache.write().await.remove(&key);
    }

    async fn evict_expired(&self) {
        let mut cache = self.resolve_cache.write().await;
        cache.retain(|_, c| c.at.elapsed() < RESOLVE_CACHE_TTL);
    }

    /// Send a GET/POST, rate-limited with one 429 retry, returning the envelope's `data`.
    /// Synthesises a Bad Gateway reqwest error when `success` is false or `data` is missing.
    async fn send_data<T, F>(&self, make: F) -> Result<T, reqwest::Error>
    where
        T: serde::de::DeserializeOwned,
        F: Fn() -> reqwest::RequestBuilder,
    {
        for attempt in 1..=6u32 {
            self.rate_limiter.wait_for_token().await;
            let resp = match make().send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt < 6 {
                        warn!("TorBox request error (attempt {}): {}", attempt, e);
                        continue;
                    }
                    return Err(e);
                }
            };
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());
                self.rate_limiter.record_throttle(retry_after).await;
                warn!("TorBox 429 (attempt {})", attempt);
                continue;
            }
            let resp = resp.error_for_status()?;
            let text = resp.text().await?;
            self.rate_limiter.record_success().await;
            match serde_json::from_str::<Envelope<T>>(&text) {
                Ok(env) if env.success => {
                    if let Some(data) = env.data {
                        return Ok(data);
                    }
                    warn!("TorBox response success but no data: {:.160}", text);
                }
                Ok(env) => {
                    warn!("TorBox response not success: {:?} body {:.160}", env.detail, text);
                }
                Err(e) => {
                    warn!("TorBox decode failed: {} body {:.160}", e, text);
                }
            }
            // Fall through to a synthetic error on the final attempt.
            if attempt == 6 {
                break;
            }
        }
        let synthetic = reqwest::Response::from(
            hyper::Response::builder()
                .status(reqwest::StatusCode::BAD_GATEWAY)
                .body(hyper::body::Bytes::from_static(b"torbox request failed"))
                .unwrap(),
        );
        Err(synthetic.error_for_status().unwrap_err())
    }

    pub async fn list_torrents_raw(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        let url = format!("{}/torrents/mylist?bypass_cache=true", TORBOX_BASE);
        let raw: Vec<TbTorrent> = self.send_data(|| self.client.get(&url)).await?;
        Ok(raw.iter().map(to_torrent).collect())
    }

    pub async fn torrent_info_raw(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        let url = format!("{}/torrents/mylist?id={}&bypass_cache=true", TORBOX_BASE, id);
        let raw: TbTorrent = self.send_data(|| self.client.get(&url)).await?;
        Ok(to_torrent_info(&raw))
    }

    pub async fn add_magnet_raw(
        &self,
        magnet: &str,
    ) -> Result<crate::rd_client::AddMagnetResponse, reqwest::Error> {
        let url = format!("{}/torrents/createtorrent", TORBOX_BASE);
        let magnet = magnet.to_string();
        let created: TbCreate = self
            .send_data(|| {
                let form = reqwest::multipart::Form::new().text("magnet", magnet.clone());
                self.client.post(&url).multipart(form)
            })
            .await?;
        Ok(crate::rd_client::AddMagnetResponse {
            id: created.torrent_id.to_string(),
            uri: magnet,
        })
    }

    pub async fn delete_torrent_raw(&self, id: &str) -> Result<(), reqwest::Error> {
        let url = format!("{}/torrents/controltorrent", TORBOX_BASE);
        let torrent_id: i64 = id.parse().unwrap_or(0);
        let body = serde_json::json!({ "torrent_id": torrent_id, "operation": "delete" });
        // controltorrent returns {success, detail} with no data — accept any 2xx.
        let _: serde_json::Value = self
            .send_data(|| self.client.post(&url).json(&body))
            .await
            .or_else(|e| {
                // success-with-no-data synthesises an error in send_data; treat 2xx delete as ok
                if e.status().is_some() {
                    Ok(serde_json::Value::Null)
                } else {
                    Err(e)
                }
            })?;
        Ok(())
    }

    pub async fn resolve_locator(&self, loc: &FileLocator) -> Result<String, AppError> {
        if let Some(url) = self.cache_get(loc).await {
            return Ok(url);
        }
        let url = format!(
            "{}/torrents/requestdl?token={}&torrent_id={}&file_id={}",
            TORBOX_BASE, self.api_key, loc.torrent_id, loc.file_id
        );
        match self.send_data::<String, _>(|| self.client.get(&url)).await {
            Ok(cdn) => {
                self.cache_put(loc, cdn.clone()).await;
                Ok(cdn)
            }
            Err(_) => {
                // requestdl failure (success:false / no data / HTTP error) means the file's
                // bytes are not currently available → signal re-acquire.
                info!(
                    "TorBox requestdl unavailable for torrent {} file {}",
                    loc.torrent_id, loc.file_id
                );
                Err(AppError::Unavailable)
            }
        }
    }
}
```

> NOTE on `delete_torrent_raw`: `controltorrent`'s response has no `data`, so `send_data` would synthesise an error for an otherwise-successful call. The `.or_else` treats a response that produced an HTTP-status-bearing synthetic error as success. If this proves awkward in practice, simpler: give `send_data` a sibling `send_ok(make)` that only checks `success` and ignores `data` — the implementer may add that helper instead and use it for `delete_torrent_raw`. Pick whichever is cleaner and keep tests green.

- [ ] **Step 4:** Run `cargo test --lib torbox_client` (cache + constructs tests pass) and `cargo build` (warning-free; `download_present`/`cached`/`detail` fields may now be referenced — if any raw field is still unused, drop it from the struct or `#[allow(dead_code)]`).

- [ ] **Step 5:** Commit:
  ```bash
  git add src/torbox_client.rs
  git commit -m "feat: TorBoxClient HTTP methods (mylist/requestdl/createtorrent/controltorrent)"
  ```

---

## Task 4: Implement `DebridProvider for TorBoxClient`

Thin trait impl delegating to the inherent methods.

**Files:** `src/torbox_client.rs`

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn torbox_client_is_a_debrid_provider() {
    use crate::provider::DebridProvider;
    let c = TorBoxClient::new("fake".to_string()).unwrap();
    let p: std::sync::Arc<dyn DebridProvider> = std::sync::Arc::new(c);
    assert_eq!(p.name(), "torbox");
}
```

- [ ] **Step 2:** Run → FAIL (trait not implemented).

- [ ] **Step 3:** Add the impl (above `mod tests`):

```rust
#[async_trait::async_trait]
impl crate::provider::DebridProvider for TorBoxClient {
    fn name(&self) -> &'static str {
        "torbox"
    }
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        self.list_torrents_raw().await
    }
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        self.torrent_info_raw(id).await
    }
    async fn add_magnet(
        &self,
        magnet: &str,
    ) -> Result<crate::rd_client::AddMagnetResponse, reqwest::Error> {
        self.add_magnet_raw(magnet).await
    }
    async fn select_files(&self, _torrent_id: &str, _file_ids: &str) -> Result<(), reqwest::Error> {
        // TorBox auto-selects all files on createtorrent; nothing to do.
        Ok(())
    }
    async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
        self.delete_torrent_raw(torrent_id).await
    }
    async fn resolve_url(&self, loc: &FileLocator) -> Result<String, AppError> {
        self.resolve_locator(loc).await
    }
    async fn invalidate(&self, loc: &FileLocator) {
        self.invalidate_locator(loc).await
    }
    async fn evict_expired_cache(&self) {
        self.evict_expired().await
    }
}
```

- [ ] **Step 4:** `cargo test --lib torbox_client` (all pass), `cargo build` + `cargo clippy --all-targets` warning-free, `cargo test --no-run` (all targets compile).

- [ ] **Step 5:** Commit:
  ```bash
  git add src/torbox_client.rs
  git commit -m "feat: implement DebridProvider for TorBoxClient"
  ```

---

## Task 5: Wire TorBox into startup selection

**Files:** `src/main.rs`

- [ ] **Step 1:** Add the import: `use debridmoviemapper::torbox_client::TorBoxClient;` (next to the `RealDebridClient` import).

- [ ] **Step 2:** In the `provider` construction `match provider_kind`, replace the `ProviderKind::TorBox` arm (currently prints "TorBox support is not yet available in this build" and exits) with:
  ```rust
          ProviderKind::TorBox => Arc::new(TorBoxClient::new(provider_token)?),
  ```

- [ ] **Step 3:** `cargo build` (warning-free), `cargo test` (green).

- [ ] **Step 4:** Manual sanity check (no real TorBox download starts — it just must construct and begin scanning; Ctrl-C / let it error on TMDB if unset). From a `.env`-free directory or with `TORBOX_API_KEY` set, confirm the binary selects TorBox and does not print "not yet available". (Detailed live verification is Task 6.)

- [ ] **Step 5:** Commit:
  ```bash
  git add src/main.rs
  git commit -m "feat: run against TorBox when TORBOX_API_KEY is set"
  ```

---

## Task 6: Live verification + docs (controller-run)

This task is run by the controller (it uses the real `TORBOX_API_KEY` from `.env` and modifies the live account by adding/deleting a Creative-Commons torrent). It is NOT a subagent task.

- [ ] **Step 1:** Build a tiny throwaway check (or use a `#[ignore]` test) that, against live TorBox: adds the Sintel CC magnet via `add_magnet`, calls `get_torrents` (asserts the hash appears with status `"downloaded"`), `get_torrent_info` (asserts the `.mp4` file with its id), `resolve_url` on the `.mp4` `FileLocator` (asserts a CDN URL string is returned), then `delete_torrent` and confirms `get_torrents` no longer lists it. Confirm each step's output. (This validates the mapping + every HTTP method end-to-end. The formal, committed version of this is Phase 5.)

- [ ] **Step 2:** Update `CLAUDE.md`: add a `torbox_client.rs` row to the module table; note that setting `TORBOX_API_KEY` now runs the service against TorBox; briefly describe the TorBox mapping (resolve by `(torrent_id, file_id)` via `requestdl`; `select_files` no-op; `download_finished → "downloaded"` so Inactive items still appear and re-acquire on playback). Update the `ratelimit.rs` row too.

- [ ] **Step 3:** Commit docs:
  ```bash
  git add CLAUDE.md
  git commit -m "docs: document TorBox client (Phase 4)"
  ```

---

## Self-review

**Spec coverage (Phase 4):**
- `DebridProvider` for TorBox against the verified API → Tasks 2–4. ✓
- Resolve by `(torrent_id, file_id)` via `requestdl`; `link: None` → Task 3 (`resolve_locator`). ✓
- `select_files` no-op; `add_magnet`→createtorrent; `delete_torrent`→controltorrent → Tasks 3–4. ✓
- Inactive/uncached items appear (status normalisation `download_finished → "downloaded"`) → Task 2 (`tb_status`). ✓
- Shared rate limiter → Task 1. ✓
- Startup wiring → Task 5. ✓
- Live verification → Task 6. Formal integration tests → **Phase 5**.

**Placeholder scan:** none — full code provided. The two NOTE callouts (delete_torrent helper choice; unused-field cleanup) give explicit alternatives, not placeholders.

**Type consistency:** mapping returns canonical `Torrent`/`TorrentInfo`/`TorrentFile` (from `rd_client`), consistent with the trait. `FileLocator` cache key `(torrent_id: String, file_id: u32)` consistent in `cache_get`/`cache_put`/`invalidate_locator`. `resolve_url -> Result<String, AppError>` and the other methods' `reqwest::Error` returns match the `DebridProvider` trait exactly (Task 4 delegates).
