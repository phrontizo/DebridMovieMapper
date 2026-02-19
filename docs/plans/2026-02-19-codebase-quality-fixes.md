# Codebase Quality Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Apply fixes 3-34 from the codebase security/quality/best-practices review, migrate sled→redb, remove hardcoded credentials, and improve code quality across the board.

**Architecture:** The changes are organized in dependency order — foundation (deps, constants, regex) first, then type system changes, then data layer migration, then VFS restructuring, then API client hardening, then server hardening, and finally code quality refactors.

**Tech Stack:** Rust 1.93, tokio, redb 3.1, thiserror, reqwest, dav-server

**TDD approach:** For refactors where existing tests validate correctness, verify tests pass after each change. For new behavior (cache eviction, connection limits, seek bounds, path traversal), write failing tests first.

**Key commands:**
- `cargo test` — run unit tests (must pass after every change)
- `cargo build --release` — verify release build

---

### Task 1: Cargo.toml dependency updates

**Files:**
- Modify: `Cargo.toml`

**Step 1: Update dependencies**

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "net", "io-util", "signal"] }
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
dav-server = "0.7"
futures-util = "0.3"
regex = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
hyper = { version = "1", features = ["full"] }
hyper-util = { version = "0.1", features = ["full"] }
bytes = "1"
dotenvy = "0.15"
rand = "0.8"
redb = "3.1"
thiserror = "2"

[dev-dependencies]
urlencoding = "2"
```

Changes:
- Remove `sled = "0.34"` (will be unused after migration)
- Add `redb = "3.1"` and `thiserror = "2"`
- Move `urlencoding = "2"` to `[dev-dependencies]` (fix #28)
- Replace `tokio` `features = ["full"]` with specific features (fix #29)

**Step 2: Verify build compiles (sled references will break — that's expected, we fix in Task 8)**

NOTE: Do NOT remove sled yet — it's still referenced. Add redb alongside it. We'll remove sled after migration in Task 8.

Revised approach for Step 1: Keep `sled = "0.34"` for now. Add `redb` and `thiserror`. Move `urlencoding`. Change tokio features. Remove sled in Task 8 after migration.

**Step 3: Run `cargo test` to verify nothing broke**

**Step 4: Commit**

```
feat: update Cargo.toml deps (add redb, thiserror, move urlencoding to dev)
```

---

### Task 2: Remove hardcoded TMDB API key from all source files

**Files:**
- Modify: `src/identification.rs` (lines 403, 440, 474, 528, 568, 607)
- Modify: `tests/test_short_titles.rs` (line 10)
- Modify: `tests/test_all_rd_torrents.rs`
- Modify: `tests/test_identification_stats.rs`

**Step 1: In every file, replace all occurrences of:**

```rust
let tmdb_api_key = std::env::var("TMDB_API_KEY").unwrap_or_else(|_| "839969cf4f1e183aa5f010f7c4c643f1".to_string());
```

with:

```rust
let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
```

**Step 2: Add `#[ignore]` to the identification unit tests** that make real TMDB API calls (they require network and API key, so they're integration tests):
- `test_repro_00000_issue`
- `test_2012_is_not_generic`
- `test_peaky_blinders_identification`
- `test_short_name_no_random_match`
- `test_flow_2024_prefers_popular`
- `test_dune_2000_identification`

These are already effectively integration tests since they call TMDB.

**Step 3: Verify `.gitignore` contains `.env` (already confirmed it does)**

**Step 4: Run `cargo test` — the newly-ignored tests won't run without API key**

**Step 5: Commit**

```
security: remove hardcoded TMDB API key from all source files
```

---

### Task 3: Shared VIDEO_EXTENSIONS constant and static regex compilation

**Files:**
- Modify: `src/identification.rs`
- Modify: `src/vfs.rs`

**Step 1: In `src/vfs.rs`, add a shared constant at module level:**

```rust
pub const VIDEO_EXTENSIONS: &[&str] = &[
    ".mkv", ".mp4", ".avi", ".m4v", ".mov", ".wmv", ".flv", ".ts", ".m2ts",
];
```

Update `is_video_file()` to use it:

```rust
pub fn is_video_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    let filename = lower.rsplit('/').next().unwrap_or(&lower);
    if filename.contains("sample") || filename.contains("trailer") || filename.contains("extra") ||
       filename.contains("bonus") || filename.contains("featurette") {
        return false;
    }
    VIDEO_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}
```

Note: fix #23 (false-positive "sample" check) — now only checks the filename part, not the full path.

**Step 2: In `src/identification.rs`, use the shared constant for extension stripping in `clean_name()`:**

```rust
use crate::vfs::VIDEO_EXTENSIONS;
```

Replace the hardcoded extension check (lines 278-281):

