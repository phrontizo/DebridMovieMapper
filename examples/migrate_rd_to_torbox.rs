//! ONE-OFF migration helper (NOT a shipped feature) — re-acquire your Real-Debrid library on
//! TorBox via the SP1 acquisition engine. No torrents are copied: each RD title is identified,
//! then the engine scrapes TorBox for an equivalent release. **Real-Debrid is never modified.**
//!
//! This file lives in `examples/`, so it is not part of the service binary and is safe to delete.
//!
//! Modes:
//!   cargo run --example migrate_rd_to_torbox             # survey  (read-only report)
//!   cargo run --example migrate_rd_to_torbox -- movies   # acquire identified movies on TorBox
//!   cargo run --example migrate_rd_to_torbox -- series   # acquire series (season packs where
//!                                                        #   possible, else individual episodes)
//!
//! Needs RD_API_TOKEN + TORBOX_API_KEY + TMDB_API_KEY in `.env` (optionally SCRAPER_ADDON_URL).
//! Engine bookkeeping is written to `migration.db` in the working directory.

use debridmoviemapper::acquire::{
    AcquireOutcome, AcquisitionEngine, HttpProber, Prober, TitleValidator, TmdbTitleValidator,
};
use debridmoviemapper::config::AcquisitionConfig;
use debridmoviemapper::identification::{identify_name, identify_torrent};
use debridmoviemapper::provider::{DebridProvider, ProviderKind};
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::scraper::{MediaKind, Scraper, TorrentioScraper};
use debridmoviemapper::store::{AcquireRequest, Store};
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::torbox_client::TorBoxClient;
use debridmoviemapper::vfs::{is_video_file, MediaMetadata, MediaType};
use debridmoviemapper::{reacquire, release};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

fn env_req(k: &str) -> String {
    std::env::var(k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            eprintln!("migrate: missing {k} in environment/.env");
            std::process::exit(2);
        })
}

/// Parse `(season, episode)` from a filename (`S01E02` or `1x02`).
static SE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)s(\d{1,2})[._ ]?e(\d{1,3})|(\d{1,2})x(\d{1,3})").unwrap());
fn parse_se(name: &str) -> Option<(u32, u32)> {
    let c = SE_RE.captures(name)?;
    if let (Some(s), Some(e)) = (c.get(1), c.get(2)) {
        Some((s.as_str().parse().ok()?, e.as_str().parse().ok()?))
    } else {
        Some((c.get(3)?.as_str().parse().ok()?, c.get(4)?.as_str().parse().ok()?))
    }
}

struct Movie {
    tmdb_id: u64,
    title: String,
    year: Option<String>,
    rd_name: String,
}

struct Show {
    title: String,
    year: Option<String>,
    episodes: BTreeSet<(u32, u32)>, // (season, episode)
}

struct Library {
    total: usize,
    movies: Vec<Movie>,
    shows: BTreeMap<u64, Show>, // tmdb_id -> show
    unidentified: Vec<String>,
    info_errors: usize,
}

