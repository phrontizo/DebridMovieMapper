//! Pure reconcile-core for the SP2 Trakt wanted-set lifecycle.
//!
//! These functions contain ZERO I/O: no store, no provider, no network, no async. They take
//! plain data assembled by the caller (Task 8 builds a [`TitleView`] per title from the store +
//! provider availability) and return the lifecycle decisions — acquire and removal [`Action`]s.
//!
//! Keeping the correctness-critical lifecycle rules pure makes them exhaustively unit-testable
//! without fixtures. The two removal triggers are:
//!   - **Trigger A** (finished): everyone who currently wants a title has finished watching it.
//!   - **Trigger B** (abandoned): the watchlist that added it was abandoned and nobody wants it.
//!
//! A title with any **manual** provenance is never auto-removed, regardless of Trakt state.

use crate::store::{Provenance, ProvenanceEntry, WantedRecord, WatchedState};
use crate::tmdb_client::ShowStatus;
use crate::vfs::MediaType;

/// The currently-owned engine copy of a title (the caller assembles this from `owned_hashes`
/// + provider availability). Movies leave `owned_episodes` empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owned {
    pub hash: String,
    pub provenance: Provenance,
    pub available: bool, // false → cache lapsed / missing / 503 → re-acquire
    pub owned_episodes: Vec<(u32, u32)>, // shows: which (season, episode) are owned
}

/// Everything the reconciler needs about ONE title (one tmdb_id), assembled by the caller (Task 8).
#[derive(Debug, Clone)]
pub struct TitleView {
    pub tmdb_id: u64,
    pub media_type: MediaType,
    pub wanted: Vec<WantedRecord>, // per-user current wanted rows for this title (empty → nobody wants it)
    /// The engine-owned copy of this title, or `None` if not owned. The reconciler models
    /// ONE owned copy per title: for shows acquired episode-by-episode (multiple hashes),
    /// the caller (Task 8) MUST aggregate all per-episode hashes into a single `Owned` whose
    /// `owned_episodes` is the union across hashes, and treat a resulting `Action::Remove` as
    /// "delete ALL owned hashes for this tmdb_id" (the `hash` field is a representative). The
    /// reconciler cannot express "remove N hashes". Assembling one `TitleView` per *hash*
    /// would make Trigger A misfire on partial coverage — assemble per *tmdb_id*.
    pub owned: Option<Owned>,
    /// Caller is responsible for de-duplicating; duplicate entries would yield duplicate AcquireEpisode actions.
    pub aired_episodes: Vec<(u32, u32)>, // shows: episodes aired as of "now" (from TMDB). movies: ignored
}

/// An action the reconciler asks the engine to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    AcquireMovie {
        tmdb_id: u64,
    },
    AcquireEpisode {
        tmdb_id: u64,
        season: u32,
        episode: u32,
    },
    Remove {
        tmdb_id: u64,
        hash: String,
    },
}

/// True iff a user currently wants this title via either Trakt source.
pub fn wants(r: &WantedRecord) -> bool {
    r.sources.watchlist || r.sources.in_progress
}

/// Has THIS user finished the title?
/// - Movie: the `watched` flag.
/// - Show: only an **ended** series counts as finishable, and then only when every aired
///   episode is in the user's watched set (100% of aired). An empty `aired` makes the
///   "all aired watched" clause vacuously true, so an ended show with nothing aired is finished.
///
/// `aired` is the TMDB-sourced aired-episode list for shows and is IGNORED for movies;
/// callers pass `&[]` for movies.
pub fn user_finished(r: &WantedRecord, aired: &[(u32, u32)]) -> bool {
    match &r.watched_state {
        WatchedState::Movie { watched } => *watched,
        WatchedState::Show { watched_episodes } => {
            r.show_status == Some(ShowStatus::Ended)
                && aired.iter().all(|e| watched_episodes.contains(e)) // O(|watched_episodes| × |aired|); fine for realistic episode counts.
        }
    }
}

/// Trigger A: every user who currently wants this title has finished it.
/// Requires at least one wanting user — with no wanters this returns `false` (Trigger B's job).
pub fn trigger_a_finished(wanted: &[WantedRecord], aired: &[(u32, u32)]) -> bool {
    let mut any_wanter = false;
    for r in wanted.iter().filter(|r| wants(r)) {
        any_wanter = true;
        if !user_finished(r, aired) {
            return false;
        }
    }
    any_wanter
}