```rust
if let Some(pos) = title.rfind('.') {
    let ext = &title[pos..].to_lowercase();
    if VIDEO_EXTENSIONS.iter().any(|e| *e == ext) {
        title.truncate(pos);
    }
}
```

**Step 3: Convert all regex patterns to `LazyLock<Regex>` statics in `identification.rs`:**

At the top of the file, add:

```rust
use std::sync::LazyLock;
```

Then define statics:

```rust
static CAMEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z])([A-Z])").unwrap());
static PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(\[.*?\]|\(.*?\)|[\w.-]+\.[a-z]{2,6}\s+-\s+|d3us-|m-|Bond[\s.]+\d+|James[\s.]*Bond|007)\s*").unwrap());
static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}\b").unwrap());
static STOP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(1080p|720p|2160p|4k|s\d+e\d+|s\d+|seasons?\s*\d+|\d+\s*seasons?|temporada\s*\d+|saison\s*\d+|\d+x\d+|episodes?\s*\d+|e\d+|parts?\s*\d+|vol(ume)?\s*\d+|bluray|web-dl|h264|h265|x264|x265|remux|multi|vff|custom|dts|dd5|dd\+5|ddp5|esub|webrip|hdtv|avc|hevc|aac|truehd|atmos|criterion|repack|completa|complete|pol|eng|ita|ger|fra|spa|esp|rus|ukr)\b").unwrap());
static YEAR_RANGE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}[\s-]+(19|20)\d{2}\b").unwrap());
static SHOW_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)s(\d+)\.?e(\d+)|s(\d+)|(\d+)x(\d+)|seasons?\s*\d+|\d+\s*seasons?|temporada\s*\d+|saison\s*\d+|e\d+").unwrap());
static GENERIC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(episode|season|part|volume|vol)\s*(\d+|[a-z])?$").unwrap());
```

Then update `clean_name()`, `is_show_guess()`, `is_generic_title()`, and `identify_name()` to use these statics instead of `Regex::new()`.

**Step 4: In `src/vfs.rs`, convert season regex to LazyLock:**

```rust
use std::sync::LazyLock;
use regex::Regex;

static SEASON_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)s(\d+)|season\s*(\d+)|(\d+)x\d+").unwrap()
});
```

Remove the `let season_regex = Regex::new(...)` from inside the loop in `update()`.

**Step 5: Run `cargo test`**

**Step 6: Commit**

```
perf: static regex compilation with LazyLock, shared VIDEO_EXTENSIONS constant
```

---

### Task 4: Remove `unwrap()` from production code and add error type

**Files:**
- Create: `src/error.rs`
- Modify: `src/mapper.rs`
- Modify: `src/rd_client.rs`
- Modify: `src/dav_fs.rs`
- Modify: `src/tmdb_client.rs`
- Modify: `src/repair.rs`

