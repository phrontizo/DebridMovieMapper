use crate::error::AppError;
use crate::rd_client::TorrentInfo;
use crate::vfs::MediaMetadata;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Current on-disk schema version. Bump when a migration is added in `run_migrations`.
/// v1→v2: additive (owned_hashes, authoritative_ids, blacklist tables).
/// v2→v3: additive (trakt_tokens, wanted tables).
/// v3→v4: additive (selection, upgrade_checks tables; OwnedRecord.provides/quality fields).
pub const SCHEMA_VERSION: u64 = 4;

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
/// Per-user Trakt OAuth tokens: user_slug -> serde_json(TraktTokens).
const TRAKT_TOKENS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("trakt_tokens");
/// Materialised per-(user, tmdb_id) wanted-set: "user|tmdb_id" -> serde_json(WantedRecord).
const WANTED_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("wanted");
/// SP3 live-selection: slot ("m|tmdb" / "e|tmdb|s|e") -> serde_json(SelectionEntry).
const SELECTION_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("selection");
/// SP3 upgrade round-robin cursor: tmdb_id (as string) -> last-checked unix secs.
const UPGRADE_CHECKS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("upgrade_checks");

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

/// The live representative for one VFS slot (movie or episode): which hash + which file path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SelectionEntry {
    pub hash: String,
    pub file_path: String,
}

/// Slot key for a movie's selection entry.
pub fn movie_slot(tmdb_id: u64) -> String {
    format!("m|{}", tmdb_id)
}
/// Slot key for one episode's selection entry.
pub fn episode_slot(tmdb_id: u64, season: u32, episode: u32) -> String {
    format!("e|{}|{}|{}", tmdb_id, season, episode)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OwnedStatus {
    Pending,
    Verified,
}

/// One reason a title is engine-owned: a manual (non-Trakt) origin, or a specific
/// Trakt user via a specific source.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProvenanceEntry {
    /// Added outside Trakt (legacy / direct). Never auto-removed by the reconciler.
    Manual,
    /// A user's Trakt watchlist caused the add.
    Watchlist { user: String },
    /// A user's Trakt in-progress (playback) caused the add.
    InProgress { user: String },
}

/// Why a title is engine-owned — the de-duplicated set of (user, source) reasons.
/// Multiple users / sources can keep one shared library title alive.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Provenance {
    pub entries: Vec<ProvenanceEntry>,
}

impl Default for Provenance {
    /// An owned record with no recorded provenance (e.g. decoded from the pre-provenance
    /// schema) is treated as a manual add — the safe default the reconciler never removes.
    fn default() -> Self {
        Provenance::manual()
    }
}

impl Provenance {
    pub fn manual() -> Self {
        Provenance {
            entries: vec![ProvenanceEntry::Manual],
        }
    }
    pub fn watchlist(user: impl Into<String>) -> Self {
        Provenance {
            entries: vec![ProvenanceEntry::Watchlist { user: user.into() }],
        }
    }
    pub fn in_progress(user: impl Into<String>) -> Self {
        Provenance {
            entries: vec![ProvenanceEntry::InProgress { user: user.into() }],
        }
    }
    /// Union `other`'s entries into `self`, de-duplicating identical (variant, user) entries.
    // O(n²) dedup, but n is small in practice (Manual + at most one entry per user per source).
    pub fn merge(&mut self, other: &Provenance) {
        for e in &other.entries {
            if !self.entries.contains(e) {
                self.entries.push(e.clone());
            }
        }
    }
    /// True if at least one entry is `Manual`. A title with any manual origin must never be
    /// auto-removed by the reconciler, regardless of Trakt state. (Provenance built via the
    /// constructors is always non-empty; `entries` is `pub` only for multi-entry construction.)
    pub fn has_manual_entry(&self) -> bool {
        self.entries
            .iter()
            .any(|e| matches!(e, ProvenanceEntry::Manual))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OwnedRecord {
    pub request: AcquireRequest,
    #[serde(default)]
    pub provenance: Provenance,
    pub added_at: u64,
    pub status: OwnedStatus,
    /// (season, episode) pairs this hash supplies. Single-episode ⇒ one pair; movie ⇒ empty;
    /// season pack ⇒ the full set (computed in `observe` from the resolved files). The union
    /// across a show's hashes is the show's owned-episode set (kills season-pack churn).
    #[serde(default)]
    pub provides: Vec<(u32, u32)>,
    /// Quality snapshot of the chosen release, for upgrade comparison. `None` on pre-SP3 records.
    #[serde(default)]
    pub quality: Option<crate::release::QualitySummary>,
}

/// Persisted per-user Trakt OAuth tokens (the `trakt_tokens` table value; key = user slug).
/// `needs_reenrolment` is set by the `sync_trakt` job when a token refresh or read fails (the
/// account likely needs re-authorising); it is cleared on the next successful sync. Old records
/// written before this field existed decode it as `false`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TraktTokens {
    pub access: String,
    pub refresh: String,
    pub expires_at: u64, // unix epoch seconds
    pub username: String,
    #[serde(default)]
    pub needs_reenrolment: bool,
}

/// Which Trakt sources put a title in a user's wanted-set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WantedSources {
    pub watchlist: bool,
    pub in_progress: bool,
}

