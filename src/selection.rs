//! Serve the best copy already in the account: repoint a movie or episode `selection` slot from the
//! copy it serves to a strictly better one that is present.
//!
//! The VFS shows only a slot's selected file. Slots are written when the engine verifies a copy
//! (`observe`) or swaps in an upgrade, and nothing else ever moved one, so a better copy that
//! arrived any other way — an account-mirror copy (pre-existing, or re-added by hand) never gets a
//! slot of its own — stayed hidden behind the worse copy the slot already named. The scan loop runs
//! [`repoint_to_better_copies`] after dedup, so every slot ends up serving the best present copy,
//! judged by the file it would serve (a mixed-quality pack is not ranked by its best file). Movie
//! slots never move onto or off a multi-film collection.

use crate::rd_client::TorrentInfo;
use crate::read_activity::ReadActivity;
use crate::store::{OwnedRecord, OwnedStatus, SelectionEntry, Store};
use crate::vfs::MediaMetadata;
use std::collections::HashSet;
use std::time::Duration;
use tracing::{debug, info, warn};

/// The upgrade engine's resolution rules, which the planner must not undo: with
/// `allow_resolution_downgrade` the engine moves slots off copies above `max_resolution` (the
/// ceiling), so the planner never moves a slot onto one. Without it the ceiling plays no part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepointRules {
    /// `MAX_RESOLUTION` as a height (720/1080/2160).
    pub max_resolution: u16,
    /// `ALLOW_RESOLUTION_DOWNGRADE`.
    pub allow_resolution_downgrade: bool,
}

/// One planned slot change: `slot` moves from `from` (what it serves now) to `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRepoint {
    pub slot: String,
    pub from: SelectionEntry,
    pub to: SelectionEntry,
}

/// PURE planner. For every `selection` slot that serves a present, `Verified` owned copy, find the
/// best OTHER present `Verified` owned copy of the same title that supplies the slot — for an episode,
/// it lists the episode in `provides` and has an emittable video file whose FIRST episode code
/// (`vfs::episode_of_file`) is that episode, so a multi-episode file is not a target for its later
/// episodes (conservative: the VFS keeps such a file visible while any slot selects it); for a
/// movie, it has an emittable video file — and plan a repoint when the rank of the file that copy
/// would serve is STRICTLY higher than the rank of the file the slot serves now. Both are ranked
/// per served file (`served_rank`): the record's [`crate::release::quality_rank`] (resolution,
/// then source tier, then checked before unverified; score ignored, so equal-quality copies never
/// churn a slot), capped by the resolution and source tier the file's own name tags, else its
/// nearest directory that tags one, and counted checked only for an MKV/MP4 file, so a
/// mixed-quality pack whose record reflects its best file never takes a slot with a worse one,
/// whether the file's name or its season folder shows that. Among several better copies the
/// highest rank wins, then score, then the smallest hash. A slot whose served copy is absent,
/// unsettled or unowned is left alone (the VFS already falls back for a stale slot), and a slot is
/// never created. `present` is the provider listing's identified torrents with their files.
/// Two exceptions keep the planner from undoing the upgrade engine (`rules`): a slot serving a
/// checked copy moves onto an unchecked one only for a strictly higher resolution, and with
/// `ALLOW_RESOLUTION_DOWNGRADE` no slot moves onto a copy above `MAX_RESOLUTION`. A movie slot
/// never moves onto a multi-film collection (`tasks::is_multi_feature_collection`), and one that
/// serves a collection stays: the VFS shows every selected film of the slot's torrent, so moving it
/// off would hide the collection's other films — dedup and the upgrade engine leave such titles
/// alone too. And a copy tagged only with other audio languages never takes a slot:
/// `release::wrong_audio_language` on the copy's torrent name, file name and the directories above
/// the file (a pack may tag a season's language on its folder only), under `prefs` with
/// `AUDIO_LANGUAGE` resolved against the title's recorded languages (the served record's, else any
/// owned record of the title's) — the test the upgrade engine applies before it stages a release,
/// and dedup before it counts one copy as making another redundant ([`copy_tags_wrong_audio`]).
/// Mirror copies are never probed, so a better-ranked wrong-language copy would otherwise take
/// every slot it provides. Under the default `AUDIO_LANGUAGE=original` the test is inert for a title
/// with no recorded language, as it is for acquisition; an explicit `AUDIO_LANGUAGE` applies it to
/// every title.
/// Every repoint moves a slot strictly up in rank, so repeated passes converge.
pub fn plan_selection_repoints(
    owned: &[(String, OwnedRecord)],
    selection: &[(String, SelectionEntry)],
    present: &[&TorrentInfo],
    rules: RepointRules,
    prefs: &crate::config::QualityPrefs,
) -> Vec<SlotRepoint> {
    use crate::scraper::MediaKind;
    use std::collections::HashMap;

    let present_by_hash: HashMap<String, &TorrentInfo> = present
        .iter()
        .map(|t| (t.hash.to_ascii_lowercase(), *t))
        .collect();
    // Settled, present owned copies, indexed by hash and grouped per title.
    let settled: HashMap<String, &OwnedRecord> = owned
        .iter()
        .filter(|(h, r)| {
            r.status == OwnedStatus::Verified
                && present_by_hash.contains_key(&h.to_ascii_lowercase())
        })
        .map(|(h, r)| (h.to_ascii_lowercase(), r))
        .collect();
    let mut by_title: HashMap<(bool, u64), Vec<(&str, &OwnedRecord)>> = HashMap::new();
    for (h, r) in &settled {
        let is_movie = r.request.kind == MediaKind::Movie;
        by_title
            .entry((is_movie, r.request.tmdb_id))
            .or_default()
            .push((h.as_str(), *r));
    }
    let title_langs = title_language_requests(owned);
    // The file a copy would serve for a slot: the largest emittable video file — for an episode,
    // among those whose first episode code is that episode. Each candidate's files are indexed once
    // per plan, on its first lookup, so a pack offered for many slots is scanned and parsed once
    // rather than once per slot.
    let mut file_index: HashMap<String, FileIndex> = HashMap::new();
    let mut file_for = |hash: &str, episode: Option<(u32, u32)>| -> Option<String> {
        let t = present_by_hash.get(hash)?;
        if !file_index.contains_key(hash) {
            file_index.insert(hash.to_string(), FileIndex::of(t));
        }
        file_index[hash].file_for(episode).map(|f| f.path.clone())
    };

    let mut out = Vec::new();
    for (slot, entry) in selection {
        let Some(SlotKey {
            is_movie,
            tmdb,
            episode,
        }) = parse_slot(slot)
        else {
            continue;
        };
        let current = entry.hash.to_ascii_lowercase();
        let Some(cur) = settled.get(&current) else {
            continue; // absent, unsettled or unowned: not this pass's business
        };
        if (cur.request.kind == MediaKind::Movie) != is_movie || cur.request.tmdb_id != tmdb {
            continue;
        }
        // A movie slot serving a multi-film collection stays: the VFS shows every selected film of
        // the slot's torrent, so moving it off would hide the collection's other films (dedup and
        // the upgrade engine leave such a title alone for the same reason).
        if is_movie
            && present_by_hash
                .get(&current)
                .is_some_and(|t| crate::tasks::is_multi_feature_collection(t))
        {
            continue;
        }
        let group = by_title.get(&(is_movie, tmdb));
        // A title held once has nothing to move to (skip the parses below).
        if group.is_none_or(|g| g.iter().all(|(h, _)| *h == current)) {
            continue;
        }
        let cur_rank = served_rank(cur.quality.as_ref(), &entry.file_path);
        // The title's audio requirement, resolved as acquisition resolves it: from the served
        // record's languages, else from any record of the title that has them.
        let audio = title_audio_prefs(
            prefs,
            &cur.request,
            title_langs.get(&(is_movie, tmdb)).copied(),
        );
        let wrong_language = |hash: &str, path: &str| -> bool {
            present_by_hash
                .get(hash)
                .is_some_and(|t| copy_tags_wrong_audio(t, path, &audio))
        };
        // Never undo what the upgrade engine adopts. Its unverified rule swaps an unchecked copy for
        // a checked one of a possibly LOWER tier at the same resolution, so a checked slot moves onto
        // an unchecked copy only for a strictly higher resolution. With downgrades allowed it moves
        // slots off copies above the ceiling, so no slot moves onto one.
        let allowed = |rank: crate::release::QualityRank| {
            unchecked_may_displace(rank, cur_rank)
                && !(rules.allow_resolution_downgrade && rank.resolution > rules.max_resolution)
        };
        // A movie copy that is a multi-film collection never takes a slot: its largest film (the
        // file it would serve) need not be this film, and the VFS would show all its films here.
        let collection = |hash: &str| {
            present_by_hash
                .get(hash)
                .is_some_and(|t| crate::tasks::is_multi_feature_collection(t))
        };
        let best = group
            .into_iter()
            .flatten()
            // The record-rank test is exact and cheap, so it runs before the file lookup and the
            // parse: `served_rank` only ever lowers a record's axes, so a copy whose record does not
            // outrank the served file can't serve a file that does.
            .filter(|(h, r)| {
                *h != current
                    && episode.is_none_or(|ep| r.provides.contains(&ep))
                    && crate::release::quality_rank(r.quality.as_ref()) > cur_rank
                    && !(is_movie && collection(h))
            })
            .filter_map(|(h, r)| file_for(h, episode).map(|path| (*h, *r, path)))
            .map(|(h, r, path)| (h, r, served_rank(r.quality.as_ref(), &path), path))
            .filter(|(h, _, rank, path)| {
                *rank > cur_rank && allowed(*rank) && !wrong_language(h, path)
            })
            .max_by(|(ha, ra, rank_a, _), (hb, rb, rank_b, _)| {
                let score =
                    |r: &OwnedRecord| r.quality.as_ref().map(|q| q.score).unwrap_or(i64::MIN);
                // Highest rank, then score; on a full tie the SMALLER hash wins.
                (rank_a, score(ra))
                    .cmp(&(rank_b, score(rb)))
                    .then_with(|| hb.cmp(ha))
            });
        if let Some((hash, _, _, path)) = best {
            out.push(SlotRepoint {
                slot: slot.clone(),
                from: entry.clone(),
                to: SelectionEntry {
                    hash: hash.to_string(),
                    file_path: path,
                },
            });
        }
    }
    out.sort_by(|a, b| a.slot.cmp(&b.slot));
    out
}

