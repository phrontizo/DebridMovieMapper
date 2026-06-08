use crate::config::{AudioReq, QualityPrefs};
use regex::Regex;
use std::sync::LazyLock;

/// A raw stream from the scraper, before parsing.
#[derive(Debug, Clone)]
pub struct RawCandidate {
    pub name: String,
    pub description: String,
    pub info_hash: String,
    pub file_idx: Option<usize>,
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec { Hevc, Avc, Other }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container { Mkv, Mp4, Other }

impl Container {
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

/// Source/release tier. `Cam` covers cam/telesync/telecine/screener/R5/workprint/pre-DVD — these
/// are hard-rejected (the quality floor). The rest rank by tier (REMUX > BluRay > WEB > HDTV).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Cam,
    Remux,
    BluRay,
    Web,
    Hdtv,
    Other,
}

impl Source {
    /// Ranking bonus by tier. `Cam` is rejected before this is consulted.
    fn tier_score(self) -> i64 {
        match self {
            Source::Remux => 8_000,
            Source::BluRay => 6_000,
            Source::Web => 3_000,
            Source::Hdtv => 1_000,
            Source::Cam | Source::Other => 0,
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
    pub source: Source,
}

static RES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4})p\b|\b(4k|uhd)\b").unwrap());
static SIZE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)([\d.]+)\s*(gb|mb)").unwrap());
static SEED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\u{1f464}\s*(\d+)").unwrap());
static GROUP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-([A-Za-z0-9]+)$").unwrap());

/// Extract a trailing `-GROUP` token, rejecting common source/codec tokens that aren't groups.
fn extract_group(s: &str) -> Option<String> {
    const GROUP_DENY: &[&str] = &[
        "dl", "rip", "hevc", "avc", "hdr", "sdr", "cam", "ts", "hdtv", "bluray",
        "bdrip", "dvdrip", "web", "webdl", "webrip", "x264", "x265", "aac", "ac3", "dts", "ddp",
    ];
    let g = GROUP_RE.captures(s.trim())?.get(1)?.as_str();
    if GROUP_DENY.iter().any(|d| d.eq_ignore_ascii_case(g)) {
        None
    } else {
        Some(g.to_string())
    }
}

// Cam / telesync / telecine / screener / R5 / workprint / pre-DVD markers (the quality floor).
static CAM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(cam|cam-?rip|hd-?cam|hq-?cam|ts|hd-?ts|tele-?sync|hd-?tc|tele-?cine|scr|screener|dvd-?scr|bd-?scr|work-?print|r5|pre-?dvd|predvd)\b",
    )
    .unwrap()
});

/// Classify the release source from its (lowercased) name/description.
fn detect_source(lower: &str) -> Source {
    if CAM_RE.is_match(lower) {
        Source::Cam
    } else if lower.contains("remux") {
        Source::Remux
    } else if lower.contains("bluray")
        || lower.contains("blu-ray")
        || lower.contains("bdrip")
        || lower.contains("brrip")
    {
        Source::BluRay
    } else if lower.contains("web-dl")
        || lower.contains("webdl")
        || lower.contains("web.dl")
        || lower.contains("webrip")
        || lower.contains("web-rip")
        || lower.contains("amzn")
        || lower.contains("dsnp")
        || lower.contains("nf.web")
    {
        Source::Web
    } else if lower.contains("hdtv") {
        Source::Hdtv
    } else {
        Source::Other
    }
}