/// Trigger B: the title entered via a watchlist that has since been abandoned, and nobody
/// currently wants it. `true` iff provenance has at least one `Watchlist` entry AND no record
/// in `wanted` currently `wants(r)` (the combined wanted-set is empty / all-false). Any user
/// still wanting it (watchlist or in-progress) blocks removal.
pub fn trigger_b_abandoned(wanted: &[WantedRecord], provenance: &Provenance) -> bool {
    let entered_via_watchlist = provenance
        .entries
        .iter()
        .any(|e| matches!(e, ProvenanceEntry::Watchlist { .. }));
    entered_via_watchlist && !wanted.iter().any(wants)
}

/// Whether the reconciler should remove this title's engine copy.
/// Only engine-owned, non-manual titles are removable; then either trigger fires.
pub fn should_remove(title: &TitleView) -> bool {
    let Some(owned) = &title.owned else {
        return false; // only engine-owned titles are removable
    };
    if owned.provenance.has_manual_entry() {
        return false; // manual provenance is never auto-removed
    }
    trigger_a_finished(&title.wanted, &title.aired_episodes)
        || trigger_b_abandoned(&title.wanted, &owned.provenance)
}

/// True iff episode `e` is owned AND currently available (a lapsed cache → false, forcing re-acquire).
fn episode_covered(title: &TitleView, e: &(u32, u32)) -> bool {
    match &title.owned {
        Some(owned) => owned.available && owned.owned_episodes.contains(e),
        None => false,
    }
}

/// Decide the actions for ONE title. Removal takes precedence over acquisition.
pub fn reconcile_title(title: &TitleView) -> Vec<Action> {
    // Removal first: if a title qualifies for removal we never also acquire it.
    // `owned` is bound only to read `owned.hash`; `should_remove` already returns false when owned is None.
    if let Some(owned) = &title.owned {
        if should_remove(title) {
            return vec![Action::Remove {
                tmdb_id: title.tmdb_id,
                hash: owned.hash.clone(),
            }];
        }
    }

    // Nothing wanted → keep as-is (kept manual/owned titles fall here too).
    if !title.wanted.iter().any(wants) {
        return vec![];
    }

    match title.media_type {
        MediaType::Movie => {
            let needs_acquire = match &title.owned {
                None => true,                    // not owned → acquire
                Some(owned) => !owned.available, // owned but lapsed → re-acquire
            };
            if needs_acquire {
                vec![Action::AcquireMovie {
                    tmdb_id: title.tmdb_id,
                }]
            } else {
                vec![]
            }
        }
        MediaType::Show => title
            .aired_episodes
            .iter()
            .filter(|e| !episode_covered(title, e))
            .map(|&(season, episode)| Action::AcquireEpisode {
                tmdb_id: title.tmdb_id,
                season,
                episode,
            })
            .collect(),
    }
}