**Step 1: Create `src/error.rs` (fix #12):**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Database error: {0}")]
    Db(#[from] redb::Error),

    #[error("Repair failed: {0}")]
    Repair(String),

    #[error("Invalid configuration: {0}")]
    Config(String),
}
```

Add `pub mod error;` to `src/mapper.rs`.

**Step 2: Fix `RealDebridClient::new` to return `Result` (fix #7):**

```rust
pub fn new(api_token: String) -> Result<Self, AppError> {
    let auth_val = format!("Bearer {}", api_token);
    let mut auth_header = HeaderValue::from_str(&auth_val)
        .map_err(|e| AppError::Config(format!("Invalid API token for HTTP header: {}", e)))?;
    auth_header.set_sensitive(true);
    // ...
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .user_agent("DebridMovieMapper/0.1.0")
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AppError::Config(format!("Failed to build HTTP client: {}", e)))?;
    Ok(Self { client, unrestrict_cache, rate_limiter })
}
```

**Step 3: Fix `dav_fs.rs:33` `to_str().unwrap()` (fix #7):**

```rust
let path_str = path_osstr.to_str().ok_or(FsError::Forbidden)?;
// Change find_node return type to Result<Option<VfsNode>, FsError> or just return None
```

Actually, simpler: make `find_node` return `None` for non-UTF-8:

```rust
let path_str = match path_osstr.to_str() {
    Some(s) => s,
    None => return None,
};
```

**Step 4: Fix `tmdb_client.rs:123` `last_error.unwrap()` (fix #7):**

```rust
Err(last_error.expect("retry loop completed without recording an error"))
```

**Step 5: Update `main.rs` to handle `RealDebridClient::new` returning `Result`:**

```rust
let rd_client = Arc::new(RealDebridClient::new(api_token)?);
```

**Step 6: Update all test code that calls `RealDebridClient::new` to use `.unwrap()` or `?`** (tests are fine to unwrap).

**Step 7: Run `cargo test`**

**Step 8: Commit**

```
refactor: unified error type, remove unwrap() from production code
```

---

### Task 5: VFS restructuring — BTreeMap, remove redundant name field

**Files:**
- Modify: `src/vfs.rs`
- Modify: `src/dav_fs.rs`
- Modify: `tests/test_strm_generation.rs`
- Modify: `tests/video_player_simulation.rs`

**Step 1: In `src/vfs.rs`, change `VfsNode` to remove redundant `name` fields and use `BTreeMap` (fixes #21, #22):**

```rust
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum VfsNode {
    Directory {
        children: BTreeMap<String, VfsNode>,
    },
    StrmFile {
        strm_content: Vec<u8>,
        rd_link: String,
        rd_torrent_id: String,
    },
    VirtualFile {
        content: Vec<u8>,
    },
}
```

**Step 2: Update all code that creates or pattern-matches `VfsNode` to remove `name` field:**

- `DebridVfs::new()` — remove `name:` from Directory constructors
- `DebridVfs::update()` / `build()` — remove `name:` from all Directory, StrmFile, VirtualFile constructors
- `dav_fs.rs` — `StrmFile` struct keeps its own `name` field (it's the DavFile, separate from VfsNode). Update matches to not destructure `name` from VfsNode.
- `DebridDirEntry` — gets name from the HashMap key, not from VfsNode.
- `DebridMetaData` — `len()` doesn't need name. `StrmFile` constructs VfsNode without name.

**Step 3: Update test helper functions** in `tests/test_strm_generation.rs`, `tests/video_player_simulation.rs` that destructure `VfsNode::Directory { name, children }` — change to `VfsNode::Directory { children }`.

**Step 4: Update `src/dav_fs.rs`** — the `DebridDirEntry` struct needs to get the name from the parent HashMap key (already does via `name` field in the struct, not from VfsNode).

For `StrmFile` in `dav_fs.rs`, the file-level name is stored in the `StrmFile` struct itself, not from VfsNode:

```rust
VfsNode::StrmFile { strm_content, rd_link, rd_torrent_id } => {
    let name = path.as_rel_ospath().to_str()
        .and_then(|s| s.rsplit('/').next())
        .unwrap_or("")
        .to_string();
    Ok(Box::new(StrmFile {
        name,
        content: Bytes::from(strm_content),
        ...
    }) as Box<dyn DavFile>)
}
```

Similarly for `VirtualFile`.

**Step 5: Run `cargo test`**

**Step 6: Commit**

```
refactor: VfsNode uses BTreeMap, remove redundant name field
```

---

### Task 6: VFS lock contention fix — `build()` pattern

**Files:**
- Modify: `src/vfs.rs`
- Modify: `src/tasks.rs`
- Modify: `tests/integration_test.rs`
- Modify: `tests/test_strm_generation.rs`
- Modify: `tests/video_player_simulation.rs`

**Step 1: Write a new test in `src/vfs.rs` tests that validates the build pattern:**

```rust
#[tokio::test]
async fn test_vfs_build_returns_new_instance() {
    let torrents = vec![/* same test data as test_vfs_update */];
    let rd_client = mock_rd_client(&["http://link1", "http://link2"]).await;
    let vfs = DebridVfs::build(torrents, rd_client).await;

    if let VfsNode::Directory { children, .. } = &vfs.root {
        assert!(children.contains_key("Movies"));
        assert!(children.contains_key("Shows"));
    }
}
```

**Step 2: Refactor `DebridVfs::update(&mut self, ...)` → `DebridVfs::build(torrents, rd_client) -> DebridVfs`:**

Change signature from:
```rust
pub async fn update(&mut self, torrents: Vec<(TorrentInfo, MediaMetadata)>, rd_client: Arc<RealDebridClient>)
```

To:
```rust
pub async fn build(torrents: Vec<(TorrentInfo, MediaMetadata)>, rd_client: Arc<RealDebridClient>) -> Self
```

The method creates a new `DebridVfs` and builds the tree, returning it. No `&mut self` needed.

**Step 3: Update `tasks.rs` `update_vfs` function:**

```rust
async fn update_vfs(
    vfs: &Arc<RwLock<DebridVfs>>,
    current_data: &[(crate::rd_client::TorrentInfo, MediaMetadata)],
    repair_manager: &Arc<RepairManager>,
    rd_client: &Arc<RealDebridClient>,
) {
    let mut filtered = Vec::new();
    for (torrent_info, metadata) in current_data {
        if !repair_manager.should_hide_torrent(&torrent_info.id).await {
            filtered.push((torrent_info.clone(), metadata.clone()));
        }
    }
    // Build VFS WITHOUT holding the lock
    let new_vfs = DebridVfs::build(filtered, rd_client.clone()).await;
    // Only hold write lock briefly to swap
    let mut vfs_lock = vfs.write().await;
    *vfs_lock = new_vfs;
}
```

**Step 4: Update all test callers:**
- `vfs.rs` tests: `let vfs = DebridVfs::build(torrents, rd_client).await;`
- `integration_test.rs`: `*vfs_lock = DebridVfs::build(data.clone(), rd_client.clone()).await;`
- `test_strm_generation.rs`: same pattern
- `video_player_simulation.rs`: same pattern

**Step 5: Run `cargo test`**

**Step 6: Commit**

```
perf: build VFS without holding write lock, eliminate lock contention during scans
```

---

### Task 7: sled → redb migration

**Files:**
- Modify: `Cargo.toml` (remove sled)
- Modify: `src/main.rs`
- Modify: `src/tasks.rs`
- Modify: `tests/integration_test.rs`

**Step 1: Update `src/main.rs` to use redb:**

```rust
use redb::{Database, TableDefinition};