/// Per title (`(is_movie, tmdb_id)`), the request of the first owned record (present or not) that
/// records the title's languages (an `original_language`): a mirror record lacks them until the
/// upgrade pass backfills them, so the title's other records stand in for it.
pub(crate) fn title_language_requests(
    owned: &[(String, OwnedRecord)],
) -> std::collections::HashMap<(bool, u64), &crate::store::AcquireRequest> {
    let mut out = std::collections::HashMap::new();
    for (_, r) in owned {
        if r.request.original_language.is_some() {
            out.entry((
                r.request.kind == crate::scraper::MediaKind::Movie,
                r.request.tmdb_id,
            ))
            .or_insert(&r.request);
        }
    }
    out
}

/// `prefs` with `AUDIO_LANGUAGE` resolved for a title as acquisition resolves it
/// (`QualityPrefs::with_resolved_audio`): from `request`'s recorded languages when it has an
/// original language, else from `fallback`'s (another record of the title,
/// [`title_language_requests`]). With neither, the default `AUDIO_LANGUAGE=original` stays
/// unresolved and [`copy_tags_wrong_audio`] flags nothing.
pub(crate) fn title_audio_prefs(
    prefs: &crate::config::QualityPrefs,
    request: &crate::store::AcquireRequest,
    fallback: Option<&crate::store::AcquireRequest>,
) -> crate::config::QualityPrefs {
    let languages = if request.original_language.is_some() {
        request
    } else {
        fallback.unwrap_or(request)
    };
    prefs.with_resolved_audio(
        languages.original_language.as_deref(),
        languages.spoken_languages.as_deref().unwrap_or_default(),
    )
}

/// Whether the file `file_path` of the present copy `torrent` is tagged only with audio languages
/// other than the title's: [`crate::release::wrong_audio_language`] under `audio` (the prefs
/// resolved for the title, [`title_audio_prefs`]) on the copy's torrent name, the file name and
/// every directory above it, one per line so no tag runs across a `/` (a pack may tag a season's
/// language on its folder only). Mirror copies are never probed, so the tags are all there is. The
/// selection planner never moves a slot onto such a file, and dedup never lets it make a
/// right-language copy redundant.
pub(crate) fn copy_tags_wrong_audio(
    torrent: &TorrentInfo,
    file_path: &str,
    audio: &crate::config::QualityPrefs,
) -> bool {
    if audio.audio == crate::config::AudioReq::Original {
        return false; // unresolved: `wrong_audio_language` never applies (skip the parse)
    }
    let name = if torrent.filename.is_empty() {
        &torrent.original_filename
    } else {
        &torrent.filename
    };
    let base = file_path.rsplit('/').next().unwrap_or(file_path);
    let components: Vec<&str> = file_path.split('/').filter(|c| !c.is_empty()).collect();
    let release = crate::release::parse(&crate::release::RawCandidate {
        name: name.clone(),
        description: components.join("\n"),
        info_hash: torrent.hash.to_ascii_lowercase(),
        file_idx: None,
        file_name: Some(base.to_string()),
    });
    crate::release::wrong_audio_language(&release, audio)
}

/// A present copy's emittable video files (`vfs::emittable_video_files`) as
/// [`plan_selection_repoints`] looks them up: the largest one overall (what a movie copy serves),
/// and per FIRST episode code (`vfs::episode_of_file`) the largest one carrying it (what an episode
/// copy serves). Of equally large files the LAST in provider order wins, as `Iterator::max_by_key`
/// picks.
struct FileIndex<'a> {
    largest: Option<&'a crate::rd_client::TorrentFile>,
    by_episode: std::collections::HashMap<(u32, u32), &'a crate::rd_client::TorrentFile>,
}

impl<'a> FileIndex<'a> {
    fn of(t: &'a TorrentInfo) -> Self {
        let mut index = FileIndex {
            largest: None,
            by_episode: std::collections::HashMap::new(),
        };
        for f in crate::vfs::emittable_video_files(t) {
            if index.largest.is_none_or(|l| f.bytes >= l.bytes) {
                index.largest = Some(f);
            }
            if let Some(ep) = crate::vfs::episode_of_file(&f.path) {
                let best = index.by_episode.entry(ep).or_insert(f);
                if f.bytes >= best.bytes {
                    *best = f;
                }
            }
        }
        index
    }

    /// The file this copy serves for a slot: `episode`'s, or for a movie (`None`) the largest.
    fn file_for(&self, episode: Option<(u32, u32)>) -> Option<&'a crate::rd_client::TorrentFile> {
        match episode {
            None => self.largest,
            Some(ep) => self.by_episode.get(&ep).copied(),
        }
    }
}

/// The [`QualityRank`](crate::release::QualityRank) of the file a copy serves for a slot: its
/// record's [`crate::release::quality_rank`], lowered on each axis to what the file's name, or
/// failing that its folders, show. A record holds ONE quality for the whole torrent (an
/// account-mirror record's comes from its largest file, `tasks::mirror_quality`), so a
/// mixed-quality pack — a "complete series" with 1080p BluRay MKVs and a 480p DVDRip `.avi` — would
/// otherwise serve every episode at its best file's rank. A resolution or source tier the file name
/// tags caps the record's. An axis the file name leaves untagged is taken from the nearest
/// directory that tags it: a repackaged pack often tags only its season folders
/// (`Pack.1080p.BluRay/Season 5 [480p DVDRip]/Show - S05E01.avi`). When no directory tags it
/// either, the record's stands, so an engine release with untagged episode files keeps its scraped
/// quality. A standard-definition source tag ([`sd_hints`]: DVD, a DVD remux included, PDTV, SDTV,
/// DSR, TVRip and VHSRip, plus HDTV and the XviD/DivX codecs, which scene naming leaves untagged
/// only on an SD release) stands for at most 576p unless the same name tags a resolution, so a
/// scene `show.s05e01.dvdrip.xvid-grp.avi` or `show.s01e01.hdtv.xvid-lol.avi` doesn't serve at its
/// pack record's 1080p BluRay. A DVD, PDTV, SDTV, DSR, TVRip or VHSRip tag also stands for the
/// lowest source tier (`release::parse` reads most of them as no source); HDTV keeps its tier, and
/// XviD/DivX tag none. A directory's cam word is not read as a cam source, since in a
/// folder it is far likelier a title word (`Body Cam/Season 1/`). The file counts as checked only
/// when the record is and the file is MKV/MP4 (the containers the probe reads). Applied alike to
/// the served copy and to every candidate, so a repoint still strictly raises the slot's rank and
/// repeated passes converge. It never exceeds the record's rank on any axis, so never
/// lexicographically either.
pub(crate) fn served_rank(
    record: Option<&crate::release::QualitySummary>,
    file_path: &str,
) -> crate::release::QualityRank {
    use crate::release::{quality_rank, QualityRank, Source};
    let rank = quality_rank(record);
    let tags = |name: &str, file_name: Option<&str>| {
        crate::release::parse(&crate::release::RawCandidate {
            name: name.to_string(),
            description: String::new(),
            info_hash: String::new(),
            file_idx: None,
            file_name: file_name.map(str::to_string),
        })
    };
    // What one path component tags: its resolution and its source tier, `None` for an axis it
    // leaves untagged. An SD source tag stands for an SD resolution, and some for the lowest tier.
    let component = |name: &str, is_file: bool| {
        let info = tags(name, is_file.then_some(name));
        let sd = sd_hints(name);
        let resolution = info
            .resolution
            .or(sd.resolution.then_some(DVD_MAX_RESOLUTION));
        let tier = match info.source {
            _ if sd.lowest_tier => Some(0),
            Source::Other => None,
            // A directory's cam word is a title word, and so is its bare `web` word when nothing
            // else in its name is a release tag (`Web of Lies/`).
            Source::Cam if !is_file => None,
            Source::Web if !is_file && crate::release::web_source_is_a_title_word(name) => None,
            source => Some(source.tier_score()),
        };
        (info, resolution, tier)
    };
    let mut components = file_path.rsplit('/');
    let base = components.next().unwrap_or(file_path);
    let (file, mut resolution, mut tier) = component(base, true);
    for dir in components.filter(|c| !c.is_empty()) {
        if resolution.is_some() && tier.is_some() {
            break;
        }
        let (_, dir_resolution, dir_tier) = component(dir, false);
        resolution = resolution.or(dir_resolution);
        tier = tier.or(dir_tier);
    }
    QualityRank {
        resolution: resolution.map_or(rank.resolution, |r| r.min(rank.resolution)),
        source_tier: tier.map_or(rank.source_tier, |t| t.min(rank.source_tier)),
        checked: rank.checked && file.container.is_verifiable(),
    }
}

/// The highest resolution a DVD holds (PAL; NTSC is 480), and the cap [`served_rank`] puts on any
/// standard-definition source that tags no resolution.
const DVD_MAX_RESOLUTION: u16 = 576;

/// What one path component's name says about a standard-definition source ([`sd_hints`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SdHints {
    /// It names a source that is standard definition when no resolution is tagged: by scene
    /// convention an `HDTV` or `XviD`/`DivX` release with no resolution tag is SD, and a DVD, PDTV,
    /// SDTV, DSR, TVRip or VHSRip one is SD whatever it says.
    resolution: bool,
    /// It names a source that is never better than the lowest tier (0): DVD (`DVDRip`, `DVD-Rip`,
    /// `DVD`, `DVDR`, `DVD5`, `DVD9`, a DVD remux), PDTV, SDTV, DSR/DSRip, TVRip, VHSRip.
    /// `release::parse` reads most of these as no source at all.
    lowest_tier: bool,
}

