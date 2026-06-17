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
    /// Parse "720"/"1080"/"2160"/"4k" into a known ceiling; `None` if unrecognised.
    pub fn try_parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "720" | "720p" => Some(MaxResolution::P720),
            "1080" | "1080p" => Some(MaxResolution::P1080),
            "2160" | "2160p" | "4k" | "uhd" => Some(MaxResolution::P2160),
            _ => None,
        }
    }
    /// Parse "720"/"1080"/"2160"/"4k"; anything else → default 1080p.
    pub fn parse(s: &str) -> Self {
        Self::try_parse(s).unwrap_or(MaxResolution::P1080)
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

/// Trakt sync configuration (SP2). Held by `Config` as `Option<TraktConfig>`.
///
/// Present only when both `TRAKT_CLIENT_ID` and `TRAKT_CLIENT_SECRET` are set;
/// absent means Trakt sync is disabled and the service runs as before.
#[derive(Clone, PartialEq, Eq)]
pub struct TraktConfig {
    pub client_id: String,
    pub client_secret: String,
    /// How often (seconds) to sync the Trakt watched-set. Default 900, min 60.
    pub sync_interval_secs: u64,
    /// How often (seconds) to check for new episodes of tracked shows. Default 3600, min 300.
    pub episode_check_interval_secs: u64,
    /// Catch-up lookback window (seconds): a show you've watched is auto-acquired (to catch up on
    /// episodes aired since) only if you last watched it within this many seconds. `None` = all-time
    /// (the default) — every show you've ever watched with unwatched aired episodes qualifies.
    /// Parsed from `TRAKT_CATCHUP_LOOKBACK` (e.g. `90d`, `12w`, `6mo`; `0`/unset = all-time).
    pub catchup_lookback_secs: Option<u64>,
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
            .finish()
    }
}

/// Parse a catch-up lookback like `90d`, `12w`, `6mo` (or a bare number = days) into seconds.
/// Empty / unset / `0` → `None` (all-time, no limit). An unparseable value warns and is treated as
/// all-time rather than failing startup.
pub fn parse_lookback(raw: &str) -> Option<u64> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let (num, unit_secs): (&str, u64) = if let Some(n) = s.strip_suffix("mo") {
        (n, 30 * 86_400)
    } else if let Some(n) = s.strip_suffix('w') {
        (n, 7 * 86_400)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400)
    } else {
        (s.as_str(), 86_400) // bare number → days
    };
    match num.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(n) => Some(n.saturating_mul(unit_secs)),
        Err(_) => {
            warn!("Invalid TRAKT_CATCHUP_LOOKBACK value '{raw}'; treating as all-time");
            None
        }
    }
}

impl TraktConfig {
    /// Pure construction from raw optional values (env-independent, for tests).
    ///
    /// Returns `None` unless both `client_id` and `client_secret` are present and
    /// non-empty after trimming. Interval values default and clamp as documented.
    pub fn from_parts(
        client_id: Option<String>,
        client_secret: Option<String>,
        sync_interval_secs: Option<String>,
        episode_check_interval_secs: Option<String>,
        catchup_lookback: Option<String>,
    ) -> Option<TraktConfig> {
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

        let sync_interval_secs = match sync_interval_secs
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            Some(s) => s.parse::<u64>().unwrap_or_else(|_| {
                warn!(
                    "Invalid TRAKT_SYNC_INTERVAL_SECS value '{}', falling back to {}",
                    s, DEFAULT_SYNC
                );
                DEFAULT_SYNC
            }),
            None => DEFAULT_SYNC,
        }
        .max(MIN_SYNC);

        let episode_check_interval_secs = match episode_check_interval_secs
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            Some(s) => s.parse::<u64>().unwrap_or_else(|_| {
                warn!(
                    "Invalid TRAKT_EPISODE_CHECK_INTERVAL_SECS value '{}', falling back to {}",
                    s, DEFAULT_EPISODE
                );
                DEFAULT_EPISODE
            }),
            None => DEFAULT_EPISODE,
        }
        .max(MIN_EPISODE);

        let catchup_lookback_secs = catchup_lookback.as_deref().and_then(parse_lookback);

        Some(TraktConfig {
            client_id,
            client_secret,
            sync_interval_secs,
            episode_check_interval_secs,
            catchup_lookback_secs,
        })
    }

    /// Read `TRAKT_CLIENT_ID`, `TRAKT_CLIENT_SECRET`, `TRAKT_SYNC_INTERVAL_SECS`,
    /// `TRAKT_EPISODE_CHECK_INTERVAL_SECS`, and `TRAKT_CATCHUP_LOOKBACK` from the process
    /// environment, then delegate to `from_parts`.
    pub fn from_env() -> Option<TraktConfig> {
        Self::from_parts(
            std::env::var("TRAKT_CLIENT_ID").ok(),
            std::env::var("TRAKT_CLIENT_SECRET").ok(),
            std::env::var("TRAKT_SYNC_INTERVAL_SECS").ok(),
            std::env::var("TRAKT_EPISODE_CHECK_INTERVAL_SECS").ok(),
            std::env::var("TRAKT_CATCHUP_LOOKBACK").ok(),
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
            .finish()
    }
}