/// Per-user watched progress snapshot; the lifecycle reconciler uses this to determine
/// when a title is fully watched and eligible for removal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WatchedState {
    Movie { watched: bool },
    Show { watched_episodes: Vec<(u32, u32)> }, // (season, episode) pairs the user has watched
}

/// One (user, tmdb_id) row of the materialised wanted-set. Self-describing: it embeds its
/// own `user`+`tmdb_id` (the composite table key is derived from them) so the reconciler
/// gets a flat, fully-keyed list from `all_wanted()` without re-parsing keys.
/// Invariant: `media_type` and the `WatchedState` variant (Movie/Show) must agree —
/// `media_type` drives acquire-engine routing, `WatchedState` drives lifecycle logic.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WantedRecord {
    pub user: String,
    pub tmdb_id: u64,
    pub media_type: crate::vfs::MediaType,
    pub sources: WantedSources,
    pub watched_state: WatchedState,
    pub show_status: Option<crate::tmdb_client::ShowStatus>, // None for movies
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
        Ok(table
            .get(SCHEMA_VERSION_KEY)?
            .map(|g| g.value())
            .unwrap_or(0))
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
            write_txn.open_table(TRAKT_TOKENS_TABLE)?; // create if absent
            write_txn.open_table(WANTED_TABLE)?; // create if absent
            write_txn.open_table(SELECTION_TABLE)?; // create if absent
            write_txn.open_table(UPGRADE_CHECKS_TABLE)?; // create if absent
            let mut meta = write_txn.open_table(META_TABLE)?; // create if absent
            meta.insert(SCHEMA_VERSION_KEY, &SCHEMA_VERSION)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Apply forward migrations from `from_version` up to `SCHEMA_VERSION`. The v1→v2 and v2→v3
    /// steps are no-ops: the new tables (owned_hashes/authoritative_ids/blacklist for v2;
    /// trakt_tokens/wanted for v3) are additive and created lazily by `ensure_schema`. Future
    /// non-additive migrations add steps here, keyed on `from_version`, before the version stamp
    /// is written.
    /// v3→v4: additive (selection, upgrade_checks tables; OwnedRecord.provides/quality serde defaults).
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
            {
                txn.open_table(OWNED_TABLE)?
                    .insert(hash.as_str(), bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn get_owned(&self, hash: String) -> Option<OwnedRecord> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(OWNED_TABLE).ok()?;
            let e = table.get(hash.as_str()).ok()??;
            serde_json::from_slice::<OwnedRecord>(e.value()).ok()
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn set_owned_status(
        &self,
        hash: String,
        status: OwnedStatus,
    ) -> Result<(), AppError> {
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
            {
                txn.open_table(OWNED_TABLE)?.remove(hash.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
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
        })
        .await
        .unwrap_or_default()
    }

    // ── authoritative_ids accessors ───────────────────────────────────────────

    pub async fn put_authoritative(
        &self,
        hash: String,
        meta: crate::vfs::MediaMetadata,
    ) -> Result<(), AppError> {
        let bytes = match serde_json::to_vec(&meta) {
            Ok(b) => b,
            Err(e) => {
                error!(
                    "Failed to serialise authoritative metadata for {}: {}",
                    hash, e
                );
                return Ok(());
            }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(AUTH_TABLE)?
                    .insert(hash.as_str(), bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn authoritative_meta(&self, hash: String) -> Option<crate::vfs::MediaMetadata> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(AUTH_TABLE).ok()?;
            let e = table.get(hash.as_str()).ok()??;
            serde_json::from_slice::<crate::vfs::MediaMetadata>(e.value()).ok()
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn remove_authoritative(&self, hash: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(AUTH_TABLE)?.remove(hash.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    // ── blacklist accessors ───────────────────────────────────────────────────

    pub async fn blacklist_add(
        &self,
        tmdb_id: u64,
        hash: String,
        reason: &str,
        at: u64,
    ) -> Result<(), AppError> {
        let key = format!("{}|{}", tmdb_id, hash);
        let bytes = match serde_json::to_vec(&serde_json::json!({"reason": reason, "at": at})) {
            Ok(b) => b,
            Err(e) => {
                error!(
                    "Failed to serialise blacklist entry {}|{}: {}",
                    tmdb_id, hash, e
                );
                return Ok(());
            }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(BLACKLIST_TABLE)?
                    .insert(key.as_str(), bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn is_blacklisted(&self, tmdb_id: u64, hash: String) -> bool {
        let key = format!("{}|{}", tmdb_id, hash);
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = match db.begin_read() {
                Ok(t) => t,
                Err(_) => return false,
            };
            let table = match txn.open_table(BLACKLIST_TABLE) {
                Ok(t) => t,
                Err(_) => return false,
            };
            matches!(table.get(key.as_str()), Ok(Some(_)))
        })
        .await
        .unwrap_or(false)
    }

    // ── trakt_tokens accessors ────────────────────────────────────────────────

    /// `slug` is the user's Trakt URL slug and is the key under which the tokens are stored.
    pub async fn put_trakt_tokens(
        &self,
        slug: String,
        tokens: TraktTokens,
    ) -> Result<(), AppError> {
        // Serialise before opening the transaction to avoid partial writes on serde failure.
        let bytes = match serde_json::to_vec(&tokens) {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to serialise TraktTokens for {}: {}", slug, e);
                return Ok(());
            }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(TRAKT_TOKENS_TABLE)?
                    .insert(slug.as_str(), bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn get_trakt_tokens(&self, slug: String) -> Option<TraktTokens> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(TRAKT_TOKENS_TABLE).ok()?;
            let e = table.get(slug.as_str()).ok()??;
            serde_json::from_slice::<TraktTokens>(e.value()).ok()
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn remove_trakt_tokens(&self, slug: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(TRAKT_TOKENS_TABLE)?.remove(slug.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    /// Returns all stored Trakt token entries as (slug, tokens) pairs.
    pub async fn all_trakt_tokens(&self) -> Vec<(String, TraktTokens)> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            if let Ok(txn) = db.begin_read() {
                if let Ok(table) = txn.open_table(TRAKT_TOKENS_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (k, v) = entry;
                            if let Ok(tokens) = serde_json::from_slice::<TraktTokens>(v.value()) {
                                out.push((k.value().to_string(), tokens));
                            }
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default()
    }

    // ── wanted accessors ──────────────────────────────────────────────────────

    pub async fn put_wanted(&self, rec: WantedRecord) -> Result<(), AppError> {
        // Serialise before opening the transaction to avoid partial writes on serde failure.
        let bytes = match serde_json::to_vec(&rec) {
            Ok(b) => b,
            Err(e) => {
                error!(
                    "Failed to serialise WantedRecord for {}|{}: {}",
                    rec.user, rec.tmdb_id, e
                );
                return Ok(());
            }
        };
        let key = format!("{}|{}", rec.user, rec.tmdb_id);
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(WANTED_TABLE)?
                    .insert(key.as_str(), bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn get_wanted(&self, user: String, tmdb_id: u64) -> Option<WantedRecord> {
        let key = format!("{}|{}", user, tmdb_id);
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(WANTED_TABLE).ok()?;
            let e = table.get(key.as_str()).ok()??;
            serde_json::from_slice::<WantedRecord>(e.value()).ok()
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn remove_wanted(&self, user: String, tmdb_id: u64) -> Result<(), AppError> {
        let key = format!("{}|{}", user, tmdb_id);
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(WANTED_TABLE)?.remove(key.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    /// Returns all wanted-set records across all users. Each record is self-keyed (embeds
    /// `user` and `tmdb_id`), so the reconciler can work with a flat list without re-parsing keys.
    pub async fn all_wanted(&self) -> Vec<WantedRecord> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            if let Ok(txn) = db.begin_read() {
                if let Ok(table) = txn.open_table(WANTED_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (_, v) = entry;
                            if let Ok(rec) = serde_json::from_slice::<WantedRecord>(v.value()) {
                                out.push(rec);
                            }
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default()
    }

    // ── selection accessors (SP3) ─────────────────────────────────────────────

    pub async fn put_selection(&self, slot: String, entry: SelectionEntry) -> Result<(), AppError> {
        let bytes = match serde_json::to_vec(&entry) {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to serialise SelectionEntry for {}: {}", slot, e);
                return Ok(());
            }
        };
        // Serialise before opening the transaction to avoid partial writes on serde failure.
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(SELECTION_TABLE)?
                    .insert(slot.as_str(), bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn get_selection(&self, slot: String) -> Option<SelectionEntry> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(SELECTION_TABLE).ok()?;
            let e = table.get(slot.as_str()).ok()??;
            serde_json::from_slice::<SelectionEntry>(e.value()).ok()
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn remove_selection(&self, slot: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(SELECTION_TABLE)?.remove(slot.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn all_selection(&self) -> Vec<(String, SelectionEntry)> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            if let Ok(txn) = db.begin_read() {
                if let Ok(table) = txn.open_table(SELECTION_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (k, v) = entry;
                            if let Ok(rec) = serde_json::from_slice::<SelectionEntry>(v.value()) {
                                out.push((k.value().to_string(), rec));
                            }
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default()
    }

    // ── upgrade_checks cursor (SP3) ───────────────────────────────────────────

    /// Returns the unix-second timestamp of the last upgrade check for `tmdb_id`, or 0 if never checked.
    pub async fn get_upgrade_checked(&self, tmdb_id: u64) -> u64 {
        let key = tmdb_id.to_string();
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = match db.begin_read() {
                Ok(t) => t,
                Err(_) => return 0,
            };
            let table = match txn.open_table(UPGRADE_CHECKS_TABLE) {
                Ok(t) => t,
                Err(_) => return 0,
            };
            table
                .get(key.as_str())
                .ok()
                .flatten()
                .map(|g| g.value())
                .unwrap_or(0)
        })
        .await
        .unwrap_or(0)
    }

    pub async fn set_upgrade_checked(&self, tmdb_id: u64, at: u64) -> Result<(), AppError> {
        let key = tmdb_id.to_string();
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(UPGRADE_CHECKS_TABLE)?
                    .insert(key.as_str(), &at)?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
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
            .replace_match(
                "old".to_string(),
                "new".to_string(),
                info("new"),
                movie("Title"),
            )
            .await
            .unwrap();
        assert!(store.get_match("old".to_string()).await.is_none());
        assert_eq!(
            store.get_match("new".to_string()).await.unwrap().0.id,
            "new"
        );
    }

    #[tokio::test]
    async fn replace_with_missing_old_id_still_inserts_new() {
        // Removing a non-existent key is a no-op in redb; the insert must still happen.
        let store = mem_store();
        store
            .replace_match(
                "ghost".to_string(),
                "fresh".to_string(),
                info("fresh"),
                movie("Fresh"),
            )
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
        let got = store
            .get_match("leg".to_string())
            .await
            .expect("legacy row");
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
            p.push(format!(
                "dmm_store_{}_{}_{}.redb",
                tag,
                std::process::id(),
                n
            ));
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
            provenance: Provenance::manual(),
            added_at: 100,
            status: OwnedStatus::Pending,
            provides: vec![],
            quality: None,
        };
        store.put_owned("h1".to_string(), rec).await.unwrap();
        assert_eq!(
            store.get_owned("h1".to_string()).await.unwrap().status,
            OwnedStatus::Pending
        );
        store
            .set_owned_status("h1".to_string(), OwnedStatus::Verified)
            .await
            .unwrap();
        assert_eq!(
            store.get_owned("h1".to_string()).await.unwrap().status,
            OwnedStatus::Verified
        );
        assert_eq!(store.all_owned().await.len(), 1);
        store.remove_owned("h1".to_string()).await.unwrap();
        assert!(store.get_owned("h1".to_string()).await.is_none());
    }

    #[tokio::test]
    async fn authoritative_round_trip() {
        let store = mem_store();
        store
            .put_authoritative("h1".to_string(), movie("Auth"))
            .await
            .unwrap();
        assert_eq!(
            store
                .authoritative_meta("h1".to_string())
                .await
                .unwrap()
                .title,
            "Auth"
        );
        store.remove_authoritative("h1".to_string()).await.unwrap();
        assert!(store.authoritative_meta("h1".to_string()).await.is_none());
    }

    #[tokio::test]
    async fn blacklist_add_and_check() {
        let store = mem_store();
        assert!(!store.is_blacklisted(27205, "h1".to_string()).await);
        store
            .blacklist_add(27205, "h1".to_string(), "WrongTitle", 100)
            .await
            .unwrap();
        assert!(store.is_blacklisted(27205, "h1".to_string()).await);
        assert!(!store.is_blacklisted(27205, "h2".to_string()).await);
        assert!(!store.is_blacklisted(99999, "h1".to_string()).await);
    }

    // ── SP2 Task 3 tests (trakt_tokens + wanted) ─────────────────────────────

    use crate::tmdb_client::ShowStatus;

    fn trakt_tokens_fixture(access: &str, username: &str) -> TraktTokens {
        TraktTokens {
            access: access.to_string(),
            refresh: "refresh_tok".to_string(),
            expires_at: 9_999_999_999,
            username: username.to_string(),
            needs_reenrolment: false,
        }
    }

    fn movie_wanted(user: &str, tmdb_id: u64) -> WantedRecord {
        WantedRecord {
            user: user.to_string(),
            tmdb_id,
            media_type: MediaType::Movie,
            sources: WantedSources {
                watchlist: true,
                in_progress: false,
            },
            watched_state: WatchedState::Movie { watched: false },
            show_status: None,
        }
    }

    fn show_wanted(user: &str, tmdb_id: u64) -> WantedRecord {
        WantedRecord {
            user: user.to_string(),
            tmdb_id,
            media_type: MediaType::Show,
            sources: WantedSources {
                watchlist: false,
                in_progress: true,
            },
            watched_state: WatchedState::Show {
                watched_episodes: vec![(1, 1), (1, 2), (2, 1)],
            },
            show_status: Some(ShowStatus::Ended),
        }
    }

    #[tokio::test]
    async fn trakt_tokens_round_trip() {
        let store = mem_store();
        let tok1 = trakt_tokens_fixture("access1", "alice");
        let tok2 = TraktTokens {
            access: "access2".to_string(),
            refresh: "ref2".to_string(),
            expires_at: 1_234_567_890,
            username: "bob".to_string(),
            needs_reenrolment: true,
        };
        store
            .put_trakt_tokens("alice".to_string(), tok1.clone())
            .await
            .unwrap();
        store
            .put_trakt_tokens("bob".to_string(), tok2.clone())
            .await
            .unwrap();

        let got1 = store
            .get_trakt_tokens("alice".to_string())
            .await
            .expect("alice present");
        assert_eq!(got1, tok1);

        let got2 = store
            .get_trakt_tokens("bob".to_string())
            .await
            .expect("bob present");
        assert_eq!(got2, tok2);

        let all = store.all_trakt_tokens().await;
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|(slug, _)| slug == "alice"));
        assert!(all.iter().any(|(slug, _)| slug == "bob"));

        store
            .remove_trakt_tokens("alice".to_string())
            .await
            .unwrap();
        assert!(store.get_trakt_tokens("alice".to_string()).await.is_none());
        assert_eq!(store.all_trakt_tokens().await.len(), 1);
    }

    /// Old-shape token JSON (written before `needs_reenrolment` existed) must decode with the
    /// flag defaulting to `false` — backward-compatible, mirroring the OwnedRecord provenance test.
    #[test]
    fn trakt_tokens_old_encoding_defaults_needs_reenrolment_false() {
        let old = serde_json::json!({
            "access": "a",
            "refresh": "r",
            "expires_at": 1,
            "username": "u",
        });
        let decoded: TraktTokens = serde_json::from_value(old).unwrap();
        assert!(!decoded.needs_reenrolment);
    }

    #[tokio::test]
    async fn wanted_round_trip() {
        let store = mem_store();
        let movie_rec = movie_wanted("alice", 27205);
        let show_rec = show_wanted("bob", 1396);

        store.put_wanted(movie_rec.clone()).await.unwrap();
        store.put_wanted(show_rec.clone()).await.unwrap();

        let got_movie = store
            .get_wanted("alice".to_string(), 27205)
            .await
            .expect("movie present");
        assert_eq!(got_movie, movie_rec);

        // Upsert: writing a modified record at the same (user, tmdb_id) must overwrite in place.
        let updated_movie = WantedRecord {
            sources: WantedSources {
                watchlist: false,
                in_progress: true,
            },
            ..movie_rec.clone()
        };
        store.put_wanted(updated_movie.clone()).await.unwrap();
        let got_updated = store
            .get_wanted("alice".to_string(), 27205)
            .await
            .expect("updated present");
        assert_eq!(got_updated, updated_movie);

        let got_show = store
            .get_wanted("bob".to_string(), 1396)
            .await
            .expect("show present");
        assert_eq!(got_show, show_rec);
        // Verify deep equality on watched_episodes
        assert_eq!(
            got_show.watched_state,
            WatchedState::Show {
                watched_episodes: vec![(1, 1), (1, 2), (2, 1)]
            }
        );
        assert_eq!(got_show.show_status, Some(ShowStatus::Ended));

        let all = store.all_wanted().await;
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|r| r.user == "alice" && r.tmdb_id == 27205));
        assert!(all.iter().any(|r| r.user == "bob" && r.tmdb_id == 1396));

        store
            .remove_wanted("alice".to_string(), 27205)
            .await
            .unwrap();
        assert!(store.get_wanted("alice".to_string(), 27205).await.is_none());
        assert_eq!(store.all_wanted().await.len(), 1);
    }

    #[tokio::test]
    async fn all_wanted_aggregates_multi_user_same_tmdb_id() {
        let store = mem_store();
        let rec_a = movie_wanted("userA", 123);
        let rec_b = WantedRecord {
            user: "userB".to_string(),
            ..movie_wanted("userB", 123)
        };
        store.put_wanted(rec_a).await.unwrap();
        store.put_wanted(rec_b).await.unwrap();

        let all = store.all_wanted().await;
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|r| r.user == "userA" && r.tmdb_id == 123));
        assert!(all.iter().any(|r| r.user == "userB" && r.tmdb_id == 123));
    }

    #[tokio::test]
    async fn migrates_v2_db_to_v3_preserving_tables() {
        let tmp = TempDb::new("migrate_v2_v3");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                // matches row
                let mdef: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");
                let mut t = txn.open_table(mdef).unwrap();
                let i = info("m2");
                let m = movie("KeptV2");
                t.insert("m2", serde_json::to_vec(&(&i, &m)).unwrap().as_slice())
                    .unwrap();

                // owned_hashes row
                let odef: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
                let mut ot = txn.open_table(odef).unwrap();
                let rec = OwnedRecord {
                    request: req("tt2", 99_999),
                    provenance: Provenance::manual(),
                    added_at: 42,
                    status: OwnedStatus::Pending,
                    provides: vec![],
                    quality: None,
                };
                ot.insert("hash2", serde_json::to_vec(&rec).unwrap().as_slice())
                    .unwrap();

                // version stamp as v2
                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &2u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();

        // existing tables survive
        assert_eq!(
            store.get_match("m2".to_string()).await.unwrap().1.title,
            "KeptV2"
        );
        assert_eq!(
            store.get_owned("hash2".to_string()).await.unwrap().status,
            OwnedStatus::Pending
        );

        // new tables are usable after migration
        let tokens = TraktTokens {
            access: "a".to_string(),
            refresh: "r".to_string(),
            expires_at: 1,
            username: "u".to_string(),
            needs_reenrolment: false,
        };
        store
            .put_trakt_tokens("u".to_string(), tokens)
            .await
            .unwrap();
        assert_eq!(
            store
                .get_trakt_tokens("u".to_string())
                .await
                .unwrap()
                .access,
            "a"
        );

        let wanted_rec = movie_wanted("u", 1);
        store.put_wanted(wanted_rec.clone()).await.unwrap();
        assert_eq!(
            store.get_wanted("u".to_string(), 1).await.unwrap(),
            wanted_rec
        );

        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "valid v2 DB must not be moved aside"
        );
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
                t.insert("m1", serde_json::to_vec(&(&i, &m)).unwrap().as_slice())
                    .unwrap();
                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &1u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();
        assert_eq!(
            store.get_match("m1".to_string()).await.unwrap().1.title,
            "Kept"
        );
        store
            .put_authoritative("h".to_string(), movie("New"))
            .await
            .unwrap();
        assert_eq!(
            store
                .authoritative_meta("h".to_string())
                .await
                .unwrap()
                .title,
            "New"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "valid v1 DB must not be moved aside"
        );
    }

    // ── SP2 Task 4 tests (per-user provenance on OwnedRecord) ─────────────────

    /// Old-schema records (with `"source"` key, no `"provenance"` key) must decode as
    /// `Provenance::manual()` — the safe default the reconciler never auto-removes.
    /// The decode must be invariant to the old string's value, not just the literal `"manual"`.
    #[tokio::test]
    async fn old_encoding_decodes_to_manual() {
        let old = serde_json::json!({
            "request": serde_json::to_value(req("tt1", 27205)).unwrap(),
            "source": "manual",
            "added_at": 100,
            "status": "Pending",
        });
        let decoded: OwnedRecord = serde_json::from_value(old).unwrap();
        assert_eq!(decoded.provenance, Provenance::manual());
        assert_eq!(decoded.status, OwnedStatus::Pending);

        // An arbitrary old "source" value must also decode as manual() — the field is ignored.
        let old_arbitrary = serde_json::json!({
            "request": serde_json::to_value(req("tt1", 27205)).unwrap(),
            "source": "test",
            "added_at": 100,
            "status": "Pending",
        });
        let decoded2: OwnedRecord = serde_json::from_value(old_arbitrary).unwrap();
        assert_eq!(decoded2.provenance, Provenance::manual());
    }

    /// A new-format record with multi-user provenance must survive a put→get round-trip
    /// through the store with all entries intact.
    #[tokio::test]
    async fn provenance_round_trips_through_store() {
        let store = mem_store();
        let expected_prov = Provenance {
            entries: vec![
                ProvenanceEntry::Watchlist {
                    user: "alice".into(),
                },
                ProvenanceEntry::InProgress { user: "bob".into() },
            ],
        };
        let rec = OwnedRecord {
            request: req("tt1", 27205),
            provenance: expected_prov.clone(),
            added_at: 100,
            status: OwnedStatus::Pending,
            provides: vec![],
            quality: None,
        };
        store.put_owned("h1".to_string(), rec).await.unwrap();
        let got = store.get_owned("h1".to_string()).await.unwrap();
        assert_eq!(got.provenance, expected_prov);
    }

    // ── SP3 Task 2 tests (OwnedRecord provides/quality, selection, upgrade_checks) ─

    #[tokio::test]
    async fn owned_record_provides_and_quality_round_trip() {
        let store = mem_store();
        let rec = OwnedRecord {
            request: req("tt1", 1396),
            provenance: Provenance::manual(),
            added_at: 100,
            status: OwnedStatus::Pending,
            provides: vec![(1, 1), (1, 2)],
            quality: Some(crate::release::QualitySummary {
                cached: true,
                source_tier: 6_000,
                resolution: 1080,
                score: 42,
            }),
        };
        store
            .put_owned("h1".to_string(), rec.clone())
            .await
            .unwrap();
        let got = store.get_owned("h1".to_string()).await.unwrap();
        assert_eq!(got.provides, vec![(1, 1), (1, 2)]);
        assert_eq!(got.quality.unwrap().source_tier, 6_000);
    }

    /// An old OwnedRecord JSON (no `provides`/`quality` keys) decodes with empty/None defaults.
    #[test]
    fn owned_record_old_encoding_defaults_provides_and_quality() {
        let old = serde_json::json!({
            "request": serde_json::to_value(req("tt1", 27205)).unwrap(),
            "provenance": serde_json::to_value(Provenance::manual()).unwrap(),
            "added_at": 1,
            "status": "Verified",
        });
        let decoded: OwnedRecord = serde_json::from_value(old).unwrap();
        assert!(decoded.provides.is_empty());
        assert!(decoded.quality.is_none());
    }

    #[tokio::test]
    async fn selection_round_trip_and_remove() {
        let store = mem_store();
        let slot = crate::store::episode_slot(1396, 1, 2);
        store
            .put_selection(
                slot.clone(),
                SelectionEntry {
                    hash: "h1".into(),
                    file_path: "S01E02.mkv".into(),
                },
            )
            .await
            .unwrap();
        let got = store.get_selection(slot.clone()).await.unwrap();
        assert_eq!(got.hash, "h1");
        assert_eq!(got.file_path, "S01E02.mkv");
        let all = store.all_selection().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, slot);
        assert_eq!(all[0].1.hash, "h1");
        store.remove_selection(slot.clone()).await.unwrap();
        assert!(store.get_selection(slot).await.is_none());
    }

    #[tokio::test]
    async fn upgrade_checked_cursor_round_trip() {
        let store = mem_store();
        assert_eq!(store.get_upgrade_checked(1396).await, 0, "absent → 0");
        store
            .set_upgrade_checked(1396, 1_700_000_000)
            .await
            .unwrap();
        assert_eq!(store.get_upgrade_checked(1396).await, 1_700_000_000);
    }

    #[test]
    fn slot_keys_distinguish_movie_and_episode() {
        assert_eq!(crate::store::movie_slot(27205), "m|27205");
        assert_eq!(crate::store::episode_slot(1396, 1, 2), "e|1396|1|2");
    }

    /// `Provenance::merge` unions entries and deduplicates; `has_manual_entry` reports correctly.
    #[test]
    fn provenance_merge_deduplicates() {
        let mut p = Provenance::watchlist("alice");
        p.merge(&Provenance::in_progress("bob"));
        assert_eq!(p.entries.len(), 2);

        // Merging an identical entry must not grow the list.
        p.merge(&Provenance::watchlist("alice"));
        assert_eq!(p.entries.len(), 2, "duplicate entry must not be added");

        assert!(
            !p.has_manual_entry(),
            "watchlist+in_progress provenance must not report has_manual_entry"
        );
        assert!(
            Provenance::manual().has_manual_entry(),
            "Manual provenance must report has_manual_entry"
        );

        // Critical mixed case: a manually-acquired title that a Trakt user later watchlisted
        // must still be protected from auto-removal (this is what the OLD .all()-based predicate
        // wrongly returned false for).
        let mut mixed = Provenance::manual();
        mixed.merge(&Provenance::watchlist("alice"));
        assert_eq!(
            mixed.entries.len(),
            2,
            "[Manual, Watchlist] should have 2 entries"
        );
        assert!(
            mixed.has_manual_entry(),
            "mixed Manual+Watchlist must report has_manual_entry"
        );

        // A pure watchlist provenance must NOT be considered manual.
        assert!(
            !Provenance::watchlist("alice").has_manual_entry(),
            "pure watchlist must not report has_manual_entry"
        );
    }
}
