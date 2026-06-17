use crate::error::AppError;
use crate::rd_client::TorrentInfo;
use crate::vfs::MediaMetadata;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Current on-disk schema version. Bump when a migration is added in `run_migrations`.
//
// v6: the `upgrade_checks` cursor key gained a media-type discriminator (`upgrade_check_key`),
//     orphaning old bare-`tmdb_id` rows; the v5→v6 migration clears the (regenerable) cursor.
// v7: the `blacklist` key gained a media-type discriminator (`blacklist_key`) so a movie and a show
//     sharing a numeric TMDB id no longer cross-contaminate rejections; the v6→v7 migration clears
//     the (regenerable) blacklist.
/// v1→v2: additive (owned_hashes, authoritative_ids, blacklist tables).
/// v2→v3: additive (trakt_tokens, wanted tables).
/// v3→v4: additive (selection, upgrade_checks tables; OwnedRecord.provides/quality fields).
/// v4→v5: the `wanted` row key gained a media-type discriminator (movie/show with the same numeric
///        TMDB id no longer collide). Old `{user}|{tmdb_id}` rows are cleared — regenerated from
///        Trakt within one sync interval (lossless).
pub const SCHEMA_VERSION: u64 = 7;

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
/// Blacklisted (media_type, tmdb_id, hash) triples: "<m|s>|tmdbid|hash" -> serde_json({reason, at}).
/// The media-type discriminator is required for the same reason as `WANTED_TABLE`'s: TMDB movie and
/// TV id-spaces are independent, so a hash rejected for the movie with id N must NOT also suppress
/// the unrelated SHOW with id N (which is a different title that may legitimately want that hash).
const BLACKLIST_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("blacklist");

/// Build a `blacklist` key from (media_type, tmdb_id, hash). The hash is lowercased so the table is
/// case-insensitive (consistent with `all_blacklisted_hashes`). `MediaKind` is used (not `MediaType`)
/// because every caller — the acquisition/upgrade engines — works in `MediaKind`.
fn blacklist_key(kind: crate::scraper::MediaKind, tmdb_id: u64, hash: &str) -> String {
    let disc = match kind {
        crate::scraper::MediaKind::Movie => 'm',
        crate::scraper::MediaKind::Series => 's',
    };
    format!("{}|{}|{}", disc, tmdb_id, hash.to_ascii_lowercase())
}
/// Per-user Trakt OAuth tokens: user_slug -> serde_json(TraktTokens).
const TRAKT_TOKENS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("trakt_tokens");
/// Materialised wanted-set: "user|<m|s>|tmdb_id" -> serde_json(WantedRecord). The media-type
/// discriminator is required because TMDB movie and TV id-spaces are independent — the same numeric
/// id can be both a movie and a show, and without it the two would collide on one key.
const WANTED_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("wanted");

/// Build a `wanted` row key from (user, media_type, tmdb_id). See [`WANTED_TABLE`].
fn wanted_key(user: &str, media_type: &crate::vfs::MediaType, tmdb_id: u64) -> String {
    let disc = match media_type {
        crate::vfs::MediaType::Movie => 'm',
        crate::vfs::MediaType::Show => 's',
    };
    format!("{}|{}|{}", user, disc, tmdb_id)
}

/// Key for the `upgrade_checks` round-robin cursor — discriminated by media type so a movie and a
/// show sharing a numeric TMDB id keep independent cursors (mirrors `wanted_key`'s discriminator).
fn upgrade_check_key(media_type: &crate::vfs::MediaType, tmdb_id: u64) -> String {
    let disc = match media_type {
        crate::vfs::MediaType::Movie => 'm',
        crate::vfs::MediaType::Show => 's',
    };
    format!("{}|{}", disc, tmdb_id)
}
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
    /// The IMDB id Trakt supplied for this title (`tt…`), if any. Preferred over re-deriving the
    /// id from TMDB's `external_ids` when acquiring, because TMDB's mapping is incomplete for some
    /// titles (e.g. a TVDB-only show), which would otherwise leave the title permanently
    /// unacquirable. `#[serde(default)]` keeps pre-existing rows (which lack it) readable; they are
    /// backfilled on the next Trakt sync, which rewrites the whole wanted-set.
    #[serde(default)]
    pub imdb_id: Option<String>,
}

/// Why opening the on-disk database failed, split by recovery strategy.
enum OpenFailure {
    /// The file is genuinely damaged or format-incompatible (corrupt / needs-repair /
    /// old-or-newer format) — safe to move aside and recreate. The regenerable `matches` cache
    /// makes this lossless in practice; non-corrupt upgrades migrate authoritative tables instead.
    Corrupt(String),
    /// A transient/operational failure: another instance holds the lock, the path is not writable,
    /// disk is full, etc. The file is intact — discarding it would lose authoritative data, so the
    /// caller fails startup and lets the operator fix the underlying condition.
    Transient(String),
}

