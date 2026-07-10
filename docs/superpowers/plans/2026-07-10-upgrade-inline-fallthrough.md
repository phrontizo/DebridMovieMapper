# In-tick Upgrade Candidate Fall-through Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the movie and per-episode upgrade paths iterate ranked candidates until one is adopted (in-tick fall-through), so a structurally-unusable top candidate no longer starves the slot of a legitimate upgrade.

**Architecture:** Replace the single-best `pick_best_change` with `rank_changes` (a best-first ranked list). `try_upgrade_movie` and the per-episode loop in `upgrade_show_episodes_and_packs` iterate the list with the same downgrade-gate / idle-check / stage handling that the pack-correction and consolidation loops already use. No config or user-facing behaviour change beyond adopting the best *usable* candidate.

**Tech Stack:** Rust (async, tokio), existing `upgrade`/`release`/`provider` modules. TDD with `cargo test`.

**Spec:** `docs/superpowers/specs/2026-07-10-upgrade-inline-fallthrough-design.md`

## Global Constraints

- Follow existing patterns; do not restructure unrelated code.
- Lint gate before every commit: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`.
- `cargo test` green before every commit.
- Commit author identity is already configured (`Kiril <54142726+phrontizo@users.noreply.github.com>`); end every commit message with:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- **Uncapped** fall-through (spec D1): try ranked candidates until one is adopted or the list is exhausted.
- Ranked list is **direction-homogeneous** (spec §4): all `Upgrade` or all `Downgrade`. A gated-off downgrade therefore `break`s the loop (one preview log), never `continue`s.
- Stage-outcome control flow (spec D4): `Ok` → `apply_upgrade` then done; `Err(NoChange)` → next candidate; `Err(Deferred)` → stop this title's loop (movie: return `Deferred`; episode: give up that episode this tick).
- Do NOT modify `release::score()`, `is_target_improvement`, `effective_score`, `stage_and_verify`, `stage_episode_candidate`, `apply_upgrade`, the blacklist, `consolidation_target`, or the already-looping pack/consolidation paths.
- `Improvement` values: `release::Improvement::{None, Upgrade, Downgrade}`.
- `UpgradeSkip` variants: `UpgradeSkip::{NoChange(String), Deferred(String)}`.

---

### Task 1: `rank_changes` + movie-path fall-through (+ per-candidate mock)

**Files:**
- Modify: `src/provider.rs` — extend the test-only `MockProvider` (`struct MockProvider` at ~line 97 and its `DebridProvider` impl) so it can return a distinct `TorrentInfo` per staged candidate.
- Modify: `src/upgrade.rs` — add `rank_changes` (keep `pick_best_change` for now; Task 2 removes it), rewrite `try_upgrade_movie`'s selection block (`src/upgrade.rs:401-454`).
- Test: `src/upgrade.rs` (`mod tests`).

**Interfaces:**
- Consumes: `release::{parse, score, is_target_improvement, Improvement, RawCandidate, ReleaseInfo}`, `QualitySummary::of`, `stage_and_verify`, `apply_upgrade`, `StagedUpgrade`, `movie_slot`, `downgrade_allowed_or_log`.
- Produces:
  - `async fn rank_changes(app: &AppState, media: MediaKind, tmdb_id: u64, raws: &[release::RawCandidate], baseline: &QualitySummary, owned_hashes: &[String]) -> Vec<(release::ReleaseInfo, release::Improvement)>`
  - `MockProvider.torrent_info_by_id: std::collections::HashMap<String, TorrentInfo>` (used by Task 2's tests too).

- [ ] **Step 1: Extend `MockProvider` for per-candidate responses**

In `src/provider.rs`, add a field to `struct MockProvider` (after `torrent_info`):

```rust
    /// Per-torrent-id `TorrentInfo` responses. When non-empty, `add_magnet` derives the returned id
    /// from the magnet's infohash and `get_torrent_info` looks the id up here (falling back to the
    /// single `torrent_info`). Lets a test stage several candidates that each resolve differently —
    /// required to exercise the upgrade engine's in-tick candidate fall-through.
    pub torrent_info_by_id: std::collections::HashMap<String, TorrentInfo>,
    /// Torrent ids whose `get_torrent_info` returns a `reqwest::Error` (a transient failure), so a
    /// test can drive `stage_and_verify`'s `Deferred` classification for one specific staged candidate.
    pub fail_info_ids: std::collections::HashSet<String>,
