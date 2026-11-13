//! Live Real-Debrid identification and VFS tests (`#[ignore]`). They need `RD_API_TOKEN` and
//! `TMDB_API_KEY`, skip when `RD_API_TOKEN` is unset or blank, and only read the account.
//!
//! Identifications are cached between runs in a `Store` under Cargo's scratch directory for
//! integration tests (`CARGO_TARGET_TMPDIR`), never in the service's own `metadata.db`.

mod common;

use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::rd_client::{RealDebridClient, TorrentFile, TorrentInfo};
use debridmoviemapper::store::Store;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::vfs::{is_video_file, DebridVfs, MediaMetadata, MediaType, VfsNode};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{OnceCell, RwLock};

/// Shared identified data, fetched and identified once and reused by every test. `None` when
/// `RD_API_TOKEN` is unset or blank, and the tests then skip.
static IDENTIFIED_DATA: OnceCell<Option<Vec<(TorrentInfo, MediaMetadata)>>> = OnceCell::const_new();

/// The identification cache's path.
fn cache_path() -> String {
    format!(
        "{}/integration_test_cache.redb",
        env!("CARGO_TARGET_TMPDIR")
    )
}

async fn get_shared_data() -> Option<&'static Vec<(TorrentInfo, MediaMetadata)>> {
    IDENTIFIED_DATA
        .get_or_init(|| async {
            tracing_subscriber::fmt::try_init().ok();
            let api_token = common::token("RD_API_TOKEN")?;
            let tmdb_client = common::tmdb_client();
            let rd_client = Arc::new(RealDebridClient::new(api_token).unwrap());
            let store = Store::open(&cache_path())
                .expect("Failed to open the integration-test identification cache");

            Some(fetch_and_identify(&rd_client, &tmdb_client, &store).await)
        })
        .await
        .as_ref()
}

/// Fetch downloaded torrents and identify them via TMDB, using the store's identification cache.
/// Respects INTEGRATION_TEST_LIMIT env var to limit the number of torrents processed.
async fn fetch_and_identify(
    rd_client: &Arc<RealDebridClient>,
    tmdb_client: &Arc<TmdbClient>,
    store: &Store,
) -> Vec<(TorrentInfo, MediaMetadata)> {
    println!("Fetching torrents...");
    let torrents = rd_client
        .get_torrents()
        .await
        .expect("Failed to get torrents");
    let downloaded: Vec<_> = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .collect();
    println!("Found {} downloaded torrents", downloaded.len());

    let limit = std::env::var("INTEGRATION_TEST_LIMIT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());

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
        if let Some(data) = store.get_match(torrent.id.clone()).await {
            println!("Cached: {} (ID: {:?})", data.0.filename, data.1.external_id);
            current_data.push(data);
        } else {
            to_identify.push(torrent.clone());
        }
    }

    if !to_identify.is_empty() {
        println!(
            "Identifying {} new torrents ({} cached)...",
            to_identify.len(),
            current_data.len()
        );
        let mut stream = futures_util::stream::iter(to_identify)
            .map(|torrent| {
                let rd_client = rd_client.clone();
                let tmdb_client = tmdb_client.clone();
                async move {
                    let info = rd_client
                        .get_torrent_info(&torrent.id)
                        .await
                        .expect("Failed to get torrent info");
                    let metadata = identify_torrent(&info, &tmdb_client).await;
                    (torrent.id, info, metadata)
                }
            })
            .buffer_unordered(1);

        while let Some((id, info, metadata)) = stream.next().await {
            println!(
                "Identified: {} as {:?} (ID: {:?})",
                info.filename, metadata.media_type, metadata.external_id
            );
            if let Err(e) = store
                .put_matches(vec![(id, info.clone(), metadata.clone())])
                .await
            {
                println!("WARNING: failed to cache identification: {e}");
            }
            current_data.push((info, metadata));
        }
    } else {
        println!("All {} torrents served from cache.", current_data.len());
    }

    current_data
}

/// Test 1: Torrent fetching and TMDB identification only. No VFS, no unrestrict.
/// Uses the identification cache so repeat runs are fast.
#[tokio::test]
#[ignore]
async fn test_identification() {
    let Some(data) = get_shared_data().await else {
        println!("skipping test_identification: RD_API_TOKEN not set");
        return;
    };

    let identified = data.iter().filter(|(_, m)| m.external_id.is_some()).count();
    let unidentified = data.len() - identified;

    println!("\n=== Identification Summary ===");
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
        "Should have at least one downloaded torrent"
    );
    assert!(identified > 0, "Should identify at least one torrent");
}