pub fn parse(c: &RawCandidate) -> ReleaseInfo {
    let text = format!("{}\n{}", c.name, c.description);
    let lower = text.to_ascii_lowercase();

    let resolution = RES_RE.captures(&text).and_then(|cap| {
        if let Some(m) = cap.get(1) { m.as_str().parse::<u16>().ok() } else { Some(2160) }
    });

    let codec = if lower.contains("x265") || lower.contains("h265") || lower.contains("hevc") {
        Codec::Hevc
    } else if lower.contains("x264") || lower.contains("h264") || lower.contains("avc") {
        Codec::Avc
    } else {
        Codec::Other
    };

    let hdr = lower.contains("hdr") || lower.contains("dolby vision") || lower.contains("dovi") || lower.contains(" dv ");

    let cached = lower.contains("rd+") || lower.contains("tb+") || text.contains('\u{26a1}');

    let size_bytes = SIZE_RE.captures(&text).and_then(|cap| {
        let n: f64 = cap.get(1)?.as_str().parse().ok()?;
        let unit = cap.get(2)?.as_str().to_ascii_lowercase();
        let mult = if unit == "gb" { 1_000_000_000.0 } else { 1_000_000.0 };
        Some((n * mult) as u64)
    });

    let seeders = SEED_RE.captures(&text).and_then(|cap| cap.get(1)?.as_str().parse::<u32>().ok());

    // Release group is the trailing "-GROUP". Prefer the file's stem; fall back to the
    // release-name line in the description (which usually carries the group).
    let group = c
        .file_name
        .as_deref()
        .map(|f| f.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(f))
        .and_then(extract_group)
        .or_else(|| c.description.lines().next().and_then(extract_group));

    let mut languages = Vec::new();
    for (word, code) in LANG_WORDS {
        if lower.contains(word) { languages.push((*code).to_string()); }
    }

    let container = c.file_name.as_deref().map(Container::from_name).unwrap_or(Container::Other);
    let source = detect_source(&lower);

    ReleaseInfo {
        info_hash: c.info_hash.clone(),
        file_idx: c.file_idx,
        file_name: c.file_name.clone(),
        resolution, codec, hdr, languages, group, size_bytes, seeders, cached, container, source,
    }
}

const LANG_WORDS: &[(&str, &str)] = &[
    ("english", "eng"), ("french", "fre"), ("german", "ger"), ("spanish", "spa"),
    ("italian", "ita"), ("russian", "rus"), ("hindi", "hin"), ("japanese", "jpn"),
    ("korean", "kor"), ("portuguese", "por"), ("multi", "mul"),
];

/// Score a release against prefs. `None` = excluded by a hard rule (resolution ceiling,
/// cam/telesync source, or an uncached zero-seeder release). Higher better.
pub fn score(r: &ReleaseInfo, prefs: &QualityPrefs) -> Option<i64> {
    // Quality floor: never acquire a cam / telesync / telecine / screener / R5 / workprint source.
    if r.source == Source::Cam {
        return None;
    }
    // A release with no parsed resolution is let through rather than excluded (don't
    // blindly drop potentially-valid releases the scraper failed to tag).
    if let Some(res) = r.resolution {
        if res > prefs.max_resolution.height() { return None; }
    }
    // An uncached release with zero seeders cannot download — ignore it entirely so the engine
    // never burns an acquire attempt on a dead torrent (nor leaves a "checking" magnet behind).
    // Cached releases are exempt: already on the provider, so live peers are irrelevant.
    if !r.cached && r.seeders == Some(0) {
        return None;
    }
    let mut s: i64 = 0;
    if r.cached { s += 1_000_000; }
    s += r.source.tier_score();
    s += r.resolution.unwrap_or(0) as i64 * 100;
    if prefs.prefer_hevc && r.codec == Codec::Hevc { s += 5_000; }
    if prefs.prefer_hdr && r.hdr { s += 3_000; }
    if r.container.is_verifiable() { s += 2_000; }
    s += (r.seeders.unwrap_or(0).min(1000) as i64) * 2;
    if let Some(sz) = r.size_bytes {
        // Reject fake/sample (<300 MB) and absurd (>80 GB) files. The upper bound is generous so
        // a legitimate REMUX (often 25–40 GB at 1080p — now our top source tier) isn't penalised.
        if sz < 300_000_000 || sz > 80_000_000_000 {
            s -= 4_000;
        } else {
            // Prefer higher bitrate (larger file) at a given resolution/source — a tiebreaker
            // capped well below the codec/source weights so it never overrides them.
            s += (sz / 1_000_000_000).min(15) as i64 * 200; // up to +3000 at ≥15 GB
        }
    }
    if let AudioReq::Lang(want) = &prefs.audio {
        if !r.languages.is_empty() && !r.languages.iter().any(|l| l == want || l == "mul") {
            s -= 50_000;
        }
    }
    Some(s)
}

