use crate::provider::FileLocator;
use crate::rd_client::TorrentInfo;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Resolved live-selection for `build`: slot key (`store::movie_slot`/`episode_slot`) ->
/// `SelectionEntry`. A slot with no managed selection (or a stale one) gets the pre-SP3 behaviour,
/// so external / un-managed torrents are unaffected: a movie falls back to its largest streamable
/// torrent, and an unmanaged show episode keeps every distinct file name, same-named copies
/// resolving to the largest torrent. A multi-episode file is hidden only when every episode it
/// carries is selected from another present copy.
pub type SelectionMap = std::collections::HashMap<String, crate::store::SelectionEntry>;

/// Parse ALL `(season, episode)` pairs a file name declares: a single `SxxEyy` (also with one `.`,
/// space or `_` before the `E`: `S01.E05`, `S01 E05`), a single `NxM` (`2x05`), their MULTI-episode
/// forms (`S01E01E02`, `S01E01-E02`, `S01E01.E02`, Sonarr's "Extend" `S01E01-02`, its "Duplicate"
/// `S01E01.S01E02` / `S01E01 - S01E02`, `1x01x02`, `1x01-1x02`, `1x01-02`), or the
/// dash-concatenated absolute code (`Show - 409 - Title` → `[(4, 9)]`). Empty when no episode code
/// is found. The first element is the file's FIRST code (`parse_se`).
///
/// The VFS build uses every code for its selection gate (a double-episode file stays visible while
/// any episode it carries is not served by another copy), and the first one for season placement.
/// The acquisition engine, the upgrade engine and the scan loop read episodes through it too
/// ([`episode_files`], [`show_episode_files`]): a show's `provides`, the `selection` slots
/// `observe` writes and the title check's episode test, so for a coded file the VFS and the engines
/// agree on which file supplies which episode. A file with no code is mapped to the episode its
/// name gives by [`codeless_episode_files`], which the build gates such a file by too, so this
/// module holds every rule that reads a torrent's files as episodes. A multi-episode file
/// contributes every episode it carries: reporting only the first would leave the second looking
/// un-owned, and `monitor_episodes` would re-acquire it as a duplicate singleton. The `NxM` and
/// dash-code arms exist for the same reason: without them a pack named `Show - 2x05 - Title` or
/// `Show - 409 - Title` derives an empty `provides`, so the reconciler sees none of its episodes as
/// owned and re-acquires the whole show as per-episode torrents (permanent duplicates), and
/// `observe` finds no file for the episode it added the pack for and rejects a correct release as
/// `EpisodeMissing`.
///
/// The `SxxEyy` arm is deliberately NOT word-anchored (unlike `SEASON_RE`); a literal `e` must
/// follow the season digits, directly or after one separator, so a mid-token match is very unlikely
/// in practice. Each continuation REQUIRES an explicit `e` before its number so a resolution/codec
/// token can't be mis-parsed as an episode (`S01E01.1080p` → the `.1080` has no `e`, so it is NOT a
/// continuation), except a dash-only one directly after the code (`S01E01-02-03`), which counts only
/// when it is a whole number naming the NEXT episode (so `S01E05-720p` and `S01E09-10bit` don't) and
/// is not the whole part of a channel layout: a single digit followed by `.` and one more digit
/// (`S01E04-5.1`, `S01E01-2.0`), while `S01E01-02.1080p` still counts. After those, a separated full
/// code of the SAME season (`.S01E02`, `-S01E02`, ` - S01E02`) continues the run too, as `1x01-1x02`
/// does for `NxM`; another season's code or an episode 00 ends it.
///
/// The `NxM` arm, tried when there is no `SxxEyy` code, takes identification's bounds: a 1-2 digit
/// season and a 2-3 digit episode (never `00`) with an optional version suffix (`1x01v2`, read as
/// `S01E01v2` is), bounded on both sides by a non-alphanumeric character or the string edge, so `_`
/// separates it as `.` does while a pixel size (`1920x1080`), an aspect ratio (`16x9`) and a codec
/// (`x264`) are no codes. Nor is a channel layout run into a codec (`DD5.1x264`): a season digit
/// right after `<digit>.` whose `x` runs into `264`/`265` is skipped, while a real code after a
/// year or a title ending in a digit (`Show.Name.2005.1x01`, `Babylon.5.2x10`) is read. Its
/// continuations are a repeated `xNN` (`1x01x02`), a separated code of the SAME season
/// (`1x01-1x02`; another season's code ends the run) and the dash-only one above (`1x01-02`).
///
/// An `NxM` or separated `S01.E01` code followed by a range to another episode (`1x01-10`,
/// `1x01~10`, `S01.E01-10`, `S01.E01-S01.E10`: `opens_episode_range`) decodes to nothing, as these
/// forms did before they were read: its first code alone would make a pack named this way read as a
/// single episode (`upgrade::is_single_episode_stream`, whose range test knows only the contiguous
/// `S01E01-10` form). An explicit second code (`1x01-1x10`, `S01.E01-E10`) gives the two endpoints,
/// as `S01E01-E10` and `S01E01-S01E10` do.
///
/// KNOWN LIMITATIONS: a RANGE form spanning more than two episodes yields only the explicit
/// endpoints (`S01E01-E03` → `[(1,1),(1,3)]`) or, without the `e`, only the first episode
/// (`S01E01-03` → `[(1,1)]`), so the middle ones may be re-acquired as duplicates (such single
/// files are uncommon); and a 3-digit dash code is decoded as concatenated `SxxEyy`, so an
/// absolute (anime) number (`One Piece - 409 - …`) reads as a phantom `(4, 9)`, which can suppress
/// re-acquiring the real S04E09 and write a wrong selection slot — accepted, since concatenated
/// Western packs are far more common and an empty `provides` was worse. Exactly 3 digits (season
/// 1–9) are accepted, so a 4-digit year/date token (`Show - 2024 - Title`) never decodes, and an
/// episode-00 decode is rejected. The dash code needs SPACE-dash-SPACE on both sides, so a dotted
/// scene form (`Show.-.409.-.Title`) yields no code at all (an empty `provides` for such a file), as
/// does other absolute numbering (`Show - 041 [1080p]`) and an episode word with no season code
/// (`E09 - Title.mkv`, `Episode 9.mkv`). A file with no code that names its episode by an episode
/// word or a trailing ` - NN` number is still mapped ([`codeless_episode_files`], read by
/// [`show_episode_files`]): a single-file release in the season the VFS places it in (season 1
/// unless the name or a directory names one), and a file of a multi-file pack only under an
/// explicit season (a season token in its name or a season directory, not a `Part N` in the file
/// name, nor a season range), and nothing when another selected file names the same episode. The
/// dotted dash form, whose number another ` - ` follows once its dots are read as spaces, stays
/// rejected as `EpisodeMissing`, as do an absolutely numbered pack with no explicit season and an
/// absolute number past season 1 (`Show - 30` for S02E05): neither names an episode there.
pub(crate) fn parse_se_all(name: &str) -> Vec<(u32, u32)> {
    if let Some(eps) = sxxeyy_codes(name).or_else(|| nxm_codes(name)) {
        return eps;
    }
    dash_code(name).map(|(_, se)| vec![se]).unwrap_or_default()
}

/// `parse_se_all`'s dash-concatenated arm (`Show - 409 - Title` → `(4, 9)`): the first three digits
/// between SPACE-dash-SPACE pairs, read as season and episode, with the match of those digits.
/// `None` when there is no such number, or it decodes to season or episode 0. `episode_code_span`
/// reads it too, so `is_video_file` and the episode reader agree on the form.
fn dash_code(name: &str) -> Option<(regex::Match<'_>, (u32, u32))> {
    static DASH_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s-\s(\d{3})\s-\s").unwrap());
    let digits = DASH_CODE.captures(name)?.get(1)?;
    let code = digits.as_str().parse::<u32>().ok()?;
    let (season, episode) = (code / 100, code % 100);
    (season >= 1 && episode >= 1).then_some((digits, (season, episode)))
}

/// An `SxxEyy` code, with one optional separator before the `E` (group 2), and its `E`-prefixed
/// continuations (group 4). `is_video_file`'s episode test uses it too.
static SXXEYY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)s(\d{1,2})([ ._]?)e(\d{1,3})((?:[-_. ]?e\d{1,3})*)").unwrap()
});

/// An `NxM` code (`2x05`, with an optional version suffix: `1x01v2`, as the `SxxEyy` arm reads
/// `S01E01v2`) and its repeated (`x02`) or separated (`-1x02`) continuations (group 3),
/// bounded by a non-alphanumeric character or the string edge on both sides. The regex crate has
/// no look-around, so these boundaries consume a character; `find_nxm` adds the checks a regex
/// can't make.
static NXM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[^a-z0-9])(\d{1,2})x(\d{2,3})(?:v\d{1,2})?((?:x\d{2,3}|[-_. ]\d{1,2}x\d{2,3})*)(?:[^a-z0-9]|$)",
    )
    .unwrap()
});

/// `parse_se_all`'s `SxxEyy` arm: `None` when `name` has no such code, or when a separated one
/// (`S01.E01`) opens a range.
fn sxxeyy_codes(name: &str) -> Option<Vec<(u32, u32)>> {
    static EP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)e(\d{1,3})").unwrap());
    let c = SXXEYY_RE.captures(name)?;
    // The `\d{1,2}`/`\d{1,3}` captures always parse to `u32`.
    let s = c.get(1)?.as_str().parse::<u32>().ok()?;
    let e1 = c.get(3)?.as_str().parse::<u32>().ok()?;
    let mut eps = vec![(s, e1)];
    for cap in EP.captures_iter(c.get(4).map_or("", |m| m.as_str())) {
        if let Some(e) = cap.get(1).and_then(|m| m.as_str().parse::<u32>().ok()) {
            if !eps.contains(&(s, e)) {
                eps.push((s, e));
            }
        }
    }
    let tail = extend_with_dash_next(c.get(0).map_or("", |m| &name[m.end()..]), s, &mut eps);
    let separated = c.get(2).is_some_and(|m| !m.as_str().is_empty());
    if separated && opens_episode_range(tail) {
        return None;
    }
    extend_with_same_season_codes(tail, s, &mut eps);
    Some(eps)
}

/// Extend `eps` (all of season `s`) with the separated full codes of the same season that open
/// `tail`: Sonarr's "Duplicate" multi-episode style (`S01E01.S01E02`, `S01E01-S01E02`,
/// `S01E01 - S01E02`), as the `NxM` arm reads `1x01-1x02`. Another season's code or an episode 00
/// ends the run.
fn extend_with_same_season_codes(mut tail: &str, s: u32, eps: &mut Vec<(u32, u32)>) {
    static NEXT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:\s-\s|[-_. ])s(\d{1,2})[ ._]?e(\d{1,3})(?:[^0-9]|$)").unwrap()
    });
    while let Some(next) = NEXT.captures(tail) {
        let season = next.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
        let episode = next.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
        let (Some(season), Some(e), Some(digits)) = (season, episode, next.get(2)) else {
            break;
        };
        if season != s || e == 0 {
            break;
        }
        if !eps.contains(&(s, e)) {
            eps.push((s, e));
        }
        tail = &tail[digits.end()..];
    }
}

/// `parse_se_all`'s `NxM` arm: `None` when `name` has no such code, or when it opens a range.
fn nxm_codes(name: &str) -> Option<Vec<(u32, u32)>> {
    static CONT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:x|[-_. ](\d{1,2})x)(\d{2,3})").unwrap());
    let c = find_nxm(name)?;
    let s = c.get(1)?.as_str().parse::<u32>().ok()?;
    let e1 = c.get(2)?.as_str().parse::<u32>().ok()?;
    let mut eps = vec![(s, e1)];
    let continuations = c.get(3)?;
    let mut end = continuations.start();
    for cap in CONT.captures_iter(continuations.as_str()) {
        let season = cap
            .get(1)
            .map_or(Some(s), |m| m.as_str().parse::<u32>().ok());
        let episode = cap.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
        let (Some(season), Some(e), Some(whole)) = (season, episode, cap.get(0)) else {
            break;
        };
        // Another season's code ends the run.
        if season != s || e == 0 {
            break;
        }
        if !eps.contains(&(s, e)) {
            eps.push((s, e));
        }
        end = continuations.start() + whole.end();
    }
    let tail = extend_with_dash_next(&name[end..], s, &mut eps);
    if opens_episode_range(tail) {
        return None;
    }
    Some(eps)
}

/// The first `NXM_RE` match in `name` that is an episode code: not episode `00`, and not a channel
/// layout run into a codec (`DD5.1x264`, `is_channel_layout_codec`).
fn find_nxm(name: &str) -> Option<regex::Captures<'_>> {
    let mut at = 0;
    while let Some(c) = NXM_RE.captures_at(name, at) {
        let season = c.get(1)?;
        let channel_layout = is_channel_layout_codec(name, &season);
        let episode_00 = c
            .get(2)
            .is_some_and(|m| m.as_str().bytes().all(|b| b == b'0'));
        if !channel_layout && !episode_00 {
            return Some(c);
        }
        // The next match's leading boundary is a non-alphanumeric character, so it lies after the
        // season digits and their `x`. Resuming at the `x` always advances, also when this match
        // began at the start of the name (`1x00.mkv`), where resuming on the season digit found the
        // same match again forever.
        at = season.end();
    }
    None
}

/// Whether the `NxM`/`NxNN` token whose season digits are `season` (a match in `s` that ends at
/// the token's `x`) is a channel layout run into an `x264`/`x265` codec (`DD5.1x264`,
/// `AAC2.0x265`): its season digit follows `<digit>.` and the digits after the `x` are exactly
/// `264` or `265`. Only that run makes a channel layout read as such a token, so a real code after
/// a year or a title ending in a digit (`Show.Name.2005.1x01`, `Babylon.5.2x10`, `24.3x05`) is no
/// channel layout. `find_nxm` and `season_tokens` both skip such a token, so the episode reader and
/// the season placement agree on it.
fn is_channel_layout_codec(s: &str, season: &regex::Match<'_>) -> bool {
    is_channel_layout_codec_at(s, season.start(), season.end())
}

/// [`is_channel_layout_codec`] for the token whose season digits span `start..x` of `s`, `x` being
/// the offset of its `x`. `pub(crate)`: identification's show guess skips such a token as an `NxM`
/// marker, so it reads `DD5.1x264` as no episode code, as the episode reader does.
pub(crate) fn is_channel_layout_codec_at(s: &str, start: usize, x: usize) -> bool {
    let bytes = s.as_bytes();
    let follows_digit_dot =
        start >= 2 && bytes[start - 1] == b'.' && bytes[start - 2].is_ascii_digit();
    let codec = x + 1;
    follows_digit_dot
        && matches!(bytes.get(codec..codec + 3), Some(b"264" | b"265"))
        && !bytes.get(codec + 3).is_some_and(u8::is_ascii_digit)
}

/// Extend `eps` (all of season `s`) with the dash-only continuations that open `tail` (Sonarr's
/// "Extend" style, `S01E01-02-03`) and return the rest of `tail`. One counts only when it names the
/// NEXT episode and ends at a separator or the end of the name (`_` included, which a `\b` would not
/// count, as the regex crate treats `_` as a word character), so a resolution or bit-depth token
/// after a dash (`-720p`, `-10bit`) and a dash-number range (`S01E01-10`) are not read as one. Nor is
/// a channel layout whose whole part is the next episode (`S01E04-5.1`, `S01E01-2.0`).
fn extend_with_dash_next<'a>(mut tail: &'a str, s: u32, eps: &mut Vec<(u32, u32)>) -> &'a str {
    static DASH_NEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-(\d{1,3})").unwrap());
    while let Some(next) = DASH_NEXT.captures(tail) {
        let (Some(whole), Some(number)) = (next.get(0), next.get(1)) else {
            break;
        };
        let Ok(e) = number.as_str().parse::<u32>() else {
            break;
        };
        let after = &tail.as_bytes()[whole.end()..];
        if after.first().is_some_and(u8::is_ascii_alphanumeric)
            || is_channel_layout(number.as_str(), after)
            || eps.last().is_none_or(|&(_, last)| e != last + 1)
            || eps.contains(&(s, e))
        {
            break;
        }
        eps.push((s, e));
        tail = &tail[whole.end()..];
    }
    tail
}

/// Whether `tail`, the text right after an episode code and its continuations, opens a range to
/// another episode (`-10`, `~10`, `-1x10`, `~E10`, `-S01E10`, `-S01.E10`) rather than carrying a
/// resolution, a bit depth or a channel layout after a dash (`-720p`, `-10bit`, `-5.1`).
fn opens_episode_range(tail: &str) -> bool {
    static RANGE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^[-~](?:s\d{1,2}[ ._]?e|e|\d{1,2}x)?(\d{1,3})").unwrap());
    RANGE.captures(tail).is_some_and(|c| {
        let (Some(whole), Some(number)) = (c.get(0), c.get(1)) else {
            return false;
        };
        let after = &tail.as_bytes()[whole.end()..];
        !after.first().is_some_and(u8::is_ascii_alphanumeric)
            && !is_channel_layout(number.as_str(), after)
    })
}

/// Whether `number`, followed by `after`, is the whole part of a channel layout (`5.1`, `2.0`): a
/// single digit followed by `.` and exactly one more digit.
fn is_channel_layout(number: &str, after: &[u8]) -> bool {
    number.len() == 1
        && after.first() == Some(&b'.')
        && after.get(1).is_some_and(u8::is_ascii_digit)
        && !after.get(2).is_some_and(u8::is_ascii_digit)
}

/// The FIRST episode code in a file name (`parse_se_all`'s first element): the file's season folder
/// and the slot `episode_of_file` reports.
fn parse_se(name: &str) -> Option<(u32, u32)> {
    parse_se_all(name).into_iter().next()
}

/// The selected video files of `t` the VFS can actually EMIT (`emittable_video_files_with_links`).
/// Every consumer that needs the VFS's view of a torrent's playable files reads this: the build's
/// episode-slot check (`present_files`), `selection::plan_selection_repoints`, and the scan loop's
/// file refresh (`tasks::refresh_fileless_infos`), the account mirror's no-video check and dedup's
/// `tasks::holds_codeless_video`. So a slot, or a decision about a torrent's files, only ever
/// concerns a file the build will emit.
pub(crate) fn emittable_video_files(t: &TorrentInfo) -> Vec<&crate::rd_client::TorrentFile> {
    emittable_video_files_with_links(t)
        .map(|(f, _)| f)
        .collect()
}

/// The SELECTED files of `t`, each with its Real-Debrid restricted link: Real-Debrid pairs `links`
/// positionally among the selected files, so the position advances once per selected file, video
/// or not, and an unselected file takes none. TorBox resolves by `(torrent_id, file_id)` and lists
/// no links, so every link is `None` there, as is a file past the end of `links` (Real-Debrid's
/// `selected != links` anomaly). The one copy of this pairing: the VFS build
/// ([`emittable_video_files_with_links`]) and the locators the instant repair and the acquisition
/// engine build ([`locator_for_selected`], [`selected_locators`]) all read it.
pub(crate) fn selected_files_with_links(
    t: &TorrentInfo,
) -> impl Iterator<Item = (&crate::rd_client::TorrentFile, Option<&String>)> {
    t.files
        .iter()
        .filter(|f| f.selected == 1)
        .enumerate()
        .map(|(link_idx, f)| (f, t.links.get(link_idx)))
}

/// A FileLocator for the SELECTED file at `path` within `info` (the first, should two share it),
/// its Real-Debrid link paired by position among the selected files ([`selected_files_with_links`];
/// `None` on TorBox, which resolves by `(torrent_id, file_id)`). `None` when no selected file is at
/// `path`. The instant repair locates the replacement's copy of a file this way, and
/// `acquire::locator_for` builds on it.
pub(crate) fn locator_for_selected(
    info: &TorrentInfo,
    hash: &str,
    path: &str,
) -> Option<FileLocator> {
    selected_files_with_links(info)
        .find(|(f, _)| f.path == path)
        .map(|(f, link)| selected_locator(info, hash, f, link))
}

/// A FileLocator for every SELECTED file of `info`, in file order, paired as
/// [`locator_for_selected`] pairs one. The instant repair records its redirect from these.
pub(crate) fn selected_locators<'a>(
    info: &'a TorrentInfo,
    hash: &'a str,
) -> impl Iterator<Item = FileLocator> + 'a {
    selected_files_with_links(info).map(move |(f, link)| selected_locator(info, hash, f, link))
}

/// The FileLocator of the selected file `f` of `info`, with its paired `link`.
fn selected_locator(
    info: &TorrentInfo,
    hash: &str,
    f: &crate::rd_client::TorrentFile,
    link: Option<&String>,
) -> FileLocator {
    FileLocator {
        hash: hash.to_string(),
        torrent_id: info.id.clone(),
        file_id: f.id,
        file_path: f.path.clone(),
        link: link.cloned(),
    }
}

/// The selected video files of `t` the VFS can actually EMIT, each with its Real-Debrid link
/// ([`selected_files_with_links`]): a selected video file is emittable only when `links` is empty
/// (TorBox, which resolves by id, so the link is `None`) or has a link at its position among the
/// SELECTED files (Real-Debrid's `selected != links` anomaly can leave a file with none). The
/// build's emit paths and `emittable_video_files` all read this one pairing, so an episode slot
/// never points at a file the build won't emit.
fn emittable_video_files_with_links(
    t: &TorrentInfo,
) -> impl Iterator<Item = (&crate::rd_client::TorrentFile, Option<&String>)> {
    let links_empty = t.links.is_empty();
    selected_files_with_links(t)
        .filter(move |(f, link)| is_video_file(&f.path) && (links_empty || link.is_some()))
}

/// The first episode code in a file's basename: the episode whose season folder the build places
/// it in. (The build's selection gate checks every code a multi-episode file carries, see
/// `parse_se_all`.)
pub(crate) fn episode_of_file(path: &str) -> Option<(u32, u32)> {
    parse_se(path.rsplit('/').next().unwrap_or(path))
}

/// `true` the first time this process sees `(kind, torrent_id)`. `build` re-runs every scan tick, so
/// a per-torrent anomaly it reports (an archive-only torrent, a selected-file/link count mismatch)
/// is logged at warn once and at debug after that, instead of ~1,440 identical warn lines a day.
fn warn_once(kind: &'static str, torrent_id: &str) -> bool {
    static WARNED: LazyLock<std::sync::Mutex<std::collections::HashSet<(&'static str, String)>>> =
        LazyLock::new(Default::default);
    WARNED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert((kind, torrent_id.to_string()))
}

/// Log the selected-file/link count mismatch of an RD torrent (warn once per torrent, see
/// `warn_once`). Such a torrent's files without a positional link are skipped.
fn note_link_count_mismatch(torrent: &TorrentInfo) {
    let selected_count = torrent.files.iter().filter(|f| f.selected == 1).count();
    if torrent.links.is_empty() || selected_count == torrent.links.len() {
        return;
    }
    if warn_once("link-count", &torrent.id) {
        tracing::warn!(
            "Torrent '{}': selected file count ({}) != link count ({})",
            torrent.filename,
            selected_count,
            torrent.links.len()
        );
    } else {
        tracing::debug!(
            "Torrent '{}': selected file count ({}) != link count ({})",
            torrent.filename,
            selected_count,
            torrent.links.len()
        );
    }
}

/// Extract the numeric tmdb id from a `MediaMetadata.external_id` like `"tmdb:1396"`.
pub(crate) fn tmdb_id_of(metadata: &MediaMetadata) -> Option<u64> {
    metadata
        .external_id
        .as_deref()
        .and_then(|s| s.strip_prefix("tmdb:"))
        .and_then(|s| s.parse::<u64>().ok())
}

pub const VIDEO_EXTENSIONS: &[&str] = &[
    ".mkv", ".mp4", ".avi", ".m4v", ".mov", ".wmv", ".flv", ".ts", ".m2ts",
];

const ARCHIVE_EXTENSIONS: &[&str] = &[".rar", ".zip", ".7z", ".tar", ".gz", ".bz2"];

/// RAR split-volume patterns: .r00, .r01, ..., .r99, .s00, etc.
/// Expects a pre-lowercased input string.
fn is_archive_part(path: &str) -> bool {
    // .r00-.r99, .s00-.s99 (split RAR volumes)
    // Safe to use bytes directly: the pattern is ASCII-only, and multi-byte
    // UTF-8 continuation bytes (0x80-0xBF) can never match '.', 'r', or 's'.
    let b = path.as_bytes();
    b.len() >= 4 && {
        let t = &b[b.len() - 4..];
        t[0] == b'.'
            && (t[1] == b'r' || t[1] == b's')
            && t[2].is_ascii_digit()
            && t[3].is_ascii_digit()
    }
}

fn is_archive_file(path: &str) -> bool {
    // ASCII lowercasing: the extensions/keywords compared are all ASCII, so full-Unicode folding
    // is unnecessary work on this per-file hot path (`build` re-runs every scan tick).
    let lower = path.to_ascii_lowercase();
    ARCHIVE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) || is_archive_part(&lower)
}

/// Parse a Real-Debrid date string into SystemTime.
/// Handles "YYYY-MM-DDTHH:mm:ss.sssZ" (JS toJSON) and "YYYY-MM-DD".
/// Falls back to UNIX_EPOCH on parse failure.
pub fn parse_rd_date(s: &str) -> SystemTime {
    fn days_from_ymd(y: u64, m: u64, d: u64) -> Option<u64> {
        let is_leap =
            |yr: i64| yr.rem_euclid(4) == 0 && (yr.rem_euclid(100) != 0 || yr.rem_euclid(400) == 0);
        let days_before_month: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
        // Reject an absurd (non-4-digit) year up front: a realistic date is 1..=9999, and bounding
        // it here makes every arithmetic below overflow-free (a crafted huge year string in a
        // provider response would otherwise overflow the i64 `(y - 1970) * 365`).
        if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }

        let y = y as i64;
        // Reject impossible days for the given month (e.g. 2023-02-30, 2023-04-31)
        // rather than silently rolling over into the following month.
        let month_lengths: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let max_day = if m == 2 && is_leap(y) {
            29
        } else {
            month_lengths[(m - 1) as usize]
        };
        if d > max_day {
            return None;
        }

        // Count leap years from year 1 to year (yr - 1) using closed-form formula
        let leap_years_before = |yr: i64| -> i64 {
            let yr = yr - 1;
            yr / 4 - yr / 100 + yr / 400
        };
        let total = (y - 1970) * 365
            + (leap_years_before(y) - leap_years_before(1970))
            + days_before_month[(m - 1) as usize]
            + if m as i64 > 2 && is_leap(y) { 1 } else { 0 }
            + (d as i64 - 1);
        if total < 0 {
            return None;
        }
        Some(total as u64)
    }

    let parse = || -> Option<SystemTime> {
        let date_part = s.split('T').next()?;
        let mut parts = date_part.split('-');
        let y: u64 = parts.next()?.parse().ok()?;
        let m: u64 = parts.next()?.parse().ok()?;
        let d: u64 = parts.next()?.parse().ok()?;
        let mut secs = days_from_ymd(y, m, d)? * 86400;

        // Parse the time-of-day if present ("HH:mm:ss" / "HH:mm:ss.sss"), returning None only when
        // the time itself is malformed/out-of-range.
        fn time_of_day_secs(time_part: &str) -> Option<u64> {
            let time_str = time_part.trim_end_matches('Z');
            let mut time_parts = time_str.split(':');
            let h: u64 = time_parts.next()?.parse().ok()?;
            let min: u64 = time_parts.next()?.parse().ok()?;
            let sec_str = time_parts.next().unwrap_or("0");
            let sec: u64 = sec_str.split('.').next()?.parse().ok()?;
            if h >= 24 || min >= 60 || sec >= 60 {
                return None;
            }
            Some(h * 3600 + min * 60 + sec)
        }

        // The DATE is already validated and converted above. If only the TIME is malformed, keep the
        // good date (add zero, i.e. midnight) rather than discarding the whole timestamp and
        // reporting a 1970 mtime — a 1970 mtime confuses media-server change detection.
        if let Some(time_part) = s.split('T').nth(1) {
            secs += time_of_day_secs(time_part).unwrap_or(0);
        }

        Some(UNIX_EPOCH + Duration::from_secs(secs))
    };

    parse().unwrap_or(UNIX_EPOCH)
}

