# SP0 — Minimal Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce the shared scaffolding the Trakt feature needs — a `Store` repository layer (with DB versioning + auto-recovery), an `AppState` context, and a layered `Config` — as a **behaviour-preserving** refactor.

**Architecture:** Extract scattered `std::env::var` reads into a `Config` struct; move the inline `redb` transactions out of `tasks.rs` into a `Store` type that owns all table definitions, stamps a schema version, and **never fails startup** on a bad/incompatible database (it moves the file aside and recreates it); consolidate the shared `Arc` handles into an `AppState` carried by the scan task. The WebDAV `dav_fs.rs` is intentionally **not** touched (it has no Store/Config concern yet). The entire existing test suite must pass unchanged — that is the proof no behaviour changed.

**Tech Stack:** Rust, tokio, redb 3.1, serde_json, thiserror, tracing.

**Branch:** `trakt-integration` (already checked out).

**Spec:** `docs/superpowers/specs/2026-06-05-trakt-integration-design.md` (§5 SP0, §8 testing, decision #17 DB recovery).

---

## File structure

| File | Change | Responsibility |
|------|--------|----------------|
| `src/config.rs` | **Create** | `Config` struct + `from_env()` / `from_parts()` (pure, testable). Owns all startup env parsing + validation. |
| `src/store.rs` | **Create** | `Store` wrapping `Arc<redb::Database>`. Owns table definitions, schema version + migration hook, `open()` with auto-recovery, and typed async accessors for the `matches` cache. |
| `src/app_state.rs` | **Create** | `AppState` — `Clone` bundle of shared handles (provider, tmdb, vfs, store, repair, config, jellyfin, http). |
| `src/mapper.rs` | Modify | Declare the three new modules. |
| `src/main.rs` | Modify | Build `Config` → `Store::open` → `AppState`; pass `AppState` to the scan task; unpack it for the existing `DebridFileSystem::new`. |
| `src/tasks.rs` | Modify | `ScanConfig` holds an `AppState`; `run_scan_loop` uses `Store` instead of inline `redb`; remove `MATCHES_TABLE` + `redb` imports. |
| `CLAUDE.md`, `README.md` | Modify | Document the new modules, DB auto-recovery, and the unchanged env vars. |

**Out of scope (later phases):** scheduler split, repair generalisation, episode model, vfs selection inversion, web router, new tables (owned/authoritative/blacklist/wanted/settings). The migration framework lands here but ships **zero** migrations.

---

## Task 0: Baseline

- [ ] **Step 1: Confirm the suite is green before any change**

Run: `cargo test`
Expected: PASS (this is the behaviour baseline SP0 must preserve).

- [ ] **Step 2: Confirm the working tree is clean and on the feature branch**

Run: `git status`
Expected: on `trakt-integration`, clean (the two spec commits already landed).

---

## Task 1: `Config` module

**Files:**
- Create: `src/config.rs`
- Modify: `src/mapper.rs` (add `pub mod config;`)
- Modify: `src/main.rs` (replace scattered env reads)

- [ ] **Step 1: Write the failing tests**

Create `src/config.rs` with only the test module (the `Config` impl comes next):

```rust
use crate::error::AppError;
use crate::provider::{choose_provider, ProviderKind};
use tracing::warn;

// (impl added in Step 3)

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(
        rd: Option<&str>,
        tb: Option<&str>,
        tmdb: Option<&str>,
        scan: Option<&str>,
        db: Option<&str>,
        port: Option<&str>,
    ) -> Result<Config, AppError> {
        Config::from_parts(
            rd.map(String::from),
            tb.map(String::from),
            tmdb.map(String::from),
            scan.map(String::from),
            db.map(String::from),
            port.map(String::from),
        )
    }

    #[test]
    fn rd_only_with_defaults() {
        let c = parts(Some("rd-tok"), None, Some("tmdb"), None, None, None).unwrap();
        assert_eq!(c.provider_kind, ProviderKind::RealDebrid);
        assert_eq!(c.provider_token, "rd-tok");
        assert_eq!(c.tmdb_api_key, "tmdb");
        assert_eq!(c.scan_interval_secs, 60);
        assert_eq!(c.db_path, "metadata.db");
        assert_eq!(c.port, 8080);
    }

    #[test]
    fn torbox_only() {
        let c = parts(None, Some("tb-tok"), Some("tmdb"), None, None, None).unwrap();
        assert_eq!(c.provider_kind, ProviderKind::TorBox);
        assert_eq!(c.provider_token, "tb-tok");
    }

    #[test]
    fn both_tokens_is_error() {
        assert!(parts(Some("a"), Some("b"), Some("tmdb"), None, None, None).is_err());
    }

    #[test]
    fn neither_token_is_error() {
        assert!(parts(None, None, Some("tmdb"), None, None, None).is_err());
    }

    #[test]
    fn missing_tmdb_is_error() {
        assert!(parts(Some("rd"), None, None, None, None, None).is_err());
    }

    #[test]
    fn scan_interval_clamped_and_parsed() {
        // Below the 10s floor → clamped to 10.
        assert_eq!(
            parts(Some("rd"), None, Some("t"), Some("5"), None, None).unwrap().scan_interval_secs,
            10
        );
        // Invalid → falls back to 60.
        assert_eq!(
            parts(Some("rd"), None, Some("t"), Some("abc"), None, None).unwrap().scan_interval_secs,
            60
        );
        // Valid → used.
        assert_eq!(
            parts(Some("rd"), None, Some("t"), Some("120"), None, None).unwrap().scan_interval_secs,
            120
        );
    }

    #[test]
    fn port_parsed_with_fallback() {
        assert_eq!(parts(Some("rd"), None, Some("t"), None, None, Some("9000")).unwrap().port, 9000);
        assert_eq!(parts(Some("rd"), None, Some("t"), None, None, Some("nope")).unwrap().port, 8080);
    }

    #[test]
    fn db_path_override() {
        assert_eq!(
            parts(Some("rd"), None, Some("t"), None, Some("/data/x.db"), None).unwrap().db_path,
            "/data/x.db"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config`
Expected: FAIL to compile — `Config` not defined.

- [ ] **Step 3: Implement `Config`**

Insert this above the `#[cfg(test)]` module in `src/config.rs`:

```rust
/// Startup configuration parsed from environment variables.
///
/// These values are fixed at startup. A future DB-backed override layer (web-UI
/// settings, SP4) will supply runtime-tunable *preferences* alongside this; the
/// startup values here (tokens, paths, port) are not runtime-overridable, so they
/// are plain fields rather than accessors.
#[derive(Debug, Clone)]
pub struct Config {
    pub provider_kind: ProviderKind,
    pub provider_token: String,
    pub tmdb_api_key: String,
    pub scan_interval_secs: u64,
    pub db_path: String,
    pub port: u16,
}

impl Config {
    /// Build from the process environment (reads the same variables as before).
    pub fn from_env() -> Result<Self, AppError> {
        Self::from_parts(
            std::env::var("RD_API_TOKEN").ok(),
            std::env::var("TORBOX_API_KEY").ok(),
            std::env::var("TMDB_API_KEY").ok(),
            std::env::var("SCAN_INTERVAL_SECS").ok(),
            std::env::var("DB_PATH").ok(),
            std::env::var("PORT").ok(),
        )
    }

    /// Pure construction from raw optional values — unit-testable without touching
    /// the process environment. Mirrors the previous inline logic exactly.
    pub fn from_parts(
        rd_token: Option<String>,
        torbox_token: Option<String>,
        tmdb_api_key: Option<String>,
        scan_interval_secs: Option<String>,
        db_path: Option<String>,
        port: Option<String>,
    ) -> Result<Self, AppError> {
        let (provider_kind, provider_token) = choose_provider(rd_token, torbox_token)?;

        let tmdb_api_key = tmdb_api_key
            .ok_or_else(|| AppError::Config("TMDB_API_KEY must be set".to_string()))?
            .trim()
            .to_string();

        let scan_interval_secs = match scan_interval_secs {
            Some(s) => s.parse::<u64>().unwrap_or_else(|_| {
                warn!("Invalid SCAN_INTERVAL_SECS value '{}', falling back to 60", s);
                60
            }),
            None => 60,
        }
        .max(10); // Enforce minimum 10s to avoid hammering the provider API.

        let db_path = db_path.unwrap_or_else(|| "metadata.db".to_string());

        let port = match port {
            Some(s) => s.parse::<u16>().unwrap_or_else(|_| {
                warn!("Invalid PORT value '{}', falling back to 8080", s);
                8080
            }),
            None => 8080,
        };

        Ok(Self {
            provider_kind,
            provider_token,
            tmdb_api_key,
            scan_interval_secs,
            db_path,
            port,
        })
    }
}
```

- [ ] **Step 4: Declare the module**

In `src/mapper.rs`, add this line to the module list (declaration order is irrelevant; keep it alphabetical for tidiness):

```rust
pub mod config;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib config`
Expected: PASS (all 8 config tests).

- [ ] **Step 6: Wire `Config` into `main.rs`**

In `src/main.rs`, update the imports — replace:

```rust
use debridmoviemapper::provider::{choose_provider, DebridProvider, ProviderKind};
```

with:

```rust
use debridmoviemapper::config::Config;
use debridmoviemapper::provider::{DebridProvider, ProviderKind};
```

and change:

```rust
use tracing::{info, warn};
```

to:

```rust
use tracing::info;
```

(`warn!` is no longer used unqualified in `main.rs` after this task; the one remaining call uses `tracing::warn!`.)

- [ ] **Step 7: Replace the provider/tmdb/scan-interval block**

In `src/main.rs`, replace this block:

```rust
    let (provider_kind, provider_token) = choose_provider(
        std::env::var("RD_API_TOKEN").ok(),
        std::env::var("TORBOX_API_KEY").ok(),
    )
    .unwrap_or_else(|e| {
        eprintln!("Configuration error: {}", e);
        std::process::exit(1);
    });

    // Construct the selected provider from the chosen token. Either client surfaces
    // a clear configuration error here (via `?`) rather than tripping a later panic.
    let provider: Arc<dyn DebridProvider> = match provider_kind {
        ProviderKind::RealDebrid => Arc::new(RealDebridClient::new(provider_token)?),
        ProviderKind::TorBox => Arc::new(TorBoxClient::new(provider_token)?),
    };

    let tmdb_api_key = std::env::var("TMDB_API_KEY")
        .unwrap_or_else(|_| {
            eprintln!("Configuration error: TMDB_API_KEY must be set");
            std::process::exit(1);
        })
        .trim()
        .to_string();
    let scan_interval_secs = match std::env::var("SCAN_INTERVAL_SECS") {
        Ok(s) => s.parse::<u64>().unwrap_or_else(|_| {
            warn!(
                "Invalid SCAN_INTERVAL_SECS value '{}', falling back to 60",
                s
            );
            60
        }),
        Err(_) => 60,
    }
    .max(10); // Enforce minimum 10s to prevent hammering the Real-Debrid API

    info!("Scan interval: {}s", scan_interval_secs);

    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key)?);
```

with:

```rust
    let config = Config::from_env().unwrap_or_else(|e| {
        eprintln!("Configuration error: {}", e);
        std::process::exit(1);
    });

    // Construct the selected provider from the chosen token. Either client surfaces
    // a clear configuration error here (via `?`) rather than tripping a later panic.
    let provider: Arc<dyn DebridProvider> = match config.provider_kind {
        ProviderKind::RealDebrid => Arc::new(RealDebridClient::new(config.provider_token.clone())?),
        ProviderKind::TorBox => Arc::new(TorBoxClient::new(config.provider_token.clone())?),
    };

    info!("Scan interval: {}s", config.scan_interval_secs);

    let tmdb_client = Arc::new(TmdbClient::new(config.tmdb_api_key.clone())?);
```

- [ ] **Step 8: Replace the `DB_PATH` read**

In `src/main.rs`, replace:

```rust
    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "metadata.db".to_string());
    // Surface a recoverable, user-fixable failure (locked DB, read-only volume) as a
    // clean error exit rather than a panic with a backtrace.
    let db = Arc::new(Database::create(&db_path)?);
```

with:

```rust
    // Surface a recoverable, user-fixable failure (locked DB, read-only volume) as a
    // clean error exit rather than a panic with a backtrace.
    let db = Arc::new(Database::create(&config.db_path)?);
```

- [ ] **Step 9: Use `config.scan_interval_secs` in `ScanConfig`**

In `src/main.rs`, in the `ScanConfig { ... }` construction, replace:

```rust
            interval_secs: scan_interval_secs,
```

with:

```rust
            interval_secs: config.scan_interval_secs,
```

- [ ] **Step 10: Replace the `PORT` read**

In `src/main.rs`, replace:

```rust
    let port: u16 = match std::env::var("PORT") {
        Ok(s) => s.parse().unwrap_or_else(|_| {
            warn!("Invalid PORT value '{}', falling back to 8080", s);
            8080
        }),
        Err(_) => 8080,
    };
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
```

with:

```rust
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
```

> Note: the `--healthcheck` early-exit block at the top of `main` keeps its own raw `PORT` read — it runs before `dotenvy`/`Config` and must stay self-contained.

- [ ] **Step 11: Build and run the full suite**

Run: `cargo build && cargo test`
Expected: PASS. Behaviour is identical; only the *source* of the values changed.

> Minor accepted deviation: a missing `TMDB_API_KEY` now prints `Configuration error: Invalid configuration: TMDB_API_KEY must be set` (was `Configuration error: TMDB_API_KEY must be set`). Same exit code (1) and same trigger; only stderr wording differs. No test asserts on this string.

- [ ] **Step 12: Commit**

```bash
git add src/config.rs src/mapper.rs src/main.rs
git commit -m "refactor: extract startup env parsing into Config" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `Store` module (with versioning + auto-recovery)

**Files:**
- Create: `src/store.rs`
- Modify: `src/mapper.rs` (add `pub mod store;`)

This task adds `Store` and its tests but does **not** yet wire it into the app (that's Task 3). A duplicate `matches` `TableDefinition` temporarily existing in both `tasks.rs` and `store.rs` is harmless — they name the same underlying table.

- [ ] **Step 1: Write the round-trip + backward-compat tests**

Create `src/store.rs` with the test module first (impl follows in Step 3):

```rust
use crate::error::AppError;
use crate::rd_client::TorrentInfo;
use crate::vfs::MediaMetadata;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

// (impl + constants added in Step 3)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MediaType;
    use redb::backends::InMemoryBackend;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn mem_store() -> Store {
        let db = Database::builder()
            .create_with_backend(InMemoryBackend::new())
            .unwrap();
        Store::from_database(Arc::new(db)).unwrap()
    }

    fn movie(title: &str) -> MediaMetadata {
        MediaMetadata {
            title: title.to_string(),
            year: Some("2023".to_string()),
            media_type: MediaType::Movie,
            external_id: None,
        }
    }

    fn info(id: &str) -> TorrentInfo {
        TorrentInfo {
            id: id.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let store = mem_store();
        store
            .put_matches(vec![("t1".to_string(), info("t1"), movie("Movie"))])
            .await
            .unwrap();
        let got = store.get_match("t1".to_string()).await.expect("present");
        assert_eq!(got.0.id, "t1");
        assert_eq!(got.1.title, "Movie");
    }

    #[tokio::test]
    async fn put_batch_then_load_all() {
        let store = mem_store();
        store
            .put_matches(vec![
                ("a".to_string(), info("a"), movie("A")),
                ("b".to_string(), info("b"), movie("B")),
            ])
            .await
            .unwrap();
        let all = store.load_all_matches().await;
        assert_eq!(all.len(), 2);
        assert!(all.contains_key("a") && all.contains_key("b"));
    }

    #[tokio::test]
    async fn remove_deletes_entry() {
        let store = mem_store();
        store
            .put_matches(vec![("x".to_string(), info("x"), movie("X"))])
            .await
            .unwrap();
        store.remove_matches(vec!["x".to_string()]).await.unwrap();
        assert!(store.get_match("x".to_string()).await.is_none());
    }

    #[tokio::test]
    async fn replace_swaps_old_for_new() {
        let store = mem_store();
        store
            .put_matches(vec![("old".to_string(), info("old"), movie("Title"))])
            .await
            .unwrap();
        store
            .replace_match("old".to_string(), "new".to_string(), info("new"), movie("Title"))
            .await
            .unwrap();
        assert!(store.get_match("old".to_string()).await.is_none());
        assert_eq!(store.get_match("new".to_string()).await.unwrap().0.id, "new");
    }

    #[tokio::test]
    async fn loads_db_written_in_old_inline_encoding() {
        // Write a row using the EXACT pre-Store encoding, then read it via Store.
        let db = Arc::new(
            Database::builder()
                .create_with_backend(InMemoryBackend::new())
                .unwrap(),
        );
        {
            let i = info("leg");
            let m = movie("Legacy");
            let bytes = serde_json::to_vec(&(&i, &m)).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let def: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");
                let mut t = txn.open_table(def).unwrap();
                t.insert("leg", bytes.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::from_database(db).unwrap();
        let got = store.get_match("leg".to_string()).await.expect("legacy row");
        assert_eq!(got.1.title, "Legacy");
    }

    // --- recovery tests (real temp files) ---

    struct TempDb {
        path: String,
    }
    impl TempDb {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let mut p = std::env::temp_dir();
            p.push(format!("dmm_store_{}_{}_{}.redb", tag, std::process::id(), n));
            TempDb {
                path: p.to_string_lossy().into_owned(),
            }
        }
        fn corrupt_path(&self) -> String {
            format!("{}.corrupt", self.path)
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(self.corrupt_path());
        }
    }

    #[tokio::test]
    async fn open_creates_fresh_versioned_db() {
        let tmp = TempDb::new("fresh");
        let store = Store::open(&tmp.path).unwrap();
        store
            .put_matches(vec![("a".to_string(), info("a"), movie("A"))])
            .await
            .unwrap();
        assert!(store.get_match("a".to_string()).await.is_some());
    }

    #[tokio::test]
    async fn open_recovers_from_corrupt_file() {
        let tmp = TempDb::new("corrupt");
        std::fs::write(&tmp.path, b"this is not a redb file").unwrap();
        let store = Store::open(&tmp.path).expect("must recover, not error");
        assert!(
            std::path::Path::new(&tmp.corrupt_path()).exists(),
            "corrupt file should be moved aside"
        );
        store
            .put_matches(vec![("a".to_string(), info("a"), movie("A"))])
            .await
            .unwrap();
        assert!(store.get_match("a".to_string()).await.is_some());
    }

    #[tokio::test]
    async fn open_keeps_data_from_unversioned_db() {
        let tmp = TempDb::new("unversioned");
        {
            // A valid redb DB with only the matches table + a row (no meta table) —
            // i.e. a database written by the pre-Store code.
            let db = Database::create(&tmp.path).unwrap();
            let i = info("keep");
            let m = movie("Keep");
            let bytes = serde_json::to_vec(&(&i, &m)).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let def: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");
                let mut t = txn.open_table(def).unwrap();
                t.insert("keep", bytes.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        } // db dropped → file lock released

        let store = Store::open(&tmp.path).unwrap();
        assert_eq!(
            store.get_match("keep".to_string()).await.unwrap().1.title,
            "Keep"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "a valid unversioned DB must NOT be moved aside"
        );
    }

    #[tokio::test]
    async fn open_recovers_from_newer_version() {
        let tmp = TempDb::new("newer");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let meta_def: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut m = txn.open_table(meta_def).unwrap();
                m.insert("schema_version", &999u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();
        assert!(
            std::path::Path::new(&tmp.corrupt_path()).exists(),
            "a newer-than-binary DB should be moved aside"
        );
        store
            .put_matches(vec![("a".to_string(), info("a"), movie("A"))])
            .await
            .unwrap();
        assert!(store.get_match("a".to_string()).await.is_some());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib store`
Expected: FAIL to compile — `Store` not defined.

- [ ] **Step 3: Implement `Store`**

Insert this above the `#[cfg(test)]` module in `src/store.rs`:

```rust
/// Current on-disk schema version. Bump when a migration is added in `run_migrations`.
pub const SCHEMA_VERSION: u64 = 1;

/// TMDB identification cache: torrent id -> serde_json((TorrentInfo, MediaMetadata)).
/// Same name + value encoding as the pre-Store inline table, so existing databases
/// load unchanged.
const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");
/// Internal metadata (schema version, etc.).
const META_TABLE: TableDefinition<&str, u64> = TableDefinition::new("meta");
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Owns the redb database and all table access. Cheap to clone (the database is an `Arc`).
#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
}

impl Store {
    /// Open (or create) the database at `path`, recovering automatically from an
    /// unreadable / incompatible / corrupt / newer-than-binary file rather than
    /// failing startup. Synchronous — called once at startup.
    pub fn open(path: &str) -> Result<Self, AppError> {
        let db = match Self::try_open(path) {
            Ok(db) => db,
            Err(reason) => {
                warn!(
                    "Database {} is unusable ({}); moving it aside and recreating",
                    path, reason
                );
                Self::move_aside_and_create(path)?
            }
        };
        Ok(Store { db: Arc::new(db) })
    }

    /// Build from an already-open database (e.g. an in-memory backend). Ensures the
    /// schema is present and current. Used by tests and callers that manage the
    /// `Database` themselves.
    pub fn from_database(db: Arc<Database>) -> Result<Self, AppError> {
        let version = Self::read_version(&db).map_err(AppError::Db)?;
        if version > SCHEMA_VERSION {
            return Err(AppError::Config(format!(
                "database schema v{} is newer than supported v{}",
                version, SCHEMA_VERSION
            )));
        }
        Self::ensure_schema(&db, version).map_err(AppError::Db)?;
        Ok(Store { db })
    }

    /// Open the file and bring its schema to the current version. Returns
    /// `Err(reason)` describing why the file is unusable so the caller can recover.
    fn try_open(path: &str) -> Result<Database, String> {
        let db = Database::create(path).map_err(|e| format!("open failed: {e}"))?;
        let version = Self::read_version(&db).map_err(|e| format!("schema read failed: {e}"))?;
        if version > SCHEMA_VERSION {
            return Err(format!(
                "schema v{version} is newer than supported v{SCHEMA_VERSION}"
            ));
        }
        Self::ensure_schema(&db, version).map_err(|e| format!("schema init failed: {e}"))?;
        Ok(db)
    }

    /// Read the stored schema version. Returns 0 when the database has no `meta`
    /// table yet (a fresh or pre-versioning database).
    fn read_version(db: &Database) -> Result<u64, redb::Error> {
        let read_txn = db.begin_read()?;
        let table = match read_txn.open_table(META_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        Ok(table.get(SCHEMA_VERSION_KEY)?.map(|g| g.value()).unwrap_or(0))
    }

    /// Ensure required tables exist, run any pending migrations, and stamp the
    /// current schema version. Idempotent.
    fn ensure_schema(db: &Database, version: u64) -> Result<(), redb::Error> {
        if version < SCHEMA_VERSION {
            Self::run_migrations(db, version)?;
        }
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(MATCHES_TABLE)?; // create if absent
            let mut meta = write_txn.open_table(META_TABLE)?; // create if absent
            meta.insert(SCHEMA_VERSION_KEY, &SCHEMA_VERSION)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Apply forward migrations from `from_version` up to `SCHEMA_VERSION`.
    /// SP0 ships **no** migrations. SP1+ add steps here (each in its own
    /// transaction, keyed on `from_version`) BEFORE the version stamp is written,
    /// migrating authoritative tables rather than dropping them.
    fn run_migrations(_db: &Database, _from_version: u64) -> Result<(), redb::Error> {
        Ok(())
    }

    /// Move an unusable database aside to `<path>.corrupt` and create a fresh one.
    /// Never deletes outright unless the rename itself fails.
    fn move_aside_and_create(path: &str) -> Result<Database, AppError> {
        let backup = format!("{}.corrupt", path);
        if std::path::Path::new(path).exists() {
            match std::fs::rename(path, &backup) {
                Ok(()) => info!("Moved aside unusable database to {}", backup),
                Err(e) => {
                    error!(
                        "Failed to move aside database {} -> {} ({}); removing it instead",
                        path, backup, e
                    );
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        let db = Database::create(path).map_err(|e| AppError::Db(e.into()))?;
        Self::ensure_schema(&db, 0).map_err(AppError::Db)?;
        Ok(db)
    }

    /// Load every cached identification. Mirrors the previous startup load: entries
    /// that fail to deserialise are skipped rather than failing the whole load.
    pub async fn load_all_matches(&self) -> HashMap<String, (TorrentInfo, MediaMetadata)> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut map = HashMap::new();
            if let Ok(read_txn) = db.begin_read() {
                if let Ok(table) = read_txn.open_table(MATCHES_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (key, value) = entry;
                            if let Ok(data) =
                                serde_json::from_slice::<(TorrentInfo, MediaMetadata)>(value.value())
                            {
                                map.insert(key.value().to_string(), data);
                            }
                        }
                    }
                }
            }
            map
        })
        .await
        .unwrap_or_else(|e| {
            error!("Failed to load persisted matches: {:?}", e);
            HashMap::new()
        })
    }

    /// Look up a single cached identification by torrent id.
    pub async fn get_match(&self, id: String) -> Option<(TorrentInfo, MediaMetadata)> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let read_txn = db.begin_read().ok()?;
            let table = read_txn.open_table(MATCHES_TABLE).ok()?;
            let entry = table.get(id.as_str()).ok()??;
            serde_json::from_slice::<(TorrentInfo, MediaMetadata)>(entry.value()).ok()
        })
        .await
        .ok()
        .flatten()
    }

    /// Batch-insert identifications. Serialises with the same encoding as before; an
    /// entry that fails to serialise is logged and skipped (matching prior behaviour).
    pub async fn put_matches(
        &self,
        entries: Vec<(String, TorrentInfo, MediaMetadata)>,
    ) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(MATCHES_TABLE)?;
                for (id, info, metadata) in &entries {
                    match serde_json::to_vec(&(info, metadata)) {
                        Ok(bytes) => {
                            table.insert(id.as_str(), bytes.as_slice())?;
                        }
                        Err(e) => error!("Failed to serialise match {}: {}", id, e),
                    }
                }
            }
            write_txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    /// Remove cached identifications by torrent id.
    pub async fn remove_matches(&self, ids: Vec<String>) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(MATCHES_TABLE)?;
                for id in &ids {
                    table.remove(id.as_str())?;
                }
            }
            write_txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    /// Atomically remove `old_id` and insert `new_id` (repair-replacement remap).
    pub async fn replace_match(
        &self,
        old_id: String,
        new_id: String,
        info: TorrentInfo,
        metadata: MediaMetadata,
    ) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(MATCHES_TABLE)?;
                table.remove(old_id.as_str())?;
                match serde_json::to_vec(&(&info, &metadata)) {
                    Ok(bytes) => {
                        table.insert(new_id.as_str(), bytes.as_slice())?;
                    }
                    Err(e) => error!("Failed to serialise replacement match {}: {}", new_id, e),
                }
            }
            write_txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    /// Collapse a `spawn_blocking` result: a redb error propagates; a join (panic)
    /// is logged and swallowed so the scan loop keeps running (matching prior
    /// "log and continue" behaviour).
    fn flatten_join(
        result: Result<Result<(), redb::Error>, tokio::task::JoinError>,
    ) -> Result<(), AppError> {
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(AppError::Db(e)),
            Err(e) => {
                error!("redb blocking task did not complete: {:?}", e);
                Ok(())
            }
        }
    }
}
```

- [ ] **Step 4: Declare the module**

In `src/mapper.rs`, add this line to the module list (alphabetically, after `pub mod repair;`):

```rust
pub mod store;
```

- [ ] **Step 5: Run the Store tests to verify they pass**

Run: `cargo test --lib store`
Expected: PASS (round-trip, backward-compat, and all four recovery tests).

> If `redb::TableError::TableDoesNotExist(_)` does not match this redb version's variant name, adjust the single match arm in `read_version` to the "table does not exist" variant — that is the only version-sensitive line.

- [ ] **Step 6: Run the full suite**

Run: `cargo test`
Expected: PASS (nothing else changed yet).

- [ ] **Step 7: Commit**

```bash
git add src/store.rs src/mapper.rs
git commit -m "feat: add Store repository layer with DB versioning and auto-recovery" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Wire `Store` + `AppState` into the application

**Files:**
- Create: `src/app_state.rs`
- Modify: `src/mapper.rs` (add `pub mod app_state;`)
- Modify: `src/main.rs` (build `Store::open` + `AppState`)
- Modify: `src/tasks.rs` (`ScanConfig` → `AppState`; replace inline `redb` with `Store`; remove `MATCHES_TABLE` + `redb` imports; update structural tests)

- [ ] **Step 1: Create `AppState`**

Create `src/app_state.rs`:

```rust
use crate::config::Config;
use crate::jellyfin_client::JellyfinClient;
use crate::provider::DebridProvider;
use crate::repair::RepairManager;
use crate::store::Store;
use crate::tmdb_client::TmdbClient;
use crate::vfs::DebridVfs;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared application state, constructed once at startup and cloned (cheaply —
/// every field is an `Arc`/handle) into the background scan task. Future phases
/// (scheduler, web UI) hang their handles off this struct.
#[derive(Clone)]
pub struct AppState {
    pub provider: Arc<dyn DebridProvider>,
    pub tmdb_client: Arc<TmdbClient>,
    pub vfs: Arc<RwLock<DebridVfs>>,
    pub store: Store,
    pub repair_manager: Arc<RepairManager>,
    pub config: Arc<Config>,
    pub jellyfin_client: Option<Arc<JellyfinClient>>,
    pub http_client: reqwest::Client,
}
```

- [ ] **Step 2: Declare the module**

In `src/mapper.rs`, add this line to the module list (alphabetically, first — before `pub mod config;`):

```rust
pub mod app_state;
```

- [ ] **Step 3: Replace the `matches`-table constant + `redb` imports in `tasks.rs`**

In `src/tasks.rs`, remove these two items:

```rust
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
```

```rust
pub const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");
```

and add these imports near the top (with the other `use crate::...` lines):

```rust
use crate::app_state::AppState;
use crate::store::Store;
```

- [ ] **Step 4: Replace `ScanConfig` with an `AppState` carrier**

In `src/tasks.rs`, replace:

```rust
pub struct ScanConfig {
    pub rd_client: Arc<dyn DebridProvider>,
    pub tmdb_client: Arc<TmdbClient>,
    pub vfs: Arc<RwLock<DebridVfs>>,
    pub db: Arc<redb::Database>,
    pub repair_manager: Arc<RepairManager>,
    pub interval_secs: u64,
    pub jellyfin_client: Option<Arc<crate::jellyfin_client::JellyfinClient>>,
}
```

with:

```rust
pub struct ScanConfig {
    pub app: AppState,
}
```

- [ ] **Step 5: Update the `run_scan_loop` signature + destructure**

In `src/tasks.rs`, replace:

```rust
pub async fn run_scan_loop(config: ScanConfig, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let ScanConfig {
        rd_client,
        tmdb_client,
        vfs,
        db,
        repair_manager,
        interval_secs,
        jellyfin_client,
    } = config;
```

with:

```rust
pub async fn run_scan_loop(scan_config: ScanConfig, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let AppState {
        provider: rd_client,
        tmdb_client,
        vfs,
        store,
        repair_manager,
        config,
        jellyfin_client,
        http_client: _,
    } = scan_config.app;
    let interval_secs = config.scan_interval_secs;
```

- [ ] **Step 6: Replace the startup persisted-load with `Store`**

In `src/tasks.rs`, replace the whole `spawn_blocking` startup-load block:

```rust
    // Load persisted matches from DB on startup
    let db_clone = db.clone();
    let persisted: HashMap<String, (crate::rd_client::TorrentInfo, MediaMetadata)> =
        tokio::task::spawn_blocking(move || {
            let mut map = HashMap::new();
            if let Ok(read_txn) = db_clone.begin_read() {
                if let Ok(table) = read_txn.open_table(MATCHES_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (key, value) = entry;
                            let id = key.value().to_string();
                            if let Ok(data) = serde_json::from_slice::<(
                                crate::rd_client::TorrentInfo,
                                MediaMetadata,
                            )>(value.value())
                            {
                                map.insert(id, data);
                            }
                        }
                    }
                }
            }
            map
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to load persisted matches: {:?}", e);
            HashMap::new()
        });
```

with:

```rust
    // Load persisted matches from DB on startup
    let persisted: HashMap<String, (crate::rd_client::TorrentInfo, MediaMetadata)> =
        store.load_all_matches().await;
```

- [ ] **Step 7: Replace the repair-replacement persist block**

In `src/tasks.rs`, replace:

```rust
                                        // Serialize from references before cloning for owned storage
                                        if let Ok(data_bytes) =
                                            serde_json::to_vec(&(&new_info, &metadata))
                                        {
                                            let db_clone = db.clone();
                                            let new_id = torrent.id.clone();
                                            let old_id = old_id.clone();
                                            match tokio::task::spawn_blocking(
                                                move || -> Result<(), redb::Error> {
                                                    let write_txn = db_clone.begin_write()?;
                                                    {
                                                        let mut table =
                                                            write_txn.open_table(MATCHES_TABLE)?;
                                                        table.remove(old_id.as_str())?;
                                                        table.insert(
                                                            new_id.as_str(),
                                                            data_bytes.as_slice(),
                                                        )?;
                                                    }
                                                    write_txn.commit()?;
                                                    Ok(())
                                                },
                                            )
                                            .await
                                            {
                                                Ok(Ok(())) => {}
                                                Ok(Err(e)) => error!("Failed to persist repair replacement to database: {}", e),
                                                Err(e) => error!("Failed to persist repair replacement to database: {:?}", e),
                                            }
                                        }
                                        seen_torrents.insert(
                                            torrent.id.clone(),
                                            (new_info.clone(), metadata.clone()),
                                        );
                                        current_data.push((new_info, metadata));
```

with:

```rust
                                        if let Err(e) = store
                                            .replace_match(
                                                old_id.clone(),
                                                torrent.id.clone(),
                                                new_info.clone(),
                                                metadata.clone(),
                                            )
                                            .await
                                        {
                                            error!(
                                                "Failed to persist repair replacement to database: {}",
                                                e
                                            );
                                        }
                                        seen_torrents.insert(
                                            torrent.id.clone(),
                                            (new_info.clone(), metadata.clone()),
                                        );
                                        current_data.push((new_info, metadata));
```

- [ ] **Step 8: Replace the per-torrent cached lookup**

In `src/tasks.rs`, replace:

```rust
                            let db_clone = db.clone();
                            let torrent_id = torrent.id.clone();
                            let cached = tokio::task::spawn_blocking(move || {
                                let read_txn = db_clone.begin_read().ok()?;
                                let table = read_txn.open_table(MATCHES_TABLE).ok()?;
                                let entry = table.get(torrent_id.as_str()).ok()??;
                                serde_json::from_slice::<(
                                    crate::rd_client::TorrentInfo,
                                    MediaMetadata,
                                )>(entry.value())
                                .ok()
                            })
                            .await
                            .ok()
                            .flatten();
```

with:

```rust
                            let cached = store.get_match(torrent.id.clone()).await;
```

- [ ] **Step 9: Change the pending-writes buffer type + push**

In `src/tasks.rs`, replace:

```rust
                    let mut pending_db_writes: Vec<(String, Vec<u8>)> = Vec::new();
```

with:

```rust
                    let mut pending_db_writes: Vec<(
                        String,
                        crate::rd_client::TorrentInfo,
                        MediaMetadata,
                    )> = Vec::new();
```

and replace:

```rust
                            Ok((id, info, metadata)) => {
                                if let Ok(data_bytes) = serde_json::to_vec(&(&info, &metadata)) {
                                    pending_db_writes.push((id.clone(), data_bytes));
                                }
                                seen_torrents.insert(id, (info.clone(), metadata.clone()));
                                current_data.push((info, metadata));
                            }
```

with:

```rust
                            Ok((id, info, metadata)) => {
                                pending_db_writes.push((id.clone(), info.clone(), metadata.clone()));
                                seen_torrents.insert(id, (info.clone(), metadata.clone()));
                                current_data.push((info, metadata));
                            }
```

- [ ] **Step 10: Point the two `flush_db_writes` call sites at `&store`**

In `src/tasks.rs`, there are two calls `flush_db_writes(&db, &mut pending_db_writes).await`. Change both to:

```rust
                                flush_db_writes(&store, &mut pending_db_writes).await;
```

(One is in the shutdown branch, one at the progress checkpoint — both inside the identification loop.)

- [ ] **Step 11: Replace the stale-removal block**

In `src/tasks.rs`, replace:

```rust
                if !stale_ids.is_empty() {
                    info!("Removing {} stale entries from database", stale_ids.len());
                    let db_clone = db.clone();
                    match tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
                        let write_txn = db_clone.begin_write()?;
                        {
                            let mut table = write_txn.open_table(MATCHES_TABLE)?;
                            for id in &stale_ids {
                                table.remove(id.as_str())?;
                            }
                        }
                        write_txn.commit()?;
                        Ok(())
                    })
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            error!("Failed to remove stale entries from database: {}", e)
                        }
                        Err(e) => {
                            error!("Failed to remove stale entries from database: {:?}", e)
                        }
                    }
                }
```

with:

```rust
                if !stale_ids.is_empty() {
                    info!("Removing {} stale entries from database", stale_ids.len());
                    if let Err(e) = store.remove_matches(stale_ids).await {
                        error!("Failed to remove stale entries from database: {}", e);
                    }
                }
```

- [ ] **Step 12: Rewrite the `flush_db_writes` helper**

In `src/tasks.rs`, replace the entire `flush_db_writes` function:

```rust
/// Flush a batch of pending DB writes in a single transaction.
/// Clears `pending_writes` on success or failure.
async fn flush_db_writes(db: &Arc<redb::Database>, pending_writes: &mut Vec<(String, Vec<u8>)>) {
    let writes = std::mem::take(pending_writes);
    let count = writes.len();
    let db_clone = db.clone();
    match tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
        let write_txn = db_clone.begin_write()?;
        {
            let mut table = write_txn.open_table(MATCHES_TABLE)?;
            for (id, data_bytes) in &writes {
                table.insert(id.as_str(), data_bytes.as_slice())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            error!(
                "Failed to persist {} torrent identifications to database: {}",
                count, e
            );
        }
        Err(e) => {
            error!(
                "Failed to persist {} torrent identifications to database: {:?}",
                count, e
            );
        }
    }
}
```

with:

```rust
/// Flush a batch of pending DB writes via the Store. Clears `pending_writes`.
async fn flush_db_writes(
    store: &Store,
    pending_writes: &mut Vec<(String, crate::rd_client::TorrentInfo, MediaMetadata)>,
) {
    if pending_writes.is_empty() {
        return;
    }
    let writes = std::mem::take(pending_writes);
    let count = writes.len();
    if let Err(e) = store.put_matches(writes).await {
        error!(
            "Failed to persist {} torrent identifications to database: {}",
            count, e
        );
    }
}
```

- [ ] **Step 13: Update `main.rs` — build `Store` + `AppState`**

In `src/main.rs`, update the import line:

```rust
use debridmoviemapper::tasks::{ScanConfig, MATCHES_TABLE};
```

to:

```rust
use debridmoviemapper::app_state::AppState;
use debridmoviemapper::tasks::ScanConfig;
```

and remove the now-unused redb import:

```rust
use redb::Database;
```

- [ ] **Step 14: Replace DB creation with `Store::open`**

In `src/main.rs`, replace:

```rust
    // Surface a recoverable, user-fixable failure (locked DB, read-only volume) as a
    // clean error exit rather than a panic with a backtrace.
    let db = Arc::new(Database::create(&config.db_path)?);

    // Ensure table exists on fresh databases
    {
        let write_txn = db.begin_write()?;
        write_txn.open_table(MATCHES_TABLE)?;
        write_txn.commit()?;
    }
```

with:

```rust
    // Open the metadata cache. Store::open never fails on an incompatible/corrupt
    // database: it moves the old file aside (<db_path>.corrupt) and recreates it.
    let store = debridmoviemapper::store::Store::open(&config.db_path)?;
```

- [ ] **Step 15: Build the `AppState` and pass it to the scan task**

In `src/main.rs`, replace the `ScanConfig` construction:

```rust
    let scan_handle = tokio::spawn(debridmoviemapper::tasks::run_scan_loop(
        ScanConfig {
            rd_client: provider.clone(),
            tmdb_client: tmdb_client.clone(),
            vfs: vfs.clone(),
            db: db.clone(),
            repair_manager: repair_manager.clone(),
            interval_secs: config.scan_interval_secs,
            jellyfin_client,
        },
        shutdown_rx,
    ));
```

with:

```rust
    let app_state = AppState {
        provider: provider.clone(),
        tmdb_client: tmdb_client.clone(),
        vfs: vfs.clone(),
        store: store.clone(),
        repair_manager: repair_manager.clone(),
        config: Arc::new(config),
        jellyfin_client,
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to build CDN HTTP client"),
    };

    let scan_handle = tokio::spawn(debridmoviemapper::tasks::run_scan_loop(
        ScanConfig {
            app: app_state.clone(),
        },
        shutdown_rx,
    ));
```

- [ ] **Step 16: Use the `AppState` http client for `dav_fs` and drop the duplicate**

In `src/main.rs`, replace:

```rust
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build CDN HTTP client");
    let dav_fs = DebridFileSystem::new(
        provider.clone(),
        vfs.clone(),
        repair_manager.clone(),
        http_client,
    );
```

with:

```rust
    let dav_fs = DebridFileSystem::new(
        app_state.provider.clone(),
        app_state.vfs.clone(),
        app_state.repair_manager.clone(),
        app_state.http_client.clone(),
    );
```

> `DebridFileSystem::new` keeps its existing signature — `dav_fs.rs` is deliberately untouched in SP0.

- [ ] **Step 17: Update the structural tests in `tasks.rs`**

In `src/tasks.rs`, in `mod tests`, replace `_assert_run_scan_loop_signature`:

```rust
    /// Compile-time check: run_scan_loop has the expected signature.
    #[allow(dead_code)]
    async fn _assert_run_scan_loop_signature(
        rd_client: Arc<dyn DebridProvider>,
        tmdb_client: Arc<TmdbClient>,
        vfs: Arc<RwLock<DebridVfs>>,
        db: Arc<redb::Database>,
        repair_manager: Arc<RepairManager>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let config = ScanConfig {
            rd_client,
            tmdb_client,
            vfs,
            db,
            repair_manager,
            interval_secs: 60,
            jellyfin_client: None,
        };
        run_scan_loop(config, shutdown).await;
    }
```

with:

```rust
    /// Compile-time check: run_scan_loop has the expected signature.
    #[allow(dead_code)]
    async fn _assert_run_scan_loop_signature(
        app: AppState,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        let config = ScanConfig { app };
        run_scan_loop(config, shutdown).await;
    }
```

Then in `mod provider_abstraction_tests`, replace `scan_config_holds_trait_object`:

```rust
    #[test]
    fn scan_config_holds_trait_object() {
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let db = Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let _config = ScanConfig {
            rd_client: provider.clone(),
            tmdb_client: Arc::new(TmdbClient::new("k".to_string()).unwrap()),
            vfs: Arc::new(RwLock::new(DebridVfs::new())),
            db,
            repair_manager: Arc::new(RepairManager::new(provider)),
            interval_secs: 60,
            jellyfin_client: None,
        };
    }
```

with:

```rust
    #[test]
    fn scan_config_holds_app_state() {
        use crate::app_state::AppState;
        use crate::config::Config;
        use crate::store::Store;

        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let db = Arc::new(
            redb::Database::builder()
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .unwrap(),
        );
        let store = Store::from_database(db).unwrap();
        let config = Config::from_parts(
            None,
            Some("tb".to_string()),
            Some("k".to_string()),
            None,
            None,
            None,
        )
        .unwrap();
        let app = AppState {
            provider: provider.clone(),
            tmdb_client: Arc::new(TmdbClient::new("k".to_string()).unwrap()),
            vfs: Arc::new(RwLock::new(DebridVfs::new())),
            store,
            repair_manager: Arc::new(RepairManager::new(provider)),
            config: Arc::new(config),
            jellyfin_client: None,
            http_client: reqwest::Client::new(),
        };
        let _config = ScanConfig { app };
    }
```

- [ ] **Step 18: Build and run the full suite**

Run: `cargo build && cargo test`
Expected: PASS. The scan loop now persists through `Store`; behaviour is identical.

- [ ] **Step 19: Manual smoke — DB recovery on a real run (optional but recommended)**

Run (no tokens needed to reach the DB step; it will exit at provider/TMDB config, but the DB opens first only if config passes — so use a throwaway env):

```bash
printf 'garbage' > /tmp/dmm-smoke.db
RD_API_TOKEN=x TMDB_API_KEY=y DB_PATH=/tmp/dmm-smoke.db timeout 3 cargo run 2>&1 | grep -i "moved aside" || echo "check logs"
ls -la /tmp/dmm-smoke.db /tmp/dmm-smoke.db.corrupt
```
Expected: a "Moved aside unusable database" log line and a `/tmp/dmm-smoke.db.corrupt` file; the process does not crash on the DB. Clean up: `rm -f /tmp/dmm-smoke.db /tmp/dmm-smoke.db.corrupt`.

- [ ] **Step 20: Commit**

```bash
git add src/app_state.rs src/mapper.rs src/main.rs src/tasks.rs
git commit -m "refactor: route persistence through Store and consolidate handles into AppState" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Documentation + final gate

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md`

- [ ] **Step 1: Update `CLAUDE.md` module table**

In the module-responsibilities table in `CLAUDE.md`, add three rows (alongside the existing entries):

```markdown
| `config.rs` | `Config` — all startup env parsing/validation (`from_env`/`from_parts`); shaped for a future DB-override layer |
| `store.rs` | `Store` — owns all redb table definitions, schema version + migration hook, `open()` with auto-recovery (moves an unreadable/incompatible DB aside and recreates it), and typed async accessors for the `matches` cache |
| `app_state.rs` | `AppState` — `Clone` bundle of shared handles (provider, tmdb, vfs, store, repair, config, jellyfin, http) carried by the scan task |
```

- [ ] **Step 2: Document DB auto-recovery in `CLAUDE.md`**

Under **Key Design Decisions** in `CLAUDE.md`, add:

```markdown
- **Database is self-healing:** `Store::open` stamps a schema version and runs forward migrations. An unreadable / incompatible / corrupt / newer-than-binary `metadata.db` (e.g. after a `redb` format change) is **moved aside** to `<db_path>.corrupt` and recreated rather than crashing the service. The `matches` table is a regenerable cache, so this is lossless in practice; authoritative tables added in later phases are migrated, never silently dropped.
```

- [ ] **Step 3: Note the persistence indirection in `CLAUDE.md`**

In the **Persistence** paragraph of `CLAUDE.md`, append:

```markdown
All `redb` access goes through `store.rs` (the `Store` type); modules never open transactions inline.
```

- [ ] **Step 4: Update `README.md` if it documents env vars or architecture**

Check `README.md` for an environment-variable list or architecture section. If present, confirm the variables are unchanged (`RD_API_TOKEN`/`TORBOX_API_KEY`, `TMDB_API_KEY`, `SCAN_INTERVAL_SECS`, `DB_PATH`, `PORT`, the three `JELLYFIN_*`) and add a one-line note that `metadata.db` auto-recovers from incompatible/corrupt files (moving the old file to `<DB_PATH>.corrupt`). No new variables are introduced in SP0.

- [ ] **Step 5: Run the complete pre-commit gate**

Run:

```bash
cargo test \
  && INTEGRATION_TEST_LIMIT=10 cargo test --test integration_test -- --ignored \
  && INTEGRATION_TEST_LIMIT=10 cargo test --test repair_integration_test -- --ignored \
  && cargo test --test lifecycle_test -- --ignored
```

Expected: PASS. (The `--ignored` suites need `RD_API_TOKEN`/`TORBOX_API_KEY` + `TMDB_API_KEY` in `.env`; each provider sub-test skips when its token is unset.)

- [ ] **Step 6: Commit**

```bash
git add CLAUDE.md README.md
git commit -m "docs: document Store, Config, AppState and DB auto-recovery" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-review

**Spec coverage (§5 SP0):**
- §5.1 `store.rs` repository layer + migrate `matches` + byte-identical encoding → Tasks 2 & 3. ✓
- Decision #17 / §5.1 DB never-fail recovery + versioning + migration hook → Task 2 (`Store::open`, `read_version`, `ensure_schema`, `run_migrations`, `move_aside_and_create`) + `store_recovery_test`. ✓
- §5.2 `AppState` consolidating shared handles, built in `main.rs`, consumed by `ScanConfig` → Task 3. ✓ (`DebridFileSystem::new` left as-is by design — dav_fs has no Store/Config concern; noted.)
- §5.3 layered `Config` from env, same vars/defaults/validation → Task 1. ✓
- §5.5 existing suite passes unchanged + `Store` round-trip tests + `Config` parsing tests → Tasks 1–3 + Task 0 baseline + Task 3 Step 18/Task 4 Step 5 gate. ✓
- §8.2 `store_integration_test` (round-trip + old-encoding load) and `store_recovery_test` and `config_test` → Tasks 1 & 2. ✓ (Implemented as in-crate `#[cfg(test)]` modules, the project's existing convention for unit tests.)
- §5.4 non-goals (scheduler/repair-generalisation/episode-model/vfs-inversion/web-router) → none attempted. ✓

**Placeholder scan:** No TBD/TODO; every code step shows complete code; every test shows assertions. ✓

**Type consistency:** `Store` methods (`open`, `from_database`, `load_all_matches`, `get_match`, `put_matches`, `remove_matches`, `replace_match`, `flatten_join`) are used with matching signatures in Task 3 and the tests. `AppState` field names (`provider`, `tmdb_client`, `vfs`, `store`, `repair_manager`, `config`, `jellyfin_client`, `http_client`) match between definition (Task 3 Step 1), construction (`main.rs` Step 15, test Step 17), and destructure (`run_scan_loop` Step 5). `Config` fields (`provider_kind`, `provider_token`, `tmdb_api_key`, `scan_interval_secs`, `db_path`, `port`) match between definition (Task 1 Step 3) and use (`main.rs`, `AppState`). ✓

**Notes for the implementer:**
- The one version-sensitive line is the `TableDoesNotExist` match arm in `read_version` — adjust to this redb version's variant if it differs.
- `run_scan_loop`'s body still references the locals `rd_client`, `tmdb_client`, `vfs`, `repair_manager`, `jellyfin_client`, `interval_secs`, `store` — the Step-5 destructure binds all of them, so the loop body needs no further edits beyond Steps 6–11.
