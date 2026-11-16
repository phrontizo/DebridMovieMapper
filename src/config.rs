use crate::error::AppError;
use crate::provider::{choose_provider, ProviderKind};
use tracing::warn;

/// Default WebDAV listen port. Single source for `resolve_port` AND the `from_parts` fall-back warn
/// so the applied value and the logged value can never drift.
pub const DEFAULT_PORT: u16 = 8080;
/// Defaults + floors for the acquisition/scan numeric knobs, passed to [`parse_num`], which uses the
/// default both as the applied value and in its "falling back to N" warning.
const DEFAULT_SCAN_INTERVAL_SECS: u64 = 60;
const MIN_SCAN_INTERVAL_SECS: u64 = 10;
const DEFAULT_STALL_TIMEOUT_SECS: u64 = 1800;
const MIN_STALL_TIMEOUT_SECS: u64 = 60;
const DEFAULT_MAX_ACQUIRE_ATTEMPTS: u32 = 5;
const MIN_MAX_ACQUIRE_ATTEMPTS: u32 = 1;
const DEFAULT_ACQUIRE_DEAD_TIMEOUT_SECS: u64 = 600;
const MIN_ACQUIRE_DEAD_TIMEOUT_SECS: u64 = 120;
const DEFAULT_SCRAPER_MIN_INTERVAL_MS: u64 = 1_000;
const MIN_SCRAPER_MIN_INTERVAL_MS: u64 = 100;
const MAX_SCRAPER_MIN_INTERVAL_MS: u64 = 60_000;

/// Resolve the WebDAV listen port from a raw `PORT` value, identically for the server
/// (`Config::from_parts`) and the `--healthcheck` probe (`main`) so the probe can never target a
/// different port than the one the server actually bound. Trims, treats blank as unset, and rejects
/// `0` (it would bind a random ephemeral port, so the healthcheck, which connects to 127.0.0.1:0,
/// fails: the container reports unhealthy and rclone, gated on `service_healthy`, never mounts).
/// Returns the resolved port plus whether a NON-EMPTY value was invalid/zero and fell back to the
/// default (so the server can warn; the healthcheck ignores the flag).
pub fn resolve_port(raw: Option<&str>) -> (u16, bool) {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => match s.parse::<u16>().ok().filter(|&p| p != 0) {
            Some(p) => (p, false),
            None => (DEFAULT_PORT, true),
        },
        None => (DEFAULT_PORT, false),
    }
}

/// What to report when loading `.env` failed, or `None` when it loaded or there is no `.env` (a
/// container, or a native run configured by exported variables). dotenvy stops at the first line it
/// can't parse, e.g. an unquoted value containing a space, and leaves every later line unset, so a
/// malformed file would otherwise start the service with those settings silently at their defaults
/// (an `UPGRADE_INTERVAL_SECS=0` lost that way turns the upgrade engine back on). The message never
/// repeats the offending text: dotenvy's error carries the line or its value, which can be a token.
fn dotenv_failure(result: &Result<std::path::PathBuf, dotenvy::Error>) -> Option<String> {
    match result {
        Ok(_) => None,
        Err(e) if e.not_found() => None,
        Err(dotenvy::Error::LineParse(..)) => Some(
            "Configuration error: a line of .env could not be parsed, and it and every setting \
             after it would be ignored. Quote a value that contains spaces (e.g. \
             TRAKT_CATCHUP_LOOKBACK=\"6 months\"), and check for an unclosed quote or a stray \
             backslash. The line is not shown, since it may hold a token."
                .to_string(),
        ),
        Err(dotenvy::Error::Io(e)) => {
            Some(format!("Configuration error: could not read .env: {e}"))
        }
        Err(_) => Some("Configuration error: could not load .env".to_string()),
    }
}

/// Load `.env` (the first one found from the working directory up) into the process environment,
/// leaving a variable that is already set alone. `Ok` when it loaded or there is none; `Err` with a
/// token-free message ([`dotenv_failure`]) when it exists but could not be loaded, since dotenvy
/// then applied only the lines before the bad one. The service exits on `Err` (`main`), and so do
/// the live test binaries (`tests/common`'s `load_env`): a test whose provider token sat below the
/// bad line would read it as unset and skip, passing the pre-commit gate having tested nothing.
pub fn load_dotenv() -> Result<(), String> {
    dotenv_failure(&dotenvy::dotenv()).map_or(Ok(()), Err)
}

/// Parse a numeric env knob. Unset or blank → `default`; a value that doesn't parse as `T` warns
/// (`Invalid NAME value '…', falling back to N`) and falls back to `default`; the result, the
/// default included, is then clamped to `[min, max]` (`max: None` = no upper bound), so lowering a
/// default below its floor can't skip the clamp.
fn parse_num<T>(raw: Option<String>, name: &str, default: T, min: T, max: Option<T>) -> T
where
    T: std::str::FromStr + Ord + Copy + std::fmt::Display,
{
    let v = match raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(s) => s.parse::<T>().unwrap_or_else(|_| {
            warn!("Invalid {name} value '{s}', falling back to {default}");
            default
        }),
        None => default,
    };
    let v = v.max(min);
    max.map_or(v, |m| v.min(m))
}

/// Hard resolution ceiling. Compare via `height()` (pixel height), not by variant order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxResolution {
    P720,
    P1080,
    P2160,
}

impl MaxResolution {
    pub fn height(self) -> u16 {
        match self {
            MaxResolution::P720 => 720,
            MaxResolution::P1080 => 1080,
            MaxResolution::P2160 => 2160,
        }
    }
    /// Parse a resolution ceiling: `720`/`1080`/`2160` (each also accepted with a `p` suffix), or
    /// the `2160` synonyms `4k`/`uhd`; case-insensitive. `None` if unrecognised.
    pub fn try_parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "720" | "720p" => Some(MaxResolution::P720),
            "1080" | "1080p" => Some(MaxResolution::P1080),
            "2160" | "2160p" | "4k" | "uhd" => Some(MaxResolution::P2160),
            _ => None,
        }
    }
    /// Parse a resolution ceiling (see `try_parse` for the accepted forms); anything else →
    /// default 1080p. Test-only convenience; production uses `try_parse` directly (it logs a
    /// warning on an unrecognised value).
    #[cfg(test)]
    pub fn parse(s: &str) -> Self {
        Self::try_parse(s).unwrap_or(MaxResolution::P1080)
    }
}

/// Required audio language: a specific ISO code, or the title's original language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioReq {
    Lang(String),
    /// Any one of these languages is acceptable. Never produced by [`AudioReq::parse`] (the env var
    /// names one language) — only by [`QualityPrefs::with_resolved_audio`] resolving `Original`
    /// against a title whose TMDB original + spoken languages span more than one language.
    AnyOf(Vec<String>),
    Original,
}

/// Three-letter words that are not ISO 639-2 codes but that an operator may write to mean "no
/// requirement". `SUBTITLE_LANGUAGE` takes `off` as `none`; otherwise they are rejected, since taken
/// as written they would match no track.
const NOT_LANGUAGE_CODES: [&str; 3] = ["off", "any", "all"];

/// ISO 639-2's special codes `und` (undetermined) and `mul` (multiple languages). The probe knows
/// them, but no parsed track carries either: both track parsers read an `und` track as untagged,
/// and a multi-audio release tags each track with its own language. Taken as written they would
/// fail every release whose tracks are all tagged, and an operator may well write them to mean
/// "any language", so they are rejected like [`NOT_LANGUAGE_CODES`].
const NO_TRACK_CODES: [&str; 2] = ["und", "mul"];

/// ISO 639-1 codes an operator may write to mean "no requirement": `no` (Norwegian) and `na`
/// (Nauruan, which also reads as "N/A"). Each is kept as its language, with a warning naming the
/// 3-letter code that keeps the language without the warning. Entries: (2-letter code, language
/// name, 3-letter code).
const MISREADABLE_CODES: [(&str, &str, &str); 2] =
    [("no", "Norwegian", "nor"), ("na", "Nauruan", "nau")];

/// Read a trimmed, non-keyword `AUDIO_LANGUAGE` / `SUBTITLE_LANGUAGE` value (`name`) as a language
/// code, lowercased, or `None` when it is no language code; the caller then falls back to its
/// default, the keyword `fallback`, which the warnings name along with what it does
/// (`fallback_means`). The probe compares this code with every track's language, so a value that
/// names no language matches no track: every release is rejected and its hash blacklisted for 30
/// days. Hence:
/// - 2 letters: kept only if `probe::is_known_language_code` knows it. Its ISO 639-1 table is
///   complete, so anything else (`xy`) is a typo and is rejected with a warning. `no` is kept as
///   Norwegian and `na` as Nauruan, each with a warning ([`MISREADABLE_CODES`]), since they read
///   like "no requirement" and "N/A".
/// - 3 letters: kept. A code the probe doesn't know is warned about, since that is what a misspelt
///   code (`egn`) looks like, but rare valid ISO 639-2 codes (`haw`) aren't in its tables either.
///   The words in [`NOT_LANGUAGE_CODES`] and the codes in [`NO_TRACK_CODES`] are rejected.
/// - anything else (a full name like `english`, a locale like `pt-BR`): rejected with a warning.
fn parse_lang_code(name: &str, v: &str, fallback: &str, fallback_means: &str) -> Option<String> {
    let code = v.to_ascii_lowercase();
    let letters = matches!(code.len(), 2 | 3) && code.bytes().all(|b| b.is_ascii_alphabetic());
    let known = crate::probe::is_known_language_code(&code);
    if !letters || (code.len() == 2 && !known) || NOT_LANGUAGE_CODES.contains(&code.as_str()) {
        warn!(
            "Invalid {name} '{v}' (expected an ISO 639 language code such as 'en' or 'eng', or \
             '{fallback}'); falling back to '{fallback}' ({fallback_means})"
        );
        return None;
    }
    if NO_TRACK_CODES.contains(&code.as_str()) {
        warn!(
            "Invalid {name} '{v}': no track is tagged with it (an 'und' track reads as untagged, \
             and a multi-audio release tags each track with its own language), so every release \
             would be rejected; falling back to '{fallback}' ({fallback_means})"
        );
        return None;
    }
    if let Some((_, language, three)) = MISREADABLE_CODES.iter().find(|(two, ..)| *two == code) {
        warn!(
            "{name} '{v}' is {language}, so a release with no {language} track is rejected. Use \
             '{fallback}' for {fallback_means}; to keep {language} without this warning, write \
             '{three}'"
        );
    } else if !known {
        warn!(
            "{name} '{v}' is not a language code this build knows; using it as written. If it is \
             misspelt, no track matches it and every release is rejected and blacklisted for 30 \
             days"
        );
    }
    Some(code)
}

impl AudioReq {
    /// `None`/empty/"original" → Original; a language code → that language, lowercased
    /// ([`parse_lang_code`], which warns about an unknown 3-letter code and about `no`/`na`); any
    /// other non-empty value is invalid (warned) and falls back to Original.
    pub fn parse(s: Option<String>) -> Self {
        match s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            None => AudioReq::Original,
            Some(v) if v.eq_ignore_ascii_case("original") => AudioReq::Original,
            Some(v) => parse_lang_code("AUDIO_LANGUAGE", &v, "original", "the title's languages")
                .map_or(AudioReq::Original, AudioReq::Lang),
        }
    }
}

/// Required subtitle language: a specific ISO code, or `None` = skip the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubReq {
    Lang(String),
    None,
}

impl SubReq {
    /// `None`/empty/"none"/"off" → None (skip); a language code → that language, lowercased
    /// ([`parse_lang_code`], which warns about an unknown 3-letter code and about `no`/`na`); any
    /// other non-empty value is invalid (warned) and falls back to None (skip the check). So is a
    /// no-language code (`xx`/`zxx`, `probe::is_no_language`): `AUDIO_LANGUAGE` reads it as "no
    /// audio check", but no subtitle track is tagged with it and the probe's subtitle check has no
    /// such exemption, so taken as written it would reject every release.
    pub fn parse(s: Option<String>) -> Self {
        match s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            None => SubReq::None,
            Some(v) if v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("off") => {
                SubReq::None
            }
            Some(v) => {
                let (fallback, fallback_means) = ("none", "no subtitle check");
                match parse_lang_code("SUBTITLE_LANGUAGE", &v, fallback, fallback_means) {
                    Some(code) if crate::probe::is_no_language(&code) => {
                        warn!(
                            "Invalid SUBTITLE_LANGUAGE '{v}': it means no language, which no \
                             subtitle track is tagged with, so every release would be \
                             rejected; falling back to '{fallback}' ({fallback_means})"
                        );
                        SubReq::None
                    }
                    code => code.map_or(SubReq::None, SubReq::Lang),
                }
            }
        }
    }
}