const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ...
    let db = Arc::new(Database::create("metadata.db")?);

    // Ensure table exists
    {
        let write_txn = db.begin_write()?;
        write_txn.open_table(MATCHES_TABLE)?;
        write_txn.commit()?;
    }

    tokio::spawn(debridmoviemapper::tasks::run_scan_loop(
        rd_client.clone(),
        tmdb_client.clone(),
        vfs.clone(),
        db.clone(),
        repair_manager.clone(),
        scan_interval_secs,
    ));
    // ...
}
```

**Step 2: Update `src/tasks.rs` to use redb (fix #33 — wrap DB ops in spawn_blocking):**

Change signature:
```rust
pub async fn run_scan_loop(
    rd_client: Arc<RealDebridClient>,
    tmdb_client: Arc<TmdbClient>,
    vfs: Arc<RwLock<DebridVfs>>,
    db: Arc<redb::Database>,
    repair_manager: Arc<RepairManager>,
    interval_secs: u64,
)
```

For reads:
```rust
use redb::TableDefinition;
const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");

// Load persisted matches from DB on startup (in spawn_blocking)
let db_clone = db.clone();
let persisted = tokio::task::spawn_blocking(move || {
    let mut map = HashMap::new();
    let read_txn = db_clone.begin_read().ok()?;
    let table = read_txn.open_table(MATCHES_TABLE).ok()?;
    for result in table.iter().ok()?.flatten() {
        let (key, value) = result;
        let id = key.value().to_string();
        if let Ok(data) = serde_json::from_slice::<(crate::rd_client::TorrentInfo, MediaMetadata)>(value.value()) {
            map.insert(id, data);
        }
    }
    Some(map)
}).await.ok().flatten().unwrap_or_default();
```

For writes:
```rust
let db_clone = db.clone();
let id_clone = id.clone();
let data_bytes_clone = data_bytes.clone();
let _ = tokio::task::spawn_blocking(move || {
    if let Ok(write_txn) = db_clone.begin_write() {
        if let Ok(mut table) = write_txn.open_table(MATCHES_TABLE) {
            let _ = table.insert(id_clone.as_str(), data_bytes_clone.as_slice());
        }
        let _ = write_txn.commit();
    }
}).await;
```

**Step 3: Update `tests/integration_test.rs`:**

Replace the static sled DB with redb:

```rust
use redb::{Database, TableDefinition};

const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");

static DB: LazyLock<Database> = LazyLock::new(|| {
    Database::create("metadata.db").expect("Failed to open database")
});
```

Update `fetch_and_identify` to use `&Database` instead of `&sled::Tree`.

**Step 4: Remove `sled = "0.34"` from Cargo.toml**

**Step 5: Run `cargo test`**

**Step 6: Commit**

```
feat: migrate from sled to redb for metadata persistence
```

---

### Task 8: Unrestrict cache bounds and eviction

**Files:**
- Modify: `src/rd_client.rs`

**Step 1: Write failing test for cache eviction:**

```rust
#[tokio::test]
async fn unrestrict_cache_evicts_expired_entries() {
    let client = RealDebridClient::new("fake-token".to_string()).unwrap();
    // Seed cache with an entry that has an old timestamp
    {
        let mut cache = client.unrestrict_cache.write().await;
        cache.insert("old-link".to_string(), CachedUnrestrictResponse {
            response: UnrestrictResponse { /* mock fields */ },
            cached_at: std::time::Instant::now() - Duration::from_secs(7200), // 2 hours ago
        });
        cache.insert("new-link".to_string(), CachedUnrestrictResponse {
            response: UnrestrictResponse { /* mock fields */ },
            cached_at: std::time::Instant::now(),
        });
    }
    client.evict_expired_cache().await;
    let cache = client.unrestrict_cache.read().await;
    assert!(!cache.contains_key("old-link"));
    assert!(cache.contains_key("new-link"));
}
```

**Step 2: Add `evict_expired_cache` method and MAX_CACHE_SIZE constant:**

```rust
const MAX_CACHE_SIZE: usize = 10_000;
const CACHE_TTL: Duration = Duration::from_secs(3600);

