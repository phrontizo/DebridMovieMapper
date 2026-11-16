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

/// The resolved language requirement (from `QualityPrefs` + the title's TMDB original and spoken
/// languages).
#[derive(Debug, Clone)]
pub struct LangReq {
    pub audio: AudioReq,
    pub subtitle: SubReq,
    pub original_language: Option<String>,
    /// The title's TMDB spoken languages (empty when unknown / never fetched).
    pub spoken_languages: Vec<String>,
}

/// ISO 639-2/T (terminological) → /B (bibliographic) for the 20 languages whose B and T codes
/// differ. The rest of the pipeline (`ISO_639_1`, TMDB `original_language`) speaks /B; muxers like
/// ffmpeg emit /T (e.g. `deu`/`fra`/`zho`/`nld`), so without this a correct German/French/… release
/// is wrongly blacklisted by the probe. B==T codes (jpn, rus, spa, …) need no entry.
const ISO_639_2_T_TO_B: &[(&str, &str)] = &[
    ("sqi", "alb"),
    ("hye", "arm"),
    ("eus", "baq"),
    ("mya", "bur"),
    ("zho", "chi"),
    ("ces", "cze"),
    ("nld", "dut"),
    ("fra", "fre"),
    ("kat", "geo"),
    ("deu", "ger"),
    ("ell", "gre"),
    ("isl", "ice"),
    ("mkd", "mac"),
    ("mri", "mao"),
    ("msa", "may"),
    ("fas", "per"),
    ("ron", "rum"),
    ("slk", "slo"),
    ("bod", "tib"),
    ("cym", "wel"),
];

/// The complete ISO 639-1 → 639-2/B table. For the 20 languages whose /B and /T differ, this uses
/// the /B form (the /T 3-letter form is folded to /B by `ISO_639_2_T_TO_B`), so all three
/// representations of a language compare equal.
const ISO_639_1: &[(&str, &str)] = &[
    ("aa", "aar"),
    ("ab", "abk"),
    ("ae", "ave"),
    ("af", "afr"),
    ("ak", "aka"),
    ("am", "amh"),
    ("an", "arg"),
    ("ar", "ara"),
    ("as", "asm"),
    ("av", "ava"),
    ("ay", "aym"),
    ("az", "aze"),
    ("ba", "bak"),
    ("be", "bel"),
    ("bg", "bul"),
    ("bh", "bih"),
    ("bi", "bis"),
    ("bm", "bam"),
    ("bn", "ben"),
    ("bo", "tib"),
    ("br", "bre"),
    ("bs", "bos"),
    ("ca", "cat"),
    ("ce", "che"),
    ("ch", "cha"),
    ("co", "cos"),
    ("cr", "cre"),
    ("cs", "cze"),
    ("cu", "chu"),
    ("cv", "chv"),
    ("cy", "wel"),
    ("da", "dan"),
    ("de", "ger"),
    ("dv", "div"),
    ("dz", "dzo"),
    ("ee", "ewe"),
    ("el", "gre"),
    ("en", "eng"),
    ("eo", "epo"),
    ("es", "spa"),
    ("et", "est"),
    ("eu", "baq"),
    ("fa", "per"),
    ("ff", "ful"),
    ("fi", "fin"),
    ("fj", "fij"),
    ("fo", "fao"),
    ("fr", "fre"),
    ("fy", "fry"),
    ("ga", "gle"),
    ("gd", "gla"),
    ("gl", "glg"),
    ("gn", "grn"),
    ("gu", "guj"),
    ("gv", "glv"),
    ("ha", "hau"),
    ("he", "heb"),
    ("hi", "hin"),
    ("ho", "hmo"),
    ("hr", "hrv"),
    ("ht", "hat"),
    ("hu", "hun"),
    ("hy", "arm"),
    ("hz", "her"),
    ("ia", "ina"),
    ("id", "ind"),
    ("ie", "ile"),
    ("ig", "ibo"),
    ("ii", "iii"),
    ("ik", "ipk"),
    ("io", "ido"),
    ("is", "ice"),
    ("it", "ita"),
    ("iu", "iku"),
    ("ja", "jpn"),
    ("jv", "jav"),
    ("ka", "geo"),
    ("kg", "kon"),
    ("ki", "kik"),
    ("kj", "kua"),
    ("kk", "kaz"),
    ("kl", "kal"),
    ("km", "khm"),
    ("kn", "kan"),
    ("ko", "kor"),
    ("kr", "kau"),
    ("ks", "kas"),
    ("ku", "kur"),
    ("kv", "kom"),
    ("kw", "cor"),
    ("ky", "kir"),
    ("la", "lat"),
    ("lb", "ltz"),
    ("lg", "lug"),
    ("li", "lim"),
    ("ln", "lin"),
    ("lo", "lao"),
    ("lt", "lit"),
    ("lu", "lub"),
    ("lv", "lav"),
    ("mg", "mlg"),
    ("mh", "mah"),
    ("mi", "mao"),
    ("mk", "mac"),
    ("ml", "mal"),
    ("mn", "mon"),
    ("mr", "mar"),
    ("ms", "may"),
    ("mt", "mlt"),
    ("my", "bur"),
    ("na", "nau"),
    ("nb", "nob"),
    ("nd", "nde"),
    ("ne", "nep"),
    ("ng", "ndo"),
    ("nl", "dut"),
    ("nn", "nno"),
    ("no", "nor"),
    ("nr", "nbl"),
    ("nv", "nav"),
    ("ny", "nya"),
    ("oc", "oci"),
    ("oj", "oji"),
    ("om", "orm"),
    ("or", "ori"),
    ("os", "oss"),
    ("pa", "pan"),
    ("pi", "pli"),
    ("pl", "pol"),
    ("ps", "pus"),
    ("pt", "por"),
    ("qu", "que"),
    ("rm", "roh"),
    ("rn", "run"),
    ("ro", "rum"),
    ("ru", "rus"),
    ("rw", "kin"),
    ("sa", "san"),
    ("sc", "srd"),
    ("sd", "snd"),
    ("se", "sme"),
    ("sg", "sag"),
    ("si", "sin"),
    ("sk", "slo"),
    ("sl", "slv"),
    ("sm", "smo"),
    ("sn", "sna"),
    ("so", "som"),
    ("sq", "alb"),
    ("sr", "srp"),
    ("ss", "ssw"),
    ("st", "sot"),
    ("su", "sun"),
    ("sv", "swe"),
    ("sw", "swa"),
    ("ta", "tam"),
    ("te", "tel"),
    ("tg", "tgk"),
    ("th", "tha"),
    ("ti", "tir"),
    ("tk", "tuk"),
    ("tl", "tgl"),
    ("tn", "tsn"),
    ("to", "ton"),
    ("tr", "tur"),
    ("ts", "tso"),
    ("tt", "tat"),
    ("tw", "twi"),
    ("ty", "tah"),
    ("ug", "uig"),
    ("uk", "ukr"),
    ("ur", "urd"),
    ("uz", "uzb"),
    ("ve", "ven"),
    ("vi", "vie"),
    ("vo", "vol"),
    ("wa", "wln"),
    ("wo", "wol"),
    ("xh", "xho"),
    ("yi", "yid"),
    ("yo", "yor"),
    ("za", "zha"),
    ("zh", "chi"),
    ("zu", "zul"),
    // Not ISO 639-1, but used by TMDB's `original_language`/`spoken_languages`: Cantonese
    // (most Hong Kong films), "No Language" (a silent or dialogue-free title: ISO 639-2 "no
    // linguistic content") and the deprecated Serbo-Croatian (the ISO 639-3 macrolanguage;
    // see `MACROLANGUAGES`).
    ("cn", "yue"),
    ("xx", "zxx"),
    ("sh", "hbs"),
];

/// Normalise a language code to ISO 639-2/B (bibliographic). Maps the COMPLETE ISO 639-1 (2-letter)
/// set to /B (`ISO_639_1`) — TMDB's `original_language` is 2-letter and the default
/// `AudioReq::Original` compares it against a 3-letter track tag, so an incomplete map would
/// FailAudio-blacklist correctly-tagged foreign releases — and canonicalises the 20 ISO 639-2/T
/// (terminological) 3-letter codes to their /B equivalents (`ISO_639_2_T_TO_B`) so a /T-tagged
/// track unifies with a /B want. Also maps the non-ISO codes TMDB writes (`cn` Cantonese, `xx` No
/// Language) and the deprecated `sh` (Serbo-Croatian), which has no ISO 639-2 code. A 3-letter code
/// already in /B form, and any code neither table knows, passes through unchanged.
pub fn to_iso639_2(code: &str) -> String {
    let c = code.trim().to_ascii_lowercase();
    let table = if c.len() == 3 {
        ISO_639_2_T_TO_B
    } else {
        ISO_639_1
    };
    match table.iter().find(|(from, _)| *from == c) {
        Some((_, to)) => (*to).to_string(),
        None => c, // already /B, or an unknown code
    }
}

/// Macrolanguages whose tracks may carry a member language's code instead (all as `to_iso639_2`
/// returns them). A Chinese track is tagged `chi`/`zho` or, more precisely, Cantonese `yue` or
/// Mandarin `cmn`; a Serbo-Croatian one `hrv`/`srp`/`bos`/`cnr`; a Norwegian one `nob`/`nno`.
const MACROLANGUAGES: &[(&str, &[&str])] = &[
    ("chi", &["yue", "cmn"]),
    ("hbs", &["hrv", "srp", "bos", "cnr"]),
    ("nor", &["nob", "nno"]),
];

/// Whether `code` (any case, surrounding whitespace ignored) is a language code the probe knows: a
/// 2-letter code in `ISO_639_1` (every ISO 639-1 code, plus TMDB's `cn`/`xx`/`sh`), or a 3-letter
/// code that table maps to, a /T form, a macrolanguage or one of its members (`MACROLANGUAGES`),
/// `mul` (multiple languages) or `und` (undetermined). The 2-letter set is complete, so `false` for
/// one means it is no language. Rare ISO 639-2 codes with no 2-letter form (`haw`, Hawaiian) aren't
/// listed, so for a 3-letter code `false` means only that it is not one of these. `config` uses it
/// to vet `AUDIO_LANGUAGE` / `SUBTITLE_LANGUAGE`.
pub fn is_known_language_code(code: &str) -> bool {
    let c = code.trim().to_ascii_lowercase();
    match c.len() {
        2 => ISO_639_1.iter().any(|(two, _)| *two == c),
        3 => {
            ISO_639_1.iter().any(|(_, b)| *b == c)
                || ISO_639_2_T_TO_B.iter().any(|(t, _)| *t == c)
                || MACROLANGUAGES
                    .iter()
                    .any(|(m, members)| *m == c || members.contains(&c.as_str()))
                || ["mul", "und"].contains(&c.as_str())
        }
        _ => false,
    }
}

