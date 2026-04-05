use crate::rd_client;
use crate::tmdb_client::{TmdbClient, TmdbSearchResult};
use crate::vfs::{is_video_file, MediaMetadata, MediaType, VIDEO_EXTENSIONS};
use chrono::Datelike;
use regex::Regex;
use std::sync::LazyLock;
use tracing::{debug, info, warn};

static CAMEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z])([A-Z])").unwrap());

static PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(\[.*?\]|\(.*?\)|[\w.-]+\.[a-z]{2,6}\s+-\s+)\s*").unwrap());

static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}\b").unwrap());

static STOP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(1080p|720p|2160p|4k|s\d+e\d+|s\d+|seasons?\s*\d+|\d+\s*seasons?|temporada\s*\d+|saison\s*\d+|\d+x\d+|episodes?\s*\d+|e\d+|parts?\s*\d+|vol(ume)?\s*\d+|bluray|web-dl|h264|h265|x264|x265|remux|multi|vff|custom|dts|dd5|dd\+5|ddp5|esub|webrip|hdtv|avc|hevc|aac|truehd|atmos|criterion|repack|completa|complete|pol|eng|ita|ger|fra|spa|esp|rus|ukr)\b").unwrap()
});

static YEAR_RANGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}[\s-]+(19|20)\d{2}\b").unwrap());

static SHOW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)s(\d+)\.?e(\d+)|s(\d+)|(\d+)x(\d+)|seasons?\s*\d+|\d+\s*seasons?|temporada\s*\d+|saison\s*\d+|e\d+").unwrap()
});

static GENERIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(episode|season|part|volume|vol)\s*(\d+|[a-z])?$").unwrap());

