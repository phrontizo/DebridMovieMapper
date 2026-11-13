use crate::error::AppError;
use crate::provider::ProviderKind;
use crate::release::RawCandidate;
use async_trait::async_trait;
use regex::Regex;
use std::sync::LazyLock;
use tracing::{debug, warn};

/// Movie vs series — the two Stremio stream endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MediaKind {
    Movie,
    Series,
}

/// Build the HTTP client used by the scraper. When `proxy_url` is `Some`, ALL of the scraper's
/// requests (HTTP and HTTPS — HTTPS via the proxy's CONNECT tunnel) are routed through that
/// HTTP(S) proxy, so only the scraper's Torrentio/addon traffic is proxied — never the debrid CDN
/// media reads, TMDB, or the provider APIs, which keep their own direct clients. A set-but-invalid
/// proxy URL is a hard error: we never silently fall back to a direct connection (that would leak
/// the real IP, defeating the proxy). The error message deliberately omits the URL value, which may
/// carry `user:pass@` credentials.
///
/// Only http(s) proxy schemes are supported (no `socks` reqwest feature is enabled). The scheme is
/// validated here rather than relying on `reqwest::Proxy::all`, which is lenient enough to accept a
/// malformed value at construction and only fail at request time — we want a misconfigured proxy to
/// fail FAST at startup with a clear message, not silently fail every scrape later.
pub fn build_http_client(proxy_url: Option<&str>) -> Result<reqwest::Client, AppError> {
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));
    if let Some(url) = proxy_url {
        // Case-insensitive scheme check (URL schemes are case-insensitive per RFC 3986, and the url
        // crate lowercases the scheme, so `HTTP://…` is valid). Only the scheme is inspected — never
        // lowercase the whole URL, which would corrupt a `user:pass@` credential.
        let scheme_ok = url.split_once("://").is_some_and(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        });
        if !scheme_ok {
            // Deliberately omit the value (it may carry `user:pass@` credentials).
            return Err(AppError::Config(
                "SCRAPER_PROXY_URL must be an http(s) proxy URL like \
                 http://[user:pass@]host:port (socks is not supported)"
                    .to_string(),
            ));
        }
        let proxy = reqwest::Proxy::all(url)
            .map_err(|_| AppError::Config("Invalid SCRAPER_PROXY_URL".to_string()))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| AppError::Config(format!("Failed to build scraper HTTP client: {}", e)))
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

/// Default spacing between scrapes (`SCRAPER_MIN_INTERVAL_MS`). Torrentio sends no rate-limit
/// headers, and in production it answered 429 once ~80–100 requests arrived within a minute (the hourly
/// episode check scrapes 100–200 episodes back to back); one request a second stays under that.
pub const DEFAULT_SCRAPER_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(1_000);

/// Requests per scrape when the addon keeps answering 429: the first try plus two retries, each
/// after the limiter's back-off (and any `Retry-After`).
pub const MAX_SCRAPE_ATTEMPTS: u32 = 3;

pub struct TorrentioScraper {
    base_url: String,
    http: reqwest::Client,
    /// Paces every request (all callers share one scraper) and backs off on a 429.
    limiter: crate::ratelimit::AdaptiveRateLimiter,
}

impl TorrentioScraper {
    pub fn new(
        override_url: Option<String>,
        provider: ProviderKind,
        token: &str,
        http: reqwest::Client,
    ) -> Self {
        Self {
            base_url: Self::build_base_url(override_url, provider, token),
            http,
            limiter: crate::ratelimit::AdaptiveRateLimiter::with_min_interval(
                DEFAULT_SCRAPER_MIN_INTERVAL,
            ),
        }
    }

    /// Space requests by `min_interval` instead of [`DEFAULT_SCRAPER_MIN_INTERVAL`]
    /// (`SCRAPER_MIN_INTERVAL_MS`).
    pub fn with_min_interval(mut self, min_interval: std::time::Duration) -> Self {
        self.limiter = crate::ratelimit::AdaptiveRateLimiter::with_min_interval(min_interval);
        self
    }

