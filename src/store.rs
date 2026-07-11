use crate::error::AppError;
use crate::rd_client::TorrentInfo;
use crate::vfs::MediaMetadata;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Current on-disk schema version. Bump when a migration is added in `run_migrations`.
///
/// Migration history (ascending):
/// - v1→v2: additive (owned_hashes, authoritative_ids, blacklist tables).
/// - v2→v3: additive (trakt_tokens, wanted tables).
/// - v3→v4: additive (selection, upgrade_checks tables; OwnedRecord.provides/quality fields).
/// - v4→v5: the `wanted` row key gained a media-type discriminator (movie/show with the same numeric
///   TMDB id no longer collide). Old `{user}|{tmdb_id}` rows are cleared — regenerated from Trakt
///   within one sync interval (lossless).
/// - v5→v6: the `upgrade_checks` cursor key gained a media-type discriminator (`upgrade_check_key`),
///   orphaning old bare-`tmdb_id` rows; the migration clears the (regenerable) cursor.
/// - v6→v7: the `blacklist` key gained a media-type discriminator (`blacklist_key`) so a movie and a
///   show sharing a numeric TMDB id no longer cross-contaminate rejections; the migration clears the
///   (regenerable) blacklist.
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
///
/// Invariant: the `|` joiner is delimiter-safe because no key input contains `|` — the discriminator
/// is a single char, `tmdb_id` is decimal, and a torrent infohash is hex. `all_blacklisted_hashes`'
/// `rsplit_once('|')` relies on this; a future non-hex key source would break it.
fn blacklist_key(kind: crate::scraper::MediaKind, tmdb_id: u64, hash: &str) -> String {
    let disc = media_type_disc(&media_kind_to_type(kind));
    format!("{}|{}|{}", disc, tmdb_id, hash.to_ascii_lowercase())
}

/// The SINGLE source of truth for the media-type discriminator char used in every key space
/// (`blacklist_key` over `MediaKind`, `wanted_key`/`upgrade_check_key` over `MediaType`). Keeping the
/// mapping in one place stops the key spaces from silently desyncing (e.g. `set_upgrade_checked`
/// stamping one char while the prune derives another), which would strand or wrongly prune cursor
/// rows.
fn media_type_disc(media_type: &crate::vfs::MediaType) -> char {
    match media_type {
        crate::vfs::MediaType::Movie => 'm',
        crate::vfs::MediaType::Show => 's',
    }
}

/// Map the acquisition/upgrade engines' `MediaKind` onto the `vfs::MediaType` discriminator, so the
/// blacklist key space (keyed by `MediaKind`) stays in lockstep with `wanted`/`upgrade_checks`
/// (keyed by `MediaType`). The single place this remap lives.
fn media_kind_to_type(kind: crate::scraper::MediaKind) -> crate::vfs::MediaType {
    match kind {
        crate::scraper::MediaKind::Movie => crate::vfs::MediaType::Movie,
        crate::scraper::MediaKind::Series => crate::vfs::MediaType::Show,
    }
}
/// Per-user Trakt OAuth tokens: user_slug -> serde_json(TraktTokens).
const TRAKT_TOKENS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("trakt_tokens");
/// Materialised wanted-set: "user|<m|s>|tmdb_id" -> serde_json(WantedRecord). The media-type
/// discriminator is required because TMDB movie and TV id-spaces are independent — the same numeric
/// id can be both a movie and a show, and without it the two would collide on one key.
const WANTED_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("wanted");

/// Build a `wanted` row key from (user, media_type, tmdb_id). See [`WANTED_TABLE`]. The `|` joiner is
/// delimiter-safe: a Trakt user slug contains no `|`, the discriminator is one char, and `tmdb_id` is
/// decimal (so `all_wanted`'s key parsing is unambiguous).
fn wanted_key(user: &str, media_type: &crate::vfs::MediaType, tmdb_id: u64) -> String {
    format!("{}|{}|{}", user, media_type_disc(media_type), tmdb_id)
}

/// Key for the `upgrade_checks` round-robin cursor — discriminated by media type so a movie and a
/// show sharing a numeric TMDB id keep independent cursors (mirrors `wanted_key`'s discriminator).
fn upgrade_check_key(media_type: &crate::vfs::MediaType, tmdb_id: u64) -> String {
    format!("{}|{}", media_type_disc(media_type), tmdb_id)
}
/// SP3 live-selection: slot ("m|tmdb" / "e|tmdb|s|e") -> serde_json(SelectionEntry).
const SELECTION_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("selection");
/// SP3 upgrade round-robin cursor: `upgrade_check_key` ("<m|s>|tmdb_id") -> last-checked unix secs.
/// (Since v6 the key carries a media-type discriminator — not the bare tmdb_id — which is why the
/// v5→v6 migration clears the table.)
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
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TraktTokens {
    pub access: String,
    pub refresh: String,
    pub expires_at: u64, // unix epoch seconds
    pub username: String,
    #[serde(default)]
    pub needs_reenrolment: bool,
}

// Manual `Debug` redacting the access/refresh tokens — a stray `{tokens:?}` must never leak them,
// matching the redaction convention used by `TraktConfig`/`FileLocator`/the provider clients.
impl std::fmt::Debug for TraktTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraktTokens")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("username", &self.username)
            .field("needs_reenrolment", &self.needs_reenrolment)
            .finish()
    }
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
    /// The file is genuinely damaged/unreadable (bad magic, truncated, redb repair aborted) — safe
    /// to move aside and recreate. The regenerable `matches` cache makes this lossless in practice;
    /// forward migrations bring an older OUR-schema DB up to date instead of discarding it.
    Corrupt(String),
    /// A transient/operational failure: another instance holds the lock, the path is not writable,
    /// disk is full, etc. — OR an INTACT database that this binary cannot safely use but must not
    /// discard: an older redb on-disk format (`UpgradeRequired`, e.g. after a redb dependency bump),
    /// or a schema written by a NEWER build of this app (version ahead of the binary, e.g. after a
    /// rollback). In every case the file is intact and holds non-regenerable authoritative data, so
    /// the caller fails startup and lets the operator fix the underlying condition (back up /
    /// migrate / roll the binary forward) instead.
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

/// A `<path>.corrupt` backup name that doesn't already exist (falling back to `<path>.corrupt.<n>`),
/// so moving an unusable DB aside never fails on — nor silently overwrites — a `.corrupt` from a
/// prior recovery. Returns `None` if the bounded search is exhausted (every candidate exists), so
/// the caller fails loudly rather than clobbering the oldest backup.
fn unique_backup_path(path: &str) -> Option<String> {
    let base = format!("{path}.corrupt");
    if !std::path::Path::new(&base).exists() {
        return Some(base);
    }
    for n in 1..1000 {
        let candidate = format!("{path}.corrupt.{n}");
        if !std::path::Path::new(&candidate).exists() {
            return Some(candidate);
        }
    }
    None
}

