# SP3 — Upgrade engine + reconciliation model — design

- **Date:** 2026-06-09
- **Status:** Approved (brainstorming session, Kiril + Claude) — ready for implementation plan
- **Phase:** SP3 of the Trakt-integration roadmap (`docs/superpowers/specs/2026-06-05-trakt-integration-design.md`)
- **Depends on:** SP0 (store/AppState/config), SP1 (acquisition engine), SP2 (Trakt sync + reconciler) — all merged to `main` locally
- **Branch:** `sp3-upgrade-reconciliation`

## 1. Overview

SP3 turns acquisition from a **synchronous, judge-immediately** operation into an
**optimistic-add + asynchronous-reconcile** model, and adds the **quality** half
of the feature: auto-upgrading owned titles to better releases and consolidating
scattered per-episode torrents into season packs — all without ever interrupting
playback.

Two interlocking goals:

1. **Robustness (always-on bug-fix).** Today `acquire` materialises synchronously
   (~15s poll), runs the pack-guard / strict title-validation / probe inline, and
   on failure deletes the torrent and tries the next candidate. A DHT torrent can
   sit at 0 seeds for minutes then come alive, so this **judges availability too
   early**. SP3 makes `acquire` add the best candidate, record it `Pending`, and
   return; an asynchronous `observe` resolves it (runs the deferred gates once
   files appear), cleans up only genuinely-dead torrents after a real (minutes)
   timeout, and recovers by re-scraping. It also records what episodes each owned
   hash **provides**, killing the SP2 season-pack re-acquire churn.

2. **Quality (on by default, daily).** A new slow `upgrade.rs` job re-scores owned
   titles and, when a clearly-better release exists, **stages** it in the
   background and **swaps** the title's representative only once the file is idle
   (proxy read-activity), then prunes the superseded torrent. The same machinery
   consolidates scattered per-episode torrents into a **full-season cached pack**.

The enabling refactor — required by both upgrades and consolidation — is
**inverting `vfs::build()` selection**: "which release represents this title" stops
being a hardcoded largest-bytes pick and becomes a persisted, injected decision.

### Goals

- Stop judging availability synchronously; let slow-to-seed releases come alive.
- Reap genuinely-dead torrents after a real timeout and recover by re-scraping.
- Record per-hash episode membership (`provides`) to end season-pack churn.
- Auto-upgrade owned titles to meaningfully-better releases, idle-gated.
- Consolidate scattered episodes into a full-season cached pack, idle-gated.
- Preserve playback: never swap/prune a release a client is actively reading.
- Provider-neutral throughout (Real-Debrid + TorBox); never touch non-owned torrents.

### Non-goals

- No persisted ranked-candidate list (recovery re-scrapes on demand).
- No speculative downloading for upgrades (upgrade targets must be **cached**).
- No per-title manual release pinning (deferred to SP4 UI; the auto-pick is trusted).
- No partial-season consolidation (full-season packs only).

## 2. Decisions log

Every row was settled during the SP3 brainstorm and is binding on the plan.

