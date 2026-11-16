use crate::rd_client;
use crate::tmdb_client::{TmdbClient, TmdbSearchResult};
use crate::vfs::{is_video_file, MediaMetadata, MediaType, VIDEO_EXTENSIONS};
use chrono::Datelike;
use regex::Regex;
use std::sync::LazyLock;
use tracing::{debug, info, warn};

static CAMEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z])([A-Z])").unwrap());

// Leading release-group / site garbage to strip: a bracket tag `[...]`, a parenthesised tag, a
// parenthesised release-year (so a leading "(2020) Title" strips), or a `site.tld - ` prefix
// (the `site` group). Any dotted name in front of a ` - ` separator fits the `site` pattern: a show
// name ("Some.Show - S01E01 - Pilot", "The.Office.UK - S01E01 - …"), a title ("Dr.No - 1962") or
// a whole release name ("Show.Name.S01E01.720p.HDTV - Group"), and stripping one of those leaves
// an episode title, a year or a group name as the TMDB query. So `clean_name` strips a `site`
// match only when `site_match_is_a_site` says it is one.
// A parenthesised TAG must look like a site/URL tag — its content carries a letter AND a `.`/space
// (e.g. `(www.site.com)`) — so a single parenthesised title WORD like `(Untitled)` (the 2009 film)
// is kept as title content, and a NON-year numeric title-paren like `(500)` — "(500) Days of
// Summer" — is likewise kept (no letter at all).
static PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(\[.*?\]|\((?:[^)]*[a-z][^)]*[.\s][^)]*|[^)]*[.\s][^)]*[a-z][^)]*|(?:19|20)\d{2})\)|(?P<site>[\w-]+(?:\.[\w-]+)*\.[a-z]{2,6}\s+-\s+))\s*",
    )
    .unwrap()
});

/// What follows a `site.tld - ` that is really a show or film name: an episode code (`S01E01`,
/// `S01`, `E05`, `Ep01`, `1x01`, `Season 2`, `Episode.5`), a year (`Dr.No - 1962`) or a bare number
/// (`Show.Name - 101`). Matched against the text after the [`PREFIX_RE`] `site` match (the `regex`
/// crate has no lookahead).
static SITE_FOLLOWED_BY_EPISODE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:s\d{1,3}\.?e\d{1,4}|s\d{1,3}\b|e\d{1,4}\b|ep\.?\s*\d+|\d{1,2}x\d{2,3}\b|seasons?[\s.]*\d+|episodes?[\s.]*\d+|(?:19|20)\d{2}\b|\d{1,4}(?:[\s._]|$))").unwrap()
});

/// A release-name token: a year, a resolution, an SxxEyy code or a source/codec tag. A release name
/// nearly always carries one and a site name never does (see [`site_match_is_a_site`]).
static RELEASE_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:(?:19|20)\d{2}|(?:360|480|540|576|720|1080|1440|2160)p|4k|bluray|web[\s.-]?dl|webrip|hdtv|bdrip|brrip|dvdrip|remux|[xh]\.?26[45]|hevc|xvid|divx)\b|\bs\d{1,3}\.?e\d{1,4}").unwrap()
});

/// Whether a [`PREFIX_RE`] `site` match (`site`, with its ` - `) in front of `rest` is a real
/// `site.tld - ` prefix rather than a dotted show, film or release name. A `www.` name always is.
/// Any other is kept as title content when it carries a release token itself
/// (`Show.Name.S01E01.720p.HDTV - Group`), when an episode code, a year or a bare number follows it
/// (`Some.Show - S01E01`, `Dr.No - 1962`, `Show.Name - 101`), or when no release name follows it
/// (`Mr.Bean - The Movie`): a site prefix is followed by a release name, which carries a release
/// token. Keeping a real site costs little, since `identify_name`'s leading-word strip recovers
/// the title, while stripping a title leaves an unrelated query.
fn site_match_is_a_site(site: &str, rest: &str) -> bool {
    if site
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("www."))
    {
        return true;
    }
    !RELEASE_TOKEN_RE.is_match(site)
        && !SITE_FOLLOWED_BY_EPISODE_RE.is_match(rest)
        && RELEASE_TOKEN_RE.is_match(rest)
}

/// A leading `[Title] (YYYY)` — bracket(s) that are PART of the title (e.g. `[REC] (2007)`),
/// confirmed by an immediately-following parenthesised year — is NOT a release-group tag, so
/// `PREFIX_RE` must not strip it (which would leave a year fragment like `2007)` as the "title"). A
/// genuine leading tag is `[group] Title …`, never `[group] (year)`.
static TITLE_BRACKET_YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\[[^\]]*\]\s*\((?:19|20)\d{2}\)").unwrap());

static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}\b").unwrap());

static STOP_RE: LazyLock<Regex> = LazyLock::new(|| {
    // NB: bare 3-letter ISO language codes (pol|eng|ita|ger|fra|spa|esp|rus|ukr) are deliberately
    // NOT stop words — they collide with real standalone title words ("Spa Night", "Pol Pot") and
    // would truncate/mangle the TMDB query. Audio/subtitle language is verified by `probe`, not by
    // filename cleaning, so dropping them costs nothing in identification value.
    //
    // For the SAME reason, the pack/label words `complete`/`completa`/`custom`/`multi` are NOT stop
    // words: they collide with real title words, so as a leftmost-match TRUNCATION point they wrongly
    // discarded the rest of a legitimate title (e.g. "A Complete Unknown" → "A", "The Custom of the
    // Country" → "The"). When such a token is a genuine release tag it is followed by a real metadata
    // token (resolution/codec/source) that truncates instead; when it is a title word the query keeps
    // it, which TMDB's fuzzy scoring tolerates — exactly as for the ISO codes above.
    // The resolution alternative is a bounded set of real heights (360p to 2160p, SD included: a
    // year-less "Movie.480p.x264" would otherwise query TMDB for "Movie 480p") rather than any
    // `\d+p`, so a title word like "300p" is never a truncation point. It accepts an optional 2-3
    // digit framerate suffix (`1080p60`/`2160p50`) followed by an optional glued letter run
    // (`[a-z]*`, e.g. the bit-depth tag in `1080p10bit`/`2160p10bit`), so a framerate- OR
    // bit-depth-glued quality tag still truncates the title rather than leaking into the TMDB
    // query. `dvdrip`/`bdrip`/`brrip`/`xvid`/`divx` are SD rip/codec tags that are never title
    // words, so they truncate too. The trailing `[a-z]*` mirrors
    // `release::RES_RE`'s boundary relaxation (the shared `\b(...)\b` would otherwise refuse to
    // match a token whose suffix runs straight into the closing word char).
    // NB: the `WxH` resolution alternative is bounded to `\d{3,4}x\d{3,4}` (e.g. 1920x1080, 720x480)
    // rather than an unbounded `\d+x\d+`. An unbounded form consumed a movie literally named `10x10`
    // as a leftmost-match truncation point, collapsing the title to its trailing year.
    // `web[\s-]?dl` (not just `web-dl`) so the dot form `WEB.DL` — which step-2 dot→space replacement
    // turns into `WEB DL` — also truncates, rather than leaking `WEB DL` into the TMDB query.
    // `ep\s*\d+` is the short episode marker (`Ep01`, `Ep.5` → `Ep 5`), so an episode title after it
    // (`Show Name - Ep01 - Title`) never reaches the query; the leading `\b` keeps `Step 2` intact.
    Regex::new(r"(?i)\b((?:360|480|540|576|720|1080|1440|2160)p(?:\d{2,3})?[a-z]*|4k|s\d+e\d+|s\d+|seasons?\s*\d+|\d+\s*seasons?|temporada\s*\d+|saison\s*\d+|\d{3,4}x\d{3,4}|episodes?\s*\d+|e\d+|ep\s*\d+|parts?\s*\d+|vol(ume)?\s*\d+|bluray|web[\s-]?dl|h264|h265|x264|x265|remux|vff|dts|dd5|dd\+5|ddp5|esub|webrip|hdtv|avc|hevc|aac|truehd|atmos|criterion|repack|dvdrip|bdrip|brrip|xvid|divx)\b").unwrap()
});

static YEAR_RANGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}[\s-]+(19|20)\d{2}\b").unwrap());

// Word-anchored STRONG show markers — everything except the loose `NxM` form, which is handled
// separately (see `NXM_RE`) so a movie *title* written `NxM` can be excluded. `\be\d{1,3}\b`
// (episode marker E05) excludes embedded matches like the `e7` in "Se7en", and
// `\bep[\s.-]*\d{1,3}\b` (the short episode word: `Ep 5`, `Ep.05`) the `ep` of "Step 2". The SxxExx
// alternative is leading-`\b`-anchored but intentionally NOT trailing-anchored, so contiguous
// multi-episode and version markers (S01E01E02, S01E13v2) still match. A season word may be dotted
// from its number (`Season.2`), as `clean_name` and `names_a_title` read it. A bare `Episode N` is
// no marker: films are named that way too (`Star.Wars.Episode.4`). The acquisition title check,
// which knows it wants a show, passes that hint itself ([`identify_name_with_show_hint`]).
static STRONG_SHOW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bs\d+\.?e\d+|\bs\d+\b|\bseasons?[\s.]*\d+|\b\d+\s*seasons?|\btemporada[\s.]*\d+|\bsaison[\s.]*\d+|\be\d{1,3}\b|\bep[\s.-]*\d{1,3}\b").unwrap()
});

// The loose `NxM` episode form (e.g. 1x01, 3x14, 10x24). Bounded so a movie filename does not
// trip it structurally: the 1-2-digit first number excludes a resolution such as 1920x1080, and
// the ≥2-digit second number excludes an aspect-ratio tag such as 16x9 / 4x3 / 21x9 (a real NxM
// episode is 1x01, not 1x9). It still matches a movie whose *title* is written `NxM` (10x10,
// 24x36); `NXM_TITLE_ONLY_RE` filters those out — see `filename_has_show_marker`.
static NXM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b\d{1,2}x\d{2,3}\b").unwrap());

// A cleaned title that is ENTIRELY an `NxM` token (e.g. `10x10`, `24x36`) — a movie whose name
// happens to be written that way, NOT an episode marker (which keeps the show name in the title).
static NXM_TITLE_ONLY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\d{1,2}x\d{2,3}$").unwrap());

/// Whether a single filename carries a genuine episode/show marker. Strong markers (SxxExx,
/// Season N, Exx, …) are unambiguous. The loose `NxM` form also matches movie *titles* written
/// that way (e.g. `10x10.2018`, `24x36.2019`): such a title is the whole cleaned name (only a
/// year/quality metadata follows), whereas a real `NxM` marker is preceded by the show name — so
/// after cleaning the former reduces to exactly the `NxM` token and is NOT treated as a marker.
/// A trailing ` - NN` / ` - EpNN` episode number ([`trailing_episode_number`], the absolute-numbered
/// anime form `[Group] Some Show - 12 (1080p).mkv`) is one too: the acquisition engine reads it as
/// the file's episode, and that rule already leaves out a release year, a single digit and a
/// numeric film title followed by its bracketed year.
///
/// The `\b`-bounded markers run on the name with `_` read as a space: the regex crate counts `_` as
/// a word character, so `Some_Show_S01E05_720p.mkv` would otherwise carry no marker while
/// `vfs::parse_se_all`, which has no `\b`, reads it as S01E05. A space rather than a `.`, so
/// `Season_2` reads as `Season 2`, as `clean_name` and [`trailing_episode_number`] read it.
fn filename_has_show_marker(filename: &str) -> bool {
    let spaced = filename.replace('_', " ");
    if STRONG_SHOW_RE.is_match(&spaced) || trailing_episode_number(filename).is_some() {
        return true;
    }
    // A channel layout run into its codec (`DD5.1x264`, `AAC2.0x264`) is no `NxM` code, as
    // `vfs::parse_se_all` reads it.
    let nxm_code = NXM_RE.find_iter(&spaced).any(|m| {
        let x = m.start() + m.as_str().find(['x', 'X']).unwrap_or(0);
        !crate::vfs::is_channel_layout_codec_at(&spaced, m.start(), x)
    });
    if nxm_code {
        let (title, _year) = clean_name(filename);
        return !NXM_TITLE_ONLY_RE.is_match(title.trim());
    }
    false
}

// A LETTER designator (e.g. "Part A") must be separated from the stop-word by whitespace, so a
// stop-word glued to a single trailing letter — which forms a real word: "Episodes", "Parts",
// "Seasons", "Party", "Volo" — is NOT treated as generic (that would strip its TMDB identity before
// any lookup). A NUMBER designator still allows the glued form ("Season1"), which is unambiguously
// a generic folder name. The season words of other spellings (the UK `Series`, German, Italian,
// Polish, Dutch, Spanish, French) count only with a number, so a bare `Series` title is kept.
static GENERIC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:(episode|season|part|volume|vol)(\s*\d+|\s+[a-z])?|(series|staffel|stagione|sezon|seizoen|temporada|saison)\s*\d+)$").unwrap()
});

/// A bare " - NNN - " episode-number separator (e.g. "Some Show - 409 - Episode Title": the
/// absolute/SxxEyy code some packs use instead of "S04E09"). The show name precedes it; everything
/// from the separator on is episode numbering/title.
static EPISODE_NUM_DASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+-\s+\d{1,3}\s+-\s+").unwrap());

/// A ` - NN` episode number that ENDS the name's title run: no second ` - ` follows it, only the
/// end of the name or a bracket group (`[SubsPlease] Some Show - 12 (1080p) [hash]`, the common
/// anime form; `Show.Name - 101`; `Some Show - Ep01 [720p]`), with an optional `Ep` prefix and
/// `v2` version suffix. Two to four digits, so a single-digit sequel (`Some Film - 2`) is kept;
/// `clean_name` also skips a 19xx/20xx year (`Dr.No - 1962`) and a number whose bracket group is
/// a release year (`Prefix - 21 (2008)`, [`BRACKETED_YEAR_RE`]): that is a numeric film title,
/// while an episode number is followed by a quality, CRC or version group. Matched after the
/// dot → space replacement.
static TRAILING_EPISODE_NUM_DASH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s+-\s+(?:ep\s*)?(\d{2,4})(?:v\d{1,2})?(?:\s*[\[(]|\s*$)").unwrap()
});

/// A release year closing the bracket group that [`TRAILING_EPISODE_NUM_DASH_RE`] has just
/// opened (`2008)` / `2007]`), matched against the text after that match.
static BRACKETED_YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:19|20)\d{2}\s*[)\]]").unwrap());

/// The first [`TRAILING_EPISODE_NUM_DASH_RE`] episode number in `title` (a name after the dot →
/// space replacement), as where its match starts and the number. A 19xx/20xx year and a number
/// followed by a bracketed release year ([`BRACKETED_YEAR_RE`]) are skipped: those are a release
/// year and a numeric film title. `clean_name` cuts the title there, and
/// [`trailing_episode_number`] reads the episode from it.
fn trailing_episode_cut(title: &str) -> Option<(usize, u32)> {
    TRAILING_EPISODE_NUM_DASH_RE
        .captures_iter(title)
        .filter(|c| {
            c.get(1).is_some_and(|n| !YEAR_RE.is_match(n.as_str()))
                && !c
                    .get(0)
                    .is_some_and(|m| BRACKETED_YEAR_RE.is_match(&title[m.end()..]))
        })
        .find_map(|c| Some((c.get(0)?.start(), c.get(1)?.as_str().parse().ok()?)))
}

/// The ` - NN` episode number a file name ends its title with (the common anime form
/// `[SubsPlease] Some Show - 12 (1080p) [ABCD1234].mkv` → 12), by the rule `clean_name` cuts the
/// title at: `None` for a release year, a numeric film title (`Some Film - 21 (2008)`), a number a
/// second ` - ` follows, or a name with no such number. The acquisition engine maps a single-file
/// release with no episode code to its episode with it.
pub(crate) fn trailing_episode_number(file_name: &str) -> Option<u32> {
    let mut name = file_name;
    if let Some(pos) = name.rfind('.') {
        let ext = name[pos..].to_lowercase();
        if VIDEO_EXTENSIONS.iter().any(|e| *e == ext) {
            name = &name[..pos];
        }
    }
    let name = name.replace(['.', '_'], " ");
    trailing_episode_cut(&name)
        .filter(|&(start, _)| start > 0)
        .map(|(_, number)| number)
}

/// Numbering that opens a name (after the dot/underscore → space replacement): an `SxxEyy` code
/// (`S01E05`), a season token (`S04`, `Season 04`, and in other spellings `Series 1`, `Staffel 2`,
/// `Stagione 1`, `Sezon 1`, `Seizoen 1`, `Temporada 2`, `Saison 3`, or a number spelled out as in
/// `Season One`), an episode word (`E09`, `Ep 9`, `Episode 9`), or a part, disc or volume number
/// (`Part 2`, `Disc 1`, `CD1`, `Vol 3`). A title run ends at any of them, so a name that opens with
/// one names no title.
static LEADING_NUMBERING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:s\d{1,3}\s?e\d{1,4}|s\d{1,3}\b|(?:e|ep|episodes?|seasons?|series|staffel|stagione|sezon|seizoen|temporada|saison|parts?|dis[ck]s?|cd|vol(?:ume)?)\s*\d{1,4}\b|(?:seasons?|series)\s+(?:one|two|three|four|five|six|seven|eight|nine|ten)\b)").unwrap()
});

/// A bare number or an `NxM` code opening a name (`05`, `1x05`). Films open with numbers too (`12
/// Angry Men`, `300 2006`, `10x10 2018`), so [`names_a_title`] reads one as an episode's numbering
/// only when nothing but a ` - `, a bracket group or a release tag follows it.
static LEADING_NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:\d{1,2}x\d{2,3}|\d{1,3})\b").unwrap());