/// Classify a `Database::create`/open error. Only a positive *corruption* signal moves the file
/// aside; lock contention (another instance), permission/disk I/O errors, and unknown future
/// variants are treated as transient so a healthy database is never silently discarded.
fn db_open_failure(e: redb::DatabaseError) -> OpenFailure {
    use redb::{DatabaseError, StorageError};
    let corrupt = match &e {
        // redb tried to auto-repair the file and gave up → genuinely damaged, safe to recreate.
        DatabaseError::RepairAborted => true,
        DatabaseError::Storage(StorageError::Corrupted(_)) => true,
        DatabaseError::Storage(StorageError::Io(io)) => io_kind_is_corruption(io.kind()),
        // Everything else is transient (fail startup, keep the file). In particular
        // `UpgradeRequired` is an INTACT database in an OLDER on-disk format (e.g. after a redb
        // dependency bump across a format boundary): moving it aside would silently drop the
        // non-regenerable authoritative tables (owned/wanted/trakt_tokens/selection), violating the
        // "never silently dropped" contract — so we fail loudly and let the operator back up /
        // migrate rather than lose data.
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
    /// Open (or create) the database at `path`, recovering automatically from a genuinely
    /// unreadable / corrupt file (bad magic, truncated, redb repair aborted) rather than failing
    /// startup. A *transient/incompatible* failure — another instance holds the lock, the path is
    /// not writable, disk full, an older redb on-disk format (`UpgradeRequired`), OR a schema written
    /// by a NEWER build of this app (e.g. after a rollback) — leaves the file intact and fails
    /// startup instead, because discarding a possibly-intact database would lose non-regenerable
    /// authoritative data. Synchronous — called once at startup.
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
            // A DB written by a NEWER build of this app (e.g. after rolling a bad deploy back to an
            // older binary) is INTACT and full of non-regenerable authoritative data
            // (owned/wanted/trakt_tokens/selection). Moving it aside would silently discard that —
            // exactly the loss the `UpgradeRequired` (older redb on-disk format) path already refuses
            // — so treat it as transient and FAIL STARTUP, letting the operator roll the binary
            // forward or restore, rather than recreate an empty DB. Mirrors `from_database`, which
            // also refuses a newer-than-supported schema.
            return Err(OpenFailure::Transient(format!(
                "schema v{version} is newer than supported v{SCHEMA_VERSION}: the database was \
                 written by a newer build of this app. Refusing to discard it — run a build at \
                 schema v{version} or newer, or back up and remove {path}."
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

            // Only (re)stamp when the version actually advanced (fresh DB reads version 0, or a
            // migration just ran). On an already-current DB this skips the one redundant meta insert
            // (the enclosing write txn is still opened/committed for the create-if-absent opens above,
            // which are themselves no-ops for an existing DB).
            if version < SCHEMA_VERSION {
                meta.insert(SCHEMA_VERSION_KEY, &SCHEMA_VERSION)?;
            }
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

    /// Move an unusable (genuinely corrupt) database aside to `<path>.corrupt` (or `<path>.corrupt.<n>`
    /// if that already exists) and create a fresh one. NEVER deletes the file: if the rename fails,
    /// or every backup name is taken, it fails startup with the DB left on disk for the operator to
    /// inspect/remove — silently deleting it (the previous `remove_file` fallback) could discard data
    /// on a misclassification.
    fn move_aside_and_create(path: &str) -> Result<Database, AppError> {
        if std::path::Path::new(path).exists() {
            // Pick a backup name that doesn't already exist so the rename can't overwrite a
            // `.corrupt` from a prior recovery.
            let backup = unique_backup_path(path).ok_or_else(|| {
                AppError::Config(format!(
                    "cannot move aside unusable database {path}: exhausted backup names \
                     (.corrupt and .corrupt.1..999 all exist). Refusing to overwrite an existing \
                     backup — archive or remove the old .corrupt file(s) and restart."
                ))
            })?;
            std::fs::rename(path, &backup).map_err(|e| {
                AppError::Config(format!(
                    "failed to move aside unusable database {path} -> {backup} ({e}). Refusing to \
                     delete it — the file is left on disk for you to inspect/remove manually."
                ))
            })?;
            info!("Moved aside unusable database to {}", backup);
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
            let read_txn = Self::read_or_log("get_match begin_read", db.begin_read())?;
            let table =
                Self::read_or_log("get_match open_table", read_txn.open_table(MATCHES_TABLE))?;
            let entry = Self::read_or_log("get_match get", table.get(id.as_str()))??;
            Self::read_or_log(
                "get_match deserialise",
                serde_json::from_slice::<(TorrentInfo, MediaMetadata)>(entry.value()),
            )
        })
        .await
        .unwrap_or_else(|e| {
            error!("get_match: blocking read task failed: {e}");
            None
        })
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
                // A serialise failure means NOTHING is written — return Err rather than a phantom
                // Ok (which would report a persistence success that never happened, contradicting
                // `flatten_join`'s contract). Unreachable in practice (plain structs), but consistent.
                error!("Failed to serialise replacement match {}: {}", new_id, e);
                return Err(AppError::Task(format!(
                    "serialise replacement match {new_id}: {e}"
                )));
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

    /// Flatten a `spawn_blocking` join result, preserving the success value `T`: a redb error maps to
    /// `Db`, and a join failure (the blocking task panicked or was cancelled) is logged AND mapped to
    /// `Task` rather than being swallowed as a false success. Returning `Err` here — rather than a
    /// log-and-swallow `Ok` — is important for writes: swallowing a join failure would report a
    /// persistence success that never happened (silent write loss). Callers that genuinely want
    /// best-effort semantics opt in explicitly with `.ok()`. Generic over `T` so both unit-returning
    /// writes and value-returning ones (e.g. `put_owned_if_absent`'s `bool`) share one implementation.
    fn flatten_join<T>(
        result: Result<Result<T, redb::Error>, tokio::task::JoinError>,
    ) -> Result<T, AppError> {
        match result {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(AppError::Db(e)),
            Err(e) => {
                error!("redb blocking write task did not complete: {:?}", e);
                Err(AppError::Task(format!(
                    "redb blocking task did not complete: {e}"
                )))
            }
        }
    }

    /// Log a read error (with `ctx`) and map it to `None`, so a point-lookup accessor distinguishes a
    /// genuine miss (`Ok(None)`) from a transient begin_read/open_table/get/deserialise failure that a
    /// bare `.ok()` would silently swallow — a false "absent" on an authoritative table (owned/wanted/
    /// tokens/selection) can drive a duplicate acquire or a wrong lifecycle decision. Mirrors the
    /// per-row logging the `all_*` scans already do.
    fn read_or_log<T, E: std::fmt::Display>(ctx: &str, r: Result<T, E>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                error!("{ctx}: {e}");
                None
            }
        }
    }

    // ── owned_hashes accessors ────────────────────────────────────────────────

    pub async fn put_owned(&self, hash: String, rec: OwnedRecord) -> Result<(), AppError> {
        // Normalise the key to lowercase so the owned table is case-consistent regardless of caller
        // (the mirror lowercases; the scraper emits lowercase infohashes). This makes the exact-match
        // get/put/remove and `put_owned_if_absent`'s atomic dedup caller-proof — a differently-cased
        // write can never produce a duplicate record for one physical torrent.
        let hash = hash.to_ascii_lowercase();
        // Serialise before opening the transaction to avoid partial writes on
        // serde failure (mirrors `replace_match`).
        let bytes = match serde_json::to_vec(&rec) {
            Ok(b) => b,
            Err(e) => {
                // A serialise failure writes nothing → Err, not a phantom Ok (see `flatten_join`).
                error!("Failed to serialise OwnedRecord for {}: {}", hash, e);
                return Err(AppError::Task(format!(
                    "serialise OwnedRecord for {hash}: {e}"
                )));
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

    /// Insert an owned record ONLY if none exists for `hash`, atomically in one write transaction.
    /// Returns `true` if it wrote, `false` if a record was already present (left intact). The account
    /// mirror uses this so a concurrent `engine.acquire` (a separate scheduler task) that wrote a
    /// provenance-bearing record for the same hash can't be clobbered by the mirror's empty-provenance
    /// write — the check-and-insert is a single transaction, closing the `get_owned`→`put_owned` race.
    pub async fn put_owned_if_absent(
        &self,
        hash: String,
        rec: OwnedRecord,
    ) -> Result<bool, AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        let bytes = match serde_json::to_vec(&rec) {
            Ok(b) => b,
            Err(e) => {
                // Return Err rather than Ok(false): `false` means "already present → left intact" to
                // the caller, which is semantically WRONG for "serialise failed, nothing happened".
                error!("Failed to serialise OwnedRecord for {}: {}", hash, e);
                return Err(AppError::Task(format!(
                    "serialise OwnedRecord for {hash}: {e}"
                )));
            }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool, redb::Error> {
            let txn = db.begin_write()?;
            let wrote = {
                let mut table = txn.open_table(OWNED_TABLE)?;
                // `.is_some()` drops the read guard before the `&mut` insert below.
                if table.get(hash.as_str())?.is_some() {
                    false
                } else {
                    table.insert(hash.as_str(), bytes.as_slice())?;
                    true
                }
            };
            txn.commit()?;
            Ok(wrote)
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn get_owned(&self, hash: String) -> Option<OwnedRecord> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = Self::read_or_log("get_owned begin_read", db.begin_read())?;
            let table = Self::read_or_log("get_owned open_table", txn.open_table(OWNED_TABLE))?;
            let e = Self::read_or_log("get_owned get", table.get(hash.as_str()))??;
            Self::read_or_log(
                "get_owned deserialise",
                serde_json::from_slice::<OwnedRecord>(e.value()),
            )
        })
        .await
        .unwrap_or_else(|e| {
            error!("get_owned: blocking read task failed: {e}");
            None
        })
    }

    pub async fn set_owned_status(
        &self,
        hash: String,
        status: OwnedStatus,
    ) -> Result<(), AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), AppError> {
            let txn = db.begin_write().map_err(|e| AppError::Db(e.into()))?;
            {
                let mut table = txn
                    .open_table(OWNED_TABLE)
                    .map_err(|e| AppError::Db(e.into()))?;
                // Read the raw bytes into an owned Vec first so the read borrow ends
                // before we call insert (which needs &mut table).
                let existing: Option<Vec<u8>> = table
                    .get(hash.as_str())
                    .map_err(|e| AppError::Db(e.into()))?
                    .map(|guard| guard.value().to_vec());
                match existing {
                    Some(raw) => {
                        // A record that won't deserialise / re-serialise is a real failure: surface
                        // it (Err, NOT a phantom Ok) via an early `?` so the txn is dropped without
                        // committing and the caller — e.g. `observe` marking a torrent `Verified` —
                        // learns the status change did not land, instead of believing it did while
                        // the record silently stays `Pending`. (Both are effectively unreachable for
                        // this plain struct, but must never be swallowed.)
                        let mut rec = serde_json::from_slice::<OwnedRecord>(&raw).map_err(|e| {
                            AppError::Task(format!(
                                "set_owned_status: corrupt owned record for {hash}: {e}"
                            ))
                        })?;
                        rec.status = status;
                        let bytes = serde_json::to_vec(&rec).map_err(|e| {
                            AppError::Task(format!(
                                "set_owned_status: re-serialise failed for {hash}: {e}"
                            ))
                        })?;
                        table
                            .insert(hash.as_str(), bytes.as_slice())
                            .map_err(|e| AppError::Db(e.into()))?;
                    }
                    // A missing record is a legitimate no-op (e.g. a hash reaped between listing and
                    // here), NOT a failure — commit nothing and return Ok.
                    None => {
                        warn!("set_owned_status: no owned record for {hash}; status change skipped")
                    }
                }
            }
            txn.commit().map_err(|e| AppError::Db(e.into()))?;
            Ok(())
        })
        .await;
        match result {
            Ok(inner) => inner,
            Err(e) => {
                error!("redb blocking write task did not complete: {:?}", e);
                Err(AppError::Task(format!(
                    "redb blocking task did not complete: {e}"
                )))
            }
        }
    }

    pub async fn remove_owned(&self, hash: String) -> Result<(), AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
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
                        for entry in iter {
                            // owned_hashes is authoritative + non-regenerable: a silently-dropped row
                            // becomes an invisible torrent (duplicate re-acquire / lost provenance /
                            // remove churn) that is undiagnosable — so log BOTH a per-row read error
                            // (which `.flatten()` would have swallowed) and a deserialise error.
                            let (k, v) = match entry {
                                Ok(kv) => kv,
                                Err(e) => {
                                    error!("owned_hashes row read failed (skipped): {}", e);
                                    continue;
                                }
                            };
                            match serde_json::from_slice::<OwnedRecord>(v.value()) {
                                Ok(rec) => out.push((k.value().to_string(), rec)),
                                Err(e) => error!(
                                    "owned_hashes row {} failed to deserialise (skipped): {}",
                                    k.value(),
                                    e
                                ),
                            }
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_else(|e| {
            // Mirror `load_all_matches`: log a blocking-read JoinError rather than silently
            // returning an empty set. These accessors read authoritative, non-regenerable tables
            // (owned/wanted/selection/trakt_tokens/blacklist); a swallowed failure reads
            // downstream as "empty" with zero diagnostic and can drive mass re-acquire / wrong
            // lifecycle decisions.
            error!("store: blocking read task failed: {e:?}");
            Default::default()
        })
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
                // Nothing written on a serialise failure → Err, not a phantom Ok (see `flatten_join`).
                error!(
                    "Failed to serialise authoritative metadata for {}: {}",
                    hash, e
                );
                return Err(AppError::Task(format!(
                    "serialise authoritative metadata for {hash}: {e}"
                )));
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
            let txn = Self::read_or_log("authoritative_meta begin_read", db.begin_read())?;
            let table =
                Self::read_or_log("authoritative_meta open_table", txn.open_table(AUTH_TABLE))?;
            let e = Self::read_or_log("authoritative_meta get", table.get(hash.as_str()))??;
            Self::read_or_log(
                "authoritative_meta deserialise",
                serde_json::from_slice::<crate::vfs::MediaMetadata>(e.value()),
            )
        })
        .await
        .unwrap_or_else(|e| {
            error!("authoritative_meta: blocking read task failed: {e}");
            None
        })
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
                // Nothing written on a serialise failure → Err, not a phantom Ok (see `flatten_join`).
                error!("Failed to serialise blacklist entry {}: {}", key, e);
                return Err(AppError::Task(format!(
                    "serialise blacklist entry {key}: {e}"
                )));
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
                        for entry in iter {
                            // Log a per-row read error rather than `.flatten()`-swallowing it: a
                            // dropped blacklist row silently re-adopts a hash the engine rejected.
                            // (Regenerable via the TTL, so it self-heals — but keep it diagnosable,
                            // consistent with the other `all_*` accessors.)
                            let (k, _) = match entry {
                                Ok(kv) => kv,
                                Err(e) => {
                                    error!("blacklist row read failed (skipped): {}", e);
                                    continue;
                                }
                            };
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
        .unwrap_or_else(|e| {
            // Mirror `load_all_matches`: log a blocking-read JoinError rather than silently
            // returning an empty set. These accessors read authoritative, non-regenerable tables
            // (owned/wanted/selection/trakt_tokens/blacklist); a swallowed failure reads
            // downstream as "empty" with zero diagnostic and can drive mass re-acquire / wrong
            // lifecycle decisions.
            error!("store: blocking read task failed: {e:?}");
            Default::default()
        })
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
            // A begin_write/open_table/iter/commit failure here means the append-only blacklist
            // stopped pruning — `warn!` it rather than silently returning 0, so a persistent write
            // failure (which would let the table grow unbounded) is visible in the logs instead of
            // reading as "nothing was stale".
            let txn = match db.begin_write() {
                Ok(t) => t,
                Err(e) => {
                    warn!("blacklist prune skipped: begin_write failed: {}", e);
                    return 0;
                }
            };
            let mut removed = 0usize;
            {
                let mut table = match txn.open_table(BLACKLIST_TABLE) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("blacklist prune skipped: open_table failed: {}", e);
                        return 0;
                    }
                };
                // Collect stale keys first (can't remove during the borrowing iteration). Log a
                // per-row read error rather than `.flatten()`-swallowing it, matching the `all_*`
                // accessors' convention — a row that can't be read escapes pruning silently otherwise.
                let stale: Vec<String> = match table.iter() {
                    Ok(iter) => iter
                        .filter_map(|entry| {
                            let (k, v) = match entry {
                                Ok(kv) => kv,
                                Err(e) => {
                                    error!("blacklist row read failed (skipped in prune): {}", e);
                                    return None;
                                }
                            };
                            let at = serde_json::from_slice::<serde_json::Value>(v.value())
                                .ok()
                                .and_then(|j| j.get("at").and_then(|a| a.as_u64()))
                                .unwrap_or(0);
                            (at < cutoff_at).then(|| k.value().to_string())
                        })
                        .collect(),
                    Err(e) => {
                        warn!("blacklist prune skipped: table iter failed: {}", e);
                        return 0;
                    }
                };
                for k in &stale {
                    if table.remove(k.as_str()).is_ok() {
                        removed += 1;
                    }
                }
            }
            // A failed commit rolls back the removals — report 0 (nothing persisted).
            if let Err(e) = txn.commit() {
                warn!(
                    "blacklist prune commit failed (removals rolled back): {}",
                    e
                );
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
                    // The key is "<m|s>|tmdb_id|hash", so this title's rows are a contiguous,
                    // lexicographically-ordered key range. A keyed range scan reads only those rows
                    // (O(log n + matches)) instead of the whole table (which grows to 30 days of
                    // churn under the TTL). Any key that sorts at/after `prefix` but does NOT start
                    // with it sorts strictly AFTER the entire matching block, so the first
                    // non-matching key ends the scan.
                    if let Ok(iter) = table.range(prefix.as_str()..) {
                        for entry in iter.flatten() {
                            let (k, _) = entry;
                            match k.value().strip_prefix(&prefix) {
                                Some(hash) => {
                                    out.insert(hash.to_ascii_lowercase());
                                }
                                None => break,
                            }
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_else(|e| {
            // Mirror `load_all_matches`: log a blocking-read JoinError rather than silently
            // returning an empty set. These accessors read authoritative, non-regenerable tables
            // (owned/wanted/selection/trakt_tokens/blacklist); a swallowed failure reads
            // downstream as "empty" with zero diagnostic and can drive mass re-acquire / wrong
            // lifecycle decisions.
            error!("store: blocking read task failed: {e:?}");
            Default::default()
        })
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
                // Nothing written on a serialise failure → Err, not a phantom Ok (see `flatten_join`).
                error!("Failed to serialise TraktTokens for {}: {}", slug, e);
                return Err(AppError::Task(format!(
                    "serialise TraktTokens for {slug}: {e}"
                )));
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

    /// Atomically apply `mutate` to the stored tokens for `slug`, but ONLY if the stored `refresh`
    /// still equals `expected_refresh`. Returns `Ok(true)` if applied, `Ok(false)` if skipped (the
    /// record was removed, or its refresh token was rotated by a concurrent refresh since it was
    /// read). This closes the read-modify-write race between concurrent Trakt refreshes (two
    /// `/refresh` clicks, or a `/refresh` racing the background sync's refresh) that would otherwise
    /// clobber a freshly-rotated single-use token with a stale one plus a spurious
    /// `needs_reenrolment` — the whole read → compare → write happens inside one redb write txn.
    pub async fn update_trakt_tokens_if_refresh_matches(
        &self,
        slug: String,
        expected_refresh: String,
        mutate: impl FnOnce(&mut TraktTokens) + Send + 'static,
    ) -> Result<bool, AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool, AppError> {
            let txn = db.begin_write().map_err(|e| AppError::Db(e.into()))?;
            let applied = {
                let mut table = txn
                    .open_table(TRAKT_TOKENS_TABLE)
                    .map_err(|e| AppError::Db(e.into()))?;
                let existing: Option<Vec<u8>> = table
                    .get(slug.as_str())
                    .map_err(|e| AppError::Db(e.into()))?
                    .map(|g| g.value().to_vec());
                match existing {
                    // Record removed since it was read (a concurrent /remove) — do not resurrect it.
                    None => false,
                    Some(raw) => {
                        let mut current: TraktTokens =
                            serde_json::from_slice(&raw).map_err(|e| {
                                AppError::Task(format!("corrupt TraktTokens for {slug}: {e}"))
                            })?;
                        if current.refresh != expected_refresh {
                            // A concurrent refresh rotated the single-use token — that write wins.
                            false
                        } else {
                            mutate(&mut current);
                            let bytes = serde_json::to_vec(&current).map_err(|e| {
                                AppError::Task(format!("re-serialise TraktTokens for {slug}: {e}"))
                            })?;
                            table
                                .insert(slug.as_str(), bytes.as_slice())
                                .map_err(|e| AppError::Db(e.into()))?;
                            true
                        }
                    }
                }
            };
            txn.commit().map_err(|e| AppError::Db(e.into()))?;
            Ok(applied)
        })
        .await;
        match result {
            Ok(inner) => inner,
            Err(e) => {
                error!("redb blocking write task did not complete: {:?}", e);
                Err(AppError::Task(format!(
                    "redb blocking task did not complete: {e}"
                )))
            }
        }
    }

    pub async fn get_trakt_tokens(&self, slug: String) -> Option<TraktTokens> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = Self::read_or_log("get_trakt_tokens begin_read", db.begin_read())?;
            let table = Self::read_or_log(
                "get_trakt_tokens open_table",
                txn.open_table(TRAKT_TOKENS_TABLE),
            )?;
            let e = Self::read_or_log("get_trakt_tokens get", table.get(slug.as_str()))??;
            Self::read_or_log(
                "get_trakt_tokens deserialise",
                serde_json::from_slice::<TraktTokens>(e.value()),
            )
        })
        .await
        .unwrap_or_else(|e| {
            error!("get_trakt_tokens: blocking read task failed: {e}");
            None
        })
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
                        for entry in iter {
                            // Authoritative table: log a per-row read error rather than `.flatten()`
                            // silently dropping it (a lost token row = an account that stops syncing).
                            let (k, v) = match entry {
                                Ok(kv) => kv,
                                Err(e) => {
                                    error!("trakt_tokens row read failed (skipped): {}", e);
                                    continue;
                                }
                            };
                            match serde_json::from_slice::<TraktTokens>(v.value()) {
                                Ok(tokens) => out.push((k.value().to_string(), tokens)),
                                Err(e) => error!(
                                    "trakt_tokens row {} failed to deserialise (skipped): {}",
                                    k.value(),
                                    e
                                ),
                            }
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_else(|e| {
            // Mirror `load_all_matches`: log a blocking-read JoinError rather than silently
            // returning an empty set. These accessors read authoritative, non-regenerable tables
            // (owned/wanted/selection/trakt_tokens/blacklist); a swallowed failure reads
            // downstream as "empty" with zero diagnostic and can drive mass re-acquire / wrong
            // lifecycle decisions.
            error!("store: blocking read task failed: {e:?}");
            Default::default()
        })
    }

    // ── wanted accessors ──────────────────────────────────────────────────────

    pub async fn put_wanted(&self, rec: WantedRecord) -> Result<(), AppError> {
        // Serialise before opening the transaction to avoid partial writes on serde failure.
        let bytes = match serde_json::to_vec(&rec) {
            Ok(b) => b,
            Err(e) => {
                // Nothing written on a serialise failure → Err, not a phantom Ok (see `flatten_join`).
                error!(
                    "Failed to serialise WantedRecord for {}|{}: {}",
                    rec.user, rec.tmdb_id, e
                );
                return Err(AppError::Task(format!(
                    "serialise WantedRecord for {}|{}: {e}",
                    rec.user, rec.tmdb_id
                )));
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

    /// Single-key `wanted` lookup. Test-only: production reads the whole set via `all_wanted`.
    #[cfg(test)]
    pub async fn get_wanted(
        &self,
        user: String,
        media_type: crate::vfs::MediaType,
        tmdb_id: u64,
    ) -> Option<WantedRecord> {
        let key = wanted_key(&user, &media_type, tmdb_id);
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = Self::read_or_log("get_wanted begin_read", db.begin_read())?;
            let table = Self::read_or_log("get_wanted open_table", txn.open_table(WANTED_TABLE))?;
            let e = Self::read_or_log("get_wanted get", table.get(key.as_str()))??;
            Self::read_or_log(
                "get_wanted deserialise",
                serde_json::from_slice::<WantedRecord>(e.value()),
            )
        })
        .await
        .unwrap_or_else(|e| {
            error!("get_wanted: blocking read task failed: {e}");
            None
        })
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
                        for entry in iter {
                            // Authoritative table: log a per-row read error (a swallowed wanted row
                            // would silently drop a user's desired title from the reconcile set).
                            let (k, v) = match entry {
                                Ok(kv) => kv,
                                Err(e) => {
                                    error!("wanted row read failed (skipped): {}", e);
                                    continue;
                                }
                            };
                            match serde_json::from_slice::<WantedRecord>(v.value()) {
                                Ok(rec) => out.push(rec),
                                Err(e) => error!(
                                    "wanted row {} failed to deserialise (skipped): {}",
                                    k.value(),
                                    e
                                ),
                            }
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_else(|e| {
            // Mirror `load_all_matches`: log a blocking-read JoinError rather than silently
            // returning an empty set. These accessors read authoritative, non-regenerable tables
            // (owned/wanted/selection/trakt_tokens/blacklist); a swallowed failure reads
            // downstream as "empty" with zero diagnostic and can drive mass re-acquire / wrong
            // lifecycle decisions.
            error!("store: blocking read task failed: {e:?}");
            Default::default()
        })
    }

    // ── selection accessors (SP3) ─────────────────────────────────────────────

    pub async fn put_selection(&self, slot: String, entry: SelectionEntry) -> Result<(), AppError> {
        let bytes = match serde_json::to_vec(&entry) {
            Ok(b) => b,
            Err(e) => {
                // Nothing written on a serialise failure → Err, not a phantom Ok (see `flatten_join`).
                error!("Failed to serialise SelectionEntry for {}: {}", slot, e);
                return Err(AppError::Task(format!(
                    "serialise SelectionEntry for {slot}: {e}"
                )));
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

    /// Single-slot `selection` point lookup. Used by `acquire::record_verified` to gate a
    /// season-pack's episode-slot overwrite on quality (don't clobber a better single), and by the
    /// tests. The VFS build path reads the whole map at once via `all_selection`.
    pub async fn get_selection(&self, slot: String) -> Option<SelectionEntry> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = Self::read_or_log("get_selection begin_read", db.begin_read())?;
            let table =
                Self::read_or_log("get_selection open_table", txn.open_table(SELECTION_TABLE))?;
            let e = Self::read_or_log("get_selection get", table.get(slot.as_str()))??;
            Self::read_or_log(
                "get_selection deserialise",
                serde_json::from_slice::<SelectionEntry>(e.value()),
            )
        })
        .await
        .unwrap_or_else(|e| {
            error!("get_selection: blocking read task failed: {e}");
            None
        })
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
                        for entry in iter {
                            // Authoritative table: log a per-row read error (a swallowed selection row
                            // would silently revert that slot to the largest-bytes VFS fallback).
                            let (k, v) = match entry {
                                Ok(kv) => kv,
                                Err(e) => {
                                    error!("selection row read failed (skipped): {}", e);
                                    continue;
                                }
                            };
                            match serde_json::from_slice::<SelectionEntry>(v.value()) {
                                Ok(rec) => out.push((k.value().to_string(), rec)),
                                Err(e) => error!(
                                    "selection row {} failed to deserialise (skipped): {}",
                                    k.value(),
                                    e
                                ),
                            }
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_else(|e| {
            // Mirror `load_all_matches`: log a blocking-read JoinError rather than silently
            // returning an empty set. These accessors read authoritative, non-regenerable tables
            // (owned/wanted/selection/trakt_tokens/blacklist); a swallowed failure reads
            // downstream as "empty" with zero diagnostic and can drive mass re-acquire / wrong
            // lifecycle decisions.
            error!("store: blocking read task failed: {e:?}");
            Default::default()
        })
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

    /// Prune `upgrade_checks` cursor rows whose `(media_type, tmdb_id)` is no longer owned. The table
    /// is otherwise append-only — `set_upgrade_checked` only ever upserts, and nothing removes a row
    /// when a title leaves the library (Trigger-A/B removal, dedup deletion, downgrade prune) — so
    /// like the append-only `blacklist` it needs a bound over a deployment's lifetime. Called ~daily
    /// from the scan loop. The keep-set is derived from the current owned set exactly as the upgrade
    /// job stamps the cursor (`upgrade_check_key(media_type, tmdb_id)`). Returns rows removed.
    pub async fn prune_upgrade_checks_not_owned(&self) -> usize {
        let db = self.db.clone();
        // Build the keep-set reading ONLY request.kind + request.tmdb_id from each owned row (via a
        // minimal serde view), not the full OwnedRecord — this runs ~daily over the whole owned
        // table, whose provenance/provides/quality blobs are irrelevant to the cursor key.
        let keep: std::collections::HashSet<String> = {
            let db = db.clone();
            tokio::task::spawn_blocking(move || {
                #[derive(serde::Deserialize)]
                struct OwnedKeyReq {
                    kind: crate::scraper::MediaKind,
                    tmdb_id: u64,
                }
                #[derive(serde::Deserialize)]
                struct OwnedKeyView {
                    request: OwnedKeyReq,
                }
                let mut keep = std::collections::HashSet::new();
                if let Ok(txn) = db.begin_read() {
                    if let Ok(table) = txn.open_table(OWNED_TABLE) {
                        if let Ok(iter) = table.iter() {
                            for entry in iter {
                                let v = match entry {
                                    Ok((_, v)) => v,
                                    // Log a row-read error rather than silently dropping the row from
                                    // the keep-set (matching the `all_*` convention): a corrupt-but-
                                    // still-owned row omitted here would prune that title's upgrade
                                    // cursor with no diagnostic.
                                    Err(e) => {
                                        error!("prune_upgrade_checks: owned row read failed: {e}");
                                        continue;
                                    }
                                };
                                match serde_json::from_slice::<OwnedKeyView>(v.value()) {
                                    Ok(view) => {
                                        let media_type = media_kind_to_type(view.request.kind);
                                        keep.insert(upgrade_check_key(
                                            &media_type,
                                            view.request.tmdb_id,
                                        ));
                                    }
                                    Err(e) => error!(
                                        "prune_upgrade_checks: owned row deserialise failed: {e}"
                                    ),
                                }
                            }
                        }
                    }
                }
                keep
            })
            .await
            .unwrap_or_else(|e| {
                // A swallowed JoinError here yields an EMPTY keep-set, which would prune every
                // title's upgrade cursor. Log it rather than doing that silently.
                error!("prune_upgrade_checks: owned-keep blocking read task failed: {e:?}");
                Default::default()
            })
        };
        tokio::task::spawn_blocking(move || -> usize {
            // As in `prune_blacklist_before`: surface a begin_write/open_table/iter/commit failure
            // via `warn!` rather than a silent 0, so a persistent write failure that stalls pruning
            // of this otherwise append-only cursor table is visible instead of reading as "nothing
            // stale".
            let txn = match db.begin_write() {
                Ok(t) => t,
                Err(e) => {
                    warn!("upgrade_checks prune skipped: begin_write failed: {}", e);
                    return 0;
                }
            };
            let mut removed = 0usize;
            {
                let mut table = match txn.open_table(UPGRADE_CHECKS_TABLE) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("upgrade_checks prune skipped: open_table failed: {}", e);
                        return 0;
                    }
                };
                // Collect keys not in the owned keep-set first (can't remove during iteration). Log a
                // per-row read error rather than `.flatten()`-swallowing it (matching the `all_*`
                // accessors' convention).
                let stale: Vec<String> = match table.iter() {
                    Ok(iter) => iter
                        .filter_map(|entry| {
                            let (k, _) = match entry {
                                Ok(kv) => kv,
                                Err(e) => {
                                    error!(
                                        "upgrade_checks row read failed (skipped in prune): {}",
                                        e
                                    );
                                    return None;
                                }
                            };
                            let key = k.value().to_string();
                            (!keep.contains(&key)).then_some(key)
                        })
                        .collect(),
                    Err(e) => {
                        warn!("upgrade_checks prune skipped: table iter failed: {}", e);
                        return 0;
                    }
                };
                for k in &stale {
                    if table.remove(k.as_str()).is_ok() {
                        removed += 1;
                    }
                }
            }
            // A failed commit rolls back the removals — report 0 (nothing persisted).
            if let Err(e) = txn.commit() {
                warn!(
                    "upgrade_checks prune commit failed (removals rolled back): {}",
                    e
                );
                return 0;
            }
            removed
        })
        .await
        .unwrap_or(0)
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

    #[tokio::test]
    async fn open_recovers_twice_when_corrupt_backup_already_exists() {
        // A SECOND recovery when `<path>.corrupt` already exists must rename to `<path>.corrupt.1`
        // (never clobbering the earlier backup) and still yield a working DB — the whole reason
        // `unique_backup_path` exists (Windows can't rename onto an existing file). Exercises
        // `move_aside_and_create`'s numbered-fallback branch end-to-end.
        let tmp = TempDb::new("corrupt_twice");
        let corrupt = tmp.corrupt_path();
        let corrupt1 = format!("{}.corrupt.1", tmp.path);
        let _ = std::fs::remove_file(&corrupt1);
        std::fs::write(&tmp.path, b"not a redb file").unwrap();
        std::fs::write(&corrupt, b"an earlier backup").unwrap();
        let store = Store::open(&tmp.path).expect("must recover a second time, not error");
        assert!(
            std::path::Path::new(&corrupt1).exists(),
            "the new corrupt file must move aside to .corrupt.1, not clobber the existing .corrupt"
        );
        assert_eq!(
            std::fs::read(&corrupt).unwrap(),
            b"an earlier backup",
            "the earlier .corrupt backup is preserved untouched"
        );
        store
            .put_matches(vec![("a".to_string(), info("a"), movie("A"))])
            .await
            .unwrap();
        assert!(store.get_match("a".to_string()).await.is_some());
        let _ = std::fs::remove_file(&corrupt1);
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
        // An INTACT database in an older redb on-disk format (`UpgradeRequired`) must NOT be moved
        // aside — its authoritative tables (owned/wanted/trakt_tokens/selection) are not regenerable,
        // so we fail startup and let the operator back up / migrate rather than silently wipe them.
        assert!(matches!(
            db_open_failure(redb::DatabaseError::UpgradeRequired(1)),
            OpenFailure::Transient(_)
        ));
    }

    #[test]
    fn unique_backup_path_avoids_an_existing_corrupt_file() {
        let mut base = std::env::temp_dir();
        base.push(format!("dmm_backup_path_test_{}", std::process::id()));
        let path = base.to_str().unwrap().to_string();
        let corrupt = format!("{path}.corrupt");
        let corrupt1 = format!("{path}.corrupt.1");
        let _ = std::fs::remove_file(&corrupt);
        let _ = std::fs::remove_file(&corrupt1);
        // No existing `.corrupt` → the plain name.
        assert_eq!(unique_backup_path(&path), Some(corrupt.clone()));
        // An existing `.corrupt` → a numbered variant, so the rename never fails on / clobbers it.
        std::fs::write(&corrupt, b"x").unwrap();
        assert_eq!(unique_backup_path(&path), Some(corrupt1.clone()));
        std::fs::write(&corrupt1, b"y").unwrap();
        assert_eq!(unique_backup_path(&path), Some(format!("{path}.corrupt.2")));
        let _ = std::fs::remove_file(&corrupt);
        let _ = std::fs::remove_file(&corrupt1);
    }

    #[test]
    fn io_kind_is_corruption_matches_only_truncation_and_bad_data() {
        // A truncated DB (UnexpectedEof) and a bad header (InvalidData) are corruption signals → move
        // aside. An operational I/O error (permission, would-block, …) must NOT be — discarding a
        // healthy-but-temporarily-unreadable DB would lose authoritative data.
        assert!(io_kind_is_corruption(std::io::ErrorKind::UnexpectedEof));
        assert!(io_kind_is_corruption(std::io::ErrorKind::InvalidData));
        assert!(!io_kind_is_corruption(std::io::ErrorKind::PermissionDenied));
        assert!(!io_kind_is_corruption(std::io::ErrorKind::WouldBlock));
        assert!(!io_kind_is_corruption(std::io::ErrorKind::NotFound));
    }

    #[test]
    fn db_open_failure_recovers_only_on_genuine_corruption() {
        // A non-redb / damaged file (redb returns Corrupted "Invalid magic number") or an aborted
        // repair are the only signals that justify moving the file aside. NB `UpgradeRequired` is
        // NOT here — it is an intact old-format DB (see the transient test above).
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
    async fn open_refuses_a_newer_version_without_discarding_it() {
        // A DB written by a NEWER build of this app (schema ahead of the binary, e.g. after a
        // rollback) is intact and holds non-regenerable authoritative data. `open` must FAIL STARTUP
        // and leave the file in place — NOT move it aside and recreate an empty DB (which would
        // silently lose owned/wanted/trakt_tokens/selection), mirroring the `UpgradeRequired` path.
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
        let result = Store::open(&tmp.path);
        assert!(
            matches!(result, Err(AppError::Config(_))),
            "a newer-schema DB must fail startup as a transient/incompatible failure"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "a newer-than-binary DB must NOT be moved aside (its authoritative data is not regenerable)"
        );
        assert!(
            std::path::Path::new(&tmp.path).exists(),
            "the intact DB must be preserved for the operator to migrate/restore"
        );
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
    async fn put_owned_if_absent_writes_only_when_absent_and_never_clobbers() {
        let store = mem_store();
        let engine_rec = OwnedRecord {
            request: req("tt1", 27205),
            provenance: Provenance::watchlist("alice"), // provenance-bearing (engine) record
            added_at: 1,
            status: OwnedStatus::Pending,
            provides: vec![],
            quality: None,
        };
        // Absent → writes, returns true.
        assert!(store
            .put_owned_if_absent("h1".into(), engine_rec.clone())
            .await
            .unwrap());
        // Present → does NOT write, returns false, and the existing engine record is preserved.
        let mirror_rec = OwnedRecord {
            provenance: Provenance { entries: vec![] }, // empty-provenance mirror
            ..engine_rec.clone()
        };
        assert!(!store
            .put_owned_if_absent("h1".into(), mirror_rec)
            .await
            .unwrap());
        let kept = store.get_owned("h1".into()).await.unwrap();
        assert!(
            !kept.provenance.entries.is_empty(),
            "the existing engine record's provenance must NOT be clobbered by the mirror write"
        );
    }

    #[tokio::test]
    async fn owned_accessors_are_case_insensitive_on_the_hash_key() {
        // The owned table is keyed case-insensitively: a mixed-case write is found by a lowercase
        // read, `put_owned_if_absent` dedups across case (so one physical torrent can't produce two
        // records), and remove works regardless of case. Guards the invariant against a future
        // non-lowercasing hash source.
        let store = mem_store();
        let rec = OwnedRecord {
            request: req("tt1", 27205),
            provenance: Provenance::watchlist("alice"),
            added_at: 1,
            status: OwnedStatus::Pending,
            provides: vec![],
            quality: None,
        };
        store.put_owned("AbCdEf".into(), rec.clone()).await.unwrap();
        assert!(
            store.get_owned("abcdef".into()).await.is_some(),
            "a lowercase read must find a mixed-case write"
        );
        // A differently-cased if-absent write must see the existing record (dedup), returning false.
        assert!(
            !store
                .put_owned_if_absent("ABCDEF".into(), rec)
                .await
                .unwrap(),
            "put_owned_if_absent must treat a differently-cased hash as already present"
        );
        assert_eq!(
            store.all_owned().await.len(),
            1,
            "no duplicate record across case"
        );
        store.remove_owned("aBcDeF".into()).await.unwrap();
        assert!(store.get_owned("abcdef".into()).await.is_none());
    }

    #[tokio::test]
    async fn set_owned_status_on_missing_hash_is_a_noop_not_a_phantom_record() {
        // A status change for a hash with no owned record must be a silent no-op (Ok), NOT create a
        // phantom record — a phantom owned row is an invisible/duplicate torrent in the lifecycle.
        let store = mem_store();
        store
            .set_owned_status("ghost".to_string(), OwnedStatus::Verified)
            .await
            .expect("a missing-hash status change must return Ok, not error");
        assert!(store.get_owned("ghost".to_string()).await.is_none());
        assert!(
            store.all_owned().await.is_empty(),
            "no phantom owned record may be created"
        );
    }

    #[tokio::test]
    async fn set_owned_status_on_a_corrupt_record_returns_err_not_phantom_ok() {
        // A status change against an existing-but-undeserialisable record must surface an Err (so the
        // caller knows the change did not land), NOT commit-and-return-Ok while the record silently
        // keeps its old status.
        let store = mem_store();
        {
            let txn = store.db.begin_write().unwrap();
            {
                let mut t = txn.open_table(OWNED_TABLE).unwrap();
                t.insert("bad", b"not a valid OwnedRecord".as_slice())
                    .unwrap();
            }
            txn.commit().unwrap();
        }
        let res = store
            .set_owned_status("bad".to_string(), OwnedStatus::Verified)
            .await;
        assert!(
            matches!(res, Err(AppError::Task(_))),
            "a corrupt owned record must yield Err, got: {res:?}"
        );
    }

    #[tokio::test]
    async fn all_owned_skips_a_corrupt_row_and_keeps_good_rows() {
        // owned_hashes is authoritative + non-regenerable: one un-deserialisable row must NOT abort
        // iteration (that would make the WHOLE library invisible → mass duplicate re-acquire). It is
        // skipped; the good rows survive.
        let store = mem_store();
        // Inject a garbage row directly into the owned table.
        {
            let txn = store.db.begin_write().unwrap();
            {
                let mut t = txn.open_table(OWNED_TABLE).unwrap();
                t.insert("bad", b"this is not a valid OwnedRecord".as_slice())
                    .unwrap();
            }
            txn.commit().unwrap();
        }
        store
            .put_owned(
                "good".to_string(),
                OwnedRecord {
                    request: req("tt1", 27205),
                    provenance: Provenance::manual(),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                },
            )
            .await
            .unwrap();
        let all = store.all_owned().await;
        assert_eq!(
            all.len(),
            1,
            "the corrupt row must be skipped, the good kept"
        );
        assert_eq!(all[0].0, "good");
    }

    #[tokio::test]
    async fn prune_blacklist_before_is_strict_at_the_cutoff_boundary() {
        // The TTL prune uses a STRICT `<`: a row stamped exactly AT the cutoff is NOT pruned (it
        // becomes retryable one second later). Guards an off-by-one (`<` → `<=`) that would change
        // when a rejected hash becomes re-acquirable.
        let store = mem_store();
        let m = MediaKind::Movie;
        let cutoff = 1_000u64;
        store
            .blacklist_add(m, 1, "before".into(), "WrongTitle", cutoff - 1)
            .await
            .unwrap();
        store
            .blacklist_add(m, 1, "at".into(), "WrongTitle", cutoff)
            .await
            .unwrap();
        store
            .blacklist_add(m, 1, "after".into(), "WrongTitle", cutoff + 1)
            .await
            .unwrap();
        let removed = store.prune_blacklist_before(cutoff).await;
        assert_eq!(
            removed, 1,
            "only the row strictly before the cutoff is pruned"
        );
        assert!(!store.is_blacklisted(m, 1, "before".into()).await);
        assert!(
            store.is_blacklisted(m, 1, "at".into()).await,
            "a row stamped exactly AT the cutoff must survive (strict <)"
        );
        assert!(store.is_blacklisted(m, 1, "after".into()).await);
    }

    #[tokio::test]
    async fn prune_blacklist_before_treats_an_at_less_row_as_age_zero() {
        // The prune's `.unwrap_or(0)` means a malformed / `at`-less row is treated as age 0 and
        // pruned (rather than lingering forever). Insert a raw row lacking `at` and confirm it's
        // removed by any cutoff.
        let store = mem_store();
        let key = blacklist_key(MediaKind::Movie, 1, "h");
        {
            let txn = store.db.begin_write().unwrap();
            {
                let mut t = txn.open_table(BLACKLIST_TABLE).unwrap();
                let bad = serde_json::to_vec(&serde_json::json!({"reason": "x"})).unwrap(); // no "at"
                t.insert(key.as_str(), bad.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }
        assert!(
            store.is_blacklisted(MediaKind::Movie, 1, "h".into()).await,
            "the raw row is present before pruning"
        );
        let removed = store.prune_blacklist_before(1).await;
        assert_eq!(removed, 1, "an at-less row is treated as age 0 and pruned");
        assert!(!store.is_blacklisted(MediaKind::Movie, 1, "h".into()).await);
    }

    #[tokio::test]
    async fn open_refuses_to_discard_an_intact_db_on_a_transient_lock_error() {
        // A transient open failure (e.g. a second instance holding the lock → DatabaseAlreadyOpen)
        // must FAIL startup, NOT move the (intact) DB aside. Regression guard for the data-loss path:
        // discarding a locked-but-fine database would lose authoritative owned/wanted/token data.
        let tmp = TempDb::new("transient_open");
        let store1 = Store::open(&tmp.path).expect("first open succeeds");
        store1
            .put_matches(vec![("a".to_string(), info("a"), movie("A"))])
            .await
            .unwrap();
        // Second open while store1 still holds the lock → transient → Err, file NOT moved aside.
        let second = Store::open(&tmp.path);
        assert!(
            matches!(second, Err(AppError::Config(_))),
            "a transient open failure must surface as Err, not recover"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "an intact-but-locked DB must NOT be moved aside"
        );
        assert!(
            store1.get_match("a".to_string()).await.is_some(),
            "the original DB is intact and still readable"
        );
    }

    #[test]
    fn from_database_refuses_a_newer_than_supported_schema() {
        // `from_database` (unlike `open`, which move-aside-recovers) must REFUSE a schema newer than
        // this binary supports rather than run against it — opposite behaviours, both must be pinned.
        let db = Database::builder()
            .create_with_backend(InMemoryBackend::new())
            .unwrap();
        {
            let txn = db.begin_write().unwrap();
            {
                let mut m = txn.open_table(META_TABLE).unwrap();
                m.insert("schema_version", &999u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let r = Store::from_database(Arc::new(db));
        assert!(
            matches!(r, Err(AppError::Config(_))),
            "a newer-than-supported schema must be refused by from_database"
        );
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
        // tmdb 13 sorts AFTER the "m|12|" block, so it exercises the range scan's break-on-first-
        // non-matching-key path (its rows must not leak in, and must not truncate 12's rows).
        store
            .blacklist_add(m, 13, "ddd".into(), "WrongTitle", 4)
            .await
            .unwrap();
        let set = store.blacklisted_hashes_for(m, 12).await;
        assert_eq!(set.len(), 2);
        assert!(set.contains("aaa")); // lowercased
        assert!(set.contains("bbb"));
        assert!(!set.contains("ccc")); // belongs to tmdb 123, not 12
        assert!(!set.contains("ddd")); // belongs to tmdb 13, not 12
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

    #[tokio::test]
    async fn update_trakt_tokens_cas_applies_only_on_matching_refresh() {
        let store = mem_store();
        store
            .put_trakt_tokens(
                "alice".to_string(),
                trakt_tokens_fixture("access1", "alice"),
            )
            .await
            .unwrap();
        // Matching expected refresh → applied; the mutation lands.
        let applied = store
            .update_trakt_tokens_if_refresh_matches(
                "alice".to_string(),
                "refresh_tok".to_string(),
                |t| {
                    t.access = "NEW".to_string();
                    t.refresh = "ROTATED".to_string();
                    t.needs_reenrolment = false;
                },
            )
            .await
            .unwrap();
        assert!(applied, "a matching refresh must apply the mutation");
        let got = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert_eq!(got.access, "NEW");
        assert_eq!(got.refresh, "ROTATED");

        // A stale expected refresh (simulating a token a concurrent refresh already rotated) → skipped;
        // the CAS must NOT clobber the fresh record with the stale flag/token.
        let applied = store
            .update_trakt_tokens_if_refresh_matches(
                "alice".to_string(),
                "refresh_tok".to_string(), // the OLD (already-rotated) token
                |t| {
                    t.refresh = "refresh_tok".to_string();
                    t.needs_reenrolment = true;
                },
            )
            .await
            .unwrap();
        assert!(!applied, "a since-rotated refresh must be skipped");
        let got = store.get_trakt_tokens("alice".to_string()).await.unwrap();
        assert_eq!(got.refresh, "ROTATED", "the fresh token must be preserved");
        assert!(
            !got.needs_reenrolment,
            "a stale CAS must not set a spurious needs_reenrolment"
        );

        // A removed account → skipped (never resurrected).
        store
            .remove_trakt_tokens("alice".to_string())
            .await
            .unwrap();
        let applied = store
            .update_trakt_tokens_if_refresh_matches(
                "alice".to_string(),
                "ROTATED".to_string(),
                |t| t.needs_reenrolment = true,
            )
            .await
            .unwrap();
        assert!(!applied, "a removed account must not be resurrected");
        assert!(store.get_trakt_tokens("alice".to_string()).await.is_none());
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

    /// The manual `Debug` impl for `TraktTokens` must redact the OAuth `access`/`refresh` tokens —
    /// a stray `debug!("{tokens:?}")` must never leak credentials. Guards against a future edit
    /// (e.g. re-adding `#[derive(Debug)]`) silently re-exposing them. Mirrors config.rs's
    /// `debug_redacts_secrets`.
    #[test]
    fn trakt_tokens_debug_redacts_secrets() {
        let tok = TraktTokens {
            access: "SECRET_ACCESS_XYZ".to_string(),
            refresh: "SECRET_REFRESH_XYZ".to_string(),
            expires_at: 1_234_567_890,
            username: "alice".to_string(),
            needs_reenrolment: false,
        };
        let dbg = format!("{tok:?}");
        assert!(
            !dbg.contains("SECRET_ACCESS_XYZ"),
            "access token leaked: {dbg}"
        );
        assert!(
            !dbg.contains("SECRET_REFRESH_XYZ"),
            "refresh token leaked: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "expected redaction marker: {dbg}"
        );
        // Non-secret fields stay visible.
        assert!(dbg.contains("alice"), "username should be visible: {dbg}");
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
    async fn run_migrations_is_idempotent_on_reentry() {
        // The migration ladder commits BEFORE the version stamp, so a crash between the two re-runs
        // run_migrations on the next boot — every step MUST be idempotent. Prove a double (indeed
        // triple, via Store::open) run from a pre-current version is a clean no-op rather than pinning
        // that invariant only in a comment: no error, and owned rows survive intact.
        let tmp = TempDb::new("migrate_reentry");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let odef: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
                let mut ot = txn.open_table(odef).unwrap();
                let rec = OwnedRecord {
                    request: req("tt9", 999),
                    provenance: Provenance::manual(),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                };
                ot.insert("hash9", serde_json::to_vec(&rec).unwrap().as_slice())
                    .unwrap();
                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                txn.open_table(vdef)
                    .unwrap()
                    .insert("schema_version", &3u64)
                    .unwrap();
            }
            txn.commit().unwrap();
            // Run the ladder from v3 TWICE against the same DB — the second must be a clean no-op.
            Store::run_migrations(&db, 3).expect("first migration run");
            Store::run_migrations(&db, 3).expect("re-run must be idempotent, not error");
        } // drop the Database handle so Store::open can take the file lock
        let store = Store::open(&tmp.path).unwrap();
        assert_eq!(
            store.get_owned("hash9".to_string()).await.unwrap().status,
            OwnedStatus::Verified,
            "owned rows must survive repeated migration runs"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "a re-migrated valid DB must not be moved aside"
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
    async fn migrates_v4_db_clears_legacy_upgrade_checks_through_the_ladder() {
        // The `from_version < 6` upgrade_checks clear must fire for a from_version BELOW 5, not only
        // for the v5→v6 step — v4 is where upgrade_checks was introduced, so it's the version most
        // likely to hold legacy bare-key rows. Mirrors the sibling
        // migrates_v3_db_clears_legacy_blacklist_through_the_ladder.
        let tmp = TempDb::new("migrate_v4_ladder_uc");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                // Legacy bare-`tmdb_id` upgrade_checks row (pre-v6 format).
                let udef: TableDefinition<&str, u64> = TableDefinition::new("upgrade_checks");
                let mut ut = txn.open_table(udef).unwrap();
                ut.insert("1396", &1_700_000_000u64).unwrap();

                // An owned row must SURVIVE (authoritative, not regenerable).
                let odef: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
                let mut ot = txn.open_table(odef).unwrap();
                let rec = OwnedRecord {
                    request: req("tt4", 1396),
                    provenance: Provenance::manual(),
                    added_at: 9,
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
        assert_eq!(
            store.get_owned("hash4".to_string()).await.unwrap().status,
            OwnedStatus::Verified,
            "owned rows must survive the v4 ladder migration"
        );
        drop(store);
        let db = Database::create(&tmp.path).unwrap();
        let txn = db.begin_read().unwrap();
        let udef: TableDefinition<&str, u64> = TableDefinition::new("upgrade_checks");
        let ut = txn.open_table(udef).unwrap();
        assert_eq!(
            ut.iter().unwrap().count(),
            0,
            "the legacy bare-key upgrade_checks row must be cleared for a v4 (< 6) from_version"
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
        // The migration must STAMP the current schema version. Without this a migration that moved
        // data correctly but stamped the wrong version would silently re-migrate on every boot.
        assert_eq!(
            Store::read_version(&db).unwrap(),
            SCHEMA_VERSION,
            "migration must stamp the current schema version"
        );
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
    async fn migrates_v3_db_clears_legacy_blacklist_through_the_ladder() {
        // The `< 7` blacklist clear must fire for a LOWER from_version too (not just v6→v7): a v3 DB
        // carrying an old bare-key blacklist row has it cleared through the full migration ladder,
        // while its authoritative owned row survives.
        let tmp = TempDb::new("migrate_v3_blacklist");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let bdef: TableDefinition<&str, &[u8]> = TableDefinition::new("blacklist");
                let mut bt = txn.open_table(bdef).unwrap();
                bt.insert(
                    "1396|deadbeef",
                    serde_json::to_vec(&serde_json::json!({"reason":"WrongTitle","at":1}))
                        .unwrap()
                        .as_slice(),
                )
                .unwrap();

                let odef: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
                let mut ot = txn.open_table(odef).unwrap();
                let rec = OwnedRecord {
                    request: req("tt3", 1396),
                    provenance: Provenance::manual(),
                    added_at: 3,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                };
                ot.insert("hash3b", serde_json::to_vec(&rec).unwrap().as_slice())
                    .unwrap();

                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &3u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();
        assert_eq!(
            store.get_owned("hash3b".to_string()).await.unwrap().status,
            OwnedStatus::Verified,
            "owned rows must survive the v3→v7 migration"
        );
        drop(store);
        let db = Database::create(&tmp.path).unwrap();
        assert_eq!(Store::read_version(&db).unwrap(), SCHEMA_VERSION);
        let txn = db.begin_read().unwrap();
        let bdef: TableDefinition<&str, &[u8]> = TableDefinition::new("blacklist");
        let bt = txn.open_table(bdef).unwrap();
        assert_eq!(
            bt.iter().unwrap().count(),
            0,
            "a legacy blacklist row must be cleared for a pre-v6 from_version too"
        );
    }

    #[tokio::test]
    async fn migrates_v5_db_preserves_new_format_wanted_rows() {
        // Guards the `wanted` clear range (`(3..5)`): a NEW-format wanted row (the v5+ key, with a
        // media-type discriminator) in a v5 DB must SURVIVE migration to current — wanted holds the
        // user's desired-state and is NOT regenerable within the same tick. If someone accidentally
        // broadened the clear to `(3..6)`/`(3..7)`, this row would be wiped and the test would fail.
        let tmp = TempDb::new("migrate_v5_keeps_wanted");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                // New-format key (user|m|tmdb), exactly as `put_wanted` would write.
                let wdef: TableDefinition<&str, &[u8]> = TableDefinition::new("wanted");
                let mut wt = txn.open_table(wdef).unwrap();
                let rec = movie_wanted("alice", 777);
                let key = wanted_key("alice", &MediaType::Movie, 777);
                wt.insert(key.as_str(), serde_json::to_vec(&rec).unwrap().as_slice())
                    .unwrap();

                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &5u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();
        let all = store.all_wanted().await;
        assert!(
            all.iter().any(|r| r.user == "alice" && r.tmdb_id == 777),
            "a new-format wanted row in a v5 DB must survive migration to current, got {all:?}"
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "valid v5 DB must not be moved aside"
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
        // The full v1→current ladder must leave the schema stamped at the current version, so the
        // service doesn't re-run the whole ladder on every subsequent boot.
        drop(store);
        let db = Database::create(&tmp.path).unwrap();
        assert_eq!(
            Store::read_version(&db).unwrap(),
            SCHEMA_VERSION,
            "the full migration ladder must stamp the current schema version"
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

    #[tokio::test]
    async fn empty_provenance_round_trips_as_empty_not_manual() {
        // The mirror-vs-manual distinction is load-bearing for the removal lifecycle: an
        // account-mirror record carries an EXPLICITLY-empty provenance that must NOT read as manual.
        // `#[serde(default)]` only fires on an ABSENT key (that path → manual, covered separately),
        // so an explicitly-empty vec must round-trip as empty. A regression (e.g. a custom
        // (de)serializer) could silently flip empty→manual and wrongly protect mirror content.
        let store = mem_store();
        let rec = OwnedRecord {
            request: req("tt1", 27205),
            provenance: Provenance { entries: vec![] },
            added_at: 100,
            status: OwnedStatus::Verified,
            provides: vec![],
            quality: None,
        };
        store.put_owned("h1".to_string(), rec).await.unwrap();
        let got = store.get_owned("h1".to_string()).await.unwrap();
        assert!(
            got.provenance.entries.is_empty(),
            "an explicitly-empty provenance must round-trip as empty"
        );
        assert!(
            !got.provenance.has_manual_entry(),
            "empty provenance must NOT read as a protected manual add"
        );
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

    #[tokio::test]
    async fn prune_upgrade_checks_drops_rows_for_titles_no_longer_owned() {
        use crate::vfs::MediaType;
        let store = mem_store();
        // Cursor rows for a still-owned movie (101), a still-owned show (303), and a title that has
        // since left the library (movie 202). A show with the SAME numeric id as an owned MOVIE (101)
        // is NOT owned → its cursor must be pruned (the media-type discriminator keeps them distinct).
        store
            .set_upgrade_checked(&MediaType::Movie, 101, 1_000)
            .await
            .unwrap();
        store
            .set_upgrade_checked(&MediaType::Movie, 202, 2_000)
            .await
            .unwrap();
        store
            .set_upgrade_checked(&MediaType::Show, 303, 3_000)
            .await
            .unwrap();
        store
            .set_upgrade_checked(&MediaType::Show, 101, 4_000)
            .await
            .unwrap();

        // Own only movie 101 and show 303.
        store
            .put_owned(
                "h101".into(),
                OwnedRecord {
                    request: req("tt101", 101),
                    provenance: Provenance::manual(),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![],
                    quality: None,
                },
            )
            .await
            .unwrap();
        let mut show_req = req("tt303", 303);
        show_req.kind = MediaKind::Series;
        store
            .put_owned(
                "h303".into(),
                OwnedRecord {
                    request: show_req,
                    provenance: Provenance::manual(),
                    added_at: 1,
                    status: OwnedStatus::Verified,
                    provides: vec![(1, 1)],
                    quality: None,
                },
            )
            .await
            .unwrap();

        let removed = store.prune_upgrade_checks_not_owned().await;
        assert_eq!(removed, 2, "movie 202 and show 101 cursors are pruned");
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 101).await,
            1_000,
            "owned movie 101 cursor kept"
        );
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Show, 303).await,
            3_000,
            "owned show 303 cursor kept"
        );
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Movie, 202).await,
            0,
            "un-owned movie 202 cursor pruned"
        );
        assert_eq!(
            store.get_upgrade_checked(&MediaType::Show, 101).await,
            0,
            "un-owned show 101 cursor pruned (distinct from owned movie 101)"
        );

        // Idempotent: a second prune with the same owned set removes nothing.
        assert_eq!(store.prune_upgrade_checks_not_owned().await, 0);
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
