use crate::error::AppError;
use crate::provider::{choose_provider, ProviderKind};
use tracing::warn;

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
    /// Parse "720"/"1080"/"2160"/"4k"; anything else → default 1080p.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "720" | "720p" => MaxResolution::P720,
            "1080" | "1080p" => MaxResolution::P1080,
            "2160" | "2160p" | "4k" | "uhd" => MaxResolution::P2160,
            _ => MaxResolution::P1080,
        }
    }
}

/// Required audio language: a specific ISO code, or the title's original language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioReq {
    Lang(String),
    Original,
}

impl AudioReq {
    /// `None`/empty/"original" → Original; otherwise the given language code.
    pub fn parse(s: Option<String>) -> Self {
        match s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            None => AudioReq::Original,
            Some(v) if v.eq_ignore_ascii_case("original") => AudioReq::Original,
            Some(v) => AudioReq::Lang(v.to_ascii_lowercase()),
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
    /// `None`/empty/"none" → None (skip); otherwise the given language code.
    pub fn parse(s: Option<String>) -> Self {
        match s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            None => SubReq::None,
            Some(v) if v.eq_ignore_ascii_case("none") => SubReq::None,
            Some(v) => SubReq::Lang(v.to_ascii_lowercase()),
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

/// Acquisition-engine configuration (SP1). Held by `Config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionConfig {
    pub prefs: QualityPrefs,
    pub stall_timeout_secs: u64,
    pub max_acquire_attempts: u32,
    /// Override for the scraper base URL; `None` → template Torrentio from the active provider.
    pub scraper_addon_url: Option<String>,
}

impl Default for AcquisitionConfig {
    fn default() -> Self {
        Self::from_parts(None, None, None, None, None, None, None)
    }
}

impl AcquisitionConfig {
    fn parse_bool(s: Option<String>, default: bool) -> bool {
        match s.map(|s| s.trim().to_ascii_lowercase()) {
            Some(v) if v == "true" || v == "1" || v == "yes" => true,
            Some(v) if v == "false" || v == "0" || v == "no" => false,
            _ => default,
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
                max_resolution: max_resolution
                    .map(|s| MaxResolution::parse(&s))
                    .unwrap_or(MaxResolution::P1080),
                audio: AudioReq::parse(audio_language),
                subtitle: SubReq::parse(subtitle_language),
                prefer_hevc: Self::parse_bool(prefer_hevc, true),
                prefer_hdr: Self::parse_bool(prefer_hdr, false),
            },
            stall_timeout_secs: match stall_timeout_secs {
                Some(s) => s.trim().parse().unwrap_or_else(|_| {
                    warn!("Invalid STALL_TIMEOUT_SECS value '{}', falling back to 1800", s);
                    1800
                }),
                None => 1800,
            },
            max_acquire_attempts: match max_acquire_attempts {
                Some(s) => s.trim().parse().unwrap_or_else(|_| {
                    warn!("Invalid MAX_ACQUIRE_ATTEMPTS value '{}', falling back to 5", s);
                    5
                }),
                None => 5,
            },
            scraper_addon_url: None,
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
        a
    }
}

/// Startup configuration parsed from environment variables.
///
/// These values are fixed at startup. A future DB-backed override layer (web-UI
/// settings, SP4) will supply runtime-tunable *preferences* alongside this; the
/// startup values here (tokens, paths, port) are not runtime-overridable, so they
/// are plain fields rather than accessors.
#[derive(Debug, Clone)]
pub struct Config {
    pub provider_kind: ProviderKind,
    pub provider_token: String,
    pub tmdb_api_key: String,
    pub scan_interval_secs: u64,
    pub db_path: String,
    pub port: u16,
    pub acquisition: AcquisitionConfig,
}

impl Config {
    /// Build from the process environment (reads the same variables as before).
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
        Ok(cfg)
    }

    /// Pure construction from raw optional values — unit-testable without touching
    /// the process environment. Mirrors the previous inline logic exactly.
    pub fn from_parts(
        rd_token: Option<String>,
        torbox_token: Option<String>,
        tmdb_api_key: Option<String>,
        scan_interval_secs: Option<String>,
        db_path: Option<String>,
        port: Option<String>,
    ) -> Result<Self, AppError> {
        let (provider_kind, provider_token) = choose_provider(rd_token, torbox_token)?;

        let tmdb_api_key = tmdb_api_key
            .ok_or_else(|| AppError::Config("TMDB_API_KEY must be set".to_string()))?
            .trim()
            .to_string();

        let scan_interval_secs = match scan_interval_secs {
            Some(s) => s.parse::<u64>().unwrap_or_else(|_| {
                warn!("Invalid SCAN_INTERVAL_SECS value '{}', falling back to 60", s);
                60
            }),
            None => 60,
        }
        .max(10); // Enforce minimum 10s to avoid hammering the provider API.

        let db_path = db_path.unwrap_or_else(|| "metadata.db".to_string());

        let port = match port {
            Some(s) => s.parse::<u16>().unwrap_or_else(|_| {
                warn!("Invalid PORT value '{}', falling back to 8080", s);
                8080
            }),
            None => 8080,
        };

        Ok(Self {
            provider_kind,
            provider_token,
            tmdb_api_key,
            scan_interval_secs,
            db_path,
            port,
            acquisition: AcquisitionConfig::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn scan_interval_clamped_and_parsed() {
        assert_eq!(
            parts(Some("rd"), None, Some("t"), Some("5"), None, None).unwrap().scan_interval_secs,
            10
        );
        assert_eq!(
            parts(Some("rd"), None, Some("t"), Some("abc"), None, None).unwrap().scan_interval_secs,
            60
        );
        assert_eq!(
            parts(Some("rd"), None, Some("t"), Some("120"), None, None).unwrap().scan_interval_secs,
            120
        );
    }

    #[test]
    fn port_parsed_with_fallback() {
        assert_eq!(parts(Some("rd"), None, Some("t"), None, None, Some("9000")).unwrap().port, 9000);
        assert_eq!(parts(Some("rd"), None, Some("t"), None, None, Some("nope")).unwrap().port, 8080);
    }

    #[test]
    fn db_path_override() {
        assert_eq!(
            parts(Some("rd"), None, Some("t"), None, Some("/data/x.db"), None).unwrap().db_path,
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
    fn max_resolution_parse_and_invalid_falls_back() {
        assert_eq!(MaxResolution::parse("720"), MaxResolution::P720);
        assert_eq!(MaxResolution::parse("1080"), MaxResolution::P1080);
        assert_eq!(MaxResolution::parse("2160"), MaxResolution::P2160);
        assert_eq!(MaxResolution::parse("4k"), MaxResolution::P2160);
        assert_eq!(MaxResolution::parse("garbage"), MaxResolution::P1080);
    }

    #[test]
    fn subtitle_none_keyword_means_skip() {
        assert_eq!(SubReq::parse(None), SubReq::None);
        assert_eq!(SubReq::parse(Some("none".into())), SubReq::None);
        assert_eq!(SubReq::parse(Some("eng".into())), SubReq::Lang("eng".into()));
    }
}