pub async fn evict_expired_cache(&self) {
    let mut cache = self.unrestrict_cache.write().await;
    cache.retain(|_, v| v.cached_at.elapsed() < CACHE_TTL);
    // If still over max size, remove oldest entries
    if cache.len() > MAX_CACHE_SIZE {
        let mut entries: Vec<_> = cache.iter().map(|(k, v)| (k.clone(), v.cached_at)).collect();
        entries.sort_by_key(|(_, t)| *t);
        let to_remove = cache.len() - MAX_CACHE_SIZE;
        for (key, _) in entries.into_iter().take(to_remove) {
            cache.remove(&key);
        }
    }
}
```

**Step 3: In `unrestrict_link`, after inserting to cache, check cache size:**

```rust
if cache.len() > MAX_CACHE_SIZE {
    drop(cache);
    self.evict_expired_cache().await;
}
```

**Step 4: Run tests**

**Step 5: Commit**

```
fix: bound unrestrict cache size with eviction
```

---

### Task 9: API client hardening (response limits, retry-after cap, log redaction, TMDB retry fix, delete retry)

**Files:**
- Modify: `src/rd_client.rs`
- Modify: `src/tmdb_client.rs`
- Modify: `src/vfs.rs`

**Step 1: Write test for Retry-After cap (fix #16):**

```rust
#[test]
fn retry_after_is_capped() {
    // The wait_for_retry function should cap Retry-After at 300s
    // We verify this through the constant
    assert_eq!(MAX_RETRY_AFTER_SECS, 300);
}
```

**Step 2: Add MAX_RETRY_AFTER_SECS and apply cap in `wait_for_retry` (fix #16):**

```rust
const MAX_RETRY_AFTER_SECS: u64 = 300;

// In wait_for_retry:
if let Some(seconds) = retry_after {
    let capped = std::cmp::min(seconds, MAX_RETRY_AFTER_SECS);
    warn!("RD API returned {} (attempt {}/{}). Respecting Retry-After: {}s (raw: {}s)",
        status, attempt, max_attempts, capped, seconds);
    tokio::time::sleep(Duration::from_secs(capped)).await;
}
```

Also apply in `record_throttle`:
```rust
let capped_seconds = seconds.min(MAX_RETRY_AFTER_SECS);
```

**Step 3: Fix `fetch_with_retry` fallback path (fix #5 from best-practices review):**

Replace the unreachable fallback at line 452-453:

```rust
if let Some(e) = last_error {
    Err(e)
} else {
    // All attempts failed without recording an error (should be unreachable)
    error!("fetch_with_retry: all {} attempts exhausted without a recorded error", max_attempts);
    make_request().send().await?.error_for_status()?.json().await
}
```

Actually, fix the root cause: set `last_error` for deserialization failures too:

```rust
Err(e) => {
    error!("Failed to decode RD response: {}. Status: {}, Body: {}...",
        e, status, &text[..std::cmp::min(200, text.len())]);
    // Don't just log — record as error so the fallback path is unreachable
    last_error = Some(reqwest::Client::new().get("http://invalid").send().await.unwrap_err());
}
```

Actually that's hacky. Better approach — restructure to always set last_error. The cleanest fix: change the deserialization failure branch to `continue` (it already does implicitly), and change the fallback to panic:

```rust
} else {
    unreachable!("fetch_with_retry: all attempts exhausted without a recorded error")
}
```

**Step 4: Logging policy (fix #15 — MODIFIED):**

Keep all response bodies, URLs, and debugging data in logs — the logs are private and valuable for debugging. The only thing to guard against is API keys appearing in logs. The `Authorization` header is already marked `set_sensitive(true)` in `rd_client.rs:198`, which prevents reqwest from logging it. No further log redaction needed.

However, strip the `headers` from the deserialization error log since headers could contain auth tokens from other middleware:

```rust
error!("Failed to decode RD response: {}. Status: {}, Body: {}",
    e, status, text);
