# Show quality upgrade/downgrade + ceiling-aware scoring — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give shows the same per-episode quality upgrade/downgrade path as movies (keeping season-pack consolidation as an add-on), and add a ceiling-aware "effective score" that lets the upgrade engine correct existing above-ceiling copies down to a conforming release, gated behind an opt-in flag.

**Architecture:** Two pure functions in `release.rs` (`effective_score`, `is_target_improvement`) express the scoring change; a `Config` flag (`allow_resolution_downgrade`) gates destructive downgrades; `upgrade.rs` grows a shared candidate-selection helper (`pick_best_change`) + a shared swap/prune tail (`apply_upgrade`) that the movie path, a new per-episode show path, and over-ceiling pack correction all feed. Consolidation is unchanged and layered after the per-episode pass.

**Tech Stack:** Rust (async, tokio), `redb` store, existing `release`/`upgrade`/`config` modules. TDD with `cargo test`.

**Spec:** `docs/superpowers/specs/2026-07-09-show-upgrade-and-ceiling-scoring-design.md`

## Global Constraints

- Follow existing patterns; do not restructure unrelated code.
- Lint gate before every commit: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`.
- `cargo test` green before every commit.
- Commit author identity is already configured (`Kiril <54142726+phrontizo@users.noreply.github.com>`); end every commit message with:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- `score()`'s hard above-ceiling filter (`release.rs:289`) is **untouched** — acquisition never acquires/ranks above-ceiling (spec D2).
- Consolidation's raw resolution/tier no-regression gate (`consolidation_target`, `upgrade.rs:567`) is **untouched** — it must never use `effective_score` (spec §3.4).
- Destructive downgrades are gated on `allow_resolution_downgrade`; pure upgrades ignore the flag (spec D1).
- Never delete a sole copy when no conforming cached replacement exists (inherent to stage-then-swap).
- `QualitySummary` fields (defined in `release.rs`): `cached: bool`, `source_tier: i64`, `resolution: u16`, `score: i64`.
- `MaxResolution::height() -> u16` (config.rs:14); `QualityPrefs.max_resolution` (config.rs:77).

---

### Task 1: `effective_score` — ceiling-aware owned-copy score

**Files:**
- Modify: `src/release.rs` (add fn after `is_meaningful_upgrade`, near line 420)
- Test: `src/release.rs` (in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `QualitySummary` (release.rs), `QualityPrefs`/`MaxResolution` (config.rs).
- Produces: `pub fn effective_score(q: &QualitySummary, prefs: &QualityPrefs) -> i64`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/release.rs`:

```rust
#[cfg(test)]
mod effective_score_tests {
    use super::{effective_score, QualitySummary};
    use crate::config::{AudioReq, MaxResolution, QualityPrefs, SubReq};

    fn prefs(ceiling: MaxResolution) -> QualityPrefs {
        QualityPrefs {
            max_resolution: ceiling,
            audio: AudioReq::Original,
            subtitle: SubReq::None,
            prefer_hevc: true,
            prefer_hdr: false,
        }
    }

    // A cached release scored by score(): CACHED_BONUS + tier + resolution*100 (+ bonuses).
    fn cached(res: u16, tier: i64) -> QualitySummary {
        QualitySummary {
            cached: true,
            source_tier: tier,
            resolution: res,
            score: 1_000_000 + tier + res as i64 * 100,
        }
    }

    #[test]
    fn within_ceiling_is_unchanged() {
        let q = cached(1080, 6_000);
        assert_eq!(effective_score(&q, &prefs(MaxResolution::P1080)), q.score);
        // At the ceiling exactly is still "within".
        assert_eq!(effective_score(&q, &prefs(MaxResolution::P2160)), q.score);
    }

    #[test]
    fn above_ceiling_scores_below_any_within_ceiling_cached() {
        let over = cached(2160, 8_000); // 4K REMUX, ceiling 1080
        let p = prefs(MaxResolution::P1080);
        let over_eff = effective_score(&over, &p);
        // Worst-case comparators that are WITHIN the 1080 ceiling and cached:
        for within in [cached(1080, 8_000), cached(1080, 3_000), cached(480, 1_000)] {
            assert!(
                over_eff < effective_score(&within, &p),
                "over-ceiling {over_eff} must be below within-ceiling {}",
                within.score
            );
        }
    }

    #[test]
    fn more_over_ceiling_scores_below_less_over_ceiling() {
        // ceiling 720: 2160 is "more over" than 1080.
        let p = prefs(MaxResolution::P720);
        let two_k = cached(2160, 8_000);
        let one_k = cached(1080, 8_000);
        assert!(effective_score(&two_k, &p) < effective_score(&one_k, &p));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib release::effective_score_tests 2>&1 | tail -20`
Expected: FAIL — `cannot find function effective_score in this scope`.

- [ ] **Step 3: Implement `effective_score`**

Add after `is_meaningful_upgrade` in `src/release.rs`:

```rust
/// Ceiling-aware score for an OWNED copy. Within the ceiling it is `q.score` unchanged; ABOVE the
/// ceiling it is heavily penalised so a within-ceiling cached release always outscores it (enabling
/// a corrective downgrade), and a more-over-ceiling copy scores below a less-over-ceiling one (so the
/// worst offender is corrected first). Used ONLY by the upgrade comparison — `score()`'s hard
/// above-ceiling filter (acquisition) is deliberately untouched.
pub fn effective_score(q: &QualitySummary, prefs: &QualityPrefs) -> i64 {
    let ceiling = prefs.max_resolution.height();
    if q.resolution <= ceiling {
        return q.score;
    }
    // score() added `resolution * 100`; subtract twice that to flip the resolution term negative
    // (higher over-ceiling resolution => lower effective score), plus a floor that dominates the
    // summed small additive bonuses (HEVC/HDR/container/bitrate/seeders <= ~16k) and the max source
    // tier (8k), so the result is strictly below ANY within-ceiling cached release regardless of tier.
    const OVER_CEILING_FLOOR: i64 = 100_000;
    q.score - 2 * (q.resolution as i64) * 100 - OVER_CEILING_FLOOR
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib release::effective_score_tests 2>&1 | tail -20`
Expected: PASS (3 tests).

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
git add src/release.rs
git commit -m "feat(release): ceiling-aware effective_score for over-ceiling owned copies

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `is_target_improvement` — unify upgrade + corrective downgrade