```

Update the two impl methods (leave all other behaviour untouched; the `fail_info_ids` error is built exactly like the existing `fail_get_torrents` path — a no-I/O URL-parse failure):

```rust
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        if self.fail_info_ids.contains(id) {
            // Drive an immediate URL-parse failure to produce a reqwest::Error without any I/O
            // (mirrors the fail_get_torrents path), so stage_and_verify classifies it as Deferred.
            return reqwest::Client::new().get("").send().await.map(|_| TorrentInfo::default());
        }
        if let Some(ti) = self.torrent_info_by_id.get(id) {
            return Ok(ti.clone());
        }
        Ok(self.torrent_info.clone().unwrap_or_default())
    }
    async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        if !self.torrent_info_by_id.is_empty() {
            // Per-candidate mode: id = the magnet's infohash, so each staged candidate resolves to
            // its own `torrent_info_by_id` entry.
            let hash = magnet.rsplit("urn:btih:").next().unwrap_or("").to_string();
            return Ok(AddMagnetResponse { id: hash, uri: String::new() });
        }
        Ok(self.add_magnet.clone().unwrap_or_default())
    }
```

`MockProvider` is built with `..Default::default()` throughout the tests. If it derives `Default`, the new `HashMap`/`HashSet` fields default to empty automatically; if it has a manual `Default` impl, add the two empty-collection fields there. Either way, every existing `MockProvider { .. ..Default::default() }` test keeps its current behaviour (empty maps → falls back to the single `torrent_info`).

- [ ] **Step 2: Run the suite to confirm the mock change is inert**

Run: `cargo test --lib 2>&1 | grep -E "^test result|error\[" | head`
Expected: all existing tests still pass (0 failed) — the new field is unused so far.

- [ ] **Step 3: Write the failing movie fall-through test**

Add to `src/upgrade.rs` `mod tests`. This reuses the existing helpers (`mem_store`, `app_with`, `movie_slot`, `MockScraper { candidates }`, `RawCandidate`, `OwnedRecord`, `QualitySummary`, `Provenance`, `SelectionEntry`, `TorrentInfo`, `TorrentFile`) — model the owned-movie setup on the existing `harness_with_owned_movie_2160_and_1080_candidate` builder. Adjust exact field names to the real structs if they differ.

```rust
#[tokio::test]
async fn movie_upgrade_falls_through_pack_to_next_candidate() {
    // Own a 720p movie. Scrape returns two IN-CEILING improvements: the top-scored one (1080p REMUX)
    // resolves to a MULTI-FEATURE pack (rejected + blacklisted); the next (1080p WEB) is a clean
    // single movie. The engine must fall through and adopt the WEB release THIS tick.
    const TMDB: u64 = 550;
    let store = mem_store();
    let owned_hash = "own720".to_string();
    store.put_owned(owned_hash.clone(), OwnedRecord {
        request: movie_req(TMDB, "tt0137523"),          // helper below (or inline an AcquireRequest)
        provenance: Provenance::watchlist("a"),
        added_at: 1,
        status: OwnedStatus::Verified,
        provides: vec![],
        quality: Some(q720_web()),
    }).await.unwrap();
    store.put_selection(movie_slot(TMDB), SelectionEntry {
        hash: owned_hash.clone(), file_path: "old.720p.mkv".into(),
    }).await.unwrap();

    // Two cached in-ceiling improvements over 720p; REMUX outscores WEB so it is tried first.
    let pack = RawCandidate { name: "Fight.Club.1999.1080p.BluRay.REMUX".into(),
        description: "REMUX\nRD+".into(), info_hash: "hpack".into(), file_idx: None, file_name: None };
    let good = RawCandidate { name: "Fight.Club.1999.1080p.WEB".into(),
        description: "WEB\nRD+".into(), info_hash: "hgood".into(), file_idx: None, file_name: None };
    let scraper = Arc::new(MockScraper { candidates: vec![pack.clone(), good.clone()] });

    let mut by_id = std::collections::HashMap::new();
    // "hpack" → two feature-sized videos ⇒ count_feature_videos > 1 ⇒ MoviePack reject.
    by_id.insert("hpack".to_string(), TorrentInfo {
        id: "hpack".into(), hash: "hpack".into(), status: "downloaded".into(),
        files: vec![
            TorrentFile { id: 0, path: "Fight.Club.1080p.mkv".into(), bytes: 8_000_000_000, selected: 1 },
            TorrentFile { id: 1, path: "Other.Movie.1080p.mkv".into(), bytes: 8_000_000_000, selected: 1 },
        ],
        links: vec!["https://cdn/p0".into(), "https://cdn/p1".into()], ..Default::default() });
    // "hgood" → single feature video ⇒ adopted.
    by_id.insert("hgood".to_string(), TorrentInfo {
        id: "hgood".into(), hash: "hgood".into(), status: "downloaded".into(),
        files: vec![TorrentFile { id: 0, path: "Fight.Club.1999.1080p.WEB.mkv".into(), bytes: 6_000_000_000, selected: 1 }],
        links: vec!["https://cdn/g0".into()], ..Default::default() });

    let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
        torrents: vec![
            Torrent { id: "own720".into(), hash: owned_hash.clone(), status: "downloaded".into(), ..Default::default() },
            Torrent { id: "hgood".into(), hash: "hgood".into(), status: "downloaded".into(), ..Default::default() },
        ],
        torrent_info_by_id: by_id,
        resolved_url: Some("https://cdn/g0".into()),
        deleted: deleted.clone(),
        ..Default::default()
    });
    let app = app_with(scraper, provider, store.clone());
    // The prober must accept (Pass/Accept) so staging the good candidate verifies — use whatever the
    // existing movie-upgrade success test uses (e.g. app_with_prober / a passing MockProber). If
    // app_with already wires an accepting prober, keep it.

    let owned = vec![owned_hash.clone()];
    let rec = app.store.get_owned(owned_hash.clone()).await.unwrap();
    let r = try_upgrade_movie(&app, TMDB, &owned, &rec, Duration::from_secs(0)).await;
    assert!(r.is_ok(), "fall-through should adopt the good candidate: {r:?}");
    assert_eq!(app.store.get_selection(&movie_slot(TMDB)).await.unwrap().hash, "hgood",
        "selection repointed to the second-best (usable) candidate");
    assert!(app.store.is_blacklisted(MediaKind::Movie, TMDB, "hpack".into()).await,
        "the multi-feature pack was blacklisted");
    assert!(app.store.get_owned("own720".into()).await.is_none(), "old 720p copy pruned");
}
```

If a `movie_req`/`q720_web`-style movie helper doesn't already exist next to the movie tests, add a thin one (mirror `ep_record`/`q1080_web` for shows). Do not duplicate an existing helper.

- [ ] **Step 4: Run to verify it fails**

Run: `cargo test --lib upgrade::tests::movie_upgrade_falls_through 2>&1 | tail -20`
Expected: FAIL — today `try_upgrade_movie` tries only the top candidate (`hpack`), rejects it, and returns `NoChange`; the selection stays `own720`.

- [ ] **Step 5: Add `rank_changes`**

In `src/upgrade.rs`, add next to `pick_best_change` (do not remove `pick_best_change` yet — the episode path still calls it until Task 2):

```rust
/// Rank all CACHED target-improvement candidates best-score-first, skipping owned and blacklisted
/// hashes and anything failing `score()`'s hard filters (ceiling / cam / dead-seeder). The caller
/// stages them in order until one is adopted (in-tick fall-through), so a top candidate that fails a
/// candidate-specific gate no longer ends the tick. The list is direction-homogeneous (all `Upgrade`
/// or all `Downgrade`) because `is_target_improvement` only rates a lower-resolution candidate as
/// improving when the owned copy is over the ceiling.
async fn rank_changes(
    app: &AppState,
    media: MediaKind,
    tmdb_id: u64,
    raws: &[release::RawCandidate],
    baseline: &QualitySummary,
    owned_hashes: &[String],
) -> Vec<(release::ReleaseInfo, release::Improvement)> {
    let prefs = &app.config.acquisition.prefs;
    let mut scored: Vec<(i64, release::ReleaseInfo, release::Improvement)> = Vec::new();
    for raw in raws {
        let r = release::parse(raw);
        if owned_hashes.iter().any(|h| h.eq_ignore_ascii_case(&r.info_hash)) {
            continue;
        }
        if app.store.is_blacklisted(media, tmdb_id, r.info_hash.clone()).await {
            continue;
        }
        if release::score(&r, prefs).is_none() {
            continue;
        }
        let q = QualitySummary::of(&r, prefs);
        let imp = release::is_target_improvement(baseline, &q, prefs);
        if imp == release::Improvement::None {
            continue;
        }
        scored.push((q.score, r, imp));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0)); // best score first (stable for ties)
    scored.into_iter().map(|(_, r, imp)| (r, imp)).collect()
}
```

- [ ] **Step 6: Rewrite `try_upgrade_movie`'s selection block**

Replace `src/upgrade.rs:401-454` (from `let Some((cand, imp)) = pick_best_change(...)` through the final `Ok(())`) with:

```rust
    let ranked = rank_changes(app, MediaKind::Movie, tmdb_id, &raws, &current, owned_hashes).await;
    if ranked.is_empty() {
        return Err(UpgradeSkip::NoChange("no meaningful upgrade".into()));
    }
    // The movie baseline spans all owned copies (the swap prunes them all), so the first hash is a
    // stable stand-in for the downgrade preview log.
    let old_hash = owned_hashes.first().map(String::as_str).unwrap_or("");
    // Try ranked candidates best-first until one is adopted (in-tick fall-through), mirroring the
    // pack-correction/consolidation loops.
    for (cand, imp) in ranked {
        // Gated-off downgrade: the list is direction-homogeneous, so every remaining candidate is a
        // gated downgrade too — break (one preview log) rather than logging a preview per candidate.
        if !downgrade_allowed_or_log(app, tmdb_id, imp, old_hash, &current, &cand) {
            break;
        }
        // Idle re-check before staging each candidate (mirrors correct_over_ceiling_packs).
        if !app.read_activity.all_idle(idle_window).await {
            return Err(UpgradeSkip::Deferred("library active; deferring upgrade".into()));
        }
        match stage_and_verify(app, tmdb_id, &req, &cand).await {
            Ok(staged) => {
                apply_upgrade(
                    app,
                    StagedUpgrade {
                        hash: staged.0.clone(),
                        torrent_id: staged.1,
                        slot_files: vec![(movie_slot(tmdb_id), staged.2)],
                    },
                    owned_hashes,
                    idle_window,
                )
                .await?;
                info!("upgrade: tmdb {} swapped to {}", tmdb_id, staged.0);
                return Ok(());
            }
            // Candidate-specific reject (already blacklisted where warranted) → try the next best.
            Err(UpgradeSkip::NoChange(_)) => continue,
            // Transient provider/probe glitch → stop; retry the whole title next tick.
            Err(UpgradeSkip::Deferred(r)) => return Err(UpgradeSkip::Deferred(r)),
        }
    }
    Err(UpgradeSkip::NoChange("no acceptable upgrade candidate".into()))
