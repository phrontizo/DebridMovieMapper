# Show quality upgrade/downgrade + ceiling-aware scoring — design

- **Date:** 2026-07-09
- **Status:** Approved (brainstorming session, Kiril + Claude) — ready for implementation plan
- **Depends on:** SP3 (`docs/superpowers/specs/2026-06-09-sp3-upgrade-reconciliation-design.md`) — the upgrade engine, `selection` table, `QualitySummary`, and `is_meaningful_upgrade` it introduced
- **Branch:** `feat/show-upgrade-ceiling-downgrade` (off `v2`)

## 1. Overview

Two coupled changes to the daily upgrade engine (`upgrade.rs`) and the scoring
core (`release.rs`):

1. **Shows get the same quality-upgrade path as movies.** Today `try_upgrade_movie`
   does genuine quality upgrades (resolution/tier) while shows only ever
   *consolidate* scattered episodes into a full-season cached pack at
   same-or-better quality. Shows must **upgrade and downgrade individual episodes
   on their own — before any season pack is available** — reusing the movie
   machinery, with pack consolidation kept as an additional step layered on top.

2. **Ceiling-aware scoring enables corrective downgrades.** When `MAX_RESOLUTION`
   is lowered below what is already owned, the upgrade engine must recognise the
   existing above-ceiling copy as *worse* than a ceiling-conforming release and
   replace it (idle-gated, non-destructive stage-then-swap). Because this is a
   destructive action driven by a config change, it is **gated behind an opt-in
   flag with a dry-run/preview default**.

### Goals

- Reuse the movie upgrade primitive for shows at **per-episode (per-slot)**
  granularity, working before a season pack exists.
- Keep full-season **consolidation** as an add-on step, unchanged in behaviour.
- Correct **over-ceiling** owned copies (movies and shows) down to a
  ceiling-conforming release when the ceiling is lowered.
- Gate destructive downgrades behind `ALLOW_RESOLUTION_DOWNGRADE` (dry-run default).
- Preserve every existing safety property: idle-gated, non-destructive
  stage-then-swap, provider-neutral, never touch non-owned torrents, never delete
  a sole copy when no conforming replacement exists.
- No DB migration: react to a ceiling change from the *stored* `QualitySummary`.

### Non-goals

- No change to **acquisition** behaviour: `release::score()` keeps its hard
  above-ceiling filter, so acquisition never acquires or ranks an above-ceiling
  release. The ceiling penalty lives only in the upgrade comparison.
- No speculative downloading — upgrade/downgrade targets must be **cached** (as today).
- No partial-season pack correction beyond what per-episode + full-season
  consolidation already cover.
- No auto-correction of copies whose quality is **untagged** (`quality: None`) —
  they have no comparable baseline (unchanged conservative skip).

## 2. Decisions log

Every row was settled during the brainstorm and is binding on the plan.