**Files:**
- Modify: `src/release.rs` (replace `is_meaningful_upgrade`, line 410; keep its doc comment intent)
- Modify: `src/upgrade.rs:236` (the sole non-test call site)
- Test: `src/release.rs` (migrate the existing `is_meaningful_upgrade` tests at ~950–1260 to the new fn + add downgrade cases)

**Interfaces:**
- Consumes: `effective_score` (Task 1), `QualitySummary`, `QualityPrefs`.
- Produces:
  - `pub enum Improvement { None, Upgrade, Downgrade }` (derive `Debug, Clone, Copy, PartialEq, Eq`)
  - `pub fn is_target_improvement(current: &QualitySummary, candidate: &QualitySummary, prefs: &QualityPrefs) -> Improvement`
- Removes: `pub fn is_meaningful_upgrade(...)`.

- [ ] **Step 1: Write/adapt the failing tests**

In `src/release.rs`, replace the two `is_meaningful_upgrade` test modules' imports and assertions. Every `assert!(is_meaningful_upgrade(a, b))` becomes `assert_eq!(is_target_improvement(a, b, &prefs()), Improvement::Upgrade)` and every `assert!(!is_meaningful_upgrade(a, b))` becomes `assert_eq!(is_target_improvement(a, b, &prefs()), Improvement::None)`. The existing `prefs()` helper in those modules provides a `P1080` ceiling — keep it. Then add a dedicated downgrade module:

```rust
#[cfg(test)]
mod target_improvement_tests {
    use super::{is_target_improvement, Improvement, QualitySummary};
    use crate::config::{AudioReq, MaxResolution, QualityPrefs, SubReq};

    fn prefs(ceiling: MaxResolution) -> QualityPrefs {
        QualityPrefs { max_resolution: ceiling, audio: AudioReq::Original,
            subtitle: SubReq::None, prefer_hevc: true, prefer_hdr: false }
    }
    fn cached(res: u16, tier: i64) -> QualitySummary {
        QualitySummary { cached: true, source_tier: tier, resolution: res,
            score: 1_000_000 + tier + res as i64 * 100 }
    }

    #[test]
    fn over_ceiling_current_and_within_candidate_is_downgrade() {
        // Own 2160; ceiling dropped to 1080; a cached 1080 exists.
        let cur = cached(2160, 8_000);
        let cand = cached(1080, 6_000);
        assert_eq!(is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                   Improvement::Downgrade);
    }

    #[test]
    fn genuine_higher_resolution_is_upgrade() {
        let cur = cached(1080, 3_000);
        let cand = cached(2160, 3_000);
        assert_eq!(is_target_improvement(&cur, &cand, &prefs(MaxResolution::P2160)),
                   Improvement::Upgrade);
    }

    #[test]
    fn same_tier_same_res_wobble_is_none() {
        let cur = cached(1080, 3_000);
        let mut cand = cached(1080, 3_000);
        cand.score += 500; // bitrate/HEVC wobble only, no category change
        assert_eq!(is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                   Improvement::None);
    }

    #[test]
    fn uncached_candidate_is_none() {
        let cur = cached(1080, 3_000);
        let mut cand = cached(2160, 8_000);
        cand.cached = false;
        assert_eq!(is_target_improvement(&cur, &cand, &prefs(MaxResolution::P2160)),
                   Improvement::None);
    }

    #[test]
    fn uncached_current_any_cached_is_upgrade() {
        let mut cur = cached(1080, 3_000);
        cur.cached = false;
        let cand = cached(720, 1_000);
        assert_eq!(is_target_improvement(&cur, &cand, &prefs(MaxResolution::P1080)),
                   Improvement::Upgrade);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib release:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function is_target_improvement` / `cannot find value Improvement`.

- [ ] **Step 3: Replace `is_meaningful_upgrade` with `is_target_improvement`**

In `src/release.rs`, delete `pub fn is_meaningful_upgrade(...)` (lines ~410–420) and add:

```rust
/// The direction of a beneficial swap under the current prefs, or `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Improvement {
    None,
    Upgrade,
    Downgrade,
}

/// Whether swapping `current` -> `candidate` moves the owned copy toward the ideal under `prefs`,
/// and in which direction. Generalises the old `is_meaningful_upgrade` to also fire a CORRECTIVE
/// DOWNGRADE when `current` is above the ceiling (its `effective_score` is penalised, so a
/// within-ceiling cached `candidate` now scores higher even at a lower raw resolution).
///
/// A `candidate` reaching this function has already passed `score()`'s hard filter, so it is always
/// within the ceiling; its effective score equals its raw score. Convergence: within a fixed ceiling
/// every accepted swap strictly increases the owned effective score (bounded) => no flip-flop.
pub fn is_target_improvement(
    current: &QualitySummary,
    candidate: &QualitySummary,
    prefs: &QualityPrefs,
) -> Improvement {
    if !candidate.cached {
        return Improvement::None;
    }
    if !current.cached {
        return Improvement::Upgrade;
    }
    let cur = effective_score(current, prefs);
    let cand = effective_score(candidate, prefs);
    if cand <= cur {
        return Improvement::None;
    }
    // Require a category change (tier or resolution differs) so a marginal same-tier/same-res
    // bitrate/HEVC/HDR/seeder wobble never triggers a swap.
    if candidate.source_tier == current.source_tier && candidate.resolution == current.resolution {
        return Improvement::None;
    }
    if candidate.resolution < current.resolution {
        Improvement::Downgrade
    } else {
        Improvement::Upgrade
    }
}
```

- [ ] **Step 4: Update the call site in `upgrade.rs`**

At `src/upgrade.rs:236`, replace:

```rust
        if !release::is_meaningful_upgrade(&current, &q) {
            continue;
        }
