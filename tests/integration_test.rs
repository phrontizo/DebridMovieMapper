//! Live identification and VFS tests (`#[ignore]`), run against every configured provider:
//! Real-Debrid when `RD_API_TOKEN` is set and TorBox when `TORBOX_API_KEY` is set (a blank token
//! counts as unset). They need `TMDB_API_KEY` too, skip when neither provider token is set, and only
//! read the account: every provider call goes through `common::ReadOnlyProvider`.
//! `INTEGRATION_TEST_LIMIT=N` identifies only the first N downloaded torrents of each account.
//!
//! Each provider's identifications are cached between runs in its own `Store` under Cargo's scratch
//! directory for integration tests (`CARGO_TARGET_TMPDIR`), never in the service's own
//! `metadata.db`. A TMDB search that fails fails the run and is never cached, a torrent cached
//! as unidentified is identified again, and an identified torrent cached with no streamable video
//! file has its files looked up again (see [`fetch_and_identify`]).

mod common;

use common::{label, token_var, INFO_NOT_FOUND_BACKOFF};
use debridmoviemapper::error::AppError;
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::provider::{
    torrent_info_retrying_not_found, DebridProvider, FileLocator, ProviderKind,
};
use debridmoviemapper::rd_client::{AddMagnetResponse, Torrent, TorrentFile, TorrentInfo};
use debridmoviemapper::store::Store;
use debridmoviemapper::tmdb_client::track_search_failures;
use debridmoviemapper::vfs::{
    is_video_file, DebridVfs, EpisodeNames, MediaMetadata, MediaType, VfsNode,
};
use futures_util::FutureExt;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use tokio::sync::OnceCell;

/// Identified torrents: each torrent's info with the metadata identification gave it.
type Identified = Vec<(TorrentInfo, MediaMetadata)>;

/// The providers the tests run against, each only when its token is set.
const PROVIDERS: [ProviderKind; 2] = [ProviderKind::RealDebrid, ProviderKind::TorBox];

/// Each provider's identified data, fetched and identified once and reused by every test. `None`
/// when the provider's token is unset or blank.
static RD_DATA: OnceCell<Option<Identified>> = OnceCell::const_new();
static TORBOX_DATA: OnceCell<Option<Identified>> = OnceCell::const_new();

/// The provider's identification cache. Each provider has its own, since the cache is keyed by
/// torrent id and the two providers' ids are unrelated.
fn cache_path(kind: ProviderKind) -> String {
    format!(
        "{}/integration_test_cache_{}.redb",
        env!("CARGO_TARGET_TMPDIR"),
        label(kind)
    )
}

/// The provider's identified data, or `None` when its token is unset or blank.
async fn identified(kind: ProviderKind) -> Option<&'static Identified> {
    let cell = match kind {
        ProviderKind::RealDebrid => &RD_DATA,
        ProviderKind::TorBox => &TORBOX_DATA,
    };
    cell.get_or_init(|| async move {
        tracing_subscriber::fmt::try_init().ok();
        let token = common::token(token_var(kind))?;
        let tmdb_client = common::tmdb_client();
        let provider = common::ReadOnlyProvider(common::client(kind, token));
        let store = Store::open(&cache_path(kind))
            .expect("Failed to open the integration-test identification cache");
        let limit = std::env::var("INTEGRATION_TEST_LIMIT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok());

        Some(
            fetch_and_identify(&provider, &store, limit, |info| {
                let tmdb_client = tmdb_client.clone();
                async move { track_search_failures(identify_torrent(&info, &tmdb_client)).await }
            })
            .await,
        )
    })
    .await
    .as_ref()
}

/// Run `check` on the identified data of every provider whose token is set, or say that `test` is
/// skipped when neither is.
async fn for_each_provider(test: &str, check: impl Fn(&str, &[(TorrentInfo, MediaMetadata)])) {
    let mut ran = false;
    for kind in PROVIDERS {
        if let Some(data) = identified(kind).await {
            println!("\n##### {test} on {} #####", label(kind));
            check(label(kind), data);
            ran = true;
        }
    }
    if !ran {
        println!("skipping {test}: neither RD_API_TOKEN nor TORBOX_API_KEY is set");
    }
}

/// Fetch the downloaded torrents (the first `limit` when set) and identify them with `identify`,
/// which returns the metadata and whether a TMDB search failed, using the store's identification
/// cache.
///
/// A failed TMDB search (an outage, a rejected `TMDB_API_KEY`) panics before its name-derived
/// fallback is cached, as the scan loop never caches one either: a cached fallback would be served
/// on every later run, failing `test_identification` long after TMDB recovered. A torrent cached
/// as unidentified (by a run that predates that rule, or a genuine no-match) is identified again,
/// at the cost of one TMDB search ladder per such torrent per run.
///
/// An identified torrent whose cached file list holds no streamable video file (cached while
/// TorBox listed it with `files: null`, or while its cache had lapsed) has its files looked up
/// again ([`refresh_fileless`]), so the VFS checks never cover such a torrent with no files for
/// good. Every other identified torrent is served from the cache with no lookup.
async fn fetch_and_identify<F, Fut>(
    provider: &dyn DebridProvider,
    store: &Store,
    limit: Option<usize>,
    identify: F,
) -> Identified
where
    F: Fn(TorrentInfo) -> Fut,
    Fut: Future<Output = (MediaMetadata, bool)>,
{
    println!("Fetching torrents...");
    let torrents = provider
        .get_torrents()
        .await
        .expect("Failed to get torrents");
    let downloaded: Vec<_> = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .collect();
    println!("Found {} downloaded torrents", downloaded.len());

    let to_process: Vec<_> = if let Some(limit) = limit {
        println!(
            "INTEGRATION_TEST_LIMIT={}, processing first {} torrents",
            limit, limit
        );
        downloaded.into_iter().take(limit).collect()
    } else {
        downloaded
    };

    let mut current_data = Vec::new();
    let mut to_identify = Vec::new();

    for torrent in &to_process {
        match store.get_match(torrent.id.clone()).await {
            Some(data) if data.1.external_id.is_some() && has_streamable_video(&data.0) => {
                println!("Cached: {} (ID: {:?})", data.0.filename, data.1.external_id);
                current_data.push(data);
            }
            Some(data) if data.1.external_id.is_some() => {
                current_data.push(refresh_fileless(provider, store, &torrent.id, data).await);
            }
            Some(data) => {
                println!(
                    "Cached as unidentified, identifying again: {}",
                    data.0.filename
                );
                to_identify.push(torrent.clone());
            }
            None => to_identify.push(torrent.clone()),
        }
    }

    if to_identify.is_empty() {
        println!("All {} torrents served from cache.", current_data.len());
        return current_data;
    }
    println!(
        "Identifying {} torrents ({} served from the cache)...",
        to_identify.len(),
        current_data.len()
    );
    for torrent in to_identify {
        let info = torrent_info_retrying_not_found(provider, &torrent.id, &INFO_NOT_FOUND_BACKOFF)
            .await
            .unwrap_or_else(|e| panic!("Failed to get torrent info for {}: {e}", torrent.id));
        let (metadata, tmdb_failed) = identify(info.clone()).await;
        assert!(
            !tmdb_failed,
            "A TMDB search failed while identifying {} (check TMDB_API_KEY and TMDB's status); \
             its fallback identification was not cached",
            info.filename
        );
        println!(
            "Identified: {} as {:?} (ID: {:?})",
            info.filename, metadata.media_type, metadata.external_id
        );
        if let Err(e) = store
            .put_matches(vec![(torrent.id, info.clone(), metadata.clone())])
            .await
        {
            println!("WARNING: failed to cache identification: {e}");
        }
        current_data.push((info, metadata));
    }
    current_data
}

