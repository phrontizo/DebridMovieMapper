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
/// - v7→v8: `QualitySummary` gained `unverified` (serde default `false`); the migration flags
///   `unverified = true` on every owned record whose quality is known and that some `selection` entry
///   serves from a non-MKV/MP4 file (a container the probe can never read), so the upgrade pass
///   replaces those releases with checked copies.
pub const SCHEMA_VERSION: u64 = 8;

/// The shape of every JSON-valued table: a string key mapping to serde_json bytes.
type BytesTable = TableDefinition<'static, &'static str, &'static [u8]>;

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
/// Staging intents of upgrade candidates and acquisitions being added: lowercased infohash ->
/// serde_json(StagingIntent). Rebuildable: losing it only forgets adds cut off before they were
/// checked or recorded (see [`StagingIntent`]). Added without a schema bump:
/// `ensure_schema` creates it on every open, and an older build never opens it.
const STAGING_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("staging");

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
    /// TMDB's `spoken_languages` (ISO 639-1) for the title. Under `AUDIO_LANGUAGE=original` an audio
    /// track in any of these — or in `original_language` — is acceptable (ranking + probe). `None` =
    /// never fetched (a record persisted before this field existed, or an account-mirror record): the
    /// upgrade pass backfills it once from TMDB; until then only `original_language` counts. `Some`
    /// (even empty) = fetched, never re-fetched.
    #[serde(default)]
    pub spoken_languages: Option<Vec<String>>,
    pub metadata: crate::vfs::MediaMetadata,
}

/// A torrent being added that no owned record names yet (the `staging` table value, keyed by
/// lowercased infohash): an upgrade candidate the engine is staging and has not checked yet, or an
/// acquisition `AcquisitionEngine::acquire` is adding before it records it `Pending`. Written before
/// the add and cleared once the hash is recorded as owned or its torrent is deleted. One cut off in
/// between (a shutdown, a panic, an OOM kill) leaves its intent behind: the account mirror never
/// adopts that hash, and `upgrade::sweep_staging_intents` deletes its torrent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StagingIntent {
    /// The torrent id the add returned; `None` until it has returned.
    pub torrent_id: Option<String>,
    /// When staging began (unix secs).
    pub started_at: u64,
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
    /// auto-removed by the reconciler, regardless of Trakt state. (The constructors always build a
    /// non-empty provenance. `entries` is `pub` for multi-entry construction and for the EMPTY
    /// provenance that marks every account-mirror record.)
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
    /// Quality snapshot of the chosen release, for upgrade comparison. `None` on pre-SP3 records, on
    /// account-mirror records written before quality capture or for an over-ceiling file (both
    /// re-derived by `tasks::backfill_owned_quality`), and on a mirror record whose torrent lists no
    /// video file (unknown-lowest).
    #[serde(default)]
    pub quality: Option<crate::release::QualitySummary>,
}

/// Persisted per-user Trakt OAuth tokens (the `trakt_tokens` table value; key = user slug).
/// `needs_reenrolment` marks an account whose last sync failed. It is set by `sync_trakt` on a
/// non-transient token-refresh failure, on any Trakt read failure (transient or not, including a
/// failed page of a paginated read) and on a store write failure during the sync. The accounts
/// page's refresh (`enrolment::refresh_account`) sets it on a non-transient refresh failure. A
/// transient refresh failure (429/5xx/transport/incoherent answer,
/// `trakt_client::refresh_error_is_transient`) never sets it. The next successful sync or refresh
/// clears it. Old records written before this field existed decode it as `false`.
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
    Movie {
        watched: bool,
    },
    Show {
        /// (season, episode) pairs the user has watched.
        watched_episodes: Vec<(u32, u32)>,
        /// Trakt's own per-show progress says the user is behind (an unwatched aired episode), or
        /// the sync could not ask (`show_progress` failed, no Trakt id) and assumed so: the
        /// catch-up source. It vetoes the finish verdict (`wanted::user_finished`), so a show is
        /// never finish-removed while Trakt says the user has an aired episode left, whatever
        /// TMDB's aired set says. Rows written before it existed read as behind (not finished)
        /// until the next successful sync rewrites them, so a legacy row is never judged finished
        /// on TMDB's aired set alone; every row this build writes sets it explicitly.
        #[serde(default = "legacy_trakt_behind")]
        trakt_behind: bool,
    },
}

/// The `trakt_behind` a show row written before the field existed reads as: behind, as for a show
/// the sync could not ask Trakt about.
fn legacy_trakt_behind() -> bool {
    true
}

/// One (user, media_type, tmdb_id) row of the materialised wanted-set. Self-describing: it embeds
/// its own `user`+`media_type`+`tmdb_id` (the composite table key is derived from them) so the reconciler
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
    /// to move aside and recreate. NOT lossless: the old file is kept as `<path>.corrupt`, but the
    /// recreated DB starts without the Trakt enrolments, provenance, selections and blacklist (only
    /// the `matches` cache and the `upgrade_checks` cursor regenerate, and `wanted` once the accounts
    /// are re-enrolled). Forward migrations bring an older OUR-schema DB up to date
    /// instead of discarding it.
    Corrupt(String),
    /// A transient/operational failure: another instance holds the lock, the path is not writable,
    /// disk is full, etc. — OR an INTACT database that this binary cannot safely use but must not
    /// discard: an older redb on-disk format (`UpgradeRequired`, e.g. after a redb dependency bump),
    /// a NEWER redb on-disk format (a rollback after a redb upgrade rewrote the file), or a schema
    /// written by a NEWER build of this app (version ahead of the binary, e.g. after a rollback). In
    /// every case the file is intact and holds non-regenerable authoritative data, so the caller
    /// fails startup and lets the operator fix the underlying condition (back up / migrate / roll
    /// the binary forward) instead.
    Transient(String),
}

/// `true` for an I/O error kind that means the *file* is malformed/truncated (corruption we can
/// recover from), as opposed to an operational failure (permissions, disk full, …) that must not
/// discard a possibly-intact database. redb surfaces a bad header as `Io(InvalidData)` and a file
/// truncated below its header as `Io(UnexpectedEof)`. (A longer truncation — an intact header
/// recording a layout longer than the file — makes redb panic instead; `try_open` catches that.)
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
        // An INTACT database in a NEWER redb on-disk format (a rollback after a redb upgrade
        // rewrote the file). redb has no dedicated variant for it: it reports the commit slot's
        // format version as `Corrupted("Expected file format version <= N, found M")`. Like an
        // older format (`UpgradeRequired`, below), it must fail startup and keep the file rather
        // than be moved aside with all its non-regenerable tables.
        DatabaseError::Storage(StorageError::Corrupted(msg))
            if msg.starts_with("Expected file format version") =>
        {
            false
        }
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
    if corrupt {
        return OpenFailure::Corrupt(format!("open failed: {e}"));
    }
    // What the operator should check, per cause (the newer-schema case in `try_open` carries its own).
    let hint = match &e {
        DatabaseError::DatabaseAlreadyOpen => " — check that no other instance is running",
        DatabaseError::Storage(StorageError::Io(_)) => {
            " — check that the path is writable and the disk is not full"
        }
        DatabaseError::Storage(StorageError::Corrupted(_)) => {
            " — the file was written by a newer redb on-disk format; run a newer build of this app"
        }
        DatabaseError::UpgradeRequired(_) => {
            " — the file is in an older redb on-disk format; back it up and migrate it"
        }
        _ => "",
    };
    OpenFailure::Transient(format!("open failed: {e}{hint}"))
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
    /// The staging intents THIS process opened and has not closed, each with when it was opened
    /// (monotonic). Shared by every clone, so the scan loop's staging sweep can tell this process's
    /// intents from those an earlier process left (see [`Store::local_staging_age`]) without
    /// comparing wall-clock seconds, which a clock step or a same-second start would get wrong.
    local_staging: LocalStaging,
}

type LocalStaging = Arc<std::sync::Mutex<HashMap<String, tokio::time::Instant>>>;