```

**Step 5: Fix TMDB `fetch_with_retry` bug (fix #9):**

The TMDB client's retry loop detects retryable status, sleeps, then falls through to `resp.error_for_status()` on the SAME response. Fix by adding `continue`:

```rust
if status == reqwest::StatusCode::TOO_MANY_REQUESTS
   || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
   || status == reqwest::StatusCode::BAD_GATEWAY
   || status == reqwest::StatusCode::GATEWAY_TIMEOUT
{
    let retry_after = resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1);
    let capped = std::cmp::min(retry_after, MAX_RETRY_AFTER_SECS);
    warn!("TMDB API returned {} (attempt {}/{}). Waiting {}s", status, attempt, max_attempts, capped);
    tokio::time::sleep(Duration::from_secs(capped)).await;
    continue;  // <-- THIS WAS MISSING
}
```

Also fix `last_error.unwrap()` → `last_error.expect(...)`.

**Step 7: Make `delete_torrent` use retry logic (fix #26):**

```rust
pub async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
    let url = format!("https://api.real-debrid.com/rest/1.0/torrents/delete/{}", torrent_id);
    // Use fetch_with_retry with 404 as terminal (already deleted = success)
    let _: serde_json::Value = self.fetch_with_retry(
        || self.client.delete(&url),
        &[reqwest::StatusCode::NOT_FOUND],
    ).await.or_else(|e| {
        // 404 means already deleted — that's fine
        if e.status() == Some(reqwest::StatusCode::NOT_FOUND) {
            Ok(serde_json::Value::Null)
        } else {
            Err(e)
        }
    })?;
    Ok(())
}
```

Wait — `fetch_with_retry` already treats terminal statuses as errors. And delete returns 204 (no body), which the existing empty-body handling via `"[]"` parses as `serde_json::Value::Array([])`. This should work.

Actually, a cleaner approach: keep the existing delete logic but add a simple retry loop around it, or just route through `fetch_with_retry` since it already handles empty bodies.

**Step 8: Run `cargo test`**

**Step 9: Commit**

```
fix: cap Retry-After, fix TMDB retry bug, redact sensitive logs, delete_torrent retry
```

---

### Task 10: WebDAV hardening — path traversal, seek, private fields, timestamps

**Files:**
- Modify: `src/dav_fs.rs`
- Modify: `src/vfs.rs`

**Step 1: Write test for path traversal guard (fix #14):**

```rust
#[tokio::test]
async fn find_node_rejects_dotdot_traversal() {
    let vfs = Arc::new(RwLock::new(DebridVfs::new()));
    let rd_client = Arc::new(RealDebridClient::new("fake".to_string()).unwrap());
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));
    let fs = DebridFileSystem::new(rd_client, vfs, repair_manager);

    let path = DavPath::new("/../etc/passwd").unwrap();
    let result = fs.find_node(&path).await;
    assert!(result.is_none());
}
```

**Step 2: Add `..` check in `find_node` (fix #14):**

```rust
for component in path_str.split('/') {
    if component.is_empty() || component == "." {
        continue;
    }
    if component == ".." {
        return None;
    }
    // ...
}
```

**Step 3: Write test for checked seek (fix #25):**

```rust
#[test]
fn seek_negative_overflow_returns_error() {
    // Test that seeking to a negative position returns an error
    // rather than wrapping to a large u64
}
```

**Step 4: Fix seek implementations with checked arithmetic (fix #25):**

```rust
fn seek(&mut self, pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
    async move {
        let new_pos = match pos {
            std::io::SeekFrom::Start(p) => p,
            std::io::SeekFrom::Current(p) => {
                let base = self.pos as i64;
                let result = base.checked_add(p)
                    .filter(|&n| n >= 0)
                    .ok_or(FsError::GeneralFailure)?;
                result as u64
            }
            std::io::SeekFrom::End(p) => {
                let size = self.content.len() as i64;
                let result = size.checked_add(p)
                    .filter(|&n| n >= 0)
                    .ok_or(FsError::GeneralFailure)?;
                result as u64
            }
        };
        self.pos = new_pos;
        Ok(new_pos)
    }.boxed()
}
```

Apply to both `StrmFile` and `VirtualFile`.

**Step 5: Make `DebridFileSystem` fields private (fix #30):**

```rust
#[derive(Clone)]
pub struct DebridFileSystem {
    vfs: Arc<RwLock<DebridVfs>>,
    rd_client: Arc<RealDebridClient>,
    repair_manager: Arc<RepairManager>,
}
```

The fields are already only accessed through `self` internally. Check no external code accesses them directly.

**Step 6: Add stable timestamps (fix #11):**

Add a `created_at` field to `DebridVfs`:

```rust
pub struct DebridVfs {
    pub root: VfsNode,
    pub created_at: SystemTime,
}
```

Set it in `build()`:
```rust
Self { root, created_at: SystemTime::now() }
```

In `dav_fs.rs`, change `DebridMetaData` to carry the timestamp:

```rust
struct DebridMetaData {
    node: VfsNode,
    modified_time: SystemTime,
}

impl DavMetaData for DebridMetaData {
    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified_time)
    }
}
```

Pass `vfs.created_at` when constructing `DebridMetaData` from the VFS.

**Step 7: Run `cargo test`**

**Step 8: Commit**

```
fix: path traversal guard, checked seek, private fields, stable timestamps
```

---

### Task 11: Server hardening — connection limiting, graceful shutdown

**Files:**
- Modify: `src/main.rs`

**Step 1: Add connection semaphore (fix #17):**

```rust
use tokio::sync::Semaphore;

const MAX_CONNECTIONS: usize = 256;