/// Whether the torrent lists a video file the VFS can stream ([`split_by_link`]).
fn has_streamable_video(info: &TorrentInfo) -> bool {
    !split_by_link(info).0.is_empty()
}

/// The cached identification `cached` of torrent `id`, whose file list holds no streamable video
/// file, with the torrent's current files when they hold one: those replace the cached list in the
/// returned data and in the cache, and the identification is kept. A lookup that fails or still
/// lists none changes nothing, so an empty list never replaces a cached one.
async fn refresh_fileless(
    provider: &dyn DebridProvider,
    store: &Store,
    id: &str,
    cached: (TorrentInfo, MediaMetadata),
) -> (TorrentInfo, MediaMetadata) {
    match torrent_info_retrying_not_found(provider, id, &INFO_NOT_FOUND_BACKOFF).await {
        Ok(fresh) if has_streamable_video(&fresh) => {
            println!(
                "Cached with no video file, now lists its files: {} (ID: {:?})",
                fresh.filename, cached.1.external_id
            );
            let metadata = cached.1;
            if let Err(e) = store
                .put_matches(vec![(id.to_string(), fresh.clone(), metadata.clone())])
                .await
            {
                println!("WARNING: failed to cache the files of {id}: {e}");
            }
            (fresh, metadata)
        }
        Ok(_) => {
            println!(
                "Cached with no video file, still lists none: {} (ID: {:?})",
                cached.0.filename, cached.1.external_id
            );
            cached
        }
        Err(e) => {
            println!(
                "WARNING: could not look up the files of {id} ({}) again: {e}; using the cached \
                 entry",
                cached.0.filename
            );
            cached
        }
    }
}

/// Test 1: Torrent fetching and TMDB identification only. No VFS, no unrestrict.
/// Uses the identification cache so repeat runs are fast.
#[tokio::test]
#[ignore]
async fn test_identification() {
    for_each_provider("test_identification", check_identification).await;
}

fn check_identification(label: &str, data: &[(TorrentInfo, MediaMetadata)]) {
    let identified = data.iter().filter(|(_, m)| m.external_id.is_some()).count();
    let unidentified = data.len() - identified;

    println!("\n=== Identification Summary ({label}) ===");
    println!("Total torrents: {}", data.len());
    println!("Identified:     {}", identified);
    println!("Unidentified:   {}", unidentified);

    if unidentified > 0 {
        println!("\nUnidentified torrents:");
        for (info, _) in data.iter().filter(|(_, m)| m.external_id.is_none()) {
            println!("  - {}", info.filename);
        }
    }

    assert!(
        !data.is_empty(),
        "[{label}] Should have at least one downloaded torrent"
    );
    assert!(
        identified > 0,
        "[{label}] Should identify at least one torrent"
    );
}

/// Test 2: Given identified data (from the identification cache), builds VFS and checks directory
/// structure.
#[tokio::test]
#[ignore]
async fn test_vfs_structure() {
    for_each_provider("test_vfs_structure", check_vfs_structure).await;
}

fn check_vfs_structure(label: &str, data: &[(TorrentInfo, MediaMetadata)]) {
    assert!(
        !data.is_empty(),
        "[{label}] Need identified torrents to test VFS structure"
    );

    println!("Building VFS...");
    let vfs = DebridVfs::build(data.to_vec(), &debridmoviemapper::vfs::SelectionMap::new());

    // Root must be a directory
    let children = match &vfs.root {
        VfsNode::Directory { children } => children,
        _ => panic!("[{label}] Root should be a directory"),
    };

    println!("\n=== VFS Structure ({label}) ===");
    println!("Root entries: {:?}", children.keys().collect::<Vec<_>>());

    // Check for Movies/ and/or Shows/ directories
    let has_movies = children.contains_key("Movies");
    let has_shows = children.contains_key("Shows");
    println!("Has Movies/: {}", has_movies);
    println!("Has Shows/:  {}", has_shows);

    assert!(
        has_movies || has_shows,
        "[{label}] VFS should contain at least Movies/ or Shows/ directory"
    );

    // Check Movies structure: Movies/<Title (Year)>/<file>.mkv|.mp4
    if let Some(VfsNode::Directory { children: movies }) = children.get("Movies") {
        println!("\nMovies/ contains {} entries", movies.len());
        for (name, node) in movies.iter().take(5) {
            match node {
                VfsNode::Directory {
                    children: movie_files,
                } => {
                    let media_count = movie_files
                        .values()
                        .filter(|n| matches!(n, VfsNode::MediaFile { .. }))
                        .count();
                    println!("  {}: {} media files", name, media_count);
                }
                _ => println!("  {} (not a directory)", name),
            }
        }
    }

    // Check Shows structure: Shows/<Title (Year)>/Season XX/<file>.mkv|.mp4
    if let Some(VfsNode::Directory { children: shows }) = children.get("Shows") {
        println!("\nShows/ contains {} entries", shows.len());
        for (name, node) in shows.iter().take(5) {
            match node {
                VfsNode::Directory { children: seasons } => {
                    let season_count = seasons.keys().filter(|k| k.starts_with("Season")).count();
                    println!("  {}: {} seasons", name, season_count);
                }
                _ => println!("  {} (not a directory)", name),
            }
        }
    }
}

/// Test 3: the VFS shows every video file it is meant to (see [`check_completeness`]). A file
/// missing for any reason other than a design rule is a build bug and fails the test, as does a
/// selected-file/link-count mismatch on more than `MAX_UNLINKED_RATE` of the selected video files
/// (Real-Debrid only: TorBox lists no per-file links, so none of its files can lack one).
#[tokio::test]
#[ignore]
async fn test_vfs_completeness() {
    for_each_provider("test_vfs_completeness", check_vfs_completeness).await;
}