async fn build_library(rd: &Arc<dyn DebridProvider>, tmdb: &TmdbClient) -> Library {
    let torrents = match rd.get_torrents().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("migrate: RD get_torrents failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "RD library: {} torrents. Identifying (fetches each file list + TMDB; a few minutes)…",
        torrents.len()
    );
    let mut movies: Vec<Movie> = Vec::new();
    let mut shows: BTreeMap<u64, Show> = BTreeMap::new();
    let mut unidentified: Vec<String> = Vec::new();
    let mut info_errors = 0usize;

    for (i, t) in torrents.iter().enumerate() {
        if i % 25 == 0 {
            eprintln!("  …identifying {}/{}", i, torrents.len());
        }
        let info = match rd.get_torrent_info(&t.id).await {
            Ok(info) => info,
            Err(_) => {
                info_errors += 1;
                unidentified.push(t.filename.clone());
                continue;
            }
        };
        let meta = identify_torrent(&info, tmdb).await;
        let tmdb_id = meta
            .external_id
            .as_deref()
            .and_then(|e| e.strip_prefix("tmdb:"))
            .and_then(|n| n.parse::<u64>().ok());
        match (tmdb_id, meta.media_type) {
            (Some(id), MediaType::Movie) => movies.push(Movie {
                tmdb_id: id,
                title: meta.title,
                year: meta.year,
                rd_name: t.filename.clone(),
            }),
            (Some(id), MediaType::Show) => {
                let show = shows.entry(id).or_insert_with(|| Show {
                    title: meta.title.clone(),
                    year: meta.year.clone(),
                    episodes: BTreeSet::new(),
                });
                for f in &info.files {
                    if is_video_file(&f.path) {
                        if let Some(se) = parse_se(&f.path) {
                            show.episodes.insert(se);
                        }
                    }
                }
            }
            (None, _) => unidentified.push(t.filename.clone()),
        }
    }
    movies.sort_by(|a, b| a.tmdb_id.cmp(&b.tmdb_id).then_with(|| a.title.cmp(&b.title)));
    movies.dedup_by_key(|m| m.tmdb_id);
    Library {
        total: torrents.len(),
        movies,
        shows,
        unidentified,
        info_errors,
    }
}

/// Build the TorBox-targeted acquisition engine + a shared TorBox provider/scraper for direct use.
fn build_engine(
    tb_token: &str,
    tmdb: Arc<TmdbClient>,
    http: reqwest::Client,
) -> (AcquisitionEngine, Arc<dyn DebridProvider>, TorrentioScraper, AcquisitionConfig) {
    let addon = std::env::var("SCRAPER_ADDON_URL").ok().filter(|s| !s.trim().is_empty());
    let tb: Arc<dyn DebridProvider> = Arc::new(TorBoxClient::new(tb_token.to_string()).expect("tb"));
    let scraper_arc: Arc<dyn Scraper> =
        Arc::new(TorrentioScraper::new(addon.clone(), ProviderKind::TorBox, tb_token, http.clone()));
    let validator: Arc<dyn TitleValidator> = Arc::new(TmdbTitleValidator { tmdb: tmdb.clone() });
    let prober: Arc<dyn Prober> = Arc::new(HttpProber { http: http.clone() });
    let store = Store::open("migration.db").expect("store");
    let acfg = AcquisitionConfig::default();
    let engine = AcquisitionEngine::new(
        tb.clone(),
        scraper_arc,
        validator,
        prober,
        store,
        acfg.prefs.clone(),
        acfg.max_acquire_attempts,
        Duration::from_secs(acfg.stall_timeout_secs),
    );
    // A second scraper for the pack path (the Arc<dyn Scraper> above is owned by the engine).
    let scraper = TorrentioScraper::new(addon, ProviderKind::TorBox, tb_token, http);
    (engine, tb, scraper, acfg)
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .try_init();
    let mode = std::env::args().nth(1).unwrap_or_else(|| "survey".to_string());
    let rd_token = env_req("RD_API_TOKEN");
    let tb_token = env_req("TORBOX_API_KEY");
    let tmdb_key = env_req("TMDB_API_KEY");

    let rd: Arc<dyn DebridProvider> = Arc::new(RealDebridClient::new(rd_token).expect("rd client"));
    let tmdb = Arc::new(TmdbClient::new(tmdb_key).expect("tmdb client"));
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client");

    // Duplicate cleanup only needs the TorBox side — skip the slow RD library scan.
    if mode == "dupes" {
        dedup_report(&tb_token, tmdb).await;
        return;
    }

    let lib = build_library(&rd, &tmdb).await;
    eprintln!(
        "\nIdentified: {} unique movies, {} shows ({} episodes total), {} unidentified ({} info errors).\n",
        lib.movies.len(),
        lib.shows.len(),
        lib.shows.values().map(|s| s.episodes.len()).sum::<usize>(),
        lib.unidentified.len(),
        lib.info_errors
    );

    match mode.as_str() {
        "survey" => survey(&lib, &tmdb, &tb_token, http).await,
        "movies" => migrate_movies(&lib, tmdb, &tb_token, http).await,
        "series" => migrate_series(&lib, tmdb, &tb_token, http).await,
        other => {
            eprintln!("migrate: unknown mode '{other}' (use: survey | movies | series | dupes)");
            std::process::exit(2);
        }
    }
}