```

- [ ] **Step 7: Run the fall-through test — verify it passes**

Run: `cargo test --lib upgrade::tests::movie_upgrade_falls_through 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 8: Add the remaining movie tests**

Add a small shared builder + three tests to `mod tests`. The builder owns a 720p movie and wires a two-candidate scrape (top-scored `hpack` = 1080p REMUX, second `hgood` = 1080p WEB) with per-candidate `torrent_info_by_id`; callers pass the resolved files for each and whether the top's info errors. (`movie_req`/`q720_web` are the thin movie helpers from Step 3; `TMDB=550`.)

```rust
async fn movie_two_candidate_app(
    top_files: Vec<TorrentFile>,
    second_files: Vec<TorrentFile>,
    fail_top_info: bool,
) -> (AppState, String) {
    const TMDB: u64 = 550;
    let store = mem_store();
    let owned_hash = "own720".to_string();
    store.put_owned(owned_hash.clone(), OwnedRecord {
        request: movie_req(TMDB, "tt0137523"),
        provenance: Provenance::watchlist("a"),
        added_at: 1, status: OwnedStatus::Verified, provides: vec![], quality: Some(q720_web()),
    }).await.unwrap();
    store.put_selection(movie_slot(TMDB), SelectionEntry {
        hash: owned_hash.clone(), file_path: "old.720p.mkv".into() }).await.unwrap();

    let pack = RawCandidate { name: "Fight.Club.1999.1080p.BluRay.REMUX".into(),
        description: "REMUX\nRD+".into(), info_hash: "hpack".into(), file_idx: None, file_name: None };
    let good = RawCandidate { name: "Fight.Club.1999.1080p.WEB".into(),
        description: "WEB\nRD+".into(), info_hash: "hgood".into(), file_idx: None, file_name: None };
    let scraper = Arc::new(MockScraper { candidates: vec![pack, good] });

    let mut by_id = std::collections::HashMap::new();
    by_id.insert("hpack".to_string(), TorrentInfo { id: "hpack".into(), hash: "hpack".into(),
        status: "downloaded".into(), files: top_files, links: vec!["https://cdn/p".into()], ..Default::default() });
    by_id.insert("hgood".to_string(), TorrentInfo { id: "hgood".into(), hash: "hgood".into(),
        status: "downloaded".into(), files: second_files, links: vec!["https://cdn/g".into()], ..Default::default() });
    let mut fail_info_ids = std::collections::HashSet::new();
    if fail_top_info { fail_info_ids.insert("hpack".to_string()); }

    let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
        torrents: vec![
            Torrent { id: "own720".into(), hash: owned_hash.clone(), status: "downloaded".into(), ..Default::default() },
            Torrent { id: "hgood".into(), hash: "hgood".into(), status: "downloaded".into(), ..Default::default() },
        ],
        torrent_info_by_id: by_id,
        fail_info_ids,
        resolved_url: Some("https://cdn/g".into()),
        deleted: Arc::new(std::sync::Mutex::new(Vec::new())),
        ..Default::default()
    });
    (app_with(scraper, provider, store), owned_hash)
}

fn feature(name: &str) -> TorrentFile {
    TorrentFile { id: 0, path: name.into(), bytes: 8_000_000_000, selected: 1 }
}

#[tokio::test]
async fn movie_upgrade_exhausts_bad_candidates_and_keeps_current() {
    // BOTH candidates resolve to multi-feature packs → both rejected+blacklisted, NoChange, nothing swapped.
    let (app, owned) = movie_two_candidate_app(
        vec![feature("A.1080p.mkv"), feature("B.1080p.mkv")],
        vec![feature("C.1080p.mkv"), feature("D.1080p.mkv")],
        false,
    ).await;
    let rec = app.store.get_owned(owned.clone()).await.unwrap();
    let r = try_upgrade_movie(&app, 550, std::slice::from_ref(&owned), &rec, Duration::from_secs(0)).await;
    assert!(matches!(r, Err(UpgradeSkip::NoChange(_))), "all bad → NoChange: {r:?}");
    assert_eq!(app.store.get_selection(&movie_slot(550)).await.unwrap().hash, owned, "selection unchanged");
    assert!(app.store.get_owned(owned.clone()).await.is_some(), "old copy kept");
}

#[tokio::test]
async fn movie_upgrade_transient_on_top_candidate_stops_without_trying_rest() {
    // Top candidate's get_torrent_info errors (transient) → stage_and_verify returns Deferred → the
    // loop STOPS (returns Deferred) and never stages the good second candidate.
    let (app, owned) = movie_two_candidate_app(
        vec![feature("Fight.Club.1080p.mkv")],       // (unused: hpack info errors before file inspection)
        vec![feature("Fight.Club.1999.1080p.WEB.mkv")],
        true, // hpack info errors
    ).await;
    let rec = app.store.get_owned(owned.clone()).await.unwrap();
    let r = try_upgrade_movie(&app, 550, std::slice::from_ref(&owned), &rec, Duration::from_secs(0)).await;
    assert!(matches!(r, Err(UpgradeSkip::Deferred(_))), "transient on top → Deferred: {r:?}");
    assert_eq!(app.store.get_selection(&movie_slot(550)).await.unwrap().hash, owned,
        "nothing adopted — the good candidate was never staged");
}

#[tokio::test]
async fn movie_over_ceiling_downgrade_dry_run_breaks_and_makes_no_change() {
    // Own a 2160p movie; ceiling 1080 (default prefs); ALLOW_RESOLUTION_DOWNGRADE off. Two in-ceiling
    // 1080p candidates → rank_changes rates both as Downgrade; the loop breaks on the first (gated),
    // logging one preview, and makes NO change / stages nothing.
    const TMDB: u64 = 603;
    let store = mem_store();
    let owned = "own2160".to_string();
    store.put_owned(owned.clone(), OwnedRecord {
        request: movie_req(TMDB, "tt0133093"), provenance: Provenance::watchlist("a"),
        added_at: 1, status: OwnedStatus::Verified, provides: vec![], quality: Some(q2160_remux()),
    }).await.unwrap();
    store.put_selection(movie_slot(TMDB), SelectionEntry { hash: owned.clone(), file_path: "old.2160p.mkv".into() }).await.unwrap();
    let c1 = RawCandidate { name: "The.Matrix.1999.1080p.REMUX".into(), description: "REMUX\nRD+".into(),
        info_hash: "hc1".into(), file_idx: None, file_name: None };
    let c2 = RawCandidate { name: "The.Matrix.1999.1080p.WEB".into(), description: "WEB\nRD+".into(),
        info_hash: "hc2".into(), file_idx: None, file_name: None };
    let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
        torrents: vec![Torrent { id: "own2160".into(), hash: owned.clone(), status: "downloaded".into(), ..Default::default() }],
        deleted: deleted.clone(), ..Default::default() // no torrent_info_by_id → staging never reached
    });
    let app = app_with(Arc::new(MockScraper { candidates: vec![c1, c2] }), provider, store.clone());
    assert!(!app.config.allow_resolution_downgrade, "flag must be off for this test");
    let rec = app.store.get_owned(owned.clone()).await.unwrap();
    let r = try_upgrade_movie(&app, TMDB, std::slice::from_ref(&owned), &rec, Duration::from_secs(0)).await;
    assert!(matches!(r, Err(UpgradeSkip::NoChange(_))), "gated downgrade → NoChange: {r:?}");
    assert_eq!(app.store.get_selection(&movie_slot(TMDB)).await.unwrap().hash, owned, "unchanged");
    assert!(deleted.lock().unwrap().is_empty(), "nothing staged (broke before staging)");
}
```