fn check_vfs_completeness(label: &str, data: &[(TorrentInfo, MediaMetadata)]) {
    /// The share of selected video files that may lack a link (a provider-side anomaly; a much
    /// higher share suggests the client mis-parses the links).
    const MAX_UNLINKED_RATE: f64 = 0.20;

    assert!(
        !data.is_empty(),
        "[{label}] Need identified torrents to test VFS completeness"
    );

    println!("Building VFS...");
    let vfs = DebridVfs::build(data.to_vec(), &debridmoviemapper::vfs::SelectionMap::new());
    let report = check_completeness(data, &vfs.root);

    let unlinked_rate = if report.selected_videos > 0 {
        report.unlinked.len() as f64 / report.selected_videos as f64
    } else {
        0.0
    };
    println!("\n=== VFS Completeness ({label}) ===");
    println!("Titles:                 {}", report.titles);
    println!("Selected video files:   {}", report.selected_videos);
    println!("Shown in the VFS:       {}", report.shown);
    println!(
        "Hidden by design:       {} (a movie's other copies, same-named copies in a season folder)",
        report.hidden
    );
    println!(
        "Without a link:         {} ({:.1}%; selected-file/link-count mismatch)",
        report.unlinked.len(),
        unlinked_rate * 100.0
    );
    println!("Missing (build bugs):   {}", report.missing.len());
    for u in &report.unlinked {
        println!("  WARNING: no link: {}", u);
    }
    for m in &report.missing {
        println!("  ERROR: {}", m);
    }

    assert!(
        report.missing.is_empty(),
        "[{label}] {} streamable video file(s) are missing from the VFS although no other copy of \
         them is shown (a movie's other copies and same-named copies in the same season folder \
         are already excluded), so the VFS build dropped them: {:#?}",
        report.missing.len(),
        report.missing
    );
    assert!(
        unlinked_rate <= MAX_UNLINKED_RATE,
        "[{label}] {}/{} selected video files ({:.1}% > {:.0}%) have no link at their position \
         among the selected files, so the VFS can't stream them. A selected-file/link-count \
         mismatch this common suggests the client mis-parses the links rather than the odd \
         provider anomaly.",
        report.unlinked.len(),
        report.selected_videos,
        unlinked_rate * 100.0,
        MAX_UNLINKED_RATE * 100.0
    );
}

/// Test 4: canonical episode names (`EPISODE_FILE_NAMES`'s default) lose no episode. The library
/// is built twice with no managed selection, with the torrents' own names and with canonical names,
/// and every episode an original name shows (its first `SxxEyy` code) must have a file in the same
/// season folder under canonical names. Every canonical name's code must name that folder's season.
#[tokio::test]
#[ignore]
async fn test_vfs_canonical_names() {
    for_each_provider("test_vfs_canonical_names", |label, data| {
        let problems = canonical_name_problems(data);
        println!(
            "\n=== Canonical names ({label}) === {} problem(s)",
            problems.len()
        );
        assert!(
            problems.is_empty(),
            "[{label}] canonical names lost or misplaced episodes: {problems:#?}"
        );
    })
    .await;
}

/// What [`test_vfs_canonical_names`] checks, as a list of problems (empty when there are none).
fn canonical_name_problems(data: &[(TorrentInfo, MediaMetadata)]) -> Vec<String> {
    static CODE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)s(\d{1,2})[ ._]?e(\d{1,3})").unwrap());
    static CANONICAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:^| - )S(\d{2,})E(\d{2,})(?:-E(\d{2,}))?\.[a-z0-9]+$").unwrap()
    });
    let number = |c: &regex::Captures, i: usize| -> u32 { c[i].parse().unwrap_or(0) };
    let none = debridmoviemapper::vfs::SelectionMap::new();
    let original = season_files(&DebridVfs::build(data.to_vec(), &none).root);
    let canonical =
        season_files(&DebridVfs::build_with(data.to_vec(), &none, EpisodeNames::Canonical).root);
    let mut problems = Vec::new();
    for ((show, season), names) in &original {
        let folder_season: u32 = season.trim_start_matches("Season ").parse().unwrap_or(0);
        let mut shown: HashSet<(u32, u32)> = HashSet::new();
        for name in canonical
            .get(&(show.clone(), season.clone()))
            .into_iter()
            .flatten()
        {
            if let Some(c) = CANONICAL.captures(name) {
                let (s, first) = (number(&c, 1), number(&c, 2));
                let last = if c.get(3).is_some() {
                    number(&c, 3)
                } else {
                    first
                };
                if s != folder_season {
                    problems.push(format!("{show}/{season}/{name}: names season {s}"));
                }
                shown.extend((first..=last).map(|e| (s, e)));
            } else if let Some(c) = CODE.captures(name) {
                shown.insert((number(&c, 1), number(&c, 2)));
            }
        }
        for name in names {
            if let Some(c) = CODE.captures(name) {
                let episode = (number(&c, 1), number(&c, 2));
                if !shown.contains(&episode) {
                    problems.push(format!(
                        "{show}/{season}: {name} ({episode:?}) has no file under canonical names"
                    ));
                }
            }
        }
    }
    problems
}

/// The media-file names of every `Shows/<show>/<season>` folder under `root`.
fn season_files(root: &VfsNode) -> HashMap<(String, String), Vec<String>> {
    let mut out = HashMap::new();
    let VfsNode::Directory { children } = root else {
        return out;
    };
    let Some(VfsNode::Directory { children: shows }) = children.get("Shows") else {
        return out;
    };
    for (show, node) in shows {
        let VfsNode::Directory { children: seasons } = node else {
            continue;
        };
        for (season, node) in seasons {
            if let VfsNode::Directory { children: files } = node {
                out.insert(
                    (show.clone(), season.clone()),
                    files
                        .iter()
                        .filter(|(_, n)| matches!(n, VfsNode::MediaFile { .. }))
                        .map(|(name, _)| name.clone())
                        .collect(),
                );
            }
        }
    }
    out
}

/// What [`check_completeness`] found.
#[derive(Debug, Default)]
struct Completeness {
    /// Title groups (one folder each).
    titles: usize,
    /// Selected video files across all torrents.
    selected_videos: usize,
    /// Streamable files the VFS shows.
    shown: usize,
    /// Streamable files the VFS leaves out by design.
    hidden: usize,
    /// Selected video files with no link at their position (selected-file/link-count mismatch).
    unlinked: Vec<String>,
    /// Streamable files left out with no design rule to explain it: build bugs.
    missing: Vec<String>,
}