```

with (Task 4 refines this further, but keep it compiling now — treat any non-`None` as acceptable here, since the movie path only does upgrades until Task 4 wires gating):

```rust
        if release::is_target_improvement(&current, &q, &app.config.acquisition.prefs)
            == release::Improvement::None
        {
            continue;
        }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib release:: 2>&1 | tail -20`
Expected: PASS (migrated + new modules).

- [ ] **Step 6: Lint + commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
git add src/release.rs src/upgrade.rs
git commit -m "feat(release): is_target_improvement unifies upgrade + corrective downgrade

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `ALLOW_RESOLUTION_DOWNGRADE` config flag

**Files:**
- Modify: `src/config.rs` — add field to `Config` (near line 503), Debug field (near 562), default in `from_parts` (near 668), wiring in `apply_household_flags` (line 593).
- Test: `src/config.rs` (extend the existing `apply_household_flags` name-wiring test near line 1176).

**Interfaces:**
- Produces: `Config.allow_resolution_downgrade: bool` (default false), set from env `ALLOW_RESOLUTION_DOWNGRADE` via the crate-wide `parse_bool`.

- [ ] **Step 1: Write the failing test**

Extend the household-flags test in `src/config.rs` (the one around line 1176 that checks `dedup_remove_duplicates` / `remove_finished_shows`). Add:

```rust
    // ALLOW_RESOLUTION_DOWNGRADE wires to allow_resolution_downgrade (and nothing else).
    c.apply_household_flags(|n| (n == "ALLOW_RESOLUTION_DOWNGRADE").then(|| "true".to_string()));
    assert!(c.allow_resolution_downgrade, "ALLOW_RESOLUTION_DOWNGRADE → allow_resolution_downgrade");
    assert!(
        !c.dedup_remove_duplicates && !c.remove_finished_shows,
        "ALLOW_RESOLUTION_DOWNGRADE must not enable the other household flags"
    );
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config:: 2>&1 | tail -20`
Expected: FAIL — `no field allow_resolution_downgrade on type ... Config`.

- [ ] **Step 3: Add the field + wiring**

In `src/config.rs`:

1. Add to the `Config` struct after `remove_finished_shows` (line ~503):

```rust
    /// When `true`, the upgrade engine may perform a CORRECTIVE DOWNGRADE — replacing an owned copy
    /// that is now above `MAX_RESOLUTION` with a ceiling-conforming cached release (deleting the
    /// higher-res copy). Default `false` only *logs* the planned downgrades (a preview). Pure quality
    /// upgrades are unaffected by this flag. Mirrors `DEDUP_REMOVE_DUPLICATES` / `REMOVE_FINISHED_SHOWS`.
    pub allow_resolution_downgrade: bool,
```

2. Add to the manual `Debug` impl after the `remove_finished_shows` field (line ~562):

```rust
            .field("allow_resolution_downgrade", &self.allow_resolution_downgrade)
```

3. Add to `from_parts`'s `Config { .. }` initializer after `remove_finished_shows: false,` (line ~668):

```rust
            allow_resolution_downgrade: false,
```

4. Add to `apply_household_flags` after the `remove_finished_shows` assignment (line ~597):

```rust
        self.allow_resolution_downgrade =
            AcquisitionConfig::parse_bool(lookup("ALLOW_RESOLUTION_DOWNGRADE"), false);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib config:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
git add src/config.rs
git commit -m "feat(config): ALLOW_RESOLUTION_DOWNGRADE flag (dry-run default)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Shared `pick_best_change` + `apply_upgrade` tail; migrate movie path + wire gating

**Files:**
- Modify: `src/upgrade.rs` — add `pick_best_change` (replaces the inline candidate loop at 214–249), add `StagedUpgrade` + `apply_upgrade` (extract the tail 250–338 of `try_upgrade_movie`), rewrite `try_upgrade_movie` to use them; update `best_owned_quality` to compare via `effective_score`.
- Test: `src/upgrade.rs` (existing `mod tests` — add a movie-downgrade dry-run/flag-on pair).

**Interfaces:**
- Consumes: `release::{effective_score, is_target_improvement, Improvement}` (Tasks 1–2), `Config.allow_resolution_downgrade` (Task 3), existing helpers `stage_and_verify`, `prune_owned_hash_in`, `best_owned_quality`, `movie_slot`.
- Produces:
  - `struct StagedUpgrade { hash: String, torrent_id: String, slot_files: Vec<(String, String)> }`
  - `async fn apply_upgrade(app: &AppState, staged: StagedUpgrade, supersede_hashes: &[String], idle_window: Duration) -> Result<(), UpgradeSkip>`
  - `async fn pick_best_change(app: &AppState, media: MediaKind, tmdb_id: u64, raws: &[RawCandidate], baseline: &QualitySummary, owned_hashes: &[String]) -> Option<(release::ReleaseInfo, release::Improvement)>`
  - `fn downgrade_allowed_or_log(app: &AppState, tmdb_id: u64, imp: release::Improvement, current: &QualitySummary, cand: &release::ReleaseInfo) -> bool`

- [ ] **Step 1: Write the failing tests**

In `src/upgrade.rs` `mod tests`, add (follow the existing mock-provider/store test harness already used by the upgrade tests — reuse its builders):

```rust
#[tokio::test]
async fn movie_over_ceiling_downgrade_is_dry_run_by_default() {
    // Own a cached 2160p movie; ceiling = 1080; a cached 1080p candidate is scrape-available.
    // Flag OFF (default): no swap, no prune — the 2160p copy stays owned and its selection unchanged.
    let h = harness_with_owned_movie_2160_and_1080_candidate().await;
    assert!(!h.app.config.allow_resolution_downgrade);
    let r = try_upgrade_movie(&h.app, h.tmdb, &h.owned_hashes, &h.rec, Duration::from_secs(0)).await;
    assert!(matches!(r, Ok(()) | Err(UpgradeSkip::NoChange(_))));
    assert!(h.app.store.get_owned(h.owned_2160_hash.clone()).await.is_some(),
        "dry-run must not prune the over-ceiling copy");
    assert_eq!(h.app.store.get_selection(&movie_slot(h.tmdb)).await.unwrap().hash, h.owned_2160_hash);
}

#[tokio::test]
async fn movie_over_ceiling_downgrade_swaps_when_flag_set() {
    let mut h = harness_with_owned_movie_2160_and_1080_candidate().await;
    h.set_allow_downgrade(true);
    let r = try_upgrade_movie(&h.app, h.tmdb, &h.owned_hashes, &h.rec, Duration::from_secs(0)).await;
    assert!(r.is_ok());
    assert!(h.app.store.get_owned(h.owned_2160_hash.clone()).await.is_none(),
        "flag on: the over-ceiling copy is pruned");
    assert_eq!(h.app.store.get_selection(&movie_slot(h.tmdb)).await.unwrap().hash, h.cand_1080_hash);
}
```