// ---------- DUPES (read-only report) ----------

/// Index TorBox movies by TMDB id and report duplicate groups, proposing the migration-added
/// copy (torrent id > 37_300_000) for deletion where a pre-existing copy survives. Read-only:
/// deletes nothing — the proposed ids are reviewed, then deleted out-of-band.
async fn dedup_report(tb_token: &str, tmdb: Arc<TmdbClient>) {
    let tb: Arc<dyn DebridProvider> = Arc::new(TorBoxClient::new(tb_token.to_string()).expect("tb"));
    let torrents = match tb.get_torrents().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("dupes: TorBox get_torrents failed: {e}");
            return;
        }
    };
    eprintln!("Indexing {} TorBox torrents for duplicate movies…", torrents.len());
    // tmdb_id -> Vec<(torrent_id, name, is_migration_added)>
    let mut idx: BTreeMap<u64, Vec<(String, String, bool)>> = BTreeMap::new();
    for (i, t) in torrents.iter().enumerate() {
        if i % 25 == 0 {
            eprintln!("  …{}/{}", i, torrents.len());
        }
        let info = match tb.get_torrent_info(&t.id).await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let meta = identify_torrent(&info, &tmdb).await;
        if meta.media_type != MediaType::Movie {
            continue;
        }
        if let Some(id) = meta
            .external_id
            .as_deref()
            .and_then(|e| e.strip_prefix("tmdb:"))
            .and_then(|n| n.parse::<u64>().ok())
        {
            let is_mig = t.id.parse::<u64>().map(|n| n > 37_300_000).unwrap_or(false);
            idx.entry(id).or_default().push((t.id.clone(), t.filename.clone(), is_mig));
        }
    }
    let mut report = String::from("==== DUPLICATE MOVIES ON TORBOX ====\n");
    let mut to_delete: Vec<String> = Vec::new();
    let mut groups = 0usize;
    for (tmdb_id, entries) in &idx {
        if entries.len() < 2 {
            continue;
        }
        groups += 1;
        report.push_str(&format!("\ntmdb:{tmdb_id}\n"));
        for (id, name, mig) in entries {
            report.push_str(&format!(
                "  [{}] id={}  {}\n",
                if *mig { "MIGRATION" } else { "pre-exist" },
                id,
                name
            ));
        }
        let has_preexisting = entries.iter().any(|(_, _, m)| !m);
        if has_preexisting {
            for (id, name, mig) in entries {
                if *mig {
                    report.push_str(&format!("  -> propose DELETE migration copy id={} ({})\n", id, name));
                    to_delete.push(id.clone());
                }
            }
        } else {
            report.push_str("  (no pre-existing copy — left for manual review)\n");
        }
    }
    report.push_str(&format!(
        "\n{groups} duplicate tmdb group(s); {} migration copies proposed for deletion.\nProposed ids: {}\n",
        to_delete.len(),
        to_delete.join(",")
    ));
    println!("{report}");
    let _ = std::fs::write("migration-dupes.txt", &report);
    eprintln!("Report written to migration-dupes.txt — NO torrents deleted.");
}

// ---------- SURVEY (read-only) ----------