/// The standard-definition hints of `name` ([`SdHints`]), read from its alphanumeric tokens, so a
/// tag counts only as a whole token (`MyDVDCollection` tags nothing) and two-token spellings
/// (`DVD-Rip`, `TV-Rip`) read like the glued ones. A DVD or TVRip token right after an `HD` token
/// is HD DVD (`HD-DVD`, `HD.DVD`, `HD-DVDRip`), high definition and no hint; a glued `HDDVD` is no
/// DVD token either.
fn sd_hints(name: &str) -> SdHints {
    let lower = name.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let mut hints = SdHints::default();
    for (i, &token) in tokens.iter().enumerate() {
        let after_hd = i > 0 && tokens[i - 1] == "hd";
        let next = tokens.get(i + 1).copied();
        let lowest = match token {
            "dvd" | "dvdrip" | "dvdr" | "dvd5" | "dvd9" | "dvdremux" | "tvrip" => !after_hd,
            "tv" | "vhs" => next == Some("rip") && !after_hd,
            "pdtv" | "sdtv" | "dsr" | "dsrip" | "vhsrip" => true,
            _ => false,
        };
        hints.lowest_tier |= lowest;
        hints.resolution |= lowest || matches!(token, "hdtv" | "hdtvrip" | "xvid" | "divx");
    }
    hints
}

/// Whether a copy of rank `new` may take a slot from one of rank `cur` as far as checking goes: a
/// slot serving a checked copy moves onto an unchecked one only for a strictly higher resolution
/// (the upgrade engine's unverified rule swaps an unchecked copy for a checked one of a possibly
/// lower tier at the same resolution, which must not be undone).
fn unchecked_may_displace(
    new: crate::release::QualityRank,
    cur: crate::release::QualityRank,
) -> bool {
    new.checked || !cur.checked || new.resolution > cur.resolution
}

/// Whether a copy may take an episode slot from the copy serving it, each given as its record's
/// quality and the file it would serve: the [`served_rank`] of its file is higher (subject to
/// [`unchecked_may_displace`]), or equal with a score at least as high. `observe`'s gate for a newly
/// verified pack meeting a slot another owned release serves. [`plan_selection_repoints`] moves a
/// slot only on a STRICTLY higher served rank under the same rules, so it never moves a slot back
/// from what this gate chose.
pub(crate) fn takes_slot_from(
    new: (Option<&crate::release::QualitySummary>, &str),
    cur: (Option<&crate::release::QualitySummary>, &str),
) -> bool {
    let new_rank = served_rank(new.0, new.1);
    let cur_rank = served_rank(cur.0, cur.1);
    let score = |q: Option<&crate::release::QualitySummary>| q.map(|q| q.score).unwrap_or(i64::MIN);
    unchecked_may_displace(new_rank, cur_rank)
        && (new_rank > cur_rank || (new_rank == cur_rank && score(new.0) >= score(cur.0)))
}

/// A parsed slot key ([`crate::store::movie_slot`] / [`crate::store::episode_slot`]).
pub(crate) struct SlotKey {
    pub(crate) is_movie: bool,
    pub(crate) tmdb: u64,
    /// `None` for a movie slot.
    pub(crate) episode: Option<(u32, u32)>,
}

/// Parse a slot key, or `None` for anything else.
pub(crate) fn parse_slot(slot: &str) -> Option<SlotKey> {
    let mut parts = slot.split('|');
    match (
        parts.next()?,
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        ("m", Some(id), None, None, None) => Some(SlotKey {
            is_movie: true,
            tmdb: id.parse().ok()?,
            episode: None,
        }),
        ("e", Some(id), Some(s), Some(e), None) => Some(SlotKey {
            is_movie: false,
            tmdb: id.parse().ok()?,
            episode: Some((s.parse().ok()?, e.parse().ok()?)),
        }),
        _ => None,
    }
}