/// Whether a file or directory name carries a title of its own: the text before its first episode
/// marker or release tag, once leading bracket tags (`[Group]`, `(2019)`) and separators are
/// dropped, is not empty. `E09 - Title.mkv`, `S01E05.mkv`, `05 [1080p].mkv`, `05 - Title.mkv`,
/// `Season 04` and `Disc 1` carry none (the text after the marker is an episode title, if
/// anything); `Show.S01E05`, `[Group] Show - 05`, `Movie (2019)` and `12 Angry Men` do. A leading
/// `[Title] (YYYY)` is a title, as in [`clean_name`]. The acquisition title check identifies a file
/// that carries none by its directory or torrent name instead.
pub(crate) fn names_a_title(name: &str) -> bool {
    let mut rest = name;
    if let Some(pos) = rest.rfind('.') {
        let ext = rest[pos..].to_lowercase();
        if VIDEO_EXTENSIONS.iter().any(|e| *e == ext) {
            rest = &rest[..pos];
        }
    }
    loop {
        rest = rest.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '[' && c != '(');
        if TITLE_BRACKET_YEAR_RE.is_match(rest) {
            return true;
        }
        let close = match rest.chars().next() {
            Some('[') => ']',
            Some('(') => ')',
            _ => break,
        };
        match rest.find(close) {
            Some(end) => rest = &rest[end + 1..],
            None => break,
        }
    }
    let spaced = rest.replace(['.', '_'], " ");
    let spaced = spaced.trim_start_matches(|c: char| !c.is_alphanumeric());
    let opens_with_tag = |s: &str| STOP_RE.find(s).is_some_and(|m| m.start() == 0);
    if spaced.is_empty() || LEADING_NUMBERING_RE.is_match(spaced) || opens_with_tag(spaced) {
        return false;
    }
    match LEADING_NUMBER_RE.find(spaced) {
        Some(number) => {
            let after = spaced[number.end()..].trim_start();
            !(after.is_empty() || after.starts_with(['-', '[', '(']) || opens_with_tag(after))
        }
        None => true,
    }
}

/// The name the file at `file_path` (in a torrent named `torrent_name`) is identified by: the file
/// name when it names a title of its own ([`names_a_title`]), so `Other.Show.S01E05.mkv` is still
/// another show whatever its torrent is called. A file name that names none (`E09 - Title.mkv`,
/// `S01E05.mkv`, `05.mkv`) says nothing about the title (what `clean_name` leaves of it is the
/// episode title, if anything), so the nearest enclosing directory that names one is used instead
/// (`Season 04`, `S04`, `Series 1`, `Part 2` and `Disc 1` are skipped), else the torrent name:
/// TorBox paths start with the torrent folder, while Real-Debrid paths
/// (`/Season 04/E09 - Title.mkv`) carry no torrent name. When nothing names a title, the file
/// name is used. The acquisition title check (`acquire::TmdbTitleValidator`) and
/// [`identify_torrent`] both identify a file by it.
pub(crate) fn identification_name<'a>(file_path: &'a str, torrent_name: &'a str) -> &'a str {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    if names_a_title(file_name) {
        return file_name;
    }
    let dir = &file_path[..file_path.len() - file_name.len()];
    dir.rsplit('/')
        .filter(|c| !c.is_empty())
        .find(|c| names_a_title(c))
        .or_else(|| Some(torrent_name).filter(|n| names_a_title(n)))
        .unwrap_or(file_name)
}

/// What [`identify_name`] concluded about a name.
#[derive(Debug, Clone)]
pub enum Identification {
    /// TMDB matched the name to this title.
    Identified(MediaMetadata),
    /// Every TMDB search answered and nothing matched (or the name has nothing to search for).
    NoMatch,
    /// A TMDB search failed (an outage that outlasted the client's retry budget, or a 4xx such as
    /// a rejected API key), whether or not something matched. The searches that failed, or the
    /// fallback stages the failure cut short, might have held the real match, so a pick from the
    /// rest is no verdict on the name: a caller must neither cache the fallback for good nor reject
    /// the release on it.
    Unavailable,
}

/// Identify a torrent by the names [`identification_names`] lists, in order: its largest selected
/// video file's name when that names a title, else the nearest titled folder or the torrent name
/// ([`identification_name`]); then the torrent name; then the file name (unselected files are
/// ignored here and in the show guess, see [`served_files`]). Falls back to a title cleaned from
/// the names (`external_id: None`) when none matches. Once a search comes back
/// [`Identification::Unavailable`] the remaining names are skipped: each would spend another full
/// retry cycle (~2.5 min) on the same outage. A caller that must tell such an outage fallback from
/// a genuine no-match wraps this in [`crate::tmdb_client::track_search_failures`].
pub async fn identify_torrent(info: &rd_client::TorrentInfo, tmdb: &TmdbClient) -> MediaMetadata {
    let representative_name = representative_name(info);

    // Set once a search comes back `Unavailable`: the fallback below is then provisional (for want
    // of an answer, not because the torrent is unidentifiable).
    let mut provisional = false;
    for (i, name) in identification_names(info).into_iter().enumerate() {
        if i > 0 {
            debug!(
                "No match found for torrent {}. Trying '{}'",
                info.filename, name
            );
        }
        match identify_name(name, &info.files, tmdb).await {
            Identification::Identified(metadata) => return metadata,
            Identification::Unavailable => {
                provisional = true;
                break;
            }
            Identification::NoMatch => {}
        }
    }

    if provisional {
        warn!(
            "TMDB unavailable; could not identify torrent: {}. Filename: {}. Using a provisional fallback title.",
            info.filename, representative_name
        );
    } else {
        warn!(
            "Could not identify torrent: {}. Filename: {}.",
            info.filename, representative_name
        );
    }
    fallback_metadata(info)
}

/// The files a torrent is identified by: the selected ones, or every file when none is selected
/// (a Real-Debrid torrent still waiting for its file selection). Real-Debrid lists unselected files
/// too, and the VFS never shows one, so a partly selected collection is not named after a film it
/// doesn't serve. TorBox marks every file selected.
fn served_files(
    files: &[rd_client::TorrentFile],
) -> impl Iterator<Item = &rd_client::TorrentFile> + '_ {
    let any_selected = files.iter().any(|f| f.selected != 0);
    files
        .iter()
        .filter(move |f| !any_selected || f.selected != 0)
}

/// The file a torrent is named after: its largest selected video file (see [`served_files`]).
fn representative_file(info: &rd_client::TorrentInfo) -> Option<&rd_client::TorrentFile> {
    served_files(&info.files)
        .filter(|f| is_video_file(&f.path))
        .max_by_key(|f| f.bytes)
}

/// The name of a torrent's [`representative_file`] (without the folder), else the torrent name:
/// what [`fallback_metadata`] cleans its title from, beside the torrent name.
fn representative_name(info: &rd_client::TorrentInfo) -> &str {
    representative_file(info)
        .map(|f| f.path.split('/').next_back().unwrap_or(&f.path))
        .unwrap_or(&info.filename)
}

/// The names [`identify_torrent`] searches, in order and each once: the
/// [`identification_name`] of its [`representative_file`] (the file name when it names a title,
/// else the nearest titled folder, else the torrent name), the torrent name (its `filename`, else
/// its `original_filename`), and the file name as a last resort. A file name that names no title
/// (`S04E09 - The Reunion.mkv`) is searched last, since what `clean_name` leaves of it is the
/// episode title, which TMDB's fuzzy search matches to some other title. A torrent with no video
/// file is searched by its name alone.
fn identification_names(info: &rd_client::TorrentInfo) -> Vec<&str> {
    let torrent_name = if info.filename.is_empty() {
        info.original_filename.as_str()
    } else {
        info.filename.as_str()
    };
    let candidates = match representative_file(info) {
        Some(file) => {
            let file_name = file.path.split('/').next_back().unwrap_or(&file.path);
            vec![
                identification_name(&file.path, torrent_name),
                torrent_name,
                file_name,
            ]
        }
        None => vec![torrent_name],
    };
    let mut names: Vec<&str> = Vec::with_capacity(candidates.len());
    for name in candidates {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The TMDB-free identification [`identify_torrent`] falls back to when nothing matched: the
/// [`fallback_title`] of the torrent and representative file names, a movie or show by its file
/// structure, and no `external_id`. A caller showing a torrent it could not ask TMDB about yet
/// uses it too, so the title doesn't change when TMDB later answers with no match.
pub fn fallback_metadata(info: &rd_client::TorrentInfo) -> MediaMetadata {
    let representative_name = representative_name(info);
    let (title, year) = fallback_title(
        clean_name(&info.filename),
        clean_name(representative_name),
        representative_name,
    );
    MediaMetadata {
        title,
        year,
        media_type: if is_show_guess(&info.files) {
            MediaType::Show
        } else {
            MediaType::Movie
        },
        external_id: None,
    }
}

/// Last-resort title when neither the file nor the torrent name matched TMDB. Prefer the first
/// cleaned candidate (torrent name, then representative file name) that is neither empty NOR
/// GENERIC: a bare "Season 1"/"Episode" is a junk library entry — worse than the raw filename it
/// was cleaned from — so a generic candidate is skipped in favour of the next source, and the raw
/// file name is used only when every cleaned candidate is empty or generic. Each candidate keeps
/// its own paired year token.
fn fallback_title(
    cleaned_torrent: (String, Option<String>),
    cleaned_file: (String, Option<String>),
    raw_file: &str,
) -> (String, Option<String>) {
    let (torrent_title, torrent_year) = cleaned_torrent;
    if !torrent_title.is_empty() && !is_generic_title(&torrent_title) {
        return (torrent_title, torrent_year);
    }
    let (file_title, file_year) = cleaned_file;
    if !file_title.is_empty() && !is_generic_title(&file_title) {
        return (file_title, file_year);
    }
    (raw_file.to_string(), file_year)
}

fn best_scored_result<'a>(
    results: &'a [TmdbSearchResult],
    normalized_query: &str,
    year: &Option<String>,
    is_short_title: bool,
) -> Option<&'a TmdbSearchResult> {
    if results.is_empty() {
        return None;
    }
    if is_short_title {
        let candidates = results.iter().filter(|r| {
            // Match against BOTH the (possibly localized) `title` and the `original_title`, mirroring
            // `score_result`/`is_exact` — a short foreign title whose TMDB `title` is translated but
            // whose `original_title` is the native short form would otherwise be wrongly dropped.
            let title_matches = normalize_title(&r.title) == normalized_query
                || r.original_title
                    .as_deref()
                    .map(|ot| normalize_title(ot) == normalized_query)
                    .unwrap_or(false);
            let year_matches = year
                .as_ref()
                .map(|y| {
                    r.release_date
                        .as_ref()
                        .map(|rd| rd.starts_with(y))
                        .unwrap_or(false)
                })
                .unwrap_or(true); // No year in filename → exact title alone is sufficient
            title_matches && year_matches
        });
        pick_highest_scored(candidates, normalized_query, year)
    } else {
        pick_highest_scored(results.iter(), normalized_query, year)
    }
}

/// Pick the highest-scoring result, computing each candidate's score exactly once (the
/// previous `max_by(|a, b| score(a).cmp(score(b)))` recomputed and re-allocated per
/// comparison). Ties resolve to the last candidate, matching `Iterator::max_by`.
fn pick_highest_scored<'a>(
    candidates: impl Iterator<Item = &'a TmdbSearchResult>,
    normalized_query: &str,
    year: &Option<String>,
) -> Option<&'a TmdbSearchResult> {
    // Compute "now" once per search rather than once per candidate (it was a syscall inside the
    // per-result scorer's recency bonus).
    let current_year = chrono::Utc::now().year();
    candidates
        .map(|r| (score_result(r, normalized_query, year, current_year), r))
        .max_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, r)| r)
}

