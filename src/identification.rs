use std::sync::LazyLock;
use regex::Regex;
use tracing::{info, warn, debug};
use crate::rd_client;
use crate::tmdb_client::{TmdbClient, TmdbSearchResult};
use crate::vfs::{MediaMetadata, MediaType, is_video_file, VIDEO_EXTENSIONS};

static CAMEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z])([A-Z])").unwrap());

static PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(\[.*?\]|\(.*?\)|[\w.-]+\.[a-z]{2,6}\s+-\s+|d3us-|m-|Bond[\s.]+\d+|James[\s.]*Bond|007)\s*").unwrap());

static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}\b").unwrap());

static STOP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(1080p|720p|2160p|4k|s\d+e\d+|s\d+|seasons?\s*\d+|\d+\s*seasons?|temporada\s*\d+|saison\s*\d+|\d+x\d+|episodes?\s*\d+|e\d+|parts?\s*\d+|vol(ume)?\s*\d+|bluray|web-dl|h264|h265|x264|x265|remux|multi|vff|custom|dts|dd5|dd\+5|ddp5|esub|webrip|hdtv|avc|hevc|aac|truehd|atmos|criterion|repack|completa|complete|pol|eng|ita|ger|fra|spa|esp|rus|ukr)\b").unwrap());

static YEAR_RANGE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}[\s-]+(19|20)\d{2}\b").unwrap());

static SHOW_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)s(\d+)\.?e(\d+)|s(\d+)|(\d+)x(\d+)|seasons?\s*\d+|\d+\s*seasons?|temporada\s*\d+|saison\s*\d+|e\d+").unwrap());

static GENERIC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(episode|season|part|volume|vol)\s*(\d+|[a-z])?$").unwrap());

pub async fn identify_torrent(info: &rd_client::TorrentInfo, tmdb: &TmdbClient) -> MediaMetadata {
    let representative_name = info.files.iter()
        .filter(|f| is_video_file(&f.path))
        .max_by_key(|f| f.bytes)
        .map(|f| f.path.split('/').next_back().unwrap_or(&f.path))
        .unwrap_or(&info.filename);

    if let Some(metadata) = identify_name(representative_name, &info.files, tmdb).await {
        // If it was identified as something very suspicious like "00000", maybe reject it?
        // But identify_name already does identification.
        return metadata;
    }

    if representative_name != info.filename {
        debug!("Filename '{}' was generic or no match found. Trying torrent name: '{}'", representative_name, info.filename);
        if let Some(metadata) = identify_name(&info.filename, &info.files, tmdb).await {
            return metadata;
        }
    }

    warn!("Could not identify torrent: {}. Filename: {}.", info.filename, representative_name);
    let (cleaned_torrent, year_t) = clean_name(&info.filename);
    if !cleaned_torrent.is_empty() {
        return MediaMetadata {
            title: cleaned_torrent,
            year: year_t,
            media_type: if is_show_guess(&info.files) { MediaType::Show } else { MediaType::Movie },
            external_id: None,
        };
    }

    let (cleaned_file, year_f) = clean_name(representative_name);
    let final_title = if !cleaned_file.is_empty() { cleaned_file } else { representative_name.to_string() };
    
    MediaMetadata {
        title: final_title,
        year: year_f,
        media_type: if is_show_guess(&info.files) { MediaType::Show } else { MediaType::Movie },
        external_id: None,
    }
}