/// Whether two language codes name the same language, in any of the forms `to_iso639_2` unifies.
/// A macrolanguage also matches each of its members (a `yue` track satisfies a `zh` want, a
/// `chi`-tagged track a Cantonese one), but two members never match each other: Cantonese is not
/// Mandarin, and Serbian is not Croatian. `pub` so `release::wrong_audio_language` compares exactly
/// as the probe does.
pub fn lang_eq(a: &str, b: &str) -> bool {
    let (a, b) = (to_iso639_2(a), to_iso639_2(b));
    let member_of = |umbrella: &str, member: &str| {
        MACROLANGUAGES
            .iter()
            .any(|(m, members)| *m == umbrella && members.contains(&member))
    };
    a == b || member_of(&a, &b) || member_of(&b, &a)
}

/// Whether `code` names no spoken language: ISO 639-2 `zxx` ("no linguistic content"), which TMDB
/// writes `xx` ("No Language") for a silent or dialogue-free title. Wanting it puts no constraint
/// on the audio, since such a title has no wrong audio language (its music track may be tagged
/// anything, and an MKV track with no Language element reads as `eng`).
pub fn is_no_language(code: &str) -> bool {
    to_iso639_2(code) == "zxx"
}

enum LangCheck {
    Pass,
    Fail,
    Inconclusive,
}