/// Check the VFS under `root`, built from `data` with no managed selection, against what the build
/// is meant to show. The build never unrestricts (a file's link is resolved only when it is read),
/// so a file is left out only by design. With no managed selection the VFS shows:
/// - for a movie, ONE copy: the largest torrent of the title with a streamable file. The title's
///   other copies are hidden.
/// - for a show, each distinct episode file name once per season folder, from the largest torrent
///   that has it. A copy in another torrent is hidden only when the season folder it belongs in
///   (see [`season_folder`]) already shows a file of that name: two seasons' `01.mkv` are both
///   shown.
///
/// A selected video file with no link at its position among the selected files (Real-Debrid's
/// selected-file/link-count mismatch) can't be streamed and is left out too; those are counted
/// separately. Any other streamable file that isn't shown is reported as missing.
fn check_completeness(data: &[(TorrentInfo, MediaMetadata)], root: &VfsNode) -> Completeness {
    // (torrent id, file path) of every media file the VFS shows, with the folder it sits in.
    let mut shown_files = HashMap::new();
    collect_locators("", root, &mut shown_files);
    let shown = |info: &TorrentInfo, file: &TorrentFile| {
        shown_files.contains_key(&(info.id.clone(), file.path.clone()))
    };

    // Group the torrents as the build does: by `MediaMetadata`'s Eq/Hash (the external id, else
    // title + year), so each group is one folder.
    let mut groups: HashMap<&MediaMetadata, Vec<&TorrentInfo>> = HashMap::new();
    for (info, metadata) in data {
        groups.entry(metadata).or_default().push(info);
    }

    let mut report = Completeness {
        titles: groups.len(),
        ..Default::default()
    };
    for (metadata, torrents) in &groups {
        let mut streamable: Vec<(&TorrentInfo, &TorrentFile)> = Vec::new();
        for info in torrents {
            let (linked, no_link) = split_by_link(info);
            report.selected_videos += linked.len() + no_link.len();
            report.unlinked.extend(
                no_link
                    .iter()
                    .map(|f| format!("Torrent: {}, File: {}", info.filename, f.path)),
            );
            streamable.extend(linked.into_iter().map(|f| (*info, f)));
        }
        let shown_here = streamable.iter().filter(|(i, f)| shown(i, f)).count();
        report.shown += shown_here;
        match metadata.media_type {
            MediaType::Movie => {
                if streamable.is_empty() {
                    continue;
                }
                if shown_here == 0 {
                    report.missing.push(format!(
                        "Movie '{}' ({:?}): none of its {} streamable file(s) is shown",
                        metadata.title,
                        metadata.external_id,
                        streamable.len()
                    ));
                } else {
                    report.hidden += streamable.len() - shown_here;
                }
            }
            MediaType::Show => {
                // (season folder, file name) of every file the show shows.
                let shown_names: HashSet<(&str, &str)> = streamable
                    .iter()
                    .filter_map(|(i, f)| {
                        shown_files
                            .get(&(i.id.clone(), f.path.clone()))
                            .map(|folder| (folder.as_str(), basename(&f.path)))
                    })
                    .collect();
                for (info, file) in streamable.iter().filter(|(i, f)| !shown(i, f)) {
                    let folder = season_folder(&file.path);
                    if shown_names.contains(&(folder.as_str(), basename(&file.path))) {
                        report.hidden += 1;
                    } else {
                        report.missing.push(format!(
                            "Show '{}' ({:?}): no copy of {} is shown in {folder} (torrent: {})",
                            metadata.title, metadata.external_id, file.path, info.filename
                        ));
                    }
                }
            }
        }
    }
    report
}

/// The torrent's selected video files, split into the ones the VFS can stream and the ones it
/// can't: a file is streamable when the torrent has no links at all (TorBox resolves by id) or has
/// a link at the file's position among the SELECTED files (Real-Debrid pairs them by position).
///
/// This restates the build's rule (`vfs::emittable_video_files`) on purpose rather than calling
/// it: it is the oracle the completeness check measures the build against, so a bug in the build's
/// rule shows up as a `missing` file instead of being shared by the check. The offline
/// `completeness_*` tests below run it against the real build on a sample library, so a change
/// to the build's rule that the sample exercises fails them under plain `cargo test`.
fn split_by_link(info: &TorrentInfo) -> (Vec<&TorrentFile>, Vec<&TorrentFile>) {
    let (mut linked, mut no_link) = (Vec::new(), Vec::new());
    for (position, file) in info.files.iter().filter(|f| f.selected == 1).enumerate() {
        if !is_video_file(&file.path) {
            continue;
        }
        if info.links.is_empty() || info.links.get(position).is_some() {
            linked.push(file);
        } else {
            no_link.push(file);
        }
    }
    (linked, no_link)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The `Season NN` folder a show's video file belongs in, restated from the build's rule for the
/// same reason as [`split_by_link`]: the season of the first episode code in the file name
/// (`S01E05`, `S01.E05`, `1x05`, or a three-digit ` - 105 - `). For a file with no code, the first
/// `Sxx` / `Season N` / `NxNN` token in the file name; else the NEAREST directory naming a season
/// (by such a token, else by a `Part N`), so each season of a multi-season pack keeps its own
/// folder; else a `Part N` in the file name; else season 1. A `Part N` that numbers a part of a
/// total (`Part 2 of 4`, an episode) names no season, in a folder name or the file name. It reads only the season from an
/// episode code, where the build decodes the whole code. As in the build, an `NxM` code may carry a
/// version suffix (`2x05v2`), and neither an `NxM` code nor an `NxNN` season token counts when it
/// is a channel layout run into an `x264`/`x265` codec (`DD5.1x264`, `AAC2.0x264`) or, for a code,
/// names episode `00`: the search moves on to the next one.
fn season_folder(path: &str) -> String {
    static SXXEYY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)s(\d{1,2})[ ._]?e\d{1,3}").unwrap());
    static NXM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(?:^|[^a-z0-9])(\d{1,2})x(\d{2,3})(?:v\d{1,2})?(?:x\d{2,3}|[-_. ]\d{1,2}x\d{2,3})*(?:[^a-z0-9]|$)",
        )
        .unwrap()
    });
    static DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s-\s(\d)(\d\d)\s-\s").unwrap());
    static STRONG: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(?:^|[^a-z0-9])s(\d{1,2})(?:e|[^a-z0-9]|$)|season[\s._-]*(\d+)|(?:^|[^a-z0-9])(\d{1,2})x\d{1,3}(?:[^a-z0-9]|$)",
        )
        .unwrap()
    });
    static PART: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:^|[^a-z0-9])part[\s._-]*(\d{1,2})(?:[^a-z0-9]|$)").unwrap()
    });
    let number = |m: Option<regex::Match>| m.and_then(|m| m.as_str().parse::<u32>().ok());
    // Whether the token whose season digits are `season` (ending at its `x`) is a channel layout
    // run into a codec: the digits follow `<digit>.` and the `x` runs into `264`/`265`.
    let channel_layout = |s: &str, season: regex::Match| {
        let (bytes, start, x) = (s.as_bytes(), season.start(), season.end());
        start >= 2
            && bytes[start - 1] == b'.'
            && bytes[start - 2].is_ascii_digit()
            && matches!(bytes.get(x + 1..x + 4), Some(b"264" | b"265"))
            && !bytes.get(x + 4).is_some_and(u8::is_ascii_digit)
    };
    let strong = |s: &str| {
        let mut at = 0;
        while let Some(c) = STRONG.captures_at(s, at) {
            let nxnn = c.get(3).filter(|m| !channel_layout(s, *m));
            if let Some(season) = number(c.get(1)).or(number(c.get(2))).or(number(nxnn)) {
                return Some(season);
            }
            // A token's trailing separator can open the next one, so the search resumes on it.
            let whole = c.get(0)?;
            at = match whole.as_str().chars().next_back() {
                Some(ch) if !ch.is_ascii_alphanumeric() => whole.end() - ch.len_utf8(),
                _ => whole.end(),
            };
        }
        None
    };
    // `Part N of M`: that `Part N` is the part of a total, an episode, not a season.
    static PART_OF_TOTAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(?:^|[^a-z0-9])part[\s._-]*(\d{1,2})[\s._-]*of[\s._-]*(\d{1,2})(?:[^a-z0-9]|$)",
        )
        .unwrap()
    });
    let part_of_total = |s: &str| {
        PART_OF_TOTAL.captures(s).and_then(|c| {
            let (n, m) = (number(c.get(1))?, number(c.get(2))?);
            (n >= 1 && n <= m).then_some(n)
        })
    };
    let part = |s: &str| {
        PART.captures(s)
            .and_then(|c| number(c.get(1)))
            .filter(|&n| part_of_total(s) != Some(n))
    };

    let name = basename(path);
    let nxm_code = || {
        let mut at = 0;
        while let Some(c) = NXM.captures_at(name, at) {
            let season = c.get(1)?;
            let episode_00 = c
                .get(2)
                .is_some_and(|m| m.as_str().bytes().all(|b| b == b'0'));
            if !channel_layout(name, season) && !episode_00 {
                return number(Some(season));
            }
            // The next match's leading boundary lies after the season digits and their `x`.
            at = season.end();
        }
        None
    };
    let dash_code = || {
        let c = DASH.captures(name)?;
        let (season, episode) = (number(c.get(1))?, number(c.get(2))?);
        (season >= 1 && episode >= 1).then_some(season)
    };
    let code = SXXEYY
        .captures(name)
        .and_then(|c| number(c.get(1)))
        .or_else(nxm_code)
        .or_else(dash_code);
    let season = code.or_else(|| strong(name)).or_else(|| {
        path[..path.len() - name.len()]
            .rsplit('/')
            .filter(|dir| !dir.is_empty())
            .find_map(|dir| strong(dir).or_else(|| part(dir)))
            .or_else(|| part(name))
    });
    format!("Season {:02}", season.unwrap_or(1))
}