async fn survey(lib: &Library, tmdb: &TmdbClient, tb_token: &str, http: reqwest::Client) {
    let addon = std::env::var("SCRAPER_ADDON_URL").ok().filter(|s| !s.trim().is_empty());
    let scraper = TorrentioScraper::new(addon, ProviderKind::TorBox, tb_token, http);
    eprintln!("Checking TorBox availability for {} unique movies (throttled)…", lib.movies.len());
    let (mut cached, mut uncached, mut no_release, mut errors) = (0, 0, 0, 0);
    let mut lines = String::new();
    for (i, m) in lib.movies.iter().enumerate() {
        if i % 25 == 0 {
            eprintln!("  …checking {}/{}", i, lib.movies.len());
        }
        let imdb = match tmdb.external_imdb_id(m.tmdb_id, MediaType::Movie).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                no_release += 1;
                continue;
            }
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        match scraper.find(&imdb, MediaKind::Movie, None, None).await {
            Ok(c) if c.is_empty() => no_release += 1,
            Ok(c) => {
                let hit = c.iter().any(|x| release::parse(x).cached);
                if hit {
                    cached += 1;
                } else {
                    uncached += 1;
                }
                lines.push_str(&format!(
                    "{}  {} ({})  [rd: {}]\n",
                    if hit { "CACHED  " } else { "uncached" },
                    m.title,
                    m.year.as_deref().unwrap_or("?"),
                    m.rd_name
                ));
            }
            Err(_) => errors += 1,
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    let summary = format!(
        "\n==== SURVEY ====\nRD torrents: {}\nmovies: {} unique\nshows: {} ({} episodes)\nunidentified: {} ({} info errors)\n\nmovies vs TorBox: CACHED {} | uncached {} | no-release {} | errors {}\n",
        lib.total, lib.movies.len(), lib.shows.len(), lib.shows.values().map(|s| s.episodes.len()).sum::<usize>(),
        lib.unidentified.len(), lib.info_errors, cached, uncached, no_release, errors
    );
    println!("{summary}");
    let _ = std::fs::write("migration-survey.txt", format!("{summary}\n{lines}"));
    eprintln!("Survey complete. RD was not modified.");
}

// ---------- MOVIES execute ----------

/// Index the TorBox library by movie tmdb id → list of (torrent_id, is_migration_added).
/// Used to skip titles already present and to report duplicates the first run created.
async fn torbox_movie_index(
    tb: &Arc<dyn DebridProvider>,
    tmdb: &TmdbClient,
) -> BTreeMap<u64, Vec<(String, bool)>> {
    let mut idx: BTreeMap<u64, Vec<(String, bool)>> = BTreeMap::new();
    let torrents = match tb.get_torrents().await {
        Ok(t) => t,
        Err(_) => return idx,
    };
    eprintln!("Indexing {} TorBox torrents (identify for skip/dedup)…", torrents.len());
    for (i, t) in torrents.iter().enumerate() {
        if i % 25 == 0 {
            eprintln!("  …indexing {}/{}", i, torrents.len());
        }
        let info = match tb.get_torrent_info(&t.id).await {
            Ok(i) => i,
            Err(_) => continue,
        };
        let meta = identify_torrent(&info, tmdb).await;
        if meta.media_type != MediaType::Movie {
            continue;
        }
        if let Some(id) = meta
            .external_id
            .as_deref()
            .and_then(|e| e.strip_prefix("tmdb:"))
            .and_then(|n| n.parse::<u64>().ok())
        {
            let is_mig = t.id.parse::<u64>().map(|n| n > 37_300_000).unwrap_or(false);
            idx.entry(id).or_default().push((t.id.clone(), is_mig));
        }
    }
    idx
}

async fn migrate_movies(lib: &Library, tmdb: Arc<TmdbClient>, tb_token: &str, http: reqwest::Client) {
    let (engine, tb, _scraper, _acfg) = build_engine(tb_token, tmdb.clone(), http);

    // Skip titles already on TorBox (avoid duplicates) and surface existing duplicates to review.
    let tb_index = torbox_movie_index(&tb, &tmdb).await;
    let present: BTreeSet<u64> = tb_index.keys().copied().collect();
    let dupes: Vec<(u64, Vec<(String, bool)>)> = tb_index
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    eprintln!(
        "TorBox already holds {} unique movies; {} tmdb id(s) have duplicates (reported, not auto-deleted).",
        present.len(),
        dupes.len()
    );

    eprintln!("Acquiring missing movies on TorBox (cached → instant; uncached → downloads, monitored by the service)…");
    let (mut acquired, mut none, mut unavail, mut pending, mut already, mut skipped) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
    let mut log = String::new();
    for (i, m) in lib.movies.iter().enumerate() {
        let label = format!("{} ({})", m.title, m.year.as_deref().unwrap_or("?"));
        if present.contains(&m.tmdb_id) {
            already += 1;
            continue;
        }
        let imdb = match tmdb.external_imdb_id(m.tmdb_id, MediaType::Movie).await {
            Ok(Some(id)) => id,
            _ => {
                skipped += 1;
                log.push_str(&format!("SKIP(no-imdb)  {label}\n"));
                continue;
            }
        };
        let (title, year, original_language) =
            tmdb.details(m.tmdb_id, MediaType::Movie).await.unwrap_or_default();
        let req = AcquireRequest {
            imdb_id: imdb,
            tmdb_id: m.tmdb_id,
            kind: MediaKind::Movie,
            season: None,
            episode: None,
            original_language,
            metadata: MediaMetadata {
                title: if title.is_empty() { m.title.clone() } else { title },
                year: year.or_else(|| m.year.clone()),
                media_type: MediaType::Movie,
                external_id: Some(format!("tmdb:{}", m.tmdb_id)),
            },
        };
        match engine.acquire(req).await {
            AcquireOutcome::Acquired(_) => {
                acquired += 1;
                log.push_str(&format!("ACQUIRED      {label}\n"));
            }
            AcquireOutcome::Pending(_) => {
                // Normal path: an uncached release is downloading. The service's observe() loop
                // (watching the same account) monitors it to completion, or fails-and-reacquires
                // the next candidate on a genuine stall.
                pending += 1;
                log.push_str(&format!("PENDING(dl)   {label}\n"));
            }
            AcquireOutcome::NoAcceptableRelease => {
                none += 1;
                log.push_str(&format!("NONE          {label}\n"));
            }
            AcquireOutcome::TemporarilyUnavailable => {
                unavail += 1;
                log.push_str(&format!("UNAVAIL       {label}\n"));
            }
        }
        if i % 10 == 0 {
            eprintln!(
                "  …{}/{}  (acq {acquired} pending {pending} none {none} present {already})",
                i, lib.movies.len()
            );
        }
    }
    let mut dupe_report = String::from("\n--- duplicate movie tmdb ids on TorBox (review) ---\n");
    for (tmdb_id, entries) in &dupes {
        let ids: Vec<String> = entries
            .iter()
            .map(|(id, mig)| format!("{}{}", id, if *mig { "(migration)" } else { "" }))
            .collect();
        dupe_report.push_str(&format!("tmdb:{} -> {}\n", tmdb_id, ids.join(", ")));
    }
    let summary = format!(
        "\n==== MOVIES MIGRATION ====\nacquired (cached):        {acquired}\npending (downloading):    {pending}\nno acceptable release:    {none}\ntemporarily unavailable:  {unavail}\nalready on TorBox (skip): {already}\nno imdb (skip):           {skipped}\nduplicate tmdb ids:       {}\n",
        dupes.len()
    );
    println!("{summary}");
    let _ = std::fs::write("migration-movies.txt", format!("{summary}{dupe_report}\n{log}"));
    eprintln!("Movies migration complete. RD was not modified.");
}

// ---------- SERIES execute (season packs where possible) ----------

/// Is this candidate a whole-season pack for `season` (not a single episode)?
fn is_season_pack(c: &release::RawCandidate, season: u32) -> bool {
    let text = format!("{} {}", c.name, c.description).to_lowercase();
    let single = Regex::new(&format!(r"(?i)s0*{season}[._ ]?e\d", )).unwrap();
    if single.is_match(&text) {
        return false; // names a specific episode → not a pack
    }
    let season_markers = [
        format!("s{:02}", season),
        format!("s{}", season),
        format!("season {}", season),
        format!("season {:02}", season),
        format!("saison {}", season),
        format!("temporada {}", season),
    ];
    let has_season = season_markers.iter().any(|m| text.contains(m.as_str()));
    let has_complete = text.contains("complete") || text.contains("season");
    has_season || has_complete
}

async fn migrate_series(lib: &Library, tmdb: Arc<TmdbClient>, tb_token: &str, http: reqwest::Client) {
    let (engine, tb, scraper, acfg) = build_engine(tb_token, tmdb.clone(), http);
    eprintln!("Migrating {} shows (season packs where possible)…", lib.shows.len());
    let (mut packs, mut eps_acq, mut eps_pending, mut eps_none, mut skipped) = (0, 0, 0, 0, 0);
    let mut log = String::new();

    for (tmdb_id, show) in &lib.shows {
        let imdb = match tmdb.external_imdb_id(*tmdb_id, MediaType::Show).await {
            Ok(Some(id)) => id,
            _ => {
                skipped += 1;
                log.push_str(&format!("SKIP show (no imdb)  {}\n", show.title));
                continue;
            }
        };
        let (s_title, s_year, s_lang) = tmdb.details(*tmdb_id, MediaType::Show).await.unwrap_or_default();
        let show_title = if s_title.is_empty() { show.title.clone() } else { s_title };
        // Group the user's episodes by season.
        let mut by_season: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for (s, e) in &show.episodes {
            by_season.entry(*s).or_default().insert(*e);
        }
        for (season, eps) in by_season {
            let mut covered: BTreeSet<u32> = BTreeSet::new();
            // 1. Try a cached season pack.
            if let Some(pack_eps) =
                try_season_pack(&scraper, &tb, &tmdb, &imdb, *tmdb_id, season).await
            {
                packs += 1;
                log.push_str(&format!("PACK   {} S{:02}  ({} eps)\n", show_title, season, pack_eps.len()));
                covered.extend(pack_eps);
            }
            // 2. Acquire remaining episodes individually.
            for ep in eps {
                if covered.contains(&ep) {
                    continue;
                }
                let req = AcquireRequest {
                    imdb_id: imdb.clone(),
                    tmdb_id: *tmdb_id,
                    kind: MediaKind::Series,
                    season: Some(season),
                    episode: Some(ep),
                    original_language: s_lang.clone(),
                    metadata: MediaMetadata {
                        title: show_title.clone(),
                        year: s_year.clone().or_else(|| show.year.clone()),
                        media_type: MediaType::Show,
                        external_id: Some(format!("tmdb:{}", tmdb_id)),
                    },
                };
                match engine.acquire(req).await {
                    AcquireOutcome::Acquired(h) => {
                        eps_acq += 1;
                        // If a pack got pulled, mark its episodes covered too.
                        covered.extend(episodes_in(&tb, &h, season).await);
                        log.push_str(&format!("EP ACQ {} S{:02}E{:02}\n", show_title, season, ep));
                    }
                    AcquireOutcome::Pending(h) => {
                        eps_pending += 1;
                        covered.extend(episodes_in(&tb, &h, season).await);
                        log.push_str(&format!("EP PND {} S{:02}E{:02}\n", show_title, season, ep));
                    }
                    AcquireOutcome::NoAcceptableRelease | AcquireOutcome::TemporarilyUnavailable => {
                        eps_none += 1;
                        log.push_str(&format!("EP --- {} S{:02}E{:02}\n", show_title, season, ep));
                    }
                }
            }
        }
        eprintln!(
            "  …{} done  (packs {packs} epAcq {eps_acq} epPend {eps_pending} epNone {eps_none})",
            show_title
        );
    }
    let _ = acfg;
    let summary = format!(
        "\n==== SERIES MIGRATION ====\nseason packs acquired: {packs}\nindividual eps acquired (cached): {eps_acq}\nindividual eps pending: {eps_pending}\neps with no release: {eps_none}\nshows skipped (no imdb): {skipped}\n"
    );
    println!("{summary}");
    let _ = std::fs::write("migration-series.txt", format!("{summary}\n{log}"));
    eprintln!("Series migration complete. RD was not modified.");
}

/// Which episodes of `season` does the TorBox torrent `hash` contain (by parsing its file names)?
async fn episodes_in(tb: &Arc<dyn DebridProvider>, hash: &str, season: u32) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    if let Ok(list) = tb.get_torrents().await {
        if let Some(t) = list.iter().find(|t| t.hash.eq_ignore_ascii_case(hash)) {
            if let Ok(info) = tb.get_torrent_info(&t.id).await {
                for f in &info.files {
                    if let Some((s, e)) = parse_se(&f.path) {
                        if s == season {
                            out.insert(e);
                        }
                    }
                }
            }
        }
    }
    out
}