| # | Topic | Decision |
|---|-------|----------|
| 1 | Scope | **Full bundle:** reconciliation rework + episode→season-pack consolidation + upgrade engine + `vfs::build` selection inversion, in one SP3 spec. |
| 2 | Architecture | **Layered fast/slow jobs (Approach A).** `observe` = scrape-free in-flight resolver every scan tick; new `upgrade.rs` = slow scraping quality-reconciler. *Not* one unified loop that re-scrapes the whole library every cycle (rejected: fights the project's rate-limit posture, fuses fast+slow concerns, harder to test). |
| 3 | Recovery model | **Re-scrape on demand.** No persisted candidate list. On death: blacklist the dead hash → re-run the acquire path (fresh scrape → next-best non-blacklisted). Fresh cache flags/seeders; one extra scrape per recovery; blacklist prevents re-picking the dead one. |
| 4 | Upgrade policy | **Moderate — meaningful jumps only.** Proactively re-score owned titles; upgrade only on a concrete category jump (uncached→cached, higher source tier, higher resolution within ceiling). Skip marginal score deltas (seeder wobble, same-tier re-encode). |
| 5 | Consolidation | **Full-season cached pack, no regression.** Consolidate scattered per-episode torrents only when a **cached** pack contains the **full season** (all *aired* episodes per TMDB), and is same-or-higher source tier + resolution than every episode currently held. Partial-season packs are never adopted. |
| 6 | Idle-gating | Swap + prune gated on **proxy read-activity**: a slot idle = no read for `UPGRADE_IDLE_SECS` (default 300s) and no open handle. The selection flip is harmless to a live stream; the **prune** is the operation that must not run under an active read. |
| 7 | Selection inversion | `vfs::build` consults a persisted **`selection`** table (slot → live `{hash, file_path}`); **falls back to largest-bytes** when no managed entry exists, so external/un-managed/pre-SP3 torrents behave exactly as today. The table is **hash-keyed**, so it is stable across same-hash repair (only a genuine upgrade/consolidation changes it). |
| 8 | Episode ownership | `OwnedRecord` gains **`provides: Vec<(season, episode)>`** — the episodes a hash supplies (single-episode ⇒ one pair; pack ⇒ the full set, SE-mapped vs TMDB). Aggregated in-memory each tick; ends the SP2 churn. No separate `owned_episodes` table. |
| 9 | Upgrade rollout | **On by default, daily.** `UPGRADE_INTERVAL_SECS` defaults to `86400`; `0` disables. Deploying SP3 begins a daily upgrade+consolidation pass for any deployment with engine-owned titles — a deliberate, documented behavioural change matching the original auto-upgrade goal. |
| 10 | Migration | **None required.** 1.0.8 (the only deployment in the wild) carries only the regenerable `matches` cache; the SP1/SP2 authoritative tables are still local/unpushed. The Store's existing additive forward-migration creates the new empty `selection` table and `matches` carries over unchanged. `provides` is `#[serde(default)]` for code robustness. No bespoke migration code or data-preserving migration test. |

## 3. Architecture

### 3.1 Two reconciliation layers

| Layer | Cadence | Scrapes? | Where | Responsibility |
|-------|---------|----------|-------|----------------|
| **In-flight resolver** | every scan tick (`SCAN_INTERVAL_SECS`) | no | `observe` in `acquire.rs` | Resolve optimistically-added `Pending` torrents: run the deferred pack-guard / title-validation / probe once files appear; record `provides` + `selection`; clean up only after `ACQUIRE_DEAD_TIMEOUT_SECS`; on death → blacklist → re-scrape → next. |
| **Quality reconciler** | slow (`UPGRADE_INTERVAL_SECS`, default daily) | yes (owned titles, budgeted) | new `upgrade.rs` job | Re-score owned titles; stage meaningful upgrades + full-season cached consolidations; idle-gated swap; prune superseded. |

### 3.2 Always-on vs gated

- **Always on** (robustness / bug-fix; no observable quality change): optimistic-add,
  deferred validation in `observe`, the real dead-timeout, re-scrape recovery,
  `provides` recording (kills churn), and the selection-inversion infrastructure
  (with largest-bytes fallback so un-managed torrents are unchanged).
- **Gated on `UPGRADE_INTERVAL_SECS`** (default daily, `0` disables): the proactive
  upgrade job — quality upgrades **and** active episode→pack consolidation. Both
  require scraping owned titles, so both live in the one job.

### 3.3 Module changes

| File | Change |
|------|--------|
| `acquire.rs` | `acquire` becomes optimistic (add best candidate → record `Pending` → return; no synchronous 15s poll/delete; instant-cached may still verify inline). `observe` grows deferred pack-guard/title-validation/probe, `provides` + `selection` writes, the dead-timeout, lazy `provides` backfill, and re-scrape recovery. |
| `upgrade.rs` | **New.** The slow quality reconciler: round-robin batch, re-score, `is_meaningful_upgrade`, staging, idle-gated swap + prune, consolidation. |
| `vfs.rs` | `build()` selection **inverted** — consult an injected selection map; show path reframed around per-episode slots; largest-bytes fallback retained. |
| `store.rs` | Schema → v4: new `selection` table (slot → `{hash, file_path}`); `OwnedRecord` gains `provides` and a compact `quality` summary. Plus a tiny `last_checked` cursor for the upgrade round-robin. |
| `app_state.rs` | Add the in-memory read-activity tracker `Arc<RwLock<HashMap<String, Instant>>>`. |
| `dav_fs.rs` | `read_bytes` stamps read-activity (no added latency on the byte path). Repair's existing mid-read swap is unchanged. |
| `scheduler.rs` | Spawn the upgrade job, gated on `UPGRADE_INTERVAL_SECS > 0`; share the scan tick's `get_torrents` snapshot where practical. |
| `config.rs` | New knobs (§7). |

## 4. Reconciliation rework (always-on)

### 4.1 `acquire` becomes optimistic

1. Scrape → rank → drop blacklisted / out-of-ceiling (unchanged).
2. Add the **best** non-blacklisted candidate by hash, `select_files`, record an
   `OwnedRecord { status: Pending }` immediately, return `Pending`. **No 15s poll,
   no synchronous pack-guard/title-validation/probe, no synchronous delete.**
3. Exception: if the add resolves **instantly cached** (files present this call),
   it may run the gates inline and return `Acquired`, preserving the cached-hit UX
   (TorBox). Otherwise the verdict belongs to `observe`.

### 4.2 `observe` is the resolver

Per tick, for each owned `Pending` torrent matched by lowercased hash against the
shared `get_torrents` list:

- **Files not resolved yet** (metadata pending / `files: null`): leave `Pending`.
  Give up only once `now − added_at > ACQUIRE_DEAD_TIMEOUT_SECS`.
- **Files resolved** → run the deferred gates now: pack-guard (movie multi-feature)
  → strict title-validation (`identify_name` ⇒ tmdb id) → if cached, probe tracks.
  - Pass ⇒ mark `Verified`, record `provides` + the `quality` summary, write the
    `selection` entry/entries (per §5).
  - Fail (pack / title / corrupt / wrong-language) ⇒ blacklist hash → **re-scrape**
    → add next-best → stay `Pending`. (Transient-probe defer keeps `MAX_VERIFY_ATTEMPTS`.)
- **Downloading with progress** → leave `Pending` (the "comes alive after minutes"
  case the model exists for).
- **Dead** = provider status `magnet_error`/`dead`/`error`/`virus`, **or** no
  progress for `ACQUIRE_DEAD_TIMEOUT_SECS`, **or** files never resolved past that
  timeout ⇒ delete (owned-only) → blacklist → re-scrape → next.

`cleanup_leaked` and the interim synchronous `materialise` poll are **superseded**
by this model. The existing manual-entry guard (`Provenance::has_manual_entry`)
still prevents auto-removal of manual torrents.

### 4.3 Timeout knobs

The interim 15s materialise give-up is replaced by **`ACQUIRE_DEAD_TIMEOUT_SECS`**
(default 600). Progress-stall detection keeps its existing meaning; both are
configurable.

## 5. Persistence + `vfs::build` selection inversion

### 5.1 `OwnedRecord.provides`

`provides: Vec<(u32, u32)>` — the episodes a hash supplies. Single-episode ⇒
`[(s,e)]`; movie ⇒ empty; **season pack ⇒ every episode its files resolve to**,
SE-mapped and validated against TMDB's episode list (the SP2 structured-episode
model), together with each episode's `file_path`. Computed at **verify time in
`observe`**.