The existing `movie_over_ceiling_downgrade_is_dry_run_by_default` / `..._swaps_when_flag_set` single-candidate tests must still pass unchanged (regression). If `q2160_remux`/`q720_web`/`movie_req` don't exist yet, add the thin helpers next to the existing quality/record builders (mirror `q1080_web` at `src/upgrade.rs:4174`).

- [ ] **Step 9: Full suite + lint + commit**

```bash
cargo test 2>&1 | grep -E "^test result: (ok|FAILED)|error\[" | head
cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
git add src/provider.rs src/upgrade.rs
git commit -m "feat(upgrade): in-tick candidate fall-through for the movie path

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: per-episode fall-through + remove `pick_best_change` + docs

**Files:**
- Modify: `src/upgrade.rs` — rewrite the per-episode candidate selection inside the singleton loop of `upgrade_show_episodes_and_packs` (`src/upgrade.rs:1170-1218`); delete the now-unused `pick_best_change`.
- Modify: `CLAUDE.md` — one-line tweak to the `upgrade.rs` row.
- Test: `src/upgrade.rs` (`mod tests`).

**Interfaces:**
- Consumes: `rank_changes` (Task 1), `stage_episode_candidate`, `apply_upgrade`, `StagedUpgrade`, `crate::store::episode_slot`, `downgrade_allowed_or_log`, and the `MockProvider.torrent_info_by_id` field (Task 1).
- Produces: nothing new; removes `pick_best_change`.

- [ ] **Step 1: Write the failing per-episode fall-through test**

Model on the show harness builders (`ep_record`, `show_meta`, `SHOW_TMDB`, `MockScraper`, `season_pack_candidate`, `q720_web`/`q1080_web`, `episode_slot`). Own S1E1 as a cached 720p singleton; scrape (for the E1 query) returns two in-ceiling improvements: the top-scored resolves to a pack whose files are S01E02 only (so it **lacks the target episode E1** → `NoChange`), the next is a clean S01E01 1080p release (adopted).

```rust
#[tokio::test]
async fn episode_upgrade_falls_through_missing_episode_to_next_candidate() {
    let store = mem_store();
    let old = "eold720".to_string();
    store.put_owned(old.clone(), ep_record(1, 1, q720_web())).await.unwrap();
    store.put_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1),
        SelectionEntry { hash: old.clone(), file_path: "old.S01E01.720p.mkv".into() }).await.unwrap();

    let wrong = RawCandidate { name: "Show.S01E02.1080p.REMUX".into(), description: "REMUX\nRD+".into(),
        info_hash: "hwrong".into(), file_idx: None, file_name: None };   // higher score, wrong episode
    let good = RawCandidate { name: "Show.S01E01.1080p.WEB".into(), description: "WEB\nRD+".into(),
        info_hash: "hgood".into(), file_idx: None, file_name: None };
    let scraper = Arc::new(MockScraper { candidates: vec![wrong.clone(), good.clone()] });

    let mut by_id = std::collections::HashMap::new();
    by_id.insert("hwrong".to_string(), TorrentInfo { id: "hwrong".into(), hash: "hwrong".into(),
        status: "downloaded".into(),
        files: vec![TorrentFile { id: 0, path: "Show.S01E02.1080p.mkv".into(), bytes: 2_000_000_000, selected: 1 }],
        links: vec!["https://cdn/w0".into()], ..Default::default() });
    by_id.insert("hgood".to_string(), TorrentInfo { id: "hgood".into(), hash: "hgood".into(),
        status: "downloaded".into(),
        files: vec![TorrentFile { id: 0, path: "Show.S01E01.1080p.mkv".into(), bytes: 2_000_000_000, selected: 1 }],
        links: vec!["https://cdn/g0".into()], ..Default::default() });

    let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
        torrents: vec![
            Torrent { id: "eold720".into(), hash: old.clone(), status: "downloaded".into(), ..Default::default() },
            Torrent { id: "hgood".into(), hash: "hgood".into(), status: "downloaded".into(), ..Default::default() },
        ],
        torrent_info_by_id: by_id,
        resolved_url: Some("https://cdn/g0".into()),
        deleted: Arc::new(std::sync::Mutex::new(Vec::new())),
        ..Default::default()
    });
    let app = app_with(scraper, provider, store.clone());

    let deferred = upgrade_show_episodes_and_packs(&app, SHOW_TMDB,
        std::slice::from_ref(&old), Duration::from_secs(0)).await;
    assert!(!deferred, "adopting a candidate is not a deferral");
    assert_eq!(app.store.get_selection(crate::store::episode_slot(SHOW_TMDB, 1, 1)).await.unwrap().hash,
        "hgood", "episode slot repointed to the correct-episode candidate");
    assert!(app.store.get_owned("eold720".into()).await.is_none(), "old singleton pruned");
}
```

(Confirm the exact reason `stage_episode_candidate` returns for a candidate that lacks the target episode by reading `src/upgrade.rs:900+`; the setup above drives that `NoChange` branch. Adjust file-name tagging if the SE parser needs a specific format.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib upgrade::tests::episode_upgrade_falls_through 2>&1 | tail -20`
Expected: FAIL — today the per-episode loop tries only `hwrong`, rejects it, and moves to the next episode without adopting `hgood`; the slot stays `eold720`.

