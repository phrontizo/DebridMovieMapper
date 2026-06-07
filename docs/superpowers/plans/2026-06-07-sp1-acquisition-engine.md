# SP1 — Acquisition Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the acquisition engine — given a title id (+ optional S/E) and quality prefs, scrape a Stremio addon, score candidates, add the best to the debrid account, validate its identity + audio/subtitle, and fall back on failure.

**Architecture:** New modules `scraper`/`release`/`probe`/`reacquire`/`acquire` on top of SP0's `Config`/`Store`/`AppState`. Repair's add/select logic is extracted into a shared `reacquire::materialise` primitive (repair keeps its semantics). Three new redb tables (`owned_hashes`, `authoritative_ids`, `blacklist`) via a v1→v2 store migration. An authoritative `hash→MediaMetadata` override is consulted in the scan loop before filename identification. Acquisition is driven in SP1 only by tests and a temporary `--acquire` CLI trigger; nothing auto-acquires.

**Tech Stack:** Rust, tokio, reqwest, redb, serde_json, regex, thiserror, tracing. No new dependencies (hand-rolled container parsing).

**Branch:** `sp1-acquisition-engine` (off `main`, which has SP0).

**Spec:** `docs/superpowers/specs/2026-06-06-sp1-acquisition-engine-design.md` (decisions S1–S9; §3–§13).

---

## Conventions for every task

- **TDD:** write the failing test(s) first, run to confirm failure, implement minimally, run to confirm pass, commit.
- **Do not pipe `cargo` to `tail`/`head`** — the environment blocks them; run plainly.
- **Known flaky test:** `dav_fs::provider_abstraction_tests::fetch_cdn_range_recovers_from_expired_url` fails occasionally under parallel runs and passes in isolation; it is unrelated to SP1. If ONLY it fails, re-run `cargo test` (or `cargo test --lib fetch_cdn_range_recovers_from_expired_url`).
- **Commit message trailer (every commit):** `-m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"`.
- The full existing suite must stay green after every task; repair behaviour must be preserved.

---

## File structure

| File | Change | Responsibility |
|------|--------|----------------|
| `src/config.rs` | Modify | Add quality-preference types (`QualityPrefs`, `AudioReq`, `SubReq`, `MaxResolution`) + an `AcquisitionConfig` sub-struct (prefs + `stall_timeout_secs`, `max_acquire_attempts`, `scraper_addon_url`); parse from env. |
| `src/tmdb_client.rs` | Modify | Add IMDB↔TMDB lookups (`find_by_imdb`, `external_imdb_id`) for the `--acquire` trigger. |
| `src/release.rs` | **Create** | `RawCandidate`→`ReleaseInfo` parse; `score()`/ranking (ceiling hard; cached-first + soft weights; layer-1 language down-rank). |
| `src/scraper.rs` | **Create** | `Scraper` trait + `TorrentioScraper` (URL templating / override) + `MockScraper`. |
| `src/probe.rs` | **Create** | Hand-rolled MKV(EBML)+MP4(ISO-BMFF) track-language extraction over a ranged HTTP reader; `verify()` + failure taxonomy. |
| `src/store.rs` | Modify | Bump `SCHEMA_VERSION` 1→2; add `owned_hashes`/`authoritative_ids`/`blacklist` tables + typed accessors. |
| `src/reacquire.rs` | **Create** | Shared `materialise(provider, hash, select)` primitive extracted from repair. |
| `src/repair.rs` | Modify | Reimplement `add_and_select_files` on `reacquire::materialise` (behaviour unchanged). |
| `src/acquire.rs` | **Create** | `AcquisitionEngine`: `acquire()` flow + `observe()`. |
| `src/tasks.rs` | Modify | Consult `store.authoritative_meta` before `identify_torrent`; call `engine.observe()` each scan tick. |
| `src/app_state.rs` | Modify | Add `scraper: Arc<dyn Scraper>` + `engine: Arc<AcquisitionEngine>` handles. |
| `src/main.rs` | Modify | Build scraper/engine into `AppState`; add the temporary `--acquire` CLI mode. |
| `src/mapper.rs` | Modify | Declare the new modules. |
| `tests/sp1_live_test.rs` | **Create** | `#[ignore]` live smoke (scraper, probe); lifecycle extension lives in `tests/lifecycle_test.rs`. |
| `tests/lifecycle_test.rs` | Modify | Add the cross-provider "acquire Sintel by IMDB id" extension. |
| `CLAUDE.md`, `README.md` | Modify | Document the engine, new modules, env vars. |

**Task order is bottom-up** so each task compiles and tests green on its own: Config types → tmdb lookups → release → scraper → probe → store tables → reacquire+repair → acquire engine → wiring (tasks/app_state) → `--acquire` trigger → live tests + docs.

---

## Task 1: Quality-preference types + `AcquisitionConfig`

**Files:**
- Modify: `src/config.rs`
- Modify: `src/mapper.rs` (no change needed; config already declared)

- [ ] **Step 1: Write failing tests** — append to the `#[cfg(test)] mod tests` in `src/config.rs`:

```rust
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
        // scraper_addon_url is parsed separately via from_env; here defaults to None.
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
        assert_eq!(MaxResolution::parse("garbage"), MaxResolution::P1080); // default
    }

    #[test]
    fn subtitle_none_keyword_means_skip() {
        assert_eq!(SubReq::parse(None), SubReq::None);
        assert_eq!(SubReq::parse(Some("none".into())), SubReq::None);
        assert_eq!(SubReq::parse(Some("eng".into())), SubReq::Lang("eng".into()));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib config`
Expected: FAIL to compile — `AcquisitionConfig`/`MaxResolution`/`AudioReq`/`SubReq` undefined.

- [ ] **Step 3: Add the types** — insert near the top of `src/config.rs` (after the existing `use` lines):

```rust
/// Hard resolution ceiling. Ordered so `as u16` gives the pixel height for comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
            stall_timeout_secs: stall_timeout_secs
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(1800),
            max_acquire_attempts: max_acquire_attempts
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(5),
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
```

- [ ] **Step 4: Add the field to `Config`** — in `src/config.rs`, add `pub acquisition: AcquisitionConfig,` to the `Config` struct; in `from_parts`, set `acquisition: AcquisitionConfig::default(),` in the returned `Self { .. }`; in `from_env`, after building via `from_parts`, set the field. Concretely, change `from_env` to:

```rust
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
```

and add `acquisition: AcquisitionConfig::default(),` to the `Ok(Self { ... })` literal in `from_parts`. (Existing `from_parts` tests are unaffected — they don't inspect `acquisition`.)

- [ ] **Step 5: Run tests**

Run: `cargo test --lib config`
Expected: PASS (existing 8 + the 4 new acquisition tests).

- [ ] **Step 6: Full suite + commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/config.rs
git commit -m "feat(config): add acquisition quality-preference config (SP1)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: TMDB IMDB↔TMDB lookups

**Files:**
- Modify: `src/tmdb_client.rs`

These back the `--acquire` trigger: it accepts either an IMDB (`tt…`) or a numeric TMDB id and must end up with **both** (IMDB for Torrentio, TMDB for authoritative identity).

- [ ] **Step 1: Write failing tests** — append to `src/tmdb_client.rs`'s `#[cfg(test)] mod tests` (parse-only, no network):

```rust
    #[test]
    fn parse_find_response_extracts_tmdb_id_and_kind() {
        // TMDB /find/{imdb} returns results bucketed by media type.
        let json = serde_json::json!({
            "movie_results": [{"id": 27205}],
            "tv_results": []
        });
        let got = super::parse_find_response(&json);
        assert_eq!(got, Some((27205, crate::vfs::MediaType::Movie)));

        let json2 = serde_json::json!({
            "movie_results": [],
            "tv_results": [{"id": 1396}]
        });
        assert_eq!(
            super::parse_find_response(&json2),
            Some((1396, crate::vfs::MediaType::Show))
        );

        let empty = serde_json::json!({"movie_results": [], "tv_results": []});
        assert_eq!(super::parse_find_response(&empty), None);
    }

    #[test]
    fn parse_external_ids_extracts_imdb() {
        let json = serde_json::json!({"imdb_id": "tt0816692"});
        assert_eq!(super::parse_external_ids(&json), Some("tt0816692".to_string()));
        let none = serde_json::json!({"imdb_id": serde_json::Value::Null});
        assert_eq!(super::parse_external_ids(&none), None);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib tmdb_client`
Expected: FAIL — `parse_find_response`/`parse_external_ids` undefined.

- [ ] **Step 3: Implement the parsers + the network methods** — add to `src/tmdb_client.rs` (the pure parsers are free functions for testability; the methods call them):

```rust
/// Parse TMDB `/find/{imdb_id}?external_source=imdb_id` into (tmdb_id, kind).
/// Prefers a movie result, then a TV result.
pub(crate) fn parse_find_response(v: &serde_json::Value) -> Option<(u64, crate::vfs::MediaType)> {
    if let Some(id) = v
        .get("movie_results")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("id"))
        .and_then(|i| i.as_u64())
    {
        return Some((id, crate::vfs::MediaType::Movie));
    }
    if let Some(id) = v
        .get("tv_results")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("id"))
        .and_then(|i| i.as_u64())
    {
        return Some((id, crate::vfs::MediaType::Show));
    }
    None
}

/// Parse TMDB `/{type}/{id}/external_ids` into the IMDB id (non-empty).
pub(crate) fn parse_external_ids(v: &serde_json::Value) -> Option<String> {
    v.get("imdb_id")
        .and_then(|i| i.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
```

Then add two methods to the `TmdbClient` impl (use the same `self.client` + api-key pattern the existing search methods use — match the existing query-string/bearer style in this file):

```rust
    /// Resolve an IMDB id (`tt…`) to (tmdb_id, kind) via TMDB /find.
    pub async fn find_by_imdb(&self, imdb_id: &str) -> Result<Option<(u64, crate::vfs::MediaType)>, reqwest::Error> {
        let url = format!("https://api.themoviedb.org/3/find/{}", imdb_id);
        let v: serde_json::Value = self
            .client
            .get(&url)
            .query(&[("api_key", self.api_key.as_str()), ("external_source", "imdb_id")])
            .send()
            .await?
            .json()
            .await?;
        Ok(parse_find_response(&v))
    }

    /// Resolve a TMDB id to its IMDB id via /{type}/{id}/external_ids.
    pub async fn external_imdb_id(&self, tmdb_id: u64, kind: crate::vfs::MediaType) -> Result<Option<String>, reqwest::Error> {
        let path = match kind {
            crate::vfs::MediaType::Movie => "movie",
            crate::vfs::MediaType::Show => "tv",
        };
        let url = format!("https://api.themoviedb.org/3/{}/{}/external_ids", path, tmdb_id);
        let v: serde_json::Value = self
            .client
            .get(&url)
            .query(&[("api_key", self.api_key.as_str())])
            .send()
            .await?
            .json()
            .await?;
        Ok(parse_external_ids(&v))
    }
```

> The implementer must confirm the field names `client` and `api_key` (or the existing accessor) against the current `TmdbClient` struct and match the file's existing auth style (query param vs bearer header). If the existing code uses a bearer token or a different field name, mirror that exactly. If it diverges enough that these methods don't fit, STOP and report NEEDS_CONTEXT.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib tmdb_client`
Expected: PASS (the two parse tests; the network methods are exercised live in Task 11).

- [ ] **Step 5: Full suite + commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/tmdb_client.rs
git commit -m "feat(tmdb): add IMDB<->TMDB id lookups for acquisition" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `release.rs` — candidate model, parsing, scoring

**Files:**
- Create: `src/release.rs`
- Modify: `src/mapper.rs` (add `pub mod release;`)

This module is provider-agnostic and pure (no I/O), so it's fully unit-testable. It owns `RawCandidate` (the scraper's output) and `ReleaseInfo` (parsed) and the scoring.

- [ ] **Step 1: Write failing tests** — create `src/release.rs` with the test module first:

```rust
use crate::config::{AudioReq, MaxResolution, QualityPrefs};
use regex::Regex;
use std::sync::LazyLock;

// (impl added in Step 3)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SubReq;

    fn prefs() -> QualityPrefs {
        QualityPrefs {
            max_resolution: MaxResolution::P1080,
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            prefer_hevc: true,
            prefer_hdr: false,
        }
    }

    fn raw(name: &str, desc: &str, hash: &str, file: Option<&str>) -> RawCandidate {
        RawCandidate {
            name: name.to_string(),
            description: desc.to_string(),
            info_hash: hash.to_string(),
            file_idx: None,
            file_name: file.map(String::from),
        }
    }

    #[test]
    fn parses_resolution_codec_hdr_size_seeders_cached() {
        let c = raw(
            "Torrentio\n1080p",
            "Movie.2023.1080p.BluRay.x265.HDR.DDP5.1-GRP\n💾 8.4 GB 👤 42 ⚙️ ThePirateBay\nRD+",
            "abc",
            Some("Movie.2023.1080p.BluRay.x265.HDR.mkv"),
        );
        let r = parse(&c);
        assert_eq!(r.resolution, Some(1080));
        assert_eq!(r.codec, Codec::Hevc);
        assert!(r.hdr);
        assert_eq!(r.size_bytes, Some((8.4 * 1_000_000_000.0) as u64));
        assert_eq!(r.seeders, Some(42));
        assert!(r.cached); // "RD+" marker
        assert_eq!(r.container, Container::Mkv);
        assert_eq!(r.group.as_deref(), Some("GRP"));
    }

    #[test]
    fn parses_4k_and_avc_and_uncached() {
        let c = raw(
            "Torrentio\n4k",
            "Show.S01E02.2160p.WEB-DL.H264-XYZ\n💾 15 GB 👤 3",
            "def",
            Some("Show.S01E02.2160p.WEB-DL.H264-XYZ.mp4"),
        );
        let r = parse(&c);
        assert_eq!(r.resolution, Some(2160));
        assert_eq!(r.codec, Codec::Avc);
        assert!(!r.hdr);
        assert!(!r.cached);
        assert_eq!(r.container, Container::Mp4);
    }

    #[test]
    fn score_excludes_above_ceiling() {
        let c = raw("Torrentio\n4k", "X.2160p.x265\nRD+", "h", Some("X.2160p.mkv"));
        let r = parse(&c);
        assert_eq!(score(&r, &prefs()), None, "2160p must be excluded at a 1080p ceiling");
    }

    #[test]
    fn score_ranks_cached_above_uncached() {
        let cached = parse(&raw("Torrentio\n1080p", "A.1080p.x265\nRD+", "h1", Some("A.1080p.mkv")));
        let uncached = parse(&raw("Torrentio\n1080p", "A.1080p.x265", "h2", Some("A.1080p.mkv")));
        assert!(score(&cached, &prefs()).unwrap() > score(&uncached, &prefs()).unwrap());
    }

    #[test]
    fn score_prefers_verifiable_container_and_hevc() {
        let mkv = parse(&raw("Torrentio\n1080p", "A.1080p.x265", "h1", Some("A.1080p.mkv")));
        let avi = parse(&raw("Torrentio\n1080p", "A.1080p.x264", "h2", Some("A.1080p.avi")));
        assert!(score(&mkv, &prefs()).unwrap() > score(&avi, &prefs()).unwrap());
    }

    #[test]
    fn rank_orders_by_score_desc_dropping_excluded() {
        let cands = vec![
            parse(&raw("t", "A.2160p.x265\nRD+", "h4k", Some("A.2160p.mkv"))), // excluded (ceiling)
            parse(&raw("t", "A.1080p.x265", "hu", Some("A.1080p.mkv"))),       // uncached
            parse(&raw("t", "A.1080p.x265\nRD+", "hc", Some("A.1080p.mkv"))),  // cached
        ];
        let ranked = rank(cands, &prefs());
        assert_eq!(ranked.len(), 2, "the 2160p candidate is dropped");
        assert_eq!(ranked[0].info_hash, "hc", "cached ranks first");
        assert_eq!(ranked[1].info_hash, "hu");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib release`
Expected: FAIL to compile — `RawCandidate`/`parse`/`score`/`rank`/`Codec`/`Container` undefined.

- [ ] **Step 3: Implement** — insert above the test module in `src/release.rs`:

```rust
/// A raw stream from the scraper, before parsing.
#[derive(Debug, Clone)]
pub struct RawCandidate {
    /// Stream `name` (often "Torrentio\n1080p").
    pub name: String,
    /// Stream `title`/`description` detail (filename, size, seeders, cache tag).
    pub description: String,
    pub info_hash: String,
    /// Index of the requested file within the torrent (Torrentio `fileIdx`).
    pub file_idx: Option<usize>,
    /// The requested file's name (Torrentio `behaviorHints.filename`), if given.
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    Avc,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mkv,
    Mp4,
    Other,
}

impl Container {
    /// Containers whose tracks the probe can language-verify.
    pub fn is_verifiable(self) -> bool {
        matches!(self, Container::Mkv | Container::Mp4)
    }
    fn from_name(name: &str) -> Container {
        let n = name.to_ascii_lowercase();
        if n.ends_with(".mkv") {
            Container::Mkv
        } else if n.ends_with(".mp4") || n.ends_with(".mov") || n.ends_with(".m4v") {
            Container::Mp4
        } else {
            Container::Other
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub info_hash: String,
    pub file_idx: Option<usize>,
    pub file_name: Option<String>,
    pub resolution: Option<u16>,
    pub codec: Codec,
    pub hdr: bool,
    pub languages: Vec<String>,
    pub group: Option<String>,
    pub size_bytes: Option<u64>,
    pub seeders: Option<u32>,
    pub cached: bool,
    pub container: Container,
}

static RES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4})p\b|\b(4k|uhd)\b").unwrap());
static SIZE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([\d.]+)\s*(gb|mb)").unwrap());
static SEED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"👤\s*(\d+)").unwrap());
static GROUP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-([A-Za-z0-9]+)$").unwrap());