    /// Override (trimmed) wins; else Torrentio templated as `<provider>=<token>` (further options
    /// are `|`-separated). The live scraper test guards the syntax.
    pub fn build_base_url(
        override_url: Option<String>,
        provider: ProviderKind,
        token: &str,
    ) -> String {
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

    pub fn stream_url(
        base: &str,
        imdb_id: &str,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> String {
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

// The infohash + fileIdx in a Torrentio debrid-resolve path: `/<infohash>/<season:episode|null>/
// <fileidx>/`. The fileIdx must be a COMPLETE numeric segment — `(\d+)` followed by `/` or
// end-of-string — which is what distinguishes the infohash from the debrid token that precedes it:
// the token is followed by `/<infohash>/<season:episode|null>`, and a `season:episode` like `1:2`
// is NOT a complete numeric segment (the trailing `/` anchor rejects the `1` prefix), while `null`
// is not numeric at all. So even a token that is exactly 40 lowercase-hex chars never matches here.
// Lowercase-only also guards against a mixed-case token. Hash and idx come from the SAME match.
static RESOLVE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/([0-9a-f]{40})/[^/]*/(\d+)(?:/|$)").unwrap());

/// Recover `(infohash, fileIdx)` from a Torrentio debrid-resolve URL of the form
/// `.../resolve/<provider>/<token>/<infohash>/<season:episode|null>/<fileidx>/<filename>`.
/// A debrid-keyed Torrentio returns url-only streams (no `infoHash`/`fileIdx` fields), so this
/// recovers the hash the acquisition engine needs to add the torrent by magnet.
/// Note: only 40-hex (BitTorrent v1) infohashes are recovered; a 32-char base32 hash (rare, and
/// not emitted by Torrentio) would not match and the stream would be skipped.
fn hash_idx_from_url(url: &str) -> Option<(String, Option<usize>)> {
    let c = RESOLVE_RE.captures(url)?;
    // Lowercase the recovered hash to match every other write path (the explicit-`infoHash` branch
    // and `record_mirror_owned` both store lowercase) so an owned/blacklist lookup keyed on this
    // hash can never miss on case. (The regex already only matches lowercase hex, so this is a
    // belt-and-braces consistency guard.)
    let hash = c.get(1)?.as_str().to_ascii_lowercase();
    let idx = c.get(2).and_then(|m| m.as_str().parse::<usize>().ok());
    Some((hash, idx))
}

/// Whether a 4xx status from the addon indicates a PERMANENT config problem (misconfigured URL /
/// revoked provider token) worth the distinct operator warning. Scoped to the auth/config statuses
/// (400/401/403) ONLY — a transient/unrelated 4xx (408 Request Timeout, 425 Too Early, 451, …) must
/// NOT trigger the alarming "misconfigured/revoked token" message. `404` is handled earlier as "no
/// streams" and `429` is the transient rate-limit; neither reaches (or matches) this predicate.
fn is_config_error_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::BAD_REQUEST
            | reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
    )
}

/// Whether `h` is a structurally-valid (already-lowercased) BitTorrent v1 infohash: 40 hex chars.
/// Guards the explicit-`infoHash` field so a malformed value (e.g. a truncated `aabbcc`) can't
/// build an invalid `magnet:?xt=urn:btih:…` and burn a provider add — the same strictness as the
/// URL-recovery path (`RESOLVE_RE` requires 40-hex). A 32-char base32 hash is rejected too: owned
/// records, the blacklist, the mirror and the provider listings all key torrents by the 40-hex form
/// a provider reports, so a base32-keyed candidate would never match its own listing entry.
fn is_valid_infohash(h: &str) -> bool {
    h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())
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
        // Take the explicit `infoHash` only when it is structurally valid (40-hex).
        // A present-but-EMPTY or malformed infoHash is degenerate (would build an invalid
        // `magnet:?xt=urn:btih:…`), so treat it like a missing one — fall through to the debrid-url
        // hash recovery (else skip). `is_valid_infohash` mirrors the URL-recovery path's strictness.
        let explicit_hash = s
            .get("infoHash")
            .and_then(|h| h.as_str())
            .map(|h| h.to_ascii_lowercase())
            .filter(|h| is_valid_infohash(h));
        let (info_hash, url_idx) = match explicit_hash {
            Some(h) => (h, None),
            None => match s
                .get("url")
                .and_then(|u| u.as_str())
                .and_then(hash_idx_from_url)
            {
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
            name: s
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string(),
            description: s
                .get("title")
                .or_else(|| s.get("description"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            info_hash,
            file_idx,
            file_name: s
                .get("behaviorHints")
                .and_then(|b| b.get("filename"))
                .and_then(|f| f.as_str())
                .map(String::from),
        });
    }
    out
}

#[async_trait]
impl Scraper for TorrentioScraper {
    async fn find(
        &self,
        imdb_id: &str,
        kind: MediaKind,
        season: Option<u32>,
        episode: Option<u32>,
    ) -> Result<Vec<RawCandidate>, AppError> {
        // Never interpolate a malformed id into the token-bearing URL path (a `/`, `?` or `#` would
        // change it): Torrentio is IMDB-keyed, so such an id could only fail anyway. Same guard as
        // `tmdb_client::find_by_imdb`; checked before the limiter so it costs no paced request.
        if !crate::tmdb_client::is_well_formed_imdb_id(imdb_id) {
            debug!(
                "scrape: skipping malformed IMDB id {:?} (expected `tt<digits>`)",
                imdb_id
            );
            return Ok(Vec::new());
        }
        let url = Self::stream_url(&self.base_url, imdb_id, kind, season, episode);
        // Paced through the shared limiter; a 429 backs it off (honouring `Retry-After`) and the
        // request is retried, up to MAX_SCRAPE_ATTEMPTS, before surfacing as the retriable error.
        let mut attempt = 0;
        let resp = loop {
            attempt += 1;
            self.limiter.wait_for_token().await;
            // The base URL embeds the provider token in its path, so any reqwest error here would
            // carry it (reqwest only redacts userinfo, not the path). Strip the URL from every error
            // before it can reach a log line — mirrors tmdb_client/torbox_client.
            let resp = self
                .http
                .get(&url)
                .send()
                .await
                .map_err(|e| AppError::Http(e.without_url()))?;
            if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
                self.limiter.record_success().await;
                break resp;
            }
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            self.limiter.record_throttle(retry_after).await;
            if attempt >= MAX_SCRAPE_ATTEMPTS {
                break resp; // still throttled: surface the 429 below
            }
            debug!(
                "scrape: {} {:?} s={:?} e={:?} → 429, backing off (retry-after {:?}) and retrying",
                imdb_id, kind, season, episode, retry_after
            );
        };
        // 404 = unknown id (genuinely no streams). Any other non-success (429/5xx) is a
        // retriable addon error — surface it so the engine treats it as TemporarilyUnavailable
        // rather than silently seeing zero candidates.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            debug!(
                "scrape: {} {:?} s={:?} e={:?} → 404 (no streams)",
                imdb_id, kind, season, episode
            );
            return Ok(Vec::new());
        }
        // A non-404 4xx (400/401/403) is a PERMANENT addon error — a misconfigured SCRAPER_ADDON_URL
        // or a revoked provider token — not the transient 429/5xx the retry-next-tick is meant for.
        // It still surfaces as an error (so the engine doesn't silently see zero candidates), but warn
        // distinctly so the operator can tell a config problem apart from a transient upstream blip.
        if is_config_error_status(resp.status()) {
            warn!(
                "scrape: addon returned {} for {} — misconfigured SCRAPER_ADDON_URL or revoked provider token?",
                resp.status(),
                imdb_id
            );
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| AppError::Http(e.without_url()))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AppError::Http(e.without_url()))?;
        let cands = parse_streams(&v);
        // NB: never log the URL/base_url — it embeds the provider token in its path.
        debug!(
            "scrape: {} {:?} s={:?} e={:?} → {} candidates",
            imdb_id,
            kind,
            season,
            episode,
            cands.len()
        );
        Ok(cands)
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
    async fn find(
        &self,
        _imdb_id: &str,
        _kind: MediaKind,
        _season: Option<u32>,
        _episode: Option<u32>,
    ) -> Result<Vec<RawCandidate>, AppError> {
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
    fn build_http_client_without_proxy_succeeds() {
        assert!(build_http_client(None).is_ok());
    }

    #[test]
    fn build_http_client_with_valid_http_and_https_proxy_succeeds() {
        // An HTTP proxy URL (HTTPS targets tunnel through it via CONNECT), with and without creds.
        assert!(build_http_client(Some("http://127.0.0.1:8888")).is_ok());
        assert!(build_http_client(Some("https://proxy.example:3128")).is_ok());
        assert!(build_http_client(Some("http://user:pass@proxy.example:8080")).is_ok());
    }

    #[test]
    fn build_http_client_rejects_unsupported_proxy_scheme_without_leaking_value() {
        // An unsupported proxy scheme (e.g. ftp, or socks without the `socks` feature) must error —
        // we never silently bypass a configured proxy — and the error message must NOT echo the
        // value (it could contain credentials).
        let err = build_http_client(Some("ftp://user:secret@proxy.example:21"))
            .expect_err("unsupported proxy scheme must error");
        let msg = err.to_string();
        assert!(msg.contains("SCRAPER_PROXY_URL"), "msg: {msg}");
        assert!(
            !msg.contains("secret"),
            "must not echo the proxy value: {msg}"
        );
        assert!(
            !msg.contains("proxy.example"),
            "must not echo the proxy value: {msg}"
        );
    }

    #[test]
    fn templates_torbox_url() {
        let base = TorrentioScraper::build_base_url(None, ProviderKind::TorBox, "TB_KEY");
        assert_eq!(base, "https://torrentio.strem.fun/torbox=TB_KEY");
    }

    #[test]
    fn override_url_is_used_verbatim_trimming_trailing_slash() {
        let base = TorrentioScraper::build_base_url(
            Some("https://my.host/comet/abc/".into()),
            ProviderKind::RealDebrid,
            "TOKEN",
        );
        assert_eq!(base, "https://my.host/comet/abc");
    }

    #[test]
    fn stream_url_for_movie_and_series() {
        assert_eq!(
            TorrentioScraper::stream_url(
                "https://torrentio.strem.fun/realdebrid=T",
                "tt0816692",
                MediaKind::Movie,
                None,
                None
            ),
            "https://torrentio.strem.fun/realdebrid=T/stream/movie/tt0816692.json"
        );
        assert_eq!(
            TorrentioScraper::stream_url(
                "https://torrentio.strem.fun/realdebrid=T",
                "tt0903747",
                MediaKind::Series,
                Some(1),
                Some(2)
            ),
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
                    "infoHash": "aabbccddeeff00112233445566778899aabbccdd",
                    "fileIdx": 0,
                    "behaviorHints": {"filename": "Movie.2023.1080p.x265-GRP.mkv"}
                },
                { "name": "Torrentio\n720p", "title": "Movie.720p.x264", "infoHash": "1122334455667788990011223344556677889900" }
            ]
        });
        let cands = parse_streams(&json);
        assert_eq!(cands.len(), 2);
        assert_eq!(
            cands[0].info_hash,
            "aabbccddeeff00112233445566778899aabbccdd"
        );
        assert_eq!(cands[0].file_idx, Some(0));
        assert_eq!(
            cands[0].file_name.as_deref(),
            Some("Movie.2023.1080p.x265-GRP.mkv")
        );
        assert_eq!(
            cands[1].info_hash,
            "1122334455667788990011223344556677889900"
        );
        assert_eq!(cands[1].file_idx, None);
    }

    #[test]
    fn parse_streams_falls_back_to_description_when_title_absent() {
        // Some Stremio addons put the release metadata blob in `description` rather than `title`.
        // `parse_streams` must fall back to it so ranking/identification still see the release
        // details, and a stream carrying BOTH must prefer `title`.
        let json = serde_json::json!({
            "streams": [
                { "name": "Addon",
                  "description": "Movie.2023.1080p.x265-GRP",
                  "infoHash": "aabbccddeeff00112233445566778899aabbccdd" },
                { "name": "Addon",
                  "title": "From.Title",
                  "description": "From.Description",
                  "infoHash": "1122334455667788990011223344556677889900" },
            ]
        });
        let cands = parse_streams(&json);
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].description, "Movie.2023.1080p.x265-GRP");
        assert_eq!(cands[1].description, "From.Title");
    }

    #[test]
    fn parse_streams_skips_a_base32_explicit_infohash() {
        // Owned records, the blacklist, the mirror and every provider listing key torrents by the
        // 40-hex form, so a 32-char base32 hash would never match its own listing entry (observe
        // would reap it as dead while the mirror re-adopted it under its hex hash). Like a malformed
        // hash, it is treated as missing: with no recoverable url the stream is skipped.
        let json = serde_json::json!({
            "streams": [
                { "name": "x", "title": "y", "infoHash": "mfrggzdfmztwq2lknnwg23tpobyxe43u" }
            ]
        });
        assert!(parse_streams(&json).is_empty());
    }

    #[test]
    fn parse_streams_rejects_a_malformed_explicit_infohash() {
        // A non-empty but structurally-invalid explicit infoHash (not 40-hex) must
        // be treated like a missing one — it would otherwise build an invalid
        // `magnet:?xt=urn:btih:aabbcc` and burn a provider add. With no recoverable url, it's skipped.
        let json = serde_json::json!({
            "streams": [
                { "name": "x", "title": "y", "infoHash": "aabbcc" },          // too short → skip
                { "name": "x", "title": "y", "infoHash": "not-hex-not-hex-not-hex-not-hex-not-hex01" }, // 40 chars, non-hex → skip
            ]
        });
        assert!(
            parse_streams(&json).is_empty(),
            "a malformed explicit infoHash with no recoverable url must be skipped"
        );

        // A malformed explicit infoHash with a RESOLVABLE debrid url falls through to url recovery.
        let json = serde_json::json!({
            "streams": [
                { "name": "x", "title": "y", "infoHash": "aabbcc",
                  "url": "https://torrentio.strem.fun/resolve/torbox/TBKEY/64877b5490208c3015c0f5121287949d62622e54/null/0/Movie.mkv" }
            ]
        });
        let cands = parse_streams(&json);
        assert_eq!(cands.len(), 1);
        assert_eq!(
            cands[0].info_hash,
            "64877b5490208c3015c0f5121287949d62622e54"
        );
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
        assert_eq!(
            cands[0].info_hash,
            "64877b5490208c3015c0f5121287949d62622e54"
        );
        assert_eq!(cands[0].file_idx, Some(0));
        assert_eq!(
            cands[0].file_name.as_deref(),
            Some("Movie.2023.1080p.x265-GRP.mkv")
        );
        assert_eq!(
            cands[0].name, "[RD+] Torrentio\n1080p",
            "cache marker preserved for ranking"
        );
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
        assert_eq!(
            cands[0].info_hash,
            "aabbccddeeff00112233445566778899aabbccdd"
        );
        assert_eq!(cands[0].file_idx, Some(3));
    }

    #[test]
    fn recovers_real_hash_when_token_is_also_40_lowercase_hex() {
        // Pathological: the debrid token is itself exactly 40 lowercase-hex chars. The structural
        // anchor (hash followed by `/<segment>/<digits>/`) must still pick the REAL infohash. Cover
        // BOTH a movie (`null`) AND a series (`1:2`) — the series segment is the tricky case: `(\d+)`
        // would otherwise match the `1` prefix of `1:2` and capture the TOKEN as the hash.
        let token = "0000000000000000000000000000000000000000";
        let real = "64877b5490208c3015c0f5121287949d62622e54";
        for (seg, idx) in [("null", 0usize), ("1:2", 3usize)] {
            let url = format!(
                "https://torrentio.strem.fun/resolve/realdebrid/{token}/{real}/{seg}/{idx}/X.mkv"
            );
            let json = serde_json::json!({"streams": [{"name": "x", "title": "y", "url": url}]});
            let cands = parse_streams(&json);
            assert_eq!(cands.len(), 1, "seg={seg}");
            assert_eq!(
                cands[0].info_hash, real,
                "must recover the real hash, not the token (seg={seg})"
            );
            assert_eq!(cands[0].file_idx, Some(idx), "seg={seg}");
        }
    }

    #[test]
    fn skips_streams_with_neither_infohash_nor_resolvable_url() {
        let json = serde_json::json!({"streams": [
            {"name": "x", "title": "y", "url": "https://example.com/no/hash/here.mkv"}
        ]});
        assert!(parse_streams(&json).is_empty());
    }

    #[test]
    fn skips_stream_with_empty_infohash() {
        // A present-but-empty infoHash must not produce a degenerate `magnet:?xt=urn:btih:` candidate
        // — with no recoverable url it's skipped, exactly like a missing infoHash.
        let json = serde_json::json!({"streams": [
            {"name": "x", "title": "y", "infoHash": ""}
        ]});
        assert!(parse_streams(&json).is_empty());
        // And an empty infoHash with a resolvable debrid url still recovers the hash from the url.
        let json2 = serde_json::json!({"streams": [
            {"name": "x", "title": "y", "infoHash": "",
             "url": "https://x/resolve/realdebrid/TOKEN/abcdef0123456789abcdef0123456789abcdef01/null/0/f.mkv"}
        ]});
        let cands = parse_streams(&json2);
        assert_eq!(cands.len(), 1);
        assert_eq!(
            cands[0].info_hash,
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn build_http_client_accepts_uppercase_proxy_scheme() {
        // URL schemes are case-insensitive; a valid `HTTP://`/`HTTPS://` proxy must not be rejected.
        assert!(build_http_client(Some("HTTP://proxy.example:8080")).is_ok());
        assert!(build_http_client(Some("HTTPS://proxy.example:8443")).is_ok());
    }

    #[tokio::test]
    async fn mock_scraper_returns_canned() {
        let mock = MockScraper {
            candidates: vec![RawCandidate {
                name: "n".into(),
                description: "d".into(),
                info_hash: "h".into(),
                file_idx: None,
                file_name: None,
            }],
        };
        let scraper: std::sync::Arc<dyn Scraper> = std::sync::Arc::new(mock);
        let got = scraper
            .find("tt1", MediaKind::Movie, None, None)
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].info_hash, "h");
    }

    /// A loopback HTTP server that answers every request with a fixed status + body.
    async fn spawn_status_server(status: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{}", addr)
    }

    /// A loopback server that answers request N with `replies[N]` (the last one repeating) and counts
    /// the requests. Each reply is `(status line, extra header lines, body)`; connections are closed
    /// after every reply so each request arrives on a fresh one.
    async fn spawn_sequence_server(
        replies: Vec<(&'static str, &'static str, &'static str)>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let (status, headers, body) = replies[n.min(replies.len() - 1)];
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{}", addr), hits)
    }

    const ONE_STREAM: &str = r#"{"streams":[{"name":"x","title":"y","infoHash":"abcdef0123456789abcdef0123456789abcdef01"}]}"#;

    fn scraper_at(url: String) -> TorrentioScraper {
        TorrentioScraper::new(
            Some(url),
            ProviderKind::TorBox,
            "tok",
            reqwest::Client::new(),
        )
    }

    /// A malformed IMDB id is never put into the (token-bearing) addon URL path: `find` returns no
    /// candidates without sending a request, as `tmdb_client::find_by_imdb` does.
    #[tokio::test]
    async fn find_skips_a_malformed_imdb_id_without_a_request() {
        use std::sync::atomic::Ordering;
        let (url, hits) = spawn_sequence_server(vec![("200 OK", "", ONE_STREAM)]).await;
        let scraper = scraper_at(url);
        for bad in ["../evil", "tt12/../x", "tt1?x=1", "tt", "12345"] {
            let got = scraper.find(bad, MediaKind::Movie, None, None).await;
            assert!(
                matches!(&got, Ok(v) if v.is_empty()),
                "{bad:?} must yield no candidates, got {got:?}"
            );
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0, "no request was sent");
        let got = scraper
            .find("tt0111161", MediaKind::Movie, None, None)
            .await
            .unwrap();
        assert_eq!(got.len(), 1, "control: a well-formed id is scraped");
    }

    /// Torrentio answers a burst with 429. A throttled scrape backs off — honouring `Retry-After` —
    /// and is retried, so the acquisition that asked for it isn't failed until the next cycle.
    #[tokio::test(start_paused = true)]
    async fn a_throttled_scrape_backs_off_and_is_retried() {
        let (url, hits) = spawn_sequence_server(vec![
            ("429 Too Many Requests", "Retry-After: 7\r\n", ""),
            ("200 OK", "", ONE_STREAM),
        ])
        .await;
        let t0 = tokio::time::Instant::now();
        let cands = scraper_at(url)
            .find("tt1", MediaKind::Movie, None, None)
            .await
            .expect("the retry succeeds");
        assert_eq!(cands.len(), 1);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            t0.elapsed() >= std::time::Duration::from_secs(7),
            "Retry-After is honoured, waited {:?}",
            t0.elapsed()
        );
    }

    /// A scrape that stays throttled gives up after `MAX_SCRAPE_ATTEMPTS` requests and returns the
    /// retriable error, as before.
    #[tokio::test(start_paused = true)]
    async fn a_scrape_that_stays_throttled_gives_up_after_a_bounded_number_of_attempts() {
        let (url, hits) = spawn_sequence_server(vec![("429 Too Many Requests", "", "")]).await;
        assert!(scraper_at(url)
            .find("tt1", MediaKind::Movie, None, None)
            .await
            .is_err());
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            MAX_SCRAPE_ATTEMPTS as usize
        );
    }

    /// Scrapes are paced: consecutive requests are at least the configured interval apart, so a
    /// reconcile's burst stays under the addon's limit instead of tripping it.
    #[tokio::test(start_paused = true)]
    async fn consecutive_scrapes_are_spaced_by_the_min_interval() {
        let (url, hits) = spawn_sequence_server(vec![("200 OK", "", ONE_STREAM)]).await;
        let scraper = scraper_at(url).with_min_interval(std::time::Duration::from_millis(500));
        let t0 = tokio::time::Instant::now();
        for _ in 0..3 {
            scraper
                .find("tt1", MediaKind::Movie, None, None)
                .await
                .unwrap();
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(1_000),
            "three scrapes span two intervals, took {:?}",
            t0.elapsed()
        );
        assert_eq!(
            DEFAULT_SCRAPER_MIN_INTERVAL,
            std::time::Duration::from_millis(1_000)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn find_maps_404_to_empty_5xx_to_err_and_parses_200() {
        // The 404-vs-other status classification in `find` is load-bearing (404 → "no streams",
        // anything else retriable → Err so the engine retries rather than seeing silent zero
        // candidates) and was only covered by the live test. Pin it offline against a loopback server.
        let s = |url: String| {
            TorrentioScraper::new(
                Some(url),
                ProviderKind::RealDebrid,
                "tok",
                reqwest::Client::new(),
            )
        };
        // 404 = unknown id → Ok(empty).
        let s404 = s(spawn_status_server("404 Not Found", "").await);
        assert!(s404
            .find("tt1", MediaKind::Movie, None, None)
            .await
            .unwrap()
            .is_empty());
        // 503 = retriable addon error → Err (NOT a silent empty).
        let s503 = s(spawn_status_server("503 Service Unavailable", "").await);
        assert!(s503
            .find("tt1", MediaKind::Movie, None, None)
            .await
            .is_err());
        // 403 = permanent addon error → also Err (warns separately; engine still doesn't see empty).
        let s403 = s(spawn_status_server("403 Forbidden", "").await);
        assert!(s403
            .find("tt1", MediaKind::Movie, None, None)
            .await
            .is_err());
        // 429 = transient rate-limit → Err (retriable), and NOT a config error (must not warn
        // "misconfigured/revoked token").
        let s429 = s(spawn_status_server("429 Too Many Requests", "").await);
        assert!(s429
            .find("tt1", MediaKind::Movie, None, None)
            .await
            .is_err());
        // 200 with valid stream JSON → parses the candidate.
        let s200 = s(spawn_status_server(
            "200 OK",
            r#"{"streams":[{"name":"x","title":"y","infoHash":"abcdef0123456789abcdef0123456789abcdef01"}]}"#,
        )
        .await);
        let cands = s200
            .find("tt1", MediaKind::Movie, None, None)
            .await
            .unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(
            cands[0].info_hash,
            "abcdef0123456789abcdef0123456789abcdef01"
        );
    }

    #[test]
    fn is_config_error_status_excludes_429_and_5xx() {
        use reqwest::StatusCode;
        // Permanent config errors → warn-worthy.
        assert!(is_config_error_status(StatusCode::BAD_REQUEST));
        assert!(is_config_error_status(StatusCode::UNAUTHORIZED));
        assert!(is_config_error_status(StatusCode::FORBIDDEN));
        // Transient / non-config → NOT warn-worthy (a 4xx that isn't an auth/config error must not
        // masquerade as "misconfigured/revoked token").
        assert!(!is_config_error_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_config_error_status(StatusCode::REQUEST_TIMEOUT)); // 408
        assert!(!is_config_error_status(StatusCode::TOO_EARLY)); // 425
        assert!(!is_config_error_status(
            StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS // 451
        ));
        assert!(!is_config_error_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_config_error_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_config_error_status(StatusCode::OK));
    }
}
