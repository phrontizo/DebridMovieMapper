use crate::error::AppError;
use crate::provider::ProviderKind;
use crate::release::RawCandidate;
use async_trait::async_trait;
use regex::Regex;
use std::sync::LazyLock;

/// Movie vs series — the two Stremio stream endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MediaKind {
    Movie,
    Series,
}

/// Abstraction over a Stremio-addon scraper. `TorrentioScraper` is the default impl.
#[async_trait]
pub trait Scraper: Send + Sync {
    async fn find(
        &self,
        imdb_id: &str,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Result<Vec<RawCandidate>, AppError>;
}

pub struct TorrentioScraper {
    base_url: String,
    http: reqwest::Client,
}

impl TorrentioScraper {
    pub fn new(override_url: Option<String>, provider: ProviderKind, token: &str, http: reqwest::Client) -> Self {
        Self { base_url: Self::build_base_url(override_url, provider, token), http }
    }

    /// Override (trimmed) wins; else template Torrentio from provider + token.
    /// NOTE: verify the `<provider>=<token>` option syntax against torrentio.strem.fun/configure;
    /// the live scraper test guards drift.
    pub fn build_base_url(override_url: Option<String>, provider: ProviderKind, token: &str) -> String {
        if let Some(u) = override_url {
            return u.trim().trim_end_matches('/').to_string();
        }
        let opt = match provider {
            ProviderKind::RealDebrid => "realdebrid",
            ProviderKind::TorBox => "torbox",
        };
        // Single provider option. Additional Torrentio options are '|'-separated
        // (e.g. "realdebrid=TOKEN|sort=size") — extend here if needed.
        format!("https://torrentio.strem.fun/{}={}", opt, token)
    }

    pub fn stream_url(base: &str, imdb_id: &str, kind: MediaKind, season: Option<u32>, episode: Option<u32>) -> String {
        match kind {
            MediaKind::Movie => format!("{}/stream/movie/{}.json", base, imdb_id),
            MediaKind::Series => {
                let s = season.unwrap_or(1);
                let e = episode.unwrap_or(1);
                format!("{}/stream/series/{}:{}:{}.json", base, imdb_id, s, e)
            }
        }
    }
}

// A 40-hex infohash as a slash-delimited path segment. Case-sensitive (lowercase) so a
// mixed-case debrid token in the same URL can't be mistaken for the hash.
static RESOLVE_HASH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/([0-9a-f]{40})/").unwrap());
// The fileIdx following the hash: `/<hash>/<season:episode|null>/<fileidx>`.
static RESOLVE_IDX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/[0-9a-f]{40}/[^/]*/(\d+)").unwrap());

/// Recover `(infohash, fileIdx)` from a Torrentio debrid-resolve URL of the form
/// `.../resolve/<provider>/<token>/<infohash>/<season:episode|null>/<fileidx>/<filename>`.
/// A debrid-keyed Torrentio returns url-only streams (no `infoHash`/`fileIdx` fields), so this
/// recovers the hash the acquisition engine needs to add the torrent by magnet.
/// Note: only 40-hex (BitTorrent v1) infohashes are recovered; a 32-char base32 hash (rare, and
/// not emitted by debrid-keyed Torrentio) would not match and the stream would be skipped.
fn hash_idx_from_url(url: &str) -> Option<(String, Option<usize>)> {
    let hash = RESOLVE_HASH_RE.captures(url)?.get(1)?.as_str().to_string();
    let idx = RESOLVE_IDX_RE
        .captures(url)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<usize>().ok());
    Some((hash, idx))
}

/// Parse a Stremio stream response into raw candidates. Uses the explicit `infoHash` field when
/// present (public Torrentio); otherwise recovers the hash from a debrid-resolved `url`
/// (debrid-keyed Torrentio). Streams with neither are skipped.
pub fn parse_streams(v: &serde_json::Value) -> Vec<RawCandidate> {
    let mut out = Vec::new();
    let Some(streams) = v.get("streams").and_then(|s| s.as_array()) else {
        return out;
    };
    for s in streams {
        let (info_hash, url_idx) = match s.get("infoHash").and_then(|h| h.as_str()) {
            Some(h) => (h.to_ascii_lowercase(), None),
            None => match s.get("url").and_then(|u| u.as_str()).and_then(hash_idx_from_url) {
                Some((h, idx)) => (h, idx),
                None => continue,
            },
        };
        let file_idx = s
            .get("fileIdx")
            .and_then(|i| i.as_u64())
            .map(|i| i as usize)
            .or(url_idx);
        out.push(RawCandidate {
            name: s.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            description: s.get("title").or_else(|| s.get("description")).and_then(|t| t.as_str()).unwrap_or("").to_string(),
            info_hash,
            file_idx,
            file_name: s.get("behaviorHints").and_then(|b| b.get("filename")).and_then(|f| f.as_str()).map(String::from),
        });
    }
    out
}

#[async_trait]
impl Scraper for TorrentioScraper {
    async fn find(&self, imdb_id: &str, kind: MediaKind, season: Option<u32>, episode: Option<u32>) -> Result<Vec<RawCandidate>, AppError> {
        let url = Self::stream_url(&self.base_url, imdb_id, kind, season, episode);
        let resp = self.http.get(&url).send().await.map_err(AppError::Http)?;
        // 404 = unknown id (genuinely no streams). Any other non-success (429/5xx) is a
        // retriable addon error — surface it so the engine treats it as TemporarilyUnavailable
        // rather than silently seeing zero candidates.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let resp = resp.error_for_status().map_err(AppError::Http)?;
        let v: serde_json::Value = resp.json().await.map_err(AppError::Http)?;
        Ok(parse_streams(&v))
    }
}

