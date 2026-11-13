//! Serve the best copy already in the account: repoint a movie or episode `selection` slot from the
//! copy it serves to a strictly better one that is present.
//!
//! The VFS shows only a slot's selected file. Slots are written when the engine verifies a copy
//! (`observe`) or swaps in an upgrade, and nothing else ever moved one, so a better copy that
//! arrived any other way — an account-mirror copy (pre-existing, or re-added by hand) never gets a
//! slot of its own — stayed hidden behind the worse copy the slot already named. The scan loop runs
//! [`repoint_to_better_copies`] after dedup, so every slot ends up serving the best present copy.

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
/// movie, it has an emittable video file — and plan a repoint when that copy's
/// [`crate::release::quality_rank`] is STRICTLY higher (resolution, then source tier, then checked
/// before unverified; score ignored, so equal-quality copies never churn a slot). Among several
/// better copies the highest rank wins, then score, then the smallest hash. A slot whose served copy
/// is absent, unsettled or unowned is left alone (the VFS already falls back for a stale slot), and
/// a slot is never created. `present` is the provider listing's identified torrents with their files.
/// Two exceptions keep the planner from undoing the upgrade engine (`rules`): a slot serving a
/// checked copy moves onto an unchecked one only for a strictly higher resolution, and with
/// `ALLOW_RESOLUTION_DOWNGRADE` no slot moves onto a copy above `MAX_RESOLUTION`. And a copy tagged
/// only with other audio languages never takes a slot: `release::wrong_audio_language` on the
/// copy's torrent + file name, under `prefs` with `AUDIO_LANGUAGE` resolved against the title's
/// recorded languages (the served record's, else any owned record of the title's) — the test the
/// upgrade engine applies before it stages a release. Mirror copies are never probed, so a
/// better-ranked wrong-language copy would otherwise take every slot it provides; with no language
/// recorded for the title the test is inert, as it is for acquisition.
/// Every repoint moves a slot strictly up in rank, so repeated passes converge.
pub fn plan_selection_repoints(
    owned: &[(String, OwnedRecord)],
    selection: &[(String, SelectionEntry)],
    present: &[&TorrentInfo],
    rules: RepointRules,
    prefs: &crate::config::QualityPrefs,
) -> Vec<SlotRepoint> {
    use crate::release::quality_rank;
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
    // Per title, a request that records the title's languages (any owned record, present or not).
    let mut title_langs: HashMap<(bool, u64), &crate::store::AcquireRequest> = HashMap::new();
    for (_, r) in owned {
        if r.request.original_language.is_some() {
            title_langs
                .entry((r.request.kind == MediaKind::Movie, r.request.tmdb_id))
                .or_insert(&r.request);
        }
    }
    // The file a copy would serve for a slot: the largest emittable video file — for an episode,
    // among those whose first episode code is that episode.
    let file_for = |hash: &str, episode: Option<(u32, u32)>| -> Option<String> {
        let t = present_by_hash.get(hash)?;
        crate::vfs::emittable_video_files(t)
            .into_iter()
            .filter(|f| episode.is_none_or(|ep| crate::vfs::episode_of_file(&f.path) == Some(ep)))
            .max_by_key(|f| f.bytes)
            .map(|f| f.path.clone())
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
        let cur_rank = quality_rank(cur.quality.as_ref());
        let group = by_title.get(&(is_movie, tmdb));
        // The title's audio requirement, resolved as acquisition resolves it: from the served
        // record's languages, else from any record of the title that has them (a mirror record
        // lacks them until the upgrade pass backfills them).
        let lang_req = if cur.request.original_language.is_some() {
            &cur.request
        } else {
            title_langs
                .get(&(is_movie, tmdb))
                .copied()
                .unwrap_or(&cur.request)
        };
        let audio = prefs.with_resolved_audio(
            lang_req.original_language.as_deref(),
            lang_req.spoken_languages.as_deref().unwrap_or_default(),
        );
        let wrong_language = |hash: &str, path: &str| -> bool {
            let Some(t) = present_by_hash.get(hash) else {
                return false;
            };
            let name = if t.filename.is_empty() {
                &t.original_filename
            } else {
                &t.filename
            };
            let base = path.rsplit('/').next().unwrap_or(path);
            let release = crate::release::parse(&crate::release::RawCandidate {
                name: name.clone(),
                description: base.to_string(),
                info_hash: hash.to_string(),
                file_idx: None,
                file_name: Some(base.to_string()),
            });
            crate::release::wrong_audio_language(&release, &audio)
        };
        // Never undo what the upgrade engine adopts. Its unverified rule swaps an unchecked copy for
        // a checked one of a possibly LOWER tier at the same resolution, so a checked slot moves onto
        // an unchecked copy only for a strictly higher resolution. With downgrades allowed it moves
        // slots off copies above the ceiling, so no slot moves onto one.
        let allowed = |rank: crate::release::QualityRank| {
            (rank.checked || !cur_rank.checked || rank.resolution > cur_rank.resolution)
                && !(rules.allow_resolution_downgrade && rank.resolution > rules.max_resolution)
        };
        let best = group
            .into_iter()
            .flatten()
            .filter(|(h, r)| {
                let rank = quality_rank(r.quality.as_ref());
                *h != current
                    && rank > cur_rank
                    && allowed(rank)
                    && episode.is_none_or(|ep| r.provides.contains(&ep))
            })
            .filter_map(|(h, r)| file_for(h, episode).map(|path| (*h, *r, path)))
            .filter(|(h, _, path)| !wrong_language(h, path))
            .max_by(|(ha, ra, _), (hb, rb, _)| {
                let key = |r: &OwnedRecord| {
                    (
                        quality_rank(r.quality.as_ref()),
                        r.quality.as_ref().map(|q| q.score).unwrap_or(i64::MIN),
                    )
                };
                // Highest rank, then score; on a full tie the SMALLER hash wins.
                key(ra).cmp(&key(rb)).then_with(|| hb.cmp(ha))
            });
        if let Some((hash, _, path)) = best {
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

    fn mem_store() -> Store {
        let db = redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .unwrap();
        Store::from_database(std::sync::Arc::new(db)).unwrap()
    }

    /// A two-copy show where the better copy holds no slot, as the executor tests' fixture.
    async fn better_copy_fixture() -> (Store, Vec<(TorrentInfo, MediaMetadata)>) {
        let store = mem_store();
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
    /// with no language recorded for the title the check is inert.
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
}
