# In-tick fall-through for movie + per-episode upgrades — design

- **Date:** 2026-07-10
- **Status:** Approved (brainstorming session, Kiril + Claude) — ready for implementation plan
- **Depends on:** the show-upgrade/ceiling-scoring feature (`docs/superpowers/specs/2026-07-09-show-upgrade-and-ceiling-scoring-design.md`, merged to `v2`) — this refines the `pick_best_change` selection it introduced
- **Branch:** `feat/upgrade-inline-fallthrough` (off `v2`)

## 1. Overview

The daily upgrade engine's movie path (`try_upgrade_movie`) and per-episode show
path (the singleton-episode loop in `upgrade_show_episodes_and_packs`) currently
select **only the single highest-scored** upgrade candidate (`pick_best_change`)
and stage it. If that one candidate fails a candidate-specific gate (multi-season
pack, missing episode, wrong title, not-actually-cached, probe reject), the tick
ends with no upgrade — and because the same candidate is re-scraped and re-picked
as best every subsequent tick, the slot is **permanently starved** of the genuine
second-best upgrade (not merely wasteful; the upgrade never happens).

The pack-correction (`correct_over_ceiling_packs`) and consolidation
(`consolidate_show_seasons`) paths already avoid this: they iterate ranked
candidates (`for cand in rank_cached_pack_candidates(…)`) and `continue` past a
rejection until one is adopted. This change brings the two single-best paths to
the **same in-tick fall-through** behaviour.

### Goals

- When the best upgrade candidate fails a candidate-specific gate, try the
  next-best **the same tick**, until one is adopted or the ranked list is
  exhausted — so a structurally-unusable top candidate can no longer block a
  legitimate upgrade.
- Match the existing pack/consolidation loop model exactly (one mental model).

### Non-goals

- **No per-tick attempt cap** (uncapped, matching the existing loops). Candidate
  lists are short in practice, and the pass is idle-gated + daily.
- **Does not eliminate the residual waste** — a structurally-bad candidate is
  still staged-and-rejected each daily tick. Removing that re-stage (pre-stage
  metadata filtering or a per-episode negative cache) is deliberately deferred
  (follow-up options 2/3 from the discussion).
- No change to `is_target_improvement`, `score()`, `effective_score`,
  `stage_and_verify`, `stage_episode_candidate`, `apply_upgrade`, the blacklist,
  consolidation, or the already-looping pack paths.
- No new config/env var; no user-facing behaviour change beyond "upgrades adopt
  the best *usable* candidate instead of giving up on the best *scored* one".

## 2. Decisions log

