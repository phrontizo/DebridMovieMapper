# Simplification Design — 2026-02-18

## Goal

Reduce code duplication (A) and improve code clarity (C) without removing functionality,
plus remove the background repair loop in favour of on-demand repair at STRM access time.

## Changes

### 1. `rd_client.rs` — Retry consolidation

**Problem:** Three nearly-identical retry methods exist:
`fetch_with_retry`, `fetch_with_retry_except_503`, `fetch_with_retry_except_404`.
`check_link_health` is a fourth hand-rolled retry loop used only by the now-deleted repair loop.

**Solution:** Replace all four with a single method:

```rust
async fn fetch_with_retry<T, F>(
    &self,
    make_request: F,
    terminal_statuses: &[StatusCode],
) -> Result<T, reqwest::Error>
```

Callers pass the status codes that should not be retried:
- `get_torrents`, `add_magnet`, `select_files` → `&[]`
- `unrestrict_link` → `&[StatusCode::SERVICE_UNAVAILABLE]`
- `get_torrent_info` → `&[StatusCode::NOT_FOUND]`

`check_link_health` is deleted entirely (only used by the repair loop).

**Naming improvements:**
- `apply_backoff` → `backoff_delay`
- `handle_retryable_status` → `wait_for_retry`
- `is_retryable_status` → `should_retry_status`

### 2. New `tasks.rs` module — Scan loop only

**Problem:** `main.rs` contains two large inline `tokio::spawn` blocks (~160 lines).
`mapper.rs` exports `run_full_scan` which is dead code (never called by `main.rs`).

**Solution:** Create `src/tasks.rs` with one public function:

```rust
pub async fn run_scan_loop(
    rd_client: Arc<RealDebridClient>,
    tmdb_client: Arc<TmdbClient>,
    vfs: Arc<RwLock<DebridVfs>>,
    db_tree: sled::Tree,
    repair_manager: Arc<RepairManager>,
    interval_secs: u64,
)
```

`main.rs` becomes ~80 lines: initialization + `tokio::spawn(tasks::run_scan_loop(...))` + WebDAV server loop.

`mapper.rs` drops `run_full_scan` and becomes purely module declarations.

### 3. Remove background repair loop — on-demand repair at STRM access

**Problem:** The repair loop polls all torrents periodically, using `check_link_health`
to detect broken links. This is expensive, adds complexity, and introduces `RepairState::Checking`.

**Solution:** Repair is triggered lazily when a STRM file is read.

**VFS build time (`vfs.rs`):**
- Pre-unrestrict links as before — needed for correct `len()` in WebDAV PROPFIND.
- If `unrestrict_link` returns 503/error, **skip the file** instead of inserting an error placeholder.
  This prevents Jellyfin from seeing a broken STRM as a valid file.

**STRM access time (`dav_fs.rs` `StrmFile::read_bytes`):**
1. Call `rd_client.unrestrict_link(rd_link)` — cache hit (1h TTL) in the healthy case, no API cost.
2. If success → return fresh URL as content (handles expired pre-resolved URLs).
3. If 503/error → call `repair_manager.mark_broken(torrent_id, link)`, spawn
   `repair_manager.repair_by_id(torrent_id)` as a background task, return `FsError::GeneralFailure`.

`StrmFile` gains `rd_client: Arc<RealDebridClient>` and `rd_link: String`.

`should_hide_torrent` is kept — once a torrent is marked broken/repairing,
subsequent reads return `FsError::GeneralFailure` immediately without making API calls.

**`repair.rs` changes:**
- Remove `check_torrent_health` and `RepairState::Checking`.
- Remove `check_link_health` delegation (method deleted from `rd_client.rs`).
- Add `repair_by_id(torrent_id: &str) -> Result<(), String>`:
  fetches `TorrentInfo` fresh via `rd_client.get_torrent_info`, then calls `repair_torrent`.

### 4. `repair.rs` — Remove background repair loop wiring

The `RepairState` enum loses `Checking`. States: `Healthy`, `Broken`, `Repairing`, `Failed`.

## File Summary

| File | Change |
|------|--------|
| `src/rd_client.rs` | Merge 3 retry variants + `check_link_health` into 1; rename helpers |
| `src/tasks.rs` | New file: `run_scan_loop` |
| `src/main.rs` | Shrinks to ~80 lines; remove repair spawn |
| `src/mapper.rs` | Remove dead `run_full_scan`; add `pub mod tasks` |
| `src/repair.rs` | Remove `check_torrent_health`, `Checking` state; add `repair_by_id` |
| `src/dav_fs.rs` | `StrmFile` re-unrestricts on read; triggers repair on 503 |
| `src/vfs.rs` | Skip files where `unrestrict_link` fails at build time |

## Non-goals

- No changes to `identification.rs` or `tmdb_client.rs`
- No changes to the WebDAV server setup
- No behavioral changes to the scan loop logic