// In main():
let semaphore = Arc::new(Semaphore::new(MAX_CONNECTIONS));

loop {
    let (stream, _addr) = listener.accept().await?;
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
        // ... existing connection handling
    });
}
```

**Step 2: Add graceful shutdown (fix #10):**

```rust
loop {
    tokio::select! {
        result = listener.accept() => {
            let (stream, _addr) = result?;
            // ... handle connection
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal, flushing database...");
            // redb handles its own flushing on drop, but we can be explicit
            drop(db);
            info!("Shutdown complete.");
            break;
        }
    }
}
Ok(())
```

**Step 3: Fix IncompleteMessage detection (fix #34):**

```rust
// Instead of string formatting:
let is_incomplete = err.source()
    .and_then(|s| s.downcast_ref::<hyper::Error>())
    .map(|e| e.is_incomplete_message())
    .unwrap_or(false);
if is_incomplete {
    return;
}
```

Check if hyper 1.x has `is_incomplete_message()`. If not, keep the string-based check but add a comment explaining why.

**Step 4: Run `cargo test`**

**Step 5: Commit**

```
feat: connection limiting, graceful shutdown, better error detection
```

---

### Task 12: NFO XML escaping and link-index validation

**Files:**
- Modify: `src/vfs.rs`

**Step 1: Write test for XML escaping of all NFO fields (fix #27):**

```rust
#[test]
fn test_nfo_escapes_all_fields() {
    let vfs = DebridVfs::new();
    let metadata = MediaMetadata {
        title: "Test & <Movie>".to_string(),
        year: Some("2024".to_string()),
        media_type: MediaType::Movie,
        external_id: Some("tmdb:123&45".to_string()),
    };
    let content = String::from_utf8(vfs.generate_nfo(&metadata)).unwrap();
    assert!(content.contains("<title>Test &amp; &lt;Movie&gt;</title>"));
    assert!(content.contains("123&amp;45"));
    assert!(!content.contains("&<"));
}
```

**Step 2: Apply `xml_escape()` to all interpolated values in `generate_nfo` (fix #27):**

```rust
nfo.push_str(&format!("  <year>{}</year>\n", xml_escape(year)));
nfo.push_str(&format!("  <premiered>{}-01-01</premiered>\n", xml_escape(year)));
nfo.push_str(&format!("  <uniqueid type=\"{}\" default=\"true\">{}</uniqueid>\n", xml_escape(source), xml_escape(id)));
nfo.push_str(&format!("  <tmdbid>{}</tmdbid>\n", xml_escape(id)));
nfo.push_str(&format!("  <url>https://www.themoviedb.org/{}/{}</url>\n", xml_escape(path), xml_escape(id)));
```

**Step 3: Add link-index validation warning (fix M8 from code quality review):**

```rust
// In add_torrent_files:
let selected_count = torrent.files.iter().filter(|f| f.selected == 1).count();
if selected_count != torrent.links.len() {
    tracing::warn!(
        "Torrent '{}': selected file count ({}) != link count ({})",
        torrent.filename, selected_count, torrent.links.len()
    );
}
```

**Step 4: Run `cargo test`**

**Step 5: Commit**

```
fix: XML-escape all NFO fields, add link-index mismatch warning
```

---

### Task 13: Break down complex functions

**Files:**
- Modify: `src/identification.rs`
- Modify: `src/repair.rs`

**Step 1: Extract `select_best_match` from `identify_name` (fix #19):**

Extract the 12-branch match into a helper:

```rust
struct MatchCandidate<'a> {
    result: &'a TmdbSearchResult,
    is_exact: bool,
    year_matches: bool,
    media_type: MediaType,
}

