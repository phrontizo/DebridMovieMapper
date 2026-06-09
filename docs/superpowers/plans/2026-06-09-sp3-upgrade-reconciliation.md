# SP3 — Upgrade Engine + Reconciliation Model — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn acquisition into optimistic-add + asynchronous-reconcile, record per-hash episode `provides` to kill season-pack churn, invert `vfs::build` selection through a persisted `selection` table, and add a daily upgrade engine that stages meaningfully-better releases (and full-season cached packs) and swaps them in only when the file is idle.

**Architecture:** Two layers over the shared `AppState`. `observe` (in `acquire.rs`, every scan tick, no scraping) is the in-flight resolver: it selects files, runs the now-deferred pack-guard / title-validation / probe once files appear, records `provides` + `selection`, reaps genuinely-dead torrents after a real timeout, and recovers by re-scraping. A new `upgrade.rs` job (slow, gated on `UPGRADE_INTERVAL_SECS`, default daily) re-scores owned titles and stages quality upgrades + full-season consolidations, swapping the persisted `selection` and pruning the superseded torrent only when the slot is idle (proxy read-activity). `vfs::build` consults the `selection` table, falling back to largest-bytes when no managed selection exists.

**Tech Stack:** Rust (async, tokio), `redb` (via `Store`), `async-trait`, `reqwest`, `serde`/`serde_json`, `regex`, `chrono`. Tests use the in-memory `redb` backend, `MockProvider`, `MockScraper`, and `tokio::test(start_paused = true)` for time-based logic.

**Spec:** `docs/superpowers/specs/2026-06-09-sp3-upgrade-reconciliation-design.md`

**Conventions in this codebase (follow them):**
- TDD per `CLAUDE.md`: write the failing test, run it, implement, run it green, commit.
- Hashes are **lowercased** end-to-end (owned/auth/selection keys).
- `Store` accessors are async, `spawn_blocking`-wrapped; serialise BEFORE opening a write txn (avoid partial writes on serde failure) — copy the existing pattern in `store.rs`.
- New tables are created in `Store::ensure_schema`; migrations are additive (no data transform — decision 10 of the spec: 1.0.8 in the wild carries only the regenerable `matches` cache).
- Run `cargo test` after every step. Commit messages: `feat(...)`, `refactor(...)`, `test(...)`, `docs(...)`.

**Phases (execute in order):**
- **Phase A — persistence & pure cores (Tasks 1–4):** config knobs, store v4 (`provides`/`quality`/`selection`/`upgrade_checks`), `QualitySummary` + `is_meaningful_upgrade`, read-activity tracker. No behaviour change yet.
- **Phase B — selection inversion & reconciliation rework (Tasks 5–8):** `vfs::build` selection, optimistic `acquire` + `observe` resolver, scan-loop + removal wiring, `provides`-based churn fix.
- **Phase C — upgrade engine (Tasks 9–11):** upgrade job (quality), consolidation, scheduler spawn.
- **Phase D — docs & live gate (Tasks 12–13).**

---

## Phase A — persistence & pure cores

### Task 1: Config — upgrade knobs + acquire dead-timeout

**Files:**
- Modify: `src/config.rs` (add `UpgradeConfig`, an `acquire_dead_timeout_secs` field on `AcquisitionConfig`, and an `upgrade` field on `Config`)
- Test: `src/config.rs` (in the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/config.rs`:

```rust
    #[test]
    fn upgrade_config_defaults_to_daily_and_clamps() {
        // Absent → daily default, all sub-defaults applied.
        let u = UpgradeConfig::from_parts(None, None, None, None);
        assert_eq!(u.interval_secs, 86_400);
        assert_eq!(u.budget_per_tick, 20);
        assert_eq!(u.idle_secs, 300);
        assert_eq!(u.stage_max_secs, 604_800);
        assert!(u.enabled(), "default (daily) is enabled");

        // interval=0 disables the job.
        let off = UpgradeConfig::from_parts(Some("0".into()), None, None, None);
        assert_eq!(off.interval_secs, 0);
        assert!(!off.enabled());

        // Below-min interval (but non-zero) clamps up to 600; sub-knobs clamp to their mins.
        let clamped = UpgradeConfig::from_parts(
            Some("60".into()), Some("0".into()), Some("5".into()), Some("10".into()),
        );
        assert_eq!(clamped.interval_secs, 600);
        assert_eq!(clamped.budget_per_tick, 1);
        assert_eq!(clamped.idle_secs, 30);
        assert_eq!(clamped.stage_max_secs, 3600);

        // Invalid → defaults.
        let bad = UpgradeConfig::from_parts(Some("x".into()), Some("y".into()), None, None);
        assert_eq!(bad.interval_secs, 86_400);
        assert_eq!(bad.budget_per_tick, 20);
    }

    #[test]
    fn acquisition_has_dead_timeout_default_and_override() {
        let a = AcquisitionConfig::from_parts(None, None, None, None, None, None, None);
        assert_eq!(a.acquire_dead_timeout_secs, 600);
        let b = AcquisitionConfig::from_parts(
            None, None, None, None, None, None, None,
        );
        assert_eq!(b.acquire_dead_timeout_secs, 600);
    }

    #[test]
    fn config_from_parts_has_upgrade_default() {
        let c = parts(Some("rd"), None, Some("tmdb"), None, None, None).unwrap();
        assert_eq!(c.upgrade.interval_secs, 86_400);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests::upgrade_config_defaults_to_daily_and_clamps`
Expected: FAIL — `UpgradeConfig` not found.

- [ ] **Step 3: Add `UpgradeConfig` and wire it into `AcquisitionConfig` + `Config`**

In `src/config.rs`, add `UpgradeConfig` after `AcquisitionConfig`:

```rust
/// Upgrade-engine configuration (SP3). Held by `Config`. The job is spawned only when
/// `interval_secs > 0` (default 86_400 = daily; set `UPGRADE_INTERVAL_SECS=0` to disable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeConfig {
    pub interval_secs: u64,
    pub budget_per_tick: u32,
    pub idle_secs: u64,
    pub stage_max_secs: u64,
}

impl Default for UpgradeConfig {
    fn default() -> Self {
        Self::from_parts(None, None, None, None)
    }
}

impl UpgradeConfig {
    /// `true` when the upgrade job should run (interval non-zero).
    pub fn enabled(&self) -> bool {
        self.interval_secs > 0
    }

    /// Pure construction from raw optional values (env-independent, for tests).
    /// `interval_secs`: 0 disables; otherwise clamped to a 600s minimum. Invalid → 86_400.
    pub fn from_parts(
        interval_secs: Option<String>,
        budget_per_tick: Option<String>,
        idle_secs: Option<String>,
        stage_max_secs: Option<String>,
    ) -> Self {
        fn num(v: Option<String>, default: u64, min: u64, name: &str) -> u64 {
            match v {
                Some(s) => match s.trim().parse::<u64>() {
                    Ok(0) if name == "UPGRADE_INTERVAL_SECS" => 0, // 0 = disabled (not clamped)
                    Ok(n) => n.max(min),
                    Err(_) => {
                        warn!("Invalid {} value '{}', falling back to {}", name, s, default);
                        default
                    }
                },
                None => default,
            }
        }
        UpgradeConfig {
            interval_secs: num(interval_secs, 86_400, 600, "UPGRADE_INTERVAL_SECS"),
            budget_per_tick: num(budget_per_tick, 20, 1, "UPGRADE_BUDGET_PER_TICK") as u32,
            idle_secs: num(idle_secs, 300, 30, "UPGRADE_IDLE_SECS"),
            stage_max_secs: num(stage_max_secs, 604_800, 3600, "UPGRADE_STAGE_MAX_SECS"),
        }
    }

    pub fn from_env() -> Self {
        Self::from_parts(
            std::env::var("UPGRADE_INTERVAL_SECS").ok(),
            std::env::var("UPGRADE_BUDGET_PER_TICK").ok(),
            std::env::var("UPGRADE_IDLE_SECS").ok(),
            std::env::var("UPGRADE_STAGE_MAX_SECS").ok(),
        )
    }
}
```

In `AcquisitionConfig`, add the field + parsing. Change the struct to add:

```rust
    /// Seconds an optimistically-added torrent may stay Pending without resolving/seeding
    /// before `observe` reaps it as dead (SP3). Default 600.
    pub acquire_dead_timeout_secs: u64,
```

In `AcquisitionConfig::from_parts`, add to the returned struct (after `scraper_addon_url: None,`):

```rust
            acquire_dead_timeout_secs: 600,
```

(There is no env override needed beyond the default for now; `from_env` leaves it at 600. If you want an override, read `ACQUIRE_DEAD_TIMEOUT_SECS` in `from_env` and set it — optional. For this task the default suffices; add the env read in `from_env`:)

```rust
        a.acquire_dead_timeout_secs = std::env::var("ACQUIRE_DEAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|n| n.max(120))
            .unwrap_or(600);
```

In `Config`, add the field:

```rust
    /// Upgrade-engine config (SP3). Always present; `upgrade.enabled()` gates the job.
    pub upgrade: UpgradeConfig,
```

In `Config::from_parts`, add `upgrade: UpgradeConfig::default(),` to the returned struct. In `Config::from_env`, after `cfg.trakt = TraktConfig::from_env();` add `cfg.upgrade = UpgradeConfig::from_env();`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib config::`
Expected: PASS (all config tests, including the new ones).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): SP3 upgrade knobs + acquire dead-timeout"
```

---

### Task 2: Store — `QualitySummary`/`provides`/`quality`, `selection` table, `upgrade_checks` cursor (schema v4)

**Files:**
- Modify: `src/release.rs` (add `QualitySummary` + make `Source::tier_score` pub) — needed by `store.rs`
- Modify: `src/store.rs` (bump `SCHEMA_VERSION` to 4; add `provides`/`quality` to `OwnedRecord`; add `selection` + `upgrade_checks` tables + accessors + slot-key helpers + `SelectionEntry`)
- Test: `src/store.rs` (`mod tests`)

- [ ] **Step 1: Add `QualitySummary` to `release.rs` first (compile dependency)**

In `src/release.rs`, make `tier_score` public:

```rust
impl Source {
    /// Ranking bonus by tier. `Cam` is rejected before this is consulted.
    pub fn tier_score(self) -> i64 {
```

Add at the end of `release.rs` (before `#[cfg(test)]`):

```rust
/// A compact, serialisable snapshot of a release's quality, recorded on `OwnedRecord` at acquire
/// time so the upgrade engine can compare a fresh candidate against what we own without
/// re-parsing the provider listing. All primitives (no enum serde needed); `source_tier` is
/// `Source::tier_score()`, so a larger value is a higher tier.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct QualitySummary {
    pub cached: bool,
    pub source_tier: i64,
    pub resolution: u16,
    pub score: i64,
}

impl QualitySummary {
    pub fn of(r: &ReleaseInfo, prefs: &QualityPrefs) -> Self {
        QualitySummary {
            cached: r.cached,
            source_tier: r.source.tier_score(),
            resolution: r.resolution.unwrap_or(0),
            score: score(r, prefs).unwrap_or(i64::MIN),
        }
    }
}

/// A candidate is a meaningful upgrade over the current owned release iff it is CACHED and
/// represents a concrete category jump: current is uncached, OR a higher source tier, OR a higher
/// resolution. Marginal score wobble (same tier + resolution) is NOT an upgrade (avoids churn).
pub fn is_meaningful_upgrade(current: &QualitySummary, candidate: &QualitySummary) -> bool {
    if !candidate.cached {
        return false;
    }
    if !current.cached {
        return true;
    }
    candidate.source_tier > current.source_tier || candidate.resolution > current.resolution
}
```

Add tests to `release.rs` `mod tests`:

```rust
    #[test]
    fn meaningful_upgrade_requires_cached_category_jump() {
        use super::{is_meaningful_upgrade, QualitySummary};
        let owned_web_1080_cached = QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 1 };
        // uncached candidate is never an upgrade
        let cand_uncached = QualitySummary { cached: false, source_tier: 8_000, resolution: 2160, score: 9 };
        assert!(!is_meaningful_upgrade(&owned_web_1080_cached, &cand_uncached));
        // cached, higher tier → upgrade
        let cand_remux = QualitySummary { cached: true, source_tier: 8_000, resolution: 1080, score: 5 };
        assert!(is_meaningful_upgrade(&owned_web_1080_cached, &cand_remux));
        // cached, same tier + same resolution → NOT an upgrade (marginal)
        let cand_same = QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 999 };
        assert!(!is_meaningful_upgrade(&owned_web_1080_cached, &cand_same));
        // cached, higher resolution → upgrade
        let cand_4k = QualitySummary { cached: true, source_tier: 3_000, resolution: 2160, score: 2 };
        assert!(is_meaningful_upgrade(&owned_web_1080_cached, &cand_4k));
        // owned uncached → any cached candidate upgrades it
        let owned_uncached = QualitySummary { cached: false, source_tier: 6_000, resolution: 1080, score: 0 };
        assert!(is_meaningful_upgrade(&owned_uncached, &cand_same));
    }
```

Run: `cargo test --lib release::tests::meaningful_upgrade_requires_cached_category_jump` → implement above → PASS.

Commit checkpoint (optional within the task): `git add src/release.rs && git commit -m "feat(release): QualitySummary + is_meaningful_upgrade"`.

- [ ] **Step 2: Write the failing store tests**

Add to `src/store.rs` `mod tests` (after the existing owned tests):

```rust
    #[tokio::test]
    async fn owned_record_provides_and_quality_round_trip() {
        let store = mem_store();
        let rec = OwnedRecord {
            request: req("tt1", 1396),
            provenance: Provenance::manual(),
            added_at: 100,
            status: OwnedStatus::Pending,
            provides: vec![(1, 1), (1, 2)],
            quality: Some(crate::release::QualitySummary { cached: true, source_tier: 6_000, resolution: 1080, score: 42 }),
        };
        store.put_owned("h1".to_string(), rec.clone()).await.unwrap();
        let got = store.get_owned("h1".to_string()).await.unwrap();
        assert_eq!(got.provides, vec![(1, 1), (1, 2)]);
        assert_eq!(got.quality.unwrap().source_tier, 6_000);
    }

    /// An old OwnedRecord JSON (no `provides`/`quality` keys) decodes with empty/None defaults.
    #[test]
    fn owned_record_old_encoding_defaults_provides_and_quality() {
        let old = serde_json::json!({
            "request": serde_json::to_value(req("tt1", 27205)).unwrap(),
            "provenance": serde_json::to_value(Provenance::manual()).unwrap(),
            "added_at": 1,
            "status": "Verified",
        });
        let decoded: OwnedRecord = serde_json::from_value(old).unwrap();
        assert!(decoded.provides.is_empty());
        assert!(decoded.quality.is_none());
    }

    #[tokio::test]
    async fn selection_round_trip_and_remove() {
        let store = mem_store();
        let slot = crate::store::episode_slot(1396, 1, 2);
        store.put_selection(slot.clone(), SelectionEntry { hash: "h1".into(), file_path: "S01E02.mkv".into() }).await.unwrap();
        let got = store.get_selection(slot.clone()).await.unwrap();
        assert_eq!(got.hash, "h1");
        assert_eq!(got.file_path, "S01E02.mkv");
        assert_eq!(store.all_selection().await.len(), 1);
        store.remove_selection(slot.clone()).await.unwrap();
        assert!(store.get_selection(slot).await.is_none());
    }

    #[tokio::test]
    async fn upgrade_checked_cursor_round_trip() {
        let store = mem_store();
        assert_eq!(store.get_upgrade_checked(1396).await, 0, "absent → 0");
        store.set_upgrade_checked(1396, 1_700_000_000).await.unwrap();
        assert_eq!(store.get_upgrade_checked(1396).await, 1_700_000_000);
    }

    #[test]
    fn slot_keys_distinguish_movie_and_episode() {
        assert_eq!(crate::store::movie_slot(27205), "m|27205");
        assert_eq!(crate::store::episode_slot(1396, 1, 2), "e|1396|1|2");
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test --lib store::tests::selection_round_trip_and_remove`
Expected: FAIL — `SelectionEntry`/`put_selection`/`episode_slot` not found, and `OwnedRecord` has no `provides`/`quality`.

- [ ] **Step 4: Implement the store changes**

In `src/store.rs`:

(a) Bump the version and document it:

```rust
/// v3→v4: additive (selection, upgrade_checks tables; OwnedRecord.provides/quality fields).
pub const SCHEMA_VERSION: u64 = 4;
```

(b) Add table constants near the others:

```rust
/// SP3 live-selection: slot ("m|tmdb" / "e|tmdb|s|e") -> serde_json(SelectionEntry).
const SELECTION_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("selection");
/// SP3 upgrade round-robin cursor: tmdb_id (as string) -> last-checked unix secs.
const UPGRADE_CHECKS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("upgrade_checks");
```

(c) Extend `OwnedRecord`:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OwnedRecord {
    pub request: AcquireRequest,
    #[serde(default)]
    pub provenance: Provenance,
    pub added_at: u64,
    pub status: OwnedStatus,
    /// (season, episode) pairs this hash supplies. Single-episode ⇒ one pair; movie ⇒ empty;
    /// season pack ⇒ the full set (computed in `observe` from the resolved files). The union
    /// across a show's hashes is the show's owned-episode set (kills season-pack churn).
    #[serde(default)]
    pub provides: Vec<(u32, u32)>,
    /// Quality snapshot of the chosen release, for upgrade comparison. `None` on pre-SP3 records.
    #[serde(default)]
    pub quality: Option<crate::release::QualitySummary>,
}
```

(d) Add `SelectionEntry` and slot-key helpers (top-level in `store.rs`, near `AcquireRequest`):

```rust
/// The live representative for one VFS slot (movie or episode): which hash + which file path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SelectionEntry {
    pub hash: String,
    pub file_path: String,
}

/// Slot key for a movie's selection entry.
pub fn movie_slot(tmdb_id: u64) -> String {
    format!("m|{}", tmdb_id)
}
/// Slot key for one episode's selection entry.
pub fn episode_slot(tmdb_id: u64, season: u32, episode: u32) -> String {
    format!("e|{}|{}|{}", tmdb_id, season, episode)
}
```

(e) Create the new tables in `ensure_schema` (inside the write txn block, alongside the others):

```rust
            write_txn.open_table(SELECTION_TABLE)?;       // create if absent
            write_txn.open_table(UPGRADE_CHECKS_TABLE)?;  // create if absent
```

(f) Add accessors (place after the wanted accessors, before the closing `}` of `impl Store`). Copy the existing async/spawn_blocking/serialise-first pattern:

```rust
    // ── selection accessors (SP3) ─────────────────────────────────────────────

    pub async fn put_selection(&self, slot: String, entry: SelectionEntry) -> Result<(), AppError> {
        let bytes = match serde_json::to_vec(&entry) {
            Ok(b) => b,
            Err(e) => { error!("Failed to serialise SelectionEntry for {}: {}", slot, e); return Ok(()); }
        };
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(SELECTION_TABLE)?.insert(slot.as_str(), bytes.as_slice())?; }
            txn.commit()?;
            Ok(())
        }).await;
        Self::flatten_join(result)
    }

    pub async fn get_selection(&self, slot: String) -> Option<SelectionEntry> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = db.begin_read().ok()?;
            let table = txn.open_table(SELECTION_TABLE).ok()?;
            let e = table.get(slot.as_str()).ok()??;
            serde_json::from_slice::<SelectionEntry>(e.value()).ok()
        }).await.ok().flatten()
    }

    pub async fn remove_selection(&self, slot: String) -> Result<(), AppError> {
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(SELECTION_TABLE)?.remove(slot.as_str())?; }
            txn.commit()?;
            Ok(())
        }).await;
        Self::flatten_join(result)
    }

    pub async fn all_selection(&self) -> Vec<(String, SelectionEntry)> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            if let Ok(txn) = db.begin_read() {
                if let Ok(table) = txn.open_table(SELECTION_TABLE) {
                    if let Ok(iter) = table.iter() {
                        for entry in iter.flatten() {
                            let (k, v) = entry;
                            if let Ok(rec) = serde_json::from_slice::<SelectionEntry>(v.value()) {
                                out.push((k.value().to_string(), rec));
                            }
                        }
                    }
                }
            }
            out
        }).await.unwrap_or_default()
    }

    // ── upgrade_checks cursor (SP3) ───────────────────────────────────────────

    pub async fn get_upgrade_checked(&self, tmdb_id: u64) -> u64 {
        let key = tmdb_id.to_string();
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = match db.begin_read() { Ok(t) => t, Err(_) => return 0 };
            let table = match txn.open_table(UPGRADE_CHECKS_TABLE) { Ok(t) => t, Err(_) => return 0 };
            table.get(key.as_str()).ok().flatten().map(|g| g.value()).unwrap_or(0)
        }).await.unwrap_or(0)
    }

    pub async fn set_upgrade_checked(&self, tmdb_id: u64, at: u64) -> Result<(), AppError> {
        let key = tmdb_id.to_string();
        let db = self.db.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let txn = db.begin_write()?;
            { txn.open_table(UPGRADE_CHECKS_TABLE)?.insert(key.as_str(), &at)?; }
            txn.commit()?;
            Ok(())
        }).await;
        Self::flatten_join(result)
    }