/// Test-only scraper returning canned candidates.
#[cfg(test)]
pub struct MockScraper {
    pub candidates: Vec<RawCandidate>,
}

#[cfg(test)]
#[async_trait]
impl Scraper for MockScraper {
    async fn find(&self, _imdb_id: &str, _kind: MediaKind, _season: Option<u32>, _episode: Option<u32>) -> Result<Vec<RawCandidate>, AppError> {
        Ok(self.candidates.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_realdebrid_url_from_provider_and_token() {
        let base = TorrentioScraper::build_base_url(None, ProviderKind::RealDebrid, "TOKEN123");
        assert_eq!(base, "https://torrentio.strem.fun/realdebrid=TOKEN123");
    }

    #[test]
    fn templates_torbox_url() {
        let base = TorrentioScraper::build_base_url(None, ProviderKind::TorBox, "TB_KEY");
        assert_eq!(base, "https://torrentio.strem.fun/torbox=TB_KEY");
    }

    #[test]
    fn override_url_is_used_verbatim_trimming_trailing_slash() {
        let base = TorrentioScraper::build_base_url(Some("https://my.host/comet/abc/".into()), ProviderKind::RealDebrid, "TOKEN");
        assert_eq!(base, "https://my.host/comet/abc");
    }

    #[test]
    fn stream_url_for_movie_and_series() {
        assert_eq!(
            TorrentioScraper::stream_url("https://torrentio.strem.fun/realdebrid=T", "tt0816692", MediaKind::Movie, None, None),
            "https://torrentio.strem.fun/realdebrid=T/stream/movie/tt0816692.json"
        );
        assert_eq!(
            TorrentioScraper::stream_url("https://torrentio.strem.fun/realdebrid=T", "tt0903747", MediaKind::Series, Some(1), Some(2)),
            "https://torrentio.strem.fun/realdebrid=T/stream/series/tt0903747:1:2.json"
        );
    }

    #[test]
    fn parses_streams_json_into_candidates() {
        let json = serde_json::json!({
            "streams": [
                {
                    "name": "Torrentio\n1080p",
                    "title": "Movie.2023.1080p.x265-GRP\n\u{1f4be} 8 GB \u{1f464} 12\nRD+",
                    "infoHash": "aabbcc",
                    "fileIdx": 0,
                    "behaviorHints": {"filename": "Movie.2023.1080p.x265-GRP.mkv"}
                },
                { "name": "Torrentio\n720p", "title": "Movie.720p.x264", "infoHash": "ddeeff" }
            ]
        });
        let cands = parse_streams(&json);
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].info_hash, "aabbcc");
        assert_eq!(cands[0].file_idx, Some(0));
        assert_eq!(cands[0].file_name.as_deref(), Some("Movie.2023.1080p.x265-GRP.mkv"));
        assert_eq!(cands[1].info_hash, "ddeeff");
        assert_eq!(cands[1].file_idx, None);
    }

    #[test]
    fn recovers_hash_and_idx_from_debrid_resolve_url() {
        // Debrid-keyed Torrentio returns url-only streams (no infoHash/fileIdx fields).
        let json = serde_json::json!({
            "streams": [
                {
                    "name": "[RD+] Torrentio\n1080p",
                    "title": "Movie.2023.1080p.x265-GRP\n\u{1f464} 5 \u{1f4be} 8 GB",
                    "url": "https://torrentio.strem.fun/resolve/realdebrid/MixedCaseTOKEN123/64877b5490208c3015c0f5121287949d62622e54/null/0/Movie.2023.1080p.x265-GRP.mkv",
                    "behaviorHints": {"filename": "Movie.2023.1080p.x265-GRP.mkv"}
                }
            ]
        });
        let cands = parse_streams(&json);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].info_hash, "64877b5490208c3015c0f5121287949d62622e54");
        assert_eq!(cands[0].file_idx, Some(0));
        assert_eq!(cands[0].file_name.as_deref(), Some("Movie.2023.1080p.x265-GRP.mkv"));
        assert_eq!(cands[0].name, "[RD+] Torrentio\n1080p", "cache marker preserved for ranking");
    }

    #[test]
    fn recovers_hash_from_series_resolve_url_with_season_episode_segment() {
        let json = serde_json::json!({
            "streams": [{
                "name": "Torrentio",
                "title": "Show.S01E02",
                "url": "https://torrentio.strem.fun/resolve/torbox/TBKEY/aabbccddeeff00112233445566778899aabbccdd/1:2/3/Show.S01E02.mkv",
            }]
        });
        let cands = parse_streams(&json);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].info_hash, "aabbccddeeff00112233445566778899aabbccdd");
        assert_eq!(cands[0].file_idx, Some(3));
    }

    #[test]
    fn skips_streams_with_neither_infohash_nor_resolvable_url() {
        let json = serde_json::json!({"streams": [
            {"name": "x", "title": "y", "url": "https://example.com/no/hash/here.mkv"}
        ]});
        assert!(parse_streams(&json).is_empty());
    }

    #[tokio::test]
    async fn mock_scraper_returns_canned() {
        let mock = MockScraper {
            candidates: vec![RawCandidate { name: "n".into(), description: "d".into(), info_hash: "h".into(), file_idx: None, file_name: None }],
        };
        let scraper: std::sync::Arc<dyn Scraper> = std::sync::Arc::new(mock);
        let got = scraper.find("tt1", MediaKind::Movie, None, None).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].info_hash, "h");
    }
}