/// Does the requirement "any of the languages in `wants`" hold for tracks of `kind`?
/// - `Pass`: a track is tagged with one of `wants`.
/// - `Inconclusive`: a track of that kind carries no determinable language (it *could* be wanted);
///   we only have positive evidence when a language is actually tagged, so don't reject.
/// - `Fail`: every track of that kind is tagged with a language outside `wants`, or there are none —
///   i.e. every wanted language is positively absent.
fn check_lang(tracks: &[Track], kind: TrackKind, wants: &[String]) -> LangCheck {
    let mut has_untagged = false;
    for t in tracks.iter().filter(|t| t.kind == kind) {
        match &t.language {
            Some(l) if wants.iter().any(|w| lang_eq(l, w)) => return LangCheck::Pass,
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
/// could be the wanted language). Note: MKV tracks that OMIT both the Language and LanguageBCP47
/// elements are pre-resolved to `eng` by the parser (Matroska's spec default); an MKV track tagged
/// `und`/empty (undetermined) or an MP4 untagged track resolves to `language: None` and reaches the
/// inconclusive branch.
pub fn verify(tracks: &[Track], req: &LangReq) -> Verify {
    if tracks.is_empty() {
        return Verify::Inconclusive;
    }
    // The acceptable audio languages: an explicit one, an explicit set, or — for the default
    // `Original` — the title's TMDB original language plus its spoken languages (the same set
    // `QualityPrefs::with_resolved_audio` ranks with). Empty = nothing known → no audio
    // requirement, and so is a no-language want (TMDB `xx`: a dialogue-free title has no wrong
    // audio language).
    let want_audio: Vec<String> = match &req.audio {
        AudioReq::Lang(l) => vec![l.clone()],
        AudioReq::AnyOf(v) => v.clone(),
        AudioReq::Original => crate::config::original_audio_languages(
            req.original_language.as_deref(),
            &req.spoken_languages,
        ),
    };
    if !want_audio.is_empty() && !want_audio.iter().any(|w| is_no_language(w)) {
        if let LangCheck::Fail = check_lang(tracks, TrackKind::Audio, &want_audio) {
            return Verify::FailAudio;
        }
    }
    if let SubReq::Lang(want) = &req.subtitle {
        if let LangCheck::Fail = check_lang(tracks, TrackKind::Subtitle, std::slice::from_ref(want))
        {
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
///
/// Distinguishes a STRUCTURAL fault (an id wider than 4 bytes) → `Corrupt`, from an under-fetched
/// read where the id is split across the fetched-buffer boundary → `Transient` (defer + retry). The
/// previous `Option` return folded both into `None`, so a truncated header was wrongly blacklisted as
/// Corrupt — the same incomplete buffer was classified `Transient` (correct) or `Corrupt` (wrong)
/// depending only on whether the cut landed in a payload or a header. (See `overrun_error`.)
fn read_ebml_id(buf: &[u8], pos: &mut usize) -> Result<u32, ProbeError> {
    let first = *buf.get(*pos).ok_or(ProbeError::Transient)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return Err(ProbeError::Corrupt); // an id wider than 4 bytes is structurally invalid
    }
    if *pos + len > buf.len() {
        return Err(ProbeError::Transient); // the id is split by the under-fetched boundary
    }
    let mut id: u32 = 0;
    for i in 0..len {
        id = (id << 8) | buf[*pos + i] as u32;
    }
    *pos += len;
    Ok(id)
}

/// Read an EBML data size vint (marker stripped). All-ones → `u64::MAX` (unknown size). Like
/// `read_ebml_id`, a leading-zero byte or a >8-byte length is structural (`Corrupt`), while a vint
/// split across the fetched boundary is an under-fetch (`Transient`).
fn read_ebml_size(buf: &[u8], pos: &mut usize) -> Result<u64, ProbeError> {
    let first = *buf.get(*pos).ok_or(ProbeError::Transient)?;
    if first == 0 {
        return Err(ProbeError::Corrupt); // a leading 0 byte = invalid vint length octet
    }
    let len = first.leading_zeros() as usize + 1;
    // `first != 0` (rejected above), so `leading_zeros()` ∈ 0..=7 and `len` ∈ 1..=8. An 8-byte size
    // vint is the EBML maximum, so — unlike `read_ebml_id`'s reachable 4-byte id cap — there is no
    // over-length case to reject here; the invariant is asserted rather than checked at runtime.
    debug_assert!(
        len <= 8,
        "a non-zero octet yields a 1..=8 byte EBML size vint"
    );
    if *pos + len > buf.len() {
        return Err(ProbeError::Transient); // the size vint is split by the under-fetched boundary
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
        Ok(u64::MAX)
    } else {
        Ok(val)
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
    // The Segment is normally capped (it declares the size of the whole file): that is expected,
    // and its children are scanned within the buffer.
    let segment = find_ebml_child(buf, 0, buf.len(), 0x18538067)?;
    let (seg_start, seg_end, _) = match segment {
        Some(r) => r,
        None => return Err(ProbeError::TracksNotFound),
    };
    let tracks = find_ebml_child(buf, seg_start, seg_end, 0x1654AE6B)?;
    let (t_start, t_end, tracks_capped) = match tracks {
        Some(r) => r,
        None => return Err(ProbeError::TracksNotFound),
    };
    // A Tracks element that runs past the fetched buffer is incomplete even when the cut lands
    // exactly on a TrackEntry boundary (the scan below would then end cleanly): the entries after
    // the cut — possibly the wanted-language track — are missing, so a partial list could reject a
    // correct release. Defer instead; `probe_tracks` then fetches Tracks' declared extent
    // (`front_mkv_tracks_extent` reports it).
    if tracks_capped {
        return Err(ProbeError::Transient);
    }
    let mut out = Vec::new();
    let mut pos = t_start;
    while pos < t_end {
        let id = read_ebml_id(buf, &mut pos)?;
        let size = read_ebml_size(buf, &mut pos)?;
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

/// Find the first child element with `target_id` between [start,end). Returns its payload range,
/// capped to the buffer, and whether it WAS capped: a known-size element whose declared payload end
/// lies past the fetched bytes. (An unknown-size element spans to the end of the search region and is
/// not reported as capped.)
fn find_ebml_child(
    buf: &[u8],
    start: usize,
    end: usize,
    target_id: u32,
) -> Result<Option<(usize, usize, bool)>, ProbeError> {
    let mut pos = start;
    while pos < end {
        let id = read_ebml_id(buf, &mut pos)?;
        let size = read_ebml_size(buf, &mut pos)?;
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
            // below is the fix for the probe deferring on every large MKV.) The caller decides
            // whether a capped match is expected (the Segment) or incomplete (Tracks).
            let capped = size != u64::MAX && payload_end > buf.len();
            return Ok(Some((payload_start, payload_end.min(buf.len()), capped)));
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

/// If the MKV front-window parse deferred (`Transient`) because a Segment child declares an extent
/// PAST the fetched buffer — a large `Attachments`/`Void`/cover-art block placed BEFORE `Tracks`, or
/// a `Tracks` element itself larger than the window — return that first overrunning child's declared
/// end (absolute offset). `probe_tracks` uses it to issue ONE larger targeted fetch instead of
/// re-fetching the same 4 MB front forever (which would blacklist a valid release after the verify
/// deadline). Mirrors `front_moov_extent` on the MP4 path. A Segment child whose HEADER (id or size
/// vint) the end of the buffer cuts gets an extent just past the buffer: with a full front read the
/// cut comes from the file's layout and would repeat on every re-probe, so it needs the recovery
/// read as much as a payload overrun does. `None` when there is no Segment (or the buffer ends
/// before the Segment's header), an unknown-size child spans to the end, or a child header is
/// structurally invalid.
fn front_mkv_tracks_extent(buf: &[u8]) -> Option<u64> {
    // Locate the Segment payload start without erroring (a header split at the boundary → None).
    let mut pos = 0usize;
    let seg_payload_start = loop {
        if pos >= buf.len() {
            return None;
        }
        let id = read_ebml_id(buf, &mut pos).ok()?;
        let size = read_ebml_size(buf, &mut pos).ok()?;
        let payload_start = pos;
        if id == 0x18538067 {
            break payload_start; // Segment: scan its children below
        }
        // A non-Segment top-level element (e.g. the EBML header). If it declares an unknown size or
        // overruns the buffer we can't even reach the Segment → let the transient path handle it.
        if size == u64::MAX {
            return None;
        }
        let end = ebml_end(payload_start, size)?;
        if end > buf.len() {
            return None;
        }
        pos = end;
    };
    // Scan the Segment's children for the FIRST whose declared end runs past the fetched buffer.
    let mut cpos = seg_payload_start;
    while cpos < buf.len() {
        let size = match read_ebml_id(buf, &mut cpos).and_then(|_| read_ebml_size(buf, &mut cpos)) {
            Ok(size) => size,
            // The header runs past the buffer: any extent past it (within the cap) gets the
            // recovery read, which covers this header and what follows it.
            Err(ProbeError::Transient) => return Some(buf.len() as u64 + 1),
            Err(_) => return None, // a structurally invalid id or size vint
        };
        let payload_start = cpos;
        if size == u64::MAX {
            return None; // unknown/streaming size → no fixed extent to fetch
        }
        let end = ebml_end(payload_start, size)?;
        if end > buf.len() {
            return Some(end as u64);
        }
        cpos = end;
    }
    None
}

/// An EBML String element's value, trimmed; `None` when it isn't UTF-8. RFC 8794 §13 lets a writer
/// terminate or pad a String with NUL octets (an in-place edit that shortens a value keeps the
/// element's size), and a reader ignores the first NUL and everything after it, so the bytes are cut
/// there BEFORE decoding (garbage after the terminator can't fail the UTF-8 check). Without the cut,
/// a NUL-padded `jpn\0` would read as a 4-character code that matches no wanted language.
fn ebml_string(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).ok().map(str::trim)
}

fn parse_mkv_track_entry(buf: &[u8], start: usize, end: usize) -> Result<Track, ProbeError> {
    let mut kind = TrackKind::Other;
    let mut language: Option<String> = None;
    // Whether a Language element was present at all — distinguishes a genuinely-absent element (which
    // Matroska's spec defaults to "eng") from a present-but-`und`/empty one (inconclusive, NOT eng).
    let mut saw_language = false;
    // The LanguageBCP47 element (Matroska v4): `None` when absent, `Some(None)` when `und`/empty.
    // When present, the legacy Language element MUST be ignored (RFC 9559), so a track written with
    // LanguageBCP47 alone (`ru`) is not read as the `eng` default.
    let mut bcp47: Option<Option<String>> = None;
    let mut pos = start;
    while pos < end {
        let id = read_ebml_id(buf, &mut pos)?;
        let size = read_ebml_size(buf, &mut pos)?;
        if size == u64::MAX {
            return Err(ProbeError::Corrupt);
        }
        let p_end = ebml_end(pos, size).ok_or(ProbeError::Corrupt)?;
        if p_end > end {
            return Err(overrun_error(p_end, buf.len()));
        }
        match id {
            0x83 => {
                // Read the TrackType byte only from THIS element's payload. A spec-violating
                // zero-size TrackType (p_end == pos) would otherwise read the *next* element's
                // first byte and mis-classify the track; guard so it stays `Other`.
                kind = if pos < p_end {
                    match buf.get(pos).copied() {
                        Some(2) => TrackKind::Audio,
                        Some(17) => TrackKind::Subtitle,
                        Some(1) => TrackKind::Video,
                        _ => TrackKind::Other,
                    }
                } else {
                    TrackKind::Other
                };
            }
            0x22B59C => {
                saw_language = true;
                language = ebml_string(&buf[pos..p_end])
                    .map(str::to_string)
                    // "und" (ISO-639 *undetermined*) and empty are INCONCLUSIVE, not a positive
                    // tag — treat them like an absent code (`None`) so `check_lang` doesn't count
                    // them as a wrong-language match (mirrors the MP4 `parse_mdhd_language` path).
                    // They are deliberately NOT defaulted to "eng" below — only a genuinely-absent
                    // element is (the Matroska spec default). Case-insensitive on `und` for exact
                    // parity with the always-lowercased MP4 path (Matroska mandates lowercase, so a
                    // stray "UND" is non-spec but must not slip through as a wrong-language tag).
                    .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("und"));
            }
            0x22B59D => {
                // Only the primary subtag names the language (`pt-BR` → `pt`, `zh-Hant-HK` → `zh`);
                // `und` and empty are inconclusive, exactly as for the legacy element.
                bcp47 = Some(
                    ebml_string(&buf[pos..p_end])
                        .and_then(|s| s.split('-').next())
                        .map(str::to_ascii_lowercase)
                        .filter(|s| !s.is_empty() && s != "und"),
                );
            }
            _ => {}
        }
        pos = p_end;
    }
    Ok(Track {
        kind,
        // LanguageBCP47 wins when present. Otherwise, Matroska spec: a track with NO Language
        // element is "eng". A present-but-`und`/empty element stays `None` (inconclusive), so it is
        // never miscounted as a wrong-language tag nor wrongly auto-passed as "eng".
        language: match bcp47 {
            Some(tag) => tag,
            None if saw_language => language,
            None => Some("eng".to_string()),
        },
    })
}

// --- MP4 (ISO-BMFF) parser ---

/// Read a box header at `pos`: returns (box_type, payload_start, box_end).
fn read_box_header(buf: &[u8], pos: usize) -> Result<([u8; 4], usize, usize), ProbeError> {
    if pos + 8 > buf.len() {
        // The 8-byte box header straddles the fetched-buffer boundary: an under-fetch (truncated
        // ranged read), not a broken structure or "no tracks" → defer + retry (Transient), matching
        // the 64-bit-largesize guard below and this file's truncation→Transient convention. All
        // current callers gate their loops on `pos + 8 <= len`, so this is defensive; if a future
        // caller drops that guard, a truncated read must NOT be mis-accepted as TracksNotFound.
        return Err(ProbeError::Transient);
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

/// If a top-level `moov` box is present at the front but its declared extent runs PAST the fetched
/// buffer (a faststart MP4 whose `moov` is larger than the front window), return that declared end
/// (absolute byte offset). `None` if no such moov is found — no moov here, the moov fits, or the
/// truncation is in an earlier box / the moov header itself (a genuine transient under-fetch). Lets
/// `probe_tracks` issue ONE targeted larger fetch instead of deferring on the same 4 MB front forever.
fn front_moov_extent(buf: &[u8]) -> Option<u64> {
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let (typ, _p_start, b_end) = read_box_header(buf, pos).ok()?;
        if &typ == b"moov" {
            // moov fully present → not the large-moov case (the normal parser handles it).
            return (b_end > buf.len()).then_some(b_end as u64);
        }
        // A pre-moov box truncated at the boundary, or a malformed/zero-advance size → let the normal
        // parser/transient path handle it.
        if b_end > buf.len() || b_end <= pos {
            return None;
        }
        pos = b_end;
    }
    None
}

/// For a non-faststart MP4 (no `moov` in the front window): walk the front's top-level boxes and
/// return the declared absolute end of the FIRST box that overruns the fetched buffer — normally the
/// `mdat`, whose end is exactly where the next top-level box (the trailing `moov`) starts. Lets
/// `probe_tracks` read the `moov` at its exact offset instead of hoping the 4 MB suffix happens to
/// start on a box boundary. `None` if nothing overruns, a header is split/malformed, a box is size 0
/// ("to EOF" — no next box), or the box is `moov` (the large-front-moov case is `front_moov_extent`).
fn front_mp4_next_box_offset(buf: &[u8]) -> Option<u64> {
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        if buf[pos..pos + 4] == [0, 0, 0, 0] {
            return None; // size 0: the box extends to EOF, so there is no next top-level box
        }
        let (typ, _p_start, b_end) = read_box_header(buf, pos).ok()?;
        if &typ == b"moov" || b_end <= pos {
            return None;
        }
        if b_end > buf.len() {
            return Some(b_end as u64);
        }
        pos = b_end;
    }
    None
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
    // A front read shorter than the container magic (8 bytes covers both the MKV EBML signature and
    // the MP4 `ftyp` box) can only be a truncated/empty transient read — no real media file is shorter
    // than its container signature. Defer + retry rather than letting `detect_container` return `None`
    // → `Unsupported`, which `acquire` maps to Accept (a wrong-language release would then be marked
    // Verified with no language check).
    if front.len() < 8 {
        return Err(ProbeError::Transient);
    }
    match detect_container(&front) {
        Some(ContainerKind::Mkv) => match parse_mkv_tracks(&front) {
            Err(ProbeError::Transient) => {
                // A Segment child BEFORE `Tracks` (a large Attachments/Void/cover-art block) — or a
                // `Tracks` element larger than the 4 MB window — declares an extent past the front
                // read, returning `Transient`; so does a Segment child header the window cuts.
                // Without this, re-probing fetches the same 4 MB and defers forever, blacklisting a
                // valid file after the verify deadline. Issue ONE larger fetch (up to
                // MAX_PROBE_FETCH) covering that element + the following Tracks and re-parse. If the
                // overrun is past the cap, accept (can't verify → unknown) rather than churn; no
                // extent to fetch (None: e.g. the front ends before the Segment's header) stays
                // `Transient` (defer). Mirrors the MP4 large-`moov` recovery below.
                match front_mkv_tracks_extent(&front) {
                    Some(end) if end <= MAX_PROBE_FETCH as u64 => {
                        // Fetch the full MAX_PROBE_FETCH window rather than exactly `[0, end)`: `end`
                        // is the extent of the FIRST overrunning Segment child, which for a large
                        // pre-`Tracks` block (Void/Attachments) is that block's end — `Tracks` follows
                        // it, so `[0, end)` would miss `Tracks` and defer again. Over-fetching to the
                        // cap covers both the pre-`Tracks` case and a large `Tracks` element in one
                        // request. (Unlike the MP4 path, whose `moov` extent CONTAINS the tracks.)
                        let bigger =
                            fetch_range(http, cdn_url, 0, MAX_PROBE_FETCH as u64 - 1).await?;
                        // The recovery fetch hits the SAME offset 0 whose front read just returned
                        // valid MKV magic, so a body that came back shorter than that front read is
                        // a transient CDN truncation, NOT a broken release. Guard it: on a
                        // truncated/garbled buffer `parse_mkv_tracks` returns `Corrupt`, which
                        // `acquire` treats as definitive and blacklists the hash for 30 days. Defer +
                        // retry instead. (The MP4 sibling is naturally safe here — a short recovery
                        // buffer yields `TracksNotFound`/accept, never `Corrupt`.)
                        if bigger.len() < front.len() {
                            return Err(ProbeError::Transient);
                        }
                        // A full window that still defers has `Tracks` past the cap (a second large
                        // pre-`Tracks` block, or `Tracks` straddling the 8 MB mark): every re-probe
                        // would repeat these two fetches, so accept as unknown rather than churn. A
                        // shorter body stays `Transient` (an under-fetch, or a file under 8 MB).
                        match parse_mkv_tracks(&bigger) {
                            Err(ProbeError::Transient) if bigger.len() >= MAX_PROBE_FETCH => {
                                Err(ProbeError::TracksNotFound)
                            }
                            r => r,
                        }
                    }
                    Some(_) => Err(ProbeError::TracksNotFound), // overrun > cap → accept, don't churn
                    None => Err(ProbeError::Transient),
                }
            }
            other => other,
        },
        Some(ContainerKind::Mp4) => match parse_mp4_tracks(&front) {
            Err(ProbeError::TracksNotFound) => {
                // No moov in the front: a non-faststart MP4 (`ftyp`, `free`, a huge `mdat`, then the
                // `moov` at EOF). The first front box that overruns the window (the `mdat`) declares
                // exactly where the next top-level box starts, so read `[off, off + MAX_PROBE_FETCH)`
                // there and parse it (a 206 shorter than requested is fine — the moov runs to EOF).
                // Any failure of that targeted read or parse (a moov larger than the cap → Transient,
                // a 416 because the mdat's size lies, garbage → Corrupt) falls through to the suffix
                // attempt below UNCHANGED, so this step can never introduce a defer-forever loop.
                if let Some(off) = front_mp4_next_box_offset(&front) {
                    let end = off.saturating_add(MAX_PROBE_FETCH as u64 - 1);
                    match fetch_range(http, cdn_url, off, end).await {
                        Ok(buf) => match parse_mp4_tracks(&buf) {
                            Ok(tracks) => return Ok(tracks),
                            Err(e) => debug!(
                                "probe: trailing moov at offset {off} unparseable ({e:?}); trying the suffix"
                            ),
                        },
                        Err(e) => debug!(
                            "probe: trailing moov read at offset {off} failed ({e:?}); trying the suffix"
                        ),
                    }
                }
                // Fallback: the moov is likely in the tail. The suffix rarely starts on a box
                // boundary, so this only parses when it happens to; otherwise the probe accepts with
                // a warning. A misaligned tail can make `parse_mp4_tracks` read a bogus box size and
                // return `Corrupt` — that is NOT evidence the release is broken, so map any
                // speculative-parse error down to `TracksNotFound` (accept) rather than blacklisting a
                // valid non-faststart MP4. (A transient *fetch* failure still propagates via `?` so it
                // defers/retries.)
                let tail = fetch_suffix(http, cdn_url, FRONT).await?;
                parse_mp4_tracks(&tail).or(Err(ProbeError::TracksNotFound))
            }
            Err(ProbeError::Transient) => {
                // A faststart `moov` at the FRONT whose declared extent runs past the 4 MB window
                // (common for long 4K titles with large sample tables) returns `Transient`. Without
                // this, re-probing fetches the same 4 MB front and defers forever — churning a valid
                // file after the verify deadline. Issue ONE targeted fetch covering the moov extent
                // (capped at MAX_PROBE_FETCH) and re-parse. If the moov is larger than the cap, accept
                // (can't verify language → unknown) rather than churn; if the truncation isn't a
                // front moov, it's a genuine under-fetch → propagate `Transient` (defer + retry).
                match front_moov_extent(&front) {
                    Some(end) if end <= MAX_PROBE_FETCH as u64 => {
                        let bigger = fetch_range(http, cdn_url, 0, end - 1).await?;
                        parse_mp4_tracks(&bigger)
                    }
                    Some(_) => Err(ProbeError::TracksNotFound), // moov > cap → accept, don't churn
                    None => Err(ProbeError::Transient),
                }
            }
            other => other,
        },
        None => Err(ProbeError::Unsupported),
    }
}

/// Cap on a single probe fetch. Callers request at most this much: the 4 MB (`FRONT`) front and
/// suffix reads, plus the up-to-8 MB recovery reads (the larger MKV `Tracks` / front-`moov` re-fetch
/// and the trailing-`moov` read at its exact offset); anything larger is refused. This is the hard
/// memory guarantee: a CDN that ignores `Range` (replying `200` with the whole multi-GB file) or
/// lies about a `206` length must never drive an unbounded allocation in the probe path.
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
/// The `MAX_PROBE_FETCH` cap is a defensive backstop on total allocation: every current caller
/// requests `want <= MAX_PROBE_FETCH` (4 MB front/suffix reads, up-to-8 MB recovery and
/// trailing-`moov` reads) so the `>= want` early-return fires first, but it bounds the buffer should
/// a future caller request a window larger than the cap.
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
    // Saturating: all current callers pass start <= end (start = 0, or the trailing-moov read's
    // `off..=off + MAX_PROBE_FETCH - 1`, itself saturating), but harden against a future caller so
    // an inverted range can never underflow-panic (debug) / wrap (release).
    let want = end.saturating_sub(start).saturating_add(1) as usize;
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
    fn ebml_elem_unknown_size(id: u32, payload: &[u8]) -> Vec<u8> {
        // Like `ebml_elem` but stamps the EBML "unknown size" data-size vint — the all-ones pattern,
        // which as a length-1 vint is the single byte 0xFF (`read_ebml_size` decodes it to u64::MAX).
        // Models a live/streaming Segment that declares no length; the parser treats its payload as
        // spanning to the end of the search region.
        let mut out = id_bytes(id);
        out.push(0xFF); // length-1 unknown-size vint (all-ones) → u64::MAX
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
    fn mdhd_v1(lang_packed: u16) -> Vec<u8> {
        // version-1 (64-bit) mdhd: the version byte is 1 and creation/modification/duration are
        // 64-bit (timescale stays 32-bit), so the layout before the 2-byte packed language is
        // version(1)+flags(3) + creation(8)+modification(8)+timescale(4)+duration(8) = 32 bytes —
        // the packed language sits at payload offset 32 (vs 20 for version 0).
        let mut p = vec![0u8; 4 + 8 + 8 + 4 + 8];
        p[0] = 1; // version 1 → 64-bit time fields → language at payload offset 32
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
    fn mkv_unknown_size_segment_spans_to_buffer_end() {
        // A Segment declared with the EBML "unknown size" vint (all-ones → u64::MAX) carries no
        // length — common for live/streaming muxes. `find_ebml_child` must treat its payload as
        // spanning to the end of the search region (capped to the buffer) and still locate Tracks
        // within it, rather than mis-parsing the size or deferring. All other fixtures use known
        // sizes, so this is the only exercise of the `size == u64::MAX` child branch.
        let mut audio = Vec::new();
        audio.extend(ebml_elem(0x83, &[2]));
        audio.extend(ebml_elem(0x22B59C, b"eng"));
        let tracks = ebml_elem(0xAE, &audio);
        let tracks_elem = ebml_elem(0x1654AE6B, &tracks);
        let segment = ebml_elem_unknown_size(0x18538067, &tracks_elem);
        let mut bytes = ebml_elem(0x1A45DFA3, &[]);
        bytes.extend(segment);

        let parsed =
            parse_mkv_tracks(&bytes).expect("unknown-size Segment MKV must parse, not defer");
        assert!(
            parsed
                .iter()
                .any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("eng")),
            "expected the audio track to be parsed from the unknown-size Segment MKV"
        );
    }
    #[test]
    fn front_mkv_tracks_extent_reports_a_pre_tracks_overrun() {
        // A large Void (reserved space) placed BEFORE Tracks declares an extent past the front
        // window, so parse_mkv_tracks defers (Transient). front_mkv_tracks_extent must report that
        // element's declared end so probe_tracks can issue ONE larger fetch (mirroring the MP4
        // large-moov recovery) instead of re-fetching the same 4 MB forever and blacklisting a valid
        // file after the verify deadline.
        let void = ebml_elem_sized(0xEC, 5_000_000, &[0u8; 4]); // Void declaring ~5 MB (≤ 8 MB cap)
        let mut audio = Vec::new();
        audio.extend(ebml_elem(0x83, &[2]));
        audio.extend(ebml_elem(0x22B59C, b"eng"));
        let tracks = ebml_elem(0xAE, &audio);
        let mut seg_payload = void;
        seg_payload.extend(ebml_elem(0x1654AE6B, &tracks));
        let segment = ebml_elem_sized(0x18538067, 8_000_000_000, &seg_payload);
        let mut bytes = ebml_elem(0x1A45DFA3, &[]);
        bytes.extend(segment);

        assert!(
            matches!(parse_mkv_tracks(&bytes), Err(ProbeError::Transient)),
            "an overrunning pre-Tracks element defers the front-window parse"
        );
        let extent =
            front_mkv_tracks_extent(&bytes).expect("a pre-Tracks overrun must be reported");
        assert!(
            extent > bytes.len() as u64 && extent <= MAX_PROBE_FETCH as u64,
            "extent {extent} must exceed the buffer {} and fit the fetch cap",
            bytes.len()
        );

        // A fully-in-window layout has no overrun → no recovery extent.
        let small_void = ebml_elem(0xEC, &[0u8; 4]);
        let mut audio2 = Vec::new();
        audio2.extend(ebml_elem(0x83, &[2]));
        audio2.extend(ebml_elem(0x22B59C, b"eng"));
        let mut seg2 = small_void;
        seg2.extend(ebml_elem(0x1654AE6B, &ebml_elem(0xAE, &audio2)));
        let segment2 = ebml_elem_sized(0x18538067, 8_000_000_000, &seg2);
        let mut bytes2 = ebml_elem(0x1A45DFA3, &[]);
        bytes2.extend(segment2);
        assert!(parse_mkv_tracks(&bytes2).is_ok());
        assert_eq!(
            front_mkv_tracks_extent(&bytes2),
            None,
            "no overrun → no recovery extent"
        );
    }

    #[test]
    fn mkv_tracks_cut_at_a_track_entry_boundary_is_transient_not_partial() {
        // A Tracks element straddling the fetched window whose cut lands EXACTLY after a complete
        // TrackEntry: the entries after the cut (here the wanted-language audio) are missing. It
        // must defer (Transient, so probe_tracks' recovery fetch runs), never return the partial
        // list — that would FailAudio and blacklist a correct release.
        let mut first = ebml_elem(0x83, &[2]);
        first.extend(ebml_elem(0x22B59C, b"fre"));
        let first = ebml_elem(0xAE, &first);
        let mut second = ebml_elem(0x83, &[2]);
        second.extend(ebml_elem(0x22B59C, b"eng"));
        let second = ebml_elem(0xAE, &second);
        // Tracks declares both entries; only the first is in the buffer.
        let tracks = ebml_elem_sized(0x1654AE6B, (first.len() + second.len()) as u64, &first);
        let segment = ebml_elem_sized(0x18538067, 8_000_000_000, &tracks);
        let mut bytes = ebml_elem(0x1A45DFA3, &[]);
        bytes.extend(segment);

        assert!(
            matches!(parse_mkv_tracks(&bytes), Err(ProbeError::Transient)),
            "a Tracks element capped at the buffer must defer, not return a partial track list"
        );
        assert_eq!(
            front_mkv_tracks_extent(&bytes),
            Some((bytes.len() + second.len()) as u64),
            "the recovery fetch must be offered the Tracks element's declared end"
        );
    }

    #[test]
    fn front_mkv_tracks_extent_offers_a_recovery_read_for_a_header_cut_by_the_window() {
        // The Segment children before Tracks end a few bytes before the edge of the fetched window,
        // so the window cuts the next child's header (here Tracks' id or its size vint). With a full
        // front read the cut comes from the file's layout and repeats on every re-probe, so it must
        // be offered the recovery read like a payload overrun. `None` would defer the probe until
        // the verify deadline blacklists a valid release.
        let cut_after = |tail: &[u8]| {
            let mut seg = ebml_elem(0xEC, &[0u8; 16]);
            seg.extend_from_slice(tail);
            let mut bytes = ebml_elem(0x1A45DFA3, &[]);
            bytes.extend(ebml_elem_sized(0x18538067, 8_000_000_000, &seg));
            bytes
        };
        for (what, tail) in [
            ("id", &[0x16, 0x54][..]),
            ("size vint", &[0x16, 0x54, 0xAE, 0x6B, 0x10, 0x00][..]),
        ] {
            let bytes = cut_after(tail);
            assert!(
                matches!(parse_mkv_tracks(&bytes), Err(ProbeError::Transient)),
                "a Tracks {what} cut by the window defers the front-window parse"
            );
            let extent = front_mkv_tracks_extent(&bytes);
            assert!(
                matches!(extent, Some(e) if e > bytes.len() as u64 && e <= MAX_PROBE_FETCH as u64),
                "a Tracks {what} cut by the window must be offered the recovery read, got {extent:?}"
            );
        }

        // A size vint with a leading 0 octet is structurally invalid, not an under-fetch: no
        // recovery read.
        let bytes = cut_after(&[0x16, 0x54, 0xAE, 0x6B, 0x00]);
        assert!(matches!(parse_mkv_tracks(&bytes), Err(ProbeError::Corrupt)));
        assert_eq!(front_mkv_tracks_extent(&bytes), None);
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
    fn ebml_header_read_split_by_buffer_boundary_is_transient_not_corrupt() {
        // An element id/size vint split across the under-fetched buffer boundary must classify
        // `Transient` (defer + retry), NOT `Corrupt` (blacklist a good release for 30 days). A
        // genuinely structural fault (id >4 bytes, size leading-0 / >8 bytes) stays `Corrupt`.
        // id: 0x10 → leading_zeros=3 → len=4, but only 1 byte present → split → Transient.
        let mut pos = 0;
        assert!(matches!(
            read_ebml_id(&[0x10], &mut pos),
            Err(ProbeError::Transient)
        ));
        // size: 0x10 → len=4, only 2 bytes present → split → Transient.
        let mut pos = 0;
        assert!(matches!(
            read_ebml_size(&[0x10, 0x00], &mut pos),
            Err(ProbeError::Transient)
        ));
        // pos at/past the end → nothing to read → under-fetch → Transient.
        let mut pos = 0;
        assert!(matches!(
            read_ebml_id(&[], &mut pos),
            Err(ProbeError::Transient)
        ));
        // STRUCTURAL faults remain Corrupt (precedence over the truncation check):
        // id wider than 4 bytes (leading 0 byte → len=9).
        let mut pos = 0;
        assert!(matches!(
            read_ebml_id(&[0x00, 1, 2, 3, 4, 5, 6, 7, 8, 9], &mut pos),
            Err(ProbeError::Corrupt)
        ));
        // size with a leading 0 length octet.
        let mut pos = 0;
        assert!(matches!(
            read_ebml_size(&[0x00], &mut pos),
            Err(ProbeError::Corrupt)
        ));
        // A complete, valid id still parses unchanged.
        let mut pos = 0;
        assert_eq!(read_ebml_id(&[0x83], &mut pos).unwrap(), 0x83);
        assert_eq!(pos, 1);
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
    fn front_moov_extent_reports_declared_end_of_a_truncated_front_moov() {
        // A faststart MP4 whose front `moov` runs past the fetched window must report its full
        // declared end so probe_tracks can issue ONE larger targeted fetch instead of deferring.
        let full = mp4_with(&[(b"soun", packed("eng"))]);
        let full_len = full.len() as u64;
        let mut truncated = full.clone();
        truncated.truncate(truncated.len() - 4); // ftyp intact, moov declared past the buffer
        assert_eq!(front_moov_extent(&truncated), Some(full_len));
        // A fully-present moov is handled by the normal parser → None.
        assert_eq!(front_moov_extent(&full), None);
        // A truncated PRE-moov box (ftyp) is a genuine under-fetch, not the large-moov case → None.
        let mut ftyp_partial = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        ftyp_partial.truncate(ftyp_partial.len() - 2);
        assert_eq!(front_moov_extent(&ftyp_partial), None);
    }
    /// A 32-byte `ftyp` + an 8-byte `free`: the top-level boxes before a non-faststart MP4's `mdat`
    /// (so the `mdat` header sits at absolute offset 40).
    fn ftyp32_free8() -> Vec<u8> {
        let mut out = mp4_box(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");
        assert_eq!(out.len(), 32);
        out.extend(mp4_box(b"free", &[]));
        assert_eq!(out.len(), 40);
        out
    }
    #[test]
    fn front_mp4_next_box_offset_reports_end_of_an_overrunning_mdat() {
        // Non-faststart MP4: the `mdat` declares an extent far past the 4 MB front window, and the
        // `moov` follows it. The declared end of that first overrunning box is exactly where the next
        // top-level box (the trailing `moov`) starts.
        let mut buf = ftyp32_free8();
        buf.extend_from_slice(&1_000_000u32.to_be_bytes());
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&[0u8; 16]);
        assert_eq!(front_mp4_next_box_offset(&buf), Some(40 + 1_000_000));
    }
    #[test]
    fn front_mp4_next_box_offset_handles_64bit_largesize_mdat() {
        // A >4 GiB `mdat` uses size field 1 + a 64-bit largesize (which includes the 16-byte header).
        let mut buf = ftyp32_free8();
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(b"mdat");
        buf.extend_from_slice(&5_000_000_000u64.to_be_bytes());
        buf.extend_from_slice(&[0u8; 16]);
        assert_eq!(front_mp4_next_box_offset(&buf), Some(40 + 5_000_000_000));
    }
    #[test]
    fn front_mp4_next_box_offset_none_for_size_zero_or_no_overrun() {
        // A size-0 box runs "to EOF": there is no next top-level box to target.
        let mut size0 = ftyp32_free8();
        size0.extend_from_slice(&0u32.to_be_bytes());
        size0.extend_from_slice(b"mdat");
        size0.extend_from_slice(&[0u8; 16]);
        assert_eq!(front_mp4_next_box_offset(&size0), None);
        // Complete boxes only (nothing overruns the buffer) → None.
        let mut complete = ftyp32_free8();
        complete.extend(mp4_box(b"mdat", &[0u8; 16]));
        assert_eq!(front_mp4_next_box_offset(&complete), None);
        // An overrunning FRONT `moov` is the large-front-moov case (`front_moov_extent`) → None.
        let mut big_moov = ftyp32_free8();
        big_moov.extend_from_slice(&1_000_000u32.to_be_bytes());
        big_moov.extend_from_slice(b"moov");
        big_moov.extend_from_slice(&[0u8; 16]);
        assert_eq!(front_mp4_next_box_offset(&big_moov), None);
        // A box header split by the buffer boundary carries no usable extent → None.
        let mut split = ftyp32_free8();
        split.extend_from_slice(&[0, 0, 0]);
        assert_eq!(front_mp4_next_box_offset(&split), None);
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
    fn read_box_header_truncated_8byte_header_is_transient() {
        // Fewer than the 8 bytes of a basic box header were fetched: an under-fetch, which must
        // defer (Transient), NOT accept-as-TracksNotFound (which would wrongly pass a truncated
        // read instead of retrying). Mirrors the largesize-truncation classification above.
        let buf = vec![0u8; 4];
        assert_eq!(read_box_header(&buf, 0), Err(ProbeError::Transient));
        // A header that starts mid-buffer and straddles the end is likewise an under-fetch.
        let buf = vec![0u8; 10];
        assert_eq!(read_box_header(&buf, 6), Err(ProbeError::Transient));
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
    fn mp4_mdhd_version1_uses_64bit_language_offset() {
        // A version-1 (64-bit) mdhd puts the packed language at payload offset 32 (vs 20 for
        // version 0), because creation/modification/duration are 64-bit. The default `mdhd` helper
        // only builds version-0 boxes, so this exercises the `version == 1` branch of
        // `parse_mdhd_language` — the language must still be decoded correctly.
        let mut mdia = hdlr(b"soun");
        mdia.extend(mdhd_v1(packed("fre")));
        let mdia_box = mp4_box(b"mdia", &mdia);
        let trak = mp4_box(b"trak", &mdia_box);
        let moov = mp4_box(b"moov", &trak);
        let mut bytes = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        bytes.extend(moov);
        let tracks = parse_mp4_tracks(&bytes).expect("parse");
        assert!(
            tracks
                .iter()
                .any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("fre")),
            "version-1 mdhd language must be read at the 64-bit offset (start + 32)"
        );
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

    #[tokio::test]
    async fn probe_tracks_defers_on_a_truncated_front_read() {
        // A "successful" 206 whose body is shorter than the container magic (a transient origin
        // hiccup on a still-broken/lapsed file) must DEFER (Transient), not be classified Unsupported
        // (which acquire maps to Accept → Verified with no language check).
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let body = b"\x1a\x45"; // 2 bytes — below the 8-byte container-magic minimum
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-1/1000000\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
            }
        });
        let url = format!("http://{}/", addr);
        let result = probe_tracks(&reqwest::Client::new(), &url).await;
        assert!(
            matches!(result, Err(ProbeError::Transient)),
            "a sub-magic front read must defer, not accept as Unsupported; got {result:?}"
        );
    }
    #[tokio::test]
    async fn probe_tracks_mkv_recovery_truncated_refetch_defers_not_corrupt() {
        // The MKV large-/pre-`Tracks` recovery issues a SECOND (larger) fetch to offset 0. The front
        // read already returned valid MKV magic, so a truncated recovery body is a transient CDN
        // hiccup, NOT a broken release — it must DEFER (Transient), never be classified `Corrupt`
        // (a 30-day blacklist of a valid file).
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Front body: valid EBML header + a Segment whose pre-`Tracks` Void declares a ~5 MB extent
        // past the front window, so the front parse defers and `front_mkv_tracks_extent` reports an
        // in-cap recovery end (same shape as `front_mkv_tracks_extent_reports_a_pre_tracks_overrun`).
        let void = ebml_elem_sized(0xEC, 5_000_000, &[0u8; 4]);
        let mut audio = Vec::new();
        audio.extend(ebml_elem(0x83, &[2]));
        audio.extend(ebml_elem(0x22B59C, b"eng"));
        let mut seg_payload = void;
        seg_payload.extend(ebml_elem(0x1654AE6B, &ebml_elem(0xAE, &audio)));
        let segment = ebml_elem_sized(0x18538067, 8_000_000_000, &seg_payload);
        let mut front_body = ebml_elem(0x1A45DFA3, &[]);
        front_body.extend(segment);
        assert!(
            front_body.len() > 8,
            "front body must exceed the container magic"
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // 1st request (front read): serve the overrunning-but-valid front body.
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/9000000000\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    front_body.len() - 1,
                    front_body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&front_body).await;
                let _ = sock.flush().await;
            }
            // 2nd request (recovery read): serve a truncated 4-byte body (a transient origin hiccup),
            // shorter than the front read — the guard must map this to Transient, not Corrupt.
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let body = b"\x1a\x45\xdf\xa3"; // 4 bytes — shorter than the front body
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/9000000000\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.flush().await;
            }
        });
        let url = format!("http://{}/", addr);
        let result = probe_tracks(&reqwest::Client::new(), &url).await;
        assert!(
            matches!(result, Err(ProbeError::Transient)),
            "a truncated MKV recovery re-fetch must defer, not blacklist as Corrupt; got {result:?}"
        );
    }

    #[tokio::test]
    async fn probe_tracks_mkv_recovery_refetch_succeeds_and_returns_tracks() {
        // The HAPPY path of the MKV pre-`Tracks` recovery: the front read defers (a Void declares more
        // than the truncated front body carries), then the SECOND (larger) fetch returns the full body
        // whose Void payload is present, so the re-parse skips it and reads the eng audio track. Guards
        // the recovery orchestration's success case (only the extent computation + the defer case were
        // covered), which a wrong second-fetch window would silently break (defer-forever → blacklist).
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut audio = Vec::new();
        audio.extend(ebml_elem(0x83, &[2])); // TrackType = audio
        audio.extend(ebml_elem(0x22B59C, b"eng")); // Language
        let tracks = ebml_elem(0x1654AE6B, &ebml_elem(0xAE, &audio));

        // Front body: a pre-`Tracks` Void DECLARING 5000 bytes but carrying only 4 — overruns the tiny
        // front buffer → the front parse defers, and front_mkv_tracks_extent reports an in-cap extent.
        let build = |void: Vec<u8>| {
            let mut seg = void;
            seg.extend(tracks.clone());
            let segment = ebml_elem_sized(0x18538067, 8_000_000_000, &seg);
            let mut body = ebml_elem(0x1A45DFA3, &[]);
            body.extend(segment);
            body
        };
        let front_body = build(ebml_elem_sized(0xEC, 5000, &[0u8; 4]));
        // Recovery body: the SAME structure with the Void's full 5000-byte payload present → parseable.
        let recovery_body = build(ebml_elem_sized(0xEC, 5000, &[0u8; 5000]));
        assert!(recovery_body.len() >= front_body.len());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for body in [front_body, recovery_body] {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/9000000000\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len() - 1,
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.flush().await;
                }
            }
        });
        let url = format!("http://{}/", addr);
        let tracks = probe_tracks(&reqwest::Client::new(), &url)
            .await
            .expect("recovery re-fetch must yield tracks");
        assert!(
            tracks
                .iter()
                .any(|t| matches!(t.kind, TrackKind::Audio) && t.language.as_deref() == Some("eng")),
            "the recovered probe must report the eng audio track, got {tracks:?}"
        );
    }

    #[tokio::test]
    async fn probe_tracks_mkv_recovery_still_past_the_cap_accepts_not_defers() {
        // The first overrunning pre-`Tracks` Void ends within the 8 MB recovery window, but a second
        // one runs past it, so `Tracks` is still out of reach after the recovery fetch. Every
        // re-probe would repeat the same two fetches, so this must accept as unknown
        // (`TracksNotFound`) rather than defer (`Transient`) until the verify deadline replaces a
        // possibly valid release.
        let mut audio = ebml_elem(0x83, &[2]);
        audio.extend(ebml_elem(0x22B59C, b"eng"));
        let mut seg = ebml_elem(0xEC, &vec![0u8; 5_000_000]);
        seg.extend(ebml_elem(0xEC, &vec![0u8; 4_000_000]));
        seg.extend(ebml_elem(0x1654AE6B, &ebml_elem(0xAE, &audio)));
        let mut file = ebml_elem(0x1A45DFA3, &[]);
        file.extend(ebml_elem(0x18538067, &seg));
        assert!(file.len() > MAX_PROBE_FETCH);
        let (url, log) = serve_ranges_logged(file).await;
        let result = probe_tracks(&reqwest::Client::new(), &url).await;
        assert!(
            matches!(result, Err(ProbeError::TracksNotFound)),
            "Tracks past the full recovery window must accept as unknown, got {result:?}"
        );
        assert_eq!(
            log.lock().unwrap().as_slice(),
            [
                format!("bytes=0-{}", 4 * 1024 * 1024 - 1),
                format!("bytes=0-{}", MAX_PROBE_FETCH - 1),
            ],
            "the front read, then one recovery read of the full window"
        );
    }

    #[tokio::test]
    async fn probe_tracks_mkv_front_window_cutting_the_tracks_header_recovers() {
        // A pre-Tracks Void sized so the 4 MB front window ends 2 bytes into Tracks' id. Every front
        // read cuts the header at the same place, so the probe must make its recovery read and parse
        // Tracks from it rather than defer on every re-probe until the verify deadline blacklists a
        // valid release.
        const FRONT: usize = 4 * 1024 * 1024;
        let mut audio = ebml_elem(0x83, &[2]);
        audio.extend(ebml_elem(0x22B59C, b"eng"));
        let tracks = ebml_elem(0x1654AE6B, &ebml_elem(0xAE, &audio));
        let build = |void_len: usize| {
            let mut seg = ebml_elem(0xEC, &vec![0u8; void_len]);
            seg.extend(tracks.clone());
            let mut file = ebml_elem(0x1A45DFA3, &[]);
            file.extend(ebml_elem(0x18538067, &seg));
            file
        };
        // Size the Void so Tracks starts 2 bytes before the window edge. Both Void lengths take a
        // 4-byte size vint, so one correction lands it.
        let first = FRONT - 64;
        let tracks_at = build(first).len() - tracks.len();
        let file = build(first + (FRONT - 2) - tracks_at);
        assert_eq!(
            file.len() - tracks.len(),
            FRONT - 2,
            "Tracks' id must straddle the window edge"
        );
        let (url, log) = serve_ranges_logged(file).await;
        let found = probe_tracks(&reqwest::Client::new(), &url)
            .await
            .expect("the recovery read must yield the tracks");
        assert!(
            found
                .iter()
                .any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("eng")),
            "the recovered probe must report the eng audio track, got {found:?}"
        );
        assert_eq!(
            log.lock().unwrap().as_slice(),
            [
                format!("bytes=0-{}", FRONT - 1),
                format!("bytes=0-{}", MAX_PROBE_FETCH - 1),
            ],
            "the front read, then one recovery read of the full window"
        );
    }

    #[tokio::test]
    async fn probe_tracks_mp4_recovery_refetch_succeeds_and_returns_tracks() {
        // The HAPPY path of the MP4 large-`moov` recovery (sibling of the MKV recovery success test):
        // a faststart MP4 whose front `moov` declares an extent PAST the front window makes
        // `parse_mp4_tracks(&front)` defer (Transient), so `probe_tracks` reads `front_moov_extent`
        // and issues ONE larger targeted fetch (`fetch_range(0, end - 1)`) covering the whole moov,
        // then re-parses. This drives that re-fetch + re-parse WIRING through an HTTP fixture — only
        // the `front_moov_extent` unit helper was covered — so an off-by-one in the `end - 1` window
        // or a wrong-offset fetch (which would defer forever and blacklist a valid file after the
        // verify deadline) is caught.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Full faststart MP4: ftyp + a front moov carrying an eng audio + ger subtitle track.
        let full = mp4_with(&[(b"soun", packed("eng")), (b"subt", packed("ger"))]);
        // Front body: the same MP4 with the moov's tail cut off — the moov header still DECLARES the
        // full size (which now overruns the shortened body), so the front parse defers and
        // `front_moov_extent` reports the full declared end (same shape as
        // `front_moov_extent_reports_declared_end_of_a_truncated_front_moov`).
        let mut front_body = full.clone();
        front_body.truncate(front_body.len() - 4);
        let recovery_body = full;
        assert!(
            front_body.len() > 8,
            "front body must exceed the container magic"
        );
        assert!(recovery_body.len() > front_body.len());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // 1st request (front read): the overrunning-but-valid truncated moov → defer.
            // 2nd request (recovery read): the full moov → parseable.
            for body in [front_body, recovery_body] {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/9000000000\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len() - 1,
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.flush().await;
                }
            }
        });
        let url = format!("http://{}/", addr);
        let tracks = probe_tracks(&reqwest::Client::new(), &url)
            .await
            .expect("MP4 recovery re-fetch must yield tracks");
        assert!(
            tracks
                .iter()
                .any(|t| matches!(t.kind, TrackKind::Audio) && t.language.as_deref() == Some("eng")),
            "the recovered probe must report the eng audio track, got {tracks:?}"
        );
        assert!(
            tracks
                .iter()
                .any(|t| matches!(t.kind, TrackKind::Subtitle)
                    && t.language.as_deref() == Some("ger")),
            "the recovered probe must report the ger subtitle track, got {tracks:?}"
        );
    }

    /// Read one HTTP request's headers from `sock` and return its `Range` header value ("" when
    /// absent), or `None` when the client went away first.
    async fn read_range_header(sock: &mut tokio::net::TcpStream) -> Option<String> {
        use tokio::io::AsyncReadExt;
        let mut req = Vec::new();
        let mut chunk = [0u8; 2048];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => req.extend_from_slice(&chunk[..n]),
            }
        }
        let req = String::from_utf8_lossy(&req).to_string();
        Some(
            req.lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("range")
                        .then(|| v.trim().to_string())
                })
                .unwrap_or_default(),
        )
    }

    /// Serve `file` over HTTP honouring `Range` like a real CDN: `bytes=a-b` / `bytes=a-` /
    /// `bytes=-n` → `206` with `Content-Range` + exact `Content-Length`; a start at/past EOF → `416`.
    /// Returns the base URL plus a log of every `Range` header received (in arrival order).
    async fn serve_ranges_logged(
        file: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::AsyncWriteExt;
        let file = std::sync::Arc::new(file);
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_log = log.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let file = file.clone();
                let log = server_log.clone();
                tokio::spawn(async move {
                    let Some(range) = read_range_header(&mut sock).await else {
                        return;
                    };
                    log.lock().unwrap().push(range.clone());
                    let len = file.len() as u64;
                    let spec = range.strip_prefix("bytes=").unwrap_or("");
                    let (a, b) = spec.split_once('-').unwrap_or(("", ""));
                    let (start, end) = if a.is_empty() {
                        let n: u64 = b.parse().unwrap_or(0);
                        (len.saturating_sub(n), len.saturating_sub(1))
                    } else {
                        let start: u64 = a.parse().unwrap_or(0);
                        let end = b
                            .parse::<u64>()
                            .unwrap_or(u64::MAX)
                            .min(len.saturating_sub(1));
                        (start, end)
                    };
                    if start >= len {
                        let head = format!(
                            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.flush().await;
                        return;
                    }
                    let body = &file[start as usize..=end as usize];
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (format!("http://{}/", addr), log)
    }
    async fn serve_ranges(file: Vec<u8>) -> String {
        serve_ranges_logged(file).await.0
    }

    /// Serve `file` like a CDN that IGNORES `Range` (TorBox's can): every request gets `200 OK` and
    /// the object from byte 0, whatever range it asked for. `Content-Length` declares a multi-GB
    /// object, but only `file` is sent and the connection is then held open, so a client that tried
    /// to read the whole body would hang instead of returning. Returns the base URL plus a log of
    /// every `Range` header received.
    async fn serve_ignoring_range(
        file: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::AsyncWriteExt;
        let file = std::sync::Arc::new(file);
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_log = log.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let file = file.clone();
                let log = server_log.clone();
                tokio::spawn(async move {
                    let Some(range) = read_range_header(&mut sock).await else {
                        return;
                    };
                    log.lock().unwrap().push(range);
                    let head = "HTTP/1.1 200 OK\r\nContent-Length: 9000000000\r\n\r\n";
                    if sock.write_all(head.as_bytes()).await.is_ok()
                        && sock.write_all(&file).await.is_ok()
                    {
                        let _ = sock.flush().await;
                    }
                    // Hold the connection (and the unsent rest of the "body") open.
                    std::future::pending::<()>().await;
                });
            }
        });
        (format!("http://{}/", addr), log)
    }

    /// Run a probe call under a deadline: one that waits for the rest of a body never sent hangs.
    async fn within_deadline<T>(fut: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(20), fut)
            .await
            .expect("the probe must not wait for the rest of a Range-ignoring 200 body")
    }

    #[tokio::test]
    async fn probe_tracks_reads_the_front_window_of_a_range_ignoring_200() {
        // TorBox's CDN may answer the offset-0 probe with `200 OK`, streaming the whole object.
        // `read_body` must accept that for the front read, keep the first 4 MB and drop the
        // connection, and the MKV parse then finds the Tracks inside that window. Requiring a `206`
        // here once made every TorBox probe defer forever.
        let mut audio = ebml_elem(0x83, &[2]);
        audio.extend(ebml_elem(0x22B59C, b"jpn"));
        let mut seg = ebml_elem(0x1654AE6B, &ebml_elem(0xAE, &audio));
        // A 6 MB Void after Tracks, so the body runs well past the 4 MB window.
        seg.extend(ebml_elem(0xEC, &vec![0u8; 6 * 1024 * 1024]));
        let mut file = ebml_elem(0x1A45DFA3, &[]);
        file.extend(ebml_elem_sized(0x18538067, 8_000_000_000, &seg));
        let (url, log) = serve_ignoring_range(file).await;
        let tracks = within_deadline(probe_tracks(&reqwest::Client::new(), &url))
            .await
            .expect("a Range-ignoring 200 at offset 0 is a usable front read");
        assert!(
            tracks
                .iter()
                .any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("jpn")),
            "got {tracks:?}"
        );
        assert_eq!(
            *log.lock().unwrap(),
            vec![format!("bytes=0-{}", 4 * 1024 * 1024 - 1)],
            "one front read, no recovery fetch"
        );
    }

    #[tokio::test]
    async fn fetch_rejects_a_range_ignoring_200_away_from_offset_zero() {
        // A `200` carries the object from byte 0, so it is the right bytes only for a read that
        // starts there: the front window is kept, anything else is the wrong bytes → Transient.
        let file: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
        let (url, _log) = serve_ignoring_range(file.clone()).await;
        let http = reqwest::Client::new();
        let front = within_deadline(fetch_range(&http, &url, 0, 99)).await;
        assert_eq!(front.as_deref(), Ok(&file[..100]));
        let mid = within_deadline(fetch_range(&http, &url, 100, 199)).await;
        assert_eq!(mid, Err(ProbeError::Transient));
        let tail = within_deadline(fetch_suffix(&http, &url, 1024)).await;
        assert_eq!(tail, Err(ProbeError::Transient));
    }

    #[tokio::test]
    async fn probe_tracks_non_faststart_mp4_behind_a_range_ignoring_200_defers() {
        // A non-faststart MP4 needs a read away from offset 0 (the trailing moov at the mdat's end,
        // then the suffix). Behind a CDN that ignores Range both come back as `200` with the head,
        // which must never be parsed as the moov: the probe defers (Transient) for a later retry.
        let with_ftyp = mp4_with(&[(b"soun", packed("eng"))]);
        let ftyp_len = u32::from_be_bytes(with_ftyp[0..4].try_into().unwrap()) as usize;
        let moov = with_ftyp[ftyp_len..].to_vec();
        let payload = 5 * 1024 * 1024;
        let file = non_faststart_mp4((8 + payload) as u32, payload, &moov);
        let (url, log) = serve_ignoring_range(file).await;
        let result = within_deadline(probe_tracks(&reqwest::Client::new(), &url)).await;
        assert_eq!(result, Err(ProbeError::Transient));
        let ranges = log.lock().unwrap().clone();
        let target = format!("bytes={}-", 40 + 8 + payload);
        assert!(
            ranges.iter().any(|r| r.starts_with(&target)),
            "the targeted trailing-moov read was attempted: {ranges:?}"
        );
        assert!(
            ranges.contains(&format!("bytes=-{}", 4 * 1024 * 1024)),
            "the suffix read was attempted: {ranges:?}"
        );
    }
    /// A non-faststart MP4: `ftyp` + `free` + an `mdat` whose header DECLARES `mdat_declared` bytes
    /// (header included) but carries `payload` zero bytes, followed by `trailer` (the `moov`, or
    /// nothing). The front 4 MB lands entirely inside the `mdat`.
    fn non_faststart_mp4(mdat_declared: u32, payload: usize, trailer: &[u8]) -> Vec<u8> {
        let mut file = ftyp32_free8();
        file.extend_from_slice(&mdat_declared.to_be_bytes());
        file.extend_from_slice(b"mdat");
        file.resize(file.len() + payload, 0);
        file.extend_from_slice(trailer);
        file
    }

    #[tokio::test]
    async fn probe_tracks_mp4_reads_trailing_moov_at_exact_offset() {
        // Non-faststart shape: ftyp, free, a 5 MiB mdat, then the moov at EOF. The 4 MB front read sees
        // no moov, and the 4 MB SUFFIX starts mid-mdat (not on a box boundary), so before the fix the
        // probe accepted as TracksNotFound without ever checking the language. The mdat's declared
        // end is the exact offset of the trailing moov → ONE targeted read there must yield tracks.
        let with_ftyp = mp4_with(&[(b"soun", packed("eng")), (b"subt", packed("ger"))]);
        let ftyp_len = u32::from_be_bytes(with_ftyp[0..4].try_into().unwrap()) as usize;
        let moov = with_ftyp[ftyp_len..].to_vec();
        assert_eq!(&moov[4..8], b"moov");
        let payload = 5 * 1024 * 1024;
        let file = non_faststart_mp4((8 + payload) as u32, payload, &moov);
        let url = serve_ranges(file).await;
        let tracks = probe_tracks(&reqwest::Client::new(), &url)
            .await
            .expect("a trailing moov at the mdat's declared end must be probed");
        assert!(
            tracks
                .iter()
                .any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("eng")),
            "the trailing-moov probe must report the eng audio track, got {tracks:?}"
        );
        assert!(
            tracks
                .iter()
                .any(|t| t.kind == TrackKind::Subtitle && t.language.as_deref() == Some("ger")),
            "the trailing-moov probe must report the ger subtitle track, got {tracks:?}"
        );
    }

    #[tokio::test]
    async fn probe_tracks_mp4_trailing_moov_failure_falls_back_to_accept() {
        // The mdat declares a size running PAST EOF, so the targeted read at its declared end gets a
        // 416. That must fall back to the existing suffix attempt (zero tail → TracksNotFound = accept),
        // NEVER surface as Transient (which would defer and re-probe the same layout forever).
        let payload = 5 * 1024 * 1024;
        let declared = 64 * 1024 * 1024u32;
        let file = non_faststart_mp4(declared, payload, &[]);
        let (url, log) = serve_ranges_logged(file).await;
        let result = probe_tracks(&reqwest::Client::new(), &url).await;
        assert!(
            matches!(result, Err(ProbeError::TracksNotFound)),
            "a failed targeted trailing-moov read must fall back to accept, got {result:?}"
        );
        let target = format!("bytes={}-", 40 + declared as u64);
        let ranges = log.lock().unwrap().clone();
        assert!(
            ranges.iter().any(|r| r.starts_with(&target)),
            "the targeted read at the mdat's declared end must have been attempted: {ranges:?}"
        );
        assert!(
            ranges
                .iter()
                .any(|r| r == &format!("bytes=-{}", 4 * 1024 * 1024)),
            "the suffix fallback must still run after the targeted read fails: {ranges:?}"
        );
    }

    #[tokio::test]
    async fn probe_tracks_mp4_trailing_moov_over_cap_falls_back_to_accept() {
        // A trailing moov LARGER than the MAX_PROBE_FETCH window: the targeted read parses as a
        // truncated moov (Transient). That must fall back to the suffix path (accept), not defer — a
        // re-probe would read the same window and defer forever.
        let payload = 5 * 1024 * 1024;
        let mut huge_moov = (9 * 1024 * 1024u32).to_be_bytes().to_vec();
        huge_moov.extend_from_slice(b"moov");
        huge_moov.extend_from_slice(&[0u8; 64]);
        let file = non_faststart_mp4((8 + payload) as u32, payload, &huge_moov);
        let url = serve_ranges(file).await;
        let result = probe_tracks(&reqwest::Client::new(), &url).await;
        assert!(
            matches!(result, Err(ProbeError::TracksNotFound)),
            "an over-cap trailing moov must fall back to accept, got {result:?}"
        );
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
            spoken_languages: vec![],
        };
        assert_eq!(verify(&tracks, &req), Verify::Pass);
        let req2 = LangReq {
            audio: AudioReq::Lang("eng".into()),
            subtitle: SubReq::None,
            original_language: None,
            spoken_languages: vec![],
        };
        assert_eq!(verify(&tracks, &req2), Verify::FailAudio);
        let req3 = LangReq {
            audio: AudioReq::Original,
            subtitle: SubReq::Lang("ger".into()),
            original_language: Some("jpn".into()),
            spoken_languages: vec![],
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
            spoken_languages: vec![],
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
            spoken_languages: vec![],
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

    /// The language of a one-track MKV whose audio TrackEntry carries the given `Language` and
    /// `LanguageBCP47` elements (each omitted when `None`).
    fn mkv_audio_language(legacy: Option<&str>, bcp47: Option<&str>) -> Option<String> {
        let mut audio = ebml_elem(0x83, &[2]);
        if let Some(l) = legacy {
            audio.extend(ebml_elem(0x22B59C, l.as_bytes()));
        }
        if let Some(l) = bcp47 {
            audio.extend(ebml_elem(0x22B59D, l.as_bytes()));
        }
        let segment = ebml_elem(0x18538067, &ebml_elem(0x1654AE6B, &ebml_elem(0xAE, &audio)));
        let mut bytes = ebml_elem(0x1A45DFA3, &[]);
        bytes.extend(segment);
        let tracks = parse_mkv_tracks(&bytes).expect("parse");
        assert_eq!(tracks.len(), 1);
        tracks[0].language.clone()
    }

    #[test]
    fn mkv_language_bcp47_overrides_the_legacy_language_element() {
        // RFC 9559: when LanguageBCP47 is present the legacy Language element (whose absence means
        // `eng`) is ignored. A track written with LanguageBCP47 alone is not English.
        let ru = mkv_audio_language(None, Some("ru"));
        assert_eq!(ru.as_deref(), Some("ru"));
        assert!(lang_eq(ru.as_deref().unwrap(), "rus"));
        assert_eq!(
            mkv_audio_language(Some("eng"), Some("ja")).as_deref(),
            Some("ja")
        );
        // Only the primary subtag names the language.
        assert_eq!(
            mkv_audio_language(None, Some("pt-BR")).as_deref(),
            Some("pt")
        );
        assert_eq!(
            mkv_audio_language(None, Some("zh-Hant-HK")).as_deref(),
            Some("zh")
        );
        // `und`/empty stay inconclusive, as with the legacy element.
        assert_eq!(mkv_audio_language(None, Some("und")), None);
        assert_eq!(mkv_audio_language(Some("eng"), Some("")), None);
        // Without LanguageBCP47 the legacy rules are unchanged.
        assert_eq!(
            mkv_audio_language(Some("fre"), None).as_deref(),
            Some("fre")
        );
        assert_eq!(mkv_audio_language(None, None).as_deref(), Some("eng"));
    }

    #[test]
    fn mkv_language_strings_end_at_a_nul_terminator() {
        // RFC 8794 §13: an EBML String may be NUL-terminated/padded, and a reader ignores the first
        // NUL and everything after it. A padded `jpn\0` must read as `jpn`, not a 4-character code
        // that matches no wanted language (→ FailAudio → a correct release blacklisted).
        let jpn = mkv_audio_language(Some("jpn\0"), None);
        assert_eq!(jpn.as_deref(), Some("jpn"));
        assert!(lang_eq(jpn.as_deref().unwrap(), "ja"));
        assert_eq!(
            mkv_audio_language(None, Some("ja\0\0\0")).as_deref(),
            Some("ja")
        );
        // Bytes after the terminator are ignored even when they aren't valid UTF-8.
        let mut audio = ebml_elem(0x83, &[2]);
        audio.extend(ebml_elem(0x22B59D, b"pt-BR\0\xff\xfe"));
        let segment = ebml_elem(0x18538067, &ebml_elem(0x1654AE6B, &ebml_elem(0xAE, &audio)));
        let mut bytes = ebml_elem(0x1A45DFA3, &[]);
        bytes.extend(segment);
        let tracks = parse_mkv_tracks(&bytes).expect("parse");
        assert_eq!(tracks[0].language.as_deref(), Some("pt"));
        // An all-NUL value is empty: inconclusive, NOT the `eng` an absent element defaults to.
        assert_eq!(mkv_audio_language(Some("\0\0\0"), None), None);
        assert_eq!(mkv_audio_language(Some("eng"), Some("\0\0")), None);
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
            spoken_languages: vec![],
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
            spoken_languages: vec![],
        };
        assert_eq!(verify(&tracks, &req), Verify::Pass);
    }

    #[test]
    fn verify_subtitle_untagged_inconclusive_but_wrong_tagged_fails() {
        let req = LangReq {
            audio: AudioReq::Lang("eng".into()),
            subtitle: SubReq::Lang("eng".into()),
            original_language: None,
            spoken_languages: vec![],
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
            spoken_languages: vec![],
        };
        assert_eq!(verify(&tracks, &req), Verify::Pass);
    }

    fn eng_audio() -> Vec<Track> {
        vec![Track {
            kind: TrackKind::Audio,
            language: Some("eng".into()),
        }]
    }

    #[test]
    fn verify_original_accepts_a_spoken_language() {
        // A film with TMDB original_language `fi`, spoken [en, fi, es]; the release's audio is `eng`.
        // Under AUDIO_LANGUAGE=original the acceptable set is original ∪ spoken → Pass, not BadAudio.
        let req = LangReq {
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            original_language: Some("fi".into()),
            spoken_languages: vec!["en".into(), "fi".into(), "es".into()],
        };
        assert_eq!(verify(&eng_audio(), &req), Verify::Pass);
        // A language outside the set is still rejected.
        let ger = vec![Track {
            kind: TrackKind::Audio,
            language: Some("ger".into()),
        }];
        assert_eq!(verify(&ger, &req), Verify::FailAudio);
    }

    #[test]
    fn verify_original_without_spoken_still_requires_original() {
        let req = LangReq {
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            original_language: Some("fi".into()),
            spoken_languages: vec![],
        };
        assert_eq!(verify(&eng_audio(), &req), Verify::FailAudio);
    }

    #[test]
    fn verify_explicit_audio_ignores_spoken() {
        let req = LangReq {
            audio: AudioReq::Lang("de".into()),
            subtitle: SubReq::None,
            original_language: Some("en".into()),
            spoken_languages: vec!["en".into()],
        };
        assert_eq!(verify(&eng_audio(), &req), Verify::FailAudio);
    }

    fn audio(lang: &str) -> Vec<Track> {
        vec![Track {
            kind: TrackKind::Audio,
            language: Some(lang.into()),
        }]
    }

    fn original(code: &str) -> LangReq {
        LangReq {
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            original_language: Some(code.into()),
            spoken_languages: vec![],
        }
    }

    #[test]
    fn verify_tmdb_cantonese_original_matches_chinese_tracks() {
        // TMDB writes `cn` (not an ISO 639-1 code) for Cantonese. Its tracks are tagged with the
        // Chinese macrolanguage (`chi`/`zho`) or ISO 639-3 `yue`, all of which must pass; a
        // Mandarin (`cmn`) dub of a Cantonese title is still the wrong language.
        let req = original("cn");
        for tag in ["chi", "zho", "yue"] {
            assert_eq!(verify(&audio(tag), &req), Verify::Pass, "{tag}");
        }
        assert_eq!(verify(&audio("cmn"), &req), Verify::FailAudio);
        assert_eq!(verify(&audio("eng"), &req), Verify::FailAudio);
        // A Mandarin title (`zh`) passes its `cmn`- or `chi`-tagged track.
        assert_eq!(verify(&audio("cmn"), &original("zh")), Verify::Pass);
        assert_eq!(verify(&audio("zho"), &original("zh")), Verify::Pass);
    }

    #[test]
    fn verify_tmdb_no_language_original_has_no_audio_requirement() {
        // TMDB's `xx` ("No Language") marks a silent or dialogue-free title: whatever its music
        // track is tagged (`eng` is the MKV default for an absent tag, or `zxx`), there is no wrong
        // language.
        let req = original("xx");
        for tag in ["eng", "zxx", "rus"] {
            assert_eq!(verify(&audio(tag), &req), Verify::Pass, "{tag}");
        }
        let mut with_spoken = original("xx");
        with_spoken.spoken_languages = vec!["en".into()];
        assert_eq!(verify(&audio("rus"), &with_spoken), Verify::Pass);
        assert!(is_no_language("xx") && is_no_language("zxx") && !is_no_language("en"));
    }

    #[test]
    fn verify_tmdb_serbo_croatian_original_matches_its_members() {
        // TMDB writes the deprecated `sh` (Serbo-Croatian) for many Yugoslav-era titles; their
        // tracks are tagged Croatian, Serbian or Bosnian.
        let req = original("sh");
        for tag in ["hrv", "srp", "bos", "hbs"] {
            assert_eq!(verify(&audio(tag), &req), Verify::Pass, "{tag}");
        }
        assert_eq!(verify(&audio("eng"), &req), Verify::FailAudio);
        // The members stay distinct languages: a Serbian track is not Croatian.
        assert_eq!(verify(&audio("srp"), &original("hr")), Verify::FailAudio);
    }

    #[test]
    fn lang_eq_matches_a_macrolanguage_with_its_members_only() {
        assert!(lang_eq("zh", "yue") && lang_eq("yue", "chi") && lang_eq("cn", "zho"));
        assert!(lang_eq("no", "nob") && lang_eq("nb", "nor") && lang_eq("nn", "no"));
        assert!(
            !lang_eq("yue", "cmn"),
            "two members of a macrolanguage differ"
        );
        assert!(!lang_eq("nob", "nno"));
        assert!(!lang_eq("hr", "sr"));
    }

    #[test]
    fn verify_any_of_passes_on_any_listed_language() {
        let req = LangReq {
            audio: AudioReq::AnyOf(vec!["fi".into(), "en".into()]),
            subtitle: SubReq::None,
            original_language: None,
            spoken_languages: vec![],
        };
        assert_eq!(verify(&eng_audio(), &req), Verify::Pass);
        let ger = vec![Track {
            kind: TrackKind::Audio,
            language: Some("deu".into()),
        }];
        assert_eq!(verify(&ger, &req), Verify::FailAudio);
    }

    #[test]
    fn is_known_language_code_knows_the_codes_the_tables_name_and_nothing_else_with_two_letters() {
        // `config` rejects a 2-letter AUDIO_LANGUAGE / SUBTITLE_LANGUAGE this doesn't know, so it
        // must know every ISO 639-1 code (the table is complete) and TMDB's `cn`/`xx`/`sh`.
        for (two, b) in ISO_639_1 {
            assert!(is_known_language_code(two), "639-1 {two}");
            assert!(is_known_language_code(b), "639-2/B {b}");
        }
        for (t, b) in ISO_639_2_T_TO_B {
            assert!(is_known_language_code(t), "/T {t}");
            // Every /T form folds to a /B code the 639-1 table produces too.
            assert!(ISO_639_1.iter().any(|(_, x)| x == b), "/B {b}");
        }
        // The other codes the probe compares: macrolanguage members, multiple, undetermined.
        for code in [
            "cmn", "cnr", "yue", "nob", "nno", "zxx", "hbs", "mul", "und",
        ] {
            assert!(is_known_language_code(code), "{code}");
        }
        // Case and surrounding whitespace don't matter, as in `to_iso639_2`.
        assert!(is_known_language_code(" DEU ") && is_known_language_code("En"));
        for code in ["xy", "zz", "egn", "off", "any", "e", "", "english", "en-US"] {
            assert!(!is_known_language_code(code), "{code}");
        }
    }
}