/// `true` for an I/O error kind that means the *file* is malformed/truncated (corruption we can
/// recover from), as opposed to an operational failure (permissions, disk full, …) that must not
/// discard a possibly-intact database. redb surfaces a bad header / too-short file as
/// `Io(InvalidData)`; a truncated file as `Io(UnexpectedEof)`.
fn io_kind_is_corruption(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
    )
}

/// Classify a `Database::create`/open error. Only a positive *corruption* signal moves the file
/// aside; lock contention (another instance), permission/disk I/O errors, and unknown future
/// variants are treated as transient so a healthy database is never silently discarded.
fn db_open_failure(e: redb::DatabaseError) -> OpenFailure {
    use redb::{DatabaseError, StorageError};
    let corrupt = match &e {
        DatabaseError::RepairAborted | DatabaseError::UpgradeRequired(_) => true,
        DatabaseError::Storage(StorageError::Corrupted(_)) => true,
        DatabaseError::Storage(StorageError::Io(io)) => io_kind_is_corruption(io.kind()),
        _ => false,
    };
    let msg = format!("open failed: {e}");
    if corrupt {
        OpenFailure::Corrupt(msg)
    } else {
        OpenFailure::Transient(msg)
    }
}

/// Classify an error surfacing during schema read/init on an already-opened database. A corruption
/// error (or a malformed-file I/O error) means the file is damaged (recover); any other error is
/// operational (do not discard).
fn schema_failure(stage: &str, e: redb::Error) -> OpenFailure {
    let corrupt = match &e {
        redb::Error::Corrupted(_) => true,
        redb::Error::Io(io) => io_kind_is_corruption(io.kind()),
        _ => false,
    };
    let msg = format!("{stage}: {e}");
    if corrupt {
        OpenFailure::Corrupt(msg)
    } else {
        OpenFailure::Transient(msg)
    }
}

/// Owns the redb database and all table access. Cheap to clone (the database is an `Arc`).
#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
}