- [ ] **Step 3: Rewrite the per-episode candidate selection**

Replace `src/upgrade.rs:1170-1218` (from `let Some((cand, imp)) = pick_best_change(...)` through the closing of the `match stage_episode_candidate(...)`) with an inner ranked loop. The `raws` fetch above it (the `app.scraper.find(...)` block) and the outer `for (hash, rec)` loop stay unchanged:

```rust
        let ranked = rank_changes(app, MediaKind::Series, tmdb_id, &raws, &q, group_hashes).await;
        for (cand, imp) in ranked {
            if !downgrade_allowed_or_log(app, tmdb_id, imp, hash, &q, &cand) {
                break; // direction-homogeneous list → all remaining are gated downgrades
            }
            if !app.read_activity.all_idle(idle_window).await {
                debug!(
                    "upgrade: tmdb {} library active; deferring remaining show upgrade work",
                    tmdb_id
                );
                // Stop ALL further staging (remaining episodes + pack correction) this tick and
                // signal deferral so the caller leaves the cursor unstamped (unchanged behaviour).
                return true;
            }
            match stage_episode_candidate(app, tmdb_id, &req, &cand, season, episode).await {
                Ok((new_hash, tid, path)) => {
                    let _ = apply_upgrade(
                        app,
                        StagedUpgrade {
                            hash: new_hash.clone(),
                            torrent_id: tid,
                            slot_files: vec![(
                                crate::store::episode_slot(tmdb_id, season, episode),
                                path,
                            )],
                        },
                        std::slice::from_ref(hash),
                        idle_window,
                    )
                    .await;
                    info!(
                        "upgrade: tmdb {} s{}e{} swapped to {}",
                        tmdb_id, season, episode, new_hash
                    );
                    break; // episode upgraded → move to the next episode
                }
                // Candidate-specific reject → try the next-best candidate for THIS episode.
                Err(UpgradeSkip::NoChange(reason)) => {
                    debug!("upgrade: tmdb {} s{}e{} no change: {}", tmdb_id, season, episode, reason);
                    continue;
                }
                // Transient glitch on this episode → give up this episode this tick (move to the
                // next episode), matching the prior per-episode fire-and-forget behaviour.
                Err(UpgradeSkip::Deferred(reason)) => {
                    debug!("upgrade: tmdb {} s{}e{} deferred: {}", tmdb_id, season, episode, reason);
                    break;
                }
            }
        }
```