Note: `harness_with_owned_movie_2160_and_1080_candidate` and `set_allow_downgrade` are small test builders — model them on the existing upgrade-test setup (mock `Scraper` returning the 1080p candidate; mock provider reporting it `downloaded`/cached; `Config` built via `Config::from_parts` with `acquisition.prefs.max_resolution = P1080`). If the existing tests already have a builder, extend it with an `allow_resolution_downgrade` setter and a two-copy owned movie.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib upgrade::tests::movie_over_ceiling 2>&1 | tail -20`
Expected: FAIL — helpers not found / downgrade not gated (either compile error on new builders or the dry-run test failing because the swap happened).

- [ ] **Step 3: Add `StagedUpgrade` + `apply_upgrade` (extract the movie tail)**

In `src/upgrade.rs`, add:

```rust
/// A staged, verified replacement ready to be swapped in. `slot_files` maps each selection slot to
/// the file_path within the staged torrent that should represent it (one entry for a movie, one per
/// episode for a pack).
struct StagedUpgrade {
    hash: String,
    torrent_id: String,
    slot_files: Vec<(String, String)>,
}

/// Shared swap/prune tail used by the movie, per-episode, and pack-correction callers. Idle-gates,
/// fetches the provider listing ONCE (before the final idle re-check, so there is no network
/// round-trip between confirming idle and the destructive prune), repoints every slot to the staged
/// release, then prunes the superseded hashes. Non-destructive rollback on defer: a mid-stage active
/// read fully deletes the staged torrent + drops its records and leaves the current release intact.
async fn apply_upgrade(
    app: &AppState,
    staged: StagedUpgrade,
    supersede_hashes: &[String],
    idle_window: Duration,
) -> Result<(), UpgradeSkip> {
    // Fetch listing first (distinguish fetch-failure from empty account: on Err roll back + defer).
    let listing = match app.provider.get_torrents().await {
        Ok(l) => l,
        Err(_) => {
            if app.provider.delete_torrent(&staged.torrent_id).await.is_ok() {
                let _ = app.store.remove_owned(staged.hash.clone()).await;
                let _ = app.store.remove_authoritative(staged.hash.clone()).await;
            }
            return Err(UpgradeSkip::Deferred("provider listing unavailable; deferring".into()));
        }
    };
    // Re-check idle immediately before the destructive swap+prune; roll back on activity.
    if !app.read_activity.all_idle(idle_window).await {
        if app.provider.delete_torrent(&staged.torrent_id).await.is_ok() {
            let _ = app.store.remove_owned(staged.hash.clone()).await;
            let _ = app.store.remove_authoritative(staged.hash.clone()).await;
        }
        return Err(UpgradeSkip::Deferred("library became active during staging; deferring".into()));
    }
    // Repoint every slot. If ANY selection write fails, do NOT prune (pruning behind a stale
    // selection would leave a slot resolving to a hash we're about to delete) — defer the prune.
    for (slot, file_path) in &staged.slot_files {
        if let Err(e) = app
            .store
            .put_selection(slot.clone(), crate::store::SelectionEntry {
                hash: staged.hash.clone(),
                file_path: file_path.clone(),
            })
            .await
        {
            return Err(UpgradeSkip::Deferred(format!("selection write failed; deferring prune: {e}")));
        }
    }
    for old in supersede_hashes {
        if old.eq_ignore_ascii_case(&staged.hash) {
            continue;
        }
        prune_owned_hash_in(app, old, &listing).await;
    }
    Ok(())
}
```

- [ ] **Step 4: Add `pick_best_change` + `downgrade_allowed_or_log`**

```rust
/// Pick the best CACHED candidate that is a target improvement over `baseline`, skipping owned and
/// blacklisted hashes. Applies the same hard filters as acquisition via `score()` (returns None for
/// above-ceiling / cam / dead-seeder), then ranks the qualifying candidates best-effective-score
/// first. Returns the release + its Improvement direction. Shared by the movie and episode paths.
async fn pick_best_change(
    app: &AppState,
    media: MediaKind,
    tmdb_id: u64,
    raws: &[crate::scraper::RawCandidate],
    baseline: &QualitySummary,
    owned_hashes: &[String],
) -> Option<(release::ReleaseInfo, release::Improvement)> {
    let prefs = &app.config.acquisition.prefs;
    let mut best: Option<(release::ReleaseInfo, QualitySummary, release::Improvement)> = None;
    for raw in raws {
        let r = release::parse(raw);
        if owned_hashes.iter().any(|h| h.eq_ignore_ascii_case(&r.info_hash)) {
            continue;
        }
        if app.store.is_blacklisted(media, tmdb_id, r.info_hash.clone()).await {
            continue;
        }
        if release::score(&r, prefs).is_none() {
            continue; // hard filters (ceiling/cam/dead-seeder) — never upgrade past the ceiling
        }
        let q = QualitySummary::of(&r, prefs);
        let imp = release::is_target_improvement(baseline, &q, prefs);
        if imp == release::Improvement::None {
            continue;
        }
        let better = best.as_ref().map(|(_, bq, _)| q.score > bq.score).unwrap_or(true);
        if better {
            best = Some((r, q, imp));
        }
    }
    best.map(|(r, _, imp)| (r, imp))
}