/// Test 2: Given identified data (from the identification cache), builds VFS and checks directory
/// structure.
#[tokio::test]
#[ignore]
async fn test_vfs_structure() {
    let Some(data) = get_shared_data().await else {
        println!("skipping test_vfs_structure: RD_API_TOKEN not set");
        return;
    };
    assert!(
        !data.is_empty(),
        "Need identified torrents to test VFS structure"
    );

    println!("Building VFS...");
    let new_vfs = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());
    let vfs = Arc::new(RwLock::new(new_vfs));

    let vfs_lock = vfs.read().await;
    let root = &vfs_lock.root;

    // Root must be a directory
    let children = match root {
        VfsNode::Directory { children } => children,
        _ => panic!("Root should be a directory"),
    };

    println!("\n=== VFS Structure ===");
    println!("Root entries: {:?}", children.keys().collect::<Vec<_>>());

    // Check for Movies/ and/or Shows/ directories
    let has_movies = children.contains_key("Movies");
    let has_shows = children.contains_key("Shows");
    println!("Has Movies/: {}", has_movies);
    println!("Has Shows/:  {}", has_shows);

    assert!(
        has_movies || has_shows,
        "VFS should contain at least Movies/ or Shows/ directory"
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
/// selected-file/link-count mismatch on more than `MAX_UNLINKED_RATE` of the selected video files.
#[tokio::test]
#[ignore]
async fn test_vfs_completeness() {
    /// The share of selected video files that may lack a link (a provider-side anomaly; a much
    /// higher share suggests the client mis-parses the links).
    const MAX_UNLINKED_RATE: f64 = 0.20;

    let Some(data) = get_shared_data().await else {
        println!("skipping test_vfs_completeness: RD_API_TOKEN not set");
        return;
    };
    assert!(
        !data.is_empty(),
        "Need identified torrents to test VFS completeness"
    );

    println!("Building VFS...");
    let vfs = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());
    let report = check_completeness(data, &vfs.root);

    let unlinked_rate = if report.selected_videos > 0 {
        report.unlinked.len() as f64 / report.selected_videos as f64
    } else {
        0.0
    };
    println!("\n=== VFS Completeness ===");
    println!("Titles:                 {}", report.titles);
    println!("Selected video files:   {}", report.selected_videos);
    println!("Shown in the VFS:       {}", report.shown);
    println!(
        "Hidden by design:       {} (a movie's other copies, same-named episode copies)",
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
        "{} streamable video file(s) are missing from the VFS although no other copy of them is \
         shown (a movie's other copies and same-named episode copies are already excluded), so the \
         VFS build dropped them: {:#?}",
        report.missing.len(),
        report.missing
    );
    assert!(
        unlinked_rate <= MAX_UNLINKED_RATE,
        "{}/{} selected video files ({:.1}% > {:.0}%) have no link at their position among the \
         selected files, so the VFS can't stream them. A selected-file/link-count mismatch this \
         common suggests the client mis-parses the links rather than the odd provider anomaly.",
        report.unlinked.len(),
        report.selected_videos,
        unlinked_rate * 100.0,
        MAX_UNLINKED_RATE * 100.0
    );
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
/// - for a show, each distinct episode file name once, from the largest torrent that has it.
///   Same-named copies in other torrents are hidden.
///
/// A selected video file with no link at its position among the selected files (Real-Debrid's
/// selected-file/link-count mismatch) can't be streamed and is left out too; those are counted
/// separately. Any other streamable file that isn't shown is reported as missing.
fn check_completeness(data: &[(TorrentInfo, MediaMetadata)], root: &VfsNode) -> Completeness {
    // (torrent id, file path) of every media file the VFS shows.
    let mut shown_files = HashSet::new();
    collect_locators(root, &mut shown_files);
    let shown = |info: &TorrentInfo, file: &TorrentFile| {
        shown_files.contains(&(info.id.clone(), file.path.clone()))
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
                let shown_names: HashSet<&str> = streamable
                    .iter()
                    .filter(|(i, f)| shown(i, f))
                    .map(|(_, f)| basename(&f.path))
                    .collect();
                for (info, file) in streamable.iter().filter(|(i, f)| !shown(i, f)) {
                    if shown_names.contains(basename(&file.path)) {
                        report.hidden += 1;
                    } else {
                        report.missing.push(format!(
                            "Show '{}' ({:?}): no copy of {} is shown (torrent: {})",
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
/// can't, mirroring the build (`vfs::emittable_video_files`, which isn't public): a file is
/// streamable when the torrent has no links at all (TorBox resolves by id) or has a link at the
/// file's position among the SELECTED files (Real-Debrid pairs them by position).
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

/// Test 4: VFS timestamps are populated from RD data and stable across rebuilds.
#[tokio::test]
#[ignore]
async fn test_vfs_timestamps_stable() {
    let Some(data) = get_shared_data().await else {
        println!("skipping test_vfs_timestamps_stable: RD_API_TOKEN not set");
        return;
    };
    assert!(
        !data.is_empty(),
        "Need identified torrents to test timestamps"
    );

    let vfs1 = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());
    let vfs2 = DebridVfs::build(data.clone(), &debridmoviemapper::vfs::SelectionMap::new());

    // Timestamps should be populated
    assert!(
        !vfs1.timestamps.is_empty(),
        "VFS timestamps should be populated from RD data"
    );

    // Every directory and file in the tree should have a timestamp
    let mut paths = Vec::new();
    collect_paths(&vfs1.root, "", &mut paths);
    for path in &paths {
        assert!(
            vfs1.timestamps.contains_key(path),
            "Missing timestamp for path: {}",
            path
        );
    }

    // Timestamps should be identical across two builds of the same data
    assert_eq!(
        vfs1.timestamps, vfs2.timestamps,
        "Timestamps must be stable across VFS rebuilds"
    );

    // No timestamp should be UNIX_EPOCH, except for empty top-level directories
    // (e.g. "Movies" when INTEGRATION_TEST_LIMIT only picks up shows)
    let epoch = std::time::UNIX_EPOCH;
    for (path, ts) in &vfs1.timestamps {
        if *ts == epoch {
            // Only top-level dirs ("Movies", "Shows") are allowed to be EPOCH when empty
            assert!(
                path == "Movies" || path == "Shows",
                "Timestamp for '{}' should not be UNIX_EPOCH — RD added date not parsed?",
                path
            );
        }
    }

    println!("\n=== VFS Timestamps ===");
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

/// The (torrent id, file path) of every media file under `node`.
fn collect_locators(node: &VfsNode, files: &mut HashSet<(String, String)>) {
    match node {
        VfsNode::Directory { children } => {
            for child in children.values() {
                collect_locators(child, files);
            }
        }
        VfsNode::MediaFile { locator, .. } => {
            files.insert((locator.torrent_id.clone(), locator.file_path.clone()));
        }
        VfsNode::VirtualFile { .. } => {}
    }
}
