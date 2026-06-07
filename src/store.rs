use crate::error::AppError;
use crate::rd_client::TorrentInfo;
use crate::vfs::MediaMetadata;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Current on-disk schema version. Bump when a migration is added in `run_migrations`.
pub const SCHEMA_VERSION: u64 = 2;

/// TMDB identification cache: torrent id -> serde_json((TorrentInfo, MediaMetadata)).
/// Same name + value encoding as the pre-Store inline table, so existing databases
/// load unchanged.
const MATCHES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");
/// Internal metadata (schema version, etc.).
const META_TABLE: TableDefinition<&str, u64> = TableDefinition::new("meta");
const SCHEMA_VERSION_KEY: &str = "schema_version";
/// Per-hash acquisition spec + status: infohash -> serde_json(OwnedRecord).
const OWNED_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
/// Authoritative identification override: infohash -> serde_json(MediaMetadata).
const AUTH_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("authoritative_ids");
/// Blacklisted (tmdb_id, hash) pairs: "tmdbid|hash" -> serde_json({reason, at}).
const BLACKLIST_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("blacklist");

/// The persisted "what to acquire" spec (also used by `acquire.rs`). Stored in `owned_hashes`
/// so `observe` can re-acquire a title after a stall/failure without external context.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AcquireRequest {
    pub imdb_id: String,
    pub tmdb_id: u64,
    pub kind: crate::scraper::MediaKind,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub original_language: Option<String>,
    pub metadata: crate::vfs::MediaMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OwnedStatus {
    Pending,
    Verified,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OwnedRecord {
    pub request: AcquireRequest,
    pub source: String,
    pub added_at: u64,
    pub status: OwnedStatus,
}

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
            write_txn.open_table(OWNED_TABLE)?; // create if absent
            write_txn.open_table(AUTH_TABLE)?; // create if absent
            write_txn.open_table(BLACKLIST_TABLE)?; // create if absent
            let mut meta = write_txn.open_table(META_TABLE)?; // create if absent
            meta.insert(SCHEMA_VERSION_KEY, &SCHEMA_VERSION)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Apply forward migrations from `from_version` up to `SCHEMA_VERSION`. The v1→v2 step is a
    /// no-op: the new tables (owned_hashes/authoritative_ids/blacklist) are additive and created
    /// lazily by `ensure_schema`. Future non-additive migrations add steps here, keyed on
    /// `from_version`, before the version stamp is written.
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
        // Serialise BEFORE opening the transaction: a serialisation failure must not
        // leave the old entry removed with no replacement (partial-write data loss).
        // This mirrors the pre-Store behaviour, where a to_vec failure skipped the
        // whole remove+insert.
        let bytes = match serde_json::to_vec(&(&info, &metadata)) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to serialise replacement match {}: {}", new_id, e);
                return Ok(());
            }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(MATCHES_TABLE)?;
                table.remove(old_id.as_str())?;
                table.insert(new_id.as_str(), bytes.as_slice())?;
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

    // ── owned_hashes accessors ────────────────────────────────────────────────

    pub async fn put_owned(&self, hash: String, rec: OwnedRecord) -> Result<(), AppError> {
        // Serialise before opening the transaction to avoid partial writes on
        // serde failure (mirrors `replace_match`).
        let bytes = match serde_json::to_vec(&rec) {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to serialise OwnedRecord for {}: {}", hash, e);
                return Ok(());
            }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(OWNED_TABLE)?.insert(hash.as_str(), bytes.as_slice())?; }
            txn.commit()?;
            Ok(())
        }).await;
        Self::flatten_join(result)
    }

    pub async fn get_owned(&self, hash: String) -> Option<OwnedRecord> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(OWNED_TABLE).ok()?;
            let e = table.get(hash.as_str()).ok()??;
            serde_json::from_slice::<OwnedRecord>(e.value()).ok()
        }).await.ok().flatten()
    }

    pub async fn set_owned_status(&self, hash: String, status: OwnedStatus) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(OWNED_TABLE)?;
                // Read the raw bytes into an owned Vec first so the read borrow ends
                // before we call insert (which needs &mut table).
                let existing: Option<Vec<u8>> = table
                    .get(hash.as_str())?
                    .map(|guard| guard.value().to_vec());
                if let Some(raw) = existing {
                    if let Ok(mut rec) = serde_json::from_slice::<OwnedRecord>(&raw) {
                        rec.status = status;
                        if let Ok(bytes) = serde_json::to_vec(&rec) {
                            table.insert(hash.as_str(), bytes.as_slice())?;
                        }
                    }
                }
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn remove_owned(&self, hash: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(OWNED_TABLE)?.remove(hash.as_str())?; }
            txn.commit()?;
            Ok(())
        }).await;
        Self::flatten_join(result)
    }

    pub async fn all_owned(&self) -> Vec<(String, OwnedRecord)> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            if let Ok(txn) = db.begin_read() {
                if let Ok(table) = txn.open_table(OWNED_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (k, v) = entry;
                            if let Ok(rec) = serde_json::from_slice::<OwnedRecord>(v.value()) {
                                out.push((k.value().to_string(), rec));
                            }
                        }
                    }
                }
            }
            out
        }).await.unwrap_or_default()
    }

    // ── authoritative_ids accessors ───────────────────────────────────────────

    pub async fn put_authoritative(&self, hash: String, meta: crate::vfs::MediaMetadata) -> Result<(), AppError> {
        let bytes = match serde_json::to_vec(&meta) {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to serialise authoritative metadata for {}: {}", hash, e);
                return Ok(());
            }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(AUTH_TABLE)?.insert(hash.as_str(), bytes.as_slice())?; }
            txn.commit()?;
            Ok(())
        }).await;
        Self::flatten_join(result)
    }

    pub async fn authoritative_meta(&self, hash: String) -> Option<crate::vfs::MediaMetadata> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(AUTH_TABLE).ok()?;
            let e = table.get(hash.as_str()).ok()??;
            serde_json::from_slice::<crate::vfs::MediaMetadata>(e.value()).ok()
        }).await.ok().flatten()
    }

    pub async fn remove_authoritative(&self, hash: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(AUTH_TABLE)?.remove(hash.as_str())?; }
            txn.commit()?;
            Ok(())
        }).await;
        Self::flatten_join(result)
    }

    // ── blacklist accessors ───────────────────────────────────────────────────

    pub async fn blacklist_add(&self, tmdb_id: u64, hash: String, reason: &str, at: u64) -> Result<(), AppError> {
        let key = format!("{}|{}", tmdb_id, hash);
        let bytes = match serde_json::to_vec(&serde_json::json!({"reason": reason, "at": at})) {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to serialise blacklist entry {}|{}: {}", tmdb_id, hash, e);
                return Ok(());
            }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(BLACKLIST_TABLE)?.insert(key.as_str(), bytes.as_slice())?; }
            txn.commit()?;
            Ok(())
        }).await;
        Self::flatten_join(result)
    }

    pub async fn is_blacklisted(&self, tmdb_id: u64, hash: String) -> bool {
        let key = format!("{}|{}", tmdb_id, hash);
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = match db.begin_read() { Ok(t) => t, Err(_) => return false };
            let table = match txn.open_table(BLACKLIST_TABLE) { Ok(t) => t, Err(_) => return false };
            matches!(table.get(key.as_str()), Ok(Some(_)))
        }).await.unwrap_or(false)
    }
}

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
    async fn replace_with_missing_old_id_still_inserts_new() {
        // Removing a non-existent key is a no-op in redb; the insert must still happen.
        let store = mem_store();
        store
            .replace_match("ghost".to_string(), "fresh".to_string(), info("fresh"), movie("Fresh"))
            .await
            .unwrap();
        assert_eq!(
            store.get_match("fresh".to_string()).await.unwrap().0.id,
            "fresh"
        );
    }

    #[tokio::test]
    async fn loads_db_written_in_old_inline_encoding() {
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
        }
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

    // ── SP1 Task 6 tests ──────────────────────────────────────────────────────

    use crate::scraper::MediaKind;

    fn req(imdb: &str, tmdb: u64) -> AcquireRequest {
        AcquireRequest {
            imdb_id: imdb.to_string(),
            tmdb_id: tmdb,
            kind: MediaKind::Movie,
            season: None,
            episode: None,
            original_language: Some("eng".to_string()),
            metadata: movie("Title"),
        }
    }

    #[tokio::test]
    async fn owned_round_trip_and_status_update() {
        let store = mem_store();
        let rec = OwnedRecord {
            request: req("tt1", 27205),
            source: "manual".to_string(),
            added_at: 100,
            status: OwnedStatus::Pending,
        };
        store.put_owned("h1".to_string(), rec).await.unwrap();
        assert_eq!(store.get_owned("h1".to_string()).await.unwrap().status, OwnedStatus::Pending);
        store.set_owned_status("h1".to_string(), OwnedStatus::Verified).await.unwrap();
        assert_eq!(store.get_owned("h1".to_string()).await.unwrap().status, OwnedStatus::Verified);
        assert_eq!(store.all_owned().await.len(), 1);
        store.remove_owned("h1".to_string()).await.unwrap();
        assert!(store.get_owned("h1".to_string()).await.is_none());
    }

    #[tokio::test]
    async fn authoritative_round_trip() {
        let store = mem_store();
        store.put_authoritative("h1".to_string(), movie("Auth")).await.unwrap();
        assert_eq!(store.authoritative_meta("h1".to_string()).await.unwrap().title, "Auth");
        store.remove_authoritative("h1".to_string()).await.unwrap();
        assert!(store.authoritative_meta("h1".to_string()).await.is_none());
    }

    #[tokio::test]
    async fn blacklist_add_and_check() {
        let store = mem_store();
        assert!(!store.is_blacklisted(27205, "h1".to_string()).await);
        store.blacklist_add(27205, "h1".to_string(), "WrongTitle", 100).await.unwrap();
        assert!(store.is_blacklisted(27205, "h1".to_string()).await);
        assert!(!store.is_blacklisted(27205, "h2".to_string()).await);
        assert!(!store.is_blacklisted(99999, "h1".to_string()).await);
    }

    #[tokio::test]
    async fn migrates_v1_db_to_v2_preserving_matches() {
        let tmp = TempDb::new("migrate");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mdef: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");
                let mut t = txn.open_table(mdef).unwrap();
                let i = info("m1");
                let m = movie("Kept");
                t.insert("m1", serde_json::to_vec(&(&i, &m)).unwrap().as_slice()).unwrap();
                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &1u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();
        assert_eq!(store.get_match("m1".to_string()).await.unwrap().1.title, "Kept");
        store.put_authoritative("h".to_string(), movie("New")).await.unwrap();
        assert_eq!(store.authoritative_meta("h".to_string()).await.unwrap().title, "New");
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "valid v1 DB must not be moved aside"
        );
    }
}