/// Quality preferences used by scoring (`release.rs`) and verification (`probe.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityPrefs {
    pub max_resolution: MaxResolution,
    pub audio: AudioReq,
    pub subtitle: SubReq,
    pub prefer_hevc: bool,
    pub prefer_hdr: bool,
}

/// The audio languages `AUDIO_LANGUAGE=original` accepts for a title: its TMDB original language
/// followed by its TMDB spoken languages — trimmed, lowercased, empties dropped, de-duplicated (two
/// codes naming the same language, e.g. `de`/`ger`, count once; the first spelling is kept), original
/// first. Empty when neither is known (no audio requirement). Shared by acquisition ranking
/// ([`QualityPrefs::with_resolved_audio`]) and the probe (`probe::verify`) so the two can't disagree
/// about which releases are the right language.
pub fn original_audio_languages(original: Option<&str>, spoken: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for code in original
        .into_iter()
        .chain(spoken.iter().map(String::as_str))
    {
        let code = code.trim().to_ascii_lowercase();
        if code.is_empty() {
            continue;
        }
        let canon = crate::probe::to_iso639_2(&code);
        if !out.iter().any(|c| crate::probe::to_iso639_2(c) == canon) {
            out.push(code);
        }
    }
    out
}

impl QualityPrefs {
    /// Return a copy whose `audio` requirement is resolved against a title's TMDB original + spoken
    /// languages ([`original_audio_languages`]), turning the default `AudioReq::Original` into a
    /// concrete `AudioReq::Lang(..)` (one language) or `AudioReq::AnyOf(..)` (several, original
    /// first). `release::score` only penalises a wrong-language release for a concrete requirement (it
    /// has no per-title context of its own), so ranking with the raw prefs would leave `Original`
    /// un-penalised and let a wrong-language release rank #1, get optimistically added, download, and
    /// only then be rejected at probe time — burning an acquire cycle. Resolving here makes
    /// acquisition ranking down-rank it up front, exactly as `probe::verify` rejects it (which
    /// resolves `Original` the same way). Accepting the spoken languages too means a title whose TMDB
    /// original language isn't its main audio (e.g. a `fi`-original film whose dialogue is mostly
    /// English) doesn't down-rank every correctly-tagged release. No known language, or an explicit
    /// `Lang`, is left unchanged.
    pub fn with_resolved_audio(
        &self,
        original_language: Option<&str>,
        spoken_languages: &[String],
    ) -> QualityPrefs {
        let mut p = self.clone();
        if self.audio == AudioReq::Original {
            let mut langs = original_audio_languages(original_language, spoken_languages);
            p.audio = match langs.len() {
                0 => AudioReq::Original,
                1 => AudioReq::Lang(langs.remove(0)),
                _ => AudioReq::AnyOf(langs),
            };
        }
        p
    }
}

/// Trakt sync configuration (SP2). Held by `Config` as `Option<TraktConfig>`.
///
/// Present only when both `TRAKT_CLIENT_ID` and `TRAKT_CLIENT_SECRET` are set;
/// absent means Trakt sync is disabled: the Trakt cycle and episode monitor aren't spawned, while
/// the scan loop and the upgrade job still run.
#[derive(Clone, PartialEq, Eq)]
pub struct TraktConfig {
    pub client_id: String,
    pub client_secret: String,
    /// How often (seconds) to sync the Trakt watched-set. Default 900, min 60.
    pub sync_interval_secs: u64,
    /// How often (seconds) to check for new episodes of tracked shows. Default 3600, min 300.
    pub episode_check_interval_secs: u64,
    /// Catch-up lookback window (seconds): a show you've watched that Trakt reports as behind gets
    /// every aired episode you haven't watched acquired (including ones that aired before your last
    /// watch), but only if you last watched it within this many seconds. `None` = all-time (the
    /// default) — every show you've ever watched with unwatched aired episodes qualifies.
    /// Parsed from `TRAKT_CATCHUP_LOOKBACK` by [`parse_lookback`] (e.g. `90d`, `12w`, `6mo`/`6m`,
    /// `1y`, `6 months`; `0`/unset = all-time).
    pub catchup_lookback_secs: Option<u64>,
    /// Extra hostnames the `/trakt` enrolment routes answer for, on top of the built-in
    /// local-network names (IP literals, `localhost`, single-label names, `.local`/`.lan`/
    /// `.internal`/`.home.arpa`) — e.g. a reverse proxy's FQDN. Lowercase, no port. Parsed from
    /// `TRAKT_ENROL_ALLOWED_HOSTS` (comma-separated); empty by default.
    pub enrol_allowed_hosts: Vec<String>,
}

// Manual Debug redacts `client_secret` so a `debug!("{config:?}")` (or a panic message) can never
// leak the Trakt app secret into the logs.
impl std::fmt::Debug for TraktConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraktConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("sync_interval_secs", &self.sync_interval_secs)
            .field(
                "episode_check_interval_secs",
                &self.episode_check_interval_secs,
            )
            .field("catchup_lookback_secs", &self.catchup_lookback_secs)
            .field("enrol_allowed_hosts", &self.enrol_allowed_hosts)
            .finish()
    }
}

/// Parse a catch-up lookback like `90d`, `12w`, `6mo`, `1y` (or a bare number = days) into seconds.
/// The unit may follow the number after spaces and is case-insensitive: `d`/`day`/`days`,
/// `w`/`wk`/`week`/`weeks`, `m`/`mo`/`mon`/`month`/`months` (30 days; `m` is a month, never a
/// minute) and `y`/`yr`/`year`/`years` (365 days). Empty / unset / `0` → `None` (all-time, no
/// limit). Any other value warns and is treated as all-time rather than failing startup.
pub fn parse_lookback(raw: &str) -> Option<u64> {
    const DAY: u64 = 86_400;
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(digits_end);
    let unit_secs = match unit.trim_start() {
        "" | "d" | "day" | "days" => Some(DAY), // bare number → days
        "w" | "wk" | "week" | "weeks" => Some(7 * DAY),
        "m" | "mo" | "mon" | "month" | "months" => Some(30 * DAY),
        "y" | "yr" | "year" | "years" => Some(365 * DAY),
        _ => None,
    };
    match (num.parse::<u64>(), unit_secs) {
        (Ok(0), Some(_)) => None,
        (Ok(n), Some(unit_secs)) => Some(n.saturating_mul(unit_secs)),
        _ => {
            warn!(
                "Invalid TRAKT_CATCHUP_LOOKBACK value '{raw}'; treating as all-time. Use a number of \
                 days, weeks, months or years, e.g. 90d, 12w, 6mo or 1y"
            );
            None
        }
    }
}

/// Parse `TRAKT_ENROL_ALLOWED_HOSTS`: comma-separated hostnames, each trimmed, lowercased and stripped
/// of a trailing root dot; blank entries are skipped. An entry that isn't a bare hostname (a URL, a
/// port, a path, a space) could never equal a request's hostname, so it is warned about and dropped.
pub fn parse_allowed_hosts(raw: Option<String>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in raw.split(',') {
        let host = entry.trim().to_ascii_lowercase();
        let host = host.strip_suffix('.').unwrap_or(&host);
        if host.is_empty() {
            continue;
        }
        let valid = host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
        if valid {
            out.push(host.to_string());
        } else {
            warn!(
                "Ignoring TRAKT_ENROL_ALLOWED_HOSTS entry '{}': expected a bare hostname \
                 (no scheme, port or path)",
                entry.trim()
            );
        }
    }
    out
}

/// `Some((present, missing))` env-var names when exactly one of `TRAKT_CLIENT_ID` /
/// `TRAKT_CLIENT_SECRET` is non-blank after trimming: Trakt sync is then disabled, which the
/// operator almost certainly didn't intend. `None` when both or neither are set.
fn trakt_half_configured(
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Option<(&'static str, &'static str)> {
    let set = |v: Option<&str>| v.is_some_and(|s| !s.trim().is_empty());
    match (set(client_id), set(client_secret)) {
        (true, false) => Some(("TRAKT_CLIENT_ID", "TRAKT_CLIENT_SECRET")),
        (false, true) => Some(("TRAKT_CLIENT_SECRET", "TRAKT_CLIENT_ID")),
        _ => None,
    }
}

impl TraktConfig {
    /// Pure construction from raw optional values (env-independent, for tests).
    ///
    /// Returns `None` unless both `client_id` and `client_secret` are present and
    /// non-empty after trimming; when only one of them is, it warns naming the missing variable.
    /// Interval values default and clamp as documented.
    pub fn from_parts(
        client_id: Option<String>,
        client_secret: Option<String>,
        sync_interval_secs: Option<String>,
        episode_check_interval_secs: Option<String>,
        catchup_lookback: Option<String>,
        enrol_allowed_hosts: Option<String>,
    ) -> Option<TraktConfig> {
        if let Some((present, missing)) =
            trakt_half_configured(client_id.as_deref(), client_secret.as_deref())
        {
            warn!("{present} is set but {missing} is not; Trakt sync is disabled");
        }
        let client_id = client_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        let client_secret = client_secret
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;

        const DEFAULT_SYNC: u64 = 900;
        const MIN_SYNC: u64 = 60;
        const DEFAULT_EPISODE: u64 = 3600;
        const MIN_EPISODE: u64 = 300;

        let sync_interval_secs = parse_num(
            sync_interval_secs,
            "TRAKT_SYNC_INTERVAL_SECS",
            DEFAULT_SYNC,
            MIN_SYNC,
            None,
        );
        let episode_check_interval_secs = parse_num(
            episode_check_interval_secs,
            "TRAKT_EPISODE_CHECK_INTERVAL_SECS",
            DEFAULT_EPISODE,
            MIN_EPISODE,
            None,
        );

        let catchup_lookback_secs = catchup_lookback.as_deref().and_then(parse_lookback);

        Some(TraktConfig {
            client_id,
            client_secret,
            sync_interval_secs,
            episode_check_interval_secs,
            catchup_lookback_secs,
            enrol_allowed_hosts: parse_allowed_hosts(enrol_allowed_hosts),
        })
    }

    /// Read `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET`, `TRAKT_SYNC_INTERVAL_SECS`,
    /// `TRAKT_EPISODE_CHECK_INTERVAL_SECS`, `TRAKT_CATCHUP_LOOKBACK` and
    /// `TRAKT_ENROL_ALLOWED_HOSTS` from the process environment, then delegate to `from_parts`.
    pub fn from_env() -> Option<TraktConfig> {
        Self::from_parts(
            std::env::var("TRAKT_CLIENT_ID").ok(),
            std::env::var("TRAKT_CLIENT_SECRET").ok(),
            std::env::var("TRAKT_SYNC_INTERVAL_SECS").ok(),
            std::env::var("TRAKT_EPISODE_CHECK_INTERVAL_SECS").ok(),
            std::env::var("TRAKT_CATCHUP_LOOKBACK").ok(),
            std::env::var("TRAKT_ENROL_ALLOWED_HOSTS").ok(),
        )
    }
}

/// Acquisition-engine configuration (SP1). Held by `Config`.
#[derive(Clone, PartialEq, Eq)]
pub struct AcquisitionConfig {
    pub prefs: QualityPrefs,
    pub stall_timeout_secs: u64,
    pub max_acquire_attempts: u32,
    /// Override for the scraper base URL; `None` → template Torrentio from the active provider.
    pub scraper_addon_url: Option<String>,
    /// Optional HTTP(S) proxy URL applied to the SCRAPER's requests only (Torrentio / addon) — not
    /// the debrid CDN media reads, TMDB, or provider APIs. `None` → direct connection. Set by
    /// `from_env` (`SCRAPER_PROXY_URL`), not `from_parts`.
    pub scraper_proxy_url: Option<String>,
    /// Seconds an optimistically-added torrent may stay Pending without resolving/seeding
    /// before `observe` reaps it as dead (SP3). Default 600.
    pub acquire_dead_timeout_secs: u64,
    /// Minimum spacing between scraper requests, in ms (`SCRAPER_MIN_INTERVAL_MS`, default 1000,
    /// clamped 100–60000); a 429 backs off from it. Set by `from_env`, not `from_parts`.
    pub scraper_min_interval_ms: u64,
}

// Manual Debug redacts `scraper_addon_url` and `scraper_proxy_url` — both can embed credentials
// (the Torrentio addon URL embeds the debrid token as `<provider>=<TOKEN>`; a proxy URL can embed
// `user:password@`), so they must never reach the logs verbatim — only whether each is configured.
impl std::fmt::Debug for AcquisitionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcquisitionConfig")
            .field("prefs", &self.prefs)
            .field("stall_timeout_secs", &self.stall_timeout_secs)
            .field("max_acquire_attempts", &self.max_acquire_attempts)
            .field(
                "scraper_addon_url",
                &self.scraper_addon_url.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "scraper_proxy_url",
                &self.scraper_proxy_url.as_ref().map(|_| "<redacted>"),
            )
            .field("acquire_dead_timeout_secs", &self.acquire_dead_timeout_secs)
            .field("scraper_min_interval_ms", &self.scraper_min_interval_ms)
            .finish()
    }
}