impl Default for AcquisitionConfig {
    fn default() -> Self {
        Self::from_parts(None, None, None, None, None, None, None)
    }
}

impl AcquisitionConfig {
    /// The single boolean-env parser for the whole crate (see `Config::from_env`'s `env_flag`,
    /// which delegates here). Accepts `true`/`1`/`yes`/`on` and `false`/`0`/`no`/`off`
    /// (case-insensitive); anything unrecognised (or absent) returns `default`.
    pub(crate) fn parse_bool(s: Option<String>, default: bool) -> bool {
        match s.map(|s| s.trim().to_ascii_lowercase()) {
            Some(v) if v == "true" || v == "1" || v == "yes" || v == "on" => true,
            Some(v) if v == "false" || v == "0" || v == "no" || v == "off" => false,
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
                // A non-empty but unrecognised MAX_RESOLUTION warns (consistent with the other
                // numeric knobs) so a typo like `4` or `2160i` isn't silently treated as 1080p;
                // unset/empty defaults quietly to 1080p.
                max_resolution: match max_resolution {
                    Some(s) if !s.trim().is_empty() => {
                        MaxResolution::try_parse(&s).unwrap_or_else(|| {
                            warn!(
                                "Invalid MAX_RESOLUTION value '{}', falling back to 1080p \
                                 (expected 720/1080/2160/4k)",
                                s
                            );
                            MaxResolution::P1080
                        })
                    }
                    _ => MaxResolution::P1080,
                },
                audio: AudioReq::parse(audio_language),
                subtitle: SubReq::parse(subtitle_language),
                prefer_hevc: Self::parse_bool(prefer_hevc, true),
                prefer_hdr: Self::parse_bool(prefer_hdr, false),
            },
            // Clamp to a sane floor: STALL_TIMEOUT_SECS=0 would declare every Pending torrent
            // stalled instantly, and MAX_ACQUIRE_ATTEMPTS=0 would make the engine never retry —
            // both footguns, consistent with the other interval knobs which all clamp.
            stall_timeout_secs: match stall_timeout_secs
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                Some(s) => s
                    .parse::<u64>()
                    .unwrap_or_else(|_| {
                        warn!(
                            "Invalid STALL_TIMEOUT_SECS value '{}', falling back to 1800",
                            s
                        );
                        1800
                    })
                    .max(60),
                None => 1800,
            },
            max_acquire_attempts: match max_acquire_attempts
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                Some(s) => s
                    .parse::<u32>()
                    .unwrap_or_else(|_| {
                        warn!(
                            "Invalid MAX_ACQUIRE_ATTEMPTS value '{}', falling back to 5",
                            s
                        );
                        5
                    })
                    .max(1),
                None => 5,
            },
            scraper_addon_url: None,
            scraper_proxy_url: None,
            acquire_dead_timeout_secs: 600,
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
        a
    }

    /// Parse `ACQUIRE_DEAD_TIMEOUT_SECS`: unset/blank → 600, an unparseable value warns + falls back
    /// to 600 (matching the other interval knobs — a typo like `10m` must be visible, not silently
    /// treated as unset), and any value is clamped up to the 120s floor. Pure for unit-testability.
    fn parse_dead_timeout(raw: Option<String>) -> u64 {
        match raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            Some(s) => s
                .parse::<u64>()
                .unwrap_or_else(|_| {
                    warn!(
                        "Invalid ACQUIRE_DEAD_TIMEOUT_SECS value '{}', falling back to 600",
                        s
                    );
                    600
                })
                .max(120),
            None => 600,
        }
    }
}

