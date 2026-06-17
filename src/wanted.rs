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
///
/// KNOWN LIMITATION (M2/L1): `available` is a SINGLE flag for the whole title and
/// `owned_episodes` is the union across all owning hashes — the reconcile-core cannot model
/// "episode S01E03's pack is present but S01E05's pack lapsed". The caller (Task 8) sets
/// `available` from the group's overall presence; if it folds episodes whose hash is absent
/// into `owned_episodes`, a lapsed per-episode pack reads as "covered" and is NOT re-acquired
/// until the WHOLE group reports unavailable. In practice the account-mirror/`provides` union
/// makes this benign (a present season pack covers the gap, and a fully-lapsed group flips
/// `available=false` → re-acquire); a per-hash `present_episodes` set would be the precise fix
/// but is deferred — it would require threading per-hash presence through the caller and is not
/// worth the churn given the union behaviour above.
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
    /// `owned_episodes` is the union across hashes. The `Action::Remove`'s `hash` field is only a
    /// representative — the executor maps the `RemoveReason` to the actual hash set to delete (via
    /// `tasks::removal_hashes`, which scopes by trigger: engine-acquired hashes only for `Abandoned`,
    /// and likewise for `Finished` except a show under `REMOVE_FINISHED_SHOWS`, which reclaims all).
    /// Assembling one `TitleView` per *hash* would make Trigger A misfire on partial coverage —
    /// assemble per *tmdb_id*.
    pub owned: Option<Owned>,
    /// Caller is responsible for de-duplicating; duplicate entries would yield duplicate AcquireEpisode actions.
    pub aired_episodes: Vec<(u32, u32)>, // shows: episodes aired as of "now" (from TMDB). movies: ignored
}