static SEASON_RE: LazyLock<Regex> = LazyLock::new(|| {
    // The `Sxx`, `NxNN` and `Part N` arms are bounded by a non-alphanumeric character (or the start
    // or end of the string) on both sides, so `_` separates them like `.`, ` ` and `-` do
    // (`Show_S02_1080p`, `Show_Part_2`); a `\b` would not, since the regex crate counts `_` as a
    // word character. The regex crate has no look-around, so these boundaries consume a character,
    // and `season_tokens` resumes each search on a match's trailing separator. The `Sxx` arm takes
    // 1-2 digits followed by `e` or such a boundary, so a codec token (`DTS5`/`DDP5`, `s`
    // mid-token) or a 4-digit `S2024` can't be read as a season while `S05E01`/`S5` still are. The
    // `season`/`part` arms accept any scene separator (`[\s._-]*`) before the number, not just
    // whitespace, so `Season.2`/`Part_2`/`Season-2` (the common dotted scene form) resolve rather
    // than mis-foldering to season 1. The `NxNN` arm is bounded to a 1-2 digit season so a
    // "WIDTHxHEIGHT" pixel resolution (e.g. 1920x1080) is not mistaken for one. The `Part N` arm
    // (group 4) takes a 1-2 digit number, so a title word (`Counterpart.2017`) or a year
    // (`Part.2019`) is never read as a season; it is also the WEAKEST signal (`season_tokens`),
    // since it is as often a two-part episode title as a season.
    Regex::new(
        r"(?i)(?:^|[^a-z0-9])s(\d{1,2})(?:e|[^a-z0-9]|$)|season[\s._-]*(\d+)|(?:^|[^a-z0-9])(\d{1,2})x\d{1,3}(?:[^a-z0-9]|$)|(?:^|[^a-z0-9])part[\s._-]*(\d{1,2})(?:[^a-z0-9]|$)",
    )
    .unwrap()
});

#[derive(Debug, Clone, PartialEq)]
pub enum VfsNode {
    Directory {
        children: BTreeMap<String, VfsNode>,
    },
    /// Media file proxied from the debrid provider's CDN.
    /// WebDAV serves the actual media bytes (not a .strm URL).
    MediaFile {
        file_size: u64,
        locator: FileLocator,
    },
    VirtualFile {
        content: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateType {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsChange {
    pub path: String,
    pub update_type: UpdateType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub enum MediaType {
    Movie,
    Show,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MediaMetadata {
    pub title: String,
    pub year: Option<String>,
    pub media_type: MediaType,
    pub external_id: Option<String>,
}

// NOTE: the unidentified-title grouping folds case with ASCII-only operations
// (`eq_ignore_ascii_case` / `to_ascii_lowercase`), so two titles differing ONLY by non-ASCII case
// (e.g. "Café" vs "CAFÉ") group — and thus folder — separately. This is self-consistent (`Hash` and
// `Eq` fold identically, so there is no correctness/panic risk), only imperfect dedup for non-ASCII
// titles; a full Unicode fold is avoided to not pull in a `unicode`-casing dependency. IDENTIFIED
// titles are compared by `external_id` and are unaffected.
impl PartialEq for MediaMetadata {
    fn eq(&self, other: &Self) -> bool {
        if self.media_type != other.media_type {
            return false;
        }
        match (&self.external_id, &other.external_id) {
            // Same external ID → same media, regardless of title/year differences
            (Some(a), Some(b)) => a == b,
            // Both unidentified → compare title case-insensitively + year (ASCII-only fold, see above)
            (None, None) => {
                self.title.eq_ignore_ascii_case(&other.title) && self.year == other.year
            }
            // One identified, one not → different
            _ => false,
        }
    }
}

impl Eq for MediaMetadata {}

impl Hash for MediaMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.media_type.hash(state);
        match &self.external_id {
            Some(id) => id.hash(state),
            None => {
                self.title.to_ascii_lowercase().hash(state);
                self.year.hash(state);
            }
        }
    }
}

pub struct DebridVfs {
    pub root: VfsNode,
    pub timestamps: HashMap<String, SystemTime>,
}

impl Default for DebridVfs {
    fn default() -> Self {
        Self::new()
    }
}

impl DebridVfs {
    pub fn new() -> Self {
        let mut children = BTreeMap::new();
        children.insert(
            "Movies".to_string(),
            VfsNode::Directory {
                children: BTreeMap::new(),
            },
        );
        children.insert(
            "Shows".to_string(),
            VfsNode::Directory {
                children: BTreeMap::new(),
            },
        );

        Self {
            root: VfsNode::Directory { children },
            timestamps: HashMap::new(),
        }
    }

    pub fn build(torrents: Vec<(TorrentInfo, MediaMetadata)>, selection: &SelectionMap) -> Self {
        let mut movies_nodes = BTreeMap::new();
        let mut shows_nodes = BTreeMap::new();
        let mut timestamps: HashMap<String, SystemTime> = HashMap::new();

        // Group torrents by media (`MediaMetadata`'s Eq: the external ID when set, else title + year).
        // Torrents of one group can still carry different titles (a cached identification next to
        // an authoritative acquire title, a TMDB rename, a case difference), and the group's
        // metadata names its folder and NFO. Pick the representative by the smallest (title, year)
        // rather than the first one listed, so the provider's listing order can't rename the folder
        // between ticks.
        let mut media_groups: HashMap<MediaMetadata, (MediaMetadata, Vec<TorrentInfo>)> =
            HashMap::new();
        for (torrent, metadata) in torrents {
            if let Some((rep, group)) = media_groups.get_mut(&metadata) {
                if (&metadata.title, &metadata.year) < (&rep.title, &rep.year) {
                    *rep = metadata;
                }
                group.push(torrent);
            } else {
                media_groups.insert(metadata.clone(), (metadata, vec![torrent]));
            }
        }

        // Sort groups to ensure deterministic naming for conflicts
        let mut sorted_groups: Vec<_> = media_groups.into_values().collect();
        sorted_groups.sort_by(|(a, _), (b, _)| {
            a.title
                .cmp(&b.title)
                .then_with(|| a.year.cmp(&b.year))
                .then_with(|| a.external_id.cmp(&b.external_id))
                .then_with(|| a.media_type.cmp(&b.media_type))
        });

        let mut used_movie_names: HashMap<String, u32> = HashMap::new();
        let mut used_show_names: HashMap<String, u32> = HashMap::new();

        for (metadata, mut torrents) in sorted_groups {
            // Sort torrents by size descending to pick the best/largest one (equal sizes by hash, so
            // the listing order can't flip which one wins).
            torrents.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.hash.cmp(&b.hash)));

            let base_name = sanitize_filename(&metadata.title);

            let (used_names, nodes) = match metadata.media_type {
                MediaType::Movie => (&mut used_movie_names, &mut movies_nodes),
                MediaType::Show => (&mut used_show_names, &mut shows_nodes),
            };

            // The dedup suffix is only *committed* once we know the folder is actually inserted
            // (an archive-only / empty group must not consume a number and leave a "Title (1)" gap
            // with no "Title"). `dedup_base` carries the base name to bump after insertion.
            let mut dedup_base: Option<String> = None;
            let folder_name = if let Some(id) = &metadata.external_id {
                if let Some((source, raw_id)) = id.split_once(':') {
                    format!("{} [{}id-{}]", base_name, source, raw_id)
                } else {
                    format!("{} [id={}]", base_name, id)
                }
            } else {
                let count = *used_names.get(&base_name).unwrap_or(&0);
                dedup_base = Some(base_name.clone());
                if count == 0 {
                    base_name.clone()
                } else {
                    format!("{} ({})", base_name, count)
                }
            };
            // Sanitize the whole folder name: the `[<source>id-<id>]` suffix is derived from
            // `external_id`, so a crafted/cached value containing a path separator must not be
            // able to introduce one into the directory path. Idempotent for normal ids.
            let folder_name = sanitize_filename(&folder_name);

            // Final uniqueness guard within this section (Movies/ or Shows/). The identified-title
            // path (`Title [tmdbid-N]`) bypasses the `used_names` counter, so without this an
            // identified folder could collide with — and SILENTLY OVERWRITE (data loss) — an
            // unidentified group whose cleaned title happens to match that exact form (or two names
            // could fold together under `sanitize_filename`). Suffix ` (N)` until unique so neither
            // entry is lost. Each `metadata` group is processed exactly once, so a pre-existing key
            // here is always a genuine cross-group collision, never a legitimate re-entry.
            let folder_name = {
                let mut candidate = folder_name.clone();
                let mut n = 1u32;
                while nodes.contains_key(&candidate) {
                    candidate = format!("{} ({})", folder_name, n);
                    n += 1;
                }
                candidate
            };

            // Warn (once per torrent, then at debug) about archive-only torrents that can't be
            // streamed. This is a purely diagnostic pass that re-scans every selected file with
            // `is_video_file`/`is_archive_file` (which the emit path below also does); skip it
            // entirely when warn-level logging is disabled so it costs nothing then (debug implies
            // warn, so the debug repeats are covered by the same check).
            if tracing::enabled!(tracing::Level::WARN) {
                for torrent in &torrents {
                    let has_video = torrent
                        .files
                        .iter()
                        .any(|f| f.selected == 1 && is_video_file(&f.path));
                    if !has_video {
                        let has_archive = torrent
                            .files
                            .iter()
                            .any(|f| f.selected == 1 && is_archive_file(&f.path));
                        if has_archive {
                            const MSG: &str = "contains only archive files (RAR/ZIP) and cannot \
                                 be streamed — replace it with a non-archive version on your debrid \
                                 service";
                            if warn_once("archive-only", &torrent.id) {
                                tracing::warn!("Torrent '{}' {MSG}", torrent.filename);
                            } else {
                                tracing::debug!("Torrent '{}' {MSG}", torrent.filename);
                            }
                        }
                    }
                }
            }

            let mut folder_inserted = false;
            match metadata.media_type {
                MediaType::Movie => {
                    // SP3: prefer the managed-selection torrent (if present in this group); else the
                    // largest (torrents are size-sorted). Then fall back to the next candidate that
                    // actually yields a streamable video file, so a movie whose top choice is
                    // archive-only (RAR/ZIP) doesn't vanish when a smaller streamable copy exists.
                    //
                    // Unlike the episode path, this matches the selection by HASH only and then emits
                    // ALL selected streamable video files of that torrent (exactly one, for a
                    // single-feature movie): the VFS does NOT enforce the stored
                    // `SelectionEntry.file_path` for a movie. That is safe because a movie torrent the
                    // engine adds holds a single feature (the acquisition + upgrade pack-guards reject
                    // multi-feature packs), so the hash uniquely identifies the file. An account-mirror
                    // copy can be a multi-film collection: emitting every selected file then shows each
                    // film, and dedup and the upgrade engine never remove such a copy
                    // (`tasks::multi_feature_hashes`). The stored `file_path` still matters elsewhere:
                    // `selection::plan_selection_repoints` ranks the served copy by it (`served_rank`)
                    // and writes the movie's largest emittable file there. A stale episode selection
                    // falls back to every distinct file name, same-named copies resolving to the
                    // largest torrent; a movie has no such fallback within its selected torrent, so
                    // enforcing the path here would risk HIDING the movie after a re-acquire shifted
                    // the path. The hash-only match is deliberate.
                    let selected_hash = tmdb_id_of(&metadata)
                        .and_then(|id| selection.get(&crate::store::movie_slot(id)))
                        .map(|sel| sel.hash.clone());
                    let mut ordered: Vec<&TorrentInfo> = Vec::with_capacity(torrents.len());
                    if let Some(h) = &selected_hash {
                        if let Some(t) = torrents.iter().find(|t| t.hash.eq_ignore_ascii_case(h)) {
                            ordered.push(t);
                        }
                    }
                    for t in &torrents {
                        if selected_hash
                            .as_ref()
                            .is_some_and(|h| t.hash.eq_ignore_ascii_case(h))
                        {
                            continue; // selection-matched torrent is already at the front
                        }
                        ordered.push(t);
                    }
                    for torrent in ordered {
                        let mut children = BTreeMap::new();
                        Self::add_torrent_files(&mut children, torrent);
                        if children.is_empty() {
                            continue; // no streamable video in this candidate — try the next
                        }
                        let torrent_ts = parse_rd_date(&torrent.added);
                        let prefix = format!("Movies/{}", folder_name);
                        for name in children.keys() {
                            timestamps.insert(format!("{}/{}", prefix, name), torrent_ts);
                        }
                        let nfo_content = Self::generate_nfo(&metadata);
                        children.insert(
                            "movie.nfo".to_string(),
                            VfsNode::VirtualFile {
                                content: nfo_content,
                            },
                        );
                        timestamps.insert(format!("{}/movie.nfo", prefix), torrent_ts);
                        nodes.insert(folder_name.clone(), VfsNode::Directory { children });
                        folder_inserted = true;
                        break;
                    }
                }
                MediaType::Show => {
                    let mut show_children = BTreeMap::new();
                    let show_prefix = format!("Shows/{}", folder_name);
                    let mut show_max_ts = UNIX_EPOCH;

                    // For shows, we process all torrents (e.g. different seasons)
                    // Torrents are sorted by size descending, so larger (better quality) torrents
                    // are processed first. If a file with the same name already exists in a season
                    // directory, skip it to avoid creating (1)/(2) duplicates from repair replacements.
                    // SP3: per-episode selection overrides for this show (slot -> entry).
                    let show_tmdb = tmdb_id_of(&metadata);
                    // The (hash, file_path) pairs actually EMITTABLE in this show's group. A
                    // selection naming a pair that isn't emittable — an absent hash, a present hash
                    // whose file was re-listed under a different path, OR a selected file with no
                    // positional link (RD's `selected != links` anomaly, see the warn below) — is
                    // stale/unservable and must NOT hide the episode (self-healing → falls back to
                    // the legacy first/largest dedup so the episode still appears). It and the emit
                    // loop below read the same pairing (`emittable_video_files_with_links`), so
                    // present_files never marks a file "present" that the emit loop then refuses to
                    // emit, which would suppress every other torrent's copy and vanish the episode.
                    // Computed BEFORE the loop below moves `torrents`. `present_files` is consulted
                    // ONLY for an episode that has a selection entry (the selection gate below),
                    // which can't happen when this show is unidentified (no `show_tmdb`) or the
                    // deployment holds no managed selections at all. Skip the build (a per-tick clone
                    // of every selected video file in the group) in those cases — the common
                    // mirror-only / pre-SP3 deployment has an empty `selection`, so this avoids the
                    // work entirely there.
                    let present_files: std::collections::HashSet<(String, String)> =
                        if show_tmdb.is_some() && !selection.is_empty() {
                            torrents
                                .iter()
                                .flat_map(|t| {
                                    let h = t.hash.to_ascii_lowercase();
                                    emittable_video_files(t)
                                        .into_iter()
                                        .map(move |f| (h.clone(), f.path.clone()))
                                })
                                .collect()
                        } else {
                            std::collections::HashSet::new()
                        };
                    for torrent in torrents {
                        let torrent_ts = parse_rd_date(&torrent.added);
                        if torrent_ts > show_max_ts {
                            show_max_ts = torrent_ts;
                        }
                        note_link_count_mismatch(&torrent);
                        // The episodes this torrent's code-less files name, as the acquisition
                        // engine maps (and slots) them: built on the first code-less file the
                        // selection gate meets.
                        let mut codeless: Option<HashMap<String, (u32, u32)>> = None;
                        for (file, link) in emittable_video_files_with_links(&torrent) {
                            let filename = file.path.split('/').next_back().unwrap_or(&file.path);
                            // Parse the episode codes once and reuse them for both the selection
                            // gate and the season-folder placement below (the regex match is not
                            // free).
                            let codes = parse_se_all(filename);
                            let se = codes.first().copied();
                            // SP3 selection gate: a file is skipped only when EVERY episode it
                            // carries has a managed selection naming another present (hash,
                            // file_path). A multi-episode file (`S01E01E02`) is therefore kept
                            // while any one of its slots selects it, or any one of its episodes has
                            // no present selection: `observe` writes a slot per episode, so its
                            // (1,1) slot can sit on a better singleton while (1,2) selects this
                            // file, and hiding the file would lose E02 (a duplicate E01 is the
                            // lesser evil). A stale selection (absent hash, or a path that no
                            // longer matches) never hides anything: that episode degrades to the
                            // legacy first/largest-wins dedup below. A file with no episode code is
                            // gated the same way on the episode the engine maps it to
                            // (`codeless_episode_files`, in the season folder it lands in
                            // below), so a slot moved off it (a repoint, a pack-served episode split
                            // to a single) hides it like a coded file, instead of leaving the
                            // episode listed twice. A file that maps to nothing is never hidden.
                            if let Some(tmdb) = show_tmdb {
                                let selected_elsewhere = |&(s, e): &(u32, u32)| {
                                    selection
                                        .get(&crate::store::episode_slot(tmdb, s, e))
                                        .is_some_and(|sel| {
                                            present_files.contains(&(
                                                sel.hash.to_ascii_lowercase(),
                                                sel.file_path.clone(),
                                            )) && !(torrent.hash.eq_ignore_ascii_case(&sel.hash)
                                                && file.path == sel.file_path)
                                        })
                                };
                                if !codes.is_empty() {
                                    if codes.iter().all(selected_elsewhere) {
                                        continue;
                                    }
                                } else if !selection.is_empty() {
                                    let mapped = codeless.get_or_insert_with(|| {
                                        codeless_episode_files(&torrent)
                                            .into_iter()
                                            .map(|(s, e, path)| (path, (s, e)))
                                            .collect()
                                    });
                                    if mapped.get(&file.path).is_some_and(selected_elsewhere) {
                                        continue;
                                    }
                                }
                            }
                            // Prefer the FIRST episode code for the folder season: it handles
                            // SxxExx, NxM AND the dash-concatenated "Show - 409 - Title"
                            // absolute-code form, so such a pack's episodes land in the right
                            // Season folder (a double-episode file goes under its first episode's
                            // season). Otherwise SEASON_RE covers the patterns `parse_se_all`
                            // doesn't ("Season 4", "Part 2", a range it leaves undecoded such as
                            // "1x01-10") in the file name and then its directories, nearest first,
                            // with a `Part N` in the file name as the weakest signal
                            // (`season_of_codeless_file`), then season 1.
                            let season = se
                                .map(|(s, _)| s)
                                .unwrap_or_else(|| season_of_codeless_file(&file.path, filename));

                            let season_name = format!("Season {:02}", season);
                            let season_dir = show_children
                                .entry(season_name.clone())
                                .or_insert_with(|| VfsNode::Directory {
                                    children: BTreeMap::new(),
                                });

                            if let VfsNode::Directory {
                                children: season_children,
                            } = season_dir
                            {
                                // Skip if this episode already exists (from a larger/earlier
                                // torrent).
                                if season_children.contains_key(filename) {
                                    continue;
                                }
                                let file_name = Self::add_path_to_tree(
                                    season_children,
                                    filename,
                                    file.bytes,
                                    FileLocator {
                                        hash: torrent.hash.clone(),
                                        torrent_id: torrent.id.clone(),
                                        file_id: file.id,
                                        file_path: file.path.clone(),
                                        link: link.cloned(),
                                    },
                                );
                                timestamps.insert(
                                    format!("{}/{}/{}", show_prefix, season_name, file_name),
                                    torrent_ts,
                                );
                            }
                        }
                    }
                    if !show_children.is_empty() {
                        let nfo_content = Self::generate_nfo(&metadata);
                        show_children.insert(
                            "tvshow.nfo".to_string(),
                            VfsNode::VirtualFile {
                                content: nfo_content,
                            },
                        );
                        timestamps.insert(format!("{}/tvshow.nfo", show_prefix), show_max_ts);
                        nodes.insert(
                            folder_name,
                            VfsNode::Directory {
                                children: show_children,
                            },
                        );
                        folder_inserted = true;
                    }
                }
            }
            // Commit the dedup number only if this group produced a folder (see `dedup_base`).
            if folder_inserted {
                if let Some(b) = dedup_base {
                    *used_names.entry(b).or_insert(0) += 1;
                }
            }
        }

        let mut root_children = BTreeMap::new();
        root_children.insert(
            "Movies".to_string(),
            VfsNode::Directory {
                children: movies_nodes,
            },
        );
        root_children.insert(
            "Shows".to_string(),
            VfsNode::Directory {
                children: shows_nodes,
            },
        );
        let root = VfsNode::Directory {
            children: root_children,
        };

        // Compute directory timestamps bottom-up (max of children)
        Self::compute_dir_timestamps(&root, "", &mut timestamps);

        Self { root, timestamps }
    }

    /// Walk the tree and insert directory timestamps as the max of their children.
    fn compute_dir_timestamps(
        node: &VfsNode,
        prefix: &str,
        timestamps: &mut HashMap<String, SystemTime>,
    ) -> SystemTime {
        match node {
            VfsNode::Directory { children } => {
                let mut max_ts = UNIX_EPOCH;
                for (name, child) in children {
                    let child_path = if prefix.is_empty() {
                        name.clone()
                    } else {
                        format!("{}/{}", prefix, name)
                    };
                    let child_ts = Self::compute_dir_timestamps(child, &child_path, timestamps);
                    if child_ts > max_ts {
                        max_ts = child_ts;
                    }
                }
                if !prefix.is_empty() {
                    // Always insert a timestamp for directories, even empty ones.
                    // Empty directories (e.g. "Movies" when only shows exist) get UNIX_EPOCH.
                    timestamps.insert(prefix.to_string(), max_ts);
                }
                max_ts
            }
            _ => {
                // Leaf node — timestamp should already be in the map
                timestamps.get(prefix).copied().unwrap_or(UNIX_EPOCH)
            }
        }
    }

    fn generate_nfo(metadata: &MediaMetadata) -> Vec<u8> {
        let tag = match metadata.media_type {
            MediaType::Movie => "movie",
            MediaType::Show => "tvshow",
        };

        let mut nfo = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\" ?>\n<{}>\n",
            tag
        );

        // Title
        nfo.push_str(&format!(
            "  <title>{}</title>\n",
            xml_escape(&metadata.title)
        ));

        // Original title (same as title for now)
        nfo.push_str(&format!(
            "  <originaltitle>{}</originaltitle>\n",
            xml_escape(&metadata.title)
        ));

        // Year
        if let Some(year) = &metadata.year {
            nfo.push_str(&format!("  <year>{}</year>\n", xml_escape(year)));
            // `<premiered>` must be a valid date. Only emit it for a clean 4-digit year;
            // a malformed value (e.g. fewer digits or stray characters from a cached entry)
            // would otherwise produce an invalid date that media servers may reject.
            if year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit()) {
                nfo.push_str(&format!("  <premiered>{}-01-01</premiered>\n", year));
            }
        }

        // External IDs
        if let Some(external_id) = &metadata.external_id {
            if let Some((source, id)) = external_id.split_once(':') {
                nfo.push_str(&format!(
                    "  <uniqueid type=\"{}\" default=\"true\">{}</uniqueid>\n",
                    xml_escape(source),
                    xml_escape(id)
                ));
                if source == "tmdb" {
                    let path = match metadata.media_type {
                        MediaType::Movie => "movie",
                        MediaType::Show => "tv",
                    };
                    nfo.push_str(&format!("  <tmdbid>{}</tmdbid>\n", xml_escape(id)));
                    nfo.push_str(&format!(
                        "  <url>https://www.themoviedb.org/{}/{}</url>\n",
                        xml_escape(path),
                        xml_escape(id)
                    ));
                }
            }
        }

        // Don't include plot/outline - let Jellyfin fetch from TMDB
        // Including placeholder text causes duplication in Jellyfin UI

        // Lockdata set to false allows Jellyfin to fetch full metadata from TMDB
        nfo.push_str("  <lockdata>false</lockdata>\n");

        // Source indicator
        nfo.push_str("  <source>debridmoviemapper</source>\n");

        nfo.push_str(&format!("</{}>\n", tag));
        nfo.into_bytes()
    }

    fn add_torrent_files(destination: &mut BTreeMap<String, VfsNode>, torrent: &TorrentInfo) {
        note_link_count_mismatch(torrent);
        for (file, link) in emittable_video_files_with_links(torrent) {
            let filename = file.path.split('/').next_back().unwrap_or(&file.path);
            Self::add_path_to_tree(
                destination,
                filename,
                file.bytes,
                FileLocator {
                    hash: torrent.hash.clone(),
                    torrent_id: torrent.id.clone(),
                    file_id: file.id,
                    file_path: file.path.clone(),
                    link: link.cloned(),
                },
            );
        }
    }

    /// Insert a video file into `children` as a MediaFile node, keyed by its filename. Callers pass
    /// a basename (the VFS tree is one level deep — Season directories are created by the caller),
    /// so the leading path, if any, is stripped. On a name collision (two selected files sharing a
    /// basename) a " (N)" suffix is appended before the extension. Returns the final name used.
    fn add_path_to_tree(
        children: &mut BTreeMap<String, VfsNode>,
        path: &str,
        size: u64,
        locator: FileLocator,
    ) -> String {
        let name = path.rsplit('/').next().unwrap_or(path);
        let mut final_name = name.to_string();
        let mut counter = 1;
        while children.contains_key(&final_name) {
            if let Some(pos) = name.rfind('.') {
                let (base, ext) = (&name[..pos], &name[pos..]);
                final_name = format!("{} ({}){}", base, counter, ext);
            } else {
                final_name = format!("{} ({})", name, counter);
            }
            counter += 1;
        }
        children.insert(
            final_name.clone(),
            VfsNode::MediaFile {
                file_size: size,
                locator,
            },
        );
        final_name
    }
}

/// Replace characters that are invalid in filenames or interpreted as path separators.
/// Covers POSIX path separators, Windows-reserved characters, trailing dots/spaces (which Windows
/// silently strips), and bare Windows reserved device names (CON, NUL, COM1…) — important for
/// WebDAV clients accessing via rclone on Windows/SMB.
///
/// NOTE on a deliberate asymmetry: this is applied to the library-CONSTRUCTED Movie/Show folder
/// names (the title, and the `[tmdbid-N]`-suffixed folder name; season folders (`Season NN`) and
/// NFO names (`movie.nfo`/`tvshow.nfo`) are fixed strings that need no sanitising) but NOT to
/// media-file LEAF names, which keep the exact torrent basename
/// (`file.path.split('/').next_back()`). That is intentional: Jellyfin/Plex parse episode/quality
/// metadata out of the original scene filename, so mangling it would degrade identification. It is
/// not a path-traversal risk — `/` is already stripped by taking the basename, the WebDAV layer
/// XML-encodes names, and `dav_fs::walk_components` rejects any `..` component. (A backslash in a
/// leaf basename is not stripped, but debrid providers / BitTorrent path lists use POSIX `/`
/// separators so it does not occur in practice, and the leaf key is inert within the VFS — it
/// creates no nested node — so a stray `\` is not a traversal vector either.)
fn sanitize_filename(name: &str) -> String {
    let mut replaced = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '/' | '\\' => replaced.push('-'),
            ':' => {
                replaced.push(' ');
                replaced.push('-');
            }
            '*' | '?' | '<' | '>' | '|' | '"' => {}
            _ => replaced.push(c),
        }
    }
    let cleaned = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    // Windows strips a trailing dot (and space) from a name, so a directory key ending in one would
    // diverge from its on-disk name over rclone/SMB (e.g. "Adam." → "Adam"), and rclone can then
    // mis-map or error. `split_whitespace` already dropped trailing spaces; strip trailing dots too.
    let cleaned = cleaned.trim_end_matches('.').trim_end().to_string();
    // A name that sanitizes to empty, "." or ".." would create a junk/unusable directory key
    // ("."/".." are also path-navigation names). Fold them to a stable placeholder. Only reachable
    // for unidentified content (identified titles carry a real TMDB-derived name).
    let cleaned = match cleaned.as_str() {
        "" | "." | ".." => return "untitled".to_string(),
        _ => cleaned,
    };
    // A Windows reserved DEVICE name (CON, NUL, COM1…) cannot be created verbatim on Windows/SMB.
    // Windows resolves the device from the base name BEFORE the first dot, so a bare `CON` AND a
    // dotted `Con.Air.1997` (base `Con`) both qualify — suffix the reserved BASE (not the whole
    // name), else appending at the end wouldn't change the pre-dot base Windows still reads as a
    // device. A dotless real title like "Con Air"/"Conan" has a non-reserved base and is untouched.
    if let Some(base) = reserved_base(&cleaned) {
        return format!("{}_{}", base, &cleaned[base.len()..]);
    }
    cleaned
}

