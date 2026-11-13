//! Helpers shared by the live (`#[ignore]`) integration-test binaries. Each binary that uses them
//! declares `mod common;`. Not every binary uses every helper, hence the `dead_code` allowance.
#![allow(dead_code)]

use debridmoviemapper::provider::DebridProvider;
use debridmoviemapper::rd_client::TorrentInfo;
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

/// A trimmed, non-empty env var (after loading `.env`), or `None` when it is unset or blank, so a
/// live test can skip when its provider token isn't configured.
pub fn token(var: &str) -> Option<String> {
    dotenvy::dotenv().ok();
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

/// Best effort: delete every torrent whose hash is one of `hashes` (compared case-insensitively),
/// from a single listing.
pub async fn delete_by_hashes(provider: &Arc<dyn DebridProvider>, hashes: &[String]) {
    if hashes.is_empty() {
        return;
    }
    let Ok(torrents) = provider.get_torrents().await else {
        return;
    };
    for t in torrents
        .iter()
        .filter(|t| hashes.iter().any(|h| t.hash.eq_ignore_ascii_case(h)))
    {
        let _ = provider.delete_torrent(&t.id).await;
    }
}

/// Best effort: delete every torrent whose hash is `hash`, e.g. a copy left by an interrupted run.
pub async fn cleanup_by_hash(provider: &Arc<dyn DebridProvider>, hash: &str) {
    delete_by_hashes(provider, &[hash.to_string()]).await;
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

/// Add Sintel by magnet, select only its video file(s), and wait until it is `downloaded`.
///
/// Real-Debrid converts the magnet before it lists files; TorBox lists them at once. Selecting
/// non-video files (subtitles, poster) makes Real-Debrid return a link array that doesn't line up
/// with the selected files, and production torrents select just the video.
pub async fn add_sintel_downloaded(provider: &Arc<dyn DebridProvider>, label: &str) -> TorrentInfo {
    let id = provider
        .add_magnet(&sintel_magnet())
        .await
        .expect("add_magnet")
        .id;
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