fn select_best_match(
    tv: Option<&TmdbSearchResult>,
    movie: Option<&TmdbSearchResult>,
    normalized_query: &str,
    year: &Option<String>,
    is_show_guess: bool,
) -> Option<(String, Option<String>, String, MediaType)> {
    let is_exact = |r: &TmdbSearchResult| -> bool {
        normalize_title(&r.title) == normalized_query
            || r.original_title
                .as_ref()
                .map(|t| normalize_title(t) == normalized_query)
                .unwrap_or(false)
    };
    let has_year_match = |r: &TmdbSearchResult| -> bool {
        year.as_ref()
            .map(|y| {
                r.release_date
                    .as_ref()
                    .map(|rd| rd.starts_with(y))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    };

    match (tv, movie) {
        (Some(tv), Some(movie)) => {
            let tv_exact = is_exact(tv);
            let movie_exact = is_exact(movie);
            let tv_year_match = has_year_match(tv);
            let movie_year_match = has_year_match(movie);
            let tv_exact_year = tv_exact && tv_year_match;
            let movie_exact_year = movie_exact && movie_year_match;

            let (selected, media_type) = if tv_exact_year && !movie_exact_year {
                (tv, MediaType::Show)
            } else if movie_exact_year && !tv_exact_year {
                (movie, MediaType::Movie)
            } else if is_show_guess && tv_year_match {
                (tv, MediaType::Show)
            } else if !is_show_guess && movie_year_match {
                (movie, MediaType::Movie)
            } else if tv_exact && !movie_exact {
                (tv, MediaType::Show)
            } else if movie_exact && !tv_exact && !is_show_guess {
                // A bare exact MOVIE-title match only wins here when the file does NOT carry an
                // episode marker. With `is_show_guess` (an SxxExx marker present), a decorated TV
                // title ("Show (US)" → not exact) must not be overridden by an exact same-named movie
                // — the episode marker is the stronger signal, so fall through to the `is_show_guess`
                // Show branch below.
                (movie, MediaType::Movie)
            } else if tv_year_match && !movie_year_match {
                (tv, MediaType::Show)
            } else if movie_year_match && !tv_year_match {
                (movie, MediaType::Movie)
            } else if is_show_guess {
                (tv, MediaType::Show)
            } else {
                (movie, MediaType::Movie)
            };
            Some((
                selected.title.clone(),
                selected.release_date.clone(),
                selected.id.to_string(),
                media_type,
            ))
        }
        (Some(tv), None) => Some((
            tv.title.clone(),
            tv.release_date.clone(),
            tv.id.to_string(),
            MediaType::Show,
        )),
        (None, Some(movie)) => Some((
            movie.title.clone(),
            movie.release_date.clone(),
            movie.id.to_string(),
            MediaType::Movie,
        )),
        (None, None) => None,
    }
}

/// Score a search result based on how well it matches the query
/// Higher score = better match
fn score_result(
    result: &TmdbSearchResult,
    normalized_query: &str,
    year: &Option<String>,
    current_year: i32,
) -> f64 {
    let mut score = 0.0;

    // Title match (most important)
    let normalized_title = normalize_title(&result.title);
    let normalized_original = result.original_title.as_ref().map(|t| normalize_title(t));

    if normalized_title == normalized_query
        || normalized_original
            .as_ref()
            .map(|t| t == normalized_query)
            .unwrap_or(false)
    {
        score += 1000.0; // Exact title match
    } else if normalized_title.contains(normalized_query)
        || normalized_original
            .as_ref()
            .map(|t| t.contains(normalized_query))
            .unwrap_or(false)
    {
        score += 100.0; // Partial title match
    }

    // Vote count and rating — smooth scaling to avoid cliffs between similarly-popular content
    let vote_count = result.vote_count.unwrap_or(0);
    if vote_count >= 10 {
        let vc = vote_count as f64;
        // Weight ramps smoothly: 5.0 at 10 votes → 10.0 at 100 → 15.0 at 1000+
        let weight = (vc.log10() * 5.0).min(15.0);
        score += result.vote_average.unwrap_or(0.0) * weight;
        score += vc.log10() * 30.0;
    }

    // Year match (important, but with tolerance for off-by-one errors)
    if let Some(y) = year {
        if let Some(release_date) = &result.release_date {
            if let Ok(query_year) = y.parse::<i32>() {
                if let Some(release_year_str) = release_date.get(0..4) {
                    if let Ok(release_year) = release_year_str.parse::<i32>() {
                        let year_diff = (query_year - release_year).abs();
                        if year_diff == 0 {
                            score += 200.0; // Exact year match
                        } else if year_diff == 1 {
                            score += 150.0; // Close year match (±1 year)
                        }
                    }
                }
            }
        }
    }

    // Recency bonus when no year is specified in the filename.
    // Helps disambiguate same-titled content (e.g., two shows both called "Sherwood")
    // by slightly preferring more recently released content.
    if year.is_none() {
        if let Some(release_date) = &result.release_date {
            if let Some(year_str) = release_date.get(0..4) {
                if let Ok(release_year) = year_str.parse::<i32>() {
                    // Skip the bonus for a future-dated (unaired) release rather than clamping its
                    // age to 0 and awarding the full +80 — an unaired title must not out-rank an
                    // already-released one during disambiguation.
                    if release_year <= current_year {
                        let age = (current_year - release_year) as f64;
                        score += (80.0 - age * 8.0).max(0.0);
                    }
                }
            }
        }
    }

    // Popularity (minor tiebreaker). CAP it: raw TMDB popularity reaches the hundreds–thousands for
    // trending titles, which — unbounded — could outweigh the exact-vs-partial title gap (900) and let
    // a trending substring match beat an obscure exact match on a year-less filename. A small cap
    // keeps it a genuine tiebreaker among otherwise-comparable candidates.
    score += result.popularity.min(50.0);

    score
}

/// Identify one name (a file or torrent name) via TMDB, running the fallback ladder (CamelCase
/// split, leading-word strip, after-dash, no-year) while searches come back empty. Returns
/// [`Identification::Unavailable`] whenever a search failed (matched or not), and stops issuing
/// further searches once one has failed.
pub async fn identify_name(
    name: &str,
    files: &[rd_client::TorrentFile],
    tmdb: &TmdbClient,
) -> Identification {
    identify_name_with_show_hint(name, files, tmdb, false).await
}

/// [`identify_name`], with `show_hint` set when the caller already knows the name is a show's: the
/// acquisition title check of a show request, whose file may name its episode by a form the show
/// guess doesn't count (`Some Show Episode 12.mkv`: a bare `Episode N` names films too). The hint
/// only settles the show-versus-film choice [`is_show_guess`] makes; the caller still requires the
/// requested TMDB id, media type and episode. Movie identification and the account mirror pass no
/// hint.
pub async fn identify_name_with_show_hint(
    name: &str,
    files: &[rd_client::TorrentFile],
    tmdb: &TmdbClient,
    show_hint: bool,
) -> Identification {
    let (cleaned_name, year) = clean_name(name);
    if cleaned_name.is_empty() || is_generic_title_given_year(&cleaned_name, &year) {
        return Identification::NoMatch;
    }
    let is_show_guess = show_hint || is_show_guess(files);

    let normalized_cleaned = normalize_title(&cleaned_name);
    // A cleaned name that survives the empty/generic guard but normalizes to empty — e.g. an
    // entirely-punctuation title like "()" (kept non-empty by `clean_name`, emptied by
    // `normalize_title`, which keeps only alphanumerics) — has nothing to match on. Left to fall
    // through, `score_result`'s `normalized_title.contains(normalized_query)` would be TRUE for
    // every TMDB candidate (an empty needle matches anything), so selection would collapse to
    // popularity/recency and could confidently mis-identify the title. Bail instead.
    if normalized_cleaned.is_empty() {
        return Identification::NoMatch;
    }

    // Try TMDB first. Track whether the request itself FAILED (a TMDB outage) as distinct from a
    // genuine empty result: each fallback stage below re-issues a full retry cycle (~2.5 min), so
    // during an outage a single unidentifiable torrent could otherwise burn ~10 min of doomed
    // retries. `tmdb_unavailable` short-circuits every remaining stage once any search has failed.
    let mut tmdb_unavailable = false;
    let (tv_out, movie_out) = tokio::join!(
        tmdb.search_tv(&cleaned_name, year.as_deref()),
        tmdb.search_movie(&cleaned_name, year.as_deref())
    );
    let mut tv_results = tv_out.into_results(&mut tmdb_unavailable);
    let mut movie_results = movie_out.into_results(&mut tmdb_unavailable);

    // If no results found, try CamelCase splitting as a fallback
    if !tmdb_unavailable && tv_results.is_empty() && movie_results.is_empty() {
        let split_name = CAMEL_RE.replace_all(&cleaned_name, "$1 $2").to_string();
        if split_name != cleaned_name {
            debug!(
                "No results for '{}', trying CamelCase split: '{}'",
                cleaned_name, split_name
            );
            let (tv_extra, movie_extra) = tokio::join!(
                tmdb.search_tv(&split_name, year.as_deref()),
                tmdb.search_movie(&split_name, year.as_deref())
            );
            tv_results.extend(tv_extra.into_results(&mut tmdb_unavailable));
            movie_results.extend(movie_extra.into_results(&mut tmdb_unavailable));
        }
    }

    // If still no results, try stripping leading words (handles franchise prefixes like "Bond 50
    // Goldfinger"). Cap the strip depth at the first few words: a real franchise/garbage prefix is
    // short, while deep strips of a long title both waste rate-limited TMDB calls (2 per attempt) and
    // risk matching an unrelated short title from the title's tail.
    if !tmdb_unavailable && tv_results.is_empty() && movie_results.is_empty() {
        const MAX_STRIP_PREFIX_WORDS: usize = 3;
        let words: Vec<&str> = cleaned_name.split_whitespace().collect();
        let max_start = words.len().min(MAX_STRIP_PREFIX_WORDS + 1);
        for start in 1..max_start {
            let stripped = words[start..].join(" ");
            if stripped.is_empty() || is_generic_title(&stripped) {
                continue;
            }
            debug!(
                "No results for '{}', trying stripped: '{}'",
                cleaned_name, stripped
            );
            let (tv_extra, movie_extra) = tokio::join!(
                tmdb.search_tv(&stripped, year.as_deref()),
                tmdb.search_movie(&stripped, year.as_deref())
            );
            let tv_extra = tv_extra.into_results(&mut tmdb_unavailable);
            let movie_extra = movie_extra.into_results(&mut tmdb_unavailable);
            if !tv_extra.is_empty() || !movie_extra.is_empty() {
                tv_results.extend(tv_extra);
                movie_results.extend(movie_extra);
                break;
            }
            if tmdb_unavailable {
                break; // outage — further strips will just re-fail
            }
        }
    }

    // If still no results and name contains a dash, try the part after the first dash
    // (handles release group prefixes like "d3us-Title" or "m-Title")
    if !tmdb_unavailable && tv_results.is_empty() && movie_results.is_empty() {
        if let Some(pos) = cleaned_name.find('-') {
            let after_dash = cleaned_name[pos + 1..].trim();
            if !after_dash.is_empty() && !is_generic_title(after_dash) {
                debug!(
                    "No results for '{}', trying after dash: '{}'",
                    cleaned_name, after_dash
                );
                let (tv_extra, movie_extra) = tokio::join!(
                    tmdb.search_tv(after_dash, year.as_deref()),
                    tmdb.search_movie(after_dash, year.as_deref())
                );
                tv_results.extend(tv_extra.into_results(&mut tmdb_unavailable));
                movie_results.extend(movie_extra.into_results(&mut tmdb_unavailable));
            }
        }
    }

    // Helper to check if results contain exact title match
    let has_exact_match = |results: &[TmdbSearchResult]| -> bool {
        results.iter().any(|r| {
            normalize_title(&r.title) == normalized_cleaned
                || r.original_title
                    .as_ref()
                    .map(|t| normalize_title(t) == normalized_cleaned)
                    .unwrap_or(false)
        })
    };

    let tv_has_exact = has_exact_match(&tv_results);
    let movie_has_exact = has_exact_match(&movie_results);

    // If we have a year but no exact title match yet, try searching without the year
    if !tmdb_unavailable && !tv_has_exact && !movie_has_exact && year.is_some() {
        let (tv_no_year, movie_no_year) = tokio::join!(
            tmdb.search_tv(&cleaned_name, None),
            tmdb.search_movie(&cleaned_name, None)
        );
        tv_results.extend(tv_no_year.into_results(&mut tmdb_unavailable));
        movie_results.extend(movie_no_year.into_results(&mut tmdb_unavailable));
    }

    // A failed search is no verdict, even when the rest matched something: with one half of a
    // joined search missing, the other half's best result can be an unrelated title (a show file
    // picked up as a movie). So don't make a pick. Past this point every search answered.
    if tmdb_unavailable {
        debug!(
            "A TMDB search failed while identifying '{}'; no verdict",
            cleaned_name
        );
        return Identification::Unavailable;
    }

    // Score all results and pick the best TV and movie matches.
    // For short titles (≤3 chars), require an exact title match — plus a year match only when the
    // filename actually carries a year (a short title with no year still matches on exact title).
    let is_short_title = is_short_title(&cleaned_name);

    let best_tv = best_scored_result(&tv_results, &normalized_cleaned, &year, is_short_title);
    let best_movie = best_scored_result(&movie_results, &normalized_cleaned, &year, is_short_title);

    let selected = select_best_match(
        best_tv,
        best_movie,
        &normalized_cleaned,
        &year,
        is_show_guess,
    );

    if let Some((title, release_date, id, mtype)) = selected {
        let year_val = release_date.and_then(|d| {
            let digits: String = d.chars().filter(|c| c.is_ascii_digit()).take(4).collect();
            // Mirror `parse_details`: only a full 4-digit year is meaningful. An empty
            // `release_date` (common for shows with no first_air_date) or a malformed short one
            // must map to None, not Some("")/Some("201") leaking into the VFS.
            (digits.len() == 4).then_some(digits)
        });
        info!(
            "Identified {} ({:?}) as {:?} via TMDB (ID: {})",
            title, year_val, mtype, id
        );
        return Identification::Identified(MediaMetadata {
            title,
            year: year_val,
            media_type: mtype,
            external_id: Some(format!("tmdb:{}", id)),
        });
    }

    Identification::NoMatch
}

/// A title is "short" (≤3 characters) if it has at most 3 Unicode scalar values.
/// Counting characters rather than bytes keeps the safeguard consistent for
/// non-ASCII titles (e.g. a 2-character CJK title is 6 bytes but still short).
fn is_short_title(name: &str) -> bool {
    name.chars().count() <= 3
}

/// Case-insensitive search for an ASCII `needle` in `haystack`, returning
/// the byte offset in `haystack` (safe for slicing). This avoids the
/// `to_lowercase()` byte-offset mismatch that can cause panics with
/// multi-byte UTF-8 characters.
fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    debug_assert!(needle.is_ascii(), "needle must be ASCII");
    let needle_len = needle.len();
    if haystack.len() < needle_len {
        return None;
    }
    let needle_bytes: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
    // Walk byte-by-byte; since needle is all ASCII, a match means all bytes
    // in the range are ASCII too, so `i` and `i + needle_len` are both valid
    // char boundaries.
    for i in 0..=(haystack.len() - needle_len) {
        let matches = haystack.as_bytes()[i..i + needle_len]
            .iter()
            .zip(&needle_bytes)
            .all(|(h, n)| h.to_ascii_lowercase() == *n);
        if matches {
            return Some(i);
        }
    }
    None
}

pub fn clean_name(name: &str) -> (String, Option<String>) {
    let mut title = name.to_string();

    // 0. Remove file extension if present
    if let Some(pos) = title.rfind('.') {
        let ext = &title[pos..].to_lowercase();
        if VIDEO_EXTENSIONS.iter().any(|e| *e == ext) {
            title.truncate(pos);
        }
    }

    // 1. Remove common site prefixes and garbage at the start
    // Patterns like "[ site ] ", "( site )", "www.site.org - ", "d3us-", "m-", "Bond.50" (a
    // dotted name before " - " that isn't a site is kept, see `site_match_is_a_site`).
    // EXCEPT a leading "[Title] (YYYY)" whose brackets are part of the title (e.g. "[REC] (2007)").
    // Loop so a RUN of consecutive leading tags ("[Group][BD] Title") is fully stripped — a single
    // pass removes only the first group and would leave the next tag's label glued on
    // ("[Judas][BD] …" -> "BD] …"). Stripping the PREFIX_RE match (incl. its trailing \s*) each
    // iteration keeps the next tag's opening bracket intact so it is recognised again.
    let mut stripped_any_prefix = false;
    while !TITLE_BRACKET_YEAR_RE.is_match(&title) {
        let Some(caps) = PREFIX_RE.captures(&title) else {
            break;
        };
        let end = caps.get(0).map_or(0, |m| m.end());
        // A dotted show, film or release name is title content, not a site (see PREFIX_RE).
        if let Some(site) = caps.name("site") {
            if !site_match_is_a_site(site.as_str(), &title[end..]) {
                break;
            }
        }
        title = title[end..].to_string();
        stripped_any_prefix = true;
    }
    // Remove any leftover leading non-alphanumeric run, but only if we stripped a prefix AND we did
    // not stop on a "[Title] (YYYY)" (whose leading '[' is title content, not junk).
    if stripped_any_prefix && !TITLE_BRACKET_YEAR_RE.is_match(&title) {
        title = title
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .to_string();
    }

    // 2. Initial cleanup: replace dots and underscores with spaces
    title = title.replace(['.', '_'], " ");

    // 3. Handle "aka" - usually "title aka alternative title"; keep the part after " aka ".
    // `find_case_insensitive` scans bytes with ASCII case-folding and returns a byte offset
    // that is always on a UTF-8 boundary (the needle is pure ASCII), so the slice below is safe.
    if let Some(aka_pos) = find_case_insensitive(&title, " aka ") {
        let after_aka = &title[aka_pos + 5..];
        if !after_aka.trim().is_empty() {
            title = after_aka.to_string();
        }
    }

    // 3b. Truncate at a bare " - NNN - " episode-number separator. Otherwise the episode
    // title leaks into the TMDB query: the show search returns nothing and a fallback can
    // match an unrelated title (`Some Show - 409 - Episode Title` landing on another show).
    if let Some(m) = EPISODE_NUM_DASH_RE.find(&title) {
        if m.start() > 0 {
            title.truncate(m.start());
        }
    }
    // Likewise at a ` - NN` episode number that no second ` - ` follows (`Some Show - 12 (1080p)`):
    // left in, TMDB finds nothing for the whole query, and the leading-word strip can end up
    // searching the number alone. A year there is the release year (`Dr.No - 1962`), kept for
    // step 4, and a number followed by a bracketed year is a numeric film title (`21 (2008)`,
    // e.g. behind a kept site prefix), kept with its year.
    if let Some((start, _)) = trailing_episode_cut(&title).filter(|&(start, _)| start > 0) {
        title.truncate(start);
    }

    // 4. Find year (19xx or 20xx). Use the LAST year token: scene names put the title first and the
    //    release year last, so for a title that itself contains a year ("Blade Runner 2049", "1917",
    //    "2012") the FIRST token is the title's number, not the release year — using it picks the
    //    wrong disambiguator and the wrong TMDB id.
    let year = YEAR_RE
        .find_iter(&title)
        .last()
        .map(|m| m.as_str().to_string());

    // 4b. Truncate at an `NxM` episode code (1x01), as step 5 does at an SxxEyy one, so the episode
    //     title after it never reaches the TMDB query (where the leading-word strip could match it
    //     alone). Not at position 0: a title that IS an `NxM` token ("10x10") is kept. After step 4,
    //     so a year after the code is still found, as for SxxEyy. `NXM_RE` never matches a
    //     resolution (1920x1080) or an aspect ratio (16x9).
    if let Some(m) = NXM_RE.find(&title) {
        if m.start() > 0 {
            title.truncate(m.start());
        }
    }

    // 5. Handle stop words (technical metadata, quality, codecs, season info)
    let title_before_stopwords = title.clone();
    while let Some(m) = STOP_RE.find(&title) {
        if m.start() == 0 {
            // Metadata at start, strip it
            title = title[m.end()..].to_string();
            title = title
                .trim_start_matches(|c: char| !c.is_alphanumeric())
                .to_string();
            if title.is_empty() {
                break;
            }
        } else {
            // Metadata in middle/end, truncate
            title.truncate(m.start());
            break;
        }
    }
    // If stripping consumed the whole title, the leading "stop word" was actually the title
    // itself (e.g. a film literally named after a metadata token like "Remux"). Restore it rather
    // than emit an empty title that cannot be searched.
    if title.is_empty() {
        title = title_before_stopwords;
    }

    // 6. Truncate at year if it appears in title (and is not at the very start). Use the LAST year
    //    token (the release year) so a title-embedded year ("Blade Runner 2049") is kept and only the
    //    trailing release year is dropped, rather than truncating at the title's own number.
    if let Some(m) = YEAR_RE.find_iter(&title).last() {
        if m.start() > 0 {
            // Only skip truncation when THIS matched (trailing) year is itself part of a
            // range, e.g. "1985-1999" / "1985 1999" — keeping a date-range title intact. A
            // range elsewhere in the title (e.g. "WWII in Color 1939-1945 2009") must NOT
            // disable dropping the separate trailing release year, so we check the matched
            // year's span against the range matches rather than testing the whole title.
            let in_range = YEAR_RANGE_RE.find_iter(&title).any(|r| {
                // The matched (trailing) year must fall inside this range match...
                if !(r.start() <= m.start() && m.end() <= r.end()) {
                    return false;
                }
                // ...AND the pair must be DASH-joined to be a real date range ("1939-1945"). Two
                // SPACE-separated standalone years are a title-year + release-year pair, NOT a
                // range — even when ASCENDING: a film whose title IS an early year with a LATER
                // release year ("1917 2019", "1941 1979") is ascending too, yet its trailing
                // release year must still be dropped or it pollutes the TMDB query and loses the
                // exact-title score boost. (A descending space pair like "Blade Runner 2049 2017"
                // was already handled; requiring a dash also covers the ascending case.)
                if !r.as_str().contains('-') {
                    return false;
                }
                let years: Vec<i32> = YEAR_RE
                    .find_iter(r.as_str())
                    .filter_map(|y| y.as_str().parse().ok())
                    .collect();
                matches!(years.as_slice(), [a, b] if a < b)
            });
            if !in_range {
                title.truncate(m.start());
            }
        }
    }

    // 7. Final cleanup: remove trailing non-alphanumeric characters and trim
    title = title
        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != ')' && c != ']')
        .to_string();

    (title.trim().to_string(), year)
}

pub fn is_show_guess(files: &[rd_client::TorrentFile]) -> bool {
    // An episode marker (SxxExx, NxM, "Season N", a trailing ` - NN`, …) in any served file name →
    // definitely a show.
    // An unselected file is never shown, so its marker doesn't count (see `served_files`).
    if served_files(files).any(|f| {
        let filename = f.path.split('/').next_back().unwrap_or(&f.path);
        filename_has_show_marker(filename)
    }) {
        return true;
    }
    // Fallback: more than one SUBSTANTIAL selected video file → a show (or multi-episode pack).
    // The size floor is essential because TorBox marks EVERY file `selected` (torbox_client maps
    // `selected: 1` for all files), so a movie that ships a small sample/featurette as a second
    // video file would otherwise be miscounted as a show. 100 MB excludes samples/extras while
    // keeping real episodes (even short-form ones).
    const MIN_SUBSTANTIAL_VIDEO_BYTES: u64 = 100_000_000;
    files
        .iter()
        .filter(|f| {
            f.selected != 0 && is_video_file(&f.path) && f.bytes >= MIN_SUBSTANTIAL_VIDEO_BYTES
        })
        .count()
        > 1
}

