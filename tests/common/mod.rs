//! Helpers shared by the live (`#[ignore]`) integration-test binaries. Each binary that uses them
//! declares `mod common;`. Not every binary uses every helper, hence the `dead_code` allowance.
#![allow(dead_code)]

use debridmoviemapper::error::AppError;
use debridmoviemapper::provider::{DebridProvider, FileLocator};
use debridmoviemapper::rd_client::{AddMagnetResponse, Torrent, TorrentInfo};
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::is_video_file;
use futures_util::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

/// Blender's *Sintel*: Creative-Commons, a single ~129 MB `.mp4`, widely seeded and cached on both
/// providers. The tests that change the live account add and delete this torrent and never touch
/// any other torrent in it.
pub const SINTEL_HASH: &str = "08ada5a7a6183aae1e09d831df6748d566095a10";

pub fn sintel_magnet() -> String {
    format!(
        "magnet:?xt=urn:btih:{}&dn=Sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce",
        SINTEL_HASH
    )
}

/// Load `.env` once per test binary, through the service's own loader (`config::load_dotenv`), and
/// panic with its message (which never repeats the offending line) when the file exists but can't
/// be loaded. dotenvy stops at the first line it can't parse and leaves every later one unset, so
/// discarding that error made a live test whose token sat below a bad line skip and report `ok`:
/// the gate passed having tested nothing, on a file the service refuses to start with. Every live
/// test loads `.env` through this (`config::tests::live_tests_load_env_only_through_load_dotenv`).
pub fn load_env() {
    static LOADED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    if let Err(message) = LOADED.get_or_init(debridmoviemapper::config::load_dotenv) {
        panic!("{message}");
    }
}

/// A trimmed, non-empty env var (after loading `.env`, [`load_env`]), or `None` when it is unset or
/// blank, so a live test can skip when its provider token isn't configured. Trimmed like the
/// service trims its tokens (`provider::choose_provider`).
pub fn token(var: &str) -> Option<String> {
    load_env();
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A provider wrapper for live tests that must only READ the account: listings, lookups and file
/// resolution reach the wrapped provider, while `add_magnet`, `select_files` and `delete_torrent`
/// never do and return an error instead. Code under test that would change the account, such as the
/// instant repair a failed read triggers (a re-add by hash; on Real-Debrid it then deletes the
/// original), fails at that step and leaves the account as it was.
#[derive(Debug)]
pub struct ReadOnlyProvider(pub Arc<dyn DebridProvider>);

impl ReadOnlyProvider {
    /// The error every refused call returns: a `reqwest::Error` (the trait's error type) built
    /// without any I/O. The refusal is logged on stderr so a failed read explains itself.
    fn refuse(&self, call: &str) -> reqwest::Error {
        eprintln!(
            "read-only test provider: refused {call} on {} (this live test must not change the \
             account)",
            self.0.name()
        );
        reqwest::Client::new()
            .get("http://[")
            .build()
            .expect_err("an unparseable URL can't build a request")
    }
}

#[async_trait::async_trait]
impl DebridProvider for ReadOnlyProvider {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        self.0.get_torrents().await
    }
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        self.0.get_torrent_info(id).await
    }
    async fn add_magnet(&self, _magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        Err(self.refuse("add_magnet"))
    }
    async fn select_files(&self, torrent_id: &str, _file_ids: &str) -> Result<(), reqwest::Error> {
        Err(self.refuse(&format!("select_files({torrent_id})")))
    }
    async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
        Err(self.refuse(&format!("delete_torrent({torrent_id})")))
    }
    async fn resolve_url(&self, loc: &FileLocator) -> Result<String, AppError> {
        self.0.resolve_url(loc).await
    }
    async fn invalidate(&self, loc: &FileLocator) {
        self.0.invalidate(loc).await
    }
}