/// `true` if `name` is exactly a Windows reserved device name (case-insensitive) — these cannot be
/// used as a file/directory name on Windows/SMB.
fn is_reserved_device(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// If `name`'s base (the part before the first dot) is a Windows reserved device name, return that
/// base; otherwise `None`. Windows resolves `Con.Air.1997` to the CON device from its pre-dot base,
/// so a dotted name qualifies just as a bare `CON` does. A dotless non-reserved title ("Con Air",
/// "Conan") — whose whole name is its base — returns `None`.
fn reserved_base(name: &str) -> Option<&str> {
    let base = name.split('.').next().unwrap_or(name);
    is_reserved_device(base).then_some(base)
}

/// Regex matching filenames that should be excluded from the VFS.
/// Uses word boundaries (`\b`) to avoid false positives on substrings
/// (e.g. "extraordinary.mkv" should NOT be filtered out by "extra"). The regex crate counts `_` as a
/// word character, so `is_video_file` maps `_` to `.` before matching (`show_s01e01_sample.mkv`).
static EXCLUDED_VIDEO_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(sample|trailer|extras?|bonus|featurette)\b").unwrap());

/// A real title whose FIRST token is `trailer`/`extra(s)` followed by a separator and more content —
/// the show "Extras", "Trailer Park Boys", the movie "Extras 2020" — must NOT be excluded. Only these
/// two keywords are realistic leading title words; `sample`/`bonus`/`featurette` essentially never
/// lead a real title (and `sample-<release>.mkv` is a real ancillary form), so they are NOT given the
/// leading exception. Matched against the filename stem (extension stripped).
static EXCLUDED_LEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(trailer|extras?)[._\s-]").unwrap());

/// A leading `trailer`/`extras` keyword immediately followed by a RESOLUTION or SOURCE/quality tag
/// (`Trailer.1080p.WEB-DL`, `Extras.720p`) is an ancillary CLIP, not a real title — so it must NOT
/// receive the `EXCLUDED_LEADING_RE` title-exemption. A genuine leading-keyword title instead keeps a
/// real word or a year after the keyword (`Trailer.Park.Boys`, `Extras.2005`, `Extras.2020.1080p…` —
/// a bare 4-digit year is NOT a resolution, so it is preserved). Anchored so the metadata token must
/// come IMMEDIATELY after the keyword+separator. Matched against the filename stem.
static EXCLUDED_LEADING_METADATA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)^(?:trailer|extras?)[._\s-](?:{RELEASE_TAG_TOKENS})\b"
    ))
    .unwrap()
});

/// The resolution, source and codec tokens that mark a keyword next to them as a clip descriptor
/// (`EXCLUDED_LEADING_METADATA_RE`, `RELEASE_TAG_RE`). A year is not one of them: after a leading
/// keyword it marks a title (`Extras.2020`), so only `RELEASE_TAG_RE` adds it.
const RELEASE_TAG_TOKENS: &str = r"\d{3,4}p|4k|uhd|web|webdl|web-dl|webrip|web-rip|bluray|blu-ray|bdrip|brrip|hdtv|dvdrip|remux|hdr|hevc|x264|x265|h264|h265";

/// A release-year token. In `is_video_file` a keyword followed by the year (and with none before it)
/// is part of the title ("The.Extra.Man.2010"), one after the year is a descriptor
/// ("Movie.2010.Trailer"). A resolution (`1080p`) or codec (`x264`) never matches: the digits run
/// into a letter, so there is no word boundary.
static RELEASE_YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:19|20)\d{2}\b").unwrap());

/// Where the first episode code starts in `name`: an `SxxEyy` or `NxM` code (`S01E05`, `S01.E05`,
/// `2x05`; ranges included), else the dash-concatenated `Show - 105 - Title` code (`dash_code`),
/// the forms `parse_se_all` reads. An episode code marks a real episode, so a file with one is
/// kept when an excluded keyword other than `sample` sits BEFORE the code (a title word:
/// "Some.Trailer.Show.S02E03"), and when it sits in an extras/bonus directory (a mis-foldered real
/// episode). It never rescues a `sample` keyword or a file in a `Sample`/`Samples` folder: scene
/// samples carry the full release name and its episode code (`sample-show.s01e01…`,
/// `Sample/show.s01e01…`). Returned as `(start, end)`, `end` being where the code's `SxxEyy` match
/// (its `E` continuations included), `NxM` match or dash code's three digits end; a keyword after
/// it is judged by `keyword_after_code_is_descriptor`.
fn episode_code_span(name: &str) -> Option<(usize, usize)> {
    let sxxeyy = SXXEYY_RE.find(name).map(|m| (m.start(), m.end()));
    let nxm = find_nxm(name).and_then(|c| Some((c.get(1)?.start(), c.get(3)?.end())));
    sxxeyy
        .into_iter()
        .chain(nxm)
        .min()
        .or_else(|| dash_code(name).map(|(digits, _)| (digits.start(), digits.end())))
}

/// A word token: a run of letters or digits (`is_video_file` has already mapped `_` to `.`).
static WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+").unwrap());

/// A token that still belongs to an episode code right after it: a dash continuation (`-02`), a
/// version suffix (`v2`), or another code (`.S01E02`, `.E02`, `-1x02`).
static CODE_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:\d{1,3}|v\d{1,2}|s\d{1,2}|e\d{1,3}|s\d{1,2}e\d{1,3}(?:e\d{1,3})*|\d{1,2}x\d{2,3}(?:v\d{1,2})?(?:x\d{2,3})*)$")
        .unwrap()
});

/// A release tag opening the text it is matched against: a resolution, source or codec token (the
/// shared `RELEASE_TAG_TOKENS`) or a release year.
static RELEASE_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)^(?:{RELEASE_TAG_TOKENS}|(?:19|20)\d{{2}})\b"
    ))
    .unwrap()
});

/// Whether the excluded keyword `kw` of `stem`, after an episode code that ends at `code_end`,
/// describes the file (an extras clip) rather than being a word of the episode title. It is a
/// descriptor in the tag part (after the first release tag that follows the code:
/// `Show.S01E01.1080p.WEB-GRP.featurette`), and as the first word after the code when the end of the
/// name, a release tag or a trailing `-GROUP` follows it (`Show.S01E05.Featurette`,
/// `Show.S01E05-trailer`, `Show.S01E05.Extras.1080p`, `Show.S01E05.Featurette-GRP`). Anything else is
/// a title word: `Show.S03E07.The.Extra.1080p`, `Show - S02E05 - Bonus Round`.
fn keyword_after_code_is_descriptor(stem: &str, code_end: usize, kw: &regex::Match<'_>) -> bool {
    let between = &stem[code_end..kw.start()];
    if WORD_RE
        .find_iter(between)
        .any(|w| RELEASE_TAG_RE.is_match(&between[w.start()..]))
    {
        return true;
    }
    if !WORD_RE
        .find_iter(between)
        .all(|w| CODE_TOKEN_RE.is_match(w.as_str()))
    {
        return false;
    }
    let rest = &stem[kw.end()..];
    match WORD_RE.find(rest) {
        None => true,
        Some(w) => {
            RELEASE_TAG_RE.is_match(&rest[w.start()..])
                || (rest[..w.start()].ends_with('-') && w.end() == rest.len())
        }
    }
}

/// Regex matching release-site / release-group advertisement clips that ship
/// alongside the real media (e.g. `RARBG.com.mp4`, `www.YTS.MX.mp4`). Matched
/// against the filename *stem* (extension stripped) and anchored end-to-end, so
/// only a filename that is entirely a promo token — optionally a `www.` prefix
/// and a single trailing domain segment — is excluded. A real title that merely
/// contains the token (e.g. `RARBG.com.Mission.Impossible.2015.mkv`) is kept.
static EXCLUDED_PROMO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(www\.)?(rarbg|rarbgt|yts|yify|eztv|ettv|etrg|torrentgalaxy|tgx)(\.[a-z]{2,6})?$",
    )
    .unwrap()
});

/// Directory names that indicate extras/bonus content. Checked case-insensitively, for files with no
/// episode code only (a real episode may be mis-foldered under one).
const EXCLUDED_DIRS: &[&str] = &[
    "extras",
    "bonus",
    "featurettes",
    "behind the scenes",
    "deleted scenes",
    "interviews",
    "trailers",
];

/// Sample folder names (`Sample/` is the scene standard). Checked case-insensitively for EVERY file,
/// episode-coded ones included: no real episode lives in a sample folder.
const SAMPLE_DIRS: &[&str] = &["sample", "samples"];

pub fn is_video_file(path: &str) -> bool {
    // ASCII lowercasing: the extensions/keywords/episode codes matched below are all ASCII, so
    // full-Unicode folding is needless work on this per-file hot path (`build` re-runs every scan
    // tick, calling this multiple times per file). A non-ASCII title char can't be part of any
    // ASCII pattern either way, so the classification is unchanged. `_` becomes `.` in the same
    // pass: the keyword and year regexes below rely on `\b`, which treats `_` as a word character,
    // so `show_s01e01_sample` and `movie_2010_trailer` would otherwise pass as content. Both are one
    // byte, so every offset below indexes the stem as it would the original name.
    let lower: String = path
        .chars()
        .map(|c| {
            if c == '_' {
                '.'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    let filename = lower.rsplit('/').next().unwrap_or(&lower);
    // The filename stem (video extension stripped) — used by the leading-keyword/episode/promo checks.
    // No video extension means not a video: return before any regex runs (most non-video files —
    // subtitles, NFOs, images, archives — end here). `.m2ts` is not shadowed by `.ts`:
    // `"x.m2ts".strip_suffix(".ts")` is `None`.
    let Some(stem) = VIDEO_EXTENSIONS
        .iter()
        .find_map(|ext| filename.strip_suffix(ext))
    else {
        return false;
    };
    let episode = episode_code_span(stem);
    // Decide whether an excluded keyword (sample/trailer/extras/…) marks this as an ANCILLARY clip
    // rather than real content. Every occurrence is judged, and ANY descriptor makes the file
    // ancillary. An occurrence is a TITLE word when:
    // - it LEADS the stem as `trailer`/`extras` (the show "Extras"/"Trailer Park Boys", the movie
    //   "Extras 2020"), unless a resolution/source tag immediately follows it (a metadata-suffixed
    //   clip like `Trailer.1080p.WEB-DL`, which would otherwise be surfaced as a bogus "movie");
    // - it comes BEFORE the episode code ("Some.Trailer.Show.S02E03");
    // - it is a word of the episode title AFTER the code ("Show.S03E07.The.Extra.1080p",
    //   "Show - S02E05 - Bonus Round"): not alone as the code's first word, and not in the tag part
    //   (`keyword_after_code_is_descriptor`);
    // - with no episode code, it sits mid-title BEFORE the release year with no year ahead of it
    //   ("The.Extra.Man.2010.1080p", "The.Bonus.Army.2024").
    // `sample` is never a title word, wherever it sits ("sample-show.s01e01…" is a sample).
    // Anything else is a descriptor: alone after the episode code ("Show.S01E05.Featurette",
    // "Show.S01E05-trailer", "Show.S01E05.Extras.1080p"), in the tag part after it
    // ("Show.S01E01.1080p.WEB-GRP.featurette"; an episode code does NOT rescue a sample either), after
    // the year ("Movie.2010.Trailer"), trailing with no year ("Movie.Name.Trailer"), or, with no
    // episode code, leading the stem as `bonus`/`featurette`.
    // KNOWN LIMITATIONS: a title that leads with `bonus`/`featurette` ("Bonus.Track.2023") is still
    // excluded, and a trailer named with the keyword before the year ("Movie.Trailer.2010") is kept.
    // After an episode code, an episode titled with the bare keyword is excluded
    // ("Show.S01E05.Extras.1080p" reads as an extras clip), and a clip whose keyword sits among
    // other words before the tags is kept ("Show.S01E05.Featurette.Making.Of",
    // "Show.S01E05.Making.Of.Featurette").
    let leading_title =
        EXCLUDED_LEADING_RE.is_match(stem) && !EXCLUDED_LEADING_METADATA_RE.is_match(stem);
    let ancillary = EXCLUDED_VIDEO_RE.find_iter(stem).any(|kw| {
        let title_word = if kw.as_str() == "sample" {
            false
        } else if kw.start() == 0 && leading_title {
            true
        } else if let Some((start, end)) = episode {
            kw.start() < start || !keyword_after_code_is_descriptor(stem, end, &kw)
        } else {
            kw.start() > 0
                && RELEASE_YEAR_RE.is_match(&stem[kw.end()..])
                && !RELEASE_YEAR_RE.is_match(&stem[..kw.start()])
        };
        !title_word
    });
    if ancillary {
        return false;
    }
    // Exclude release-site / release-group advertisement clips (e.g. "RARBG.com.mp4").
    if EXCLUDED_PROMO_RE.is_match(stem) {
        return false;
    }
    // Exclude files in a sample folder (always), and in directories that indicate extras/bonus
    // content unless the file is a real episode (a mis-foldered "Extras/Show.S01E01.mkv" episode
    // must not be dropped).
    for component in lower.split('/') {
        if component == filename {
            continue;
        }
        // Normalise scene separators (`Deleted.Scenes`, `behind_the_scenes`) to spaces so the
        // dotted/underscored directory forms match the space-separated EXCLUDED_DIRS entries —
        // not only literal-space directory names.
        let normalized = normalize_dir_component(component);
        let dir = normalized.as_ref();
        if SAMPLE_DIRS.contains(&dir) || (episode.is_none() && EXCLUDED_DIRS.contains(&dir)) {
            return false;
        }
    }
    true
}

/// Collapse `.`/`_`/`-`/space runs in a directory component to single spaces so scene-style names
/// (`Behind.The.Scenes`, `deleted_scenes`) compare equal to the space-separated `EXCLUDED_DIRS`
/// entries. Returns the component borrowed unchanged when it has no such separators (the common
/// case), so a normal component costs no allocation.
fn normalize_dir_component(component: &str) -> std::borrow::Cow<'_, str> {
    if component.contains(['.', '_', '-']) {
        std::borrow::Cow::Owned(
            component
                .split(['.', '_', '-', ' '])
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        )
    } else {
        std::borrow::Cow::Borrowed(component)
    }
}

/// The season numbers an arbitrary string (a file name or one directory name) names via SEASON_RE,
/// as `(strong, part)`: `strong` from its first `Sxx` / `Season N` / `NxNN` token, `part` from its
/// first `Part N` token. Kept apart because `Part N` is the weaker signal (a two-part episode title
/// as often as a season). An `NxNN` token that is a channel layout run into an `x264`/`x265` codec
/// (`AAC2.0x264`, `is_channel_layout_codec`) names no season. Used by the season-folder placement
/// in the show-emit loop, and by the upgrade engine to tell a season pack's torrent name
/// (`Show.S01.1080p`) from a single's (`upgrade::is_pack_stream`).
pub(crate) fn season_tokens(s: &str) -> (Option<u32>, Option<u32>) {
    let (mut strong, mut part) = (None, None);
    let mut at = 0;
    while let Some(cap) = SEASON_RE.captures_at(s, at) {
        let number = |i| cap.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
        // An `NxNN` token that is a channel layout run into a codec (`AAC2.0x264`) names no season,
        // as `find_nxm` reads it.
        let nxnn = cap
            .get(3)
            .filter(|m| !is_channel_layout_codec(s, m))
            .and_then(|m| m.as_str().parse::<u32>().ok());
        strong = strong.or_else(|| number(1).or_else(|| number(2)).or(nxnn));
        part = part.or_else(|| number(4));
        if strong.is_some() && part.is_some() {
            break;
        }
        // A token's trailing separator can be the next token's leading one (`Part_2_S03`), so the
        // next search starts on it. A match is at least two characters long before that separator,
        // so the search always moves forward.
        let Some(whole) = cap.get(0) else { break };
        at = match whole.as_str().chars().next_back() {
            Some(c) if !c.is_ascii_alphanumeric() => whole.end() - c.len_utf8(),
            _ => whole.end(),
        };
    }
    (strong, part)
}

/// The season folder of a show file with no `parse_se_all` episode code, from strongest signal to
/// weakest: an `Sxx` / `Season N` / `NxNN` token in the file name; else the NEAREST directory that
/// names a season (preferring such a strong token within one directory name over its `Part N`); else
/// a `Part N` in the file name; else season 1.
///
/// The file name keeps precedence for a strong token, so a contradictory "1x05" in a "/Season 03/"
/// path still prefers the file name. Season packs often name the season only in a directory
/// ("Show/Season 04/09 - Title.mkv"), and the NEAREST one decides because a TorBox path includes the
/// torrent's root folder: a multi-season pack's root ("Show.S01-S03/Season 2/01.mkv") would
/// otherwise put every season in the root's first one. A `Part N` in the file name is usually a
/// two-part episode title ("Season 3/05 - Title, Part 2.mkv"), so any directory season beats it. A
/// `Part N` DIRECTORY, though, names a season (a show released in parts, "Show/Part 3/01.mkv"), and
/// deliberately beats a season token in an outer directory too ("Show.S04/Part 2/01.mkv" lands in
/// Season 02), since the root of such a pack often names a season RANGE ("Show.S01-S05/Part 3/")
/// whose first season would be wrong. A directory that names only a season range
/// (`Show Seasons 2-3/01.mkv`) places the file in the range's first season, as `Show.S02-S03/`
/// does. The episode mapping reads an absolute-numbered single-file release's season from it too
/// ([`codeless_episode`], [`codeless_episode_files`]), so the engine slots the episode the VFS
/// places the file under. For a file of a multi-file pack it reads
/// [`explicit_season_of_codeless_file`], built on the same lookup ([`nearest_codeless_season`]).
pub(crate) fn season_of_codeless_file(path: &str, filename: &str) -> u32 {
    let (_, part) = season_tokens(filename);
    nearest_codeless_season(path, filename)
        .map(|(season, _)| season)
        .or(part)
        .unwrap_or(1)
}

/// The season a show file with no episode code names EXPLICITLY, as [`season_of_codeless_file`]
/// reads it: an `Sxx` / `Season N` / `NxNN` token in the file name, else the nearest directory
/// naming one (a `Part N` directory included). `None` when nothing does (the season-1 default and a
/// file-name `Part N`, usually a two-part episode's title, don't count), and when the nearest name
/// that names a season names a RANGE of them (`[Group] Show (Season 1-2)`, `Show.S01-S03`,
/// `Show.S01-03`, `Season 1&2`, `Seasons 1-3`; [`season_range`]): which season the file is in isn't
/// said there, and an outer directory says even less. A code-less file of a multi-file pack is
/// mapped only under such a season ([`codeless_episode_files`]), since the season-1 default would
/// record an absolutely numbered batch's S02 episodes as S01E26 and on.
pub(crate) fn explicit_season_of_codeless_file(path: &str, filename: &str) -> Option<u32> {
    nearest_codeless_season(path, filename)
        .filter(|&(_, range)| !range)
        .map(|(season, _)| season)
}

/// The lookup [`season_of_codeless_file`] and [`explicit_season_of_codeless_file`] share: the
/// season of the strongest name that names one (the file name's strong token, else the nearest
/// directory's strong token or `Part N`, or the first season of the range it names when it names
/// only a range, as `Seasons 1-3` does), and whether that name names a season range
/// ([`season_range`]).
fn nearest_codeless_season(path: &str, filename: &str) -> Option<(u32, bool)> {
    if let Some(season) = season_tokens(filename).0 {
        return Some((season, season_range(filename, false).is_some()));
    }
    let dir = &path[..path.len() - filename.len()];
    dir.rsplit('/').filter(|c| !c.is_empty()).find_map(|c| {
        let (strong, part) = season_tokens(c);
        let range = season_range(c, true);
        match strong.or(part) {
            Some(season) => Some((season, range.is_some())),
            // `Seasons 1-3` holds no token `season_tokens` reads, yet names seasons.
            None => range.map(|first| (first, true)),
        }
    })
}

/// A season range in one name: `S01-S03`, `S01 - S03`, `Season 1-2`, `Seasons 1 - 3`,
/// `Season 1 to 3`, `Season 1 & 2`, `Season 1&2`, `Season 1+2`, `Season 1 - Season 2`. A dash with
/// a bare number after it is a range only glued to `Season N` (`Season 1-2`) or after `Seasons`:
/// `Show S2 - 05` and `Show Season 2 - 05` are season 2's episode 5. A glued `S01-03` is read as a
/// range only in a directory name ([`GLUED_SEASON_RANGE_RE`]).
static SEASON_RANGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[^a-z0-9])s(\d{1,2})[\s._]*[-~][\s._]*s(\d{1,2})(?:[^a-z0-9]|$)|season[\s._-]*(\d{1,2})[-~](\d{1,2})(?:[^a-z0-9]|$)|seasons[\s._-]*(\d{1,2})[\s._]*(?:-|~|&|\+|to|and)[\s._]*(\d{1,2})(?:[^a-z0-9]|$)|season[\s._-]*(\d{1,2})[\s._]*[&+][\s._]*(?:seasons?[\s._-]*)?(\d{1,2})(?:[^a-z0-9]|$)|season[\s._-]*(\d{1,2})[\s._]+(?:to|and)[\s._]+(?:seasons?[\s._-]*)?(\d{1,2})(?:[^a-z0-9]|$)|season[\s._-]*(\d{1,2})[\s._]*[-~][\s._]*seasons?[\s._-]*(\d{1,2})(?:[^a-z0-9]|$)",
    )
    .unwrap()
});

/// An `Sxx` token with a second, bare season number glued on by a dash (`Show.S01-03`,
/// `Show S1-3 [1080p]`). A directory name holds no episode, so there the second number is a season
/// when it is the greater one. A file name's `S2-05` may be an episode, so [`season_range`] reads
/// this only for a directory.
static GLUED_SEASON_RANGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^a-z0-9])s(\d{1,2})[-~](\d{1,2})(?:[^a-z0-9]|$)").unwrap()
});

/// The first season of the range of seasons a file or directory name (`dir`) names rather than
/// one season, or `None`: a range token ([`SEASON_RANGE_RE`]) of two different seasons, in a
/// directory a glued `S01-03` ([`GLUED_SEASON_RANGE_RE`]) whose second number is the greater (not
/// the whole part of a channel layout, `S02-5.1`), or two different seasons named apart
/// (`Show.S01.S02`), where an aspect ratio (`16x9`, a single digit after the `x`) names no second
/// season.
fn season_range(s: &str, dir: bool) -> Option<u32> {
    let ranged = SEASON_RANGE_RE.captures_iter(s).find_map(|c| {
        let mut numbers = c
            .iter()
            .skip(1)
            .flatten()
            .filter_map(|m| m.as_str().parse::<u32>().ok());
        match (numbers.next(), numbers.next()) {
            (Some(a), Some(b)) if a != b => Some(a),
            _ => None,
        }
    });
    if ranged.is_some() {
        return ranged;
    }
    if dir {
        let glued = GLUED_SEASON_RANGE_RE.captures_iter(s).find_map(|c| {
            let (first, second) = (c.get(1)?, c.get(2)?);
            let a = first.as_str().parse::<u32>().ok()?;
            let b = second.as_str().parse::<u32>().ok()?;
            let after = &s.as_bytes()[second.end()..];
            (b > a && !is_channel_layout(second.as_str(), after)).then_some(a)
        });
        if glued.is_some() {
            return glued;
        }
    }
    // Two different strong tokens apart: `season_tokens` reads only the first, so read on past it.
    let first = season_tokens(s).0?;
    let mut at = 0;
    while let Some(cap) = SEASON_RE.captures_at(s, at) {
        let Some(whole) = cap.get(0) else { break };
        let number = |i| cap.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
        let nxnn = cap
            .get(3)
            .filter(|m| !is_channel_layout_codec(s, m) && !is_aspect_ratio(s, m))
            .and_then(|m| m.as_str().parse::<u32>().ok());
        if number(1)
            .or_else(|| number(2))
            .or(nxnn)
            .is_some_and(|n| n != first)
        {
            return Some(first);
        }
        at = match whole.as_str().chars().next_back() {
            Some(c) if !c.is_ascii_alphanumeric() => whole.end() - c.len_utf8(),
            _ => whole.end(),
        };
    }
    None
}

/// Whether the `NxNN` token whose season digits are `season` (a match in `s` that ends at the
/// token's `x`) is an aspect ratio (`16x9`, `4x3`, `21x9`): a single digit after the `x`, where an
/// episode number has two or three.
fn is_aspect_ratio(s: &str, season: &regex::Match<'_>) -> bool {
    let after = s.as_bytes().get(season.end() + 1..).unwrap_or_default();
    after.first().is_some_and(u8::is_ascii_digit) && !after.get(1).is_some_and(u8::is_ascii_digit)
}

/// Map a torrent's SELECTED video files to `(season, episode, file_path)` by their episode codes
/// ([`parse_se_all`]: the `SxxEyy`, `NxM`, separated and dash-concatenated forms). `pub(crate)` for
/// the account mirror (`tasks::record_mirror_owned`) and `tasks::backfill_owned_provides`, which
/// read episode codes only. `observe` and the upgrade engine's staging sites read
/// [`show_episode_files`], which adds the code-less mapping.
///
/// Each episode appears once, from the FIRST file (in provider file order) that carries it: a pack
/// holding one episode twice (per-language folders, two resolutions) is title-checked and probed on
/// that file by `observe`, so its slot must serve that file and `provides` must list the episode
/// once (a single-episode torrent with two versions is not a pack).
pub(crate) fn episode_files(info: &TorrentInfo) -> Vec<(u32, u32, String)> {
    let mut seen = std::collections::HashSet::new();
    info.files
        .iter()
        .filter(|f| f.selected == 1 && is_video_file(&f.path))
        .flat_map(|f| {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path);
            // A multi-episode file contributes ALL its episodes (so `provides` is complete and the
            // reconciler doesn't re-acquire a contained episode as a duplicate singleton).
            parse_se_all(name)
                .into_iter()
                .map(move |(s, e)| (s, e, f.path.clone()))
        })
        .filter(|(s, e, _)| seen.insert((*s, *e)))
        .collect()
}

/// An episode named by a word and a number (`E05`, `Ep 5`, `Episode.12`), bounded like the season
/// tokens, which [`parse_se_all`] doesn't read apart from a season code. Capture 1 is the number.
/// `pub(crate)`: the upgrade engine's `is_pack_stream` reads it to keep such a stream.
pub(crate) static EPISODE_WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^a-z0-9])(?:e|ep|episode)[\s._-]*(\d{1,3})(?:[^a-z0-9]|$)").unwrap()
});

/// The episode number a show file name with no [`parse_se_all`] code names by itself: an episode
/// word (`E09 - Title.mkv`, `Episode 9.mkv`, [`EPISODE_WORD_RE`]), else a trailing ` - NN` number,
/// the common anime form (`[Group] Show - 12 (1080p) [ABCD1234].mkv`,
/// `identification::trailing_episode_number`). Never 0.
fn codeless_episode_number(name: &str) -> Option<u32> {
    EPISODE_WORD_RE
        .captures(name)
        .and_then(|c| c.get(1)?.as_str().parse().ok())
        .or_else(|| crate::identification::trailing_episode_number(name))
        .filter(|&e| e >= 1)
}

