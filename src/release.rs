use crate::config::{AudioReq, QualityPrefs};
use regex::Regex;
use std::sync::LazyLock;

/// A raw stream from the scraper, before parsing.
#[derive(Debug, Clone)]
pub struct RawCandidate {
    pub name: String,
    pub description: String,
    pub info_hash: String,
    /// The addon-provided `fileIdx` (faithful parse output, asserted in `scraper` tests). It has NO
    /// production consumer — `release::parse` drops it, and `acquire`/`observe` select the largest
    /// video so they always agree, and a misleading addon index can't diverge them (see
    /// `acquire::select_target`). Retained deliberately as a parse-only capture that documents the
    /// Torrentio resolve-URL structure (guarded by the `hash_idx_from_url` tests), NOT dead flow to
    /// be wired into selection.
    pub file_idx: Option<usize>,
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
    pub fn is_verifiable(self) -> bool {
        matches!(self, Container::Mkv | Container::Mp4)
    }
    /// Classify a file by its extension. `pub` so the account mirror (`tasks::mirror_quality`), the
    /// schema-v8 migration and the upgrade rankers can tell whether a file is one the probe can read.
    pub fn from_name(name: &str) -> Container {
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
    pub fn tier_score(self) -> i64 {
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
    pub file_name: Option<String>,
    pub resolution: Option<u16>,
    pub codec: Codec,
    pub hdr: bool,
    pub languages: Vec<String>,
    pub size_bytes: Option<u64>,
    pub seeders: Option<u32>,
    pub cached: bool,
    pub container: Container,
    pub source: Source,
}

// An optional 2-3 digit framerate suffix (`(?:\d{2,3})?`) is tolerated after the `p` so a
// framerate-glued token like "2160p60"/"1080p50" is still recognised as its base resolution.
// There is NO trailing word-boundary: a token glued to a following letter/bit-depth marker
// ("2160p10bit"/"2160pHDR"/"2160pHEVC") must still read as its base resolution, because `score`
// lets a `resolution: None` release THROUGH the ceiling check, silently bypassing the
// `MAX_RESOLUTION` hard-filter. A `\b` there (the prior form) made those tokens match nothing →
// over-ceiling bypass. The LEADING `\b` is kept (rejecting "x2160p"), and only a literal `\d{3,4}p`
// run is matched, so a glued suffix can only turn a would-be-`None` into the correct base
// resolution — never mis-tag a non-resolution token (no real release name has such a token). The
// `regex` crate has no lookahead, so this boundary-free form is the portable fix.
static RES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(\d{3,4})p(?:\d{2,3})?|\b(4k|uhd)\b").unwrap());
// Torrentio's size follows its `💾` (U+1F4BE) marker, as the seeders follow `👤`. `SIZE_RE` is the
// fallback for a description without one: a bare `<number> <unit>` whose number doesn't start
// inside a word or number, so a codec run (`x265.200MB`) is not read as 265.2 MB. `parse` likewise
// rejects a unit glued to a following letter or digit, so a unit that runs on into a word is no size
// (`DD5.1.TBS` is no 1 TB release, `DD5.1.MBC` no 1 MB sample), while `_`, `.`, `-` and spaces end
// it on both sides (`Movie_2020_4.7GB_x264`); `TB+` still matches, see the guard in `parse`. (Not a
// trailing `\b`: that counts `_` as a word character.)
static SIZE_MARKED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\u{1f4be}\s*([\d.]+)\s*(tb|gb|mb)").unwrap());
static SIZE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:^|[^a-z\d])([\d.]+)\s*(tb|gb|mb)").unwrap());
static SEED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\u{1f464}\s*(\d+)").unwrap());
// The debrid-cached marker (`RD+`/`TB+`) matched as a WORD-BOUNDED token, not a raw substring, so a
// release name that merely CONTAINS the letters (`Standard+`, `Hard+`, `Record+`) is not mis-flagged
// as cached — which would rank an uncached release to the top, get it optimistically added, then
// stall-reaped by `observe`. The `⚡` (U+26A1) emoji marker is checked separately in `parse`.
static CACHED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:rd|tb)\+").unwrap());

// Cam / telesync / telecine / screener / R5 / workprint / pre-DVD markers (the quality floor) come
// in two groups. This first group is never an ordinary word, so it counts anywhere in the text.
// NOTE: bare `ts` is handled by `TS_RE` (below), NOT here — `\bts\b` would match the `.ts`/`.m2ts`
// transport-stream CONTAINER extension (both are served video formats), wrongly cam-rejecting a
// legitimate `.ts` release. `hd-?ts` stays here (unambiguous — never a container extension).
static CAM_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(cam-?rip|hd-?cam|hq-?cam|hd-?ts|tele-?sync|hd-?tc|tele-?cine|dvd-?scr|bd-?scr|work-?print|pre-?dvd|predvd)\b",
    )
    .unwrap()
});
// The bare markers, which are also title words (Cam, Body Cam, Nanny Cam). They count only in a
// line's tag part (`cam_word_part`) or inside brackets, so a title containing one is not
// cam-rejected.
// Bare `tc` (telecine) is here alongside `hd-?tc`/`tele-?cine`: unlike `ts`, there is no `.tc`
// CONTAINER extension, so the word-bounded `\btc\b` matches a standalone `TC` tag, not "catch".
static CAM_WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(cam|tc|scr|screener|r5)\b").unwrap());

// Where a line's release tags begin: its first year, SxxEyy (season/episode) or `Season N` token.
// Scene and P2P names put the title first with the year or episode code straight after it, so a
// title word (`Cam.2018…`, `Charlotte's.Web.2006…`) sits before this point and a tag
// (`….2025.HDCAM…`) after it.
// A resolution token is deliberately NOT an anchor: in `Movie HDCAM 720p` the tag precedes it.
static TAGS_START_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:(?:19|20)\d{2}\b|s\d{1,3}(?:e\d{1,4})?|seasons?[\s._-]*\d{1,2}\b)").unwrap()
});

// A show line's episode code (`S02E03`, `S01E01E02`, `S01E13v2`, `S02`) or `Season N` token. Unlike
// `TAGS_START_RE` it is word-bounded on both sides, so a title such as `S1m0ne` is not one.
static EPISODE_CODE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:s\d{1,3}(?:e\d{1,4})*(?:v\d{1,2})?|seasons?[\s._-]*\d{1,2})\b").unwrap()
});

// The first resolution / source / codec tag: where a show line's tags begin after its episode
// title. The resolution has no trailing boundary, like `RES_RE` (`1080p10bit`).
static QUALITY_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:\d{3,4}p|(?:4k|uhd|hdtv|web|web-?dl|web-?rip|blu-?ray|bdrip|brrip|dvdrip|hdrip|remux|[xh]\.?26[45]|hevc|avc|xvid)\b)").unwrap()
});

// A bracketed group on a line (`[HDCAM]`, `(2025)`): a tag group even when it precedes the title.
static BRACKET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[(]([^\[\]()]*)[\])]").unwrap());

/// The part of one (lowercased) release-name line that carries its tags: everything after its first
/// year, SxxEyy or `Season N` token, or the whole line when it has none (a year-less name can't be
/// split).
fn tag_part(line: &str) -> &str {
    TAGS_START_RE.find(line).map_or(line, |m| &line[m.end()..])
}

/// The part of one (lowercased) line in which the bare cam markers count. A show line names its
/// episode after the episode code (`Show.S02E03.Nanny.Cam.720p.HDTV`, `Show - S01E05 - Hidden Cam
/// WEBDL-1080p`), so there it starts at the first resolution/source/codec tag after the code. When
/// no such tag follows (a pack's file line, `Show - S02E03 - Hidden Cam.mkv`), the token after the
/// code counts only when it is the line's last one, bar a container extension (`Show.S01E01.CAM`,
/// `Show.S01E01.CAM.mkv`), with its separators so the markers' boundaries still hold: an episode
/// title that merely starts with a cam word (`Show - S01E05 - Cam Girl.mkv`) is not a tag. Any
/// other line: its [`tag_part`]. Telesync/cam releases of TV episodes are all but unheard of, and
/// the unambiguous tags (`HDCAM`, `TELESYNC`, …) and a bracketed `[CAM]` still count anywhere.
fn cam_word_part(line: &str) -> &str {
    let Some(code) = EPISODE_CODE_RE.find(line) else {
        return tag_part(line);
    };
    let rest = &line[code.end()..];
    if let Some(tag) = QUALITY_TAG_RE.find(rest) {
        return &rest[tag.start()..];
    }
    // The text after the code without a trailing container extension (`.mkv`). `.ts` is one too, so
    // a line ending `….ts` keeps reading as the container, as before.
    let stem = crate::vfs::VIDEO_EXTENSIONS
        .iter()
        .find_map(|ext| rest.strip_suffix(ext))
        .unwrap_or(rest);
    let start = stem
        .find(|c: char| c.is_alphanumeric())
        .unwrap_or(stem.len());
    let end = stem[start..]
        .find(|c: char| !c.is_alphanumeric())
        .map_or(stem.len(), |i| start + i);
    if stem[end..].contains(|c: char| c.is_alphanumeric()) {
        // More than one token: an episode title, not a tag.
        return "";
    }
    // Keep one trailing separator: `TS_RE` needs it, and a token ending the line without one is a
    // `.ts` container extension, not a telesync tag.
    let end = rest[end..]
        .chars()
        .next()
        .map_or(end, |c| end + c.len_utf8());
    &rest[..end]
}

/// Whether the (lowercased) name/description marks a cam / telesync / screener source.
fn is_cam_source(lower: &str) -> bool {
    CAM_TAG_RE.is_match(lower)
        || lower.lines().any(|line| {
            let tags = cam_word_part(line);
            CAM_WORD_RE.is_match(tags)
                || TS_RE.is_match(tags)
                || BRACKET_RE.captures_iter(line).any(|group| {
                    let group = &group[1];
                    // Padded so a lone `[TS]` meets `TS_RE`'s separators; a bracketed `….ts` file
                    // name is still the container, not a telesync tag.
                    CAM_WORD_RE.is_match(group)
                        || (TS_RE.is_match(&format!(" {group} ")) && !group.ends_with(".ts"))
                })
        })
}

// A bare `ts` telesync tag, but NOT a trailing `.ts` container extension. A bare `ts` counts only
// with a separator after it, and `is_cam_source` matches this one line at a time (a line's
// `cam_word_part`, or a padded bracket group of it), so a line ending `….ts` (`…1080p.HDTV.ts`, a
// real transport-stream file) is read as the container, while a mid-line telesync tag
// (`…1080p.TS.x265`, ` ts `, `-ts-`) is still caught by the quality floor. Never add `$` to the
// trailing class: it would cam-reject every `.ts` file line. A line holds no `\n`, so neither class
// lists it.
static TS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|[ .\-_])ts[ .\-_]").unwrap());

// A bare `web` source token (the canonical scene tag, e.g. `…1080p.WEB.H264-GROUP`) that the
// explicit `web-dl`/`webrip`/etc. checks miss. Separator-bounded (start/end, space, `.`, `-` or `_`
// on both sides; it only ever sees one line, so no `\n`) so it matches `WEB` as a standalone token
// but not a substring of another word (`webrip` is handled explicitly; `spiderweb` won't match).
// Without this, a bare-`WEB` release falls to `Source::Other` (tier 0) — scoring BELOW HDTV and
// blocking a legitimate HDTV→WEB upgrade. Like the bare cam words it is matched only in a line's
// `tag_part`, so the title word in
// `Charlotte's.Web.2006.720p.HDTV` doesn't make an HDTV release WEB. A show line's tag part still
// holds its episode title (`S01E05.Web.of.Lies.720p.HDTV`), as does a pack's file line
// (`05 - Web of Lies.mkv`), so the bare token also counts only when the text names no other source
// (`NON_WEB_SOURCE_TAGS`): a release is never both HDTV and WEB.
static WEB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[ ._-])web(?:[ ._-]|$)").unwrap());
// Source tags a bare `web` token never sits beside on a real WEB release (BluRay/REMUX win earlier).
const NON_WEB_SOURCE_TAGS: [&str; 5] = ["hdtv", "pdtv", "sdtv", "tvrip", "dvdrip"];

// A separator-bounded `multi` (multi-language) token — see the language-parsing loop in `parse`.
// It runs on the joined `name\ndescription` text, so `\n` is a separator here.
static MULTI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[ ._\n-])multi(?:[ ._\n-]|$)").unwrap());

// A WHOLE subtitle-marker token: `sub`, `subs`, `subbed`, `subtitle(s)`, `hardsub(s)`, `softsub(s)`.
// The trailing `\b` keeps a word that merely starts with `sub` (`SUBFRENCH`, `Suburbia`) out.
const SUB_MARKER: &str = r"(?:hard|soft)?sub(?:s|bed|titles?)?\b";
// What may sit between a language word / `multi` and an adjacent subtitle marker. Never `\n`: the
// marker must be on the same line of the joined Torrentio name/description.
const SUB_SEP: &str = r"[ ._\-\[\]():,/+]*";
// A subtitle marker at the START of the text after a token (`ENG SUB`, `Eng.Subs`, `Multi Subs`).
static SUB_AFTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^{SUB_SEP}{SUB_MARKER}")).unwrap());
// A subtitle marker at the END of the text before a token (`SUB.ITA`, `Subtitles: English`).
static SUB_BEFORE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"\b{SUB_MARKER}{SUB_SEP}$")).unwrap());
// Text made only of those separators (two tokens are adjacent).
static SUB_SEP_ONLY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("^{SUB_SEP}$")).unwrap());

// A separator-bounded Dolby-Vision `dv` token — matches `Movie.2160p.DV.HEVC` (dot-separated, the
// common Torrentio form) as well as space-separated ` dv `, without false-positiving on words that
// merely contain "dv". Bounded like `MULTI_RE`, `\n` included: it runs on the joined text too.
static DV_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[ ._\n-])dv(?:[ ._\n-]|$)").unwrap());