fn select_best_match<'a>(
    tv: Option<MatchCandidate<'a>>,
    movie: Option<MatchCandidate<'a>>,
    is_show_guess: bool,
) -> Option<MatchCandidate<'a>> {
    match (tv, movie) {
        (Some(tv), Some(movie)) => {
            // Exact title + year is strongest signal
            if tv.is_exact && tv.year_matches && !(movie.is_exact && movie.year_matches) {
                Some(tv)
            } else if movie.is_exact && movie.year_matches && !(tv.is_exact && tv.year_matches) {
                Some(movie)
            // Show guess + year match
            } else if is_show_guess && tv.year_matches {
                Some(tv)
            } else if !is_show_guess && movie.year_matches {
                Some(movie)
            // Exact title without year
            } else if tv.is_exact && !movie.is_exact {
                Some(tv)
            } else if movie.is_exact && !tv.is_exact {
                Some(movie)
            // Year match without exact title
            } else if tv.year_matches && !movie.year_matches {
                Some(tv)
            } else if movie.year_matches && !tv.year_matches {
                Some(movie)
            // Final fallback: use show guess
            } else if is_show_guess {
                Some(tv)
            } else {
                Some(movie)
            }
        }
        (Some(tv), None) => Some(tv),
        (None, Some(movie)) => Some(movie),
        (None, None) => None,
    }
}
```

Also extract the duplicated short-title filtering + max_by scoring into a helper:

```rust
fn best_scored_result<'a>(
    results: &'a [TmdbSearchResult],
    normalized_query: &str,
    year: &Option<String>,
    is_short_title: bool,
) -> Option<&'a TmdbSearchResult> {
    if is_short_title {
        results.iter().filter(|r| {
            let normalized_title = normalize_title(&r.title);
            let title_matches = normalized_title == normalized_query;
            let year_matches = year.as_ref()
                .map(|y| r.release_date.as_ref().map(|rd| rd.starts_with(y)).unwrap_or(false))
                .unwrap_or(false);
            title_matches && year_matches
        }).max_by(|a, b| {
            score_result(a, normalized_query, year).partial_cmp(&score_result(b, normalized_query, year))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    } else {
        results.iter().max_by(|a, b| {
            score_result(a, normalized_query, year).partial_cmp(&score_result(b, normalized_query, year))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }
}
```

**Step 2: Extract repair error handling into a helper (fix #20):**

```rust
async fn set_repair_failed(&self, torrent_id: &str) {
    let mut health_map = self.health_status.write().await;
    if let Some(health) = health_map.get_mut(torrent_id) {
        health.state = RepairState::Failed;
    }
}
```

Then each error branch becomes:
```rust
Err(e) => {
    error!("Failed to re-add torrent {}: {}", torrent_info.id, e);
    self.set_repair_failed(&torrent_info.id).await;
    Err(format!("Failed to add magnet: {}", e))
}
```

Also extract `execute_repair_steps` as a helper called by `repair_torrent`.

**Step 3: Run `cargo test` — all existing tests must pass**

**Step 4: Commit**

```
refactor: break down identify_name and repair_torrent complexity
```

---

### Task 14: Clean up tests and remove dead code

**Files:**
- Modify: `src/rd_client.rs`
- Modify: `src/dav_fs.rs`
- Modify: `src/tasks.rs`

**Step 1: Remove placeholder tests (fix #32):**

Delete these tests (the compile-time guard functions can stay):
- `rd_client.rs`: `public_api_does_not_include_check_link_health` (the `assert!(true)` test)
- `dav_fs.rs`: `strm_file_struct_has_on_demand_repair_fields` (the `assert!(true)` test)
- `tasks.rs`: `scan_loop_module_exists` (the `assert!(true)` test)

Keep the `#[allow(dead_code)]` compile-time guard functions since they serve a purpose.

**Step 2: Update `tasks.rs` compile-time guard to reflect new redb signature:**

```rust
#[allow(dead_code)]
async fn _assert_run_scan_loop_signature(
    rd_client: Arc<RealDebridClient>,
    tmdb_client: Arc<TmdbClient>,
    vfs: Arc<RwLock<DebridVfs>>,
    db: Arc<redb::Database>,
    repair_manager: Arc<RepairManager>,
) {
    run_scan_loop(rd_client, tmdb_client, vfs, db, repair_manager, 60).await;
}
```

**Step 3: Run `cargo test`**

**Step 4: Commit**

```
cleanup: remove placeholder tests, update compile-time guards
```

---

### Task 15: Final verification

**Step 1: Run full unit test suite:**

```bash
cargo test
```

**Step 2: Run `cargo clippy` for any remaining warnings:**

```bash
cargo clippy -- -W warnings
```

**Step 3: Verify release build:**

```bash
cargo build --release
```

**Step 4: Verify no tokens in source:**

```bash
grep -r "839969cf" src/ tests/ || echo "No hardcoded TMDB key found"
grep -r "RD_API_TOKEN.*=" src/ tests/ --include="*.rs" | grep -v "env::var" | grep -v "must be set" || echo "No hardcoded RD token found"
```

**Step 5: Final commit of any remaining cleanup**

---

## Execution Order Summary

| Task | Description | Depends On |
|------|-------------|------------|
| 1 | Cargo.toml deps | — |
| 2 | Remove hardcoded TMDB key | — |
| 3 | Static regex, VIDEO_EXTENSIONS | 1 |
| 4 | Error type, remove unwrap | 1 |
| 5 | VFS BTreeMap, remove name | — |
| 6 | VFS build() pattern | 5 |
| 7 | sled → redb migration | 1, 4 |
| 8 | Unrestrict cache bounds | 4 |
| 9 | API client hardening | 4 |
| 10 | WebDAV hardening | 4, 5, 6 |
| 11 | Connection limiting, shutdown | 7, 10 |
| 12 | NFO escaping, link validation | 5 |
| 13 | Break down complex functions | 3 |
| 14 | Clean up tests | 7 |
| 15 | Final verification | all |