/// A TMDB client from `TMDB_API_KEY`. A missing or blank key fails the test rather than skipping
/// it: the live tests need TMDB, and a skip would let the gate pass without testing anything.
pub fn tmdb_client() -> Arc<TmdbClient> {
    let key = token("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
    Arc::new(TmdbClient::new(key).unwrap())
}

/// The ids of every torrent in the account whose hash is `hash` (compared case-insensitively), or
/// the listing's error, so an assertion can tell a failed listing from a missing torrent.
pub async fn find_ids_by_hash(
    provider: &Arc<dyn DebridProvider>,
    hash: &str,
) -> Result<Vec<String>, reqwest::Error> {
    Ok(provider
        .get_torrents()
        .await?
        .into_iter()
        .filter(|t| t.hash.eq_ignore_ascii_case(hash))
        .map(|t| t.id)
        .collect())
}

/// Call `list` (a listing of one hash's torrent ids, e.g. [`find_ids_by_hash`]) up to `attempts`
/// times, `every` apart, until it returns an id. TorBox's backend can lag right after an add: the
/// listing may leave out a torrent whose own lookup already reports `downloaded` (the same
/// inconsistency that makes `mylist?id=` 404 a torrent that exists, see `STAGED_INFO_BACKOFF` in
/// `src/upgrade.rs`), so one listing can't show the torrent is missing. On giving up, the error
/// says how many listings failed (with the last error) and how many left the torrent out.
pub async fn poll_until_listed<F, Fut, E>(
    mut list: F,
    attempts: u32,
    every: Duration,
) -> Result<Vec<String>, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Vec<String>, E>>,
    E: std::fmt::Display,
{
    let (mut missing, mut errored) = (0u32, 0u32);
    let mut last_error = None;
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(every).await;
        }
        match list().await {
            Ok(ids) if !ids.is_empty() => return Ok(ids),
            Ok(_) => missing += 1,
            Err(e) => {
                errored += 1;
                last_error = Some(e.to_string());
            }
        }
    }
    let outcome = match (missing, errored) {
        (_, 0) => format!("the torrent was missing from all {missing} listings"),
        (0, _) => format!("all {errored} listings errored"),
        _ => format!(
            "the torrent was missing from {missing} listings and {errored} listings errored"
        ),
    };
    let last = last_error
        .map(|e| format!("; last listing error: {e}"))
        .unwrap_or_default();
    Err(format!(
        "{outcome} ({attempts} polls, {}s apart){last}",
        every.as_secs()
    ))
}

/// How many listings a cleanup sweep makes at most, and how far apart.
const SWEEP_LISTINGS: u32 = 3;
const SWEEP_EVERY: Duration = Duration::from_secs(2);

/// The ids of the torrents a live test's body added, shared with its cleanup, so the cleanup can
/// delete them directly when the provider's listing lags the add (TorBox's can for 30 s and more).
pub type AddedIds = Arc<std::sync::Mutex<Vec<String>>>;

/// Remember a torrent id the body added (each id once).
pub fn record_added_id(added: &AddedIds, id: &str) {
    let mut ids = added.lock().unwrap_or_else(|e| e.into_inner());
    if !ids.iter().any(|known| known == id) {
        ids.push(id.to_string());
    }
}