/// Reconcile a batch of titles, preserving per-title and within-title action order.
pub fn reconcile(titles: &[TitleView]) -> Vec<Action> {
    titles.iter().flat_map(reconcile_title).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::WantedSources;

    // ── constructors ──────────────────────────────────────────────────────────

    /// A movie wanted via watchlist only, with the given watched flag.
    fn watchlist_movie_record(user: &str, tmdb: u64, watched: bool) -> WantedRecord {
        movie_record(
            user, tmdb, /*watchlist*/ true, /*in_progress*/ false, watched,
        )
    }

    /// A movie wanted record with explicit sources + watched flag.
    fn movie_record(
        user: &str,
        tmdb: u64,
        watchlist: bool,
        in_progress: bool,
        watched: bool,
    ) -> WantedRecord {
        WantedRecord {
            user: user.to_string(),
            tmdb_id: tmdb,
            media_type: MediaType::Movie,
            sources: WantedSources {
                watchlist,
                in_progress,
            },
            watched_state: WatchedState::Movie { watched },
            show_status: None,
        }
    }

    /// A show wanted record with explicit sources, watched-episode set, and series status.
    fn show_record(
        user: &str,
        tmdb: u64,
        watchlisted: bool,
        in_progress: bool,
        watched_eps: Vec<(u32, u32)>,
        status: ShowStatus,
    ) -> WantedRecord {
        WantedRecord {
            user: user.to_string(),
            tmdb_id: tmdb,
            media_type: MediaType::Show,
            sources: WantedSources {
                watchlist: watchlisted,
                in_progress,
            },
            watched_state: WatchedState::Show {
                watched_episodes: watched_eps,
            },
            show_status: Some(status),
        }
    }

    fn owned(hash: &str, provenance: Provenance, available: bool, eps: Vec<(u32, u32)>) -> Owned {
        Owned {
            hash: hash.to_string(),
            provenance,
            available,
            owned_episodes: eps,
        }
    }

    fn movie_title(tmdb: u64, wanted: Vec<WantedRecord>, owned: Option<Owned>) -> TitleView {
        TitleView {
            tmdb_id: tmdb,
            media_type: MediaType::Movie,
            wanted,
            owned,
            aired_episodes: vec![],
        }
    }

    fn show_title(
        tmdb: u64,
        wanted: Vec<WantedRecord>,
        owned: Option<Owned>,
        aired: Vec<(u32, u32)>,
    ) -> TitleView {
        TitleView {
            tmdb_id: tmdb,
            media_type: MediaType::Show,
            wanted,
            owned,
            aired_episodes: aired,
        }
    }

    // ── removal lifecycle ─────────────────────────────────────────────────────

    #[test]
    fn finished_movie_is_removed_trigger_a() {
        let t = movie_title(
            1,
            vec![watchlist_movie_record("alice", 1, true)],
            Some(owned("h", Provenance::watchlist("alice"), true, vec![])),
        );
        assert!(should_remove(&t));
        assert_eq!(
            reconcile_title(&t),
            vec![Action::Remove {
                tmdb_id: 1,
                hash: "h".into()
            }]
        );
    }

    #[test]
    fn finished_ended_show_is_removed() {
        let aired = vec![(1, 1), (1, 2)];
        let t = show_title(
            2,
            vec![show_record(
                "alice",
                2,
                true,
                false,
                vec![(1, 1), (1, 2)],
                ShowStatus::Ended,
            )],
            Some(owned(
                "h",
                Provenance::watchlist("alice"),
                true,
                vec![(1, 1), (1, 2)],
            )),
            aired,
        );
        assert!(should_remove(&t));
        assert_eq!(
            reconcile_title(&t),
            vec![Action::Remove {
                tmdb_id: 2,
                hash: "h".into()
            }]
        );
    }

    #[test]
    fn returning_fully_watched_show_is_kept_no_acquire() {
        let aired = vec![(1, 1), (1, 2)];
        let t = show_title(
            3,
            vec![show_record(
                "alice",
                3,
                true,
                false,
                vec![(1, 1), (1, 2)],
                ShowStatus::Returning,
            )],
            Some(owned(
                "h",
                Provenance::watchlist("alice"),
                true,
                vec![(1, 1), (1, 2)],
            )),
            aired,
        );
        assert!(
            !should_remove(&t),
            "a returning show is never finished, even at 100% watched"
        );
        assert_eq!(
            reconcile_title(&t),
            vec![],
            "all aired owned+available → nothing to acquire"
        );
    }

    #[test]
    fn abandoned_watchlist_movie_removed_regardless_of_watch_percent_trigger_b() {
        // alice un-watchlisted (both sources false) — nobody wants it now. Trigger B fires
        // regardless of how much she had watched (incl. the record being gone entirely).
        for watched in [false, true] {
            let rec = movie_record(
                "alice", 4, /*watchlist*/ false, /*in_progress*/ false, watched,
            );
            let t = movie_title(
                4,
                vec![rec],
                Some(owned("h", Provenance::watchlist("alice"), true, vec![])),
            );
            assert!(should_remove(&t), "watched={watched}");
            assert_eq!(
                reconcile_title(&t),
                vec![Action::Remove {
                    tmdb_id: 4,
                    hash: "h".into()
                }]
            );
        }
        // Record removed entirely (empty wanted-set) — still removed.
        let t = movie_title(
            4,
            vec![],
            Some(owned("h", Provenance::watchlist("alice"), true, vec![])),
        );
        assert!(should_remove(&t));
    }

    #[test]
    fn abandoned_watchlist_show_removed_at_0_partial_full_watch_trigger_b() {
        let aired = vec![(1, 1), (1, 2), (1, 3)];
        for watched_eps in [vec![], vec![(1, 1)], vec![(1, 1), (1, 2), (1, 3)]] {
            // present but un-watchlisted (both sources false) → nobody wants it
            let rec = show_record(
                "alice",
                5,
                false,
                false,
                watched_eps.clone(),
                ShowStatus::Returning,
            );
            let t = show_title(
                5,
                vec![rec],
                Some(owned(
                    "h",
                    Provenance::watchlist("alice"),
                    true,
                    watched_eps.clone(),
                )),
                aired.clone(),
            );
            assert!(should_remove(&t), "watched_eps={watched_eps:?}");
            assert_eq!(
                reconcile_title(&t),
                vec![Action::Remove {
                    tmdb_id: 5,
                    hash: "h".into()
                }]
            );
        }
    }

    #[test]
    fn another_user_watchlisted_unstarted_keeps_title() {
        // Provenance is alice's watchlist; alice gone, but bob now watchlists it (unstarted).
        let t = movie_title(
            6,
            vec![watchlist_movie_record("bob", 6, false)],
            Some(owned("h", Provenance::watchlist("alice"), true, vec![])),
        );
        assert!(
            !should_remove(&t),
            "bob still wants it (no B) and hasn't finished it (no A)"
        );
        assert_eq!(
            reconcile_title(&t),
            vec![],
            "already owned + available → no acquire"
        );
    }

    #[test]
    fn another_user_in_progress_keeps_title() {
        let t = movie_title(
            7,
            vec![movie_record(
                "bob", 7, /*watchlist*/ false, /*in_progress*/ true, false,
            )],
            Some(owned("h", Provenance::watchlist("alice"), true, vec![])),
        );
        assert!(!should_remove(&t));
        assert_eq!(reconcile_title(&t), vec![]);
    }

    #[test]
    fn manual_provenance_is_never_removed() {
        let t = movie_title(
            8,
            vec![],
            Some(owned("h", Provenance::manual(), true, vec![])),
        );
        assert!(!should_remove(&t));
        assert_eq!(reconcile_title(&t), vec![]);

        // A merged provenance that still CONTAINS Manual must also be protected.
        let mut prov = Provenance::watchlist("alice");
        prov.merge(&Provenance::manual());
        let t2 = movie_title(8, vec![], Some(owned("h", prov, true, vec![])));
        assert!(!should_remove(&t2), "any manual entry blocks removal");
    }

    #[test]
    fn trigger_a_blocked_by_midwatch_user() {
        let t = movie_title(
            9,
            vec![
                watchlist_movie_record("alice", 9, true), // finished
                watchlist_movie_record("bob", 9, false),  // mid-watch
            ],
            Some(owned("h", Provenance::watchlist("alice"), true, vec![])),
        );
        assert!(
            !should_remove(&t),
            "bob hasn't finished → Trigger A blocked"
        );
        assert_eq!(
            reconcile_title(&t),
            vec![],
            "still wanted, owned+available → no acquire"
        );
    }

    // ── acquire ───────────────────────────────────────────────────────────────

    #[test]
    fn wanted_movie_not_owned_acquires() {
        let t = movie_title(10, vec![watchlist_movie_record("alice", 10, false)], None);
        assert_eq!(
            reconcile_title(&t),
            vec![Action::AcquireMovie { tmdb_id: 10 }]
        );
    }

    #[test]
    fn wanted_movie_owned_but_lapsed_reacquires() {
        let t = movie_title(
            11,
            vec![watchlist_movie_record("alice", 11, false)],
            Some(owned("h", Provenance::watchlist("alice"), false, vec![])),
        );
        assert_eq!(
            reconcile_title(&t),
            vec![Action::AcquireMovie { tmdb_id: 11 }]
        );
    }

    #[test]
    fn wanted_movie_owned_and_available_no_action() {
        let t = movie_title(
            12,
            vec![watchlist_movie_record("alice", 12, false)],
            Some(owned("h", Provenance::watchlist("alice"), true, vec![])),
        );
        assert_eq!(reconcile_title(&t), vec![]);
    }

    #[test]
    fn tracked_show_back_catalogue_acquires_all_aired_in_order() {
        let aired = vec![(1, 1), (1, 2), (2, 1)];
        let t = show_title(
            13,
            vec![show_record(
                "alice",
                13,
                true,
                false,
                vec![],
                ShowStatus::Returning,
            )],
            None,
            aired,
        );
        assert_eq!(
            reconcile_title(&t),
            vec![
                Action::AcquireEpisode {
                    tmdb_id: 13,
                    season: 1,
                    episode: 1
                },
                Action::AcquireEpisode {
                    tmdb_id: 13,
                    season: 1,
                    episode: 2
                },
                Action::AcquireEpisode {
                    tmdb_id: 13,
                    season: 2,
                    episode: 1
                },
            ]
        );
    }

    #[test]
    fn tracked_show_partially_owned_available_acquires_missing_only() {
        let aired = vec![(1, 1), (1, 2)];
        let t = show_title(
            14,
            vec![show_record(
                "alice",
                14,
                true,
                false,
                vec![],
                ShowStatus::Returning,
            )],
            Some(owned(
                "h",
                Provenance::watchlist("alice"),
                true,
                vec![(1, 1)],
            )),
            aired,
        );
        assert_eq!(
            reconcile_title(&t),
            vec![Action::AcquireEpisode {
                tmdb_id: 14,
                season: 1,
                episode: 2
            }]
        );
    }

    #[test]
    fn tracked_show_owned_but_lapsed_reacquires_all_aired() {
        let aired = vec![(1, 1), (1, 2)];
        let t = show_title(
            15,
            vec![show_record(
                "alice",
                15,
                true,
                false,
                vec![],
                ShowStatus::Returning,
            )],
            // owned_episodes cover both, but available=false ⇒ nothing is "covered"
            Some(owned(
                "h",
                Provenance::watchlist("alice"),
                false,
                vec![(1, 1), (1, 2)],
            )),
            aired,
        );
        assert_eq!(
            reconcile_title(&t),
            vec![
                Action::AcquireEpisode {
                    tmdb_id: 15,
                    season: 1,
                    episode: 1
                },
                Action::AcquireEpisode {
                    tmdb_id: 15,
                    season: 1,
                    episode: 2
                },
            ]
        );
    }

    // ── predicate-level ───────────────────────────────────────────────────────

    #[test]
    fn wants_predicate() {
        assert!(wants(&movie_record("a", 1, true, false, false)));
        assert!(wants(&movie_record("a", 1, false, true, false)));
        assert!(wants(&movie_record("a", 1, true, true, false)));
        assert!(!wants(&movie_record("a", 1, false, false, false)));
    }

    #[test]
    fn user_finished_movie() {
        assert!(user_finished(&movie_record("a", 1, true, false, true), &[]));
        assert!(!user_finished(
            &movie_record("a", 1, true, false, false),
            &[]
        ));
    }

    #[test]
    fn user_finished_show_ended_and_returning() {
        let aired = vec![(1, 1), (1, 2)];
        // Ended + all aired watched → finished
        assert!(user_finished(
            &show_record("a", 1, true, false, vec![(1, 1), (1, 2)], ShowStatus::Ended),
            &aired
        ));
        // Ended + partial coverage → not finished
        assert!(!user_finished(
            &show_record("a", 1, true, false, vec![(1, 1)], ShowStatus::Ended),
            &aired
        ));
        // Returning + all watched → not finished (still producing)
        assert!(!user_finished(
            &show_record(
                "a",
                1,
                true,
                false,
                vec![(1, 1), (1, 2)],
                ShowStatus::Returning
            ),
            &aired
        ));
        // Ended + empty aired → vacuously finished
        assert!(user_finished(
            &show_record("a", 1, true, false, vec![], ShowStatus::Ended),
            &[]
        ));
        // Other status (not Ended) → not finished
        assert!(!user_finished(
            &show_record("a", 1, true, false, vec![(1, 1), (1, 2)], ShowStatus::Other),
            &aired
        ));
    }

    #[test]
    fn trigger_a_finished_predicate() {
        let aired: Vec<(u32, u32)> = vec![];
        // no wanters → false
        assert!(!trigger_a_finished(
            &[movie_record("a", 1, false, false, true)],
            &aired
        ));
        // empty list → false
        assert!(!trigger_a_finished(&[], &aired));
        // single wanter, finished → true
        assert!(trigger_a_finished(
            &[movie_record("a", 1, true, false, true)],
            &aired
        ));
        // one wanter unfinished → false
        assert!(!trigger_a_finished(
            &[
                movie_record("a", 1, true, false, true),
                movie_record("b", 1, true, false, false),
            ],
            &aired
        ));
        // a non-wanter who is unfinished must NOT block (only wanters count)
        assert!(trigger_a_finished(
            &[
                movie_record("a", 1, true, false, true),   // wanter, finished
                movie_record("b", 1, false, false, false), // not a wanter, ignored
            ],
            &aired
        ));
    }

    #[test]
    fn trigger_b_abandoned_predicate() {
        let empty: Vec<WantedRecord> = vec![];
        // watchlist prov + empty wanted → true
        assert!(trigger_b_abandoned(&empty, &Provenance::watchlist("alice")));
        // watchlist prov + a still-wanting user → false
        assert!(!trigger_b_abandoned(
            &[watchlist_movie_record("bob", 1, false)],
            &Provenance::watchlist("alice")
        ));
        // no watchlist prov + empty wanted → false (in-progress-only or manual origin)
        assert!(!trigger_b_abandoned(
            &empty,
            &Provenance::in_progress("alice")
        ));
        assert!(!trigger_b_abandoned(&empty, &Provenance::manual()));
        // watchlist prov + record present but all-false sources → true (un-watchlisted)
        assert!(trigger_b_abandoned(
            &[movie_record("alice", 1, false, false, false)],
            &Provenance::watchlist("alice")
        ));
    }

    #[test]
    fn reconcile_flat_maps_and_preserves_order() {
        let titles = vec![
            // wanted, not owned → AcquireMovie
            movie_title(10, vec![watchlist_movie_record("a", 10, false)], None),
            // finished + owned (non-manual) → Remove
            movie_title(
                1,
                vec![watchlist_movie_record("a", 1, true)],
                Some(owned("h", Provenance::watchlist("a"), true, vec![])),
            ),
            // nothing wanted, not owned → no actions
            movie_title(12, vec![], None),
        ];
        assert_eq!(
            reconcile(&titles),
            vec![
                Action::AcquireMovie { tmdb_id: 10 },
                Action::Remove {
                    tmdb_id: 1,
                    hash: "h".into()
                },
            ]
        );
    }

    #[test]
    fn show_not_wanted_owned_available_no_action() {
        // Nobody wants this show (empty wanted), but it is owned and available.
        // The Show arm in reconcile_title must NOT emit AcquireEpisode when nobody wants it —
        // the "nothing wanted → keep as-is" early-return fires before the Show branch.
        let t = show_title(
            20,
            vec![],
            Some(owned("h", Provenance::manual(), true, vec![(1, 1)])),
            vec![(1, 1)],
        );
        assert!(!should_remove(&t), "manual provenance blocks removal");
        assert_eq!(
            reconcile_title(&t),
            vec![],
            "nobody wants it → keep as-is, no AcquireEpisode"
        );
    }

    #[test]
    fn user_finished_dispatches_on_watched_state_not_media_type() {
        // Documents current behaviour: user_finished dispatches on `watched_state`, NOT `media_type`.
        // A WantedRecord whose media_type is Movie but whose watched_state is Show follows the
        // SHOW branch — meaning show_status + aired-episode coverage decides "finished".
        // This is an invariant-pinning test; do not change the code to match media_type instead.
        let r = WantedRecord {
            user: "alice".to_string(),
            tmdb_id: 99,
            media_type: MediaType::Movie, // media_type says Movie …
            sources: WantedSources {
                watchlist: true,
                in_progress: false,
            },
            watched_state: WatchedState::Show {
                watched_episodes: vec![(1, 1)],
            }, // … but state is Show
            show_status: Some(ShowStatus::Ended),
        };
        // Show branch: Ended + all aired [(1,1)] are in watched_episodes → finished = true.
        assert!(
            user_finished(&r, &[(1, 1)]),
            "user_finished dispatches on watched_state (Show branch), not media_type"
        );
    }
}