// --- Offline checks of `check_completeness` (fictional torrents; no account needed) ---

/// A downloaded Real-Debrid-style torrent: every file selected, one positional link per file.
fn rd_torrent(id: &str, bytes: u64, paths: &[&str]) -> TorrentInfo {
    TorrentInfo {
        id: id.to_string(),
        filename: format!("torrent-{id}"),
        hash: format!("{id:0>40}"),
        bytes,
        status: "downloaded".to_string(),
        added: "2024-01-01T00:00:00.000Z".to_string(),
        files: paths
            .iter()
            .enumerate()
            .map(|(n, p)| TorrentFile {
                id: n as u32 + 1,
                path: p.to_string(),
                bytes: bytes / paths.len() as u64,
                selected: 1,
            })
            .collect(),
        links: paths
            .iter()
            .enumerate()
            .map(|(n, _)| format!("https://example.invalid/d/{id}-{n}"))
            .collect(),
        ..Default::default()
    }
}

fn title(name: &str, tmdb: u64, media_type: MediaType) -> MediaMetadata {
    MediaMetadata {
        title: name.to_string(),
        year: Some("2010".to_string()),
        media_type,
        external_id: Some(format!("tmdb:{tmdb}")),
    }
}

/// Two copies of a movie, a show whose second torrent repeats an episode file name of the first,
/// and a torrent with fewer links than selected files.
fn sample_library() -> Vec<(TorrentInfo, MediaMetadata)> {
    let film = title("Some Film", 1, MediaType::Movie);
    let show = title("Some Show", 2, MediaType::Show);
    let mut short_of_links = rd_torrent(
        "5",
        2_000,
        &["/Other.Show.S01E01.mkv", "/Other.Show.S01E02.mkv"],
    );
    short_of_links.links.truncate(1);
    vec![
        (
            rd_torrent("1", 9_000, &["/Some.Film.1080p.mkv"]),
            film.clone(),
        ),
        (rd_torrent("2", 4_000, &["/Some.Film.720p.mkv"]), film),
        (
            rd_torrent(
                "3",
                8_000,
                &["/Some.Show.S01E01.mkv", "/Some.Show.S01E02.mkv"],
            ),
            show.clone(),
        ),
        (
            rd_torrent(
                "4",
                3_000,
                &["/Some.Show.S01E01.mkv", "/Some.Show.S01E03.mkv"],
            ),
            show,
        ),
        (short_of_links, title("Other Show", 3, MediaType::Show)),
    ]
}

#[test]
fn completeness_does_not_count_copies_hidden_by_design_as_missing() {
    let data = sample_library();
    let vfs = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());
    let report = check_completeness(&data, &vfs.root);
    assert!(report.missing.is_empty(), "{report:#?}");
    assert_eq!(report.titles, 3);
    assert_eq!(report.selected_videos, 8);
    // The 1080p film, show episodes E01, E02 and E03, and Other Show's linked E01.
    assert_eq!(report.shown, 5, "{report:#?}");
    // The 720p film copy and torrent 4's same-named E01.
    assert_eq!(report.hidden, 2, "{report:#?}");
    assert_eq!(report.unlinked.len(), 1, "{report:#?}");
    assert!(
        report.unlinked[0].contains("Other.Show.S01E02"),
        "{report:#?}"
    );
}