/// Parse a raw candidate into structured `ReleaseInfo`. Pure; tolerant of missing fields.
pub fn parse(c: &RawCandidate) -> ReleaseInfo {
    let text = format!("{}\n{}", c.name, c.description);
    let lower = text.to_ascii_lowercase();

    let resolution = RES_RE.captures(&text).and_then(|cap| {
        if let Some(m) = cap.get(1) {
            m.as_str().parse::<u16>().ok()
        } else {
            Some(2160) // 4k/uhd
        }
    });

    let codec = if lower.contains("x265") || lower.contains("h265") || lower.contains("hevc") {
        Codec::Hevc
    } else if lower.contains("x264") || lower.contains("h264") || lower.contains("avc") {
        Codec::Avc
    } else {
        Codec::Other
    };

    let hdr = lower.contains("hdr") || lower.contains("dolby vision") || lower.contains("dovi")
        || lower.contains(" dv ");

    // Cached marker: Torrentio appends a provider tag like "RD+"/"TB+" / a ⚡ for cached.
    let cached = lower.contains("rd+")
        || lower.contains("tb+")
        || lower.contains("[rd+]")
        || lower.contains("[tb+]")
        || text.contains('⚡');

    let size_bytes = SIZE_RE.captures(&text).and_then(|cap| {
        let n: f64 = cap.get(1)?.as_str().parse().ok()?;
        let unit = cap.get(2)?.as_str().to_ascii_lowercase();
        let mult = if unit == "gb" { 1_000_000_000.0 } else { 1_000_000.0 };
        Some((n * mult) as u64)
    });

    let seeders = SEED_RE
        .captures(&text)
        .and_then(|cap| cap.get(1)?.as_str().parse::<u32>().ok());

    // Group: trailing "-GRP" on the filename if present, else on the description's first line.
    let group_source = c.file_name.clone().unwrap_or_else(|| {
        c.description.lines().next().unwrap_or("").to_string()
    });
    let group_source = group_source
        .rsplit('.')
        .next()
        .map(|ext_stripped| group_source.trim_end_matches(&format!(".{}", ext_stripped)).to_string())
        .unwrap_or(group_source);
    let group = GROUP_RE
        .captures(group_source.trim())
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()));

    // Languages: best-effort — collect explicit language words present in the text.
    let mut languages = Vec::new();
    for (word, code) in LANG_WORDS {
        if lower.contains(word) {
            languages.push((*code).to_string());
        }
    }

    let container = c
        .file_name
        .as_deref()
        .map(Container::from_name)
        .unwrap_or(Container::Other);

    ReleaseInfo {
        info_hash: c.info_hash.clone(),
        file_idx: c.file_idx,
        file_name: c.file_name.clone(),
        resolution,
        codec,
        hdr,
        languages,
        group,
        size_bytes,
        seeders,
        cached,
        container,
    }
}

/// Best-effort language-word → ISO 639-2 code table for the layer-1 filter.
const LANG_WORDS: &[(&str, &str)] = &[
    ("english", "eng"),
    ("french", "fre"),
    ("german", "ger"),
    ("spanish", "spa"),
    ("italian", "ita"),
    ("russian", "rus"),
    ("hindi", "hin"),
    ("japanese", "jpn"),
    ("korean", "kor"),
    ("portuguese", "por"),
    ("multi", "mul"),
];

/// Score a release against prefs. `None` means excluded by a hard rule (resolution ceiling).
/// Higher is better. Cached dominates so cached releases always rank first.
pub fn score(r: &ReleaseInfo, prefs: &QualityPrefs) -> Option<i64> {
    if let Some(res) = r.resolution {
        if res > prefs.max_resolution.height() {
            return None;
        }
    }
    let mut s: i64 = 0;
    if r.cached {
        s += 1_000_000;
    }
    s += r.resolution.unwrap_or(0) as i64 * 100; // higher res better (within ceiling)
    if prefs.prefer_hevc && r.codec == Codec::Hevc {
        s += 5_000;
    }
    if prefs.prefer_hdr && r.hdr {
        s += 3_000;
    }
    if r.container.is_verifiable() {
        s += 2_000;
    }
    s += (r.seeders.unwrap_or(0).min(1000) as i64) * 2; // availability / tie-break
    // Size: mildly penalise extremes (tiny re-encodes < 300 MB, oversized remuxes > 25 GB).
    if let Some(sz) = r.size_bytes {
        if sz < 300_000_000 || sz > 25_000_000_000 {
            s -= 4_000;
        }
    }
    // Layer-1 language filter: if a specific audio language is required and the release
    // is tagged with languages that don't include it (and isn't multi), down-rank.
    if let AudioReq::Lang(want) = &prefs.audio {
        if !r.languages.is_empty()
            && !r.languages.iter().any(|l| l == want || l == "mul")
        {
            s -= 50_000;
        }
    }
    Some(s)
}

/// Parse-already-done candidates → ranked best-first, dropping hard-excluded ones.
pub fn rank(candidates: Vec<ReleaseInfo>, prefs: &QualityPrefs) -> Vec<ReleaseInfo> {
    let mut scored: Vec<(i64, ReleaseInfo)> = candidates
        .into_iter()
        .filter_map(|r| score(&r, prefs).map(|s| (s, r)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, r)| r).collect()
}
```

> Note: the `AudioReq` import is used by `score`; `MaxResolution` by the ceiling check. The cached-marker and size/seeder patterns are best-effort against Torrentio's current detail format and are covered by fixture tests in Task 4 + the live test in Task 11 (which will reveal any format drift).

- [ ] **Step 4: Declare module + run tests**

Add `pub mod release;` to `src/mapper.rs`. Run: `cargo test --lib release`
Expected: PASS (all 6 tests).

- [ ] **Step 5: Full suite + commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/release.rs src/mapper.rs
git commit -m "feat(release): candidate parsing + quality scoring/ranking" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `scraper.rs` — Scraper trait, Torrentio impl, Mock

> **Type note (resolved in Task 6):** `MediaKind` must be serialisable because it's persisted inside `AcquireRequest`. Define it as `#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)] pub enum MediaKind { Movie, Series }` (add the two serde derives to the snippet below when implementing).


**Files:**
- Create: `src/scraper.rs`
- Modify: `src/mapper.rs` (add `pub mod scraper;`)

- [ ] **Step 1: Write failing tests** — create `src/scraper.rs` with the test module first:

```rust
use crate::error::AppError;
use crate::provider::ProviderKind;
use crate::release::RawCandidate;
use async_trait::async_trait;

// (impl added in Step 3)

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
                    "title": "Movie.2023.1080p.x265-GRP\n💾 8 GB 👤 12\nRD+",
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
        let got = scraper.find("tt1", MediaKind::Movie, None, None).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].info_hash, "h");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib scraper`
Expected: FAIL to compile — `Scraper`/`TorrentioScraper`/`MockScraper`/`MediaKind`/`parse_streams` undefined.

- [ ] **Step 3: Implement** — insert above the test module in `src/scraper.rs`:

```rust
/// Movie vs series — the two Stremio stream endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Movie,
    Series,
}

/// Abstraction over a Stremio-addon scraper. `TorrentioScraper` is the default impl;
/// future sources (Prowlarr, …) implement the same trait.
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
    pub fn new(
        override_url: Option<String>,
        provider: ProviderKind,
        token: &str,
        http: reqwest::Client,
    ) -> Self {
        Self {
            base_url: Self::build_base_url(override_url, provider, token),
            http,
        }
    }

    /// Build the addon base URL: explicit override (trimmed) wins; otherwise template
    /// Torrentio from the active provider + token.
    /// NOTE: the `<provider>=<token>` option syntax must be verified against the current
    /// torrentio.strem.fun/configure output; the live `scraper_live_test` guards drift.
    pub fn build_base_url(override_url: Option<String>, provider: ProviderKind, token: &str) -> String {
        if let Some(u) = override_url {
            return u.trim().trim_end_matches('/').to_string();
        }
        let opt = match provider {
            ProviderKind::RealDebrid => "realdebrid",
            ProviderKind::TorBox => "torbox",
        };
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

/// Parse a Stremio stream response into raw candidates. Streams without an `infoHash`
/// (e.g. direct-URL entries) are skipped.
pub fn parse_streams(v: &serde_json::Value) -> Vec<RawCandidate> {
    let mut out = Vec::new();
    let Some(streams) = v.get("streams").and_then(|s| s.as_array()) else {
        return out;
    };
    for s in streams {
        let Some(info_hash) = s.get("infoHash").and_then(|h| h.as_str()) else {
            continue;
        };
        out.push(RawCandidate {
            name: s.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            description: s
                .get("title")
                .or_else(|| s.get("description"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            info_hash: info_hash.to_string(),
            file_idx: s.get("fileIdx").and_then(|i| i.as_u64()).map(|i| i as usize),
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
        let url = Self::stream_url(&self.base_url, imdb_id, kind, season, episode);
        let resp = self.http.get(&url).send().await.map_err(AppError::Http)?;
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
```

> `MockScraper` is `#[cfg(test)]` here; if Task 8's `acquire_integration_test` lives in a separate integration-test crate it can't see `#[cfg(test)]` items — keep the acquire tests as `#[cfg(test)] mod` *inside* `src/acquire.rs` (in-crate), matching the existing `MockProvider` pattern. (All SP1 deterministic tests are in-crate unit tests.)

- [ ] **Step 4: Declare module + run tests**

Add `pub mod scraper;` to `src/mapper.rs`. Run: `cargo test --lib scraper`
Expected: PASS (all 7 tests).

