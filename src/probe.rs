use crate::config::{AudioReq, SubReq};
use tracing::debug;

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
    /// Recognised container, definitively broken structure → treat as a bad release.
    Corrupt,
    /// Container we don't parse (AVI/TS/…) → accept-with-warning.
    Unsupported,
    /// Parsed OK but no track info located in the fetched bytes → accept-with-warning.
    TracksNotFound,
}

/// The resolved language requirement (from `QualityPrefs` + the title's TMDB original language).
#[derive(Debug, Clone)]
pub struct LangReq {
    pub audio: AudioReq,
    pub subtitle: SubReq,
    pub original_language: Option<String>,
}

/// Normalise a language code to ISO 639-2/B (bibliographic). Maps the COMPLETE ISO 639-1 (2-letter)
/// set to /B — TMDB's `original_language` is 2-letter and the default `AudioReq::Original` compares
/// it against a 3-letter track tag, so an incomplete map would FailAudio-blacklist correctly-tagged
/// foreign releases — and canonicalises the 20 ISO 639-2/T (terminological) 3-letter codes to their
/// /B equivalents so a /T-tagged track unifies with a /B want. Unknown/already-/B 3-letter and
/// unknown 2-letter codes pass through unchanged.
pub fn to_iso639_2(code: &str) -> String {
    let c = code.trim().to_ascii_lowercase();
    if c.len() == 3 {
        // ISO 639-2/T → /B for the 20 languages whose B and T codes differ. The rest of the
        // pipeline (the 639-1 map below, TMDB original_language) speaks /B; muxers like ffmpeg emit
        // /T (e.g. `deu`/`fra`/`zho`/`nld`), so without this a correct German/French/… release is
        // wrongly blacklisted by the probe. B==T codes (jpn, rus, spa, …) need no entry.
        return match c.as_str() {
            "sqi" => "alb",
            "hye" => "arm",
            "eus" => "baq",
            "mya" => "bur",
            "zho" => "chi",
            "ces" => "cze",
            "nld" => "dut",
            "fra" => "fre",
            "kat" => "geo",
            "deu" => "ger",
            "ell" => "gre",
            "isl" => "ice",
            "mkd" => "mac",
            "mri" => "mao",
            "msa" => "may",
            "fas" => "per",
            "ron" => "rum",
            "slk" => "slo",
            "bod" => "tib",
            "cym" => "wel",
            _ => return c, // already /B, or an unknown 3-letter code
        }
        .to_string();
    }
    // Complete ISO 639-1 → 639-2/B table. For the 20 languages whose /B and /T differ, this uses
    // the /B form (the /T 3-letter form is folded to /B by the block above), so all three
    // representations of a language compare equal.
    match c.as_str() {
        "aa" => "aar",
        "ab" => "abk",
        "ae" => "ave",
        "af" => "afr",
        "ak" => "aka",
        "am" => "amh",
        "an" => "arg",
        "ar" => "ara",
        "as" => "asm",
        "av" => "ava",
        "ay" => "aym",
        "az" => "aze",
        "ba" => "bak",
        "be" => "bel",
        "bg" => "bul",
        "bh" => "bih",
        "bi" => "bis",
        "bm" => "bam",
        "bn" => "ben",
        "bo" => "tib",
        "br" => "bre",
        "bs" => "bos",
        "ca" => "cat",
        "ce" => "che",
        "ch" => "cha",
        "co" => "cos",
        "cr" => "cre",
        "cs" => "cze",
        "cu" => "chu",
        "cv" => "chv",
        "cy" => "wel",
        "da" => "dan",
        "de" => "ger",
        "dv" => "div",
        "dz" => "dzo",
        "ee" => "ewe",
        "el" => "gre",
        "en" => "eng",
        "eo" => "epo",
        "es" => "spa",
        "et" => "est",
        "eu" => "baq",
        "fa" => "per",
        "ff" => "ful",
        "fi" => "fin",
        "fj" => "fij",
        "fo" => "fao",
        "fr" => "fre",
        "fy" => "fry",
        "ga" => "gle",
        "gd" => "gla",
        "gl" => "glg",
        "gn" => "grn",
        "gu" => "guj",
        "gv" => "glv",
        "ha" => "hau",
        "he" => "heb",
        "hi" => "hin",
        "ho" => "hmo",
        "hr" => "hrv",
        "ht" => "hat",
        "hu" => "hun",
        "hy" => "arm",
        "hz" => "her",
        "ia" => "ina",
        "id" => "ind",
        "ie" => "ile",
        "ig" => "ibo",
        "ii" => "iii",
        "ik" => "ipk",
        "io" => "ido",
        "is" => "ice",
        "it" => "ita",
        "iu" => "iku",
        "ja" => "jpn",
        "jv" => "jav",
        "ka" => "geo",
        "kg" => "kon",
        "ki" => "kik",
        "kj" => "kua",
        "kk" => "kaz",
        "kl" => "kal",
        "km" => "khm",
        "kn" => "kan",
        "ko" => "kor",
        "kr" => "kau",
        "ks" => "kas",
        "ku" => "kur",
        "kv" => "kom",
        "kw" => "cor",
        "ky" => "kir",
        "la" => "lat",
        "lb" => "ltz",
        "lg" => "lug",
        "li" => "lim",
        "ln" => "lin",
        "lo" => "lao",
        "lt" => "lit",
        "lu" => "lub",
        "lv" => "lav",
        "mg" => "mlg",
        "mh" => "mah",
        "mi" => "mao",
        "mk" => "mac",
        "ml" => "mal",
        "mn" => "mon",
        "mr" => "mar",
        "ms" => "may",
        "mt" => "mlt",
        "my" => "bur",
        "na" => "nau",
        "nb" => "nob",
        "nd" => "nde",
        "ne" => "nep",
        "ng" => "ndo",
        "nl" => "dut",
        "nn" => "nno",
        "no" => "nor",
        "nr" => "nbl",
        "nv" => "nav",
        "ny" => "nya",
        "oc" => "oci",
        "oj" => "oji",
        "om" => "orm",
        "or" => "ori",
        "os" => "oss",
        "pa" => "pan",
        "pi" => "pli",
        "pl" => "pol",
        "ps" => "pus",
        "pt" => "por",
        "qu" => "que",
        "rm" => "roh",
        "rn" => "run",
        "ro" => "rum",
        "ru" => "rus",
        "rw" => "kin",
        "sa" => "san",
        "sc" => "srd",
        "sd" => "snd",
        "se" => "sme",
        "sg" => "sag",
        "si" => "sin",
        "sk" => "slo",
        "sl" => "slv",
        "sm" => "smo",
        "sn" => "sna",
        "so" => "som",
        "sq" => "alb",
        "sr" => "srp",
        "ss" => "ssw",
        "st" => "sot",
        "su" => "sun",
        "sv" => "swe",
        "sw" => "swa",
        "ta" => "tam",
        "te" => "tel",
        "tg" => "tgk",
        "th" => "tha",
        "ti" => "tir",
        "tk" => "tuk",
        "tl" => "tgl",
        "tn" => "tsn",
        "to" => "ton",
        "tr" => "tur",
        "ts" => "tso",
        "tt" => "tat",
        "tw" => "twi",
        "ty" => "tah",
        "ug" => "uig",
        "uk" => "ukr",
        "ur" => "urd",
        "uz" => "uzb",
        "ve" => "ven",
        "vi" => "vie",
        "vo" => "vol",
        "wa" => "wln",
        "wo" => "wol",
        "xh" => "xho",
        "yi" => "yid",
        "yo" => "yor",
        "za" => "zha",
        "zh" => "chi",
        "zu" => "zul",
        _ => return c,
    }
    .to_string()
}