/// Classify the release source from its (lowercased) name/description.
fn detect_source(lower: &str) -> Source {
    if is_cam_source(lower) {
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
        // Streaming-provider source tags a release may carry INSTEAD of a literal `web` token (e.g.
        // `...2160p.ATVP.DV.HDR.HEVC-FLUX`). Without these such a release falls to `Source::Other`
        // (tier 0), ranking below same-resolution HDTV and blocking the HDTV→streaming upgrade. Only
        // distinctive codes that can't occur as a substring of a common word are matched with a plain
        // `contains` (`nf.web`/`amzn.web` etc. are already caught by WEB_RE / the web-dl/webrip checks).
        || lower.contains("amzn")
        || lower.contains("dsnp")
        || lower.contains("atvp")
        || lower.contains("hmax")
        || lower.contains("hulu")
        || lower.contains("pcok")
        || lower.contains("pmtp")
        || lower.contains("itunes")
        || (lower.lines().any(|line| WEB_RE.is_match(tag_part(line)))
            && !NON_WEB_SOURCE_TAGS.iter().any(|tag| lower.contains(tag)))
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
    // The regex crate's `\b` counts `_` as a word character, so in an underscore-separated name
    // (`Movie_2019_2160p_UHD_BluRay_x265`) every `\b`-bounded matcher below (the resolution, the cam
    // and source tags, the language words) would read nothing, and a missing resolution passes the
    // `MAX_RESOLUTION` ceiling. They read `tags`, the text with `_` mapped to `.` (as
    // `vfs::is_video_file` does). It has the same length, so match offsets stay valid. The size, the
    // seeders and the cached marker still read the original `text`.
    let tags = text.replace('_', ".");
    let lower = tags.to_ascii_lowercase();

    // Prefer an explicit `\d{3,4}p` resolution over a bare `4k`/`uhd` tag REGARDLESS of position: a
    // plain leftmost `captures()` would read a "4K Remastered" tag preceding an actually-`1080p`
    // release as 2160 (then wrongly exclude it under a 1080 ceiling). Take the first digit resolution
    // if any; otherwise fall back to a `4k`/`uhd` bareword → 2160.
    let resolution = {
        let mut digit_res = None;
        let mut had_4k = false;
        for cap in RES_RE.captures_iter(&tags) {
            if let Some(m) = cap.get(1) {
                if let Ok(r) = m.as_str().parse::<u16>() {
                    digit_res = Some(r);
                    break;
                }
            } else {
                had_4k = true;
            }
        }
        digit_res.or(if had_4k { Some(2160) } else { None })
    };

    let codec = if lower.contains("x265")
        || lower.contains("h265")
        || lower.contains("h.265")
        || lower.contains("hevc")
    {
        Codec::Hevc
    } else if lower.contains("x264")
        || lower.contains("h264")
        || lower.contains("h.264")
        || lower.contains("avc")
    {
        Codec::Avc
    } else {
        Codec::Other
    };

    // A substring test, so a glued `2160pHDR` still counts, run with the two common non-HDR tokens
    // containing "hdr" removed first: `HDRip` (a plain HD rip) and `HDRezka` (a Russian voice-over
    // studio label).
    let hdr_text = lower.replace("hdrip", " ").replace("hdrezka", " ");
    let hdr = hdr_text.contains("hdr")
        || hdr_text.contains("dolby vision")
        || hdr_text.contains("dovi")
        || DV_RE.is_match(&hdr_text);

    let cached = CACHED_RE.is_match(&text) || text.contains('\u{26a1}');

    // The `💾` size wins: the description starts with the release title, so a per-episode size in a
    // pack's name (`[150MB/ep]`) would otherwise come first. Either way take the FIRST
    // `<number><unit>` whose number actually parses, rather than the first regex match: `[\d.]+` can
    // capture a non-numeric run like `1.2.3` (which fails `parse::<f64>`), and without iterating we'd
    // silently drop the size instead of trying the next candidate.
    let size_of = |cap: regex::Captures| {
        let unit_match = cap.get(2)?;
        let unit = unit_match.as_str().to_ascii_lowercase();
        let after_unit = &text[unit_match.end()..];
        // A unit running on into a word or number is part of it (`DD5.1.TBS`), not a size.
        if after_unit
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            return None;
        }
        // `TB+` is the debrid-cached marker, not a size (`H.264 TB+`).
        if unit == "tb" && after_unit.starts_with('+') {
            return None;
        }
        let n: f64 = cap.get(1)?.as_str().parse().ok()?;
        let mult = match unit.as_str() {
            "tb" => 1_000_000_000_000.0,
            "gb" => 1_000_000_000.0,
            _ => 1_000_000.0,
        };
        Some((n * mult) as u64)
    };
    let size_bytes = SIZE_MARKED_RE
        .captures_iter(&text)
        .find_map(size_of)
        .or_else(|| SIZE_RE.captures_iter(&text).find_map(size_of));

    let seeders = SEED_RE
        .captures(&text)
        .and_then(|cap| cap.get(1)?.as_str().parse::<u32>().ok());

    // Language words (full names AND scene abbreviations like `Rus`/`ITA`/`VFF`) are matched as
    // SEPARATOR-BOUNDED tokens (`\b…\b`), not raw substrings, so a longer word that merely *contains*
    // a language name doesn't inject a spurious code (e.g. "Germany" must not tag `ger`, "Frenchman"
    // must not tag `fre`, "Russell" must not tag `rus`). This mirrors the bounded `MULTI_RE`/`WEB_RE`
    // matching and keeps the wrong-audio penalty in `score` honest.
    //
    // A language word ADJACENT to a subtitle marker names a SUBTITLE language, not an audio one, and
    // is skipped — otherwise `[ENG SUB]` on a Japanese title would read as an English dub (inventing
    // a penalty) and `KOREAN…ENG.SUB` would carry `eng` (cancelling the Korean one). Two forms:
    // - prefix: the marker comes first (`SUB.ITA`, `Subtitles: English`), and every language word
    //   that directly continues such a run is a subtitle too (`Sub.ITA.ENG`);
    // - suffix: the marker follows (`ENG SUB`, `Eng.Subs`, `ENG Hardsub`) — unless that marker
    //   directly introduces the NEXT language word, in which case it is that word's prefix and this
    //   one stays audio (`iTA-ENG.Sub.ITA` = Italian + English audio, Italian subs).
    let mut languages: Vec<String> = Vec::new();
    let mut push_lang = |code: &str| {
        if !languages.iter().any(|l| l == code) {
            languages.push(code.to_string());
        }
    };
    let lang_hits: Vec<(usize, usize, &str)> = LANG_RE
        .find_iter(&lower)
        .filter_map(|m| {
            let (_, code) = LANG_WORDS.iter().find(|(w, _)| *w == m.as_str())?;
            Some((m.start(), m.end(), *code))
        })
        .collect();
    let adjacent = |from: usize, to: usize| SUB_SEP_ONLY_RE.is_match(&lower[from..to]);
    let mut prev_prefix_sub_end: Option<usize> = None;
    for (i, &(start, end, code)) in lang_hits.iter().enumerate() {
        let prefix_sub = SUB_BEFORE_RE.is_match(&lower[..start])
            || prev_prefix_sub_end.is_some_and(|prev_end| adjacent(prev_end, start));
        let suffix_sub = SUB_AFTER_RE.find(&lower[end..]).is_some_and(|marker| {
            let marker_end = end + marker.end();
            !lang_hits
                .get(i + 1)
                .is_some_and(|&(next_start, _, _)| adjacent(marker_end, next_start))
        });
        prev_prefix_sub_end = prefix_sub.then_some(end);
        if !prefix_sub && !suffix_sub {
            push_lang(code);
        }
    }
    // Torrentio renders a release's languages as regional-indicator flag emoji (🇷🇺, 🇬🇧 / 🇮🇹).
    // A flag can also name a SUBTITLE language (a VOSTFR release shows 🇫🇷), so this only ever feeds
    // the soft ranking penalty in `score` — never an exclusion; the probe remains the real gate.
    for code in flag_languages(&text) {
        push_lang(code);
    }
    // `multi` (multi-language) is matched as a SEPARATOR-BOUNDED token, not a raw substring, because
    // its `mul` code is a WILDCARD in `score` (`|| l == "mul"` satisfies any required AUDIO_LANGUAGE).
    // A raw `contains("multi")` would mis-tag `MultiSubs`/`multichannel` as multi-language and let a
    // wrong-audio release escape the language penalty for a non-default `AUDIO_LANGUAGE` user. For the
    // same reason a `multi` directly followed by a WHOLE subtitle-marker token (Torrentio's `Multi
    // Subs` = several SUBTITLE languages; `Multi.Sub`, `Multi Subtitles`) is not multi-AUDIO and
    // doesn't tag `mul` — but a word that merely starts with `sub` doesn't count, so the French scene
    // `MULTi.SUBFRENCH` (multi-audio + French subs) still does.
    if MULTI_RE
        .find_iter(&lower)
        .any(|m| !SUB_AFTER_RE.is_match(&lower[m.end()..]))
    {
        push_lang("mul");
    }

    let container = c
        .file_name
        .as_deref()
        .map(Container::from_name)
        .unwrap_or(Container::Other);
    let source = detect_source(&lower);

    ReleaseInfo {
        info_hash: c.info_hash.clone(),
        file_name: c.file_name.clone(),
        resolution,
        codec,
        hdr,
        languages,
        size_bytes,
        seeders,
        cached,
        container,
        source,
    }
}

/// Language words tagged in a release's name/description (matched separator-bounded via `LANG_RE`).
///
/// Known limitation: a title that itself contains one of these words (`The Italian Job`, `My Big Fat
/// Greek Wedding`, `The Danish Girl`) tags that language on EVERY one of its releases. That penalty
/// is shared by all of them, so it only reorders the ones that also carry `MULTi` or the required
/// language — accepted rather than plumbing the requested title into `parse`.
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
    ("polish", "pol"),
    ("ukrainian", "ukr"),
    ("dutch", "dut"),
    ("czech", "cze"),
    ("hungarian", "hun"),
    ("finnish", "fin"),
    ("swedish", "swe"),
    ("danish", "dan"),
    ("norwegian", "nor"),
    ("turkish", "tur"),
    ("greek", "gre"),
    ("hebrew", "heb"),
    ("arabic", "ara"),
    ("chinese", "chi"),
    ("thai", "tha"),
    ("vietnamese", "vie"),
    ("tamil", "tam"),
    ("telugu", "tel"),
    ("malayalam", "mal"),
    ("kannada", "kan"),
    // Scene/tracker abbreviations (3+ letters only — a 2-letter token like `it`/`de`/`es` collides
    // with ordinary words in release names). French variants: `vff` (VF France), `vfq` (VF Québec),
    // `vfi` (VF internationale), `truefrench`; Spanish: `castellano` (Spain), `latino` (LatAm);
    // Portuguese: `dublado` (Brazilian dub).
    ("eng", "eng"),
    ("rus", "rus"),
    ("ita", "ita"),
    ("ger", "ger"),
    ("deu", "ger"),
    ("fre", "fre"),
    ("fra", "fre"),
    ("vff", "fre"),
    ("vfq", "fre"),
    ("vfi", "fre"),
    ("truefrench", "fre"),
    ("spa", "spa"),
    ("esp", "spa"),
    ("castellano", "spa"),
    ("latino", "spa"),
    ("por", "por"),
    ("dublado", "por"),
    ("pol", "pol"),
    ("jpn", "jpn"),
    ("kor", "kor"),
    ("hin", "hin"),
    ("ukr", "ukr"),
    // `multi` is intentionally NOT here — it's matched separator-bounded via `MULTI_RE` (see `parse`)
    // because its `mul` code is a wildcard in `score`. Subtitle-only markers (`vostfr`, `napisy`,
    // `subs`) are deliberately absent too: they say nothing about the AUDIO language.
];

/// Torrentio's regional-indicator flags, keyed by the 2-letter country code they spell, mapped to
/// the ISO 639-2/B code(s) of that country's language (the form `score` compares against). A flag
/// Torrentio uses for several languages maps to all of them: 🇮🇳 stands for Hindi, Tamil or Telugu
/// audio (and other Indian languages), so it satisfies a requirement for any of them while still
/// marking a release as a dub for, say, an English title.
const FLAG_LANGS: &[(&str, &[&str])] = &[
    ("GB", &["eng"]),
    ("US", &["eng"]),
    ("RU", &["rus"]),
    ("FR", &["fre"]),
    ("IT", &["ita"]),
    ("ES", &["spa"]),
    ("MX", &["spa"]),
    ("DE", &["ger"]),
    ("PT", &["por"]),
    ("BR", &["por"]),
    ("PL", &["pol"]),
    ("CZ", &["cze"]),
    ("SK", &["slo"]),
    ("NL", &["dut"]),
    ("JP", &["jpn"]),
    ("KR", &["kor"]),
    ("CN", &["chi"]),
    ("TW", &["chi"]),
    ("IN", &["hin", "tam", "tel", "mal", "kan"]),
    ("UA", &["ukr"]),
    ("HU", &["hun"]),
    ("BG", &["bul"]),
    ("RS", &["srp"]),
    ("HR", &["hrv"]),
    ("DK", &["dan"]),
    ("SE", &["swe"]),
    ("NO", &["nor"]),
    ("FI", &["fin"]),
    ("TR", &["tur"]),
    ("IL", &["heb"]),
    ("GR", &["gre"]),
    ("RO", &["rum"]),
    ("LT", &["lit"]),
    ("LV", &["lav"]),
    ("EE", &["est"]),
    ("SI", &["slv"]),
    ("SA", &["ara"]),
    ("IR", &["per"]),
    ("VN", &["vie"]),
    ("ID", &["ind"]),
    ("MY", &["may"]),
    ("TH", &["tha"]),
];

/// Map each regional-indicator flag emoji in `text` (a pair of chars in `U+1F1E6..=U+1F1FF`, which
/// spell a 2-letter country code `A`–`Z`) to its language code(s) via `FLAG_LANGS`, in order of
/// appearance. Consecutive indicators pair up left to right (so `🇬🇧🇮🇹` is GB, IT); a lone indicator
/// and an unknown country are ignored.
fn flag_languages(text: &str) -> Vec<&'static str> {
    const RI_A: u32 = 0x1F1E6;
    let ri = |c: char| {
        let v = c as u32;
        (RI_A..=RI_A + 25)
            .contains(&v)
            .then(|| char::from(b'A' + (v - RI_A) as u8))
    };
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let Some(first) = ri(c) else { continue };
        let Some(second) = chars.peek().copied().and_then(ri) else {
            continue;
        };
        chars.next();
        let country: String = [first, second].iter().collect();
        if let Some((_, codes)) = FLAG_LANGS.iter().find(|(cc, _)| *cc == country) {
            out.extend_from_slice(codes);
        }
    }
    out
}

// Word-boundary alternation of the `LANG_WORDS` names, built once from the table above so the two
// can never drift. Matched against the lowercased text in `parse`.
static LANG_RE: LazyLock<Regex> = LazyLock::new(|| {
    let alt = LANG_WORDS
        .iter()
        .map(|(w, _)| *w)
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!(r"\b({alt})\b")).unwrap()
});

/// The dominant score term: a release already present/cached on the provider outranks any uncached
/// one. Named so `QualitySummary::mark_cached` can keep a stored score consistent with `score()`.
pub const CACHED_BONUS: i64 = 1_000_000;

/// Score a release against prefs. `None` = excluded by a hard rule (resolution ceiling,
/// cam/telesync source, or an uncached zero-seeder release). Higher is better.
///
/// Weight hierarchy (by magnitude): cached availability dominates (1_000_000); then resolution
/// (`height * 100`, so up to ~216k) — note this intentionally outweighs the source-tier band
/// (1_000–8_000), since a candidate is only ever scored after the `MAX_RESOLUTION` hard-filter, so
/// "higher resolution within the allowed ceiling" wins over a lower-resolution higher-tier release;
/// then source tier (1_000–8_000). The remaining preference bonuses are ADDITIVE and combine into
/// the weighted sum rather than forming a strictly-descending tie-break chain: HEVC (+5_000), HDR
/// (+3_000), verifiable container (+2_000), bitrate (up to +3_000), seeders (up to +2_000) — so,
/// e.g., the HEVC bonus can outweigh a one-step source-tier difference, and bitrate can outweigh the
/// container bonus. See the body for exact magnitudes and the `AUDIO_LANGUAGE`-mismatch penalty.
/// [`rank`] does not sort on this sum alone: it orders by the cached, resolution and language terms
/// first and puts a tagged source before an untagged one within them.
pub fn score(r: &ReleaseInfo, prefs: &QualityPrefs) -> Option<i64> {
    // Quality floor: never acquire a cam / telesync / telecine / screener / R5 / workprint source.
    if r.source == Source::Cam {
        return None;
    }
    // A release with no parsed resolution is let through rather than excluded (don't
    // blindly drop potentially-valid releases the scraper failed to tag).
    if let Some(res) = r.resolution {
        if res > prefs.max_resolution.height() {
            return None;
        }
    }
    // An uncached release with zero seeders cannot download — ignore it entirely so the engine
    // never burns an acquire attempt on a dead torrent (nor leaves a "checking" magnet behind).
    // Cached releases are exempt: already on the provider, so live peers are irrelevant.
    if !r.cached && r.seeders == Some(0) {
        return None;
    }
    let mut s = coarse_score(r, prefs);
    s += r.source.tier_score();
    if prefs.prefer_hevc && r.codec == Codec::Hevc {
        s += 5_000;
    }
    if prefs.prefer_hdr && r.hdr {
        s += 3_000;
    }
    if r.container.is_verifiable() {
        s += 2_000;
    }
    s += (r.seeders.unwrap_or(0).min(1000) as i64) * 2;
    if let Some(sz) = r.size_bytes {
        // Penalise fake/sample (<50 MB) and absurd (>80 GB) files. The lower bound targets ACTUAL
        // samples — a few-second clip is tens of MB — rather than "short" content: a 300 MB floor
        // wrongly penalised legitimate short TV episodes (e.g. a ~22-min 720p episode) and short
        // films, which `monitor_episodes` acquires per-episode, deprioritising a perfectly valid
        // small release below a larger one. The upper bound is generous so a legitimate REMUX
        // (often 25–40 GB at 1080p — our top source tier) isn't penalised.
        if !(50_000_000..=80_000_000_000).contains(&sz) {
            s -= 4_000;
        } else {
            // Prefer higher bitrate (larger file) at a given resolution/source — a tiebreaker
            // capped well below the codec/source weights so it never overrides them.
            s += (sz / 1_000_000_000).min(15) as i64 * 200; // up to +3000 at ≥15 GB
        }
    }
    Some(s)
}

/// `score`'s coarse terms: cached availability, resolution and the wrong-`AUDIO_LANGUAGE` penalty.
/// The source tier and the additive bonuses make up the rest of the score. [`rank`] orders by this
/// first, so the source tie-break it applies within one coarse group never crosses a cached,
/// resolution or language difference.
fn coarse_score(r: &ReleaseInfo, prefs: &QualityPrefs) -> i64 {
    let mut s: i64 = 0;
    if r.cached {
        s += CACHED_BONUS;
    }
    s += r.resolution.unwrap_or(0) as i64 * 100;
    if wrong_audio_language(r, prefs) {
        s -= 50_000;
    }
    s
}

/// Whether `r` carries [`score`]'s wrong-`AUDIO_LANGUAGE` penalty under `prefs`: it names (or flags)
/// audio languages and none of them — nor a `mul` multi-audio tag — satisfies a CONCRETE requirement:
/// one language (`Lang`), or any of several (`AnyOf` — `Original` resolved against the title's TMDB
/// original + spoken languages by `with_resolved_audio`). A raw `Original` has no per-title context,
/// so it never applies; nor does it to an untagged release, or when a wanted language is "no
/// language" (TMDB `xx`, a dialogue-free title — `probe::verify` skips its audio check too).
pub fn wrong_audio_language(r: &ReleaseInfo, prefs: &QualityPrefs) -> bool {
    let wants: &[String] = match &prefs.audio {
        AudioReq::Lang(want) => std::slice::from_ref(want),
        AudioReq::AnyOf(wants) => wants,
        AudioReq::Original => &[],
    };
    if wants.is_empty()
        || r.languages.is_empty()
        || wants.iter().any(|w| crate::probe::is_no_language(w))
    {
        return false;
    }
    // Compare with `probe::lang_eq`, exactly as `probe::verify` does: it normalises both sides to
    // ISO 639-2 (3-letter) and knows macrolanguages. `AudioReq::parse` only lowercases the
    // configured value, so a 2-letter `AUDIO_LANGUAGE` (e.g. `en`) must map to `eng` to match a
    // release tagged "English" (parsed to `eng`); a raw `l == want` would otherwise penalise the
    // CORRECT-language release here even though the probe accepts it — an inconsistency between
    // acquisition ranking and verification.
    !r.languages
        .iter()
        .any(|l| l == "mul" || wants.iter().any(|w| crate::probe::lang_eq(l, w)))
}

/// The candidates [`score`] accepts, best first. Ordered by `score`'s coarse terms
/// ([`coarse_score`]: cached, resolution, the language penalty), then a tagged source before an
/// untagged one (`Source::Other`), then the full score. Within one coarse group a release with no
/// source tag therefore ranks below every tagged one, whatever its bonuses: it is recorded with
/// source tier 0, which the upgrade pass treats as unknown quality ([`is_unknown_quality`]) and
/// replaces with any known release at its resolution, so acquiring it over a tagged release would
/// only download the title twice. Across coarse groups the order is `score`'s, bar a resolution
/// gap small enough for the tier and bonuses to overturn (576p against 720p): there the higher
/// resolution ranks first.
pub fn rank(candidates: Vec<ReleaseInfo>, prefs: &QualityPrefs) -> Vec<ReleaseInfo> {
    let mut scored: Vec<(RankKey, ReleaseInfo)> = candidates
        .into_iter()
        .filter_map(|r| Some((rank_key(&r, prefs)?, r)))
        .collect();
    scored.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
    scored.into_iter().map(|(_, r)| r).collect()
}

/// [`rank`]'s order key, higher first: [`coarse_score`], then whether the source is tagged, then
/// the full [`score`]. A total order (a plain tuple), so a sort by it is well defined.
pub(crate) type RankKey = (i64, bool, i64);