/// Plan [`plan_selection_repoints`] over the current library and, when the library is idle (a
/// repoint changes which file an episode shows, so it would cut off a stream of that episode), apply
/// it. Each repoint is a compare-and-set ([`Store::repoint_selection_if`]), so a slot the upgrade
/// engine swapped or dedup cleared since the plan is left alone. Torrents hidden by repair are not
/// candidates, and a wholesale-empty listing plans nothing. Returns how many slots moved; the VFS
/// shows the change on the next scan tick.
pub async fn repoint_to_better_copies(
    store: &Store,
    read_activity: &ReadActivity,
    idle_window: Duration,
    hidden_ids: &HashSet<String>,
    current: &[(TorrentInfo, MediaMetadata)],
    rules: RepointRules,
    prefs: &crate::config::QualityPrefs,
) -> usize {
    let present: Vec<&TorrentInfo> = current
        .iter()
        .map(|(info, _)| info)
        .filter(|info| !hidden_ids.contains(&info.id))
        .collect();
    if present.is_empty() {
        return 0;
    }
    let owned = store.all_owned().await;
    let selection = store.all_selection().await;
    let plan = plan_selection_repoints(&owned, &selection, &present, rules, prefs);
    if plan.is_empty() {
        return 0;
    }
    if !read_activity.all_idle(idle_window).await {
        debug!(
            "selection: {} slot(s) could serve a better copy — library not idle, deferring",
            plan.len()
        );
        return 0;
    }
    let short = |h: &str| h.chars().take(8).collect::<String>();
    let mut moved = 0usize;
    for r in &plan {
        match store
            .repoint_selection_if(r.slot.clone(), r.from.clone(), r.to.clone())
            .await
        {
            Ok(true) => {
                moved += 1;
                info!(
                    "selection: {} now served from {} ({}) instead of {} — a better copy is present",
                    r.slot,
                    short(&r.to.hash),
                    r.to.file_path.rsplit('/').next().unwrap_or(&r.to.file_path),
                    short(&r.from.hash)
                );
            }
            Ok(false) => debug!("selection: {} changed since planning — left alone", r.slot),
            Err(e) => warn!("selection: repointing {} failed: {}", r.slot, e),
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rd_client::TorrentFile;
    use crate::release::QualitySummary;
    use crate::scraper::MediaKind;
    use crate::store::{episode_slot, movie_slot, AcquireRequest, Provenance};
    use crate::vfs::{MediaMetadata, MediaType};

    /// The default rules: `ALLOW_RESOLUTION_DOWNGRADE` off, so the ceiling plays no part.
    const OFF: RepointRules = RepointRules {
        max_resolution: 1080,
        allow_resolution_downgrade: false,
    };

    /// The default acquisition prefs: `AUDIO_LANGUAGE=original`, resolved per title.
    fn prefs() -> crate::config::QualityPrefs {
        crate::config::QualityPrefs {
            max_resolution: crate::config::MaxResolution::P1080,
            audio: crate::config::AudioReq::Original,
            subtitle: crate::config::SubReq::None,
            prefer_hevc: true,
            prefer_hdr: false,
        }
    }

    fn q(resolution: u16, source_tier: i64, score: i64) -> Option<QualitySummary> {
        Some(QualitySummary {
            cached: true,
            source_tier,
            resolution,
            score,
            unverified: false,
        })
    }

    fn rec(
        kind: MediaKind,
        tmdb: u64,
        provides: Vec<(u32, u32)>,
        quality: Option<QualitySummary>,
    ) -> OwnedRecord {
        OwnedRecord {
            request: AcquireRequest {
                imdb_id: String::new(),
                tmdb_id: tmdb,
                kind,
                season: None,
                episode: None,
                original_language: None,
                spoken_languages: Some(vec![]),
                metadata: MediaMetadata {
                    title: "T".into(),
                    year: None,
                    media_type: if kind == MediaKind::Movie {
                        MediaType::Movie
                    } else {
                        MediaType::Show
                    },
                    external_id: Some(format!("tmdb:{tmdb}")),
                },
            },
            provenance: Provenance { entries: vec![] },
            added_at: 1,
            status: OwnedStatus::Verified,
            provides,
            quality,
        }
    }

    fn info(hash: &str, files: &[(&str, u64)]) -> TorrentInfo {
        TorrentInfo {
            hash: hash.into(),
            status: "downloaded".into(),
            files: files
                .iter()
                .enumerate()
                .map(|(i, (path, bytes))| TorrentFile {
                    id: i as u32,
                    path: (*path).into(),
                    bytes: *bytes,
                    selected: 1,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn sel(hash: &str, path: &str) -> SelectionEntry {
        SelectionEntry {
            hash: hash.into(),
            file_path: path.into(),
        }
    }

    fn eps(season: u32, n: u32) -> Vec<(u32, u32)> {
        (1..=n).map(|e| (season, e)).collect()
    }

    /// The planner's cheap pre-filter (a copy whose record rank doesn't beat the served file's is
    /// skipped before its file is looked up and parsed) is exact only because a file's served rank
    /// never exceeds its record's rank.
    #[test]
    fn served_rank_never_exceeds_the_record_rank() {
        let unverified = Some(QualitySummary {
            unverified: true,
            ..q(1080, 6_000, 5).unwrap()
        });
        let qualities = [
            None,
            q(1080, 6_000, 5),
            q(720, 0, 5),
            q(0, 8_000, 5),
            q(2160, 8_000, 5), // over a 1080p ceiling
            unverified,
        ];
        let files = [
            "Show.S01E01.2160p.REMUX.mkv",
            "Show.S01E01.480p.DVDRip.XviD.avi",
            "Show.S01E01.mkv",
            "Show.S01E01.avi",
            "Movie.2020.1080p.WEB-DL.mp4",
            "Pack.2160p.REMUX/Season 1 [480p DVDRip]/Show - S01E01.avi",
            "/Pack/Season 2 [2160p BluRay]/Show - S02E01.mkv",
            SCENE_DVDRIP_S05E01,
            SCENE_HDTV_S05E01,
            SCENE_PDTV_S05E01,
            "Show.S01E01.DVD.REMUX.mkv",
            "Pack/Season 3 [DVD9]/Show - S03E01.mkv",
        ];
        for quality in &qualities {
            for file in files {
                let served = served_rank(quality.as_ref(), file);
                let record = crate::release::quality_rank(quality.as_ref());
                assert!(
                    served <= record,
                    "{quality:?} {file}: {served:?} > {record:?}"
                );
            }
        }
    }

    /// `observe`'s pack gate ranks like the planner: a higher-scoring pack of a lower source tier
    /// (the HEVC bonus outweighs a tier step) or an unchecked one never takes a better single's
    /// slot, so the planner never has to move the slot back.
    #[test]
    fn takes_slot_from_ranks_by_quality_before_score() {
        let single = q(1080, 8_000, 1_000_000); // a REMUX x264 single
        let bluray_hevc_pack = q(1080, 6_000, 1_100_000);
        assert!(!takes_slot_from(
            (
                bluray_hevc_pack.as_ref(),
                "P/Show.S01E01.1080p.BluRay.x265.mkv"
            ),
            (single.as_ref(), "Show.S01E01.1080p.REMUX.mkv"),
        ));
        let unchecked_pack = Some(QualitySummary {
            unverified: true,
            ..q(1080, 8_000, 2_000_000).unwrap()
        });
        assert!(!takes_slot_from(
            (unchecked_pack.as_ref(), "P/Show.S01E01.mkv"),
            (single.as_ref(), "Show.S01E01.mkv"),
        ));
        // A higher rank takes it; at equal rank the score decides (ties go to the newcomer).
        assert!(takes_slot_from(
            (q(2160, 3_000, 1).as_ref(), "P/Show.S01E01.mkv"),
            (single.as_ref(), "Show.S01E01.mkv"),
        ));
        assert!(takes_slot_from(
            (q(1080, 8_000, 1_000_000).as_ref(), "P/Show.S01E01.mkv"),
            (single.as_ref(), "Show.S01E01.mkv"),
        ));
        assert!(!takes_slot_from(
            (q(1080, 8_000, 999_999).as_ref(), "P/Show.S01E01.mkv"),
            (single.as_ref(), "Show.S01E01.mkv"),
        ));
        // The file the pack would serve counts, not its best one.
        assert!(!takes_slot_from(
            (
                q(1080, 8_000, 2_000_000).as_ref(),
                "P/Show.S01E01.480p.DVDRip.XviD.avi"
            ),
            (single.as_ref(), "Show.S01E01.mkv"),
        ));
    }

    /// The motivating case: every slot serves the S01–S02 BluRay pack, and a REMUX copy of S01
    /// (a pre-existing / re-added copy, so it has no slots) is present. S01's slots move to the REMUX
    /// files; S02 (which only the pack has) stays.
    #[test]
    fn a_better_present_copy_takes_over_the_slots_it_supplies() {
        let mut pack_eps = eps(1, 2);
        pack_eps.extend(eps(2, 1));
        let owned = vec![
            (
                "pack".to_string(),
                rec(MediaKind::Series, 7, pack_eps, q(1080, 6_000, 1_100_000)),
            ),
            (
                "remux".to_string(),
                rec(MediaKind::Series, 7, eps(1, 2), q(1080, 8_000, 1_090_000)),
            ),
        ];
        let pack = info(
            "pack",
            &[
                ("P/Show.S01E01.1080p.BluRay.mkv", 2),
                ("P/Show.S01E02.1080p.BluRay.mkv", 2),
                ("P/Show.S02E01.1080p.BluRay.mkv", 2),
            ],
        );
        let remux = info(
            "remux",
            &[
                ("R/Show.S01E01.1080p.REMUX.mkv", 9),
                ("R/Show.S01E02.1080p.REMUX.mkv", 9),
                ("R/sample.txt", 1),
            ],
        );
        let selection = vec![
            (
                episode_slot(7, 1, 1),
                sel("pack", "P/Show.S01E01.1080p.BluRay.mkv"),
            ),
            (
                episode_slot(7, 1, 2),
                sel("pack", "P/Show.S01E02.1080p.BluRay.mkv"),
            ),
            (
                episode_slot(7, 2, 1),
                sel("pack", "P/Show.S02E01.1080p.BluRay.mkv"),
            ),
        ];
        let plan = plan_selection_repoints(&owned, &selection, &[&pack, &remux], OFF, &prefs());
        assert_eq!(
            plan,
            vec![
                SlotRepoint {
                    slot: episode_slot(7, 1, 1),
                    from: sel("pack", "P/Show.S01E01.1080p.BluRay.mkv"),
                    to: sel("remux", "R/Show.S01E01.1080p.REMUX.mkv"),
                },
                SlotRepoint {
                    slot: episode_slot(7, 1, 2),
                    from: sel("pack", "P/Show.S01E02.1080p.BluRay.mkv"),
                    to: sel("remux", "R/Show.S01E02.1080p.REMUX.mkv"),
                },
            ]
        );
        // Re-planning after the repoint changes nothing (converges).
        let after: Vec<_> = selection
            .iter()
            .map(|(slot, e)| {
                plan.iter()
                    .find(|r| &r.slot == slot)
                    .map(|r| (slot.clone(), r.to.clone()))
                    .unwrap_or_else(|| (slot.clone(), e.clone()))
            })
            .collect();
        assert!(
            plan_selection_repoints(&owned, &after, &[&pack, &remux], OFF, &prefs()).is_empty()
        );
    }

    /// Only a STRICTLY better rank moves a slot: an equal-quality copy (even one with a higher
    /// score) and a worse one leave it alone.
    #[test]
    fn equal_or_worse_copies_never_move_a_slot() {
        for other in [
            q(1080, 6_000, 9_999_999), // same rank, higher score
            q(720, 8_000, 9_999_999),  // lower resolution
            q(1080, 0, 9_999_999),     // same resolution, untagged source
            None,                      // no recorded quality
        ] {
            let owned = vec![
                (
                    "cur".to_string(),
                    rec(MediaKind::Series, 7, eps(1, 1), q(1080, 6_000, 1)),
                ),
                (
                    "other".to_string(),
                    rec(MediaKind::Series, 7, eps(1, 1), other.clone()),
                ),
            ];
            let cur = info("cur", &[("Show.S01E01.mkv", 1)]);
            let alt = info("other", &[("Show.S01E01.other.mkv", 9)]);
            let selection = vec![(episode_slot(7, 1, 1), sel("cur", "Show.S01E01.mkv"))];
            assert!(
                plan_selection_repoints(&owned, &selection, &[&cur, &alt], OFF, &prefs())
                    .is_empty(),
                "{other:?} must not take the slot"
            );
        }
    }

    /// A checked copy beats an unverified one of the same resolution and tier (the rank's last axis).
    #[test]
    fn a_checked_copy_takes_a_slot_from_an_unverified_one_of_equal_quality() {
        let mut unchecked = q(1080, 3_000, 5);
        unchecked.as_mut().unwrap().unverified = true;
        let owned = vec![
            (
                "avi".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), unchecked),
            ),
            (
                "mkv".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 3_000, 1)),
            ),
        ];
        let avi = info("avi", &[("Show.S01E01.avi", 1)]);
        let mkv = info("mkv", &[("Show.S01E01.mkv", 1)]);
        let selection = vec![(episode_slot(7, 1, 1), sel("avi", "Show.S01E01.avi"))];
        let plan = plan_selection_repoints(&owned, &selection, &[&avi, &mkv], OFF, &prefs());
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].to, sel("mkv", "Show.S01E01.mkv"));
    }

    /// The best of several better copies wins: highest rank, then score, then the smallest hash.
    #[test]
    fn the_best_better_copy_wins() {
        let owned = vec![
            (
                "cur".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 3_000, 1)),
            ),
            (
                "bluray".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 6_000, 9)),
            ),
            (
                "zremux".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 8_000, 1)),
            ),
            (
                "aremux".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 8_000, 1)),
            ),
        ];
        let infos: Vec<TorrentInfo> = ["cur", "bluray", "zremux", "aremux"]
            .iter()
            .map(|h| info(h, &[(&format!("{h}/Show.S01E01.mkv"), 1)]))
            .collect();
        let refs: Vec<&TorrentInfo> = infos.iter().collect();
        let selection = vec![(episode_slot(7, 1, 1), sel("cur", "cur/Show.S01E01.mkv"))];
        let plan = plan_selection_repoints(&owned, &selection, &refs, OFF, &prefs());
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].to, sel("aremux", "aremux/Show.S01E01.mkv"));
    }

    /// A better copy must be settled, present, of the same title, and actually hold an emittable file
    /// for the slot's episode; the served copy must itself be present, settled and owned.
    #[test]
    fn only_a_settled_present_copy_with_the_episode_file_qualifies() {
        let base = || {
            vec![
                (
                    "cur".to_string(),
                    rec(MediaKind::Series, 7, eps(1, 2), q(720, 3_000, 1)),
                ),
                (
                    "better".to_string(),
                    rec(MediaKind::Series, 7, eps(1, 2), q(1080, 8_000, 1)),
                ),
            ]
        };
        let cur = info("cur", &[("Show.S01E01.mkv", 1), ("Show.S01E02.mkv", 1)]);
        let better = info(
            "better",
            &[("B/Show.S01E01.mkv", 5), ("B/Show.S01E02.mkv", 5)],
        );
        let selection = vec![(episode_slot(7, 1, 1), sel("cur", "Show.S01E01.mkv"))];
        assert_eq!(
            plan_selection_repoints(&base(), &selection, &[&cur, &better], OFF, &prefs()).len(),
            1,
            "control: the better copy takes the slot"
        );

        let mut pending = base();
        pending[1].1.status = OwnedStatus::Pending;
        assert!(
            plan_selection_repoints(&pending, &selection, &[&cur, &better], OFF, &prefs())
                .is_empty()
        );

        assert!(
            plan_selection_repoints(&base(), &selection, &[&cur], OFF, &prefs()).is_empty(),
            "an absent copy can't serve"
        );

        let mut other_title = base();
        other_title[1].1.request.tmdb_id = 8;
        assert!(
            plan_selection_repoints(&other_title, &selection, &[&cur, &better], OFF, &prefs())
                .is_empty()
        );

        let no_file = info("better", &[("B/Show.S01E02.mkv", 5)]);
        assert!(
            plan_selection_repoints(&base(), &selection, &[&cur, &no_file], OFF, &prefs())
                .is_empty(),
            "provides lists E01 but no file is E01"
        );
        let mut unselected = better.clone();
        unselected.files[0].selected = 0;
        assert!(
            plan_selection_repoints(&base(), &selection, &[&cur, &unselected], OFF, &prefs())
                .is_empty(),
            "a file the VFS wouldn't emit can't serve"
        );

        let mut not_provided = base();
        not_provided[1].1.provides = vec![(1, 2)];
        assert!(plan_selection_repoints(
            &not_provided,
            &selection,
            &[&cur, &better],
            OFF,
            &prefs()
        )
        .is_empty());

        let mut cur_pending = base();
        cur_pending[0].1.status = OwnedStatus::Pending;
        assert!(
            plan_selection_repoints(&cur_pending, &selection, &[&cur, &better], OFF, &prefs())
                .is_empty(),
            "a slot serving an unsettled copy is left to observe"
        );
        assert!(
            plan_selection_repoints(&base(), &selection, &[&better], OFF, &prefs()).is_empty(),
            "a slot serving an absent copy is left alone (the VFS already falls back)"
        );
    }

    /// A two-copy show where the better copy holds no slot, as the executor tests' fixture.
    async fn better_copy_fixture() -> (Store, Vec<(TorrentInfo, MediaMetadata)>) {
        let store = Store::in_memory();
        store
            .put_owned(
                "pack".into(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 6_000, 1)),
            )
            .await
            .unwrap();
        store
            .put_owned(
                "remux".into(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 8_000, 1)),
            )
            .await
            .unwrap();
        store
            .put_selection(episode_slot(7, 1, 1), sel("pack", "Show.S01E01.BluRay.mkv"))
            .await
            .unwrap();
        let meta = rec(MediaKind::Series, 7, vec![], None).request.metadata;
        let mut pack = info("pack", &[("Show.S01E01.BluRay.mkv", 1)]);
        pack.id = "id-pack".into();
        let mut remux = info("remux", &[("Show.S01E01.REMUX.mkv", 9)]);
        remux.id = "id-remux".into();
        (store, vec![(pack, meta.clone()), (remux, meta)])
    }

    #[tokio::test]
    async fn the_executor_repoints_when_idle() {
        let (store, current) = better_copy_fixture().await;
        let idle = ReadActivity::without_startup_grace();
        let moved = repoint_to_better_copies(
            &store,
            &idle,
            Duration::from_secs(300),
            &HashSet::new(),
            &current,
            OFF,
            &prefs(),
        )
        .await;
        assert_eq!(moved, 1);
        assert_eq!(
            store.get_selection(episode_slot(7, 1, 1)).await,
            Some(sel("remux", "Show.S01E01.REMUX.mkv"))
        );
    }

    /// Repointing changes the file an episode shows, so it waits until nothing is being read, and it
    /// never moves a slot onto a torrent repair has hidden.
    #[tokio::test]
    async fn the_executor_waits_for_an_idle_library_and_skips_hidden_torrents() {
        let (store, current) = better_copy_fixture().await;
        let busy = ReadActivity::without_startup_grace();
        busy.touch("pack").await;
        let window = Duration::from_secs(300);
        assert_eq!(
            repoint_to_better_copies(
                &store,
                &busy,
                window,
                &HashSet::new(),
                &current,
                OFF,
                &prefs()
            )
            .await,
            0
        );
        let idle = ReadActivity::without_startup_grace();
        let hidden: HashSet<String> = ["id-remux".to_string()].into_iter().collect();
        assert_eq!(
            repoint_to_better_copies(&store, &idle, window, &hidden, &current, OFF, &prefs()).await,
            0
        );
        assert_eq!(
            repoint_to_better_copies(&store, &idle, window, &HashSet::new(), &[], OFF, &prefs())
                .await,
            0,
            "an empty listing plans nothing"
        );
        assert_eq!(
            store.get_selection(episode_slot(7, 1, 1)).await,
            Some(sel("pack", "Show.S01E01.BluRay.mkv")),
            "the slot is unchanged"
        );
    }

    /// Movies: the slot moves to the better copy's largest emittable video file.
    #[test]
    fn a_movie_slot_moves_to_a_better_copy() {
        let owned = vec![
            (
                "web".to_string(),
                rec(MediaKind::Movie, 5, vec![], q(1080, 3_000, 1)),
            ),
            (
                "bluray".to_string(),
                rec(MediaKind::Movie, 5, vec![], q(1080, 6_000, 1)),
            ),
        ];
        let web = info("web", &[("Film.1080p.WEB.mkv", 3)]);
        let bluray = info(
            "bluray",
            &[("Film/sample.mkv", 1), ("Film/Film.1080p.BluRay.mkv", 8)],
        );
        let selection = vec![(movie_slot(5), sel("web", "Film.1080p.WEB.mkv"))];
        let plan = plan_selection_repoints(&owned, &selection, &[&web, &bluray], OFF, &prefs());
        assert_eq!(
            plan,
            vec![SlotRepoint {
                slot: movie_slot(5),
                from: sel("web", "Film.1080p.WEB.mkv"),
                to: sel("bluray", "Film/Film.1080p.BluRay.mkv"),
            }]
        );
    }

    /// The file a better copy serves for each slot, as the planner looks it up from the copy's
    /// per-plan file index: the largest emittable video file whose FIRST episode code is the slot's
    /// episode (the last of equally large ones, as `Iterator::max_by_key` picks), or for a movie
    /// the largest emittable video file. An episode the pack has no emittable file for (only a
    /// sample) is not repointed.
    #[test]
    fn a_pack_serving_several_slots_serves_each_its_own_largest_file() {
        let owned = vec![
            (
                "cur".to_string(),
                rec(MediaKind::Series, 7, eps(1, 4), q(720, 3_000, 1)),
            ),
            (
                "pack".to_string(),
                rec(MediaKind::Series, 7, eps(1, 4), q(1080, 8_000, 1)),
            ),
            (
                "web".to_string(),
                rec(MediaKind::Movie, 5, vec![], q(720, 3_000, 1)),
            ),
            (
                "bluray".to_string(),
                rec(MediaKind::Movie, 5, vec![], q(1080, 6_000, 1)),
            ),
        ];
        let cur = info(
            "cur",
            &[
                ("Show.S01E01.mkv", 1),
                ("Show.S01E02.mkv", 1),
                ("Show.S01E03.mkv", 1),
                ("Show.S01E04.mkv", 1),
            ],
        );
        let pack = info(
            "pack",
            &[
                ("P/a/Show.S01E01.mkv", 5),
                ("P/b/Show.S01E01.mkv", 5),
                ("P/Show.S01E02E03.mkv", 9),
                ("P/Show.S01E02.mkv", 4),
                ("P/Show.S01E03.mkv", 6),
                ("P/Sample/Show.S01E04.mkv", 3),
            ],
        );
        let web = info("web", &[("Film.720p.WEB.mkv", 3)]);
        let bluray = info(
            "bluray",
            &[("Film/Film.Part.A.mkv", 8), ("Film/Film.Part.B.mkv", 8)],
        );
        let selection: Vec<_> = (1..=4)
            .map(|e| {
                (
                    episode_slot(7, 1, e),
                    sel("cur", &format!("Show.S01E0{e}.mkv")),
                )
            })
            .chain([(movie_slot(5), sel("web", "Film.720p.WEB.mkv"))])
            .collect();
        let plan = plan_selection_repoints(
            &owned,
            &selection,
            &[&cur, &pack, &web, &bluray],
            OFF,
            &prefs(),
        );
        let to: Vec<_> = plan
            .iter()
            .map(|r| (r.slot.clone(), r.to.clone()))
            .collect();
        let mut expected = vec![
            (episode_slot(7, 1, 1), sel("pack", "P/b/Show.S01E01.mkv")),
            (episode_slot(7, 1, 2), sel("pack", "P/Show.S01E02E03.mkv")),
            (episode_slot(7, 1, 3), sel("pack", "P/Show.S01E03.mkv")),
            (movie_slot(5), sel("bluray", "Film/Film.Part.B.mkv")),
        ];
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(to, expected);
    }

    /// A slot never moves from a checked copy onto an unchecked one at the same resolution, even of
    /// a higher tier: the upgrade engine's unverified rule deliberately swaps an unchecked copy for a
    /// checked one of a lower tier (e.g. an unverified BluRay `.avi` pack -> a checked WEB x265 MKV),
    /// and the planner must not move the slot straight back. A strictly higher resolution still wins.
    #[test]
    fn a_checked_slot_is_not_moved_onto_an_unchecked_copy_at_the_same_resolution() {
        let unchecked_pack = |resolution: u16| {
            let mut quality = q(resolution, 6_000, 9);
            quality.as_mut().unwrap().unverified = true;
            rec(MediaKind::Series, 7, eps(1, 2), quality)
        };
        let single = rec(MediaKind::Series, 7, eps(1, 1), q(1080, 3_000, 9));
        let pack = info(
            "pack",
            &[("P/Show.S01E01.avi", 5), ("P/Show.S01E02.avi", 5)],
        );
        let web = info("web", &[("Show.S01E01.WEB.x265.mkv", 2)]);
        let selection = vec![(
            episode_slot(7, 1, 1),
            sel("web", "Show.S01E01.WEB.x265.mkv"),
        )];

        let owned = vec![
            ("pack".to_string(), unchecked_pack(1080)),
            ("web".to_string(), single.clone()),
        ];
        assert!(
            plan_selection_repoints(&owned, &selection, &[&pack, &web], OFF, &prefs()).is_empty(),
            "a checked 1080p copy keeps its slot against an unchecked 1080p pack of a higher tier"
        );

        let owned = vec![
            ("pack".to_string(), unchecked_pack(2160)),
            ("web".to_string(), single),
        ];
        let plan = plan_selection_repoints(&owned, &selection, &[&pack, &web], OFF, &prefs());
        assert_eq!(plan.len(), 1, "a higher resolution still takes the slot");
        assert_eq!(plan[0].to, sel("pack", "P/Show.S01E01.avi"));
    }

    /// With `ALLOW_RESOLUTION_DOWNGRADE` on, the upgrade engine moves slots off copies above the
    /// ceiling, so the planner never moves a slot onto one. With the flag off the ceiling plays no
    /// part (the documented behaviour: a better copy is served whatever its resolution).
    #[test]
    fn an_over_ceiling_copy_takes_no_slot_when_downgrades_are_allowed() {
        let owned = vec![
            (
                "uhd".to_string(),
                rec(MediaKind::Series, 7, eps(1, 2), q(2160, 6_000, 9)),
            ),
            (
                "hd".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 6_000, 9)),
            ),
        ];
        let uhd = info(
            "uhd",
            &[
                ("U/Show.S01E01.2160p.mkv", 9),
                ("U/Show.S01E02.2160p.mkv", 9),
            ],
        );
        let hd = info("hd", &[("Show.S01E01.1080p.mkv", 4)]);
        let selection = vec![(episode_slot(7, 1, 1), sel("hd", "Show.S01E01.1080p.mkv"))];
        let on = RepointRules {
            max_resolution: 1080,
            allow_resolution_downgrade: true,
        };
        assert!(
            plan_selection_repoints(&owned, &selection, &[&uhd, &hd], on, &prefs()).is_empty(),
            "the 2160p pack must not take a slot from the 1080p copy above a 1080p ceiling"
        );
        let plan = plan_selection_repoints(&owned, &selection, &[&uhd, &hd], OFF, &prefs());
        assert_eq!(plan.len(), 1, "with the flag off the ceiling plays no part");
        assert_eq!(plan[0].to, sel("uhd", "U/Show.S01E01.2160p.mkv"));
        let raised = RepointRules {
            max_resolution: 2160,
            allow_resolution_downgrade: true,
        };
        assert_eq!(
            plan_selection_repoints(&owned, &selection, &[&uhd, &hd], raised, &prefs()).len(),
            1,
            "within the ceiling the better copy takes the slot"
        );
    }

    /// A copy tagged only with another audio language never takes a slot from the copy it serves,
    /// exactly as the upgrade engine never stages one (`release::wrong_audio_language` under the
    /// title's resolved `AUDIO_LANGUAGE`): mirror copies are never probed, so the tag is all there
    /// is.
    /// A copy that also carries the title's language, or no language tag, still takes the slot, and
    /// under the default `AUDIO_LANGUAGE=original` with no language recorded for the title the check
    /// is inert.
    #[test]
    fn a_wrong_language_copy_takes_no_slot() {
        let english = |mut r: OwnedRecord| {
            r.request.original_language = Some("en".into());
            r
        };
        let served = english(rec(MediaKind::Series, 7, eps(1, 1), q(1080, 3_000, 9)));
        let selection = vec![(
            episode_slot(7, 1, 1),
            sel("web", "W/Show.S01E01.1080p.WEB.mkv"),
        )];
        let mut web = info("web", &[("W/Show.S01E01.1080p.WEB.mkv", 4)]);
        web.filename = "Show.S01.1080p.WEB".into();
        let bluray = |name: &str| {
            let mut t = info("bluray", &[("B/Show.S01E01.1080p.BluRay.mkv", 8)]);
            t.filename = name.into();
            t
        };
        // The better copy is a mirror record: no recorded language of its own.
        let owned = |served: OwnedRecord| {
            vec![
                ("web".to_string(), served),
                (
                    "bluray".to_string(),
                    rec(MediaKind::Series, 7, eps(1, 1), q(1080, 6_000, 9)),
                ),
            ]
        };

        let rus = bluray("Show.S01.1080p.BluRay.RUS");
        assert!(
            plan_selection_repoints(
                &owned(served.clone()),
                &selection,
                &[&web, &rus],
                OFF,
                &prefs()
            )
            .is_empty(),
            "a Russian-only copy must not serve an English show"
        );

        for name in ["Show.S01.1080p.BluRay.RUS.ENG", "Show.S01.1080p.BluRay"] {
            let t = bluray(name);
            let plan = plan_selection_repoints(
                &owned(served.clone()),
                &selection,
                &[&web, &t],
                OFF,
                &prefs(),
            );
            assert_eq!(plan.len(), 1, "{name}: the better copy takes the slot");
            assert_eq!(plan[0].to.hash, "bluray");
        }

        // No language recorded anywhere for the title: inert, as for acquisition.
        let unknown = rec(MediaKind::Series, 7, eps(1, 1), q(1080, 3_000, 9));
        assert_eq!(
            plan_selection_repoints(&owned(unknown), &selection, &[&web, &rus], OFF, &prefs())
                .len(),
            1
        );

        // The served record lacks a language but another record of the title has one.
        let mut group = owned(rec(MediaKind::Series, 7, eps(1, 1), q(1080, 3_000, 9)));
        group.push((
            "other".to_string(),
            english(rec(MediaKind::Series, 7, vec![(1, 2)], q(1080, 3_000, 9))),
        ));
        assert!(
            plan_selection_repoints(&group, &selection, &[&web, &rus], OFF, &prefs()).is_empty(),
            "the title's language is taken from any record of the group"
        );
    }

    /// A pack may tag a season's audio language on its folder only (`Season 2 [German]/`), so the
    /// language test reads the file's directories as well as its torrent and file names. A
    /// subtitle language on a folder is no audio language.
    #[test]
    fn a_language_tagged_on_a_folder_counts_for_the_files_under_it() {
        let mut served = rec(MediaKind::Series, 7, vec![(2, 1)], q(1080, 3_000, 9));
        served.request.original_language = Some("en".into());
        let owned = vec![
            ("web".to_string(), served),
            (
                "pack".to_string(),
                rec(
                    MediaKind::Series,
                    7,
                    vec![(1, 1), (2, 1)],
                    q(1080, 6_000, 9),
                ),
            ),
        ];
        let web = info("web", &[("Show.S02E01.1080p.WEB.mkv", 4)]);
        let selection = vec![(
            episode_slot(7, 2, 1),
            sel("web", "Show.S02E01.1080p.WEB.mkv"),
        )];
        let pack = |season_2: &str| {
            let mut t = info(
                "pack",
                &[
                    ("Pack/Season 1/Show.S01E01.1080p.BluRay.mkv", 8),
                    (&format!("Pack/{season_2}/Show.S02E01.1080p.BluRay.mkv"), 8),
                ],
            );
            t.filename = "Show.Complete.1080p.BluRay".into();
            t
        };
        let german = pack("Season 2 [German]");
        assert!(
            plan_selection_repoints(&owned, &selection, &[&web, &german], OFF, &prefs()).is_empty(),
            "a German-only season must not serve an English show"
        );
        for folder in ["Season 2", "Season 2 [German Subs]"] {
            let t = pack(folder);
            assert_eq!(
                plan_selection_repoints(&owned, &selection, &[&web, &t], OFF, &prefs()).len(),
                1,
                "{folder}: the better copy takes the slot"
            );
        }
    }

    /// An explicit `AUDIO_LANGUAGE` is not resolved against the title, so the language test applies
    /// even when no language is recorded for the title (only the default `original` is inert then).
    #[test]
    fn an_explicit_audio_language_applies_to_a_title_with_no_recorded_language() {
        let owned = vec![
            (
                "web".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 3_000, 9)),
            ),
            (
                "bluray".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 6_000, 9)),
            ),
        ];
        let web = info("web", &[("W/Show.S01E01.1080p.WEB.mkv", 4)]);
        let mut rus = info("bluray", &[("B/Show.S01E01.1080p.BluRay.mkv", 8)]);
        rus.filename = "Show.S01.1080p.BluRay.RUS".into();
        let selection = vec![(
            episode_slot(7, 1, 1),
            sel("web", "W/Show.S01E01.1080p.WEB.mkv"),
        )];
        let english = crate::config::QualityPrefs {
            audio: crate::config::AudioReq::Lang("en".into()),
            ..prefs()
        };
        assert!(
            plan_selection_repoints(&owned, &selection, &[&web, &rus], OFF, &english).is_empty(),
            "AUDIO_LANGUAGE=en keeps a Russian-only copy off the slot"
        );
        assert_eq!(
            plan_selection_repoints(&owned, &selection, &[&web, &rus], OFF, &prefs()).len(),
            1,
            "control: under AUDIO_LANGUAGE=original with nothing recorded the test is inert"
        );
    }

    /// A mixed-quality pack (an account-mirror "complete series" whose record quality comes from its
    /// largest file) is ranked by the file it would serve, not by that record: its 480p DVDRip `.avi`
    /// S01E01 never takes the slot from a 1080p WEB single, and an unchecked `.avi` at the same
    /// resolution never takes a checked slot through the pack's MKV-derived `checked` flag.
    #[test]
    fn a_mixed_quality_pack_is_ranked_by_the_file_it_would_serve() {
        let pack_with = |s01e01: &str| {
            info(
                "pack",
                &[
                    ("Pack/Show.S05E10.1080p.BluRay.mkv", 9),
                    (&format!("Pack/{s01e01}"), 2),
                ],
            )
        };
        let owned = vec![
            (
                "single".to_string(),
                rec(MediaKind::Series, 7, eps(1, 1), q(1080, 3_000, 9)),
            ),
            (
                "pack".to_string(),
                rec(
                    MediaKind::Series,
                    7,
                    vec![(1, 1), (5, 10)],
                    q(1080, 6_000, 9),
                ),
            ),
        ];
        let single = info("single", &[("Show.S01E01.1080p.WEB-DL.mkv", 4)]);
        let selection = vec![(
            episode_slot(7, 1, 1),
            sel("single", "Show.S01E01.1080p.WEB-DL.mkv"),
        )];

        // Control: a pack whose S01E01 file matches its record takes the slot.
        let good = pack_with("Show.S01E01.1080p.BluRay.mkv");
        assert_eq!(
            plan_selection_repoints(&owned, &selection, &[&single, &good], OFF, &prefs()).len(),
            1,
            "control: a BluRay MKV episode file beats the WEB single"
        );

        let dvd = pack_with("Show.S01E01.480p.DVDRip.XviD.avi");
        assert!(
            plan_selection_repoints(&owned, &selection, &[&single, &dvd], OFF, &prefs()).is_empty(),
            "the pack's 480p .avi must not take the slot from a 1080p single"
        );

        let avi = pack_with("Show.S01E01.1080p.BluRay.avi");
        assert!(
            plan_selection_repoints(&owned, &selection, &[&single, &avi], OFF, &prefs()).is_empty(),
            "an unchecked .avi at the same resolution must not take a checked slot"
        );

        // A slot already serving the pack's 480p file moves to the 1080p single.
        let on_dvd = vec![(
            episode_slot(7, 1, 1),
            sel("pack", "Pack/Show.S01E01.480p.DVDRip.XviD.avi"),
        )];
        let plan = plan_selection_repoints(&owned, &on_dvd, &[&single, &dvd], OFF, &prefs());
        assert_eq!(
            plan,
            vec![SlotRepoint {
                slot: episode_slot(7, 1, 1),
                from: sel("pack", "Pack/Show.S01E01.480p.DVDRip.XviD.avi"),
                to: sel("single", "Show.S01E01.1080p.WEB-DL.mkv"),
            }]
        );
        // ...and stays there (converges).
        let moved = vec![(episode_slot(7, 1, 1), plan[0].to.clone())];
        assert!(
            plan_selection_repoints(&owned, &moved, &[&single, &dvd], OFF, &prefs()).is_empty()
        );
    }

    /// A repackaged "complete series" tags its quality on the season folders and leaves the episode
    /// files untagged. A file name's untagged axis comes from the nearest directory that tags it
    /// (still capped by the record), and only then from the record; a tag in the file name itself
    /// wins over any directory, and the container still comes from the file name alone.
    #[test]
    fn served_rank_reads_an_untagged_axis_from_the_nearest_directory_that_tags_it() {
        use crate::release::QualityRank;
        let rank = |resolution, source_tier, checked| QualityRank {
            resolution,
            source_tier,
            checked,
        };
        let bluray = q(1080, 6_000, 9);
        // A DVD source is the lowest tier (see the DVD test below), so the torrent folder's BluRay
        // never reaches a file in the DVDRip season folder.
        assert_eq!(
            served_rank(bluray.as_ref(), FOLDER_TAGGED_S05E01),
            rank(480, 0, false)
        );
        // Real-Debrid paths start with a `/`.
        assert_eq!(
            served_rank(
                bluray.as_ref(),
                "/Show.Complete.1080p.BluRay/Season 5 [480p DVDRip]/Show - S05E01 - Title.mkv"
            ),
            rank(480, 0, true)
        );
        // The nearest tagging directory wins over an outer one.
        assert_eq!(
            served_rank(
                q(2160, 8_000, 9).as_ref(),
                "Show.2160p.REMUX/Season 1 [720p WEB-DL]/Show - S01E01.mkv"
            ),
            rank(720, 3_000, true)
        );
        // A tag in the file name wins over any directory.
        assert_eq!(
            served_rank(
                bluray.as_ref(),
                "Season 1 [480p HDTV]/Show.S01E01.1080p.WEB.mkv"
            ),
            rank(1080, 3_000, true)
        );
        // A directory never raises an axis above the record's.
        assert_eq!(
            served_rank(
                q(720, 3_000, 9).as_ref(),
                "Show.S01.2160p.REMUX/Show - S01E01.mkv"
            ),
            rank(720, 3_000, true)
        );
        // A show titled with a cam word names no cam source in its folder, though the folder name
        // alone parses as one (control).
        let folder = crate::release::parse(&crate::release::RawCandidate {
            name: "Body Cam".into(),
            description: String::new(),
            info_hash: String::new(),
            file_idx: None,
            file_name: None,
        });
        assert_eq!(folder.source, crate::release::Source::Cam);
        assert_eq!(
            served_rank(bluray.as_ref(), "Body Cam/Season 1/Body Cam - S01E01.mkv"),
            rank(1080, 6_000, true)
        );
        // Likewise a folder titled with the bare word `Web` names no WEB source, though it parses
        // as one (control); a folder that also tags a resolution, or an explicit WEB-DL, does.
        let folder = crate::release::parse(&crate::release::RawCandidate {
            name: "Web of Lies".into(),
            description: String::new(),
            info_hash: String::new(),
            file_idx: None,
            file_name: None,
        });
        assert_eq!(folder.source, crate::release::Source::Web);
        assert_eq!(
            served_rank(
                bluray.as_ref(),
                "Web of Lies/Season 1/Web of Lies - S01E01.mkv"
            ),
            rank(1080, 6_000, true)
        );
        for path in [
            "Show.Complete.1080p.WEB/Season 1/Show - S01E01.mkv",
            "Show.Complete.WEB-DL/Show - S01E01.mkv",
            "Show.Complete.WEB.x264-GRP/Show - S01E01.mkv",
        ] {
            assert_eq!(
                served_rank(bluray.as_ref(), path),
                rank(1080, 3_000, true),
                "{path}"
            );
        }
        // No directory tags anything: the record's axes, as before.
        assert_eq!(
            served_rank(bluray.as_ref(), "Show/Season 1/Show - S01E01.mkv"),
            rank(1080, 6_000, true)
        );
    }

    /// A repackaged pack's S01E01, in a season folder tagged like its torrent name and record.
    const FOLDER_TAGGED_S01E01: &str =
        "Show.Complete.1080p.BluRay/Season 1 [1080p BluRay]/Show - S01E01 - Title.mkv";
    /// The same pack's S05E01, in a season folder tagged 480p DVDRip.
    const FOLDER_TAGGED_S05E01: &str =
        "Show.Complete.1080p.BluRay/Season 5 [480p DVDRip]/Show - S05E01 - Title.avi";
    /// Another pack's S05E01: a scene DVDRip whose name and folders tag no resolution.
    const SCENE_DVDRIP_S05E01: &str =
        "Show.Complete.1080p.BluRay/Season 5/show.s05e01.dvdrip.xvid-grp.avi";
    /// Another pack's S05E01: a 2000s scene HDTV XviD rip, standard definition by scene convention.
    const SCENE_HDTV_S05E01: &str =
        "Show.Complete.1080p.BluRay/Season 5/show.s05e01.hdtv.xvid-lol.avi";
    /// Another pack's S05E01: a scene PDTV XviD rip.
    const SCENE_PDTV_S05E01: &str =
        "Show.Complete.1080p.BluRay/Season 5/show.s05e01.pdtv.xvid-grp.avi";

    /// Scene naming leaves the resolution off a standard-definition release: an `HDTV.XviD`,
    /// `PDTV`, `DSR`, `SDTV`, `TVRip` or `VHSRip` episode, like a DVD one, is SD, yet would serve at
    /// its pack record's 1080p BluRay (`release::parse` reads no resolution in it, and reads `PDTV`
    /// and the rest as no source at all). A name that tags such a source, or the SD codecs XviD or
    /// DivX, is read as at most 576p when it tags no resolution. A DVD (a DVD remux included), PDTV,
    /// SDTV, DSR, TVRip or VHSRip source also counts as the lowest source tier, while HDTV keeps its
    /// tier and XviD/DivX name no source. An explicit resolution still wins (`720p.HDTV`), and the
    /// record still caps.
    #[test]
    fn an_sd_source_with_no_resolution_tag_serves_as_standard_definition() {
        use crate::release::QualityRank;
        let rank = |resolution, source_tier, checked| QualityRank {
            resolution,
            source_tier,
            checked,
        };
        let bluray = q(1080, 6_000, 9);
        for (name, expected) in [
            ("show.s01e01.hdtv.xvid-lol.avi", rank(576, 1_000, false)),
            ("Show.S01E01.HDTVRip.mkv", rank(576, 1_000, true)),
            ("show.s01e01.pdtv.xvid.avi", rank(576, 0, false)),
            ("show.s01e01.dsr.xvid.avi", rank(576, 0, false)),
            ("Show.S01E01.DSRip.mkv", rank(576, 0, true)),
            ("Show.S01E01.SDTV.mkv", rank(576, 0, true)),
            ("Show.S01E01.TVRip.mkv", rank(576, 0, true)),
            ("Show.S01E01.TV-Rip.mkv", rank(576, 0, true)),
            ("Show.S01E01.VHSRip.avi", rank(576, 0, false)),
            ("Show.S01E01.DVDRemux.mkv", rank(576, 0, true)),
            ("Show.S01E01.DVD.REMUX.mkv", rank(576, 0, true)),
            ("show.s01e01.xvid-grp.avi", rank(576, 6_000, false)),
            ("Show.S01E01.DivX.avi", rank(576, 6_000, false)),
            // An explicit resolution wins.
            ("show.s01e01.720p.hdtv.x264-grp.mkv", rank(720, 1_000, true)),
            ("Show.S01E01.720p.XviD.avi", rank(720, 6_000, false)),
            // A folder's SD tag counts for a file that tags nothing.
            (
                "Show.S01.HDTV.XviD/Show - S01E01.avi",
                rank(576, 1_000, false),
            ),
        ] {
            assert_eq!(served_rank(bluray.as_ref(), name), expected, "{name}");
        }
    }

    /// A DVD is standard definition, and `release::parse` reads `DVDRip` as no source at all, so a
    /// scene `DVDRip.XviD` episode that tags no resolution would serve at its pack record's 1080p
    /// BluRay. A name (the file's, or failing that the nearest folder's) that tags a DVD source is
    /// read as at most 576p when it tags no resolution, at the lowest source tier, still capped by
    /// the record. `HD-DVD` is high definition and no DVD.
    #[test]
    fn a_dvd_source_with_no_resolution_tag_serves_as_standard_definition() {
        use crate::release::QualityRank;
        let rank = |resolution, source_tier, checked| QualityRank {
            resolution,
            source_tier,
            checked,
        };
        let bluray = q(1080, 6_000, 9);
        assert_eq!(
            served_rank(bluray.as_ref(), SCENE_DVDRIP_S05E01),
            rank(576, 0, false)
        );
        // A folder's DVD tag counts for a file that tags nothing.
        assert_eq!(
            served_rank(
                bluray.as_ref(),
                "Show.Complete.1080p.BluRay/Season 5 [DVDRip]/Show - S05E01 - Title.mkv"
            ),
            rank(576, 0, true)
        );
        for name in [
            "Show.S05E01.DVD-Rip.mkv",
            "Show S05E01 DVD.mkv",
            "Show_S05E01_DVD9.mkv",
            "Show.S05E01.DVD5.mkv",
            "Show.S05E01.DVDR.mkv",
        ] {
            assert_eq!(
                served_rank(bluray.as_ref(), name),
                rank(576, 0, true),
                "{name}"
            );
        }
        // An explicit resolution wins over the DVD tag (an upscaled DVDRip), in the file name or
        // in a folder.
        assert_eq!(
            served_rank(bluray.as_ref(), "Show.S05E01.720p.DVDRip.mkv"),
            rank(720, 0, true)
        );
        assert_eq!(
            served_rank(bluray.as_ref(), "Season 5 [480p]/Show.S05E01.DVDRip.mkv"),
            rank(576, 0, true),
            "the file's own DVD tag is nearer than the folder's resolution"
        );
        // The record still caps, and an unknown record stays unknown.
        assert_eq!(
            served_rank(q(480, 1_000, 9).as_ref(), SCENE_DVDRIP_S05E01),
            rank(480, 0, false)
        );
        assert_eq!(
            served_rank(q(0, 0, 9).as_ref(), SCENE_DVDRIP_S05E01),
            rank(0, 0, false)
        );
        // HD DVD is high definition, and a word merely containing `dvd` tags nothing.
        for name in [
            "Movie.2006.HD-DVD.x264.mkv",
            "Movie.2006.HDDVD.x264.mkv",
            "Movie.2006.HD.DVD.x264.mkv",
            "Show/MyDVDCollection/Show - S01E01.mkv",
        ] {
            assert_eq!(
                served_rank(bluray.as_ref(), name),
                rank(1080, 6_000, true),
                "{name}"
            );
        }
    }

    /// A pack whose torrent name and largest file claim 1080p BluRay, but whose season 5 folder is
    /// a 480p DVDRip, serves S05E01 at 480p: its `.avi` never takes the slot from a checked 720p
    /// WEB single, and a slot already on it moves to the single.
    #[test]
    fn a_pack_tagged_on_its_season_folders_is_ranked_by_the_folder_of_the_file_it_would_serve() {
        // The scene SD episodes, which tag no resolution, behave the same way.
        for s05e01 in [
            FOLDER_TAGGED_S05E01,
            SCENE_DVDRIP_S05E01,
            SCENE_HDTV_S05E01,
            SCENE_PDTV_S05E01,
        ] {
            pack_s05e01_never_takes_a_better_singles_slot(s05e01);
        }
    }

    fn pack_s05e01_never_takes_a_better_singles_slot(s05e01: &str) {
        let owned = vec![
            (
                "single".to_string(),
                rec(MediaKind::Series, 7, vec![(5, 1)], q(720, 3_000, 9)),
            ),
            (
                "pack".to_string(),
                rec(
                    MediaKind::Series,
                    7,
                    vec![(1, 1), (5, 1)],
                    q(1080, 6_000, 9),
                ),
            ),
        ];
        let pack = info("pack", &[(FOLDER_TAGGED_S01E01, 9), (s05e01, 2)]);
        let single = info("single", &[("Show.S05E01.720p.WEB.mkv", 4)]);
        let on_single = vec![(
            episode_slot(7, 5, 1),
            sel("single", "Show.S05E01.720p.WEB.mkv"),
        )];
        assert!(
            plan_selection_repoints(&owned, &on_single, &[&single, &pack], OFF, &prefs())
                .is_empty(),
            "the pack's SD {s05e01} must not take the slot from a 720p single"
        );
        let on_pack = vec![(episode_slot(7, 5, 1), sel("pack", s05e01))];
        assert_eq!(
            plan_selection_repoints(&owned, &on_pack, &[&single, &pack], OFF, &prefs()),
            vec![SlotRepoint {
                slot: episode_slot(7, 5, 1),
                from: sel("pack", s05e01),
                to: sel("single", "Show.S05E01.720p.WEB.mkv"),
            }]
        );
        // `observe`'s pack gate ranks the same way.
        assert!(!takes_slot_from(
            (q(1080, 6_000, 2_000_000).as_ref(), s05e01),
            (q(720, 3_000, 1).as_ref(), "Show.S05E01.720p.WEB.mkv"),
        ));
    }

    /// Dedup ranks a copy's coverage of an episode with [`served_rank`] too: the folder-tagged
    /// pack's 480p S05E01, or a scene DVDRip S05E01 that tags no resolution, never makes a 720p
    /// single of S05E01 redundant.
    #[test]
    fn dedup_keeps_a_single_a_folder_tagged_pack_serves_at_a_lower_resolution() {
        for s05e01 in [
            FOLDER_TAGGED_S05E01,
            SCENE_DVDRIP_S05E01,
            SCENE_HDTV_S05E01,
            SCENE_PDTV_S05E01,
        ] {
            dedup_keeps_the_single_beside(s05e01);
        }
    }

    fn dedup_keeps_the_single_beside(s05e01: &str) {
        let owned = vec![
            (
                "single".to_string(),
                rec(MediaKind::Series, 7, vec![(5, 1)], q(720, 3_000, 1)),
            ),
            (
                "pack".to_string(),
                rec(
                    MediaKind::Series,
                    7,
                    vec![(1, 1), (5, 1)],
                    q(1080, 6_000, 9),
                ),
            ),
        ];
        let pack = info(
            "pack",
            &[(FOLDER_TAGGED_S01E01, 9_000_000_000), (s05e01, 700_000_000)],
        );
        let single = info("single", &[("Show.S05E01.720p.WEB.mkv", 3_000_000_000)]);
        let files: std::collections::HashMap<String, &TorrentInfo> =
            [("pack".to_string(), &pack), ("single".to_string(), &single)]
                .into_iter()
                .collect();
        let present: HashSet<String> = ["pack".to_string(), "single".to_string()].into();
        let plans = crate::tasks::plan_dedup(
            &owned,
            &present,
            &["single".to_string()].into(),
            &HashSet::new(),
            &files,
            &prefs(),
        );
        assert!(
            plans.iter().all(|p| p.remove.is_empty()),
            "the 720p single is kept beside {s05e01}: {plans:?}"
        );
    }

    /// A feature-sized video, as a multi-film collection holds several of.
    const FEATURE: u64 = 4_000_000_000;

    /// The VFS shows every selected film of a movie slot's torrent, so a movie slot is never moved
    /// onto a multi-film collection (its largest film may not even be this film), and a slot already
    /// on one stays there: moving it off would hide the collection's other films. The upgrade engine
    /// and dedup leave such titles alone for the same reason.
    #[test]
    fn a_movie_slot_never_moves_onto_or_off_a_multi_film_collection() {
        let collection = |second_selected: u32| {
            let mut t = info(
                "coll",
                &[
                    ("Coll/Film.One.1999.1080p.BluRay.mkv", FEATURE + 1),
                    ("Coll/Film.Two.2003.1080p.BluRay.mkv", FEATURE),
                ],
            );
            t.files[1].selected = second_selected;
            t
        };
        let owned = vec![
            (
                "web".to_string(),
                rec(MediaKind::Movie, 603, vec![], q(1080, 3_000, 1)),
            ),
            (
                "coll".to_string(),
                rec(MediaKind::Movie, 603, vec![], q(1080, 6_000, 1)),
            ),
        ];
        let web = info("web", &[("Film.One.1999.1080p.WEB.mkv", FEATURE)]);
        let on_web = vec![(movie_slot(603), sel("web", "Film.One.1999.1080p.WEB.mkv"))];

        // Control: with one film selected it is a single-film copy and takes the slot.
        assert_eq!(
            plan_selection_repoints(&owned, &on_web, &[&web, &collection(0)], OFF, &prefs()).len(),
            1,
            "control: a single-film copy takes the slot"
        );
        assert!(
            plan_selection_repoints(&owned, &on_web, &[&web, &collection(1)], OFF, &prefs())
                .is_empty(),
            "a collection never takes a movie slot"
        );

        // A slot on the collection stays there, even against a better single-film copy.
        let mut with_remux = owned.clone();
        with_remux.push((
            "remux".to_string(),
            rec(MediaKind::Movie, 603, vec![], q(1080, 8_000, 1)),
        ));
        let remux = info("remux", &[("Film.One.1999.1080p.REMUX.mkv", FEATURE)]);
        let on_coll = vec![(
            movie_slot(603),
            sel("coll", "Coll/Film.One.1999.1080p.BluRay.mkv"),
        )];
        assert!(
            plan_selection_repoints(
                &with_remux,
                &on_coll,
                &[&web, &collection(1), &remux],
                OFF,
                &prefs()
            )
            .is_empty(),
            "moving the slot off a collection would hide its other films"
        );
    }
}