/// Try to acquire a cached season pack for `(imdb, season)`. Returns the covered episode numbers
/// on success. Validates the pack resolves to `show_tmdb_id` before keeping it.
async fn try_season_pack(
    scraper: &TorrentioScraper,
    tb: &Arc<dyn DebridProvider>,
    tmdb: &TmdbClient,
    imdb: &str,
    show_tmdb_id: u64,
    season: u32,
) -> Option<BTreeSet<u32>> {
    let cands = scraper.find(imdb, MediaKind::Series, Some(season), Some(1)).await.ok()?;
    let packs: Vec<_> = cands.iter().filter(|c| is_season_pack(c, season)).cloned().collect();
    if packs.is_empty() {
        return None;
    }
    let ranked = release::rank(packs.iter().map(release::parse).collect(), &AcquisitionConfig::default().prefs);
    for r in ranked {
        if !r.cached {
            continue; // packs-where-possible = only adopt a pack if it's cached/instant
        }
        let select = |info: &debridmoviemapper::rd_client::TorrentInfo| -> Vec<u32> {
            info.files.iter().filter(|f| is_video_file(&f.path)).map(|f| f.id).collect()
        };
        let (id, _post) = match reacquire::materialise(&**tb, &r.info_hash, Duration::from_secs(1), select).await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let info = match tb.get_torrent_info(&id).await {
            Ok(i) => i,
            Err(_) => {
                let _ = tb.delete_torrent(&id).await;
                continue;
            }
        };
        // Validate: a representative episode file must identify as this show.
        let repr = info
            .files
            .iter()
            .filter(|f| is_video_file(&f.path))
            .max_by_key(|f| f.bytes)
            .map(|f| f.path.rsplit('/').next().unwrap_or(&f.path).to_string());
        let ok = match repr {
            Some(name) => {
                let files = [debridmoviemapper::rd_client::TorrentFile {
                    id: 0,
                    path: name.clone(),
                    bytes: 0,
                    selected: 1,
                }];
                matches!(
                    identify_name(&name, &files, tmdb).await,
                    Some(m) if m.external_id.as_deref() == Some(format!("tmdb:{}", show_tmdb_id).as_str())
                )
            }
            None => false,
        };
        if !ok {
            let _ = tb.delete_torrent(&id).await;
            continue;
        }
        let eps: BTreeSet<u32> = info
            .files
            .iter()
            .filter_map(|f| parse_se(&f.path))
            .filter(|(s, _)| *s == season)
            .map(|(_, e)| e)
            .collect();
        if eps.len() <= 1 {
            // Not actually a multi-episode pack — let the per-episode path handle it cleanly.
            let _ = tb.delete_torrent(&id).await;
            continue;
        }
        return Some(eps);
    }
    None
}