/// The episode a show file with no [`parse_se_all`] code names ([`codeless_episode_number`]), in
/// the season the VFS places the file in ([`season_of_codeless_file`]: season 1 unless the name or
/// a directory names one, so `Show S2 - 05 (1080p).mkv` is S02E05). `None` for a file with an
/// episode code or no such number. An absolute number past season 1 (`Show - 30` for S02E05)
/// therefore names no requested episode. A file of a multi-file pack is mapped only under an
/// explicit season ([`codeless_episode_files`]); this is the acquisition title check's reading of
/// the one file it is given.
pub(crate) fn codeless_episode(path: &str) -> Option<(u32, u32)> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if !parse_se_all(name).is_empty() {
        return None;
    }
    let episode = codeless_episode_number(name)?;
    Some((season_of_codeless_file(path, name), episode))
}

/// The selected video files of a show torrent that carry no [`parse_se_all`] code but name their
/// episode by [`codeless_episode_number`], each mapped to `(season, episode, file_path)`:
///
/// - a single-file release in the season the VFS places it in ([`codeless_episode`]; season 1
///   unless something names one);
/// - a file of a multi-file pack only under an explicit season
///   ([`explicit_season_of_codeless_file`]: a `Season 04/E09 - Title.mkv`, a
///   `[Group] Show S2 - 05.mkv`). A pack of absolutely numbered files (`[Group] Show - 01..50`)
///   maps nothing, and nor does one under a season RANGE (`[Group] Show (Season 1-2)/`,
///   `Show.S01-S03/`): placing a number past season 1 needs TMDB's per-season episode counts, and
///   the season-1 default would record S01E26..E50 for what TMDB calls S02.
///
/// The mapping is all or nothing: when a mapped episode is also named by another selected video
/// file (coded or not), the names are ambiguous and nothing is mapped. `observe` and the upgrade
/// engine's staging sites map episodes with it ([`show_episode_files`]), the VFS build gates a
/// code-less file on the episode it maps to, and `tasks::holds_codeless_video` (dedup and the
/// show-swap prune guard) reads it to tell whether a record's `provides` accounts for a code-less
/// file. The account mirror's `provides` reads episode codes alone ([`episode_files`]), so a
/// user's own code-less copy keeps an empty `provides` (and stays protected), while the build
/// hides a code-less file whose episode another present copy's slot serves.
pub(crate) fn codeless_episode_files(info: &TorrentInfo) -> Vec<(u32, u32, String)> {
    let videos: Vec<&crate::rd_client::TorrentFile> = info
        .files
        .iter()
        .filter(|f| f.selected == 1 && is_video_file(&f.path))
        .collect();
    let single = videos.len() == 1;
    let mut named: HashMap<(u32, u32), usize> = HashMap::new();
    let mut mapped = Vec::new();
    for f in &videos {
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        let codes = parse_se_all(name);
        if !codes.is_empty() {
            for ep in codes {
                *named.entry(ep).or_default() += 1;
            }
            continue;
        }
        let Some(episode) = codeless_episode_number(name) else {
            continue;
        };
        let season = if single {
            Some(season_of_codeless_file(&f.path, name))
        } else {
            explicit_season_of_codeless_file(&f.path, name)
        };
        if let Some(season) = season {
            *named.entry((season, episode)).or_default() += 1;
            mapped.push((season, episode, f.path.clone()));
        }
    }
    if mapped.iter().any(|(s, e, _)| named[&(*s, *e)] > 1) {
        return Vec::new();
    }
    mapped
}

/// Every episode a show torrent's selected video files supply, as `(season, episode, file_path)` in
/// provider file order: the coded files' episodes ([`episode_files`], each episode once, from its
/// first file) and the code-less files [`codeless_episode_files`] maps. What `observe` records as a
/// verified show copy's `provides` (and slots), and what the upgrade engine's staging sites read
/// (per-episode staging, pack correction and consolidation), so they accept and record the
/// releases acquisition accepts the way `observe` records them.
pub(crate) fn show_episode_files(info: &TorrentInfo) -> Vec<(u32, u32, String)> {
    let codeless: HashMap<String, (u32, u32)> = codeless_episode_files(info)
        .into_iter()
        .map(|(s, e, path)| (path, (s, e)))
        .collect();
    if codeless.is_empty() {
        return episode_files(info);
    }
    let mut coded = episode_files(info).into_iter().peekable();
    let mut out = Vec::new();
    for f in info
        .files
        .iter()
        .filter(|f| f.selected == 1 && is_video_file(&f.path))
    {
        if let Some(&(s, e)) = codeless.get(&f.path) {
            out.push((s, e, f.path.clone()));
        }
        while let Some(entry) = coded.next_if(|(_, _, path)| *path == f.path) {
            out.push(entry);
        }
    }
    out
}

fn xml_escape(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&apos;"),
            // Strip XML 1.0 invalid control chars (keep tab, newline, carriage return), plus the
            // two non-character code points U+FFFE/U+FFFF, which are also invalid under the XML 1.0
            // `Char` production and would make a strict media-server NFO parser reject the file.
            '\u{0000}'..='\u{0008}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000E}'..='\u{001F}'
            | '\u{FFFE}'
            | '\u{FFFF}' => {}
            _ => result.push(c),
        }
    }
    result
}

/// Diff two VFS trees and return the list of changed paths at the deepest
/// meaningful level (e.g. season folder for a new episode, movie folder for
/// a new movie).
///
/// Changes are reported at the directory level, never at the individual file
/// level. When leaf nodes (MediaFile, VirtualFile) inside a directory change,
/// the directory itself is reported as Modified.
pub fn diff_trees(old: &VfsNode, new: &VfsNode, prefix: &str) -> Vec<VfsChange> {
    let mut changes = Vec::new();

    let (old_children, new_children) = match (old, new) {
        (VfsNode::Directory { children: old_c }, VfsNode::Directory { children: new_c }) => {
            (old_c, new_c)
        }
        // Both are the same leaf — compare for equality
        (a, b) if a == b => return changes,
        // Both are leaves but different — parent will report this
        _ => return changes,
    };

    // Track whether any non-directory (leaf) children changed. If so, we
    // report this directory as the change point rather than individual files.
    let mut has_leaf_changes = false;
    // Built only for the directory branches that use it (not per leaf).
    let child_path_of = |name: &str| {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", prefix, name)
        }
    };

    for (name, old_child) in old_children {
        match new_children.get(name) {
            None => {
                match old_child {
                    VfsNode::Directory { .. } => {
                        // A child directory was removed — report it as deleted
                        changes.push(VfsChange {
                            path: child_path_of(name),
                            update_type: UpdateType::Deleted,
                        });
                    }
                    _ => {
                        // A leaf was removed — flag for parent-level reporting
                        has_leaf_changes = true;
                    }
                }
            }
            Some(new_child) => {
                match (old_child, new_child) {
                    (VfsNode::Directory { .. }, VfsNode::Directory { .. }) => {
                        // Both directories — recurse. Two differing directories always yield at
                        // least one change at or below `child_path` (a removed/added subdirectory
                        // or a leaf difference reported against `child_path`), so the result needs
                        // no extra deep comparison.
                        changes.extend(diff_trees(old_child, new_child, &child_path_of(name)));
                    }
                    (a, b) if a != b => {
                        // Leaf node changed — flag for parent-level reporting
                        has_leaf_changes = true;
                    }
                    _ => {
                        // Identical leaf nodes — no change
                    }
                }
            }
        }
    }

    // Check for new children
    for (name, new_child) in new_children {
        if !old_children.contains_key(name) {
            match new_child {
                VfsNode::Directory { children } => {
                    // A new child directory — report the deepest new directory
                    let deepest = find_deepest_new_dir(&child_path_of(name), children);
                    changes.push(VfsChange {
                        path: deepest,
                        update_type: UpdateType::Created,
                    });
                }
                _ => {
                    // A new leaf was added — flag for parent-level reporting
                    has_leaf_changes = true;
                }
            }
        }
    }

    // If any leaf-level changes occurred, report this directory as modified.
    // But only if we have a prefix (i.e., we're not at the root level).
    if has_leaf_changes && !prefix.is_empty() {
        changes.push(VfsChange {
            path: prefix.to_string(),
            update_type: UpdateType::Modified,
        });
    }

    changes
}

/// Walk down a newly created directory tree to find the deepest directory.
/// For "Shows/Breaking Bad/Season 01/S01E01.mkv", returns "Shows/Breaking Bad/Season 01".
fn find_deepest_new_dir(path: &str, children: &BTreeMap<String, VfsNode>) -> String {
    // If all children are leaves (files), this is the deepest directory
    let dir_children: Vec<(&String, &BTreeMap<String, VfsNode>)> = children
        .iter()
        .filter_map(|(name, node)| {
            if let VfsNode::Directory { children: c } = node {
                Some((name, c))
            } else {
                None
            }
        })
        .collect();

    // Only descend when the sole child is a single subdirectory. If there are also
    // leaf files at this level (e.g. a movie file alongside a `Subs/` folder), this
    // is the directory worth reporting — descending into the subdirectory would
    // notify the media server of the wrong path and the file could go unscanned.
    if children.len() == 1 && dir_children.len() == 1 {
        let (name, sub_children) = dir_children[0];
        let sub_path = format!("{}/{}", path, name);
        find_deepest_new_dir(&sub_path, sub_children)
    } else {
        path.to_string()
    }
}