/// Upgrade-engine configuration (SP3). Held by `Config`. The job is spawned only when
/// `interval_secs > 0` (default 86_400 = daily; set `UPGRADE_INTERVAL_SECS=0` to disable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeConfig {
    pub interval_secs: u64,
    pub budget_per_tick: u32,
    pub idle_secs: u64,
    pub stage_max_secs: u64,
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self::from_parts(None, None, None, None)
    }
}

impl UpgradeConfig {
    /// `true` when the upgrade job should run (interval non-zero).
    pub fn enabled(&self) -> bool {
        self.interval_secs > 0
    }

    /// Pure construction from raw optional values (env-independent, for tests).
    /// `interval_secs`: 0 disables; otherwise clamped to a 600s minimum. Invalid → 86_400.
    pub fn from_parts(
        interval_secs: Option<String>,
        budget_per_tick: Option<String>,
        idle_secs: Option<String>,
        stage_max_secs: Option<String>,
    ) -> Self {
        fn num(v: Option<String>, default: u64, min: u64, zero_disables: bool, name: &str) -> u64 {
            // Treat unset OR blank as "use the default" (consistent with how blank string knobs are
            // treated as unset) — only a non-blank unparseable value warns.
            match v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
                Some(s) => match s.parse::<u64>() {
                    Ok(0) if zero_disables => 0,
                    Ok(n) => n.max(min),
                    Err(_) => {
                        warn!(
                            "Invalid {} value '{}', falling back to {}",
                            name, s, default
                        );
                        default
                    }
                },
                None => default,
            }
        }
        UpgradeConfig {
            interval_secs: num(interval_secs, 86_400, 600, true, "UPGRADE_INTERVAL_SECS"),
            budget_per_tick: num(budget_per_tick, 20, 1, false, "UPGRADE_BUDGET_PER_TICK")
                .min(u32::MAX as u64) as u32,
            idle_secs: num(idle_secs, 300, 30, false, "UPGRADE_IDLE_SECS"),
            stage_max_secs: num(
                stage_max_secs,
                604_800,
                3600,
                false,
                "UPGRADE_STAGE_MAX_SECS",
            ),
        }
    }

    /// Read `UPGRADE_INTERVAL_SECS`, `UPGRADE_BUDGET_PER_TICK`, `UPGRADE_IDLE_SECS`,
    /// and `UPGRADE_STAGE_MAX_SECS` from the process environment, then delegate to `from_parts`.
    pub fn from_env() -> Self {
        Self::from_parts(
            std::env::var("UPGRADE_INTERVAL_SECS").ok(),
            std::env::var("UPGRADE_BUDGET_PER_TICK").ok(),
            std::env::var("UPGRADE_IDLE_SECS").ok(),
            std::env::var("UPGRADE_STAGE_MAX_SECS").ok(),
        )
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
            .finish()
    }
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
        cfg.trakt = TraktConfig::from_env();
        cfg.upgrade = UpgradeConfig::from_env();
        // Delegate to the single crate-wide boolean parser so every flag accepts the same set
        // (`1`/`true`/`yes`/`on`), defaulting to false when unset/unrecognised.
        fn env_flag(name: &str) -> bool {
            AcquisitionConfig::parse_bool(std::env::var(name).ok(), false)
        }
        cfg.dedup_remove_duplicates = env_flag("DEDUP_REMOVE_DUPLICATES");
        cfg.remove_finished_shows = env_flag("REMOVE_FINISHED_SHOWS");
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

        // Reject a blank key (whitespace-only) rather than deferring the failure to TmdbClient,
        // mirroring how `choose_provider` treats a whitespace-only provider token as unset.
        let tmdb_api_key = tmdb_api_key
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::Config("TMDB_API_KEY must be set".to_string()))?;

        // Treat unset OR blank as the default (consistent with `db_path` below and the string knobs).
        let scan_interval_secs = match scan_interval_secs
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            Some(s) => s.parse::<u64>().unwrap_or_else(|_| {
                warn!(
                    "Invalid SCAN_INTERVAL_SECS value '{}', falling back to 60",
                    s
                );
                60
            }),
            None => 60,
        }
        .max(10); // Enforce minimum 10s to avoid hammering the provider API.

        // Trim + treat blank as unset (consistent with the other env knobs — a quoted/whitespace
        // `DB_PATH` from compose YAML, or an empty `DB_PATH=`, falls back to the default rather than
        // becoming a whitespace/empty path).
        let db_path = db_path
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "metadata.db".to_string());

        let port = match port.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            // Reject 0 alongside unparseable values: port 0 binds a random ephemeral port (nothing
            // is configured for it) and makes `--healthcheck` connect to 127.0.0.1:0 → fail →
            // container restart loop. Fall back to 8080 with the same warn.
            Some(s) => s
                .parse::<u16>()
                .ok()
                .filter(|&p| p != 0)
                .unwrap_or_else(|| {
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
            trakt: None,
            upgrade: UpgradeConfig::default(),
            dedup_remove_duplicates: false,
            remove_finished_shows: false,
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
        // 127.0.0.1:0 and fails → restart loop). Treat it like the unparseable case → default 8080.
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
    }

    #[test]
    fn parse_bool_accepts_all_synonyms_and_falls_back() {
        // The single crate-wide bool parser backs DEDUP_REMOVE_DUPLICATES / REMOVE_FINISHED_SHOWS,
        // documented to accept true/1/yes/on (and false/0/no/off), case-insensitively.
        for t in ["true", "1", "yes", "on", "ON", "Yes"] {
            assert!(
                AcquisitionConfig::parse_bool(Some(t.into()), false),
                "{t} should parse true"
            );
        }
        for f in ["false", "0", "no", "off", "OFF"] {
            assert!(
                !AcquisitionConfig::parse_bool(Some(f.into()), true),
                "{f} should parse false"
            );
        }
        // An unrecognised value (or absence) returns the supplied default, both polarities.
        assert!(AcquisitionConfig::parse_bool(Some("garbage".into()), true));
        assert!(!AcquisitionConfig::parse_bool(
            Some("garbage".into()),
            false
        ));
        assert!(AcquisitionConfig::parse_bool(None, true));
        assert!(!AcquisitionConfig::parse_bool(None, false));
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
    }

    #[test]
    fn trakt_catchup_lookback_parsed_into_config() {
        let t = TraktConfig::from_parts(
            Some("id".into()),
            Some("sec".into()),
            None,
            None,
            Some("6mo".into()),
        )
        .unwrap();
        assert_eq!(t.catchup_lookback_secs, Some(6 * 30 * 86_400));
        // unset → all-time (None)
        let t2 = trakt(Some("id"), Some("sec"), None, None).unwrap();
        assert_eq!(t2.catchup_lookback_secs, None);
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
    fn upgrade_config_defaults_to_daily_and_clamps() {
        // Absent → daily default, all sub-defaults applied.
        let u = UpgradeConfig::from_parts(None, None, None, None);
        assert_eq!(u.interval_secs, 86_400);
        assert_eq!(u.budget_per_tick, 20);
        assert_eq!(u.idle_secs, 300);
        assert_eq!(u.stage_max_secs, 604_800);
        assert!(u.enabled(), "default (daily) is enabled");

        // interval=0 disables the job.
        let off = UpgradeConfig::from_parts(Some("0".into()), None, None, None);
        assert_eq!(off.interval_secs, 0);
        assert!(!off.enabled());

        // Below-min interval (but non-zero) clamps up to 600; sub-knobs clamp to their mins.
        let clamped = UpgradeConfig::from_parts(
            Some("60".into()),
            Some("0".into()),
            Some("5".into()),
            Some("10".into()),
        );
        assert_eq!(clamped.interval_secs, 600);
        assert_eq!(clamped.budget_per_tick, 1);
        assert_eq!(clamped.idle_secs, 30);
        assert_eq!(clamped.stage_max_secs, 3600);

        // Invalid → defaults.
        let bad = UpgradeConfig::from_parts(Some("x".into()), Some("y".into()), None, None);
        assert_eq!(bad.interval_secs, 86_400);
        assert_eq!(bad.budget_per_tick, 20);
    }

    #[test]
    fn acquisition_dead_timeout_defaults_to_600() {
        // The env-override path (ACQUIRE_DEAD_TIMEOUT_SECS) is exercised via from_env,
        // which is not unit-testable here without mutating the process environment.
        let a = AcquisitionConfig::from_parts(None, None, None, None, None, None, None);
        assert_eq!(a.acquire_dead_timeout_secs, 600);
    }

    #[test]
    fn config_from_parts_has_upgrade_default() {
        let c = parts(Some("rd"), None, Some("tmdb"), None, None, None).unwrap();
        assert_eq!(c.upgrade.interval_secs, 86_400);
    }
}