#[test]
fn completeness_reports_files_the_build_left_out() {
    let data = sample_library();
    // A VFS built without the film's torrents and the show's second torrent stands in for a build
    // that dropped them: the film has no copy shown and E03 has none either.
    let partial: Vec<_> = data
        .iter()
        .filter(|(info, _)| !["1", "2", "4"].contains(&info.id.as_str()))
        .cloned()
        .collect();
    let vfs = DebridVfs::build(partial, &debridmoviemapper::vfs::SelectionMap::new());
    let report = check_completeness(&data, &vfs.root);
    assert_eq!(report.missing.len(), 2, "{report:#?}");
    assert!(
        report.missing.iter().any(|m| m.contains("Some Film")),
        "{report:#?}"
    );
    assert!(
        report
            .missing
            .iter()
            .any(|m| m.contains("Some.Show.S01E03")),
        "{report:#?}"
    );
    // Torrent 4's E01 is still covered by torrent 3's.
    assert_eq!(report.hidden, 1, "{report:#?}");
}

/// A show held as season packs of code-less file names: two seasons with the same `01.mkv`, and a
/// smaller copy of season 1 under a `Season.1` folder.
fn codeless_packs() -> Vec<(TorrentInfo, MediaMetadata)> {
    let show = title("Pack Show", 4, MediaType::Show);
    vec![
        (
            rd_torrent(
                "6",
                6_000,
                &["/Pack.Show.S01/01.mkv", "/Pack.Show.S01/02.mkv"],
            ),
            show.clone(),
        ),
        (
            rd_torrent("7", 5_000, &["/Pack.Show.S02/01.mkv"]),
            show.clone(),
        ),
        (
            rd_torrent("8", 2_000, &["/Pack.Show.Season.1.720p/01.mkv"]),
            show,
        ),
    ]
}

#[test]
fn completeness_hides_a_same_named_file_only_within_its_season() {
    let data = codeless_packs();
    let vfs = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());
    let report = check_completeness(&data, &vfs.root);
    assert!(report.missing.is_empty(), "{report:#?}");
    // Season 1's 01.mkv and 02.mkv, and season 2's 01.mkv.
    assert_eq!(report.shown, 3, "{report:#?}");
    // Torrent 8's 01.mkv is the same season-1 episode as torrent 6's.
    assert_eq!(report.hidden, 1, "{report:#?}");
}

#[test]
fn completeness_reports_a_dropped_file_named_like_another_seasons() {
    let data = codeless_packs();
    // A VFS built without the season-2 pack stands in for a build that dropped its 01.mkv, e.g. by
    // filing it under Season 01, where a file of that name already is.
    let partial: Vec<_> = data
        .iter()
        .filter(|(info, _)| info.id != "7")
        .cloned()
        .collect();
    let vfs = DebridVfs::build(partial, &debridmoviemapper::vfs::SelectionMap::new());
    let report = check_completeness(&data, &vfs.root);
    assert_eq!(report.missing.len(), 1, "{report:#?}");
    assert!(
        report.missing[0].contains("Pack.Show.S02/01.mkv"),
        "{report:#?}"
    );
    // Torrent 8's 01.mkv is still covered by torrent 6's in Season 01.
    assert_eq!(report.hidden, 1, "{report:#?}");
}

/// The canonical-name check passes on the sample library and on a show whose episode titles hold
/// codes a player misreads (an extra titled after an episode, a title of digits and dashes).
#[test]
fn canonical_names_lose_no_episode_of_the_sample_library() {
    let mut data = sample_library();
    data.push((
        rd_torrent(
            "70",
            3_000,
            &[
                "/Pack/Other.Show.S03E03.4-5-1.1080p.mkv",
                "/Pack/S00E41 - Inside the Episode S2 E1.mkv",
                "/Pack/Other.Show.S01E01E02.mkv",
            ],
        ),
        title("Other Show", 70, MediaType::Show),
    ));
    assert_eq!(canonical_name_problems(&data), Vec::<String>::new());
}

/// `season_folder` and the build agree on the season folder of each kind of show file name, so the
/// live check never reports a copy as missing because the two disagree.
#[test]
fn season_folder_agrees_with_the_build() {
    let cases = [
        ("/Show.S01E05.mkv", "Season 01"),
        ("/Show.S03.E05.mkv", "Season 03"),
        ("/Show.2x05.Title.mkv", "Season 02"),
        ("/Show - 409 - Title.mkv", "Season 04"),
        // An episode code in the file name beats a directory's season.
        ("/Show.S01/Show.S02E03.mkv", "Season 02"),
        ("/Show.S03/01.mkv", "Season 03"),
        ("/Show.Season.2/01.mkv", "Season 02"),
        ("/Show_S02_1080p/01.mkv", "Season 02"),
        // The nearest directory decides, not a multi-season pack's root folder.
        ("/Show.S01-S03/Season 2/01.mkv", "Season 02"),
        ("/Show.S04/Part 2/01.mkv", "Season 02"),
        // A directory's season beats a `Part N` in the file name, which is the weakest signal.
        ("/Show/Season 3/05 - Title, Part 2.mkv", "Season 03"),
        ("/Show/05 - Title, Part 2.mkv", "Season 02"),
        // ... unless it numbers a part of a total: that is the episode, in a folder too.
        ("/Show/Doc - Part 2 of 4.mkv", "Season 01"),
        (
            "/Doc.Part.2.of.4.1080p/Doc.Part.2.of.4.1080p.mkv",
            "Season 01",
        ),
        ("/Show Part 3 - Episode 2 of 6.mkv", "Season 03"),
        ("/Show/01.mkv", "Season 01"),
        // An `NxM` code with a version suffix, and one after a year or a title ending in a digit.
        ("/Show/Show.2x05v2.mkv", "Season 02"),
        ("/Show/Show.Name.2005.2x05.mkv", "Season 02"),
        ("/Show/Babylon.5.2x10.mkv", "Season 02"),
        // A channel layout run into a codec is no code and names no season, and an episode 00 is
        // skipped for the next code.
        ("/Show.S02/Show.E05.DD5.1x264.mkv", "Season 02"),
        ("/Show/01.AAC2.0x264.mkv", "Season 01"),
        ("/Show/05 - Title.AAC2.0x264.S03.mkv", "Season 03"),
        ("/Show/Show.1x00.2x05.mkv", "Season 02"),
        ("/Show/1x00.2x05.mkv", "Season 02"),
    ];
    for (n, (path, expected)) in cases.into_iter().enumerate() {
        let id = (n + 1).to_string();
        let data = vec![(
            rd_torrent(&id, 1_000, &[path]),
            title("Some Show", 5, MediaType::Show),
        )];
        let vfs = DebridVfs::build(data, &debridmoviemapper::vfs::SelectionMap::new());
        let mut files = HashMap::new();
        collect_locators("", &vfs.root, &mut files);
        assert_eq!(season_folder(path), expected, "season_folder({path})");
        assert_eq!(
            files.get(&(id, path.to_string())).map(String::as_str),
            Some(expected),
            "the build's folder for {path}"
        );
    }
}