impl Default for AcquisitionConfig {
    fn default() -> Self {
        Self::from_parts(None, None, None, None, None, None, None)
    }
}

impl AcquisitionConfig {
    /// The single boolean-env parser for the whole crate (`Config::apply_household_flags` and the
    /// `PREFER_*` knobs delegate here). Accepts `true`/`1`/`yes`/`on` and `false`/`0`/`no`/`off`
    /// (case-insensitive); anything else returns `default`. An unrecognised non-blank value of the
    /// env var `name` is `warn!`ed like every other knob's (a typo such as `ture` must be visible,
    /// not silently read as the default); absent or blank stays silent.
    pub(crate) fn parse_bool(s: Option<String>, name: &str, default: bool) -> bool {
        let Some(raw) = s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
            return default;
        };
        match raw.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => {
                warn!("Invalid {name} value '{raw}', falling back to {default}");
                default
            }
        }
    }

    /// Pure construction from raw values (env-independent, for tests).
    /// `scraper_addon_url` is set by `from_env`, not here.
    pub fn from_parts(
        max_resolution: Option<String>,
        audio_language: Option<String>,
        subtitle_language: Option<String>,
        prefer_hevc: Option<String>,
        prefer_hdr: Option<String>,
        stall_timeout_secs: Option<String>,
        max_acquire_attempts: Option<String>,
    ) -> Self {
        AcquisitionConfig {
            prefs: QualityPrefs {
                // A non-empty but unrecognised MAX_RESOLUTION warns (consistent with the other
                // numeric knobs) so a typo like `4` or `2160i` isn't silently treated as 1080p;
                // unset/empty defaults quietly to 1080p.
                max_resolution: match max_resolution {
                    Some(s) if !s.trim().is_empty() => {
                        MaxResolution::try_parse(&s).unwrap_or_else(|| {
                            warn!(
                                "Invalid MAX_RESOLUTION value '{}', falling back to 1080p \
                                 (expected 720/1080/2160, optionally 'p'-suffixed, or 4k/uhd)",
                                s
                            );
                            MaxResolution::P1080
                        })
                    }
                    _ => MaxResolution::P1080,
                },
                audio: AudioReq::parse(audio_language),
                subtitle: SubReq::parse(subtitle_language),
                prefer_hevc: Self::parse_bool(prefer_hevc, "PREFER_HEVC", true),
                prefer_hdr: Self::parse_bool(prefer_hdr, "PREFER_HDR", false),
            },
            // Clamp to a sane floor: STALL_TIMEOUT_SECS=0 would declare every Pending torrent
            // stalled instantly, and MAX_ACQUIRE_ATTEMPTS=0 would make the engine never retry —
            // both footguns, consistent with the other interval knobs which all clamp.
            stall_timeout_secs: parse_num(
                stall_timeout_secs,
                "STALL_TIMEOUT_SECS",
                DEFAULT_STALL_TIMEOUT_SECS,
                MIN_STALL_TIMEOUT_SECS,
                None,
            ),
            max_acquire_attempts: parse_num(
                max_acquire_attempts,
                "MAX_ACQUIRE_ATTEMPTS",
                DEFAULT_MAX_ACQUIRE_ATTEMPTS,
                MIN_MAX_ACQUIRE_ATTEMPTS,
                None,
            ),
            scraper_addon_url: None,
            scraper_proxy_url: None,
            acquire_dead_timeout_secs: DEFAULT_ACQUIRE_DEAD_TIMEOUT_SECS,
            scraper_min_interval_ms: DEFAULT_SCRAPER_MIN_INTERVAL_MS,
        }
    }

    pub fn from_env() -> Self {
        let mut a = Self::from_parts(
            std::env::var("MAX_RESOLUTION").ok(),
            std::env::var("AUDIO_LANGUAGE").ok(),
            std::env::var("SUBTITLE_LANGUAGE").ok(),
            std::env::var("PREFER_HEVC").ok(),
            std::env::var("PREFER_HDR").ok(),
            std::env::var("STALL_TIMEOUT_SECS").ok(),
            std::env::var("MAX_ACQUIRE_ATTEMPTS").ok(),
        );
        a.scraper_addon_url = std::env::var("SCRAPER_ADDON_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        a.scraper_proxy_url = std::env::var("SCRAPER_PROXY_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        a.acquire_dead_timeout_secs =
            Self::parse_dead_timeout(std::env::var("ACQUIRE_DEAD_TIMEOUT_SECS").ok());
        a.scraper_min_interval_ms =
            Self::parse_scraper_min_interval(std::env::var("SCRAPER_MIN_INTERVAL_MS").ok());
        a
    }

    /// Parse `SCRAPER_MIN_INTERVAL_MS`: unset/blank → 1000, an unparseable value warns + falls back to
    /// 1000, and any value is clamped to 100–60000 (the limiter needs a non-zero interval to back off
    /// from, and over a minute per scrape would stall acquisition). Pure for unit-testability.
    fn parse_scraper_min_interval(raw: Option<String>) -> u64 {
        parse_num(
            raw,
            "SCRAPER_MIN_INTERVAL_MS",
            DEFAULT_SCRAPER_MIN_INTERVAL_MS,
            MIN_SCRAPER_MIN_INTERVAL_MS,
            Some(MAX_SCRAPER_MIN_INTERVAL_MS),
        )
    }

    /// Parse `ACQUIRE_DEAD_TIMEOUT_SECS`: unset/blank → 600, an unparseable value warns + falls back
    /// to 600 (matching the other interval knobs — a typo like `10m` must be visible, not silently
    /// treated as unset), and any value is clamped up to the 120s floor. Pure for unit-testability.
    fn parse_dead_timeout(raw: Option<String>) -> u64 {
        parse_num(
            raw,
            "ACQUIRE_DEAD_TIMEOUT_SECS",
            DEFAULT_ACQUIRE_DEAD_TIMEOUT_SECS,
            MIN_ACQUIRE_DEAD_TIMEOUT_SECS,
            None,
        )
    }
}

/// Upgrade-engine configuration (SP3). Held by `Config`. The job is spawned only when
/// `interval_secs > 0` (default 86_400 = daily; set `UPGRADE_INTERVAL_SECS=0` to disable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeConfig {
    pub interval_secs: u64,
    /// Maximum titles visited per pass (`UPGRADE_BUDGET_PER_TICK`); `None` (the default) = every
    /// owned title, every pass.
    pub budget_per_tick: Option<u32>,
    pub idle_secs: u64,
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self::from_parts(None, None, None)
    }
}

impl UpgradeConfig {
    /// `true` when the upgrade job should run (interval non-zero).
    pub fn enabled(&self) -> bool {
        self.interval_secs > 0
    }

    /// Pure construction from raw optional values (env-independent, for tests).
    /// `interval_secs`: see [`parse_upgrade_interval`].
    pub fn from_parts(
        interval_secs: Option<String>,
        budget_per_tick: Option<String>,
        idle_secs: Option<String>,
    ) -> Self {
        UpgradeConfig {
            interval_secs: parse_upgrade_interval(interval_secs),
            budget_per_tick: parse_upgrade_budget(budget_per_tick),
            idle_secs: parse_num(idle_secs, "UPGRADE_IDLE_SECS", 300, 30, None),
        }
    }

    /// Read `UPGRADE_INTERVAL_SECS`, `UPGRADE_BUDGET_PER_TICK` and `UPGRADE_IDLE_SECS` from the
    /// process environment, then delegate to `from_parts`.
    pub fn from_env() -> Self {
        Self::from_parts(
            std::env::var("UPGRADE_INTERVAL_SECS").ok(),
            std::env::var("UPGRADE_BUDGET_PER_TICK").ok(),
            std::env::var("UPGRADE_IDLE_SECS").ok(),
        )
    }
}

/// `UPGRADE_INTERVAL_SECS`: unset or blank = daily (86_400); `0`, or one of the off-words
/// [`AcquisitionConfig::parse_bool`] takes (`false`/`no`/`off`, case-insensitive) = disabled; any
/// other number is clamped to a 600 s minimum. It is the only switch that stops the upgrade engine,
/// which replaces and deletes torrents the user added, so an off-word must not read as a typo and
/// leave the engine running. Anything else warns, saying the engine is on, and falls back to daily.
fn parse_upgrade_interval(v: Option<String>) -> u64 {
    const DAILY: u64 = 86_400;
    let Some(s) = v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return DAILY;
    };
    if ["false", "no", "off"]
        .iter()
        .any(|w| s.eq_ignore_ascii_case(w))
    {
        return 0;
    }
    match s.parse::<u64>() {
        Ok(0) => 0,
        Ok(n) => n.max(600),
        Err(_) => {
            warn!(
                "Invalid UPGRADE_INTERVAL_SECS value '{s}', falling back to {DAILY}: the upgrade \
                 engine is ON (set 0 or off to disable it)"
            );
            DAILY
        }
    }
}

/// `UPGRADE_BUDGET_PER_TICK`: unset or blank = no budget (every pass visits the whole library); a
/// number caps the titles visited per pass (minimum 1); anything else warns and falls back to no
/// budget.
fn parse_upgrade_budget(v: Option<String>) -> Option<u32> {
    let s = v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())?;
    match s.parse::<u64>() {
        Ok(n) => Some(n.clamp(1, u32::MAX as u64) as u32),
        Err(_) => {
            warn!(
                "Invalid UPGRADE_BUDGET_PER_TICK value '{}', falling back to no budget (the whole library each pass)",
                s
            );
            None
        }
    }
}

/// Startup configuration parsed from environment variables.
///
/// These values are fixed at startup. A future DB-backed override layer (web-UI
/// settings, SP4) will supply runtime-tunable *preferences* alongside this; the
/// startup values here (tokens, paths, port) are not runtime-overridable, so they
/// are plain fields rather than accessors.
#[derive(Clone)]
pub struct Config {
    pub provider_kind: ProviderKind,
    pub provider_token: String,
    pub tmdb_api_key: String,
    pub scan_interval_secs: u64,
    pub db_path: String,
    pub port: u16,
    pub acquisition: AcquisitionConfig,
    /// Trakt sync config. `None` when `TRAKT_CLIENT_ID`/`TRAKT_CLIENT_SECRET` are absent.
    pub trakt: Option<TraktConfig>,
    /// Upgrade-engine config (SP3). Always present; `upgrade.enabled()` gates the job.
    pub upgrade: UpgradeConfig,
    /// When `true`, the duplicate-dedup pass actually DELETES redundant torrents from the provider.
    /// Default `false` = dry-run (logs the plan only). Set `DEDUP_REMOVE_DUPLICATES=true` to enable
    /// destructive removal once the logged plan has been reviewed.
    pub dedup_remove_duplicates: bool,
    /// When `true`, a fully-watched ENDED show that isn't watchlisted is auto-removed (Trigger-A
    /// finish cleanup) — including ones already in the library. Default `false` only *logs* the
    /// finish-removal candidates (a preview) without removing. Set `REMOVE_FINISHED_SHOWS=true` to
    /// enable removal once the logged candidates have been reviewed.
    pub remove_finished_shows: bool,
    /// When `true`, the upgrade engine may perform a CORRECTIVE DOWNGRADE — replacing an owned copy
    /// that is now above `MAX_RESOLUTION` with a ceiling-conforming cached release (deleting the
    /// higher-res copy). Default `false` only *logs* the planned downgrades (a preview). Pure quality
    /// upgrades are unaffected by this flag. Mirrors `DEDUP_REMOVE_DUPLICATES` / `REMOVE_FINISHED_SHOWS`.
    pub allow_resolution_downgrade: bool,
    /// Size (bytes) of the FIRST read-ahead fetch when a media file is opened — front-loads the
    /// container header + early index so a player's open/ffprobe burst needs fewer cold CDN
    /// round-trips before playback. Subsequent reads use the normal `BUFFER_SIZE` (2 MB) window.
    /// From `CDN_FIRST_READ_MB` (default 8, clamped 2–16 MB; 2 = no enlargement).
    pub cdn_first_read_bytes: u64,
}

