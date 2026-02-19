# Codebase Quality Review - Design

**Date:** 2026-02-19
**Scope:** Fixes 3-34 from the security/quality/best-practices review

## Architectural Changes

### A. sled -> redb migration
Replace `sled` (unmaintained, known corruption bugs) with `redb` (actively maintained, ACID).
- Single table: `MATCHES_TABLE: TableDefinition<&str, &[u8]>`
- Key: torrent ID string, Value: JSON-serialized `(TorrentInfo, MediaMetadata)`
- `Arc<redb::Database>` passed to `run_scan_loop` instead of `sled::Tree`
- Read/write transactions replace direct get/insert/iter

### B. VFS lock contention fix
Change `DebridVfs::update(&mut self, ...)` to `DebridVfs::build(torrents, rd_client) -> DebridVfs`.
Caller builds VFS without lock, acquires write lock only to swap.

### C. Static regex compilation
All `Regex::new()` calls in `identification.rs` and `vfs.rs` become `LazyLock<Regex>` statics.

### D. Remove hardcoded TMDB key
Replace all hardcoded fallbacks with `.expect("TMDB_API_KEY must be set")`.

## Localized Fixes

| # | Fix | Files |
|---|-----|-------|
| 5 | Unrestrict cache: add max size + periodic eviction | rd_client.rs |
| 6 | Bound response body sizes | rd_client.rs |
| 7 | Replace unwrap() with proper error handling | rd_client.rs, dav_fs.rs, tmdb_client.rs |
| 9 | Fix TMDB retry bug (falls through instead of retrying) | tmdb_client.rs |
| 10 | Graceful shutdown with tokio::signal | main.rs |
| 11 | Stable modified() timestamps | dav_fs.rs, vfs.rs |
| 12 | Unified error type with thiserror | new: error.rs |
| 13 | TorrentStatus enum, structured ExternalId | rd_client.rs, vfs.rs |
| 14 | Explicit `..` path traversal guard | dav_fs.rs |
| 15 | Remove headers from deserialization error log (keep URLs/bodies for debugging) | rd_client.rs |
| 16 | Cap Retry-After to 300s | rd_client.rs |
| 17 | Connection limiting via Semaphore | main.rs |
| 19 | Break down identify_name() complexity | identification.rs |
| 20 | Break down repair_torrent() complexity | repair.rs |
| 21 | HashMap -> BTreeMap for deterministic ordering | vfs.rs |
| 22 | Remove redundant VfsNode name field | vfs.rs, dav_fs.rs |
| 23 | Fix is_video_file "sample" false positives | vfs.rs |
| 24 | Shared VIDEO_EXTENSIONS constant | vfs.rs, identification.rs |
| 25 | Checked arithmetic in seek() | dav_fs.rs |
| 26 | delete_torrent uses fetch_with_retry | rd_client.rs |
| 27 | XML-escape all NFO fields | vfs.rs |
| 28 | urlencoding -> dev-dependencies | Cargo.toml |
| 29 | tokio: specify only needed features | Cargo.toml |
| 30 | Make DebridFileSystem fields private | dav_fs.rs |
| 31 | Extract shared test helpers | tests/common/mod.rs |
| 32 | Remove assert!(true) placeholder tests | rd_client.rs, dav_fs.rs, tasks.rs |
| 33 | Wrap sled(redb) I/O in spawn_blocking | tasks.rs |
| 34 | Better IncompleteMessage detection | main.rs |