/// The ids recorded so far.
pub fn added_ids(added: &AddedIds) -> Vec<String> {
    added.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The cleanup core: `delete` each known id in `ids` first (an already-deleted one just fails,
/// which is ignored), then sweep by `hashes`: up to `attempts` listings (`list` returns
/// `(id, hash)` pairs), `every` apart, deleting every listed torrent that carries one of `hashes`
/// (case-insensitively) or one of `ids`, until a listing shows none. `Err` says what it couldn't
/// confirm gone: a torrent still listed, or, when no listing succeeded, every hash and id.
pub async fn delete_ids_then_sweep_with<L, LFut, E, D, DFut>(
    mut list: L,
    mut delete: D,
    ids: &[String],
    hashes: &[String],
    attempts: u32,
    every: Duration,
) -> Result<(), String>
where
    L: FnMut() -> LFut,
    LFut: Future<Output = Result<Vec<(String, String)>, E>>,
    E: std::fmt::Display,
    D: FnMut(String) -> DFut,
    DFut: Future<Output = ()>,
{
    if ids.is_empty() && hashes.is_empty() {
        return Ok(());
    }
    for id in ids {
        delete(id.clone()).await;
    }
    let mut still_listed: Option<Vec<(String, String)>> = None;
    let mut last_error = None;
    for attempt in 0..attempts {
        if attempt > 0 {
            tokio::time::sleep(every).await;
        }
        match list().await {
            Ok(torrents) => {
                let ours: Vec<(String, String)> = torrents
                    .into_iter()
                    .filter(|(id, hash)| {
                        ids.contains(id) || hashes.iter().any(|h| hash.eq_ignore_ascii_case(h))
                    })
                    .collect();
                if ours.is_empty() {
                    return Ok(());
                }
                for (id, _) in &ours {
                    delete(id.clone()).await;
                }
                still_listed = Some(ours);
            }
            Err(e) => last_error = Some(e.to_string()),
        }
    }
    let what = match still_listed {
        Some(ours) => format!(
            "still listed: {}",
            ours.iter()
                .map(|(id, hash)| format!("id {id} (hash {hash})"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => format!("no listing succeeded; hashes {hashes:?}, ids {ids:?}"),
    };
    let last = last_error
        .map(|e| format!("; last listing error: {e}"))
        .unwrap_or_default();
    Err(format!(
        "could not confirm the cleanup after {attempts} listings {}s apart, {what}{last}",
        every.as_secs()
    ))
}

/// Best effort: delete the torrents with the known `ids`, then every torrent whose hash is one of
/// `hashes` (compared case-insensitively), re-listing a few times. Warns on stderr, naming them,
/// when it can't confirm they are gone, since a leftover stays in an account a production
/// deployment may share.
pub async fn delete_ids_then_sweep(
    provider: &Arc<dyn DebridProvider>,
    ids: &[String],
    hashes: &[String],
) {
    let outcome = delete_ids_then_sweep_with(
        || async {
            provider.get_torrents().await.map(|torrents| {
                torrents
                    .into_iter()
                    .map(|t| (t.id, t.hash))
                    .collect::<Vec<_>>()
            })
        },
        |id: String| async move {
            let _ = provider.delete_torrent(&id).await;
        },
        ids,
        hashes,
        SWEEP_LISTINGS,
        SWEEP_EVERY,
    )
    .await;
    if let Err(why) = outcome {
        eprintln!("WARNING: live-test cleanup may have left a torrent behind: {why}");
    }
}

/// Best effort: delete every torrent whose hash is one of `hashes` (see [`delete_ids_then_sweep`]).
pub async fn delete_by_hashes(provider: &Arc<dyn DebridProvider>, hashes: &[String]) {
    delete_ids_then_sweep(provider, &[], hashes).await;
}

/// Best effort: delete every torrent whose hash is `hash`, e.g. a copy left by an interrupted run.
pub async fn cleanup_by_hash(provider: &Arc<dyn DebridProvider>, hash: &str) {
    delete_by_hashes(provider, &[hash.to_string()]).await;
}

/// Best effort: delete the torrents the body recorded in `added`, then every torrent whose hash is
/// `hash` (see [`delete_ids_then_sweep`]).
pub async fn cleanup_added(provider: &Arc<dyn DebridProvider>, added: &AddedIds, hash: &str) {
    delete_ids_then_sweep(provider, &added_ids(added), &[hash.to_string()]).await;
}

/// Run `body`, then run `cleanup` whether or not `body` panicked, and only then re-raise the panic.
/// A failed assertion in a live test would otherwise skip its cleanup and leave a torrent behind in
/// an account a production deployment may share.
pub async fn with_cleanup<T>(
    body: impl Future<Output = T>,
    cleanup: impl Future<Output = ()>,
) -> T {
    let outcome = AssertUnwindSafe(body).catch_unwind().await;
    cleanup.await;
    match outcome {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Add Sintel by magnet, select only its video file(s), and wait until it is `downloaded`. The new
/// torrent's id goes into `added` right after the add, so the cleanup can delete it even when this
/// panics before the listing shows it.
///
/// Real-Debrid converts the magnet before it lists files; TorBox lists them at once. Selecting
/// non-video files (subtitles, poster) makes Real-Debrid return a link array that doesn't line up
/// with the selected files, and production torrents select just the video.
pub async fn add_sintel_downloaded(
    provider: &Arc<dyn DebridProvider>,
    added: &AddedIds,
    label: &str,
) -> TorrentInfo {
    let id = provider
        .add_magnet(&sintel_magnet())
        .await
        .expect("add_magnet")
        .id;
    record_added_id(added, &id);
    println!("[{label}] add_magnet -> id={id}");

    let mut pre = None;
    for _ in 0..40 {
        if let Ok(info) = provider.get_torrent_info(&id).await {
            if !info.files.is_empty() {
                pre = Some(info);
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let pre = pre.expect("torrent file list should become available");
    let video_ids: Vec<String> = pre
        .files
        .iter()
        .filter(|f| is_video_file(&f.path))
        .map(|f| f.id.to_string())
        .collect();
    assert!(
        !video_ids.is_empty(),
        "[{label}] torrent should contain a video file"
    );
    provider
        .select_files(&id, &video_ids.join(","))
        .await
        .expect("select_files");

    // Sintel is widely cached, so this is usually near-instant.
    for _ in 0..40 {
        match provider.get_torrent_info(&id).await {
            Ok(info) if info.status == "downloaded" => {
                println!("[{label}] downloaded; {} file(s)", info.files.len());
                return info;
            }
            Ok(info) => println!("[{label}] status={} — waiting", info.status),
            Err(e) => println!("[{label}] get_torrent_info error: {e} — retrying"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("[{label}] torrent should reach 'downloaded' (Sintel is widely cached)");
}

/// Percent-encode each segment of a WebDAV path, keeping the `/` separators.
pub fn encode_path_preserve_slashes(p: &str) -> String {
    p.split('/')
        .map(|seg| urlencoding::encode(seg).to_string())
        .collect::<Vec<_>>()
        .join("/")
}