/// Default first-read window in MB (used when `CDN_FIRST_READ_MB` is unset/invalid).
const CDN_FIRST_READ_MB_DEFAULT: u64 = 8;
/// Lower bound = the normal read-ahead window (2 MB); upper bound = the per-fetch cap (16 MB), which
/// mirror `dav_fs::BUFFER_SIZE` / `MAX_FETCH_SIZE`.
const CDN_FIRST_READ_MB_MIN: u64 = 2;
const CDN_FIRST_READ_MB_MAX: u64 = 16;

// Compile-time invariants: the knob's lower bound must equal the normal read-ahead window so a
// configured value can never undercut it, the default must sit within [min, max], and the upper
// bound must not exceed the per-fetch cap. A future change to `dav_fs::BUFFER_SIZE` that broke the
// first of these would fail the build here rather than silently mis-sizing the first read.
const _: () = assert!(CDN_FIRST_READ_MB_MIN * 1024 * 1024 == crate::dav_fs::BUFFER_SIZE as u64);
const _: () = assert!(CDN_FIRST_READ_MB_MAX * 1024 * 1024 <= crate::dav_fs::MAX_FETCH_SIZE as u64);
const _: () = assert!(
    CDN_FIRST_READ_MB_DEFAULT >= CDN_FIRST_READ_MB_MIN
        && CDN_FIRST_READ_MB_DEFAULT <= CDN_FIRST_READ_MB_MAX
);

/// Parse `CDN_FIRST_READ_MB` (whole MB) into a byte count, clamped to [2, 16] MB. Unset/blank/invalid
/// → the 8 MB default. Standalone + pure so the parse/clamp is unit-testable without the environment.
fn parse_cdn_first_read_bytes(raw: Option<String>) -> u64 {
    let mb = parse_num(
        raw,
        "CDN_FIRST_READ_MB",
        CDN_FIRST_READ_MB_DEFAULT,
        CDN_FIRST_READ_MB_MIN,
        Some(CDN_FIRST_READ_MB_MAX),
    );
    mb * 1024 * 1024
}

// Manual Debug redacts the provider token and TMDB API key so a `debug!("{config:?}")` (or a panic
// message carrying the Config) can never leak credentials. `AcquisitionConfig`/`TraktConfig` have
// their own redacting Debug impls (proxy URL / Trakt secret).
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("provider_kind", &self.provider_kind)
            .field("provider_token", &"<redacted>")
            .field("tmdb_api_key", &"<redacted>")
            .field("scan_interval_secs", &self.scan_interval_secs)
            .field("db_path", &self.db_path)
            .field("port", &self.port)
            .field("acquisition", &self.acquisition)
            .field("trakt", &self.trakt)
            .field("upgrade", &self.upgrade)
            .field("dedup_remove_duplicates", &self.dedup_remove_duplicates)
            .field("remove_finished_shows", &self.remove_finished_shows)
            .field(
                "allow_resolution_downgrade",
                &self.allow_resolution_downgrade,
            )
            .field("cdn_first_read_bytes", &self.cdn_first_read_bytes)
            .finish()
    }
}

impl Config {
    /// Build from the process environment.
    pub fn from_env() -> Result<Self, AppError> {
        let mut cfg = Self::from_parts(
            std::env::var("RD_API_TOKEN").ok(),
            std::env::var("TORBOX_API_KEY").ok(),
            std::env::var("TMDB_API_KEY").ok(),
            std::env::var("SCAN_INTERVAL_SECS").ok(),
            std::env::var("DB_PATH").ok(),
            std::env::var("PORT").ok(),
        )?;
        cfg.acquisition = AcquisitionConfig::from_env();
        cfg.trakt = TraktConfig::from_env();
        cfg.upgrade = UpgradeConfig::from_env();
        cfg.cdn_first_read_bytes =
            parse_cdn_first_read_bytes(std::env::var("CDN_FIRST_READ_MB").ok());
        cfg.apply_household_flags(|name| std::env::var(name).ok());
        Ok(cfg)
    }

    /// Apply the three household-policy flags from a name→value `lookup`. Factored out of `from_env` so
    /// the env-NAME → field wiring is unit-testable WITHOUT mutating the process environment (a
    /// name swap would silently enable destructive duplicate deletion when the operator meant
    /// finished-show removal). Each flag delegates to the crate-wide boolean parser, so it accepts the
    /// same synonyms (`1`/`true`/`yes`/`on`) and defaults to false when unset/unrecognised (an
    /// unrecognised value is `warn!`ed).
    fn apply_household_flags(&mut self, lookup: impl Fn(&str) -> Option<String>) {
        let flag = |name: &str| AcquisitionConfig::parse_bool(lookup(name), name, false);
        self.dedup_remove_duplicates = flag("DEDUP_REMOVE_DUPLICATES");
        self.remove_finished_shows = flag("REMOVE_FINISHED_SHOWS");
        self.allow_resolution_downgrade = flag("ALLOW_RESOLUTION_DOWNGRADE");
    }

    /// Pure construction from raw optional values — unit-testable without touching
    /// the process environment.
    pub fn from_parts(
        rd_token: Option<String>,
        torbox_token: Option<String>,
        tmdb_api_key: Option<String>,
        scan_interval_secs: Option<String>,
        db_path: Option<String>,
        port: Option<String>,
    ) -> Result<Self, AppError> {
        let (provider_kind, provider_token) = choose_provider(rd_token, torbox_token)?;

        // Reject a blank key (whitespace-only) rather than deferring the failure to TmdbClient,
        // mirroring how `choose_provider` treats a whitespace-only provider token as unset.
        let tmdb_api_key = tmdb_api_key
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::Config("TMDB_API_KEY must be set".to_string()))?;

        // The floor keeps a tiny interval from hammering the provider API.
        let scan_interval_secs = parse_num(
            scan_interval_secs,
            "SCAN_INTERVAL_SECS",
            DEFAULT_SCAN_INTERVAL_SECS,
            MIN_SCAN_INTERVAL_SECS,
            None,
        );