// --- Offline checks of `fetch_and_identify` (a fictional account; no provider or TMDB call) ---

/// A fictional account: lists `torrents` and answers a lookup with the torrent's info, except that
/// the first `misses` lookups answer 404 (TorBox's spurious `mylist?id=` 404). `lookups` counts the
/// lookups. Every call that would change an account panics.
#[derive(Debug, Default)]
struct FakeAccount {
    torrents: Vec<TorrentInfo>,
    misses: usize,
    lookups: AtomicUsize,
}

impl FakeAccount {
    fn new(torrents: Vec<TorrentInfo>) -> Self {
        Self {
            torrents,
            ..Default::default()
        }
    }
}

/// A 404 as the provider clients return it, built without any I/O.
fn not_found() -> reqwest::Error {
    reqwest::Response::from(
        hyper::Response::builder()
            .status(reqwest::StatusCode::NOT_FOUND)
            .body(hyper::body::Bytes::new())
            .expect("an empty 404 response always builds"),
    )
    .error_for_status()
    .expect_err("404 is an error status")
}

#[async_trait::async_trait]
impl DebridProvider for FakeAccount {
    fn name(&self) -> &'static str {
        "fake-account"
    }
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        Ok(self
            .torrents
            .iter()
            .map(|t| Torrent {
                id: t.id.clone(),
                filename: t.filename.clone(),
                hash: t.hash.clone(),
                bytes: t.bytes,
                status: t.status.clone(),
                added: t.added.clone(),
                ..Default::default()
            })
            .collect())
    }
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        if self.lookups.fetch_add(1, Ordering::SeqCst) < self.misses {
            return Err(not_found());
        }
        self.torrents
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or_else(not_found)
    }
    async fn add_magnet(&self, _magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        panic!("the fake account must not be changed: add_magnet")
    }
    async fn select_files(&self, _id: &str, _file_ids: &str) -> Result<(), reqwest::Error> {
        panic!("the fake account must not be changed: select_files")
    }
    async fn delete_torrent(&self, _id: &str) -> Result<(), reqwest::Error> {
        panic!("the fake account must not be changed: delete_torrent")
    }
    async fn resolve_url(&self, _loc: &FileLocator) -> Result<String, AppError> {
        Err(AppError::Unavailable)
    }
    async fn invalidate(&self, _loc: &FileLocator) {}
}

/// A fresh identification cache for one offline check, in Cargo's integration-test scratch
/// directory (a file an earlier, interrupted run left is removed first).
fn scratch_store(name: &str) -> (Store, String) {
    let path = format!(
        "{}/integration_test_offline_{name}_{}.redb",
        env!("CARGO_TARGET_TMPDIR"),
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    (Store::open(&path).expect("open the scratch store"), path)
}

/// What `identify_torrent` returns when nothing matched: a name-derived fallback with no id.
fn unidentified(info: &TorrentInfo) -> MediaMetadata {
    MediaMetadata {
        title: info.filename.clone(),
        year: None,
        media_type: MediaType::Movie,
        external_id: None,
    }
}

/// The paths of a torrent's files, in order.
fn paths(info: &TorrentInfo) -> Vec<&str> {
    info.files.iter().map(|f| f.path.as_str()).collect()
}

/// A run while TMDB is failing (an outage, a rejected key) must fail, and must not cache the
/// name-derived fallback it got: a later run would serve it from the cache and keep failing
/// `test_identification` after TMDB recovers.
#[tokio::test]
async fn a_failed_tmdb_search_fails_the_run_and_caches_nothing() {
    let (store, path) = scratch_store("failed_search");
    let account = FakeAccount::new(vec![rd_torrent("1", 1_000, &["/Some.Film.2010.mkv"])]);

    let outcome = AssertUnwindSafe(fetch_and_identify(
        &account,
        &store,
        None,
        |info| async move { (unidentified(&info), true) },
    ))
    .catch_unwind()
    .await;

    let cached = store.get_match("1".to_string()).await;
    drop(store);
    let _ = std::fs::remove_file(&path);
    assert!(outcome.is_err(), "a failed TMDB search must fail the run");
    assert!(
        cached.is_none(),
        "the fallback a failed TMDB search produced must not be cached"
    );
}

/// A torrent the cache holds as unidentified (cached by a run that predates the check above, or a
/// genuine no-match) is identified again; an identified one is served from the cache.
#[tokio::test]
async fn a_cached_unidentified_torrent_is_identified_again() {
    let (store, path) = scratch_store("cached_unidentified");
    let film = rd_torrent("1", 1_000, &["/Some.Film.2010.mkv"]);
    let other = rd_torrent("2", 1_000, &["/Other.Film.2010.mkv"]);
    store
        .put_matches(vec![
            (
                "1".to_string(),
                film.clone(),
                title("Some Film", 1, MediaType::Movie),
            ),
            ("2".to_string(), other.clone(), unidentified(&other)),
        ])
        .await
        .unwrap();
    let account = FakeAccount::new(vec![film, other]);
    let identified_ids = Mutex::new(Vec::new());

    let data = fetch_and_identify(&account, &store, None, |info| {
        identified_ids.lock().unwrap().push(info.id.clone());
        async move { (title("Other Film", 2, MediaType::Movie), false) }
    })
    .await;

    let recached = store.get_match("2".to_string()).await;
    drop(store);
    let _ = std::fs::remove_file(&path);
    assert_eq!(*identified_ids.lock().unwrap(), vec!["2".to_string()]);
    assert_eq!(account.lookups.load(Ordering::SeqCst), 1);
    let ids: HashMap<String, Option<String>> = data
        .into_iter()
        .map(|(info, metadata)| (info.id, metadata.external_id))
        .collect();
    assert_eq!(ids["1"], Some("tmdb:1".to_string()));
    assert_eq!(ids["2"], Some("tmdb:2".to_string()));
    assert_eq!(
        recached.and_then(|(_, metadata)| metadata.external_id),
        Some("tmdb:2".to_string()),
        "the new identification replaces the cached fallback"
    );
}

/// An identified torrent cached while its file list held no streamable video file (TorBox lists a
/// lapsed or not yet resolved torrent with `files: null`) gets its current files: otherwise the
/// completeness check would cover none of its files on every later run. The identification is
/// kept, the cache takes the fresh files, and an identified entry that has its files costs no
/// lookup.
#[tokio::test]
async fn a_cached_torrent_without_a_video_file_gets_its_files_again() {
    let (store, path) = scratch_store("cached_fileless");
    let fileless = rd_torrent("1", 1_000, &[]);
    let listed = rd_torrent("1", 1_000, &["/Some.Film.2010.mkv"]);
    let other = rd_torrent("2", 1_000, &["/Other.Film.2010.mkv"]);
    store
        .put_matches(vec![
            (
                "1".to_string(),
                fileless,
                title("Some Film", 1, MediaType::Movie),
            ),
            (
                "2".to_string(),
                other.clone(),
                title("Other Film", 2, MediaType::Movie),
            ),
        ])
        .await
        .unwrap();
    let account = FakeAccount::new(vec![listed.clone(), other]);

    let data = fetch_and_identify(&account, &store, None, |info| async move {
        panic!("an identified torrent is not identified again: {}", info.id)
    })
    .await;

    let recached = store.get_match("1".to_string()).await;
    drop(store);
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        account.lookups.load(Ordering::SeqCst),
        1,
        "only the entry with no video file is looked up"
    );
    let (info, metadata) = data
        .iter()
        .find(|(info, _)| info.id == "1")
        .expect("torrent 1 is in the data");
    assert_eq!(
        paths(info),
        paths(&listed),
        "the data holds the fresh files"
    );
    assert_eq!(metadata.external_id, Some("tmdb:1".to_string()));
    let (cached_info, cached_metadata) = recached.expect("torrent 1 stays cached");
    assert_eq!(
        paths(&cached_info),
        paths(&listed),
        "the cache takes the files"
    );
    assert_eq!(cached_metadata.external_id, Some("tmdb:1".to_string()));
    assert_eq!(data.len(), 2);
}