fn best_scored_result<'a>(
    results: &'a [TmdbSearchResult],
    normalized_query: &str,
    year: &Option<String>,
    is_short_title: bool,
) -> Option<&'a TmdbSearchResult> {
    if results.is_empty() {
        return None;
    }
    if is_short_title {
        results.iter()
            .filter(|r| {
                let normalized_title = normalize_title(&r.title);
                let title_matches = normalized_title == normalized_query;
                let year_matches = year.as_ref()
                    .map(|y| r.release_date.as_ref().map(|rd| rd.starts_with(y)).unwrap_or(false))
                    .unwrap_or(false);
                title_matches && year_matches
            })
            .max_by(|a, b| {
                score_result(a, normalized_query, year)
                    .partial_cmp(&score_result(b, normalized_query, year))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    } else {
        results.iter().max_by(|a, b| {
            score_result(a, normalized_query, year)
                .partial_cmp(&score_result(b, normalized_query, year))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }
}

fn select_best_match(
    tv: Option<&TmdbSearchResult>,
    movie: Option<&TmdbSearchResult>,
    normalized_query: &str,
    year: &Option<String>,
    is_show_guess: bool,
) -> Option<(String, Option<String>, String, &'static str, MediaType)> {
    let is_exact = |r: &TmdbSearchResult| -> bool {
        normalize_title(&r.title) == normalized_query ||
        r.original_title.as_ref().map(|t| normalize_title(t) == normalized_query).unwrap_or(false)
    };
    let has_year_match = |r: &TmdbSearchResult| -> bool {
        year.as_ref()
            .map(|y| r.release_date.as_ref().map(|rd| rd.starts_with(y)).unwrap_or(false))
            .unwrap_or(false)
    };

    match (tv, movie) {
        (Some(tv), Some(movie)) => {
            let tv_exact = is_exact(tv);
            let movie_exact = is_exact(movie);
            let tv_year_match = has_year_match(tv);
            let movie_year_match = has_year_match(movie);
            let tv_exact_year = tv_exact && tv_year_match;
            let movie_exact_year = movie_exact && movie_year_match;

            let (selected, media_type) = if tv_exact_year && !movie_exact_year {
                (tv, MediaType::Show)
            } else if movie_exact_year && !tv_exact_year {
                (movie, MediaType::Movie)
            } else if is_show_guess && tv_year_match {
                (tv, MediaType::Show)
            } else if !is_show_guess && movie_year_match {
                (movie, MediaType::Movie)
            } else if tv_exact && !movie_exact {
                (tv, MediaType::Show)
            } else if movie_exact && !tv_exact {
                (movie, MediaType::Movie)
            } else if tv_year_match && !movie_year_match {
                (tv, MediaType::Show)
            } else if movie_year_match && !tv_year_match {
                (movie, MediaType::Movie)
            } else if is_show_guess {
                (tv, MediaType::Show)
            } else {
                (movie, MediaType::Movie)
            };
            Some((selected.title.clone(), selected.release_date.clone(), selected.id.to_string(), "tmdb", media_type))
        }
        (Some(tv), None) => Some((tv.title.clone(), tv.release_date.clone(), tv.id.to_string(), "tmdb", MediaType::Show)),
        (None, Some(movie)) => Some((movie.title.clone(), movie.release_date.clone(), movie.id.to_string(), "tmdb", MediaType::Movie)),
        (None, None) => None,
    }
}

/// Score a search result based on how well it matches the query
/// Higher score = better match
fn score_result(result: &TmdbSearchResult, normalized_query: &str, year: &Option<String>) -> f64 {
    let mut score = 0.0;

    // Title match (most important)
    let normalized_title = normalize_title(&result.title);
    let normalized_original = result.original_title.as_ref().map(|t| normalize_title(t));

    if normalized_title == normalized_query || normalized_original.as_ref().map(|t| t == normalized_query).unwrap_or(false) {
        score += 1000.0; // Exact title match
    } else if normalized_title.contains(normalized_query) || normalized_original.as_ref().map(|t| t.contains(normalized_query)).unwrap_or(false) {
        score += 100.0; // Partial title match
    }

    // Vote count and rating — smooth scaling to avoid cliffs between similarly-popular content
    let vote_count = result.vote_count.unwrap_or(0);
    if vote_count >= 10 {
        let vc = vote_count as f64;
        // Weight ramps smoothly: 5.0 at 10 votes → 10.0 at 100 → 15.0 at 1000+
        let weight = (vc.log10() * 5.0).min(15.0);
        score += result.vote_average.unwrap_or(0.0) * weight;
        score += vc.log10() * 30.0;
    }

    // Year match (important, but with tolerance for off-by-one errors)
    if let Some(y) = year {
        if let Some(release_date) = &result.release_date {
            if let Ok(query_year) = y.parse::<i32>() {
                if let Some(release_year_str) = release_date.get(0..4) {
                    if let Ok(release_year) = release_year_str.parse::<i32>() {
                        let year_diff = (query_year - release_year).abs();
                        if year_diff == 0 {
                            score += 200.0; // Exact year match
                        } else if year_diff == 1 {
                            score += 150.0; // Close year match (±1 year)
                        }
                    }
                }
            }
        }
    }

    // Recency bonus when no year is specified in the filename.
    // Helps disambiguate same-titled content (e.g., two shows both called "Sherwood")
    // by slightly preferring more recently released content.
    if year.is_none() {
        if let Some(release_date) = &result.release_date {
            if let Some(year_str) = release_date.get(0..4) {
                if let Ok(release_year) = year_str.parse::<i32>() {
                    let current_year = (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() / 31_557_600) as i32 + 1970;
                    let age = (current_year - release_year).max(0) as f64;
                    score += (80.0 - age * 8.0).max(0.0);
                }
            }
        }
    }

    // Popularity (minor tiebreaker)
    score += result.popularity;

    score
}

pub async fn identify_name(name: &str, files: &[rd_client::TorrentFile], tmdb: &TmdbClient) -> Option<MediaMetadata> {
    let (cleaned_name, year) = clean_name(name);
    if cleaned_name.is_empty() || is_generic_title(&cleaned_name) {
        return None;
    }
    let is_show_guess = is_show_guess(files);

    let normalized_cleaned = normalize_title(&cleaned_name);

    // Try TMDB first
    let (tv_initial, movie_initial) = tokio::join!(
        tmdb.search_tv(&cleaned_name, year.as_deref()),
        tmdb.search_movie(&cleaned_name, year.as_deref())
    );
    let mut tv_results = tv_initial.clone();
    let mut movie_results = movie_initial.clone();

    // If no results found, try CamelCase splitting as a fallback
    if tv_results.is_empty() && movie_results.is_empty() {
        let split_name = CAMEL_RE.replace_all(&cleaned_name, "$1 $2").to_string();
        if split_name != cleaned_name {
            debug!("No results for '{}', trying CamelCase split: '{}'", cleaned_name, split_name);
            let (tv_extra, movie_extra) = tokio::join!(
                tmdb.search_tv(&split_name, year.as_deref()),
                tmdb.search_movie(&split_name, year.as_deref())
            );
            tv_results.extend(tv_extra);
            movie_results.extend(movie_extra);
        }
    }

    // Helper to check if results contain exact title match
    let has_exact_match = |results: &[TmdbSearchResult]| -> bool {
        results.iter().any(|r| {
            normalize_title(&r.title) == normalized_cleaned ||
            r.original_title.as_ref().map(|t| normalize_title(t) == normalized_cleaned).unwrap_or(false)
        })
    };

    let tv_has_exact = has_exact_match(&tv_results);
    let movie_has_exact = has_exact_match(&movie_results);

    // If we have a year but no exact title match yet, try searching without the year
    if !tv_has_exact && !movie_has_exact && year.is_some() {
        let (tv_no_year, movie_no_year) = tokio::join!(
            tmdb.search_tv(&cleaned_name, None),
            tmdb.search_movie(&cleaned_name, None)
        );
        tv_results.extend(tv_no_year);
        movie_results.extend(movie_no_year);
    }

    // Score all results and pick the best TV and movie matches
    // For short titles (≤3 chars), require exact match + year match
    let is_short_title = cleaned_name.len() <= 3;

    let best_tv = best_scored_result(&tv_results, &normalized_cleaned, &year, is_short_title);
    let best_movie = best_scored_result(&movie_results, &normalized_cleaned, &year, is_short_title);

    let selected = select_best_match(best_tv, best_movie, &normalized_cleaned, &year, is_show_guess);

    if let Some((title, release_date, id, source, mtype)) = selected {
        let year_val = release_date.map(|d| d.chars().filter(|c| c.is_ascii_digit()).take(4).collect());
        info!("Identified {} ({:?}) as {:?} via TMDB (ID: {})", title, year_val, mtype, id);
        return Some(MediaMetadata {
            title,
            year: year_val,
            media_type: mtype,
            external_id: Some(format!("{}:{}", source, id)),
        });
    }

    None
}

pub fn clean_name(name: &str) -> (String, Option<String>) {
    let mut title = name.to_string();
    
    // 0. Remove file extension if present
    if let Some(pos) = title.rfind('.') {
        let ext = &title[pos..].to_lowercase();
        if VIDEO_EXTENSIONS.iter().any(|e| *e == ext) {
            title.truncate(pos);
        }
    }

    // 1. Remove common site prefixes and garbage at the start
    // Patterns like "[ site ] ", "( site )", "site.com - ", "d3us-", "m-", "Bond.50."
    if let Some(m) = PREFIX_RE.find(&title) {
        // Only strip if it's followed by a separator (dot, space, dash) or it's a known prefix
        title = title[m.end()..].trim_start_matches(|c: char| !c.is_alphanumeric()).to_string();
    }

    // 2. Initial cleanup: replace dots and underscores with spaces
    title = title.replace(['.', '_'], " ");

    // 3. Handle "aka" - usually title aka alternative title
    if let Some(pos) = title.to_lowercase().find(" aka ") {
        // Prefer the part after "aka" as it's often the English title in non-English releases
        let after_aka = &title[pos + 5..];
        if !after_aka.trim().is_empty() {
            title = after_aka.to_string();
        }
    }

    // 4. Find year (19xx or 20xx)
    let year = YEAR_RE.find(&title).map(|m| m.as_str().to_string());

    // 5. Handle stop words (technical metadata, quality, codecs, season info)
    while let Some(m) = STOP_RE.find(&title) {
        if m.start() == 0 {
            // Metadata at start, strip it
            title = title[m.end()..].to_string();
            title = title.trim_start_matches(|c: char| !c.is_alphanumeric()).to_string();
            if title.is_empty() { break; }
        } else {
            // Metadata in middle/end, truncate
            title.truncate(m.start());
            break;
        }
    }

    // 6. Truncate at year if it appears in title (and is not at the very start)
    if let Some(m) = YEAR_RE.find(&title) {
        if m.start() > 0 {
            // Check if this year is part of a range (e.g. 1985-1999 or 1985 1999)
            if !YEAR_RANGE_RE.is_match(&title) {
                title.truncate(m.start());
            }
        }
    }

    // 7. Final cleanup: remove trailing non-alphanumeric characters and trim
    title = title.trim_end_matches(|c: char| !c.is_alphanumeric() && c != ')' && c != ']').to_string();

    (title.trim().to_string(), year)
}

pub fn is_show_guess(files: &[rd_client::TorrentFile]) -> bool {
    files.iter().any(|f| {
        let filename = f.path.split('/').next_back().unwrap_or(&f.path);
        SHOW_RE.is_match(filename)
    }) ||
    files.iter().filter(|f| is_video_file(&f.path)).count() > 1
}

pub fn normalize_title(s: &str) -> String {
    s.to_lowercase()
        .replace(" and ", " & ") // Standardize 'and' to '&' for comparison
        .chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            _ => c,
        })
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn is_generic_title(s: &str) -> bool {
    let lower = s.to_lowercase();
    if lower.is_empty() { return true; }

    // Check if it's just a sequence of 5 or more digits (like BDMV 00000.m2ts)
    // or if it's a very small number like 0, 1, 2
    if s.chars().all(|c| c.is_ascii_digit()) {
        if s.len() >= 5 {
            return true;
        }
        if let Ok(n) = s.parse::<u32>() {
            if n < 10 {
                return true;
            }
        }
    }

    // Generic terms that might have survived cleaning
    if GENERIC_RE.is_match(&lower) {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rd_client::{TorrentInfo, TorrentFile};

    #[tokio::test]
    #[ignore]
    async fn test_repro_00000_issue() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        let info = TorrentInfo {
            id: "test_id".to_string(),
            filename: "Inception.2010.BluRay.REMUX.1080p.mkv".to_string(),
            original_filename: "Inception.2010.BluRay.REMUX.1080p.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 30000000000,
            original_bytes: 30000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2023-01-01".to_string(),
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "BDMV/STREAM/00000.m2ts".to_string(),
                    bytes: 25000000000,
                    selected: 1,
                }
            ],
            links: vec!["http://link1".to_string()],
            ended: Some("2023-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;
        
        // It should NOT be identified as "00000" [id=tmdb:886864]
        assert_ne!(metadata.external_id, Some("tmdb:886864".to_string()));
        assert_eq!(metadata.title, "Inception");
    }

    #[tokio::test]
    #[ignore]
    async fn test_2012_is_not_generic() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        let info = TorrentInfo {
            id: "test_id_2012".to_string(),
            filename: "2012.mkv".to_string(),
            original_filename: "2012.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 1000000000,
            original_bytes: 1000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2023-01-01".to_string(),
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "2012.mkv".to_string(),
                    bytes: 1000000000,
                    selected: 1,
                }
            ],
            links: vec!["http://link1".to_string()],
            ended: Some("2023-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;
        assert_eq!(metadata.title, "2012");
    }

    #[tokio::test]
    #[ignore]
    async fn test_peaky_blinders_identification() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        let info = TorrentInfo {
            id: "peaky_id".to_string(),
            filename: "Peaky.Blinders.S01.1080p.BluRay.x264-DON".to_string(),
            original_filename: "Peaky.Blinders.S01.1080p.BluRay.x264-DON".to_string(),
            hash: "hash".to_string(),
            bytes: 30000000000,
            original_bytes: 30000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2023-01-01".to_string(),
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "1x05 Episode 5.mkv".to_string(),
                    bytes: 2000000000,
                    selected: 1,
                }
            ],
            links: vec!["http://link1".to_string()],
            ended: Some("2023-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;
        
        assert_eq!(metadata.media_type, MediaType::Show);
        assert_eq!(metadata.title, "Peaky Blinders");
        assert_eq!(metadata.external_id, Some("tmdb:60574".to_string()));
    }

    #[tokio::test]
    async fn test_is_generic_title() {
        assert!(is_generic_title("00000"));
        assert!(is_generic_title("1"));
        assert!(is_generic_title("Episode 5"));
        assert!(is_generic_title("Season 1"));
        assert!(is_generic_title("Part 2"));
        assert!(is_generic_title("Volume 10"));
        assert!(is_generic_title("Vol 3"));
        assert!(is_generic_title("Episode"));
        assert!(is_generic_title("Part A"));
        
        assert!(!is_generic_title("Inception"));
        assert!(!is_generic_title("2012")); // 4 digits, but not < 10
        assert!(!is_generic_title("The Episode"));
    }

    #[tokio::test]
    #[ignore]
    async fn test_short_name_no_random_match() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        // "UC" cleans to "UC"
        let info = TorrentInfo {
            id: "uc_id".to_string(),
            filename: "UC.S01.1080p.mkv".to_string(),
            original_filename: "UC.S01.1080p.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 1000000000,
            original_bytes: 1000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2023-01-01".to_string(),
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "UC.S01E01.mkv".to_string(),
                    bytes: 1000000000,
                    selected: 1,
                }
            ],
            links: vec!["http://link1".to_string()],
            ended: Some("2023-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;
        
        // It should NOT be identified as Gundam Unicorn (45500) because "UC" is too short for a broad match fallback
        assert_ne!(metadata.external_id, Some("tmdb:45500".to_string()));
        // It should fallback to cleaned name "UC"
        assert_eq!(metadata.title, "UC");
        assert_eq!(metadata.external_id, None);
    }

    #[tokio::test]
    #[ignore]
    async fn test_flow_2024_prefers_popular() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        let info = TorrentInfo {
            id: "flow_id".to_string(),
            filename: "Flow.2024.1080p.BluRay.x264.mkv".to_string(),
            original_filename: "Flow.2024.1080p.BluRay.x264.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 5000000000,
            original_bytes: 5000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2024-01-01".to_string(),
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "Flow.2024.1080p.BluRay.x264.mkv".to_string(),
                    bytes: 5000000000,
                    selected: 1,
                }
            ],
            links: vec!["http://link1".to_string()],
            ended: Some("2024-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should identify as the popular Oscar-nominated animated film (823219)
        // NOT the 8-minute short film (1281775)
        assert_eq!(metadata.external_id, Some("tmdb:823219".to_string()));
        assert_eq!(metadata.title, "Flow");
        assert_eq!(metadata.media_type, MediaType::Movie);
    }

    #[tokio::test]
    #[ignore]
    async fn test_sherwood_s02_disambiguation() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        let info = TorrentInfo {
            id: "sherwood_id".to_string(),
            filename: "Sherwood.S02E01.1080p.WEB.H264-DiMEPiECE.mkv".to_string(),
            original_filename: "Sherwood.S02E01.1080p.WEB.H264-DiMEPiECE.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 2000000000,
            original_bytes: 2000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2025-01-01".to_string(),
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "Sherwood.S02E01.1080p.WEB.H264-DiMEPiECE.mkv".to_string(),
                    bytes: 2000000000,
                    selected: 1,
                },
            ],
            links: vec!["http://link1".to_string()],
            ended: Some("2025-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should be Sherwood (155243), NOT the wrong one (87399)
        assert_eq!(metadata.external_id, Some("tmdb:155243".to_string()));
    }

    #[tokio::test]
    #[ignore]
    async fn test_dune_2000_identification() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        let info = TorrentInfo {
            id: "dune_id".to_string(),
            filename: "Dune.2000.S01E01.1080p.BluRay.x264-PFa.mkv".to_string(),
            original_filename: "Dune.2000.S01E01.1080p.BluRay.x264-PFa.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 3000000000,
            original_bytes: 3000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2000-01-01".to_string(),
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "Dune.2000.S01E01.1080p.BluRay.x264-PFa.mkv".to_string(),
                    bytes: 3000000000,
                    selected: 1,
                }
            ],
            links: vec!["http://link1".to_string()],
            ended: Some("2000-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should identify as Frank Herbert's Dune miniseries (19566)
        // NOT Fly Tales (15911) or Dune 2021 (438631)
        assert_eq!(metadata.external_id, Some("tmdb:19566".to_string()));
        assert_eq!(metadata.title, "Frank Herbert's Dune");
        assert_eq!(metadata.media_type, MediaType::Show);
    }
}