| # | Topic | Decision |
|---|-------|----------|
| D1 | Downgrade gating | Opt-in env flag `ALLOW_RESOLUTION_DOWNGRADE` (default false ⇒ dry-run/preview log only), mirroring `DEDUP_REMOVE_DUPLICATES` / `REMOVE_FINISHED_SHOWS`. Pure *upgrades* ignore the flag. |
| D2 | Scoring locus | Keep `release::score()`'s hard above-ceiling filter (acquisition unchanged). Add a separate **effective score** applied only to the *owned* copy in the upgrade comparison. |
| D3 | Show granularity | Per-**episode** upgrade/downgrade of singleton-held episodes, independent of and prior to season packs. Over-ceiling **packs** corrected at pack level. Consolidation unchanged, layered after. |
| D4 | Structure | Approach 1 — generalise the movie primitive into a per-*slot* `apply_upgrade`; movie/episode/pack map onto it. (Not the minimal near-copy #2, nor the full generic refcount engine #3.) |
| D5 | Convergence | Within a fixed ceiling every accepted swap strictly increases the owned effective score (bounded) ⇒ no flip-flop. A corrective downgrade lands on a within-ceiling copy, after which no further change qualifies. |

## 3. Components

### 3.1 `release::effective_score(&QualitySummary, &QualityPrefs) -> i64` (new, pure)

- When `summary.resolution <= prefs.max_resolution.height()`: returns
  `summary.score` unchanged.
- When above the ceiling: subtracts a penalty that (a) scales with how far over
  the ceiling the copy is, and (b) is large enough to place it strictly **below
  any within-ceiling cached release** and below a *less*-over-ceiling copy.
  - Concretely: `score - 2 * (resolution as i64) * 100 - OVER_CEILING_FLOOR`,
    where `OVER_CEILING_FLOOR` dominates the sum of the small additive bonuses
    (HEVC/HDR/container/bitrate/seeders ≤ ~16k). This flips the resolution term
    negative so a higher over-ceiling resolution scores *lower* (most-over-ceiling
    corrected first), and guarantees `effective_score(over-ceiling) <
    score(any within-ceiling cached candidate)`.
  - Exact constants pinned by unit tests (§6), not by prose.
- Computed from the **stored** `resolution` against the **current** ceiling ⇒ a
  ceiling change is picked up with no record rewrite / no DB migration.

### 3.2 `release::is_target_improvement(current, candidate, prefs) -> Improvement` (replaces `is_meaningful_upgrade`)

- Returns an enum/struct distinguishing **None**, **Upgrade**, and **Downgrade**
  (the latter = a resolution regression that only qualifies because `current` is
  over-ceiling), so the executor can gate downgrades (D1).
- Qualifies when: `candidate.cached`; and (`!current.cached` ⇒ Upgrade); else
  `effective_score(candidate) > effective_score(current)` **and** a category
  change (`candidate.source_tier != current.source_tier || candidate.resolution
  != current.resolution`) — the category clause rejects marginal
  same-tier/same-res wobble (bitrate/HEVC/HDR/seeders).
- Direction: `candidate.resolution < current.resolution` ⇒ Downgrade; otherwise
  Upgrade. (`candidate` is always within-ceiling by construction — it passed
  `score()`'s filter — so its effective score equals its raw score.)
- `is_meaningful_upgrade`'s existing call sites and tests are migrated to the new
  fn; the old name is removed.

### 3.3 Config: `ALLOW_RESOLUTION_DOWNGRADE` (`config.rs`)

- Bool env, parsed like the other truthy flags (`true`/`1`/`yes`/`on`), default
  false. Lives on the upgrade config alongside `UPGRADE_*`.
- False ⇒ a `Downgrade` change is **logged and skipped**:
  `upgrade: tmdb {id} would downgrade {hash}({res}p) -> {cand}({res}p) [set ALLOW_RESOLUTION_DOWNGRADE=true to enable]`.
- True ⇒ a `Downgrade` proceeds through the normal idle-gated stage→swap→prune.

### 3.4 Shared upgrade core: `apply_upgrade(...)` (`upgrade.rs`)

Extract the tail of `try_upgrade_movie` (scrape-filtered candidate → idle gate →
`stage_and_verify` → swap `selection` → prune superseded, with its existing
listing-fetch ordering and rollback discipline) into a slot-parameterised
primitive. Parameters:

- `slots`: the `selection` slot(s) to repoint on success.
- `supersede_hashes`: the owned hash(es) to prune once the swap persists.
- `baseline`: the current owned `QualitySummary` (via `best_owned_quality`, which
  is updated to compare with `effective_score`).
- `scrape_query`: movie (imdb) vs episode (imdb+season+episode).
- `validate` closure: movie pack-guard + title-validate vs episode SE-match +
  title-validate (both already exist as helpers).
- `coverage`: the candidate must supply exactly the target unit.
- `Improvement` handling: a `Downgrade` is gated on D1; an `Upgrade` always runs.

Callers:

- **Movie** — `try_upgrade_movie` becomes a thin `apply_upgrade` call: one
  `movie_slot(tmdb_id)`, supersede = all owned movie hashes. Behaviour identical.
- **Show, per singleton episode** — new `try_upgrade_show` iterates each owned
  episode held as a singleton (`provides.len() == 1`) and calls `apply_upgrade`
  at `episode_slot(tmdb, s, e)`, supersede = that episode's singleton hash. This
  is safe to prune (a singleton supplies exactly its own episode) and runs before
  any pack exists (D3).
- **Show, over-ceiling pack** — when a pack's `QualitySummary` is over the
  ceiling, correct it at pack level: find a within-ceiling cached full-season
  replacement, repoint all its episode slots, prune the old pack (a generalised
  consolidation acceptance rule reusing the existing repoint/prune body). Gated
  on D1 as a downgrade.
- **Consolidation** — `try_consolidate_show` runs unchanged, *after* the
  per-episode upgrade pass, still merging scattered episodes into a full-season
  pack at same-or-better quality. Its raw resolution/tier no-regression gate is
  **left as-is** and never uses `effective_score` — otherwise an over-ceiling
  owned episode would score low enough that a within-ceiling pack looked like "no
  regression", turning consolidation into an *ungated* downgrade path. Over-ceiling
  packs are corrected **only** by the flag-gated pack-correction step above.

Each title (movie or show) remains **one round-robin budget unit**
(`UPGRADE_BUDGET_PER_TICK`); per-slot work inside a show is idle-gated exactly as
today (idle re-checked immediately before every destructive prune).

## 4. Data flow (show upgrade tick, illustrative)

1. Round-robin picks a show `tmdb_id`; library-wide idle gate as today.
2. For each **singleton** episode slot: scrape (imdb+s+e) → filter through
   `score()` (drops above-ceiling candidates) → pick best
   `is_target_improvement` → if `Downgrade` and flag off, log+skip → else
   idle-gate → `stage_and_verify` → repoint the episode slot → prune the old
   singleton.
3. For each **pack** whose stored quality is over-ceiling (flag-gated): find a
   within-ceiling full-season cached replacement → stage → repoint all its
   episode slots → prune the old pack. If none exists, keep the pack (never delete
   a sole copy).
4. Run consolidation (unchanged).
5. Stamp the round-robin cursor on completion; `Deferred` (transient) leaves the
   cursor unchanged (existing semantics).

## 5. Error handling & safety properties (all preserved)

- **Non-destructive stage-then-swap:** the superseded hash is pruned only after
  the replacement is added, verified cached, probed, and its selection repoint has
  persisted — so a lowered ceiling with **no** conforming cached release available
  leaves the over-ceiling copy intact (sole-copy safety).
- **Idle-gated:** every destructive prune is preceded by an immediate idle
  re-check; a mid-stage active read fully rolls back the staged candidate.
- **Provider-neutral; owned-only:** unchanged — never touches a non-owned torrent.
- **Transient vs terminal:** scrape/add/info failures → `Deferred` (retry, cursor
  unchanged); candidate-specific failures → `NoChange` (cursor advances). Same as
  the movie path today.
- **Convergence (D5):** bounded effective score, strict-gain + category-change
  gate ⇒ no oscillation, up or down.

## 6. Testing (TDD)

Pure-fn unit tests (no I/O):

- `effective_score`: below/at ceiling (unchanged); above ceiling (penalised); an
  over-ceiling copy scores strictly below every within-ceiling cached release;
  more-over-ceiling scores below less-over-ceiling.
- `is_target_improvement`: real upgrade (tier/res gain) ⇒ Upgrade; over-ceiling
  current + within-ceiling cached candidate ⇒ Downgrade; same-tier/same-res
  wobble ⇒ None; uncached candidate ⇒ None; uncached current ⇒ Upgrade.

Engine tests (mock provider/store, as in existing `upgrade.rs` tests):

- Singleton-episode upgrade: better cached release swaps the episode slot, prunes
  the old singleton.
- Corrective downgrade **dry-run** (flag off): logs, performs **no** swap/prune.
- Corrective downgrade **flag on**: stages within-ceiling replacement, swaps,
  prunes the over-ceiling copy.
- Over-ceiling **pack** correction: repoints all episode slots, prunes old pack.
- **Sole-copy safety:** over-ceiling copy + no conforming cached candidate ⇒ kept.
- Untagged-quality copy ⇒ skipped (no baseline).

Docs: update `CLAUDE.md` (upgrade.rs row, upgrade design-decision paragraph,
env-var list) and `README.md` (env-var table) for `ALLOW_RESOLUTION_DOWNGRADE` and
the show upgrade/downgrade behaviour.

## 7. Limitations

- A pre-existing/mirror copy with `quality: None` (untagged resolution or source
  tier) is not auto-corrected — no comparable baseline.
- Per-episode scraping adds one scrape per singleton episode per budgeted show
  tick (same cost profile as `monitor_episodes`); bounded by
  `UPGRADE_BUDGET_PER_TICK` and the daily cadence.
- Over-ceiling pack correction requires a within-ceiling **full-season cached**
  replacement pack to exist; if only scattered within-ceiling episodes exist, the
  over-ceiling pack is kept until a conforming pack appears (no fragmentation).