/// A lookup that still lists no streamable video file never replaces the cached entry.
#[tokio::test]
async fn a_lookup_still_without_a_video_file_keeps_the_cached_entry() {
    let (store, path) = scratch_store("still_fileless");
    let cached = rd_torrent("1", 1_000, &["/Some.Film.2010.nfo"]);
    store
        .put_matches(vec![(
            "1".to_string(),
            cached.clone(),
            title("Some Film", 1, MediaType::Movie),
        )])
        .await
        .unwrap();
    let account = FakeAccount::new(vec![rd_torrent("1", 1_000, &[])]);

    let data = fetch_and_identify(&account, &store, None, |info| async move {
        panic!("an identified torrent is not identified again: {}", info.id)
    })
    .await;

    let recached = store.get_match("1".to_string()).await;
    drop(store);
    let _ = std::fs::remove_file(&path);
    assert_eq!(account.lookups.load(Ordering::SeqCst), 1);
    assert_eq!(data.len(), 1);
    assert_eq!(paths(&data[0].0), paths(&cached));
    let (cached_info, _) = recached.expect("torrent 1 stays cached");
    assert_eq!(
        paths(&cached_info),
        paths(&cached),
        "the cached files are kept"
    );
}

/// A spurious 404 on a torrent's lookup is ridden out instead of failing the run.
#[tokio::test(start_paused = true)]
async fn fetch_and_identify_retries_a_404_lookup() {
    let (store, path) = scratch_store("lookup_404");
    let account = FakeAccount {
        misses: 2,
        ..FakeAccount::new(vec![rd_torrent("1", 1_000, &["/Some.Film.2010.mkv"])])
    };

    let data = fetch_and_identify(&account, &store, None, |_| async move {
        (title("Some Film", 1, MediaType::Movie), false)
    })
    .await;

    drop(store);
    let _ = std::fs::remove_file(&path);
    assert_eq!(data.len(), 1);
    assert_eq!(account.lookups.load(Ordering::SeqCst), 3);
}

/// Test 4: VFS timestamps are populated from the provider's added dates and stable across
/// rebuilds.
#[tokio::test]
#[ignore]
async fn test_vfs_timestamps_stable() {
    for_each_provider("test_vfs_timestamps_stable", check_vfs_timestamps_stable).await;
}

fn check_vfs_timestamps_stable(label: &str, data: &[(TorrentInfo, MediaMetadata)]) {
    assert!(
        !data.is_empty(),
        "[{label}] Need identified torrents to test timestamps"
    );

    let vfs1 = DebridVfs::build(data.to_vec(), &debridmoviemapper::vfs::SelectionMap::new());
    let vfs2 = DebridVfs::build(data.to_vec(), &debridmoviemapper::vfs::SelectionMap::new());

    // Timestamps should be populated
    assert!(
        !vfs1.timestamps.is_empty(),
        "[{label}] VFS timestamps should be populated from the provider's data"
    );

    // Every directory and file in the tree should have a timestamp
    let mut paths = Vec::new();
    collect_paths(&vfs1.root, "", &mut paths);
    for path in &paths {
        assert!(
            vfs1.timestamps.contains_key(path),
            "[{label}] Missing timestamp for path: {}",
            path
        );
    }

    // Timestamps should be identical across two builds of the same data
    assert_eq!(
        vfs1.timestamps, vfs2.timestamps,
        "[{label}] Timestamps must be stable across VFS rebuilds"
    );

    // No timestamp should be UNIX_EPOCH, except for empty top-level directories
    // (e.g. "Movies" when INTEGRATION_TEST_LIMIT only picks up shows)
    let epoch = std::time::UNIX_EPOCH;
    for (path, ts) in &vfs1.timestamps {
        if *ts == epoch {
            // Only top-level dirs ("Movies", "Shows") are allowed to be EPOCH when empty
            assert!(
                path == "Movies" || path == "Shows",
                "[{label}] Timestamp for '{}' should not be UNIX_EPOCH — the provider's added \
                 date not parsed?",
                path
            );
        }
    }

    println!("\n=== VFS Timestamps ({label}) ===");
    println!("Total paths with timestamps: {}", vfs1.timestamps.len());
    println!("All timestamps stable across rebuilds: ✓");
    println!("No UNIX_EPOCH fallbacks: ✓");
}

fn collect_paths(node: &VfsNode, prefix: &str, paths: &mut Vec<String>) {
    if let VfsNode::Directory { children } = node {
        for (name, child) in children {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            paths.push(path.clone());
            collect_paths(child, &path, paths);
        }
    }
}

/// Every media file under the directory `node`, named `name`, as its (torrent id, file path)
/// mapped to the name of the directory holding it: a show episode's `Season NN`, a movie's title
/// folder.
fn collect_locators(name: &str, node: &VfsNode, files: &mut HashMap<(String, String), String>) {
    let VfsNode::Directory { children } = node else {
        return;
    };
    for (child_name, child) in children {
        match child {
            VfsNode::Directory { .. } => collect_locators(child_name, child, files),
            VfsNode::MediaFile { locator, .. } => {
                files.insert(
                    (locator.torrent_id.clone(), locator.file_path.clone()),
                    name.to_string(),
                );
            }
            VfsNode::VirtualFile { .. } => {}
        }
    }
}
