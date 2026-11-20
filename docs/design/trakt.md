# Trakt sync and the removal lifecycle

## Trakt-driven desired state (SP2)

A persisted `wanted` set (per user, per **(media_type, tmdb_id)** — so a movie and a show that share a numeric TMDB id are tracked independently; which sources want it — watchlist / in-progress — plus watched-state and, for shows, TMDB status, **and the IMDB id Trakt supplied**) is materialised from each enrolled user's Trakt account by `sync_trakt`, from reads that follow **every page** (Trakt paginates the watchlist + watched endpoints by default since 2026; a failed page fails the read — see `docs/modules/trakt_client.md`) with watched shows requested as `extended=progress` (the only form that still carries per-episode watched state).

**Catch-up source:** a show you've *watched* (from `/sync/watched/shows`) is also folded into the wanted-set as an in-progress source (so its unwatched aired episodes are auto-acquired, every one of them, including episodes that aired before the last watch), gated by `TRAKT_CATCHUP_LOOKBACK` (last-watched within the window; default all-time) and skipped when the show is already watchlisted or in progress (those sources already want it, and the watchlist its whole run): no `show_progress` call is made for it, and it is never a finish-cleanup candidate on this path.
The catch-up membership is **precisely gated on Trakt per-show progress** (`GET /shows/{trakt_id}/progress/watched`): a within-lookback watched show is folded in **only if Trakt reports it has ≥1 unwatched aired episode** (`next_episode` non-null = behind), so fully-caught-up shows don't bloat the wanted-set or drive a fruitless reconcile each cycle.
The progress check runs in `sync_trakt_user` (one call per catch-up candidate; on a missing Trakt id / progress error it conservatively includes the show — degrades to lookback-only rather than dropping a possibly-behind title) and is passed to the PURE `build_wanted` as the fold set.

**Finish-cleanup fold:** a caught-up (not-behind) watched show that is **owned AND `Ended`** is a finish-removal candidate; `sync_trakt_user` LOGS these every sync (a preview) and — **only when `REMOVE_FINISHED_SHOWS` is set** — also folds them into the wanted-set so the episode monitor's (`monitor_episodes`) Trigger-A finish removal fires on its next run (every `TRAKT_EPISODE_CHECK_INTERVAL_SECS`, idle-gated) (removal + zero acquisition hold **when TMDB and Trakt agree on the aired set**; on a TMDB/Trakt aired-set disagreement the TMDB-authoritative reconcile keeps the show and catch-up acquires the genuinely-unwatched-per-TMDB episode instead — consistent, not a phantom acquire; in the opposite direction, when Trakt counts an aired episode TMDB lacks, Trakt reports the show behind, so it is in `behind`, marked `trakt_behind` and kept).
When the flag is off they're preview-only and never folded, so nothing is removed.
`finished_ended_candidates`/`shows_to_fold` are the pure helpers.
A catch-up show acquires only the **unwatched** aired episodes (you want what you haven't seen); a **watchlisted** show acquires **every** aired episode (the whole run, even already-watched, for a re-watch); and `reconcile_title`'s Show branch won't acquire a show all watchers have *finished* (symmetry with Trigger A — a watchlisted show is never "finished", so it still acquires).
**IMDB resolution prefers Trakt's id** (`WantedRecord.imdb_id`, carried through to the `Acquire` op) over re-deriving it from TMDB's `external_ids`, because TMDB's mapping is incomplete for some titles (e.g. a TVDB-only show) — `build_acquire_request` falls back to TMDB only when Trakt gave none, and returns `Ok(None)` (skip quietly, logged at debug — Torrentio is IMDB-keyed so it can't be scraped) when no IMDB id exists anywhere, rather than WARN-ing every reconcile tick.
A PURE reconcile-core (`wanted.rs`) diffs wanted-vs-owned-and-available and emits acquire/remove `Action`s, executed by `reconcile_wanted` (movies incl. their Trigger-A/B removal + show Trigger-B removal) and `monitor_episodes` (show episode acquisition + Trigger-A finish removal, via TMDB air dates).
There is **one shared household library**: acquisition is the union across users; removal is per-user-aware.

## Removal lifecycle

The Trakt lifecycle (Trigger A/B) removes only engine-acquired hashes (`removal_hashes` → `OwnedGroup.engine_hashes`), except a show finish-removed under `REMOVE_FINISHED_SHOWS`.
Account-mirror copies (torrents the user added; empty provenance) are otherwise kept by the lifecycle, but can still be removed by dedup, by the upgrade prune when a better copy supersedes them, by `REMOVE_FINISHED_SHOWS` finish-cleanup, and by `observe`'s dead-status reap (`fail_and_reacquire`: a Real-Debrid `magnet_error`/`dead`/`error`/`virus` status on a record with an IMDB id; not idle-gated, since such a torrent can't be streamed).
`Manual` provenance exists only on legacy records from the removed `--acquire` CLI; the Trakt lifecycle and dedup never remove it and the upgrade engine skips the whole title (M-1, `Provenance::has_manual_entry`), but a dead `Manual` torrent is still reaped by `observe` and re-acquired with its provenance kept.