> If `AppError` has no `Http(reqwest::Error)` variant with that exact shape, use the existing conversion (SP0's `AppError` has `Http(#[from] reqwest::Error)`, so `.map_err(AppError::Http)` works, or use `?` directly). Verify against `src/error.rs`.

- [ ] **Step 5: Full suite + commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/scraper.rs src/mapper.rs
git commit -m "feat(scraper): Scraper trait + Torrentio impl with URL templating + Mock" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `probe.rs` — container track-language probe

**Files:**
- Create: `src/probe.rs`
- Modify: `src/mapper.rs` (add `pub mod probe;`)

The pure parsers (`parse_mkv_tracks`/`parse_mp4_tracks` over `&[u8]`) and `verify()` are unit-tested with synthetic bytes; the HTTP orchestration `probe_tracks` is thin and covered by the live test in Task 11. **No new dependencies** — hand-rolled EBML + ISO-BMFF.

### Reference: what we extract
- **MKV (EBML):** element IDs (as they appear on disk, marker bits included) — `Segment 0x18538067`, `Tracks 0x1654AE6B`, `TrackEntry 0xAE`, `TrackType 0x83` (1=video, 2=audio, 17=subtitle), `Language 0x22B59C` (ISO-639-2; default `"eng"` if absent), `LanguageBCP47 0x22B59D`. Magic: first 4 bytes `1A 45 DF A3`.
- **MP4 (ISO-BMFF):** box = 4-byte BE size + 4-byte type (size==1 → 64-bit size follows; size==0 → to EOF). Walk `moov → trak → mdia → {hdlr, mdhd}`. `hdlr` handler type at payload offset 8 (`soun`=audio, `vide`=video, `subt`/`sbtl`/`text`=subtitle). `mdhd` language is a packed 16-bit field (3×5-bit, each +0x60) at a version-dependent offset. Magic: bytes 4..8 == `ftyp`.

- [ ] **Step 1: Write failing tests** — create `src/probe.rs` with the test module first (uses in-test byte builders so no binary fixtures are needed):

```rust
use crate::config::{AudioReq, SubReq};

// (impl added in Steps 3–6)

#[cfg(test)]
mod tests {
    use super::*;

    // --- minimal EBML builder ---
    fn vint(size: u64) -> Vec<u8> {
        // Encode `size` as an EBML data-size vint using the shortest length 1..=8.
        for len in 1u32..=8 {
            let max = (1u64 << (7 * len)) - 1; // max representable with this length (excl. all-ones)
            if size < max {
                let marker = 1u64 << (7 * len);
                let val = marker | size;
                let bytes = val.to_be_bytes();
                return bytes[(8 - len as usize)..].to_vec();
            }
        }
        panic!("size too large for test");
    }
    fn id_bytes(id: u32) -> Vec<u8> {
        // Element IDs: emit the minimal big-endian bytes (id already includes marker bits).
        let b = id.to_be_bytes();
        let first = b.iter().position(|&x| x != 0).unwrap_or(3);
        b[first..].to_vec()
    }
    fn ebml_elem(id: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = id_bytes(id);
        out.extend(vint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn mkv_with(audio_lang: &str, sub_lang: Option<&str>) -> Vec<u8> {
        // EBML header (magic) + Segment{ Tracks{ TrackEntry(audio), [TrackEntry(sub)] } }
        let mut audio = Vec::new();
        audio.extend(ebml_elem(0x83, &[2])); // TrackType audio
        audio.extend(ebml_elem(0x22B59C, audio_lang.as_bytes())); // Language
        let mut tracks = ebml_elem(0xAE, &audio);
        if let Some(sl) = sub_lang {
            let mut sub = Vec::new();
            sub.extend(ebml_elem(0x83, &[17])); // TrackType subtitle
            sub.extend(ebml_elem(0x22B59C, sl.as_bytes()));
            tracks.extend(ebml_elem(0xAE, &sub));
        }
        let tracks_elem = ebml_elem(0x1654AE6B, &tracks);
        let segment = ebml_elem(0x18538067, &tracks_elem);
        let mut out = vec![0x1A, 0x45, 0xDF, 0xA3]; // magic (EBML header id; minimal)
        out.extend(ebml_elem(0xEC, &[])); // a Void-ish filler so magic isn't the only header
        out.extend(segment);
        out
    }

    // --- minimal MP4 builder ---
    fn mp4_box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut out = size.to_be_bytes().to_vec();
        out.extend_from_slice(typ);
        out.extend_from_slice(payload);
        out
    }
    fn mdhd(lang_packed: u16) -> Vec<u8> {
        // version0 mdhd: version(1)+flags(3)+creation(4)+modification(4)+timescale(4)+duration(4)+language(2)+pre_defined(2)
        let mut p = vec![0u8; 4 + 16];
        p.extend_from_slice(&lang_packed.to_be_bytes());
        p.extend_from_slice(&[0, 0]);
        mp4_box(b"mdhd", &p)
    }
    fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
        // version(1)+flags(3)+pre_defined(4)+handler_type(4)+reserved(12)+name(0)
        let mut p = vec![0u8; 4 + 4];
        p.extend_from_slice(handler);
        p.extend_from_slice(&[0u8; 12]);
        mp4_box(b"hdlr", &p)
    }
    fn trak(handler: &[u8; 4], lang_packed: u16) -> Vec<u8> {
        let mut mdia = hdlr(handler);
        mdia.extend(mdhd(lang_packed));
        let mdia_box = mp4_box(b"mdia", &mdia);
        mp4_box(b"trak", &mdia_box)
    }
    fn mp4_with(tracks: &[(&[u8; 4], u16)]) -> Vec<u8> {
        let mut moov = Vec::new();
        for (h, l) in tracks {
            moov.extend(trak(h, *l));
        }
        let moov_box = mp4_box(b"moov", &moov);
        let mut out = mp4_box(b"ftyp", b"isom\0\0\0\0isom"); // ftyp magic
        out.extend(moov_box);
        out
    }
    // "eng" packed: each char - 0x60, 5 bits: e=5,n=14,g=7 -> (5<<10)|(14<<5)|7
    fn packed(lang: &str) -> u16 {
        let b = lang.as_bytes();
        (((b[0] - 0x60) as u16) << 10) | (((b[1] - 0x60) as u16) << 5) | ((b[2] - 0x60) as u16)
    }

    #[test]
    fn mkv_audio_and_subtitle_languages() {
        let bytes = mkv_with("eng", Some("fre"));
        let tracks = parse_mkv_tracks(&bytes).expect("parse");
        assert!(tracks.iter().any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("eng")));
        assert!(tracks.iter().any(|t| t.kind == TrackKind::Subtitle && t.language.as_deref() == Some("fre")));
    }

    #[test]
    fn mkv_truncated_is_corrupt() {
        let mut bytes = mkv_with("eng", None);
        bytes.truncate(bytes.len() - 3); // cut mid-element
        assert!(matches!(parse_mkv_tracks(&bytes), Err(ProbeError::Corrupt)));
    }

    #[test]
    fn mp4_tracks_front_moov() {
        let bytes = mp4_with(&[(b"soun", packed("eng")), (b"subt", packed("ger"))]);
        let tracks = parse_mp4_tracks(&bytes).expect("parse");
        assert!(tracks.iter().any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("eng")));
        assert!(tracks.iter().any(|t| t.kind == TrackKind::Subtitle && t.language.as_deref() == Some("ger")));
    }

    #[test]
    fn mp4_no_moov_is_tracks_not_found() {
        // Just ftyp + mdat, no moov in this buffer (simulates moov-at-end before the tail is fetched).
        let mut bytes = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        bytes.extend(mp4_box(b"mdat", &[0u8; 16]));
        assert!(matches!(parse_mp4_tracks(&bytes), Err(ProbeError::TracksNotFound)));
    }

    #[test]
    fn mp4_bad_box_size_is_corrupt() {
        let mut bytes = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        // a box claiming size 4 (smaller than the 8-byte header) -> corrupt
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(b"moov");
        assert!(matches!(parse_mp4_tracks(&bytes), Err(ProbeError::Corrupt)));
    }

    #[test]
    fn detect_container_by_magic() {
        assert_eq!(detect_container(&mkv_with("eng", None)), Some(ContainerKind::Mkv));
        assert_eq!(detect_container(&mp4_with(&[(b"soun", packed("eng"))])), Some(ContainerKind::Mp4));
        assert_eq!(detect_container(b"RIFF\0\0\0\0AVI LIST"), None); // AVI -> unsupported
    }

    #[test]
    fn verify_audio_original_and_subtitle_rules() {
        let tracks = vec![
            Track { kind: TrackKind::Audio, language: Some("jpn".into()) },
            Track { kind: TrackKind::Subtitle, language: Some("eng".into()) },
        ];
        // original audio = jpn -> pass audio; subtitle eng required and present -> pass
        let req = LangReq { audio: AudioReq::Original, subtitle: SubReq::Lang("eng".into()), original_language: Some("jpn".into()) };
        assert_eq!(verify(&tracks, &req), Verify::Pass);

        // require eng audio but only jpn present -> FailAudio
        let req2 = LangReq { audio: AudioReq::Lang("eng".into()), subtitle: SubReq::None, original_language: None };
        assert_eq!(verify(&tracks, &req2), Verify::FailAudio);

        // require ger subtitle, absent -> FailSubtitle
        let req3 = LangReq { audio: AudioReq::Original, subtitle: SubReq::Lang("ger".into()), original_language: Some("jpn".into()) };
        assert_eq!(verify(&tracks, &req3), Verify::FailSubtitle);

        // no tracks parsed -> Inconclusive
        assert_eq!(verify(&[], &req), Verify::Inconclusive);
    }

    #[test]
    fn iso_639_1_to_2_mapping() {
        assert_eq!(to_iso639_2("en"), "eng");
        assert_eq!(to_iso639_2("eng"), "eng"); // already 639-2
        assert_eq!(to_iso639_2("ja"), "jpn");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib probe`
Expected: FAIL to compile — types/functions undefined.

- [ ] **Step 3: Types + language helpers** — insert above the test module in `src/probe.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Subtitle,
    Video,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub kind: TrackKind,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    Mkv,
    Mp4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    Pass,
    FailAudio,
    FailSubtitle,
    Inconclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// Fetch failed (network / non-success HTTP) — retry/defer, never a verdict.
    Transient,
    /// Recognised container, definitively broken structure → treat as bad release.
    Corrupt,
    /// Container we don't parse (AVI/TS/…) → accept-with-warning.
    Unsupported,
    /// Parsed OK but no track info located in the fetched bytes → accept-with-warning.
    TracksNotFound,
}

/// The resolved language requirement (built from `QualityPrefs` + the title's TMDB original language).
#[derive(Debug, Clone)]
pub struct LangReq {
    pub audio: AudioReq,
    pub subtitle: SubReq,
    pub original_language: Option<String>, // ISO 639-1 or -2 from TMDB
}

/// Minimal ISO 639-1 → 639-2/B map for the common languages; passes through anything
/// already 3 letters. (Extend as needed; unknown 2-letter codes pass through unchanged.)
pub fn to_iso639_2(code: &str) -> String {
    let c = code.trim().to_ascii_lowercase();
    if c.len() == 3 {
        return c;
    }
    match c.as_str() {
        "en" => "eng",
        "fr" => "fre",
        "de" => "ger",
        "es" => "spa",
        "it" => "ita",
        "ru" => "rus",
        "hi" => "hin",
        "ja" => "jpn",
        "ko" => "kor",
        "pt" => "por",
        "zh" => "chi",
        "nl" => "dut",
        "sv" => "swe",
        "no" => "nor",
        "da" => "dan",
        "fi" => "fin",
        "pl" => "pol",
        _ => return c, // unknown — pass through
    }
    .to_string()
}

fn lang_eq(a: &str, b: &str) -> bool {
    to_iso639_2(a) == to_iso639_2(b)
}

/// Verify parsed tracks against the requirement. Audio always enforced; subtitle only when set.
pub fn verify(tracks: &[Track], req: &LangReq) -> Verify {
    if tracks.is_empty() {
        return Verify::Inconclusive;
    }
    let audios: Vec<&str> = tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio)
        .filter_map(|t| t.language.as_deref())
        .collect();
    let subs: Vec<&str> = tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Subtitle)
        .filter_map(|t| t.language.as_deref())
        .collect();

    // Audio check
    let want_audio: Option<String> = match &req.audio {
        AudioReq::Lang(l) => Some(l.clone()),
        AudioReq::Original => req.original_language.clone(),
    };
    if let Some(want) = want_audio {
        if !audios.iter().any(|a| lang_eq(a, &want)) {
            return Verify::FailAudio;
        }
    }
    // Subtitle check (independent)
    if let SubReq::Lang(want) = &req.subtitle {
        if !subs.iter().any(|s| lang_eq(s, want)) {
            return Verify::FailSubtitle;
        }
    }
    Verify::Pass
}

/// Detect container by magic bytes. Returns `None` for anything we don't parse.
pub fn detect_container(buf: &[u8]) -> Option<ContainerKind> {
    if buf.len() >= 4 && buf[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(ContainerKind::Mkv);
    }
    if buf.len() >= 8 && &buf[4..8] == b"ftyp" {
        return Some(ContainerKind::Mp4);
    }
    None
}
```

- [ ] **Step 4: EBML (MKV) parser** — append to `src/probe.rs`:

```rust
/// Read an EBML element id (1..=4 bytes, marker bits retained). Advances `pos`.
fn read_ebml_id(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let first = *buf.get(*pos)?;
    let len = first.leading_zeros() as usize + 1; // 1..=4 for valid ids
    if len > 4 || *pos + len > buf.len() {
        return None;
    }
    let mut id: u32 = 0;
    for i in 0..len {
        id = (id << 8) | buf[*pos + i] as u32;
    }
    *pos += len;
    Some(id)
}

/// Read an EBML data size vint (marker stripped). Returns `None` on truncation.
/// An all-ones value signals "unknown size" → returned as `u64::MAX`.
fn read_ebml_size(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let first = *buf.get(*pos)?;
    if first == 0 {
        return None; // invalid
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || *pos + len > buf.len() {
        return None;
    }
    let mut val: u64 = (first as u64) & (0xFF >> len);
    let mut all_ones = val == (0xFFu64 >> len);
    for i in 1..len {
        let b = buf[*pos + i];
        val = (val << 8) | b as u64;
        all_ones = all_ones && b == 0xFF;
    }
    *pos += len;
    if all_ones {
        Some(u64::MAX)
    } else {
        Some(val)
    }
}

/// Parse MKV track languages. Returns `Corrupt` on a structurally-broken header,
/// `TracksNotFound` if no `Tracks` element is present in the buffer.
pub fn parse_mkv_tracks(buf: &[u8]) -> Result<Vec<Track>, ProbeError> {
    if detect_container(buf) != Some(ContainerKind::Mkv) {
        return Err(ProbeError::Corrupt);
    }
    // Scan top level for the Segment, then within it for Tracks.
    let segment = find_ebml_child(buf, 0, buf.len(), 0x18538067)?;
    let (seg_start, seg_end) = match segment {
        Some(range) => range,
        None => return Err(ProbeError::TracksNotFound),
    };
    let tracks = find_ebml_child(buf, seg_start, seg_end, 0x1654AE6B)?;
    let (t_start, t_end) = match tracks {
        Some(range) => range,
        None => return Err(ProbeError::TracksNotFound),
    };
    // Iterate TrackEntry (0xAE) children of Tracks.
    let mut out = Vec::new();
    let mut pos = t_start;
    while pos < t_end {
        let id = read_ebml_id(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        let size = read_ebml_size(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        let end = if size == u64::MAX { t_end } else { pos + size as usize };
        if end > buf.len() || end > t_end {
            return Err(ProbeError::Corrupt);
        }
        if id == 0xAE {
            out.push(parse_mkv_track_entry(buf, pos, end)?);
        }
        pos = end;
    }
    Ok(out)
}

/// Find the first child element with `target_id` between [start,end). Returns its payload range.
/// `Ok(None)` if absent; `Err(Corrupt)` on a malformed element.
fn find_ebml_child(
    buf: &[u8],
    start: usize,
    end: usize,
    target_id: u32,
) -> Result<Option<(usize, usize)>, ProbeError> {
    let mut pos = start;
    // Skip the top-level EBML header (id 0x1A45DFA3) if we're at file start.
    while pos < end {
        let id = match read_ebml_id(buf, &mut pos) {
            Some(id) => id,
            None => return Err(ProbeError::Corrupt),
        };
        let size = match read_ebml_size(buf, &mut pos) {
            Some(s) => s,
            None => return Err(ProbeError::Corrupt),
        };
        let payload_start = pos;
        let payload_end = if size == u64::MAX { end } else { payload_start + size as usize };
        if payload_end > end {
            // unknown/oversized — clamp to end (live streams use unknown Segment sizes)
            if size == u64::MAX {
                return Ok(if id == target_id { Some((payload_start, end)) } else { None });
            }
            return Err(ProbeError::Corrupt);
        }
        if id == target_id {
            return Ok(Some((payload_start, payload_end)));
        }
        pos = payload_end;
    }
    Ok(None)
}

fn parse_mkv_track_entry(buf: &[u8], start: usize, end: usize) -> Result<Track, ProbeError> {
    let mut kind = TrackKind::Other;
    let mut language: Option<String> = None;
    let mut pos = start;
    while pos < end {
        let id = read_ebml_id(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        let size = read_ebml_size(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        let p_end = pos + size as usize;
        if size == u64::MAX || p_end > end {
            return Err(ProbeError::Corrupt);
        }
        match id {
            0x83 => {
                // TrackType (1 byte)
                kind = match buf.get(pos).copied() {
                    Some(2) => TrackKind::Audio,
                    Some(17) => TrackKind::Subtitle,
                    Some(1) => TrackKind::Video,
                    _ => TrackKind::Other,
                };
            }
            0x22B59C => {
                language = std::str::from_utf8(&buf[pos..p_end]).ok().map(|s| s.trim().to_string());
            }
            _ => {}
        }
        pos = p_end;
    }
    // MKV default language is "eng" when the Language element is absent.
    Ok(Track {
        kind,
        language: language.or_else(|| Some("eng".to_string())),
    })
}
```

- [ ] **Step 5: MP4 (ISO-BMFF) parser** — append to `src/probe.rs`:

```rust
/// Read a box header at `pos`: returns (box_type, payload_start, box_end). Advances nothing.
fn read_box_header(buf: &[u8], pos: usize) -> Result<([u8; 4], usize, usize), ProbeError> {
    if pos + 8 > buf.len() {
        return Err(ProbeError::TracksNotFound); // ran out of buffer cleanly
    }
    let size32 = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
    let typ = [buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]];
    let (payload_start, box_end) = if size32 == 1 {
        // 64-bit size
        if pos + 16 > buf.len() {
            return Err(ProbeError::Corrupt);
        }
        let big = u64::from_be_bytes(buf[pos + 8..pos + 16].try_into().unwrap());
        (pos + 16, pos + big as usize)
    } else if size32 == 0 {
        (pos + 8, buf.len()) // extends to end
    } else {
        (pos + 8, pos + size32 as usize)
    };
    if box_end < payload_start {
        return Err(ProbeError::Corrupt); // size smaller than header
    }
    Ok((typ, payload_start, box_end))
}

/// Walk top-level boxes for `moov`; parse its `trak`s. `TracksNotFound` if no `moov` here.
pub fn parse_mp4_tracks(buf: &[u8]) -> Result<Vec<Track>, ProbeError> {
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let (typ, p_start, b_end) = read_box_header(buf, pos)?;
        if b_end > buf.len() {
            // moov truncated in this buffer; but if it's not moov just stop scanning
            if &typ == b"moov" {
                return Err(ProbeError::Corrupt);
            }
            break;
        }
        if &typ == b"moov" {
            return parse_mp4_moov(buf, p_start, b_end);
        }
        pos = b_end;
    }
    Err(ProbeError::TracksNotFound)
}

fn parse_mp4_moov(buf: &[u8], start: usize, end: usize) -> Result<Vec<Track>, ProbeError> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end {
        let (typ, p_start, b_end) = read_box_header(buf, pos)?;
        if b_end > end {
            return Err(ProbeError::Corrupt);
        }
        if &typ == b"trak" {
            out.push(parse_mp4_trak(buf, p_start, b_end)?);
        }
        pos = b_end;
    }
    Ok(out)
}

fn parse_mp4_trak(buf: &[u8], start: usize, end: usize) -> Result<Track, ProbeError> {
    // descend trak -> mdia -> {hdlr, mdhd}
    let mdia = find_mp4_child(buf, start, end, b"mdia")?;
    let (m_start, m_end) = match mdia {
        Some(r) => r,
        None => return Ok(Track { kind: TrackKind::Other, language: None }),
    };
    let mut kind = TrackKind::Other;
    let mut language: Option<String> = None;
    let mut pos = m_start;
    while pos + 8 <= m_end {
        let (typ, p_start, b_end) = read_box_header(buf, pos)?;
        if b_end > m_end {
            return Err(ProbeError::Corrupt);
        }
        if &typ == b"hdlr" {
            // handler_type at payload offset 8 (version/flags 4 + pre_defined 4)
            if p_start + 12 <= b_end {
                let h = &buf[p_start + 8..p_start + 12];
                kind = match h {
                    b"soun" => TrackKind::Audio,
                    b"vide" => TrackKind::Video,
                    b"subt" | b"sbtl" | b"text" | b"clcp" => TrackKind::Subtitle,
                    _ => TrackKind::Other,
                };
            }
        } else if &typ == b"mdhd" {
            language = parse_mdhd_language(buf, p_start, b_end);
        }
        pos = b_end;
    }
    Ok(Track { kind, language })
}

fn find_mp4_child(
    buf: &[u8],
    start: usize,
    end: usize,
    target: &[u8; 4],
) -> Result<Option<(usize, usize)>, ProbeError> {
    let mut pos = start;
    while pos + 8 <= end {
        let (typ, p_start, b_end) = read_box_header(buf, pos)?;
        if b_end > end {
            return Err(ProbeError::Corrupt);
        }
        if &typ == target {
            return Ok(Some((p_start, b_end)));
        }
        pos = b_end;
    }
    Ok(None)
}

/// Decode the packed 3×5-bit ISO-639-2 language from an `mdhd` payload.
fn parse_mdhd_language(buf: &[u8], start: usize, end: usize) -> Option<String> {
    let version = *buf.get(start)?;
    // version0: lang at offset 4+4+4+4+4 = 20; version1: 4+8+8+4+8 = 32
    let lang_off = if version == 1 { start + 32 } else { start + 20 };
    if lang_off + 2 > end {
        return None;
    }
    let packed = u16::from_be_bytes([buf[lang_off], buf[lang_off + 1]]);
    let c1 = ((packed >> 10) & 0x1F) as u8 + 0x60;
    let c2 = ((packed >> 5) & 0x1F) as u8 + 0x60;
    let c3 = (packed & 0x1F) as u8 + 0x60;
    let s: String = [c1 as char, c2 as char, c3 as char].iter().collect();
    if s == "und" || !s.chars().all(|c| c.is_ascii_lowercase()) {
        None
    } else {
        Some(s)
    }
}
```

- [ ] **Step 6: HTTP orchestration** — append to `src/probe.rs`:

```rust
/// Fetch the container header(s) over ranged GETs and extract tracks.
/// `Transient` on any fetch failure (caller re-resolves/retries). Reuses the
/// Range-header pattern from `dav_fs::ProxiedMediaFile::fetch_cdn_range`.
pub async fn probe_tracks(http: &reqwest::Client, cdn_url: &str) -> Result<Vec<Track>, ProbeError> {
    const FRONT: u64 = 4 * 1024 * 1024; // 4 MB front region (covers MKV Tracks + front moov)
    let front = fetch_range(http, cdn_url, 0, FRONT - 1).await?;
    match detect_container(&front) {
        Some(ContainerKind::Mkv) => parse_mkv_tracks(&front),
        Some(ContainerKind::Mp4) => match parse_mp4_tracks(&front) {
            Err(ProbeError::TracksNotFound) => {
                // moov likely at the end — fetch the tail and retry.
                let tail = fetch_suffix(http, cdn_url, FRONT).await?;
                parse_mp4_tracks(&tail)
            }
            other => other,
        },
        None => Err(ProbeError::Unsupported),
    }
}

async fn fetch_range(http: &reqwest::Client, url: &str, start: u64, end: u64) -> Result<Vec<u8>, ProbeError> {
    let resp = http
        .get(url)
        .header("Range", format!("bytes={}-{}", start, end))
        .send()
        .await
        .map_err(|_| ProbeError::Transient)?;
    if !resp.status().is_success() {
        return Err(ProbeError::Transient);
    }
    resp.bytes().await.map(|b| b.to_vec()).map_err(|_| ProbeError::Transient)
}

async fn fetch_suffix(http: &reqwest::Client, url: &str, len: u64) -> Result<Vec<u8>, ProbeError> {
    let resp = http
        .get(url)
        .header("Range", format!("bytes=-{}", len))
        .send()
        .await
        .map_err(|_| ProbeError::Transient)?;
    if !resp.status().is_success() {
        return Err(ProbeError::Transient);
    }
    resp.bytes().await.map(|b| b.to_vec()).map_err(|_| ProbeError::Transient)
}
```

- [ ] **Step 7: Declare module + run tests**

Add `pub mod probe;` to `src/mapper.rs`. Run: `cargo test --lib probe`
Expected: PASS (all probe tests). If a synthetic-bytes test reveals an off-by-one in a parser, fix the parser (the tests are the spec) — do not loosen the tests.

- [ ] **Step 8: Full suite + commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/probe.rs src/mapper.rs
git commit -m "feat(probe): hand-rolled MKV/MP4 track-language probe + verification" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `store.rs` — v1→v2 migration + new tables

**Files:**
- Modify: `src/store.rs`

Adds `owned_hashes`, `authoritative_ids`, `blacklist` plus the persisted `AcquireRequest`/`OwnedRecord` types. Bumps `SCHEMA_VERSION` to 2; existing `matches` data is preserved (redb creates the new tables lazily, so the migration is a version bump + ensuring the tables exist).

- [ ] **Step 1: Write failing tests** — append to `src/store.rs`'s `#[cfg(test)] mod tests`:

```rust
    use crate::scraper::MediaKind;

    fn req(imdb: &str, tmdb: u64) -> AcquireRequest {
        AcquireRequest {
            imdb_id: imdb.to_string(),
            tmdb_id: tmdb,
            kind: MediaKind::Movie,
            season: None,
            episode: None,
            original_language: Some("eng".to_string()),
            metadata: movie("Title"),
        }
    }

    #[tokio::test]
    async fn owned_round_trip_and_status_update() {
        let store = mem_store();
        let rec = OwnedRecord {
            request: req("tt1", 27205),
            source: "manual".to_string(),
            added_at: 100,
            status: OwnedStatus::Pending,
        };
        store.put_owned("h1".to_string(), rec).await.unwrap();
        assert_eq!(store.get_owned("h1".to_string()).await.unwrap().status, OwnedStatus::Pending);
        store.set_owned_status("h1".to_string(), OwnedStatus::Verified).await.unwrap();
        assert_eq!(store.get_owned("h1".to_string()).await.unwrap().status, OwnedStatus::Verified);
        let all = store.all_owned().await;
        assert_eq!(all.len(), 1);
        store.remove_owned("h1".to_string()).await.unwrap();
        assert!(store.get_owned("h1".to_string()).await.is_none());
    }

    #[tokio::test]
    async fn authoritative_round_trip() {
        let store = mem_store();
        store.put_authoritative("h1".to_string(), movie("Auth")).await.unwrap();
        assert_eq!(store.authoritative_meta("h1".to_string()).await.unwrap().title, "Auth");
        store.remove_authoritative("h1".to_string()).await.unwrap();
        assert!(store.authoritative_meta("h1".to_string()).await.is_none());
    }

    #[tokio::test]
    async fn blacklist_add_and_check() {
        let store = mem_store();
        assert!(!store.is_blacklisted(27205, "h1".to_string()).await);
        store.blacklist_add(27205, "h1".to_string(), "WrongTitle", 100).await.unwrap();
        assert!(store.is_blacklisted(27205, "h1".to_string()).await);
        // different title or hash is independent
        assert!(!store.is_blacklisted(27205, "h2".to_string()).await);
        assert!(!store.is_blacklisted(99999, "h1".to_string()).await);
    }

    #[tokio::test]
    async fn migrates_v1_db_to_v2_preserving_matches() {
        // Build a v1 DB (matches + meta(version=1)), then open via Store -> version 2, matches kept, new tables usable.
        let tmp = TempDb::new("migrate");
        {
            let db = Database::create(&tmp.path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mdef: TableDefinition<&str, &[u8]> = TableDefinition::new("matches");
                let mut t = txn.open_table(mdef).unwrap();
                let i = info("m1");
                let m = movie("Kept");
                t.insert("m1", serde_json::to_vec(&(&i, &m)).unwrap().as_slice()).unwrap();
                let vdef: TableDefinition<&str, u64> = TableDefinition::new("meta");
                let mut v = txn.open_table(vdef).unwrap();
                v.insert("schema_version", &1u64).unwrap();
            }
            txn.commit().unwrap();
        }
        let store = Store::open(&tmp.path).unwrap();
        assert_eq!(store.get_match("m1".to_string()).await.unwrap().1.title, "Kept");
        // new tables work post-migration
        store.put_authoritative("h".to_string(), movie("New")).await.unwrap();
        assert_eq!(store.authoritative_meta("h".to_string()).await.unwrap().title, "New");
        assert!(!std::path::Path::new(&tmp.corrupt_path()).exists(), "valid v1 DB must not be moved aside");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib store`
Expected: FAIL — new types/methods undefined.

- [ ] **Step 3: Bump version, add tables + types** — in `src/store.rs`: change `pub const SCHEMA_VERSION: u64 = 1;` to `= 2;`, add table consts next to the existing ones, and add the record types:

```rust
const OWNED_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_hashes");
const AUTH_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("authoritative_ids");
const BLACKLIST_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("blacklist");

/// The persisted "what to acquire" spec (also used by `acquire.rs`). Stored in `owned_hashes`
/// so `observe` can re-acquire a title after a stall/failure without external context.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AcquireRequest {
    pub imdb_id: String,
    pub tmdb_id: u64,
    pub kind: crate::scraper::MediaKind,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub original_language: Option<String>,
    pub metadata: crate::vfs::MediaMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OwnedStatus {
    Pending,
    Verified,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OwnedRecord {
    pub request: AcquireRequest,
    pub source: String,
    pub added_at: u64,
    pub status: OwnedStatus,
}
```

- [ ] **Step 4: Ensure new tables on open** — in `ensure_schema`, open the three new tables alongside the existing ones so a migrated/fresh DB has them. Change the inner block of `ensure_schema` to:

```rust
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(MATCHES_TABLE)?;
            write_txn.open_table(OWNED_TABLE)?;
            write_txn.open_table(AUTH_TABLE)?;
            write_txn.open_table(BLACKLIST_TABLE)?;
            let mut meta = write_txn.open_table(META_TABLE)?;
            meta.insert(SCHEMA_VERSION_KEY, &SCHEMA_VERSION)?;
        }
        write_txn.commit()?;
```

(`run_migrations` stays a no-op — there is no data transform from v1 to v2; the tables are additive.)

- [ ] **Step 5: Add accessors** — add to the `impl Store` block (they follow the SP0 `spawn_blocking` + `flatten_join` pattern):

```rust
    pub async fn put_owned(&self, hash: String, rec: OwnedRecord) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let bytes = serde_json::to_vec(&rec).map_err(|e| {
                redb::Error::from(redb::StorageError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
            })?;
            let txn = db.begin_write()?;
            { txn.open_table(OWNED_TABLE)?.insert(hash.as_str(), bytes.as_slice())?; }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn get_owned(&self, hash: String) -> Option<OwnedRecord> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(OWNED_TABLE).ok()?;
            let e = table.get(hash.as_str()).ok()??;
            serde_json::from_slice::<OwnedRecord>(e.value()).ok()
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn set_owned_status(&self, hash: String, status: OwnedStatus) -> Result<(), AppError> {
        if let Some(mut rec) = self.get_owned(hash.clone()).await {
            rec.status = status;
            self.put_owned(hash, rec).await
        } else {
            Ok(())
        }
    }

    pub async fn remove_owned(&self, hash: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(OWNED_TABLE)?.remove(hash.as_str())?; }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn all_owned(&self) -> Vec<(String, OwnedRecord)> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            if let Ok(txn) = db.begin_read() {
                if let Ok(table) = txn.open_table(OWNED_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (k, v) = entry;
                            if let Ok(rec) = serde_json::from_slice::<OwnedRecord>(v.value()) {
                                out.push((k.value().to_string(), rec));
                            }
                        }
                    }
                }
            }
            out
        })
        .await
        .unwrap_or_default()
    }

    pub async fn put_authoritative(&self, hash: String, meta: crate::vfs::MediaMetadata) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let bytes = serde_json::to_vec(&meta).map_err(|e| {
                redb::Error::from(redb::StorageError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
            })?;
            let txn = db.begin_write()?;
            { txn.open_table(AUTH_TABLE)?.insert(hash.as_str(), bytes.as_slice())?; }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn authoritative_meta(&self, hash: String) -> Option<crate::vfs::MediaMetadata> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(AUTH_TABLE).ok()?;
            let e = table.get(hash.as_str()).ok()??;
            serde_json::from_slice::<crate::vfs::MediaMetadata>(e.value()).ok()
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn remove_authoritative(&self, hash: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(AUTH_TABLE)?.remove(hash.as_str())?; }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn blacklist_add(&self, tmdb_id: u64, hash: String, reason: &str, at: u64) -> Result<(), AppError> {
        let key = format!("{}|{}", tmdb_id, hash);
        let reason = reason.to_string();
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let bytes = serde_json::to_vec(&serde_json::json!({"reason": reason, "at": at})).map_err(|e| {
                redb::Error::from(redb::StorageError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
            })?;
            let txn = db.begin_write()?;
            { txn.open_table(BLACKLIST_TABLE)?.insert(key.as_str(), bytes.as_slice())?; }
            txn.commit()?;
            Ok(())
        })
        .await;
        Self::flatten_join(result)
    }

    pub async fn is_blacklisted(&self, tmdb_id: u64, hash: String) -> bool {
        let key = format!("{}|{}", tmdb_id, hash);
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = match db.begin_read() { Ok(t) => t, Err(_) => return false };
            let table = match txn.open_table(BLACKLIST_TABLE) { Ok(t) => t, Err(_) => return false };
            matches!(table.get(key.as_str()), Ok(Some(_)))
        })
        .await
        .unwrap_or(false)
    }
```

> `redb::StorageError::Io` wrapping converts a serde error into `redb::Error` so it flows through `flatten_join`; if that variant name differs in redb 3.1, serialise the value before opening the txn (as `Store::replace_match` already does) and skip-or-error on failure. Verify against the existing `store.rs` error handling.

- [ ] **Step 6: Run tests + commit**

Run: `cargo test --lib store` then `cargo test`
Expected: PASS (existing store tests + the 4 new ones; full suite green).

```bash
git add src/store.rs
git commit -m "feat(store): v2 migration + owned/authoritative/blacklist tables" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: `reacquire.rs` — shared primitive + repair refactor

**Files:**
- Create: `src/reacquire.rs`
- Modify: `src/mapper.rs` (add `pub mod reacquire;`)
- Modify: `src/repair.rs` (reimplement `add_and_select_files` on `materialise`)

- [ ] **Step 1: Write failing tests** — create `src/reacquire.rs` with the test module first:

```rust
use crate::error::AppError;
use crate::provider::DebridProvider;
use crate::rd_client::TorrentInfo;
use std::time::Duration;

// (impl added in Step 3)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::rd_client::{AddMagnetResponse, TorrentFile};
    use std::sync::Arc;

    fn mock(status: &str) -> MockProvider {
        MockProvider {
            add_magnet: Some(AddMagnetResponse { id: "new".into(), uri: String::new() }),
            torrent_info: Some(TorrentInfo {
                id: "new".into(),
                hash: "H".into(),
                status: status.into(),
                files: vec![TorrentFile { id: 7, path: "/Movie.mkv".into(), bytes: 10, selected: 0 }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn materialise_selects_via_closure_and_returns_info() {
        let provider: Arc<dyn DebridProvider> = Arc::new(mock("downloaded"));
        let (id, info) = materialise(&*provider, "H", Duration::from_millis(0), |info| {
            info.files.iter().filter(|f| f.path.ends_with(".mkv")).map(|f| f.id).collect()
        })
        .await
        .expect("materialise");
        assert_eq!(id, "new");
        assert_eq!(info.id, "new");
    }

    #[tokio::test]
    async fn materialise_errors_when_selector_matches_nothing() {
        let provider: Arc<dyn DebridProvider> = Arc::new(mock("downloaded"));
        let r = materialise(&*provider, "H", Duration::from_millis(0), |_| Vec::<u32>::new()).await;
        assert!(r.is_err(), "no selected files must be an error");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib reacquire`
Expected: FAIL — `materialise` undefined.

- [ ] **Step 3: Implement** — insert above the test module in `src/reacquire.rs`:

```rust
/// Shared add→select primitive used by both repair (same-hash) and acquisition (candidate).
/// Adds the magnet for `hash`, waits `settle`, fetches info, asks `select` which file ids to
/// select (by id), selects them, and returns `(new_torrent_id, post_add_info)`. On any failure
/// after the add, the leaked torrent is deleted before returning `Err` (so nothing leaks).
/// The caller decides cached-vs-not by re-fetching final info (mirrors the existing repair flow).
pub async fn materialise(
    provider: &dyn DebridProvider,
    hash: &str,
    settle: Duration,
    select: impl Fn(&TorrentInfo) -> Vec<u32>,
) -> Result<(String, TorrentInfo), AppError> {
    let magnet = format!("magnet:?xt=urn:btih:{}", hash);
    let added = provider.add_magnet(&magnet).await.map_err(AppError::Http)?;
    let new_id = added.id;

    if !settle.is_zero() {
        tokio::time::sleep(settle).await;
    }

    let info = match provider.get_torrent_info(&new_id).await {
        Ok(i) => i,
        Err(e) => {
            let _ = provider.delete_torrent(&new_id).await;
            return Err(AppError::Http(e));
        }
    };

    let ids = select(&info);
    if ids.is_empty() {
        let _ = provider.delete_torrent(&new_id).await;
        return Err(AppError::Repair(format!(
            "no matching files to select in torrent {}",
            new_id
        )));
    }
    let ids_str = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    if let Err(e) = provider.select_files(&new_id, &ids_str).await {
        let _ = provider.delete_torrent(&new_id).await;
        return Err(AppError::Http(e));
    }
    Ok((new_id, info))
}
```

- [ ] **Step 4: Declare module + run tests**

Add `pub mod reacquire;` to `src/mapper.rs`. Run: `cargo test --lib reacquire`
Expected: PASS.

- [ ] **Step 5: Reimplement repair's `add_and_select_files` on `materialise`** — in `src/repair.rs`, replace the body of `add_and_select_files` (keep its signature `async fn add_and_select_files(&self, old_torrent_id, old_info, wait_duration) -> Result<(String, TorrentInfo), String>`) with a call to `materialise`, preserving the existing path-matching selector and the `set_repair_failed(old)` behaviour on error:

```rust
    async fn add_and_select_files(
        &self,
        old_torrent_id: &str,
        old_info: &TorrentInfo,
        wait_duration: Duration,
    ) -> Result<(String, TorrentInfo), String> {
        // Selector: match the new torrent's files to the old torrent's previously-selected paths.
        let old_info_cl = old_info.clone();
        let select = move |new_info: &TorrentInfo| -> Vec<u32> {
            old_info_cl
                .files
                .iter()
                .filter(|f| f.selected == 1)
                .filter_map(|of| new_info.files.iter().find(|nf| nf.path == of.path).map(|nf| nf.id))
                .collect()
        };
        match crate::reacquire::materialise(&*self.rd_client, &old_info.hash, wait_duration, select).await {
            Ok(pair) => Ok(pair),
            Err(e) => {
                self.set_repair_failed(old_torrent_id).await;
                Err(e.to_string())
            }
        }
    }
```

> This preserves behaviour: `materialise` deletes the leaked new torrent on failure (replacing the old `cleanup_leaked_torrent` calls inside `add_and_select_files`), and the wrapper still marks the OLD torrent failed. If `add_and_select_files` previously logged step-by-step inside, those logs move to the caller `repair_torrent` (unchanged). Remove the now-dead private `cleanup_leaked_torrent` only if it's no longer referenced elsewhere (it's still used by `try_instant_repair`'s non-`add_and_select_files` paths — keep it if so).

- [ ] **Step 6: Run the repair regression + full suite**

Run: `cargo test --lib repair` then `cargo test`
Expected: PASS — all existing repair unit tests green (behaviour preserved).

- [ ] **Step 7: Commit**

```bash
git add src/reacquire.rs src/repair.rs src/mapper.rs
git commit -m "refactor(repair): extract shared reacquire::materialise primitive" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: `acquire.rs` — the acquisition engine

**Files:**
- Create: `src/acquire.rs`
- Modify: `src/mapper.rs` (add `pub mod acquire;`)

Three seams (`Scraper` from Task 4, plus `TitleValidator` and `Prober` defined here) make the whole flow unit-testable with mocks + the in-memory `Store` + `MockProvider`.

- [ ] **Step 1: Write failing tests** — create `src/acquire.rs` with the test module first:

```rust
use crate::config::{AudioReq, QualityPrefs, SubReq};
use crate::error::AppError;
use crate::probe::{self, ProbeError, Track, TrackKind, Verify};
use crate::provider::{DebridProvider, FileLocator};
use crate::rd_client::TorrentInfo;
use crate::release::{self, ReleaseInfo};
use crate::scraper::{MediaKind, Scraper};
use crate::store::{AcquireRequest, OwnedStatus, Store};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{info, warn};

// (impl added in Steps 3–7)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::MockProvider;
    use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo as TI};
    use crate::scraper::MockScraper;
    use crate::release::RawCandidate;
    use crate::vfs::{MediaMetadata, MediaType};
    use redb::backends::InMemoryBackend;

    fn store() -> Store {
        Store::from_database(Arc::new(
            redb::Database::builder().create_with_backend(InMemoryBackend::new()).unwrap(),
        ))
        .unwrap()
    }
    fn prefs() -> QualityPrefs {
        QualityPrefs { max_resolution: crate::config::MaxResolution::P1080, audio: AudioReq::Original, subtitle: SubReq::None, prefer_hevc: true, prefer_hdr: false }
    }
    fn meta() -> MediaMetadata {
        MediaMetadata { title: "Movie".into(), year: Some("2023".into()), media_type: MediaType::Movie, external_id: Some("tmdb:27205".into()) }
    }
    fn req() -> AcquireRequest {
        AcquireRequest { imdb_id: "tt1".into(), tmdb_id: 27205, kind: MediaKind::Movie, season: None, episode: None, original_language: Some("eng".into()), metadata: meta() }
    }
    fn cand(hash: &str, cached: bool) -> RawCandidate {
        RawCandidate {
            name: "Torrentio\n1080p".into(),
            description: format!("Movie.2023.1080p.x265{}", if cached { "\nRD+" } else { "" }),
            info_hash: hash.into(),
            file_idx: Some(0),
            file_name: Some("Movie.2023.1080p.x265.mkv".into()),
        }
    }
    fn provider_returning(status: &str, hash: &str) -> Arc<dyn DebridProvider> {
        Arc::new(MockProvider {
            add_magnet: Some(AddMagnetResponse { id: format!("tid_{hash}"), uri: String::new() }),
            torrent_info: Some(TI {
                id: format!("tid_{hash}"),
                hash: hash.into(),
                status: status.into(),
                files: vec![TorrentFile { id: 0, path: "Movie.2023.1080p.x265.mkv".into(), bytes: 10, selected: 1 }],
                links: vec!["https://cdn/file".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/file".into()),
            ..Default::default()
        })
    }

    // Test seams
    struct OkValidator(bool);
    #[async_trait]
    impl TitleValidator for OkValidator {
        async fn validate(&self, _f: &str, _t: u64, _k: MediaKind, _s: Option<u32>, _e: Option<u32>) -> bool { self.0 }
    }
    struct CannedProber(Result<Vec<Track>, ProbeError>);
    #[async_trait]
    impl Prober for CannedProber {
        async fn probe(&self, _url: &str) -> Result<Vec<Track>, ProbeError> { self.0.clone() }
    }

    fn engine(provider: Arc<dyn DebridProvider>, scraper: Arc<dyn Scraper>, validator: Arc<dyn TitleValidator>, prober: Arc<dyn Prober>, store: Store) -> AcquisitionEngine {
        AcquisitionEngine::new(provider, scraper, validator, prober, store, prefs(), 5, Duration::from_secs(1800))
    }

    #[tokio::test]
    async fn cached_pass_records_owned_and_authoritative() {
        let st = store();
        let scraper = Arc::new(MockScraper { candidates: vec![cand("h1", true)] });
        let prober = Arc::new(CannedProber(Ok(vec![Track { kind: TrackKind::Audio, language: Some("eng".into()) }])));
        let eng = engine(provider_returning("downloaded", "h1"), scraper, Arc::new(OkValidator(true)), prober, st.clone());
        let out = eng.acquire(req()).await;
        assert_eq!(out, AcquireOutcome::Acquired("h1".into()));
        assert_eq!(st.get_owned("h1".into()).await.unwrap().status, OwnedStatus::Verified);
        assert_eq!(st.authoritative_meta("h1".into()).await.unwrap().external_id.as_deref(), Some("tmdb:27205"));
    }

    #[tokio::test]
    async fn wrong_title_is_blacklisted_and_not_recorded() {
        let st = store();
        let scraper = Arc::new(MockScraper { candidates: vec![cand("h1", true)] });
        let prober = Arc::new(CannedProber(Ok(vec![])));
        let eng = engine(provider_returning("downloaded", "h1"), scraper, Arc::new(OkValidator(false)), prober, st.clone());
        let out = eng.acquire(req()).await;
        assert_eq!(out, AcquireOutcome::NoAcceptableRelease);
        assert!(st.get_owned("h1".into()).await.is_none(), "rejected hash must not be recorded");
        assert!(st.is_blacklisted(27205, "h1".into()).await, "rejected hash must be blacklisted");
    }

    #[tokio::test]
    async fn bad_audio_blacklists_and_returns_no_acceptable() {
        let st = store();
        let scraper = Arc::new(MockScraper { candidates: vec![cand("h1", true)] });
        // probe returns only jpn audio; request wants eng (original_language eng) -> FailAudio
        let prober = Arc::new(CannedProber(Ok(vec![Track { kind: TrackKind::Audio, language: Some("jpn".into()) }])));
        let eng = engine(provider_returning("downloaded", "h1"), scraper, Arc::new(OkValidator(true)), prober, st.clone());
        let out = eng.acquire(req()).await;
        assert_eq!(out, AcquireOutcome::NoAcceptableRelease);
        assert!(st.is_blacklisted(27205, "h1".into()).await);
        assert!(st.get_owned("h1".into()).await.is_none());
    }

    #[tokio::test]
    async fn uncached_pick_returns_pending() {
        let st = store();
        let scraper = Arc::new(MockScraper { candidates: vec![cand("h1", false)] });
        let prober = Arc::new(CannedProber(Ok(vec![])));
        let eng = engine(provider_returning("downloading", "h1"), scraper, Arc::new(OkValidator(true)), prober, st.clone());
        let out = eng.acquire(req()).await;
        assert_eq!(out, AcquireOutcome::Pending("h1".into()));
        assert_eq!(st.get_owned("h1".into()).await.unwrap().status, OwnedStatus::Pending);
    }

    #[tokio::test]
    async fn inconclusive_probe_accepts() {
        let st = store();
        let scraper = Arc::new(MockScraper { candidates: vec![cand("h1", true)] });
        let prober = Arc::new(CannedProber(Err(ProbeError::Unsupported)));
        let eng = engine(provider_returning("downloaded", "h1"), scraper, Arc::new(OkValidator(true)), prober, st.clone());
        let out = eng.acquire(req()).await;
        assert_eq!(out, AcquireOutcome::Acquired("h1".into()));
    }

    #[tokio::test]
    async fn already_owned_is_idempotent() {
        let st = store();
        st.put_owned("h1".into(), crate::store::OwnedRecord { request: req(), source: "manual".into(), added_at: 1, status: OwnedStatus::Verified }).await.unwrap();
        let scraper = Arc::new(MockScraper { candidates: vec![cand("h1", true)] });
        let prober = Arc::new(CannedProber(Ok(vec![])));
        let eng = engine(provider_returning("downloaded", "h1"), scraper, Arc::new(OkValidator(true)), prober, st.clone());
        let out = eng.acquire(req()).await;
        assert_eq!(out, AcquireOutcome::Acquired("h1".into()));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib acquire`
Expected: FAIL — types/engine undefined.

- [ ] **Step 3: Outcome + seams** — insert above the test module in `src/acquire.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireOutcome {
    Acquired(String),       // confirmed (cached + verified/accepted)
    Pending(String),        // added; downloading or probe deferred — observe finishes it
    NoAcceptableRelease,    // no candidate passed (incl. all blacklisted / above ceiling / wrong-title)
    TemporarilyUnavailable, // scraper unreachable; retry later
}

/// Validates an acquired file genuinely matches the requested title (reuses `identify_name`).
#[async_trait]
pub trait TitleValidator: Send + Sync {
    async fn validate(&self, file_name: &str, expected_tmdb_id: u64, kind: MediaKind, season: Option<u32>, episode: Option<u32>) -> bool;
}

/// Probes a cached file's tracks (seam over `probe::probe_tracks` for testability).
#[async_trait]
pub trait Prober: Send + Sync {
    async fn probe(&self, cdn_url: &str) -> Result<Vec<Track>, ProbeError>;
}

pub struct HttpProber {
    pub http: reqwest::Client,
}
#[async_trait]
impl Prober for HttpProber {
    async fn probe(&self, cdn_url: &str) -> Result<Vec<Track>, ProbeError> {
        probe::probe_tracks(&self.http, cdn_url).await
    }
}

pub struct TmdbTitleValidator {
    pub tmdb: Arc<crate::tmdb_client::TmdbClient>,
}
#[async_trait]
impl TitleValidator for TmdbTitleValidator {
    async fn validate(&self, file_name: &str, expected: u64, kind: MediaKind, season: Option<u32>, episode: Option<u32>) -> bool {
        // Reuse the existing identification logic; confident == resolves to the expected tmdb id.
        let meta = crate::identification::identify_name(file_name, &self.tmdb).await;
        let id_ok = matches!(&meta, Some(m) if m.external_id.as_deref() == Some(format!("tmdb:{}", expected).as_str()));
        if !id_ok {
            return false;
        }
        if kind == MediaKind::Series {
            matches!((season, episode, parse_se(file_name)), (Some(s), Some(e), Some((fs, fe))) if fs == s && fe == e)
        } else {
            true
        }
    }
}

fn parse_se(name: &str) -> Option<(u32, u32)> {
    use regex::Regex;
    use std::sync::LazyLock;
    static SE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)s(\d{1,2})e(\d{1,3})").unwrap());
    let c = SE.captures(name)?;
    Some((c.get(1)?.as_str().parse().ok()?, c.get(2)?.as_str().parse().ok()?))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
```

> **Verify at implementation:** `crate::identification::identify_name`'s exact signature/async-ness and that `MediaMetadata.external_id` is `Some("tmdb:<id>")`. If `identify_name` is named differently or takes the whole `TorrentInfo`, adapt the validator to call the closest single-name identification entry point. If none exists, add a thin `identify_name(&str, &TmdbClient) -> Option<MediaMetadata>` wrapper in `identification.rs`.

- [ ] **Step 4: Engine struct + helpers** — append to `src/acquire.rs`:

```rust
pub struct AcquisitionEngine {
    provider: Arc<dyn DebridProvider>,
    scraper: Arc<dyn Scraper>,
    validator: Arc<dyn TitleValidator>,
    prober: Arc<dyn Prober>,
    store: Store,
    prefs: QualityPrefs,
    max_attempts: u32,
    stall_timeout: Duration,
    /// torrent_id -> (last progress, when first seen at that progress) for stall detection.
    progress: Arc<Mutex<HashMap<String, (f64, Instant)>>>,
}

/// Choose file ids to select for a candidate: the addon's named/index file, else the largest video.
fn select_file_ids(info: &TorrentInfo, file_hint: Option<&str>, file_idx: Option<usize>) -> Vec<u32> {
    if let Some(hint) = file_hint {
        let hint_base = hint.rsplit('/').next().unwrap_or(hint);
        if let Some(f) = info.files.iter().find(|f| f.path.rsplit('/').next().unwrap_or(&f.path) == hint_base) {
            return vec![f.id];
        }
    }
    if let Some(idx) = file_idx {
        if let Some(f) = info.files.get(idx) {
            return vec![f.id];
        }
    }
    info.files
        .iter()
        .filter(|f| crate::vfs::is_video_file(&f.path))
        .max_by_key(|f| f.bytes)
        .map(|f| vec![f.id])
        .unwrap_or_default()
}

/// Build a FileLocator for `path` within `info` (pairs the per-file link by position among selected).
fn locator_for(info: &TorrentInfo, hash: &str, path: &str) -> FileLocator {
    let mut link_idx = 0;
    for f in &info.files {
        if f.selected == 1 {
            if f.path == path {
                return FileLocator {
                    hash: hash.to_string(),
                    torrent_id: info.id.clone(),
                    file_id: f.id,
                    file_path: path.to_string(),
                    link: info.links.get(link_idx).cloned(),
                };
            }
            link_idx += 1;
        }
    }
    FileLocator { hash: hash.to_string(), torrent_id: info.id.clone(), file_id: 0, file_path: path.to_string(), link: None }
}

enum CandidateResult {
    Done(AcquireOutcome),
    Next,
}

enum VerifyResult {
    Pass,
    Accept,
    Reject(&'static str),
    Defer,
}
```

- [ ] **Step 5: `acquire()` + `try_candidate()` + `verify_file()`** — append to `src/acquire.rs` (inside `impl AcquisitionEngine`):

```rust
impl AcquisitionEngine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn DebridProvider>,
        scraper: Arc<dyn Scraper>,
        validator: Arc<dyn TitleValidator>,
        prober: Arc<dyn Prober>,
        store: Store,
        prefs: QualityPrefs,
        max_attempts: u32,
        stall_timeout: Duration,
    ) -> Self {
        Self { provider, scraper, validator, prober, store, prefs, max_attempts, stall_timeout, progress: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub async fn acquire(&self, req: AcquireRequest) -> AcquireOutcome {
        let candidates = match self.scraper.find(&req.imdb_id, req.kind, req.season, req.episode).await {
            Ok(c) => c,
            Err(e) => {
                warn!("scrape failed for {}: {}", req.imdb_id, e);
                return AcquireOutcome::TemporarilyUnavailable;
            }
        };
        let mut parsed: Vec<ReleaseInfo> = Vec::new();
        for c in &candidates {
            let r = release::parse(c);
            if self.store.is_blacklisted(req.tmdb_id, r.info_hash.clone()).await {
                continue;
            }
            parsed.push(r);
        }
        let ranked = release::rank(parsed, &self.prefs);

        let mut attempts = 0u32;
        for cand in ranked {
            if attempts >= self.max_attempts {
                break;
            }
            attempts += 1;
            if self.store.get_owned(cand.info_hash.clone()).await.is_some() {
                return AcquireOutcome::Acquired(cand.info_hash.clone()); // idempotent
            }
            match self.try_candidate(&req, &cand).await {
                CandidateResult::Done(o) => return o,
                CandidateResult::Next => continue,
            }
        }
        AcquireOutcome::NoAcceptableRelease
    }

    async fn try_candidate(&self, req: &AcquireRequest, cand: &ReleaseInfo) -> CandidateResult {
        let hash = cand.info_hash.clone();
        let hint = cand.file_name.clone();
        let idx = cand.file_idx;
        let selector = move |info: &TorrentInfo| select_file_ids(info, hint.as_deref(), idx);

        let (new_id, _post) = match crate::reacquire::materialise(&*self.provider, &hash, Duration::from_secs(1), selector).await {
            Ok(p) => p,
            Err(e) => {
                warn!("materialise failed for {}: {} — trying next", hash, e);
                return CandidateResult::Next;
            }
        };

        let final_info = match self.provider.get_torrent_info(&new_id).await {
            Ok(i) => i,
            Err(e) => {
                warn!("get_torrent_info failed for {}: {}", new_id, e);
                let _ = self.provider.delete_torrent(&new_id).await;
                return CandidateResult::Next;
            }
        };
        let Some(selected_path) = final_info.files.iter().find(|f| f.selected == 1).map(|f| f.path.clone()) else {
            let _ = self.provider.delete_torrent(&new_id).await;
            return CandidateResult::Next;
        };
        let file_name = selected_path.rsplit('/').next().unwrap_or(&selected_path).to_string();

        // STRICT title validation BEFORE recording anything.
        if !self.validator.validate(&file_name, req.tmdb_id, req.kind, req.season, req.episode).await {
            warn!("title validation rejected {} (hash {})", file_name, hash);
            let _ = self.store.blacklist_add(req.tmdb_id, hash.clone(), "WrongTitle", now_secs()).await;
            let _ = self.provider.delete_torrent(&new_id).await;
            return CandidateResult::Next;
        }

        let _ = self.store.put_owned(hash.clone(), crate::store::OwnedRecord {
            request: req.clone(),
            source: "manual".to_string(),
            added_at: now_secs(),
            status: OwnedStatus::Pending,
        }).await;
        let _ = self.store.put_authoritative(hash.clone(), req.metadata.clone()).await;

        if final_info.status != "downloaded" {
            info!("acquired {} (downloading; verify on completion)", hash);
            return CandidateResult::Done(AcquireOutcome::Pending(hash));
        }

        let locator = locator_for(&final_info, &hash, &selected_path);
        match self.verify_file(&locator, req).await {
            VerifyResult::Pass | VerifyResult::Accept => {
                let _ = self.store.set_owned_status(hash.clone(), OwnedStatus::Verified).await;
                CandidateResult::Done(AcquireOutcome::Acquired(hash))
            }
            VerifyResult::Defer => CandidateResult::Done(AcquireOutcome::Pending(hash)),
            VerifyResult::Reject(reason) => {
                warn!("probe rejected {} ({}) — blacklisting", hash, reason);
                let _ = self.store.blacklist_add(req.tmdb_id, hash.clone(), reason, now_secs()).await;
                let _ = self.store.remove_owned(hash.clone()).await;
                let _ = self.store.remove_authoritative(hash.clone()).await;
                let _ = self.provider.delete_torrent(&new_id).await;
                CandidateResult::Next
            }
        }
    }

    async fn verify_file(&self, locator: &FileLocator, req: &AcquireRequest) -> VerifyResult {
        let url = match self.provider.resolve_url(locator).await {
            Ok(u) => u,
            Err(_) => return VerifyResult::Defer, // can't reach it now; observe retries
        };
        let langreq = probe::LangReq {
            audio: self.prefs.audio.clone(),
            subtitle: self.prefs.subtitle.clone(),
            original_language: req.original_language.clone(),
        };
        match self.prober.probe(&url).await {
            Ok(tracks) => match probe::verify(&tracks, &langreq) {
                Verify::Pass => VerifyResult::Pass,
                Verify::FailAudio => VerifyResult::Reject("BadAudio"),
                Verify::FailSubtitle => VerifyResult::Reject("BadSubtitle"),
                Verify::Inconclusive => VerifyResult::Accept,
            },
            Err(ProbeError::Corrupt) => VerifyResult::Reject("Corrupt"),
            Err(ProbeError::Unsupported) | Err(ProbeError::TracksNotFound) => VerifyResult::Accept,
            Err(ProbeError::Transient) => VerifyResult::Defer,
        }
    }
}
```

- [ ] **Step 6: `observe()`** — append inside `impl AcquisitionEngine`:

```rust
    /// Called each scan tick with the current torrent list. Probes completed Pending owned
    /// torrents, and re-acquires owned torrents that have stalled/died/failed verification.
    pub async fn observe(&self, torrents: &[crate::rd_client::Torrent]) {
        let owned = self.store.all_owned().await;
        let by_hash: HashMap<&str, &crate::rd_client::Torrent> =
            torrents.iter().map(|t| (t.hash.as_str(), t)).collect();

        for (hash, rec) in owned {
            let Some(t) = by_hash.get(hash.as_str()).copied() else {
                continue; // not in the current account listing; leave it
            };
            let dead = matches!(t.status.as_str(), "magnet_error" | "dead" | "error" | "virus");
            if dead {
                self.fail_and_reacquire(&hash, &t.id, &rec.request, "Dead").await;
                continue;
            }
            if t.status == "downloaded" {
                if rec.status == OwnedStatus::Pending {
                    self.verify_pending(&hash, &t.id, &rec.request).await;
                }
                self.progress.lock().await.remove(&t.id);
                continue;
            }
            // still downloading — stall check
            if self.is_stalled(&t.id, t.progress).await {
                self.fail_and_reacquire(&hash, &t.id, &rec.request, "Stalled").await;
            }
        }
    }

    async fn is_stalled(&self, torrent_id: &str, progress: f64) -> bool {
        let mut map = self.progress.lock().await;
        let entry = map.entry(torrent_id.to_string()).or_insert((progress, Instant::now()));
        if (progress - entry.0).abs() > f64::EPSILON {
            *entry = (progress, Instant::now()); // progressed — reset
            false
        } else {
            entry.1.elapsed() >= self.stall_timeout
        }
    }

    async fn verify_pending(&self, hash: &str, torrent_id: &str, req: &AcquireRequest) {
        let info = match self.provider.get_torrent_info(torrent_id).await {
            Ok(i) => i,
            Err(_) => return,
        };
        let Some(path) = info.files.iter().find(|f| f.selected == 1).map(|f| f.path.clone()) else {
            return;
        };
        let locator = locator_for(&info, hash, &path);
        match self.verify_file(&locator, req).await {
            VerifyResult::Pass | VerifyResult::Accept => {
                let _ = self.store.set_owned_status(hash.to_string(), OwnedStatus::Verified).await;
            }
            VerifyResult::Defer => {} // retry next tick
            VerifyResult::Reject(reason) => self.fail_and_reacquire(hash, torrent_id, req, reason).await,
        }
    }

    async fn fail_and_reacquire(&self, hash: &str, torrent_id: &str, req: &AcquireRequest, reason: &str) {
        warn!("owned torrent {} failed ({}) — blacklist + re-acquire", hash, reason);
        let _ = self.store.blacklist_add(req.tmdb_id, hash.to_string(), reason, now_secs()).await;
        let _ = self.store.remove_owned(hash.to_string()).await;
        let _ = self.store.remove_authoritative(hash.to_string()).await;
        let _ = self.provider.delete_torrent(torrent_id).await;
        self.progress.lock().await.remove(torrent_id);
        let _ = self.acquire(req.clone()).await; // promotes the next candidate (re-scrape; bad hash now blacklisted)
    }
}
```

- [ ] **Step 7: Declare module + run tests**

Add `pub mod acquire;` to `src/mapper.rs`. Run: `cargo test --lib acquire`
Expected: PASS (all 6 acquire tests).

- [ ] **Step 8: Full suite + commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/acquire.rs src/mapper.rs
git commit -m "feat(acquire): acquisition engine with strict validation + probe + observe" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Wiring — `AppState`, identification hook, `observe`

**Files:**
- Modify: `src/app_state.rs` (add `scraper` + `engine` handles)
- Modify: `src/main.rs` (construct scraper/validator/prober/engine into `AppState`)
- Modify: `src/tasks.rs` (authoritative identification hook + `observe` call + update the structural test)

- [ ] **Step 1: Add fields to `AppState`** — in `src/app_state.rs`, add imports and two fields:

```rust
use crate::acquire::AcquisitionEngine;
use crate::scraper::Scraper;
```
and inside `pub struct AppState { ... }`:
```rust
    pub scraper: Arc<dyn Scraper>,
    pub engine: Arc<AcquisitionEngine>,
```

- [ ] **Step 2: Construct them in `main.rs`** — in `src/main.rs`, after `store` and `tmdb_client` and the `http_client` are available but **before** the `AppState { ... }` literal (which moves `config`), build the engine using `config.acquisition.*`:

```rust
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build CDN HTTP client");

    let scraper: Arc<dyn debridmoviemapper::scraper::Scraper> =
        Arc::new(debridmoviemapper::scraper::TorrentioScraper::new(
            config.acquisition.scraper_addon_url.clone(),
            config.provider_kind,
            &config.provider_token,
            http_client.clone(),
        ));
    let validator: Arc<dyn debridmoviemapper::acquire::TitleValidator> =
        Arc::new(debridmoviemapper::acquire::TmdbTitleValidator { tmdb: tmdb_client.clone() });
    let prober: Arc<dyn debridmoviemapper::acquire::Prober> =
        Arc::new(debridmoviemapper::acquire::HttpProber { http: http_client.clone() });
    let engine = Arc::new(debridmoviemapper::acquire::AcquisitionEngine::new(
        provider.clone(),
        scraper.clone(),
        validator,
        prober,
        store.clone(),
        config.acquisition.prefs.clone(),
        config.acquisition.max_acquire_attempts,
        std::time::Duration::from_secs(config.acquisition.stall_timeout_secs),
    ));
```

Then build `AppState` with the new fields and reuse this `http_client` (replace the inline `http_client` construction inside the `AppState { ... }` literal from SP0 with the local built above):

```rust
    let app_state = AppState {
        provider: provider.clone(),
        tmdb_client: tmdb_client.clone(),
        vfs: vfs.clone(),
        store: store.clone(),
        repair_manager: repair_manager.clone(),
        config: Arc::new(config),
        jellyfin_client,
        http_client: http_client.clone(),
        scraper: scraper.clone(),
        engine: engine.clone(),
    };
```

(Keep the existing `DebridFileSystem::new(app_state.provider.clone(), app_state.vfs.clone(), app_state.repair_manager.clone(), app_state.http_client.clone())` and the `ScanConfig { app: app_state.clone() }` from SP0 — they're unchanged. The standalone `http_client` built earlier in SP0's main now lives above; remove the duplicate construction so there's a single `http_client`.)

- [ ] **Step 3: Extract the identification helper + write its test** — in `src/tasks.rs`, add a helper and a test (the override branch is deterministic; it returns the stored metadata without any TMDB call):

```rust
/// Resolve a torrent's metadata: an authoritative `hash → MediaMetadata` (recorded by the
/// acquisition engine for content we chose) wins over filename-based TMDB identification.
async fn resolve_metadata(
    store: &Store,
    tmdb_client: &TmdbClient,
    info: &crate::rd_client::TorrentInfo,
) -> MediaMetadata {
    match store.authoritative_meta(info.hash.clone()).await {
        Some(m) => m,
        None => identify_torrent(info, tmdb_client).await,
    }
}
```

Add to `tasks.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn authoritative_metadata_overrides_identification() {
        use crate::store::Store;
        use crate::vfs::{MediaMetadata, MediaType};
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder().create_with_backend(redb::backends::InMemoryBackend::new()).unwrap(),
        )).unwrap();
        let meta = MediaMetadata { title: "Authoritative".into(), year: Some("2020".into()), media_type: MediaType::Movie, external_id: Some("tmdb:99".into()) };
        store.put_authoritative("HASH".to_string(), meta.clone()).await.unwrap();
        let tmdb = TmdbClient::new("k".to_string()).unwrap();
        let info = crate::rd_client::TorrentInfo { hash: "HASH".into(), filename: "totally.unrelated.name.mkv".into(), ..Default::default() };
        // Authoritative entry present → returned verbatim, no TMDB lookup on the filename.
        let got = resolve_metadata(&store, &tmdb, &info).await;
        assert_eq!(got.title, "Authoritative");
        assert_eq!(got.external_id.as_deref(), Some("tmdb:99"));
    }
```

- [ ] **Step 4: Use the helper + call `observe` in `run_scan_loop`** — in `src/tasks.rs`:
  1. Add `engine` to the `AppState` destructure at the top of `run_scan_loop` (alongside `provider`, `store`, etc.): `engine,`.
  2. Inside the `Ok(torrents) =>` arm of `match provider.get_torrents().await`, **before** the dedup/identify work, add: `engine.observe(&torrents).await;`
  3. In the identification stream closure, replace `let metadata = identify_torrent(&info, &tmdb_client).await;` with `let metadata = resolve_metadata(&store, &tmdb_client, &info).await;` and clone `store` into the closure (add `let store = store.clone();` next to the existing `let provider = provider.clone();` / `let tmdb_client = tmdb_client.clone();`).

- [ ] **Step 5: Update the structural test** — in `tasks.rs`'s `scan_config_holds_app_state`, add the two new `AppState` fields by constructing real (unused) engine deps:

```rust
        // ... existing provider/tmdb/vfs/store/repair/config/jellyfin/http_client setup ...
        let scraper: std::sync::Arc<dyn crate::scraper::Scraper> =
            std::sync::Arc::new(crate::scraper::TorrentioScraper::new(None, crate::provider::ProviderKind::TorBox, "tok", reqwest::Client::new()));
        let validator: std::sync::Arc<dyn crate::acquire::TitleValidator> =
            std::sync::Arc::new(crate::acquire::TmdbTitleValidator { tmdb: std::sync::Arc::new(TmdbClient::new("k".to_string()).unwrap()) });
        let prober: std::sync::Arc<dyn crate::acquire::Prober> =
            std::sync::Arc::new(crate::acquire::HttpProber { http: reqwest::Client::new() });
        let engine = std::sync::Arc::new(crate::acquire::AcquisitionEngine::new(
            provider.clone(), scraper.clone(), validator, prober, store.clone(),
            crate::config::AcquisitionConfig::default().prefs, 5, std::time::Duration::from_secs(1800),
        ));
        let app = AppState {
            // ... existing fields ...
            scraper,
            engine,
        };
```

(Adapt to the exact field set the SP0 test already builds; the point is the two new fields compile.)

- [ ] **Step 6: Run tests + commit**

Run: `cargo test`
Expected: PASS (incl. the new `authoritative_metadata_overrides_identification`). The scan loop now consults authoritative ids and drives `observe` (a no-op until something is owned, so existing deployments are unaffected).

```bash
git add src/app_state.rs src/main.rs src/tasks.rs
git commit -m "feat: wire acquisition engine into AppState, scan loop (observe + authoritative ids)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Temporary `--acquire` CLI trigger

**Files:**
- Modify: `src/tmdb_client.rs` (add a details-by-id fetch for metadata)
- Modify: `src/main.rs` (add the `--acquire` mode)

- [ ] **Step 1: Add a TMDB details fetch + parse test** — append to `src/tmdb_client.rs`:

```rust
/// Parse a TMDB movie/tv details object into (title, year, original_language).
pub(crate) fn parse_details(v: &serde_json::Value, kind: crate::vfs::MediaType) -> (String, Option<String>, Option<String>) {
    let title = match kind {
        crate::vfs::MediaType::Movie => v.get("title"),
        crate::vfs::MediaType::Show => v.get("name"),
    }
    .and_then(|t| t.as_str())
    .unwrap_or("")
    .to_string();
    let date = match kind {
        crate::vfs::MediaType::Movie => v.get("release_date"),
        crate::vfs::MediaType::Show => v.get("first_air_date"),
    }
    .and_then(|d| d.as_str())
    .unwrap_or("");
    let year = date.split('-').next().filter(|y| y.len() == 4).map(String::from);
    let original_language = v.get("original_language").and_then(|l| l.as_str()).map(String::from);
    (title, year, original_language)
}
```

and a method:

```rust
    /// Fetch (title, year, original_language) for a TMDB id.
    pub async fn details(&self, tmdb_id: u64, kind: crate::vfs::MediaType) -> Result<(String, Option<String>, Option<String>), reqwest::Error> {
        let path = match kind { crate::vfs::MediaType::Movie => "movie", crate::vfs::MediaType::Show => "tv" };
        let url = format!("https://api.themoviedb.org/3/{}/{}", path, tmdb_id);
        let v: serde_json::Value = self.client.get(&url).query(&[("api_key", self.api_key.as_str())]).send().await?.json().await?;
        Ok(parse_details(&v, kind))
    }
```

and a test:

```rust
    #[test]
    fn parse_details_movie_and_show() {
        let m = serde_json::json!({"title": "Inception", "release_date": "2010-07-16", "original_language": "en"});
        assert_eq!(super::parse_details(&m, crate::vfs::MediaType::Movie), ("Inception".into(), Some("2010".into()), Some("en".into())));
        let s = serde_json::json!({"name": "Breaking Bad", "first_air_date": "2008-01-20", "original_language": "en"});
        assert_eq!(super::parse_details(&s, crate::vfs::MediaType::Show), ("Breaking Bad".into(), Some("2008".into()), Some("en".into())));
    }
```

Run `cargo test --lib tmdb_client` → PASS.

- [ ] **Step 2: Add the `--acquire` mode to `main.rs`** — after the `AppState`/`engine` is built (Task 9) and **before** binding the WebDAV listener, insert a one-shot mode. TEMPORARY — clearly flagged for removal:

```rust
    // TEMPORARY (SP1) dev/verification trigger — remove once SP2 (Trakt) / SP4 (ad-hoc add) exist.
    // Usage: --acquire <movie|series> <imdb-or-tmdb-id> [season episode]
    if let Some(pos) = std::env::args().position(|a| a == "--acquire") {
        let args: Vec<String> = std::env::args().collect();
        let kind_s = args.get(pos + 1).cloned().unwrap_or_default();
        let id_s = args.get(pos + 2).cloned().unwrap_or_default();
        let season = args.get(pos + 3).and_then(|s| s.parse::<u32>().ok());
        let episode = args.get(pos + 4).and_then(|s| s.parse::<u32>().ok());
        let kind = match kind_s.as_str() {
            "movie" => debridmoviemapper::scraper::MediaKind::Movie,
            "series" => debridmoviemapper::scraper::MediaKind::Series,
            other => {
                eprintln!("--acquire: kind must be 'movie' or 'series', got '{}'", other);
                std::process::exit(2);
            }
        };
        let media_type = match kind {
            debridmoviemapper::scraper::MediaKind::Movie => debridmoviemapper::vfs::MediaType::Movie,
            debridmoviemapper::scraper::MediaKind::Series => debridmoviemapper::vfs::MediaType::Show,
        };
        // Resolve both ids.
        let (imdb_id, tmdb_id) = if id_s.starts_with("tt") {
            match tmdb_client.find_by_imdb(&id_s).await {
                Ok(Some((tid, _))) => (id_s.clone(), tid),
                _ => { eprintln!("--acquire: could not resolve IMDB id {}", id_s); std::process::exit(2); }
            }
        } else {
            let tid: u64 = id_s.parse().unwrap_or_else(|_| { eprintln!("--acquire: invalid id {}", id_s); std::process::exit(2); });
            match tmdb_client.external_imdb_id(tid, media_type.clone()).await {
                Ok(Some(imdb)) => (imdb, tid),
                _ => { eprintln!("--acquire: could not resolve IMDB id for tmdb {}", tid); std::process::exit(2); }
            }
        };
        let (title, year, original_language) = tmdb_client.details(tmdb_id, media_type.clone()).await.unwrap_or_default();
        let req = debridmoviemapper::store::AcquireRequest {
            imdb_id,
            tmdb_id,
            kind,
            season,
            episode,
            original_language,
            metadata: debridmoviemapper::vfs::MediaMetadata {
                title,
                year,
                media_type,
                external_id: Some(format!("tmdb:{}", tmdb_id)),
            },
        };
        let outcome = engine.acquire(req).await;
        println!("--acquire outcome: {:?}", outcome);
        return Ok(());
    }
```

> Note `--acquire` runs after the normal startup wiring (config/provider/store/engine), unlike `--healthcheck` which exits before any wiring. It does not start the WebDAV server. `AcquireOutcome` must derive `Debug` (it does, Task 8).

- [ ] **Step 3: Build + manual smoke (optional, needs tokens) + commit**

Run: `cargo build && cargo test`
Expected: PASS.

Optional manual check (needs real tokens + a Creative-Commons title, cleans up after):
```bash
RD_API_TOKEN=... TMDB_API_KEY=... cargo run -- --acquire movie tt1727587   # Sintel
```
Expected: prints `--acquire outcome: Acquired(...)` or `Pending(...)`; the torrent appears in the account. (Delete it afterwards.)

```bash
git add src/tmdb_client.rs src/main.rs
git commit -m "feat: temporary --acquire CLI trigger for SP1 verification" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Live smoke tests, lifecycle extension, docs, gate

**Files:**
- Create: `tests/sp1_live_test.rs`
- Modify: `tests/lifecycle_test.rs`
- Modify: `CLAUDE.md`, `README.md`

- [ ] **Step 1: Scraper live smoke** — create `tests/sp1_live_test.rs`:

```rust
//! SP1 live smoke tests. `#[ignore]`; require real tokens in `.env`.
use debridmoviemapper::provider::{choose_provider, ProviderKind};
use debridmoviemapper::scraper::{MediaKind, Scraper, TorrentioScraper};

fn provider_from_env() -> Option<(ProviderKind, String)> {
    dotenvy::dotenv().ok();
    choose_provider(std::env::var("RD_API_TOKEN").ok(), std::env::var("TORBOX_API_KEY").ok()).ok()
}

#[tokio::test]
#[ignore]
async fn scraper_live_returns_parseable_streams() {
    let Some((kind, token)) = provider_from_env() else {
        eprintln!("skipping: no provider token");
        return;
    };
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap();
    let scraper = TorrentioScraper::new(std::env::var("SCRAPER_ADDON_URL").ok(), kind, &token, http);
    // Sintel (Creative Commons): tt1727587
    let cands = scraper.find("tt1727587", MediaKind::Movie, None, None).await.expect("scrape");
    assert!(!cands.is_empty(), "expected at least one stream for Sintel — check the Torrentio URL/option format");
    assert!(cands.iter().all(|c| !c.info_hash.is_empty()));
    eprintln!("scraper_live: {} candidates; first cached flag from parse: {:?}", cands.len(), debridmoviemapper::release::parse(&cands[0]).cached);
}
```

> This test is the guard for the "exact Torrentio option-string" open item: if the templated URL is wrong, it returns zero streams and fails loudly. Adjust `TorrentioScraper::build_base_url`'s option syntax until it passes against the live addon.

- [ ] **Step 2: Lifecycle extension (cross-provider acquire)** — in `tests/lifecycle_test.rs`, add an acquisition test mirroring the existing add→appears→delete→disappears pattern. It builds a real engine and acquires Sintel **by IMDB id**, then cleans up the service-owned torrent. Use a temp DB and skip when the provider token is unset.

```rust
#[tokio::test]
#[ignore]
async fn lifecycle_acquire_sintel_by_imdb() {
    dotenvy::dotenv().ok();
    let Ok((kind, token)) = debridmoviemapper::provider::choose_provider(
        std::env::var("RD_API_TOKEN").ok(),
        std::env::var("TORBOX_API_KEY").ok(),
    ) else {
        eprintln!("skipping: no provider token");
        return;
    };
    let Ok(tmdb_key) = std::env::var("TMDB_API_KEY") else {
        eprintln!("skipping: no TMDB key");
        return;
    };

    use std::sync::Arc;
    let provider: Arc<dyn debridmoviemapper::provider::DebridProvider> = match kind {
        debridmoviemapper::provider::ProviderKind::RealDebrid =>
            Arc::new(debridmoviemapper::rd_client::RealDebridClient::new(token.clone()).unwrap()),
        debridmoviemapper::provider::ProviderKind::TorBox =>
            Arc::new(debridmoviemapper::torbox_client::TorBoxClient::new(token.clone()).unwrap()),
    };
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().unwrap();
    let tmdb = Arc::new(debridmoviemapper::tmdb_client::TmdbClient::new(tmdb_key).unwrap());
    let mut dbp = std::env::temp_dir();
    dbp.push(format!("dmm_sp1_lifecycle_{}.redb", std::process::id()));
    let store = debridmoviemapper::store::Store::open(dbp.to_str().unwrap()).unwrap();
    let scraper: Arc<dyn debridmoviemapper::scraper::Scraper> = Arc::new(
        debridmoviemapper::scraper::TorrentioScraper::new(std::env::var("SCRAPER_ADDON_URL").ok(), kind, &token, http.clone()));
    let validator: Arc<dyn debridmoviemapper::acquire::TitleValidator> =
        Arc::new(debridmoviemapper::acquire::TmdbTitleValidator { tmdb: tmdb.clone() });
    let prober: Arc<dyn debridmoviemapper::acquire::Prober> =
        Arc::new(debridmoviemapper::acquire::HttpProber { http: http.clone() });
    let engine = debridmoviemapper::acquire::AcquisitionEngine::new(
        provider.clone(), scraper, validator, prober, store.clone(),
        debridmoviemapper::config::AcquisitionConfig::default().prefs, 5, std::time::Duration::from_secs(1800));

    let req = debridmoviemapper::store::AcquireRequest {
        imdb_id: "tt1727587".into(), // Sintel
        tmdb_id: 45745,              // Sintel on TMDB
        kind: debridmoviemapper::scraper::MediaKind::Movie,
        season: None, episode: None,
        original_language: Some("en".into()),
        metadata: debridmoviemapper::vfs::MediaMetadata {
            title: "Sintel".into(), year: Some("2010".into()),
            media_type: debridmoviemapper::vfs::MediaType::Movie,
            external_id: Some("tmdb:45745".into()),
        },
    };
    let outcome = engine.acquire(req).await;
    eprintln!("acquire outcome: {:?}", outcome);
    assert!(matches!(outcome,
        debridmoviemapper::acquire::AcquireOutcome::Acquired(_)
        | debridmoviemapper::acquire::AcquireOutcome::Pending(_)),
        "Sintel should be acquirable; got {:?}", outcome);

    // Cleanup: delete every service-owned torrent we just added.
    for (_hash, _rec) in store.all_owned().await {
        // find the torrent by hash and delete it
        if let Ok(torrents) = provider.get_torrents().await {
            for t in torrents.iter().filter(|t| t.hash == _hash) {
                let _ = provider.delete_torrent(&t.id).await;
            }
        }
    }
    let _ = std::fs::remove_file(&dbp);
    let _ = std::fs::remove_file(format!("{}.corrupt", dbp.to_str().unwrap()));
}
```

> Verify the Sintel TMDB id (45745) at implementation; if different, `find_by_imdb("tt1727587")` in the test can resolve it instead of hardcoding.

- [ ] **Step 3: Update `CLAUDE.md`** — add module rows (`scraper.rs`, `release.rs`, `probe.rs`, `reacquire.rs`, `acquire.rs`); a **Key Design Decision** describing the acquisition engine (scrape → score → materialise → strict identity validation → probe → fallback; `observe` for stall/completion; the authoritative `hash→MediaMetadata` override consulted before `identify_torrent`); note the v2 schema (`owned_hashes`/`authoritative_ids`/`blacklist`); and the new env vars. Add the `--acquire` temporary trigger under Commands with a "temporary, removed in a later phase" note.

- [ ] **Step 4: Update `README.md`** — add the acquisition env vars to the table: `SCRAPER_ADDON_URL` (optional; defaults to a Torrentio URL auto-built from your provider token), `MAX_RESOLUTION` (default 1080), `AUDIO_LANGUAGE` (default `original`), `SUBTITLE_LANGUAGE` (default `none`), `PREFER_HEVC` (true), `PREFER_HDR` (false), `STALL_TIMEOUT_SECS` (1800), `MAX_ACQUIRE_ATTEMPTS` (5). Add a short "Acquisition (SP1)" subsection summarising the behaviour and the temporary `--acquire` command.

- [ ] **Step 5: Full pre-commit gate**

Run:
```bash
cargo test \
  && INTEGRATION_TEST_LIMIT=10 cargo test --test integration_test -- --ignored \
  && INTEGRATION_TEST_LIMIT=10 cargo test --test repair_integration_test -- --ignored \
  && cargo test --test lifecycle_test -- --ignored
```
Expected: `cargo test` PASS (all new SP1 unit tests + existing suite; mind the known dav_fs flake). The `--ignored` suites pass/skip per available tokens; the new `lifecycle_acquire_sintel_by_imdb` runs when tokens are present and must succeed (or be reported as an environment/scraper-format issue, not a code regression).

- [ ] **Step 6: Commit**

```bash
git add tests/sp1_live_test.rs tests/lifecycle_test.rs CLAUDE.md README.md
git commit -m "test+docs: SP1 live smoke, lifecycle acquire extension, docs" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-review

**1. Spec coverage** (spec §/decision → task):
- S1 temporary trigger → T10. S2 repair shared primitive → T7. S3 re-scrape/blacklist-only state → T8 (`acquire` re-scrapes; `observe` re-acquires). S4 hand-rolled MKV/MP4 probe → T5. S5 unverifiable→accept + verifiable-container preference → T5 (`Unsupported`/`TracksNotFound`→Accept) + T3 (scoring bonus). S6 corrupt→blacklist+next → T5 (`Corrupt`) + T8 (`Reject("Corrupt")`). S7 probe taxonomy → T5 + T8 (`verify_file` mapping). S8 strict title validation → T8 (`try_candidate` validates before recording) + T2 (lookups) + T10. S9 scraper URL templating/override → T4. New tables + migration → T6. Identification hook → T9. Config knobs → T1. `--acquire` + tmdb details → T10. Tests (release/probe/reacquire/acquire/title/migration/backoff/override/live/lifecycle) → T3–T11. §6 known limitation (no body-corruption detection) → documented, no task needed.
- **Gap noted:** `backoff_429_test` from the spec is **not** a standalone task — the back-off lives in the provider's existing `AdaptiveRateLimiter` (SP0/unchanged) and `materialise` surfaces add errors as "try next". An explicit 429 unit test would need a provider-level HTTP mock the codebase lacks; **the implementer should add a `backoff_429_test` only if a provider HTTP-mock seam already exists**, otherwise rely on the existing rate-limiter tests and note the omission. (No silent gap: documented here.)

**2. Placeholder scan:** No `TBD`/`TODO`/"implement later". The several "verify at implementation" notes (identify_name signature, `redb::StorageError::Io` variant, `TmdbClient` field names/auth style, Torrentio option-string, Sintel TMDB id) are **specific** verification points each with a concrete fallback — not vague placeholders.

**3. Type consistency:** `AcquireRequest`/`OwnedRecord`/`OwnedStatus` defined in `store.rs` (T6), consumed in `acquire.rs` (T8) and `tasks.rs` (T9). `MediaKind` (T4) is serde-derived (noted) for use in `AcquireRequest`. `QualityPrefs`/`AudioReq`/`SubReq`/`MaxResolution` defined in `config.rs` (T1), used by `release.rs` (T3), `probe.rs::LangReq` (T5), `acquire.rs` (T8). `Track`/`Verify`/`ProbeError`/`LangReq` (T5) used by `acquire.rs` (T8) via the `Prober` seam. `Scraper`/`MediaKind`/`RawCandidate` flow T4→T3→T8. `AcquireOutcome` (T8) printed by `--acquire` (T10) and asserted in the lifecycle test (T11). `materialise` signature (T7) matches its callers in `repair.rs` (T7) and `acquire.rs` (T8). Consistent.

**Resolved during review:** added the `MediaKind` serde-derive note to T4; flagged the `backoff_429_test` conditionality above.

---

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-07-sp1-acquisition-engine.md`. Two execution options:

1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task with two-stage review (spec compliance, then code quality) between tasks, same as SP0.
2. **Inline Execution** — execute the tasks in this session via executing-plans with checkpoints.

Which approach?






