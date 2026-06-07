use crate::config::{AudioReq, SubReq};

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

/// Minimal ISO 639-1 → 639-2/B map; passes through 3-letter codes and unknown 2-letter codes.
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
        _ => return c,
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

    let want_audio: Option<String> = match &req.audio {
        AudioReq::Lang(l) => Some(l.clone()),
        AudioReq::Original => req.original_language.clone(),
    };
    if let Some(want) = want_audio {
        if !audios.iter().any(|a| lang_eq(a, &want)) {
            return Verify::FailAudio;
        }
    }
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
            pos.checked_add(size as usize).ok_or(ProbeError::Corrupt)?
        };
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
            payload_start.checked_add(size as usize).ok_or(ProbeError::Corrupt)?
        };
        if payload_end > end {
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
        if size == u64::MAX {
            return Err(ProbeError::Corrupt);
        }
        let p_end = pos.checked_add(size as usize).ok_or(ProbeError::Corrupt)?;
        if p_end > end {
            return Err(ProbeError::Corrupt);
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
                language = std::str::from_utf8(&buf[pos..p_end])
                    .ok()
                    .map(|s| s.trim().to_string());
            }
            _ => {}
        }
        pos = p_end;
    }
    Ok(Track {
        kind,
        language: language.or_else(|| Some("eng".to_string())),
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
            return Err(ProbeError::Corrupt);
        }
        let big = u64::from_be_bytes(buf[pos + 8..pos + 16].try_into().unwrap());
        // A crafted 64-bit largesize must not overflow usize (input is untrusted CDN bytes).
        let end = usize::try_from(big)
            .ok()
            .and_then(|b| pos.checked_add(b))
            .ok_or(ProbeError::Corrupt)?;
        (pos + 16, end)
    } else if size32 == 0 {
        (pos + 8, buf.len())
    } else {
        let end = pos.checked_add(size32 as usize).ok_or(ProbeError::Corrupt)?;
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
            return Err(ProbeError::Corrupt);
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
                // returns TracksNotFound and acquisition accepts with a warning.
                // TODO(SP1+): scan the tail for the `moov` box signature for reliable handling.
                let tail = fetch_suffix(http, cdn_url, FRONT).await?;
                parse_mp4_tracks(&tail)
            }
            other => other,
        },
        None => Err(ProbeError::Unsupported),
    }
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
        .map_err(|_| ProbeError::Transient)?;
    if !resp.status().is_success() {
        return Err(ProbeError::Transient);
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|_| ProbeError::Transient)
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
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|_| ProbeError::Transient)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(tracks.iter().any(|t| t.kind == TrackKind::Audio && t.language.as_deref() == Some("eng")));
        assert!(tracks.iter().any(|t| t.kind == TrackKind::Subtitle && t.language.as_deref() == Some("fre")));
    }
    #[test]
    fn mkv_truncated_is_corrupt() {
        let mut bytes = mkv_with("eng", None);
        bytes.truncate(bytes.len() - 3);
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
        let mut bytes = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        bytes.extend(mp4_box(b"mdat", &[0u8; 16]));
        assert!(matches!(parse_mp4_tracks(&bytes), Err(ProbeError::TracksNotFound)));
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
        assert_eq!(detect_container(&mkv_with("eng", None)), Some(ContainerKind::Mkv));
        assert_eq!(detect_container(&mp4_with(&[(b"soun", packed("eng"))])), Some(ContainerKind::Mp4));
        assert_eq!(detect_container(b"RIFF\0\0\0\0AVI LIST"), None);
    }
    #[test]
    fn verify_audio_original_and_subtitle_rules() {
        let tracks = vec![
            Track { kind: TrackKind::Audio, language: Some("jpn".into()) },
            Track { kind: TrackKind::Subtitle, language: Some("eng".into()) },
        ];
        let req = LangReq { audio: AudioReq::Original, subtitle: SubReq::Lang("eng".into()), original_language: Some("jpn".into()) };
        assert_eq!(verify(&tracks, &req), Verify::Pass);
        let req2 = LangReq { audio: AudioReq::Lang("eng".into()), subtitle: SubReq::None, original_language: None };
        assert_eq!(verify(&tracks, &req2), Verify::FailAudio);
        let req3 = LangReq { audio: AudioReq::Original, subtitle: SubReq::Lang("ger".into()), original_language: Some("jpn".into()) };
        assert_eq!(verify(&tracks, &req3), Verify::FailSubtitle);
        assert_eq!(verify(&[], &req), Verify::Inconclusive);
    }
    #[test]
    fn iso_639_1_to_2_mapping() {
        assert_eq!(to_iso639_2("en"), "eng");
        assert_eq!(to_iso639_2("eng"), "eng");
        assert_eq!(to_iso639_2("ja"), "jpn");
    }
}