`group_owned_by_tmdb` aggregates the **union of `provides`** across a show's owned
hashes, so a pack reports all its episodes as owned and `monitor_episodes` stops
re-acquiring (re-scraping) episodes the pack already covers — the SP2 churn fix.
`provides` is the single source of truth (persisted per record, aggregated
in-memory each tick; correct across restart). **Lazy backfill:** when `observe`
meets an owned show torrent whose `provides` is empty, it computes it then and
there (relevant only to local dev DBs predating SP3; no production data exists).

### 5.2 `OwnedRecord.quality`

A compact summary recorded at verify time — `{ cached, source_tier, resolution,
score }` — so the upgrade job compares the freshly-scraped top candidate against
the owned release apples-to-apples, without re-parsing the listing name.

### 5.3 `selection` table

Slot → the live representative the VFS shows.

- **Key** (slot): movie ⇒ `tmdb_id`; episode ⇒ `tmdb_id|season|episode`.
- **Value:** `{ hash, file_path }`.
- **Hash-keyed ⇒ stable across repair.** Repair re-adds the *same* infohash with a
  new `torrent_id`; `vfs::build` re-resolves hash→current torrent from the live
  listing, so repair needs no selection change. Only a genuine upgrade/consolidation
  (different hash) rewrites it.
- **Writers:** `observe` writes it when a slot's torrent becomes `Verified`; the
  upgrade job rewrites it at idle-swap; removal (Trigger A/B) deletes the title's
  slot entries.