/// The key [`rank`] orders `r` by under `prefs`, or `None` when [`score`] rejects it. The upgrade
/// engine's pack ranker orders by it too, so a pack it stages is the one acquisition would pick.
pub(crate) fn rank_key(r: &ReleaseInfo, prefs: &QualityPrefs) -> Option<RankKey> {
    let s = score(r, prefs)?;
    Some((coarse_score(r, prefs), r.source != Source::Other, s))
}

/// A compact, serialisable snapshot of a release's quality, recorded on `OwnedRecord` at acquire
/// time so the upgrade engine can compare a fresh candidate against what we own without
/// re-parsing the provider listing. All primitives (no enum serde needed); `source_tier` is
/// `Source::tier_score()`, so a larger value is a higher tier.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct QualitySummary {
    pub cached: bool,
    pub source_tier: i64,
    pub resolution: u16,
    pub score: i64,
    /// The release's audio/subtitle tracks could NOT be checked: `observe` accepted it on a probe
    /// `Accept` (an unreadable container — `.avi`, an MP4 whose `moov` couldn't be read — or no
    /// tracks), or it is an account-mirror copy whose file isn't MKV/MP4 (the schema-v8 migration
    /// backfills it for existing records a `selection` entry serves from such a file).
    /// `is_target_improvement` lets an equal-quality cached release
    /// replace such a copy, and the upgrade pass only ever adopts a probe `Pass`, so an unchecked
    /// copy is swapped for a checked one and never the reverse. `false` (the serde default) for a
    /// record persisted before the flag existed.
    #[serde(default)]
    pub unverified: bool,
}

impl QualitySummary {
    /// Snapshot a candidate's quality. Always `unverified: false` — the caller sets the flag from
    /// the probe verdict once the release has actually been checked (or couldn't be).
    pub fn of(r: &ReleaseInfo, prefs: &QualityPrefs) -> Self {
        QualitySummary {
            cached: r.cached,
            source_tier: r.source.tier_score(),
            resolution: r.resolution.unwrap_or(0),
            score: score(r, prefs).unwrap_or(i64::MIN),
            unverified: false,
        }
    }

    /// The baseline for an owned copy with NO recorded quality (`quality: None`): unknown on both
    /// axes, so [`is_unknown_quality`] holds and it ranks below every known quality (see
    /// [`quality_rank`]). `cached: true` — an owned copy is in the account — so only a KNOWN-quality
    /// cached candidate improves on it ([`is_target_improvement`]), never another unknown one.
    pub fn unknown() -> Self {
        QualitySummary {
            cached: true,
            source_tier: 0,
            resolution: 0,
            score: i64::MIN,
            unverified: false,
        }
    }

    /// Snapshot an OWNED file's quality for comparison, bypassing the acquisition hard filters that
    /// make [`score`] return `None`: a file ABOVE the resolution ceiling keeps its real resolution
    /// (scored as if the ceiling were the maximum, 2160p) instead of being dropped — so an
    /// over-ceiling copy is never mistaken for unknown-lowest and stays under the corrective-downgrade
    /// rules; a cam source keeps tier 0 (unknown-lowest); an untagged axis stays 0 (unknown). Equal to
    /// [`QualitySummary::of`] for any release `score` accepts. Used for account-mirror copies, whose
    /// quality is derived from the file name rather than a scraped release.
    pub fn of_owned(r: &ReleaseInfo, prefs: &QualityPrefs) -> Self {
        let uncapped = QualityPrefs {
            max_resolution: crate::config::MaxResolution::P2160,
            ..prefs.clone()
        };
        QualitySummary {
            score: score(r, prefs)
                .or_else(|| score(r, &uncapped))
                .unwrap_or(i64::MIN),
            ..QualitySummary::of(r, prefs)
        }
    }

    /// Mark this owned release as now present/cached on the provider — used when an UNCACHED torrent
    /// finishes downloading (so its acquire-time snapshot recorded `cached: false`). Flips `cached`
    /// AND adds [`CACHED_BONUS`] to `score` so the stored summary matches what `score()` would yield
    /// for a cached release. Both are required: `is_target_improvement` short-circuits to "any cached
    /// candidate is an upgrade" while `!current.cached`, and its fallback compares `score` — so a
    /// stale `cached:false` (or a score missing the bonus) would let the upgrade engine replace this
    /// working copy with a LOWER-quality cached release and delete it. Idempotent.
    pub fn mark_cached(&mut self) {
        if !self.cached {
            self.cached = true;
            self.score = self.score.saturating_add(CACHED_BONUS);
        }
    }
}

/// Whether `q` is UNKNOWN quality — ranked LOW wherever qualities are compared (dedup
/// keep-priority, upgrade baselines, consolidation): its resolution or source tier is 0 (that axis
/// untagged). A known RESOLUTION still stays a floor: an unknown copy is below every known quality
/// at its resolution, never below a lower-resolution one ([`quality_rank`]), and the upgrade pass
/// only replaces it with a known release at or above that resolution ([`is_target_improvement`]).
/// A copy with NO recorded quality is unknown too ([`QualitySummary::unknown`]). The one exception
/// is a copy whose resolution is ABOVE `ceiling` (`MAX_RESOLUTION`): its resolution is known and
/// higher than anything acquisition would fetch, so it is never unknown-lowest — replacing it is a
/// corrective downgrade, gated by `ALLOW_RESOLUTION_DOWNGRADE`, whatever its source tier.
pub fn is_unknown_quality(q: &QualitySummary, ceiling: u16) -> bool {
    q.resolution <= ceiling && (q.resolution == 0 || q.source_tier == 0)
}

/// A copy's quality rank, compared field by field (the derived `Ord`): resolution, then source
/// tier, then checked before unchecked (`unverified` — tracks that could not be read). An untagged
/// axis is 0, so UNKNOWN quality ([`is_unknown_quality`]) is the lowest at its resolution — a known
/// source beats an untagged one — but a KNOWN resolution stays a floor: a 1080p copy with an
/// untagged source outranks a 720p WEB copy, so dedup never trades a known resolution down. An
/// untagged resolution (0) is the lowest of all, and no recorded quality is below everything.
/// Score is deliberately NOT part of it: two copies with the same rank are the same quality. The
/// ceiling plays no part — a copy above `MAX_RESOLUTION` is never ranked down for it (the
/// corrective downgrade is the upgrade engine's job, under `ALLOW_RESOLUTION_DOWNGRADE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct QualityRank {
    pub resolution: u16,
    pub source_tier: i64,
    pub checked: bool,
}

/// The [`QualityRank`] of an owned copy's recorded quality (`None` = nothing recorded = the lowest).
pub fn quality_rank(q: Option<&QualitySummary>) -> QualityRank {
    match q {
        Some(q) => QualityRank {
            resolution: q.resolution,
            source_tier: q.source_tier,
            checked: !q.unverified,
        },
        None => QualityRank {
            resolution: 0,
            source_tier: 0,
            checked: false,
        },
    }
}

/// The direction of a beneficial swap under the current prefs, or `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Improvement {
    None,
    Upgrade,
    Downgrade,
}

/// Whether swapping `current` -> `candidate` moves the owned copy toward the ideal under `prefs`,
/// and in which direction. Generalises the plain upgrade-only comparison to also fire a CORRECTIVE
/// DOWNGRADE when `current` is above the ceiling (its `effective_score` is penalised, so a
/// within-ceiling cached `candidate` now scores higher even at a lower raw resolution).
///
/// A `candidate` reaching this function has already passed `score()`'s hard filter, so it is always
/// within the ceiling; its effective score equals its raw score. An `Upgrade` is a strict score gain
/// plus a category jump: a higher resolution (any tier — resolution dominates), or a higher source
/// tier at the same resolution (never a tier drop for a bonus). Convergence: within a fixed ceiling
/// every accepted swap strictly increases the owned effective score (bounded) => no flip-flop — the
/// one equal-score case, replacing an `unverified` copy, adopts only a probe-checked release, so the
/// new copy is verified and that rule cannot fire for it again.
///
/// UNKNOWN quality ([`is_unknown_quality`]) is the lowest: within the ceiling an unknown current is
/// improved by any known cached candidate at or above its resolution when that is tagged (scores
/// ignored — a known resolution stays a floor, so a 1080p copy with an untagged source is never
/// traded for 720p), and a known current is never improved by an unknown candidate within the
/// ceiling. Replacing an unknown copy records a known one, so that rule fires at most once per slot
/// (converges). The one exception: an over-ceiling current is corrected by any within-ceiling
/// candidate with a tagged resolution, its source untagged included (a corrective `Downgrade`).
/// `upgrade::rank_changes` orders a tagged source first at each resolution (`rank_key`), so such a
/// release is adopted only when no tagged one at its resolution qualifies; the next pass may then
/// upgrade it once.
pub fn is_target_improvement(
    current: &QualitySummary,
    candidate: &QualitySummary,
    prefs: &QualityPrefs,
) -> Improvement {
    if !candidate.cached {
        return Improvement::None;
    }
    if !current.cached {
        return Improvement::Upgrade;
    }
    // Unknown quality ranks below every known quality ([`is_unknown_quality`]). For a WITHIN-ceiling
    // current that decides the comparison outright, scores ignored: an unknown current is improved by
    // ANY known-quality cached within-ceiling candidate (the upgrade pass adopts it only on a probe
    // `Pass`) that keeps its resolution when that is tagged — a known resolution stays a floor, so
    // this ungated path never trades resolution down — and never by an unknown one; a known current
    // is never "improved" by an unknown candidate. The swap records the adopted release's known
    // quality, so the next visit compares two known copies under the rules below — it converges. An
    // over-ceiling current is never unknown (its known resolution is above the ceiling) and falls
    // through to the corrective-downgrade rules.
    let ceiling = prefs.max_resolution.height();
    if current.resolution <= ceiling {
        let candidate_known =
            !is_unknown_quality(candidate, ceiling) && candidate.resolution <= ceiling;
        if is_unknown_quality(current, ceiling) {
            let keeps_resolution =
                current.resolution == 0 || candidate.resolution >= current.resolution;
            return if candidate_known && keeps_resolution {
                Improvement::Upgrade
            } else {
                Improvement::None
            };
        }
        if !candidate_known {
            return Improvement::None;
        }
    }
    let cur = effective_score(current, prefs);
    let cand = effective_score(candidate, prefs);
    // An owned copy whose tracks could never be checked (`unverified`) is replaced by ANY cached
    // candidate at the same-or-better effective score and same-or-higher resolution — no category
    // jump needed. The upgrade pass only adopts a candidate whose probe PASSES, so this trades an
    // unchecked copy for a checked one at no quality cost; the replacement is recorded verified, so
    // the rule can't fire again for it (converges). `resolution >=` keeps it from ever trading
    // resolution (and, since an over-ceiling `current` can't be matched by a within-ceiling
    // candidate, leaves the corrective-downgrade direction below untouched).
    if current.unverified && cand >= cur && candidate.resolution >= current.resolution {
        return Improvement::Upgrade;
    }
    if cand <= cur {
        return Improvement::None;
    }
    // At the SAME resolution, require a HIGHER source tier: a marginal same-tier bitrate/HEVC/HDR/
    // seeder wobble never triggers a swap, and neither does a LOWER tier that out-scores the owned
    // copy on those additive bonuses (e.g. WEB x265 over BluRay x264: HEVC's +5k beats the 3k tier
    // gap). Trading the tier for a bonus would also disagree with full-season consolidation, which
    // judges "no regression" on tier + resolution alone: a show's singles swapped down a tier could
    // be folded back into the lower-tier pack phase (a2) had split them from, and split again, every
    // visit. With this rule every upgrade of a verified copy (the unverified rule above aside — and
    // the upgrade pass only adopts verified copies) raises the resolution, or the tier at the same
    // resolution: a move no show pass reverses.
    if candidate.resolution == current.resolution && candidate.source_tier <= current.source_tier {
        return Improvement::None;
    }
    if candidate.resolution < current.resolution {
        // A lower-resolution candidate is only ever a CORRECTIVE downgrade — and only when `current`
        // actually sits ABOVE the ceiling. Within the ceiling we never trade resolution for a
        // higher tier/bonus (a within-ceiling copy can be out-scored at a lower resolution when it
        // carries e.g. a wrong-audio penalty; that must NOT prune the higher-res copy). Guarding on
        // the ceiling keeps `rank_changes`'s direction-homogeneous assumption true: an over-ceiling
        // `current` yields only `Downgrade`s, a within-ceiling `current` only `Upgrade`s.
        //
        // Additionally require the candidate to carry a KNOWN resolution (`> 0`). `score()` lets an
        // untagged release (`resolution == None` -> `0`) through its ceiling filter, so an untagged
        // candidate is NOT proven within the ceiling — and a corrective downgrade DELETES the known
        // over-ceiling copy. Adopting an unverifiable-resolution release could leave us still over the
        // ceiling and record a within-ceiling `resolution: 0` lie. Skip it (`None`): the sole
        // over-ceiling copy is then kept, matching "no conforming replacement -> keep". A `None` here
        // is filtered by `rank_changes`, so direction-homogeneity is preserved (the remaining
        // `Downgrade`s all carry a tagged, provably-within-ceiling resolution).
        if current.resolution > prefs.max_resolution.height() && candidate.resolution > 0 {
            Improvement::Downgrade
        } else {
            Improvement::None
        }
    } else {
        Improvement::Upgrade
    }
}