/// Torrent fixtures and tree walks shared by the VFS unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{DebridVfs, VfsNode};
    use crate::provider::FileLocator;
    use crate::rd_client::{TorrentFile, TorrentInfo};
    use std::collections::BTreeMap;

    /// A Real-Debrid-style movie torrent: one selected `Movie.2023.1080p.mkv` of `bytes` bytes, with
    /// its restricted link.
    pub(crate) fn movie_torrent(id: &str, hash: &str, bytes: u64) -> TorrentInfo {
        TorrentInfo {
            id: id.into(),
            hash: hash.into(),
            bytes,
            status: "downloaded".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Movie.2023.1080p.mkv".into(),
                bytes,
                selected: 1,
            }],
            links: vec!["https://cdn/movie".into()],
            ..Default::default()
        }
    }

    /// A TorBox-style (link-less) show torrent whose selected files are all videos.
    pub(crate) fn show_torrent(id: &str, hash: &str, bytes: u64, paths: &[&str]) -> TorrentInfo {
        TorrentInfo {
            id: id.into(),
            hash: hash.into(),
            bytes,
            status: "downloaded".into(),
            files: paths
                .iter()
                .enumerate()
                .map(|(i, p)| TorrentFile {
                    id: i as u32,
                    path: (*p).into(),
                    bytes: bytes / paths.len() as u64,
                    selected: 1,
                })
                .collect(),
            ..Default::default()
        }
    }

    /// The children of the directory at `path` below `node` (`/`-separated; `""` is `node`
    /// itself). Panics naming the first segment that is missing or not a directory, so a test that
    /// walks the tree fails when the tree's shape changes instead of passing without asserting.
    pub(crate) fn dir<'a>(node: &'a VfsNode, path: &str) -> &'a BTreeMap<String, VfsNode> {
        let VfsNode::Directory { children } = node else {
            panic!("the walk from `{path}` must start at a directory");
        };
        let mut children = children;
        let mut walked = String::new();
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            match children.get(segment) {
                Some(VfsNode::Directory { children: next }) => children = next,
                Some(_) => panic!("`{segment}` under `{walked}/` is not a directory"),
                None => panic!(
                    "no `{segment}` under `{walked}/`; it holds {:?}",
                    children.keys().collect::<Vec<_>>()
                ),
            }
            walked = format!("{walked}/{segment}");
        }
        children
    }

    /// The media file at `path` below `node`: its size and locator. Panics like [`dir`] when the
    /// path is missing or names something else.
    pub(crate) fn media_file<'a>(node: &'a VfsNode, path: &str) -> (u64, &'a FileLocator) {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let files = dir(node, parent);
        match files.get(name) {
            Some(VfsNode::MediaFile { file_size, locator }) => (*file_size, locator),
            Some(_) => panic!("`{path}` is not a media file"),
            None => panic!(
                "no `{name}` in `{parent}/`; it holds {:?}",
                files.keys().collect::<Vec<_>>()
            ),
        }
    }

    /// The children of the only show folder under `Shows/`. Panics unless there is exactly one.
    pub(crate) fn only_show(vfs: &DebridVfs) -> &BTreeMap<String, VfsNode> {
        let shows = dir(&vfs.root, "Shows");
        let [(name, _)] = shows.iter().collect::<Vec<_>>()[..] else {
            panic!(
                "expected exactly one show folder, got {:?}",
                shows.keys().collect::<Vec<_>>()
            );
        };
        dir(&vfs.root, &format!("Shows/{name}"))
    }

    /// The sorted media-file names under `Shows/<every show>/<season>`.
    pub(crate) fn season_file_names(vfs: &DebridVfs, season: &str) -> Vec<String> {
        let mut names: Vec<String> = dir(&vfs.root, "Shows")
            .values()
            .filter_map(|show| match show {
                VfsNode::Directory { children: seasons } => seasons.get(season),
                _ => None,
            })
            .flat_map(|s| match s {
                VfsNode::Directory { children: eps } => eps
                    .iter()
                    .filter(|(_, n)| matches!(n, VfsNode::MediaFile { .. }))
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{dir, media_file, only_show};
    use super::*;
    use crate::rd_client::{TorrentFile, TorrentInfo};

    // The "skip the file on a missing link rather than inserting a broken-link placeholder"
    // behaviour is verified by `build_handles_link_count_mismatch` (which asserts no
    // placeholder node appears), not by grepping this file's source text.

    #[test]
    fn build_stores_file_locator_on_media_node() {
        let torrents = vec![(
            TorrentInfo {
                id: "tid".to_string(),
                filename: "Movie.2023.mkv".to_string(),
                hash: "deadbeef".to_string(),
                bytes: 1000,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![TorrentFile {
                    id: 7,
                    path: "/Movie.2023.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["https://rd/restricted".to_string()],
                ..Default::default()
            },
            MediaMetadata {
                title: "Movie".to_string(),
                year: Some("2023".to_string()),
                media_type: MediaType::Movie,
                external_id: None,
            },
        )];
        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
        let (file_size, locator) = media_file(&vfs.root, "Movies/Movie/Movie.2023.mkv");
        assert_eq!(file_size, 1000);
        assert_eq!(locator.hash, "deadbeef");
        assert_eq!(locator.torrent_id, "tid");
        assert_eq!(locator.file_id, 7);
        assert_eq!(locator.file_path, "/Movie.2023.mkv");
        assert_eq!(locator.link.as_deref(), Some("https://rd/restricted"));
    }

    #[test]
    fn build_excludes_release_site_promo_from_movie_folder() {
        // A correctly-identified movie torrent that also ships a release-site
        // advertisement clip ("RARBG.com.mp4") must expose only the real film,
        // not the junk promo — and the promo must not consume a per-file link.
        let torrents = vec![(
            TorrentInfo {
                id: "mi".to_string(),
                filename: "Mission.Impossible.Rogue.Nation.2015.1080p.BluRay.x264-RARBG"
                    .to_string(),
                hash: "h".to_string(),
                bytes: 2_000_000_000,
                status: "downloaded".to_string(),
                added: "2015-01-01".to_string(),
                files: vec![
                    TorrentFile {
                        id: 1,
                        path: "Mission.Impossible.Rogue.Nation.2015.1080p.BluRay.x264-RARBG.mkv"
                            .to_string(),
                        bytes: 2_000_000_000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 2,
                        path: "RARBG.com.mp4".to_string(),
                        bytes: 1_000_000,
                        selected: 1,
                    },
                ],
                // Two selected files → two restricted links, in file order.
                links: vec![
                    "http://link_movie".to_string(),
                    "http://link_promo".to_string(),
                ],
                ..Default::default()
            },
            MediaMetadata {
                title: "Mission: Impossible - Rogue Nation".to_string(),
                year: Some("2015".to_string()),
                media_type: MediaType::Movie,
                external_id: Some("tmdb:177677".to_string()),
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
        let folder = "Movies/Mission - Impossible - Rogue Nation [tmdbid-177677]";
        let files = dir(&vfs.root, folder);
        assert!(
            !files.contains_key("RARBG.com.mp4"),
            "promo clip must be excluded, got: {:?}",
            files.keys().collect::<Vec<_>>()
        );
        // The real film must still pair with its own (first) link, not the promo's.
        let (_, locator) = media_file(
            &vfs.root,
            &format!("{folder}/Mission.Impossible.Rogue.Nation.2015.1080p.BluRay.x264-RARBG.mkv"),
        );
        assert_eq!(locator.link.as_deref(), Some("http://link_movie"));
    }

    #[test]
    fn build_falls_back_to_streamable_movie_when_largest_is_archive_only() {
        // Two torrents in the same movie group: the LARGER is archive-only (RAR — unstreamable),
        // the smaller has a real video. The movie must still appear, sourced from the streamable
        // copy, rather than vanishing because the largest candidate yields no video.
        let archive = (
            TorrentInfo {
                id: "big".to_string(),
                filename: "Movie.2020.2160p".to_string(),
                hash: "harchive".to_string(),
                bytes: 9_000_000_000,
                status: "downloaded".to_string(),
                added: "2020-01-01".to_string(),
                files: vec![TorrentFile {
                    id: 1,
                    path: "Movie.2020.2160p.rar".to_string(),
                    bytes: 9_000_000_000,
                    selected: 1,
                }],
                ..Default::default()
            },
            MediaMetadata {
                title: "Movie".to_string(),
                year: Some("2020".to_string()),
                media_type: MediaType::Movie,
                external_id: Some("tmdb:500".to_string()),
            },
        );
        let streamable = (
            TorrentInfo {
                id: "small".to_string(),
                filename: "Movie.2020.1080p".to_string(),
                hash: "hvideo".to_string(),
                bytes: 2_000_000_000,
                status: "downloaded".to_string(),
                added: "2020-01-01".to_string(),
                files: vec![TorrentFile {
                    id: 1,
                    path: "Movie.2020.1080p.mkv".to_string(),
                    bytes: 2_000_000_000,
                    selected: 1,
                }],
                ..Default::default()
            },
            MediaMetadata {
                title: "Movie".to_string(),
                year: Some("2020".to_string()),
                media_type: MediaType::Movie,
                external_id: Some("tmdb:500".to_string()),
            },
        );
        // Archive-only listed first (it is the larger of the two).
        let vfs = DebridVfs::build(vec![archive, streamable], &crate::vfs::SelectionMap::new());
        let (_, folder) = dir(&vfs.root, "Movies")
            .iter()
            .next()
            .expect("the movie folder must exist (not vanish to the archive-only torrent)");
        let VfsNode::Directory { children: files } = folder else {
            panic!("movie folder is a directory");
        };
        assert!(
            files.contains_key("Movie.2020.1080p.mkv"),
            "the streamable copy must be surfaced, got: {:?}",
            files.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn build_includes_linkless_torbox_file() {
        // A TorBox-style movie torrent: files present, but no per-file links array.
        let torrents = vec![(
            TorrentInfo {
                id: "99".to_string(),
                filename: "Sintel".to_string(),
                hash: "deadbeef".to_string(),
                status: "downloaded".to_string(),
                added: "2024-01-01".to_string(),
                files: vec![TorrentFile {
                    id: 10,
                    path: "Sintel/Sintel.mp4".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec![], // TorBox has no per-file links
                ..Default::default()
            },
            MediaMetadata {
                title: "Sintel".to_string(),
                year: None,
                media_type: MediaType::Movie,
                external_id: None,
            },
        )];
        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
        // The movie folder and its media file appear even without a link.
        let (_, locator) = media_file(&vfs.root, "Movies/Sintel/Sintel.mp4");
        assert_eq!(locator.link, None);
        assert_eq!(locator.torrent_id, "99");
        assert_eq!(locator.file_id, 10);
        assert_eq!(locator.hash, "deadbeef");
    }

    #[test]
    fn test_vfs_update() {
        let torrents = vec![
            (
                TorrentInfo {
                    id: "1".to_string(),
                    filename: "Movie.2023.1080p.mkv".to_string(),
                    hash: "hash1".to_string(),
                    bytes: 1000,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.2023.1080p.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    }],
                    links: vec!["http://link1".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "Movie".to_string(),
                    year: Some("2023".to_string()),
                    media_type: MediaType::Movie,
                    external_id: None,
                },
            ),
            (
                TorrentInfo {
                    id: "2".to_string(),
                    filename: "Show.S01E01.mkv".to_string(),
                    hash: "hash2".to_string(),
                    bytes: 500,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Show.S01E01.mkv".to_string(),
                        bytes: 500,
                        selected: 1,
                    }],
                    links: vec!["http://link2".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "Show".to_string(),
                    year: Some("2023".to_string()),
                    media_type: MediaType::Show,
                    external_id: None,
                },
            ),
        ];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let files = dir(&vfs.root, "Movies/Movie");
        assert!(
            files.contains_key("Movie.2023.1080p.mkv"),
            "Movie media file not found"
        );
        assert!(files.contains_key("movie.nfo"), "movie.nfo not found");

        let seasons = dir(&vfs.root, "Shows/Show");
        assert!(seasons.contains_key("Season 01"), "Season 01 not found");
        assert!(seasons.contains_key("tvshow.nfo"), "tvshow.nfo not found");
    }

    #[test]
    fn test_nfo_generation() {
        let metadata = MediaMetadata {
            title: "Test Movie".to_string(),
            year: Some("2024".to_string()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:12345".to_string()),
        };
        let content = String::from_utf8(DebridVfs::generate_nfo(&metadata)).unwrap();
        assert!(content.contains("<movie>"));
        assert!(content.contains("<title>Test Movie</title>"));
        assert!(content.contains("<originaltitle>Test Movie</originaltitle>"));
        assert!(content.contains("<year>2024</year>"));
        assert!(content.contains("<premiered>2024-01-01</premiered>"));
        assert!(content.contains("<uniqueid type=\"tmdb\" default=\"true\">12345</uniqueid>"));
        assert!(content.contains("<tmdbid>12345</tmdbid>"));
        assert!(content.contains("<url>https://www.themoviedb.org/movie/12345</url>"));
        assert!(content.contains("<lockdata>false</lockdata>"));
        assert!(content.contains("<source>debridmoviemapper</source>"));
    }

    #[test]
    fn nfo_premiered_only_emitted_for_valid_four_digit_year() {
        // A clean 4-digit year emits <premiered>.
        let good = MediaMetadata {
            title: "Movie".to_string(),
            year: Some("2024".to_string()),
            media_type: MediaType::Movie,
            external_id: None,
        };
        let good_nfo = String::from_utf8(DebridVfs::generate_nfo(&good)).unwrap();
        assert!(good_nfo.contains("<premiered>2024-01-01</premiered>"));

        // A malformed year still emits <year> (best-effort) but NOT an invalid <premiered>.
        for bad_year in ["20", "abcd", "2024 ", "20245"] {
            let bad = MediaMetadata {
                title: "Movie".to_string(),
                year: Some(bad_year.to_string()),
                media_type: MediaType::Movie,
                external_id: None,
            };
            let nfo = String::from_utf8(DebridVfs::generate_nfo(&bad)).unwrap();
            assert!(
                !nfo.contains("<premiered>"),
                "year {:?} must not produce a <premiered> tag",
                bad_year
            );
        }
    }

    #[test]
    fn build_sanitizes_path_separator_in_external_id() {
        // A crafted external_id with a path separator must not leak into the folder path.
        let torrents = vec![(
            TorrentInfo {
                id: "1".to_string(),
                filename: "Movie.mkv".to_string(),
                hash: "h".to_string(),
                bytes: 1000,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![TorrentFile {
                    id: 1,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["http://link1".to_string()],
                ..Default::default()
            },
            MediaMetadata {
                title: "Movie".to_string(),
                year: None,
                media_type: MediaType::Movie,
                external_id: Some("tmdb:12/34".to_string()),
            },
        )];
        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
        let key = dir(&vfs.root, "Movies")
            .keys()
            .next()
            .expect("a movie folder");
        assert!(
            !key.contains('/'),
            "folder name must not contain '/': {key}"
        );
        assert_eq!(key, "Movie [tmdbid-12-34]");
    }

    #[test]
    fn build_does_not_lose_a_group_when_an_unidentified_title_mimics_an_identified_folder() {
        // Regression: the identified path builds `Title [tmdbid-N]` without a uniqueness guard, so an
        // unidentified group whose cleaned title is EXACTLY that string must not silently overwrite
        // (data-loss) the identified group's folder. Both must survive (one ` (N)`-suffixed).
        let mk = |id: &str, hash: &str, title: &str, ext: Option<&str>| {
            (
                TorrentInfo {
                    id: id.to_string(),
                    filename: "Movie.mkv".to_string(),
                    hash: hash.to_string(),
                    bytes: 1000,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    }],
                    links: vec!["http://link".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: title.to_string(),
                    year: None,
                    media_type: MediaType::Movie,
                    external_id: ext.map(String::from),
                },
            )
        };
        // Identified "Movie" (tmdb:5) → folder "Movie [tmdbid-5]"; unidentified title that mimics it.
        let torrents = vec![
            mk("1", "h1", "Movie", Some("tmdb:5")),
            mk("2", "h2", "Movie [tmdbid-5]", None),
        ];
        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
        let movie_children = dir(&vfs.root, "Movies");
        assert_eq!(
            movie_children.len(),
            2,
            "both groups must survive the folder-name collision, got keys: {:?}",
            movie_children.keys().collect::<Vec<_>>()
        );
        assert!(movie_children.contains_key("Movie [tmdbid-5]"));
        assert!(movie_children.contains_key("Movie [tmdbid-5] (1)"));
    }

    #[test]
    fn test_vfs_conflicts() {
        let torrents = vec![
            (
                TorrentInfo {
                    id: "1".to_string(),
                    filename: "Movie.2023.mkv".to_string(),
                    hash: "h1".to_string(),
                    bytes: 1000,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    }],
                    links: vec!["http://l1".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "Same Title".to_string(),
                    year: Some("2023".to_string()),
                    media_type: MediaType::Movie,
                    external_id: Some("tmdb:1".to_string()),
                },
            ),
            (
                TorrentInfo {
                    id: "2".to_string(),
                    filename: "Movie.2024.mkv".to_string(),
                    hash: "h2".to_string(),
                    bytes: 1000,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    }],
                    links: vec!["http://l2".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "Same Title".to_string(),
                    year: Some("2024".to_string()),
                    media_type: MediaType::Movie,
                    external_id: Some("tmdb:2".to_string()),
                },
            ),
        ];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let movie_children = dir(&vfs.root, "Movies");
        assert!(movie_children.contains_key("Same Title [tmdbid-1]"));
        assert!(movie_children.contains_key("Same Title [tmdbid-2]"));
    }

    #[test]
    fn test_show_case_insensitive_grouping() {
        // Bug: "ted" and "Ted" (different casing, no TMDB ID) created separate
        // show folders instead of being merged into one.
        let torrents = vec![
            (
                TorrentInfo {
                    id: "s1".to_string(),
                    filename: "Ted.S01E01.Just.Say.Yes.1080p.mkv".to_string(),
                    hash: "h_s1".to_string(),
                    bytes: 2000,
                    status: "downloaded".to_string(),
                    added: "2024-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Ted.S01E01.Just.Say.Yes.1080p.mkv".to_string(),
                        bytes: 2000,
                        selected: 1,
                    }],
                    links: vec!["http://link_s1".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "Ted".to_string(),
                    year: Some("2024".to_string()),
                    media_type: MediaType::Show,
                    external_id: None,
                },
            ),
            (
                TorrentInfo {
                    id: "s2".to_string(),
                    filename: "ted.S02E01.Talk.Dirty.to.Me.1080p.mkv".to_string(),
                    hash: "h_s2".to_string(),
                    bytes: 2500,
                    status: "downloaded".to_string(),
                    added: "2024-06-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/ted.S02E01.Talk.Dirty.to.Me.1080p.mkv".to_string(),
                        bytes: 2500,
                        selected: 1,
                    }],
                    links: vec!["http://link_s2".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "ted".to_string(),
                    year: Some("2024".to_string()),
                    media_type: MediaType::Show,
                    external_id: None,
                },
            ),
        ];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        // Exactly ONE show folder, not two, and it holds both seasons.
        let seasons = only_show(&vfs);
        assert!(
            seasons.contains_key("Season 01"),
            "Season 01 missing: {:?}",
            seasons.keys().collect::<Vec<_>>()
        );
        assert!(
            seasons.contains_key("Season 02"),
            "Season 02 missing: {:?}",
            seasons.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn build_places_dash_coded_episode_in_correct_season_folder() {
        // A dash-concatenated absolute-code file ("Show - 409 - Title") must land in Season 04, not
        // Season 01 — the season folder is derived via parse_se (which decodes the dash code), with
        // SEASON_RE (which cannot) as a fallback only.
        let torrents = vec![(
            TorrentInfo {
                id: "s1".to_string(),
                filename: "Some Show - 409 - Episode Title.mkv".to_string(),
                hash: "h1".to_string(),
                bytes: 3000,
                status: "downloaded".to_string(),
                added: "2024-01-01".to_string(),
                files: vec![TorrentFile {
                    id: 1,
                    path: "/Some Show - 409 - Episode Title.mkv".to_string(),
                    bytes: 3000,
                    selected: 1,
                }],
                links: vec!["http://link1".to_string()],
                ..Default::default()
            },
            MediaMetadata {
                title: "Some Show".to_string(),
                year: Some("2014".to_string()),
                media_type: MediaType::Show,
                external_id: Some("tmdb:900011".to_string()),
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let seasons = only_show(&vfs);
        assert!(
            seasons.contains_key("Season 04"),
            "expected Season 04, got {:?}",
            seasons.keys().collect::<Vec<_>>()
        );
        assert!(
            !seasons.contains_key("Season 01"),
            "dash-coded episode must NOT fall back to Season 01"
        );
    }

    #[test]
    fn build_uses_parent_directory_season_when_filename_lacks_one() {
        // A season pack that encodes the season in the DIRECTORY, not the basename
        // ("Breaking Bad Season 2/02 - Cats in the Bag.mkv"), must land in Season 02 rather than
        // silently defaulting to Season 01 — the basename alone yields no season/episode code.
        let torrents = vec![(
            TorrentInfo {
                id: "s1".to_string(),
                filename: "02 - Cats in the Bag.mkv".to_string(),
                hash: "h1".to_string(),
                bytes: 3000,
                status: "downloaded".to_string(),
                added: "2024-01-01".to_string(),
                files: vec![TorrentFile {
                    id: 1,
                    path: "/Breaking Bad Season 2/02 - Cats in the Bag.mkv".to_string(),
                    bytes: 3000,
                    selected: 1,
                }],
                links: vec!["http://link1".to_string()],
                ..Default::default()
            },
            MediaMetadata {
                title: "Breaking Bad".to_string(),
                year: Some("2008".to_string()),
                media_type: MediaType::Show,
                external_id: Some("tmdb:1396".to_string()),
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let seasons = only_show(&vfs);
        assert!(
            seasons.contains_key("Season 02"),
            "expected Season 02 from the directory, got {:?}",
            seasons.keys().collect::<Vec<_>>()
        );
        assert!(
            !seasons.contains_key("Season 01"),
            "directory-coded season must NOT fall back to Season 01"
        );
    }

    /// A multi-season pack whose ROOT folder names a season range (`Show.S01-S03…`) and whose
    /// per-season subfolders hold code-less file names: the NEAREST directory that names a season
    /// decides, not the first season token in the whole path (the root's `S01`), so each season's
    /// files land in their own folder and a basename repeated across seasons (`Episode 01.mkv`) is
    /// not dropped as a duplicate. A season named only by the root still applies.
    #[test]
    fn build_uses_the_nearest_directory_season_in_a_multi_season_pack() {
        let paths = [
            "Show.S01-S03.1080p/Season 1/01 - Pilot.mkv",
            "Show.S01-S03.1080p/Season 2/01 - Return.mkv",
            "Show.S01-S03.1080p/Season 1/Episode 01.mkv",
            "Show.S01-S03.1080p/Season.2/Episode 01.mkv",
            "[Grp] Show (Season 1-3) [1080p]/Season 3/Episode 01.mkv",
            "Show.S04.1080p/01 - Title.mkv",
        ];
        let torrents = vec![(
            TorrentInfo {
                id: "pack".to_string(),
                filename: "Show.S01-S03.1080p".to_string(),
                hash: "h1".to_string(),
                status: "downloaded".to_string(),
                added: "2024-01-01".to_string(),
                files: paths
                    .iter()
                    .enumerate()
                    .map(|(i, p)| TorrentFile {
                        id: i as u32 + 1,
                        path: p.to_string(),
                        bytes: 3000,
                        selected: 1,
                    })
                    .collect(),
                // TorBox: no per-file links, every file is addressed by (torrent_id, file_id).
                links: vec![],
                ..Default::default()
            },
            MediaMetadata {
                title: "Show".to_string(),
                year: None,
                media_type: MediaType::Show,
                external_id: Some("tmdb:1".to_string()),
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let seasons = only_show(&vfs);
        let files_in = |season: &str| -> Vec<String> {
            match seasons.get(season) {
                Some(VfsNode::Directory { children }) => children.keys().cloned().collect(),
                _ => Vec::new(),
            }
        };
        assert_eq!(files_in("Season 01"), ["01 - Pilot.mkv", "Episode 01.mkv"]);
        assert_eq!(files_in("Season 02"), ["01 - Return.mkv", "Episode 01.mkv"]);
        assert_eq!(files_in("Season 03"), ["Episode 01.mkv"]);
        assert_eq!(files_in("Season 04"), ["01 - Title.mkv"]);
    }

    /// `Part N` is the weakest season signal for a code-less file: a `Part N` in the BASENAME (a
    /// two-part episode title) applies only when no directory names a season, and `part` must be a
    /// whole word with a 1-2 digit number, so a title word (`Counterpart`) or a year is never read
    /// as a season. A `Part N` DIRECTORY still names a season (a show released in parts) and, as
    /// the nearest season-naming directory, wins over a root folder's season token: deliberately so
    /// for `Show.S04/Part 2/` too, since a root often names a season RANGE (`Show.S01-S05/Part 3/`)
    /// whose first season would be wrong.
    #[test]
    fn build_treats_part_n_as_the_weakest_season_signal() {
        let season_of = |path: &str| -> Vec<String> {
            let torrents = vec![(
                TorrentInfo {
                    id: "pack".to_string(),
                    filename: "pack".to_string(),
                    hash: "h1".to_string(),
                    status: "downloaded".to_string(),
                    added: "2024-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: path.to_string(),
                        bytes: 3000,
                        selected: 1,
                    }],
                    links: vec![],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "Show".to_string(),
                    year: None,
                    media_type: MediaType::Show,
                    external_id: Some("tmdb:1".to_string()),
                },
            )];
            let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
            only_show(&vfs)
                .keys()
                .filter(|k| k.starts_with("Season "))
                .cloned()
                .collect()
        };
        // A two-part episode title in a per-season subfolder: the directory's season wins.
        assert_eq!(
            season_of("Show.S01-S03.1080p/Season 3/05 - Title, Part 2.mkv"),
            ["Season 03"]
        );
        // `part` inside a title word followed by a year is not a season.
        assert_eq!(
            season_of("Counterpart.2017.S01.1080p/01 - Pilot.mkv"),
            ["Season 01"]
        );
        // A `Part N` directory still names the season.
        assert_eq!(season_of("Show/Part 3/01.mkv"), ["Season 03"]);
        // With no directory season, the basename's `Part N` still applies.
        assert_eq!(season_of("Show/01 - Title, Part 2.mkv"), ["Season 02"]);
        // Deliberate: the nearest season-naming directory (`Part 2`) wins over the root's `S04`.
        assert_eq!(season_of("Show.S04/Part 2/01.mkv"), ["Season 02"]);
        // A strong basename code still beats a `Part N` directory.
        assert_eq!(season_of("Show/Part 3/1x05 - Title.mkv"), ["Season 01"]);
        // Within one name, a strong token beats an earlier `Part N`.
        assert_eq!(season_of("Show/Part 2 - 3x05.mkv"), ["Season 03"]);
        // `_` separates every token, like `.`, ` ` and `-`.
        assert_eq!(season_of("Show_Part_2/01.mkv"), ["Season 02"]);
        assert_eq!(season_of("Show/Part_2_Title.mkv"), ["Season 02"]);
        assert_eq!(season_of("Show_S02_1080p/01.mkv"), ["Season 02"]);
        assert_eq!(season_of("Show_3x05_Title/01.mkv"), ["Season 03"]);
    }

    #[test]
    fn test_show_same_tmdb_id_different_title_case() {
        // When two torrents have the same TMDB external_id but different title
        // casing (e.g. TMDB returned "Ted" one time and "ted" another), they
        // should be grouped into a single folder.
        let torrents = vec![
            (
                TorrentInfo {
                    id: "t1".to_string(),
                    filename: "Ted.S01E01.mkv".to_string(),
                    hash: "ht1".to_string(),
                    bytes: 2000,
                    status: "downloaded".to_string(),
                    added: "2024-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Ted.S01E01.mkv".to_string(),
                        bytes: 2000,
                        selected: 1,
                    }],
                    links: vec!["http://link_t1".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "Ted".to_string(),
                    year: Some("2024".to_string()),
                    media_type: MediaType::Show,
                    external_id: Some("tmdb:253472".to_string()),
                },
            ),
            (
                TorrentInfo {
                    id: "t2".to_string(),
                    filename: "ted.S02E01.mkv".to_string(),
                    hash: "ht2".to_string(),
                    bytes: 2500,
                    status: "downloaded".to_string(),
                    added: "2024-06-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/ted.S02E01.mkv".to_string(),
                        bytes: 2500,
                        selected: 1,
                    }],
                    links: vec!["http://link_t2".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "ted".to_string(),
                    year: Some("2024".to_string()),
                    media_type: MediaType::Show,
                    external_id: Some("tmdb:253472".to_string()),
                },
            ),
        ];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let show_children = dir(&vfs.root, "Shows");
        assert_eq!(
            show_children.len(),
            1,
            "Same TMDB ID should produce 1 folder, got {}: {:?}",
            show_children.len(),
            show_children.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn same_tmdb_id_group_names_its_folder_independently_of_input_order() {
        // Two torrents share a TMDB id but carry different titles (e.g. a cached identification and
        // an authoritative acquire title). The folder name and NFO title must not depend on which
        // one the provider listing happens to put first, or the folder is renamed between ticks.
        let torrent = |id: &str, path: &str| TorrentInfo {
            id: id.to_string(),
            hash: format!("h{id}"),
            bytes: 1000,
            status: "downloaded".to_string(),
            added: "2024-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: path.to_string(),
                bytes: 1000,
                selected: 1,
            }],
            ..Default::default()
        };
        let meta = |title: &str| MediaMetadata {
            title: title.to_string(),
            year: Some("2024".to_string()),
            media_type: MediaType::Show,
            external_id: Some("tmdb:4242".to_string()),
        };
        let a = (torrent("1", "Foo.S01E01.mkv"), meta("Foo"));
        let b = (torrent("2", "Foo.Bar.S01E02.mkv"), meta("Foo Bar"));

        let shows_of = |vfs: &DebridVfs| dir(&vfs.root, "Shows").clone();
        let forward = shows_of(&DebridVfs::build(
            vec![a.clone(), b.clone()],
            &SelectionMap::new(),
        ));
        let reverse = shows_of(&DebridVfs::build(vec![b, a], &SelectionMap::new()));
        assert_eq!(
            forward.len(),
            1,
            "one folder per TMDB id: {:?}",
            forward.keys()
        );
        assert_eq!(
            forward, reverse,
            "folder name, episodes and tvshow.nfo must be identical in either input order"
        );
    }

    #[test]
    fn test_nfo_escapes_special_xml_characters() {
        let metadata = MediaMetadata {
            title: "Test & <Movie>".to_string(),
            year: Some("2024".to_string()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:123".to_string()),
        };
        let content = String::from_utf8(DebridVfs::generate_nfo(&metadata)).unwrap();
        assert!(
            content.contains("<title>Test &amp; &lt;Movie&gt;</title>"),
            "Title should be XML-escaped"
        );
        assert!(
            content.contains("<originaltitle>Test &amp; &lt;Movie&gt;</originaltitle>"),
            "Original title should be XML-escaped"
        );
        // Year and IDs should also be escaped (even if unlikely to contain special chars)
        assert!(
            !content.contains("&<"),
            "No unescaped special characters should appear"
        );
        // Apostrophe and double-quote must ALSO be escaped (the prior assertions only covered
        // &/</>). A direct check pins all five metacharacters, and an NFO round-trip proves a
        // title like "It's a \"Test\"" is emitted safely (these reach `<uniqueid type="…">` too).
        assert_eq!(
            xml_escape("O'Brien \"Q\" & <b>"),
            "O&apos;Brien &quot;Q&quot; &amp; &lt;b&gt;"
        );
        let quoted = MediaMetadata {
            title: "It's a \"Test\"".to_string(),
            year: Some("2024".to_string()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:123".to_string()),
        };
        let quoted_nfo = String::from_utf8(DebridVfs::generate_nfo(&quoted)).unwrap();
        assert!(
            quoted_nfo.contains("<title>It&apos;s a &quot;Test&quot;</title>"),
            "apostrophe and double-quote in a title must be XML-escaped"
        );
    }

    #[test]
    fn test_nfo_strips_control_characters() {
        let metadata = MediaMetadata {
            title: "Movie\x00With\x07Control\x1FChars".to_string(),
            year: Some("2024".to_string()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:123".to_string()),
        };
        let content = String::from_utf8(DebridVfs::generate_nfo(&metadata)).unwrap();
        assert!(
            content.contains("<title>MovieWithControlChars</title>"),
            "Control characters should be stripped from NFO XML"
        );
        // Tab, newline, carriage return are valid XML and should be preserved
        assert_eq!(xml_escape("a\tb\nc\rd"), "a\tb\nc\rd");
        // U+FFFE / U+FFFF are the only remaining `char`-expressible code points that are invalid
        // under the XML 1.0 Char production — a strict media-server parser rejects an NFO
        // containing one, so they must be stripped like the C0 controls.
        assert_eq!(xml_escape("a\u{FFFE}b\u{FFFF}c"), "abc");
    }

    #[test]
    fn test_vfs_duplicates() {
        let metadata = MediaMetadata {
            title: "Duplicate Movie".to_string(),
            year: Some("2023".to_string()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:123".to_string()),
        };

        let torrents = vec![
            (
                TorrentInfo {
                    id: "small".to_string(),
                    filename: "Movie.small.mkv".to_string(),
                    hash: "h1".to_string(),
                    bytes: 1000,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    }],
                    links: vec!["http://small".to_string()],
                    ..Default::default()
                },
                metadata.clone(),
            ),
            (
                TorrentInfo {
                    id: "large".to_string(),
                    filename: "Movie.large.mkv".to_string(),
                    hash: "h2".to_string(),
                    bytes: 5000,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.mkv".to_string(),
                        bytes: 5000,
                        selected: 1,
                    }],
                    links: vec!["http://large".to_string()],
                    ..Default::default()
                },
                metadata,
            ),
        ];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let (_, locator) = media_file(&vfs.root, "Movies/Duplicate Movie [tmdbid-123]/Movie.mkv");
        assert_eq!(
            locator.torrent_id, "large",
            "Should have picked the large torrent"
        );
    }

    #[test]
    fn is_archive_file_detects_archives() {
        assert!(is_archive_file("movie.rar"));
        assert!(is_archive_file("movie.r00"));
        assert!(is_archive_file("movie.r99"));
        assert!(is_archive_file("movie.zip"));
        assert!(is_archive_file("movie.7z"));
        assert!(is_archive_file("/path/to/movie.RAR"));
        assert!(is_archive_file("movie.s00"));
        assert!(!is_archive_file("movie.mkv"));
        assert!(!is_archive_file("movie.nfo"));
        assert!(!is_archive_file("movie.srt"));
    }

    #[test]
    fn is_archive_part_handles_unicode_filenames() {
        // Filenames with multi-byte UTF-8 characters must not panic
        // when is_archive_part slices to find the last 4 characters.
        // Note: is_archive_part expects pre-lowercased input.
        assert!(!is_archive_part("ö.r0")); // 2-byte char followed by 3 ASCII chars
        assert!(is_archive_part("café.r00")); // multi-byte in prefix, valid archive ext
        assert!(!is_archive_part("über.mkv")); // multi-byte, not archive
        assert!(!is_archive_part("日本語.mp4")); // 3-byte chars, not archive
        assert!(is_archive_part("データ.s01")); // 3-byte chars with valid archive ext
    }

    // --- is_video_file word-boundary filtering ---

    #[test]
    fn is_video_file_accepts_normal_video() {
        assert!(is_video_file("Movie.2023.1080p.mkv"));
        assert!(is_video_file("/path/to/Movie.mp4"));
        assert!(is_video_file("Show.S01E01.avi"));
    }

    #[test]
    fn is_video_file_rejects_sample() {
        assert!(!is_video_file("sample.mkv"));
        assert!(!is_video_file("Movie-sample.mkv"));
        assert!(!is_video_file("/path/Sample.mp4"));
    }

    #[test]
    fn is_video_file_rejects_trailer() {
        assert!(!is_video_file("trailer.mkv"));
        assert!(!is_video_file("Movie-trailer.mkv"));
    }

    #[test]
    fn is_video_file_rejects_extras() {
        assert!(!is_video_file("extras.mkv"));
        assert!(!is_video_file("extra.mkv"));
        assert!(!is_video_file("Movie.Extra.Feature.mkv"));
        // Directory-level filtering is also handled (see is_video_file_rejects_extras_directories).
    }

    #[test]
    fn is_video_file_rejects_bonus() {
        assert!(!is_video_file("bonus.mkv"));
        assert!(!is_video_file("Movie-Bonus.mkv"));
    }

    #[test]
    fn is_video_file_rejects_featurette() {
        assert!(!is_video_file("featurette.mkv"));
        assert!(!is_video_file("Making.Of.Featurette.mkv"));
    }

    #[test]
    fn is_video_file_no_false_positive_on_extra_substring() {
        // "extraordinary" contains "extra" as a substring but should NOT be filtered
        assert!(is_video_file("extraordinary.mkv"));
        assert!(is_video_file("Extraordinary.Attorney.Woo.S01E01.mkv"));
    }

    #[test]
    fn is_video_file_no_false_positive_on_bonus_substring() {
        // "Bonus" inside a longer word is not the keyword (for "The.Bonus.Army.2024", where it is a
        // whole title word before the year, see
        // `is_video_file_keeps_a_title_word_keyword_before_the_release_year`).
        assert!(is_video_file("Bonusville.2024.mkv"));
    }

    #[test]
    fn is_video_file_no_false_positive_on_sample_substring() {
        // "Sampler" contains "sample" as a substring but should NOT be filtered
        assert!(is_video_file("The.Sampler.2024.mkv"));
    }

    #[test]
    fn is_video_file_no_false_positive_on_trailer_substring() {
        // "Trailerman" contains "trailer" as a substring but should NOT be filtered
        assert!(is_video_file("Trailerman.2024.mkv"));
    }

    #[test]
    fn is_video_file_keeps_title_whose_first_word_is_an_excluded_keyword() {
        // The keyword leads the real title — a show ("Extras", "Trailer Park Boys") or a movie
        // ("Extras 2020") — so the file must be KEPT, not dropped as an ancillary clip.
        assert!(is_video_file("Extras.S01E01.720p.mkv"));
        assert!(is_video_file("Trailer.Park.Boys.S05E01.1080p.mkv"));
        assert!(is_video_file("Extras.2020.1080p.BluRay.mkv"));
        // A real title with the keyword mid-name BEFORE the episode code is kept.
        assert!(is_video_file("Some.Trailer.Show.S02E03.mkv"));
        // A real episode mis-foldered under an extras/bonus directory is still kept.
        assert!(is_video_file("Extras/Extras.S01E01.mkv"));
        // But genuine ancillary clips are STILL excluded (no leading keyword, no episode code).
        assert!(!is_video_file("Movie.2020.1080p.sample.mkv"));
        assert!(!is_video_file("The.Movie.Name.Trailer.mkv"));
        assert!(!is_video_file("Movie.2020/Extras/featurette.mkv"));
        assert!(!is_video_file("sample.mkv"));
    }

    #[test]
    fn is_video_file_excludes_metadata_suffixed_trailer_and_extras_clips() {
        // A leading keyword immediately followed by a RESOLUTION/SOURCE tag is an ancillary clip, not
        // a real title — the exemption must NOT apply (previously these leaked into the movie folder
        // as bogus playable "movies", inconsistent with `Trailer.mkv` being correctly excluded).
        assert!(!is_video_file("Trailer.1080p.WEB-DL.mkv"));
        assert!(!is_video_file("Extras.1080p.mkv"));
        assert!(!is_video_file("Extras.720p.WEB.x264.mkv"));
        assert!(!is_video_file("Trailer.2160p.HDR.HEVC.mkv"));
        assert!(!is_video_file("Extra.480p.mkv"));
        // The genuine-title cases (a real word or a bare year after the keyword) are still KEPT —
        // a year is not a resolution, so the exemption survives.
        assert!(is_video_file("Extras.2020.1080p.BluRay.mkv"));
        assert!(is_video_file("Trailer.Park.Boys.2005.1080p.WEB.mkv"));
    }

    #[test]
    fn is_video_file_excludes_episode_coded_ancillary_clips() {
        // An episode code must NOT rescue a sample/trailer whose keyword is a TRAILING descriptor
        // (after the episode marker) — scene/P2P sample files carry the full release name + code.
        assert!(!is_video_file(
            "The.Office.S01E01.1080p.BluRay.x264-GRP.sample.mkv"
        ));
        assert!(!is_video_file("show.s01e01.1080p-grp.sample.mkv"));
        assert!(!is_video_file("Sample/show.s01e01.1080p-grp.sample.mkv"));
        // `sample-<release>.mkv` is a real ancillary form — `sample` is NOT a leading-title keyword.
        assert!(!is_video_file("sample-the.matrix.1999.1080p.mkv"));
        // ... including for an episode: a `sample` keyword BEFORE the episode code is still the
        // sample marker, never a title word. (Kept as a video, such a clip showed next to the real
        // S01E01 and could take its selection slot.)
        assert!(!is_video_file("sample-show.s01e01.1080p-grp.mkv"));
        // A file in a Sample/ (or Samples/) folder is a sample even when it carries an episode code
        // and no keyword in its own name: no real episode ever lives in a sample folder.
        assert!(!is_video_file(
            "Show.S01E01.1080p-GRP/Sample/show.s01e01.1080p-grp.mkv"
        ));
        assert!(!is_video_file("Show.S01.1080p-GRP/Samples/show.s01e02.mkv"));
        assert!(!is_video_file("Movie.2010/Sample/movie.2010.1080p.mkv"));
        // A mis-foldered real episode under an extras/bonus directory is still kept.
        assert!(is_video_file("Extras/Extras.S01E01.mkv"));
        assert!(is_video_file("Show/Bonus/Show.S01E01.mkv"));
    }

    #[test]
    fn is_video_file_keeps_an_episode_title_word_after_the_episode_code() {
        // A keyword in an episode title after the code is part of that title. Excluding it hid the
        // episode, kept Real-Debrid from selecting it, and left it out of a pack's `provides`.
        assert!(is_video_file(
            "Show.S03E07.The.Extra.1080p.WEB.h264-GRP.mkv"
        ));
        assert!(is_video_file("Show - S02E05 - Bonus Round.mkv"));
        assert!(is_video_file(
            "Show (2010) - S01E10 - Trailer Trash (1080p BluRay x265).mkv"
        ));
        assert!(is_video_file("Show.S04E02.Extra.Credit.mkv"));
        assert!(is_video_file("Show - 2x05 - Bonus Round.mkv"));
        assert!(is_video_file("Show.S01E05.Featurette.Making.Of.mkv"));
        // The keyword alone after the code (the end of the name, a release tag or a group follows
        // it), a Plex-style suffix, and a keyword in the tag part are still descriptors.
        assert!(!is_video_file("Show.S01E05.Featurette.mkv"));
        assert!(!is_video_file("Show.S01E05-trailer.mkv"));
        assert!(!is_video_file("Show.S01E05.Extras.1080p.mkv"));
        assert!(!is_video_file("Show.S01E05.Featurette-GRP.mkv"));
        assert!(!is_video_file("Show - S01E05 - Trailer (1080p).mkv"));
        assert!(!is_video_file("Show.S01E01E02.Extras.720p.mkv"));
        assert!(!is_video_file("Show.S01E01-02.Bonus.mkv"));
        assert!(!is_video_file(
            "Show.S01E01.1080p.WEB.x264-GRP.featurette.mkv"
        ));
        assert!(!is_video_file("Show.S01E01.1080p.Extra.Credit.mkv"));
        // `sample` stays a descriptor wherever it sits.
        assert!(!is_video_file("Show.S01E05.Sample.Round.mkv"));
    }

    #[test]
    fn is_video_file_keeps_a_title_word_keyword_before_the_release_year() {
        // A keyword mid-title, before the release year, is part of the title ("The Extra Man",
        // 2010), not a descriptor. Excluding it hid the film and made every acquisition candidate
        // look file-less, so each was reaped and blacklisted.
        assert!(is_video_file("The.Extra.Man.2010.1080p.BluRay.x264.mkv"));
        assert!(is_video_file("The.Bonus.Army.2024.mkv"));
        // A descriptor after the year, a trailing keyword with no year, a `sample` anywhere, and a
        // second, trailing descriptor on such a title are all still excluded.
        assert!(!is_video_file("The.Extra.Man.2010.1080p.sample.mkv"));
        assert!(!is_video_file("sample-the.extra.man.2010.1080p.mkv"));
        assert!(!is_video_file("Movie.Name.2010.Trailer.mkv"));
        assert!(!is_video_file("Movie.Name.Trailer.mkv"));
        assert!(!is_video_file("Movie.Name.Extras.mkv"));
        assert!(!is_video_file("Movie.2010.1080p-sample.mkv"));
        // A sample of a show whose title leads with a keyword is a sample, not an episode.
        assert!(!is_video_file("Extras.S01E01.sample.mkv"));
    }

    #[test]
    fn is_video_file_treats_an_underscore_as_a_separator() {
        // The regex crate's `\b` counts `_` as a word character, so these clips used to pass.
        assert!(!is_video_file("show_s01e01_720p_sample.mkv"));
        assert!(!is_video_file("sample_show_s01e01.mkv"));
        assert!(!is_video_file("movie_2010_trailer.mkv"));
        assert!(!is_video_file("Trailer_1080p_WEB.mkv"));
        assert!(!is_video_file("Movie_Name_Extras.mkv"));
        // Titles holding a keyword are still kept.
        assert!(is_video_file("The_Extra_Man_2010_1080p.mkv"));
        assert!(is_video_file("Trailer_Park_Boys_S01E01.mkv"));
        assert!(is_video_file("Extras_2020_1080p.mkv"));
    }

    #[test]
    fn is_video_file_counts_nxm_and_separated_episode_codes() {
        // Every episode-code form `parse_se_all` reads makes a keyword before it a title word, and
        // keeps a real episode mis-foldered under an extras folder.
        assert!(is_video_file("Some.Trailer.Show.2x03.mkv"));
        assert!(is_video_file("Some.Trailer.Show.S02.E03.mkv"));
        assert!(is_video_file("Show/Extras/Show - 1x01 - Pilot.mkv"));
        assert!(is_video_file("Show/Extras/Show S01 E01.mkv"));
        assert!(is_video_file("Show/Extras/Show.Name.2005.1x01.Pilot.mkv"));
        // A sample is still a sample, and a channel layout run into a codec is no episode code.
        assert!(!is_video_file("Show - 1x01 - Pilot.sample.mkv"));
        assert!(!is_video_file("Movie/Extras/Movie.2010.DD5.1x264.mkv"));
        assert!(!is_video_file("Movie/Extras/Making.Of.1920x1080.mkv"));
    }

    #[test]
    fn is_video_file_counts_the_dash_concatenated_episode_code() {
        // The `Show - 105 - Title` code `parse_se_all` reads makes an episode-title keyword after
        // it a title word, a keyword before it a title word, and keeps a mis-foldered episode.
        assert!(is_video_file("Show - 105 - The Extra Mile.mkv"));
        assert!(is_video_file("Show - 304 - Bonus Round.mkv"));
        assert!(is_video_file("Show/Extras/Show - 105 - Pilot.mkv"));
        assert!(is_video_file("Some.Trailer.Show - 203 - Title.mkv"));
        // The keyword alone after the code is still a clip, and a sample is still a sample.
        assert!(!is_video_file("Show - 105 - Trailer.mkv"));
        assert!(!is_video_file("Show - 105 - Title.sample.mkv"));
        // A dash code `parse_se_all` rejects (episode 00) rescues nothing.
        assert!(!is_video_file("Show/Extras/Show - 100 - Pilot.mkv"));
    }

    #[test]
    fn is_video_file_rejects_non_video_extensions() {
        assert!(!is_video_file("movie.srt"));
        assert!(!is_video_file("movie.nfo"));
        assert!(!is_video_file("movie.txt"));
        assert!(!is_video_file("movie.mkv.part"));
        // `.m2ts` is stripped as a whole (`.ts` does not match `x.m2ts`), so the promo check sees
        // the bare stem.
        assert!(!is_video_file("RARBG.m2ts"));
        assert!(is_video_file("Movie.2010.m2ts"));
    }

    #[test]
    fn warn_once_is_true_only_the_first_time_a_kind_and_torrent_are_seen() {
        assert!(warn_once("test-kind", "warn-once-id-x"));
        assert!(!warn_once("test-kind", "warn-once-id-x"));
        assert!(warn_once("test-kind", "warn-once-id-y"), "another torrent");
        assert!(
            warn_once("test-other-kind", "warn-once-id-x"),
            "another kind"
        );
    }

    #[test]
    fn parse_se_all_reads_every_episode_code_and_parse_se_the_first() {
        assert_eq!(parse_se_all("Show.S04E09.1080p.mkv"), vec![(4, 9)]);
        assert_eq!(parse_se_all("Show.S01E01E02.mkv"), vec![(1, 1), (1, 2)]);
        assert_eq!(parse_se_all("Show.S01E01-E02.mkv"), vec![(1, 1), (1, 2)]);
        assert_eq!(parse_se_all("Show.S01E01.1080p.x265.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("Show - 409 - Title.mkv"), vec![(4, 9)]);
        assert!(parse_se_all("Show - 2024 - Title.mkv").is_empty());
        assert!(parse_se_all("Show - 400 - Title.mkv").is_empty());
        assert!(parse_se_all("Movie.2009.1080p.BluRay.mkv").is_empty());
        assert_eq!(parse_se("Show.S01E01E02.mkv"), Some((1, 1)));
        assert_eq!(parse_se("Show - 409 - Title.mkv"), Some((4, 9)));
        assert_eq!(parse_se("Movie.2009.mkv"), None);
    }

    #[test]
    fn parse_se_all_handles_sxxexx_and_dash_concatenated_codes() {
        // A single-digit season/episode.
        assert_eq!(parse_se_all("Show S1E5.mkv"), vec![(1, 5)]);
        // Dash-concatenated 3-digit form ("409" = S04E09).
        assert_eq!(
            parse_se_all("Some Show - 409 - Episode Title.mkv"),
            vec![(4, 9)]
        );
        // Must NOT misread a resolution or year as an episode code.
        assert!(parse_se_all("Movie (1999).mkv").is_empty());
        // A date-named daily-show file must NOT decode (no 3-digit dash token).
        assert!(parse_se_all("The Daily Show - 2021 - 03 - 14.mkv").is_empty());
        // No episode marker at all.
        assert!(parse_se_all("Just A Movie.mkv").is_empty());
    }

    #[test]
    fn parse_se_all_handles_multi_episode_files() {
        // Contiguous multi-episode files contribute ALL their episodes (so `provides` is complete and
        // the reconciler doesn't re-acquire a contained episode as a duplicate singleton).
        assert_eq!(
            parse_se_all("Show.S01E01E02.1080p.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(
            parse_se_all("Show.S01E01E02E03.mkv"),
            vec![(1, 1), (1, 2), (1, 3)]
        );
        assert_eq!(
            parse_se_all("Show S02E03E04E05.mkv"),
            vec![(2, 3), (2, 4), (2, 5)]
        );
        // A resolution/codec token after the episode must NOT be parsed as a further episode (the
        // continuation requires an explicit `e` before each number).
        assert_eq!(parse_se_all("Show.S01E07.720p.WEB.mkv"), vec![(1, 7)]);
        // Sonarr's default "Extend" style: a dash-only continuation naming the NEXT episode.
        assert_eq!(parse_se_all("Show.S01E01-02.mkv"), vec![(1, 1), (1, 2)]);
        assert_eq!(
            parse_se_all("Show.S01E01-02-03.mkv"),
            vec![(1, 1), (1, 2), (1, 3)]
        );
        assert_eq!(
            parse_se_all("Show.S01E01-E02-03.1080p.mkv"),
            vec![(1, 1), (1, 2), (1, 3)]
        );
        // ...but a resolution, bit depth or audio token after a dash is not an episode.
        assert_eq!(parse_se_all("Show.S01E05-720p.mkv"), vec![(1, 5)]);
        assert_eq!(parse_se_all("Show.S01E09-10bit.mkv"), vec![(1, 9)]);
        assert_eq!(parse_se_all("Show.S01E05-5.1.mkv"), vec![(1, 5)]);
        // A channel layout whose whole part happens to be the next episode is still audio.
        assert_eq!(parse_se_all("Show.S01E04-5.1.mkv"), vec![(1, 4)]);
        assert_eq!(parse_se_all("Show.S01E01-2.0.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("Show.S01E06-7.1.mkv"), vec![(1, 6)]);
        assert_eq!(parse_se_all("Show.S01E06-7.1.2.Atmos.mkv"), vec![(1, 6)]);
        // ...while a real double followed by a resolution still counts.
        assert_eq!(
            parse_se_all("Show.S01E01-02.1080p.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(
            parse_se_all("Show.S01E01-2.1080p.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(parse_se_all("Show.S01E04-5.mkv"), vec![(1, 4), (1, 5)]);
    }

    #[test]
    fn parse_se_all_documented_limitations_are_stable() {
        // Pin the CURRENT (documented) behaviour so a silent regression is caught.

        // KNOWN LIMITATION: a >2-episode RANGE records only the explicit endpoints, dropping the
        // middle — "S01E01-E03" → [(1,1),(1,3)], NOT (1,2).
        assert_eq!(
            parse_se_all("Show.S01E01-E03.1080p.mkv"),
            vec![(1, 1), (1, 3)]
        );
        // KNOWN LIMITATION: a dash-only continuation counts only when it names the NEXT episode
        // (Sonarr's "Extend" style), so a dash-number RANGE keeps only its first episode.
        assert_eq!(parse_se_all("Show.S01E01-03.1080p.mkv"), vec![(1, 1)]);

        // KNOWN LIMITATION: the dash-concatenated code fallback requires SPACE-dash-SPACE, so a
        // DOT-separated Torrentio name is not matched and yields empty `provides`.
        assert!(parse_se_all("Some.Show.-.409.-.Episode.Title.mkv").is_empty());

        // KNOWN LIMITATION: anime ABSOLUTE numbering is indistinguishable from a concatenated
        // SxxEyy code, so absolute "409" decodes to a phantom (4, 9) rather than the real ep 409.
        assert_eq!(parse_se_all("One Piece - 409 - Return.mkv"), vec![(4, 9)]);
    }

    #[test]
    fn parse_se_all_reads_nxm_and_separated_sxxeyy_codes() {
        // The `NxM` form, bounded by a separator (`_` included) or the string edge on both sides.
        assert_eq!(parse_se_all("Show - 2x05 - Title.mkv"), vec![(2, 5)]);
        assert_eq!(parse_se_all("Show.1x01.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("Show_1x01_Title.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("1x01 - Pilot.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("Show.10x112.mkv"), vec![(10, 112)]);
        assert_eq!(parse_se_all("Show.2x05.DD5.1.x264.mkv"), vec![(2, 5)]);
        // A separator between the season digits and the `E`.
        assert_eq!(parse_se_all("Show.S01.E05.mkv"), vec![(1, 5)]);
        assert_eq!(parse_se_all("Show S01 E05.mkv"), vec![(1, 5)]);
        assert_eq!(parse_se_all("Show_S01_E05_720p.mkv"), vec![(1, 5)]);
        // Their multi-episode forms.
        assert_eq!(
            parse_se_all("Show - 1x01x02 - Title.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(
            parse_se_all("Show - 1x01-1x02 - Title.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(
            parse_se_all("Show - 1x01-02 - Title.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(parse_se_all("Show.S01.E01E02.mkv"), vec![(1, 1), (1, 2)]);
        assert_eq!(parse_se_all("Show.S01.E01-02.mkv"), vec![(1, 1), (1, 2)]);
        // A code from another season ends the run.
        assert_eq!(parse_se_all("Show.1x10.2x01.mkv"), vec![(1, 10)]);
        // Not episode codes: a pixel size, an aspect ratio, a codec, a channel layout run into a
        // codec, an episode 00, and a version or resolution glued to the number.
        assert!(parse_se_all("Movie.2010.1920x1080.mkv").is_empty());
        assert!(parse_se_all("Movie.2010.[1280x720].mkv").is_empty());
        assert!(parse_se_all("Movie.2010.16x9.mkv").is_empty());
        assert!(parse_se_all("Movie.2010.x264.mkv").is_empty());
        assert!(parse_se_all("Movie.2010.DD5.1x264.mkv").is_empty());
        assert!(parse_se_all("Show.1x00.mkv").is_empty());
        // An episode 00 at the very start of the name is skipped too, and the search goes on.
        assert!(parse_se_all("1x00.mkv").is_empty());
        assert_eq!(parse_se_all("1x00.2x05.mkv"), vec![(2, 5)]);
        assert!(parse_se_all("Show.2x720p.mkv").is_empty());
        // The contiguous form keeps winning over a later channel layout.
        assert_eq!(parse_se_all("Show.S01E01.DD5.1x264.mkv"), vec![(1, 1)]);
        // Only a run into an `x264`/`x265` codec is a channel layout: a real code after a year or
        // a title ending in a digit is still read.
        assert!(parse_se_all("Movie.2010.AAC2.0x265.mkv").is_empty());
        assert_eq!(parse_se_all("Show.Name.2005.1x01.Pilot.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("Babylon.5.2x10.mkv"), vec![(2, 10)]);
        assert_eq!(parse_se_all("24.3x05.mkv"), vec![(3, 5)]);
        assert_eq!(parse_se_all("Stargate.SG-1.1x05.mkv"), vec![(1, 5)]);
        // A version suffix is read as the `SxxEyy` arm reads it (`S01E01v2`).
        assert_eq!(parse_se_all("Show - 1x01v2 - Title.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("Show.1x01v2.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("Show.S01E01v2.mkv"), vec![(1, 1)]);
    }

    #[test]
    fn parse_se_all_reads_a_separated_same_season_sxxeyy_code() {
        // Sonarr's "Duplicate" multi-episode style repeats the whole code after the separator, as
        // the `NxM` arm's `1x01-1x02` does.
        assert_eq!(
            parse_se_all("Show.S01E01.S01E02.720p.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(
            parse_se_all("Show.S01E01-S01E02.720p.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(
            parse_se_all("Show - S01E01 - S01E02 - Title.mkv"),
            vec![(1, 1), (1, 2)]
        );
        assert_eq!(parse_se_all("Show S01E01 S01E02.mkv"), vec![(1, 1), (1, 2)]);
        // A range's explicit second code gives its endpoints, as `S01E01-E10` does.
        assert_eq!(
            parse_se_all("Show S01E01 - S01E10 1080p"),
            vec![(1, 1), (1, 10)]
        );
        // Another season's code, or an episode 00, ends the run.
        assert_eq!(parse_se_all("Show.S01E01.S02E05.mkv"), vec![(1, 1)]);
        assert_eq!(parse_se_all("Show.S01E01.S01E00.mkv"), vec![(1, 1)]);
        // A separated first code that opens a range still decodes to nothing.
        assert!(parse_se_all("Show.S01.E01-S01.E10.720p").is_empty());
    }

    #[test]
    fn parse_se_all_leaves_an_nxm_or_separated_range_undecoded() {
        // A range from an `NxM` or separated `S01.E01` code to a later, non-next episode decodes to
        // nothing, as before these forms were read: its first code alone would make a pack named
        // this way read as a single episode (`upgrade::is_single_episode_stream`, whose range test
        // knows only the contiguous `S01E01-10` form).
        assert!(parse_se_all("Show 1x01-10 720p").is_empty());
        assert!(parse_se_all("Show 1x01~10 720p").is_empty());
        assert!(parse_se_all("Show.S01.E01-10.720p").is_empty());
        assert!(parse_se_all("Show.S01.E01-S01.E10.720p").is_empty());
        assert!(parse_se_all("Show S01 E01~E10").is_empty());
        // An explicit second code is read like `S01E01-E10`: its endpoints.
        assert_eq!(parse_se_all("Show 1x01-1x10 720p"), vec![(1, 1), (1, 10)]);
        assert_eq!(parse_se_all("Show.S01.E01-E10.720p"), vec![(1, 1), (1, 10)]);
        // A resolution, bit depth or channel layout after a dash is no range.
        assert_eq!(parse_se_all("Show.2x05-720p.mkv"), vec![(2, 5)]);
        assert_eq!(parse_se_all("Show.2x09-10bit.mkv"), vec![(2, 9)]);
        assert_eq!(parse_se_all("Show.S01.E04-5.1.mkv"), vec![(1, 4)]);
        assert_eq!(parse_se_all("Show.1x04-5.1.mkv"), vec![(1, 4)]);
        // An episode title after a spaced dash is no range either.
        assert_eq!(parse_se_all("Show - 1x01 - 10 Things.mkv"), vec![(1, 1)]);
        // The contiguous form keeps its first code (`upgrade`'s range test covers it).
        assert_eq!(parse_se_all("Show.S01E01-10.720p"), vec![(1, 1)]);
    }

    #[test]
    fn parse_se_all_reads_a_dash_continuation_followed_by_an_underscore() {
        assert_eq!(
            parse_se_all("Show.S01E01-02_Title.mkv"),
            vec![(1, 1), (1, 2)]
        );
    }

    #[test]
    fn is_video_file_rejects_release_site_promos() {
        // Release-site/group advertisement clips that ship alongside the real media.
        assert!(!is_video_file("RARBG.com.mp4"));
        assert!(!is_video_file("RARBG.mp4"));
        assert!(!is_video_file("RARBGT.mp4"));
        assert!(!is_video_file("www.YTS.MX.mp4"));
        assert!(!is_video_file("YTS.MX.mp4"));
        assert!(!is_video_file("YIFY.mp4"));
        // Still excluded when nested inside the torrent's folder.
        assert!(!is_video_file(
            "Mission.Impossible.Rogue.Nation.2015/RARBG.com.mp4"
        ));
    }

    #[test]
    fn is_video_file_no_false_positive_on_promo_substring() {
        // A real title is only dropped when the *entire* stem is the promo token.
        // A movie whose name merely contains the token (or extra words) must survive.
        assert!(is_video_file("RARBG.com.Mission.Impossible.2015.mkv"));
        assert!(is_video_file("The.Yify.Story.2024.mkv"));
        assert!(is_video_file("Rarbgville.2024.mkv"));
    }

    #[test]
    fn sanitize_filename_replaces_slashes() {
        assert_eq!(sanitize_filename("Alex/October"), "Alex-October");
        assert_eq!(sanitize_filename("Back\\Slash"), "Back-Slash");
        assert_eq!(sanitize_filename("Title: Subtitle"), "Title - Subtitle");
        assert_eq!(sanitize_filename("Normal Title"), "Normal Title");
        assert_eq!(sanitize_filename("What? *Really*"), "What Really");
        assert_eq!(sanitize_filename("A<B>C|D\"E"), "ABCDE");
    }

    #[test]
    fn sanitize_filename_folds_dot_and_empty_names_to_placeholder() {
        // Names that would otherwise become "", "." or ".." — junk/path-navigation directory keys.
        assert_eq!(sanitize_filename(".."), "untitled");
        assert_eq!(sanitize_filename("."), "untitled");
        assert_eq!(sanitize_filename("***"), "untitled");
        assert_eq!(sanitize_filename("   "), "untitled");
    }

    #[test]
    fn sanitize_filename_strips_trailing_dots_and_guards_reserved_names() {
        // Windows silently strips a trailing dot, so the key must not end in one (would diverge from
        // the on-disk name over rclone/SMB).
        assert_eq!(sanitize_filename("Adam."), "Adam");
        assert_eq!(sanitize_filename("Title..."), "Title");
        // Bare reserved device names are suffixed so they're creatable on Windows...
        assert_eq!(sanitize_filename("CON"), "CON_");
        assert_eq!(sanitize_filename("nul"), "nul_");
        assert_eq!(sanitize_filename("Com1"), "Com1_");
        // ...as is a DOTTED name whose base (before the first dot) is a reserved device — Windows
        // resolves `Con.Air.1997` to the CON device, so the base itself must be suffixed
        // (appending at the very end wouldn't change the reserved pre-dot base).
        assert_eq!(sanitize_filename("Con.Air.1997"), "Con_.Air.1997");
        assert_eq!(sanitize_filename("Aux.Something"), "Aux_.Something");
        assert_eq!(sanitize_filename("nul.point"), "nul_.point");
        // ...but a real title that merely contains a reserved word is untouched.
        assert_eq!(sanitize_filename("Con Air"), "Con Air");
        assert_eq!(sanitize_filename("Conan"), "Conan");
        assert_eq!(sanitize_filename("Aux Battements"), "Aux Battements");
        // A dotted title whose base is NOT a device name is also untouched.
        assert_eq!(sanitize_filename("G.I. Joe"), "G.I. Joe");
    }

    #[test]
    fn build_sanitizes_title_with_slash() {
        let torrents = vec![(
            TorrentInfo {
                id: "1".to_string(),
                filename: "Alex.October.mkv".to_string(),
                hash: "h".to_string(),
                bytes: 1000,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![TorrentFile {
                    id: 1,
                    path: "/Alex.October.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["http://link1".to_string()],
                ..Default::default()
            },
            MediaMetadata {
                title: "Alex/October".to_string(),
                year: None,
                media_type: MediaType::Movie,
                external_id: Some("tmdb:856721".to_string()),
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
        let movie_children = dir(&vfs.root, "Movies");
        assert!(
            movie_children.contains_key("Alex-October [tmdbid-856721]"),
            "Expected sanitized folder name, got: {:?}",
            movie_children.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn build_stores_per_path_timestamps() {
        let torrents = vec![
            (
                TorrentInfo {
                    id: "1".to_string(),
                    filename: "Movie.2023.1080p.mkv".to_string(),
                    hash: "".to_string(),
                    bytes: 1000,
                    status: "downloaded".to_string(),
                    added: "2023-06-15T14:30:45.000Z".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.2023.1080p.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    }],
                    links: vec!["http://link1".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "Movie".to_string(),
                    year: Some("2023".to_string()),
                    media_type: MediaType::Movie,
                    external_id: None,
                },
            ),
            (
                TorrentInfo {
                    id: "2".to_string(),
                    filename: "Show.S01E01.mkv".to_string(),
                    hash: "".to_string(),
                    bytes: 500,
                    status: "downloaded".to_string(),
                    added: "2024-03-10T08:00:00.000Z".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Show.S01E01.mkv".to_string(),
                        bytes: 500,
                        selected: 1,
                    }],
                    links: vec!["http://link2".to_string()],
                    ..Default::default()
                },
                MediaMetadata {
                    title: "Show".to_string(),
                    year: Some("2024".to_string()),
                    media_type: MediaType::Show,
                    external_id: None,
                },
            ),
        ];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());
        let movie_ts = parse_rd_date("2023-06-15T14:30:45.000Z");
        let show_ts = parse_rd_date("2024-03-10T08:00:00.000Z");

        // Leaf files get the torrent's added timestamp
        assert_eq!(
            vfs.timestamps.get("Movies/Movie/Movie.2023.1080p.mkv"),
            Some(&movie_ts)
        );
        assert_eq!(
            vfs.timestamps.get("Movies/Movie/movie.nfo"),
            Some(&movie_ts)
        );
        assert_eq!(
            vfs.timestamps.get("Shows/Show/Season 01/Show.S01E01.mkv"),
            Some(&show_ts)
        );
        assert_eq!(vfs.timestamps.get("Shows/Show/tvshow.nfo"), Some(&show_ts));

        // Directories get the max timestamp of their children
        assert_eq!(vfs.timestamps.get("Movies/Movie"), Some(&movie_ts));
        assert_eq!(vfs.timestamps.get("Shows/Show/Season 01"), Some(&show_ts));
        assert_eq!(vfs.timestamps.get("Shows/Show"), Some(&show_ts));
        // Root-level dirs get max of all their children
        assert_eq!(vfs.timestamps.get("Movies"), Some(&movie_ts));
        assert_eq!(vfs.timestamps.get("Shows"), Some(&show_ts));
    }

    #[test]
    fn show_duplicate_torrents_deduplicated() {
        // When two torrents for the same show have identical episode filenames
        // (e.g. after repair creates a replacement torrent), the VFS should
        // keep only one copy per episode, not create (1)/(2) duplicates.
        let metadata = MediaMetadata {
            title: "Mr. Mercedes".to_string(),
            year: Some("2017".to_string()),
            media_type: MediaType::Show,
            external_id: Some("tmdb:70485".to_string()),
        };

        let torrents = vec![
            (
                TorrentInfo {
                    id: "old_torrent".to_string(),
                    filename: "Mr.Mercedes.S03".to_string(),
                    hash: "h1".to_string(),
                    bytes: 5000,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![
                        TorrentFile {
                            id: 1,
                            path: "/Mr.Mercedes.S03E01.mkv".to_string(),
                            bytes: 2500,
                            selected: 1,
                        },
                        TorrentFile {
                            id: 2,
                            path: "/Mr.Mercedes.S03E02.mkv".to_string(),
                            bytes: 2500,
                            selected: 1,
                        },
                    ],
                    links: vec![
                        "http://old_link1".to_string(),
                        "http://old_link2".to_string(),
                    ],
                    ..Default::default()
                },
                metadata.clone(),
            ),
            (
                TorrentInfo {
                    id: "new_torrent".to_string(),
                    filename: "Mr.Mercedes.S03".to_string(),
                    hash: "h1".to_string(),
                    bytes: 5000,
                    status: "downloaded".to_string(),
                    added: "2023-01-02".to_string(),
                    files: vec![
                        TorrentFile {
                            id: 1,
                            path: "/Mr.Mercedes.S03E01.mkv".to_string(),
                            bytes: 2500,
                            selected: 1,
                        },
                        TorrentFile {
                            id: 2,
                            path: "/Mr.Mercedes.S03E02.mkv".to_string(),
                            bytes: 2500,
                            selected: 1,
                        },
                    ],
                    links: vec![
                        "http://new_link1".to_string(),
                        "http://new_link2".to_string(),
                    ],
                    ..Default::default()
                },
                metadata,
            ),
        ];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let episodes = dir(&vfs.root, "Shows/Mr. Mercedes [tmdbid-70485]/Season 03");
        // Should have exactly 2 episodes, no (1)/(2) duplicates
        assert_eq!(
            episodes.len(),
            2,
            "Expected 2 episodes but got {}: {:?}",
            episodes.len(),
            episodes.keys().collect::<Vec<_>>()
        );
        assert!(episodes.contains_key("Mr.Mercedes.S03E01.mkv"));
        assert!(episodes.contains_key("Mr.Mercedes.S03E02.mkv"));
    }

    #[test]
    fn diff_trees_identical_trees_no_changes() {
        let old = VfsNode::Directory {
            children: BTreeMap::from([(
                "Movies".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::from([(
                        "Inception".to_string(),
                        VfsNode::Directory {
                            children: BTreeMap::from([(
                                "Inception.mkv".to_string(),
                                VfsNode::MediaFile {
                                    file_size: 1000,
                                    locator: FileLocator {
                                        link: Some("http://link1".to_string()),
                                        torrent_id: "t1".to_string(),
                                        ..Default::default()
                                    },
                                },
                            )]),
                        },
                    )]),
                },
            )]),
        };
        let changes = diff_trees(&old, &old, "");
        assert!(
            changes.is_empty(),
            "Identical trees should produce no changes"
        );
    }

    #[test]
    fn diff_trees_new_directory_created() {
        let old = VfsNode::Directory {
            children: BTreeMap::from([(
                "Shows".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::new(),
                },
            )]),
        };
        let new = VfsNode::Directory {
            children: BTreeMap::from([(
                "Shows".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::from([(
                        "Breaking Bad".to_string(),
                        VfsNode::Directory {
                            children: BTreeMap::from([(
                                "Season 01".to_string(),
                                VfsNode::Directory {
                                    children: BTreeMap::from([(
                                        "S01E01.mkv".to_string(),
                                        VfsNode::MediaFile {
                                            file_size: 1000,
                                            locator: FileLocator {
                                                link: Some("http://link".to_string()),
                                                torrent_id: "t1".to_string(),
                                                ..Default::default()
                                            },
                                        },
                                    )]),
                                },
                            )]),
                        },
                    )]),
                },
            )]),
        };
        let changes = diff_trees(&old, &new, "");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "Shows/Breaking Bad/Season 01");
        assert_eq!(changes[0].update_type, UpdateType::Created);
    }

    #[test]
    fn diff_trees_directory_deleted() {
        let old = VfsNode::Directory {
            children: BTreeMap::from([(
                "Movies".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::from([(
                        "Inception".to_string(),
                        VfsNode::Directory {
                            children: BTreeMap::from([(
                                "Inception.mkv".to_string(),
                                VfsNode::MediaFile {
                                    file_size: 1000,
                                    locator: FileLocator {
                                        link: Some("http://link".to_string()),
                                        torrent_id: "t1".to_string(),
                                        ..Default::default()
                                    },
                                },
                            )]),
                        },
                    )]),
                },
            )]),
        };
        let new = VfsNode::Directory {
            children: BTreeMap::from([(
                "Movies".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::new(),
                },
            )]),
        };
        let changes = diff_trees(&old, &new, "");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "Movies/Inception");
        assert_eq!(changes[0].update_type, UpdateType::Deleted);
    }

    #[test]
    fn diff_trees_file_modified() {
        let old = VfsNode::Directory {
            children: BTreeMap::from([(
                "Movies".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::from([(
                        "Inception".to_string(),
                        VfsNode::Directory {
                            children: BTreeMap::from([(
                                "Inception.mkv".to_string(),
                                VfsNode::MediaFile {
                                    file_size: 1000,
                                    locator: FileLocator {
                                        link: Some("http://old-link".to_string()),
                                        torrent_id: "t1".to_string(),
                                        ..Default::default()
                                    },
                                },
                            )]),
                        },
                    )]),
                },
            )]),
        };
        let new = VfsNode::Directory {
            children: BTreeMap::from([(
                "Movies".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::from([(
                        "Inception".to_string(),
                        VfsNode::Directory {
                            children: BTreeMap::from([(
                                "Inception.mkv".to_string(),
                                VfsNode::MediaFile {
                                    file_size: 1000,
                                    locator: FileLocator {
                                        link: Some("http://new-link".to_string()),
                                        torrent_id: "t2".to_string(),
                                        ..Default::default()
                                    },
                                },
                            )]),
                        },
                    )]),
                },
            )]),
        };
        let changes = diff_trees(&old, &new, "");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "Movies/Inception");
        assert_eq!(changes[0].update_type, UpdateType::Modified);
    }

    #[test]
    fn diff_trees_new_episode_in_existing_show() {
        let old = VfsNode::Directory {
            children: BTreeMap::from([(
                "Shows".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::from([(
                        "Breaking Bad".to_string(),
                        VfsNode::Directory {
                            children: BTreeMap::from([(
                                "Season 01".to_string(),
                                VfsNode::Directory {
                                    children: BTreeMap::from([(
                                        "S01E01.mkv".to_string(),
                                        VfsNode::MediaFile {
                                            file_size: 1000,
                                            locator: FileLocator {
                                                link: Some("http://link1".to_string()),
                                                torrent_id: "t1".to_string(),
                                                ..Default::default()
                                            },
                                        },
                                    )]),
                                },
                            )]),
                        },
                    )]),
                },
            )]),
        };
        let new = VfsNode::Directory {
            children: BTreeMap::from([(
                "Shows".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::from([(
                        "Breaking Bad".to_string(),
                        VfsNode::Directory {
                            children: BTreeMap::from([(
                                "Season 01".to_string(),
                                VfsNode::Directory {
                                    children: BTreeMap::from([
                                        (
                                            "S01E01.mkv".to_string(),
                                            VfsNode::MediaFile {
                                                file_size: 1000,
                                                locator: FileLocator {
                                                    link: Some("http://link1".to_string()),
                                                    torrent_id: "t1".to_string(),
                                                    ..Default::default()
                                                },
                                            },
                                        ),
                                        (
                                            "S01E02.mkv".to_string(),
                                            VfsNode::MediaFile {
                                                file_size: 1000,
                                                locator: FileLocator {
                                                    link: Some("http://link2".to_string()),
                                                    torrent_id: "t2".to_string(),
                                                    ..Default::default()
                                                },
                                            },
                                        ),
                                    ]),
                                },
                            )]),
                        },
                    )]),
                },
            )]),
        };
        let changes = diff_trees(&old, &new, "");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "Shows/Breaking Bad/Season 01");
        assert_eq!(changes[0].update_type, UpdateType::Modified);
    }

    #[test]
    fn parse_rd_date_full_iso() {
        // RD returns "YYYY-MM-DDTHH:mm:ss.sssZ" (JavaScript toJSON format)
        let ts = parse_rd_date("2023-06-15T14:30:45.000Z");
        // 2023-06-15T14:30:45Z = 1686839445 seconds since UNIX_EPOCH
        let expected = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1686839445);
        assert_eq!(ts, expected);
    }

    #[test]
    fn parse_rd_date_date_only() {
        // Test data uses "YYYY-MM-DD"
        let ts = parse_rd_date("2023-01-01");
        let expected = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1672531200);
        assert_eq!(ts, expected);
    }

    #[test]
    fn parse_rd_date_invalid_falls_back() {
        let ts = parse_rd_date("not-a-date");
        assert_eq!(ts, std::time::UNIX_EPOCH);
    }

    #[test]
    fn parse_rd_date_invalid_time_components_keep_the_date() {
        // A malformed/out-of-range TIME must keep the (already-validated) DATE at midnight rather
        // than discarding the whole timestamp for a 1970 mtime (which confuses media-server change
        // detection). 2023-01-01T00:00:00Z = 1672531200.
        let midnight = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1672531200);
        assert_eq!(parse_rd_date("2023-01-01T25:00:00Z"), midnight);
        assert_eq!(parse_rd_date("2023-01-01T00:60:00Z"), midnight);
        assert_eq!(parse_rd_date("2023-01-01T00:00:60Z"), midnight);
        assert_eq!(parse_rd_date("2023-01-01Tgarbage"), midnight);
        // A valid time still adds its seconds; a malformed DATE still falls back to UNIX_EPOCH.
        assert_ne!(parse_rd_date("2023-01-01T23:59:59Z"), midnight);
        assert_ne!(parse_rd_date("2023-01-01T23:59:59Z"), std::time::UNIX_EPOCH);
        assert_eq!(parse_rd_date("2023-13-01T10:00:00Z"), std::time::UNIX_EPOCH);
    }

    #[test]
    fn parse_rd_date_empty_string_falls_back() {
        assert_eq!(parse_rd_date(""), std::time::UNIX_EPOCH);
    }

    #[test]
    fn parse_rd_date_impossible_day_falls_back() {
        // Days that cannot exist in the given month must fall back to UNIX_EPOCH
        // rather than silently rolling into the following month.
        assert_eq!(parse_rd_date("2023-02-30"), std::time::UNIX_EPOCH);
        assert_eq!(parse_rd_date("2023-04-31"), std::time::UNIX_EPOCH);
        // 2023 is not a leap year.
        assert_eq!(parse_rd_date("2023-02-29"), std::time::UNIX_EPOCH);
        // A genuine leap day must still parse.
        assert_ne!(parse_rd_date("2024-02-29"), std::time::UNIX_EPOCH);
        // Last valid day of a 30-day month must still parse.
        assert_ne!(parse_rd_date("2023-04-30"), std::time::UNIX_EPOCH);
    }

    #[test]
    fn parse_rd_date_absurd_year_falls_back_without_overflow() {
        // A crafted huge (non-4-digit) year must fall back to UNIX_EPOCH, not overflow the i64
        // day arithmetic (which would panic in debug builds).
        assert_eq!(
            parse_rd_date("25000000000000000-01-01"),
            std::time::UNIX_EPOCH
        );
        assert_eq!(parse_rd_date("99999-01-01"), std::time::UNIX_EPOCH);
        assert_eq!(parse_rd_date("0-01-01"), std::time::UNIX_EPOCH);
        // The maximum 4-digit year still parses.
        assert_ne!(parse_rd_date("9999-12-31"), std::time::UNIX_EPOCH);
    }

    #[test]
    fn find_deepest_new_dir_stops_at_level_with_leaf_files() {
        // A directory containing a media file alongside a single subdirectory should
        // report itself, not descend into the subdirectory (which would mis-notify
        // the media server of the wrong path).
        let children = BTreeMap::from([
            (
                "Movie.mkv".to_string(),
                VfsNode::MediaFile {
                    file_size: 1,
                    locator: Default::default(),
                },
            ),
            (
                "Subs".to_string(),
                VfsNode::Directory {
                    children: BTreeMap::new(),
                },
            ),
        ]);
        assert_eq!(
            find_deepest_new_dir("Movies/Title", &children),
            "Movies/Title"
        );
    }

    #[test]
    fn find_deepest_new_dir_descends_single_subdirectory_chain() {
        // A pure single-subdirectory chain (e.g. a show season) descends to the leaf level.
        let season = BTreeMap::from([(
            "S01E01.mkv".to_string(),
            VfsNode::MediaFile {
                file_size: 1,
                locator: Default::default(),
            },
        )]);
        let show = BTreeMap::from([(
            "Season 01".to_string(),
            VfsNode::Directory { children: season },
        )]);
        assert_eq!(
            find_deepest_new_dir("Shows/Breaking Bad", &show),
            "Shows/Breaking Bad/Season 01"
        );
    }

    #[test]
    fn diff_trees_multiple_changes() {
        let old = VfsNode::Directory {
            children: BTreeMap::from([
                (
                    "Movies".to_string(),
                    VfsNode::Directory {
                        children: BTreeMap::from([(
                            "Old Movie".to_string(),
                            VfsNode::Directory {
                                children: BTreeMap::from([(
                                    "old.mkv".to_string(),
                                    VfsNode::MediaFile {
                                        file_size: 1000,
                                        locator: FileLocator {
                                            link: Some("http://old".to_string()),
                                            torrent_id: "t1".to_string(),
                                            ..Default::default()
                                        },
                                    },
                                )]),
                            },
                        )]),
                    },
                ),
                (
                    "Shows".to_string(),
                    VfsNode::Directory {
                        children: BTreeMap::new(),
                    },
                ),
            ]),
        };
        let new = VfsNode::Directory {
            children: BTreeMap::from([
                (
                    "Movies".to_string(),
                    VfsNode::Directory {
                        children: BTreeMap::from([(
                            "New Movie".to_string(),
                            VfsNode::Directory {
                                children: BTreeMap::from([(
                                    "new.mkv".to_string(),
                                    VfsNode::MediaFile {
                                        file_size: 1000,
                                        locator: FileLocator {
                                            link: Some("http://new".to_string()),
                                            torrent_id: "t2".to_string(),
                                            ..Default::default()
                                        },
                                    },
                                )]),
                            },
                        )]),
                    },
                ),
                (
                    "Shows".to_string(),
                    VfsNode::Directory {
                        children: BTreeMap::from([(
                            "New Show".to_string(),
                            VfsNode::Directory {
                                children: BTreeMap::from([(
                                    "Season 01".to_string(),
                                    VfsNode::Directory {
                                        children: BTreeMap::from([(
                                            "S01E01.mkv".to_string(),
                                            VfsNode::MediaFile {
                                                file_size: 1000,
                                                locator: FileLocator {
                                                    link: Some("http://ep".to_string()),
                                                    torrent_id: "t3".to_string(),
                                                    ..Default::default()
                                                },
                                            },
                                        )]),
                                    },
                                )]),
                            },
                        )]),
                    },
                ),
            ]),
        };
        let changes = diff_trees(&old, &new, "");
        // Old Movie deleted, New Movie created, New Show/Season 01 created
        assert_eq!(changes.len(), 3);
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert!(paths.contains(&"Movies/Old Movie"));
        assert!(paths.contains(&"Movies/New Movie"));
        assert!(paths.contains(&"Shows/New Show/Season 01"));
    }

    /// link_idx must increment for ALL selected files, not just video files.
    /// If a torrent has selected non-video files (subtitles, NFOs) before the
    /// video file, their links must be skipped to use the correct RD link
    /// for the video file.
    #[test]
    fn link_idx_skips_non_video_selected_files() {
        let torrents = vec![(
            TorrentInfo {
                id: "t1".to_string(),
                filename: "Movie.2023".to_string(),
                hash: "h1".to_string(),
                bytes: 3000,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![
                    // Non-video file selected first -- takes link index 0
                    TorrentFile {
                        id: 1,
                        path: "/subs.srt".to_string(),
                        bytes: 100,
                        selected: 1,
                    },
                    // Video file selected second -- should use link index 1
                    TorrentFile {
                        id: 2,
                        path: "/Movie.mkv".to_string(),
                        bytes: 2000,
                        selected: 1,
                    },
                    // Unselected file -- no link consumed
                    TorrentFile {
                        id: 3,
                        path: "/extras.mkv".to_string(),
                        bytes: 900,
                        selected: 0,
                    },
                ],
                links: vec![
                    "http://link_for_srt".to_string(),
                    "http://link_for_movie".to_string(),
                ],
                ..Default::default()
            },
            MediaMetadata {
                title: "Link Index Test".to_string(),
                year: Some("2023".to_string()),
                media_type: MediaType::Movie,
                external_id: Some("tmdb:999".to_string()),
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let (_, locator) = media_file(&vfs.root, "Movies/Link Index Test [tmdbid-999]/Movie.mkv");
        assert_eq!(
            locator.link.as_deref(),
            Some("http://link_for_movie"),
            "Video file should use link index 1 (after non-video selected file), not link index 0"
        );
    }

    /// Same link_idx logic for shows -- non-video selected files must be skipped.
    #[test]
    fn link_idx_skips_non_video_selected_files_for_shows() {
        let torrents = vec![(
            TorrentInfo {
                id: "t1".to_string(),
                filename: "Show.S01".to_string(),
                hash: "h1".to_string(),
                bytes: 3000,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![
                    // Non-video selected file takes link index 0
                    TorrentFile {
                        id: 1,
                        path: "/subs.srt".to_string(),
                        bytes: 100,
                        selected: 1,
                    },
                    // Video file should use link index 1
                    TorrentFile {
                        id: 2,
                        path: "/Show.S01E01.mkv".to_string(),
                        bytes: 1500,
                        selected: 1,
                    },
                    // Another video file should use link index 2
                    TorrentFile {
                        id: 3,
                        path: "/Show.S01E02.mkv".to_string(),
                        bytes: 1400,
                        selected: 1,
                    },
                ],
                links: vec![
                    "http://link_srt".to_string(),
                    "http://link_ep1".to_string(),
                    "http://link_ep2".to_string(),
                ],
                ..Default::default()
            },
            MediaMetadata {
                title: "Show Link Test".to_string(),
                year: Some("2023".to_string()),
                media_type: MediaType::Show,
                external_id: Some("tmdb:888".to_string()),
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let season = "Shows/Show Link Test [tmdbid-888]/Season 01";
        let (_, ep1) = media_file(&vfs.root, &format!("{season}/Show.S01E01.mkv"));
        assert_eq!(
            ep1.link.as_deref(),
            Some("http://link_ep1"),
            "Episode 1 should use link index 1 (after non-video selected file)"
        );
        let (_, ep2) = media_file(&vfs.root, &format!("{season}/Show.S01E02.mkv"));
        assert_eq!(
            ep2.link.as_deref(),
            Some("http://link_ep2"),
            "Episode 2 should use link index 2"
        );
    }

    /// Movie VFS build should not include unselected files.
    #[test]
    fn build_skips_unselected_files() {
        let torrents = vec![(
            TorrentInfo {
                id: "t1".to_string(),
                filename: "Movie.2023".to_string(),
                hash: "h1".to_string(),
                bytes: 3000,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![
                    TorrentFile {
                        id: 1,
                        path: "/Movie.mkv".to_string(),
                        bytes: 2000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 2,
                        path: "/Deleted.Scenes.mkv".to_string(),
                        bytes: 1000,
                        selected: 0,
                    },
                ],
                links: vec!["http://link_movie".to_string()],
                ..Default::default()
            },
            MediaMetadata {
                title: "Unselected Test".to_string(),
                year: None,
                media_type: MediaType::Movie,
                external_id: None,
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let files = dir(&vfs.root, "Movies/Unselected Test");
        // Only Movie.mkv should be present (Deleted.Scenes.mkv is not selected
        // and also would be filtered by is_video_file's "extras" check... but
        // the point is unselected=0 files never consume a link index either)
        assert_eq!(files.len(), 2, "Should have 1 media + 1 nfo");
        assert!(files.contains_key("Movie.mkv"));
        assert!(files.contains_key("movie.nfo"));
    }

    /// When a torrent has more selected files than links (mismatch), the VFS
    /// should not panic -- it should simply skip files that have no link.
    #[test]
    fn build_handles_link_count_mismatch() {
        let torrents = vec![(
            TorrentInfo {
                id: "t1".to_string(),
                filename: "Mismatched".to_string(),
                hash: "h1".to_string(),
                bytes: 3000,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![
                    TorrentFile {
                        id: 1,
                        path: "/File1.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 2,
                        path: "/File2.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 3,
                        path: "/File3.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    },
                ],
                // Only 2 links for 3 selected files
                links: vec!["http://link1".to_string(), "http://link2".to_string()],
                ..Default::default()
            },
            MediaMetadata {
                title: "Mismatch Test".to_string(),
                year: None,
                media_type: MediaType::Movie,
                external_id: None,
            },
        )];

        // Should not panic
        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        let files = dir(&vfs.root, "Movies/Mismatch Test");
        // Only File1 and File2 should be present (File3 has no link)
        let media_count = files
            .values()
            .filter(|n| matches!(n, VfsNode::MediaFile { .. }))
            .count();
        assert_eq!(
            media_count, 2,
            "Only files with links should be added to VFS"
        );
        // A missing link must SKIP the file, never insert a broken-link
        // placeholder node (behavioural replacement for the former source-text check).
        assert!(
            files.keys().all(|name| {
                !name.contains("Error") && !name.to_lowercase().contains("unrestrict")
            }),
            "missing links must be skipped, not placeholdered: {:?}",
            files.keys().collect::<Vec<_>>()
        );
    }

    // --- is_archive_part expects pre-lowercased input ---

    #[test]
    fn is_archive_part_works_with_pre_lowercased_input() {
        // is_archive_part no longer lowercases internally; callers must pre-lowercase
        assert!(is_archive_part("movie.r00"));
        assert!(is_archive_part("movie.r99"));
        assert!(is_archive_part("movie.s00"));
        assert!(!is_archive_part("movie.mkv"));
        assert!(!is_archive_part("movie.mp4"));
        // Uppercase input is NOT lowercased by is_archive_part — caller must do it
        assert!(!is_archive_part("MOVIE.R00"));
        // But is_archive_file (which calls is_archive_part) handles uppercase
        assert!(is_archive_file("MOVIE.R00"));
    }

    // --- is_video_file excludes files in extras directories ---

    #[test]
    fn is_video_file_rejects_extras_directories() {
        assert!(!is_video_file("/path/Extras/deleted_scene.mkv"));
        assert!(!is_video_file("/path/extras/making_of.mp4"));
        assert!(!is_video_file("/path/Bonus/clip.mkv"));
        assert!(!is_video_file("/path/bonus/clip.avi"));
        assert!(!is_video_file("/path/Featurettes/feature.mkv"));
        assert!(!is_video_file("/path/Behind The Scenes/bts.mkv"));
        assert!(!is_video_file("/path/behind the scenes/bts.mkv"));
        assert!(!is_video_file("/path/Deleted Scenes/scene1.mkv"));
        assert!(!is_video_file("/path/Interviews/interview.mkv"));
        assert!(!is_video_file("/path/Trailers/trailer.mkv"));
        assert!(!is_video_file("/path/Samples/sample_clip.mkv"));
        // Dotted/underscored scene-style directory names (the common remux/scene layout) must be
        // excluded too — not only literal-space directory names.
        assert!(!is_video_file("/path/Behind.The.Scenes/clip.mkv"));
        assert!(!is_video_file("/path/Deleted.Scenes/scene1.mkv"));
        assert!(!is_video_file("/path/Behind_The_Scenes/bts.mkv"));
        assert!(!is_video_file("/path/deleted-scenes/scene.mp4"));
        // Normal directories should not be excluded
        assert!(is_video_file("/path/Season 01/episode.mkv"));
        assert!(is_video_file("/path/Show/S01E01.mkv"));
        // A dotted normal directory must still pass (normalisation must not over-match).
        assert!(is_video_file("/path/The.Movie.2020/feature.mkv"));
    }

    // --- season_tokens: the (strong, part) season numbers SEASON_RE finds in one name ---

    #[test]
    fn season_tokens_reads_part_numbering_as_the_part_signal() {
        assert_eq!(season_tokens("Show.Part1.E01.mkv"), (None, Some(1)));
        assert_eq!(season_tokens("Show.Part 2.E01.mkv"), (None, Some(2)));
        assert_eq!(season_tokens("show.part3.e01.mkv"), (None, Some(3)));
        // An `Sxx` code is a strong token, not a part.
        assert_eq!(season_tokens("Show.S05E01.mkv"), (Some(5), None));
        // Every match is scanned, so a strong token after a `Part N` is kept apart from it.
        assert_eq!(season_tokens("Part 2 - 3x05"), (Some(3), Some(2)));
    }

    #[test]
    fn season_tokens_ignores_pixel_resolution() {
        // A "WIDTHxHEIGHT" pixel resolution must NOT be read as a season number.
        assert_eq!(season_tokens("Show.Episode.5.1920x1080.mkv"), (None, None));
        assert_eq!(season_tokens("Show.720x480.mkv"), (None, None));
        // The "NxNN" episode-numbering convention is still detected.
        assert_eq!(season_tokens("Show.1x05.mkv"), (Some(1), None));
        assert_eq!(season_tokens("Show.12x05.mkv"), (Some(12), None));
        // An explicit SxxExx marker still wins even when a resolution is present.
        assert_eq!(season_tokens("Show.S03E05.1920x1080.mkv"), (Some(3), None));
        // Nor is a channel layout run into a codec a season (`parse_se_all` reads no code there
        // either), so the directory's season places such a code-less file.
        assert_eq!(season_tokens("05 - Title.AAC2.0x264.mkv"), (None, None));
        assert_eq!(season_tokens("05 - Title.DD5.1x264.mkv"), (None, None));
        assert_eq!(season_tokens("Show.AAC2.0x264.mkv"), (None, None));
        assert_eq!(season_tokens("Show.DD5.1x265.mkv"), (None, None));
        // A real `NxNN` code after a year or a title ending in a digit still names its season.
        assert_eq!(season_tokens("Show.Name.2005.2x05.mkv"), (Some(2), None));
        assert_eq!(season_tokens("Babylon.5.2x10.mkv"), (Some(2), None));
        assert_eq!(
            season_of_codeless_file(
                "Show/Season 3/05 - Title.AAC2.0x264.mkv",
                "05 - Title.AAC2.0x264.mkv"
            ),
            3
        );
        assert_eq!(
            season_of_codeless_file(
                "Show/Season 3/05 - Title.DD5.1x264.mkv",
                "05 - Title.DD5.1x264.mkv"
            ),
            3
        );
    }

    #[test]
    fn season_tokens_ignores_mid_token_s_and_oversized_season_number() {
        // A codec token like DTS5 / DDP5 must NOT be read as "season 5": the `s`/digit is mid-token,
        // not a season marker. (Previously `s(\d+)` matched the `s5` inside `DTS5` → bogus Season 05.)
        assert_eq!(season_tokens("Show.E05.DTS5.1.1080p.mkv"), (None, None));
        assert_eq!(season_tokens("Show.E05.DDP5.1.mkv"), (None, None));
        // A 4-digit S-number (a stray S2024) must NOT be read as "season 2024": seasons are 1-2 digit.
        assert_eq!(season_tokens("Show.S2024E01.mkv"), (None, None));
        // Nor is a `part` inside a title word, or a `Part` followed by a year.
        assert_eq!(season_tokens("Counterpart.2017.S01.1080p"), (Some(1), None));
        assert_eq!(season_tokens("Counterpart.2017.1080p"), (None, None));
        assert_eq!(season_tokens("Show.Part.2019.mkv"), (None, None));
        // Genuine markers still resolve.
        assert_eq!(season_tokens("Show.S05E01.mkv"), (Some(5), None));
        assert_eq!(season_tokens("Show.S5.E01.mkv"), (Some(5), None));
        assert_eq!(season_tokens("Show.Part2.E01.mkv"), (None, Some(2)));
        assert_eq!(season_tokens("Show.1x05.mkv"), (Some(1), None));
    }

    #[test]
    fn season_tokens_accepts_dotted_and_underscore_separators() {
        // Scene names separate the spelled-out "Season"/"Part" from the number with a dot/underscore/
        // dash, not just a space; these must resolve, not mis-fold to season 1.
        assert_eq!(season_tokens("Show.Season.2.E05.mkv"), (Some(2), None));
        assert_eq!(season_tokens("Show.Season_2.E05.mkv"), (Some(2), None));
        assert_eq!(season_tokens("Show.Season-2.E05.mkv"), (Some(2), None));
        assert_eq!(season_tokens("Show.Part.3.E01.mkv"), (None, Some(3)));
        // The space/no-separator forms still work.
        assert_eq!(season_tokens("Show.Season 4.E01.mkv"), (Some(4), None));
        assert_eq!(season_tokens("Show.Season5.E01.mkv"), (Some(5), None));
        // `_` also bounds the `Part N`, `Sxx` and `NxNN` tokens (the regex crate's `\b` treats it as
        // a word character, so a word-bounded arm missed these).
        assert_eq!(season_tokens("Show_Part_2_E01.mkv"), (None, Some(2)));
        assert_eq!(season_tokens("Show_Part_2"), (None, Some(2)));
        assert_eq!(season_tokens("Part_2_Title.mkv"), (None, Some(2)));
        assert_eq!(season_tokens("Show_S02_1080p"), (Some(2), None));
        assert_eq!(season_tokens("Show_1x05_Title.mkv"), (Some(1), None));
        // A separator shared by two tokens bounds both of them.
        assert_eq!(season_tokens("Part_2_S03"), (Some(3), Some(2)));
        assert_eq!(season_tokens("Show.S02.Part.3"), (Some(2), Some(3)));
        // A keyword with no following digit ("Seasonal"), or `part` inside a word ("Departure"),
        // must NOT match.
        assert_eq!(season_tokens("Seasonal.Documentary.mkv"), (None, None));
        assert_eq!(season_tokens("Departure.2.mkv"), (None, None));
    }

    #[test]
    fn episode_files_extracts_dash_concatenated_pack() {
        // A pre-existing absolute/concatenated-coded season pack must report its episodes (so the
        // reconciler doesn't re-acquire them as per-episode duplicates).
        let info = TorrentInfo {
            hash: "h".into(),
            files: vec![
                TorrentFile {
                    id: 0,
                    path: "Some Show - 409 - Episode Title.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                },
                TorrentFile {
                    id: 1,
                    path: "Some Show - 410 - Another Title.mkv".into(),
                    bytes: 2_000_000_000,
                    selected: 1,
                },
            ],
            ..Default::default()
        };
        let mut eps: Vec<(u32, u32)> = episode_files(&info)
            .into_iter()
            .map(|(s, e, _)| (s, e))
            .collect();
        eps.sort_unstable();
        assert_eq!(eps, vec![(4, 9), (4, 10)]);
    }

    #[test]
    fn episode_files_reports_all_episodes_of_a_multi_episode_file() {
        // A single file covering two episodes must report BOTH (and both point to that file), so the
        // reconciler sees the second episode as owned instead of re-acquiring it as a duplicate.
        let info = TorrentInfo {
            hash: "h".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Show.S01E01E02.1080p.mkv".into(),
                bytes: 2_000_000_000,
                selected: 1,
            }],
            ..Default::default()
        };
        let eps = episode_files(&info);
        assert_eq!(
            eps,
            vec![
                (1, 1, "Show.S01E01E02.1080p.mkv".to_string()),
                (1, 2, "Show.S01E01E02.1080p.mkv".to_string()),
            ]
        );
    }

    #[test]
    fn episode_files_keeps_the_first_file_of_each_episode() {
        // A pack holding one episode twice (language folders, two resolutions) yields it once, from
        // its first file: the one `observe` title-checks and probes. A multi-episode file still
        // yields every episode it carries.
        let file = |id, path: &str| TorrentFile {
            id,
            path: path.into(),
            bytes: 2_000_000_000,
            selected: 1,
        };
        let info = TorrentInfo {
            hash: "h".into(),
            files: vec![
                file(0, "A/Show.S01E01.mkv"),
                file(1, "B/Show.S01E01.mkv"),
                file(2, "A/Show.S01E02E03.mkv"),
                file(3, "B/Show.S01E03.mkv"),
            ],
            ..Default::default()
        };
        assert_eq!(
            episode_files(&info),
            vec![
                (1, 1, "A/Show.S01E01.mkv".to_string()),
                (1, 2, "A/Show.S01E02E03.mkv".to_string()),
                (1, 3, "A/Show.S01E02E03.mkv".to_string()),
            ]
        );
    }

    /// `explicit_season_of_codeless_file` reads `season_of_codeless_file`'s season signals
    /// bar the season-1 default and a file name's `Part N`: wherever it names a season, the VFS
    /// places the file in that season. The nearest name that names a season RANGE names none.
    #[test]
    fn explicit_codeless_season_agrees_with_the_vfs_placement() {
        for (path, expected) in [
            ("Show.S04/Season 04/E09 - B.mkv", Some(4)),
            ("Show S2 - 05 (1080p).mkv", Some(2)),
            ("[Group] Show Season 2 - 05.mkv", Some(2)),
            ("Show.S01-S03/Season 2/E01.mkv", Some(2)),
            ("Show/Part 3/E01.mkv", Some(3)),
            ("Show - 05 (1080p).mkv", None),
            ("Pack/[Group] Show - 05.mkv", None),
            ("Pack/E05 - Title, Part 2.mkv", None),
            (
                "[Group] Show (Season 1-2) [1080p]/[Group] Show - 13.mkv",
                None,
            ),
            ("Show.S01-S03/Show.E01.mkv", None),
            ("Show Seasons 1 to 3/E01.mkv", None),
            ("Show.S01.S02/E01.mkv", None),
            // Glued and symbol-joined ranges in a pack folder.
            ("Show.S01-03.1080p/Show - 26.mkv", None),
            ("Show S1-3 [1080p]/Show - 26.mkv", None),
            ("Show Season 1&2/Show - 26.mkv", None),
            ("Show Season 1+2/Show - 26.mkv", None),
            ("Show Seasons 1-3/Show - 26.mkv", None),
            // A range in an outer directory doesn't hide a season the nearer one names.
            ("Show.S01-S03/Part 2/E01.mkv", Some(2)),
        ] {
            let name = path.rsplit('/').next().unwrap();
            assert_eq!(
                explicit_season_of_codeless_file(path, name),
                expected,
                "{path}"
            );
            if let Some(s) = expected {
                assert_eq!(season_of_codeless_file(path, name), s, "{path}");
            }
        }
    }

    /// An absolutely numbered batch under a season-range folder (TorBox paths start with the torrent
    /// folder) names no season for its files: mapping them to the range's first season would record
    /// S01E26 and on for what TMDB calls S02.
    #[test]
    fn a_batch_under_a_season_range_maps_nothing() {
        let info = TorrentInfo {
            files: ["13", "14"]
                .iter()
                .enumerate()
                .map(|(i, n)| TorrentFile {
                    id: i as u32,
                    path: format!("[Group] Show (Season 1-2) [1080p]/[Group] Show - {n}.mkv"),
                    bytes: 1_000_000_000,
                    selected: 1,
                })
                .collect(),
            ..Default::default()
        };
        assert!(codeless_episode_files(&info).is_empty());
    }

    /// `locator_for_selected` advances the link position ONLY for `selected == 1` files, pairing
    /// RD's per-file restricted links positionally among the SELECTED files. With a SELECTED file
    /// AND an UNSELECTED file both preceding the target, the target is the 2nd selected file
    /// (`link_idx == 1`) and must pair `links[1]`; the unselected file must NOT consume an index.
    /// The instant repair locates a replacement's file this way, and its single-selected-file tests
    /// never reach `link_idx > 0`, so this guards it, and the redirect's `selected_locators`.
    #[test]
    fn locator_for_selected_pairs_link_by_position_among_selected_files() {
        let info = TorrentInfo {
            id: "tid".to_string(),
            hash: "H".to_string(),
            status: "downloaded".to_string(),
            files: vec![
                // Selected, but not the target → pushes link_idx to 1.
                TorrentFile {
                    id: 10,
                    path: "/SelectedFirst.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                },
                // Unselected → must be skipped WITHOUT consuming a link index.
                TorrentFile {
                    id: 20,
                    path: "/Unselected.mkv".to_string(),
                    bytes: 500,
                    selected: 0,
                },
                // The target: the 2nd selected file → must pair links[1].
                TorrentFile {
                    id: 30,
                    path: "/Target.mkv".to_string(),
                    bytes: 2000,
                    selected: 1,
                },
            ],
            // RD emits one link per SELECTED file, in selected order: [SelectedFirst, Target].
            links: vec![
                "https://rd/selected-first".to_string(),
                "https://rd/target".to_string(),
            ],
            ..Default::default()
        };

        let loc = locator_for_selected(&info, "H", "/Target.mkv")
            .expect("target file should resolve to a locator");

        assert_eq!(loc.file_id, 30);
        assert_eq!(loc.file_path, "/Target.mkv");
        assert_eq!(loc.hash, "H");
        assert_eq!(loc.torrent_id, "tid");
        assert_eq!(
            loc.link.as_deref(),
            Some("https://rd/target"),
            "the 2nd selected file must pair links[1]; the unselected file must not consume an index"
        );
        let all: Vec<(u32, Option<String>)> = selected_locators(&info, "H")
            .map(|l| (l.file_id, l.link))
            .collect();
        assert_eq!(
            all,
            vec![
                (10, Some("https://rd/selected-first".to_string())),
                (30, Some("https://rd/target".to_string())),
            ]
        );
        assert!(locator_for_selected(&info, "H", "/Unselected.mkv").is_none());
    }

    /// A pack folder that names a season range with a glued second number (`S01-03`, `S1-3`), an
    /// `&`/`+` with no spaces (`Season 1&2`), or only `Seasons N-M` names no single season, so an
    /// absolutely numbered batch under it maps nothing (`codeless_episode_files`). A file name's
    /// `S2 - 05` and `Season 2 - 05` stay season 2's episode 5, and a nearer season folder still
    /// wins.
    #[test]
    fn explicit_season_of_codeless_file_treats_glued_and_symbol_ranges_as_ranges() {
        for path in [
            "Show.S01-03.1080p/Show - 26.mkv",
            "Show S1-3 [1080p]/Show - 26.mkv",
            "Show Season 1&2/Show - 26.mkv",
            "Show Season 1+2/Show - 26.mkv",
            "Show Seasons 1-3/Show - 26.mkv",
            "Show Seasons 1 to 3/E01.mkv",
            "Show.Season.1&Season.2/Show - 26.mkv",
        ] {
            let name = path.rsplit('/').next().unwrap();
            assert_eq!(explicit_season_of_codeless_file(path, name), None, "{path}");
        }
        for (path, season) in [
            ("Show S2 - 05 (1080p).mkv", 2),
            ("[Group] Show Season 2 - 05.mkv", 2),
            ("Show.S01-S03/Season 2/E01.mkv", 2),
            ("Show.S01-03.1080p/Season 2/E01.mkv", 2),
            ("Show.S02.1080p/E05.mkv", 2),
            // A glued dash that is no range: a resolution, a bit depth, a channel layout.
            ("Show.S02-1080p/E05.mkv", 2),
            ("Show.S02-10bit/E05.mkv", 2),
            ("Show.S02-5.1/E05.mkv", 2),
            // An aspect-ratio tag beside the season is no second season.
            ("Show S02 (16x9)/E05.mkv", 2),
        ] {
            let name = path.rsplit('/').next().unwrap();
            assert_eq!(
                explicit_season_of_codeless_file(path, name),
                Some(season),
                "{path}"
            );
        }
        let files = |dir: &str| TorrentInfo {
            files: ["26", "27"]
                .iter()
                .enumerate()
                .map(|(i, n)| TorrentFile {
                    id: i as u32,
                    path: format!("{dir}/Show - {n}.mkv"),
                    bytes: 1_000_000_000,
                    selected: 1,
                })
                .collect(),
            ..Default::default()
        };
        for dir in ["Show S1-3 [1080p]", "Show Season 1&2", "Show Seasons 1-3"] {
            assert!(codeless_episode_files(&files(dir)).is_empty(), "{dir}");
        }
    }

    /// Test that archive-only torrents produce an empty movie folder (no media files).
    #[test]
    fn build_skips_archive_only_torrents() {
        let torrents = vec![(
            TorrentInfo {
                id: "t1".to_string(),
                filename: "Archive.Movie".to_string(),
                hash: "h1".to_string(),
                bytes: 5000,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![
                    TorrentFile {
                        id: 1,
                        path: "/movie.rar".to_string(),
                        bytes: 3000,
                        selected: 1,
                    },
                    TorrentFile {
                        id: 2,
                        path: "/movie.r00".to_string(),
                        bytes: 2000,
                        selected: 1,
                    },
                ],
                links: vec!["http://link1".to_string(), "http://link2".to_string()],
                ..Default::default()
            },
            MediaMetadata {
                title: "Archive Movie".to_string(),
                year: None,
                media_type: MediaType::Movie,
                external_id: None,
            },
        )];

        let vfs = DebridVfs::build(torrents, &crate::vfs::SelectionMap::new());

        // Archive-only torrents should produce no folder (no video files = no entry)
        assert!(
            !dir(&vfs.root, "Movies").contains_key("Archive Movie"),
            "Archive-only torrent should not produce a folder in VFS"
        );
    }
}

#[cfg(test)]
mod selection_tests {
    use super::test_support::{dir, movie_torrent, season_file_names, show_torrent};
    use super::*;
    use crate::rd_client::{TorrentFile, TorrentInfo};
    use crate::store::{episode_slot, movie_slot, SelectionEntry};

    fn movie_meta(tmdb: u64) -> MediaMetadata {
        MediaMetadata {
            title: "Movie".into(),
            year: Some("2023".into()),
            media_type: MediaType::Movie,
            external_id: Some(format!("tmdb:{}", tmdb)),
        }
    }
    fn show_meta(tmdb: u64) -> MediaMetadata {
        MediaMetadata {
            title: "Show".into(),
            year: None,
            media_type: MediaType::Show,
            external_id: Some(format!("tmdb:{}", tmdb)),
        }
    }
    fn movie_file_locators(vfs: &DebridVfs) -> Vec<FileLocator> {
        // Collect every MediaFile locator under Movies/.
        fn walk(n: &VfsNode, out: &mut Vec<FileLocator>) {
            match n {
                VfsNode::Directory { children } => {
                    for c in children.values() {
                        walk(c, out)
                    }
                }
                VfsNode::MediaFile { locator, .. } => out.push(locator.clone()),
                VfsNode::VirtualFile { .. } => {}
            }
        }
        let mut out = Vec::new();
        for folder in dir(&vfs.root, "Movies").values() {
            walk(folder, &mut out);
        }
        out
    }

    #[test]
    fn movie_empty_selection_falls_back_to_largest() {
        // Two torrents for one movie; no selection ⇒ largest (h_big) wins (legacy behaviour).
        let torrents = vec![
            (
                movie_torrent("small", "h_small", 1_000_000_000),
                movie_meta(27205),
            ),
            (
                movie_torrent("big", "h_big", 9_000_000_000),
                movie_meta(27205),
            ),
        ];
        let vfs = DebridVfs::build(torrents, &SelectionMap::new());
        let locs = movie_file_locators(&vfs);
        assert_eq!(locs.len(), 1);
        assert_eq!(
            locs[0].hash, "h_big",
            "largest-bytes fallback picks the big torrent"
        );
    }

    #[test]
    fn movie_selection_overrides_largest() {
        // Selection points at the SMALL torrent's hash ⇒ it must win over the larger one.
        let mut sel = SelectionMap::new();
        sel.insert(
            movie_slot(27205),
            SelectionEntry {
                hash: "h_small".into(),
                file_path: "Movie.2023.1080p.mkv".into(),
            },
        );
        let torrents = vec![
            (
                movie_torrent("small", "h_small", 1_000_000_000),
                movie_meta(27205),
            ),
            (
                movie_torrent("big", "h_big", 9_000_000_000),
                movie_meta(27205),
            ),
        ];
        let vfs = DebridVfs::build(torrents, &sel);
        let locs = movie_file_locators(&vfs);
        assert_eq!(locs.len(), 1);
        assert_eq!(
            locs[0].hash, "h_small",
            "selection overrides the largest-bytes default"
        );
    }

    #[test]
    fn movie_selection_falls_back_to_streamable_when_selection_is_archive_only() {
        // The managed selection points at a torrent that yields NO streamable video (archive-only
        // RAR). The movie must still appear — self-healing to the streamable copy elsewhere in the
        // group — rather than vanishing because the selected hash has nothing to stream.
        let archive = TorrentInfo {
            id: "arch".into(),
            hash: "h_arch".into(),
            bytes: 9_000_000_000,
            status: "downloaded".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Movie.2023.2160p.rar".into(),
                bytes: 9_000_000_000,
                selected: 1,
            }],
            ..Default::default()
        };
        let streamable = movie_torrent("small", "h_video", 2_000_000_000);
        let mut sel = SelectionMap::new();
        sel.insert(
            movie_slot(27205),
            SelectionEntry {
                hash: "h_arch".into(),
                file_path: "Movie.2023.2160p.rar".into(),
            },
        );
        let vfs = DebridVfs::build(
            vec![
                (archive, movie_meta(27205)),
                (streamable, movie_meta(27205)),
            ],
            &sel,
        );
        let locs = movie_file_locators(&vfs);
        assert_eq!(
            locs.len(),
            1,
            "the movie must still appear, self-healed to the streamable copy"
        );
        assert_eq!(
            locs[0].hash, "h_video",
            "an archive-only selection must fall through to the streamable copy"
        );
    }

    /// A code-less pack's file is gated on the episode the engine maps it to, as a coded file is on
    /// its code: once its slot has moved to another present copy (a repoint, or a pack-served
    /// episode split to a single), the pack's file for that episode is hidden instead of listed
    /// beside the new copy. A stale slot hides nothing, and nor does a slot elsewhere for a file
    /// that maps to no episode (an absolutely numbered pack with no explicit season).
    #[test]
    fn a_codeless_pack_file_is_hidden_once_its_slot_moved_to_another_copy() {
        let pack = || {
            show_torrent(
                "pack",
                "h_pack",
                4_000_000_000,
                &[
                    "Show.S04/Season 04/E09 - Title.mkv",
                    "Show.S04/Season 04/E10 - Title.mkv",
                ],
            )
        };
        let single = || {
            show_torrent(
                "single",
                "h_single",
                3_000_000_000,
                &["Show.S04E09.1080p.mkv"],
            )
        };
        let slots = |e09_hash: &str, e09_path: &str| {
            let mut sel = SelectionMap::new();
            sel.insert(
                episode_slot(1396, 4, 9),
                SelectionEntry {
                    hash: e09_hash.into(),
                    file_path: e09_path.into(),
                },
            );
            sel.insert(
                episode_slot(1396, 4, 10),
                SelectionEntry {
                    hash: "h_pack".into(),
                    file_path: "Show.S04/Season 04/E10 - Title.mkv".into(),
                },
            );
            sel
        };
        // (4,9) moved to the single: one E09 file, the single's, and the pack's E10.
        let vfs = DebridVfs::build(
            vec![(pack(), show_meta(1396)), (single(), show_meta(1396))],
            &slots("h_single", "Show.S04E09.1080p.mkv"),
        );
        assert_eq!(
            season_file_names(&vfs, "Season 04"),
            vec!["E10 - Title.mkv", "Show.S04E09.1080p.mkv"]
        );
        // A stale (4,9) slot (its hash isn't listed) hides nothing.
        let vfs = DebridVfs::build(
            vec![(pack(), show_meta(1396)), (single(), show_meta(1396))],
            &slots("h_gone", "Show.S04E09.1080p.mkv"),
        );
        assert_eq!(
            season_file_names(&vfs, "Season 04"),
            vec![
                "E09 - Title.mkv",
                "E10 - Title.mkv",
                "Show.S04E09.1080p.mkv"
            ]
        );
        // An absolutely numbered pack with no explicit season maps nothing, so it stays whole.
        let batch = show_torrent(
            "batch",
            "h_batch",
            4_000_000_000,
            &["Pack/[Group] Show - 05.mkv", "Pack/[Group] Show - 06.mkv"],
        );
        let coded = show_torrent("coded", "h_coded", 1_000_000_000, &["Show.S01E05.mkv"]);
        let mut sel = SelectionMap::new();
        sel.insert(
            episode_slot(1396, 1, 5),
            SelectionEntry {
                hash: "h_coded".into(),
                file_path: "Show.S01E05.mkv".into(),
            },
        );
        let vfs = DebridVfs::build(
            vec![(batch, show_meta(1396)), (coded, show_meta(1396))],
            &sel,
        );
        assert_eq!(
            season_file_names(&vfs, "Season 01"),
            vec![
                "Show.S01E05.mkv",
                "[Group] Show - 05.mkv",
                "[Group] Show - 06.mkv"
            ]
        );
    }

    #[test]
    fn episode_selection_picks_the_selected_hash_per_episode() {
        // Two torrents both contain S01E01; selection picks the per-episode hash "h_pack".
        let pack = TorrentInfo {
            id: "pack".into(),
            hash: "h_pack".into(),
            bytes: 5_000_000_000,
            status: "downloaded".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Show.S01E01.1080p.mkv".into(),
                bytes: 2_000_000_000,
                selected: 1,
            }],
            links: vec!["https://cdn/pack-e1".into()],
            ..Default::default()
        };
        let single = TorrentInfo {
            id: "single".into(),
            hash: "h_single".into(),
            bytes: 9_000_000_000,
            status: "downloaded".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Show.S01E01.2160p.mkv".into(),
                bytes: 9_000_000_000,
                selected: 1,
            }],
            links: vec!["https://cdn/single-e1".into()],
            ..Default::default()
        };
        let mut sel = SelectionMap::new();
        sel.insert(
            episode_slot(1396, 1, 1),
            SelectionEntry {
                hash: "h_pack".into(),
                file_path: "Show.S01E01.1080p.mkv".into(),
            },
        );
        // single is larger, so without selection it would win; selection forces h_pack.
        let vfs = DebridVfs::build(
            vec![(single, show_meta(1396)), (pack, show_meta(1396))],
            &sel,
        );
        // S01E01 is present once, from the selected hash.
        let hashes: Vec<&str> = dir(&vfs.root, "Shows/Show [tmdbid-1396]/Season 01")
            .values()
            .filter_map(|n| match n {
                VfsNode::MediaFile { locator, .. } => Some(locator.hash.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(hashes, ["h_pack"], "per-episode selection wins");
    }

    #[test]
    fn episode_stale_selection_degrades_to_legacy_dedup() {
        // A selection entry naming a hash that is NOT in the group is stale; it must NOT hide the
        // episode — self-healing falls through to the legacy dedup so the episode still appears.
        let torrent = TorrentInfo {
            id: "t".into(),
            hash: "h_live".into(),
            bytes: 1_000_000_000,
            status: "downloaded".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Show.S01E01.1080p.mkv".into(),
                bytes: 1_000_000_000,
                selected: 1,
            }],
            links: vec!["https://cdn/e1".into()],
            ..Default::default()
        };
        let mut sel = SelectionMap::new();
        sel.insert(
            episode_slot(1396, 1, 1),
            SelectionEntry {
                hash: "h_gone".into(),
                file_path: "Show.S01E01.1080p.mkv".into(),
            },
        );
        let vfs = DebridVfs::build(vec![(torrent, show_meta(1396))], &sel);
        // Episode S01E01 must still be present under Shows/<show>/Season 01.
        assert_eq!(
            season_file_names(&vfs, "Season 01"),
            ["Show.S01E01.1080p.mkv"],
            "stale selection (absent hash) must not hide the episode"
        );
    }

    #[test]
    fn episode_selection_present_hash_wrong_path_degrades_to_legacy_dedup() {
        // The selection names a hash that IS present, but a file_path that no file under that hash
        // matches (e.g. the provider re-listed the torrent with a slightly different path). The
        // episode must still appear — the (hash, file_path) pair isn't emittable, so it degrades to
        // the legacy dedup rather than skipping every copy and vanishing.
        let torrent = TorrentInfo {
            id: "t".into(),
            hash: "h_live".into(),
            bytes: 1_000_000_000,
            status: "downloaded".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Show.S01E01.1080p.mkv".into(),
                bytes: 1_000_000_000,
                selected: 1,
            }],
            links: vec!["https://cdn/e1".into()],
            ..Default::default()
        };
        let mut sel = SelectionMap::new();
        sel.insert(
            episode_slot(1396, 1, 1),
            SelectionEntry {
                hash: "h_live".into(),
                // Present hash, but a path no file under it matches.
                file_path: "Show.S01E01.720p.mkv".into(),
            },
        );
        let vfs = DebridVfs::build(vec![(torrent, show_meta(1396))], &sel);
        assert_eq!(
            season_file_names(&vfs, "Season 01"),
            ["Show.S01E01.1080p.mkv"],
            "a selection with a present hash but mismatched file_path must not hide the episode"
        );
    }

    #[test]
    fn episode_selection_pointing_at_a_linkless_file_degrades_to_legacy_dedup() {
        // RD's `selected != links` anomaly: torrent A has TWO selected videos but only ONE link, so
        // its S01E02 (link_idx 1) has NO positional link and is NOT emittable. A managed selection
        // points S01E02 at that non-emittable copy; torrent B holds an emittable S01E02. The episode
        // must NOT vanish — because A's S01E02 isn't emittable it must not count as "present", so the
        // gate degrades to legacy dedup and B's copy is emitted. (Before the fix, present_files
        // included the link-less file, the gate suppressed B's copy, and S01E02 disappeared.)
        let a = TorrentInfo {
            id: "ta".into(),
            hash: "h_sel".into(),
            bytes: 2_000_000_000,
            status: "downloaded".into(),
            files: vec![
                TorrentFile {
                    id: 0,
                    path: "Show.S01E01.1080p.mkv".into(),
                    bytes: 1_000_000_000,
                    selected: 1,
                },
                TorrentFile {
                    id: 1,
                    path: "Show.S01E02.1080p.mkv".into(),
                    bytes: 1_000_000_000,
                    selected: 1,
                },
            ],
            links: vec!["https://cdn/e1".into()], // ONE link for TWO selected files (the anomaly)
            ..Default::default()
        };
        let b = TorrentInfo {
            id: "tb".into(),
            hash: "h_alt".into(),
            bytes: 1_000_000_000,
            status: "downloaded".into(),
            files: vec![TorrentFile {
                id: 0,
                path: "Show.S01E02.1080p.mkv".into(),
                bytes: 1_000_000_000,
                selected: 1,
            }],
            links: vec!["https://cdn/e2_alt".into()],
            ..Default::default()
        };
        let mut sel = SelectionMap::new();
        sel.insert(
            episode_slot(1396, 1, 2),
            SelectionEntry {
                hash: "h_sel".into(),
                file_path: "Show.S01E02.1080p.mkv".into(),
            },
        );
        let vfs = DebridVfs::build(vec![(a, show_meta(1396)), (b, show_meta(1396))], &sel);
        // Season 01 must hold BOTH episodes: S01E01 (from A, emittable) and S01E02 (self-healed to
        // B, since A's copy is link-less). Before the fix only S01E01 survived.
        let names = season_file_names(&vfs, "Season 01");
        assert_eq!(
            names,
            ["Show.S01E01.1080p.mkv", "Show.S01E02.1080p.mkv"],
            "S01E02 must be self-healed to the emittable copy, not vanished"
        );
    }

    fn sel_entry(hash: &str, file_path: &str) -> SelectionEntry {
        SelectionEntry {
            hash: hash.into(),
            file_path: file_path.into(),
        }
    }

    #[test]
    fn multi_episode_file_selected_for_its_second_episode_stays_visible() {
        // Pack B holds a double-episode file; a better singleton C holds S01E01. `observe` keeps the
        // (1,1) slot on C and gives the empty (1,2) slot to B's double file. The double file is keyed
        // by its FIRST code, (1,1), whose selection names C, so it used to be hidden, and with it the
        // only copy of E02. A file some slot selects must be emitted.
        let b = show_torrent(
            "tb",
            "h_b",
            4_000_000_000,
            &["Show.S01E01E02.mkv", "Show.S01E03.mkv"],
        );
        let c = show_torrent("tc", "h_c", 1_000_000_000, &["Show.S01E01.mkv"]);
        let mut sel = SelectionMap::new();
        sel.insert(
            episode_slot(1396, 1, 1),
            sel_entry("h_c", "Show.S01E01.mkv"),
        );
        sel.insert(
            episode_slot(1396, 1, 2),
            sel_entry("h_b", "Show.S01E01E02.mkv"),
        );
        let vfs = DebridVfs::build(vec![(b, show_meta(1396)), (c, show_meta(1396))], &sel);
        assert_eq!(
            season_file_names(&vfs, "Season 01"),
            vec!["Show.S01E01.mkv", "Show.S01E01E02.mkv", "Show.S01E03.mkv"],
            "E02's only copy (the double file its slot selects) must not vanish"
        );
    }

    #[test]
    fn multi_episode_file_with_an_unselected_second_episode_stays_visible() {
        // An unmanaged (mirror) pack: nothing selects (1,2), and (1,1) selects another copy. The
        // double file is the only copy of E02, so it falls back to the legacy dedup and is emitted.
        let b = show_torrent(
            "tb",
            "h_b",
            4_000_000_000,
            &["Show.S01E01E02.mkv", "Show.S01E03.mkv"],
        );
        let c = show_torrent("tc", "h_c", 1_000_000_000, &["Show.S01E01.mkv"]);
        let mut sel = SelectionMap::new();
        sel.insert(
            episode_slot(1396, 1, 1),
            sel_entry("h_c", "Show.S01E01.mkv"),
        );
        let vfs = DebridVfs::build(vec![(b, show_meta(1396)), (c, show_meta(1396))], &sel);
        assert_eq!(
            season_file_names(&vfs, "Season 01"),
            vec!["Show.S01E01.mkv", "Show.S01E01E02.mkv", "Show.S01E03.mkv"],
        );
    }

    #[test]
    fn multi_episode_file_selected_elsewhere_for_every_episode_is_suppressed() {
        // Both episodes the double file carries are selected from other present copies, so it adds
        // nothing and stays hidden (no duplicates).
        let b = show_torrent(
            "tb",
            "h_b",
            4_000_000_000,
            &["Show.S01E01E02.mkv", "Show.S01E03.mkv"],
        );
        let c = show_torrent("tc", "h_c", 1_000_000_000, &["Show.S01E01.mkv"]);
        let d = show_torrent("td", "h_d", 1_000_000_000, &["Show.S01E02.mkv"]);
        let mut sel = SelectionMap::new();
        sel.insert(
            episode_slot(1396, 1, 1),
            sel_entry("h_c", "Show.S01E01.mkv"),
        );
        sel.insert(
            episode_slot(1396, 1, 2),
            sel_entry("h_d", "Show.S01E02.mkv"),
        );
        let vfs = DebridVfs::build(
            vec![
                (b, show_meta(1396)),
                (c, show_meta(1396)),
                (d, show_meta(1396)),
            ],
            &sel,
        );
        assert_eq!(
            season_file_names(&vfs, "Season 01"),
            vec!["Show.S01E01.mkv", "Show.S01E02.mkv", "Show.S01E03.mkv"],
        );
    }

    #[test]
    fn movie_colliding_basenames_in_subpaths_both_kept() {
        // A movie torrent with TWO selected videos sharing a basename in different sub-paths
        // (`/A/Movie.mkv` and `/B/Movie.mkv`). The VFS keys leaf nodes by basename, so without the
        // `add_path_to_tree` " (N)" rename branch the second insert would SILENTLY OVERWRITE the
        // first (one file lost). Both must survive — one renamed `Movie (1).mkv` — and each must
        // keep its own distinct locator/link (this branch is live for movies; the show path
        // pre-filters collisions, so only movies exercise it).
        let torrent = TorrentInfo {
            id: "t".into(),
            hash: "h".into(),
            bytes: 6_000_000_000,
            status: "downloaded".into(),
            files: vec![
                TorrentFile {
                    id: 0,
                    path: "/A/Movie.mkv".into(),
                    bytes: 3_000_000_000,
                    selected: 1,
                },
                TorrentFile {
                    id: 1,
                    path: "/B/Movie.mkv".into(),
                    bytes: 3_000_000_000,
                    selected: 1,
                },
            ],
            links: vec!["https://cdn/a".into(), "https://cdn/b".into()],
            ..Default::default()
        };
        let vfs = DebridVfs::build(vec![(torrent, movie_meta(27205))], &SelectionMap::new());

        // Collect the media-file leaf names + their links in the movie's folder.
        let mut names: Vec<String> = Vec::new();
        let mut links: Vec<String> = Vec::new();
        for (name, node) in dir(&vfs.root, "Movies/Movie [tmdbid-27205]") {
            if let VfsNode::MediaFile { locator, .. } = node {
                names.push(name.clone());
                links.push(locator.link.clone().unwrap_or_default());
            }
        }
        names.sort();

        assert_eq!(
            names.len(),
            2,
            "both colliding-basename videos must be kept, got {:?}",
            names
        );
        assert!(
            names.contains(&"Movie.mkv".to_string()),
            "the first file keeps its basename, got {:?}",
            names
        );
        assert!(
            names.iter().any(|n| n == "Movie (1).mkv"),
            "the colliding file must be renamed (not lost), got {:?}",
            names
        );
        // Each kept file must retain its own distinct locator/link — proof neither overwrote the
        // other (an overwrite would leave a single locator behind both names).
        links.sort();
        links.dedup();
        assert_eq!(
            links.len(),
            2,
            "each kept file must retain its distinct link, got {:?}",
            links
        );
    }
}