impl Store {
    /// Open (or create) the database at `path`, recovering automatically from an
    /// unreadable / incompatible / corrupt / newer-than-binary file rather than
    /// failing startup. A *transient* failure (another instance holds the lock, the
    /// path is not writable, disk full, …) leaves the file intact and fails startup
    /// instead — discarding a possibly-intact database would lose authoritative data.
    /// Synchronous — called once at startup.
    pub fn open(path: &str) -> Result<Self, AppError> {
        let db = match Self::try_open(path) {
            Ok(db) => db,
            Err(OpenFailure::Corrupt(reason)) => {
                warn!(
                    "Database {} is unusable ({}); moving it aside and recreating",
                    path, reason
                );
                Self::move_aside_and_create(path)?
            }
            Err(OpenFailure::Transient(reason)) => {
                return Err(AppError::Config(format!(
                    "Database {path} could not be opened ({reason}). Refusing to discard a \
                     possibly-intact database — check that no other instance is running and that \
                     {path} is writable."
                )));
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

    /// Open the file and bring its schema to the current version. Returns an
    /// [`OpenFailure`] classifying *why* it failed so the caller can decide between
    /// recovering (move aside) and refusing to discard an intact database.
    fn try_open(path: &str) -> Result<Database, OpenFailure> {
        let db = Database::create(path).map_err(db_open_failure)?;
        let version =
            Self::read_version(&db).map_err(|e| schema_failure("schema read failed", e))?;
        if version > SCHEMA_VERSION {
            // Newer-than-binary is an incompatibility we recover from (move aside), per the
            // documented self-heal contract.
            return Err(OpenFailure::Corrupt(format!(
                "schema v{version} is newer than supported v{SCHEMA_VERSION}"
            )));
        }
        Self::ensure_schema(&db, version).map_err(|e| schema_failure("schema init failed", e))?;
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
        // INVARIANT: migrations commit in their own transaction(s) BEFORE the version stamp below,
        // so a crash between the two re-runs `run_migrations` on the next boot. Every migration step
        // MUST therefore be idempotent (all three current steps — the `3..5` `wanted` clear, the `<6`
        // `upgrade_checks` clear, and the `<7` `blacklist` clear, each `t.retain(|_,_| false)` — are).
        // A future non-idempotent migration must instead fold its work into the same write txn that
        // stamps `SCHEMA_VERSION` (below) so the two commit atomically.
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
    /// v4→v5: the `wanted` row key gained a media-type discriminator. Old `{user}|{tmdb_id}` rows are
    /// unreachable by the new key, so clear the table — it repopulates from Trakt within one sync
    /// interval (lossless). Only relevant when a pre-v5 DB already has a populated `wanted` table
    /// (introduced in v3); a fresh DB (from_version 0) has no rows to clear.
    /// v5→v6: the `upgrade_checks` cursor key likewise gained a media-type discriminator
    /// (`upgrade_check_key`). Old bare-`tmdb_id` rows are unreachable by the new key, so clear the
    /// table — it is a regenerable round-robin cursor, so clearing it just resets the cursor once
    /// (lossless). Cleared for any pre-v6 DB; a fresh DB has no rows to clear.
    /// v6→v7: the `blacklist` key gained a media-type discriminator (`blacklist_key`) so a movie and a
    /// show sharing a numeric TMDB id no longer cross-contaminate each other's rejection history. Old
    /// `{tmdb_id}|{hash}` rows are unreachable by the new key, so clear the table — the blacklist is
    /// regenerable (a rejected hash is simply re-probed and re-blacklisted), so clearing it is lossless
    /// in practice. Cleared for any pre-v7 DB; a fresh DB has no rows to clear.
    fn run_migrations(db: &Database, from_version: u64) -> Result<(), redb::Error> {
        if (3..5).contains(&from_version) {
            let write_txn = db.begin_write()?;
            {
                // Opening creates the table if absent; clearing then is a harmless no-op.
                let mut t = write_txn.open_table(WANTED_TABLE)?;
                t.retain(|_, _| false)?;
            }
            write_txn.commit()?;
        }
        if from_version < 6 {
            let write_txn = db.begin_write()?;
            {
                // Opening creates the table if absent; clearing then is a harmless no-op.
                let mut t = write_txn.open_table(UPGRADE_CHECKS_TABLE)?;
                t.retain(|_, _| false)?;
            }
            write_txn.commit()?;
        }
        if from_version < 7 {
            let write_txn = db.begin_write()?;
            {
                // Opening creates the table if absent; clearing then is a harmless no-op.
                let mut t = write_txn.open_table(BLACKLIST_TABLE)?;
                t.retain(|_, _| false)?;
            }
            write_txn.commit()?;
        }
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

    /// Collapse a `spawn_blocking` result for a WRITE accessor: a redb error propagates, and a join
    /// failure (the blocking task panicked or was cancelled) is logged AND returned as an error.
    /// Returning `Err` here — rather than the old log-and-swallow `Ok(())` — is important for writes:
    /// swallowing a join failure would report a persistence success that never happened (silent write
    /// loss). Callers that genuinely want best-effort semantics opt in explicitly with `.ok()`.
    fn flatten_join(
        result: Result<Result<(), redb::Error>, tokio::task::JoinError>,
    ) -> Result<(), AppError> {
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(AppError::Db(e)),
            Err(e) => {
                error!("redb blocking write task did not complete: {:?}", e);
                Err(AppError::Task(format!(
                    "redb blocking task did not complete: {e}"
                )))
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
                match existing {
                    Some(raw) => match serde_json::from_slice::<OwnedRecord>(&raw) {
                        Ok(mut rec) => {
                            rec.status = status;
                            match serde_json::to_vec(&rec) {
                                Ok(bytes) => {
                                    table.insert(hash.as_str(), bytes.as_slice())?;
                                }
                                // Effectively unreachable for this plain struct, but never drop a
                                // write silently — surface it so a lost status change is diagnosable.
                                Err(e) => error!(
                                    "set_owned_status: re-serialise failed for {hash} ({e}); status change dropped"
                                ),
                            }
                        }
                        Err(e) => error!(
                            "set_owned_status: corrupt owned record for {hash} ({e}); status change dropped"
                        ),
                    },
                    None => warn!("set_owned_status: no owned record for {hash}; status change dropped"),
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
        kind: crate::scraper::MediaKind,
        tmdb_id: u64,
        hash: String,
        reason: &str,
        at: u64,
    ) -> Result<(), AppError> {
        // Keyed by (media_type, tmdb_id, lowercased hash) — see `blacklist_key`.
        let key = blacklist_key(kind, tmdb_id, &hash);
        let bytes = match serde_json::to_vec(&serde_json::json!({"reason": reason, "at": at})) {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to serialise blacklist entry {}: {}", key, e);
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

    pub async fn is_blacklisted(
        &self,
        kind: crate::scraper::MediaKind,
        tmdb_id: u64,
        hash: String,
    ) -> bool {
        let key = blacklist_key(kind, tmdb_id, &hash);
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

    /// Every blacklisted infohash (lowercased), across ALL media types and tmdb_ids. Used by the
    /// account mirror to avoid re-adopting a hash the engine rejected — deliberately media-type- and
    /// tmdb-agnostic (the key is `<m|s>|tmdb_id|hash`, so the hash is the segment after the LAST `|`)
    /// so it still catches a torrent that re-identifies to a DIFFERENT title than the one it was
    /// blacklisted under (the wrong-title case). One read per scan tick.
    pub async fn all_blacklisted_hashes(&self) -> std::collections::HashSet<String> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = std::collections::HashSet::new();
            if let Ok(txn) = db.begin_read() {
                if let Ok(table) = txn.open_table(BLACKLIST_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (k, _) = entry;
                            // key = "<m|s>|tmdb_id|hash" → the hash is everything after the LAST '|'.
                            if let Some((_, hash)) = k.value().rsplit_once('|') {
                                out.insert(hash.to_ascii_lowercase());
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

    /// Remove blacklist entries stamped (`at`) strictly before `cutoff_at` (Unix seconds). The
    /// blacklist is otherwise append-only, so without a TTL prune it grows for the deployment's
    /// lifetime (library churn leaves dead rows behind) and the O(total) read paths
    /// (`all_blacklisted_hashes` / `blacklisted_hashes_for`) get progressively slower. Pruning also
    /// makes a long-rejected hash eligible to retry again, in case a better/fixed release later
    /// appears under it. Returns the number of rows removed. A malformed/`at`-less row is treated as
    /// age 0 (pruned) — it predates the `at` field and is regenerable anyway.
    pub async fn prune_blacklist_before(&self, cutoff_at: u64) -> usize {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || -> usize {
            let txn = match db.begin_write() {
                Ok(t) => t,
                Err(_) => return 0,
            };
            let mut removed = 0usize;
            {
                let mut table = match txn.open_table(BLACKLIST_TABLE) {
                    Ok(t) => t,
                    Err(_) => return 0,
                };
                // Collect stale keys first (can't remove during the borrowing iteration).
                let stale: Vec<String> = match table.iter() {
                    Ok(iter) => iter
                        .flatten()
                        .filter_map(|(k, v)| {
                            let at = serde_json::from_slice::<serde_json::Value>(v.value())
                                .ok()
                                .and_then(|j| j.get("at").and_then(|a| a.as_u64()))
                                .unwrap_or(0);
                            (at < cutoff_at).then(|| k.value().to_string())
                        })
                        .collect(),
                    Err(_) => return 0,
                };
                for k in &stale {
                    if table.remove(k.as_str()).is_ok() {
                        removed += 1;
                    }
                }
            }
            // A failed commit rolls back the removals — report 0 (nothing persisted).
            if txn.commit().is_err() {
                return 0;
            }
            removed
        })
        .await
        .unwrap_or(0)
    }

    /// Every blacklisted infohash (lowercased) for a SINGLE `(media_type, tmdb_id)` title — the
    /// per-title rejected set the acquisition engine filters candidates against. One read instead of
    /// one `is_blacklisted` per scraped candidate. The media-type discriminator + trailing `|` in the
    /// prefix scope it to exactly this title (so the movie with id N doesn't see the show-N rejects)
    /// and prevent a tmdb_id-prefix collision (e.g. `m|12|…` must not match `m|123|…`).
    pub async fn blacklisted_hashes_for(
        &self,
        kind: crate::scraper::MediaKind,
        tmdb_id: u64,
    ) -> std::collections::HashSet<String> {
        // Reuse the key builder with an empty hash to get the exact `<m|s>|tmdb_id|` prefix.
        let prefix = blacklist_key(kind, tmdb_id, "");
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = std::collections::HashSet::new();
            if let Ok(txn) = db.begin_read() {
                if let Ok(table) = txn.open_table(BLACKLIST_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (k, _) = entry;
                            if let Some(hash) = k.value().strip_prefix(&prefix) {
                                out.insert(hash.to_ascii_lowercase());
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
        let key = wanted_key(&rec.user, &rec.media_type, rec.tmdb_id);
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

    pub async fn get_wanted(
        &self,
        user: String,
        media_type: crate::vfs::MediaType,
        tmdb_id: u64,
    ) -> Option<WantedRecord> {
        let key = wanted_key(&user, &media_type, tmdb_id);
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

    pub async fn remove_wanted(
        &self,
        user: String,
        media_type: crate::vfs::MediaType,
        tmdb_id: u64,
    ) -> Result<(), AppError> {
        let key = wanted_key(&user, &media_type, tmdb_id);
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

    /// Returns the unix-second timestamp of the last upgrade check for `(media_type, tmdb_id)`, or 0
    /// if never checked. Keyed by media-type discriminator + id (like `wanted`) because a movie and a
    /// show can share a numeric TMDB id; without it, checking one would falsely advance the other's
    /// round-robin cursor and starve it of upgrade/consolidation passes.
    pub async fn get_upgrade_checked(
        &self,
        media_type: &crate::vfs::MediaType,
        tmdb_id: u64,
    ) -> u64 {
        let key = upgrade_check_key(media_type, tmdb_id);
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

    pub async fn set_upgrade_checked(
        &self,
        media_type: &crate::vfs::MediaType,
        tmdb_id: u64,
        at: u64,
    ) -> Result<(), AppError> {
        let key = upgrade_check_key(media_type, tmdb_id);
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

    #[tokio::test]
    async fn flatten_join_surfaces_write_task_failure_as_err() {
        // A panic (or cancellation) in a write accessor's blocking task must surface as an error,
        // NOT a swallowed Ok(()) that would report a persistence success that never happened.
        let joined: Result<Result<(), redb::Error>, tokio::task::JoinError> =
            tokio::task::spawn_blocking(|| -> Result<(), redb::Error> {
                panic!("simulated redb write-task panic")
            })
            .await;
        assert!(joined.is_err(), "the panicking task must yield a JoinError");
        assert!(
            matches!(Store::flatten_join(joined), Err(AppError::Task(_))),
            "a write-task join failure must map to AppError::Task, not Ok(())"
        );
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

    #[test]
    fn db_open_failure_keeps_intact_db_on_transient_errors() {
        // Lock contention (another instance) and I/O / permission errors must NOT move the DB
        // aside — discarding a possibly-intact database would lose authoritative data.
        assert!(matches!(
            db_open_failure(redb::DatabaseError::DatabaseAlreadyOpen),
            OpenFailure::Transient(_)
        ));
        assert!(matches!(
            db_open_failure(redb::DatabaseError::Storage(redb::StorageError::Io(
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied")
            ))),
            OpenFailure::Transient(_)
        ));
    }

    #[test]
    fn db_open_failure_recovers_only_on_genuine_corruption() {
        // A non-redb / damaged file (redb returns Corrupted "Invalid magic number"), an old format,
        // or an aborted repair are the only signals that justify moving the file aside.
        assert!(matches!(
            db_open_failure(redb::DatabaseError::Storage(redb::StorageError::Corrupted(
                "Invalid magic number".into()
            ))),
            OpenFailure::Corrupt(_)
        ));
        // A bad header / too-short file surfaces as Io(InvalidData) — recover, don't propagate.
        assert!(matches!(
            db_open_failure(redb::DatabaseError::Storage(redb::StorageError::Io(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "bad header")
            ))),
            OpenFailure::Corrupt(_)
        ));
        assert!(matches!(
            db_open_failure(redb::DatabaseError::UpgradeRequired(1)),
            OpenFailure::Corrupt(_)
        ));
        assert!(matches!(
            db_open_failure(redb::DatabaseError::RepairAborted),
            OpenFailure::Corrupt(_)
        ));
    }

    #[test]
    fn schema_failure_classifies_corrupt_vs_transient() {
        // The sibling of db_open_failure, for errors during schema read/init: a corruption signal
        // (or a malformed-file Io) means recover; any other (operational) error must KEEP the
        // possibly-intact DB rather than discard it.
        assert!(matches!(
            schema_failure("stamp", redb::Error::Corrupted("bad".into())),
            OpenFailure::Corrupt(_)
        ));
        assert!(matches!(
            schema_failure(
                "stamp",
                redb::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bad header"
                )),
            ),
            OpenFailure::Corrupt(_)
        ));
        // A transient Io (permission) must NOT discard the DB.
        assert!(matches!(
            schema_failure(
                "stamp",
                redb::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied"
                )),
            ),
            OpenFailure::Transient(_)
        ));
        // A non-Io / non-corruption redb error is operational → keep the DB.
        assert!(matches!(
            schema_failure("stamp", redb::Error::ValueTooLarge(1)),
            OpenFailure::Transient(_)
        ));
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
        let m = MediaKind::Movie;
        assert!(!store.is_blacklisted(m, 27205, "h1".to_string()).await);
        store
            .blacklist_add(m, 27205, "h1".to_string(), "WrongTitle", 100)
            .await
            .unwrap();
        assert!(store.is_blacklisted(m, 27205, "h1".to_string()).await);
        assert!(!store.is_blacklisted(m, 27205, "h2".to_string()).await);
        assert!(!store.is_blacklisted(m, 99999, "h1".to_string()).await);
    }

    #[tokio::test]
    async fn blacklist_is_scoped_by_media_type() {
        // A movie and a show sharing a numeric TMDB id must keep INDEPENDENT rejection histories —
        // a hash rejected for the movie must not suppress the unrelated show (which may legitimately
        // want it), and vice versa.
        let store = mem_store();
        store
            .blacklist_add(MediaKind::Movie, 1396, "h".to_string(), "WrongTitle", 1)
            .await
            .unwrap();
        assert!(
            store
                .is_blacklisted(MediaKind::Movie, 1396, "h".to_string())
                .await
        );
        assert!(
            !store
                .is_blacklisted(MediaKind::Series, 1396, "h".to_string())
                .await,
            "the show with the same tmdb_id must not inherit the movie's rejection"
        );
        assert!(
            store
                .blacklisted_hashes_for(MediaKind::Series, 1396)
                .await
                .is_empty(),
            "the per-title set for the show must not include the movie's blacklisted hash"
        );
        assert!(store
            .blacklisted_hashes_for(MediaKind::Movie, 1396)
            .await
            .contains("h"));
        // The mirror's hash-scoped view still sees it (media-type-agnostic, by design).
        assert!(store.all_blacklisted_hashes().await.contains("h"));
    }

    #[tokio::test]
    async fn blacklist_is_case_insensitive_on_hash() {
        // A hash added in one case must be found when queried in another (and vice versa), so a
        // rejected hash can never be silently re-added on a case mismatch.
        let store = mem_store();
        let m = MediaKind::Movie;
        let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF01".to_string();
        let lower = upper.to_ascii_lowercase();
        store
            .blacklist_add(m, 27205, upper.clone(), "WrongTitle", 100)
            .await
            .unwrap();
        assert!(store.is_blacklisted(m, 27205, lower.clone()).await);
        assert!(store.is_blacklisted(m, 27205, upper.clone()).await);
        // `all_blacklisted_hashes` returns the lowercased form.
        assert!(store.all_blacklisted_hashes().await.contains(&lower));
    }

    #[tokio::test]
    async fn prune_blacklist_before_removes_only_stale_rows() {
        let store = mem_store();
        let m = MediaKind::Movie;
        store
            .blacklist_add(m, 1, "old".into(), "WrongTitle", 100)
            .await
            .unwrap();
        store
            .blacklist_add(m, 1, "recent".into(), "Corrupt", 5_000)
            .await
            .unwrap();
        // Prune everything stamped before 1_000: only "old" (at=100) qualifies.
        let removed = store.prune_blacklist_before(1_000).await;
        assert_eq!(removed, 1);
        assert!(!store.is_blacklisted(m, 1, "old".into()).await);
        assert!(
            store.is_blacklisted(m, 1, "recent".into()).await,
            "a row newer than the cutoff must survive"
        );
    }

    #[tokio::test]
    async fn blacklisted_hashes_for_is_scoped_to_tmdb_and_lowercased() {
        let store = mem_store();
        let m = MediaKind::Movie;
        store
            .blacklist_add(m, 12, "AAA".into(), "WrongTitle", 1)
            .await
            .unwrap();
        store
            .blacklist_add(m, 12, "bbb".into(), "Corrupt", 2)
            .await
            .unwrap();
        // A different tmdb_id that shares the "12" prefix must NOT leak in (the trailing `|` guards).
        store
            .blacklist_add(m, 123, "ccc".into(), "WrongTitle", 3)
            .await
            .unwrap();
        let set = store.blacklisted_hashes_for(m, 12).await;
        assert_eq!(set.len(), 2);
        assert!(set.contains("aaa")); // lowercased
        assert!(set.contains("bbb"));
        assert!(!set.contains("ccc")); // belongs to tmdb 123, not 12
        assert!(store.blacklisted_hashes_for(m, 999).await.is_empty());
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
            imdb_id: None,
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
            imdb_id: None,
        }
    }

    #[test]
    fn wanted_record_legacy_row_without_imdb_field_reads_back_as_none() {
        // Backward-compat: rows persisted before the additive `imdb_id` field must still
        // deserialize (as None) so an existing DB isn't broken — they're rewritten with the field
        // on the next Trakt sync. Simulate a legacy row by dropping the key from the JSON.
        let rec = movie_wanted("alice", 27205);
        let mut val = serde_json::to_value(&rec).unwrap();
        val.as_object_mut().unwrap().remove("imdb_id");
        let back: WantedRecord = serde_json::from_value(val).expect("legacy row must read back");
        assert_eq!(back.imdb_id, None);
        assert_eq!(back, rec);
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
            .get_wanted("alice".to_string(), MediaType::Movie, 27205)
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
            .get_wanted("alice".to_string(), MediaType::Movie, 27205)
            .await
            .expect("updated present");
        assert_eq!(got_updated, updated_movie);

        let got_show = store
            .get_wanted("bob".to_string(), MediaType::Show, 1396)
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
            .remove_wanted("alice".to_string(), MediaType::Movie, 27205)
            .await
            .unwrap();
        assert!(store
            .get_wanted("alice".to_string(), MediaType::Movie, 27205)
            .await
            .is_none());
        assert_eq!(store.all_wanted().await.len(), 1);
    }

    #[tokio::test]
    async fn wanted_movie_and_show_with_same_tmdb_id_coexist() {
        // TMDB movie and TV id-spaces are independent: a movie and a show with the SAME numeric id
        // must both persist (the key carries a media-type discriminator), not overwrite each other.
        let store = mem_store();
        let movie = movie_wanted("alice", 1396);
        let show = show_wanted("alice", 1396);
        store.put_wanted(movie.clone()).await.unwrap();
        store.put_wanted(show.clone()).await.unwrap();

        assert_eq!(store.all_wanted().await.len(), 2, "both must persist");
        assert_eq!(
            store
                .get_wanted("alice".to_string(), MediaType::Movie, 1396)
                .await,
            Some(movie)
        );
        assert_eq!(
            store
                .get_wanted("alice".to_string(), MediaType::Show, 1396)
                .await,
            Some(show)
        );
        // Removing the movie leaves the show intact.
        store
            .remove_wanted("alice".to_string(), MediaType::Movie, 1396)
            .await
            .unwrap();
        assert!(store
            .get_wanted("alice".to_string(), MediaType::Movie, 1396)
            .await
            .is_none());
        assert!(store
            .get_wanted("alice".to_string(), MediaType::Show, 1396)
            .await
            .is_some());
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
    async fn migrates_v2_db_to_current_preserving_tables() {
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
            store
                .get_wanted("u".to_string(), MediaType::Movie, 1)
                .await
                .unwrap(),
            wanted_rec
        );

        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "valid v2 DB must not be moved aside"
        );
    }

    #[tokio::test]
    async fn migrates_v4_db_clears_legacy_wanted_rows_but_keeps_owned() {
        let tmp = TempDb::new("migrate_v4_v5");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                // A legacy wanted row keyed by the OLD `{user}|{tmdb_id}` format (no media-type
                // discriminator). Migration to v5 must clear it (it repopulates from Trakt).
                let wdef: TableDefinition<&str, &[u8]> = TableDefinition::new("wanted");
                let mut wt = txn.open_table(wdef).unwrap();
                let legacy = movie_wanted("alice", 27205);
                wt.insert(
                    "alice|27205",
                    serde_json::to_vec(&legacy).unwrap().as_slice(),
                )
                .unwrap();

                // An owned row must SURVIVE (authoritative, not regenerable).
                let odef: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
                let mut ot = txn.open_table(odef).unwrap();
                let rec = OwnedRecord {
                    request: req("tt4", 4242),
                    provenance: Provenance::manual(),
                    added_at: 7,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                };
                ot.insert("hash4", serde_json::to_vec(&rec).unwrap().as_slice())
                    .unwrap();

                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &4u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();

        assert!(
            store.all_wanted().await.is_empty(),
            "legacy wanted rows must be cleared on the v4→v5 migration"
        );
        assert_eq!(
            store.get_owned("hash4".to_string()).await.unwrap().status,
            OwnedStatus::Verified,
            "owned rows must survive the migration"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "valid v4 DB must not be moved aside"
        );
    }

    #[tokio::test]
    async fn migrates_v3_db_clears_legacy_wanted_rows_but_keeps_owned() {
        // v3 is the version `wanted` was introduced in; the `(3..5)` migration branch must clear it
        // too (not just v4). Sibling to the v4 test, asserting the v3 path of the same branch.
        let tmp = TempDb::new("migrate_v3_current");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let wdef: TableDefinition<&str, &[u8]> = TableDefinition::new("wanted");
                let mut wt = txn.open_table(wdef).unwrap();
                let legacy = movie_wanted("alice", 27205);
                wt.insert(
                    "alice|27205",
                    serde_json::to_vec(&legacy).unwrap().as_slice(),
                )
                .unwrap();

                let odef: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
                let mut ot = txn.open_table(odef).unwrap();
                let rec = OwnedRecord {
                    request: req("tt3", 3333),
                    provenance: Provenance::manual(),
                    added_at: 5,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                };
                ot.insert("hash3", serde_json::to_vec(&rec).unwrap().as_slice())
                    .unwrap();

                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &3u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();

        assert!(
            store.all_wanted().await.is_empty(),
            "legacy wanted rows must be cleared on the v3→current migration"
        );
        assert_eq!(
            store.get_owned("hash3".to_string()).await.unwrap().status,
            OwnedStatus::Verified,
            "owned rows must survive the migration"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "valid v3 DB must not be moved aside"
        );
    }

    #[tokio::test]
    async fn migrates_v5_db_clears_legacy_upgrade_checks_but_keeps_owned() {
        let tmp = TempDb::new("migrate_v5_v6");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                // A legacy upgrade_checks row keyed by the OLD bare-`tmdb_id` format (no media-type
                // discriminator). Migration to v6 must clear it — it's a regenerable round-robin
                // cursor, so a one-time reset is lossless.
                let udef: TableDefinition<&str, u64> = TableDefinition::new("upgrade_checks");
                let mut ut = txn.open_table(udef).unwrap();
                ut.insert("1396", &1_700_000_000u64).unwrap();

                // An owned row must SURVIVE (authoritative, not regenerable).
                let odef: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
                let mut ot = txn.open_table(odef).unwrap();
                let rec = OwnedRecord {
                    request: req("tt5", 1396),
                    provenance: Provenance::manual(),
                    added_at: 9,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                };
                ot.insert("hash5", serde_json::to_vec(&rec).unwrap().as_slice())
                    .unwrap();

                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &5u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();

        assert_eq!(
            store.get_owned("hash5".to_string()).await.unwrap().status,
            OwnedStatus::Verified,
            "owned rows must survive the migration"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "valid v5 DB must not be moved aside"
        );

        // Verify the clear DIRECTLY against the raw table — a `get_upgrade_checked` lookup would
        // return 0 whether or not the migration ran (the new key is `s|1396`, not the legacy
        // `1396`), so it can't prove the orphaned row was removed. Drop the store to release the
        // single-writer lock, reopen raw, and assert the table is empty.
        drop(store);
        let db = Database::create(&tmp.path).unwrap();
        let txn = db.begin_read().unwrap();
        let udef: TableDefinition<&str, u64> = TableDefinition::new("upgrade_checks");
        let ut = txn.open_table(udef).unwrap();
        assert_eq!(
            ut.iter().unwrap().count(),
            0,
            "the legacy bare-key upgrade_checks row must be cleared by the v5→v6 migration"
        );
    }

    #[tokio::test]
    async fn migrates_v6_db_clears_legacy_blacklist_but_keeps_owned() {
        let tmp = TempDb::new("migrate_v6_v7");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                // A legacy blacklist row keyed by the OLD `{tmdb_id}|{hash}` format (no media-type
                // discriminator). Migration to v7 must clear it — the blacklist is regenerable
                // (a rejected hash is simply re-probed), so a one-time reset is lossless.
                let bdef: TableDefinition<&str, &[u8]> = TableDefinition::new("blacklist");
                let mut bt = txn.open_table(bdef).unwrap();
                bt.insert(
                    "1396|deadbeef",
                    serde_json::to_vec(&serde_json::json!({"reason":"WrongTitle","at":1}))
                        .unwrap()
                        .as_slice(),
                )
                .unwrap();

                // An owned row must SURVIVE (authoritative, not regenerable).
                let odef: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
                let mut ot = txn.open_table(odef).unwrap();
                let rec = OwnedRecord {
                    request: req("tt6", 1396),
                    provenance: Provenance::manual(),
                    added_at: 11,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                };
                ot.insert("hash6", serde_json::to_vec(&rec).unwrap().as_slice())
                    .unwrap();

                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &6u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();
        assert_eq!(
            store.get_owned("hash6".to_string()).await.unwrap().status,
            OwnedStatus::Verified,
            "owned rows must survive the migration"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "valid v6 DB must not be moved aside"
        );
        // Verify the clear directly against the raw table (the new key is `<m|s>|1396|hash`, so a
        // typed lookup couldn't distinguish "cleared" from "unreachable by the new key").
        drop(store);
        let db = Database::create(&tmp.path).unwrap();
        let txn = db.begin_read().unwrap();
        let bdef: TableDefinition<&str, &[u8]> = TableDefinition::new("blacklist");
        let bt = txn.open_table(bdef).unwrap();
        assert_eq!(
            bt.iter().unwrap().count(),
            0,
            "the legacy bare-key blacklist row must be cleared by the v6→v7 migration"
        );
    }

    #[tokio::test]
    async fn migrates_v1_db_to_current_preserving_matches() {
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
        use crate::vfs::MediaType;
        let store = mem_store();
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Show, 1396).await,
            0,
            "absent → 0"
        );
        store
            .set_upgrade_checked(&MediaType::Show, 1396, 1_700_000_000)
            .await
            .unwrap();
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Show, 1396).await,
            1_700_000_000
        );
        // A movie sharing the same numeric id keeps an INDEPENDENT cursor (no collision).
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 1396).await,
            0,
            "movie cursor is independent of the show cursor for the same id"
        );
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