**Trigger A (finished):** every user who **in-progresses** the title has finished it (movie watched; show = 100% of *aired* episodes watched AND its TMDB status is `Ended` (ended or cancelled — `ShowStatus::Ended`; a returning/in-production/planned show never qualifies even if fully watched)).
A show Trakt reports the user behind on (`WatchedState::Show.trakt_behind`, including one the sync assumed behind; see `docs/modules/wanted.md`) is never finished, so Trigger A needs TMDB and Trakt to agree.
Trigger A needs a current wanter.
Trakt drops a finished title from `/sync/playback`, and a title still there never reads as finished (`tasks::build_wanted` writes a playback movie as not watched and a playback show as `trakt_behind`: the user is partway through it, a paused re-watch included; for a paused re-watch, meaning a pause on an episode the user had watched before pausing it (its `WatchedShow.episode_watched_at` is earlier than the pause's `paused_at`), it also drops the earliest such paused episode and every later one from that row's watched set, so a re-watch begun without a Trakt progress reset acquires from the pause on, a lapsed episode included, instead of none of it.
A pause on an episode never watched (an abandoned first-watch pause) or watched only after the pause leaves the watched set alone, so a first watch acquires only its unwatched episodes, never the ones watched since), so in practice Trigger A fires only for a show folded back in by `REMOVE_FINISHED_SHOWS`; an in-progress-only movie whose wanted row is gone is kept (pinned by `wanted::tests::finished_in_progress_movie_no_longer_wanted_is_kept`).
**A watchlisted title is NEVER finished-removed** — the watchlist is an explicit "keep this available" signal (re-watchlisting something you've watched means you want to re-watch it), so it stays acquired regardless of watched state and `trigger_a_finished` short-circuits to `false` while any user watchlists it.
Without that guard a watched+watchlisted title oscillated acquire↔remove every reconcile tick (the acquire path ignores watched), flapping the library.

**Trigger B (abandoned):** a watchlist-provenance user un-watchlisted it AND no user currently wants it (via either source) — this is the sole removal path for a watchlisted title.
Otherwise the title is kept.
Provenance is **sticky** — recorded at acquire time (one entry per wanting user+source) and preserved across re-acquire, including any `Manual` entry.
Both removal triggers are executed through the **idle-gated** `execute_remove` (`UPGRADE_IDLE_SECS`), so a removal never tears content out from under an active stream (a non-idle library defers the removal to a later tick, leaving the owned record intact for retry).

## Availability re-acquire

A wanted title that is owned but no longer present in the provider listing is treated as lapsed (cache expired / lost) and proactively re-acquired.
For shows this is per episode: an episode counts as covered only while a record that supplies it is present or still `Pending`, so an episode whose only copy left the account is re-acquired even while the rest of the show is present (`plan_episode_actions`).

## Multi-user enrolment

Accounts are linked via device-flow OAuth on the local-network enrolment page (`/trakt/accounts`, served on the WebDAV listener — no auth, trusted-LAN model; the routes answer only for local-network `Host` names, plus `TRAKT_ENROL_ALLOWED_HOSTS`, reject cross-origin POSTs, and refuse to be framed (`frame-ancestors 'none'`, `X-Frame-Options: DENY`)).
Tokens are refreshed once they expire within `REFRESH_BUFFER_SECS` (1 h): Trakt access tokens last 24 h, and a longer buffer would refresh, and rotate the single-use refresh token, on every sync.
Tokens are keyed by Trakt **slug**.
A token-refresh or read failure — including a failure of **any page** of a paginated read, or a list seen to change mid-read (`docs/modules/trakt_client.md` names the one shift that isn't seen) — marks the account `needs_reenrolment` (shown on the page as "⚠ the last sync failed" with what to do; cleared by the next successful sync) and leaves its prior `wanted` set intact — the reconciler never acts on a failed or truncated fetch.
A transient refresh failure (a persistent 429, a 5xx, a transport failure or an incoherent answer: `trakt_client::refresh_error_is_transient`) skips the cycle without flagging `needs_reenrolment`, both in background sync and for the page's Refresh button; a 4xx such as `invalid_grant` still flags.
Remove is two-step: a GET confirmation page that warns what the next sync will delete (all watchlist-acquired content when it is the last account), then the POST that removes.