/// Gate a corrective downgrade on the opt-in flag. An `Upgrade` always proceeds. A `Downgrade` with
/// the flag OFF is logged (preview) and rejected; with the flag ON it proceeds.
fn downgrade_allowed_or_log(
    app: &AppState,
    tmdb_id: u64,
    imp: release::Improvement,
    current: &QualitySummary,
    cand: &release::ReleaseInfo,
) -> bool {
    match imp {
        release::Improvement::Upgrade => true,
        release::Improvement::Downgrade if app.config.allow_resolution_downgrade => true,
        release::Improvement::Downgrade => {
            info!(
                "upgrade: tmdb {} would downgrade {}p -> {} ({}p) [set ALLOW_RESOLUTION_DOWNGRADE=true to enable]",
                tmdb_id, current.resolution, cand.info_hash, cand.resolution.unwrap_or(0)
            );
            false
        }
        release::Improvement::None => false,
    }
}
```

- [ ] **Step 5: Rewrite `try_upgrade_movie` to use the shared helpers**

Replace the body from the candidate loop (214) through the end (338) so it: builds `req`, scrapes, calls `pick_best_change`, gates via `downgrade_allowed_or_log`, idle-gates, `stage_and_verify`, then `apply_upgrade` with `slot_files = vec![(movie_slot(tmdb_id), staged_path)]` and `supersede_hashes = owned_hashes`:

```rust
    let raws = app.scraper.find(&req.imdb_id, MediaKind::Movie, None, None).await
        .map_err(|e| UpgradeSkip::Deferred(format!("scrape failed: {e}")))?;
    let Some((cand, imp)) = pick_best_change(app, MediaKind::Movie, tmdb_id, &raws, &current, owned_hashes).await else {
        return Err(UpgradeSkip::NoChange("no meaningful upgrade".into()));
    };
    if !downgrade_allowed_or_log(app, tmdb_id, imp, &current, &cand) {
        return Err(UpgradeSkip::NoChange("downgrade gated (dry-run)".into()));
    }
    if !app.read_activity.all_idle(idle_window).await {
        return Err(UpgradeSkip::Deferred("library active; deferring upgrade".into()));
    }
    let staged = stage_and_verify(app, tmdb_id, &req, &cand).await?;
    apply_upgrade(
        app,
        StagedUpgrade { hash: staged.0.clone(), torrent_id: staged.1, slot_files: vec![(movie_slot(tmdb_id), staged.2)] },
        owned_hashes,
        idle_window,
    ).await?;
    info!("upgrade: tmdb {} swapped to {}", tmdb_id, staged.0);
    Ok(())
```

Then update `best_owned_quality` (line 129) to pick the best copy by `effective_score` (so the baseline is the best *ceiling-aware* copy — an over-ceiling copy no longer masquerades as "best" and block its own correction):

```rust
        let q = rec.quality?;
        let better = best
            .as_ref()
            .map(|b| release::effective_score(&q, &app.config.acquisition.prefs)
                     > release::effective_score(b, &app.config.acquisition.prefs))
            .unwrap_or(true);
        if better {
            best = Some(q);
        }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib upgrade:: 2>&1 | tail -25`
Expected: PASS (existing movie-upgrade tests + the two new downgrade tests).

- [ ] **Step 7: Lint + commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
git add src/upgrade.rs
git commit -m "feat(upgrade): shared pick_best_change/apply_upgrade; movie corrective downgrade (gated)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: `try_upgrade_show` — per-episode upgrade/downgrade + over-ceiling pack correction

**Files:**
- Modify: `src/upgrade.rs` — add `stage_episode_candidate`, `try_upgrade_show`; change the `run_upgrade_once` Show dispatch (line 92).
- Test: `src/upgrade.rs` (`mod tests`).

**Interfaces:**
- Consumes: `pick_best_change`, `apply_upgrade`, `StagedUpgrade`, `downgrade_allowed_or_log` (Task 4); `episode_slot`, `best_owned_quality`; existing helpers `await_downloaded`, `crate::acquire::episode_files`, `app.engine.validate_title`, `app.engine.probe_file`, `try_consolidate_show`.
- Produces:
  - `async fn try_upgrade_show(app, tmdb_id, group_hashes, idle_window) -> Result<(), UpgradeSkip>` (new Show dispatch target)
  - `async fn stage_episode_candidate(app, tmdb_id, req, cand, season, episode) -> Result<(String /*hash*/, String /*torrent_id*/, String /*file_path*/), UpgradeSkip>`

- [ ] **Step 1: Write the failing tests**

In `mod tests` add (reuse the show-test harness that already backs the consolidation tests):

```rust
#[tokio::test]
async fn singleton_episode_upgrades_to_better_cached_release() {
    // Own S1E1 as a cached 720p singleton; a cached 1080p S1E1 is scrape-available; ceiling 1080.
    let h = show_harness_singleton_720_ep().await;
    let r = try_upgrade_show(&h.app, h.tmdb, &h.group_hashes, Duration::from_secs(0)).await;
    assert!(r.is_ok());
    let sel = h.app.store.get_selection(&episode_slot(h.tmdb, 1, 1)).await.unwrap();
    assert_eq!(sel.hash, h.cand_1080_hash, "episode slot repointed to the 1080p release");
    assert!(h.app.store.get_owned(h.old_720_hash.clone()).await.is_none(), "old singleton pruned");
}

#[tokio::test]
async fn singleton_episode_downgrade_respects_flag() {
    // Own S1E1 cached 2160p singleton; ceiling dropped to 1080; cached 1080p available.
    let mut h = show_harness_singleton_2160_ep().await;
    // Flag OFF: kept.
    let _ = try_upgrade_show(&h.app, h.tmdb, &h.group_hashes, Duration::from_secs(0)).await;
    assert!(h.app.store.get_owned(h.old_2160_hash.clone()).await.is_some(), "dry-run keeps 2160p");
    // Flag ON: corrected.
    h.set_allow_downgrade(true);
    let r = try_upgrade_show(&h.app, h.tmdb, &h.group_hashes, Duration::from_secs(0)).await;
    assert!(r.is_ok());
    assert!(h.app.store.get_owned(h.old_2160_hash.clone()).await.is_none(), "flag on prunes 2160p");
}