pub fn normalize_title(s: &str) -> String {
    // Standardize 'and' to '&' for comparison.
    let lowered = s.to_lowercase().replace(" and ", " & ");
    // Fold accents/ligatures to their ASCII base, then keep only alphanumerics (spaces/punctuation/
    // the '&' connector are dropped). A manual push loop (rather than `.map().filter()`) so a single
    // source char can expand to MULTIPLE ASCII chars — `æ`→`ae`, `œ`→`oe`, `ß`→`ss`, `þ`→`th` — which
    // a 1:1 `char` map cannot express (e.g. `Æon Flux` → `aeonflux` now matches a filename's `aeonflux`).
    let mut out = String::with_capacity(lowered.len());
    for c in lowered.chars() {
        match c {
            // Single-base folds. Each arm lists the common Latin diacritic variants (acute/grave/
            // circumflex/umlaut/ring/macron/breve/ogonek/caron/dot/stroke) that fold to one ASCII
            // letter, so a TMDB title and a filename that spell the same word with/without accents
            // normalise identically. Multi-char folds (æ/œ/ß/þ) are handled below.
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => out.push('e'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ĭ' | 'į' | 'ı' => out.push('i'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => out.push('o'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'ñ' | 'ń' | 'ň' | 'ņ' => out.push('n'),
            'ç' | 'č' | 'ć' | 'ĉ' | 'ċ' => out.push('c'),
            'š' | 'ś' | 'ŝ' | 'ş' => out.push('s'),
            'ž' | 'ź' | 'ż' => out.push('z'),
            'ł' | 'ĺ' | 'ľ' | 'ļ' => out.push('l'),
            'ð' | 'đ' | 'ď' => out.push('d'),
            'ř' | 'ŕ' => out.push('r'),
            'ť' | 'ţ' | 'ŧ' => out.push('t'),
            'ğ' | 'ĝ' | 'ġ' | 'ģ' => out.push('g'),
            'þ' => out.push_str("th"),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'ß' => out.push_str("ss"),
            c if c.is_alphanumeric() => out.push(c),
            _ => {} // drop non-alphanumeric (spaces, punctuation, the '&' connector)
        }
    }
    out
}

/// [`is_generic_title`] for a name cleaned along with its release `year`: a short number that came
/// with a year is a film title (`9` from 2009, `1` from 2013), not a numbered file such as `1.mkv`.
/// Searched with the year, the short-title rule ([`is_short_title`]) accepts only an exact title
/// released that year. Only the name as a whole gets this: the fallback stages never search a
/// number on its own (a sequel digit the leading-word strip leaves).
fn is_generic_title_given_year(s: &str, year: &Option<String>) -> bool {
    if year.is_some() && s.len() < 5 && s.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    is_generic_title(s)
}

fn is_generic_title(s: &str) -> bool {
    let lower = s.to_lowercase();
    if lower.is_empty() {
        return true;
    }

    // Check if it's just a sequence of 5 or more digits (like BDMV 00000.m2ts)
    // or if it's a very small number like 0, 1, 2
    if s.chars().all(|c| c.is_ascii_digit()) {
        if s.len() >= 5 {
            return true;
        }
        if let Ok(n) = s.parse::<u32>() {
            if n < 10 {
                return true;
            }
        }
    }

    // Generic terms that might have survived cleaning
    GENERIC_RE.is_match(&lower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rd_client::{TorrentFile, TorrentInfo};

    #[test]
    fn normalize_title_folds_accents_and_unifies_and_with_ampersand() {
        // Accent folding + non-alphanumeric stripping (spaces/punctuation removed).
        assert_eq!(normalize_title("Pokémon"), "pokemon");
        assert_eq!(normalize_title("Amélie"), "amelie");
        assert_eq!(normalize_title("WALL·E (2008)"), "walle2008");
        assert_eq!(normalize_title("Coraline & Co."), "coralineco");
        // " and " is rewritten to " & " BEFORE alphanumeric filtering drops the connector, so the
        // "and" and "&" spellings normalise to the SAME string (the point of the rule — without it,
        // the "and" letters would survive and the two forms wouldn't match).
        assert_eq!(
            normalize_title("Tom and Jerry"),
            normalize_title("Tom & Jerry")
        );
        assert_eq!(normalize_title("Tom and Jerry"), "tomjerry");
    }

    #[test]
    fn normalize_title_folds_ligatures_and_nordic_letters() {
        // Multi-char ligatures/letters fold to their ASCII base so a title matches a filename's
        // ASCII spelling (which a 1:1 char map couldn't do).
        assert_eq!(normalize_title("Æon Flux"), "aeonflux");
        assert_eq!(normalize_title("Cœur"), "coeur");
        assert_eq!(normalize_title("Straße"), "strasse");
        assert_eq!(normalize_title("Þór"), "thor");
        assert_eq!(normalize_title("Øystein"), "oystein");
        assert_eq!(normalize_title("Škoda"), "skoda");
        assert_eq!(normalize_title("Æon Flux"), normalize_title("Aeon Flux"));
    }

    #[test]
    fn normalize_title_folds_extended_diacritics_consistently() {
        // Macrons, dotted/stroked and Central-European diacritics must fold to their ASCII base too,
        // so a TMDB title and a filename spelling the same word ASCII-only normalise identically.
        assert_eq!(normalize_title("Tōkyō"), normalize_title("Tokyo")); // macrons ō→o
        assert_eq!(normalize_title("Łódź"), normalize_title("Lodz")); // ł→l, ó→o, ź→z
        assert_eq!(normalize_title("Dvořák"), normalize_title("Dvorak")); // ř→r, á→a
        assert_eq!(normalize_title("Gdańsk"), normalize_title("Gdansk")); // ń→n
        assert_eq!(normalize_title("Wałęsa"), normalize_title("Walesa")); // ł→l, ę→e
        assert_eq!(normalize_title("Međimurje"), normalize_title("Medimurje")); // đ→d
    }

    #[tokio::test]
    async fn identify_name_bails_on_a_title_that_normalizes_to_empty() {
        // "()" survives `clean_name` (stays non-empty) and is not generic, but `normalize_title`
        // keeps only alphanumerics so it collapses to "". Such a title must return NoMatch rather
        // than fall through to universal partial-match scoring (an empty needle matches every
        // candidate), which would confidently mis-identify it. The guard returns before any TMDB
        // call: the loopback server must see no request.
        let (cleaned, _year) = clean_name("()");
        assert!(
            !cleaned.is_empty() && normalize_title(&cleaned).is_empty(),
            "precondition: '()' must clean non-empty yet normalize to empty (got cleaned={cleaned:?})"
        );
        let server = crate::trakt_client::test_http::spawn(Vec::new()).await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        assert!(
            matches!(
                identify_name("()", &[], &tmdb).await,
                Identification::NoMatch
            ),
            "a title that normalizes to empty must not be identified"
        );
        assert_eq!(server.requests(), Vec::<String>::new(), "no TMDB request");
    }

    /// The unmatched half of the verdicts; `a_match_picked_after_a_failed_search_is_no_verdict`
    /// covers a match with and without a failed search.
    #[tokio::test]
    async fn a_failed_tmdb_search_is_unavailable_matched_or_not() {
        use crate::trakt_client::test_http::{spawn, Reply};
        // Nothing matched and every search answered: a genuine "no such title".
        let empty = spawn(vec![
            (
                "/search/movie?api_key=k&query=Zzqx",
                Reply::json(r#"{"results":[]}"#),
            ),
            (
                "/search/tv?api_key=k&query=Zzqx",
                Reply::json(r#"{"results":[]}"#),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), empty.base_url.clone()).unwrap();
        assert!(matches!(
            identify_name("Zzqx.mkv", &[], &tmdb).await,
            Identification::NoMatch
        ));
        // Nothing matched but the searches failed (here a 404, as for a rejected key): no verdict,
        // so the caller can retry later instead of caching the fallback or blacklisting the
        // release.
        let failing = spawn(Vec::new()).await;
        let tmdb = TmdbClient::with_base_url("k".into(), failing.base_url.clone()).unwrap();
        assert!(matches!(
            identify_name("Zzqx.mkv", &[], &tmdb).await,
            Identification::Unavailable
        ));
    }

    /// A torrent whose only file is `name`, for the loopback identification tests.
    fn single_file_torrent(name: &str) -> TorrentInfo {
        TorrentInfo {
            id: "t1".to_string(),
            filename: name.to_string(),
            original_filename: name.to_string(),
            hash: "0123456789abcdef0123456789abcdef01234567".to_string(),
            bytes: 2_000_000_000,
            original_bytes: 2_000_000_000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2020-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: name.to_string(),
                bytes: 2_000_000_000,
                selected: 1,
            }],
            links: vec![],
            ended: None,
        }
    }

    /// When one of the two joined searches fails, the other half's results are no verdict: the
    /// missing half (or a stage the failure cut short) could have held the real match. Picking from
    /// what is left identified a show as an unrelated movie, the scan loop cached it for good, and
    /// `title_check` read it as a mismatch and got the right release blacklisted.
    #[tokio::test]
    async fn a_match_picked_after_a_failed_search_is_no_verdict() {
        use crate::acquire::{TitleCheck, TitleValidator, TmdbTitleValidator};
        use crate::scraper::MediaKind;
        use crate::trakt_client::test_http::{spawn, Reply};
        const UNRELATED: &str = r#"{"results":[{"id":99,"title":"Unrelated Film",
            "release_date":"2001-05-01","popularity":20.0,"vote_count":500,"vote_average":7.5}]}"#;
        // `/search/tv` is not routed, so it answers 404: a failed search.
        let server = spawn(vec![(
            "/search/movie?api_key=k&query=Zzqx",
            Reply::json(UNRELATED),
        )])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();

        assert!(
            matches!(
                identify_name("Zzqx.mkv", &[], &tmdb).await,
                Identification::Unavailable
            ),
            "a pick from half the results must not be a match"
        );
        assert_eq!(server.hits("/search/tv"), 1);

        // The scan loop sees a failed search and a fallback it must not cache.
        let (metadata, failed) = crate::tmdb_client::track_search_failures(identify_torrent(
            &single_file_torrent("Zzqx.mkv"),
            &tmdb,
        ))
        .await;
        assert!(failed);
        assert_eq!(metadata.external_id, None);
        assert_eq!(metadata.title, "Zzqx");

        // The acquisition title check defers instead of rejecting the release.
        let validator = TmdbTitleValidator {
            tmdb: std::sync::Arc::new(tmdb),
        };
        assert_eq!(
            validator
                .validate("Zzqx.mkv", "Zzqx.mkv", 1, MediaKind::Movie, None, None)
                .await,
            TitleCheck::Unavailable
        );

        // Control: with both searches answering, the same movie route identifies an exact title.
        let ok = spawn(vec![
            (
                "/search/movie?api_key=k&query=Zzqx",
                Reply::json(r#"{"results":[{"id":7,"title":"Zzqx","release_date":"2001-05-01"}]}"#),
            ),
            (
                "/search/tv?api_key=k&query=Zzqx",
                Reply::json(r#"{"results":[]}"#),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), ok.base_url.clone()).unwrap();
        assert!(matches!(
            identify_name("Zzqx.mkv", &[], &tmdb).await,
            Identification::Identified(m) if m.external_id.as_deref() == Some("tmdb:7")
        ));
    }

    #[tokio::test]
    #[ignore]
    async fn test_repro_00000_issue() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        let info = TorrentInfo {
            id: "test_id".to_string(),
            filename: "Inception.2010.BluRay.REMUX.1080p.mkv".to_string(),
            original_filename: "Inception.2010.BluRay.REMUX.1080p.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 30000000000,
            original_bytes: 30000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2023-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "BDMV/STREAM/00000.m2ts".to_string(),
                bytes: 25000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("2023-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // It should NOT be identified as "00000" [id=tmdb:886864]
        assert_ne!(metadata.external_id, Some("tmdb:886864".to_string()));
        assert_eq!(metadata.title, "Inception");
    }

    #[tokio::test]
    #[ignore]
    async fn test_2012_is_not_generic() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        let info = TorrentInfo {
            id: "test_id_2012".to_string(),
            filename: "2012.mkv".to_string(),
            original_filename: "2012.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 1000000000,
            original_bytes: 1000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2023-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "2012.mkv".to_string(),
                bytes: 1000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("2023-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;
        assert_eq!(metadata.title, "2012");
    }

    #[tokio::test]
    #[ignore]
    async fn test_peaky_blinders_identification() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        let info = TorrentInfo {
            id: "peaky_id".to_string(),
            filename: "Peaky.Blinders.S01.1080p.BluRay.x264-DON".to_string(),
            original_filename: "Peaky.Blinders.S01.1080p.BluRay.x264-DON".to_string(),
            hash: "hash".to_string(),
            bytes: 30000000000,
            original_bytes: 30000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2023-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "1x05 Episode 5.mkv".to_string(),
                bytes: 2000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("2023-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        assert_eq!(metadata.media_type, MediaType::Show);
        assert_eq!(metadata.title, "Peaky Blinders");
        assert_eq!(metadata.external_id, Some("tmdb:60574".to_string()));
    }

    #[test]
    fn fallback_title_skips_a_generic_cleaned_name() {
        // Torrent name cleans to a generic folder token → skip it in favour of the file name,
        // which cleans to a real title (with its own year), rather than emitting junk "Season 1".
        let (title, year) = fallback_title(
            ("Season 1".to_string(), None),
            ("Real Show".to_string(), Some("2019".to_string())),
            "Real.Show.S01E01.mkv",
        );
        assert_eq!(title, "Real Show");
        assert_eq!(year, Some("2019".to_string()));

        // A real torrent title is used as-is, paired with its own year.
        let (title, year) = fallback_title(
            ("Some Movie".to_string(), Some("2020".to_string())),
            ("Whatever".to_string(), None),
            "raw.mkv",
        );
        assert_eq!(title, "Some Movie");
        assert_eq!(year, Some("2020".to_string()));

        // Every cleaned candidate is generic or empty → the raw file name is the last resort.
        let (title, _year) = fallback_title(
            ("Episode".to_string(), None),
            (String::new(), None),
            "mystery-blob.mkv",
        );
        assert_eq!(title, "mystery-blob.mkv");
    }

    #[test]
    fn fallback_metadata_skips_a_generic_torrent_name_for_the_representative_file() {
        // A torrent named after its season folder falls back to its largest video file's name, so
        // a caller showing it before (or without) TMDB never lists it as a "Season 1" show.
        let mut info = single_file_torrent("Season 1");
        info.files = vec![
            TorrentFile {
                id: 1,
                path: "Season 1/Some.Show.S01E01.720p.mkv".to_string(),
                bytes: 900_000_000,
                selected: 1,
            },
            TorrentFile {
                id: 2,
                path: "Season 1/Some.Show.S01E01.720p.nfo".to_string(),
                bytes: 2_000_000_000,
                selected: 1,
            },
        ];
        let m = fallback_metadata(&info);
        assert_eq!(m.title, "Some Show");
        assert_eq!(m.media_type, MediaType::Show);
        assert_eq!(m.external_id, None);

        // A real torrent name is used as-is, with its year, as a movie by its file structure.
        let m = fallback_metadata(&single_file_torrent("Some.Movie.2020.1080p.mkv"));
        assert_eq!(
            (m.title.as_str(), m.year.as_deref()),
            ("Some Movie", Some("2020"))
        );
        assert_eq!(m.media_type, MediaType::Movie);
        assert_eq!(m.external_id, None);
    }

    /// A Real-Debrid collection where the user selected only one film lists the other films too,
    /// with `selected: 0`. The VFS never shows an unselected file, so the torrent must be named
    /// after the selected film, not the largest one in the list.
    #[test]
    fn identification_takes_its_name_from_the_selected_files() {
        let mut info = single_file_torrent("Volume 1");
        info.files = vec![
            TorrentFile {
                id: 1,
                path: "Volume 1/Film.One.2001.1080p.mkv".to_string(),
                bytes: 5_000_000_000,
                selected: 0,
            },
            TorrentFile {
                id: 2,
                path: "Volume 1/Film.Two.2005.1080p.mkv".to_string(),
                bytes: 4_000_000_000,
                selected: 1,
            },
            TorrentFile {
                id: 3,
                path: "Volume 1/Film.Three.2010.1080p.mkv".to_string(),
                bytes: 8_000_000_000,
                selected: 0,
            },
        ];
        assert_eq!(representative_name(&info), "Film.Two.2005.1080p.mkv");
        let m = fallback_metadata(&info);
        assert_eq!(
            (m.title.as_str(), m.year.as_deref(), m.media_type),
            ("Film Two", Some("2005"), MediaType::Movie)
        );

        // With nothing selected (a Real-Debrid torrent still waiting for its file selection)
        // every file counts, so the largest video names it as before.
        for f in &mut info.files {
            f.selected = 0;
        }
        assert_eq!(representative_name(&info), "Film.Three.2010.1080p.mkv");
        let m = fallback_metadata(&info);
        assert_eq!(
            (m.title.as_str(), m.year.as_deref()),
            ("Film Three", Some("2010"))
        );
    }

    #[test]
    fn test_is_generic_title() {
        assert!(is_generic_title("00000"));
        assert!(is_generic_title("1"));
        assert!(is_generic_title("Episode 5"));
        assert!(is_generic_title("Season 1"));
        assert!(is_generic_title("Part 2"));
        assert!(is_generic_title("Volume 10"));
        assert!(is_generic_title("Vol 3"));
        assert!(is_generic_title("Episode"));
        assert!(is_generic_title("Part A"));
        // A glued number is still a generic folder name (no separating space required).
        assert!(is_generic_title("Season1"));
        // Season folders in other spellings are generic with a number, a plain word without one.
        assert!(is_generic_title("Series 1"));
        assert!(is_generic_title("Staffel 2"));
        assert!(is_generic_title("Seizoen 3"));
        assert!(!is_generic_title("Series"));
        assert!(!is_generic_title("Staffel"));

        assert!(!is_generic_title("Inception"));
        assert!(!is_generic_title("2012")); // 4 digits, but not < 10
        assert!(!is_generic_title("The Episode"));
        // A stop-word GLUED to a single trailing letter forms a real word — it must NOT be
        // treated as generic, or the title loses its TMDB identity before any lookup runs
        // (Showtime's "Episodes", the films/shows "Parts"/"Seasons"/"Party", the show "Volo").
        assert!(!is_generic_title("Episodes"));
        assert!(!is_generic_title("Parts"));
        assert!(!is_generic_title("Seasons"));
        assert!(!is_generic_title("Party"));
        assert!(!is_generic_title("Volo"));
    }

    #[test]
    fn find_case_insensitive_basic() {
        assert_eq!(find_case_insensitive("Hello AKA World", " aka "), Some(5));
        assert_eq!(find_case_insensitive("Hello Aka World", " aka "), Some(5));
        assert_eq!(find_case_insensitive("Hello aka World", " aka "), Some(5));
        assert_eq!(find_case_insensitive("No match here", " aka "), None);
        assert_eq!(find_case_insensitive("aka", " aka "), None); // too short
    }

    #[test]
    fn is_short_title_counts_characters_not_bytes() {
        // ASCII short titles
        assert!(is_short_title("Us"));
        assert!(is_short_title("Don"));
        assert!(!is_short_title("Drive"));
        // Multi-byte titles must be measured in characters, not bytes:
        // "日本" is 2 characters (6 bytes) -> short; "東京物語" is 4 characters -> not short.
        assert!(is_short_title("日本"));
        assert!(!is_short_title("東京物語"));
    }

    #[test]
    fn find_case_insensitive_unicode_safe() {
        // Multi-byte UTF-8 characters before the pattern must not cause a panic.
        // 'Ü' is 2 bytes in UTF-8, so "Üntersuchung" is 13 bytes, not 12.
        assert_eq!(
            find_case_insensitive("Üntersuchung aka Study", " aka "),
            Some(13)
        );
        // Multi-byte char right before " aka " - the 'ü' is 2 bytes,
        // so "Tü" is 3 bytes and the space before "aka" is at byte offset 3.
        assert_eq!(find_case_insensitive("Tü aka X", " aka "), Some(3));
        // Only multi-byte chars, no match
        assert_eq!(find_case_insensitive("日本語テスト", " aka "), None);
    }

    #[test]
    fn clean_name_handles_aka_with_unicode() {
        // Verify that clean_name's "aka" handling doesn't panic on multi-byte UTF-8
        let (name, _year) = clean_name("Üntersuchung aka Study 2024 1080p");
        assert_eq!(name, "Study");
    }

    #[test]
    fn clean_name_truncates_at_bare_episode_number_dash() {
        // Packs that name files "Show - NNN - Episode Title" (the absolute/SxxEyy episode
        // code without the S/E letters, e.g. 409 = S04E09) must reduce to just the show
        // name. Otherwise the episode title leaks into the TMDB query, the show search
        // returns nothing, and a fallback matches an unrelated title.
        assert_eq!(
            clean_name("Some Show - 409 - Episode Title.mkv").0,
            "Some Show"
        );
        assert_eq!(
            clean_name("Some Show - 401 - First Title. Second Part.mkv").0,
            "Some Show"
        );
        // A hyphenated title without the episode-number pattern is unaffected.
        assert_eq!(
            clean_name("Spider-Man - Far From Home 1080p").0,
            "Spider-Man - Far From Home"
        );
    }

    #[test]
    fn trailing_episode_number_reads_the_number_clean_name_cuts_at() {
        // The acquisition engine maps an absolute-numbered anime file to its episode with the same
        // rule `clean_name` cuts the title at.
        for (name, number) in [
            ("[SubsPlease] Some Show - 12 (1080p) [ABCD1234].mkv", 12),
            ("[Group] Some Show - 05v2 [1080p].mkv", 5),
            ("[Group] Some Show S2 - 05 (1080p).mkv", 5),
            ("Some Show - Ep01 [720p].mkv", 1),
            ("Some_Show_-_12_(1080p).mkv", 12),
            ("Some.Show - 12.mkv", 12),
        ] {
            assert_eq!(trailing_episode_number(name), Some(number), "{name:?}");
        }
        // A release year, a number followed by a bracketed release year (a numeric film title), a
        // single digit, a number a second ` - ` follows, and a name with no such number are none.
        for name in [
            "Dr.No - 1962.mkv",
            "Some Show - 12 (2008).mkv",
            "Some.Collection - 1408 [2007] 1080p.mkv",
            "Some Film - 2.mkv",
            "Some Show - 12 - Title (1080p).mkv",
            "Some.Show.S01E12.1080p.mkv",
            " - 12 (1080p).mkv",
        ] {
            assert_eq!(trailing_episode_number(name), None, "{name:?}");
        }
    }

    #[test]
    fn clean_name_truncates_at_a_trailing_dash_episode_number() {
        // The common anime form ends the title at a ` - NN` episode number that no second ` - `
        // follows. Left in the query, TMDB finds nothing, and the leading-word strip can end up
        // searching the number alone (`12` → an unrelated film).
        for (name, title) in [
            (
                "[SubsPlease] Some Show - 12 (1080p) [ABCD1234].mkv",
                "Some Show",
            ),
            ("[Group] Some Show - 05v2 [1080p].mkv", "Some Show"),
            ("[Group] Some Show - 1000 [1080p][HEVC].mkv", "Some Show"),
            ("Show.Name - 101.mkv", "Show Name"),
            ("Some.Show - Ep.5 - The End", "Some Show"),
            ("Some Show - Ep01 [720p].mkv", "Some Show"),
            ("Some Show - EP 07.mkv", "Some Show"),
        ] {
            assert_eq!(clean_name(name).0, title, "{name:?}");
        }
        // A year after the dash is the release year, not an episode number.
        assert_eq!(
            clean_name("Dr.No - 1962.mkv"),
            ("Dr No".to_string(), Some("1962".to_string()))
        );
        assert_eq!(
            clean_name("The Matrix - 1999 - 1080p"),
            ("The Matrix".to_string(), Some("1999".to_string()))
        );
        assert_eq!(
            clean_name("Some Film - 2049.mkv"),
            ("Some Film".to_string(), Some("2049".to_string()))
        );
        // A number followed by a bracketed release year is a numeric film title (`21`, `300`),
        // here behind a kept site/collection prefix: cutting it would take the year with it.
        assert_eq!(
            clean_name("Torrent9.to - 21 (2008) 1080p BluRay.mkv"),
            ("Torrent9 to - 21".to_string(), Some("2008".to_string()))
        );
        assert_eq!(
            clean_name("Some.Collection - 300 (2006) 1080p.mkv"),
            (
                "Some Collection - 300".to_string(),
                Some("2006".to_string())
            )
        );
        assert_eq!(
            clean_name("Some.Collection - 1408 [2007] 1080p.mkv"),
            (
                "Some Collection - 1408".to_string(),
                Some("2007".to_string())
            )
        );
        // A single-digit sequel number and a hyphenated subtitle are title content.
        assert_eq!(
            clean_name("Some Film - 2 2019 1080p"),
            ("Some Film - 2".to_string(), Some("2019".to_string()))
        );
        assert_eq!(
            clean_name("Spider-Man - Far From Home 1080p").0,
            "Spider-Man - Far From Home"
        );
        // A word merely starting with `ep` is not an episode marker.
        assert_eq!(clean_name("Step Up 2 2008 1080p").0, "Step Up 2");
    }

    #[test]
    fn clean_name_truncates_at_framerate_suffixed_resolution() {
        // A framerate-glued quality tag ("2160p60"/"1080p50") is still metadata and must
        // truncate the title, not leak into the TMDB query (mirrors the RES_RE framerate fix).
        assert_eq!(
            clean_name("Spider-Man - Far From Home 2160p60 WEB-DL").0,
            "Spider-Man - Far From Home"
        );
        // A bit-depth-glued quality tag ("1080p10bit"/"2160p10bit") — no separator between the
        // resolution and the bit-depth — must likewise truncate rather than leak into the query.
        assert_eq!(clean_name("Movie.1080p10bit.mkv").0, "Movie");
        assert_eq!(clean_name("Some Show 2160p10bit HEVC").0, "Some Show");
    }

    #[test]
    fn clean_name_truncates_at_an_sd_resolution_tag() {
        // A year-less name has no year to truncate at, so an SD resolution tag must stop the title
        // itself (the strip-prefix fallback only removes LEADING words, so a trailing "480p" would
        // sink every TMDB query).
        assert_eq!(clean_name("Movie.Name.480p.x264.mkv").0, "Movie Name");
        assert_eq!(clean_name("Movie.Name.576p.mkv").0, "Movie Name");
        assert_eq!(clean_name("Movie Name 360p.mkv").0, "Movie Name");
        assert_eq!(clean_name("Movie Name DVDRip XviD.avi").0, "Movie Name");
    }

    #[test]
    fn clean_name_strips_a_site_prefix_but_not_a_dotted_title() {
        // A real `site.tld - ` prefix is stripped.
        assert_eq!(
            clean_name("www.UIndex.org    -    Some Movie 1999 1080p.mkv").0,
            "Some Movie"
        );
        assert_eq!(
            clean_name("Torrent9.ws - Some Movie 2020.mkv").0,
            "Some Movie"
        );
        // A dot-separated SHOW name in front of a ` - ` separator is title content, not a site:
        // stripping it left only the episode title ("Pilot") as the TMDB query.
        assert_eq!(clean_name("Some.Show - S01E01 - Pilot.mkv").0, "Some Show");
        assert_eq!(
            clean_name("Other.Series - S02E03 - Rose.mkv").0,
            "Other Series"
        );
        assert_eq!(
            clean_name("The.Office.Space - S01E01.mkv").0,
            "The Office Space"
        );
        // A show name ending in a country code or a TLD-like word is not a site either: a site
        // prefix is followed by a release name, never straight by an episode code.
        assert_eq!(
            clean_name("The.Office.UK - S01E01 - Downsized.mkv").0,
            "The Office UK"
        );
        assert_eq!(
            clean_name("Shameless.UK - S01E01 - Pilot.mkv").0,
            "Shameless UK"
        );
        assert_eq!(
            clean_name("The.Net - 1x01 - Pilot.mkv").0,
            "The Net",
            "an NxM episode code after the separator keeps the show name"
        );
        assert_eq!(
            clean_name("Some.Show.NZ - 101 - Pilot.mkv").0,
            "Some Show NZ"
        );
        // A dotted RELEASE name in front of a ` - Group` suffix is no site either: its year,
        // SxxEyy or quality tags are what identify it.
        assert_eq!(
            clean_name("Show.Name.S01E01.720p.HDTV - Group.mkv").0,
            "Show Name"
        );
        assert_eq!(
            clean_name("Some.Movie.2019.1080p.BluRay - Group.mkv"),
            ("Some Movie".to_string(), Some("2019".to_string()))
        );
        // Nor is a dotted title followed by a year, a bare episode number or another episode form.
        assert_eq!(
            clean_name("The.Matrix - 1999 - 1080p"),
            ("The Matrix".to_string(), Some("1999".to_string()))
        );
        assert_eq!(
            clean_name("Dr.No - 1962.mkv"),
            ("Dr No".to_string(), Some("1962".to_string()))
        );
        assert_eq!(clean_name("Show.Name - Season.1").0, "Show Name");
        for name in [
            "Show.Name - Ep01 - Title",
            "Show.Name - 101.mkv",
            "Show.Name - Episode.5",
        ] {
            assert_eq!(clean_name(name).0, "Show Name", "{name:?}");
        }
        // KNOWN LIMITATION: a bare episode number followed by an episode title (no second ` - `)
        // stays in the title; cutting before any trailing word would also cut a hyphenated film
        // subtitle that starts with a number.
        assert_eq!(
            clean_name("Show.Name - 101.Pilot.mkv").0,
            "Show Name - 101 Pilot"
        );
        // A dotted title followed by more title words, with no release tag after it, is kept too:
        // a site prefix is followed by a release name (a year or a quality tag).
        assert!(
            clean_name("Mr.Bean - The Movie.mkv")
                .0
                .starts_with("Mr Bean"),
            "got {:?}",
            clean_name("Mr.Bean - The Movie.mkv").0
        );
        assert_eq!(
            clean_name("Some.Show.Net - Season 2 - 1080p.mkv").0,
            "Some Show Net"
        );
        // A site whose TLD is on no list is still a site when a release name follows it.
        assert_eq!(
            clean_name("Torrent9.to - Some Movie 2020.mkv").0,
            "Some Movie"
        );
        assert_eq!(
            clean_name("Some-Site.lat - Other Movie 2019 1080p.mkv").0,
            "Other Movie"
        );
        // A `www.` prefix is always a site, even before a year-less release name.
        assert_eq!(
            clean_name("www.Some-Site.org - Other Movie.mkv").0,
            "Other Movie"
        );
        // A site followed by a dotted release name (an episode code inside it, not at its start).
        assert_eq!(
            clean_name("Torrent9.ws - Some.Show.S02E05.720p.mkv").0,
            "Some Show"
        );
    }

    #[test]
    fn clean_name_truncates_at_an_nxm_episode_code() {
        // An `NxM` episode code ends the title like an SxxEyy one: the episode title after it must
        // not reach the TMDB query, where the leading-word strip could match it on its own.
        assert_eq!(
            clean_name("Show.Name.1x01.Pilot.720p.HDTV.x264.mkv").0,
            "Show Name"
        );
        assert_eq!(
            clean_name("Some.Show.3x14.The.Long.Goodbye.HDTV.XviD").0,
            "Some Show"
        );
        // A year before the code is still the year, and dropped from the title.
        assert_eq!(
            clean_name("Show.Name.2005.1x01.Pilot.mkv"),
            ("Show Name".to_string(), Some("2005".to_string()))
        );
        // A title that IS an `NxM` token is kept, and a resolution or aspect ratio is no code.
        assert_eq!(
            clean_name("10x10.2018.1080p.WEB.mkv"),
            ("10x10".to_string(), Some("2018".to_string()))
        );
        assert_eq!(clean_name("10x10.2018.1080p.mkv").0, "10x10");
        assert_eq!(clean_name("Movie.1920x1080.mkv").0, "Movie");
        assert_eq!(clean_name("Some.Movie.16x9.mkv").0, "Some Movie 16x9");
        // The cleaned name no longer carries the code, yet the file still reads as an episode.
        assert!(filename_has_show_marker(
            "Show.Name.1x01.Pilot.720p.HDTV.x264.mkv"
        ));
        assert!(!filename_has_show_marker("10x10.2018.1080p.mkv"));
    }

    #[test]
    fn clean_name_preserves_title_that_is_entirely_a_stopword() {
        // A film literally named after a metadata word must not clean to an empty title.
        assert_eq!(clean_name("Complete").0, "Complete");
        assert_eq!(clean_name("Multi").0, "Multi");
        assert_eq!(clean_name("Custom").0, "Custom");
        // Normal trailing-metadata stripping is unaffected.
        assert_eq!(clean_name("The Matrix 1080p").0, "The Matrix");
    }

    #[test]
    fn clean_name_truncates_at_dot_form_web_dl() {
        // "WEB.DL" becomes "WEB DL" after dot→space replacement; STOP_RE's web[\s-]?dl truncates it
        // (the hyphen form already did). Without this the title kept a trailing "WEB DL" fragment.
        assert_eq!(clean_name("The.Movie.WEB.DL.mkv").0, "The Movie");
        assert_eq!(clean_name("The.Movie.WEB-DL.mkv").0, "The Movie");
    }

    #[test]
    fn clean_name_keeps_an_nxm_titled_movie() {
        // "10x10" (2018) must survive: the resolution-form STOP_RE alternative is bounded to
        // \d{3,4}x\d{3,4}, so an unbounded \d+x\d+ no longer consumes the title as a truncation
        // point (which had collapsed it to just its year).
        let (title, year) = clean_name("10x10.2018.1080p.BluRay.mkv");
        assert_eq!(title, "10x10");
        assert_eq!(year.as_deref(), Some("2018"));
        // A genuine WxH resolution token is still stripped.
        assert_eq!(clean_name("Nosferatu 1920x1080 WEB-DL").0, "Nosferatu");
    }

    #[test]
    fn clean_name_keeps_a_leading_parenthesised_title_word() {
        // A leading "(word)" whose content is a single title word (no dot/whitespace) is NOT a
        // site/URL release tag — it is part of the title (the 2009 film "(Untitled)"). PREFIX_RE
        // must only strip a letter-paren that looks like a tag (its content carries a '.'/space, e.g.
        // "(www.site.com)"). Regression: before, "(Untitled).2009" cleaned to the title "2009".
        let (title, year) = clean_name("(Untitled).2009.1080p.mkv");
        assert!(title.contains("Untitled"), "got {title:?}");
        assert_ne!(title, "2009");
        assert_eq!(year.as_deref(), Some("2009"));
        // Regression: a genuine parenthesised URL/site tag (its content has dots) is STILL stripped.
        assert_eq!(
            clean_name("(www.site.com) Inception 2010 1080p").0,
            "Inception"
        );
        // Regression: a leading parenthesised YEAR is STILL stripped.
        assert_eq!(clean_name("(2020) Tenet 1080p BluRay").0, "Tenet");
    }

    #[test]
    fn clean_name_strips_a_run_of_leading_bracket_tags() {
        // Consecutive leading tags ("[Group][BD] Title", common on anime releases) must ALL be
        // stripped. A single-pass strip removed only the first group and left the second's label
        // glued on ("[Judas][BD] …" -> "BD] …") unless the second happened to be a stopword.
        assert_eq!(
            clean_name("[Judas][BD] Cowboy Bebop 1999 1080p.mkv").0,
            "Cowboy Bebop"
        );
        // Three consecutive bracket tags.
        assert_eq!(
            clean_name("[Erai-raws][1080p][HEVC] Attack on Titan 2013 1080p.mkv").0,
            "Attack on Titan"
        );
        // Regression guard: a leading "[Title] (YYYY)" whose bracket is title content is still kept,
        // even after a preceding release-group tag is stripped.
        assert_eq!(clean_name("[Nyaa] [REC] (2007) 1080p.mkv").0, "[REC]");
    }

    #[test]
    fn clean_name_keeps_titles_containing_ambiguous_pack_words() {
        // Pack/label words that COLLIDE with real title words (complete/completa/custom/multi) must
        // not truncate a legitimate title mid-word — exactly the reason the 3-letter ISO codes are
        // excluded from STOP_RE. Regression: STOP_RE's leftmost match on "Complete" truncated the
        // 2024 film "A Complete Unknown" to "A" (a 1-char short title that then fails identification).
        let (title, year) = clean_name("A.Complete.Unknown.2024.1080p.BluRay.x264.mkv");
        assert_eq!(title, "A Complete Unknown");
        assert_eq!(year.as_deref(), Some("2024"));
        // "custom" mid-title (the film "The Custom of the Country") — same truncation path.
        assert_eq!(
            clean_name("The Custom of the Country 2019 1080p").0,
            "The Custom of the Country"
        );
        // Spanish "completa" as a title word.
        assert_eq!(clean_name("Vida Completa 2018 1080p").0, "Vida Completa");
        // A trailing resolution/codec token still truncates (the ambiguous words no longer shadow it).
        assert_eq!(clean_name("The Matrix 1080p MULTI").0, "The Matrix");
    }

    #[test]
    fn clean_name_does_not_strip_title_words_that_are_language_codes() {
        // 3-letter ISO language codes (spa/ita/ger/…) are NOT stop words: they collide with real
        // title words and would mangle the search. Audio language is verified by probe, not here.
        // Regression: the 2016 film "Spa Night" must not clean to "Night".
        let (title, year) = clean_name("Spa Night 2016 1080p");
        assert_eq!(title, "Spa Night");
        assert_eq!(year.as_deref(), Some("2016"));
        // A genuine trailing language token next to other metadata is still removed by the year /
        // resolution truncation that follows it.
        assert_eq!(clean_name("Some Movie 2020 ITA 1080p").0, "Some Movie");
    }

    #[test]
    fn clean_name_keeps_title_brackets_and_parens() {
        // A leading "[Title] (YYYY)" is the title, not a release tag — must not strip to "2007)".
        let (rec, rec_year) = clean_name("[REC] (2007) 1080p BluRay.mkv");
        assert_eq!(rec, "[REC]");
        assert_eq!(rec_year.as_deref(), Some("2007"));
        // A title that itself CONTAINS a year must use the LAST year token as the release year, not
        // the first — else "Blade Runner 2049" (2017) is identified by year 2049 (and truncated to
        // "Blade Runner"), matching the wrong film. The title-embedded "2049" is preserved.
        let (br, br_year) = clean_name("Blade.Runner.2049.2017.1080p.BluRay.mkv");
        assert_eq!(
            br_year.as_deref(),
            Some("2017"),
            "release year is the LAST token"
        );
        assert_eq!(
            br, "Blade Runner 2049",
            "the title-embedded year is preserved AND the trailing release year is dropped — \
             two adjacent standalone years (2049 then a SMALLER 2017) are not a date range"
        );
        // 1917 (the 2019 war film): release year 2019, title "1917" kept — the trailing release
        // year is dropped even though "1917 2019" is ASCENDING, because a real date range is
        // dash-joined ("1939-1945"), not two space-separated standalone years.
        let (war, war_year) = clean_name("1917.2019.1080p.BluRay.x264.mkv");
        assert_eq!(war_year.as_deref(), Some("2019"));
        assert_eq!(war, "1917", "got {war:?}");
        // Same shape: a title-year that precedes its release year ("1941" dir. Spielberg, 1979).
        let (forty_one, forty_one_year) = clean_name("1941.1979.1080p.BluRay.x264.mkv");
        assert_eq!(forty_one_year.as_deref(), Some("1979"));
        assert_eq!(forty_one, "1941", "got {forty_one:?}");
        // A non-year numeric title-paren is kept (the film "(500) Days of Summer").
        assert_eq!(
            clean_name("(500) Days of Summer 2009 1080p").0,
            "(500) Days of Summer"
        );
        // Regression: a genuine leading bracket/site tag is STILL stripped.
        assert_eq!(clean_name("[YTS.MX] The Matrix 1999 1080p").0, "The Matrix");
        assert_eq!(
            clean_name("(www.site.com) Inception 2010 1080p").0,
            "Inception"
        );
        // Regression: a leading parenthesised YEAR prefix is STILL stripped.
        assert_eq!(clean_name("(2020) Tenet 1080p BluRay").0, "Tenet");
    }

    #[test]
    fn clean_name_drops_release_year_even_when_title_contains_a_year_range() {
        // A date-range in the title (e.g. a war/history doc) must NOT disable dropping the
        // separate trailing release year. The range is checked against the matched year's span,
        // not the whole title.
        let (title, year) = clean_name("WWII.in.Color.1939-1945.2009.1080p.mkv");
        assert_eq!(
            year.as_deref(),
            Some("2009"),
            "release year is the LAST token"
        );
        assert_eq!(
            title, "WWII in Color 1939-1945",
            "the title-embedded date range is preserved and only the trailing release year dropped"
        );
        // A title that is JUST a date range (no separate release year) keeps the whole range.
        let (only_range, _) = clean_name("The Great War 1914-1918 1080p BluRay.mkv");
        assert_eq!(only_range, "The Great War 1914-1918");
    }

    // --- Direct tests for the score_result tuning constants ---

    #[test]
    fn score_result_exact_year_beats_off_by_one_and_wrong_year() {
        let q = "themovie";
        // Three identical titles differing only by release year vs the queried 2010.
        let exact = make_result(1, "The Movie", Some("2010-01-01"), 0.0, None, None);
        let off_by_one = make_result(2, "The Movie", Some("2011-01-01"), 0.0, None, None);
        let wrong = make_result(3, "The Movie", Some("2020-01-01"), 0.0, None, None);
        let year = Some("2010".to_string());
        let s_exact = score_result(&exact, q, &year, 2024);
        let s_off = score_result(&off_by_one, q, &year, 2024);
        let s_wrong = score_result(&wrong, q, &year, 2024);
        // Exact (+200) > ±1 (+150) > wrong (+0) for the year component, on top of the shared
        // exact-title +1000.
        assert!(s_exact > s_off, "{s_exact} vs {s_off}");
        assert!(s_off > s_wrong, "{s_off} vs {s_wrong}");
        assert_eq!(s_exact - s_off, 50.0);
        assert_eq!(s_off - s_wrong, 150.0);
    }

    #[test]
    fn score_result_exact_title_dominates_partial_even_with_wrong_year() {
        // An exact-title-but-wrong-year result must still beat a partial-title-but-exact-year one:
        // the exact/partial gap (1000 - 100 = 900) dwarfs the year gap (200).
        let exact_wrong_year = make_result(1, "Heat", Some("1999-01-01"), 0.0, None, None);
        let partial_right_year = make_result(2, "Heat Wave", Some("1995-01-01"), 0.0, None, None);
        let year = Some("1995".to_string());
        let s_exact = score_result(&exact_wrong_year, "heat", &year, 2024);
        let s_partial = score_result(&partial_right_year, "heat", &year, 2024);
        assert!(
            s_exact > s_partial,
            "exact-title must win: {s_exact} vs {s_partial}"
        );
    }

    #[test]
    fn score_result_recency_only_applies_without_a_year() {
        // With no filename year, a newer release scores higher via the recency ramp.
        let new = make_result(1, "Sherwood", Some("2022-01-01"), 0.0, None, None);
        let old = make_result(2, "Sherwood", Some("2010-01-01"), 0.0, None, None);
        let s_new = score_result(&new, "sherwood", &None, 2024);
        let s_old = score_result(&old, "sherwood", &None, 2024);
        assert!(s_new > s_old, "recency tiebreaker: {s_new} vs {s_old}");
        // The ramp floors at 0 (no negative penalty for very old content beyond ~10 years).
        let ancient = make_result(3, "Sherwood", Some("1980-01-01"), 0.0, None, None);
        let s_ancient = score_result(&ancient, "sherwood", &None, 2024);
        assert_eq!(
            s_ancient, 1000.0,
            "old content gets exact-title only, no recency"
        );
        // A future-dated (unaired) release must NOT out-rank an already-released one. Previously its
        // age was clamped to 0 and it got the full +80, beating the current-year title; now it gets
        // no recency bonus at all.
        let unaired = make_result(4, "Sherwood", Some("2030-01-01"), 0.0, None, None);
        let current = make_result(5, "Sherwood", Some("2024-01-01"), 0.0, None, None);
        let s_unaired = score_result(&unaired, "sherwood", &None, 2024);
        let s_current = score_result(&current, "sherwood", &None, 2024);
        assert_eq!(s_unaired, 1000.0, "no recency bonus for an unaired release");
        assert!(
            s_current > s_unaired,
            "a just-released title must beat a future-dated one: {s_current} vs {s_unaired}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_short_name_no_random_match() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        // "UC" cleans to "UC"
        let info = TorrentInfo {
            id: "uc_id".to_string(),
            filename: "UC.S01.1080p.mkv".to_string(),
            original_filename: "UC.S01.1080p.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 1000000000,
            original_bytes: 1000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2023-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "UC.S01E01.mkv".to_string(),
                bytes: 1000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("2023-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // It should NOT be identified as Gundam Unicorn (45500) because "UC" is too short for a broad match fallback
        assert_ne!(metadata.external_id, Some("tmdb:45500".to_string()));
        // It should fallback to cleaned name "UC"
        assert_eq!(metadata.title, "UC");
        assert_eq!(metadata.external_id, None);
    }

    #[tokio::test]
    #[ignore]
    async fn test_flow_2024_prefers_popular() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        let info = TorrentInfo {
            id: "flow_id".to_string(),
            filename: "Flow.2024.1080p.BluRay.x264.mkv".to_string(),
            original_filename: "Flow.2024.1080p.BluRay.x264.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 5000000000,
            original_bytes: 5000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2024-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "Flow.2024.1080p.BluRay.x264.mkv".to_string(),
                bytes: 5000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("2024-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should identify as the popular Oscar-nominated animated film (823219)
        // NOT the 8-minute short film (1281775)
        assert_eq!(metadata.external_id, Some("tmdb:823219".to_string()));
        assert_eq!(metadata.title, "Flow");
        assert_eq!(metadata.media_type, MediaType::Movie);
    }

    #[tokio::test]
    #[ignore]
    async fn test_sherwood_s02_disambiguation() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        let info = TorrentInfo {
            id: "sherwood_id".to_string(),
            filename: "Sherwood.S02E01.1080p.WEB.H264-DiMEPiECE.mkv".to_string(),
            original_filename: "Sherwood.S02E01.1080p.WEB.H264-DiMEPiECE.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 2000000000,
            original_bytes: 2000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2025-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "Sherwood.S02E01.1080p.WEB.H264-DiMEPiECE.mkv".to_string(),
                bytes: 2000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("2025-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should be Sherwood (155243), NOT the wrong one (87399)
        assert_eq!(metadata.external_id, Some("tmdb:155243".to_string()));
    }

    #[tokio::test]
    #[ignore]
    async fn test_dune_2000_identification() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        let info = TorrentInfo {
            id: "dune_id".to_string(),
            filename: "Dune.2000.S01E01.1080p.BluRay.x264-PFa.mkv".to_string(),
            original_filename: "Dune.2000.S01E01.1080p.BluRay.x264-PFa.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 3000000000,
            original_bytes: 3000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2000-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "Dune.2000.S01E01.1080p.BluRay.x264-PFa.mkv".to_string(),
                bytes: 3000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("2000-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should identify as Frank Herbert's Dune miniseries (19566)
        // NOT Fly Tales (15911) or Dune 2021 (438631)
        assert_eq!(metadata.external_id, Some("tmdb:19566".to_string()));
        assert_eq!(metadata.title, "Frank Herbert's Dune");
        assert_eq!(metadata.media_type, MediaType::Show);
    }

    #[tokio::test]
    #[ignore]
    async fn test_don_2022_tamil_identification() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        let info = TorrentInfo {
            id: "don_id".to_string(),
            filename: "Don (2022) UNCUT 1080p 10bit NF WEBRip x265 HEVC [Org YT Hindi DD 2.0 ~192Kbps + Tamil DD 5.1] ESub ~ Immortal.mkv".to_string(),
            original_filename: "Don (2022) UNCUT 1080p 10bit NF WEBRip x265 HEVC [Org YT Hindi DD 2.0 ~192Kbps + Tamil DD 5.1] ESub ~ Immortal.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 2000000000,
            original_bytes: 2000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2022-05-12".to_string(),
            files: vec![
                TorrentFile {
                    id: 1,
                    path: "Don (2022) UNCUT 1080p 10bit NF WEBRip x265 HEVC [Org YT Hindi DD 2.0 ~192Kbps + Tamil DD 5.1] ESub ~ Immortal.mkv".to_string(),
                    bytes: 2000000000,
                    selected: 1,
                }
            ],
            links: vec!["http://link1".to_string()],
            ended: Some("2022-05-12".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should identify as the 2022 Tamil movie "Don" (TMDB 895033)
        // Filename contains Hindi+Tamil audio tracks; this is the same Tamil film
        // tested in tests/test_short_titles.rs with a generic filename.
        // NOT "Makoto Kitano: Don't You Guys Go..." (1266372)
        assert_eq!(metadata.external_id, Some("tmdb:895033".to_string()));
        assert_eq!(metadata.title, "Don");
        assert_eq!(metadata.media_type, MediaType::Movie);
    }

    #[tokio::test]
    #[ignore]
    async fn test_bond_collection_prefix_stripped_generically() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        let info = TorrentInfo {
            id: "bond_id".to_string(),
            filename: "Bond.50.Goldfinger.1964.1080p.BluRay.x264.mkv".to_string(),
            original_filename: "Bond.50.Goldfinger.1964.1080p.BluRay.x264.mkv".to_string(),
            hash: "hash".to_string(),
            bytes: 5000000000,
            original_bytes: 5000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "1964-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "Bond.50.Goldfinger.1964.1080p.BluRay.x264.mkv".to_string(),
                bytes: 5000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("1964-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        // Should identify as Goldfinger (658) via word-stripping fallback
        assert_eq!(metadata.external_id, Some("tmdb:658".to_string()));
        assert_eq!(metadata.title, "Goldfinger");
        assert_eq!(metadata.media_type, MediaType::Movie);
    }

    #[tokio::test]
    #[ignore]
    async fn test_short_title_ted_identified() {
        crate::config::load_dotenv().expect("load .env");
        let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
        let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

        // "ted" is a 3-char title with no year — should still identify via TMDB
        let info = TorrentInfo {
            id: "ted_id".to_string(),
            filename: "ted.S02E01.Talk.Dirty.to.Me.1080p.AMZN.WEB-DL.DDP5.1.H.264-RAWR.mkv"
                .to_string(),
            original_filename:
                "ted.S02E01.Talk.Dirty.to.Me.1080p.AMZN.WEB-DL.DDP5.1.H.264-RAWR.mkv".to_string(),
            hash: "hash_ted".to_string(),
            bytes: 3_000_000_000,
            original_bytes: 3_000_000_000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2024-06-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: "ted.S02E01.Talk.Dirty.to.Me.1080p.AMZN.WEB-DL.DDP5.1.H.264-RAWR.mkv"
                    .to_string(),
                bytes: 3_000_000_000,
                selected: 1,
            }],
            links: vec!["http://link_ted".to_string()],
            ended: Some("2024-06-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        assert_eq!(metadata.media_type, MediaType::Show);
        assert!(
            metadata.external_id.is_some(),
            "Short title 'ted' should be identified via TMDB, got external_id=None"
        );
    }

    /// A season pack whose files carry the dash-concatenated code (`Some Show - 409 - Episode
    /// Title`, no `S04E09`) is identified as the show: `clean_name` cuts the name at the code, so
    /// the episode title never reaches the TMDB query, where a fallback matched an unrelated title.
    #[tokio::test]
    async fn a_dash_coded_season_pack_is_identified_by_its_show_name() {
        use crate::trakt_client::test_http::{spawn, Reply};
        let server = spawn(vec![
            (
                "/search/tv?api_key=k&query=Some+Show",
                Reply::json(
                    r#"{"results":[{"id":11,"title":"Some Show","release_date":"2013-12-02",
                        "popularity":30.0,"vote_count":900,"vote_average":8.5}]}"#,
                ),
            ),
            (
                "/search/movie?api_key=k&query=Some+Show",
                Reply::json(r#"{"results":[]}"#),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        let folder = "Some Show (4 season) [1080p]";
        let files: Vec<TorrentFile> = [
            (3_300_000_000u64, "Some Show - 409 - Episode Title.mkv"),
            (
                3_100_000_000,
                "Some Show - 401 - First Title. Second Part.mkv",
            ),
            (2_900_000_000, "Some Show - 410 - Another Title.mkv"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (bytes, name))| TorrentFile {
            id: i as u32 + 1,
            path: format!("{folder}/{name}"),
            bytes: *bytes,
            selected: 1,
        })
        .collect();
        let info = TorrentInfo {
            id: "t1".to_string(),
            filename: folder.to_string(),
            original_filename: folder.to_string(),
            status: "downloaded".to_string(),
            files,
            ..Default::default()
        };
        let metadata = identify_torrent(&info, &tmdb).await;
        assert_eq!(metadata.media_type, MediaType::Show);
        assert_eq!(metadata.title, "Some Show");
        assert_eq!(
            metadata.external_id.as_deref(),
            Some("tmdb:11"),
            "requests {:?}",
            server.requests()
        );
        assert!(
            !server.requests().iter().any(|r| r.contains("Title")),
            "the episode title never reaches the query: {:?}",
            server.requests()
        );
    }

    // --- Helper for best_scored_result / select_best_match tests ---

    fn make_result(
        id: u64,
        title: &str,
        release_date: Option<&str>,
        popularity: f64,
        vote_avg: Option<f64>,
        vote_count: Option<u32>,
    ) -> TmdbSearchResult {
        TmdbSearchResult {
            id,
            title: title.to_string(),
            original_title: None,
            release_date: release_date.map(|s| s.to_string()),
            popularity,
            vote_average: vote_avg,
            vote_count,
        }
    }

    // --- Tests for best_scored_result ---

    #[test]
    fn best_scored_result_empty_returns_none() {
        let results: Vec<TmdbSearchResult> = vec![];
        let got = best_scored_result(&results, "anything", &None, false);
        assert!(got.is_none());
    }

    #[test]
    fn best_scored_result_single_result() {
        let results = vec![make_result(
            1,
            "Inception",
            Some("2010-07-16"),
            80.0,
            Some(8.4),
            Some(30000),
        )];
        let got = best_scored_result(
            &results,
            &normalize_title("Inception"),
            &Some("2010".to_string()),
            false,
        );
        assert_eq!(got.unwrap().id, 1);
    }

    #[test]
    fn best_scored_result_exact_title_wins() {
        // "Flow" exact match with low popularity should beat a partial/non-match with higher popularity
        let results = vec![
            make_result(
                1,
                "Overflow",
                Some("2020-01-01"),
                200.0,
                Some(7.0),
                Some(500),
            ),
            make_result(2, "Flow", Some("2024-01-01"), 10.0, Some(7.5), Some(100)),
        ];
        let nq = normalize_title("Flow");
        let got = best_scored_result(&results, &nq, &None, false);
        assert_eq!(got.unwrap().id, 2);
    }

    #[test]
    fn best_scored_result_year_match_boosts() {
        let results = vec![
            make_result(1, "Dune", Some("2021-10-22"), 50.0, Some(7.8), Some(8000)),
            make_result(2, "Dune", Some("1984-12-14"), 20.0, Some(6.3), Some(2000)),
        ];
        let nq = normalize_title("Dune");
        let got = best_scored_result(&results, &nq, &Some("1984".to_string()), false);
        assert_eq!(got.unwrap().id, 2);
    }

    #[test]
    fn best_scored_result_short_title_requires_exact_and_year() {
        // Short title "IT" — only the result with exact title AND matching year should qualify
        let results = vec![
            make_result(1, "It", Some("2017-09-06"), 100.0, Some(7.3), Some(15000)),
            make_result(2, "It", Some("1990-11-18"), 30.0, Some(6.9), Some(2000)),
            make_result(3, "Italy", Some("2017-05-01"), 5.0, Some(5.0), Some(50)),
        ];
        let nq = normalize_title("IT");
        let got = best_scored_result(&results, &nq, &Some("1990".to_string()), true);
        // Only id=2 has exact title "it" AND year 1990
        assert_eq!(got.unwrap().id, 2);
    }

    #[test]
    fn best_scored_result_short_title_no_year_allows_exact_match() {
        // Short title "ted" with NO year in the filename should still match
        // an exact TMDB result rather than failing identification entirely.
        let results = vec![
            make_result(1, "Ted", Some("2024-01-11"), 80.0, Some(6.5), Some(500)),
            make_result(
                2,
                "Ted Lasso",
                Some("2020-08-14"),
                90.0,
                Some(8.0),
                Some(3000),
            ),
        ];
        let nq = normalize_title("ted");
        // year = None (no year in filename)
        let got = best_scored_result(&results, &nq, &None, true);
        // Should pick id=1 (exact title match "Ted" == "ted"), not None
        assert!(
            got.is_some(),
            "short title with no year should still match exact title"
        );
        assert_eq!(got.unwrap().id, 1);
    }

    #[test]
    fn best_scored_result_short_title_matches_via_original_title() {
        // A short title whose TMDB `title` is localized but whose `original_title` is the native short
        // form must still match (mirrors score_result/is_exact) — not be dropped as a non-match.
        let mut r = make_result(
            1,
            "The ABC Movie",
            Some("2020-01-01"),
            50.0,
            Some(7.0),
            Some(500),
        );
        r.original_title = Some("ABC".to_string());
        let nq = normalize_title("ABC");
        let got = best_scored_result(std::slice::from_ref(&r), &nq, &None, true);
        assert!(
            got.is_some(),
            "a short title matching original_title must not be dropped"
        );
        assert_eq!(got.unwrap().id, 1);
    }

    #[test]
    fn popularity_term_is_capped_so_a_trending_partial_cannot_beat_an_exact_match() {
        // Raw TMDB popularity can reach the thousands; uncapped it would swamp the exact-vs-partial
        // title gap and let a trending substring match beat an obscure exact match on a year-less
        // filename. The cap keeps popularity a tiebreaker, not a dominator.
        let results = vec![
            // Trending partial (contains "flow") with enormous popularity.
            make_result(
                1,
                "Flowing River",
                Some("2024-01-01"),
                5000.0,
                Some(8.0),
                Some(99999),
            ),
            // Obscure exact match.
            make_result(2, "Flow", Some("2024-01-01"), 1.0, Some(6.0), Some(10)),
        ];
        let nq = normalize_title("Flow");
        let got = best_scored_result(&results, &nq, &None, false);
        assert_eq!(
            got.unwrap().id,
            2,
            "an exact match must beat a trending partial despite huge popularity"
        );
    }

    #[test]
    fn best_scored_result_short_title_no_year_rejects_partial() {
        // Short title "UC" with NO year — no exact title match exists, should still return None
        let results = vec![
            make_result(
                1,
                "Gundam Unicorn",
                Some("2010-02-20"),
                50.0,
                Some(7.0),
                Some(200),
            ),
            make_result(2, "UC Browser", None, 10.0, None, None),
        ];
        let nq = normalize_title("UC");
        let got = best_scored_result(&results, &nq, &None, true);
        assert!(
            got.is_none(),
            "short title with no exact match should still return None"
        );
    }

    #[test]
    fn best_scored_result_short_title_no_match_returns_none() {
        // Short title "UC" with no exact+year match should return None
        let results = vec![
            make_result(
                1,
                "Gundam Unicorn",
                Some("2010-02-20"),
                50.0,
                Some(7.0),
                Some(200),
            ),
            make_result(2, "UC Browser", None, 10.0, None, None),
        ];
        let nq = normalize_title("UC");
        let got = best_scored_result(&results, &nq, &Some("2023".to_string()), true);
        assert!(got.is_none());
    }

    #[test]
    fn best_scored_result_higher_votes_breaks_tie() {
        // Two exact title matches, same year — higher vote count should win
        let results = vec![
            make_result(1, "Flow", Some("2024-08-30"), 50.0, Some(8.0), Some(5000)),
            make_result(2, "Flow", Some("2024-03-15"), 40.0, Some(7.5), Some(20)),
        ];
        let nq = normalize_title("Flow");
        let got = best_scored_result(&results, &nq, &Some("2024".to_string()), false);
        assert_eq!(got.unwrap().id, 1);
    }

    // --- Tests for select_best_match ---

    #[test]
    fn select_best_match_both_none() {
        let got = select_best_match(None, None, "anything", &None, false);
        assert!(got.is_none());
    }

    #[test]
    fn select_best_match_only_tv() {
        let tv = make_result(
            100,
            "Breaking Bad",
            Some("2008-01-20"),
            200.0,
            Some(8.9),
            Some(10000),
        );
        let got = select_best_match(
            Some(&tv),
            None,
            &normalize_title("Breaking Bad"),
            &None,
            true,
        );
        let (title, _date, id, media_type) = got.unwrap();
        assert_eq!(title, "Breaking Bad");
        assert_eq!(id, "100");
        assert_eq!(media_type, MediaType::Show);
    }

    #[test]
    fn select_best_match_episode_marker_beats_exact_movie_coincidence() {
        // ID2 regression: with an SxxExx marker (is_show_guess=true), a decorated TV title (not an
        // exact match to the query) must NOT be overridden by an exact same-named MOVIE — the episode
        // marker is the stronger signal.
        let tv = make_result(
            300,
            "Show Origins",
            Some("2015-01-01"),
            50.0,
            Some(7.0),
            Some(500),
        );
        let movie = make_result(400, "Show", Some("1999-01-01"), 40.0, Some(6.5), Some(800));
        let got = select_best_match(
            Some(&tv),
            Some(&movie),
            &normalize_title("Show"),
            &None,
            true,
        );
        let (_title, _date, id, media_type) = got.unwrap();
        assert_eq!(
            media_type,
            MediaType::Show,
            "an episode marker must beat an exact movie-title coincidence"
        );
        assert_eq!(id, "300");
    }

    #[test]
    fn select_best_match_only_movie() {
        let movie = make_result(
            200,
            "Inception",
            Some("2010-07-16"),
            100.0,
            Some(8.4),
            Some(30000),
        );
        let got = select_best_match(
            None,
            Some(&movie),
            &normalize_title("Inception"),
            &None,
            false,
        );
        let (title, _date, id, media_type) = got.unwrap();
        assert_eq!(title, "Inception");
        assert_eq!(id, "200");
        assert_eq!(media_type, MediaType::Movie);
    }

    #[test]
    fn select_best_match_tv_exact_year_wins() {
        // TV has exact title + year match; movie has exact title but wrong year
        let tv = make_result(10, "Dune", Some("2000-12-03"), 30.0, Some(7.0), Some(500));
        let movie = make_result(20, "Dune", Some("2021-10-22"), 80.0, Some(7.8), Some(8000));
        let nq = normalize_title("Dune");
        let year = Some("2000".to_string());
        let got = select_best_match(Some(&tv), Some(&movie), &nq, &year, false);
        let (_title, _date, id, media_type) = got.unwrap();
        assert_eq!(id, "10");
        assert_eq!(media_type, MediaType::Show);
    }

    #[test]
    fn select_best_match_movie_exact_year_wins() {
        // Movie has exact title + year match; TV has exact title but wrong year
        let tv = make_result(10, "Don", Some("2006-10-20"), 30.0, Some(6.5), Some(200));
        let movie = make_result(20, "Don", Some("2022-05-13"), 50.0, Some(7.2), Some(1000));
        let nq = normalize_title("Don");
        let year = Some("2022".to_string());
        let got = select_best_match(Some(&tv), Some(&movie), &nq, &year, true);
        let (_title, _date, id, media_type) = got.unwrap();
        assert_eq!(id, "20");
        assert_eq!(media_type, MediaType::Movie);
    }

    #[test]
    fn select_best_match_show_guess_prefers_tv() {
        // Both have exact title, no year info — is_show_guess=true should prefer TV
        let tv = make_result(
            10,
            "Sherwood",
            Some("2022-06-13"),
            40.0,
            Some(7.0),
            Some(300),
        );
        let movie = make_result(
            20,
            "Sherwood",
            Some("2019-11-22"),
            20.0,
            Some(6.5),
            Some(100),
        );
        let nq = normalize_title("Sherwood");
        let got = select_best_match(Some(&tv), Some(&movie), &nq, &None, true);
        let (_title, _date, id, media_type) = got.unwrap();
        assert_eq!(id, "10");
        assert_eq!(media_type, MediaType::Show);
    }

    #[test]
    fn select_best_match_no_show_guess_prefers_movie() {
        // Both have exact title, no year info — is_show_guess=false should prefer movie
        let tv = make_result(
            10,
            "Sherwood",
            Some("2022-06-13"),
            40.0,
            Some(7.0),
            Some(300),
        );
        let movie = make_result(
            20,
            "Sherwood",
            Some("2019-11-22"),
            20.0,
            Some(6.5),
            Some(100),
        );
        let nq = normalize_title("Sherwood");
        let got = select_best_match(Some(&tv), Some(&movie), &nq, &None, false);
        let (_title, _date, id, media_type) = got.unwrap();
        assert_eq!(id, "20");
        assert_eq!(media_type, MediaType::Movie);
    }

    #[test]
    fn is_show_guess_single_selected_video_is_movie() {
        // A movie torrent with one selected video and unselected extras
        // should NOT be classified as a show
        let files = vec![
            TorrentFile {
                id: 1,
                path: "/Movie.2023.mkv".to_string(),
                bytes: 10_000_000_000,
                selected: 1,
            },
            TorrentFile {
                id: 2,
                path: "/Making.Of.mkv".to_string(),
                bytes: 500_000_000,
                selected: 0,
            },
            TorrentFile {
                id: 3,
                path: "/Commentary.mkv".to_string(),
                bytes: 8_000_000_000,
                selected: 0,
            },
        ];
        assert!(
            !is_show_guess(&files),
            "Movie with unselected extras should not be guessed as show"
        );
    }

    #[test]
    fn is_show_guess_ignores_an_unselected_episode_coded_file() {
        // An unselected file is never shown, so its episode code must not make the one selected
        // film a show.
        let mut files = vec![
            TorrentFile {
                id: 1,
                path: "/Movie.2023.1080p.mkv".to_string(),
                bytes: 4_000_000_000,
                selected: 1,
            },
            TorrentFile {
                id: 2,
                path: "/Extras/Extras.S01E01.mkv".to_string(),
                bytes: 300_000_000,
                selected: 0,
            },
        ];
        assert!(
            !is_show_guess(&files),
            "an unselected episode-coded extra must not make a selected film a show"
        );

        // With nothing selected yet every file counts, so the episode code still registers.
        files[0].selected = 0;
        assert!(is_show_guess(&files));
    }

    #[test]
    fn is_show_guess_multiple_selected_videos_is_show() {
        // A show torrent with multiple selected video files
        let files = vec![
            TorrentFile {
                id: 1,
                path: "/Episode1.mkv".to_string(),
                bytes: 1_000_000_000,
                selected: 1,
            },
            TorrentFile {
                id: 2,
                path: "/Episode2.mkv".to_string(),
                bytes: 1_000_000_000,
                selected: 1,
            },
        ];
        assert!(
            is_show_guess(&files),
            "Multiple selected video files should be guessed as show"
        );
    }

    #[test]
    fn is_show_guess_episode_pattern_detected() {
        let files = vec![TorrentFile {
            id: 1,
            path: "/Show.S01E01.mkv".to_string(),
            bytes: 1_000_000_000,
            selected: 1,
        }];
        assert!(
            is_show_guess(&files),
            "Episode pattern in filename should be guessed as show"
        );
    }

    fn one_file(name: &str) -> Vec<TorrentFile> {
        vec![TorrentFile {
            id: 1,
            path: format!("/{name}"),
            bytes: 1_000_000_000,
            selected: 1,
        }]
    }

    #[test]
    fn is_show_guess_detects_real_episode_markers() {
        for name in [
            "Show.S01E01.1080p.mkv",
            "Show.S1.mkv",
            "Show.1x01.mkv",
            "Show.E05.mkv",
            "Show.Season 2.mkv",
            // Contiguous multi-episode + version markers must still register as shows (the SxxExx
            // alternative is intentionally not trailing-anchored).
            "Friends.S10E17E18.720p.mkv",
            "Show.S01E01E02.1080p.mkv",
            "Anime.S01E13v2.mkv",
            // Underscore-separated codes: the regex crate's `\b` counts `_` as a word character, so
            // these read as movies while `vfs::parse_se_all` read the episode.
            "Some_Show_S01E05_720p_HDTV.mkv",
            "some_show_s01e05.mkv",
            "Some_Show_1x05_720p.mkv",
            "Some_Show_Season_2_E05.mkv",
            // The short episode word, separated from its number.
            "Some Show Ep 12 [720p].mkv",
            "Some.Show.Ep.05.mkv",
            // A dotted season word, as `clean_name` and `names_a_title` read it.
            "Show.Name.Season.2.Episode.5.mkv",
            "Show.Temporada.2.mkv",
            // An `NxM` code after a title ending in a digit, or after a year, is no channel layout.
            "Babylon.5.2x10.mkv",
            "Show.Name.2005.1x01.mkv",
        ] {
            assert!(is_show_guess(&one_file(name)), "{name} should be a show");
        }
    }

    #[test]
    fn is_show_guess_detects_a_trailing_episode_number() {
        // The absolute-numbered anime form names a single episode file by a ` - NN` number alone.
        // Read as a movie, the title check picked a same-named film over the series and rejected
        // the release, and the account mirror recorded the user's episode as that film.
        for name in [
            "[SubsPlease] Some Show - 12 (1080p) [ABCD1234].mkv",
            "[Group] Some Show - 05v2 [1080p].mkv",
            "Some Show - Ep01 [720p].mkv",
            "Some_Show_-_12_(1080p).mkv",
        ] {
            assert!(is_show_guess(&one_file(name)), "{name} should be a show");
        }
        // `trailing_episode_number` reads none of these: a release year, a number followed by a
        // bracketed release year (a numeric film title), a single-digit sequel, and a number a
        // second ` - ` follows.
        for name in [
            "Dr.No - 1962.mkv",
            "Some Show - 12 (2008).mkv",
            "Some Film - 2.mkv",
            "Some Show - 12 - Title (1080p).mkv",
        ] {
            assert!(
                !is_show_guess(&one_file(name)),
                "{name} should NOT be a show"
            );
        }
    }

    /// A single-file absolute-numbered episode whose series TMDB lists under another title (or
    /// beside a same-titled film) is undecided by title and year, so `select_best_match` falls back
    /// on the show guess. Guessed a movie, the film won and the title check rejected the release.
    #[tokio::test]
    async fn a_single_absolute_numbered_episode_is_identified_as_the_show() {
        use crate::acquire::{TitleCheck, TitleValidator, TmdbTitleValidator};
        use crate::scraper::MediaKind;
        let name = "[Group] Some Show - 12 (1080p).mkv";
        let files = one_file(name);
        for answers in SHOW_OR_FILM {
            let (server, tmdb) = some_show_tmdb(answers).await;
            let found = identify_name(name, &files, &tmdb).await;
            assert!(
                matches!(
                    &found,
                    Identification::Identified(m)
                        if m.media_type == MediaType::Show
                            && m.external_id.as_deref() == Some("tmdb:11")
                ),
                "expected the series, got {found:?} (requests {:?})",
                server.requests()
            );
            // The account mirror identifies the user's single-episode torrent as the series.
            let mirrored = identify_torrent(&single_file_torrent(name), &tmdb).await;
            assert_eq!(mirrored.media_type, MediaType::Show);
            assert_eq!(mirrored.external_id.as_deref(), Some("tmdb:11"));
            // The acquisition title check accepts the release for its episode.
            let validator = TmdbTitleValidator {
                tmdb: std::sync::Arc::new(tmdb),
            };
            assert_eq!(
                validator
                    .validate(name, name, 11, MediaKind::Series, Some(1), Some(12))
                    .await,
                TitleCheck::Match
            );
        }
    }

    /// The two TMDB answers to a `Some Show` search that leave the show-versus-film choice to the
    /// show guess: a series under a longer title beside a franchise film (neither exact), and a
    /// series beside a same-titled film (both exact). As `(tv, movie)` search bodies.
    const SHOW_OR_FILM: [(&str, &str); 2] = [
        (
            r#"{"results":[{"id":11,"title":"Some Show: Rising","original_title":"Rising",
                "release_date":"2019-04-06","popularity":30.0,"vote_count":900,"vote_average":8.5}]}"#,
            r#"{"results":[{"id":22,"title":"Some Show the Movie: Train","release_date":"2020-10-16",
                "popularity":30.0,"vote_count":900,"vote_average":8.5}]}"#,
        ),
        (
            r#"{"results":[{"id":11,"title":"Some Show","release_date":"2019-04-06",
                "popularity":30.0,"vote_count":900,"vote_average":8.5}]}"#,
            r#"{"results":[{"id":22,"title":"Some Show","release_date":"2020-10-16",
                "popularity":30.0,"vote_count":900,"vote_average":8.5}]}"#,
        ),
    ];

    /// A loopback TMDB answering a `Some Show` search with `(tv, movie)`.
    async fn some_show_tmdb(
        (tv, movie): (&str, &str),
    ) -> (crate::trakt_client::test_http::Server, TmdbClient) {
        use crate::trakt_client::test_http::{spawn, Reply};
        let server = spawn(vec![
            ("/search/tv?api_key=k&query=Some+Show", Reply::json(tv)),
            (
                "/search/movie?api_key=k&query=Some+Show",
                Reply::json(movie),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        (server, tmdb)
    }

    /// An episode code glued to the title by underscores is a show marker, as `vfs::parse_se_all`
    /// reads it. The regex crate's `\b` counts `_` as a word character, so the show guess used to
    /// call the file a movie: the same-titled film won, the title check rejected the release, and
    /// the account mirror recorded the user's episode as the film.
    #[tokio::test]
    async fn an_underscore_named_episode_is_identified_as_the_show() {
        use crate::acquire::{TitleCheck, TitleValidator, TmdbTitleValidator};
        use crate::scraper::MediaKind;
        let name = "Some_Show_S01E05_720p_HDTV.mkv";
        for answers in SHOW_OR_FILM {
            let (server, tmdb) = some_show_tmdb(answers).await;
            let found = identify_name(name, &one_file(name), &tmdb).await;
            assert!(
                matches!(
                    &found,
                    Identification::Identified(m)
                        if m.media_type == MediaType::Show
                            && m.external_id.as_deref() == Some("tmdb:11")
                ),
                "expected the series, got {found:?} (requests {:?})",
                server.requests()
            );
            let mirrored = identify_torrent(&single_file_torrent(name), &tmdb).await;
            assert_eq!(mirrored.media_type, MediaType::Show);
            assert_eq!(mirrored.external_id.as_deref(), Some("tmdb:11"));
            let validator = TmdbTitleValidator {
                tmdb: std::sync::Arc::new(tmdb),
            };
            assert_eq!(
                validator
                    .validate(name, name, 11, MediaKind::Series, Some(1), Some(5))
                    .await,
                TitleCheck::Match
            );
        }
    }

    /// A show request's file may name its episode by an episode word the show guess doesn't count
    /// (`Episode 12`: films are named that way too). The title check knows it wants a show, so the
    /// same-titled film can't win there, while plain identification (the account mirror) still
    /// guesses a film for such a name.
    #[tokio::test]
    async fn a_show_request_identifies_an_episode_word_file_as_the_show() {
        use crate::acquire::{TitleCheck, TitleValidator, TmdbTitleValidator};
        use crate::scraper::MediaKind;
        for name in ["Some Show Ep 12 [720p].mkv", "Some Show Episode 12.mkv"] {
            for answers in SHOW_OR_FILM {
                let (server, tmdb) = some_show_tmdb(answers).await;
                let validator = TmdbTitleValidator {
                    tmdb: std::sync::Arc::new(tmdb),
                };
                assert_eq!(
                    validator
                        .validate(name, name, 11, MediaKind::Series, Some(1), Some(12))
                        .await,
                    TitleCheck::Match,
                    "{name} (requests {:?})",
                    server.requests()
                );
            }
        }
        // Without the hint, a bare `Episode N` is no show marker: with both titles exact, the film.
        let (_server, tmdb) = some_show_tmdb(SHOW_OR_FILM[1]).await;
        let name = "Some Show Episode 12.mkv";
        assert!(matches!(
            identify_name(name, &one_file(name), &tmdb).await,
            Identification::Identified(m) if m.media_type == MediaType::Movie
        ));
    }

    /// A file name that names no title (`E09 - Title.mkv`, `S01E05.mkv`) is identified by its folder
    /// or its torrent's name; one that does is identified by itself, whatever the torrent is called.
    #[tokio::test]
    async fn the_title_check_identifies_a_title_less_file_by_its_folder_or_torrent() {
        use crate::acquire::{TitleCheck, TitleValidator, TmdbTitleValidator};
        use crate::scraper::MediaKind;
        use crate::trakt_client::test_http::{spawn, Reply};
        const NONE: &str = r#"{"results":[]}"#;
        let server = spawn(vec![
            (
                "/search/tv?api_key=k&query=Some+Show",
                Reply::json(
                    r#"{"results":[{"id":11,"title":"Some Show","release_date":"2019-04-06",
                        "popularity":30.0,"vote_count":900,"vote_average":8.5}]}"#,
                ),
            ),
            ("/search/movie?api_key=k&query=Some+Show", Reply::json(NONE)),
            (
                "/search/tv?api_key=k&query=Other+Show",
                Reply::json(
                    r#"{"results":[{"id":33,"title":"Other Show","release_date":"2015-01-01",
                        "popularity":30.0,"vote_count":900,"vote_average":8.5}]}"#,
                ),
            ),
            (
                "/search/movie?api_key=k&query=Other+Show",
                Reply::json(NONE),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        let validator = TmdbTitleValidator {
            tmdb: std::sync::Arc::new(tmdb),
        };
        for (path, torrent, (season, episode), verdict) in [
            // TorBox: the path starts with the torrent folder; `Season 04` names no title.
            (
                "Some.Show.S04.1080p/Season 04/E09 - Title.mkv",
                "Some.Show.S04.1080p",
                (4, 9),
                TitleCheck::Match,
            ),
            // Real-Debrid: no torrent folder in the path, so the torrent name.
            (
                "/Season 04/E09 - Title.mkv",
                "Some Show S04 1080p",
                (4, 9),
                TitleCheck::Match,
            ),
            (
                "/S01E05.mkv",
                "Other Show S01 1080p",
                (1, 5),
                TitleCheck::Mismatch,
            ),
            // A file name that names a title is identified by itself.
            (
                "Other.Show.S01E05.mkv",
                "Some.Show.S01.1080p",
                (1, 5),
                TitleCheck::Mismatch,
            ),
            (
                "Some.Show.S01.1080p/Some.Show.S01E05.mkv",
                "Other Show S01",
                (1, 5),
                TitleCheck::Match,
            ),
            // A `Series 1` or `Staffel 2` season folder names no title, so the torrent folder.
            (
                "Some.Show.S01.1080p/Series 1/S01E05.mkv",
                "Some.Show.S01.1080p",
                (1, 5),
                TitleCheck::Match,
            ),
            (
                "Some.Show.S02.1080p/Staffel 2/S02E05.mkv",
                "Some.Show.S02.1080p",
                (2, 5),
                TitleCheck::Match,
            ),
        ] {
            assert_eq!(
                validator
                    .validate(
                        path,
                        torrent,
                        11,
                        MediaKind::Series,
                        Some(season),
                        Some(episode)
                    )
                    .await,
                verdict,
                "{path} in {torrent} (requests {:?})",
                server.requests()
            );
        }
    }

    /// The name a file is identified by: the file name when it names a title, else the
    /// nearest directory that does (season, part and disc folders skipped), else the torrent name,
    /// else the file name as before.
    /// The scan loop and the account mirror identify a torrent whose largest file names no title
    /// (`S04E09 - The Reunion.mkv`, `E09 - The Reunion.mkv`) by its folder or its name, not by
    /// what `clean_name` leaves of the file name, the episode title, which TMDB's fuzzy search
    /// matches to some other title. A file that names a title is still identified by itself.
    #[tokio::test]
    async fn a_torrent_whose_file_names_no_title_is_identified_by_its_folder_or_name() {
        use crate::trakt_client::test_http::{spawn, Reply};
        const NONE: &str = r#"{"results":[]}"#;
        let tv = |id: u64, title: &str| {
            Reply::json(&format!(
                r#"{{"results":[{{"id":{id},"title":"{title}","release_date":"2019-04-06",
                    "popularity":30.0,"vote_count":900,"vote_average":8.5}}]}}"#
            ))
        };
        let server = spawn(vec![
            ("/search/tv?api_key=k&query=Some+Show", tv(11, "Some Show")),
            ("/search/movie?api_key=k&query=Some+Show", Reply::json(NONE)),
            (
                "/search/tv?api_key=k&query=The+Reunion",
                tv(99, "The Reunion"),
            ),
            (
                "/search/movie?api_key=k&query=The+Reunion",
                Reply::json(NONE),
            ),
            (
                "/search/tv?api_key=k&query=Other+Show",
                tv(33, "Other Show"),
            ),
            (
                "/search/movie?api_key=k&query=Other+Show",
                Reply::json(NONE),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        let torrent = |path: &str| TorrentInfo {
            id: "t1".to_string(),
            filename: "Some.Show.S04.1080p".to_string(),
            original_filename: "Some.Show.S04.1080p".to_string(),
            status: "downloaded".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: path.to_string(),
                bytes: 2_000_000_000,
                selected: 1,
            }],
            ..Default::default()
        };
        for (path, expected) in [
            // TorBox: the path starts with the torrent folder.
            ("Some.Show.S04.1080p/S04E09 - The Reunion.mkv", "tmdb:11"),
            // Real-Debrid: no torrent folder in the path, so the torrent name.
            ("/E09 - The Reunion.mkv", "tmdb:11"),
            // A file name that names a title is identified by itself.
            ("/Other.Show.S04E09.mkv", "tmdb:33"),
        ] {
            let found = identify_torrent(&torrent(path), &tmdb).await;
            assert_eq!(found.media_type, MediaType::Show, "{path}");
            assert_eq!(
                found.external_id.as_deref(),
                Some(expected),
                "{path} (requests {:?})",
                server.requests()
            );
        }
        assert!(
            !server.requests().iter().any(|r| r.contains("The+Reunion")),
            "the episode title is never searched: {:?}",
            server.requests()
        );
    }

    /// A single-digit film title that comes with its release year (`9` from 2009) is no generic
    /// file name such as `1.mkv`: it is searched with the year, and the short-title rule accepts
    /// only an exact title released that year. Read as generic, every release of the film was
    /// rejected as WrongTitle and blacklisted, and the account mirror recorded no id for it.
    #[tokio::test]
    async fn a_single_digit_title_with_a_year_is_searched() {
        use crate::acquire::{TitleCheck, TitleValidator, TmdbTitleValidator};
        use crate::scraper::MediaKind;
        use crate::trakt_client::test_http::{spawn, Reply};
        let server = spawn(vec![
            (
                "/search/movie?api_key=k&query=9&primary_release_year=2009",
                Reply::json(
                    r#"{"results":[{"id":900001,"title":"9","release_date":"2009-08-19",
                        "popularity":30.0,"vote_count":900,"vote_average":7.0}]}"#,
                ),
            ),
            (
                "/search/tv?api_key=k&query=9&first_air_date_year=2009",
                Reply::json(r#"{"results":[]}"#),
            ),
        ])
        .await;
        let tmdb = TmdbClient::with_base_url("k".into(), server.base_url.clone()).unwrap();
        let name = "9.2009.1080p.BluRay.x264-GRP.mkv";
        let found = identify_name(name, &one_file(name), &tmdb).await;
        assert!(
            matches!(
                &found,
                Identification::Identified(m)
                    if m.media_type == MediaType::Movie
                        && m.external_id.as_deref() == Some("tmdb:900001")
            ),
            "expected the film, got {found:?} (requests {:?})",
            server.requests()
        );
        let validator = TmdbTitleValidator {
            tmdb: std::sync::Arc::new(tmdb),
        };
        assert_eq!(
            validator
                .validate(name, name, 900001, MediaKind::Movie, None, None)
                .await,
            TitleCheck::Match
        );
        // A bare digit with no year is still a generic file name, never searched.
        let quiet = spawn(vec![]).await;
        let tmdb = TmdbClient::with_base_url("k".into(), quiet.base_url.clone()).unwrap();
        assert!(matches!(
            identify_name("1.mkv", &one_file("1.mkv"), &tmdb).await,
            Identification::NoMatch
        ));
        assert!(quiet.requests().is_empty(), "{:?}", quiet.requests());
    }

    #[test]
    fn identification_name_falls_back_to_the_folder_then_the_torrent() {
        for (path, torrent, expected) in [
            (
                "Show.S04.1080p/Season 04/E09 - Title.mkv",
                "Show.S04.1080p",
                "Show.S04.1080p",
            ),
            (
                "/Season 04/E09 - Title.mkv",
                "Show S04 1080p",
                "Show S04 1080p",
            ),
            ("Show.S01/S01E05.mkv", "", "Show.S01"),
            (
                "[Group] Show (Season 2)/05.mkv",
                "x",
                "[Group] Show (Season 2)",
            ),
            ("Show/Disc 1/Part 2/E01.mkv", "", "Show"),
            // A file name that names a title is kept, whatever the folder or torrent is called.
            (
                "Pack/Other.Show.S01E05.mkv",
                "Show",
                "Other.Show.S01E05.mkv",
            ),
            // Nothing names a title: the file name, as before.
            ("/S01E05.mkv", "S01", "S01E05.mkv"),
            ("/S01E05.mkv", "", "S01E05.mkv"),
        ] {
            assert_eq!(
                identification_name(path, torrent),
                expected,
                "{path} in {torrent:?}"
            );
        }
    }

    #[test]
    fn names_a_title_reads_the_text_before_the_numbering() {
        for name in [
            "Show.S01E05.mkv",
            "[Group] Show - 05 [1080p].mkv",
            "Movie (2019).mkv",
            "[REC] (2007).mkv",
            "Some.Show.S04.1080p",
            "Show_Name_S01E05.mkv",
            "1917.2019.1080p.mkv",
            "Se7en.mkv",
            // Films whose titles open with a number, which more title words or a year follow.
            "12.Angry.Men.1957.mkv",
            "300.2006.1080p.BluRay.mkv",
            "10x10.2018.1080p.mkv",
            "28 Days Later (2002).mkv",
        ] {
            assert!(names_a_title(name), "{name} names a title");
        }
        for name in [
            "E09 - Title.mkv",
            "Episode 9.mkv",
            "Ep.05.mkv",
            "S01E05.mkv",
            "S01E05 - Pilot.mkv",
            "1x05.mkv",
            "1x05 - Title.mkv",
            "05.mkv",
            "05.1080p.mkv",
            "05 - Title.mkv",
            "[Group] 05 [1080p].mkv",
            "[1080p].mkv",
            "1080p.WEB.mkv",
            "Season 04",
            "S04",
            "Season.2",
            "Part 2",
            "Disc 1",
            "CD1",
            "",
            // Season folders in other spellings: the UK `Series N`, German, Italian, Polish and
            // Dutch, and a season number spelled out.
            "Series 1",
            "Series.2",
            "Staffel 2",
            "Stagione 1",
            "Sezon 1",
            "Seizoen 1",
            "Season One",
            "Series Two",
            "Temporada 2",
            "Saison 3",
        ] {
            assert!(!names_a_title(name), "{name} names no title");
        }
        // A film whose title opens with such a word names no title by this rule either, so it is
        // identified by its own file name when nothing else names a title.
        assert_eq!(
            identification_name(
                "Series.7.The.Contenders.2001/Series.7.The.Contenders.2001.mkv",
                "Series.7.The.Contenders.2001"
            ),
            "Series.7.The.Contenders.2001.mkv"
        );
    }

    #[test]
    fn is_show_guess_ignores_small_sample_second_video() {
        // TorBox marks EVERY file `selected: 1`, so a movie + a small sample/featurette would be
        // two "selected" videos. The size floor must keep this classified as a MOVIE.
        let files = vec![
            TorrentFile {
                id: 1,
                path: "/Movie.2023.1080p.mkv".to_string(),
                bytes: 8_000_000_000,
                selected: 1,
            },
            TorrentFile {
                id: 2,
                path: "/sample.mkv".to_string(),
                bytes: 40_000_000, // 40 MB sample — below the substantial-video floor
                selected: 1,
            },
        ];
        assert!(
            !is_show_guess(&files),
            "a movie + small sample (both 'selected' on TorBox) must not be guessed as a show"
        );
    }

    #[test]
    fn is_show_guess_does_not_misclassify_movies() {
        // A resolution (1920x1080) must not trip the NxM episode form, and an embedded `e7`
        // (Se7en) must not trip the lone-episode form. Aspect-ratio tags (16x9 / 4x3 / 21x9)
        // must likewise not trip it — their single-digit second number isn't a zero-padded episode.
        for name in [
            "Se7en.1995.1080p.BluRay.x264.mkv",
            "Movie.1920x1080.HDR.mkv",
            "Code8.2019.2160p.mkv",
            "Some.Movie.2001.16x9.DVDRip.mkv",
            "Old.Cartoon.4x3.mkv",
            "Ultrawide.Film.21x9.mkv",
            // A movie whose *title* is written NxM (10x10 [2018], 24x36 [2019]) must not be
            // guessed as a show even though the NxM alternative matches — the title is the whole
            // cleaned name (only a year/quality metadata follows), unlike a real episode marker.
            "10x10.2018.1080p.BluRay.x264.mkv",
            "24x36.2019.1080p.WEBRip.mkv",
            "10x10.1080p.mkv",
            // The same with underscores, which the show markers read as spaces.
            "Se7en_1995_1080p_BluRay_x264.mkv",
            "Movie_1920x1080_HDR.mkv",
            "10x10_2018_1080p_BluRay.mkv",
            // A film named by a bare `Episode N`, and a sequel number after a word ending in `ep`.
            "Star.Wars.Episode.4.mkv",
            "Step Up 3.mkv",
            "Step.2.mkv",
            // A channel layout run into its codec is no `NxM` code (nor to `vfs::parse_se_all`).
            "Movie.2019.1080p.WEB-DL.DD5.1x264-GRP.mkv",
            "Movie.AAC2.0x264-GRP.mkv",
            "Movie_2019_DD5.1x265.mkv",
        ] {
            assert!(
                !is_show_guess(&one_file(name)),
                "{name} should NOT be a show"
            );
        }
    }
}