```

(g) Fix every existing `OwnedRecord { .. }` literal in `store.rs` tests and elsewhere to include the two new fields. The struct-update `..` is not used for `OwnedRecord` literals; the simplest fix is to add `provides: vec![], quality: None,` to each. In `store.rs` tests these are in: `owned_round_trip_and_status_update`, `migrates_v2_db_to_v3_preserving_tables`, `provenance_round_trips_through_store`. Add the two fields to each literal.

(NOTE: other modules — `acquire.rs`, `scheduler.rs`, `tasks.rs` — also construct `OwnedRecord`. Those are updated in their own tasks. To keep the build green after THIS task, add `provides: vec![], quality: None,` to EVERY `OwnedRecord { .. }` literal across the crate now — a quick compiler-guided sweep. `cargo build` will name each one.)

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib store::`
Expected: PASS. Then `cargo test` (whole suite) to confirm the OwnedRecord field sweep compiles everywhere.

- [ ] **Step 6: Commit**

```bash
git add src/store.rs src/release.rs
git commit -m "feat(store): schema v4 — selection table, owned provides/quality, upgrade cursor"
```

---

### Task 3: Read-activity tracker module

**Files:**
- Create: `src/read_activity.rs`
- Modify: `src/mapper.rs` (add `pub mod read_activity;`)
- Test: `src/read_activity.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Create `src/read_activity.rs`:

```rust
//! In-memory proxy read-activity tracker (SP3). The WebDAV read path stamps a path on every
//! `read_bytes`; the upgrade engine consults `is_idle` before swapping/pruning a slot so a file
//! that is being streamed is never pulled out from under a player. Best-effort and in-memory only:
//! after a restart everything reads idle (there are no pre-existing open handles to disturb).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

#[derive(Clone, Default)]
pub struct ReadActivity {
    last_read: Arc<RwLock<HashMap<String, Instant>>>,
}