pub async fn identify_torrent(info: &rd_client::TorrentInfo, tmdb: &TmdbClient) -> MediaMetadata {
    let representative_name = info
        .files
        .iter()
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
        debug!(
            "Filename '{}' was generic or no match found. Trying torrent name: '{}'",
            representative_name, info.filename
        );
        if let Some(metadata) = identify_name(&info.filename, &info.files, tmdb).await {
            return metadata;
        }
    }

    warn!(
        "Could not identify torrent: {}. Filename: {}.",
        info.filename, representative_name
    );
    let (cleaned_torrent, year_t) = clean_name(&info.filename);
    if !cleaned_torrent.is_empty() {
        return MediaMetadata {
            title: cleaned_torrent,
            year: year_t,
            media_type: if is_show_guess(&info.files) {
                MediaType::Show
            } else {
                MediaType::Movie
            },
            external_id: None,
        };
    }

    let (cleaned_file, year_f) = clean_name(representative_name);
    let final_title = if !cleaned_file.is_empty() {
        cleaned_file
    } else {
        representative_name.to_string()
    };

    MediaMetadata {
        title: final_title,
        year: year_f,
        media_type: if is_show_guess(&info.files) {
            MediaType::Show
        } else {
            MediaType::Movie
        },
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
        results
            .iter()
            .filter(|r| {
                let normalized_title = normalize_title(&r.title);
                let title_matches = normalized_title == normalized_query;
                let year_matches = year
                    .as_ref()
                    .map(|y| {
                        r.release_date
                            .as_ref()
                            .map(|rd| rd.starts_with(y))
                            .unwrap_or(false)
                    })
                    .unwrap_or(true); // No year in filename → exact title alone is sufficient
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
        normalize_title(&r.title) == normalized_query
            || r.original_title
                .as_ref()
                .map(|t| normalize_title(t) == normalized_query)
                .unwrap_or(false)
    };
    let has_year_match = |r: &TmdbSearchResult| -> bool {
        year.as_ref()
            .map(|y| {
                r.release_date
                    .as_ref()
                    .map(|rd| rd.starts_with(y))
                    .unwrap_or(false)
            })
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
            Some((
                selected.title.clone(),
                selected.release_date.clone(),
                selected.id.to_string(),
                "tmdb",
                media_type,
            ))
        }
        (Some(tv), None) => Some((
            tv.title.clone(),
            tv.release_date.clone(),
            tv.id.to_string(),
            "tmdb",
            MediaType::Show,
        )),
        (None, Some(movie)) => Some((
            movie.title.clone(),
            movie.release_date.clone(),
            movie.id.to_string(),
            "tmdb",
            MediaType::Movie,
        )),
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

    if normalized_title == normalized_query
        || normalized_original
            .as_ref()
            .map(|t| t == normalized_query)
            .unwrap_or(false)
    {
        score += 1000.0; // Exact title match
    } else if normalized_title.contains(normalized_query)
        || normalized_original
            .as_ref()
            .map(|t| t.contains(normalized_query))
            .unwrap_or(false)
    {
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
                    let current_year = chrono::Utc::now().year();
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

pub async fn identify_name(
    name: &str,
    files: &[rd_client::TorrentFile],
    tmdb: &TmdbClient,
) -> Option<MediaMetadata> {
    let (cleaned_name, year) = clean_name(name);
    if cleaned_name.is_empty() || is_generic_title(&cleaned_name) {
        return None;
    }
    let is_show_guess = is_show_guess(files);

    let normalized_cleaned = normalize_title(&cleaned_name);

    // Try TMDB first
    let (mut tv_results, mut movie_results) = tokio::join!(
        tmdb.search_tv(&cleaned_name, year.as_deref()),
        tmdb.search_movie(&cleaned_name, year.as_deref())
    );

    // If no results found, try CamelCase splitting as a fallback
    if tv_results.is_empty() && movie_results.is_empty() {
        let split_name = CAMEL_RE.replace_all(&cleaned_name, "$1 $2").to_string();
        if split_name != cleaned_name {
            debug!(
                "No results for '{}', trying CamelCase split: '{}'",
                cleaned_name, split_name
            );
            let (tv_extra, movie_extra) = tokio::join!(
                tmdb.search_tv(&split_name, year.as_deref()),
                tmdb.search_movie(&split_name, year.as_deref())
            );
            tv_results.extend(tv_extra);
            movie_results.extend(movie_extra);
        }
    }

    // If still no results, try stripping leading words (handles franchise prefixes like "Bond 50 Goldfinger")
    if tv_results.is_empty() && movie_results.is_empty() {
        let words: Vec<&str> = cleaned_name.split_whitespace().collect();
        for start in 1..words.len() {
            let stripped = words[start..].join(" ");
            if stripped.is_empty() || is_generic_title(&stripped) {
                continue;
            }
            debug!(
                "No results for '{}', trying stripped: '{}'",
                cleaned_name, stripped
            );
            let (tv_extra, movie_extra) = tokio::join!(
                tmdb.search_tv(&stripped, year.as_deref()),
                tmdb.search_movie(&stripped, year.as_deref())
            );
            if !tv_extra.is_empty() || !movie_extra.is_empty() {
                tv_results.extend(tv_extra);
                movie_results.extend(movie_extra);
                break;
            }
        }
    }

    // If still no results and name contains a dash, try the part after the first dash
    // (handles release group prefixes like "d3us-Title" or "m-Title")
    if tv_results.is_empty() && movie_results.is_empty() {
        if let Some(pos) = cleaned_name.find('-') {
            let after_dash = cleaned_name[pos + 1..].trim();
            if !after_dash.is_empty() && !is_generic_title(after_dash) {
                debug!(
                    "No results for '{}', trying after dash: '{}'",
                    cleaned_name, after_dash
                );
                let (tv_extra, movie_extra) = tokio::join!(
                    tmdb.search_tv(after_dash, year.as_deref()),
                    tmdb.search_movie(after_dash, year.as_deref())
                );
                tv_results.extend(tv_extra);
                movie_results.extend(movie_extra);
            }
        }
    }

    // Helper to check if results contain exact title match
    let has_exact_match = |results: &[TmdbSearchResult]| -> bool {
        results.iter().any(|r| {
            normalize_title(&r.title) == normalized_cleaned
                || r.original_title
                    .as_ref()
                    .map(|t| normalize_title(t) == normalized_cleaned)
                    .unwrap_or(false)
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

    let selected = select_best_match(
        best_tv,
        best_movie,
        &normalized_cleaned,
        &year,
        is_show_guess,
    );

    if let Some((title, release_date, id, source, mtype)) = selected {
        let year_val =
            release_date.map(|d| d.chars().filter(|c| c.is_ascii_digit()).take(4).collect());
        info!(
            "Identified {} ({:?}) as {:?} via TMDB (ID: {})",
            title, year_val, mtype, id
        );
        return Some(MediaMetadata {
            title,
            year: year_val,
            media_type: mtype,
            external_id: Some(format!("{}:{}", source, id)),
        });
    }

    None
}

/// Case-insensitive search for an ASCII `needle` in `haystack`, returning
/// the byte offset in `haystack` (safe for slicing). This avoids the
/// `to_lowercase()` byte-offset mismatch that can cause panics with
/// multi-byte UTF-8 characters.
fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    debug_assert!(needle.is_ascii(), "needle must be ASCII");
    let needle_len = needle.len();
    if haystack.len() < needle_len {
        return None;
    }
    let needle_bytes: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
    // Walk byte-by-byte; since needle is all ASCII, a match means all bytes
    // in the range are ASCII too, so `i` and `i + needle_len` are both valid
    // char boundaries.
    for i in 0..=(haystack.len() - needle_len) {
        let matches = haystack.as_bytes()[i..i + needle_len]
            .iter()
            .zip(&needle_bytes)
            .all(|(h, n)| h.to_ascii_lowercase() == *n);
        if matches {
            return Some(i);
        }
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
        title = title[m.end()..]
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .to_string();
    }

    // 2. Initial cleanup: replace dots and underscores with spaces
    title = title.replace(['.', '_'], " ");

    // 3. Handle "aka" - usually title aka alternative title
    // Search case-insensitively by finding " aka " on the original title using
    // a regex-free approach that respects UTF-8 boundaries. We scan for the
    // pattern in the lowercased version but use char_indices on the original
    // to find a byte-boundary-safe offset.
    if let Some(aka_pos) = find_case_insensitive(&title, " aka ") {
        let after_aka = &title[aka_pos + 5..];
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
            title = title
                .trim_start_matches(|c: char| !c.is_alphanumeric())
                .to_string();
            if title.is_empty() {
                break;
            }
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
    title = title
        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != ')' && c != ']')
        .to_string();

    (title.trim().to_string(), year)
}

pub fn is_show_guess(files: &[rd_client::TorrentFile]) -> bool {
    files.iter().any(|f| {
        let filename = f.path.split('/').next_back().unwrap_or(&f.path);
        SHOW_RE.is_match(filename)
    }) || files
        .iter()
        .filter(|f| f.selected != 0 && is_video_file(&f.path))
        .count()
        > 1
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
    if lower.is_empty() {
        return true;
    }

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
    use crate::rd_client::{TorrentFile, TorrentInfo};

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
            files: vec![TorrentFile {
                id: 1,
                path: "BDMV/STREAM/00000.m2ts".to_string(),
                bytes: 25000000000,
                selected: 1,
            }],
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
            files: vec![TorrentFile {
                id: 1,
                path: "2012.mkv".to_string(),
                bytes: 1000000000,
                selected: 1,
            }],
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
            files: vec![TorrentFile {
                id: 1,
                path: "1x05 Episode 5.mkv".to_string(),
                bytes: 2000000000,
                selected: 1,
            }],
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

    #[test]
    fn find_case_insensitive_basic() {
        assert_eq!(find_case_insensitive("Hello AKA World", " aka "), Some(5));
        assert_eq!(find_case_insensitive("Hello Aka World", " aka "), Some(5));
        assert_eq!(find_case_insensitive("Hello aka World", " aka "), Some(5));
        assert_eq!(find_case_insensitive("No match here", " aka "), None);
        assert_eq!(find_case_insensitive("aka", " aka "), None); // too short
    }

    #[test]
    fn find_case_insensitive_unicode_safe() {
        // Multi-byte UTF-8 characters before the pattern must not cause a panic.
        // 'Ü' is 2 bytes in UTF-8, so "Üntersuchung" is 13 bytes, not 12.
        assert_eq!(
            find_case_insensitive("Üntersuchung aka Study", " aka "),
            Some(13)
        );
        // Multi-byte char right before " aka " - the 'ü' is 2 bytes,
        // so "Tü" is 3 bytes and the space before "aka" is at byte offset 3.
        assert_eq!(find_case_insensitive("Tü aka X", " aka "), Some(3));
        // Only multi-byte chars, no match
        assert_eq!(find_case_insensitive("日本語テスト", " aka "), None);
    }

    #[test]
    fn clean_name_handles_aka_with_unicode() {
        // Verify that clean_name's "aka" handling doesn't panic on multi-byte UTF-8
        let (name, _year) = clean_name("Üntersuchung aka Study 2024 1080p");
        assert_eq!(name, "Study");
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
            files: vec![TorrentFile {
                id: 1,
                path: "UC.S01E01.mkv".to_string(),
                bytes: 1000000000,
                selected: 1,
            }],
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
            files: vec![TorrentFile {
                id: 1,
                path: "Flow.2024.1080p.BluRay.x264.mkv".to_string(),
                bytes: 5000000000,
                selected: 1,
            }],
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
            files: vec![TorrentFile {
                id: 1,
                path: "Sherwood.S02E01.1080p.WEB.H264-DiMEPiECE.mkv".to_string(),
                bytes: 2000000000,
                selected: 1,
            }],
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
            files: vec![TorrentFile {
                id: 1,
                path: "Dune.2000.S01E01.1080p.BluRay.x264-PFa.mkv".to_string(),
                bytes: 3000000000,
                selected: 1,
            }],
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

    #[tokio::test]
    #[ignore]
    async fn test_don_2022_tamil_identification() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        let info = TorrentInfo {
            id: "don_id".to_string(),
            filename: "Don (2022) UNCUT 1080p 10bit NF WEBRip x265 HEVC [Org YT Hindi DD 2.0 ~192Kbps + Tamil DD 5.1] ESub ~ Immortal.mkv".to_string(),
            original_filename: "Don (2022) UNCUT 1080p 10bit NF WEBRip x265 HEVC [Org YT Hindi DD 2.0 ~192Kbps + Tamil DD 5.1] ESub ~ Immortal.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 2000000000,
            original_bytes: 2000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2022-05-12".to_string(),
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "Don (2022) UNCUT 1080p 10bit NF WEBRip x265 HEVC [Org YT Hindi DD 2.0 ~192Kbps + Tamil DD 5.1] ESub ~ Immortal.mkv".to_string(),
                    bytes: 2000000000,
                    selected: 1,
                }
            ],
            links: vec!["http://link1".to_string()],
            ended: Some("2022-05-12".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should identify as the 2022 Tamil movie "Don" (TMDB 895033)
        // Filename contains Hindi+Tamil audio tracks; this is the same Tamil film
        // tested in tests/test_short_titles.rs with a generic filename.
        // NOT "Makoto Kitano: Don't You Guys Go..." (1266372)
        assert_eq!(metadata.external_id, Some("tmdb:895033".to_string()));
        assert_eq!(metadata.title, "Don");
        assert_eq!(metadata.media_type, MediaType::Movie);
    }

    #[tokio::test]
    #[ignore]
    async fn test_bond_collection_prefix_stripped_generically() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        let info = TorrentInfo {
            id: "bond_id".to_string(),
            filename: "Bond.50.Goldfinger.1964.1080p.BluRay.x264.mkv".to_string(),
            original_filename: "Bond.50.Goldfinger.1964.1080p.BluRay.x264.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 5000000000,
            original_bytes: 5000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "1964-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "Bond.50.Goldfinger.1964.1080p.BluRay.x264.mkv".to_string(),
                bytes: 5000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("1964-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should identify as Goldfinger (658) via word-stripping fallback
        assert_eq!(metadata.external_id, Some("tmdb:658".to_string()));
        assert_eq!(metadata.title, "Goldfinger");
        assert_eq!(metadata.media_type, MediaType::Movie);
    }

    #[tokio::test]
    #[ignore]
    async fn test_short_title_ted_identified() {
        dotenvy::dotenv().ok();
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key);

        // "ted" is a 3-char title with no year — should still identify via TMDB
        let info = TorrentInfo {
            id: "ted_id".to_string(),
            filename: "ted.S02E01.Talk.Dirty.to.Me.1080p.AMZN.WEB-DL.DDP5.1.H.264-RAWR.mkv"
                .to_string(),
            original_filename:
                "ted.S02E01.Talk.Dirty.to.Me.1080p.AMZN.WEB-DL.DDP5.1.H.264-RAWR.mkv"
                    .to_string(),
            hash: "hash_ted".to_string(),
            bytes: 3_000_000_000,
            original_bytes: 3_000_000_000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2024-06-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "ted.S02E01.Talk.Dirty.to.Me.1080p.AMZN.WEB-DL.DDP5.1.H.264-RAWR.mkv"
                    .to_string(),
                bytes: 3_000_000_000,
                selected: 1,
            }],
            links: vec!["http://link_ted".to_string()],
            ended: Some("2024-06-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        assert_eq!(metadata.media_type, MediaType::Show);
        assert!(
            metadata.external_id.is_some(),
            "Short title 'ted' should be identified via TMDB, got external_id=None"
        );
    }

    // --- Helper for best_scored_result / select_best_match tests ---

    fn make_result(
        id: u32,
        title: &str,
        release_date: Option<&str>,
        popularity: f64,
        vote_avg: Option<f64>,
        vote_count: Option<u32>,
    ) -> TmdbSearchResult {
        TmdbSearchResult {
            id,
            title: title.to_string(),
            original_title: None,
            release_date: release_date.map(|s| s.to_string()),
            popularity,
            vote_average: vote_avg,
            vote_count,
        }
    }

    // --- Tests for best_scored_result ---

    #[test]
    fn best_scored_result_empty_returns_none() {
        let results: Vec<TmdbSearchResult> = vec![];
        let got = best_scored_result(&results, "anything", &None, false);
        assert!(got.is_none());
    }

    #[test]
    fn best_scored_result_single_result() {
        let results = vec![make_result(
            1,
            "Inception",
            Some("2010-07-16"),
            80.0,
            Some(8.4),
            Some(30000),
        )];
        let got = best_scored_result(
            &results,
            &normalize_title("Inception"),
            &Some("2010".to_string()),
            false,
        );
        assert_eq!(got.unwrap().id, 1);
    }

    #[test]
    fn best_scored_result_exact_title_wins() {
        // "Flow" exact match with low popularity should beat a partial/non-match with higher popularity
        let results = vec![
            make_result(
                1,
                "Overflow",
                Some("2020-01-01"),
                200.0,
                Some(7.0),
                Some(500),
            ),
            make_result(2, "Flow", Some("2024-01-01"), 10.0, Some(7.5), Some(100)),
        ];
        let nq = normalize_title("Flow");
        let got = best_scored_result(&results, &nq, &None, false);
        assert_eq!(got.unwrap().id, 2);
    }

    #[test]
    fn best_scored_result_year_match_boosts() {
        let results = vec![
            make_result(1, "Dune", Some("2021-10-22"), 50.0, Some(7.8), Some(8000)),
            make_result(2, "Dune", Some("1984-12-14"), 20.0, Some(6.3), Some(2000)),
        ];
        let nq = normalize_title("Dune");
        let got = best_scored_result(&results, &nq, &Some("1984".to_string()), false);
        assert_eq!(got.unwrap().id, 2);
    }

    #[test]
    fn best_scored_result_short_title_requires_exact_and_year() {
        // Short title "IT" — only the result with exact title AND matching year should qualify
        let results = vec![
            make_result(1, "It", Some("2017-09-06"), 100.0, Some(7.3), Some(15000)),
            make_result(2, "It", Some("1990-11-18"), 30.0, Some(6.9), Some(2000)),
            make_result(3, "Italy", Some("2017-05-01"), 5.0, Some(5.0), Some(50)),
        ];
        let nq = normalize_title("IT");
        let got = best_scored_result(&results, &nq, &Some("1990".to_string()), true);
        // Only id=2 has exact title "it" AND year 1990
        assert_eq!(got.unwrap().id, 2);
    }

    #[test]
    fn best_scored_result_short_title_no_year_allows_exact_match() {
        // Short title "ted" with NO year in the filename should still match
        // an exact TMDB result rather than failing identification entirely.
        let results = vec![
            make_result(1, "Ted", Some("2024-01-11"), 80.0, Some(6.5), Some(500)),
            make_result(
                2,
                "Ted Lasso",
                Some("2020-08-14"),
                90.0,
                Some(8.0),
                Some(3000),
            ),
        ];
        let nq = normalize_title("ted");
        // year = None (no year in filename)
        let got = best_scored_result(&results, &nq, &None, true);
        // Should pick id=1 (exact title match "Ted" == "ted"), not None
        assert!(got.is_some(), "short title with no year should still match exact title");
        assert_eq!(got.unwrap().id, 1);
    }

    #[test]
    fn best_scored_result_short_title_no_year_rejects_partial() {
        // Short title "UC" with NO year — no exact title match exists, should still return None
        let results = vec![
            make_result(
                1,
                "Gundam Unicorn",
                Some("2010-02-20"),
                50.0,
                Some(7.0),
                Some(200),
            ),
            make_result(2, "UC Browser", None, 10.0, None, None),
        ];
        let nq = normalize_title("UC");
        let got = best_scored_result(&results, &nq, &None, true);
        assert!(got.is_none(), "short title with no exact match should still return None");
    }

    #[test]
    fn best_scored_result_short_title_no_match_returns_none() {
        // Short title "UC" with no exact+year match should return None
        let results = vec![
            make_result(
                1,
                "Gundam Unicorn",
                Some("2010-02-20"),
                50.0,
                Some(7.0),
                Some(200),
            ),
            make_result(2, "UC Browser", None, 10.0, None, None),
        ];
        let nq = normalize_title("UC");
        let got = best_scored_result(&results, &nq, &Some("2023".to_string()), true);
        assert!(got.is_none());
    }

    #[test]
    fn best_scored_result_higher_votes_breaks_tie() {
        // Two exact title matches, same year — higher vote count should win
        let results = vec![
            make_result(1, "Flow", Some("2024-08-30"), 50.0, Some(8.0), Some(5000)),
            make_result(2, "Flow", Some("2024-03-15"), 40.0, Some(7.5), Some(20)),
        ];
        let nq = normalize_title("Flow");
        let got = best_scored_result(&results, &nq, &Some("2024".to_string()), false);
        assert_eq!(got.unwrap().id, 1);
    }

    // --- Tests for select_best_match ---

    #[test]
    fn select_best_match_both_none() {
        let got = select_best_match(None, None, "anything", &None, false);
        assert!(got.is_none());
    }

    #[test]
    fn select_best_match_only_tv() {
        let tv = make_result(
            100,
            "Breaking Bad",
            Some("2008-01-20"),
            200.0,
            Some(8.9),
            Some(10000),
        );
        let got = select_best_match(
            Some(&tv),
            None,
            &normalize_title("Breaking Bad"),
            &None,
            true,
        );
        let (title, _date, id, _source, media_type) = got.unwrap();
        assert_eq!(title, "Breaking Bad");
        assert_eq!(id, "100");
        assert_eq!(media_type, MediaType::Show);
    }

    #[test]
    fn select_best_match_only_movie() {
        let movie = make_result(
            200,
            "Inception",
            Some("2010-07-16"),
            100.0,
            Some(8.4),
            Some(30000),
        );
        let got = select_best_match(
            None,
            Some(&movie),
            &normalize_title("Inception"),
            &None,
            false,
        );
        let (title, _date, id, _source, media_type) = got.unwrap();
        assert_eq!(title, "Inception");
        assert_eq!(id, "200");
        assert_eq!(media_type, MediaType::Movie);
    }

    #[test]
    fn select_best_match_tv_exact_year_wins() {
        // TV has exact title + year match; movie has exact title but wrong year
        let tv = make_result(10, "Dune", Some("2000-12-03"), 30.0, Some(7.0), Some(500));
        let movie = make_result(20, "Dune", Some("2021-10-22"), 80.0, Some(7.8), Some(8000));
        let nq = normalize_title("Dune");
        let year = Some("2000".to_string());
        let got = select_best_match(Some(&tv), Some(&movie), &nq, &year, false);
        let (_title, _date, id, _source, media_type) = got.unwrap();
        assert_eq!(id, "10");
        assert_eq!(media_type, MediaType::Show);
    }

    #[test]
    fn select_best_match_movie_exact_year_wins() {
        // Movie has exact title + year match; TV has exact title but wrong year
        let tv = make_result(10, "Don", Some("2006-10-20"), 30.0, Some(6.5), Some(200));
        let movie = make_result(20, "Don", Some("2022-05-13"), 50.0, Some(7.2), Some(1000));
        let nq = normalize_title("Don");
        let year = Some("2022".to_string());
        let got = select_best_match(Some(&tv), Some(&movie), &nq, &year, true);
        let (_title, _date, id, _source, media_type) = got.unwrap();
        assert_eq!(id, "20");
        assert_eq!(media_type, MediaType::Movie);
    }

    #[test]
    fn select_best_match_show_guess_prefers_tv() {
        // Both have exact title, no year info — is_show_guess=true should prefer TV
        let tv = make_result(
            10,
            "Sherwood",
            Some("2022-06-13"),
            40.0,
            Some(7.0),
            Some(300),
        );
        let movie = make_result(
            20,
            "Sherwood",
            Some("2019-11-22"),
            20.0,
            Some(6.5),
            Some(100),
        );
        let nq = normalize_title("Sherwood");
        let got = select_best_match(Some(&tv), Some(&movie), &nq, &None, true);
        let (_title, _date, id, _source, media_type) = got.unwrap();
        assert_eq!(id, "10");
        assert_eq!(media_type, MediaType::Show);
    }

    #[test]
    fn select_best_match_no_show_guess_prefers_movie() {
        // Both have exact title, no year info — is_show_guess=false should prefer movie
        let tv = make_result(
            10,
            "Sherwood",
            Some("2022-06-13"),
            40.0,
            Some(7.0),
            Some(300),
        );
        let movie = make_result(
            20,
            "Sherwood",
            Some("2019-11-22"),
            20.0,
            Some(6.5),
            Some(100),
        );
        let nq = normalize_title("Sherwood");
        let got = select_best_match(Some(&tv), Some(&movie), &nq, &None, false);
        let (_title, _date, id, _source, media_type) = got.unwrap();
        assert_eq!(id, "20");
        assert_eq!(media_type, MediaType::Movie);
    }

    #[test]
    fn is_show_guess_single_selected_video_is_movie() {
        // A movie torrent with one selected video and unselected extras
        // should NOT be classified as a show
        let files = vec![
            TorrentFile {
                id: 1,
                path: "/Movie.2023.mkv".to_string(),
                bytes: 10_000_000_000,
                selected: 1,
            },
            TorrentFile {
                id: 2,
                path: "/Making.Of.mkv".to_string(),
                bytes: 500_000_000,
                selected: 0,
            },
            TorrentFile {
                id: 3,
                path: "/Commentary.mkv".to_string(),
                bytes: 8_000_000_000,
                selected: 0,
            },
        ];
        assert!(
            !is_show_guess(&files),
            "Movie with unselected extras should not be guessed as show"
        );
    }

    #[test]
    fn is_show_guess_multiple_selected_videos_is_show() {
        // A show torrent with multiple selected video files
        let files = vec![
            TorrentFile {
                id: 1,
                path: "/Episode1.mkv".to_string(),
                bytes: 1_000_000_000,
                selected: 1,
            },
            TorrentFile {
                id: 2,
                path: "/Episode2.mkv".to_string(),
                bytes: 1_000_000_000,
                selected: 1,
            },
        ];
        assert!(
            is_show_guess(&files),
            "Multiple selected video files should be guessed as show"
        );
    }

    #[test]
    fn is_show_guess_episode_pattern_detected() {
        let files = vec![TorrentFile {
            id: 1,
            path: "/Show.S01E01.mkv".to_string(),
            bytes: 1_000_000_000,
            selected: 1,
        }];
        assert!(
            is_show_guess(&files),
            "Episode pattern in filename should be guessed as show"
        );
    }
}