#[tokio::test]
async fn over_ceiling_pack_with_no_conforming_candidate_is_kept() {
    // Own a cached 2160p full-season pack; ceiling 1080; scraper returns NO within-ceiling pack.
    let mut h = show_harness_pack_2160_no_candidate().await;
    h.set_allow_downgrade(true);
    let _ = try_upgrade_show(&h.app, h.tmdb, &h.group_hashes, Duration::from_secs(0)).await;
    assert!(h.app.store.get_owned(h.pack_hash.clone()).await.is_some(),
        "sole over-ceiling pack kept when no conforming replacement exists");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib upgrade::tests 2>&1 | tail -25`
Expected: FAIL — `try_upgrade_show` not found / helpers not found.

- [ ] **Step 3: Implement `stage_episode_candidate`**

Model on `stage_and_verify` but target one `(season, episode)`: add magnet, get info, select the video files, `await_downloaded`, gate on cached, find the target episode's file via `crate::acquire::episode_files`, reject a multi-season pack (episode path adopts a single-episode-or-in-season file only), `validate_title(fname, tmdb, Series, Some(s), Some(e))`, `probe_file`, record `OwnedRecord` (status `Verified`, `provides: vec![(season, episode)]`, `quality: Some(QualitySummary::of(cand, prefs))`, sticky provenance via `base_req_provenance(app, MediaType::Show, tmdb)`), and `put_authoritative`. On any candidate-specific failure delete the staged torrent + blacklist (`WrongTitle`/probe reason); on transient failure `Deferred`. Return `(hash, torrent_id, file_path)`.

```rust
async fn stage_episode_candidate(
    app: &AppState,
    tmdb_id: u64,
    base_req: &crate::store::AcquireRequest,
    cand: &release::ReleaseInfo,
    season: u32,
    episode: u32,
) -> Result<(String, String, String), UpgradeSkip> {
    let hash = cand.info_hash.clone();
    let magnet = format!("magnet:?xt=urn:btih:{}", hash);
    let added = app.provider.add_magnet(&magnet).await
        .map_err(|e| UpgradeSkip::Deferred(format!("add failed: {e}")))?;
    let info = match app.provider.get_torrent_info(&added.id).await {
        Ok(i) => i,
        Err(e) => { let _ = app.provider.delete_torrent(&added.id).await;
            return Err(UpgradeSkip::Deferred(format!("info failed: {e}"))); }
    };
    let ids: Vec<u32> = info.files.iter()
        .filter(|f| crate::vfs::is_video_file(&f.path)).map(|f| f.id).collect();
    if !ids.is_empty() {
        if let Err(e) = app.provider.select_files(
            &added.id, &ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")).await {
            warn!("upgrade: select_files for staged episode {} failed: {}", hash, e);
        }
    }
    let fresh = await_downloaded(app.provider.as_ref(), &added.id).await.unwrap_or(info);
    if fresh.status != "downloaded" {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err(UpgradeSkip::NoChange("candidate not cached".into()));
    }
    let eps = crate::acquire::episode_files(&fresh);
    // Adopt a per-episode release: the target (s,e) must be present, and the pack must not span other
    // seasons (a multi-season pack is left to consolidation, not the per-episode path).
    if eps.iter().any(|(s, _, _)| *s != season) {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err(UpgradeSkip::NoChange("multi-season pack; not a per-episode candidate".into()));
    }
    let Some((_, _, path)) = eps.iter().find(|(s, e, _)| *s == season && *e == episode) else {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err(UpgradeSkip::NoChange("candidate lacks the target episode".into()));
    };
    let file_path = path.clone();
    let fname = file_path.rsplit('/').next().unwrap_or(&file_path).to_string();
    if !app.engine.validate_title(&fname, tmdb_id, MediaKind::Series, Some(season), Some(episode)).await {
        let _ = app.provider.delete_torrent(&added.id).await;
        let _ = app.store.blacklist_add(MediaKind::Series, tmdb_id, hash.clone(), "WrongTitle", now_secs()).await;
        return Err(UpgradeSkip::NoChange("title mismatch".into()));
    }
    let probe_req = crate::store::AcquireRequest { season: Some(season), episode: Some(episode), ..base_req.clone() };
    match app.engine.probe_file(&fresh, &hash, &file_path, &probe_req).await {
        crate::acquire::VerifyResult::Pass | crate::acquire::VerifyResult::Accept => {}
        crate::acquire::VerifyResult::Reject(reason) => {
            let _ = app.provider.delete_torrent(&added.id).await;
            let _ = app.store.blacklist_add(MediaKind::Series, tmdb_id, hash.clone(), reason, now_secs()).await;
            return Err(UpgradeSkip::NoChange(format!("probe rejected: {reason}")));
        }
        crate::acquire::VerifyResult::Defer => {
            let _ = app.provider.delete_torrent(&added.id).await;
            return Err(UpgradeSkip::Deferred("probe deferred".into()));
        }
    }
    let prov = base_req_provenance(app, MediaType::Show, tmdb_id).await;
    let _ = app.store.put_owned(hash.clone(), OwnedRecord {
        request: base_req.clone(), provenance: prov, added_at: now_secs(),
        status: OwnedStatus::Verified, provides: vec![(season, episode)],
        quality: Some(QualitySummary::of(cand, &app.config.acquisition.prefs)),
    }).await;
    let _ = app.store.put_authoritative(hash.clone(), base_req.metadata.clone()).await;
    Ok((hash, added.id, file_path))
}
```

- [ ] **Step 4: Implement `try_upgrade_show` and wire the dispatch**

`try_upgrade_show` gathers owned records; for each episode held as a **singleton** (`provides.len() == 1`) whose `quality` is known, scrape `(imdb, Series, Some(season), Some(episode))`, `pick_best_change` against that episode's `QualitySummary`, gate downgrade, idle-gate, `stage_episode_candidate`, then `apply_upgrade` with `slot_files = vec![(episode_slot(tmdb,s,e), path)]`, `supersede = [that singleton hash]`. Then over-ceiling **pack** correction: for each owned pack (`provides.len() > 1`) whose `quality` is over the ceiling, scrape the season, find a within-ceiling cached full-season replacement via `pick_best_change` (baseline = pack quality; the candidate must cover the pack's episodes — reuse the consolidation coverage check), gate as a downgrade, stage all files, `apply_upgrade` repointing every covered episode slot, supersede = old pack hash. Finally call `try_consolidate_show(app, tmdb_id, group_hashes, idle_window)`.

```rust
async fn try_upgrade_show(
    app: &AppState,
    tmdb_id: u64,
    group_hashes: &[String],
    idle_window: Duration,
) -> Result<(), UpgradeSkip> {
    let mut owned: Vec<(String, OwnedRecord)> = Vec::new();
    for h in group_hashes {
        if let Some(r) = app.store.get_owned(h.clone()).await { owned.push((h.clone(), r)); }
    }
    let Some(imdb_id) = group_imdb_id(app, group_hashes).await else {
        // No imdb id anywhere → still try consolidation (it self-skips on the same condition).
        return try_consolidate_show(app, tmdb_id, group_hashes, idle_window).await;
    };
    let prefs = &app.config.acquisition.prefs;

    // (a) Per-singleton-episode upgrade/downgrade.
    for (hash, rec) in owned.iter().filter(|(_, r)| r.provides.len() == 1) {
        let Some(q) = rec.quality.clone() else { continue }; // untagged → no baseline
        let (season, episode) = rec.provides[0];
        let req = crate::store::AcquireRequest { imdb_id: imdb_id.clone(),
            season: Some(season), episode: Some(episode), ..rec.request.clone() };
        let raws = match app.scraper.find(&imdb_id, MediaKind::Series, Some(season), Some(episode)).await {
            Ok(r) => r,
            Err(_) => continue, // transient; other episodes/consolidation still run this tick
        };
        let Some((cand, imp)) = pick_best_change(app, MediaKind::Series, tmdb_id, &raws, &q, group_hashes).await else { continue };
        if !downgrade_allowed_or_log(app, tmdb_id, imp, &q, &cand) { continue; }
        if !app.read_activity.all_idle(idle_window).await {
            return Err(UpgradeSkip::Deferred("library active; deferring show upgrade".into()));
        }
        match stage_episode_candidate(app, tmdb_id, &req, &cand, season, episode).await {
            Ok((new_hash, tid, path)) => {
                let _ = apply_upgrade(app, StagedUpgrade {
                    hash: new_hash.clone(), torrent_id: tid,
                    slot_files: vec![(episode_slot(tmdb_id, season, episode), path)],
                }, std::slice::from_ref(hash), idle_window).await;
                info!("upgrade: tmdb {} s{}e{} swapped to {}", tmdb_id, season, episode, new_hash);
            }
            Err(UpgradeSkip::Deferred(reason)) =>
                debug!("upgrade: tmdb {} s{}e{} deferred: {}", tmdb_id, season, episode, reason),
            Err(UpgradeSkip::NoChange(reason)) =>
                debug!("upgrade: tmdb {} s{}e{} no change: {}", tmdb_id, season, episode, reason),
        }
    }

    // (b) Over-ceiling pack correction (flag-gated): replace a pack above the ceiling with a
    //     within-ceiling cached full-season replacement that covers the SAME episodes; keep the pack
    //     if no conforming replacement exists (sole-copy safety). Implemented by reusing
    //     pick_best_change (baseline = pack quality) + the season staging + apply_upgrade repointing
    //     every covered episode slot. See stage/coverage notes below.
    correct_over_ceiling_packs(app, tmdb_id, &owned, &imdb_id, idle_window).await;

    // (c) Consolidation, unchanged, layered after per-episode work.
    try_consolidate_show(app, tmdb_id, group_hashes, idle_window).await
}
```

Add `correct_over_ceiling_packs` (reuses `rank_cached_pack_candidates` from the consolidation path at `upgrade.rs:802`; the add/select/poll block is intentionally similar to `stage_episode_candidate` and may be factored into a shared local closure to reduce duplication):

```rust
/// Replace an owned season pack that is now ABOVE the ceiling with a within-ceiling cached pack that
/// covers the SAME episodes, repointing every covered episode slot and pruning the old pack. Gated as
/// a downgrade. Keeps the old pack when no conforming replacement exists (sole-copy safety).
async fn correct_over_ceiling_packs(
    app: &AppState,
    tmdb_id: u64,
    owned: &[(String, OwnedRecord)],
    imdb_id: &str,
    idle_window: Duration,
) {
    let prefs = &app.config.acquisition.prefs;
    let ceiling = prefs.max_resolution.height();
    let owned_hashes: Vec<String> = owned.iter().map(|(h, _)| h.clone()).collect();
    for (pack_hash, rec) in owned.iter().filter(|(_, r)| r.provides.len() > 1) {
        let Some(pack_q) = rec.quality.clone() else { continue }; // untagged → skip
        if pack_q.resolution <= ceiling { continue; } // within ceiling → not a correction target
        let season = rec.provides[0].0;
        if rec.provides.iter().any(|(s, _)| *s != season) { continue; } // single-season packs only
        let need: Vec<u32> = rec.provides.iter().map(|(_, e)| *e).collect();

        let Ok(raws) = app.scraper.find(imdb_id, MediaKind::Series, Some(season), Some(1)).await else { continue };
        for cand in rank_cached_pack_candidates(&raws, prefs, &owned_hashes) {
            if app.store.is_blacklisted(MediaKind::Series, tmdb_id, cand.info_hash.clone()).await { continue; }
            let cand_q = QualitySummary::of(&cand, prefs);
            let imp = release::is_target_improvement(&pack_q, &cand_q, prefs);
            if imp == release::Improvement::None { continue; }
            if !downgrade_allowed_or_log(app, tmdb_id, imp, &pack_q, &cand) { break; }
            if !app.read_activity.all_idle(idle_window).await { return; }

            let magnet = format!("magnet:?xt=urn:btih:{}", cand.info_hash);
            let Ok(added) = app.provider.add_magnet(&magnet).await else { continue };
            let Ok(info) = app.provider.get_torrent_info(&added.id).await else {
                let _ = app.provider.delete_torrent(&added.id).await; continue };
            let ids: Vec<u32> = info.files.iter().filter(|f| crate::vfs::is_video_file(&f.path)).map(|f| f.id).collect();
            if !ids.is_empty() {
                let _ = app.provider.select_files(&added.id,
                    &ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")).await;
            }
            let fresh = await_downloaded(app.provider.as_ref(), &added.id).await.unwrap_or(info);
            if fresh.status != "downloaded" { let _ = app.provider.delete_torrent(&added.id).await; continue; }
            let eps = crate::acquire::episode_files(&fresh);
            if eps.iter().any(|(s, _, _)| *s != season) { let _ = app.provider.delete_torrent(&added.id).await; continue; }
            if !need.iter().all(|e| eps.iter().any(|(_, pe, _)| pe == e)) {
                let _ = app.provider.delete_torrent(&added.id).await; continue; // must cover every owned episode
            }
            let Some((rs, re, rpath)) = eps.iter().find(|(s, _, _)| *s == season) else {
                let _ = app.provider.delete_torrent(&added.id).await; continue };
            let fname = rpath.rsplit('/').next().unwrap_or(rpath).to_string();
            if !app.engine.validate_title(&fname, tmdb_id, MediaKind::Series, Some(*rs), Some(*re)).await {
                let _ = app.provider.delete_torrent(&added.id).await;
                let _ = app.store.blacklist_add(MediaKind::Series, tmdb_id, cand.info_hash.clone(), "WrongTitle", now_secs()).await;
                continue;
            }
            let probe_req = crate::store::AcquireRequest { imdb_id: imdb_id.to_string(),
                season: Some(*rs), episode: Some(*re), ..rec.request.clone() };
            match app.engine.probe_file(&fresh, &cand.info_hash, rpath, &probe_req).await {
                crate::acquire::VerifyResult::Pass | crate::acquire::VerifyResult::Accept => {}
                crate::acquire::VerifyResult::Reject(reason) => {
                    let _ = app.provider.delete_torrent(&added.id).await;
                    let _ = app.store.blacklist_add(MediaKind::Series, tmdb_id, cand.info_hash.clone(), reason, now_secs()).await;
                    continue;
                }
                crate::acquire::VerifyResult::Defer => { let _ = app.provider.delete_torrent(&added.id).await; continue; }
            }
            let prov = base_req_provenance(app, MediaType::Show, tmdb_id).await;
            let base_req = crate::store::AcquireRequest { imdb_id: imdb_id.to_string(), ..rec.request.clone() };
            let provides: Vec<(u32, u32)> = eps.iter().filter(|(s, _, _)| *s == season).map(|(s, e, _)| (*s, *e)).collect();
            let _ = app.store.put_owned(cand.info_hash.clone(), OwnedRecord {
                request: base_req.clone(), provenance: prov, added_at: now_secs(),
                status: OwnedStatus::Verified, provides,
                quality: Some(cand_q.clone()),
            }).await;
            let _ = app.store.put_authoritative(cand.info_hash.clone(), base_req.metadata.clone()).await;
            let slot_files: Vec<(String, String)> = eps.iter().filter(|(s, _, _)| *s == season)
                .map(|(s, e, p)| (episode_slot(tmdb_id, *s, *e), p.clone())).collect();
            let _ = apply_upgrade(app, StagedUpgrade {
                hash: cand.info_hash.clone(), torrent_id: added.id, slot_files,
            }, std::slice::from_ref(pack_hash), idle_window).await;
            info!("upgrade: tmdb {} s{} pack corrected to within-ceiling {}", tmdb_id, season, cand.info_hash);
            break; // one correction per pack per tick
        }
    }
}
```

Then change the dispatch at `src/upgrade.rs:92`:

```rust
            MediaType::Show => try_upgrade_show(app, tmdb_id, &hashes, idle_window).await,
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib upgrade:: 2>&1 | tail -30`
Expected: PASS (singleton upgrade; singleton downgrade dry-run→flag-on; over-ceiling pack sole-copy kept; existing consolidation tests still green because consolidation is still invoked).

- [ ] **Step 6: Full suite + lint + commit**

```bash
cargo test 2>&1 | tail -15
cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
git add src/upgrade.rs
git commit -m "feat(upgrade): per-episode show upgrade/downgrade + over-ceiling pack correction

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Documentation

**Files:**
- Modify: `CLAUDE.md` — env-var list (add `ALLOW_RESOLUTION_DOWNGRADE`), the `upgrade.rs` table row, and the SP3 upgrade design-decision paragraph (note shows now upgrade/downgrade per-episode; over-ceiling correction is flag-gated).
- Modify: `README.md` — env-var table (add `ALLOW_RESOLUTION_DOWNGRADE`; note the show upgrade/downgrade + ceiling-correction behaviour).

**Interfaces:** none (docs only).

- [ ] **Step 1: Update `CLAUDE.md`**

Add to the "Upgrade engine (SP3)" env-var block:

```markdown
- `ALLOW_RESOLUTION_DOWNGRADE` (default unset/`false`) — when `true`/`1`/`yes`/`on`, the upgrade engine may perform a **corrective downgrade**: when `MAX_RESOLUTION` is lowered below an owned copy, it replaces that above-ceiling copy with a ceiling-conforming CACHED release and prunes the higher-res copy (idle-gated, and only when such a replacement exists — a sole over-ceiling copy with no conforming cached release is kept). Default is a **dry-run** that only logs the planned downgrades (`upgrade: … would downgrade …`). Pure quality upgrades are unaffected by this flag.
```

Update the `upgrade.rs` row to note: movies **and shows** are quality-upgraded; shows upgrade/downgrade **per episode** (before any season pack) via the shared `pick_best_change`/`apply_upgrade` core, with full-season **consolidation** layered after; a ceiling-aware `effective_score` drives corrective downgrades (gated by `ALLOW_RESOLUTION_DOWNGRADE`). Update the `release.rs` row: `is_meaningful_upgrade` → `is_target_improvement` (+ `effective_score`).

- [ ] **Step 2: Update `README.md`**

Add the `ALLOW_RESOLUTION_DOWNGRADE` row to the env-var table with the same description, and a sentence in the upgrade section that shows are now upgraded/downgraded per-episode and that lowering the ceiling corrects existing above-ceiling copies only when the flag is set.

- [ ] **Step 3: Verify docs match code**

Run: `grep -n "ALLOW_RESOLUTION_DOWNGRADE" CLAUDE.md README.md`
Expected: at least one hit in each file.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md
git commit -m "docs: show upgrade/downgrade + ALLOW_RESOLUTION_DOWNGRADE

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Notes for the implementer

- The upgrade tests already use mock `Scraper`/`DebridProvider`/`Store` builders — reuse them; do not invent a new harness. If a builder for "owned two copies of a movie" or "owned singleton episode" doesn't exist, add a thin helper next to the existing ones rather than duplicating setup in each test.
- `QualitySummary::of(&ReleaseInfo, &QualityPrefs)` is the existing constructor used throughout `upgrade.rs`; use it, don't hand-build summaries in non-test code.
- `episode_files(&TorrentInfo) -> Vec<(u32, u32, String)>` and `count_feature_videos` are `pub(crate)` in `acquire.rs`.
- Keep `Deferred` vs `NoChange` semantics exactly as the movie path uses them (transient → `Deferred`, cursor unchanged; candidate-specific → `NoChange`, cursor advances).