pub fn rank(candidates: Vec<ReleaseInfo>, prefs: &QualityPrefs) -> Vec<ReleaseInfo> {
    let mut scored: Vec<(i64, ReleaseInfo)> = candidates.into_iter()
        .filter_map(|r| score(&r, prefs).map(|s| (s, r)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MaxResolution, SubReq};

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
            "Movie.2023.1080p.BluRay.x265.HDR.DDP5.1-GRP\n\u{1f4be} 8.4 GB \u{1f464} 42 \u{2699}\u{fe0f} ThePirateBay\nRD+",
            "abc",
            Some("Movie.2023.1080p.BluRay.x265.HDR.mkv"),
        );
        let r = parse(&c);
        assert_eq!(r.resolution, Some(1080));
        assert_eq!(r.codec, Codec::Hevc);
        assert!(r.hdr);
        assert_eq!(r.size_bytes, Some((8.4 * 1_000_000_000.0) as u64));
        assert_eq!(r.seeders, Some(42));
        assert!(r.cached);
        assert_eq!(r.container, Container::Mkv);
        assert_eq!(r.group.as_deref(), Some("GRP"));
    }

    #[test]
    fn parses_4k_and_avc_and_uncached() {
        let c = raw(
            "Torrentio\n4k",
            "Show.S01E02.2160p.WEB-DL.H264-XYZ\n\u{1f4be} 15 GB \u{1f464} 3",
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
            parse(&raw("t", "A.2160p.x265\nRD+", "h4k", Some("A.2160p.mkv"))),
            parse(&raw("t", "A.1080p.x265", "hu", Some("A.1080p.mkv"))),
            parse(&raw("t", "A.1080p.x265\nRD+", "hc", Some("A.1080p.mkv"))),
        ];
        let ranked = rank(cands, &prefs());
        assert_eq!(ranked.len(), 2, "the 2160p candidate is dropped");
        assert_eq!(ranked[0].info_hash, "hc", "cached ranks first");
        assert_eq!(ranked[1].info_hash, "hu");
    }

    #[test]
    fn score_downranks_wrong_audio_language() {
        let mut p = prefs();
        p.audio = AudioReq::Lang("eng".to_string());
        let wrong = parse(&raw("t", "Film.1080p.x265 German", "h1", Some("Film.mkv")));   // languages=["ger"]
        let untagged = parse(&raw("t", "Film.1080p.x265", "h2", Some("Film.mkv")));        // languages=[]
        assert!(score(&wrong, &p).unwrap() < score(&untagged, &p).unwrap(),
            "a release tagged as a non-required language must rank below an untagged one");
    }

    #[test]
    fn score_penalises_tiny_and_absurd_sizes_and_prefers_bitrate() {
        let normal = parse(&raw("t", "A.1080p.x265\n\u{1f4be} 8 GB", "h1", Some("A.mkv")));
        let tiny = parse(&raw("t", "A.1080p.x265\n\u{1f4be} 150 MB", "h2", Some("A.mkv")));
        let absurd = parse(&raw("t", "A.1080p.x265\n\u{1f4be} 120 GB", "h3", Some("A.mkv")));
        let bigger = parse(&raw("t", "A.1080p.x265\n\u{1f4be} 12 GB", "h4", Some("A.mkv")));
        let remux = parse(&raw("t", "A.1080p.x265\n\u{1f4be} 35 GB", "h5", Some("A.mkv")));
        assert!(score(&normal, &prefs()).unwrap() > score(&tiny, &prefs()).unwrap());
        assert!(score(&normal, &prefs()).unwrap() > score(&absurd, &prefs()).unwrap());
        // Higher bitrate (larger) preferred at the same resolution/source…
        assert!(score(&bigger, &prefs()).unwrap() > score(&normal, &prefs()).unwrap());
        // …and a 35 GB REMUX-sized file is no longer penalised (it's capped, not rejected).
        assert!(score(&remux, &prefs()).unwrap() > score(&normal, &prefs()).unwrap());
    }

    #[test]
    fn score_rejects_cam_and_telesync_sources() {
        // The quality floor: cam/telesync/screener are excluded even when cached at the ceiling.
        for marker in ["HDTS", "CAM", "HDCAM", "TELESYNC", "DVDScr", "R5", "TS", "WORKPRINT"] {
            let c = raw(
                "Torrentio\n1080p",
                &format!("Movie.2025.1080p.{marker}.x265\nRD+"),
                "h",
                Some("Movie.2025.1080p.mkv"),
            );
            assert_eq!(score(&parse(&c), &prefs()), None, "{marker} must be rejected by the quality floor");
        }
        // A real BluRay at the same resolution is accepted.
        let good = parse(&raw("Torrentio\n1080p", "Movie.2025.1080p.BluRay.x265\nRD+", "h", Some("Movie.mkv")));
        assert!(score(&good, &prefs()).is_some());
    }

    #[test]
    fn score_excludes_uncached_zero_seeder_keeps_cached() {
        // 👤0 and uncached → undownloadable → excluded outright (not scored).
        let dead = parse(&raw("Torrentio 1080p", "A.2009.1080p.BluRay.x264\n\u{1f464} 0 \u{1f4be} 6 GB", "h", Some("A.mkv")));
        assert_eq!(dead.seeders, Some(0));
        assert!(!dead.cached);
        assert_eq!(score(&dead, &prefs()), None);
        // Same release but cached → kept: a cached copy needs no live peers.
        let cached_dead = parse(&raw("Torrentio 1080p", "A.2009.1080p.BluRay.x264\n\u{1f464} 0 \u{1f4be} 6 GB\nRD+", "h", Some("A.mkv")));
        assert!(cached_dead.cached);
        assert!(score(&cached_dead, &prefs()).is_some());
        // Seeded uncached → kept.
        let seeded = parse(&raw("Torrentio 1080p", "A.2009.1080p.BluRay.x264\n\u{1f464} 10 \u{1f4be} 6 GB", "h", Some("A.mkv")));
        assert_eq!(seeded.seeders, Some(10));
        assert!(score(&seeded, &prefs()).is_some());
    }

    #[test]
    fn score_prefers_higher_source_tier() {
        let remux = parse(&raw("t", "A.1080p.BluRay.REMUX.x265\nRD+", "h1", Some("A.1080p.mkv")));
        let web = parse(&raw("t", "A.1080p.WEB-DL.x265\nRD+", "h2", Some("A.1080p.mkv")));
        assert_eq!(remux.source, Source::Remux);
        assert_eq!(web.source, Source::Web);
        assert!(
            score(&remux, &prefs()).unwrap() > score(&web, &prefs()).unwrap(),
            "REMUX should outrank WEB-DL at the same resolution"
        );
    }

    #[test]
    fn group_parsed_from_description_when_no_file_name() {
        let r = parse(&raw("t", "Movie.2023.1080p.BluRay.x265-GRP", "h", None));
        assert_eq!(r.group.as_deref(), Some("GRP"));
    }

    #[test]
    fn group_rejects_tech_tokens() {
        let r = parse(&raw("t", "Movie.2023.1080p.WEB-DL", "h", None));
        assert_eq!(r.group, None, "WEB-DL must not be parsed as group 'DL'");
    }
}