impl Store {
    /// Open (or create) the database at `path`, recovering automatically from a genuinely
    /// unreadable / corrupt file (bad magic, truncated, redb repair aborted) rather than failing
    /// startup: the file is moved aside to `<path>.corrupt` and a fresh, empty DB is created. A
    /// *transient/incompatible* failure — another instance holds the lock, the path is not writable,
    /// disk full, an older or NEWER redb on-disk format, OR a schema written by a NEWER build of this
    /// app (e.g. after a rollback) — leaves the file intact and fails startup instead, because
    /// discarding a possibly-intact database would lose non-regenerable authoritative data.
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
                     possibly-intact database."
                )));
            }
        };
        Ok(Store {
            db: Arc::new(db),
            local_staging: LocalStaging::default(),
        })
    }

    /// The test constructor behind [`Store::in_memory`] and the `fail_writes` helpers: a store over
    /// an already-open (in-memory or failure-injecting) database. Ensures the schema is present and
    /// current, and refuses a newer-than-supported schema the way `try_open` does. Production opens
    /// through [`Store::open`], which adds the corruption recovery this skips.
    #[cfg(test)]
    pub(crate) fn from_database(db: Arc<Database>) -> Result<Self, AppError> {
        let version = Self::read_version(&db).map_err(AppError::Db)?;
        if version > SCHEMA_VERSION {
            return Err(AppError::Config(format!(
                "database schema v{} is newer than supported v{}",
                version, SCHEMA_VERSION
            )));
        }
        Self::ensure_schema(&db, version).map_err(AppError::Db)?;
        Ok(Store {
            db,
            local_staging: LocalStaging::default(),
        })
    }

    /// A fresh, empty store on an in-memory redb backend at the current schema, for tests (so test
    /// modules outside this file need no redb types of their own).
    #[cfg(test)]
    pub(crate) fn in_memory() -> Store {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .expect("in-memory redb");
        Store::from_database(Arc::new(db)).expect("schema")
    }

    /// Open the file and bring its schema to the current version. Returns an
    /// [`OpenFailure`] classifying *why* it failed so the caller can decide between
    /// recovering (move aside) and refusing to discard an intact database.
    fn try_open(path: &str) -> Result<Database, OpenFailure> {
        // redb `assert!`s rather than returning an error on some damaged files (a header that is
        // intact but records a layout longer than the file, e.g. after an interrupted copy, or a
        // damaged page-size field). Catch that panic and classify it as corruption so `open` moves
        // the file aside like any other damaged DB instead of crash-looping at startup. The unwind
        // drops the file handle (and its lock), so the rename that follows works.
        let db = match std::panic::catch_unwind(|| Database::create(path)) {
            Ok(r) => r.map_err(db_open_failure)?,
            Err(_) => {
                return Err(OpenFailure::Corrupt(
                    "redb panicked opening the file (truncated or damaged header)".into(),
                ))
            }
        };
        let version =
            Self::read_version(&db).map_err(|e| schema_failure("schema read failed", e))?;
        if version > SCHEMA_VERSION {
            // A DB written by a NEWER build of this app (e.g. after rolling a bad deploy back to an
            // older binary) is INTACT and full of non-regenerable authoritative data
            // (owned/wanted/trakt_tokens/selection). Moving it aside would silently discard that —
            // exactly the loss the `UpgradeRequired` (older redb on-disk format) path already refuses
            // — so treat it as transient and FAIL STARTUP, letting the operator roll the binary
            // forward or restore, rather than recreate an empty DB. The test constructor
            // `from_database` applies the same refusal.
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
        // MUST therefore be idempotent (all four current steps are: the `3..5` `wanted` clear, the `<6`
        // `upgrade_checks` clear and the `<7` `blacklist` clear, each `t.retain(|_,_| false)`, and the
        // `1..8` unverified-flag backfill, which only ever sets a flag).
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
            write_txn.open_table(STAGING_TABLE)?; // create if absent
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
    /// v7→v8: `QualitySummary.unverified` is additive (`#[serde(default)]` → `false`), but records
    /// acquired before it existed may be SERVED from a file whose tracks could never be checked (an
    /// `.avi` the probe can't read). Flag every owned record with a known `quality` that ANY `selection`
    /// entry points at with a non-MKV/MP4 `file_path` (`release::Container::from_name`), in ONE write
    /// transaction. Idempotent (it only ever sets the flag). Skipped for a fresh DB (from_version 0),
    /// which has no rows.
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
        if (1..8).contains(&from_version) {
            Self::flag_unverifiable_selected_owned(db)?;
        }
        Ok(())
    }

    /// The v7→v8 step (see [`Self::run_migrations`]): set `quality.unverified` on every owned record
    /// served (by any `selection` entry) from a file the probe can't read. Undecodable rows are left
    /// untouched — the typed accessors already skip them.
    fn flag_unverifiable_selected_owned(db: &Database) -> Result<(), redb::Error> {
        let write_txn = db.begin_write()?;
        {
            // Opening creates the tables if absent (a pre-v4 DB has no `selection` yet).
            let sel = write_txn.open_table(SELECTION_TABLE)?;
            let mut unverifiable: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for row in sel.iter()? {
                let (_, v) = row?;
                if let Ok(entry) = serde_json::from_slice::<SelectionEntry>(v.value()) {
                    if !crate::release::Container::from_name(&entry.file_path).is_verifiable() {
                        unverifiable.insert(entry.hash.to_ascii_lowercase());
                    }
                }
            }
            drop(sel);
            if !unverifiable.is_empty() {
                let mut owned = write_txn.open_table(OWNED_TABLE)?;
                let mut updates: Vec<(String, Vec<u8>)> = Vec::new();
                for row in owned.iter()? {
                    let (k, v) = row?;
                    let hash = k.value().to_string();
                    if !unverifiable.contains(&hash.to_ascii_lowercase()) {
                        continue;
                    }
                    let Ok(mut rec) = serde_json::from_slice::<OwnedRecord>(v.value()) else {
                        continue;
                    };
                    match rec.quality.as_mut() {
                        Some(q) if !q.unverified => q.unverified = true,
                        _ => continue, // unknown quality (nothing to flag) or already flagged
                    }
                    if let Ok(bytes) = serde_json::to_vec(&rec) {
                        updates.push((hash, bytes));
                    }
                }
                for (hash, bytes) in &updates {
                    owned.insert(hash.as_str(), bytes.as_slice())?;
                }
                if !updates.is_empty() {
                    info!(
                        "store: v8 migration flagged {} owned release(s) served from an unverifiable container",
                        updates.len()
                    );
                }
            }
        }
        write_txn.commit()?;
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
        let r = tokio::task::spawn_blocking(move || {
            Self::scan_json::<(TorrentInfo, MediaMetadata)>(&db, MATCHES_TABLE, "matches")
                .into_iter()
                .collect()
        })
        .await;
        Self::join_or_default("load_all_matches", r)
    }

    /// Whether the identification cache holds any entry. The scan loop keeps the cache through a run
    /// of wholesale-empty provider listings and drops it only once it accepts the account as empty,
    /// so the reconciler treats an empty listing as suspect while this is `true`. A read failure is
    /// logged and reads as `true` (suspect: the reconciler then holds back rather than acting).
    pub async fn has_matches(&self) -> bool {
        let db = self.db.clone();
        let r = tokio::task::spawn_blocking(move || {
            let read = || -> Result<bool, redb::Error> {
                let txn = db.begin_read()?;
                let table = txn.open_table(MATCHES_TABLE)?;
                let any = table.first()?.is_some();
                Ok(any)
            };
            read().unwrap_or_else(|e| {
                error!("has_matches: read failed: {e}");
                true
            })
        })
        .await;
        r.unwrap_or_else(|e| {
            error!("has_matches: blocking read task failed: {e:?}");
            true
        })
    }

    /// Look up a single cached identification by torrent id.
    pub async fn get_match(&self, id: String) -> Option<(TorrentInfo, MediaMetadata)> {
        self.get_json(MATCHES_TABLE, id, "get_match").await
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

    /// Like `flatten_join`, but for a blocking closure that itself returns `Result<T, AppError>`
    /// (its inner error is already an `AppError` — e.g. a `serde` re-serialise failure — so it can't
    /// go through `flatten_join`, which maps a `redb::Error`). Collapses the `spawn_blocking`
    /// JoinError into an `AppError::Task` identically.
    fn flatten_join_app<T>(
        result: Result<Result<T, AppError>, tokio::task::JoinError>,
    ) -> Result<T, AppError> {
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

    /// Log a read error (with `ctx`) and map it to `None`, so a point-lookup accessor distinguishes a
    /// genuine miss (`Ok(None)`) from a transient begin_read/open_table/get/deserialise failure that a
    /// bare `.ok()` would silently swallow — a false "absent" on an authoritative table (owned/wanted/
    /// tokens/selection) can drive a duplicate acquire or a wrong lifecycle decision. Mirrors the
    /// per-row logging the `all_*` scans already do. Logged as `{label} {stage}: {e}`.
    fn read_or_log<T, E: std::fmt::Display>(
        label: &str,
        stage: &str,
        r: Result<T, E>,
    ) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                error!("{label} {stage}: {e}");
                None
            }
        }
    }

    /// Collapse a read accessor's `spawn_blocking` join result: a JoinError (the blocking task
    /// panicked or was cancelled) is logged under `label` and falls back to the empty/zero default
    /// rather than reading downstream as a silent "empty". Most of these accessors read
    /// authoritative, non-regenerable tables (owned/wanted/selection/trakt_tokens/blacklist), where
    /// an undiagnosed empty read can drive mass re-acquire or wrong lifecycle decisions.
    fn join_or_default<T: Default>(label: &str, r: Result<T, tokio::task::JoinError>) -> T {
        r.unwrap_or_else(|e| {
            error!("{label}: blocking read task failed: {e:?}");
            T::default()
        })
    }

    /// Visit every row of the `&str → bytes` table `def` in one read transaction. A begin_read /
    /// open_table / iter failure and every per-row read failure is logged under `label` rather
    /// than silently swallowed. Returns `false` when the table could not be read at all (no rows
    /// were visited), `true` otherwise (an unreadable row is skipped).
    fn scan_rows(
        db: &Database,
        def: BytesTable,
        label: &str,
        mut visit: impl FnMut(&str, &[u8]),
    ) -> bool {
        let txn = match db.begin_read() {
            Ok(t) => t,
            Err(e) => {
                error!("{label}: begin_read failed: {e}");
                return false;
            }
        };
        let table = match txn.open_table(def) {
            Ok(t) => t,
            Err(e) => {
                error!("{label}: open_table failed: {e}");
                return false;
            }
        };
        let iter = match table.iter() {
            Ok(i) => i,
            Err(e) => {
                error!("{label}: iter failed: {e}");
                return false;
            }
        };
        for entry in iter {
            match entry {
                Ok((k, v)) => visit(k.value(), v.value()),
                Err(e) => error!("{label} row read failed (skipped): {e}"),
            }
        }
        true
    }

    /// Every row of `def` with its key, deserialised as `T`. A row that won't deserialise is logged
    /// and skipped (see [`Self::scan_rows`] for the read failures).
    fn scan_json<T: serde::de::DeserializeOwned>(
        db: &Database,
        def: BytesTable,
        label: &str,
    ) -> Vec<(String, T)> {
        let mut out = Vec::new();
        Self::scan_rows(db, def, label, |k, v| {
            match serde_json::from_slice::<T>(v) {
                Ok(rec) => out.push((k.to_string(), rec)),
                Err(e) => error!("{label} row {k} failed to deserialise (skipped): {e}"),
            }
        });
        out
    }

    /// One read-modify-write of the JSON record at `key` in `def`, in a single write transaction.
    /// `Ok(None)` when there is no record (nothing is written, so a missing record is never
    /// resurrected). Otherwise `f` mutates the decoded record and returns whether it changed it; a
    /// changed record is re-serialised and written back, and the result is `Ok(Some(changed))`. A
    /// record that won't deserialise or re-serialise is an `Err` (`AppError::Task`, prefixed with
    /// `ctx`), never a phantom `Ok`: the transaction is dropped without committing, so the caller
    /// learns the change did not land.
    fn update_json<T: serde::Serialize + serde::de::DeserializeOwned>(
        db: &Database,
        def: BytesTable,
        key: &str,
        ctx: &str,
        f: impl FnOnce(&mut T) -> bool,
    ) -> Result<Option<bool>, AppError> {
        let txn = db.begin_write().map_err(|e| AppError::Db(e.into()))?;
        let outcome = {
            let mut table = txn.open_table(def).map_err(|e| AppError::Db(e.into()))?;
            // Copy the bytes out so the read borrow ends before the `&mut` insert.
            let existing: Option<Vec<u8>> = table
                .get(key)
                .map_err(|e| AppError::Db(e.into()))?
                .map(|guard| guard.value().to_vec());
            match existing {
                None => None,
                Some(raw) => {
                    let mut rec = serde_json::from_slice::<T>(&raw).map_err(|e| {
                        AppError::Task(format!("{ctx}: corrupt record for {key}: {e}"))
                    })?;
                    let changed = f(&mut rec);
                    if changed {
                        let bytes = serde_json::to_vec(&rec).map_err(|e| {
                            AppError::Task(format!("{ctx}: re-serialise failed for {key}: {e}"))
                        })?;
                        table
                            .insert(key, bytes.as_slice())
                            .map_err(|e| AppError::Db(e.into()))?;
                    }
                    Some(changed)
                }
            }
        };
        txn.commit().map_err(|e| AppError::Db(e.into()))?;
        Ok(outcome)
    }

    /// Upsert the JSON encoding of `value` at `key` in `def`, in one write transaction. Serialised
    /// BEFORE the transaction opens, so a serde failure writes nothing: it is logged (`what` names
    /// the record type) and returned as `AppError::Task`, never a phantom `Ok` (see `flatten_join`).
    async fn put_json<T: serde::Serialize>(
        &self,
        def: BytesTable,
        key: String,
        value: &T,
        what: &str,
    ) -> Result<(), AppError> {
        let bytes = match serde_json::to_vec(value) {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to serialise {what} for {key}: {e}");
                return Err(AppError::Task(format!("serialise {what} for {key}: {e}")));
            }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(def)?
                    .insert(key.as_str(), bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    /// Remove `key` from `def` in one write transaction (a missing key is a no-op, not an error).
    async fn remove_key(&self, def: BytesTable, key: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(def)?.remove(key.as_str())?;
            }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    /// Point lookup of the JSON record at `key` in `def`. `None` both for a genuine miss and for a
    /// read/deserialise failure, which (unlike a miss) is logged under `label` (see `read_or_log`),
    /// as is a failed blocking task (`join_or_default`).
    async fn get_json<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        def: BytesTable,
        key: String,
        label: &'static str,
    ) -> Option<T> {
        let db = self.db.clone();
        let r = tokio::task::spawn_blocking(move || {
            let txn = Self::read_or_log(label, "begin_read", db.begin_read())?;
            let table = Self::read_or_log(label, "open_table", txn.open_table(def))?;
            let entry = Self::read_or_log(label, "get", table.get(key.as_str()))??;
            Self::read_or_log(
                label,
                "deserialise",
                serde_json::from_slice::<T>(entry.value()),
            )
        })
        .await;
        Self::join_or_default(label, r)
    }

    // ── owned_hashes accessors ────────────────────────────────────────────────

    pub async fn put_owned(&self, hash: String, rec: OwnedRecord) -> Result<(), AppError> {
        // Normalise the key to lowercase so the owned table is case-consistent regardless of caller
        // (the mirror lowercases; the scraper emits lowercase infohashes). This makes the exact-match
        // get/put/remove and `put_owned_if_absent`'s atomic dedup caller-proof — a differently-cased
        // write can never produce a duplicate record for one physical torrent.
        let hash = hash.to_ascii_lowercase();
        self.put_json(OWNED_TABLE, hash, &rec, "OwnedRecord").await
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
        self.get_json(OWNED_TABLE, hash, "get_owned").await
    }

    /// Mark the owned record for `hash` `Verified`, as one read-modify-write transaction: its
    /// `provides` replaced by `provides` when given (a series), and its quality snapshot (if any)
    /// marked cached with `unverified` stamped on it. Returns the record's quality as written, or
    /// `None` when there is no record — so a record removed concurrently (dedup, Trigger A/B, a
    /// reap) is never written back. A record that won't deserialise is an `Err` (left untouched),
    /// so `observe` learns the change did not land instead of believing it did.
    pub async fn mark_verified_if_present(
        &self,
        hash: String,
        provides: Option<Vec<(u32, u32)>>,
        unverified: bool,
    ) -> Result<Option<Option<crate::release::QualitySummary>>, AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        let db = self.db.clone();
        type Written = Option<Option<crate::release::QualitySummary>>;
        let result = tokio::task::spawn_blocking(move || -> Result<Written, AppError> {
            let mut quality = None;
            let outcome = Self::update_json::<OwnedRecord>(
                &db,
                OWNED_TABLE,
                &hash,
                "mark_verified_if_present",
                |rec| {
                    rec.status = OwnedStatus::Verified;
                    if let Some(p) = provides {
                        rec.provides = p;
                    }
                    if let Some(q) = rec.quality.as_mut() {
                        q.mark_cached();
                        q.unverified = unverified;
                    }
                    quality = rec.quality.clone();
                    true
                },
            )?;
            Ok(outcome.map(|_| quality))
        })
        .await;
        Self::flatten_join_app(result)
    }

    /// Fill the TMDB language fields of the owned record for `hash` that are still unset, as one
    /// read-modify-write transaction that touches no other field: `request.spoken_languages =
    /// Some(spoken)` only if it is `None` (never fetched), and `request.original_language =
    /// original_language` only if it is `None` (an account-mirror record never had one). So the upgrade
    /// pass's one-time TMDB backfill can't clobber a concurrent `observe`/acquire update to the same
    /// record, overwrite a list that was already fetched, or replace a recorded original language.
    /// Returns `true` if it wrote; `false` when nothing was unset (or nothing to fill) or the record
    /// is missing (never resurrected).
    pub async fn backfill_title_languages_if_unset(
        &self,
        hash: String,
        original_language: Option<String>,
        spoken: Vec<String>,
    ) -> Result<bool, AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool, AppError> {
            let outcome = Self::update_json::<OwnedRecord>(
                &db,
                OWNED_TABLE,
                &hash,
                "backfill_title_languages_if_unset",
                |rec| {
                    let mut changed = false;
                    if rec.request.spoken_languages.is_none() {
                        rec.request.spoken_languages = Some(spoken);
                        changed = true;
                    }
                    if rec.request.original_language.is_none() && original_language.is_some() {
                        rec.request.original_language = original_language;
                        changed = true;
                    }
                    changed
                },
            )?;
            Ok(outcome == Some(true))
        })
        .await;
        Self::flatten_join_app(result)
    }

    /// Set the `quality` of the owned record for `hash` ONLY if it has none (`None`), as one
    /// read-modify-write transaction that touches no other field — so the quality backfill (which
    /// re-derives a missing quality from the torrent's file name) can never overwrite a recorded
    /// quality or clobber a concurrent `observe`/upgrade write to another field. Returns `true` if it
    /// wrote; `false` when a quality was already recorded or the record is missing (never resurrected).
    pub async fn set_quality_if_unset(
        &self,
        hash: String,
        quality: crate::release::QualitySummary,
    ) -> Result<bool, AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool, AppError> {
            let outcome = Self::update_json::<OwnedRecord>(
                &db,
                OWNED_TABLE,
                &hash,
                "set_quality_if_unset",
                |rec| {
                    if rec.quality.is_none() {
                        rec.quality = Some(quality);
                        true
                    } else {
                        false
                    }
                },
            )?;
            Ok(outcome == Some(true))
        })
        .await;
        Self::flatten_join_app(result)
    }

    /// Set `request.imdb_id` of the owned record for `hash` ONLY while it is empty, as one
    /// read-modify-write transaction that touches no other field. The account mirror writes an empty
    /// id when TMDB's `external_ids` lookup fails transiently, and nothing else would ever fill it
    /// (the hash stays owned, so the mirror never revisits it): the scan loop's daily backfill uses
    /// this so it can't overwrite a recorded id or clobber a concurrent write to another field.
    /// Returns `true` if it wrote; `false` when an id is already recorded or the record is missing
    /// (never resurrected).
    pub async fn set_imdb_id_if_empty(
        &self,
        hash: String,
        imdb_id: String,
    ) -> Result<bool, AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool, AppError> {
            let outcome = Self::update_json::<OwnedRecord>(
                &db,
                OWNED_TABLE,
                &hash,
                "set_imdb_id_if_empty",
                |rec| {
                    if rec.request.imdb_id.is_empty() {
                        rec.request.imdb_id = imdb_id;
                        true
                    } else {
                        false
                    }
                },
            )?;
            Ok(outcome == Some(true))
        })
        .await;
        Self::flatten_join_app(result)
    }

    /// Set `provides` of the owned record for `hash` to `provides` ONLY while the record is a
    /// `Verified` show record whose `provides` is empty, as one read-modify-write transaction that
    /// touches no other field. The account mirror writes a show's `provides` once, from the file
    /// names it could decode then, and never rewrites an existing record: the scan loop's provides
    /// backfill uses this to heal a record whose files now decode, without overwriting a recorded set
    /// or a `Pending` record's (`observe` settles that one). Returns `true` if it wrote; `false` for
    /// an empty `provides`, any other record, or a missing one (never resurrected).
    pub async fn set_provides_if_empty(
        &self,
        hash: String,
        provides: Vec<(u32, u32)>,
    ) -> Result<bool, AppError> {
        if provides.is_empty() {
            return Ok(false);
        }
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool, AppError> {
            let outcome = Self::update_json::<OwnedRecord>(
                &db,
                OWNED_TABLE,
                &hash,
                "set_provides_if_empty",
                |rec| {
                    if rec.request.kind == crate::scraper::MediaKind::Series
                        && rec.status == OwnedStatus::Verified
                        && rec.provides.is_empty()
                    {
                        rec.provides = provides;
                        true
                    } else {
                        false
                    }
                },
            )?;
            Ok(outcome == Some(true))
        })
        .await;
        Self::flatten_join_app(result)
    }

    pub async fn remove_owned(&self, hash: String) -> Result<(), AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        self.remove_key(OWNED_TABLE, hash).await
    }

    pub async fn all_owned(&self) -> Vec<(String, OwnedRecord)> {
        let db = self.db.clone();
        // owned_hashes is authoritative + non-regenerable: a silently-dropped row becomes an
        // invisible torrent (duplicate re-acquire / lost provenance / remove churn), so every read
        // and deserialise failure is logged (`scan_json`).
        let r = tokio::task::spawn_blocking(move || {
            Self::scan_json::<OwnedRecord>(&db, OWNED_TABLE, "owned_hashes")
        })
        .await;
        Self::join_or_default("all_owned", r)
    }

    // ── authoritative_ids accessors ───────────────────────────────────────────

    pub async fn put_authoritative(
        &self,
        hash: String,
        meta: crate::vfs::MediaMetadata,
    ) -> Result<(), AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        self.put_json(AUTH_TABLE, hash, &meta, "authoritative metadata")
            .await
    }

    pub async fn authoritative_meta(&self, hash: String) -> Option<crate::vfs::MediaMetadata> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        self.get_json(AUTH_TABLE, hash, "authoritative_meta").await
    }

    pub async fn remove_authoritative(&self, hash: String) -> Result<(), AppError> {
        let hash = hash.to_ascii_lowercase(); // case-normalised key (see put_owned)
        self.remove_key(AUTH_TABLE, hash).await
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
        let entry = serde_json::json!({"reason": reason, "at": at});
        self.put_json(BLACKLIST_TABLE, key, &entry, "blacklist entry")
            .await
    }

    /// Single-hash blacklist lookup. Test-only: production filters with `blacklisted_hashes_for` /
    /// `all_blacklisted_hashes` (one read per title / per tick rather than one per candidate).
    #[cfg(test)]
    pub async fn is_blacklisted(
        &self,
        kind: crate::scraper::MediaKind,
        tmdb_id: u64,
        hash: String,
    ) -> bool {
        let key = blacklist_key(kind, tmdb_id, &hash);
        let db = self.db.clone();
        let r = tokio::task::spawn_blocking(move || {
            // Log-then-default on a transient read failure, matching `read_or_log`'s convention: a
            // silent false "not blacklisted" would let a rejected hash be re-added, invisibly.
            let txn = match db.begin_read() {
                Ok(t) => t,
                Err(e) => {
                    error!("is_blacklisted begin_read: {e}");
                    return false;
                }
            };
            let table = match txn.open_table(BLACKLIST_TABLE) {
                Ok(t) => t,
                Err(e) => {
                    error!("is_blacklisted open_table: {e}");
                    return false;
                }
            };
            match table.get(key.as_str()) {
                Ok(v) => v.is_some(),
                Err(e) => {
                    error!("is_blacklisted get: {e}");
                    false
                }
            }
        })
        .await;
        Self::join_or_default("is_blacklisted", r)
    }

    /// Every blacklisted infohash (lowercased), across ALL media types and tmdb_ids. Used by the
    /// account mirror to avoid re-adopting a hash the engine rejected — deliberately media-type- and
    /// tmdb-agnostic (the key is `<m|s>|tmdb_id|hash`, so the hash is the segment after the LAST `|`)
    /// so it still catches a torrent that re-identifies to a DIFFERENT title than the one it was
    /// blacklisted under (the wrong-title case). One read per scan tick.
    pub async fn all_blacklisted_hashes(&self) -> std::collections::HashSet<String> {
        let db = self.db.clone();
        let r = tokio::task::spawn_blocking(move || {
            // A dropped blacklist row silently re-adopts a hash the engine rejected (regenerable via
            // the TTL, so it self-heals — but keep it diagnosable): `scan_rows` logs read failures.
            let mut out = std::collections::HashSet::new();
            Self::scan_rows(&db, BLACKLIST_TABLE, "blacklist", |k, _| {
                // key = "<m|s>|tmdb_id|hash" → the hash is everything after the LAST '|'.
                if let Some((_, hash)) = k.rsplit_once('|') {
                    out.insert(hash.to_ascii_lowercase());
                }
            });
            out
        })
        .await;
        Self::join_or_default("all_blacklisted_hashes", r)
    }

    /// For every blacklisted infohash (lowercased, across all titles as in
    /// [`Self::all_blacklisted_hashes`]), the newest `at` among its entries whose reason is not in
    /// `ignore_reasons`. A hash whose every entry carries an ignored reason is absent; an entry with
    /// no `at` counts as 0. One read of the table.
    pub async fn blacklist_newest_at_by_hash(
        &self,
        ignore_reasons: &'static [&'static str],
    ) -> HashMap<String, u64> {
        let db = self.db.clone();
        let r = tokio::task::spawn_blocking(move || {
            let mut out: HashMap<String, u64> = HashMap::new();
            Self::scan_rows(&db, BLACKLIST_TABLE, "blacklist", |k, v| {
                // key = "<m|s>|tmdb_id|hash" → the hash is everything after the LAST '|'.
                let Some((_, hash)) = k.rsplit_once('|') else {
                    return;
                };
                let entry = serde_json::from_slice::<serde_json::Value>(v).ok();
                let field = |name: &str| entry.as_ref().and_then(|j| j.get(name).cloned());
                let reason = field("reason");
                if ignore_reasons.contains(&reason.as_ref().and_then(|r| r.as_str()).unwrap_or(""))
                {
                    return;
                }
                let at = field("at").and_then(|a| a.as_u64()).unwrap_or(0);
                let newest = out.entry(hash.to_ascii_lowercase()).or_insert(at);
                *newest = (*newest).max(at);
            });
            out
        })
        .await;
        Self::join_or_default("blacklist_newest_at_by_hash", r)
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
        let r = tokio::task::spawn_blocking(move || -> usize {
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
        .await;
        Self::join_or_default("prune_blacklist_before", r)
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
        let r = tokio::task::spawn_blocking(move || {
            let mut out = std::collections::HashSet::new();
            let label = "blacklisted_hashes_for";
            let Some(txn) = Self::read_or_log(label, "begin_read", db.begin_read()) else {
                return out;
            };
            let Some(table) =
                Self::read_or_log(label, "open_table", txn.open_table(BLACKLIST_TABLE))
            else {
                return out;
            };
            // The key is "<m|s>|tmdb_id|hash", so this title's rows are a contiguous,
            // lexicographically-ordered key range. A keyed range scan reads only those rows
            // (O(log n + matches)) instead of the whole table (which grows to 30 days of churn under
            // the TTL). Any key that sorts at/after `prefix` but does NOT start with it sorts
            // strictly AFTER the entire matching block, so the first non-matching key ends the scan.
            let Some(iter) = Self::read_or_log(label, "range", table.range(prefix.as_str()..))
            else {
                return out;
            };
            for entry in iter {
                let (k, _) = match entry {
                    Ok(kv) => kv,
                    Err(e) => {
                        error!("blacklist row read failed (skipped): {e}");
                        continue;
                    }
                };
                match k.value().strip_prefix(&prefix) {
                    Some(hash) => {
                        out.insert(hash.to_ascii_lowercase());
                    }
                    None => break,
                }
            }
            out
        })
        .await;
        Self::join_or_default("blacklisted_hashes_for", r)
    }

    // ── trakt_tokens accessors ────────────────────────────────────────────────

    /// `slug` is the user's Trakt URL slug and is the key under which the tokens are stored.
    pub async fn put_trakt_tokens(
        &self,
        slug: String,
        tokens: TraktTokens,
    ) -> Result<(), AppError> {
        self.put_json(TRAKT_TOKENS_TABLE, slug, &tokens, "TraktTokens")
            .await
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
            // A missing record (removed since it was read — a concurrent /remove) is `None`: never
            // resurrected. A rotated refresh token means a concurrent refresh won — leave it alone.
            let outcome = Self::update_json::<TraktTokens>(
                &db,
                TRAKT_TOKENS_TABLE,
                &slug,
                "update_trakt_tokens_if_refresh_matches",
                |current| {
                    if current.refresh != expected_refresh {
                        return false;
                    }
                    mutate(current);
                    true
                },
            )?;
            Ok(outcome == Some(true))
        })
        .await;
        Self::flatten_join_app(result)
    }

    pub async fn get_trakt_tokens(&self, slug: String) -> Option<TraktTokens> {
        self.get_json(TRAKT_TOKENS_TABLE, slug, "get_trakt_tokens")
            .await
    }

    /// Remove only the token row for `slug`. Test-only: production removes an account via
    /// `remove_trakt_account`, which also drops its `wanted` rows in the same transaction (removing
    /// the token row alone would orphan them). Tests use it to remove an account mid-sync.
    #[cfg(test)]
    pub async fn remove_trakt_tokens(&self, slug: String) -> Result<(), AppError> {
        self.remove_key(TRAKT_TOKENS_TABLE, slug).await
    }

    /// Remove a Trakt account: its token row AND every one of its `wanted` rows, in ONE write
    /// transaction, so a store failure part-way can't leave wanted rows behind for a slug that no
    /// longer appears anywhere (nothing would ever prune them: only a sync of an enrolled account
    /// rewrites its rows). The user's rows are one contiguous key range (`wanted_key` starts with
    /// `"{slug}|"`, and a slug contains no `|`).
    pub async fn remove_trakt_account(&self, slug: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            {
                txn.open_table(TRAKT_TOKENS_TABLE)?.remove(slug.as_str())?;
                let mut wanted = txn.open_table(WANTED_TABLE)?;
                let prefix = format!("{slug}|");
                let mut keys = Vec::new();
                for entry in wanted.range(prefix.as_str()..)? {
                    let (k, _) = entry?;
                    if !k.value().starts_with(&prefix) {
                        break;
                    }
                    keys.push(k.value().to_string());
                }
                for k in &keys {
                    wanted.remove(k.as_str())?;
                }
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
        // Authoritative table: a lost token row is an account that silently stops syncing.
        let r = tokio::task::spawn_blocking(move || {
            Self::scan_json::<TraktTokens>(&db, TRAKT_TOKENS_TABLE, "trakt_tokens")
        })
        .await;
        Self::join_or_default("all_trakt_tokens", r)
    }

    // ── wanted accessors ──────────────────────────────────────────────────────

    /// Upsert one `wanted` row. Test-only: production writes a user's rows via
    /// `replace_user_wanted_if_enrolled` (atomic with the enrolment check).
    #[cfg(test)]
    pub async fn put_wanted(&self, rec: WantedRecord) -> Result<(), AppError> {
        let key = wanted_key(&rec.user, &rec.media_type, rec.tmdb_id);
        self.put_json(WANTED_TABLE, key, &rec, "WantedRecord").await
    }

    /// Rewrite one user's `wanted` rows — remove the `remove` keys, upsert `put` — in ONE write
    /// transaction that first checks the user is still enrolled (has a token row). Returns
    /// `Ok(false)` and writes nothing when the account is gone, so a sync racing an account removal
    /// can never resurrect the rows `remove_trakt_account` just deleted as permanent orphans (the
    /// check and the writes are atomic). Every `put` record must belong to `slug`.
    pub async fn replace_user_wanted_if_enrolled(
        &self,
        slug: String,
        remove: Vec<(crate::vfs::MediaType, u64)>,
        put: Vec<WantedRecord>,
    ) -> Result<bool, AppError> {
        // Serialise before opening the transaction so a serde failure writes nothing.
        let mut rows = Vec::with_capacity(put.len());
        for rec in &put {
            debug_assert_eq!(rec.user, slug, "a wanted row written for another user");
            let bytes = serde_json::to_vec(rec).map_err(|e| {
                AppError::Task(format!(
                    "serialise WantedRecord for {}|{}: {e}",
                    rec.user, rec.tmdb_id
                ))
            })?;
            rows.push((wanted_key(&rec.user, &rec.media_type, rec.tmdb_id), bytes));
        }
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool, redb::Error> {
            let txn = db.begin_write()?;
            {
                if txn
                    .open_table(TRAKT_TOKENS_TABLE)?
                    .get(slug.as_str())?
                    .is_none()
                {
                    return Ok(false); // dropped without committing: nothing written
                }
                let mut wanted = txn.open_table(WANTED_TABLE)?;
                for (media_type, tmdb_id) in &remove {
                    wanted.remove(wanted_key(&slug, media_type, *tmdb_id).as_str())?;
                }
                for (key, bytes) in &rows {
                    wanted.insert(key.as_str(), bytes.as_slice())?;
                }
            }
            txn.commit()?;
            Ok(true)
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
        self.get_json(WANTED_TABLE, key, "get_wanted").await
    }

    /// Remove one `wanted` row. Test-only: production removes a user's rows via
    /// `replace_user_wanted_if_enrolled`, or all of them with the account via
    /// `remove_trakt_account`.
    #[cfg(test)]
    pub async fn remove_wanted(
        &self,
        user: String,
        media_type: crate::vfs::MediaType,
        tmdb_id: u64,
    ) -> Result<(), AppError> {
        let key = wanted_key(&user, &media_type, tmdb_id);
        self.remove_key(WANTED_TABLE, key).await
    }

    /// Returns all wanted-set records across all users. Each record is self-keyed (embeds
    /// `user` and `tmdb_id`), so the reconciler can work with a flat list without re-parsing keys.
    pub async fn all_wanted(&self) -> Vec<WantedRecord> {
        let db = self.db.clone();
        // Authoritative table: a swallowed row would silently drop a user's desired title from the
        // reconcile set.
        let r = tokio::task::spawn_blocking(move || {
            Self::scan_json::<WantedRecord>(&db, WANTED_TABLE, "wanted")
                .into_iter()
                .map(|(_, rec)| rec)
                .collect()
        })
        .await;
        Self::join_or_default("all_wanted", r)
    }

    // ── selection accessors (SP3) ─────────────────────────────────────────────

    pub async fn put_selection(&self, slot: String, entry: SelectionEntry) -> Result<(), AppError> {
        self.put_json(SELECTION_TABLE, slot, &entry, "SelectionEntry")
            .await
    }

    /// Single-slot `selection` point lookup. Used by `acquire::record_verified` to gate a
    /// season-pack's episode-slot overwrite on quality (don't clobber a better single), by the
    /// upgrade engine to look up the copy a slot serves before a swap or prune (the read-gate's
    /// `copies_serving`, an episode's served baseline, consolidation's repoint set, the over-ceiling
    /// retention check), and by the tests. The VFS build path reads the whole map at once via
    /// `all_selection`.
    pub async fn get_selection(&self, slot: String) -> Option<SelectionEntry> {
        self.get_json(SELECTION_TABLE, slot, "get_selection").await
    }

    /// Atomically repoint `slot` to `new` only while it still holds `expected` (compare-and-set, one
    /// write transaction). Returns whether it wrote. Used by `selection::repoint_to_better_copies`,
    /// which plans from a snapshot: a slot the upgrade engine swapped, or dedup cleared, in the
    /// meantime is left alone rather than overwritten or re-created.
    pub async fn repoint_selection_if(
        &self,
        slot: String,
        expected: SelectionEntry,
        new: SelectionEntry,
    ) -> Result<bool, AppError> {
        let bytes = serde_json::to_vec(&new)
            .map_err(|e| AppError::Task(format!("serialise SelectionEntry for {slot}: {e}")))?;
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool, AppError> {
            let txn = db.begin_write().map_err(|e| AppError::Db(e.into()))?;
            let wrote = {
                let mut table = txn
                    .open_table(SELECTION_TABLE)
                    .map_err(|e| AppError::Db(e.into()))?;
                let current: Option<SelectionEntry> = table
                    .get(slot.as_str())
                    .map_err(|e| AppError::Db(e.into()))?
                    .and_then(|v| serde_json::from_slice(v.value()).ok());
                if current.as_ref() == Some(&expected) {
                    table
                        .insert(slot.as_str(), bytes.as_slice())
                        .map_err(|e| AppError::Db(e.into()))?;
                    true
                } else {
                    false
                }
            };
            txn.commit().map_err(|e| AppError::Db(e.into()))?;
            Ok(wrote)
        })
        .await;
        Self::flatten_join_app(result)
    }

    pub async fn remove_selection(&self, slot: String) -> Result<(), AppError> {
        self.remove_key(SELECTION_TABLE, slot).await
    }

    pub async fn all_selection(&self) -> Vec<(String, SelectionEntry)> {
        let db = self.db.clone();
        // Authoritative table: a swallowed row would silently revert that slot to the VFS's
        // unmanaged fallback (a movie's largest streamable torrent; every distinct file name for an
        // episode).
        let r = tokio::task::spawn_blocking(move || {
            Self::scan_json::<SelectionEntry>(&db, SELECTION_TABLE, "selection")
        })
        .await;
        Self::join_or_default("all_selection", r)
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
        let r = tokio::task::spawn_blocking(move || {
            // Log-then-default on a transient read failure, matching `read_or_log`'s convention: a
            // silent 0 ("never checked") would make an already-checked title re-process early, invisibly.
            let txn = match db.begin_read() {
                Ok(t) => t,
                Err(e) => {
                    error!("get_upgrade_checked begin_read: {e}");
                    return 0;
                }
            };
            let table = match txn.open_table(UPGRADE_CHECKS_TABLE) {
                Ok(t) => t,
                Err(e) => {
                    error!("get_upgrade_checked open_table: {e}");
                    return 0;
                }
            };
            match table.get(key.as_str()) {
                Ok(v) => v.map(|g| g.value()).unwrap_or(0),
                Err(e) => {
                    error!("get_upgrade_checked get: {e}");
                    0
                }
            }
        })
        .await;
        Self::join_or_default("get_upgrade_checked", r)
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
        let keep: Option<std::collections::HashSet<String>> = {
            let db = db.clone();
            let r = tokio::task::spawn_blocking(move || {
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
                // A row-read error (logged by `scan_rows`) or a deserialise error omits that row from
                // the keep-set, pruning its title's cursor (regenerable) — logged, not silent.
                let read =
                    Self::scan_rows(&db, OWNED_TABLE, "prune_upgrade_checks: owned", |_, v| {
                        match serde_json::from_slice::<OwnedKeyView>(v) {
                            Ok(view) => {
                                let media_type = media_kind_to_type(view.request.kind);
                                keep.insert(upgrade_check_key(&media_type, view.request.tmdb_id));
                            }
                            Err(e) => {
                                error!("prune_upgrade_checks: owned row deserialise failed: {e}")
                            }
                        }
                    });
                read.then_some(keep)
            })
            .await;
            Self::join_or_default("prune_upgrade_checks: owned keep-set", r)
        };
        // The owned table could not be read: an empty keep-set would prune EVERY title's cursor, so
        // prune nothing this time (the failure is already logged).
        let Some(keep) = keep else {
            return 0;
        };
        let r = tokio::task::spawn_blocking(move || -> usize {
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
        .await;
        Self::join_or_default("prune_upgrade_checks_not_owned", r)
    }

    // ── staging intents (see `StagingIntent`) ─────────────────────────────────

    /// Open a staging intent for `hash` (no torrent id yet), replacing any earlier one, and note it
    /// as this process's ([`Store::local_staging_age`]). The local note is taken before the write
    /// (and dropped again if the write fails), so the sweep never sees the row without it.
    pub async fn put_staging_intent(&self, hash: String, started_at: u64) -> Result<(), AppError> {
        let key = hash.to_ascii_lowercase();
        let intent = StagingIntent {
            torrent_id: None,
            started_at,
        };
        self.local_staging()
            .insert(key.clone(), tokio::time::Instant::now());
        let written = self
            .put_json(STAGING_TABLE, key.clone(), &intent, "StagingIntent")
            .await;
        if written.is_err() {
            self.local_staging().remove(&key);
        }
        written
    }

    fn local_staging(&self) -> std::sync::MutexGuard<'_, HashMap<String, tokio::time::Instant>> {
        self.local_staging
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How long ago THIS process opened the staging intent of `hash` (monotonic), or `None` when it
    /// did not open it (or has closed it): an intent with no local entry was left by an earlier
    /// process.
    pub(crate) fn local_staging_age(&self, hash: &str) -> Option<std::time::Duration> {
        self.local_staging()
            .get(&hash.to_ascii_lowercase())
            .map(|opened| opened.elapsed())
    }

    /// Forget which staging intents this process opened, as a restart would (tests only).
    #[cfg(test)]
    pub(crate) fn forget_local_staging_intents(&self) {
        self.local_staging().clear();
    }

    /// Record the torrent id the add returned on `hash`'s open intent. A hash with no intent is left
    /// without one (`Ok`): nothing is resurrected.
    pub async fn set_staging_torrent_id(
        &self,
        hash: String,
        torrent_id: String,
    ) -> Result<(), AppError> {
        let key = hash.to_ascii_lowercase();
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || {
            Self::update_json::<StagingIntent>(
                &db,
                STAGING_TABLE,
                &key,
                "set_staging_torrent_id",
                |intent| {
                    let changed = intent.torrent_id.as_deref() != Some(torrent_id.as_str());
                    intent.torrent_id = Some(torrent_id);
                    changed
                },
            )
            .map(|_| ())
        })
        .await;
        Self::flatten_join_app(result)
    }

    /// Close `hash`'s staging intent (a missing one is a no-op). The local note
    /// ([`Store::local_staging_age`]) goes once the row is gone; a failed removal keeps both.
    pub async fn clear_staging_intent(&self, hash: String) -> Result<(), AppError> {
        let key = hash.to_ascii_lowercase();
        let removed = self.remove_key(STAGING_TABLE, key.clone()).await;
        if removed.is_ok() {
            self.local_staging().remove(&key);
        }
        removed
    }

    /// Every open staging intent, keyed by lowercased infohash. A read failure is logged and reads
    /// as no intents.
    pub async fn all_staging_intents(&self) -> Vec<(String, StagingIntent)> {
        let db = self.db.clone();
        let r = tokio::task::spawn_blocking(move || {
            Self::scan_json::<StagingIntent>(&db, STAGING_TABLE, "staging")
        })
        .await;
        Self::join_or_default("all_staging_intents", r)
    }
}

/// A [`Store`] whose writes a test can make fail from a chosen commit on (a full disk, an I/O
/// error), shared by every module's tests: [`fail_after_arm_store`] builds one, and its
/// [`WriteFailSwitch`] arms it.
#[cfg(test)]
pub(crate) mod fail_writes {
    use super::Store;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The bytes of an in-memory redb file, shared so a second `Database` can be opened on what a
    /// first one committed ([`WriteFailSwitch::reopen`]).
    #[derive(Debug, Clone, Default)]
    struct SharedMemory(Arc<std::sync::RwLock<Vec<u8>>>);
    impl redb::StorageBackend for SharedMemory {
        fn len(&self) -> Result<u64, std::io::Error> {
            Ok(self.0.read().unwrap().len() as u64)
        }
        fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), std::io::Error> {
            let bytes = self.0.read().unwrap();
            let start = offset as usize;
            let src = bytes
                .get(start..start + out.len())
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
            out.copy_from_slice(src);
            Ok(())
        }
        fn set_len(&self, len: u64) -> Result<(), std::io::Error> {
            self.0.write().unwrap().resize(len as usize, 0);
            Ok(())
        }
        fn sync_data(&self) -> Result<(), std::io::Error> {
            Ok(())
        }
        fn write(&self, offset: u64, data: &[u8]) -> Result<(), std::io::Error> {
            let mut bytes = self.0.write().unwrap();
            let start = offset as usize;
            if bytes.len() < start + data.len() {
                bytes.resize(start + data.len(), 0);
            }
            bytes[start..start + data.len()].copy_from_slice(data);
            Ok(())
        }
    }

    /// A redb backend over [`SharedMemory`] that fails every `write`/`set_len` once ARMED
    /// ([`WriteFailSwitch`]), so a test can make a store commit return `Err` deterministically. Seed
    /// the store BEFORE arming. redb is copy-on-write — a failed page write aborts the commit without
    /// flipping the committed root — but it then treats the database as failed: a later read that
    /// its page cache can't serve returns an error. So a test verifies the committed state through a
    /// fresh database on the same bytes ([`WriteFailSwitch::reopen`]). Every commit ends in
    /// `sync_data`, which this counts so a switch can let a given number of commits through first.
    #[derive(Debug)]
    struct ArmableFailBackend {
        inner: SharedMemory,
        switch: WriteFailSwitch,
    }
    impl redb::StorageBackend for ArmableFailBackend {
        fn len(&self) -> Result<u64, std::io::Error> {
            self.inner.len()
        }
        fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), std::io::Error> {
            self.inner.read(offset, out)
        }
        fn set_len(&self, len: u64) -> Result<(), std::io::Error> {
            if self.switch.failing() {
                return Err(std::io::Error::other("set_len blocked (armed)"));
            }
            self.inner.set_len(len)
        }
        fn sync_data(&self) -> Result<(), std::io::Error> {
            self.switch.syncs.fetch_add(1, Ordering::SeqCst);
            self.inner.sync_data()
        }
        fn write(&self, offset: u64, data: &[u8]) -> Result<(), std::io::Error> {
            if self.switch.failing() {
                return Err(std::io::Error::other("write blocked (armed)"));
            }
            self.inner.write(offset, data)
        }
    }

    /// Arms an [`ArmableFailBackend`]: writes fail once `syncs` reaches `fail_from`.
    #[derive(Debug, Clone)]
    pub(crate) struct WriteFailSwitch {
        syncs: Arc<AtomicUsize>,
        fail_from: Arc<AtomicUsize>,
        memory: SharedMemory,
    }
    impl WriteFailSwitch {
        /// A store on a fresh database over the bytes committed so far (the armed one may refuse
        /// reads after its failed write). Disarm first.
        pub(crate) fn reopen(&self) -> Store {
            let db = redb::Database::builder()
                .create_with_backend(self.memory.clone())
                .unwrap();
            Store::from_database(Arc::new(db)).unwrap()
        }
        fn failing(&self) -> bool {
            self.syncs.load(Ordering::SeqCst) >= self.fail_from.load(Ordering::SeqCst)
        }
        /// Let the next `commits` commits (of `syncs_per_commit` syncs each) through, then fail every
        /// write.
        pub(crate) fn arm_after(&self, commits: usize, syncs_per_commit: usize) {
            self.fail_from.store(
                self.syncs.load(Ordering::SeqCst) + commits * syncs_per_commit,
                Ordering::SeqCst,
            );
        }
        pub(crate) fn disarm(&self) {
            self.fail_from.store(usize::MAX, Ordering::SeqCst);
        }
    }

    /// A store whose writes fail once its switch is armed.
    pub(crate) fn fail_after_arm_store() -> (Store, WriteFailSwitch) {
        let switch = WriteFailSwitch {
            syncs: Arc::new(AtomicUsize::new(0)),
            fail_from: Arc::new(AtomicUsize::new(usize::MAX)),
            memory: SharedMemory::default(),
        };
        let backend = ArmableFailBackend {
            inner: switch.memory.clone(),
            switch: switch.clone(),
        };
        let db = redb::Database::builder()
            .create_with_backend(backend)
            .unwrap();
        (Store::from_database(Arc::new(db)).unwrap(), switch)
    }

    /// How many `sync_data` calls one store commit makes on `store`'s backend (measured with a
    /// harmless write), for [`WriteFailSwitch::arm_after`].
    pub(crate) async fn syncs_per_commit(store: &Store, switch: &WriteFailSwitch) -> usize {
        let before = switch.syncs.load(Ordering::SeqCst);
        store
            .set_upgrade_checked(&crate::vfs::MediaType::Movie, 424_242, 1)
            .await
            .unwrap();
        let n = switch.syncs.load(Ordering::SeqCst) - before;
        assert!(n > 0, "a commit syncs");
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MediaType;
    use redb::backends::InMemoryBackend;
    use std::sync::atomic::{AtomicU32, Ordering};

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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
    async fn has_matches_reports_whether_the_cache_holds_any_entry() {
        let store = Store::in_memory();
        assert!(!store.has_matches().await);
        store
            .put_matches(vec![("a".to_string(), info("a"), movie("A"))])
            .await
            .unwrap();
        assert!(store.has_matches().await);
        store.remove_matches(vec!["a".to_string()]).await.unwrap();
        assert!(!store.has_matches().await);
    }

    #[tokio::test]
    async fn remove_deletes_entry() {
        let store = Store::in_memory();
        store
            .put_matches(vec![("x".to_string(), info("x"), movie("X"))])
            .await
            .unwrap();
        store.remove_matches(vec!["x".to_string()]).await.unwrap();
        assert!(store.get_match("x".to_string()).await.is_none());
    }

    #[tokio::test]
    async fn replace_swaps_old_for_new() {
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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

    #[tokio::test]
    async fn open_corrupt_db_errors_when_all_backup_names_taken() {
        // Data-loss guard for `unique_backup_path`'s exhaustion return (→ `AppError::Config` in
        // `move_aside_and_create`): when `.corrupt` AND `.corrupt.1 ..= .corrupt.999` (all 1000
        // candidate backup names) already exist, there is nowhere left to move the corrupt DB.
        // `move_aside_and_create`'s whole contract is that it NEVER deletes/overwrites the file, so
        // this must surface as a hard startup Err with the original corrupt DB left intact on disk —
        // never a silent wipe. Exercises that branch end-to-end (only the pieces were unit-tested).
        let tmp = TempDb::new("backup_exhausted");
        const CORRUPT_CONTENTS: &[u8] = b"genuinely-not-a-redb-file";
        std::fs::write(&tmp.path, CORRUPT_CONTENTS).unwrap();

        // Pre-create every candidate backup name: `.corrupt` and `.corrupt.1 ..= .corrupt.999`.
        let backups: Vec<String> = std::iter::once(tmp.corrupt_path())
            .chain((1..1000).map(|n| format!("{}.corrupt.{n}", tmp.path)))
            .collect();
        for b in &backups {
            std::fs::write(b, b"existing backup").unwrap();
        }

        let result = Store::open(&tmp.path);

        // Exhausted backup names → hard startup error, NOT a recovery.
        assert!(
            matches!(result, Err(AppError::Config(_))),
            "exhausted backup names must fail startup, not silently recover"
        );
        // The whole point of move_aside_and_create: the corrupt DB is NEVER deleted or overwritten.
        assert_eq!(
            std::fs::read(&tmp.path).unwrap(),
            CORRUPT_CONTENTS,
            "the original corrupt DB must be left intact on disk, never deleted/overwritten"
        );

        // Clean up the numbered backups (the plain `.corrupt` + `path` are handled by TempDb::Drop).
        for b in &backups {
            let _ = std::fs::remove_file(b);
        }
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

    /// Write a real file DB with one cached match through `Store::open`, then close it.
    async fn write_real_db(path: &str) {
        let store = Store::open(path).unwrap();
        store
            .put_matches(vec![("a".to_string(), info("a"), movie("A"))])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn open_refuses_a_newer_redb_format_without_discarding_it() {
        // redb rejects a file whose commit-slot format version is ahead of the library with
        // `StorageError::Corrupted("Expected file format version <= 3, found N")` — no dedicated
        // variant. That is an INTACT database written by a newer redb (a rollback after a redb
        // upgrade rewrote the file), so `open` must fail startup and keep the file, exactly like the
        // older-format (`UpgradeRequired`) and newer-schema cases — never move it aside.
        let tmp = TempDb::new("newer_redb_format");
        write_real_db(&tmp.path).await;
        let mut bytes = std::fs::read(&tmp.path).unwrap();
        // The two commit slots' format-version bytes (redb 3 header layout).
        bytes[64] = 4;
        bytes[192] = 4;
        std::fs::write(&tmp.path, &bytes).unwrap();

        let result = Store::open(&tmp.path);
        assert!(
            matches!(result, Err(AppError::Config(_))),
            "a newer redb on-disk format must fail startup, got {:?}",
            result.as_ref().map(|_| ())
        );
        assert!(
            !std::path::Path::new(&tmp.corrupt_path()).exists(),
            "an intact newer-format DB must NOT be moved aside"
        );
        assert_eq!(
            std::fs::read(&tmp.path).unwrap(),
            bytes,
            "the file must be left exactly as it was"
        );
    }

    #[tokio::test]
    async fn open_recovers_from_a_truncated_db_instead_of_panicking() {
        // A file that keeps an intact header but is shorter than the layout it records (an
        // interrupted copy/restore) makes redb PANIC inside `Database::create` (an `assert!` on the
        // file length), not return an error. `open` must catch that and move the file aside like
        // any other corruption, rather than crash-looping the service at startup.
        let tmp = TempDb::new("truncated");
        write_real_db(&tmp.path).await;
        let len = std::fs::metadata(&tmp.path).unwrap().len();
        assert!(len > 1000, "a real DB is larger than its header");
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp.path)
            .unwrap();
        f.set_len(len / 2).unwrap();
        drop(f);

        let store = Store::open(&tmp.path).expect("a truncated DB must be recovered, not panic");
        assert!(
            std::path::Path::new(&tmp.corrupt_path()).exists(),
            "the truncated file must be moved aside"
        );
        assert!(
            store.get_match("a".to_string()).await.is_none(),
            "the recreated DB starts empty"
        );
        store
            .put_matches(vec![("b".to_string(), info("b"), movie("B"))])
            .await
            .unwrap();
        assert!(store.get_match("b".to_string()).await.is_some());
    }

    // ── owned / authoritative / blacklist tests ───────────────────────────────

    use crate::scraper::MediaKind;

    fn req(imdb: &str, tmdb: u64) -> AcquireRequest {
        AcquireRequest {
            imdb_id: imdb.to_string(),
            tmdb_id: tmdb,
            kind: MediaKind::Movie,
            season: None,
            episode: None,
            original_language: Some("eng".to_string()),
            spoken_languages: Some(vec![]),
            metadata: movie("Title"),
        }
    }

    #[tokio::test]
    async fn owned_round_trip_and_status_update() {
        let store = Store::in_memory();
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
            .mark_verified_if_present("h1".to_string(), None, false)
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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
    async fn authoritative_accessors_are_case_insensitive_on_the_hash_key() {
        // Like the owned table: a provider-cased write must be found, and removed, by the lowercase
        // owned key, so a removal can never leave a stale identification override behind.
        let store = Store::in_memory();
        store
            .put_authoritative("ABC".into(), movie("A"))
            .await
            .unwrap();
        assert_eq!(
            store.authoritative_meta("abc".into()).await,
            Some(movie("A")),
            "a lowercase read must find an upper-case write"
        );
        store.remove_authoritative("AbC".into()).await.unwrap();
        assert!(store.authoritative_meta("abc".into()).await.is_none());
        assert!(store.authoritative_meta("ABC".into()).await.is_none());
    }

    #[tokio::test]
    async fn mark_verified_if_present_on_a_corrupt_record_returns_err() {
        // Settling an existing-but-undeserialisable record must surface an Err (so `observe` knows
        // the change did not land), NOT commit-and-return-Ok while the record silently stays as it
        // was; and the raw row is left untouched.
        let store = Store::in_memory();
        let raw: &[u8] = b"not a valid OwnedRecord";
        {
            let txn = store.db.begin_write().unwrap();
            {
                let mut t = txn.open_table(OWNED_TABLE).unwrap();
                t.insert("bad", raw).unwrap();
            }
            txn.commit().unwrap();
        }
        let res = store
            .mark_verified_if_present("bad".to_string(), None, false)
            .await;
        assert!(
            matches!(res, Err(AppError::Task(_))),
            "a corrupt owned record must yield Err, got: {res:?}"
        );
        let txn = store.db.begin_read().unwrap();
        let t = txn.open_table(OWNED_TABLE).unwrap();
        assert_eq!(t.get("bad").unwrap().unwrap().value(), raw);
    }

    #[tokio::test]
    async fn all_owned_skips_a_corrupt_row_and_keeps_good_rows() {
        // owned_hashes is authoritative + non-regenerable: one un-deserialisable row must NOT abort
        // iteration (that would make the WHOLE library invisible → mass duplicate re-acquire). It is
        // skipped; the good rows survive.
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
        // `from_database` must REFUSE a schema newer than this binary supports rather than run against
        // it. `open` refuses it too (`open_refuses_a_newer_version_without_discarding_it`); this test
        // pins the `from_database` path.
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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
    async fn blacklist_newest_at_by_hash_takes_the_newest_entry_and_skips_ignored_reasons() {
        let store = Store::in_memory();
        let (m, s) = (MediaKind::Movie, MediaKind::Series);
        // Two titles blacklisted the same hash (in different cases): the newest `at` wins.
        store
            .blacklist_add(m, 1, "AA".into(), "WrongTitle", 100)
            .await
            .unwrap();
        store
            .blacklist_add(s, 2, "aa".into(), "AudioMismatch", 300)
            .await
            .unwrap();
        // An ignored reason does not count, even when it is the newest entry.
        store
            .blacklist_add(m, 3, "aa".into(), "Dead", 900)
            .await
            .unwrap();
        // A hash with only ignored entries is absent.
        store
            .blacklist_add(m, 4, "bb".into(), "Dead", 50)
            .await
            .unwrap();
        let newest = store.blacklist_newest_at_by_hash(&["Dead"]).await;
        assert_eq!(newest.get("aa"), Some(&300));
        assert!(!newest.contains_key("bb"), "{newest:?}");
        // With nothing ignored, every entry counts.
        let all = store.blacklist_newest_at_by_hash(&[]).await;
        assert_eq!(all.get("aa"), Some(&900));
        assert_eq!(all.get("bb"), Some(&50));
    }

    #[tokio::test]
    async fn staging_intents_round_trip() {
        let store = Store::in_memory();
        assert!(store.all_staging_intents().await.is_empty());
        store.put_staging_intent("ABC".into(), 100).await.unwrap();
        assert_eq!(
            store.all_staging_intents().await,
            vec![(
                "abc".to_string(),
                StagingIntent {
                    torrent_id: None,
                    started_at: 100
                }
            )]
        );
        store
            .set_staging_torrent_id("abc".into(), "T1".into())
            .await
            .unwrap();
        assert_eq!(
            store.all_staging_intents().await[0].1.torrent_id.as_deref(),
            Some("T1")
        );
        // Setting an id on a hash with no intent writes nothing.
        store
            .set_staging_torrent_id("other".into(), "T2".into())
            .await
            .unwrap();
        assert_eq!(store.all_staging_intents().await.len(), 1);
        // Re-opening replaces the intent (the id is reset until the new add returns).
        store.put_staging_intent("abc".into(), 200).await.unwrap();
        assert_eq!(
            store.all_staging_intents().await[0].1,
            StagingIntent {
                torrent_id: None,
                started_at: 200
            }
        );
        store.clear_staging_intent("ABC".into()).await.unwrap();
        assert!(store.all_staging_intents().await.is_empty());
        // Clearing a missing intent is a no-op.
        store.clear_staging_intent("abc".into()).await.unwrap();
    }

    #[tokio::test]
    async fn prune_blacklist_before_removes_only_stale_rows() {
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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

    // ── trakt_tokens + wanted tests ───────────────────────────────────────────

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
                trakt_behind: true,
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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
                watched_episodes: vec![(1, 1), (1, 2), (2, 1)],
                trakt_behind: true,
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

    /// A show row written before `trakt_behind` existed reads as behind, so it is never judged
    /// finished on TMDB's aired set alone before a sync has asked Trakt (and an older build, which
    /// ignores the unknown field, reads a new row).
    #[test]
    fn a_legacy_show_watched_state_reads_as_behind() {
        let legacy: WatchedState =
            serde_json::from_str(r#"{"Show":{"watched_episodes":[[1,1]]}}"#).unwrap();
        assert_eq!(
            legacy,
            WatchedState::Show {
                watched_episodes: vec![(1, 1)],
                trakt_behind: true,
            }
        );
        // A row the current build writes keeps its explicit value.
        let row = WatchedState::Show {
            watched_episodes: vec![],
            trakt_behind: false,
        };
        let back: WatchedState =
            serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
        assert_eq!(back, row);
    }

    #[tokio::test]
    async fn wanted_movie_and_show_with_same_tmdb_id_coexist() {
        // TMDB movie and TV id-spaces are independent: a movie and a show with the SAME numeric id
        // must both persist (the key carries a media-type discriminator), not overwrite each other.
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
    async fn migrates_v7_db_flags_unverifiable_selected_files() {
        // v7→v8: an owned record SERVED from a file the probe can never read (any selection entry
        // pointing at its hash has a non-MKV/MP4 extension — e.g. a show episode served from a Russian-dub `.avi`
        // pack) is flagged `quality.unverified`, so the upgrade pass replaces it with a checked copy.
        let tmp = TempDb::new("migrate_v7_v8");
        let q = || {
            Some(crate::release::QualitySummary {
                cached: true,
                source_tier: 3_000,
                resolution: 1080,
                score: 1_111_000,
                unverified: false,
            })
        };
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let odef: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
                let mut ot = txn.open_table(odef).unwrap();
                // a: served from an `.avi` → flagged. b: served from an `.mkv` → stays checked.
                // c: `.avi` but no quality snapshot → nothing to flag. d: no selection → untouched.
                // e: a pack serving one `.mkv` slot AND one `.avi` slot → flagged (ANY entry).
                for (h, quality) in [("a", q()), ("b", q()), ("c", None), ("d", q()), ("e", q())] {
                    let rec = OwnedRecord {
                        request: req("tt7", 5150),
                        provenance: Provenance::watchlist("u"),
                        added_at: 1,
                        status: OwnedStatus::Verified,
                        provides: vec![],
                        quality,
                    };
                    // Encode as a genuine v7 row: no `unverified` key in the quality snapshot.
                    let mut v = serde_json::to_value(&rec).unwrap();
                    if let Some(q) = v.get_mut("quality").and_then(|q| q.as_object_mut()) {
                        q.remove("unverified");
                    }
                    ot.insert(h, serde_json::to_vec(&v).unwrap().as_slice())
                        .unwrap();
                }
                let sdef: TableDefinition<&str, &[u8]> = TableDefinition::new("selection");
                let mut st = txn.open_table(sdef).unwrap();
                for (slot, hash, path) in [
                    (
                        "e|5150|1|4",
                        "a",
                        "x/Show.S01E01-E04.WEBRip.XviD.Rus/Show.s01e04.avi",
                    ),
                    ("m|77", "b", "y/Movie.2020.1080p.BluRay.mkv"),
                    ("e|5150|1|5", "c", "x/Show.s01e05.avi"),
                    ("e|500|1|1", "e", "z/Show.s01e01.mkv"),
                    ("e|500|1|2", "e", "z/Show.s01e02.AVI"),
                ] {
                    let entry = SelectionEntry {
                        hash: hash.into(),
                        file_path: path.into(),
                    };
                    st.insert(slot, serde_json::to_vec(&entry).unwrap().as_slice())
                        .unwrap();
                }
                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &7u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();
        let unverified = |rec: Option<OwnedRecord>| rec.unwrap().quality.map(|q| q.unverified);
        assert_eq!(
            unverified(store.get_owned("a".into()).await),
            Some(true),
            "an owned record served from an .avi must be flagged unverified"
        );
        assert_eq!(
            unverified(store.get_owned("b".into()).await),
            Some(false),
            "an owned record served from an .mkv stays checked"
        );
        assert_eq!(
            unverified(store.get_owned("c".into()).await),
            None,
            "a record with no quality snapshot has nothing to flag"
        );
        assert_eq!(
            unverified(store.get_owned("d".into()).await),
            Some(false),
            "a record no selection points at is untouched"
        );
        assert_eq!(
            unverified(store.get_owned("e".into()).await),
            Some(true),
            "a pack serving ANY unverifiable file is flagged"
        );
        // Owned + selection rows are otherwise intact.
        let a = store.get_owned("a".into()).await.unwrap();
        assert_eq!(a.status, OwnedStatus::Verified);
        assert_eq!(a.provenance, Provenance::watchlist("u"));
        assert_eq!(a.quality.unwrap().score, 1_111_000);
        assert_eq!(
            store.get_selection("e|5150|1|4".into()).await.unwrap().hash,
            "a"
        );
        drop(store);
        let db = Database::create(&tmp.path).unwrap();
        assert_eq!(Store::read_version(&db).unwrap(), SCHEMA_VERSION);
        assert_eq!(SCHEMA_VERSION, 8);
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

    // ── per-user provenance on OwnedRecord tests ──────────────────────────────

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

    /// A record persisted before `AcquireRequest.spoken_languages` existed must still decode, with the
    /// field `None` ("never fetched") so the upgrade pass knows to backfill it from TMDB.
    #[test]
    fn acquire_request_without_spoken_languages_decodes_as_none() {
        let rec = OwnedRecord {
            request: req("tt1", 27205),
            provenance: Provenance::watchlist("alice"),
            added_at: 100,
            status: OwnedStatus::Verified,
            provides: vec![],
            quality: None,
        };
        let mut val = serde_json::to_value(&rec).unwrap();
        val["request"]
            .as_object_mut()
            .unwrap()
            .remove("spoken_languages");
        assert!(val["request"].get("spoken_languages").is_none());
        let back: OwnedRecord = serde_json::from_value(val).expect("legacy record must decode");
        assert_eq!(back.request.spoken_languages, None);
        assert_eq!(back.request.original_language.as_deref(), Some("eng"));
        assert_eq!(back.request.tmdb_id, 27205);
        // A fetched (even empty) list round-trips as `Some` — never collapsed back to "unfetched".
        let mut fetched = rec.clone();
        fetched.request.spoken_languages = Some(vec![]);
        let rt: OwnedRecord =
            serde_json::from_value(serde_json::to_value(&fetched).unwrap()).unwrap();
        assert_eq!(rt.request.spoken_languages, Some(vec![]));
    }

    /// The upgrade pass's TMDB language backfill fills ONLY the unset (`None`) language fields of a
    /// record, in one write transaction, touching nothing else — so it can't clobber a concurrent
    /// `observe` update, overwrite a list that was already fetched, or replace a recorded original
    /// language. An account-mirror record (both `None`) gains both.
    #[tokio::test]
    async fn backfill_title_languages_if_unset_fills_only_unset_fields() {
        let store = Store::in_memory();
        let mut legacy = OwnedRecord {
            request: req("tt1", 27205),
            provenance: Provenance::watchlist("alice"),
            added_at: 100,
            status: OwnedStatus::Pending,
            provides: vec![(1, 2)],
            quality: None,
        };
        legacy.request.spoken_languages = None;
        store
            .put_owned("hleg".into(), legacy.clone())
            .await
            .unwrap();
        let mut fetched = legacy.clone();
        fetched.request.spoken_languages = Some(vec!["de".into()]);
        store.put_owned("hnew".into(), fetched).await.unwrap();
        let mut mirror = legacy.clone();
        mirror.request.original_language = None;
        store.put_owned("hmir".into(), mirror).await.unwrap();

        let langs = vec!["en".to_string(), "fi".to_string()];
        let fi = Some("fi".to_string());
        assert!(store
            .backfill_title_languages_if_unset("HLEG".into(), fi.clone(), langs.clone())
            .await
            .unwrap());
        let got = store.get_owned("hleg".into()).await.unwrap();
        assert_eq!(got.request.spoken_languages, Some(langs.clone()));
        assert_eq!(
            got.request.original_language.as_deref(),
            Some("eng"),
            "a recorded original language is kept"
        );
        assert_eq!(got.status, OwnedStatus::Pending, "other fields intact");
        assert_eq!(got.provides, vec![(1, 2)]);
        assert_eq!(got.provenance, Provenance::watchlist("alice"));
        // Account-mirror record: both fields were never fetched → both filled.
        assert!(store
            .backfill_title_languages_if_unset("hmir".into(), fi.clone(), langs.clone())
            .await
            .unwrap());
        let got = store.get_owned("hmir".into()).await.unwrap();
        assert_eq!(got.request.spoken_languages, Some(langs.clone()));
        assert_eq!(got.request.original_language, fi);
        // Already fetched, original already known → untouched.
        assert!(!store
            .backfill_title_languages_if_unset("hnew".into(), fi.clone(), langs.clone())
            .await
            .unwrap());
        let got = store.get_owned("hnew".into()).await.unwrap();
        assert_eq!(got.request.spoken_languages, Some(vec!["de".to_string()]));
        assert_eq!(got.request.original_language.as_deref(), Some("eng"));
        // TMDB knows no original language → only the spoken list is filled.
        let mut blank = legacy.clone();
        blank.request.original_language = None;
        store.put_owned("hblank".into(), blank).await.unwrap();
        assert!(store
            .backfill_title_languages_if_unset("hblank".into(), None, vec![])
            .await
            .unwrap());
        let got = store.get_owned("hblank".into()).await.unwrap();
        assert_eq!(got.request.spoken_languages, Some(vec![]));
        assert_eq!(got.request.original_language, None);
        // Missing record → no-op, never resurrected.
        assert!(!store
            .backfill_title_languages_if_unset("hgone".into(), fi, langs)
            .await
            .unwrap());
        assert!(store.get_owned("hgone".into()).await.is_none());
    }

    /// The repoint is a compare-and-set: it writes only while the slot still holds the entry the
    /// caller planned from, so it can never clobber a swap the upgrade engine made in between.
    #[tokio::test]
    async fn repoint_selection_if_writes_only_over_the_expected_entry() {
        let store = Store::in_memory();
        let entry = |h: &str, p: &str| SelectionEntry {
            hash: h.into(),
            file_path: p.into(),
        };
        let slot = episode_slot(7, 1, 1);
        store
            .put_selection(slot.clone(), entry("old", "a.mkv"))
            .await
            .unwrap();
        assert!(store
            .repoint_selection_if(slot.clone(), entry("old", "a.mkv"), entry("new", "b.mkv"))
            .await
            .unwrap());
        assert_eq!(
            store.get_selection(slot.clone()).await,
            Some(entry("new", "b.mkv"))
        );
        // The slot moved on since the plan → no write.
        assert!(!store
            .repoint_selection_if(slot.clone(), entry("old", "a.mkv"), entry("other", "c.mkv"))
            .await
            .unwrap());
        assert_eq!(
            store.get_selection(slot.clone()).await,
            Some(entry("new", "b.mkv"))
        );
        // A slot cleared since the plan is not re-created.
        let gone = episode_slot(7, 1, 2);
        assert!(!store
            .repoint_selection_if(gone.clone(), entry("old", "a.mkv"), entry("new", "b.mkv"))
            .await
            .unwrap());
        assert_eq!(store.get_selection(gone).await, None);
    }

    #[tokio::test]
    async fn set_imdb_id_if_empty_fills_only_an_empty_imdb_id() {
        let store = Store::in_memory();
        let rec = OwnedRecord {
            request: req("", 603),
            provenance: Provenance { entries: vec![] },
            added_at: 100,
            status: OwnedStatus::Verified,
            provides: vec![(1, 2)],
            quality: None,
        };
        store.put_owned("hempty".into(), rec.clone()).await.unwrap();
        store
            .put_owned(
                "hset".into(),
                OwnedRecord {
                    request: req("tt9", 603),
                    ..rec.clone()
                },
            )
            .await
            .unwrap();
        assert!(store
            .set_imdb_id_if_empty("HEMPTY".into(), "tt0133093".into())
            .await
            .unwrap());
        let got = store.get_owned("hempty".into()).await.unwrap();
        assert_eq!(got.request.imdb_id, "tt0133093");
        assert_eq!(got.provides, vec![(1, 2)], "other fields intact");
        assert_eq!(got.added_at, 100);
        assert!(
            !store
                .set_imdb_id_if_empty("hset".into(), "tt0133093".into())
                .await
                .unwrap(),
            "a recorded IMDB id is never overwritten"
        );
        assert_eq!(
            store
                .get_owned("hset".into())
                .await
                .unwrap()
                .request
                .imdb_id,
            "tt9"
        );
        assert!(
            !store
                .set_imdb_id_if_empty("missing".into(), "tt1".into())
                .await
                .unwrap(),
            "a missing record is never resurrected"
        );
        assert!(store.get_owned("missing".into()).await.is_none());
    }

    fn wanted_row(user: &str, tmdb_id: u64) -> WantedRecord {
        WantedRecord {
            user: user.into(),
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

    fn tokens_for(user: &str) -> TraktTokens {
        TraktTokens {
            access: "a".into(),
            refresh: "r".into(),
            expires_at: 0,
            username: user.into(),
            needs_reenrolment: false,
        }
    }

    #[tokio::test]
    async fn replace_user_wanted_if_enrolled_writes_nothing_for_a_removed_account() {
        let store = Store::in_memory();
        assert!(!store
            .replace_user_wanted_if_enrolled("alice".into(), vec![], vec![wanted_row("alice", 1)])
            .await
            .unwrap());
        assert!(
            store.all_wanted().await.is_empty(),
            "no row is written for an account without tokens"
        );

        store
            .put_trakt_tokens("alice".into(), tokens_for("alice"))
            .await
            .unwrap();
        store.put_wanted(wanted_row("alice", 1)).await.unwrap();
        store.put_wanted(wanted_row("bob", 1)).await.unwrap();
        assert!(store
            .replace_user_wanted_if_enrolled(
                "alice".into(),
                vec![(MediaType::Movie, 1)],
                vec![wanted_row("alice", 2)]
            )
            .await
            .unwrap());
        let mut rows: Vec<(String, u64)> = store
            .all_wanted()
            .await
            .into_iter()
            .map(|r| (r.user, r.tmdb_id))
            .collect();
        rows.sort();
        assert_eq!(
            rows,
            vec![("alice".to_string(), 2), ("bob".to_string(), 1)],
            "the pruned key is gone, the new row written, another user's row untouched"
        );
    }

    #[tokio::test]
    async fn remove_trakt_account_drops_the_tokens_and_only_that_users_wanted_rows() {
        let store = Store::in_memory();
        store
            .put_trakt_tokens("alice".into(), tokens_for("alice"))
            .await
            .unwrap();
        store
            .put_trakt_tokens("bob".into(), tokens_for("bob"))
            .await
            .unwrap();
        store.put_wanted(wanted_row("alice", 1)).await.unwrap();
        store.put_wanted(wanted_row("alice", 2)).await.unwrap();
        store.put_wanted(wanted_row("bob", 1)).await.unwrap();
        store.remove_trakt_account("alice".into()).await.unwrap();
        assert!(store.get_trakt_tokens("alice".into()).await.is_none());
        assert!(store.get_trakt_tokens("bob".into()).await.is_some());
        let rows: Vec<String> = store
            .all_wanted()
            .await
            .into_iter()
            .map(|r| r.user)
            .collect();
        assert_eq!(rows, vec!["bob".to_string()]);
    }

    /// The quality backfill (re-deriving a `quality: None` record's quality from its file name) sets
    /// `quality` ONLY where it is still `None`, in one write transaction touching nothing else — a
    /// recorded quality is never overwritten and a missing record is never resurrected.
    #[tokio::test]
    async fn set_quality_if_unset_fills_only_a_missing_quality() {
        let store = Store::in_memory();
        let rec = OwnedRecord {
            request: req("tt1", 27205),
            provenance: Provenance { entries: vec![] },
            added_at: 100,
            status: OwnedStatus::Verified,
            provides: vec![(1, 2)],
            quality: None,
        };
        store.put_owned("hnone".into(), rec.clone()).await.unwrap();
        let recorded = crate::release::QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 720,
            score: 7,
            unverified: false,
        };
        store
            .put_owned(
                "hsome".into(),
                OwnedRecord {
                    quality: Some(recorded.clone()),
                    ..rec.clone()
                },
            )
            .await
            .unwrap();
        let derived = crate::release::QualitySummary {
            cached: true,
            source_tier: 8_000,
            resolution: 2160,
            score: 9,
            unverified: false,
        };
        assert!(store
            .set_quality_if_unset("HNONE".into(), derived.clone())
            .await
            .unwrap());
        let got = store.get_owned("hnone".into()).await.unwrap();
        assert_eq!(got.quality, Some(derived.clone()));
        assert_eq!(got.provides, vec![(1, 2)], "other fields intact");
        assert_eq!(got.added_at, 100);
        assert!(got.provenance.entries.is_empty());
        assert!(!store
            .set_quality_if_unset("hsome".into(), derived.clone())
            .await
            .unwrap());
        assert_eq!(
            store.get_owned("hsome".into()).await.unwrap().quality,
            Some(recorded),
            "a recorded quality is never overwritten"
        );
        assert!(!store
            .set_quality_if_unset("hgone".into(), derived)
            .await
            .unwrap());
        assert!(store.get_owned("hgone".into()).await.is_none());
    }

    /// The provides backfill (re-deriving an empty `provides` from a show torrent's file names) sets
    /// `provides` ONLY on a `Verified` show record whose `provides` is still empty, in one write
    /// transaction touching nothing else: a recorded set is never overwritten, a `Pending` record
    /// (whose `provides` `observe` settles) or a movie is left alone, and a missing record is never
    /// resurrected.
    #[tokio::test]
    async fn set_provides_if_empty_fills_only_an_empty_verified_show_record() {
        let store = Store::in_memory();
        let show = OwnedRecord {
            request: AcquireRequest {
                kind: MediaKind::Series,
                ..req("tt1", 1396)
            },
            provenance: Provenance { entries: vec![] },
            added_at: 100,
            status: OwnedStatus::Verified,
            provides: vec![],
            quality: None,
        };
        store
            .put_owned("hempty".into(), show.clone())
            .await
            .unwrap();
        store
            .put_owned(
                "hset".into(),
                OwnedRecord {
                    provides: vec![(1, 1)],
                    ..show.clone()
                },
            )
            .await
            .unwrap();
        store
            .put_owned(
                "hpending".into(),
                OwnedRecord {
                    status: OwnedStatus::Pending,
                    ..show.clone()
                },
            )
            .await
            .unwrap();
        store
            .put_owned(
                "hmovie".into(),
                OwnedRecord {
                    request: req("tt2", 27205),
                    ..show.clone()
                },
            )
            .await
            .unwrap();
        let derived = vec![(1, 1), (1, 2)];
        assert!(store
            .set_provides_if_empty("HEMPTY".into(), derived.clone())
            .await
            .unwrap());
        let got = store.get_owned("hempty".into()).await.unwrap();
        assert_eq!(got.provides, derived);
        assert_eq!(got.added_at, 100, "other fields intact");
        assert_eq!(got.status, OwnedStatus::Verified);
        for (hash, expected) in [
            ("hset", vec![(1, 1)]),
            ("hpending", vec![]),
            ("hmovie", vec![]),
        ] {
            assert!(
                !store
                    .set_provides_if_empty(hash.into(), derived.clone())
                    .await
                    .unwrap(),
                "{hash} must not be written"
            );
            assert_eq!(
                store.get_owned(hash.into()).await.unwrap().provides,
                expected,
                "{hash} keeps its provides"
            );
        }
        assert!(!store
            .set_provides_if_empty("hgone".into(), derived.clone())
            .await
            .unwrap());
        assert!(store.get_owned("hgone".into()).await.is_none());
        assert!(
            !store
                .set_provides_if_empty("hempty2".into(), vec![])
                .await
                .unwrap(),
            "an empty set is never written"
        );
    }

    /// A new-format record with multi-user provenance must survive a put→get round-trip
    /// through the store with all entries intact.
    #[tokio::test]
    async fn provenance_round_trips_through_store() {
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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

    // ── OwnedRecord provides/quality, selection, upgrade_checks tests ─────────

    #[tokio::test]
    async fn owned_record_provides_and_quality_round_trip() {
        let store = Store::in_memory();
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
                unverified: false,
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

    /// `mark_verified_if_present` settles a record in one transaction and never writes back one
    /// removed meanwhile (dedup, Trigger A/B, a reap all remove records concurrently with `observe`).
    #[tokio::test]
    async fn mark_verified_if_present_never_resurrects_a_removed_record() {
        let store = Store::in_memory();
        let rec = OwnedRecord {
            request: req("tt1", 1396),
            provenance: Provenance::manual(),
            added_at: 100,
            status: OwnedStatus::Pending,
            provides: vec![(1, 1)],
            quality: Some(crate::release::QualitySummary {
                cached: false,
                source_tier: 6_000,
                resolution: 1080,
                score: 42,
                unverified: false,
            }),
        };
        store.put_owned("H1".into(), rec.clone()).await.unwrap();
        let written = store
            .mark_verified_if_present("h1".into(), Some(vec![(1, 1), (1, 2)]), true)
            .await
            .unwrap()
            .expect("present");
        let got = store.get_owned("h1".into()).await.unwrap();
        assert_eq!(got.status, OwnedStatus::Verified);
        assert_eq!(got.provides, vec![(1, 1), (1, 2)]);
        let q = got.quality.unwrap();
        assert!(q.cached && q.unverified, "{q:?}");
        assert_eq!(written, Some(q));
        // A movie keeps its `provides`.
        store.put_owned("h2".into(), rec).await.unwrap();
        store
            .mark_verified_if_present("h2".into(), None, false)
            .await
            .unwrap();
        assert_eq!(
            store.get_owned("h2".into()).await.unwrap().provides,
            vec![(1, 1)]
        );

        // Removed between `observe`'s read and its write: nothing comes back.
        store.remove_owned("h1".into()).await.unwrap();
        assert_eq!(
            store
                .mark_verified_if_present("h1".into(), Some(vec![(1, 1)]), false)
                .await
                .unwrap(),
            None
        );
        assert!(store.get_owned("h1".into()).await.is_none());
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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
        let store = Store::in_memory();
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