/// Ceiling-aware score for an OWNED copy. Within the ceiling it is `q.score` unchanged; ABOVE the
/// ceiling it is heavily penalised so a within-ceiling cached release always outscores it (enabling
/// a corrective downgrade), and a more-over-ceiling copy scores below a less-over-ceiling one (so the
/// worst offender is corrected first). Both properties hold UNCONDITIONALLY, regardless of source
/// tier or bonuses. Used ONLY by the upgrade comparison — `score()`'s hard above-ceiling filter
/// (acquisition) is deliberately untouched.
pub fn effective_score(q: &QualitySummary, prefs: &QualityPrefs) -> i64 {
    let ceiling = prefs.max_resolution.height();
    if q.resolution <= ceiling {
        return q.score;
    }
    // Penalise by the amount over the ceiling at a per-unit rate that dominates the entire
    // non-resolution score range (source tier <= 8k, additive bonuses <= ~15k, the up-to-50k
    // language penalty). This makes both properties hold UNCONDITIONALLY, regardless of tier/bonus:
    //   * every over-ceiling copy sits far below any within-ceiling cached release, and
    //   * a more-over-ceiling copy always scores below a less-over-ceiling one (worst offender first).
    // Saturating arithmetic mirrors `mark_cached` and avoids underflow on an i64::MIN sentinel score.
    const OVER_CEILING_PENALTY_PER_UNIT: i64 = 100_000;
    let over = (q.resolution as i64) - ceiling as i64;
    q.score
        .saturating_sub(over.saturating_mul(OVER_CEILING_PENALTY_PER_UNIT))
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

    /// The regex crate's `\b` counts `_` as a word character, so the separator-bounded tag
    /// matchers read nothing in an underscore-separated name: no resolution (which lets a release
    /// through the `MAX_RESOLUTION` ceiling), no cam source (past the cam floor), no language. A
    /// name line without its own quality label (another addon than Torrentio) relies on them.
    #[test]
    fn underscore_separated_tags_are_read() {
        let p = prefs();
        let uhd = parse(&raw(
            "Addon",
            "Movie_2019_2160p_UHD_BluRay_x265\nRD+",
            "h",
            None,
        ));
        assert_eq!(uhd.resolution, Some(2160));
        assert_eq!(uhd.source, Source::BluRay);
        assert!(score(&uhd, &p).is_none(), "above the 1080 ceiling");

        let cam = parse(&raw("Addon", "Movie_2025_HDCAM_x264\nRD+", "h", None));
        assert_eq!(cam.source, Source::Cam);
        assert!(score(&cam, &p).is_none(), "cam floor");

        let ita = parse(&raw("Addon", "Movie_2020_ITA_1080p_WEB\nRD+", "h", None));
        assert_eq!(ita.resolution, Some(1080));
        assert_eq!(ita.source, Source::Web);
        assert_eq!(ita.languages, vec!["ita".to_string()]);

        // The size still reads from the original text.
        let sized = parse(&raw("Addon", "Movie_2020_4.7GB_x264\nRD+", "h", None));
        assert_eq!(sized.size_bytes, Some(4_700_000_000));
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
    }

    #[test]
    fn cached_marker_is_word_bounded_not_a_raw_substring() {
        // A real RD+/TB+ token (word-bounded) is cached; the ⚡ emoji too.
        assert!(parse(&raw("X\nRD+ 1080p", "d", "h", None)).cached);
        assert!(parse(&raw("X\n[TB+] 1080p", "d", "h", None)).cached);
        assert!(parse(&raw("X\n\u{26a1} 1080p", "d", "h", None)).cached);
        // But the letters embedded in a longer word must NOT read as cached (was a raw `contains`).
        assert!(!parse(&raw("Standard+ Edition 1080p", "d", "h", None)).cached);
        assert!(!parse(&raw("Hard+ 1080p", "d", "h", None)).cached);
    }

    #[test]
    fn explicit_digit_resolution_wins_over_a_leading_4k_tag() {
        // A "4K Remastered" marketing tag preceding an actually-1080p release must NOT read as 2160
        // (which a leftmost-match would, then wrongly exclude under a 1080 ceiling).
        assert_eq!(
            parse(&raw("Movie 4K Remastered 1080p BluRay", "d", "h", None)).resolution,
            Some(1080)
        );
        // A bare 4k/uhd tag with no explicit digit resolution still resolves to 2160.
        assert_eq!(
            parse(&raw("Movie 4K UHD BluRay", "d", "h", None)).resolution,
            Some(2160)
        );
        assert_eq!(
            parse(&raw("Movie 2160p BluRay", "d", "h", None)).resolution,
            Some(2160)
        );
    }

    #[test]
    fn framerate_suffixed_resolution_is_still_parsed_and_ceiling_filtered() {
        // A framerate-glued resolution token ("2160p60") must still be read as 2160 so the
        // MAX_RESOLUTION hard-filter can exclude it — otherwise `resolution: None` lets an
        // over-ceiling 4K release through the ceiling check (see `score`). The clean Torrentio
        // `name` line is deliberately omitted here so only the description-side token is available.
        let c = raw(
            "Torrentio",
            "Movie.2023.2160p60.WEB-DL.x265-GRP\n\u{1f4be} 30 GB \u{1f464} 9\nRD+",
            "fps",
            Some("Movie.2023.2160p60.WEB-DL.x265-GRP.mkv"),
        );
        let r = parse(&c);
        assert_eq!(r.resolution, Some(2160), "2160p60 must parse as 2160");
        // Excluded at a 1080 ceiling.
        assert_eq!(score(&r, &prefs()), None);
    }

    #[test]
    fn letter_glued_resolution_token_is_still_parsed_and_ceiling_filtered() {
        // A resolution token glued directly to a following bit-depth/HDR/codec marker
        // ("2160p10bit", "2160pHDR", "2160pHEVC") must still read as 2160 so the MAX_RESOLUTION
        // hard-filter can exclude it. The old trailing word-boundary made such tokens match nothing
        // (`resolution: None`), and `score` lets a None-resolution release THROUGH the ceiling — the
        // same over-ceiling bypass the framerate-suffix case fixed, but for a glued LETTER. The clean
        // Torrentio `name` line is omitted so only the glued description-side token is available.
        for token in ["2160p10bit", "2160pHDR", "2160pHEVC"] {
            let c = raw(
                "Torrentio",
                &format!("Movie.2023.{token}.WEB-DL-GRP\n\u{1f4be} 30 GB \u{1f464} 9\nRD+"),
                "fps",
                Some("Movie.2023.mkv"),
            );
            let r = parse(&c);
            assert_eq!(r.resolution, Some(2160), "{token} must parse as 2160");
            assert_eq!(
                score(&r, &prefs()),
                None,
                "{token} must be excluded at a 1080 ceiling"
            );
        }
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
        let c = raw(
            "Torrentio\n4k",
            "X.2160p.x265\nRD+",
            "h",
            Some("X.2160p.mkv"),
        );
        let r = parse(&c);
        assert_eq!(
            score(&r, &prefs()),
            None,
            "2160p must be excluded at a 1080p ceiling"
        );
    }

    #[test]
    fn score_ranks_cached_above_uncached() {
        let cached = parse(&raw(
            "Torrentio\n1080p",
            "A.1080p.x265\nRD+",
            "h1",
            Some("A.1080p.mkv"),
        ));
        let uncached = parse(&raw(
            "Torrentio\n1080p",
            "A.1080p.x265",
            "h2",
            Some("A.1080p.mkv"),
        ));
        assert!(score(&cached, &prefs()).unwrap() > score(&uncached, &prefs()).unwrap());
    }

    #[test]
    fn score_prefers_verifiable_container_and_hevc() {
        // Vary ONE axis at a time — the old single mkv(x265)>avi(x264) assertion passed if EITHER the
        // container OR the HEVC bonus existed, masking a regression in the other.
        // (a) Container only: same codec (x265), .mkv (verifiable) vs .avi (not).
        let mkv = parse(&raw("T\n1080p", "A.1080p.x265", "h1", Some("A.1080p.mkv")));
        let avi = parse(&raw("T\n1080p", "A.1080p.x265", "h2", Some("A.1080p.avi")));
        assert!(
            score(&mkv, &prefs()).unwrap() > score(&avi, &prefs()).unwrap(),
            "a verifiable container (.mkv) must outscore a non-verifiable (.avi) at equal codec"
        );
        // (b) Codec only: same container (.mkv), HEVC (x265) vs AVC (x264).
        let hevc = parse(&raw("T\n1080p", "A.1080p.x265", "h3", Some("A.1080p.mkv")));
        let avc = parse(&raw("T\n1080p", "A.1080p.x264", "h4", Some("A.1080p.mkv")));
        assert!(
            score(&hevc, &prefs()).unwrap() > score(&avc, &prefs()).unwrap(),
            "HEVC must outscore AVC at equal container (PREFER_HEVC default true)"
        );
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

    /// A release whose name tags no source (`Source::Other`) is recorded with source tier 0, which
    /// the upgrade pass treats as unknown quality and replaces with any known release at its
    /// resolution. The HEVC bonus (+5k) outweighs the WEB tier (3k), so ranking on score alone
    /// added the untagged x265 encode, and the next upgrade pass swapped it for the WEB release
    /// acquisition had ranked below it. At the same cached state, resolution and language state a
    /// tagged source ranks first.
    #[test]
    fn rank_puts_a_tagged_source_above_an_untagged_one_at_the_same_resolution() {
        let untagged = parse(&raw(
            "Torrentio\n1080p",
            "Show.S01E01.1080p.HEVC.x265-MeGusta\n\u{1f4be} 1.2 GB \u{1f464} 50\nRD+",
            "huntagged",
            Some("Show.S01E01.1080p.HEVC.x265-MeGusta.mkv"),
        ));
        let web = parse(&raw(
            "Torrentio\n1080p",
            "Show.S01E01.1080p.WEB.h264-GROUP\n\u{1f4be} 1.2 GB \u{1f464} 50\nRD+",
            "hweb",
            Some("Show.S01E01.1080p.WEB.h264-GROUP.mkv"),
        ));
        assert_eq!(untagged.source, Source::Other, "premise: no source tag");
        assert_eq!(web.source, Source::Web);
        assert!(
            score(&untagged, &prefs()).unwrap() > score(&web, &prefs()).unwrap(),
            "premise: the HEVC bonus out-scores the WEB tier"
        );
        let ranked = rank(vec![untagged.clone(), web.clone()], &prefs());
        assert_eq!(ranked[0].info_hash, "hweb", "the tagged source ranks first");
        // Input order doesn't matter.
        let ranked = rank(vec![web, untagged], &prefs());
        assert_eq!(ranked[0].info_hash, "hweb");
    }

    /// The tagged-source tie-break never crosses `score`'s coarse terms: a higher resolution, or a
    /// cached release, still ranks above a tagged one.
    #[test]
    fn rank_keeps_resolution_and_cache_above_the_source_tie_break() {
        let mut p = prefs();
        p.max_resolution = MaxResolution::P2160;
        let untagged_2160 = parse(&raw("t", "Film.2160p.x264\nRD+", "h2160", Some("F.mkv")));
        let remux_1080 = parse(&raw(
            "t",
            "Film.1080p.REMUX.x265\nRD+",
            "hremux",
            Some("F.mkv"),
        ));
        let untagged_1080 = parse(&raw("t", "Film.1080p.x265\nRD+", "h1080", Some("F.mkv")));
        let bluray_720 = parse(&raw(
            "t",
            "Film.720p.BluRay.x265\nRD+",
            "h720",
            Some("F.mkv"),
        ));
        let uncached_remux_1080 = parse(&raw(
            "t",
            "Film.1080p.REMUX.x265 \u{1f464} 500",
            "hunc",
            Some("F.mkv"),
        ));
        let ranked: Vec<String> = rank(
            vec![
                uncached_remux_1080,
                bluray_720,
                untagged_1080,
                remux_1080,
                untagged_2160,
            ],
            &p,
        )
        .into_iter()
        .map(|r| r.info_hash)
        .collect();
        assert_eq!(ranked, ["h2160", "hremux", "h1080", "h720", "hunc"]);
    }

    /// The wrong-`AUDIO_LANGUAGE` penalty keeps its weight across resolutions: it is one of the
    /// coarse terms `rank` orders by first, exactly as in `score`.
    #[test]
    fn rank_keeps_the_language_penalty_order_across_resolutions() {
        let mut p = prefs();
        p.max_resolution = MaxResolution::P2160;
        p.audio = AudioReq::Lang("eng".to_string());
        let wrong_2160 = parse(&raw(
            "t",
            "Film.2160p.BluRay.x265 German\nRD+",
            "hw2160",
            Some("F.mkv"),
        ));
        let right_1080 = parse(&raw(
            "t",
            "Film.1080p.BluRay.x265\nRD+",
            "hr1080",
            Some("F.mkv"),
        ));
        let ranked = rank(vec![right_1080, wrong_2160], &p);
        assert_eq!(
            ranked[0].info_hash, "hw2160",
            "a wrong-language 2160p still outranks a 1080p, as its score does"
        );

        let wrong_1080 = parse(&raw(
            "t",
            "Film.1080p.REMUX.x265 German\nRD+",
            "hw1080",
            Some("F.mkv"),
        ));
        let right_720 = parse(&raw(
            "t",
            "Film.720p.HDTV.x264\nRD+",
            "hr720",
            Some("F.mkv"),
        ));
        let ranked = rank(vec![wrong_1080, right_720], &p);
        assert_eq!(
            ranked[0].info_hash, "hr720",
            "a wrong-language 1080p still ranks below a correct 720p"
        );
    }

    #[test]
    fn score_downranks_wrong_audio_language() {
        let mut p = prefs();
        p.audio = AudioReq::Lang("eng".to_string());
        let wrong = parse(&raw("t", "Film.1080p.x265 German", "h1", Some("Film.mkv"))); // languages=["ger"]
        let untagged = parse(&raw("t", "Film.1080p.x265", "h2", Some("Film.mkv"))); // languages=[]
        assert!(
            score(&wrong, &p).unwrap() < score(&untagged, &p).unwrap(),
            "a release tagged as a non-required language must rank below an untagged one"
        );
    }

    #[test]
    fn multi_is_separator_bounded_not_a_substring() {
        // `MULTI` as a standalone token tags multi-language (`mul`, the audio wildcard); but
        // `MultiSubs`/`multichannel` (no separator after `multi`) must NOT — otherwise a wrong-audio
        // release escapes the language penalty for a non-default AUDIO_LANGUAGE user.
        let tagged = parse(&raw(
            "t",
            "Film.2020.MULTI.1080p.x265",
            "h",
            Some("Film.mkv"),
        ));
        assert!(
            tagged.languages.iter().any(|l| l == "mul"),
            "a standalone MULTI token must tag multi-language"
        );
        // `MultiSubs` (multi-SUBTITLES) must NOT tag multi-AUDIO.
        let subs = parse(&raw(
            "t",
            "Film.2020.German.MultiSubs.1080p",
            "h",
            Some("Film.mkv"),
        ));
        assert!(
            !subs.languages.iter().any(|l| l == "mul"),
            "MultiSubs must not be mis-tagged as multi-language"
        );
        // The German-audio + MultiSubs release is still penalised for an English-audio user.
        let mut p = prefs();
        p.audio = AudioReq::Lang("eng".to_string());
        let untagged = parse(&raw("t", "Film.1080p.x265", "h2", Some("Film.mkv")));
        assert!(
            score(&subs, &p).unwrap() < score(&untagged, &p).unwrap(),
            "German+MultiSubs must NOT escape the wrong-audio penalty via a spurious `mul` tag"
        );
    }

    #[test]
    fn language_words_are_separator_bounded_not_substrings() {
        // A language NAME embedded in a longer word must NOT inject a spurious code: "Germany" is a
        // place, not German audio; "Frenchman" is not French audio. Without bounding, a wrong-audio
        // release could escape the penalty for a concrete-AUDIO_LANGUAGE user.
        let germany = parse(&raw(
            "t",
            "Germany.Year.Zero.1948.1080p",
            "h1",
            Some("f.mkv"),
        ));
        assert!(
            !germany.languages.iter().any(|l| l == "ger"),
            "\"Germany\" (place) must not be tagged as German audio"
        );
        let frenchman = parse(&raw("t", "The.Frenchman.2019.1080p", "h2", Some("f.mkv")));
        assert!(
            !frenchman.languages.iter().any(|l| l == "fre"),
            "\"Frenchman\" must not be tagged as French audio"
        );
        // A genuine standalone language token is still tagged.
        let german = parse(&raw(
            "t",
            "Film.2020.German.1080p.x265",
            "h3",
            Some("f.mkv"),
        ));
        assert!(german.languages.iter().any(|l| l == "ger"));
        // Repeated mentions don't duplicate the code.
        let twice = parse(&raw(
            "t",
            "English.Film.2020 English.1080p",
            "h4",
            Some("f.mkv"),
        ));
        assert_eq!(
            twice.languages.iter().filter(|l| *l == "eng").count(),
            1,
            "a repeated language word must not produce duplicate codes"
        );
    }

    fn langs(name: &str, desc: &str) -> Vec<String> {
        parse(&raw(name, desc, "h", Some("f.mkv"))).languages
    }

    #[test]
    fn parse_tags_language_abbreviations() {
        // Scene/tracker abbreviations, not just full language names (e.g. `Rus`, `VFF`).
        let rus = langs("t", "Show.s01e04.WEBRip.XviD.Rus.RuDub.tv.avi");
        assert!(rus.contains(&"rus".to_string()), "{rus:?}");
        let vff = langs("t", "Film.2026.MULTi.VFF.1080p.WEBRip");
        assert!(vff.contains(&"fre".to_string()), "{vff:?}");
        assert!(vff.contains(&"mul".to_string()), "{vff:?}");
        let esp = langs("t", "Film (2026) [Bluray 1080p][Esp]");
        assert!(esp.contains(&"spa".to_string()), "{esp:?}");
        let ita_eng = langs("t", "Film.ITA.ENG.1080p");
        assert!(ita_eng.contains(&"ita".to_string()), "{ita_eng:?}");
        assert!(ita_eng.contains(&"eng".to_string()), "{ita_eng:?}");
        // Still separator-bounded: an abbreviation embedded in a longer word tags nothing.
        let embedded = langs("t", "Engaged.Russell.Italy.Spawn.Portal.1080p");
        assert!(embedded.is_empty(), "{embedded:?}");
    }

    #[test]
    fn parse_maps_torrentio_flags_to_languages() {
        // Torrentio renders a release's audio languages as regional-indicator flag emoji.
        let ru = langs("t", "👤 9 💾 1.7 GB ⚙️ EXT\n🇷🇺");
        assert_eq!(ru, vec!["rus".to_string()]);
        let gb_it = langs("t", "👤 9 💾 1.7 GB ⚙️ EXT\n🇬🇧 / 🇮🇹");
        assert!(gb_it.contains(&"eng".to_string()), "{gb_it:?}");
        assert!(gb_it.contains(&"ita".to_string()), "{gb_it:?}");
        // Adjacent flags pair up correctly; a flag that agrees with a word tag isn't duplicated; an
        // unknown flag is ignored.
        let adjacent = langs("t", "Film.English.1080p\n🇺🇸🇫🇷🇦🇶");
        assert_eq!(adjacent, vec!["eng".to_string(), "fre".to_string()]);
    }

    #[test]
    fn tmdb_pseudo_language_codes_are_not_penalised_wrongly() {
        // TMDB's non-ISO codes reach `score` through `with_resolved_audio`: `cn` (Cantonese) is
        // satisfied by a Chinese tag, `sh` (Serbo-Croatian) by a Croatian or Serbian one, and `xx`
        // (No Language) puts no requirement on the audio at all.
        let tagged = |desc: &str| parse(&raw("t", desc, "h", Some("f.mkv")));
        let want = |code: &str| {
            let mut p = prefs();
            p.audio = AudioReq::Lang(code.to_string());
            p
        };
        assert!(!wrong_audio_language(
            &tagged("Film.1080p\n🇨🇳"),
            &want("cn")
        ));
        assert!(!wrong_audio_language(
            &tagged("Film.1080p.Chinese"),
            &want("cn")
        ));
        assert!(wrong_audio_language(&tagged("Film.1080p\n🇬🇧"), &want("cn")));
        assert!(!wrong_audio_language(
            &tagged("Film.1080p\n🇭🇷"),
            &want("sh")
        ));
        assert!(!wrong_audio_language(
            &tagged("Film.1080p\n🇷🇸"),
            &want("sh")
        ));
        assert!(wrong_audio_language(&tagged("Film.1080p\n🇬🇧"), &want("sh")));
        assert!(!wrong_audio_language(
            &tagged("Film.1080p.Rus"),
            &want("xx")
        ));
        let any_of = {
            let mut p = prefs();
            p.audio = AudioReq::AnyOf(vec!["xx".into(), "en".into()]);
            p
        };
        assert!(!wrong_audio_language(&tagged("Film.1080p.Rus"), &any_of));
    }

    #[test]
    fn india_flag_covers_every_language_torrentio_renders_with_it() {
        // Torrentio shows Hindi, Tamil and Telugu audio all as 🇮🇳, so the flag says the audio is
        // one of India's languages, not which. A 🇮🇳 release of a Tamil or Telugu title is not
        // penalised; for an English title it still is.
        let flagged = parse(&raw("t", "Film.2023.1080p.WEB-DL\n🇮🇳", "h1", Some("f.mkv")));
        let untagged = parse(&raw("t", "Film.2023.1080p.WEB-DL", "h2", Some("f.mkv")));
        for want in ["hi", "ta", "te", "ml", "kn"] {
            let mut p = prefs();
            p.audio = AudioReq::Lang(want.to_string());
            assert!(!wrong_audio_language(&flagged, &p), "🇮🇳 for a {want} title");
            assert_eq!(score(&flagged, &p), score(&untagged, &p), "{want}");
        }
        let mut en = prefs();
        en.audio = AudioReq::Lang("en".to_string());
        assert!(
            wrong_audio_language(&flagged, &en),
            "🇮🇳 for an English title"
        );
        // The full language names are tagged too.
        let tamil = langs("t", "Film (2023) Tamil 1080p WEB-DL");
        assert_eq!(tamil, vec!["tam".to_string()]);
        let telugu = langs("t", "Film (2023) Telugu 1080p WEB-DL");
        assert_eq!(telugu, vec!["tel".to_string()]);
    }

    #[test]
    fn multi_subs_marker_is_not_multi_audio() {
        // Torrentio's `Multi Subs` marks multiple SUBTITLE languages: it must not tag the `mul`
        // multi-audio wildcard (which would let a wrong-audio release escape the penalty).
        let subs = langs("t", "Multi Subs / 🇷🇺");
        assert!(!subs.contains(&"mul".to_string()), "{subs:?}");
        assert!(subs.contains(&"rus".to_string()), "{subs:?}");
        let audio = langs("t", "Multi Audio / 🇬🇧 / 🇫🇷");
        assert!(audio.contains(&"mul".to_string()), "{audio:?}");
        // A separate audio MULTi token still tags `mul` even when `Multi Subs` is present too.
        let both = langs("Film.2020.MULTi.1080p", "Multi Subs / 🇫🇷");
        assert!(both.contains(&"mul".to_string()), "{both:?}");
        // Only a WHOLE subtitle token is the marker: `SUBFRENCH` (French subtitles) is a different
        // word, so the `MULTi` before it is still multi-AUDIO (French scene `MULTi` = VF + VO).
        let subfrench = langs("t", "Movie.2020.MULTi.SUBFRENCH.1080p.BluRay");
        assert!(subfrench.contains(&"mul".to_string()), "{subfrench:?}");
        // `Multi Subtitles` / `Multi.Sub` are the same subtitle marker as `Multi Subs`.
        for name in [
            "Movie.2020.1080p Multi Subtitles",
            "Movie.2020.1080p.Multi.Sub",
        ] {
            let l = langs("t", name);
            assert!(!l.contains(&"mul".to_string()), "{name}: {l:?}");
        }
    }

    #[test]
    fn subtitle_only_markers_do_not_tag_audio() {
        // VOSTFR = original audio with French subtitles; `Napisy` (Polish) = subtitles. Neither says
        // anything about the AUDIO language, so neither may tag one.
        let vostfr = langs("t", "Show S01E04 - Saison 1 VOSTFR HDTV 2026");
        assert!(vostfr.is_empty(), "{vostfr:?}");
        let napisy = langs("t", "Film.2020.1080p.WEB-DL.Napisy.PL.Subs");
        assert!(napisy.is_empty(), "{napisy:?}");
    }

    #[test]
    fn language_next_to_a_subtitle_marker_is_not_audio() {
        // Suffix form (`[ENG SUB]`, `Eng.Subs`, `ENG Hardsub`, `English Subbed`) and prefix form
        // (`SUB.ITA`, `Subtitles: English`) both name a SUBTITLE language, not an audio one.
        for name in [
            "[SubsPlease] Frieren - 01 (1080p) [ENG SUB]",
            "Frieren.S01E01.1080p.WEB-DL.Eng.Subs",
            "Frieren.S01E01.1080p.WEB-DL.ENG.Hardsub",
            "Movie.2020.1080p.WEB-DL.English.Subbed",
            "Movie.2020.1080p.WEB-DL.SUB.ITA",
            "Movie.2020.1080p.WEB-DL [Subtitles: English]",
        ] {
            let l = langs("t", name);
            assert!(l.is_empty(), "{name}: {l:?}");
        }
        // An audio tag elsewhere in the name is kept; only the subtitle one is dropped.
        for name in [
            "Movie.2020.KOREAN.1080p.WEB-DL.ENG.SUB",
            "Movie.2020.KOREAN.1080p.WEB-DL [Eng Subs]",
        ] {
            assert_eq!(langs("t", name), vec!["kor".to_string()], "{name}");
        }
        // A `Sub` that introduces a following language belongs to THAT language, so the one before
        // it stays audio (`iTA-ENG.Sub.ITA` = Italian + English audio, Italian subs) …
        let dual = langs("t", "Film.2020.iTA-ENG.Sub.ITA.1080p");
        assert_eq!(dual, vec!["ita".to_string(), "eng".to_string()]);
        // … and a prefix run lists only subtitles (`ITA.Sub.ITA.ENG` = Italian audio; Italian +
        // English subs), so the English after it isn't an audio tag either.
        let run = langs("t", "Film.2020.ITA.AC3.Sub.ITA.ENG.1080p");
        assert_eq!(run, vec!["ita".to_string()]);
        // A word that merely starts with `sub` is not a subtitle marker.
        let suburbia = langs("t", "Movie.2020.ENG.Suburbia.1080p");
        assert_eq!(suburbia, vec!["eng".to_string()]);
    }

    #[test]
    fn subtitle_language_neither_invents_nor_cancels_the_audio_penalty() {
        let with_audio = |code: &str| {
            let mut p = prefs();
            p.audio = AudioReq::Lang(code.to_string());
            p
        };
        let s =
            |name: &str, p: &QualityPrefs| score(&parse(&raw("t", name, "h", Some("f.mkv"))), p);
        // Japanese audio + English subs is what an original-audio user wants for a Japanese title:
        // the `[ENG SUB]` marker must not make it look like an English dub.
        let ja = with_audio("ja");
        assert_eq!(
            s("[SubsPlease] Frieren - 01 (1080p) [ENG SUB]", &ja),
            s("[SubsPlease] Frieren - 01 (1080p)", &ja)
        );
        // English audio + Italian subs, for an English title: no wrong-audio penalty.
        let en = with_audio("en");
        assert_eq!(
            s("Movie.2020.1080p.WEB-DL.SUB.ITA", &en),
            s("Movie.2020.1080p.WEB-DL", &en)
        );
        // Korean audio + English subs, for an English title: still penalised — the `ENG` subtitle
        // tag must not cancel the Korean audio tag's penalty.
        let kor = s("Movie.2020.KOREAN.1080p.WEB-DL.ENG.SUB", &en).unwrap();
        let plain = s("Movie.2020.1080p.WEB-DL", &en).unwrap();
        assert!(plain - kor >= 50_000, "kor={kor} plain={plain}");
    }

    #[test]
    fn score_penalises_russian_tagged_release_for_english_original() {
        let mut p = prefs();
        p.audio = AudioReq::Lang("en".to_string());
        let rus = parse(&raw(
            "t",
            "Show.S01E04.1080p.Rus.ColdFilm",
            "h1",
            Some("Show.mkv"),
        ));
        let web = parse(&raw("t", "Show.S01E04.1080p.WEB", "h2", Some("Show.mkv")));
        let (rus_s, web_s) = (score(&rus, &p).unwrap(), score(&web, &p).unwrap());
        assert!(
            web_s - rus_s >= 50_000,
            "a Rus-tagged release must take the wrong-audio penalty: rus={rus_s} web={web_s}"
        );
    }

    #[test]
    fn telesync_tag_at_start_of_description_is_caught() {
        // The `\n` joining name+description is a real token boundary; a `TS` telesync tag at the
        // start of the description must still hit the cam/telesync quality floor (source = Cam).
        let c = raw(
            "Movie.2020.1080p",
            "TS.x264-GRP\n\u{1f464} 5",
            "h",
            Some("Movie.mkv"),
        );
        assert_eq!(parse(&c).source, Source::Cam);
        // A real `.ts` transport-stream container (name ends `.ts`, joined as `.ts\n…`) must NOT
        // be cam-rejected.
        let container = raw("Movie.1080p.HDTV.ts", "desc", "h2", Some("Movie.ts"));
        assert_ne!(parse(&container).source, Source::Cam);
    }

    #[test]
    fn score_audio_language_is_iso_normalized() {
        // A 2-letter AUDIO_LANGUAGE (`en`) must match a release tagged "English" (parsed to `eng`),
        // consistent with probe::verify (which normalises via to_iso639_2). A raw `l == want` would
        // wrongly penalise the correct-language release.
        let mut p = prefs();
        p.audio = AudioReq::Lang("en".to_string()); // ISO 639-1
        let english = parse(&raw("t", "Film.1080p.x265 English", "h1", Some("Film.mkv"))); // languages=["eng"]
        let untagged = parse(&raw("t", "Film.1080p.x265", "h2", Some("Film.mkv")));
        assert_eq!(
            score(&english, &p).unwrap(),
            score(&untagged, &p).unwrap(),
            "a correct-language release (en→eng) must NOT be penalised vs an untagged one"
        );
        // And a genuinely wrong language is still penalised under the 2-letter setting.
        let german = parse(&raw("t", "Film.1080p.x265 German", "h3", Some("Film.mkv"))); // languages=["ger"]
        assert!(score(&german, &p).unwrap() < score(&untagged, &p).unwrap());
    }

    #[test]
    fn score_any_of_does_not_penalise_a_spoken_language() {
        // `AUDIO_LANGUAGE=original` resolved against a title whose TMDB original is `fi` but which
        // lists English as a spoken language → AnyOf([fi, en]): an English-tagged release is NOT
        // penalised, a German-tagged one still is.
        let mut p = prefs();
        p.audio = AudioReq::AnyOf(vec!["fi".to_string(), "en".to_string()]);
        let english = parse(&raw("t", "Film.1080p.x265 English", "h1", Some("Film.mkv")));
        let untagged = parse(&raw("t", "Film.1080p.x265", "h2", Some("Film.mkv")));
        let german = parse(&raw("t", "Film.1080p.x265 German", "h3", Some("Film.mkv")));
        let (en_s, un_s, de_s) = (
            score(&english, &p).unwrap(),
            score(&untagged, &p).unwrap(),
            score(&german, &p).unwrap(),
        );
        assert_eq!(
            en_s, un_s,
            "a spoken-language release must not be penalised"
        );
        assert!(un_s - de_s >= 50_000, "de={de_s} untagged={un_s}");
    }

    #[test]
    fn score_penalises_tiny_and_absurd_sizes_and_prefers_bitrate() {
        let normal = parse(&raw(
            "t",
            "A.1080p.x265\n\u{1f4be} 8 GB",
            "h1",
            Some("A.mkv"),
        ));
        // A genuine sample (a few-second clip) is tens of MB — below the 50 MB floor. A 150 MB
        // file is a legitimate short episode/film and must NOT be sample-penalised (see below).
        let tiny = parse(&raw(
            "t",
            "A.1080p.x265\n\u{1f4be} 20 MB",
            "h2",
            Some("A.mkv"),
        ));
        let absurd = parse(&raw(
            "t",
            "A.1080p.x265\n\u{1f4be} 120 GB",
            "h3",
            Some("A.mkv"),
        ));
        let bigger = parse(&raw(
            "t",
            "A.1080p.x265\n\u{1f4be} 12 GB",
            "h4",
            Some("A.mkv"),
        ));
        let remux = parse(&raw(
            "t",
            "A.1080p.x265\n\u{1f4be} 35 GB",
            "h5",
            Some("A.mkv"),
        ));
        // A legitimate short episode/film (150 MB) is NOT sample-penalised: it sits in the valid
        // band, so it scores ~4000 higher than a true 20 MB sample (both have ~0 bitrate bonus).
        let short_ep = parse(&raw(
            "t",
            "A.1080p.x265\n\u{1f4be} 150 MB",
            "h6",
            Some("A.mkv"),
        ));
        assert!(score(&normal, &prefs()).unwrap() > score(&tiny, &prefs()).unwrap());
        assert!(score(&normal, &prefs()).unwrap() > score(&absurd, &prefs()).unwrap());
        assert!(
            score(&short_ep, &prefs()).unwrap() - score(&tiny, &prefs()).unwrap() >= 4_000,
            "a 150 MB short episode must avoid the sample penalty that a 20 MB clip incurs"
        );
        // Higher bitrate (larger) preferred at the same resolution/source…
        assert!(score(&bigger, &prefs()).unwrap() > score(&normal, &prefs()).unwrap());
        // …and a 35 GB REMUX-sized file is no longer penalised (it's capped, not rejected).
        assert!(score(&remux, &prefs()).unwrap() > score(&normal, &prefs()).unwrap());
    }

    #[test]
    fn score_rejects_cam_and_telesync_sources() {
        // The quality floor: cam/telesync/screener are excluded even when cached at the ceiling.
        for marker in [
            "HDTS",
            "CAM",
            "HDCAM",
            "TELESYNC",
            "TELECINE",
            "TC", // bare telecine tag — must be rejected like HDTC/TELECINE
            "DVDScr",
            "R5",
            "TS",
            "WORKPRINT",
        ] {
            let c = raw(
                "Torrentio\n1080p",
                &format!("Movie.2025.1080p.{marker}.x265\nRD+"),
                "h",
                Some("Movie.2025.1080p.mkv"),
            );
            assert_eq!(
                score(&parse(&c), &prefs()),
                None,
                "{marker} must be rejected by the quality floor"
            );
        }
        // A real BluRay at the same resolution is accepted.
        let good = parse(&raw(
            "Torrentio\n1080p",
            "Movie.2025.1080p.BluRay.x265\nRD+",
            "h",
            Some("Movie.mkv"),
        ));
        assert!(score(&good, &prefs()).is_some());
    }

    #[test]
    fn cam_word_in_the_title_is_not_a_cam_source() {
        // The bare cam markers (`cam`, `tc`, `scr`, `r5`, `ts`) are also ordinary title words. A
        // title such as Cam (2018) must not have every one of its releases cam-rejected: the
        // markers are read only after the title, i.e. after the first year or SxxEyy token of a
        // line.
        for name in [
            "Cam.2018.1080p.NF.WEB-DL.DDP5.1.x264-NTG",
            "Spy.Cam.2023.1080p.BluRay.x264",
            "Body.Cam.2020.1080p.WEB",
            "Nanny Cam (2014) [720p] [WEBRip]",
            "Dash.Cam.S01E02.1080p.WEB.H264-GRP",
        ] {
            let c = raw(
                "Torrentio\n1080p",
                &format!("{name}\nRD+"),
                "h",
                Some("x.mkv"),
            );
            let r = parse(&c);
            assert_ne!(
                r.source,
                Source::Cam,
                "{name}: the title word is not a cam tag"
            );
            assert!(score(&r, &prefs()).is_some(), "{name} must be acquirable");
        }
        // The same file name on its own (an account-mirror copy) is clean too.
        let mirror = raw("Cam.2018.1080p.NF.WEB-DL.x264.mkv", "", "h", None);
        assert_eq!(parse(&mirror).source, Source::Web);
        // A cam tag after the year, a year-less line, a bracketed tag before the title, an
        // unambiguous tag before the year, and Torrentio's own quality line are all still caught.
        for (name, desc) in [
            ("Torrentio\n1080p", "Movie.2025.1080p.CAM.x265"),
            ("Torrentio\n1080p", "Movie.2025.HDTS.x264"),
            ("Torrentio\n720p", "Movie Name HDCAM 720p"),
            ("Torrentio\n1080p", "[HDCAM] Movie (2025)"),
            ("Torrentio\n1080p", "[CAM] Movie (2025) 1080p"),
            ("Torrentio\n1080p", "[TS] Movie (2025) 1080p"),
            ("Torrentio\n1080p", "TELESYNC Movie 2025 1080p"),
            ("Torrentio\nCAM", "Movie.2025.1080p.x264"),
            ("Torrentio\n1080p", "Cam.2018.1080p.CAM.x264"),
        ] {
            let c = raw(name, &format!("{desc}\nRD+"), "h", Some("x.mkv"));
            assert_eq!(
                parse(&c).source,
                Source::Cam,
                "{name} / {desc} is a cam source"
            );
        }
    }

    #[test]
    fn cam_word_in_an_episode_title_is_not_a_cam_source() {
        // A show's episode title follows its SxxEyy code, so a bare cam word in it sits after the
        // code. On a show line the bare markers count only from the first resolution/source/codec
        // tag on, and a year-less `Season N` pack name is split at its season token too.
        for (name, source) in [
            ("Some.Show.S02E03.Nanny.Cam.720p.HDTV.x264", Source::Hdtv),
            ("Show - S01E05 - Hidden Cam WEBDL-1080p.mkv", Source::Web),
            ("Some.Show.S01E07.The.TS.Files.1080p.WEB.H264", Source::Web),
            (
                "Some.Show.2019.S01E02.Spy.Cam.1080p.BluRay.x264",
                Source::BluRay,
            ),
            ("Dash.Cam.Season.1.1080p.WEB", Source::Web),
        ] {
            let c = raw(
                "Torrentio\n1080p",
                &format!("{name}\nRD+"),
                "h",
                Some("x.mkv"),
            );
            let r = parse(&c);
            assert_eq!(
                r.source, source,
                "{name}: the episode title is not a cam tag"
            );
            assert!(score(&r, &prefs()).is_some(), "{name} must be acquirable");
        }
        // A season pack whose description lists such an episode file is clean too.
        let pack = raw(
            "Torrentio\n720p",
            "Some.Show.S02.720p.HDTV.x264\nSome.Show.S02E03.Nanny.Cam.720p.HDTV.x264.mkv\nRD+",
            "h",
            Some("x.mkv"),
        );
        assert_ne!(parse(&pack).source, Source::Cam);
        // So is one whose listed file name carries no quality tag after the episode title (pack
        // files often don't), including one behind a `Season N/` folder.
        for desc in [
            "Some.Show.S02.1080p.WEB-DL.x264\nSome Show - S02E03 - Hidden Cam.mkv",
            "Some.Show.S02.1080p.WEB-DL.x264\nSeason 2/Some.Show.S02E03.Nanny.Cam.mkv",
            "Some.Show.S02E03.Nanny.Cam.mkv",
            "Some.Show.S02E03.Nanny.Cam",
            // An episode title that STARTS with a cam word: the token after the code counts only
            // when it is the line's only one (bar a container extension).
            "Some.Show.S01.1080p.WEB-DL.x264\nSome Show - S01E05 - Cam Girl.mkv",
            "Some.Show.S01E05.Cam.Girl.mkv",
            "Some Show - S01E07 - Screener Day.mkv",
        ] {
            let c = raw(
                "Torrentio\n1080p",
                &format!("{desc}\nRD+"),
                "h",
                Some("x.mkv"),
            );
            let r = parse(&c);
            assert_ne!(
                r.source,
                Source::Cam,
                "{desc}: the episode title is no cam tag"
            );
            assert!(score(&r, &prefs()).is_some(), "{desc} must be acquirable");
        }
        // Real cam tags on a show line are still caught: an unambiguous tag anywhere, a bracketed
        // one, and a bare one after the quality tags or right after the episode code.
        for desc in [
            "Some.Show.S01E01.720p.HDCAM",
            "Some.Show.S01E01.[CAM].mkv",
            "Some.Show.S01E01.1080p.CAM.x264",
            "Some.Show.S01E01.1080p.TS.x264",
            "Some.Show.S01E01.CAM",
            "Some.Show.S01E01.CAM.mkv",
            "Some Show - S01E01 - TS.mkv",
        ] {
            let c = raw(
                "Torrentio\n1080p",
                &format!("{desc}\nRD+"),
                "h",
                Some("x.mkv"),
            );
            assert_eq!(parse(&c).source, Source::Cam, "{desc} is a cam source");
        }
    }

    #[test]
    fn web_word_in_the_title_is_not_a_web_source() {
        // A bare `web` token is the scene WEB tag only after the title: Charlotte's Web is not a
        // WEB release unless it says so after its year.
        let hdtv = raw("t", "Charlotte's.Web.2006.720p.HDTV.x264", "h", None);
        assert_eq!(parse(&hdtv).source, Source::Hdtv);
        let dvdrip = raw("t", "Charlotte's.Web.2006.DVDRip.x264", "h", None);
        assert_eq!(parse(&dvdrip).source, Source::Other);
        let show = raw("t", "Web.Therapy.S01E01.720p.HDTV.x264", "h", None);
        assert_eq!(parse(&show).source, Source::Hdtv);
        // An episode title after the code sits inside the tag part, so a `Web` in it must not
        // outrank the line's own source tag.
        let episode = raw(
            "t",
            "Some.Show.S01E05.Web.of.Lies.720p.HDTV.x264",
            "h",
            None,
        );
        assert_eq!(parse(&episode).source, Source::Hdtv);
        let episode = raw("t", "Some.Show.S01E05.Tangled.Web.DVDRip.x264", "h", None);
        assert_eq!(parse(&episode).source, Source::Other);
        // Nor on a pack's file line, whose source tag is on the torrent's line.
        let pack = raw(
            "t",
            "Some.Show.S01.720p.HDTV.x264-GRP\n05 - Web of Lies.mkv",
            "h",
            None,
        );
        assert_eq!(parse(&pack).source, Source::Hdtv);
        // A real WEB tag after the year/episode, or on a year-less line, still counts.
        for name in [
            "Movie.2024.1080p.WEB.H264-GRP",
            "Movie.Name.WEB.1080p",
            "Charlotte's.Web.2006.1080p.WEB.x264",
            "Show.S01E01.1080p.WEB",
        ] {
            assert_eq!(
                parse(&raw("t", name, "h", None)).source,
                Source::Web,
                "{name}"
            );
        }
    }

    #[test]
    fn ts_container_extension_is_not_treated_as_telesync() {
        // A trailing `.ts`/`.m2ts` is a transport-stream CONTAINER (a served video format), not a
        // telesync — it must NOT be cam-rejected. Regression guard for the `\bts\b` false positive.
        for name in [
            "Show.S01E01.1080p.HDTV.ts",
            "Movie.2025.2160p.WEB-DL.x265.m2ts",
        ] {
            let c = raw("Torrentio\n1080p", &format!("{name}\nRD+"), "h", Some(name));
            assert!(
                score(&parse(&c), &prefs()).is_some(),
                "{name} is a transport-stream container, not a telesync — must be accepted"
            );
        }
        // But a mid-name `.TS.` telesync TAG is still rejected (separator on both sides).
        let tagged = raw(
            "Torrentio\n1080p",
            "Movie.2025.1080p.TS.x264\nRD+",
            "h",
            Some("Movie.mkv"),
        );
        assert_eq!(
            score(&parse(&tagged), &prefs()),
            None,
            "a mid-name .TS. telesync tag must still be rejected by the quality floor"
        );
    }

    #[test]
    fn quality_summary_of_captures_score_and_fallbacks() {
        // (a) A normal in-ceiling cached release: real score (not the sentinel), resolution + tier set.
        let normal = QualitySummary::of(
            &parse(&raw(
                "Torrentio\n1080p",
                "Movie.2025.1080p.BluRay.x265\nRD+",
                "h",
                Some("Movie.mkv"),
            )),
            &prefs(),
        );
        assert!(normal.cached, "RD+ marks it cached");
        assert_eq!(normal.resolution, 1080);
        assert!(normal.source_tier > 0, "BluRay is a known tier");
        assert_ne!(
            normal.score,
            i64::MIN,
            "an acceptable release has a real score"
        );

        // (b) An EXCLUDED release (above the P1080 ceiling) → `score` is None → `of` records the
        // i64::MIN sentinel, which `is_target_improvement`'s strict `>` comparison relies on.
        let excluded = QualitySummary::of(
            &parse(&raw(
                "Torrentio\n2160p",
                "Movie.2025.2160p.BluRay.x265\nRD+",
                "h",
                Some("Movie.mkv"),
            )),
            &prefs(),
        );
        assert_eq!(
            excluded.score,
            i64::MIN,
            "an above-ceiling release scores the sentinel, never an upgrade target"
        );

        // (c) An untagged-resolution release → the `unwrap_or(0)` fallback yields resolution 0.
        let untagged = QualitySummary::of(
            &parse(&raw(
                "Torrentio",
                "Movie.BluRay.x265\nRD+",
                "h",
                Some("Movie.mkv"),
            )),
            &prefs(),
        );
        assert_eq!(untagged.resolution, 0, "no resolution token → 0");
    }

    #[test]
    fn score_excludes_uncached_zero_seeder_keeps_cached() {
        // 👤0 and uncached → undownloadable → excluded outright (not scored).
        let dead = parse(&raw(
            "Torrentio 1080p",
            "A.2009.1080p.BluRay.x264\n\u{1f464} 0 \u{1f4be} 6 GB",
            "h",
            Some("A.mkv"),
        ));
        assert_eq!(dead.seeders, Some(0));
        assert!(!dead.cached);
        assert_eq!(score(&dead, &prefs()), None);
        // Same release but cached → kept: a cached copy needs no live peers.
        let cached_dead = parse(&raw(
            "Torrentio 1080p",
            "A.2009.1080p.BluRay.x264\n\u{1f464} 0 \u{1f4be} 6 GB\nRD+",
            "h",
            Some("A.mkv"),
        ));
        assert!(cached_dead.cached);
        assert!(score(&cached_dead, &prefs()).is_some());
        // Seeded uncached → kept.
        let seeded = parse(&raw(
            "Torrentio 1080p",
            "A.2009.1080p.BluRay.x264\n\u{1f464} 10 \u{1f4be} 6 GB",
            "h",
            Some("A.mkv"),
        ));
        assert_eq!(seeded.seeders, Some(10));
        assert!(score(&seeded, &prefs()).is_some());
    }

    #[test]
    fn bare_web_source_tag_is_web_not_other() {
        // The canonical scene `WEB` tag (no `-DL`/`Rip`) must classify as Source::Web, not Other —
        // otherwise it scores BELOW HDTV at the same resolution and blocks a legitimate HDTV→WEB
        // upgrade (`is_target_improvement` needs the candidate's tier > the owned tier).
        for name in [
            "Show.S01E01.1080p.WEB.H264-GROUP",
            "Show S01E01 1080p WEB x264",
            "Show.S01E01.1080p.WEB", // bare WEB at the end (before the \n description)
        ] {
            let r = parse(&raw(
                "Torrentio\n1080p",
                &format!("{name}\nRD+"),
                "h",
                Some("Show.mkv"),
            ));
            assert_eq!(
                r.source,
                Source::Web,
                "{name} should classify as Source::Web"
            );
        }
        // A bare-WEB release outranks a same-resolution HDTV (tier 3000 > 1000).
        let web = parse(&raw(
            "Torrentio\n1080p",
            "Show.S01E01.1080p.WEB.x264\nRD+",
            "h",
            Some("Show.mkv"),
        ));
        let hdtv = parse(&raw(
            "Torrentio\n1080p",
            "Show.S01E01.1080p.HDTV.x264\nRD+",
            "h",
            Some("Show.mkv"),
        ));
        assert!(
            score(&web, &prefs()) > score(&hdtv, &prefs()),
            "bare WEB must outrank HDTV at the same resolution"
        );
        // A title that merely CONTAINS "web" as a word but has a real higher-tier source is still
        // classified by that source (the bluray/remux checks run first).
        assert_eq!(
            parse(&raw(
                "t",
                "Charlottes Web 2006 1080p BluRay x264\nRD+",
                "h",
                Some("x.mkv"),
            ))
            .source,
            Source::BluRay,
            "a real BluRay source wins even when the title contains 'web'"
        );
    }

    #[test]
    fn score_prefers_higher_source_tier() {
        let remux = parse(&raw(
            "t",
            "A.1080p.BluRay.REMUX.x265\nRD+",
            "h1",
            Some("A.1080p.mkv"),
        ));
        let web = parse(&raw(
            "t",
            "A.1080p.WEB-DL.x265\nRD+",
            "h2",
            Some("A.1080p.mkv"),
        ));
        assert_eq!(remux.source, Source::Remux);
        assert_eq!(web.source, Source::Web);
        assert!(
            score(&remux, &prefs()).unwrap() > score(&web, &prefs()).unwrap(),
            "REMUX should outrank WEB-DL at the same resolution"
        );
    }

    #[test]
    fn meaningful_upgrade_requires_cached_category_jump() {
        use super::{is_target_improvement, Improvement, QualitySummary};
        let owned_web_1080_cached = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 1,
            unverified: false,
        };
        // uncached candidate is never an upgrade
        let cand_uncached = QualitySummary {
            cached: false,
            source_tier: 8_000,
            resolution: 2160,
            score: 9,
            unverified: false,
        };
        assert_eq!(
            is_target_improvement(&owned_web_1080_cached, &cand_uncached, &prefs()),
            Improvement::None
        );
        // cached, higher tier → upgrade
        let cand_remux = QualitySummary {
            cached: true,
            source_tier: 8_000,
            resolution: 1080,
            score: 5,
            unverified: false,
        };
        assert_eq!(
            is_target_improvement(&owned_web_1080_cached, &cand_remux, &prefs()),
            Improvement::Upgrade
        );
        // cached, same tier + same resolution → NOT an upgrade (marginal)
        let cand_same = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 999,
            unverified: false,
        };
        assert_eq!(
            is_target_improvement(&owned_web_1080_cached, &cand_same, &prefs()),
            Improvement::None
        );
        // cached, higher resolution (within the P1080 ceiling — a fresh 720p owned copy) → upgrade
        let owned_720 = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 720,
            score: 1,
            unverified: false,
        };
        let cand_1080 = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 2,
            unverified: false,
        };
        assert_eq!(
            is_target_improvement(&owned_720, &cand_1080, &prefs()),
            Improvement::Upgrade
        );
        // owned uncached → any cached candidate upgrades it
        let owned_uncached = QualitySummary {
            cached: false,
            source_tier: 6_000,
            resolution: 1080,
            score: 0,
            unverified: false,
        };
        assert_eq!(
            is_target_improvement(&owned_uncached, &cand_same, &prefs()),
            Improvement::Upgrade
        );
    }

    #[test]
    fn meaningful_upgrade_follows_score_no_flip_flop_no_cross_axis_strand() {
        use super::{is_target_improvement, Improvement, QualitySummary};
        // A P2160 ceiling keeps every candidate here (up to 2160p) within-ceiling, so
        // `effective_score` reduces to the plain `score` — matching the ceiling-unaware upgrade
        // comparison this test was originally written against.
        let prefs = QualityPrefs {
            max_resolution: MaxResolution::P2160,
            ..prefs()
        };
        // Realistic scores: cached(1_000_000) + tier + resolution*100 (resolution dominates).
        let bluray_720 = QualitySummary {
            cached: true,
            source_tier: 6_000, // BluRay
            resolution: 720,
            score: 1_078_000,
            unverified: false,
        };
        let web_1080 = QualitySummary {
            cached: true,
            source_tier: 3_000, // Web
            resolution: 1080,
            score: 1_111_000,
            unverified: false,
        };
        let remux_1080 = QualitySummary {
            cached: true,
            source_tier: 8_000, // Remux
            resolution: 1080,
            score: 1_116_000,
            unverified: false,
        };
        let web_2160 = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 2160,
            score: 1_219_000,
            unverified: false,
        };

        // Legitimate cross-axis upgrade: 720p BluRay → 1080p WEB scores higher (resolution wins),
        // so the engine prefers it — must be an upgrade (a no-regression-on-both-axes rule would
        // wrongly strand the library on the 720p BluRay).
        assert_eq!(
            is_target_improvement(&bluray_720, &web_1080, &prefs),
            Improvement::Upgrade,
            "higher-scored cross-axis (720p BluRay → 1080p WEB) must upgrade"
        );

        // No flip-flop on the 2160p WEB ↔ 1080p REMUX pair: only the score-increasing direction
        // qualifies, so it converges to 2160p WEB and never oscillates.
        assert_eq!(
            is_target_improvement(&remux_1080, &web_2160, &prefs),
            Improvement::Upgrade,
            "1080p REMUX → 2160p WEB is the higher-scored direction → upgrade (converge)"
        );
        assert_eq!(
            is_target_improvement(&web_2160, &remux_1080, &prefs),
            Improvement::None,
            "2160p WEB → 1080p REMUX is a score downgrade → NOT an upgrade (no flip-flop)"
        );

        // Same tier + resolution, higher score (bitrate wobble) → NOT an upgrade (no churn).
        let web_1080_marginal = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 1_111_500, // slightly higher than web_1080 but same tier+res
            unverified: false,
        };
        assert_eq!(
            is_target_improvement(&web_1080, &web_1080_marginal, &prefs),
            Improvement::None,
            "marginal same-tier-same-resolution score wobble is not an upgrade"
        );
    }

    #[test]
    fn same_resolution_lower_tier_is_never_an_upgrade_even_when_it_scores_higher() {
        use super::{is_target_improvement, Improvement, QualitySummary};
        // At the same resolution only a HIGHER source tier is a category jump. A lower-tier release
        // can out-score the owned copy on the additive bonuses (HEVC's +5k beats BluRay's +3k tier
        // gap over WEB), but swapping to it would trade the source tier for a bonus: full-season
        // consolidation (tier + resolution only) could then fold the singles back into a lower-tier
        // pack that phase (a2) splits again — a cycle through all three show passes.
        let q = |name: &str| {
            QualitySummary::of(
                &parse(&raw("Torrentio\n1080p", &format!("{name}\nRD+"), "h", None)),
                &prefs(),
            )
        };
        let bluray_x264 = q("Show.S01E01.1080p.BluRay.x264-BD\n👤 20 💾 2 GB");
        let web_x265 = q("Show.S01E01.1080p.WEB-DL.x265-HV\n👤 20 💾 1.1 GB");
        assert!(
            web_x265.score > bluray_x264.score && web_x265.source_tier < bluray_x264.source_tier,
            "fixture: the WEB x265 out-scores the BluRay x264 at a lower tier ({web_x265:?} vs {bluray_x264:?})"
        );
        assert_eq!(
            is_target_improvement(&bluray_x264, &web_x265, &prefs()),
            Improvement::None,
            "a same-resolution tier drop is not an upgrade, whatever the score"
        );
        // The higher-tier direction is still an upgrade when it scores higher.
        let web_x264 = q("Show.S01.1080p.WEB-DL.x264-GRP\n👤 20 💾 12 GB");
        assert!(bluray_x264.score > web_x264.score, "fixture");
        assert_eq!(
            is_target_improvement(&web_x264, &bluray_x264, &prefs()),
            Improvement::Upgrade,
            "WEB -> BluRay at the same resolution is a category jump"
        );
    }

    #[test]
    fn parse_detects_torbox_and_lightning_cached_markers() {
        // Dual-provider: TorBox's `TB+` marker and Torrentio's ⚡ (U+26A1) instant-availability glyph
        // must both read as cached, exactly like Real-Debrid's `RD+`. If `tb+` detection regressed,
        // EVERY TorBox cached release would read as uncached — acquisition would not prefer the cached
        // copy and the upgrade engine would treat all TorBox releases as never-an-upgrade.
        let tb = raw(
            "Torrentio\n1080p",
            "Movie.2023.1080p.WEB-DL.x264-GRP\nTB+",
            "h1",
            Some("Movie.2023.1080p.WEB-DL.x264-GRP.mkv"),
        );
        assert!(parse(&tb).cached, "TB+ must be detected as cached");
        let lightning = raw(
            "Torrentio\n1080p",
            "Movie.2023.1080p.WEB-DL.x264-GRP \u{26a1} Instant",
            "h2",
            Some("Movie.2023.1080p.WEB-DL.x264-GRP.mkv"),
        );
        assert!(
            parse(&lightning).cached,
            "the ⚡ (U+26A1) instant-availability glyph must be detected as cached"
        );
        // And a cached marker scores a release above an otherwise-identical uncached one.
        let cached_score = score(&parse(&tb), &prefs()).unwrap();
        let uncached = raw(
            "Torrentio\n1080p",
            "Movie.2023.1080p.WEB-DL.x264-GRP\n\u{1f464} 50",
            "h3",
            Some("Movie.2023.1080p.WEB-DL.x264-GRP.mkv"),
        );
        assert!(
            cached_score > score(&parse(&uncached), &prefs()).unwrap(),
            "a cached TB+ release must outscore an identical uncached one"
        );
    }

    #[test]
    fn parse_classifies_streaming_provider_and_rip_source_aliases() {
        // Scene releases often carry only a streaming-provider tag (AMZN/DSNP/NF) with no literal
        // `web` token; these must map to Source::Web (tier 3000), not Source::Other (tier 0, below
        // HDTV), else the documented HDTV→WEB upgrade path breaks and a large fraction of the
        // catalogue is misranked. BDRip/BRRip must map to BluRay.
        let amzn = raw("x", "Movie.2020.1080p.AMZN.DDP5.1.H264-GRP", "a", None);
        assert_eq!(parse(&amzn).source, Source::Web, "AMZN → Web");
        let dsnp = raw("x", "Movie.2020.1080p.DSNP.DDP5.1.H264-GRP", "b", None);
        assert_eq!(parse(&dsnp).source, Source::Web, "DSNP → Web");
        let nf = raw("x", "Movie.2020.1080p.NF.WEB.H264-GRP", "c", None);
        assert_eq!(parse(&nf).source, Source::Web, "NF.WEB → Web");
        // The subtle case the alias list exists for: a provider tag with NO literal `web` token.
        // Without the alias these fall to Source::Other (below HDTV) and mis-rank.
        let atvp = raw("x", "Movie.2021.2160p.ATVP.DV.HDR.HEVC-FLUX", "f", None);
        assert_eq!(
            parse(&atvp).source,
            Source::Web,
            "ATVP (no web token) → Web"
        );
        let hmax = raw("x", "Movie.2021.2160p.HMAX.DV.HDR.HEVC-GRP", "g", None);
        assert_eq!(
            parse(&hmax).source,
            Source::Web,
            "HMAX (no web token) → Web"
        );
        let hulu = raw("x", "Movie.2021.1080p.HULU.DDP5.1.H264-GRP", "h", None);
        assert_eq!(
            parse(&hulu).source,
            Source::Web,
            "HULU (no web token) → Web"
        );
        let itunes = raw("x", "Movie.2021.2160p.iTunes.DV.HEVC-GRP", "i", None);
        assert_eq!(
            parse(&itunes).source,
            Source::Web,
            "iTunes (no web token) → Web"
        );
        let bdrip = raw("x", "Movie.2020.1080p.BDRip.x264-GRP", "d", None);
        assert_eq!(parse(&bdrip).source, Source::BluRay, "BDRip → BluRay");
        let brrip = raw("x", "Movie.2020.1080p.BRRip.x264-GRP", "e", None);
        assert_eq!(parse(&brrip).source, Source::BluRay, "BRRip → BluRay");
    }

    #[test]
    fn parse_detects_hdr_and_dolby_vision_variants_without_false_positives() {
        // HDR / Dolby-Vision tags drive the PREFER_HDR scoring bonus. The `dovi` and separator-bounded
        // ` dv ` forms must be detected; a `dvdscr` (DVD-screener) token contains "dv" but must NOT
        // be mistaken for Dolby Vision (it's a screener source, filtered separately).
        let dovi = raw("x", "Movie.2020.2160p.WEB-DL.DoVi.HDR10-GRP", "a", None);
        assert!(parse(&dovi).hdr, "DoVi → hdr");
        let dv = raw("x", "Movie.2020.2160p.WEB-DL dv HEVC-GRP", "b", None);
        assert!(parse(&dv).hdr, "separator-bounded ' dv ' → hdr");
        let dv_dot = raw("x", "Movie.2020.2160p.WEB-DL.DV.HEVC-GRP", "e", None);
        assert!(
            parse(&dv_dot).hdr,
            "dot-separated '.DV.' (the common Torrentio form) → hdr"
        );
        let dolby = raw("x", "Movie 2020 2160p Dolby Vision WEB-DL GRP", "c", None);
        assert!(parse(&dolby).hdr, "spaced 'dolby vision' → hdr");
        let screener = raw("x", "Movie.2020.DVDSCR.x264-GRP", "d", None);
        assert!(
            !parse(&screener).hdr,
            "DVDSCR contains 'dv' but must NOT be read as Dolby Vision"
        );
        let dvd = raw("x", "Movie.2020.1080p.DVD.x264-GRP", "f", None);
        assert!(
            !parse(&dvd).hdr,
            "'DVD' contains 'dv' but must NOT be read as Dolby Vision"
        );
        // `HDRip` (a plain HD rip) and `HDRezka` (a Russian voice-over studio) contain "hdr" but
        // are not HDR.
        let hdrip = raw("x", "Movie.2019.720p.HDRip.x264", "g", None);
        assert!(!parse(&hdrip).hdr, "HDRip is not HDR");
        let hdrezka = raw("x", "Movie.2020.1080p.WEB-DL.HDRezka.Studio", "h", None);
        assert!(!parse(&hdrezka).hdr, "HDRezka is not HDR");
        // Real HDR tags, including one glued to the resolution, still are.
        let hdr10 = raw("x", "Movie.2020.2160p.HDR10+.WEB-DL", "i", None);
        assert!(parse(&hdr10).hdr, "HDR10+ → hdr");
        let glued = raw("x", "Movie.2160pHDR.x265", "j", None);
        assert!(parse(&glued).hdr, "glued 2160pHDR → hdr");
        let both = raw("x", "Movie.2020.2160p.HDR.HDRip.x265", "k", None);
        assert!(parse(&both).hdr, "an HDR tag next to HDRip still counts");
    }

    #[test]
    fn parse_detects_dotted_h265_and_h264_codecs() {
        // Torrentio names sometimes spell the codec with a dot (H.265 / H.264); these must map to
        // Hevc/Avc, not Codec::Other — otherwise a HEVC encode silently loses the PREFER_HEVC bonus.
        assert_eq!(
            parse(&raw("x", "Movie.2020.1080p.WEB-DL.H.265-GRP", "a", None)).codec,
            Codec::Hevc,
            "H.265 → Hevc"
        );
        assert_eq!(
            parse(&raw("x", "Movie.2020.1080p.WEB-DL.H.264-GRP", "b", None)).codec,
            Codec::Avc,
            "H.264 → Avc"
        );
    }

    #[test]
    fn parse_size_skips_a_non_numeric_run_and_takes_the_next_candidate() {
        // `[\d.]+` can capture a non-numeric run like `1.2.3` that DOES form a full `SIZE_RE` match
        // (`1.2.3 GB`) yet fails `parse::<f64>()`; the size parse must `find_map`-skip it and use the
        // next well-formed `<n> GB` token rather than silently dropping the size. The bad run is a
        // real match here (unit-suffixed), so this exercises the parse-fail-then-continue branch —
        // a single-`captures()` implementation would return None and fail this test. (No `💾`
        // marker: a marked size would be taken first, bypassing the bare-token scan.)
        let c = raw("x", "Bad 1.2.3 GB then\n7.5 GB \u{1f464} 10", "a", None);
        assert_eq!(
            parse(&c).size_bytes,
            Some((7.5 * 1_000_000_000.0) as u64),
            "a malformed but unit-suffixed numeric run must be skipped, not drop the real size"
        );
    }

    #[test]
    fn parse_size_prefers_the_floppy_marked_size_over_a_size_in_the_release_name() {
        // Torrentio's description starts with the release title, so a per-episode size marker in a
        // pack's name (`…x265.200MB`, `[150MB/ep]`) comes before the real `💾` size. The marked size
        // wins; a bare token is only the fallback when there is no `💾` size.
        for desc in [
            "Show.S01.720p.WEB-DL.x265.200MB\n\u{1f4be} 5.2 GB \u{1f464} 10",
            "Show S01 [150MB/ep] 720p WEB-DL\n\u{1f464} 10 \u{1f4be} 5.2 GB",
        ] {
            assert_eq!(
                parse(&raw("x", desc, "a", None)).size_bytes,
                Some((5.2 * 1_000_000_000.0) as u64),
                "{desc}"
            );
        }
        // No marked size: the first bare size token is the fallback, and its number doesn't run on
        // from a codec tag before it (`x265.200MB` once read as 265.2 MB).
        for (desc, size) in [
            ("Show.S01.720p.WEB-DL.x265.200MB", 200_000_000),
            ("Movie.2020.1080p.WEB.5.2GB", 5_200_000_000),
            ("4.7GB Movie.2020.1080p", 4_700_000_000),
            // `_` separates a size from the next tag as it does before the number.
            ("Movie_2020_1080p_4.7GB_x264", 4_700_000_000),
            (
                "Show.S01E01.DDP5.1.TBS.WEB-DL.x264 2.3 GB",
                (2.3 * 1_000_000_000.0) as u64,
            ),
        ] {
            assert_eq!(
                parse(&raw("x", desc, "a", None)).size_bytes,
                Some(size),
                "{desc}"
            );
        }
        // A unit glued to more letters is a word, not a size: the `TBS` network tag after an audio
        // `5.1` is no 1 TB release, nor a `MBC` broadcaster tag a 1 MB sample.
        for desc in [
            "Show.S01E01.DDP5.1.TBS.WEB-DL.x264",
            "Movie.2020.1080p.DD5.1.MBC.WEB",
            "Show.S01E01.1080p.WEB.H.264.TBS",
        ] {
            assert_eq!(parse(&raw("x", desc, "a", None)).size_bytes, None, "{desc}");
        }
    }

    #[test]
    fn parse_size_reads_terabytes() {
        // A pack over 1 TB is listed in TB. Without the unit its size was unknown, so it escaped the
        // >80 GB penalty.
        let c = raw(
            "x",
            "Show.S01-S10.1080p.BluRay\n\u{1f4be} 1.2 TB \u{1f464} 3",
            "a",
            None,
        );
        let r = parse(&c);
        assert_eq!(r.size_bytes, Some((1.2 * 1_000_000_000_000.0) as u64));
        let no_size = parse(&raw(
            "x",
            "Show.S01-S10.1080p.BluRay\n\u{1f464} 3",
            "a",
            None,
        ));
        assert!(
            score(&r, &prefs()).unwrap() < score(&no_size, &prefs()).unwrap(),
            "a 1.2 TB release takes the absurd-size penalty"
        );
        // The debrid-cached marker is not a size: `H.264 TB+` is no 264 TB release, nor
        // `DD5.1 TB+` a 1 TB one. Both numbers follow a separator, so only the `TB+` guard in
        // `parse` keeps them from being read as sizes.
        for description in [
            "Movie.2020.1080p.WEB.H.264 TB+",
            "Movie.2020.1080p.WEB.DD5.1 TB+",
        ] {
            let cached = parse(&raw("Torrentio\n1080p", description, "a", None));
            assert!(
                cached.cached,
                "{description}: the marker still flags it cached"
            );
            assert_eq!(cached.size_bytes, None, "{description}");
        }
        // `x264 TB+` never reaches that guard: `264` follows a letter, so `SIZE_RE` reads no size.
        let cached = parse(&raw(
            "Torrentio\n1080p",
            "Movie.2020.1080p.WEB.x264 TB+",
            "a",
            None,
        ));
        assert_eq!(cached.size_bytes, None);
    }

    #[test]
    fn score_passes_through_a_resolution_less_release() {
        // A release with no parseable resolution token must still be SCORED (passed through the
        // MAX_RESOLUTION ceiling), not excluded. Cached so the uncached-zero-seeder floor doesn't
        // confound the ceiling behaviour under test.
        let c = raw("x", "Movie.BluRay.x265-GRP\nRD+", "a", None);
        let r = parse(&c);
        assert_eq!(r.resolution, None, "no resolution token → None");
        assert!(
            score(&r, &prefs()).is_some(),
            "a resolution-less release must still be scored, not excluded by the ceiling"
        );
    }

    #[test]
    fn is_target_improvement_requires_strictly_greater_score_at_equal_score() {
        // The flip-flop-prevention guarantee rests on the score comparison being STRICT (`>`). Two
        // cached releases with an EQUAL score but a genuine tier/resolution category jump must NOT be
        // upgrades of each other — if this regressed to `>=`, the upgrade engine would swap+prune
        // forever each idle tick (churn + a torn-down stream).
        // A P2160 ceiling keeps the resolution-jump candidate within-ceiling here too, so the
        // equal-score comparison — not the over-ceiling penalty — is what's under test.
        let prefs = QualityPrefs {
            max_resolution: MaxResolution::P2160,
            ..prefs()
        };
        let cur_tier = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 100,
            unverified: false,
        };
        let cand_higher_tier = QualitySummary {
            cached: true,
            source_tier: 8_000, // a real tier jump…
            resolution: 1080,
            score: 100, // …but identical score
            unverified: false,
        };
        assert_eq!(
            is_target_improvement(&cur_tier, &cand_higher_tier, &prefs),
            Improvement::None,
            "equal score + tier jump must NOT be an upgrade (strict > guards against flip-flop)"
        );
        let cur_res = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 1080,
            score: 200,
            unverified: false,
        };
        let cand_higher_res = QualitySummary {
            cached: true,
            source_tier: 3_000,
            resolution: 2160, // a real resolution jump…
            score: 200,       // …but identical score
            unverified: false,
        };
        assert_eq!(
            is_target_improvement(&cur_res, &cand_higher_res, &prefs),
            Improvement::None,
            "equal score + resolution jump must NOT be an upgrade"
        );
    }

    #[test]
    fn mark_cached_flips_flag_adds_bonus_and_is_idempotent() {
        // An uncached snapshot scored without the cached bonus…
        let uncached = parse(&raw(
            "t",
            "A.1080p.WEB.x265\n\u{1f4be} 8 GB",
            "h",
            Some("A.mkv"),
        ));
        let mut q = QualitySummary::of(&uncached, &prefs());
        assert!(!q.cached);
        let base_score = q.score;
        // …must, once the torrent is present/cached, equal what `of()` yields for the cached release.
        q.mark_cached();
        let cached = parse(&raw(
            "t",
            "A.1080p.WEB.x265\n\u{1f4be} 8 GB",
            "h",
            Some("A.mkv"),
        ));
        let cached_q = QualitySummary::of(
            &ReleaseInfo {
                cached: true,
                ..cached
            },
            &prefs(),
        );
        assert!(q.cached);
        assert_eq!(q.score, base_score + CACHED_BONUS);
        assert_eq!(
            q.score, cached_q.score,
            "mark_cached must match a freshly-scored cached release"
        );
        // Idempotent: a second call must not double-add the bonus.
        let after = q.score;
        q.mark_cached();
        assert_eq!(q.score, after);
    }

    #[test]
    fn upgrade_does_not_downgrade_resolution_after_uncached_copy_is_marked_cached() {
        // Regression for the stale-`cached` bug: a movie acquired while UNCACHED (1080p WEB) records
        // `cached:false`; once it downloads, `record_verified` calls `mark_cached`. After that, a
        // LOWER-resolution but higher-tier CACHED candidate (720p BluRay) must NOT be a "meaningful
        // upgrade" — resolution dominates, so swapping+pruning to it would be a downgrade + data loss.
        let owned_uncached = parse(&raw(
            "t",
            "Movie.1080p.WEB.x265\n\u{1f4be} 8 GB",
            "h1",
            Some("Movie.mkv"),
        ));
        let mut current = QualitySummary::of(&owned_uncached, &prefs());
        // Before the fix `current.cached` stayed false → the `!current.cached` short-circuit made the
        // 720p candidate a (wrong) upgrade. After `mark_cached`, the score comparison runs correctly.
        current.mark_cached();
        let cand_720_bluray = parse(&raw(
            "t",
            "Movie.720p.BluRay.x265\n\u{1f4be} 6 GB",
            "h2",
            Some("Movie.mkv"),
        ));
        let candidate = QualitySummary::of(
            &ReleaseInfo {
                cached: true,
                ..cand_720_bluray
            },
            &prefs(),
        );
        assert!(candidate.cached && current.cached);
        assert_eq!(
            is_target_improvement(&current, &candidate, &prefs()),
            Improvement::None,
            "a lower-resolution cached release must not upgrade a now-cached higher-resolution owned copy"
        );
    }

    #[cfg(test)]
    mod target_improvement_tests {
        use super::{is_target_improvement, Improvement, QualitySummary};
        use crate::config::{AudioReq, MaxResolution, QualityPrefs, SubReq};

        fn prefs(ceiling: MaxResolution) -> QualityPrefs {
            QualityPrefs {
                max_resolution: ceiling,
                audio: AudioReq::Original,
                subtitle: SubReq::None,
                prefer_hevc: true,
                prefer_hdr: false,
            }
        }
        fn cached(res: u16, tier: i64) -> QualitySummary {
            QualitySummary {
                cached: true,
                source_tier: tier,
                resolution: res,
                score: 1_000_000 + tier + res as i64 * 100,
                unverified: false,
            }
        }

        #[test]
        fn over_ceiling_current_and_within_candidate_is_downgrade() {
            // Own 2160; ceiling dropped to 1080; a cached 1080 exists.
            let cur = cached(2160, 8_000);
            let cand = cached(1080, 6_000);
            assert_eq!(
                is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                Improvement::Downgrade
            );
        }

        #[test]
        fn over_ceiling_current_and_untagged_candidate_is_not_downgrade() {
            // A corrective downgrade DELETES the known over-ceiling copy, so it must only ever replace
            // it with a PROVABLY within-ceiling release. An untagged candidate (`resolution == 0`) is
            // NOT filtered by `score()` (it lets unknown resolutions through), so it might actually
            // still be over the ceiling — and would then be pruned-in and recorded as a within-ceiling
            // `resolution: 0` lie. It must read as `None`, so the sole over-ceiling copy is kept rather
            // than swapped for an unverifiable one.
            let cur = cached(2160, 8_000);
            let mut untagged = cached(1080, 6_000);
            untagged.resolution = 0; // scraper failed to tag the resolution
            assert_eq!(
                is_target_improvement(&cur, &untagged, &prefs(MaxResolution::P1080)),
                Improvement::None,
                "an untagged-resolution candidate must not trigger a corrective downgrade"
            );
            // A tagged within-ceiling candidate still downgrades (guard is scoped to untagged only).
            let tagged = cached(1080, 6_000);
            assert_eq!(
                is_target_improvement(&cur, &tagged, &prefs(MaxResolution::P1080)),
                Improvement::Downgrade
            );
        }

        #[test]
        fn within_ceiling_current_is_not_downgraded_for_higher_tier_lower_res() {
            // Regression: a WITHIN-ceiling current copy must never be traded DOWN in resolution for a
            // higher-tier/bonus lower-res candidate. Here the current 1080p (exactly at the 1080
            // ceiling) carries a wrong-audio penalty, so a 720p REMUX+HEVC candidate out-scores it —
            // but 720p is a resolution regression and the current copy is NOT over the ceiling, so
            // this must be `None`, not `Downgrade`. As `Downgrade` it would, under
            // ALLOW_RESOLUTION_DOWNGRADE, prune the 1080p copy — a silent resolution loss the flag
            // never authorised (the flag only corrects copies sitting ABOVE the ceiling).
            let cur = QualitySummary {
                cached: true,
                source_tier: 3_000, // WEB
                resolution: 1080,   // exactly at the ceiling -> within
                // 1_000_000 + 3_000 + 108_000 - 50_000 (wrong-audio penalty) = 1_061_000
                score: 1_061_000,
                unverified: false,
            };
            let cand = QualitySummary {
                cached: true,
                source_tier: 8_000, // REMUX
                resolution: 720,
                // 1_000_000 + 8_000 + 72_000 + 5_000 (hevc) = 1_085_000 (> cur)
                score: 1_085_000,
                unverified: false,
            };
            assert!(
                cand.score > cur.score,
                "test setup: candidate must out-score current"
            );
            assert_eq!(
                is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                Improvement::None,
                "within-ceiling current must not be downgraded to a lower resolution"
            );
        }

        #[test]
        fn genuine_higher_resolution_is_upgrade() {
            let cur = cached(1080, 3_000);
            let cand = cached(2160, 3_000);
            assert_eq!(
                is_target_improvement(&cur, &cand, &prefs(MaxResolution::P2160)),
                Improvement::Upgrade
            );
        }

        #[test]
        fn same_tier_same_res_wobble_is_none() {
            let cur = cached(1080, 3_000);
            let mut cand = cached(1080, 3_000);
            cand.score += 500; // bitrate/HEVC wobble only, no category change
            assert_eq!(
                is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                Improvement::None
            );
        }

        #[test]
        fn uncached_candidate_is_none() {
            let cur = cached(1080, 3_000);
            let mut cand = cached(2160, 8_000);
            cand.cached = false;
            assert_eq!(
                is_target_improvement(&cur, &cand, &prefs(MaxResolution::P2160)),
                Improvement::None
            );
        }

        #[test]
        fn uncached_current_any_cached_is_upgrade() {
            let mut cur = cached(1080, 3_000);
            cur.cached = false;
            let cand = cached(720, 1_000);
            assert_eq!(
                is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                Improvement::Upgrade
            );
        }

        #[test]
        fn unverified_current_is_improved_by_equal_score_candidate() {
            // An owned copy whose tracks could never be checked (an `.avi`, or an MP4 whose `moov`
            // couldn't be read) is replaced by an EQUAL-quality cached release — no category jump
            // needed — because the replacement will be probed (the upgrade pass only adopts a
            // `Pass`), so the library trades an unchecked copy for a checked one at no quality cost.
            let mut cur = cached(1080, 3_000);
            cur.unverified = true;
            let cand = cached(1080, 3_000);
            assert_eq!(cand.score, cur.score, "test setup: equal score");
            assert_eq!(
                is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                Improvement::Upgrade
            );
            // A higher-scoring same-category candidate qualifies too (`>=`, not only `==`).
            let mut better = cached(1080, 3_000);
            better.score += 500;
            assert_eq!(
                is_target_improvement(&cur, &better, &prefs(MaxResolution::P1080)),
                Improvement::Upgrade
            );
            // A LOWER-scoring same-resolution candidate does not: the swap must never cost quality.
            let mut worse = cached(1080, 3_000);
            worse.score -= 1;
            assert_eq!(
                is_target_improvement(&cur, &worse, &prefs(MaxResolution::P1080)),
                Improvement::None
            );
        }

        #[test]
        fn verified_current_equal_score_is_still_none() {
            // The no-churn guard is unchanged for a checked copy: an equal (or same-category
            // marginally better) candidate is not an upgrade.
            let cur = cached(1080, 3_000);
            assert!(!cur.unverified);
            let cand = cached(1080, 3_000);
            assert_eq!(
                is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                Improvement::None
            );
            let mut wobble = cached(1080, 3_000);
            wobble.score += 500;
            assert_eq!(
                is_target_improvement(&cur, &wobble, &prefs(MaxResolution::P1080)),
                Improvement::None
            );
        }

        #[test]
        fn unverified_current_never_trades_resolution() {
            // The unverified rule needs `resolution >= current`: a within-ceiling unverified copy is
            // never traded DOWN in resolution, even for a higher-scoring candidate.
            let mut cur = cached(1080, 3_000);
            cur.unverified = true;
            let mut cand = cached(720, 8_000);
            cand.score = cur.score + 10_000;
            assert_eq!(
                is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                Improvement::None,
                "a lower-resolution candidate must not replace a within-ceiling unverified copy"
            );
            // Over-ceiling corrective-downgrade semantics are unchanged by the flag.
            let mut over = cached(2160, 8_000);
            over.unverified = true;
            assert_eq!(
                is_target_improvement(&over, &cached(1080, 6_000), &prefs(MaxResolution::P1080)),
                Improvement::Downgrade
            );
        }

        #[test]
        fn legacy_quality_summary_decodes_as_verified() {
            // A `QualitySummary` persisted before the `unverified` flag existed decodes as checked
            // (`false`), so legacy owned records keep their exact upgrade behaviour.
            let q: QualitySummary = serde_json::from_str(
                r#"{"cached":true,"source_tier":3000,"resolution":1080,"score":1111000}"#,
            )
            .expect("legacy summary must decode");
            assert!(!q.unverified);
            assert_eq!(q.resolution, 1080);
        }
    }

    #[cfg(test)]
    mod effective_score_tests {
        use super::{effective_score, QualitySummary};
        use crate::config::{AudioReq, MaxResolution, QualityPrefs, SubReq};

        fn prefs(ceiling: MaxResolution) -> QualityPrefs {
            QualityPrefs {
                max_resolution: ceiling,
                audio: AudioReq::Original,
                subtitle: SubReq::None,
                prefer_hevc: true,
                prefer_hdr: false,
            }
        }

        // A cached release scored by score(): CACHED_BONUS + tier + resolution*100 (+ bonuses).
        fn cached(res: u16, tier: i64) -> QualitySummary {
            QualitySummary {
                cached: true,
                source_tier: tier,
                resolution: res,
                score: 1_000_000 + tier + res as i64 * 100,
                unverified: false,
            }
        }

        #[test]
        fn within_ceiling_is_unchanged() {
            let q = cached(1080, 6_000);
            assert_eq!(effective_score(&q, &prefs(MaxResolution::P1080)), q.score);
            // At the ceiling exactly is still "within".
            assert_eq!(effective_score(&q, &prefs(MaxResolution::P2160)), q.score);
        }

        #[test]
        fn above_ceiling_scores_below_any_within_ceiling_cached() {
            let over = cached(2160, 8_000); // 4K REMUX, ceiling 1080
            let p = prefs(MaxResolution::P1080);
            let over_eff = effective_score(&over, &p);
            // Worst-case comparators that are WITHIN the 1080 ceiling and cached:
            for within in [cached(1080, 8_000), cached(1080, 3_000), cached(480, 1_000)] {
                assert!(
                    over_eff < effective_score(&within, &p),
                    "over-ceiling {over_eff} must be below within-ceiling {}",
                    within.score
                );
            }
        }

        #[test]
        fn more_over_ceiling_scores_below_less_over_ceiling() {
            let p = prefs(MaxResolution::P720);
            // Less-over-ceiling copy with the WEAKEST non-resolution score...
            let less_over = QualitySummary {
                cached: true,
                source_tier: 0,
                resolution: 1080,
                score: 1_000_000 + 1080 * 100,
                unverified: false,
            };
            // ...must still beat a more-over-ceiling copy with the STRONGEST (max tier + bonuses).
            let more_over = QualitySummary {
                cached: true,
                source_tier: 8_000,
                resolution: 2160,
                score: 1_000_000 + 8_000 + 2160 * 100 + 15_000,
                unverified: false,
            };
            assert!(effective_score(&more_over, &p) < effective_score(&less_over, &p));
        }
    }

    /// Unknown quality is LOW quality: a `None` summary, or one whose resolution or source tier is 0
    /// (that axis untagged), ranks below every known quality — EXCEPT a copy whose recorded resolution
    /// is above `MAX_RESOLUTION`, which is never unknown-lowest (it stays under the corrective-downgrade
    /// rules and `ALLOW_RESOLUTION_DOWNGRADE`).
    #[cfg(test)]
    mod unknown_quality_tests {
        use super::{
            is_target_improvement, is_unknown_quality, parse, quality_rank, Improvement,
            QualitySummary, RawCandidate,
        };
        use crate::config::{AudioReq, MaxResolution, QualityPrefs, SubReq};

        fn prefs(ceiling: MaxResolution) -> QualityPrefs {
            QualityPrefs {
                max_resolution: ceiling,
                audio: AudioReq::Original,
                subtitle: SubReq::None,
                prefer_hevc: true,
                prefer_hdr: false,
            }
        }
        fn q(res: u16, tier: i64, score: i64) -> QualitySummary {
            QualitySummary {
                cached: true,
                source_tier: tier,
                resolution: res,
                score,
                unverified: false,
            }
        }

        #[test]
        fn unknown_is_none_or_an_untagged_axis_but_never_an_over_ceiling_resolution() {
            assert!(is_unknown_quality(&QualitySummary::unknown(), 1080));
            assert!(
                is_unknown_quality(&q(0, 3_000, 9), 1080),
                "resolution untagged"
            );
            assert!(
                is_unknown_quality(&q(1080, 0, 9), 1080),
                "source tier untagged"
            );
            assert!(
                !is_unknown_quality(&q(720, 3_000, 9), 1080),
                "both axes known"
            );
            assert!(
                !is_unknown_quality(&q(2160, 0, 9), 1080),
                "a resolution above the ceiling is never unknown-lowest, whatever its tier"
            );
            assert!(
                is_unknown_quality(&q(2160, 0, 9), 2160),
                "at a 2160 ceiling the same copy is within the ceiling, so its untagged tier is unknown"
            );
        }

        #[test]
        fn quality_rank_is_resolution_then_tier_then_checked() {
            let web720 = quality_rank(Some(&q(720, 3_000, 1)));
            let untagged_source_1080 = quality_rank(Some(&q(1080, 0, 1)));
            let untagged_res_remux = quality_rank(Some(&q(0, 8_000, 999_999)));
            let none = quality_rank(None);
            assert!(
                untagged_source_1080 > web720,
                "a known resolution stays a floor: 1080p with an untagged source outranks 720p WEB"
            );
            assert!(
                quality_rank(Some(&q(1080, 1_000, 1))) > untagged_source_1080,
                "at the same resolution a known source tier beats an untagged one"
            );
            assert!(
                web720 > untagged_res_remux,
                "an untagged resolution is the lowest, whatever its tier or score"
            );
            assert!(
                untagged_res_remux > none,
                "a recorded tier beats nothing at all"
            );
            assert!(
                quality_rank(Some(&q(1080, 8_000, 1))) > quality_rank(Some(&q(1080, 6_000, 9))),
                "same resolution → higher tier wins (score is not part of the rank)"
            );
            assert!(
                quality_rank(Some(&q(2160, 3_000, 1))) > quality_rank(Some(&q(1080, 8_000, 9))),
                "resolution dominates tier, and an over-ceiling copy is never ranked down"
            );
            assert!(
                quality_rank(Some(&q(2160, 0, 1))) > quality_rank(Some(&q(1080, 8_000, 9))),
                "an over-ceiling copy with an untagged tier is not unknown-lowest"
            );
            assert_eq!(
                quality_rank(Some(&q(1080, 6_000, 1))),
                quality_rank(Some(&q(1080, 6_000, 9))),
                "same resolution + tier = same quality (score ignored)"
            );
            let mut unchecked = q(1080, 6_000, 9);
            unchecked.unverified = true;
            assert!(
                quality_rank(Some(&q(1080, 6_000, 1))) > quality_rank(Some(&unchecked)),
                "at the same resolution and tier a checked copy beats an unchecked one"
            );
            assert!(
                quality_rank(Some(&unchecked)) > quality_rank(Some(&q(1080, 3_000, 9))),
                "but `unverified` never outweighs a better tier"
            );
        }

        #[test]
        fn any_known_within_ceiling_candidate_improves_an_unknown_current() {
            let p = prefs(MaxResolution::P1080);
            let known = q(720, 3_000, 1_000_100);
            for current in [
                QualitySummary::unknown(),
                q(0, 3_000, 9_000_000), // resolution untagged, even with a higher score
                q(720, 0, 9_000_000),   // source untagged, at the same resolution
                q(0, 0, 1),
            ] {
                assert_eq!(
                    is_target_improvement(&current, &known, &p),
                    Improvement::Upgrade,
                    "{current:?} is unknown → any known cached within-ceiling candidate improves it"
                );
                assert_eq!(
                    is_target_improvement(&current, &q(1080, 0, 9_999_999), &p),
                    Improvement::None,
                    "an UNKNOWN candidate never improves an unknown current (no unknown→unknown churn)"
                );
                let mut uncached = known.clone();
                uncached.cached = false;
                assert_eq!(
                    is_target_improvement(&current, &uncached, &p),
                    Improvement::None,
                    "an uncached candidate is never an improvement"
                );
            }
        }

        /// An unknown copy whose RESOLUTION is known keeps it as a floor: only a known candidate at
        /// that resolution or higher improves it, so a 1080p copy with an untagged source is never
        /// replaced by a 720p release on the ungated upgrade path (only a corrective downgrade, under
        /// `ALLOW_RESOLUTION_DOWNGRADE`, ever trades resolution down).
        #[test]
        fn an_unknown_current_keeps_its_known_resolution_as_a_floor() {
            let p = prefs(MaxResolution::P1080);
            let untagged_source_1080 = q(1080, 0, 9_000_000);
            assert_eq!(
                is_target_improvement(&untagged_source_1080, &q(720, 6_000, 1_000_100), &p),
                Improvement::None,
                "a known 720p BluRay never replaces a 1080p copy with an untagged source"
            );
            assert_eq!(
                is_target_improvement(&untagged_source_1080, &q(1080, 1_000, 1), &p),
                Improvement::Upgrade,
                "a known 1080p release does, whatever its score"
            );
            assert_eq!(
                is_target_improvement(&q(720, 0, 9_000_000), &q(1080, 3_000, 1), &p),
                Improvement::Upgrade,
                "and so does a higher resolution"
            );
            assert_eq!(
                is_target_improvement(&q(0, 8_000, 9_000_000), &q(720, 3_000, 1), &p),
                Improvement::Upgrade,
                "an untagged resolution sets no floor"
            );
        }

        #[test]
        fn an_unknown_candidate_never_improves_a_known_within_ceiling_current() {
            let p = prefs(MaxResolution::P2160);
            let current = q(720, 3_000, 1_075_000);
            // Higher resolution AND higher score, but its source tier is untagged → unknown → lower.
            let cand = q(1080, 0, 1_110_000);
            assert_eq!(
                is_target_improvement(&current, &cand, &p),
                Improvement::None,
                "unknown quality ranks below every known quality"
            );
        }

        #[test]
        fn an_over_ceiling_untagged_tier_copy_stays_under_the_corrective_downgrade_rules() {
            let p = prefs(MaxResolution::P1080);
            let over = q(2160, 0, 1_216_000);
            assert_eq!(
                is_target_improvement(&over, &q(1080, 6_000, 1_114_000), &p),
                Improvement::Downgrade,
                "an over-ceiling copy is replaced only as a gated corrective downgrade, never as unknown"
            );
        }

        #[test]
        fn replacing_an_unknown_copy_converges() {
            let p = prefs(MaxResolution::P1080);
            let adopted = q(1080, 3_000, 1_113_000);
            assert_eq!(
                is_target_improvement(&QualitySummary::unknown(), &adopted, &p),
                Improvement::Upgrade
            );
            // Once adopted the copy is known: the same release, an equal one, or any unknown one is
            // no improvement on the next visit.
            for again in [
                adopted.clone(),
                q(1080, 3_000, 1_113_500),
                q(0, 8_000, 9_999_999),
            ] {
                assert_eq!(
                    is_target_improvement(&adopted, &again, &p),
                    Improvement::None
                );
            }
        }

        #[test]
        fn of_owned_records_an_over_ceiling_file_instead_of_dropping_it() {
            let p = prefs(MaxResolution::P1080);
            let r = |name: &str| {
                let mut r = parse(&RawCandidate {
                    name: name.into(),
                    description: String::new(),
                    info_hash: "h".into(),
                    file_idx: None,
                    file_name: Some(name.into()),
                });
                r.cached = true;
                r
            };
            let over = QualitySummary::of_owned(&r("Show.S04E05.2160p.WEB.h265-GRP.mkv"), &p);
            assert_eq!((over.resolution, over.source_tier), (2160, 3_000));
            assert!(over.cached);
            assert!(
                over.score > 1_000_000,
                "scored as if uncapped (not the hard-filter sentinel), got {}",
                over.score
            );
            assert!(!is_unknown_quality(&over, 1080));
            let within = QualitySummary::of_owned(&r("Movie.2020.1080p.BluRay.x264.mkv"), &p);
            assert_eq!(
                within,
                QualitySummary::of(&r("Movie.2020.1080p.BluRay.x264.mkv"), &p)
            );
            let untagged = QualitySummary::of_owned(&r("The Matrix (1999).mkv"), &p);
            assert_eq!((untagged.resolution, untagged.source_tier), (0, 0));
            assert!(is_unknown_quality(&untagged, 1080));
            let cam = QualitySummary::of_owned(&r("Movie.2024.1080p.CAM.x264.mkv"), &p);
            assert_eq!(
                cam.source_tier, 0,
                "a cam copy is unknown-lowest, never acquired over"
            );
            assert!(is_unknown_quality(&cam, 1080));
        }
    }

    /// `wrong_audio_language` is exactly the condition `score`'s wrong-`AUDIO_LANGUAGE` penalty
    /// applies on: a release naming (or flagging) only audio languages outside a CONCRETE
    /// requirement. Untagged and multi-audio releases, and a raw `original` (no per-title language),
    /// never carry it.
    #[test]
    fn wrong_audio_language_matches_the_score_penalty() {
        let rel = |tag: &str, flag: &str| {
            parse(&raw(
                "Torrentio\n1080p",
                &format!("M.2020.1080p.BluRay.x264.{tag}\n{flag}\nRD+"),
                "h",
                None,
            ))
        };
        let rus = rel("Rus", "\u{1f1f7}\u{1f1fa}");
        let eng = rel("ENG", "\u{1f1ec}\u{1f1e7}");
        let untagged = rel("GRP", "");
        let multi = rel("MULTi", "");
        let english = prefs().with_resolved_audio(Some("en"), &[]);
        assert!(wrong_audio_language(&rus, &english));
        assert!(!wrong_audio_language(&eng, &english));
        assert!(!wrong_audio_language(&untagged, &english));
        assert!(!wrong_audio_language(&multi, &english));
        assert!(
            !wrong_audio_language(&rus, &prefs()),
            "a raw `original` has no language to compare against"
        );
        let configured = QualityPrefs {
            audio: AudioReq::Lang("en".into()),
            ..prefs()
        };
        assert!(wrong_audio_language(&rus, &configured));
        for r in [&rus, &eng, &untagged, &multi] {
            assert_eq!(
                score(r, &english).unwrap() < score(r, &prefs()).unwrap(),
                wrong_audio_language(r, &english),
                "{:?}: the penalty applies exactly when the predicate holds",
                r.languages
            );
        }
    }
}