| # | Topic | Decision |
|---|-------|----------|
| D1 | Attempt cap | **Uncapped** — try ranked candidates until one is adopted or the list is exhausted, matching `correct_over_ceiling_packs` / `consolidate_show_seasons`. |
| D2 | Selection shape | Replace `pick_best_change` (returns one `Option`) with `rank_changes` (returns a best-first `Vec<(ReleaseInfo, Improvement)>`); callers loop. |
| D3 | Gated downgrade in the loop | `break` on the first gated-off downgrade (the ranked list is direction-homogeneous — see §4 — so all remaining are gated too; `break` logs exactly one preview instead of one per candidate). |
| D4 | Stage outcome handling | `Ok` → `apply_upgrade`, propagate its `Deferred` else return `Ok`; `Err(NoChange)` (candidate-specific) → `continue` to next; `Err(Deferred)` (transient provider/probe) → stop, return `Deferred` (retry the whole title next tick — preserves today's contract). |
| D5 | Idle check | Re-checked before staging each candidate (mirrors the pack loop's mid-loop defer); a non-idle library stops the loop with `Deferred`. `apply_upgrade`'s own pre-prune idle re-check is unchanged. |

## 3. Components (`src/upgrade.rs` only)

### 3.1 `rank_changes` (replaces `pick_best_change`)

```
async fn rank_changes(
    app: &AppState,
    media: MediaKind,
    tmdb_id: u64,
    raws: &[release::RawCandidate],
    baseline: &QualitySummary,
    owned_hashes: &[String],
) -> Vec<(release::ReleaseInfo, release::Improvement)>
```

Same per-candidate filtering as `pick_best_change` today (skip owned
case-insensitively, skip blacklisted, skip `score()==None`, keep only
`is_target_improvement != None`), but collects **all** qualifying candidates and
returns them **sorted by `score` descending** (best-first) instead of returning
the single max. Ties keep a stable order.

### 3.2 `try_upgrade_movie` — loop over `rank_changes`

Replace the single `pick_best_change` + stage + `apply_upgrade` block with:

- Build `ranked = rank_changes(app, MediaKind::Movie, tmdb_id, &raws, &current, owned_hashes)`.
- Empty → `Err(NoChange("no meaningful upgrade"))`.
- `old_hash = owned_hashes.first()` (as today, for the downgrade preview log).
- For each `(cand, imp)` in `ranked`:
  - `downgrade_allowed_or_log(...)==false` → `break` (D3).
  - `!all_idle(idle_window)` → `return Err(Deferred("library active; deferring upgrade"))` (D5).
  - `match stage_and_verify(app, tmdb_id, &req, &cand)`:
    - `Ok(staged)` → `apply_upgrade(app, StagedUpgrade{ hash, torrent_id, slot_files: [(movie_slot(tmdb_id), path)] }, owned_hashes, idle_window)?;` then `info!` swapped + `return Ok(())`.
    - `Err(UpgradeSkip::NoChange(_))` → `continue`.
    - `Err(UpgradeSkip::Deferred(r))` → `return Err(UpgradeSkip::Deferred(r))`.
- Loop exhausted → `Err(NoChange("no acceptable upgrade candidate"))`.

### 3.3 Per-episode loop in `upgrade_show_episodes_and_packs`

Currently, per singleton episode, it calls `pick_best_change` once and stages the
one result. Replace with the same inner loop as §3.2 but per episode: build
`rank_changes(app, MediaKind::Series, tmdb_id, &raws, &q, group_hashes)`; iterate
with the identical gate/idle/stage handling, using `stage_episode_candidate(app,
tmdb_id, &req, &cand, season, episode)` and repointing `episode_slot(tmdb, s, e)`
via `apply_upgrade` (superseding only that singleton hash). Preserve the existing
deferral semantics: a mid-loop non-idle stop must still make the whole
`upgrade_show_episodes_and_packs` return `true` (deferred) so `try_upgrade_show`
returns `Deferred` and the round-robin cursor stays unstamped (the behaviour added
in the previous feature). A per-episode `continue`/adopt does **not** defer the
show.

Because §3.2 and §3.3 share the same inner per-candidate logic, factor it into one
small helper (e.g. `try_rank_and_stage`/an inline closure) where it cleanly
reduces duplication — but staging differs (`stage_and_verify` vs
`stage_episode_candidate`) and the movie path returns a `Result` while the episode
loop feeds the show's deferral bool, so a shared *candidate-iteration* helper
parameterised by the stage step is the natural seam; do not force unification that
obscures those two differences.

## 4. Direction-homogeneity (why D3 `break` is correct)

`is_target_improvement(baseline, candidate, prefs)` compares each candidate
against the SAME baseline. Because resolution dominates `score()` within the
ceiling:
- **Within-ceiling baseline:** a candidate with lower resolution can't out-score
  it, so it isn't an improvement (filtered to `None`); every improvement is an
  `Upgrade`.
- **Over-ceiling baseline:** `effective_score` penalises it below every
  within-ceiling cached candidate, and every such candidate is lower-resolution,
  so every improvement is a `Downgrade`.

So a given tick's ranked list is all-`Upgrade` or all-`Downgrade`. If the top is a
gated-off downgrade, all are — `break` (one preview log) is correct; there is no
mixed list where `break` would skip a viable `Upgrade`.

## 5. Error handling

- `NoChange` vs `Deferred` semantics are unchanged at the source
  (`stage_and_verify` / `stage_episode_candidate` classify their own outcome);
  this change only decides the *control flow* over them (D4): `NoChange` advances
  to the next candidate, `Deferred` stops the title for this tick.
- Non-destructive throughout: every rejected candidate has already deleted its own
  staged torrent (existing behaviour in the stage helpers); `apply_upgrade` still
  prunes only after a verified swap. No new deletion path.

## 6. Testing (TDD)

Engine tests (mock provider/store/scraper, reuse existing harness/builders):

- **Movie fall-through:** top candidate is a multi-feature pack (→ `NoChange` +
  blacklist), second is a valid cached upgrade → the second is adopted **this
  tick** (movie selection = second hash; old pruned).
- **Per-episode fall-through:** top candidate lacks the target episode, second is
  valid → the episode slot repoints to the second this tick.
- **Exhaustion:** every candidate fails structurally → `NoChange`, selection and
  owned record untouched.
- **Transient short-circuit:** a `Deferred` (provider add failure or probe defer)
  on the top candidate → returns `Deferred`; the remaining candidates are NOT
  staged (assert via the mock provider's add/delete call count); for the show
  path, the deferral propagates (cursor unstamped).
- **Gated downgrade:** over-ceiling owned + `ALLOW_RESOLUTION_DOWNGRADE` off → the
  loop `break`s after one preview log; `NoChange`; nothing staged past the first.
- **Regression:** a single valid best candidate is still adopted (existing
  movie/episode upgrade tests stay green).

Docs: a one-line tweak to the `upgrade.rs` row in `CLAUDE.md` (the movie/episode
upgrade now iterates ranked candidates until one is usable, like the pack paths).
No env/behaviour change otherwise.