- [ ] **Step 4: Delete the now-unused `pick_best_change`**

Remove the `pick_best_change` function from `src/upgrade.rs` (both callers now use `rank_changes`). Confirm no references remain:

Run: `grep -rn "pick_best_change" src/`
Expected: no matches.

- [ ] **Step 5: Run the per-episode test + full suite**

Run: `cargo test --lib upgrade:: 2>&1 | tail -25`
Expected: the new episode test PASSES; every pre-existing `upgrade::` test (movie, per-episode, pack correction, consolidation, defer) still passes. Confirm no dead-code warning for `pick_best_change` (it's deleted).

- [ ] **Step 6: Update `CLAUDE.md`**

In the `upgrade.rs` row of the module table, update the movie/per-episode description to note the fall-through. Change the phrasing that describes picking "a meaningfully-better CACHED release" to note that the engine now **tries ranked candidates in order until one is usable** (the same in-tick fall-through the pack-correction and consolidation loops already use), so a structurally-unusable top candidate no longer blocks the slot. Keep it to one or two sentences; do not restructure the row.

- [ ] **Step 7: Lint + commit**

```bash
cargo test 2>&1 | grep -E "^test result: (ok|FAILED)|error\[" | head
cargo fmt && cargo clippy --all-targets -- -D warnings 2>&1 | tail -3
git add src/upgrade.rs CLAUDE.md
git commit -m "feat(upgrade): per-episode candidate fall-through; drop pick_best_change; docs

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Notes for the implementer

- The upgrade tests use mock `Scraper`/`DebridProvider`/`Store` builders already in `src/upgrade.rs` `mod tests` and `src/provider.rs`. Reuse them; the only new test infra is `MockProvider.torrent_info_by_id` (Task 1 Step 1).
- `QualitySummary::of(&ReleaseInfo, &QualityPrefs)` is the existing summary constructor; `release::score` returns `Option<i64>` (`None` = hard-filtered).
- `stage_and_verify` returns `Result<(String hash, String torrent_id, String file_path), UpgradeSkip>` and classifies its own `NoChange`/`Deferred`. `stage_episode_candidate` has the same return shape. Do not change either — this task only changes the control flow around them.
- Keep `Deferred` vs `NoChange` exactly as those helpers emit them; this change only decides how the loop reacts (spec D4).