        // Trim + treat blank as unset (consistent with the other env knobs — a quoted/whitespace
        // `DB_PATH` from compose YAML, or an empty `DB_PATH=`, falls back to the default rather than
        // becoming a whitespace/empty path).
        let db_path = db_path
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "metadata.db".to_string());

        // Shared with the `--healthcheck` probe (see `resolve_port`) so both target the same port.
        let port_raw = port;
        let (port, port_fell_back) = resolve_port(port_raw.as_deref());
        if port_fell_back {
            warn!(
                "Invalid PORT value '{}', falling back to {}",
                port_raw.as_deref().unwrap_or("").trim(),
                DEFAULT_PORT
            );
        }

        Ok(Self {
            provider_kind,
            provider_token,
            tmdb_api_key,
            scan_interval_secs,
            db_path,
            port,
            acquisition: AcquisitionConfig::default(),
            trakt: None,
            upgrade: UpgradeConfig::default(),
            dedup_remove_duplicates: false,
            remove_finished_shows: false,
            allow_resolution_downgrade: false,
            // Default window; `from_env` overrides from `CDN_FIRST_READ_MB`. (Not a `from_parts`
            // parameter — it's a perf knob, not part of the core provider/port/db wiring.)
            cdn_first_read_bytes: CDN_FIRST_READ_MB_DEFAULT * 1024 * 1024,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_port_matches_server_and_healthcheck() {
        // Unset / blank → default, no fallback flag.
        assert_eq!(resolve_port(None), (8080, false));
        assert_eq!(resolve_port(Some("")), (8080, false));
        assert_eq!(resolve_port(Some("   ")), (8080, false));
        // Valid, incl. surrounding whitespace (quoted compose YAML).
        assert_eq!(resolve_port(Some("9000")), (9000, false));
        assert_eq!(resolve_port(Some("  9000 ")), (9000, false));
        // Invalid / zero → fall back to 8080 AND flag it (so the server warns).
        assert_eq!(resolve_port(Some("0")), (8080, true));
        assert_eq!(resolve_port(Some("70000")), (8080, true));
        assert_eq!(resolve_port(Some("nope")), (8080, true));
    }

    fn parts(
        rd: Option<&str>,
        tb: Option<&str>,
        tmdb: Option<&str>,
        scan: Option<&str>,
        db: Option<&str>,
        port: Option<&str>,
    ) -> Result<Config, AppError> {
        Config::from_parts(
            rd.map(String::from),
            tb.map(String::from),
            tmdb.map(String::from),
            scan.map(String::from),
            db.map(String::from),
            port.map(String::from),
        )
    }

    #[test]
    fn rd_only_with_defaults() {
        let c = parts(Some("rd-tok"), None, Some("tmdb"), None, None, None).unwrap();
        assert_eq!(c.provider_kind, ProviderKind::RealDebrid);
        assert_eq!(c.provider_token, "rd-tok");
        assert_eq!(c.tmdb_api_key, "tmdb");
        assert_eq!(c.scan_interval_secs, 60);
        assert_eq!(c.db_path, "metadata.db");
        assert_eq!(c.port, 8080);
    }

    #[test]
    fn torbox_only() {
        let c = parts(None, Some("tb-tok"), Some("tmdb"), None, None, None).unwrap();
        assert_eq!(c.provider_kind, ProviderKind::TorBox);
        assert_eq!(c.provider_token, "tb-tok");
    }

    #[test]
    fn debug_redacts_secrets() {
        // Config Debug must never print the provider token or TMDB key (a stray `debug!("{config:?}")`
        // would otherwise leak credentials).
        let mut c = parts(
            Some("rd-SECRET-tok"),
            None,
            Some("tmdb-SECRET-key"),
            None,
            None,
            None,
        )
        .unwrap();
        c.trakt = TraktConfig::from_parts(
            Some("trakt-client-id".into()),
            Some("trakt-SECRET-secret".into()),
            None,
            None,
            None,
            None,
        );
        c.acquisition.scraper_proxy_url = Some("http://user:SECRETpw@proxy:8080".into());
        // A custom Torrentio addon URL embeds the debrid token (`realdebrid=<TOKEN>`), so it too
        // must never be printed verbatim.
        c.acquisition.scraper_addon_url =
            Some("https://torrentio.strem.fun/realdebrid=rd-SECRET-addontok/manifest.json".into());
        let dbg = format!("{c:?}");
        assert!(
            !dbg.contains("rd-SECRET-tok"),
            "provider token leaked: {dbg}"
        );
        assert!(!dbg.contains("tmdb-SECRET-key"), "tmdb key leaked: {dbg}");
        assert!(
            !dbg.contains("trakt-SECRET-secret"),
            "trakt secret leaked: {dbg}"
        );
        assert!(!dbg.contains("SECRETpw"), "proxy creds leaked: {dbg}");
        assert!(
            !dbg.contains("rd-SECRET-addontok"),
            "scraper addon-url token leaked: {dbg}"
        );
        assert!(
            dbg.contains("<redacted>"),
            "expected redaction markers: {dbg}"
        );
        // Non-secret fields are still printed.
        assert!(dbg.contains("RealDebrid"));
        assert!(dbg.contains("trakt-client-id"));
        assert!(dbg.contains("cdn_first_read_bytes"));
    }

    #[test]
    fn both_tokens_is_error() {
        assert!(parts(Some("a"), Some("b"), Some("tmdb"), None, None, None).is_err());
    }

    #[test]
    fn neither_token_is_error() {
        assert!(parts(None, None, Some("tmdb"), None, None, None).is_err());
    }

    #[test]
    fn missing_tmdb_is_error() {
        assert!(parts(Some("rd"), None, None, None, None, None).is_err());
    }

    #[test]
    fn blank_tmdb_is_error() {
        // A whitespace-only key is rejected at config time, not deferred to TmdbClient.
        assert!(parts(Some("rd"), None, Some("   "), None, None, None).is_err());
    }

    #[test]
    fn scan_interval_and_port_tolerate_surrounding_whitespace() {
        // Quoted compose YAML can yield values with surrounding whitespace; honour them rather
        // than silently falling back to the default.
        let c = parts(
            Some("rd"),
            None,
            Some("t"),
            Some(" 120 "),
            None,
            Some(" 9000 "),
        )
        .unwrap();
        assert_eq!(c.scan_interval_secs, 120);
        assert_eq!(c.port, 9000);
    }

    #[test]
    fn scan_interval_clamped_and_parsed() {
        assert_eq!(
            parts(Some("rd"), None, Some("t"), Some("5"), None, None)
                .unwrap()
                .scan_interval_secs,
            10
        );
        assert_eq!(
            parts(Some("rd"), None, Some("t"), Some("abc"), None, None)
                .unwrap()
                .scan_interval_secs,
            60
        );
        assert_eq!(
            parts(Some("rd"), None, Some("t"), Some("120"), None, None)
                .unwrap()
                .scan_interval_secs,
            120
        );
    }

    #[test]
    fn port_parsed_with_fallback() {
        assert_eq!(
            parts(Some("rd"), None, Some("t"), None, None, Some("9000"))
                .unwrap()
                .port,
            9000
        );
        assert_eq!(
            parts(Some("rd"), None, Some("t"), None, None, Some("nope"))
                .unwrap()
                .port,
            8080
        );
        // PORT=0 is a footgun (binds a random ephemeral port; the healthcheck then connects to
        // 127.0.0.1:0 and fails, so the container reports unhealthy and rclone, gated on
        // `service_healthy`, never mounts). Treat it like the unparseable case → default 8080.
        assert_eq!(
            parts(Some("rd"), None, Some("t"), None, None, Some("0"))
                .unwrap()
                .port,
            8080
        );
    }

    #[test]
    fn db_path_override() {
        assert_eq!(
            parts(Some("rd"), None, Some("t"), None, Some("/data/x.db"), None)
                .unwrap()
                .db_path,
            "/data/x.db"
        );
    }

    #[test]
    fn acquisition_defaults() {
        let a = AcquisitionConfig::from_parts(None, None, None, None, None, None, None);
        assert_eq!(a.prefs.max_resolution, MaxResolution::P1080);
        assert_eq!(a.prefs.audio, AudioReq::Original);
        assert_eq!(a.prefs.subtitle, SubReq::None);
        assert!(a.prefs.prefer_hevc);
        assert!(!a.prefs.prefer_hdr);
        assert_eq!(a.stall_timeout_secs, 1800);
        assert_eq!(a.max_acquire_attempts, 5);
        assert_eq!(a.scraper_addon_url, None);
        assert_eq!(a.scraper_proxy_url, None);
    }

    #[test]
    fn acquisition_parses_overrides() {
        let a = AcquisitionConfig::from_parts(
            Some("2160".into()),
            Some("eng".into()),
            Some("eng".into()),
            Some("false".into()),
            Some("true".into()),
            Some("600".into()),
            Some("3".into()),
        );
        assert_eq!(a.prefs.max_resolution, MaxResolution::P2160);
        assert_eq!(a.prefs.audio, AudioReq::Lang("eng".into()));
        assert_eq!(a.prefs.subtitle, SubReq::Lang("eng".into()));
        assert!(!a.prefs.prefer_hevc);
        assert!(a.prefs.prefer_hdr);
        assert_eq!(a.stall_timeout_secs, 600);
        assert_eq!(a.max_acquire_attempts, 3);
    }

    #[test]
    fn scraper_min_interval_defaults_warns_and_clamps() {
        let p = AcquisitionConfig::parse_scraper_min_interval;
        assert_eq!(p(None), 1_000);
        assert_eq!(p(Some("  ".into())), 1_000);
        assert_eq!(p(Some("2500".into())), 2_500);
        assert_eq!(p(Some("1s".into())), 1_000, "unparseable → default");
        assert_eq!(
            p(Some("0".into())),
            100,
            "the limiter needs an interval to back off from"
        );
        assert_eq!(p(Some("600000".into())), 60_000);
        assert_eq!(AcquisitionConfig::default().scraper_min_interval_ms, 1_000);
    }

    #[test]
    fn acquisition_clamps_zero_footguns() {
        // STALL_TIMEOUT_SECS=0 (instant-stall) and MAX_ACQUIRE_ATTEMPTS=0 (never retry) are
        // nonsensical — clamp them to sane floors instead of accepting the footgun.
        let a = AcquisitionConfig::from_parts(
            None,
            None,
            None,
            None,
            None,
            Some("0".into()),
            Some("0".into()),
        );
        assert_eq!(a.stall_timeout_secs, 60);
        assert_eq!(a.max_acquire_attempts, 1);
    }

    #[test]
    fn acquire_dead_timeout_parses_clamps_and_falls_back() {
        // unset/blank → 600; below the floor → 120; valid → as given; non-numeric/typo → 600.
        assert_eq!(AcquisitionConfig::parse_dead_timeout(None), 600);
        assert_eq!(
            AcquisitionConfig::parse_dead_timeout(Some("  ".into())),
            600
        );
        assert_eq!(
            AcquisitionConfig::parse_dead_timeout(Some("30".into())),
            120
        );
        assert_eq!(
            AcquisitionConfig::parse_dead_timeout(Some("900".into())),
            900
        );
        assert_eq!(
            AcquisitionConfig::parse_dead_timeout(Some("10m".into())),
            600,
            "a typo must warn-and-default, not be treated as a tiny timeout"
        );
    }

    #[test]
    fn acquisition_non_numeric_strings_fall_back_to_defaults() {
        // A non-numeric STALL_TIMEOUT_SECS / MAX_ACQUIRE_ATTEMPTS must warn-and-default (1800 / 5),
        // symmetric with SCAN_INTERVAL_SECS="abc"→60. Without this an operator typo could silently
        // produce a 0/garbage timeout.
        let a = AcquisitionConfig::from_parts(
            None,
            None,
            None,
            None,
            None,
            Some("abc".into()),
            Some("xyz".into()),
        );
        assert_eq!(a.stall_timeout_secs, 1800);
        assert_eq!(a.max_acquire_attempts, 5);
    }

    #[test]
    fn max_resolution_parse_and_invalid_falls_back() {
        assert_eq!(MaxResolution::parse("720"), MaxResolution::P720);
        assert_eq!(MaxResolution::parse("1080"), MaxResolution::P1080);
        assert_eq!(MaxResolution::parse("2160"), MaxResolution::P2160);
        assert_eq!(MaxResolution::parse("4k"), MaxResolution::P2160);
        assert_eq!(MaxResolution::parse("garbage"), MaxResolution::P1080);
        // try_parse distinguishes "unrecognised" (None) from a real ceiling, which the env-read
        // site uses to warn rather than silently default.
        assert_eq!(MaxResolution::try_parse("garbage"), None);
        assert_eq!(MaxResolution::try_parse("2160i"), None);
        assert_eq!(MaxResolution::try_parse("720"), Some(MaxResolution::P720));
        // An unrecognised env value still yields the 1080p default (the warn is a side effect).
        let a =
            AcquisitionConfig::from_parts(Some("2160i".into()), None, None, None, None, None, None);
        assert_eq!(a.prefs.max_resolution, MaxResolution::P1080);
    }

    #[test]
    fn subtitle_none_keyword_means_skip() {
        assert_eq!(SubReq::parse(None), SubReq::None);
        assert_eq!(SubReq::parse(Some("none".into())), SubReq::None);
        assert_eq!(
            SubReq::parse(Some("eng".into())),
            SubReq::Lang("eng".into())
        );
    }

    #[test]
    fn audio_req_parse_keyword_empty_and_lang() {
        // Symmetric with subtitle_none_keyword_means_skip: the "original" keyword (any case), an
        // empty/whitespace value, and absence all mean Original; a code is lowercased.
        assert_eq!(AudioReq::parse(None), AudioReq::Original);
        assert_eq!(AudioReq::parse(Some("original".into())), AudioReq::Original);
        assert_eq!(AudioReq::parse(Some("ORIGINAL".into())), AudioReq::Original);
        assert_eq!(AudioReq::parse(Some("   ".into())), AudioReq::Original);
        assert_eq!(
            AudioReq::parse(Some("ENG".into())),
            AudioReq::Lang("eng".into()),
            "a language code is normalised to lowercase"
        );
        // A 2-letter ISO 639-1 code is also accepted.
        assert_eq!(
            AudioReq::parse(Some("en".into())),
            AudioReq::Lang("en".into())
        );
    }

    #[test]
    fn quality_prefs_resolve_audio_maps_original_to_title_language() {
        let base = QualityPrefs {
            max_resolution: MaxResolution::P1080,
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            prefer_hevc: true,
            prefer_hdr: false,
        };
        // A known original language turns the default `Original` into a concrete `Lang(...)` (lowercased),
        // so acquisition ranking down-ranks a wrong-language release exactly as the probe rejects it.
        assert_eq!(
            base.with_resolved_audio(Some("JA"), &[]).audio,
            AudioReq::Lang("ja".into())
        );
        // An unknown / empty / whitespace original language leaves `Original` untouched (no penalty).
        assert_eq!(
            base.with_resolved_audio(None, &[]).audio,
            AudioReq::Original
        );
        assert_eq!(
            base.with_resolved_audio(Some(""), &[]).audio,
            AudioReq::Original
        );
        assert_eq!(
            base.with_resolved_audio(Some("  "), &[]).audio,
            AudioReq::Original
        );
        // An explicit `Lang` is never overridden by the title's original language.
        let explicit = QualityPrefs {
            audio: AudioReq::Lang("eng".into()),
            ..base.clone()
        };
        assert_eq!(
            explicit.with_resolved_audio(Some("ja"), &[]).audio,
            AudioReq::Lang("eng".into())
        );
        // Non-audio fields are preserved.
        let r = base.with_resolved_audio(Some("de"), &[]);
        assert_eq!(r.max_resolution, base.max_resolution);
        assert!(r.prefer_hevc);
        assert!(!r.prefer_hdr);
    }

    #[test]
    fn with_resolved_audio_unions_original_and_spoken() {
        let base = QualityPrefs {
            max_resolution: MaxResolution::P1080,
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            prefer_hevc: true,
            prefer_hdr: false,
        };
        let spoken = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // A `fi`-original film, spoken [en, fi, es] → any of them, original first, de-duplicated.
        assert_eq!(
            base.with_resolved_audio(Some("fi"), &spoken(&["en", "fi", "es"]))
                .audio,
            AudioReq::AnyOf(vec!["fi".into(), "en".into(), "es".into()])
        );
        // Spoken list that adds nothing beyond the original collapses to a single `Lang`.
        assert_eq!(
            base.with_resolved_audio(Some("FI"), &spoken(&["fi"])).audio,
            AudioReq::Lang("fi".into())
        );
        // No original but spoken languages known → those (lowercased, empties dropped).
        assert_eq!(
            base.with_resolved_audio(None, &spoken(&["EN", " ", "en"]))
                .audio,
            AudioReq::Lang("en".into())
        );
        assert_eq!(
            base.with_resolved_audio(Some(""), &spoken(&["en", "de"]))
                .audio,
            AudioReq::AnyOf(vec!["en".into(), "de".into()])
        );
        // An explicit language ignores the title's spoken languages entirely.
        let explicit = QualityPrefs {
            audio: AudioReq::Lang("de".into()),
            ..base.clone()
        };
        assert_eq!(
            explicit
                .with_resolved_audio(Some("fi"), &spoken(&["en", "fi"]))
                .audio,
            AudioReq::Lang("de".into())
        );
        // `AnyOf` is never produced by parsing the env var.
        assert!(!matches!(
            AudioReq::parse(Some("fi,en".into())),
            AudioReq::AnyOf(_)
        ));
    }

    #[test]
    fn language_reqs_reject_an_unknown_two_letter_code_to_the_default() {
        // `probe::to_iso639_2` holds the complete ISO 639-1 table, so a 2-letter value it doesn't
        // know is a typo. Taken as written it would match no track, and every release would be
        // rejected and blacklisted for 30 days.
        let logs = captured_warnings(|| {
            assert_eq!(AudioReq::parse(Some("xy".into())), AudioReq::Original);
            assert_eq!(SubReq::parse(Some("xy".into())), SubReq::None);
        });
        assert!(
            logs.contains("Invalid AUDIO_LANGUAGE 'xy'")
                && logs.contains("Invalid SUBTITLE_LANGUAGE 'xy'"),
            "an unknown 2-letter code must be rejected with a warning: {logs:?}"
        );
        // Known codes, TMDB's non-ISO `cn` / `sh` and a /T form are kept, silently. The
        // no-language `xx` / `zxx` are kept for the audio only (they turn its check off); the
        // subtitle side refuses them, see `subtitle_language_refuses_no_language_to_none`.
        let logs = captured_warnings(|| {
            for code in ["en", "eng", "deu", "cn", "sh", "yue", "cmn"] {
                assert_eq!(
                    AudioReq::parse(Some(code.into())),
                    AudioReq::Lang(code.into()),
                    "{code}"
                );
                assert_eq!(
                    SubReq::parse(Some(code.into())),
                    SubReq::Lang(code.into()),
                    "{code}"
                );
            }
            for code in ["xx", "zxx"] {
                assert_eq!(
                    AudioReq::parse(Some(code.into())),
                    AudioReq::Lang(code.into()),
                    "{code}"
                );
            }
        });
        assert!(logs.is_empty(), "a known code must not warn: {logs:?}");
    }

    #[test]
    fn subtitle_language_refuses_no_language_to_none() {
        // `xx` (TMDB's "No Language") / `zxx` name no language, and no subtitle track is tagged
        // with them. The probe's subtitle check has no no-language exemption, so taken as written
        // every probed release would fail it and be blacklisted for 30 days.
        let logs = captured_warnings(|| {
            for code in ["xx", "zxx", " ZXX ", "XX"] {
                assert_eq!(SubReq::parse(Some(code.into())), SubReq::None, "{code}");
            }
        });
        for code in ["xx", "zxx", "ZXX", "XX"] {
            assert!(
                logs.contains(&format!("Invalid SUBTITLE_LANGUAGE '{code}'")),
                "a no-language subtitle code must be refused with a warning: {logs:?}"
            );
        }
        assert!(
            logs.contains("falling back to 'none'"),
            "the warning names the fallback: {logs:?}"
        );
    }

    #[test]
    fn language_reqs_keep_an_unknown_three_letter_code_with_a_warning() {
        // A transposed 3-letter code (`egn`) is the commonest typo, but rare valid ISO 639-2 codes
        // (`haw`, Hawaiian) aren't in the probe's tables either, so it is kept and warned about.
        let logs = captured_warnings(|| {
            assert_eq!(
                AudioReq::parse(Some("EGN".into())),
                AudioReq::Lang("egn".into())
            );
            assert_eq!(
                SubReq::parse(Some("egn".into())),
                SubReq::Lang("egn".into())
            );
        });
        assert!(
            logs.contains("AUDIO_LANGUAGE 'EGN' is not a language code")
                && logs.contains("SUBTITLE_LANGUAGE 'egn' is not a language code"),
            "an unknown 3-letter code must warn, naming the variable: {logs:?}"
        );
        assert!(
            logs.contains("blacklisted"),
            "the warning says what a misspelt code does: {logs:?}"
        );
    }

    #[test]
    fn language_reqs_read_words_that_are_not_codes_as_the_default() {
        // `off` is the boolean knobs' word for "no", so SUBTITLE_LANGUAGE takes it as `none`,
        // silently. `any` / `all` are three letters but no ISO 639-2 code: taken as written they
        // would match no track, so they fall back to the default with a warning.
        let logs = captured_warnings(|| {
            assert_eq!(SubReq::parse(Some("off".into())), SubReq::None);
            assert_eq!(SubReq::parse(Some(" OFF ".into())), SubReq::None);
        });
        assert!(logs.is_empty(), "`off` is an alias of `none`: {logs:?}");
        let logs = captured_warnings(|| {
            for word in ["any", "ALL"] {
                assert_eq!(SubReq::parse(Some(word.into())), SubReq::None, "{word}");
            }
            for word in ["off", "any", "All"] {
                assert_eq!(
                    AudioReq::parse(Some(word.into())),
                    AudioReq::Original,
                    "{word}"
                );
            }
        });
        for warned in [
            "Invalid SUBTITLE_LANGUAGE 'any'",
            "Invalid SUBTITLE_LANGUAGE 'ALL'",
            "Invalid AUDIO_LANGUAGE 'off'",
            "Invalid AUDIO_LANGUAGE 'any'",
            "Invalid AUDIO_LANGUAGE 'All'",
        ] {
            assert!(logs.contains(warned), "{warned}: {logs:?}");
        }
    }

    #[test]
    fn language_reqs_reject_the_special_codes_no_track_carries() {
        // `und` (undetermined) and `mul` (multiple languages) are ISO 639-2 codes the probe knows,
        // but no parsed track carries them: both track parsers read an `und` track as untagged, and
        // a multi-audio release tags each track with its own language. Taken as written either
        // would fail every release whose tracks are all tagged, so, like `any`, they fall back to
        // the default with a warning that says why.
        let logs = captured_warnings(|| {
            for code in ["und", "MUL", " mul "] {
                assert_eq!(
                    AudioReq::parse(Some(code.into())),
                    AudioReq::Original,
                    "{code}"
                );
                assert_eq!(SubReq::parse(Some(code.into())), SubReq::None, "{code}");
            }
        });
        for warned in [
            "Invalid AUDIO_LANGUAGE 'und'",
            "Invalid AUDIO_LANGUAGE 'MUL'",
            "Invalid AUDIO_LANGUAGE 'mul'",
            "Invalid SUBTITLE_LANGUAGE 'und'",
            "Invalid SUBTITLE_LANGUAGE 'MUL'",
            "Invalid SUBTITLE_LANGUAGE 'mul'",
        ] {
            assert!(logs.contains(warned), "{warned}: {logs:?}");
        }
        assert!(
            logs.contains("no track") && logs.contains("falling back to 'original'"),
            "the warning says the code matches no track, and what is used instead: {logs:?}"
        );
    }

    #[test]
    fn language_reqs_keep_no_as_norwegian_with_a_warning() {
        // `no` is Norwegian's ISO 639-1 code, so it is kept, but it reads like "no requirement";
        // the warning names the keyword that skips the check and a spelling that keeps Norwegian.
        let logs = captured_warnings(|| {
            assert_eq!(SubReq::parse(Some("no".into())), SubReq::Lang("no".into()));
            assert_eq!(
                AudioReq::parse(Some("NO".into())),
                AudioReq::Lang("no".into())
            );
        });
        assert!(
            logs.contains("SUBTITLE_LANGUAGE 'no' is Norwegian")
                && logs.contains("AUDIO_LANGUAGE 'NO' is Norwegian"),
            "{logs:?}"
        );
        assert!(
            logs.contains("'none'") && logs.contains("'nor'"),
            "{logs:?}"
        );
        // The 3-letter spellings are plain Norwegian, with no warning.
        let logs = captured_warnings(|| {
            assert_eq!(
                SubReq::parse(Some("nor".into())),
                SubReq::Lang("nor".into())
            );
            assert_eq!(
                AudioReq::parse(Some("nob".into())),
                AudioReq::Lang("nob".into())
            );
        });
        assert!(logs.is_empty(), "{logs:?}");
    }

    #[test]
    fn language_reqs_keep_na_as_nauruan_with_a_warning() {
        // `na` is Nauruan's ISO 639-1 code, so it is kept, but an operator may write it for "N/A";
        // taken silently, every release without a Nauruan track would be rejected and blacklisted.
        let logs = captured_warnings(|| {
            assert_eq!(SubReq::parse(Some("na".into())), SubReq::Lang("na".into()));
            assert_eq!(
                AudioReq::parse(Some("NA".into())),
                AudioReq::Lang("na".into())
            );
        });
        assert!(
            logs.contains("SUBTITLE_LANGUAGE 'na' is Nauruan")
                && logs.contains("AUDIO_LANGUAGE 'NA' is Nauruan"),
            "{logs:?}"
        );
        assert!(
            logs.contains("'none'") && logs.contains("'original'") && logs.contains("'nau'"),
            "{logs:?}"
        );
        // The 3-letter spelling is plain Nauruan, with no warning.
        let logs = captured_warnings(|| {
            assert_eq!(
                SubReq::parse(Some("nau".into())),
                SubReq::Lang("nau".into())
            );
            assert_eq!(
                AudioReq::parse(Some("nau".into())),
                AudioReq::Lang("nau".into())
            );
        });
        assert!(logs.is_empty(), "{logs:?}");
    }

    #[test]
    fn language_reqs_reject_non_iso_values_to_the_default() {
        // A full language name or a locale is not a 2-/3-letter ISO code, so it never matches the
        // codes `probe` extracts. Accepting it verbatim would silently penalise every candidate
        // and quietly stop acquisition — instead we fall back to the default (Original / skip).
        assert_eq!(
            AudioReq::parse(Some("english".into())),
            AudioReq::Original,
            "a full language name falls back to Original, not Lang(\"english\")"
        );
        assert_eq!(AudioReq::parse(Some("en-US".into())), AudioReq::Original);
        assert_eq!(AudioReq::parse(Some("e".into())), AudioReq::Original);
        assert_eq!(
            SubReq::parse(Some("english".into())),
            SubReq::None,
            "a full language name falls back to skip, not Lang(\"english\")"
        );
        assert_eq!(SubReq::parse(Some("pt-br".into())), SubReq::None);
        // Valid codes still pass through unchanged.
        assert_eq!(
            AudioReq::parse(Some("fra".into())),
            AudioReq::Lang("fra".into())
        );
        assert_eq!(SubReq::parse(Some("de".into())), SubReq::Lang("de".into()));
    }

    #[test]
    fn parse_bool_accepts_all_synonyms_and_falls_back() {
        // The single crate-wide bool parser backs DEDUP_REMOVE_DUPLICATES / REMOVE_FINISHED_SHOWS,
        // documented to accept true/1/yes/on (and false/0/no/off), case-insensitively.
        for t in ["true", "1", "yes", "on", "ON", "Yes"] {
            assert!(
                AcquisitionConfig::parse_bool(Some(t.into()), "X", false),
                "{t} should parse true"
            );
        }
        for f in ["false", "0", "no", "off", "OFF"] {
            assert!(
                !AcquisitionConfig::parse_bool(Some(f.into()), "X", true),
                "{f} should parse false"
            );
        }
        // An unrecognised value (or absence) returns the supplied default, both polarities.
        assert!(AcquisitionConfig::parse_bool(
            Some("garbage".into()),
            "X",
            true
        ));
        assert!(!AcquisitionConfig::parse_bool(
            Some("garbage".into()),
            "X",
            false
        ));
        assert!(AcquisitionConfig::parse_bool(None, "X", true));
        assert!(!AcquisitionConfig::parse_bool(None, "X", false));
        assert!(AcquisitionConfig::parse_bool(Some("  ".into()), "X", true));
    }

    /// Everything `warn!`ed (or worse) while `f` runs, on this thread.
    fn captured_warnings(f: impl FnOnce()) -> String {
        crate::test_log::capture(tracing::Level::WARN, f)
    }

    #[test]
    fn parse_bool_warns_on_an_unrecognised_value_but_not_on_blank_or_absent() {
        // A typo like `ALLOW_RESOLUTION_DOWNGRADE=ture` must be visible (as every numeric knob's is),
        // not silently read as the default while the operator believes the flag is on.
        let logs = captured_warnings(|| {
            assert!(!AcquisitionConfig::parse_bool(
                Some("ture".into()),
                "ALLOW_RESOLUTION_DOWNGRADE",
                false
            ));
        });
        assert!(
            logs.contains("Invalid ALLOW_RESOLUTION_DOWNGRADE value 'ture', falling back to false"),
            "an unrecognised value must warn, naming the variable: {logs:?}"
        );
        // Absent or blank (compose passes `${VAR:-}` empties) stays silent, as does a valid value.
        let logs = captured_warnings(|| {
            AcquisitionConfig::parse_bool(None, "PREFER_HDR", false);
            AcquisitionConfig::parse_bool(Some("  ".into()), "PREFER_HDR", false);
            AcquisitionConfig::parse_bool(Some("On".into()), "PREFER_HDR", false);
        });
        assert!(
            logs.is_empty(),
            "absent/blank/valid must not warn: {logs:?}"
        );
    }

    // ── TraktConfig tests ────────────────────────────────────────────────────

    fn trakt(
        id: Option<&str>,
        secret: Option<&str>,
        sync: Option<&str>,
        episode: Option<&str>,
    ) -> Option<TraktConfig> {
        TraktConfig::from_parts(
            id.map(String::from),
            secret.map(String::from),
            sync.map(String::from),
            episode.map(String::from),
            None, // catchup lookback: covered by its own tests
            None, // enrol allowed hosts: covered by its own tests
        )
    }

    #[test]
    fn parse_lookback_units_and_defaults() {
        assert_eq!(parse_lookback(""), None); // unset → all-time
        assert_eq!(parse_lookback("  "), None);
        assert_eq!(parse_lookback("0"), None); // explicit 0 → all-time
        assert_eq!(parse_lookback("90"), Some(90 * 86_400)); // bare → days
        assert_eq!(parse_lookback("90d"), Some(90 * 86_400));
        assert_eq!(parse_lookback("12w"), Some(12 * 7 * 86_400));
        assert_eq!(parse_lookback("6mo"), Some(6 * 30 * 86_400));
        assert_eq!(parse_lookback("3MO"), Some(3 * 30 * 86_400)); // case-insensitive
        assert_eq!(parse_lookback("garbage"), None); // unparseable → all-time

        // A bare unit with no number (an empty digit run) is unparseable → all-time, not a panic.
        assert_eq!(parse_lookback("d"), None);
        assert_eq!(parse_lookback("w"), None);
        assert_eq!(parse_lookback("mo"), None);
        // A negative number can't parse as u64 → all-time (never a startup failure).
        assert_eq!(parse_lookback("-5"), None);
        assert_eq!(parse_lookback("-5d"), None);
    }

    #[test]
    fn parse_lookback_accepts_the_common_unit_spellings() {
        // An operator who sets the lookback means to BOUND catch-up, and the all-time fallback is
        // the most expensive setting, so the natural spellings of a bounded window must parse.
        const DAY: u64 = 86_400;
        assert_eq!(parse_lookback("6m"), Some(6 * 30 * DAY)); // m = month, not minute
        assert_eq!(parse_lookback("6 months"), Some(6 * 30 * DAY));
        assert_eq!(parse_lookback("1month"), Some(30 * DAY));
        assert_eq!(parse_lookback("2mon"), Some(2 * 30 * DAY));
        assert_eq!(parse_lookback("1y"), Some(365 * DAY));
        assert_eq!(parse_lookback("2yr"), Some(2 * 365 * DAY));
        assert_eq!(parse_lookback("1 year"), Some(365 * DAY));
        assert_eq!(parse_lookback("2Years"), Some(2 * 365 * DAY));
        assert_eq!(parse_lookback("90days"), Some(90 * DAY));
        assert_eq!(parse_lookback("1 day"), Some(DAY));
        assert_eq!(parse_lookback("12 weeks"), Some(12 * 7 * DAY));
        assert_eq!(parse_lookback("3wk"), Some(3 * 7 * DAY));
        assert_eq!(parse_lookback("1week"), Some(7 * DAY));
        assert_eq!(parse_lookback(" 90 d "), Some(90 * DAY));
        assert_eq!(parse_lookback("0 months"), None); // 0 → all-time, whatever the unit

        // A unit that is still unknown keeps the documented warn-and-all-time fallback.
        assert_eq!(parse_lookback("6x"), None);
        assert_eq!(parse_lookback("6 fortnights"), None);
        assert_eq!(parse_lookback("months"), None);
        assert_eq!(parse_lookback("1.5y"), None);
    }

    #[test]
    fn parse_lookback_warning_lists_the_accepted_forms() {
        let logs = captured_warnings(|| {
            assert_eq!(parse_lookback("6x"), None);
        });
        assert!(
            logs.contains("Invalid TRAKT_CATCHUP_LOOKBACK value '6x'")
                && logs.contains("treating as all-time"),
            "an unknown unit must warn, naming the variable: {logs:?}"
        );
        for form in ["90d", "12w", "6mo", "1y"] {
            assert!(
                logs.contains(form),
                "the warning should list the accepted form {form}: {logs:?}"
            );
        }
    }

    #[test]
    fn trakt_catchup_lookback_parsed_into_config() {
        let t = TraktConfig::from_parts(
            Some("id".into()),
            Some("sec".into()),
            None,
            None,
            Some("6mo".into()),
            None,
        )
        .unwrap();
        assert_eq!(t.catchup_lookback_secs, Some(6 * 30 * 86_400));
        // unset → all-time (None)
        let t2 = trakt(Some("id"), Some("sec"), None, None).unwrap();
        assert_eq!(t2.catchup_lookback_secs, None);
    }

    #[test]
    fn trakt_enrol_allowed_hosts_parsed_into_config() {
        let t = TraktConfig::from_parts(
            Some("id".into()),
            Some("sec".into()),
            None,
            None,
            None,
            Some(" Media.Example.com , ,proxy.example.org. ".into()),
        )
        .unwrap();
        assert_eq!(
            t.enrol_allowed_hosts,
            vec![
                "media.example.com".to_string(),
                "proxy.example.org".to_string()
            ],
            "trimmed, lowercased, root dot dropped, blanks skipped"
        );
        // Unset → no extra hosts (the built-in local-network names still apply).
        let t2 = trakt(Some("id"), Some("sec"), None, None).unwrap();
        assert!(t2.enrol_allowed_hosts.is_empty());
    }

    #[test]
    fn parse_allowed_hosts_drops_entries_that_are_not_bare_hostnames() {
        // A URL, a port, a path or a space can never equal a Host header's hostname, so such an
        // entry is warned about and dropped rather than silently never matching.
        assert_eq!(
            parse_allowed_hosts(Some(
                "https://a.example, b.example:443, c example, d.example/x, ok.example".into()
            )),
            vec!["ok.example".to_string()]
        );
        assert!(parse_allowed_hosts(None).is_empty());
        assert!(parse_allowed_hosts(Some("  ".into())).is_empty());
    }

    #[test]
    fn trakt_both_present_uses_defaults() {
        let t = trakt(Some("id123"), Some("secret456"), None, None).unwrap();
        assert_eq!(t.client_id, "id123");
        assert_eq!(t.client_secret, "secret456");
        assert_eq!(t.sync_interval_secs, 900);
        assert_eq!(t.episode_check_interval_secs, 3600);
    }

    #[test]
    fn trakt_id_absent_gives_none() {
        assert!(trakt(None, Some("secret"), None, None).is_none());
    }

    #[test]
    fn trakt_secret_absent_gives_none() {
        assert!(trakt(Some("id"), None, None, None).is_none());
    }

    #[test]
    fn trakt_both_absent_gives_none() {
        assert!(trakt(None, None, None, None).is_none());
    }

    #[test]
    fn trakt_empty_id_gives_none() {
        assert!(trakt(Some("  "), Some("secret"), None, None).is_none());
    }

    #[test]
    fn trakt_empty_secret_gives_none() {
        assert!(trakt(Some("id"), Some(""), None, None).is_none());
    }

    #[test]
    fn trakt_half_configured_names_the_present_and_missing_variables() {
        assert_eq!(
            trakt_half_configured(Some("id"), None),
            Some(("TRAKT_CLIENT_ID", "TRAKT_CLIENT_SECRET"))
        );
        assert_eq!(
            trakt_half_configured(None, Some("secret")),
            Some(("TRAKT_CLIENT_SECRET", "TRAKT_CLIENT_ID"))
        );
        // Blank counts as unset (compose passes `${TRAKT_CLIENT_SECRET:-}` as an empty value).
        assert_eq!(
            trakt_half_configured(Some("id"), Some("  ")),
            Some(("TRAKT_CLIENT_ID", "TRAKT_CLIENT_SECRET"))
        );
        assert_eq!(trakt_half_configured(Some("id"), Some("secret")), None);
        assert_eq!(trakt_half_configured(None, None), None);
        assert_eq!(trakt_half_configured(Some(" "), Some("")), None);
    }

    #[test]
    fn trakt_half_configured_warns_and_stays_disabled() {
        // A missing or typo'd secret must be visible, not just the generic "Trakt sync disabled"
        // info line, while the operator believes Trakt is on.
        let logs = captured_warnings(|| {
            assert!(trakt(Some("id"), None, None, None).is_none());
        });
        assert!(
            logs.contains(
                "TRAKT_CLIENT_ID is set but TRAKT_CLIENT_SECRET is not; Trakt sync is disabled"
            ),
            "a half-configured Trakt must warn, naming both variables: {logs:?}"
        );
        let logs = captured_warnings(|| {
            assert!(trakt(None, Some("secret"), None, None).is_none());
        });
        assert!(
            logs.contains("TRAKT_CLIENT_SECRET is set but TRAKT_CLIENT_ID is not"),
            "{logs:?}"
        );
        // Neither set (or both blank, compose's default) is the normal Trakt-off deployment: silent.
        let logs = captured_warnings(|| {
            assert!(trakt(None, None, None, None).is_none());
            assert!(trakt(Some(""), Some("  "), None, None).is_none());
            assert!(trakt(Some("id"), Some("secret"), None, None).is_some());
        });
        assert!(logs.is_empty(), "must not warn: {logs:?}");
    }

    #[test]
    fn trakt_sync_interval_clamped() {
        // Below min → clamped to 60
        assert_eq!(
            trakt(Some("id"), Some("s"), Some("5"), None)
                .unwrap()
                .sync_interval_secs,
            60
        );
        // Exactly at min → kept as-is (boundary)
        assert_eq!(
            trakt(Some("id"), Some("s"), Some("60"), None)
                .unwrap()
                .sync_interval_secs,
            60
        );
        // Invalid → default 900
        assert_eq!(
            trakt(Some("id"), Some("s"), Some("abc"), None)
                .unwrap()
                .sync_interval_secs,
            900
        );
        // Valid above min → kept
        assert_eq!(
            trakt(Some("id"), Some("s"), Some("1200"), None)
                .unwrap()
                .sync_interval_secs,
            1200
        );
    }

    #[test]
    fn trakt_episode_check_interval_clamped() {
        // Below min → clamped to 300
        assert_eq!(
            trakt(Some("id"), Some("s"), None, Some("100"))
                .unwrap()
                .episode_check_interval_secs,
            300
        );
        // Exactly at min → kept as-is (boundary)
        assert_eq!(
            trakt(Some("id"), Some("s"), None, Some("300"))
                .unwrap()
                .episode_check_interval_secs,
            300
        );
        // Invalid → default 3600
        assert_eq!(
            trakt(Some("id"), Some("s"), None, Some("xyz"))
                .unwrap()
                .episode_check_interval_secs,
            3600
        );
        // Valid above min → kept
        assert_eq!(
            trakt(Some("id"), Some("s"), None, Some("7200"))
                .unwrap()
                .episode_check_interval_secs,
            7200
        );
    }

    #[test]
    fn config_from_parts_has_trakt_none() {
        let c = parts(Some("rd"), None, Some("tmdb"), None, None, None).unwrap();
        assert_eq!(c.trakt, None);
    }

    #[test]
    fn household_flags_map_each_env_name_to_its_own_field() {
        // Guards against a NAME SWAP that would silently enable destructive duplicate deletion when
        // the operator meant finished-show removal (or vice-versa). Pure lookup → no env mutation.
        let mut c = parts(Some("rd"), None, Some("tmdb"), None, None, None).unwrap();
        assert!(!c.dedup_remove_duplicates && !c.remove_finished_shows);
        // Only DEDUP set → only dedup flips.
        c.apply_household_flags(|n| (n == "DEDUP_REMOVE_DUPLICATES").then(|| "yes".to_string()));
        assert!(c.dedup_remove_duplicates, "DEDUP_REMOVE_DUPLICATES → dedup");
        assert!(
            !c.remove_finished_shows,
            "DEDUP must NOT set remove_finished_shows"
        );
        // Only REMOVE_FINISHED set → only finished flips (and dedup resets to false).
        c.apply_household_flags(|n| (n == "REMOVE_FINISHED_SHOWS").then(|| "1".to_string()));
        assert!(
            !c.dedup_remove_duplicates,
            "REMOVE_FINISHED must NOT set dedup"
        );
        assert!(
            c.remove_finished_shows,
            "REMOVE_FINISHED_SHOWS → remove_finished_shows"
        );
        // ALLOW_RESOLUTION_DOWNGRADE wires to allow_resolution_downgrade (and nothing else).
        c.apply_household_flags(|n| {
            (n == "ALLOW_RESOLUTION_DOWNGRADE").then(|| "true".to_string())
        });
        assert!(
            c.allow_resolution_downgrade,
            "ALLOW_RESOLUTION_DOWNGRADE → allow_resolution_downgrade"
        );
        assert!(
            !c.dedup_remove_duplicates && !c.remove_finished_shows,
            "ALLOW_RESOLUTION_DOWNGRADE must not enable the other household flags"
        );
        // Unset → all default false.
        c.apply_household_flags(|_| None);
        assert!(
            !c.dedup_remove_duplicates && !c.remove_finished_shows && !c.allow_resolution_downgrade
        );
    }

    #[test]
    fn upgrade_config_defaults_to_daily_and_clamps() {
        // Absent → daily default, all sub-defaults applied.
        let u = UpgradeConfig::from_parts(None, None, None);
        assert_eq!(u.interval_secs, 86_400);
        assert_eq!(
            u.budget_per_tick, None,
            "no budget by default: every pass covers the library"
        );
        assert_eq!(u.idle_secs, 300);
        assert!(u.enabled(), "default (daily) is enabled");

        // interval=0 disables the job.
        let off = UpgradeConfig::from_parts(Some("0".into()), None, None);
        assert_eq!(off.interval_secs, 0);
        assert!(!off.enabled());

        // Below-min interval (but non-zero) clamps up to 600; sub-knobs clamp to their mins.
        let clamped =
            UpgradeConfig::from_parts(Some("60".into()), Some("0".into()), Some("5".into()));
        assert_eq!(clamped.interval_secs, 600);
        assert_eq!(clamped.budget_per_tick, Some(1));
        assert_eq!(clamped.idle_secs, 30);

        // Invalid → defaults.
        let bad = UpgradeConfig::from_parts(Some("x".into()), Some("y".into()), Some("z".into()));
        assert_eq!(bad.interval_secs, 86_400);
        assert_eq!(bad.budget_per_tick, None);
        assert_eq!(bad.idle_secs, 300);

        // An explicit budget caps each pass; blank is unset.
        let capped = UpgradeConfig::from_parts(None, Some(" 400 ".into()), None);
        assert_eq!(capped.budget_per_tick, Some(400));
        let blank = UpgradeConfig::from_parts(None, Some("  ".into()), None);
        assert_eq!(blank.budget_per_tick, None);
    }

    #[test]
    fn upgrade_interval_reads_the_boolean_off_words_as_disabled() {
        // UPGRADE_INTERVAL_SECS is the only switch that stops the upgrade engine's swaps and prunes,
        // so `off`/`false`/`no` (what the boolean switches beside it take) must turn it off, not
        // fall back to the daily default with the engine still on.
        let logs = captured_warnings(|| {
            for off in ["off", " FALSE ", "No", "0", " 0 "] {
                let u = UpgradeConfig::from_parts(Some(off.into()), None, None);
                assert_eq!(u.interval_secs, 0, "{off:?}");
                assert!(!u.enabled(), "{off:?}");
            }
        });
        assert!(logs.is_empty(), "an off-word is not a typo: {logs:?}");

        // Any other non-number still keeps the daily default, and the warning says the engine is
        // on and how to turn it off.
        let logs = captured_warnings(|| {
            let u = UpgradeConfig::from_parts(Some("disable".into()), None, None);
            assert_eq!(u.interval_secs, 86_400);
            assert!(u.enabled());
        });
        assert!(
            logs.contains("Invalid UPGRADE_INTERVAL_SECS value 'disable'")
                && logs.contains("the upgrade engine is ON")
                && logs.contains("off"),
            "{logs:?}"
        );
    }

    #[test]
    fn acquisition_dead_timeout_defaults_to_600() {
        let a = AcquisitionConfig::from_parts(None, None, None, None, None, None, None);
        assert_eq!(a.acquire_dead_timeout_secs, 600);
    }

    #[test]
    fn config_from_parts_has_upgrade_default() {
        let c = parts(Some("rd"), None, Some("tmdb"), None, None, None).unwrap();
        assert_eq!(c.upgrade.interval_secs, 86_400);
    }

    #[test]
    fn config_from_parts_has_cdn_first_read_default() {
        let c = parts(Some("rd"), None, Some("tmdb"), None, None, None).unwrap();
        assert_eq!(c.cdn_first_read_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn cdn_first_read_parses_clamps_and_defaults() {
        let mb = |n: u64| n * 1024 * 1024;
        // Unset / blank / non-numeric → 8 MB default.
        assert_eq!(parse_cdn_first_read_bytes(None), mb(8));
        assert_eq!(parse_cdn_first_read_bytes(Some("   ".into())), mb(8));
        assert_eq!(parse_cdn_first_read_bytes(Some("garbage".into())), mb(8));
        // Valid value, with surrounding whitespace, in bytes.
        assert_eq!(parse_cdn_first_read_bytes(Some(" 12 ".into())), mb(12));
        // Clamped to [2, 16] MB.
        assert_eq!(parse_cdn_first_read_bytes(Some("0".into())), mb(2));
        assert_eq!(parse_cdn_first_read_bytes(Some("1".into())), mb(2));
        assert_eq!(parse_cdn_first_read_bytes(Some("999".into())), mb(16));
    }

    // The `dotenv_failure` tests build dotenvy's results without touching the process environment.
    // Never call `load_dotenv` from a unit test: it searches from the working directory up, so it
    // would load the developer's own `.env`, live tokens included.

    #[test]
    fn dotenv_failure_ignores_a_loaded_or_missing_file() {
        assert_eq!(dotenv_failure(&Ok(std::path::PathBuf::from(".env"))), None);
        // No .env at all (a container, or a native run configured by exported variables).
        let missing = dotenvy::Error::Io(std::io::ErrorKind::NotFound.into());
        assert_eq!(dotenv_failure(&Err(missing)), None);
    }

    #[test]
    fn dotenv_failure_reports_a_malformed_line_without_echoing_it() {
        // dotenvy stops at the first line it can't parse (an unquoted value with a space), so every
        // later setting would silently fall back to its default. Built through `from_read_iter`, which
        // parses without touching the process environment.
        let err = dotenvy::from_read_iter("RD_API_TOKEN=SECRET token".as_bytes())
            .next()
            .expect("one line")
            .expect_err("an unquoted space is a parse error");
        let msg = dotenv_failure(&Err(err)).expect("a malformed .env must be reported");
        assert!(msg.contains("quote"), "{msg}");
        // The offending text can be a token, so it is never repeated.
        assert!(!msg.contains("SECRET"), "{msg}");
    }

    #[test]
    fn dotenv_failure_reports_an_unreadable_file() {
        let denied = dotenvy::Error::Io(std::io::ErrorKind::PermissionDenied.into());
        let msg = dotenv_failure(&Err(denied)).expect("an unreadable .env must be reported");
        assert!(msg.contains(".env"), "{msg}");
    }

    /// A live test binary that loaded `.env` itself and discarded the error (`dotenvy::dotenv().ok()`)
    /// would, on a line dotenvy can't parse, read every setting after it as unset: a provider token
    /// there makes each live test skip and report `ok`, so the pre-commit gate passes having tested
    /// nothing, on a file the service refuses to start with. So they load it only through
    /// `tests/common`'s `load_env`, which panics on that error (`load_dotenv`). The library's own
    /// live tests (`#[ignore]`, e.g. identification's TMDB regressions) call `load_dotenv` directly,
    /// so their panic names the bad `.env` rather than a missing key.
    #[test]
    fn live_tests_load_env_only_through_load_dotenv() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let src_offenders: Vec<String> = std::fs::read_dir(&src)
            .expect("read src/")
            .map(|entry| entry.expect("src/ entry").path())
            // This file owns `load_dotenv`, the one place `.env` is loaded.
            .filter(|p| p.extension().is_some_and(|e| e == "rs") && !p.ends_with("config.rs"))
            .filter(|p| {
                std::fs::read_to_string(p)
                    .expect("read a library source")
                    .contains("dotenvy::dotenv")
            })
            .map(|p| p.display().to_string())
            .collect();
        assert!(
            src_offenders.is_empty(),
            "these load .env themselves; call `crate::config::load_dotenv()` instead: \
             {src_offenders:?}"
        );
        let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
        let mut files = vec![tests.join("common").join("mod.rs")];
        for entry in std::fs::read_dir(&tests).expect("read tests/") {
            let path = entry.expect("tests/ entry").path();
            if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
        assert!(
            files.len() > 5,
            "the scan should find the test binaries: {files:?}"
        );
        let offenders: Vec<String> = files
            .iter()
            .filter(|p| {
                std::fs::read_to_string(p)
                    .expect("read a test source")
                    .contains("dotenvy::dotenv")
            })
            .map(|p| p.display().to_string())
            .collect();
        assert!(
            offenders.is_empty(),
            "these load .env themselves; call `common::load_env()` (or `common::token`) instead: \
             {offenders:?}"
        );
        let common = std::fs::read_to_string(&files[0]).expect("read tests/common/mod.rs");
        assert!(
            common.contains("config::load_dotenv"),
            "tests/common must load .env through config::load_dotenv"
        );
    }

    /// In the container the app reads no `.env` (its working directory is `/data`), so compose.yml's
    /// `environment:` list is the only way a setting reaches it. Every variable the service reads
    /// must be there: a pass-through (`- NAME=${NAME:-}`; empty means the documented default) or,
    /// for the stack's fixed paths, a literal. A knob left out (or commented out) is silently
    /// ignored when set in `.env`, as `UPGRADE_INTERVAL_SECS=0` once was. PORT is the exception: it
    /// sets only the published host port and must not reach the container.
    #[test]
    fn compose_passes_every_setting_through_to_the_container() {
        let compose = include_str!("../compose.yml");
        let service = compose
            .split("\n  rclone:")
            .next()
            .expect("compose.yml lists the debridmoviemapper service first");
        let sources = [
            include_str!("config.rs"),
            include_str!("main.rs"),
            include_str!("jellyfin_client.rs"),
        ];
        let reads = regex::Regex::new(r#"(?:env::var|flag)\("([A-Z][A-Z0-9_]*)"\)"#).unwrap();
        let mut names: Vec<&str> = sources
            .iter()
            .flat_map(|src| reads.captures_iter(src).map(|c| c.get(1).unwrap().as_str()))
            .collect();
        names.sort_unstable();
        names.dedup();
        assert!(
            [
                "UPGRADE_INTERVAL_SECS",
                "DEDUP_REMOVE_DUPLICATES",
                "RUST_LOG",
                "JELLYFIN_URL"
            ]
            .iter()
            .all(|n| names.contains(n)),
            "the scan should find the settings the service reads: {names:?}"
        );

        let entries: Vec<&str> = service
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("- "))
            .collect();
        let listed = |name: &str| entries.iter().any(|l| l.starts_with(&format!("- {name}=")));
        let passed_through = |name: &str| {
            entries
                .iter()
                .any(|l| l.starts_with(&format!("- {name}=${{{name}:")))
        };
        const FIXED: [&str; 3] = ["DB_PATH", "JELLYFIN_URL", "JELLYFIN_RCLONE_MOUNT_PATH"];
        let mut missing = Vec::new();
        for name in names {
            match name {
                "PORT" => assert!(
                    !listed("PORT"),
                    "PORT must not reach the container: it listens on 8080 for rclone and the \
                     healthcheck, and PORT sets only the published host port"
                ),
                n if FIXED.contains(&n) => {
                    if !listed(n) {
                        missing.push(n);
                    }
                }
                n => {
                    if !passed_through(n) {
                        missing.push(n);
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "compose.yml doesn't pass these settings through to the container, so a value set in \
             .env is ignored; add `- NAME=${{NAME:-}}` for each: {missing:?}"
        );
    }
}