/// Why a title is being removed — determines WHICH owned hashes the executor deletes
/// (see `tasks::removal_hashes`).
/// - `Finished` (Trigger A): the title is fully watched + ended (and not watchlisted). It removes
///   EVERY owned hash — including pre-existing account-mirror copies — ONLY for a **show** under
///   `REMOVE_FINISHED_SHOWS` (the opt-in "incl. shows already in the library" cleanup). For a movie,
///   or a show with the flag off, it removes only the ENGINE-acquired hashes, so a genuine
///   in-progress re-watch never deletes the user's pre-existing library.
/// - `Abandoned` (Trigger B): a watchlist was un-watchlisted and nobody wants the title. This always
///   removes only the ENGINE-acquired hashes and KEEPs pre-existing mirror copies (un-watchlisting a
///   Trakt title must never delete content the user added to the account themselves).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveReason {
    Finished,
    Abandoned,
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
        reason: RemoveReason,
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
///
/// A **watchlisted** title is an explicit "keep this available" signal and is NEVER finished-removed:
/// re-adding a title you've already watched to your watchlist means you want to (re)watch it, so it
/// must stay acquired regardless of watched state. So Trigger A does not fire while ANY user
/// watchlists the title — it only reclaims *in-progress-only* titles (scrobbled but not watchlisted)
/// once everyone watching them has finished. A watchlisted title is removed solely via Trigger B
/// (un-watchlisted AND nobody wants it). Without this guard, a watched + watchlisted title oscillates
/// every reconcile tick — acquired (acquire ignores watched) then deleted (Trigger A) — flapping the
/// library.
pub fn trigger_a_finished(wanted: &[WantedRecord], aired: &[(u32, u32)]) -> bool {
    if wanted.iter().any(|r| r.sources.watchlist) {
        return false;
    }
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
            // Distinguish the trigger so the executor removes the right hash set: Trigger A
            // (finished) is a full cleanup incl. mirror copies; Trigger B (abandoned) keeps mirror.
            // should_remove == A || B, so if A didn't fire it must be B.
            let reason = if trigger_a_finished(&title.wanted, &title.aired_episodes) {
                RemoveReason::Finished
            } else {
                RemoveReason::Abandoned
            };
            return vec![Action::Remove {
                tmdb_id: title.tmdb_id,
                hash: owned.hash.clone(),
                reason,
            }];
        }
    }

    // Nothing wanted → keep as-is (kept manual/owned titles fall here too).
    if !title.wanted.iter().any(wants) {
        return vec![];
    }

    match title.media_type {
        MediaType::Movie => {
            // Symmetry with removal (mirrors the Show branch below): don't re-acquire a movie every
            // in-progress wanter has finished (watched, not watchlisted). Trigger A reclaims such a
            // movie; without this guard the acquire path re-adds it the very next tick, flip-flopping
            // acquire↔remove on a paused re-watch (still in Trakt /sync/playback AND /sync/watched) —
            // exactly the oscillation the watchlist guard prevents, but on the in-progress axis. The
            // watchlist short-circuit in `trigger_a_finished` keeps a watchlisted movie acquiring.
            // EXCEPTION: manual content is removal-protected (`has_manual_entry`), so it is never
            // reclaimed and never flip-flops — it must still re-acquire on lapse even when watched.
            let manual = title
                .owned
                .as_ref()
                .map(|o| o.provenance.has_manual_entry())
                .unwrap_or(false);
            if !manual && trigger_a_finished(&title.wanted, &title.aired_episodes) {
                return vec![];
            }
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
        MediaType::Show => {
            // Symmetry with removal (mirrors the Movie branch): don't acquire a show every watcher
            // has finished (an ended show, fully watched). The Trigger-A watchlist guard means a
            // watchlisted show is never "finished", so it still acquires — only in-progress-only
            // finished shows are skipped. EXCEPTION: manual content is removal-protected and never
            // flip-flops, so it is not suppressed here — it must stay available and restore its
            // previously-owned episodes if the cache lapses (see the `owned_eps` filter below).
            let manual = title
                .owned
                .as_ref()
                .map(|o| o.provenance.has_manual_entry())
                .unwrap_or(false);
            if !manual && trigger_a_finished(&title.wanted, &title.aired_episodes) {
                return vec![];
            }
            // Acquire aired episodes we don't already own. For a CATCH-UP show (wanted because it's
            // been watched, not via the watchlist) skip episodes already watched — you only want to
            // catch up on what you haven't seen. A WATCHLISTED show is an explicit "keep the whole
            // show", so it acquires every aired episode regardless of watched state.
            let watchlisted = title.wanted.iter().any(|r| r.sources.watchlist);
            // Household union-acquisition: for a catch-up show, skip an episode ONLY if EVERY
            // catch-up wanter has watched it (the INTERSECTION of their watched sets). An episode
            // unseen by any wanting user is still acquired so that user can catch up — a union here
            // would skip episodes one user watched elsewhere even though another hasn't seen them
            // (under-acquisition for multi-user households).
            let watched: std::collections::HashSet<(u32, u32)> = if watchlisted {
                std::collections::HashSet::new()
            } else {
                let per_user: Vec<std::collections::HashSet<(u32, u32)>> = title
                    .wanted
                    .iter()
                    // Only CURRENT wanters define the catch-up skip-set. A persisted but no-longer-
                    // wanting record (both sources false) must not narrow the intersection — its
                    // (often empty) watched set would otherwise force re-acquisition of episodes the
                    // real catch-up wanter has already seen. Matches the "EVERY catch-up wanter" doc.
                    .filter(|r| wants(r))
                    .filter_map(|r| match &r.watched_state {
                        WatchedState::Show { watched_episodes } => {
                            Some(watched_episodes.iter().copied().collect())
                        }
                        WatchedState::Movie { .. } => None,
                    })
                    .collect();
                match per_user.split_first() {
                    None => std::collections::HashSet::new(),
                    Some((first, rest)) => {
                        let mut acc = first.clone();
                        for s in rest {
                            acc.retain(|e| s.contains(e));
                        }
                        acc
                    }
                }
            };
            // Previously-owned episodes (for manual restore-on-lapse). A manual show is never
            // removed, so an episode it owned that has lapsed must be restored even though it's been
            // watched — but we do NOT auto-expand a manual show to aired episodes it never owned (it
            // is user-curated, not a catch-up subscription), so the restore is scoped to this set.
            let owned_eps: std::collections::HashSet<(u32, u32)> = title
                .owned
                .as_ref()
                .map(|o| o.owned_episodes.iter().copied().collect())
                .unwrap_or_default();
            title
                .aired_episodes
                .iter()
                .filter(|e| !episode_covered(title, e))
                .filter(|e| !watched.contains(e) || (manual && owned_eps.contains(e)))
                .map(|&(season, episode)| Action::AcquireEpisode {
                    tmdb_id: title.tmdb_id,
                    season,
                    episode,
                })
                .collect()
        }
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
            imdb_id: None,
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
            imdb_id: None,
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
        // An IN-PROGRESS-only (not watchlisted) movie that's been watched is reclaimed by Trigger A.
        let t = movie_title(
            1,
            vec![movie_record(
                "alice", 1, /*watchlist*/ false, /*in_progress*/ true,
                /*watched*/ true,
            )],
            Some(owned("h", Provenance::in_progress("alice"), true, vec![])),
        );
        assert!(should_remove(&t));
        assert_eq!(
            reconcile_title(&t),
            vec![Action::Remove {
                tmdb_id: 1,
                hash: "h".into(),
                reason: RemoveReason::Finished
            }]
        );
    }

    #[test]
    fn watchlisted_watched_movie_is_kept_not_trigger_a() {
        // A movie you've watched but kept on your watchlist must STAY acquired (re-watch): Trigger A
        // must not fire, and reconcile must not remove or (since owned+available) re-acquire it.
        let t = movie_title(
            1,
            vec![watchlist_movie_record("alice", 1, /*watched*/ true)],
            Some(owned("h", Provenance::watchlist("alice"), true, vec![])),
        );
        assert!(!should_remove(&t));
        assert_eq!(reconcile_title(&t), vec![]);
    }

    #[test]
    fn finished_ended_show_is_removed() {
        // An IN-PROGRESS-only (not watchlisted) ended show, fully watched, is reclaimed by Trigger A.
        let aired = vec![(1, 1), (1, 2)];
        let t = show_title(
            2,
            vec![show_record(
                "alice",
                2,
                /*watchlisted*/ false,
                /*in_progress*/ true,
                vec![(1, 1), (1, 2)],
                ShowStatus::Ended,
            )],
            Some(owned(
                "h",
                Provenance::in_progress("alice"),
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
                hash: "h".into(),
                reason: RemoveReason::Finished
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
    fn catchup_show_acquires_only_unwatched_aired_episodes() {
        // A show wanted via the CATCH-UP source (in-progress, not watchlisted): you've watched
        // s1e1+s1e2; s1e3 has since aired. Acquire ONLY the unwatched episode (catch up), not the
        // ones you've already seen.
        let aired = vec![(1, 1), (1, 2), (1, 3)];
        let t = show_title(
            5,
            vec![show_record(
                "alice",
                5,
                /*watchlisted*/ false,
                /*in_progress*/ true,
                vec![(1, 1), (1, 2)],
                ShowStatus::Returning,
            )],
            None,
            aired,
        );
        assert_eq!(
            reconcile_title(&t),
            vec![Action::AcquireEpisode {
                tmdb_id: 5,
                season: 1,
                episode: 3
            }]
        );
    }

    #[test]
    fn multi_user_catchup_acquires_episode_unseen_by_any_wanter() {
        // Household: alice watched s1e1+s1e2; bob watched only s1e1. s1e3 has aired, nothing owned.
        // Union-acquisition: acquire s1e2 (bob hasn't seen it) AND s1e3 (neither has). Only s1e1 —
        // watched by EVERYONE (the intersection) — is skipped. A union-of-watched rule would wrongly
        // skip s1e2 because alice saw it, denying bob the catch-up.
        let aired = vec![(1, 1), (1, 2), (1, 3)];
        let t = show_title(
            9,
            vec![
                show_record(
                    "alice",
                    9,
                    /*watchlisted*/ false,
                    /*in_progress*/ true,
                    vec![(1, 1), (1, 2)],
                    ShowStatus::Returning,
                ),
                show_record(
                    "bob",
                    9,
                    /*watchlisted*/ false,
                    /*in_progress*/ true,
                    vec![(1, 1)],
                    ShowStatus::Returning,
                ),
            ],
            None,
            aired,
        );
        assert_eq!(
            reconcile_title(&t),
            vec![
                Action::AcquireEpisode {
                    tmdb_id: 9,
                    season: 1,
                    episode: 2
                },
                Action::AcquireEpisode {
                    tmdb_id: 9,
                    season: 1,
                    episode: 3
                },
            ]
        );
    }

    #[test]
    fn catchup_skip_set_ignores_non_wanting_records() {
        // alice is the only current catch-up wanter (in_progress), having watched s1e1+s1e2. A stale,
        // no-longer-wanting record (both sources false, empty watched set) is still present in
        // `wanted`. It must NOT narrow the watched intersection — only alice's watched set defines the
        // skip, so only the unseen s1e3 is acquired. Before the fix, the stale record's empty set made
        // the intersection empty, wrongly re-acquiring alice's already-watched s1e1+s1e2.
        let aired = vec![(1, 1), (1, 2), (1, 3)];
        let t = show_title(
            11,
            vec![
                show_record(
                    "alice",
                    11,
                    /*watchlisted*/ false,
                    /*in_progress*/ true,
                    vec![(1, 1), (1, 2)],
                    ShowStatus::Returning,
                ),
                show_record(
                    "carol",
                    11,
                    /*watchlisted*/ false,
                    /*in_progress*/ false,  // no longer wants it
                    vec![], // and has watched nothing recorded
                    ShowStatus::Returning,
                ),
            ],
            None,
            aired,
        );
        assert_eq!(
            reconcile_title(&t),
            vec![Action::AcquireEpisode {
                tmdb_id: 11,
                season: 1,
                episode: 3
            }],
            "only the unseen episode should be acquired; a non-wanting record must not narrow the skip-set"
        );
    }

    #[test]
    fn watchlisted_show_acquires_all_aired_even_already_watched() {
        // A WATCHLISTED show is "keep the whole show": acquire every aired episode, including ones
        // already watched (for a re-watch), unlike the catch-up source.
        let aired = vec![(1, 1), (1, 2)];
        let t = show_title(
            6,
            vec![show_record(
                "alice",
                6,
                /*watchlisted*/ true,
                /*in_progress*/ false,
                vec![(1, 1)], // watched s1e1
                ShowStatus::Returning,
            )],
            None,
            aired,
        );
        assert_eq!(
            reconcile_title(&t),
            vec![
                Action::AcquireEpisode {
                    tmdb_id: 6,
                    season: 1,
                    episode: 1
                },
                Action::AcquireEpisode {
                    tmdb_id: 6,
                    season: 1,
                    episode: 2
                },
            ]
        );
    }

    #[test]
    fn catchup_finished_ended_show_is_not_acquired() {
        // A fully-watched ENDED show surfaced only via catch-up is finished → don't acquire it (the
        // symmetry guard), so it can't oscillate acquire/remove.
        let aired = vec![(1, 1), (1, 2)];
        let t = show_title(
            7,
            vec![show_record(
                "alice",
                7,
                /*watchlisted*/ false,
                /*in_progress*/ true,
                vec![(1, 1), (1, 2)], // all aired watched
                ShowStatus::Ended,
            )],
            None,
            aired,
        );
        assert_eq!(reconcile_title(&t), vec![]);
    }

    #[test]
    fn manual_lapsed_finished_show_restores_owned_episodes_only() {
        // Symmetric with the manual movie case: a manual (removal-protected) ENDED show that's been
        // fully watched and whose cache lapsed must restore its PREVIOUSLY-OWNED episodes (stay
        // available) — the Trigger-A acquire-suppression is skipped for manual content (it is never
        // removed, so it cannot flip-flop). It must NOT auto-expand to aired episodes it never owned
        // ((1,3) here): manual content is user-curated, not a catch-up subscription.
        let aired = vec![(1, 1), (1, 2), (1, 3)];
        let t = show_title(
            30,
            vec![show_record(
                "alice",
                30,
                /*watchlisted*/ false,
                /*in_progress*/ true,
                /*watched_eps*/ vec![(1, 1), (1, 2), (1, 3)],
                ShowStatus::Ended,
            )],
            Some(owned(
                "h",
                Provenance::manual(),
                /*available*/ false,
                /*owned_episodes*/ vec![(1, 1), (1, 2)],
            )),
            aired,
        );
        assert!(!should_remove(&t)); // manual content is never removed
        assert_eq!(
            reconcile_title(&t),
            vec![
                Action::AcquireEpisode {
                    tmdb_id: 30,
                    season: 1,
                    episode: 1
                },
                Action::AcquireEpisode {
                    tmdb_id: 30,
                    season: 1,
                    episode: 2
                },
            ]
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
                    hash: "h".into(),
                    reason: RemoveReason::Abandoned
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
                    hash: "h".into(),
                    reason: RemoveReason::Abandoned
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
        // IN-PROGRESS-only records (no watchlist) so the watchlist guard does NOT short-circuit
        // `trigger_a_finished` — the block must come from the user_finished loop seeing bob unfinished.
        let t = movie_title(
            9,
            vec![
                movie_record("alice", 9, false, true, true), // in-progress, finished
                movie_record("bob", 9, false, true, false),  // in-progress, mid-watch
            ],
            Some(owned("h", Provenance::in_progress("alice"), true, vec![])),
        );
        assert!(
            !should_remove(&t),
            "an unfinished in-progress wanter (bob) must block Trigger A (not the watchlist guard)"
        );
        // Sanity: with bob ALSO finished, Trigger A fires (proving the block above was bob, not a
        // structural always-false) — this is the discriminating case the old all-watchlist test missed.
        let all_finished = movie_title(
            9,
            vec![
                movie_record("alice", 9, false, true, true),
                movie_record("bob", 9, false, true, true),
            ],
            Some(owned("h", Provenance::in_progress("alice"), true, vec![])),
        );
        assert!(
            should_remove(&all_finished),
            "all in-progress wanters finished → Trigger A removes"
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
    fn inprogress_watched_movie_not_reacquired_after_reclaim() {
        // Symmetry with Trigger-A removal (mirrors the Show branch guard): a not-watchlisted movie
        // that every in-progress wanter has watched is reclaimed by Trigger A. Once removed it is no
        // longer owned; the acquire path must NOT re-add it, or it flip-flops acquire↔remove every
        // reconcile tick (Trigger A removes a re-watched-and-paused movie that's still in Trakt
        // /sync/playback AND /sync/watched). The intended end state is "reclaimed" — re-watch via the
        // watchlist, which short-circuits Trigger A.
        let t = movie_title(
            20,
            vec![movie_record(
                "alice", 20, /*watchlist*/ false, /*in_progress*/ true,
                /*watched*/ true,
            )],
            None,
        );
        assert_eq!(reconcile_title(&t), vec![]);
    }

    #[test]
    fn manual_lapsed_watched_movie_still_reacquired() {
        // Manual content is removal-protected (has_manual_entry → should_remove false), so it reaches
        // the Movie branch even when watched+in-progress. The symmetry guard must NOT block it: a
        // manually-curated movie whose cache lapsed must still be re-acquired to stay available, even
        // though it's been watched — there is no flip-flop for manual content (it is never removed).
        let t = movie_title(
            21,
            vec![movie_record(
                "alice", 21, /*watchlist*/ false, /*in_progress*/ true,
                /*watched*/ true,
            )],
            Some(owned(
                "h",
                Provenance::manual(),
                /*available*/ false,
                vec![],
            )),
        );
        assert!(!should_remove(&t));
        assert_eq!(
            reconcile_title(&t),
            vec![Action::AcquireMovie { tmdb_id: 21 }]
        );
    }

    #[test]
    fn inprogress_unwatched_movie_acquires() {
        // An in-progress (not watchlisted), NOT-yet-watched movie is a normal acquire — the symmetry
        // guard only fires once the movie is finished (watched), so first-watch acquisition is intact.
        let t = movie_title(
            22,
            vec![movie_record(
                "alice", 22, /*watchlist*/ false, /*in_progress*/ true,
                /*watched*/ false,
            )],
            None,
        );
        assert_eq!(
            reconcile_title(&t),
            vec![Action::AcquireMovie { tmdb_id: 22 }]
        );
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
        // a WATCHLISTED title is never finished-removed, even fully watched → false (keep it)
        assert!(!trigger_a_finished(
            &[movie_record(
                "a", 1, /*watchlist*/ true, false, /*watched*/ true
            )],
            &aired
        ));
        // single IN-PROGRESS-only wanter, finished → true
        assert!(trigger_a_finished(
            &[movie_record("a", 1, false, /*in_progress*/ true, true)],
            &aired
        ));
        // one in-progress wanter unfinished → false
        assert!(!trigger_a_finished(
            &[
                movie_record("a", 1, false, true, true),
                movie_record("b", 1, false, true, false),
            ],
            &aired
        ));
        // a non-wanter who is unfinished must NOT block (only wanters count)
        assert!(trigger_a_finished(
            &[
                movie_record("a", 1, false, true, true), // in-progress wanter, finished
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
            // finished + owned (non-manual), in-progress-only (not watchlisted) → Remove
            movie_title(
                1,
                vec![movie_record(
                    "a", 1, /*watchlist*/ false, /*in_progress*/ true, true,
                )],
                Some(owned("h", Provenance::in_progress("a"), true, vec![])),
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
                    hash: "h".into(),
                    reason: RemoveReason::Finished
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
            imdb_id: None,
        };
        // Show branch: Ended + all aired [(1,1)] are in watched_episodes → finished = true.
        assert!(
            user_finished(&r, &[(1, 1)]),
            "user_finished dispatches on watched_state (Show branch), not media_type"
        );
    }
}