- **Self-healing:** a stale entry (hash no longer listed) ⇒ `vfs::build` falls back
  to largest-bytes for that slot, and `observe`/upgrade rewrite it. Never fatal.

### 5.4 `vfs::build` inversion

`build()` is given the selection map (loaded once per scan from the store):

- **Movies:** if a selection entry exists *and its hash is present + not hidden*,
  use it (locate the file by stored `file_path`); **else fall back to largest-bytes**
  — today's exact behaviour.
- **Shows:** reframe building around per-episode slots — prefer the selection entry
  `{hash, file_path}` per episode; **else fall back to today's filename-derived pick**.
  This is the larger refactor (the show path currently iterates all torrents and
  dedupes by filename within seasons); it is behaviour-preserving when no managed
  selection exists, and is delivered as a focused, well-tested task.

The fallback is what keeps external / un-managed / pre-SP3 torrents behaving exactly
as today.

## 6. Upgrade engine (`upgrade.rs`, gated)

A new slow periodic job, spawned when `UPGRADE_INTERVAL_SECS > 0`. Each tick:

### 6.1 Bounded scope

Read owned titles grouped by tmdb_id; process a batch of `UPGRADE_BUDGET_PER_TICK`
(default 20), **least-recently-checked first** (a tiny persisted `last_checked` per
tmdb_id, survives restart), so the whole library is covered over many cycles without
storming the scraper.

### 6.2 Meaningful-jump predicate

`is_meaningful_upgrade(current, candidate)` — concrete category jumps only, never
raw score wobble:

- candidate must be **cached** (we never speculatively download an "upgrade";
  uncached-lapse is handled by SP2's availability re-acquire), **and**
- current is uncached/lapsed, **or** candidate source tier is higher
  (WEB→BluRay→REMUX), **or** candidate resolution is higher within the ceiling.
- Same-tier re-encodes, seeder changes, marginal size diffs ⇒ no upgrade.

### 6.3 Stage → verify (playback untouched)

Add the candidate (cached ⇒ resolves fast), `select_files`, run the **same gates as
acquire** (pack-guard / strict title-validation / probe). Record it as a staged
owned entry, provenance copied (sticky) from the current record. Both old + new are
now owned; **`selection` still points at the old release ⇒ VFS unchanged ⇒ in-flight
playback uninterrupted.**

### 6.4 Idle-gated swap + prune

Using the read-activity tracker (§6.5), a slot is **idle** when it has had no proxy
read for `UPGRADE_IDLE_SECS` (default 300s) and has no open handle. When the stage
is verified **and** the slot is idle, atomically:

1. point `selection` slot → new `{hash, file_path}`, update `provides`, mark new
   `Verified`;
2. **prune** the superseded torrent (owned-only guard) + its records.

The selection flip alone is harmless to a live stream (it affects only *new* opens);
it is the **prune** that must not happen under an active read — so both are gated on
idle. If never idle, the stage waits; `UPGRADE_STAGE_MAX_SECS` (default 7d) abandons
an un-swapped stage so we never hold two copies forever.

### 6.5 Read-activity tracker

`Arc<RwLock<HashMap<String, Instant>>>` in `AppState`, stamped (cheap, short critical
section) on each `dav_fs::read_bytes`. `is_idle(path) = now − last_read >
UPGRADE_IDLE_SECS` (true if never read). In-memory only; after a restart everything
reads idle, which is safe (no pre-existing open handles to disturb). It is a
best-effort hint, never a correctness dependency, and must add **no latency to the
byte path**.

SP2's availability re-acquire (owned title absent from the listing → re-acquire)
stays in `reconcile_wanted`; the upgrade job does not duplicate it.

## 7. Consolidation (episode → season pack)

Rides on §6's stage → idle-swap → prune machinery; a *many→one* swap evaluated
during the upgrade job's per-show pass.

### 7.1 Detection

For a show in the batch, per season, find episodes held as **individual per-episode
torrents** (selection slots pointing at single-`provides` hashes). If the season is
already backed by a full-season pack, skip. Otherwise scrape and look for season
packs for that season.

### 7.2 Trigger conditions (strict)

- the pack is **cached**;
- the pack is a **full-season pack** — its files SE-map (validated against TMDB's
  aired-episode list for that season) to **every aired episode of the season**, not
  merely the episodes we hold;
- **no regression** — for every episode currently owned individually, the pack's
  copy is same-or-higher source tier *and* resolution. If the pack is worse for any
  owned episode, skip.

Partial-season packs are excluded by the full-season rule (we never adopt something
that would drop episodes).

### 7.3 Stage

Add the pack (cached ⇒ fast), `select_files`, run show title-validation + SE-mapping
vs TMDB + per-episode probe (the heaviest step — bounded by season length, once per
consolidation; may be sampled if it proves too slow). Record staged with
`provides = full season`. Episode slots still point at the individual torrents ⇒ VFS
unchanged.

### 7.4 Idle-gated swap + prune (whole-season unit)

When the pack is verified **and** the season is idle (no active read on any of its
episodes + no open handles to the torrents being pruned):

1. repoint **every** episode slot of that season `selection → { pack_hash, file_path }`;
2. **prune all** the superseded per-episode torrents (owned-only) + their records;
3. previously-missing aired episodes the pack carries get fresh selection entries —
   **the season's gaps are filled** as a side-effect.

### 7.5 Steady state

Because `provides` records the pack's full season, `monitor_episodes` won't
re-acquire those episodes (churn fixed passively *and* actively). For a still-airing
show, newly aired episodes arrive as individual torrents and are re-consolidated once
a cached full-season pack covering them appears; for an Ended show the full-season
pack is stable and final.

## 8. Config, error handling, provider neutrality

### 8.1 New env vars

Parsed in `config.rs`, read through `Config` accessors so SP4's DB-override layer can
later supply them.

| Var | Default | Min | Effect |
|-----|---------|-----|--------|
| `UPGRADE_INTERVAL_SECS` | `86400` (daily) | 600 | Paces the upgrade/consolidation job; `0` disables (job not spawned). |
| `UPGRADE_BUDGET_PER_TICK` | `20` | 1 | Max titles re-scraped per upgrade tick (least-recently-checked first). |
| `UPGRADE_IDLE_SECS` | `300` | 30 | Idle window gating swap+prune. |
| `UPGRADE_STAGE_MAX_SECS` | `604800` (7d) | 3600 | Abandon an un-swapped stage so we never hold two copies forever. |
| `ACQUIRE_DEAD_TIMEOUT_SECS` | `600` | 120 | Patient "never came alive" ceiling in `observe`, replacing the interim 15s give-up. |

Same parse/validate/exit style as today.

### 8.2 Error handling — upgrades are non-destructive by construction

- Scrape failure in the upgrade job ⇒ skip that title, log, advance the cursor
  (don't block the batch) — mirrors the SP2 `get_torrents`-fail guard.
- Staging add-fail / not-actually-cached / title-validation/probe fail ⇒ blacklist
  the candidate, clean up any leaked torrent (**owned-only**), **leave the current
  release untouched**. We never degrade a working release on a failed upgrade.
- The **prune** is owned-only (`owned_hashes` guard) and idle-gated; a slot that
  never goes idle is simply never pruned (safety over eagerness); manual/external
  torrents are never touched.
- `observe` dead-timeout deletes only genuinely-dead **owned Pending** torrents;
  manual entries keep their never-auto-remove guard.
- `selection` is self-healing (§5.3); a stale entry degrades to the largest-bytes
  fallback, never an error.
- Read-activity stamping adds no latency to byte serving; it is a hint.

### 8.3 Provider neutrality

All add/delete/select/cached-check via `DebridProvider` (RD + TorBox). Known TorBox
edge: "Download already queued" is a queue state *not* shown in `mylist`, so an
optimistically-added hash can be invisible to `get_torrents`. Handled by a generous
dead-timeout + **blacklist-on-death → re-scrape picks a *different* hash**, so we
never loop re-adding the same queued one.

### 8.4 Backwards-compat & docs

No migration (decision 10). With no `selection` entries the VFS builds exactly as
today. `CLAUDE.md` + `README.md` updated for the new vars, `upgrade.rs`, the
`selection` table / `provides` / `quality` fields, the read-activity tracker, and the
reconciliation-model change (per the project's docs-in-sync rule).

## 9. Testing strategy

Per the project's TDD rule and three-layer model (deterministic CI · fixtures · live
`#[ignore]`). Integration tests are the final gate.

### 9.1 Deterministic (CI, no tokens — `MockScraper`/`MockProvider`/`MockTrakt` + in-memory redb)

**Reconciliation rework** — `acquire` adds best candidate → records `Pending` →
returns (no synchronous delete / 15s block); `observe` runs pack-guard /
title-validation / probe only once files resolve; **slow-seed patience**
(downloading-with-progress within timeout stays `Pending` — the explicit "no longer
judges too early" regression); **dead-timeout** (paused clock `start_paused`:
stuck-past-`ACQUIRE_DEAD_TIMEOUT_SECS` → delete + blacklist + re-scrape next);
re-scrape recovery excludes the blacklisted dead hash.

**Selection inversion** — `vfs::build` uses the selection map for managed slots and
falls back to largest-bytes when absent (behaviour-preserving); hash-keyed stability
across a `torrent_id` change (repair scenario); per-episode show slots pick
pack-vs-individual from selection.

**Churn fix** — a pack's `provides` covers the full season ⇒ `group_owned_by_tmdb`
reports all episodes owned ⇒ `monitor_episodes` acquires nothing (the SP2 follow-up
regression); lazy `provides` backfill in `observe`.

**Upgrade engine** — `is_meaningful_upgrade` table-test (cached/tier/resolution jumps
trigger; marginal deltas don't); **stage→idle-swap→prune** (quality A owned, cached
better B offered → B staged; swap deferred while a simulated recent read marks the
slot active; swap + owned-only prune occur once idle); non-destructive failure
(staged B fails validation → blacklisted, A untouched); stage-max abandonment;
budget / round-robin cursor.

**Consolidation** — full-season cached pack with no regression → episodes staged →
idle swap repoints all season slots + prunes per-episode torrents + fills gaps;
**partial-season pack ⇒ not consolidated**; regression guard (pack worse for any
owned episode ⇒ not consolidated).

**Plumbing** — read-activity tracker unit tests (stamp / `is_idle` / never-read =
idle); `config_test` additions (new vars, **daily default**, min bounds, `0`
disables); `store` smoke that a `matches`-only (1.0.8-style) DB opens and starts the
new tables empty (no bespoke migration needed).

### 9.2 Live (`#[ignore]`, token-gated), cross-provider

- Adapt `lifecycle_acquire_sintel` to the async model — assert `acquire` returns
  `Pending` fast and `observe` drives it to Acquired/cleanup (RD + TorBox).
- Upgrade/consolidation can't be forced live on demand (no guaranteed "better
  release") ⇒ **deterministic-only**; not in the live gate.
- Pre-commit gate unchanged in shape: deterministic tests join `cargo test`; the
  adapted `lifecycle_test` joins the `-- --ignored` gate.
- Opportunistically fix the known flaky
  `dav_fs::provider_abstraction_tests::fetch_cdn_range_recovers_from_expired_url`
  while SP3 is in `dav_fs.rs`.

## 10. Open questions (resolve during planning/implementation, not now)

- Exact shape of the `selection` slot key encoding and the `quality` summary type
  (where it lives relative to `ReleaseInfo`).
- Whether per-episode probe of a full season pack needs sampling for performance, or
  is acceptable in full (decide against a real pack during implementation).
- Whether the upgrade `last_checked` cursor is its own tiny table or folded into an
  existing structure (a store-design detail).
- Read-activity map keying: VFS path vs `torrent_id|file_path` (pick the one
  `dav_fs` already has cheaply at `read_bytes`).