impl ReadActivity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `path` was just read. Cheap; called on every proxy read.
    pub async fn touch(&self, path: &str) {
        self.last_read.write().await.insert(path.to_string(), Instant::now());
    }

    /// `true` if `path` has had no read within `window` (never-read counts as idle).
    pub async fn is_idle(&self, path: &str, window: Duration) -> bool {
        match self.last_read.read().await.get(path) {
            Some(t) => t.elapsed() >= window,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn never_read_is_idle() {
        let ra = ReadActivity::new();
        assert!(ra.is_idle("Movies/X/x.mkv", Duration::from_secs(300)).await);
    }

    #[tokio::test]
    async fn just_touched_is_not_idle_then_becomes_idle() {
        let ra = ReadActivity::new();
        ra.touch("p").await;
        assert!(!ra.is_idle("p", Duration::from_secs(300)).await, "just-read is active");
        // A zero-length window makes any elapsed time count as idle.
        assert!(ra.is_idle("p", Duration::from_secs(0)).await);
    }
}
```

In `src/mapper.rs`, add (with the other `pub mod` lines, alphabetically near `provider`/`release`):

```rust
pub mod read_activity;
```

- [ ] **Step 2: Run the test to verify it fails, then passes**

Run: `cargo test --lib read_activity::`
Expected first: FAIL (module not declared) → after adding the `pub mod`, PASS.

- [ ] **Step 3: Commit**

```bash
git add src/read_activity.rs src/mapper.rs
git commit -m "feat(read_activity): in-memory proxy read-activity tracker"
```

---

### Task 4: Wire `ReadActivity` into `AppState`, `dav_fs`, and `main.rs`

**Files:**
- Modify: `src/app_state.rs` (add `read_activity` field)
- Modify: `src/dav_fs.rs` (thread `ReadActivity` into `DebridFileSystem` + `ProxiedMediaFile`; stamp on read)
- Modify: `src/main.rs` (construct one `ReadActivity`, put it in `AppState`, pass it to `DebridFileSystem::new`)
- Modify: `src/tasks.rs`, `src/scheduler.rs` test helper, `src/tasks.rs` `scan_config_holds_app_state` test (add the field to every `AppState { .. }` literal)
- Test: `src/dav_fs.rs` (assert the read path touches the tracker — see below)

- [ ] **Step 1: Add the field to `AppState`**

In `src/app_state.rs` add to the struct:

```rust
    /// Proxy read-activity tracker (SP3) — stamped by `dav_fs` reads, read by the upgrade engine.
    pub read_activity: Arc<crate::read_activity::ReadActivity>,
```

- [ ] **Step 2: Thread it through `dav_fs`**

In `src/dav_fs.rs`:

Add field to `DebridFileSystem`:

```rust
    read_activity: Arc<crate::read_activity::ReadActivity>,
```

Change `DebridFileSystem::new` to accept it (add param + assign). New signature:

```rust
    pub fn new(
        rd_client: Arc<dyn DebridProvider>,
        vfs: Arc<RwLock<DebridVfs>>,
        repair_manager: Arc<RepairManager>,
        http_client: reqwest::Client,
        read_activity: Arc<crate::read_activity::ReadActivity>,
    ) -> Self {
        Self { vfs, rd_client, repair_manager, http_client, read_activity }
    }
```

Add a field to `ProxiedMediaFile`:

```rust
    read_activity: Arc<crate::read_activity::ReadActivity>,
    vfs_path: String,
```

Find where `ProxiedMediaFile { .. }` is constructed (in the `open` impl — search for `ProxiedMediaFile {`). Add to that literal:

```rust
                read_activity: self.read_activity.clone(),
                vfs_path: path_str.to_string(),
```

where `path_str` is the requested VFS path string. Inside `open`, derive it from the `DavPath` argument: add near the top of the `open` future `let vfs_path = path.as_rel_ospath().to_string_lossy().into_owned();` and use `vfs_path` in the literal (clone it). Match the existing `open` body's variable names; the key requirement is that `vfs_path` equals the slash-joined relative path so it matches the timestamps/selection-derived keys used elsewhere.

Stamp on read — change `read_bytes`:

```rust
    fn read_bytes(&mut self, len: usize) -> FsFuture<'_, Bytes> {
        async move {
            if self
                .repair_manager
                .should_hide_torrent(&self.locator.torrent_id)
                .await
            {
                return Err(FsError::GeneralFailure);
            }
            self.read_activity.touch(&self.vfs_path).await;
            self.fetch_bytes(len).await
        }
        .boxed()
    }
```

- [ ] **Step 3: Update `main.rs` wiring**

In `src/main.rs`, before building `AppState`, construct the tracker:

```rust
    let read_activity = Arc::new(debridmoviemapper::read_activity::ReadActivity::new());
```

Add `read_activity: read_activity.clone(),` to the `AppState { .. }` literal. Change the `DebridFileSystem::new(...)` call to pass it:

```rust
    let dav_fs = DebridFileSystem::new(
        app_state.provider.clone(),
        app_state.vfs.clone(),
        app_state.repair_manager.clone(),
        app_state.http_client.clone(),
        app_state.read_activity.clone(),
    );
```

- [ ] **Step 4: Fix the other `AppState { .. }` literals**

`cargo build` will flag them. Add `read_activity: Arc::new(crate::read_activity::ReadActivity::new()),` (or the `debridmoviemapper::` path in integration tests) to:
- `src/scheduler.rs` `make_test_app`
- `src/tasks.rs` `scan_config_holds_app_state`
- any integration test that builds `AppState` (compiler will name them)

- [ ] **Step 5: Write a dav_fs read-activity test**

Add to `src/dav_fs.rs` `#[cfg(test)] mod provider_abstraction_tests` (or the existing test module). A focused unit test that constructs a `ProxiedMediaFile` directly and asserts `read_bytes` touches the tracker:

```rust
    #[tokio::test]
    async fn read_bytes_stamps_read_activity() {
        use crate::read_activity::ReadActivity;
        let ra = Arc::new(ReadActivity::new());
        let provider: Arc<dyn DebridProvider> = Arc::new(crate::provider::MockProvider {
            resolved_url: Some("http://127.0.0.1:0/none".into()),
            ..Default::default()
        });
        let mut f = ProxiedMediaFile {
            name: "x.mkv".into(),
            locator: crate::provider::FileLocator { torrent_id: "t".into(), ..Default::default() },
            file_size: 10,
            repair_manager: Arc::new(RepairManager::new(provider.clone())),
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: None,
            buffer: Bytes::new(),
            buffer_start: 0,
            read_activity: ra.clone(),
            vfs_path: "Movies/X/x.mkv".into(),
        };
        // The CDN fetch will fail (unroutable URL), but the stamp happens before the fetch.
        let _ = f.read_bytes(4).await;
        assert!(!ra.is_idle("Movies/X/x.mkv", std::time::Duration::from_secs(300)).await);
    }
```

(`ProxiedMediaFile` is private; this test lives in `dav_fs.rs`'s own test module so it can name it. If the field set differs, match the struct exactly.)

- [ ] **Step 6: Run and commit**

Run: `cargo test` (whole suite).
Expected: PASS.

```bash
git add src/app_state.rs src/dav_fs.rs src/main.rs src/scheduler.rs src/tasks.rs
git commit -m "feat(dav_fs): stamp proxy read-activity; thread ReadActivity through AppState"
```

---

## Phase B — selection inversion & reconciliation rework

### Task 5: `vfs::build` selection inversion (with largest-bytes fallback)

**Files:**
- Modify: `src/vfs.rs` (`build` gains a `selection: &SelectionMap` argument; movie + per-episode override; helper `parse_se`)
- Modify: `src/tasks.rs` (`update_vfs` builds and passes the selection map — done fully in Task 7; for THIS task, update the single call so it compiles by passing an empty map)
- Test: `src/vfs.rs` (`#[cfg(test)] mod tests` — add if absent at bottom of file)

- [ ] **Step 1: Define `SelectionMap` and the parse helper**

In `src/vfs.rs`, add near the top (after `use` lines):

```rust
/// Resolved live-selection for `build`: slot key (`store::movie_slot`/`episode_slot`) ->
/// `SelectionEntry`. An empty map ⇒ no managed selection ⇒ build uses its largest-bytes fallback,
/// i.e. exactly the pre-SP3 behaviour (external / un-managed torrents are unaffected).
pub type SelectionMap = std::collections::HashMap<String, crate::store::SelectionEntry>;

/// Parse a SxxExx episode code from a file name. Mirrors `acquire::parse_se`.
fn parse_se(name: &str) -> Option<(u32, u32)> {
    static SE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)s(\d{1,2})e(\d{1,3})").unwrap());
    let c = SE.captures(name)?;
    Some((c.get(1)?.as_str().parse().ok()?, c.get(2)?.as_str().parse().ok()?))
}

/// Extract the numeric tmdb id from a `MediaMetadata.external_id` like `"tmdb:1396"`.
fn tmdb_id_of(metadata: &MediaMetadata) -> Option<u64> {
    metadata.external_id.as_deref()
        .and_then(|s| s.strip_prefix("tmdb:"))
        .and_then(|s| s.parse::<u64>().ok())
}
```

- [ ] **Step 2: Write the failing tests**

Add a `#[cfg(test)] mod selection_tests` at the bottom of `src/vfs.rs`:

```rust
#[cfg(test)]
mod selection_tests {
    use super::*;
    use crate::rd_client::{TorrentFile, TorrentInfo};
    use crate::store::{episode_slot, movie_slot, SelectionEntry};

    fn movie_meta(tmdb: u64) -> MediaMetadata {
        MediaMetadata { title: "Movie".into(), year: Some("2023".into()), media_type: MediaType::Movie, external_id: Some(format!("tmdb:{}", tmdb)) }
    }
    fn show_meta(tmdb: u64) -> MediaMetadata {
        MediaMetadata { title: "Show".into(), year: None, media_type: MediaType::Show, external_id: Some(format!("tmdb:{}", tmdb)) }
    }
    fn movie_torrent(id: &str, hash: &str, bytes: u64) -> TorrentInfo {
        TorrentInfo {
            id: id.into(), hash: hash.into(), bytes, status: "downloaded".into(),
            files: vec![TorrentFile { id: 0, path: "Movie.2023.1080p.mkv".into(), bytes, selected: 1 }],
            links: vec!["https://cdn/movie".into()],
            ..Default::default()
        }
    }

    fn movie_file_locators(vfs: &DebridVfs) -> Vec<FileLocator> {
        // Collect every MediaFile locator under Movies/.
        fn walk(n: &VfsNode, out: &mut Vec<FileLocator>) {
            match n {
                VfsNode::Directory { children } => for c in children.values() { walk(c, out) },
                VfsNode::MediaFile { locator, .. } => out.push(locator.clone()),
                VfsNode::VirtualFile { .. } => {}
            }
        }
        let mut out = Vec::new();
        if let VfsNode::Directory { children } = &vfs.root {
            if let Some(m) = children.get("Movies") { walk(m, &mut out); }
        }
        out
    }

    #[test]
    fn movie_empty_selection_falls_back_to_largest() {
        // Two torrents for one movie; no selection ⇒ largest (h_big) wins (legacy behaviour).
        let torrents = vec![
            (movie_torrent("small", "h_small", 1_000_000_000), movie_meta(27205)),
            (movie_torrent("big", "h_big", 9_000_000_000), movie_meta(27205)),
        ];
        let vfs = DebridVfs::build(torrents, &SelectionMap::new());
        let locs = movie_file_locators(&vfs);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].hash, "h_big", "largest-bytes fallback picks the big torrent");
    }

    #[test]
    fn movie_selection_overrides_largest() {
        // Selection points at the SMALL torrent's hash ⇒ it must win over the larger one.
        let mut sel = SelectionMap::new();
        sel.insert(movie_slot(27205), SelectionEntry { hash: "h_small".into(), file_path: "Movie.2023.1080p.mkv".into() });
        let torrents = vec![
            (movie_torrent("small", "h_small", 1_000_000_000), movie_meta(27205)),
            (movie_torrent("big", "h_big", 9_000_000_000), movie_meta(27205)),
        ];
        let vfs = DebridVfs::build(torrents, &sel);
        let locs = movie_file_locators(&vfs);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].hash, "h_small", "selection overrides the largest-bytes default");
    }

    #[test]
    fn episode_selection_picks_the_selected_hash_per_episode() {
        // Two torrents both contain S01E01; selection picks the per-episode hash "h_pack".
        let pack = TorrentInfo {
            id: "pack".into(), hash: "h_pack".into(), bytes: 5_000_000_000, status: "downloaded".into(),
            files: vec![TorrentFile { id: 0, path: "Show.S01E01.1080p.mkv".into(), bytes: 2_000_000_000, selected: 1 }],
            links: vec!["https://cdn/pack-e1".into()],
            ..Default::default()
        };
        let single = TorrentInfo {
            id: "single".into(), hash: "h_single".into(), bytes: 9_000_000_000, status: "downloaded".into(),
            files: vec![TorrentFile { id: 0, path: "Show.S01E01.2160p.mkv".into(), bytes: 9_000_000_000, selected: 1 }],
            links: vec!["https://cdn/single-e1".into()],
            ..Default::default()
        };
        let mut sel = SelectionMap::new();
        sel.insert(episode_slot(1396, 1, 1), SelectionEntry { hash: "h_pack".into(), file_path: "Show.S01E01.1080p.mkv".into() });
        // single is larger, so without selection it would win; selection forces h_pack.
        let vfs = DebridVfs::build(vec![(single, show_meta(1396)), (pack, show_meta(1396))], &sel);
        // Find the S01E01 media file's locator.
        let mut found: Option<FileLocator> = None;
        if let VfsNode::Directory { children } = &vfs.root {
            if let Some(VfsNode::Directory { children: shows }) = children.get("Shows") {
                for show in shows.values() {
                    if let VfsNode::Directory { children: seasons } = show {
                        if let Some(VfsNode::Directory { children: eps }) = seasons.get("Season 01") {
                            for n in eps.values() {
                                if let VfsNode::MediaFile { locator, .. } = n { found = Some(locator.clone()); }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(found.expect("S01E01 present").hash, "h_pack", "per-episode selection wins");
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test --lib vfs::selection_tests`
Expected: FAIL — `DebridVfs::build` takes one arg, not two.

- [ ] **Step 4: Change `build` to consult the selection map**

Change the signature:

```rust
    pub fn build(torrents: Vec<(TorrentInfo, MediaMetadata)>, selection: &SelectionMap) -> Self {
```

**Movie path** — replace the movie branch's torrent pick. Currently:

```rust
                MediaType::Movie => {
                    let mut children = BTreeMap::new();
                    // For movies, only take the largest torrent to avoid duplicates
                    if let Some(torrent) = torrents.first() {
```

with a selection-aware pick (insert immediately after `let mut children = BTreeMap::new();`):

```rust
                    // SP3: if a managed selection names a hash present in this group, that torrent
                    // represents the movie; else fall back to the largest (torrents are size-sorted).
                    let chosen = tmdb_id_of(&metadata)
                        .and_then(|id| selection.get(&crate::store::movie_slot(id)))
                        .and_then(|sel| torrents.iter().find(|t| t.hash.eq_ignore_ascii_case(&sel.hash)))
                        .or_else(|| torrents.first());
                    if let Some(torrent) = chosen {
```

(Leave the rest of the movie branch unchanged — it uses `torrent`.)

**Show path** — make per-episode selection authoritative. Just before the `for torrent in torrents {` loop in the `MediaType::Show` branch, compute the show's per-episode overrides:

```rust
                    // SP3: per-episode selection overrides for this show (slot -> entry).
                    let show_tmdb = tmdb_id_of(&metadata);
                    // Hashes present in this show's group — a selection naming an absent hash is
                    // stale and must NOT hide the episode (self-healing). Compute BEFORE the loop
                    // below moves `torrents`.
                    let present_hashes: std::collections::HashSet<String> =
                        torrents.iter().map(|t| t.hash.to_ascii_lowercase()).collect();
```

Inside the loop, where a video file is about to be added (the block guarded by `if is_video_file(&file.path) {` and after `let filename = ...`), insert a selection gate right after `filename` is computed and before the `season` computation:

```rust
                                        // SP3 selection gate: if this episode has a managed
                                        // selection, only the selected (hash, file_path) is used;
                                        // any other torrent's copy of the episode is skipped. With
                                        // no selection entry, the legacy first/largest-wins dedup
                                        // below applies unchanged.
                                        if let (Some(tmdb), Some((se_s, se_e))) =
                                            (show_tmdb, parse_se(filename))
                                        {
                                            if let Some(sel) = selection
                                                .get(&crate::store::episode_slot(tmdb, se_s, se_e))
                                            {
                                                // Enforce the override only when the selected hash
                                                // is actually present; a stale selection degrades to
                                                // the legacy dedup so the episode still appears.
                                                if present_hashes.contains(&sel.hash.to_ascii_lowercase()) {
                                                    let is_selected = torrent.hash.eq_ignore_ascii_case(&sel.hash)
                                                        && file.path == sel.file_path;
                                                    if !is_selected {
                                                        link_idx += 1;
                                                        continue;
                                                    }
                                                }
                                            }
                                        }
```

**Placement:** the gate goes immediately after `let filename = file.path.split('/').next_back()...;` and **before** `let season = SEASON_RE...`, at the same nesting level as the `filename` binding (inside the `if link.is_some() || torrent.links.is_empty()` block). The existing code runs `link_idx += 1` at the end of the per-file `if file.selected == 1 { ... }` block, so a `continue` from the gate must do `link_idx += 1` first (as shown above) to keep link/file alignment — exactly mirroring the existing dedup-skip `continue`.

- [ ] **Step 5: Make the single production caller compile**

In `src/tasks.rs` `update_vfs`, change:

```rust
    let new_vfs = DebridVfs::build(filtered);
```

to (temporary — Task 7 builds the real map):

```rust
    let new_vfs = DebridVfs::build(filtered, &crate::vfs::SelectionMap::new());
```

Also update any other `DebridVfs::build(` call sites flagged by the compiler (e.g. existing vfs tests) to pass `&SelectionMap::new()` / `&crate::vfs::SelectionMap::new()`.

- [ ] **Step 6: Run tests and commit**

Run: `cargo test --lib vfs::` then `cargo test`.
Expected: PASS.

```bash
git add src/vfs.rs src/tasks.rs
git commit -m "refactor(vfs): inject selection into build() with largest-bytes fallback"
```

---

### Task 6: Optimistic `acquire` + `observe` resolver (dead-timeout, deferred gates, provides, selection, re-scrape)

**Files:**
- Modify: `src/acquire.rs` (rewrite `acquire` to optimistic-add; rewrite `observe`/`verify_pending`/`fail_and_reacquire`; add `select_ids_for`, `episode_files`, `compute_provides`, `write_selection`, `dead_timeout` field)
- Test: `src/acquire.rs` (`mod tests`)

This is the largest task — multiple commits. The `AcquisitionEngine` gains a `dead_timeout: Duration` field and a `prefs` is already present.

- [ ] **Step 1 (commit 1): Add `dead_timeout` + `prefs`-based quality to the engine; make `acquire` optimistic**

Add a field to `AcquisitionEngine`:

```rust
    dead_timeout: Duration,
```

Change `AcquisitionEngine::new` to accept it as the last parameter:

```rust
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn DebridProvider>,
        scraper: Arc<dyn Scraper>,
        validator: Arc<dyn TitleValidator>,
        prober: Arc<dyn Prober>,
        store: Store,
        prefs: QualityPrefs,
        max_attempts: u32,
        stall_timeout: Duration,
        dead_timeout: Duration,
    ) -> Self {
        Self {
            provider, scraper, validator, prober, store, prefs, max_attempts,
            stall_timeout, dead_timeout,
            progress: Arc::new(Mutex::new(HashMap::new())),
            verify_attempts: Arc::new(Mutex::new(HashMap::new())),
        }
    }
```

Update all `AcquisitionEngine::new(...)` call sites to pass the dead timeout:
- `src/acquire.rs` test helper `engine(...)`: add `, Duration::from_secs(600)` (and make it short where a test needs a fast dead-timeout — see Step 4).
- `src/main.rs`: `std::time::Duration::from_secs(config.acquisition.acquire_dead_timeout_secs)`.
- `src/scheduler.rs` `make_test_app` and `src/tasks.rs` `scan_config_holds_app_state`: `std::time::Duration::from_secs(600)`.

Add the magnet + selection helpers (free functions near `select_file_ids`):

```rust
/// Select file ids appropriate to the request kind: a single target video for a movie (so the
/// movie-pack guard can reject multi-feature packs), or ALL video files for a series (so a season
/// pack downloads fully on providers that don't auto-select, and `provides` covers every episode).
fn select_ids_for(kind: MediaKind, info: &TorrentInfo, hint: Option<&str>, idx: Option<usize>) -> Vec<u32> {
    match kind {
        MediaKind::Movie => select_file_ids(info, hint, idx),
        MediaKind::Series => info.files.iter()
            .filter(|f| crate::vfs::is_video_file(&f.path))
            .map(|f| f.id)
            .collect(),
    }
}

/// Map a torrent's SELECTED video files to (season, episode, file_path) by parsing SxxExx.
/// `pub(crate)` so the upgrade engine can reuse it for consolidation.
pub(crate) fn episode_files(info: &TorrentInfo) -> Vec<(u32, u32, String)> {
    info.files.iter()
        .filter(|f| f.selected == 1 && crate::vfs::is_video_file(&f.path))
        .filter_map(|f| {
            let name = f.path.rsplit('/').next().unwrap_or(&f.path);
            parse_se(name).map(|(s, e)| (s, e, f.path.clone()))
        })
        .collect()
}
```

Rewrite `acquire` to optimistic-add (replace the whole `pub async fn acquire` body):

```rust
    /// Optimistically acquire `req`: scrape, rank, add the best non-blacklisted candidate, record
    /// it `Pending`, and return — WITHOUT synchronously selecting/validating/probing (that is
    /// `observe`'s job once the torrent's files resolve). A slow-to-seed release is therefore no
    /// longer judged or deleted prematurely. `provenance` is recorded and preserved across
    /// `observe`'s re-acquire (sticky).
    pub async fn acquire(&self, req: AcquireRequest, provenance: Provenance) -> AcquireOutcome {
        let candidates = match self
            .scraper
            .find(&req.imdb_id, req.kind, req.season, req.episode)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!("scrape failed for {}: {}", req.imdb_id, e);
                return AcquireOutcome::TemporarilyUnavailable;
            }
        };
        let mut parsed: Vec<ReleaseInfo> = Vec::new();
        for c in &candidates {
            let r = release::parse(c);
            if self.store.is_blacklisted(req.tmdb_id, r.info_hash.clone()).await {
                continue;
            }
            parsed.push(r);
        }
        let ranked = release::rank(parsed, &self.prefs);

        for cand in ranked.into_iter().take(self.max_attempts as usize) {
            if self.store.get_owned(cand.info_hash.clone()).await.is_some() {
                return AcquireOutcome::Acquired(cand.info_hash.clone()); // idempotent
            }
            let magnet = format!("magnet:?xt=urn:btih:{}", cand.info_hash);
            let added = match self.provider.add_magnet(&magnet).await {
                Ok(a) => a,
                Err(e) => {
                    warn!("add_magnet failed for {}: {} — trying next", cand.info_hash, e);
                    continue;
                }
            };
            // Record Pending immediately (the verdict belongs to observe).
            let provides = match (req.kind, req.season, req.episode) {
                (MediaKind::Series, Some(s), Some(e)) => vec![(s, e)],
                _ => vec![],
            };
            let _ = self.store.put_owned(
                cand.info_hash.clone(),
                OwnedRecord {
                    request: req.clone(),
                    provenance: provenance.clone(),
                    added_at: now_secs(),
                    status: OwnedStatus::Pending,
                    provides,
                    quality: Some(release::QualitySummary::of(&cand, &self.prefs)),
                },
            ).await;
            let _ = self.store.put_authoritative(cand.info_hash.clone(), req.metadata.clone()).await;
            // Best-effort: if the file list is already present (cached), select now so it is
            // immediately resolvable; otherwise observe selects once metadata resolves.
            if let Ok(info) = self.provider.get_torrent_info(&added.id).await {
                let ids = select_ids_for(req.kind, &info, cand.file_name.as_deref(), cand.file_idx);
                if !ids.is_empty() {
                    let csv = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
                    let _ = self.provider.select_files(&added.id, &csv).await;
                }
            }
            return AcquireOutcome::Pending(cand.info_hash);
        }
        AcquireOutcome::NoAcceptableRelease
    }
```

Delete the now-unused `try_candidate`, `cleanup_leaked`, `count_feature_videos` will move to observe (keep it — observe uses it), `CandidateResult` enum (delete), `locator_for` (keep — observe uses it). `verify_file` stays (observe uses it). Keep `select_target`, `select_file_ids`, `parse_se`, `now_secs`.

Build will flag dead code; remove `try_candidate`, `cleanup_leaked`, `CandidateResult` if unused after the rewrite (observe in commit 2 reuses `count_feature_videos`, `select_target`, `locator_for`, `verify_file`).

- [ ] **Step 2 (commit 1): Update the acquire tests to the optimistic model**

The existing tests assert the OLD synchronous outcomes. Rewrite them:

```rust
    #[tokio::test]
    async fn acquire_records_pending_and_quality_optimistically() {
        let st = store();
        let scraper = Arc::new(MockScraper { candidates: vec![cand("h1", true)] });
        let eng = engine(
            provider_returning("downloaded", "h1"),
            scraper,
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        let out = eng.acquire(req(), Provenance::watchlist("alice")).await;
        assert_eq!(out, AcquireOutcome::Pending("h1".into()), "acquire is optimistic: always Pending");
        let rec = st.get_owned("h1".into()).await.unwrap();
        assert_eq!(rec.status, OwnedStatus::Pending);
        assert_eq!(rec.provenance, Provenance::watchlist("alice"));
        assert!(rec.quality.unwrap().cached, "cached candidate's quality recorded");
        assert_eq!(
            st.authoritative_meta("h1".into()).await.unwrap().external_id.as_deref(),
            Some("tmdb:27205")
        );
    }

    #[tokio::test]
    async fn acquire_idempotent_when_already_owned() {
        let st = store();
        st.put_owned("h1".into(), OwnedRecord {
            request: req(), provenance: Provenance::manual(), added_at: 1,
            status: OwnedStatus::Verified, provides: vec![], quality: None,
        }).await.unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![cand("h1", true)] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        assert_eq!(eng.acquire(req(), Provenance::manual()).await, AcquireOutcome::Acquired("h1".into()));
    }

    #[tokio::test]
    async fn acquire_no_candidates_is_no_acceptable() {
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            store(),
        );
        assert_eq!(eng.acquire(req(), Provenance::manual()).await, AcquireOutcome::NoAcceptableRelease);
    }
```

Delete the old tests that asserted synchronous Acquired/blacklist-on-acquire (`cached_pass_records_owned_and_authoritative`, `wrong_title_is_blacklisted_and_not_recorded`, `bad_audio_blacklists_and_returns_no_acceptable`, `uncached_pick_returns_pending`, `inconclusive_probe_accepts`, `already_owned_is_idempotent`, `movie_pack_candidate_is_rejected`, `cleanup_leaked_deletes_only_hash_matching_torrents`). Their behaviours move to `observe` and are re-tested in Step 4. Keep `observe_caps_deferred_probes_and_accepts` but update its `OwnedRecord` literal to add `provides: vec![], quality: None,`.

Run: `cargo test --lib acquire::` → PASS. Commit:

```bash
git add src/acquire.rs src/main.rs src/scheduler.rs src/tasks.rs
git commit -m "refactor(acquire): optimistic add — record Pending, defer the verdict to observe"
```

- [ ] **Step 3 (commit 2): Rewrite `observe` as the resolver**

Add the provides/selection writer helper as a method on `AcquisitionEngine`:

```rust
    /// Write `provides` and the per-slot `selection` entries for a now-Verified owned torrent,
    /// and update its status. For a movie: one `movie_slot` entry for the selected file. For a
    /// show: `episode_slot` entries for every SE-mapped selected file, and `provides` is that set.
    async fn record_verified(&self, hash: &str, req: &AcquireRequest, info: &TorrentInfo, selected_path: &str) {
        match req.kind {
            MediaKind::Movie => {
                if let Some(id) = tmdb_to_u64(&req.metadata) {
                    let _ = self.store.put_selection(
                        crate::store::movie_slot(id),
                        crate::store::SelectionEntry { hash: hash.to_string(), file_path: selected_path.to_string() },
                    ).await;
                }
            }
            MediaKind::Series => {
                let eps = episode_files(info);
                if let Some(id) = tmdb_to_u64(&req.metadata) {
                    for (s, e, path) in &eps {
                        let _ = self.store.put_selection(
                            crate::store::episode_slot(id, *s, *e),
                            crate::store::SelectionEntry { hash: hash.to_string(), file_path: path.clone() },
                        ).await;
                    }
                }
                // Persist provides = the SE-mapped episode set (the churn fix).
                if let Some(mut rec) = self.store.get_owned(hash.to_string()).await {
                    rec.provides = eps.iter().map(|(s, e, _)| (*s, *e)).collect();
                    let _ = self.store.put_owned(hash.to_string(), rec).await;
                }
            }
        }
        let _ = self.store.set_owned_status(hash.to_string(), OwnedStatus::Verified).await;
    }
```

Add the helper `tmdb_to_u64` (free fn):

```rust
/// Extract the numeric tmdb id from `MediaMetadata.external_id` (`"tmdb:1396"`).
fn tmdb_to_u64(m: &crate::vfs::MediaMetadata) -> Option<u64> {
    m.external_id.as_deref()
        .and_then(|s| s.strip_prefix("tmdb:"))
        .and_then(|s| s.parse::<u64>().ok())
}
```

Replace `observe` with the resolver (work from the owned record, run the deferred gates once files appear, reap after `dead_timeout`):

```rust
    /// Called each scan tick with the current torrent list. Resolves optimistically-added Pending
    /// torrents (select files → pack-guard → title-validation → probe → Verified + provides +
    /// selection), reaps genuinely-dead/never-resolving ones after `dead_timeout`, and recovers by
    /// re-scraping. No scraping on the happy path.
    pub async fn observe(&self, torrents: &[crate::rd_client::Torrent]) {
        let owned = self.store.all_owned().await;
        let by_hash: HashMap<String, &crate::rd_client::Torrent> = torrents
            .iter()
            .map(|t| (t.hash.to_ascii_lowercase(), t))
            .collect();

        for (hash, rec) in &owned {
            let Some(t) = by_hash.get(hash.as_str()).copied() else {
                // Not in the listing. A Pending torrent that never registered/resolved is dead
                // once it has been waiting longer than the dead-timeout.
                if rec.status == OwnedStatus::Pending
                    && now_secs().saturating_sub(rec.added_at) > self.dead_timeout.as_secs()
                {
                    self.fail_and_reacquire(hash, "", &rec.request, "NeverResolved", &rec.provenance).await;
                }
                continue;
            };
            if matches!(t.status.as_str(), "magnet_error" | "dead" | "error" | "virus") {
                self.fail_and_reacquire(hash, &t.id, &rec.request, "Dead", &rec.provenance).await;
                continue;
            }
            if rec.status == OwnedStatus::Verified {
                self.progress.lock().await.remove(&t.id);
                continue;
            }
            // Pending: fetch info to inspect files.
            let info = match self.provider.get_torrent_info(&t.id).await {
                Ok(i) => i,
                Err(_) => continue,
            };
            let has_files = info.files.iter().any(|f| crate::vfs::is_video_file(&f.path));
            if !has_files {
                if now_secs().saturating_sub(rec.added_at) > self.dead_timeout.as_secs() {
                    self.fail_and_reacquire(hash, &t.id, &rec.request, "NeverResolved", &rec.provenance).await;
                }
                continue;
            }
            // Ensure something is selected so it downloads (RD: nothing downloads until selected).
            let none_selected = info.files.iter().all(|f| f.selected != 1);
            if none_selected {
                let ids = select_ids_for(rec.request.kind, &info, None, None);
                if !ids.is_empty() {
                    let csv = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
                    let _ = self.provider.select_files(&t.id, &csv).await;
                }
                continue; // re-inspect next tick after selection settles
            }
            // Movie-pack guard (deferred from acquire).
            if rec.request.kind == MediaKind::Movie && count_feature_videos(&info) > 1 {
                self.fail_and_reacquire(hash, &t.id, &rec.request, "MoviePack", &rec.provenance).await;
                continue;
            }
            // Choose the representative file to validate + probe. Movie: the feature video.
            // Series: the file matching the REQUESTED episode (validating a pack's largest file
            // against the requested (s,e) would misfire — a pack holds many episodes).
            let selected_path = match rec.request.kind {
                MediaKind::Movie => select_target(&info, None, None).map(|f| f.path.clone()),
                MediaKind::Series => episode_files(&info)
                    .into_iter()
                    .find(|(s, e, _)| Some(*s) == rec.request.season && Some(*e) == rec.request.episode)
                    .map(|(_, _, p)| p),
            };
            let Some(selected_path) = selected_path else {
                // The requested episode isn't present (or no video resolved). Past the dead-timeout,
                // treat it as a wrong/incomplete pick and re-acquire; otherwise wait (metadata may
                // still be settling).
                if now_secs().saturating_sub(rec.added_at) > self.dead_timeout.as_secs() {
                    self.fail_and_reacquire(hash, &t.id, &rec.request, "EpisodeMissing", &rec.provenance).await;
                }
                continue;
            };
            let file_name = selected_path.rsplit('/').next().unwrap_or(&selected_path).to_string();
            if !self.validator.validate(&file_name, rec.request.tmdb_id, rec.request.kind, rec.request.season, rec.request.episode).await {
                self.fail_and_reacquire(hash, &t.id, &rec.request, "WrongTitle", &rec.provenance).await;
                continue;
            }
            if t.status != "downloaded" {
                // Still downloading — stall check (uses dead_timeout as the no-progress ceiling).
                if self.is_stalled(&t.id, t.progress).await {
                    self.fail_and_reacquire(hash, &t.id, &rec.request, "Stalled", &rec.provenance).await;
                }
                continue;
            }
            // Downloaded → probe and finalise.
            let locator = locator_for(&info, hash, &selected_path);
            match self.verify_file(&locator, &rec.request).await {
                VerifyResult::Pass | VerifyResult::Accept => {
                    self.record_verified(hash, &rec.request, &info, &selected_path).await;
                    self.verify_attempts.lock().await.remove(hash);
                    self.progress.lock().await.remove(&t.id);
                }
                VerifyResult::Defer => {
                    let n = {
                        let mut m = self.verify_attempts.lock().await;
                        let n = m.entry(hash.to_string()).or_insert(0);
                        *n += 1;
                        *n
                    };
                    if n >= MAX_VERIFY_ATTEMPTS {
                        warn!("giving up verifying {} after {} deferred probes; accepting unverified", hash, n);
                        self.record_verified(hash, &rec.request, &info, &selected_path).await;
                    }
                }
                VerifyResult::Reject(reason) => {
                    self.verify_attempts.lock().await.remove(hash);
                    self.fail_and_reacquire(hash, &t.id, &rec.request, reason, &rec.provenance).await;
                }
            }
        }

        // Bound the in-memory maps to live torrents / owned hashes.
        let live_ids: std::collections::HashSet<&str> = torrents.iter().map(|t| t.id.as_str()).collect();
        self.progress.lock().await.retain(|tid, _| live_ids.contains(tid.as_str()));
        let owned_hashes: std::collections::HashSet<&str> = owned.iter().map(|(h, _)| h.as_str()).collect();
        self.verify_attempts.lock().await.retain(|h, _| owned_hashes.contains(h.as_str()));
    }
```

Update `fail_and_reacquire` to also delete the title's stale selection entries before re-acquiring, and to tolerate an empty `torrent_id` (the NeverResolved-not-listed case):

```rust
    async fn fail_and_reacquire(
        &self,
        hash: &str,
        torrent_id: &str,
        req: &AcquireRequest,
        reason: &str,
        provenance: &Provenance,
    ) {
        warn!("owned torrent {} failed ({}) — blacklist + re-acquire", hash, reason);
        let _ = self.store.blacklist_add(req.tmdb_id, hash.to_string(), reason, now_secs()).await;
        let _ = self.store.remove_owned(hash.to_string()).await;
        let _ = self.store.remove_authoritative(hash.to_string()).await;
        // Drop any selection slots this hash represented so the VFS stops showing the dead release.
        for (slot, entry) in self.store.all_selection().await {
            if entry.hash.eq_ignore_ascii_case(hash) {
                let _ = self.store.remove_selection(slot).await;
            }
        }
        if !torrent_id.is_empty() {
            let _ = self.provider.delete_torrent(torrent_id).await;
        }
        self.progress.lock().await.remove(torrent_id);
        self.verify_attempts.lock().await.remove(hash);
        let _ = self.acquire(req.clone(), provenance.clone()).await; // bad hash now blacklisted
    }
```

Delete `verify_pending` (its logic now lives inline in `observe`). Keep `is_stalled`, `verify_file`.

- [ ] **Step 4 (commit 2): Write observe resolver tests**

Add to `mod tests`. Use a short dead-timeout engine variant:

```rust
    fn engine_dead(provider: Arc<dyn DebridProvider>, scraper: Arc<dyn Scraper>, validator: Arc<dyn TitleValidator>, prober: Arc<dyn Prober>, store: Store, dead_secs: u64) -> AcquisitionEngine {
        AcquisitionEngine::new(provider, scraper, validator, prober, store, prefs(), 5, Duration::from_secs(1800), Duration::from_secs(dead_secs))
    }

    fn torrent(id: &str, hash: &str, status: &str, progress: f64) -> crate::rd_client::Torrent {
        crate::rd_client::Torrent { id: id.into(), hash: hash.into(), status: status.into(), progress, ..Default::default() }
    }

    #[tokio::test]
    async fn observe_verifies_pending_cached_and_writes_selection() {
        let st = store();
        st.put_owned("h1".into(), OwnedRecord {
            request: req(), provenance: Provenance::manual(), added_at: now_secs(),
            status: OwnedStatus::Pending, provides: vec![], quality: None,
        }).await.unwrap();
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![Track { kind: crate::probe::TrackKind::Audio, language: Some("eng".into()) }]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)]).await;
        assert_eq!(st.get_owned("h1".into()).await.unwrap().status, OwnedStatus::Verified);
        // movie selection slot written for tmdb 27205.
        assert_eq!(st.get_selection(crate::store::movie_slot(27205)).await.unwrap().hash, "h1");
    }

    #[tokio::test]
    async fn observe_wrong_title_blacklists_and_reacquires() {
        let st = store();
        st.put_owned("h1".into(), OwnedRecord {
            request: req(), provenance: Provenance::manual(), added_at: now_secs(),
            status: OwnedStatus::Pending, provides: vec![], quality: None,
        }).await.unwrap();
        // Scraper returns nothing, so re-acquire finds no replacement; the dead hash is blacklisted.
        let eng = engine(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(false)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloaded", 100.0)]).await;
        assert!(st.is_blacklisted(27205, "h1".into()).await);
        assert!(st.get_owned("h1".into()).await.is_none());
    }

    #[tokio::test]
    async fn observe_reaps_never_resolved_after_dead_timeout() {
        let st = store();
        // added_at far in the past; dead-timeout = 0 ⇒ immediately past it. Not in the listing.
        st.put_owned("h1".into(), OwnedRecord {
            request: req(), provenance: Provenance::manual(), added_at: 1,
            status: OwnedStatus::Pending, provides: vec![], quality: None,
        }).await.unwrap();
        let eng = engine_dead(
            provider_returning("downloaded", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            0,
        );
        eng.observe(&[]).await; // h1 absent from listing
        assert!(st.get_owned("h1".into()).await.is_none(), "never-resolved Pending is reaped");
        assert!(st.is_blacklisted(27205, "h1".into()).await);
    }

    #[tokio::test]
    async fn observe_leaves_downloading_with_recent_progress_pending() {
        let st = store();
        st.put_owned("h1".into(), OwnedRecord {
            request: req(), provenance: Provenance::manual(), added_at: now_secs(),
            status: OwnedStatus::Pending, provides: vec![], quality: None,
        }).await.unwrap();
        let eng = engine_dead(
            provider_returning("downloading", "h1"),
            Arc::new(MockScraper { candidates: vec![] }),
            Arc::new(OkValidator(true)),
            Arc::new(CannedProber(Ok(vec![]))),
            st.clone(),
            600,
        );
        eng.observe(&[torrent("tid_h1", "h1", "downloading", 12.0)]).await;
        assert_eq!(st.get_owned("h1".into()).await.unwrap().status, OwnedStatus::Pending, "slow-seed not judged early");
    }
```

(`provider_returning` returns a TI whose `id` is `tid_<hash>` and whose `files` has one selected `.mkv` — matching `tid_h1`. For the Series/provides path, add a dedicated provider in a separate test if you exercise packs; the movie path above is enough for this task.)

Run: `cargo test --lib acquire::` → PASS. Commit:

```bash
git add src/acquire.rs
git commit -m "feat(acquire): observe resolves Pending — deferred gates, dead-timeout, provides+selection, re-scrape"
```

---

### Task 7: Scan-loop wiring — build the selection map; removal clears selection

**Files:**
- Modify: `src/tasks.rs` (`update_vfs` loads + passes the selection map; `execute_remove` also deletes the title's selection slots + drops `provides` slots)
- Test: `src/tasks.rs` (`mod tests`)

- [ ] **Step 1: Build the selection map in the scan loop**

`update_vfs` needs the store to read selection. Add a `store: &Store` parameter to `update_vfs` and load the map:

Change `update_vfs` signature:

```rust
async fn update_vfs(
    vfs: &Arc<RwLock<DebridVfs>>,
    current_data: &[(crate::rd_client::TorrentInfo, MediaMetadata)],
    repair_manager: &Arc<RepairManager>,
    jellyfin_client: &Option<Arc<crate::jellyfin_client::JellyfinClient>>,
    store: &Store,
) {
    let hidden_ids = repair_manager.hidden_torrent_ids().await;
    let filtered: Vec<_> = current_data
        .iter()
        .filter(|(torrent_info, _)| !hidden_ids.contains(&torrent_info.id))
        .map(|(torrent_info, metadata)| (torrent_info.clone(), metadata.clone()))
        .collect();
    // SP3: resolve the live-selection map so build() shows the managed release per slot.
    let selection: crate::vfs::SelectionMap = store.all_selection().await.into_iter().collect();
    let new_vfs = DebridVfs::build(filtered, &selection);
    // ... rest unchanged (diff + swap + notify) ...
```

Update every `update_vfs(...)` call in `run_scan_loop` to pass `&store` (there are several: the pre-populate call, the shutdown-flush call, the progress-checkpoint call, and the else-branch call). Note `run_scan_loop` destructures `store` from `AppState` — it's in scope.

- [ ] **Step 2: Removal clears selection (write the failing test first)**

Add a test to `src/tasks.rs` `mod tests`:

```rust
    #[tokio::test]
    async fn execute_remove_drops_owned_and_selection() {
        use crate::store::{Store, OwnedRecord, OwnedStatus, Provenance, AcquireRequest, SelectionEntry, movie_slot};
        use crate::scraper::MediaKind;
        use crate::vfs::{MediaMetadata, MediaType};
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder().create_with_backend(redb::backends::InMemoryBackend::new()).unwrap(),
        )).unwrap();
        let req = AcquireRequest {
            imdb_id: "tt1".into(), tmdb_id: 27205, kind: MediaKind::Movie, season: None, episode: None,
            original_language: None,
            metadata: MediaMetadata { title: "M".into(), year: None, media_type: MediaType::Movie, external_id: Some("tmdb:27205".into()) },
        };
        store.put_owned("h1".into(), OwnedRecord { request: req, provenance: Provenance::watchlist("a"), added_at: 1, status: OwnedStatus::Verified, provides: vec![], quality: None }).await.unwrap();
        store.put_selection(movie_slot(27205), SelectionEntry { hash: "h1".into(), file_path: "m.mkv".into() }).await.unwrap();

        let provider: std::sync::Arc<dyn crate::provider::DebridProvider> = std::sync::Arc::new(crate::provider::MockProvider {
            torrents: vec![Torrent { id: "tid".into(), hash: "h1".into(), status: "downloaded".into(), ..Default::default() }],
            ..Default::default()
        });
        let torrents = provider.get_torrents().await.unwrap();
        execute_remove(&provider, &torrents, &store, 27205, &["h1".to_string()]).await;
        assert!(store.get_owned("h1".into()).await.is_none());
        assert!(store.get_selection(movie_slot(27205)).await.is_none(), "removal must clear the selection slot");
    }
```

- [ ] **Step 3: Implement selection cleanup in `execute_remove`**

In `execute_remove`, after `store.remove_owned(hash.clone())` succeeds, delete any selection slot pointing at this hash. Because selection is keyed by slot (not hash), scan `all_selection` for entries whose `hash` matches and remove them:

```rust
        if let Err(e) = store.remove_owned(hash.clone()).await {
            warn!("reconcile: remove_owned {} (tmdb {}) failed: {}", hash, tmdb_id, e);
        }
        // SP3: drop any selection slots this hash represented.
        for (slot, entry) in store.all_selection().await {
            if entry.hash.eq_ignore_ascii_case(hash) {
                let _ = store.remove_selection(slot).await;
            }
        }
```

- [ ] **Step 4: Run and commit**

Run: `cargo test --lib tasks::` then `cargo test`.
Expected: PASS.

```bash
git add src/tasks.rs
git commit -m "feat(tasks): build live-selection map for VFS; clear selection on removal"
```

---

### Task 8: Churn fix — `group_owned_by_tmdb` aggregates `provides`

**Files:**
- Modify: `src/tasks.rs` (`group_owned_by_tmdb` uses `rec.provides` for `owned_episodes`)
- Test: `src/tasks.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn group_owned_uses_provides_for_episode_set() {
        use crate::store::{Store, OwnedRecord, OwnedStatus, Provenance, AcquireRequest};
        use crate::scraper::MediaKind;
        use crate::vfs::{MediaMetadata, MediaType};
        let store = Store::from_database(std::sync::Arc::new(
            redb::Database::builder().create_with_backend(redb::backends::InMemoryBackend::new()).unwrap(),
        )).unwrap();
        // A single season-pack hash acquired via an S01E01 request, but `provides` records the WHOLE
        // season — the churn fix: the group's owned_episodes must reflect every provided episode.
        let req = AcquireRequest {
            imdb_id: "tt2".into(), tmdb_id: 1396, kind: MediaKind::Series, season: Some(1), episode: Some(1),
            original_language: None,
            metadata: MediaMetadata { title: "S".into(), year: None, media_type: MediaType::Show, external_id: Some("tmdb:1396".into()) },
        };
        store.put_owned("pack".into(), OwnedRecord {
            request: req, provenance: Provenance::watchlist("a"), added_at: 1, status: OwnedStatus::Verified,
            provides: vec![(1, 1), (1, 2), (1, 3)], quality: None,
        }).await.unwrap();

        let groups = group_owned_by_tmdb(&store).await;
        let g = groups.get(&1396).unwrap();
        let mut eps = g.owned_episodes.clone();
        eps.sort_unstable();
        assert_eq!(eps, vec![(1, 1), (1, 2), (1, 3)], "owned_episodes is the union of provides, not the request's single (s,e)");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib tasks::tests::group_owned_uses_provides_for_episode_set`
Expected: FAIL — current code pushes only `rec.request.(season,episode)`, yielding `[(1,1)]`.

- [ ] **Step 3: Implement**

In `group_owned_by_tmdb`, replace the per-record episode aggregation:

```rust
        group.hashes.push(hash.to_ascii_lowercase());
        group.provenance.merge(&rec.provenance);
        // SP3: prefer the recorded `provides` (a pack supplies many episodes); fall back to the
        // request's single (season, episode) for pre-SP3 records that have no `provides` yet.
        if rec.provides.is_empty() {
            if let (Some(s), Some(e)) = (rec.request.season, rec.request.episode) {
                group.owned_episodes.push((s, e));
            }
        } else {
            group.owned_episodes.extend(rec.provides.iter().copied());
        }
```

Dedup `owned_episodes` at the end of `group_owned_by_tmdb` (after the loop, before `owned_by`):

```rust
    for g in owned_by.values_mut() {
        g.owned_episodes.sort_unstable();
        g.owned_episodes.dedup();
    }
```

> **Note (intentional omission):** the spec mentions a "lazy `provides` backfill" in `observe` for pre-SP3 owned packs whose `provides` is empty. It is **not** implemented: per decision 10, no deployed DB carries SP1/SP2 owned records, and every new acquire sets `provides` at verify time. The fallback above (use the request's single `(s,e)` when `provides` is empty) keeps any local-dev pre-SP3 record correct-enough until it is next re-acquired. If a real backfill is ever needed, add it where `observe` meets a Verified show torrent with empty `provides`.

- [ ] **Step 4: Run and commit**

Run: `cargo test --lib tasks::` then `cargo test`.
Expected: PASS.

```bash
git add src/tasks.rs
git commit -m "fix(tasks): owned_episodes = union of provides (kills season-pack re-acquire churn)"
```

---

## Phase C — upgrade engine

### Task 9: `upgrade.rs` — quality upgrades (stage → idle-gated swap → prune)

**Files:**
- Create: `src/upgrade.rs`
- Modify: `src/mapper.rs` (`pub mod upgrade;`)
- Test: `src/upgrade.rs` (`#[cfg(test)] mod tests`)

The job operates on `AppState`. It re-scores owned MOVIE titles (shows are extended in Task 10), stages a cached meaningful upgrade, and — when the slot is idle — repoints the `selection` and prunes the old torrent.

- [ ] **Step 1: Define the module skeleton + the pure idle/decision helpers (write tests first)**

Create `src/upgrade.rs`:

```rust
//! SP3 upgrade engine. A slow periodic job (gated on `UPGRADE_INTERVAL_SECS`, default daily) that
//! re-scores owned titles and stages meaningfully-better CACHED releases — and full-season cached
//! packs (Task 10) — swapping the persisted `selection` and pruning the superseded torrent only
//! once the slot is idle (proxy read-activity). Upgrades are non-destructive: a failed stage never
//! degrades the working release.

use crate::app_state::AppState;
use crate::release::{self, QualitySummary};
use crate::scraper::MediaKind;
use crate::store::{movie_slot, OwnedRecord, OwnedStatus};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
```

- [ ] **Step 2: Write the failing test for the budgeted owned-movie scan + meaningful staging**

This is an integration-style test over `AppState` with mocks. Because building an `AppState` is verbose, reuse the pattern from `scheduler.rs::make_test_app` but parameterise the scraper/provider. Add to `src/upgrade.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::config::{AcquisitionConfig, Config};
    use crate::provider::{DebridProvider, MockProvider, ProviderKind};
    use crate::rd_client::{AddMagnetResponse, Torrent, TorrentFile, TorrentInfo};
    use crate::repair::RepairManager;
    use crate::scraper::{MockScraper, Scraper};
    use crate::store::{AcquireRequest, Provenance, SelectionEntry, Store};
    use crate::release::RawCandidate;
    use crate::tmdb_client::TmdbClient;
    use crate::vfs::{DebridVfs, MediaMetadata, MediaType};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn mem_store() -> Store {
        Store::from_database(Arc::new(redb::Database::builder().create_with_backend(redb::backends::InMemoryBackend::new()).unwrap())).unwrap()
    }

    fn movie_meta() -> MediaMetadata {
        MediaMetadata { title: "M".into(), year: Some("2020".into()), media_type: MediaType::Movie, external_id: Some("tmdb:27205".into()) }
    }
    fn movie_req() -> AcquireRequest {
        AcquireRequest { imdb_id: "tt1".into(), tmdb_id: 27205, kind: MediaKind::Movie, season: None, episode: None, original_language: Some("eng".into()), metadata: movie_meta() }
    }
    /// A cached REMUX 1080p candidate (a meaningful upgrade over a cached WEB 1080p).
    fn remux_candidate() -> RawCandidate {
        RawCandidate { name: "Torrentio\n1080p".into(), description: "M.2020.1080p.BluRay.REMUX.x265\nRD+".into(), info_hash: "hnew".into(), file_idx: Some(0), file_name: Some("M.2020.1080p.REMUX.mkv".into()) }
    }

    fn app_with(scraper: Arc<dyn Scraper>, provider: Arc<dyn DebridProvider>, store: Store) -> AppState {
        let mut config = Config::from_parts(None, Some("tb".into()), Some("k".into()), None, None, None).unwrap();
        config.acquisition = AcquisitionConfig::default();
        let tmdb = Arc::new(TmdbClient::new("k".into()).unwrap());
        let validator: Arc<dyn crate::acquire::TitleValidator> = Arc::new(crate::acquire::TmdbTitleValidator { tmdb: tmdb.clone() });
        let prober: Arc<dyn crate::acquire::Prober> = Arc::new(crate::acquire::HttpProber { http: reqwest::Client::new() });
        let engine = Arc::new(crate::acquire::AcquisitionEngine::new(
            provider.clone(), scraper.clone(), validator, prober, store.clone(),
            config.acquisition.prefs.clone(), 5, Duration::from_secs(1800), Duration::from_secs(600),
        ));
        AppState {
            provider: provider.clone(),
            tmdb_client: tmdb,
            vfs: Arc::new(RwLock::new(DebridVfs::new())),
            store,
            repair_manager: Arc::new(RepairManager::new(provider)),
            config: Arc::new(config),
            jellyfin_client: None,
            http_client: reqwest::Client::new(),
            scraper,
            engine,
            trakt_client: None,
            read_activity: Arc::new(crate::read_activity::ReadActivity::new()),
        }
    }

    #[tokio::test]
    async fn idle_movie_with_cached_better_release_is_staged_swapped_and_old_pruned() {
        let store = mem_store();
        // Owned: cached WEB 1080p movie, Verified, with a movie selection pointing at it.
        store.put_owned("hold".into(), OwnedRecord {
            request: movie_req(), provenance: Provenance::watchlist("a"), added_at: 1, status: OwnedStatus::Verified,
            provides: vec![],
            quality: Some(QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 10 }),
        }).await.unwrap();
        store.put_selection(movie_slot(27205), SelectionEntry { hash: "hold".into(), file_path: "old.mkv".into() }).await.unwrap();

        // Scraper offers a cached REMUX (higher tier → meaningful upgrade).
        let scraper = Arc::new(MockScraper { candidates: vec![remux_candidate()] });
        // Provider: both torrents listed; the new one resolves cached/downloaded with a video file.
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![
                Torrent { id: "told".into(), hash: "hold".into(), status: "downloaded".into(), ..Default::default() },
                Torrent { id: "tnew".into(), hash: "hnew".into(), status: "downloaded".into(), ..Default::default() },
            ],
            add_magnet: Some(AddMagnetResponse { id: "tnew".into(), uri: String::new() }),
            torrent_info: Some(TorrentInfo {
                id: "tnew".into(), hash: "hnew".into(), status: "downloaded".into(),
                files: vec![TorrentFile { id: 0, path: "M.2020.1080p.REMUX.mkv".into(), bytes: 30_000_000_000, selected: 1 }],
                links: vec!["https://cdn/new".into()],
                ..Default::default()
            }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());

        // The movie slot has never been read ⇒ idle ⇒ swap + prune proceed.
        run_upgrade_once(&app).await;

        let sel = store.get_selection(movie_slot(27205)).await.unwrap();
        assert_eq!(sel.hash, "hnew", "selection swapped to the upgraded release");
        assert!(store.get_owned("hnew".into()).await.is_some(), "new release recorded owned+verified");
        assert!(store.get_owned("hold".into()).await.is_none(), "old release pruned from owned");
        assert!(deleted.lock().unwrap().contains(&"told".to_string()), "old torrent deleted from provider");
    }

    #[tokio::test]
    async fn active_movie_is_not_pruned() {
        let store = mem_store();
        store.put_owned("hold".into(), OwnedRecord {
            request: movie_req(), provenance: Provenance::watchlist("a"), added_at: 1, status: OwnedStatus::Verified,
            provides: vec![], quality: Some(QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 10 }),
        }).await.unwrap();
        store.put_selection(movie_slot(27205), SelectionEntry { hash: "hold".into(), file_path: "old.mkv".into() }).await.unwrap();
        let scraper = Arc::new(MockScraper { candidates: vec![remux_candidate()] });
        let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            torrents: vec![Torrent { id: "told".into(), hash: "hold".into(), status: "downloaded".into(), ..Default::default() }],
            add_magnet: Some(AddMagnetResponse { id: "tnew".into(), uri: String::new() }),
            torrent_info: Some(TorrentInfo { id: "tnew".into(), hash: "hnew".into(), status: "downloaded".into(), files: vec![TorrentFile { id: 0, path: "new.mkv".into(), bytes: 30_000_000_000, selected: 1 }], links: vec!["https://cdn/new".into()], ..Default::default() }),
            resolved_url: Some("https://cdn/new".into()),
            deleted: deleted.clone(),
            ..Default::default()
        });
        let app = app_with(scraper, provider, store.clone());
        // The idle gate is library-wide: a recent read anywhere defers all swaps this tick.
        app.read_activity.touch("Movies/anything.mkv").await;

        run_upgrade_once(&app).await;

        // Nothing pruned, nothing staged, selection unchanged — the upgrade was deferred.
        assert!(deleted.lock().unwrap().is_empty(), "active library must not be pruned");
        assert!(store.get_owned("hnew".into()).await.is_none(), "no stage while active");
        assert_eq!(store.get_selection(movie_slot(27205)).await.unwrap().hash, "hold", "selection unchanged");
    }
```

(Note: the active-path test depends on how the job derives the idle key. Decide that contract in Step 3 and make the test touch exactly that key. If deriving the precise VFS path is awkward in the job, gate idle on a key the job can compute deterministically from the slot — see Step 3.)

- [ ] **Step 3: Implement `run_upgrade_once` (one tick) and the idle contract**

**Idle key contract:** the read-activity tracker is keyed by VFS path, but the upgrade job works from tmdb slots and doesn't cheaply know the rendered VFS path. To keep it deterministic and testable, gate idle on the **selection slot key** instead of the VFS path, and have `dav_fs` stamp BOTH the VFS path (for any future per-path needs) and — no. Simpler and consistent: keep `dav_fs` stamping the VFS path, and in the upgrade job, look up the owned title's current torrent_id and check whether ANY currently-open read maps to it. Since the tracker is path-keyed and the job doesn't have paths, adopt this concrete rule:

- `ReadActivity` additionally exposes `fn most_recent(&self) -> Option<Instant>` and the job treats the WHOLE library as the idle unit per tick: if ANY read happened within `idle_secs`, defer ALL swaps this tick. This is conservative (a single active stream defers all upgrades) but simple, race-free, and never prunes under load.

Add to `src/read_activity.rs`:

```rust
    /// The most recent read across all paths, if any.
    pub async fn most_recent(&self) -> Option<Instant> {
        self.last_read.read().await.values().copied().max()
    }

    /// `true` if NO path has been read within `window`.
    pub async fn all_idle(&self, window: Duration) -> bool {
        match self.most_recent().await {
            Some(t) => t.elapsed() >= window,
            None => true,
        }
    }
```

(Add a quick unit test for `all_idle` mirroring `is_idle`.)

Now implement the job in `src/upgrade.rs`:

```rust
/// Run one upgrade tick over `app`: re-score a budgeted batch of owned MOVIE titles, stage any
/// cached meaningful upgrade, and — if the library is idle — swap selection + prune the old torrent.
pub async fn run_upgrade_once(app: &AppState) {
    let budget = app.config.upgrade.budget_per_tick as usize;
    let idle_window = Duration::from_secs(app.config.upgrade.idle_secs);

    // Group owned by tmdb_id (reuse the tasks helper) and pick the least-recently-checked movies.
    let groups = crate::tasks::group_owned_by_tmdb(&app.store).await;
    let mut candidates: Vec<(u64, Vec<String>, OwnedRecord)> = Vec::new();
    for (tmdb_id, g) in &groups {
        if g.media_type != MediaType::Movie {
            continue; // Task 10 handles shows/consolidation
        }
        // Representative owned record (movies have one hash).
        let Some(hash) = g.hashes.first().cloned() else { continue };
        let Some(rec) = app.store.get_owned(hash.clone()).await else { continue };
        if rec.status != OwnedStatus::Verified {
            continue; // only upgrade settled titles
        }
        candidates.push((*tmdb_id, g.hashes.clone(), rec));
    }
    // Least-recently-checked first.
    let mut ordered: Vec<_> = Vec::new();
    for (id, hashes, rec) in candidates {
        let last = app.store.get_upgrade_checked(id).await;
        ordered.push((last, id, hashes, rec));
    }
    ordered.sort_by_key(|(last, ..)| *last);
    ordered.truncate(budget);

    for (_, tmdb_id, hashes, rec) in ordered {
        app.store.set_upgrade_checked(tmdb_id, now_secs()).await.ok();
        if let Err(e) = try_upgrade_movie(app, tmdb_id, &hashes, &rec, idle_window).await {
            warn!("upgrade: tmdb {} skipped: {}", tmdb_id, e);
        }
    }
}

/// Stage + (idle-gated) swap a single movie title. Returns Err(reason) on a non-fatal skip.
async fn try_upgrade_movie(
    app: &AppState,
    tmdb_id: u64,
    owned_hashes: &[String],
    owned_rec: &OwnedRecord,
    idle_window: Duration,
) -> Result<(), String> {
    // 1. Scrape fresh candidates for this title.
    let raws = app.scraper
        .find(&owned_rec.request.imdb_id, MediaKind::Movie, None, None)
        .await
        .map_err(|e| format!("scrape failed: {e}"))?;
    let current = owned_rec.quality.clone().unwrap_or_default();
    // 2. Best cached meaningful upgrade not already owned/blacklisted.
    let mut best: Option<(release::ReleaseInfo, QualitySummary)> = None;
    for raw in &raws {
        let r = release::parse(raw);
        if owned_hashes.iter().any(|h| h.eq_ignore_ascii_case(&r.info_hash)) { continue; }
        if app.store.is_blacklisted(tmdb_id, r.info_hash.clone()).await { continue; }
        let q = QualitySummary::of(&r, &app.config.acquisition.prefs);
        if !release::is_meaningful_upgrade(&current, &q) { continue; }
        if best.as_ref().map(|(_, bq)| q.score > bq.score).unwrap_or(true) {
            best = Some((r, q));
        }
    }
    let Some((cand, _q)) = best else { return Err("no meaningful upgrade".into()) };

    // 3. Idle gate FIRST. Upgrade targets are cached-only (instant to add), so there is no benefit
    //    to pre-staging a download — we only commit when the library is idle, and skip otherwise.
    //    This guarantees we never hold two copies of a title (no dangling stage), which is why
    //    `UPGRADE_STAGE_MAX_SECS` is config-only/reserved on this cached path (kept for forward-compat
    //    with a future speculative-download upgrade mode; not consulted here).
    if !app.read_activity.all_idle(idle_window).await {
        return Err("library active; deferring upgrade".into());
    }

    // 4. Stage the cached candidate: add + validate + record Verified (non-destructive — any failure
    //    leaves the current release untouched). Returns (hash, torrent_id, selected_file_path).
    let staged = stage_and_verify(app, tmdb_id, &owned_rec.request, &cand).await?;

    // 5. Swap selection → new hash, then prune every old owned hash.
    app.store.put_selection(
        crate::store::movie_slot(tmdb_id),
        crate::store::SelectionEntry { hash: staged.0.clone(), file_path: staged.2.clone() },
    ).await.ok();
    for old in owned_hashes {
        if old.eq_ignore_ascii_case(&staged.0) { continue; }
        prune_owned_hash(app, old).await;
    }
    info!("upgrade: tmdb {} swapped to {}", tmdb_id, staged.0);
    Ok(())
}
```

Add `stage_and_verify` and `prune_owned_hash`:

```rust
/// Add the candidate, wait briefly for it to resolve (it should be cached), validate + record it
/// Verified with sticky provenance, and return (hash, torrent_id, selected_file_path). On any
/// failure the candidate is cleaned up and the current release is left untouched (non-destructive).
async fn stage_and_verify(
    app: &AppState,
    tmdb_id: u64,
    base_req: &crate::store::AcquireRequest,
    cand: &release::ReleaseInfo,
) -> Result<(String, String, String), String> {
    let hash = cand.info_hash.clone();
    let magnet = format!("magnet:?xt=urn:btih:{}", hash);
    let added = app.provider.add_magnet(&magnet).await.map_err(|e| format!("add failed: {e}"))?;
    let info = app.provider.get_torrent_info(&added.id).await.map_err(|e| format!("info failed: {e}"))?;
    // Must be cached/downloaded with a video file to stage (we never speculatively download upgrades).
    if info.status != "downloaded" {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err("candidate not cached".into());
    }
    // Select the single feature file.
    let Some(file) = info.files.iter().filter(|f| crate::vfs::is_video_file(&f.path)).max_by_key(|f| f.bytes) else {
        let _ = app.provider.delete_torrent(&added.id).await;
        return Err("no video file".into());
    };
    let csv = file.id.to_string();
    let _ = app.provider.select_files(&added.id, &csv).await;
    let selected_path = file.path.clone();
    let file_name = selected_path.rsplit('/').next().unwrap_or(&selected_path).to_string();
    // Title validation (the engine exposes `validate_title` — see below).
    if !app.engine.validate_title(&file_name, tmdb_id, MediaKind::Movie, None, None).await {
        let _ = app.provider.delete_torrent(&added.id).await;
        let _ = app.store.blacklist_add(tmdb_id, hash.clone(), "WrongTitle", now_secs()).await;
        return Err("title mismatch".into());
    }
    // Record Verified with sticky provenance from the owned record we are upgrading.
    let prov = base_req_provenance(app, tmdb_id).await;
    let _ = app.store.put_owned(hash.clone(), OwnedRecord {
        request: base_req.clone(),
        provenance: prov,
        added_at: now_secs(),
        status: OwnedStatus::Verified,
        provides: vec![],
        quality: Some(QualitySummary::of(cand, &app.config.acquisition.prefs)),
    }).await;
    let _ = app.store.put_authoritative(hash.clone(), base_req.metadata.clone()).await;
    Ok((hash, added.id, selected_path))
}

/// Delete a torrent (owned-only) and drop its owned record + authoritative id.
async fn prune_owned_hash(app: &AppState, hash: &str) {
    if let Ok(torrents) = app.provider.get_torrents().await {
        for t in torrents.iter().filter(|t| t.hash.eq_ignore_ascii_case(hash)) {
            let _ = app.provider.delete_torrent(&t.id).await;
        }
    }
    let _ = app.store.remove_owned(hash.to_string()).await;
    let _ = app.store.remove_authoritative(hash.to_string()).await;
}

/// The provenance to keep on a staged upgrade: the merged provenance of the title's current owned
/// hashes (sticky — preserves Manual / per-user origins across the swap).
async fn base_req_provenance(app: &AppState, tmdb_id: u64) -> crate::store::Provenance {
    let groups = crate::tasks::group_owned_by_tmdb(&app.store).await;
    groups.get(&tmdb_id).map(|g| g.provenance.clone()).unwrap_or_else(crate::store::Provenance::manual)
}
```

Title validation lives on the engine's private `validator`, so add a small public method to `AcquisitionEngine` (in `src/acquire.rs`) to expose it for the upgrade + consolidation paths used above:

```rust
    /// Validate a file name against an expected title (used by the upgrade engine before staging).
    pub async fn validate_title(&self, file_name: &str, tmdb_id: u64, kind: MediaKind, season: Option<u32>, episode: Option<u32>) -> bool {
        self.validator.validate(file_name, tmdb_id, kind, season, episode).await
    }
```

Declare the module in `src/mapper.rs`: `pub mod upgrade;`.

- [ ] **Step 4: Run and commit**

Run: `cargo test --lib upgrade::` then `cargo test`.
Expected: PASS. (Adjust the `active_movie_is_not_pruned` test to touch a path so `all_idle` returns false — e.g. `app.read_activity.touch("anything").await;` before `run_upgrade_once`, since `all_idle` is library-wide.)

```bash
git add src/upgrade.rs src/mapper.rs src/acquire.rs src/read_activity.rs
git commit -m "feat(upgrade): daily quality upgrade — stage cached better release, idle-gated swap + prune"
```

---

### Task 10: Consolidation — full-season cached pack replaces scattered episodes

**Files:**
- Modify: `src/upgrade.rs` (handle `MediaType::Show` in `run_upgrade_once`; add `try_consolidate_show` + full-season detection)
- Modify: `src/tasks.rs` (expose `aired_episodes` as `pub(crate)` so upgrade can ask "what is the full aired season?")
- Test: `src/upgrade.rs` (`mod tests`)

- [ ] **Step 1: Expose the aired-season helper**

In `src/tasks.rs`, change `async fn aired_episodes(...)` to `pub(crate) async fn aired_episodes(...)` (it already returns `Vec<(u32,u32)>`). Also add a pure helper for the full-season-of-a-season filter:

```rust
/// Filter aired pairs to one season's episode numbers (sorted).
pub(crate) fn season_aired(aired: &[(u32, u32)], season: u32) -> Vec<u32> {
    let mut v: Vec<u32> = aired.iter().filter(|(s, _)| *s == season).map(|(_, e)| *e).collect();
    v.sort_unstable();
    v.dedup();
    v
}
```

- [ ] **Step 2: Write the failing consolidation test**

Add to `src/upgrade.rs` `mod tests`. The setup: a show owned as TWO scattered per-episode torrents (S01E01, S01E02), each its own hash with single `provides`. The scraper offers a CACHED season pack containing S01E01–E03. TMDB says season 1 has aired E01–E03. Idle ⇒ consolidate: selection for E01/E02/E03 → pack hash, both old episode torrents pruned.

Because `aired_episodes` calls TMDB (network), make the test inject the aired set via a seam: add a parameter to `run_upgrade_once`'s show path that takes an aired-resolver closure, OR — simpler for the unit test — factor the consolidation decision into a PURE function and test that directly:

```rust
    use super::{consolidation_target, ConsolidationInput};

    #[test]
    fn full_cached_season_pack_no_regression_consolidates() {
        // Owned scattered: E01 (WEB 1080p), E02 (WEB 1080p). Pack: cached BluRay 1080p covering E01-E03.
        let input = ConsolidationInput {
            season: 1,
            aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![
                (1, crate::release::QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 1 }),
                (2, crate::release::QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 1 }),
            ],
            pack_cached: true,
            pack_episodes: vec![1, 2, 3],
            pack_quality: crate::release::QualitySummary { cached: true, source_tier: 6_000, resolution: 1080, score: 5 },
        };
        assert!(consolidation_target(&input), "cached full-season pack, no regression → consolidate");
    }

    #[test]
    fn partial_season_pack_is_rejected() {
        let input = ConsolidationInput {
            season: 1, aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![(1, q1080_web()), (2, q1080_web())],
            pack_cached: true, pack_episodes: vec![1, 2], // missing E03
            pack_quality: q1080_bluray(),
        };
        assert!(!consolidation_target(&input), "partial-season pack must not consolidate");
    }

    #[test]
    fn quality_regression_pack_is_rejected() {
        let input = ConsolidationInput {
            season: 1, aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![(1, q2160_remux()), (2, q1080_web())], // E01 is 2160 REMUX
            pack_cached: true, pack_episodes: vec![1, 2, 3],
            pack_quality: q1080_bluray(), // worse than E01 → regression
        };
        assert!(!consolidation_target(&input), "a pack worse than any owned episode must not consolidate");
    }

    #[test]
    fn uncached_pack_is_rejected() {
        let input = ConsolidationInput {
            season: 1, aired_episodes: vec![1, 2, 3],
            owned_episode_quality: vec![(1, q1080_web())],
            pack_cached: false, pack_episodes: vec![1, 2, 3], pack_quality: q1080_bluray(),
        };
        assert!(!consolidation_target(&input));
    }

    fn q1080_web() -> crate::release::QualitySummary { crate::release::QualitySummary { cached: true, source_tier: 3_000, resolution: 1080, score: 1 } }
    fn q1080_bluray() -> crate::release::QualitySummary { crate::release::QualitySummary { cached: true, source_tier: 6_000, resolution: 1080, score: 2 } }
    fn q2160_remux() -> crate::release::QualitySummary { crate::release::QualitySummary { cached: true, source_tier: 8_000, resolution: 2160, score: 9 } }
```

- [ ] **Step 3: Implement the pure decision + the show consolidation path**

Add to `src/upgrade.rs`:

```rust
/// Inputs to the pure consolidation decision for ONE season.
#[derive(Debug, Clone)]
pub struct ConsolidationInput {
    pub season: u32,
    /// Episodes of this season aired per TMDB (the "full season" target).
    pub aired_episodes: Vec<u32>,
    /// (episode, quality) for each episode we currently own INDIVIDUALLY in this season.
    pub owned_episode_quality: Vec<(u32, QualitySummary)>,
    pub pack_cached: bool,
    /// Episodes the candidate pack supplies for this season.
    pub pack_episodes: Vec<u32>,
    /// Quality of the pack (per-episode quality is assumed uniform across the pack).
    pub pack_quality: QualitySummary,
}

/// Pure: should we consolidate this season's scattered episodes into the candidate pack?
/// Requires: the pack is CACHED; it is a FULL-season pack (covers every aired episode); and it is
/// not a quality regression vs ANY episode we currently own (same-or-higher tier AND resolution).
pub fn consolidation_target(i: &ConsolidationInput) -> bool {
    if !i.pack_cached {
        return false;
    }
    // Full season: every aired episode must be in the pack.
    let covers_full_season = i.aired_episodes.iter().all(|e| i.pack_episodes.contains(e));
    if !covers_full_season || i.aired_episodes.is_empty() {
        return false;
    }
    // No regression vs any owned episode.
    for (_, owned_q) in &i.owned_episode_quality {
        let no_regression = i.pack_quality.source_tier >= owned_q.source_tier
            && i.pack_quality.resolution >= owned_q.resolution;
        if !no_regression {
            return false;
        }
    }
    true
}
```

Then the show path in `run_upgrade_once`: for each owned SHOW group, for each season represented by scattered single-`provides` hashes, scrape the show, find season packs, build `ConsolidationInput` (aired from `crate::tasks::aired_episodes` + `season_aired`; pack episodes from the pack candidate's SE-mapped files after a stage; owned per-episode quality from each episode-hash's `OwnedRecord.quality`), and if `consolidation_target` is true AND idle → stage the pack, write episode selection slots for the whole season → pack, and prune the scattered episode torrents.

Because staging a pack requires materialising it to read its files (to confirm `pack_episodes`), structure it as: detect a likely full-season pack from the scrape (Torrentio season packs are tagged with the season and usually list the episode count), stage it (add + get info + SE-map files = `episode_files`), THEN apply `consolidation_target` with the resolved `pack_episodes`. If it fails any gate, prune the staged pack (non-destructive). On success + idle, write per-episode selection for every `(season, e)` in the pack and prune the old per-episode torrents.

Add `try_consolidate_show(app, tmdb_id, group, idle_window)` mirroring `try_upgrade_movie`'s structure; show the full implementation:

```rust
async fn try_consolidate_show(
    app: &AppState,
    tmdb_id: u64,
    group_hashes: &[String],
    idle_window: Duration,
) -> Result<(), String> {
    // Owned per-episode records for this show: hash -> (provides, quality).
    let mut owned: Vec<(String, OwnedRecord)> = Vec::new();
    for h in group_hashes {
        if let Some(r) = app.store.get_owned(h.clone()).await {
            owned.push((h.clone(), r));
        }
    }
    // A representative request (for imdb id + metadata) — any owned record works.
    let Some((_, sample)) = owned.first().cloned() else { return Err("no owned records".into()) };
    let today = chrono::Utc::now().date_naive();
    let aired = crate::tasks::aired_episodes(&app.tmdb_client, tmdb_id, today).await;

    // Seasons currently held as SCATTERED single-episode torrents (provides.len()==1).
    let mut seasons: Vec<u32> = owned.iter()
        .filter(|(_, r)| r.provides.len() == 1)
        .map(|(_, r)| r.provides[0].0)
        .collect();
    seasons.sort_unstable();
    seasons.dedup();

    for season in seasons {
        let season_aired = crate::tasks::season_aired(&aired, season);
        if season_aired.is_empty() { continue; }
        // Already a full-season pack owned for this season? (any hash whose provides covers it)
        let already_pack = owned.iter().any(|(_, r)| {
            let eps: Vec<u32> = r.provides.iter().filter(|(s, _)| *s == season).map(|(_, e)| *e).collect();
            season_aired.iter().all(|e| eps.contains(e)) && r.provides.len() > 1
        });
        if already_pack { continue; }

        // Scrape the season (episode 1 query returns season packs too).
        let raws = match app.scraper.find(&sample.request.imdb_id, MediaKind::Series, Some(season), Some(1)).await {
            Ok(r) => r,
            Err(e) => { warn!("consolidate: scrape s{} failed: {}", season, e); continue; }
        };
        // Try cached candidates that look like packs (file_name absent or multiple videos after stage).
        for raw in &raws {
            let r = release::parse(raw);
            if !r.cached { continue; }
            if app.store.is_blacklisted(tmdb_id, r.info_hash.clone()).await { continue; }
            if group_hashes.iter().any(|h| h.eq_ignore_ascii_case(&r.info_hash)) { continue; }

            // Stage: add + resolve + SE-map files.
            let magnet = format!("magnet:?xt=urn:btih:{}", r.info_hash);
            let Ok(added) = app.provider.add_magnet(&magnet).await else { continue };
            let Ok(info) = app.provider.get_torrent_info(&added.id).await else {
                let _ = app.provider.delete_torrent(&added.id).await; continue;
            };
            if info.status != "downloaded" { let _ = app.provider.delete_torrent(&added.id).await; continue; }
            // Select all videos so the pack is fully available.
            let ids: Vec<u32> = info.files.iter().filter(|f| crate::vfs::is_video_file(&f.path)).map(|f| f.id).collect();
            if !ids.is_empty() {
                let _ = app.provider.select_files(&added.id, &ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")).await;
            }
            let fresh = app.provider.get_torrent_info(&added.id).await.unwrap_or(info);
            let eps = crate::acquire::episode_files(&fresh); // (s,e,path) — pub(crate) from Task 6
            let pack_episodes: Vec<u32> = eps.iter().filter(|(s, _, _)| *s == season).map(|(_, e, _)| *e).collect();

            // Owned per-episode quality for this season.
            let owned_episode_quality: Vec<(u32, QualitySummary)> = owned.iter()
                .filter(|(_, rec)| rec.provides.len() == 1 && rec.provides[0].0 == season)
                .map(|(_, rec)| (rec.provides[0].1, rec.quality.clone().unwrap_or_default()))
                .collect();

            let input = ConsolidationInput {
                season,
                aired_episodes: season_aired.clone(),
                owned_episode_quality,
                pack_cached: true,
                pack_episodes: { let mut v = pack_episodes.clone(); v.sort_unstable(); v.dedup(); v },
                pack_quality: QualitySummary::of(&r, &app.config.acquisition.prefs),
            };
            if !consolidation_target(&input) {
                let _ = app.provider.delete_torrent(&added.id).await; // non-destructive: drop the staged pack
                continue;
            }
            // Validate the show identity on a representative episode file.
            if let Some((es, ee, path)) = eps.iter().find(|(s, _, _)| *s == season) {
                let fname = path.rsplit('/').next().unwrap_or(path).to_string();
                if !app.engine.validate_title(&fname, tmdb_id, MediaKind::Series, Some(*es), Some(*ee)).await {
                    let _ = app.provider.delete_torrent(&added.id).await;
                    let _ = app.store.blacklist_add(tmdb_id, r.info_hash.clone(), "WrongTitle", now_secs()).await;
                    continue;
                }
            }
            // Idle gate. If the library is active, drop the staged pack (no dangling stage) and
            // retry on a later tick; consolidation re-stages cheaply (the pack is cached).
            if !app.read_activity.all_idle(idle_window).await {
                info!("consolidate: tmdb {} s{} deferred (library active); dropping staged pack", tmdb_id, season);
                let _ = app.provider.delete_torrent(&added.id).await;
                return Ok(());
            }
            // Record the pack owned+verified with full-season provides + sticky provenance.
            let prov = base_req_provenance(app, tmdb_id).await;
            let provides: Vec<(u32, u32)> = eps.iter().map(|(s, e, _)| (*s, *e)).collect();
            let _ = app.store.put_owned(r.info_hash.clone(), OwnedRecord {
                request: crate::store::AcquireRequest { season: Some(season), episode: Some(1), ..sample.request.clone() },
                provenance: prov,
                added_at: now_secs(),
                status: OwnedStatus::Verified,
                provides: provides.clone(),
                quality: Some(QualitySummary::of(&r, &app.config.acquisition.prefs)),
            }).await;
            let _ = app.store.put_authoritative(r.info_hash.clone(), sample.request.metadata.clone()).await;
            // Repoint every episode slot of this season to the pack.
            for (s, e, path) in &eps {
                if *s != season { continue; }
                let _ = app.store.put_selection(
                    crate::store::episode_slot(tmdb_id, *s, *e),
                    crate::store::SelectionEntry { hash: r.info_hash.clone(), file_path: path.clone() },
                ).await;
            }
            // Prune the scattered episode torrents for this season.
            for (h, rec) in &owned {
                if rec.provides.len() == 1 && rec.provides[0].0 == season {
                    prune_owned_hash(app, h).await;
                }
            }
            info!("consolidate: tmdb {} season {} -> pack {}", tmdb_id, season, r.info_hash);
            break; // one pack per season per tick
        }
    }
    Ok(())
}
```

`crate::acquire::episode_files` is already `pub(crate)` (Task 6), so it is callable here directly.

Wire the show path into `run_upgrade_once`: in the candidate-collection loop, **stop skipping shows** — instead include both movies and shows in the least-recently-checked, budget-truncated batch (store the `media_type` alongside each entry). Then in the dispatch loop, after `set_upgrade_checked`, branch on kind:

```rust
        match media_type {
            MediaType::Movie => {
                if let Err(e) = try_upgrade_movie(app, tmdb_id, &hashes, &rec, idle_window).await {
                    warn!("upgrade: tmdb {} skipped: {}", tmdb_id, e);
                }
            }
            MediaType::Show => {
                if let Err(e) = try_consolidate_show(app, tmdb_id, &hashes, idle_window).await {
                    warn!("consolidate: tmdb {} skipped: {}", tmdb_id, e);
                }
            }
        }
```

i.e. change the collected tuple to `(last_checked, tmdb_id, media_type, hashes, rec)` and drop the `if g.media_type != MediaType::Movie { continue; }` guard so each title (movie or show) is one budget unit.

- [ ] **Step 4: Run and commit**

Run: `cargo test --lib upgrade::` then `cargo test`.
Expected: PASS.

```bash
git add src/upgrade.rs src/tasks.rs src/acquire.rs
git commit -m "feat(upgrade): consolidate scattered episodes into a full-season cached pack (no regression)"
```

---

### Task 11: Scheduler — spawn the upgrade job (gated)

**Files:**
- Modify: `src/scheduler.rs` (spawn `run_upgrade_once` via `periodic`, gated on `app.config.upgrade.enabled()`)
- Test: `src/scheduler.rs` (gate test)

- [ ] **Step 1: Write the failing gate test**

Add to `mod trakt_gate_tests` (or a new `mod upgrade_gate_tests`) in `src/scheduler.rs`:

```rust
    #[test]
    fn upgrade_gate_follows_config() {
        let mut app = make_test_app(false);
        // default config has upgrade enabled (daily)
        assert!(app.config.upgrade.enabled());
        // a config with interval 0 disables it
        let mut cfg = (*app.config).clone();
        cfg.upgrade = crate::config::UpgradeConfig::from_parts(Some("0".into()), None, None, None);
        app.config = std::sync::Arc::new(cfg);
        assert!(!app.config.upgrade.enabled());
    }
```

(This asserts the gate predicate; `make_test_app` builds a default `Config`, whose `upgrade` defaults to enabled.)

- [ ] **Step 2: Spawn the job in `run`**

In `src/scheduler.rs::run`, after the Trakt block, add:

```rust
    if app.config.upgrade.enabled() {
        let secs = app.config.upgrade.interval_secs;
        info!("Upgrade engine enabled: re-scoring owned titles every {}s", secs);
        let upgrade_app = app.clone();
        handles.push(tokio::spawn(periodic(
            Duration::from_secs(secs),
            shutdown.clone(),
            move || {
                let app = upgrade_app.clone();
                async move { crate::upgrade::run_upgrade_once(&app).await; }
            },
        )));
    } else {
        info!("Upgrade engine disabled (UPGRADE_INTERVAL_SECS=0)");
    }
```

- [ ] **Step 3: Run and commit**

Run: `cargo test --lib scheduler::` then `cargo test`.
Expected: PASS.

```bash
git add src/scheduler.rs
git commit -m "feat(scheduler): spawn the upgrade job, gated on UPGRADE_INTERVAL_SECS"
```

---

## Phase D — docs & live gate

### Task 12: Documentation

**Files:**
- Modify: `CLAUDE.md`, `README.md`

- [ ] **Step 1: Update `CLAUDE.md`**

- Add `upgrade.rs` and `read_activity.rs` rows to the module table.
- Update the `acquire.rs` row: "acquire is optimistic (add best candidate → record Pending → return); observe runs the deferred pack-guard/title-validation/probe once files resolve, reaps dead torrents after `ACQUIRE_DEAD_TIMEOUT_SECS`, records `provides`, and writes the `selection` table; recovery is re-scrape."
- Update the `store.rs` row: schema v4; `selection` table; `OwnedRecord.provides`/`quality`; `upgrade_checks` cursor.
- Update the `vfs.rs` row: `build` consults the injected `SelectionMap` (largest-bytes fallback).
- Add a "Background tasks" bullet for the upgrade job (daily, gated on `UPGRADE_INTERVAL_SECS`).
- Add the new env vars under **Optional** with defaults: `UPGRADE_INTERVAL_SECS` (86400; 0 disables), `UPGRADE_BUDGET_PER_TICK` (20), `UPGRADE_IDLE_SECS` (300), `UPGRADE_STAGE_MAX_SECS` (604800), `ACQUIRE_DEAD_TIMEOUT_SECS` (600).
- Add a Key Design Decision: "SP3 optimistic-add + async reconcile; selection-inversion via the persisted `selection` table; daily upgrade engine (stage → idle-gated swap → prune); full-season cached consolidation; `provides` kills season-pack churn."

- [ ] **Step 2: Update `README.md`**

- Mirror the env-var additions and a short "Auto-upgrades & consolidation (SP3)" paragraph explaining the daily upgrade pass (on by default; disable with `UPGRADE_INTERVAL_SECS=0`), idle-gating, and full-season consolidation.

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md README.md
git commit -m "docs: SP3 upgrade engine, reconciliation rework, selection table, new env vars"
```

---

### Task 13: Adapt the live lifecycle test to the optimistic model

**Files:**
- Modify: `tests/lifecycle_test.rs` (`lifecycle_acquire_sintel_by_imdb`)

The optimistic model means `engine.acquire` returns `Pending` immediately and `observe` drives it to `Verified`. The existing live test likely asserts a near-synchronous Acquired/appears.

- [ ] **Step 1: Read the current test and identify the assertion that expects synchronous acquire**

Read `tests/lifecycle_test.rs` (the `lifecycle_acquire_sintel_by_imdb` function). Find where it calls `engine.acquire(...)` and asserts the outcome / VFS presence.

- [ ] **Step 2: Adapt the flow**

Change the assertion sequence to:
1. `let out = engine.acquire(req, Provenance::manual()).await;` then `assert!(matches!(out, AcquireOutcome::Pending(_) | AcquireOutcome::Acquired(_)));`
2. Poll `engine.observe(&provider.get_torrents().await?)` in a bounded loop (e.g. up to ~20 iterations with a short sleep) until `store.get_owned(hash).await.unwrap().status == OwnedStatus::Verified` (Sintel is a small cached CC torrent, so it should verify quickly).
3. Build the VFS via `DebridVfs::build(filtered, &selection_map)` where `selection_map = store.all_selection().await.into_iter().collect()` and assert the Sintel file appears under `Movies/`.
4. Cleanup as before (delete the owned torrent, remove owned + selection).

Show the loop:

```rust
    let hash = match engine.acquire(req.clone(), Provenance::manual()).await {
        AcquireOutcome::Pending(h) | AcquireOutcome::Acquired(h) => h,
        other => panic!("unexpected acquire outcome: {other:?}"),
    };
    let mut verified = false;
    for _ in 0..20 {
        let torrents = provider.get_torrents().await.expect("get_torrents");
        engine.observe(&torrents).await;
        if matches!(store.get_owned(hash.clone()).await.map(|r| r.status), Some(OwnedStatus::Verified)) {
            verified = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    assert!(verified, "Sintel should verify via observe within the poll window");
```

- [ ] **Step 3: Run the live gate (token-gated; run locally with creds) and commit**

Run (with `.env` providing exactly one provider token + `TMDB_API_KEY`):
```bash
cargo test --test lifecycle_test -- --ignored
```
Expected: PASS for the configured provider(s) (RD and/or TorBox); the other sub-test skips.

```bash
git add tests/lifecycle_test.rs
git commit -m "test(lifecycle): adapt Sintel acquire to the optimistic add + observe-verify model"
```

---

## Final verification (before merging the branch)

Per `CLAUDE.md`'s pre-commit gate — run all and confirm green:

```bash
cargo test \
  && INTEGRATION_TEST_LIMIT=10 cargo test --test integration_test -- --ignored \
  && INTEGRATION_TEST_LIMIT=10 cargo test --test repair_integration_test -- --ignored \
  && cargo test --test lifecycle_test -- --ignored
```

Also run the interactive Trakt live test if Trakt creds are present (supplementary):
```bash
cargo test --test trakt_integration_test -- --ignored --nocapture
```

Then integrate per the project's git workflow (local merge to `main`, no auto-push; one branch per phase).
