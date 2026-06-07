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

static RES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4})p\b|\b(4k|uhd)\b").unwrap());
static SIZE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)([\d.]+)\s*(gb|mb)").unwrap());
static SEED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\u{1f464}\s*(\d+)").unwrap());
static GROUP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-([A-Za-z0-9]+)$").unwrap());

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

    let cached = lower.contains("rd+") || lower.contains("tb+") || lower.contains("[rd+]") || lower.contains("[tb+]") || text.contains('\u{26a1}');

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
        .and_then(|stem| GROUP_RE.captures(stem.trim()).and_then(|cap| cap.get(1).map(|m| m.as_str().to_string())))
        .or_else(|| {
            c.description
                .lines()
                .next()
                .and_then(|line| GROUP_RE.captures(line.trim()).and_then(|cap| cap.get(1).map(|m| m.as_str().to_string())))
        });

    let mut languages = Vec::new();
    for (word, code) in LANG_WORDS {
        if lower.contains(word) { languages.push((*code).to_string()); }
    }

    let container = c.file_name.as_deref().map(Container::from_name).unwrap_or(Container::Other);

    ReleaseInfo {
        info_hash: c.info_hash.clone(),
        file_idx: c.file_idx,
        file_name: c.file_name.clone(),
        resolution, codec, hdr, languages, group, size_bytes, seeders, cached, container,
    }
}

const LANG_WORDS: &[(&str, &str)] = &[
    ("english", "eng"), ("french", "fre"), ("german", "ger"), ("spanish", "spa"),
    ("italian", "ita"), ("russian", "rus"), ("hindi", "hin"), ("japanese", "jpn"),
    ("korean", "kor"), ("portuguese", "por"), ("multi", "mul"),
];

/// Score a release against prefs. `None` = excluded by a hard rule (resolution ceiling). Higher better.
pub fn score(r: &ReleaseInfo, prefs: &QualityPrefs) -> Option<i64> {
    if let Some(res) = r.resolution {
        if res > prefs.max_resolution.height() { return None; }
    }
    let mut s: i64 = 0;
    if r.cached { s += 1_000_000; }
    s += r.resolution.unwrap_or(0) as i64 * 100;
    if prefs.prefer_hevc && r.codec == Codec::Hevc { s += 5_000; }
    if prefs.prefer_hdr && r.hdr { s += 3_000; }
    if r.container.is_verifiable() { s += 2_000; }
    s += (r.seeders.unwrap_or(0).min(1000) as i64) * 2;
    if let Some(sz) = r.size_bytes {
        if sz < 300_000_000 || sz > 25_000_000_000 { s -= 4_000; }
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
}