fn lang_eq(a: &str, b: &str) -> bool {
    to_iso639_2(a) == to_iso639_2(b)
}

enum LangCheck {
    Pass,
    Fail,
    Inconclusive,
}

/// Does the requirement for language `want` hold for tracks of `kind`?
/// - `Pass`: a track is tagged with `want`.
/// - `Inconclusive`: a track of that kind carries no determinable language (it *could* be `want`);
///   we only have positive evidence when a language is actually tagged, so don't reject.
/// - `Fail`: every track of that kind is tagged with a *different* language, or there are none —
///   i.e. the wanted language is positively absent.
fn check_lang(tracks: &[Track], kind: TrackKind, want: &str) -> LangCheck {
    let mut has_untagged = false;
    for t in tracks.iter().filter(|t| t.kind == kind) {
        match &t.language {
            Some(l) if lang_eq(l, want) => return LangCheck::Pass,
            Some(_) => {}
            None => has_untagged = true,
        }
    }
    if has_untagged {
        LangCheck::Inconclusive
    } else {
        LangCheck::Fail
    }
}

/// Verify parsed tracks against the requirement. Reject only on *positive* evidence of a
/// violation (a track tagged with a non-matching language, or the required track positively
/// absent). Untagged tracks (no language metadata) are inconclusive — not a failure — so a
/// correct-but-untagged release isn't wrongly rejected. This deliberately also accepts a
/// non-matching *tagged* track when an untagged track is present alongside it (the untagged one
/// could be the wanted language). Note: MKV tracks that OMIT the Language element are pre-resolved
/// to `eng` by the parser (Matroska's spec default); an MKV track tagged `und`/empty (undetermined)
/// or an MP4 untagged track resolves to `language: None` and reaches the inconclusive branch.
pub fn verify(tracks: &[Track], req: &LangReq) -> Verify {
    if tracks.is_empty() {
        return Verify::Inconclusive;
    }
    let want_audio: Option<String> = match &req.audio {
        AudioReq::Lang(l) => Some(l.clone()),
        AudioReq::Original => req.original_language.clone(),
    };
    if let Some(want) = want_audio {
        if let LangCheck::Fail = check_lang(tracks, TrackKind::Audio, &want) {
            return Verify::FailAudio;
        }
    }
    if let SubReq::Lang(want) = &req.subtitle {
        if let LangCheck::Fail = check_lang(tracks, TrackKind::Subtitle, want) {
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

/// Classify an element/box whose declared extent runs past a boundary. If it runs past the
/// *fetched buffer* we simply under-fetched (a truncated ranged read under load) → `Transient`
/// (defer + retry), never a verdict. If it stays within the fetched bytes but violates a parent
/// boundary, the structure is genuinely broken → `Corrupt`.
fn overrun_error(declared_end: usize, buf_len: usize) -> ProbeError {
    if declared_end > buf_len {
        ProbeError::Transient
    } else {
        ProbeError::Corrupt
    }
}

// --- MKV (EBML) parser ---

/// Read an EBML element id (1..=4 bytes, marker bits retained). Advances `pos`.
fn read_ebml_id(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let first = *buf.get(*pos)?;
    let len = first.leading_zeros() as usize + 1;
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

/// Read an EBML data size vint (marker stripped). All-ones → `u64::MAX` (unknown size).
fn read_ebml_size(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let first = *buf.get(*pos)?;
    if first == 0 {
        return None;
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

/// `base + size` as a `usize`, or `None` on overflow. `usize::try_from` first so a declared `size`
/// (a `u64` read from untrusted bytes) larger than `usize::MAX` can't truncate on a 32-bit target
/// and silently wrap past the overrun guard (the deployment targets are 64-bit, where `try_from` is
/// always Ok, so this is hardening, not a behaviour change).
fn ebml_end(base: usize, size: u64) -> Option<usize> {
    usize::try_from(size).ok().and_then(|s| base.checked_add(s))
}

/// Parse MKV track languages. `Corrupt` on a structurally-broken header,
/// `TracksNotFound` if no `Tracks` element is present in the buffer.
pub fn parse_mkv_tracks(buf: &[u8]) -> Result<Vec<Track>, ProbeError> {
    if detect_container(buf) != Some(ContainerKind::Mkv) {
        return Err(ProbeError::Corrupt);
    }
    let segment = find_ebml_child(buf, 0, buf.len(), 0x18538067)?;
    let (seg_start, seg_end) = match segment {
        Some(r) => r,
        None => return Err(ProbeError::TracksNotFound),
    };
    let tracks = find_ebml_child(buf, seg_start, seg_end, 0x1654AE6B)?;
    let (t_start, t_end) = match tracks {
        Some(r) => r,
        None => return Err(ProbeError::TracksNotFound),
    };
    let mut out = Vec::new();
    let mut pos = t_start;
    while pos < t_end {
        let id = read_ebml_id(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        let size = read_ebml_size(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        let end = if size == u64::MAX {
            t_end
        } else {
            ebml_end(pos, size).ok_or(ProbeError::Corrupt)?
        };
        if end > buf.len() || end > t_end {
            return Err(overrun_error(end, buf.len()));
        }
        if id == 0xAE {
            out.push(parse_mkv_track_entry(buf, pos, end)?);
        }
        pos = end;
    }
    Ok(out)
}

/// Find the first child element with `target_id` between [start,end). Returns its payload range.
fn find_ebml_child(
    buf: &[u8],
    start: usize,
    end: usize,
    target_id: u32,
) -> Result<Option<(usize, usize)>, ProbeError> {
    let mut pos = start;
    while pos < end {
        let id = read_ebml_id(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        let size = read_ebml_size(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        let payload_start = pos;
        let payload_end = if size == u64::MAX {
            end // unknown/streaming size → spans to the end of the search region
        } else {
            ebml_end(payload_start, size).ok_or(ProbeError::Corrupt)?
        };
        if id == target_id {
            // A matched container legitimately extends past the fetched window — the top-level MKV
            // Segment declares the size of the WHOLE (multi-GB) file, far beyond our 4 MB front
            // read. That is NOT a truncated read: return it capped to what we have and let the
            // caller scan its children within the buffer. (Checking this before the overrun guard
            // below is the fix for the probe deferring on every large MKV.)
            return Ok(Some((payload_start, payload_end.min(buf.len()))));
        }
        // Not the target: reaching the next sibling means skipping this whole element. If it runs
        // past the fetched buffer we genuinely can't — a truncated read (defer) or a broken size.
        if payload_end > end {
            return Err(overrun_error(payload_end, buf.len()));
        }
        pos = payload_end;
    }
    Ok(None)
}

fn parse_mkv_track_entry(buf: &[u8], start: usize, end: usize) -> Result<Track, ProbeError> {
    let mut kind = TrackKind::Other;
    let mut language: Option<String> = None;
    // Whether a Language element was present at all — distinguishes a genuinely-absent element (which
    // Matroska's spec defaults to "eng") from a present-but-`und`/empty one (inconclusive, NOT eng).
    let mut saw_language = false;
    let mut pos = start;
    while pos < end {
        let id = read_ebml_id(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        let size = read_ebml_size(buf, &mut pos).ok_or(ProbeError::Corrupt)?;
        if size == u64::MAX {
            return Err(ProbeError::Corrupt);
        }
        let p_end = ebml_end(pos, size).ok_or(ProbeError::Corrupt)?;
        if p_end > end {
            return Err(overrun_error(p_end, buf.len()));
        }
        match id {
            0x83 => {
                kind = match buf.get(pos).copied() {
                    Some(2) => TrackKind::Audio,
                    Some(17) => TrackKind::Subtitle,
                    Some(1) => TrackKind::Video,
                    _ => TrackKind::Other,
                };
            }
            0x22B59C => {
                saw_language = true;
                language = std::str::from_utf8(&buf[pos..p_end])
                    .ok()
                    .map(|s| s.trim().to_string())
                    // "und" (ISO-639 *undetermined*) and empty are INCONCLUSIVE, not a positive
                    // tag — treat them like an absent code (`None`) so `check_lang` doesn't count
                    // them as a wrong-language match (mirrors the MP4 `parse_mdhd_language` path).
                    // They are deliberately NOT defaulted to "eng" below — only a genuinely-absent
                    // element is (the Matroska spec default). Case-insensitive on `und` for exact
                    // parity with the always-lowercased MP4 path (Matroska mandates lowercase, so a
                    // stray "UND" is non-spec but must not slip through as a wrong-language tag).
                    .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("und"));
            }
            _ => {}
        }
        pos = p_end;
    }
    Ok(Track {
        kind,
        // Matroska spec: a track with NO Language element is "eng". A present-but-`und`/empty element
        // stays `None` (inconclusive), so it is never miscounted as a wrong-language tag nor wrongly
        // auto-passed as "eng".
        language: if saw_language {
            language
        } else {
            Some("eng".to_string())
        },
    })
}

// --- MP4 (ISO-BMFF) parser ---

/// Read a box header at `pos`: returns (box_type, payload_start, box_end).
fn read_box_header(buf: &[u8], pos: usize) -> Result<([u8; 4], usize, usize), ProbeError> {
    if pos + 8 > buf.len() {
        return Err(ProbeError::TracksNotFound);
    }
    let size32 = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
    let typ = [buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]];
    let (payload_start, box_end) = if size32 == 1 {
        if pos + 16 > buf.len() {
            // The 16-byte 64-bit-largesize header straddles the fetched-buffer boundary: that is
            // an under-fetch (truncated ranged read), not a broken structure — defer + retry
            // (Transient) rather than blacklisting the release as Corrupt.
            return Err(overrun_error(pos + 16, buf.len()));
        }
        let big = u64::from_be_bytes(buf[pos + 8..pos + 16].try_into().unwrap());
        // A crafted 64-bit largesize must not overflow usize (input is untrusted CDN bytes).
        let end = ebml_end(pos, big).ok_or(ProbeError::Corrupt)?;
        (pos + 16, end)
    } else if size32 == 0 {
        (pos + 8, buf.len())
    } else {
        let end = pos
            .checked_add(size32 as usize)
            .ok_or(ProbeError::Corrupt)?;
        (pos + 8, end)
    };
    if box_end < payload_start {
        return Err(ProbeError::Corrupt);
    }
    Ok((typ, payload_start, box_end))
}

/// Walk top-level boxes for `moov`; parse its `trak`s. `TracksNotFound` if no `moov` here.
pub fn parse_mp4_tracks(buf: &[u8]) -> Result<Vec<Track>, ProbeError> {
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let (typ, p_start, b_end) = read_box_header(buf, pos)?;
        if b_end > buf.len() {
            if &typ == b"moov" {
                // moov declared past the fetched bytes → truncated read, not a broken file.
                return Err(ProbeError::Transient);
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
            return Err(overrun_error(b_end, buf.len()));
        }
        if &typ == b"trak" {
            out.push(parse_mp4_trak(buf, p_start, b_end)?);
        }
        pos = b_end;
    }
    Ok(out)
}

fn parse_mp4_trak(buf: &[u8], start: usize, end: usize) -> Result<Track, ProbeError> {
    let mdia = find_mp4_child(buf, start, end, b"mdia")?;
    let (m_start, m_end) = match mdia {
        Some(r) => r,
        None => {
            return Ok(Track {
                kind: TrackKind::Other,
                language: None,
            })
        }
    };
    let mut kind = TrackKind::Other;
    let mut language: Option<String> = None;
    let mut pos = m_start;
    while pos + 8 <= m_end {
        let (typ, p_start, b_end) = read_box_header(buf, pos)?;
        if b_end > m_end {
            return Err(overrun_error(b_end, buf.len()));
        }
        if &typ == b"hdlr" {
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
            return Err(overrun_error(b_end, buf.len()));
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

// --- HTTP orchestration ---

/// Fetch the container header(s) over ranged GETs and extract tracks. `Transient` on any fetch
/// failure (caller re-resolves/retries). Reuses the Range-header pattern from `dav_fs`.
pub async fn probe_tracks(http: &reqwest::Client, cdn_url: &str) -> Result<Vec<Track>, ProbeError> {
    const FRONT: u64 = 4 * 1024 * 1024;
    let front = fetch_range(http, cdn_url, 0, FRONT - 1).await?;
    match detect_container(&front) {
        Some(ContainerKind::Mkv) => parse_mkv_tracks(&front),
        Some(ContainerKind::Mp4) => match parse_mp4_tracks(&front) {
            Err(ProbeError::TracksNotFound) => {
                // moov is likely at the tail (non-faststart MP4). The suffix rarely starts on a
                // box boundary, so this only parses when it happens to; otherwise the probe
                // accepts with a warning. A misaligned tail can make `parse_mp4_tracks` read a
                // bogus box size and return `Corrupt` — that is NOT evidence the release is
                // broken, so map any speculative-parse error down to `TracksNotFound` (accept)
                // rather than blacklisting a valid non-faststart MP4. (A transient *fetch* failure
                // still propagates via `?` so it defers/retries.)
                // TODO(SP1+): scan the tail for the `moov` box signature for reliable handling.
                let tail = fetch_suffix(http, cdn_url, FRONT).await?;
                parse_mp4_tracks(&tail).or(Err(ProbeError::TracksNotFound))
            }
            other => other,
        },
        None => Err(ProbeError::Unsupported),
    }
}

/// Cap on a single probe fetch. We only ever request `FRONT` (4 MB) windows, so allow a little
/// slack above that and refuse anything larger. This is the hard memory guarantee: a CDN that
/// ignores `Range` (replying `200` with the whole multi-GB file) or lies about a `206` length must
/// never drive an unbounded allocation in the probe path.
const MAX_PROBE_FETCH: usize = 8 * 1024 * 1024;

/// Whether a probe fetch's response status is usable. `206 Partial Content` always is. A `200 OK`
/// means the CDN ignored the `Range` header and is streaming the whole object **from byte 0** —
/// usable ONLY for a front (offset-0) read, where the first window is exactly what we want; for a
/// suffix read a `200` returns the wrong bytes (the head, not the tail) so it's rejected. TorBox's
/// CDN does exactly this (replies `200`, ignoring `Range`) — without accepting it the probe defers
/// forever, even though playback works (`dav_fs::fetch_cdn_range` already accepts a `pos == 0` 200).
fn probe_status_ok(status: reqwest::StatusCode, accept_200_from_start: bool) -> bool {
    status == reqwest::StatusCode::PARTIAL_CONTENT
        || (status == reqwest::StatusCode::OK && accept_200_from_start)
}

/// Read up to `want` bytes of a ranged response body for the probe. On a `200` (Range-ignoring CDN
/// streaming the whole multi-GB object) we stop and drop the connection as soon as we have `want`
/// bytes, so we never download more than a window. A status mismatch or a read error → `Transient`
/// (defer + retry; we never blacklist a release merely because a probe window couldn't be fetched).
/// The `MAX_PROBE_FETCH` cap is a defensive backstop on total allocation: with the current callers
/// `want` (4 MB) is below it so the `>= want` early-return fires first, but it bounds the buffer
/// should a future caller request a window larger than the cap.
async fn read_body(
    resp: reqwest::Response,
    want: usize,
    accept_200_from_start: bool,
) -> Result<Vec<u8>, ProbeError> {
    if !probe_status_ok(resp.status(), accept_200_from_start) {
        debug!(
            "probe: unusable response status {} (accept_200_from_start={})",
            resp.status(),
            accept_200_from_start
        );
        return Err(ProbeError::Transient);
    }
    let mut resp = resp;
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        debug!("probe: body read error: {}", e.without_url());
        ProbeError::Transient
    })? {
        buf.extend_from_slice(&chunk);
        if buf.len() >= want {
            buf.truncate(want);
            return Ok(buf); // got the window — drop the connection, don't drain the whole object
        }
        if buf.len() > MAX_PROBE_FETCH {
            // Defensive backstop only — unreachable while `want <= MAX_PROBE_FETCH` (the `>= want`
            // check above returns first). Guards against a future caller requesting a larger window.
            return Err(ProbeError::Transient);
        }
    }
    Ok(buf) // 206 whose range was shorter than `want` (e.g. a small file) — return what we got
}

async fn fetch_range(
    http: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
) -> Result<Vec<u8>, ProbeError> {
    let resp = http
        .get(url)
        .header("Range", format!("bytes={}-{}", start, end))
        .send()
        .await
        .map_err(|e| {
            debug!(
                "probe: fetch send error (range {start}-{end}): {}",
                e.without_url()
            );
            ProbeError::Transient
        })?;
    let want = (end - start + 1) as usize;
    // Offset-0 (front) read: a Range-ignoring 200 streams from byte 0, which is exactly our window.
    read_body(resp, want, start == 0).await
}

async fn fetch_suffix(http: &reqwest::Client, url: &str, len: u64) -> Result<Vec<u8>, ProbeError> {
    let resp = http
        .get(url)
        .header("Range", format!("bytes=-{}", len))
        .send()
        .await
        .map_err(|e| {
            debug!("probe: suffix fetch send error: {}", e.without_url());
            ProbeError::Transient
        })?;
    // A suffix read needs the TAIL; a Range-ignoring 200 gives the head → reject (206 only).
    read_body(resp, len as usize, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ebml_end_adds_and_guards_overflow() {
        assert_eq!(ebml_end(10, 5), Some(15));
        assert_eq!(ebml_end(0, 0), Some(0));
        // base + size overflowing usize → None (rejected as Corrupt by callers).
        assert_eq!(ebml_end(usize::MAX, 1), None);
        assert_eq!(ebml_end(usize::MAX - 2, 5), None);
    }

    #[test]
    fn probe_status_ok_accepts_206_always_and_200_only_from_start() {
        use reqwest::StatusCode;
        // 206 Partial Content is always usable (well-behaved CDN).
        assert!(probe_status_ok(StatusCode::PARTIAL_CONTENT, true));
        assert!(probe_status_ok(StatusCode::PARTIAL_CONTENT, false));
        // 200 OK = CDN ignored Range, streaming from byte 0 (TorBox): usable for a front read only.
        assert!(probe_status_ok(StatusCode::OK, true));
        assert!(!probe_status_ok(StatusCode::OK, false)); // suffix read → 200 is the wrong bytes
                                                          // Anything else (expired URL, error) → not usable → Transient.
        assert!(!probe_status_ok(StatusCode::NOT_FOUND, true));
        assert!(!probe_status_ok(StatusCode::FORBIDDEN, true));
        assert!(!probe_status_ok(StatusCode::INTERNAL_SERVER_ERROR, true));
    }

    fn vint(size: u64) -> Vec<u8> {
        for len in 1u32..=8 {
            let max = (1u64 << (7 * len)) - 1;
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
    fn ebml_elem_sized(id: u32, declared_size: u64, payload: &[u8]) -> Vec<u8> {
        // Like `ebml_elem` but stamps an explicit (here: deliberately oversized) data size rather
        // than the payload's real length — models a top-level Segment whose size spans the whole
        // multi-GB file, far beyond the bytes actually fetched into the front window.
        let mut out = id_bytes(id);
        out.extend(vint(declared_size));
        out.extend_from_slice(payload);
        out
    }
    fn mkv_with(audio_lang: &str, sub_lang: Option<&str>) -> Vec<u8> {
        let mut audio = Vec::new();
        audio.extend(ebml_elem(0x83, &[2]));
        audio.extend(ebml_elem(0x22B59C, audio_lang.as_bytes()));
        let mut tracks = ebml_elem(0xAE, &audio);
        if let Some(sl) = sub_lang {
            let mut sub = Vec::new();
            sub.extend(ebml_elem(0x83, &[17]));
            sub.extend(ebml_elem(0x22B59C, sl.as_bytes()));
            tracks.extend(ebml_elem(0xAE, &sub));
        }
        let tracks_elem = ebml_elem(0x1654AE6B, &tracks);
        let segment = ebml_elem(0x18538067, &tracks_elem);
        // Minimal valid EBML header element (id 0x1A45DFA3 + size 0), then the Segment.
        let mut out = ebml_elem(0x1A45DFA3, &[]);
        out.extend(segment);
        out
    }
    fn mp4_box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut out = size.to_be_bytes().to_vec();
        out.extend_from_slice(typ);
        out.extend_from_slice(payload);
        out
    }
    fn mdhd(lang_packed: u16) -> Vec<u8> {
        let mut p = vec![0u8; 4 + 16];
        p.extend_from_slice(&lang_packed.to_be_bytes());
        p.extend_from_slice(&[0, 0]);
        mp4_box(b"mdhd", &p)
    }
    fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
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
        let mut out = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        out.extend(moov_box);
        out
    }
    fn packed(lang: &str) -> u16 {
        let b = lang.as_bytes();
        (((b[0] - 0x60) as u16) << 10) | (((b[1] - 0x60) as u16) << 5) | ((b[2] - 0x60) as u16)
    }

    #[test]
    fn mkv_audio_and_subtitle_languages() {
        let bytes = mkv_with("eng", Some("fre"));
        let tracks = parse_mkv_tracks(&bytes).expect("parse");
        assert!(tracks
            .iter()
            .any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("eng")));
        assert!(tracks
            .iter()
            .any(|t| t.kind == TrackKind::Subtitle && t.language.as_deref() == Some("fre")));
    }
    #[test]
    fn mkv_known_size_segment_larger_than_buffer_parses() {
        // Regression: a real MKV's top-level Segment declares the size of the WHOLE (multi-GB)
        // file, which dwarfs our 4 MB front read — yet the Tracks header still sits inside the
        // fetched window. The parser must NOT mistake the oversized Segment for a truncated read
        // (`Transient`); it must cap the Segment to the buffer and find Tracks within it. Before
        // the fix this deferred essentially every large MKV (the overrun guard fired on the
        // Segment itself, before the id match).
        let mut audio = Vec::new();
        audio.extend(ebml_elem(0x83, &[2]));
        audio.extend(ebml_elem(0x22B59C, b"eng"));
        let tracks = ebml_elem(0xAE, &audio);
        let tracks_elem = ebml_elem(0x1654AE6B, &tracks);
        // Segment claims ~8 GB — orders of magnitude beyond the bytes actually present.
        let segment = ebml_elem_sized(0x18538067, 8_000_000_000, &tracks_elem);
        let mut bytes = ebml_elem(0x1A45DFA3, &[]);
        bytes.extend(segment);

        let parsed = parse_mkv_tracks(&bytes).expect("oversized-Segment MKV must parse, not defer");
        assert!(
            parsed
                .iter()
                .any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("eng")),
            "expected the in-window audio track to be parsed from the oversized-Segment MKV"
        );
    }
    #[test]
    fn mkv_truncated_is_transient() {
        // A truncated ranged read (an element declared past the fetched bytes) is an under-fetch
        // to defer, not a broken file to blacklist.
        let mut bytes = mkv_with("eng", None);
        bytes.truncate(bytes.len() - 3);
        assert!(matches!(
            parse_mkv_tracks(&bytes),
            Err(ProbeError::Transient)
        ));
    }
    #[test]
    fn mp4_truncated_moov_is_transient() {
        let mut bytes = mp4_with(&[(b"soun", packed("eng"))]);
        bytes.truncate(bytes.len() - 4);
        assert!(matches!(
            parse_mp4_tracks(&bytes),
            Err(ProbeError::Transient)
        ));
    }
    #[test]
    fn overrun_classifies_under_fetch_vs_malformed() {
        assert_eq!(overrun_error(120, 100), ProbeError::Transient); // past the fetched buffer
        assert_eq!(overrun_error(80, 100), ProbeError::Corrupt); // within buffer, past a parent
        assert_eq!(overrun_error(100, 100), ProbeError::Corrupt); // exactly at end, not past
    }
    #[test]
    fn read_box_header_truncated_largesize_is_transient() {
        // size32 == 1 signals a 64-bit largesize, but only 12 of the 16 header bytes were fetched
        // — an under-fetch (truncated ranged read), so this must defer (Transient), not blacklist.
        let mut buf = vec![0u8; 12];
        buf[0..4].copy_from_slice(&1u32.to_be_bytes());
        buf[4..8].copy_from_slice(b"moov");
        assert_eq!(read_box_header(&buf, 0), Err(ProbeError::Transient));
    }
    #[test]
    fn mp4_tracks_front_moov() {
        let bytes = mp4_with(&[(b"soun", packed("eng")), (b"subt", packed("ger"))]);
        let tracks = parse_mp4_tracks(&bytes).expect("parse");
        assert!(tracks
            .iter()
            .any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("eng")));
        assert!(tracks
            .iter()
            .any(|t| t.kind == TrackKind::Subtitle && t.language.as_deref() == Some("ger")));
    }
    #[test]
    fn mp4_no_moov_is_tracks_not_found() {
        let mut bytes = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        bytes.extend(mp4_box(b"mdat", &[0u8; 16]));
        assert!(matches!(
            parse_mp4_tracks(&bytes),
            Err(ProbeError::TracksNotFound)
        ));
    }
    #[test]
    fn mp4_bad_box_size_is_corrupt() {
        let mut bytes = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(b"moov");
        assert!(matches!(parse_mp4_tracks(&bytes), Err(ProbeError::Corrupt)));
    }
    #[test]
    fn mp4_largesize_overflow_is_corrupt() {
        // 64-bit largesize (size==1) set to u64::MAX must be rejected, never panic/overflow.
        let mut bytes = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(b"moov");
        bytes.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(matches!(parse_mp4_tracks(&bytes), Err(ProbeError::Corrupt)));
    }
    #[test]
    fn detect_container_by_magic() {
        assert_eq!(
            detect_container(&mkv_with("eng", None)),
            Some(ContainerKind::Mkv)
        );
        assert_eq!(
            detect_container(&mp4_with(&[(b"soun", packed("eng"))])),
            Some(ContainerKind::Mp4)
        );
        assert_eq!(detect_container(b"RIFF\0\0\0\0AVI LIST"), None);
    }
    #[test]
    fn verify_audio_original_and_subtitle_rules() {
        let tracks = vec![
            Track {
                kind: TrackKind::Audio,
                language: Some("jpn".into()),
            },
            Track {
                kind: TrackKind::Subtitle,
                language: Some("eng".into()),
            },
        ];
        let req = LangReq {
            audio: AudioReq::Original,
            subtitle: SubReq::Lang("eng".into()),
            original_language: Some("jpn".into()),
        };
        assert_eq!(verify(&tracks, &req), Verify::Pass);
        let req2 = LangReq {
            audio: AudioReq::Lang("eng".into()),
            subtitle: SubReq::None,
            original_language: None,
        };
        assert_eq!(verify(&tracks, &req2), Verify::FailAudio);
        let req3 = LangReq {
            audio: AudioReq::Original,
            subtitle: SubReq::Lang("ger".into()),
            original_language: Some("jpn".into()),
        };
        assert_eq!(verify(&tracks, &req3), Verify::FailSubtitle);
        assert_eq!(verify(&[], &req), Verify::Inconclusive);
    }

    #[test]
    fn verify_untagged_audio_is_not_rejected() {
        // Audio with no determinable language (e.g. MP4 with und/missing mdhd) is inconclusive,
        // not a BadAudio rejection — a correct-but-untagged release must not be dropped.
        let tracks = vec![
            Track {
                kind: TrackKind::Video,
                language: None,
            },
            Track {
                kind: TrackKind::Audio,
                language: None,
            },
        ];
        let req = LangReq {
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            original_language: Some("eng".into()),
        };
        assert_eq!(verify(&tracks, &req), Verify::Pass);
    }

    #[test]
    fn mkv_und_and_empty_language_are_inconclusive_not_a_wrong_match() {
        // A present `und` (ISO-639 undetermined) or empty Language element must resolve to None
        // (inconclusive — like an MP4 untagged track), NOT a positively-tagged wrong language and
        // NOT the "eng" default. Otherwise a (very common) und-tagged MKV release is wrongly
        // rejected as wrong-language, asymmetric with the MP4 path.
        let bytes = mkv_with("und", Some(""));
        let tracks = parse_mkv_tracks(&bytes).expect("parse");
        let audio = tracks
            .iter()
            .find(|t| t.kind == TrackKind::Audio)
            .expect("audio");
        assert_eq!(
            audio.language, None,
            "und audio must be inconclusive (None), not Some(\"und\") or \"eng\""
        );
        let sub = tracks
            .iter()
            .find(|t| t.kind == TrackKind::Subtitle)
            .expect("sub");
        assert_eq!(
            sub.language, None,
            "empty subtitle language must be inconclusive (None)"
        );
        // An eng-audio requirement must NOT reject a release whose only audio is `und`.
        let req = LangReq {
            audio: AudioReq::Lang("eng".into()),
            subtitle: SubReq::None,
            original_language: None,
        };
        assert_ne!(
            verify(&tracks, &req),
            Verify::FailAudio,
            "und audio must be inconclusive, never a wrong-language rejection"
        );
    }

    #[test]
    fn mkv_absent_language_element_defaults_to_eng() {
        // Matroska spec: a track that OMITS the Language element is "eng" (distinct from a present
        // `und`, which is inconclusive — see the test above).
        let mut audio = Vec::new();
        audio.extend(ebml_elem(0x83, &[2])); // audio track type, NO Language element
        let tracks_inner = ebml_elem(0xAE, &audio);
        let tracks_elem = ebml_elem(0x1654AE6B, &tracks_inner);
        let segment = ebml_elem(0x18538067, &tracks_elem);
        let mut bytes = ebml_elem(0x1A45DFA3, &[]);
        bytes.extend(segment);
        let tracks = parse_mkv_tracks(&bytes).expect("parse");
        let audio = tracks
            .iter()
            .find(|t| t.kind == TrackKind::Audio)
            .expect("audio");
        assert_eq!(
            audio.language.as_deref(),
            Some("eng"),
            "an absent Language element defaults to eng (Matroska spec)"
        );
    }

    #[test]
    fn verify_tagged_wrong_audio_still_fails() {
        // A track positively tagged with a non-matching language is still rejected (a real dub).
        let tracks = vec![Track {
            kind: TrackKind::Audio,
            language: Some("ita".into()),
        }];
        let req = LangReq {
            audio: AudioReq::Lang("eng".into()),
            subtitle: SubReq::None,
            original_language: None,
        };
        assert_eq!(verify(&tracks, &req), Verify::FailAudio);
    }

    #[test]
    fn verify_wrong_tagged_plus_untagged_audio_accepts() {
        // Deliberate softening (pinned): a non-matching tagged track alongside an untagged one is
        // accepted — the untagged track could be the wanted language. Only an all-tagged,
        // all-wrong set is rejected.
        let tracks = vec![
            Track {
                kind: TrackKind::Audio,
                language: Some("ita".into()),
            },
            Track {
                kind: TrackKind::Audio,
                language: None,
            },
        ];
        let req = LangReq {
            audio: AudioReq::Lang("eng".into()),
            subtitle: SubReq::None,
            original_language: None,
        };
        assert_eq!(verify(&tracks, &req), Verify::Pass);
    }

    #[test]
    fn verify_subtitle_untagged_inconclusive_but_wrong_tagged_fails() {
        let req = LangReq {
            audio: AudioReq::Lang("eng".into()),
            subtitle: SubReq::Lang("eng".into()),
            original_language: None,
        };
        // Untagged subtitle present → could be the wanted one → inconclusive (accept).
        let untagged_sub = vec![
            Track {
                kind: TrackKind::Audio,
                language: Some("eng".into()),
            },
            Track {
                kind: TrackKind::Subtitle,
                language: None,
            },
        ];
        assert_eq!(verify(&untagged_sub, &req), Verify::Pass);
        // Only a differently-tagged subtitle → the wanted subtitle is positively absent → fail.
        let wrong_sub = vec![
            Track {
                kind: TrackKind::Audio,
                language: Some("eng".into()),
            },
            Track {
                kind: TrackKind::Subtitle,
                language: Some("fre".into()),
            },
        ];
        assert_eq!(verify(&wrong_sub, &req), Verify::FailSubtitle);
    }
    #[test]
    fn iso_639_1_to_2_mapping() {
        assert_eq!(to_iso639_2("en"), "eng");
        assert_eq!(to_iso639_2("eng"), "eng");
        assert_eq!(to_iso639_2("ja"), "jpn");
    }

    #[test]
    fn iso_639_1_map_covers_common_originals() {
        // The default AudioReq::Original compares TMDB's 2-letter original_language against a
        // 3-letter track tag, so every realistic original language must canonicalise to its /B
        // 3-letter form (regression guard for previously-missing originals).
        assert_eq!(to_iso639_2("tr"), "tur"); // Turkish
        assert_eq!(to_iso639_2("ar"), "ara"); // Arabic
        assert_eq!(to_iso639_2("th"), "tha"); // Thai
        assert_eq!(to_iso639_2("he"), "heb"); // Hebrew
        assert_eq!(to_iso639_2("uk"), "ukr"); // Ukrainian
        assert_eq!(to_iso639_2("hu"), "hun"); // Hungarian
        assert_eq!(to_iso639_2("id"), "ind"); // Indonesian
        assert_eq!(to_iso639_2("vi"), "vie"); // Vietnamese
        assert_eq!(to_iso639_2("ta"), "tam"); // Tamil
        assert_eq!(to_iso639_2("te"), "tel"); // Telugu
        assert_eq!(to_iso639_2("uz"), "uzb"); // Uzbek
        assert_eq!(to_iso639_2("hr"), "hrv"); // Croatian
        assert_eq!(to_iso639_2("sr"), "srp"); // Serbian
        assert_eq!(to_iso639_2("bg"), "bul"); // Bulgarian
                                              // Unknown / non-language tokens still pass through unchanged.
        assert_eq!(to_iso639_2("zz"), "zz");
    }

    #[test]
    fn iso_639_b_t_and_1_all_unify() {
        // For every language whose /B and /T codes differ, the 639-1, /B, and /T forms must all
        // canonicalise to the same /B value (cross-checks the 639-1 map against the /T map, so a
        // typo in either table is caught here).
        let triples = [
            ("sq", "alb", "sqi"),
            ("hy", "arm", "hye"),
            ("eu", "baq", "eus"),
            ("my", "bur", "mya"),
            ("zh", "chi", "zho"),
            ("cs", "cze", "ces"),
            ("nl", "dut", "nld"),
            ("fr", "fre", "fra"),
            ("ka", "geo", "kat"),
            ("de", "ger", "deu"),
            ("el", "gre", "ell"),
            ("is", "ice", "isl"),
            ("mk", "mac", "mkd"),
            ("mi", "mao", "mri"),
            ("ms", "may", "msa"),
            ("fa", "per", "fas"),
            ("ro", "rum", "ron"),
            ("sk", "slo", "slk"),
            ("bo", "tib", "bod"),
            ("cy", "wel", "cym"),
        ];
        for (iso1, b, t) in triples {
            assert_eq!(to_iso639_2(iso1), b, "639-1 {iso1} → {b}");
            assert_eq!(to_iso639_2(t), b, "/T {t} → {b}");
            assert!(lang_eq(iso1, t), "{iso1} should equal {t}");
            assert!(lang_eq(b, t), "{b} should equal {t}");
        }
    }

    #[test]
    fn iso_639_2t_canonicalises_to_2b() {
        // /T (terminological) codes unify with their /B (bibliographic) equivalents.
        assert_eq!(to_iso639_2("deu"), "ger");
        assert_eq!(to_iso639_2("fra"), "fre");
        assert_eq!(to_iso639_2("zho"), "chi");
        assert_eq!(to_iso639_2("nld"), "dut");
        assert_eq!(to_iso639_2("ces"), "cze");
        // /B and B==T codes pass through unchanged.
        assert_eq!(to_iso639_2("ger"), "ger");
        assert_eq!(to_iso639_2("jpn"), "jpn");
        assert_eq!(to_iso639_2("eng"), "eng");
        // 639-1 want and a /T-tagged track now compare equal across all three forms.
        assert!(lang_eq("de", "deu"));
        assert!(lang_eq("deu", "ger"));
        assert!(lang_eq("de", "ger"));
    }

    #[test]
    fn verify_t_tagged_audio_matches_b_or_iso1_want() {
        // A German-original title (TMDB original_language = "de") with a single audio track tagged
        // ISO 639-2/T "deu" (the form ffmpeg/MP4 muxers emit) must PASS, not be FailAudio-blacklisted.
        let tracks = vec![Track {
            kind: TrackKind::Audio,
            language: Some("deu".into()),
        }];
        let req = LangReq {
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            original_language: Some("de".into()),
        };
        assert_eq!(verify(&tracks, &req), Verify::Pass);
    }
}
