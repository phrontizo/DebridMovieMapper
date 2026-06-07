# SP1 — Acquisition engine — design

- **Date:** 2026-06-06
- **Status:** Approved for plan
- **Phase:** SP1 of the Trakt-integration roadmap (SP0 foundation merged to `main`)
- **Parent spec:** `docs/superpowers/specs/2026-06-05-trakt-integration-design.md` (decisions log #1–#17; this document refines the "SP1 — Acquisition engine" sketch)
- **Branch:** `sp1-acquisition-engine`

## 1. Overview

SP1 is the foundational capability the whole Trakt feature stands on: **given a title, find a release and put it in the debrid account at the wanted quality.** Today the service only *reads* the account; SP1 makes it *acquire*. Given a TMDB/IMDB id (+ optional season/episode) and quality preferences, the engine queries a Stremio-addon scraper (Torrentio by default, auto-configured from the active provider's token — overridable to any addon), parses and scores candidate releases, picks the best (cached-first, always-fill), adds the magnet, selects the right file(s), tags the torrent as service-owned, records an authoritative `hash → MediaMetadata`, then file-probes the cached container to verify audio/subtitle language requirements — falling back to the next candidate on failure, stall, bad audio, or corruption.

The engine's job ends at "magnet added + tagged + (probe) verified." The existing scan → identify → VFS pipeline then serves the torrent unchanged, so SP1 reuses almost all of the current serving path. There is **no Trakt and no web UI in SP1**; acquisition is exercised by tests and a temporary `--acquire` CLI trigger.

### Goals
- Resolve a title id → a ranked candidate list → acquire the best acceptable release.
- Honour a hard resolution ceiling + ranked soft prefs (HEVC/HDR/verifiable-container/seeders/size), cached-first, always-fill.
- Verify audio (a language or `original`) and subtitle (a language or `none`) by probing the cached container; on mismatch/corruption blacklist and try the next candidate.
- Auto-recover from failed/stalled/corrupt/bad-audio releases by promoting the next candidate.
- Generalise the repair add/select primitive so repair and acquisition share it without duplicating logic.
- Make our adds authoritatively identified (no filename guessing for content we chose).

### Non-goals (SP1)
- Trakt sync / wanted-set / reconciler (SP2), upgrades (SP3), web UI (SP4).
- No removal-on-Trakt-removal; the only deletes in SP1 are blacklist cleanup of service-owned torrents the engine itself added.
- No automatic playback/decode-failure detection (see §6 known limitation).

## 2. Decisions log (SP1-specific)

The parent spec's decisions #1–#17 stand. SP1 adds:

| # | Topic | Decision |
|---|-------|----------|
| S1 | SP1 trigger | A **temporary** `--acquire <movie\|series> <imdb-or-tmdb-id> [season episode]` CLI mode (mirrors `--healthcheck`), clearly marked for removal once SP2/SP4 drivers exist. The engine is also callable as a library (tests, lifecycle integration test). |
| S2 | Repair generalisation | Extract only the **shared low-level primitive** (`reacquire::materialise`: add hash → select chosen files → report cached); repair keeps its same-hash semantics + health state machine and is reimplemented on top of it. Acquisition owns its own candidate-loop + blacklist. (acquire↔upgrade consolidation is revisited in SP3.) |
| S3 | Candidate management | **Re-scrape on fallback**; the candidate list is never persisted (always recomputed fresh, picking up new releases + current cache flags). Only the blacklist (+ owned/authoritative tables) persists. |
| S4 | Probe parser | **Hand-rolled** focused parser: EBML (MKV) + ISO-BMFF (MP4/MOV/M4V), reading incrementally over ranged CDN GETs. No new dependencies. |
| S5 | Unverifiable containers | Probe covers MKV + ISO-BMFF. Other/unparseable containers (AVI/TS/WMV/FLV) → `Inconclusive` → **accept with warning**; scoring **prefers verifiable containers** so an MKV/MP4 is chosen over an AVI at comparable quality. |
| S6 | Corrupt header | A **recognised** container (correct magic) with a **definitively broken** header → treated as a bad release: blacklist + delete + next candidate. Detection is conservative (magic matches AND structure clearly broken); ambiguous/under-fetched cases stay `Inconclusive`→accept. |
| S7 | Probe failure taxonomy | Transient fetch errors are retried/deferred (re-probed next `observe` cycle), never a verdict; only positive language-mismatch and definitive corruption blacklist. |
| S8 | Title-identity validation | Torrentio routinely returns wrong-title torrents. After `materialise`, **strictly** validate the selected file via the existing `identify_name`: accept only when it **confidently** resolves to the expected `tmdb_id` (+ matching season/episode for series); anything uncertain / unidentifiable / different-title / wrong-S·E → blacklist `WrongTitle` + delete + next candidate. Runs **before** recording `authoritative_ids` and before probing, so the authoritative override is only ever applied to a confirmed-correct file — the validation **is** the safety net the override would otherwise bypass. May leave a title unfilled if no candidate confidently matches (correctness over fill). |
| S9 | Scraper URL templating | The Torrentio URL is **templated by default** from the active provider + token (Torrentio supports both Real-Debrid and TorBox as of 2026), so the user configures nothing beyond the debrid key already set. `SCRAPER_ADDON_URL` is an **optional override** used verbatim — for a self-hosted Torrentio or a different addon (Comet/MediaFusion). The default sends the debrid token to the public `torrentio.strem.fun`; privacy-conscious users override with a self-hosted instance. If the active provider has no default mapping, acquisition errors clearly asking for the override. Exact Torrentio option-string syntax is verified at implementation. |

## 3. Architecture

### 3.1 New modules

| Module | Responsibility | Key interface |
|--------|----------------|---------------|
| `scraper.rs` | `Scraper` trait + `TorrentioScraper` + `MockScraper`. `TorrentioScraper` builds `addon_base` by templating the active provider+token (or uses `SCRAPER_ADDON_URL` verbatim if set), then GET `{addon_base}/stream/{movie\|series}/{id}.json` (id = `tt…` or `tt…:S:E`), parse `streams[]`. | `find(imdb_id, kind, season, episode) -> Result<Vec<RawCandidate>, AppError>` |
| `release.rs` | Parse a `RawCandidate` → `ReleaseInfo`; score it against `QualityPrefs`; layer-1 language down-rank. | `parse(&RawCandidate) -> ReleaseInfo`; `score(&ReleaseInfo, &QualityPrefs) -> Option<Score>` (None = excluded by the hard resolution ceiling) |
| `probe.rs` | Hand-rolled EBML + ISO-BMFF track extraction over ranged CDN GETs; verify language requirements. | `probe_tracks(&Client, url) -> Result<Vec<Track>, ProbeError>`; `verify(&[Track], &LangReq) -> Verify` |
| `reacquire.rs` | **Shared primitive** extracted from repair: add magnet by hash → wait → get info → select the chosen files → re-get → report `{new_id, info, cached}`. The file-selection is a parameter. | `materialise(&dyn DebridProvider, hash, select: impl Fn(&TorrentInfo) -> Vec<u32>) -> Result<Materialised, AppError>` |
| `acquire.rs` | The engine: scrape → rank → pick → materialise → **validate-identity** → verify → fallback; owns blacklist/owned/authoritative writes. | `acquire(AcquireRequest) -> AcquireOutcome`; `observe(&[Torrent])` |

### 3.2 Types (shape, not final signatures)
- `RawCandidate { name: String, description: String, info_hash: String, file_idx: Option<usize>, file_hint: Option<String> }` — the raw stream + the addon's pointer to which file is the requested title.
- `ReleaseInfo { resolution: Resolution, codec, hdr: bool, audio_tags: Vec<String>, languages: Vec<String>, group: Option<String>, size: Option<u64>, seeders: Option<u32>, cached: bool, container: Container, info_hash, file_idx, file_hint }` — `cached` is parsed from the addon's per-account flag (e.g. `[RD+]`/`[TB+]`).
- `QualityPrefs { max_resolution, audio: AudioReq, subtitle: SubReq, prefer_hevc: bool, prefer_hdr: bool }`; `AudioReq = Lang(String) | Original`; `SubReq = Lang(String) | None`.
- `Track { kind: TrackKind /* Audio|Subtitle|Video|Other */, language: Option<String> }`.
- `Verify = Pass | FailAudio | FailSubtitle | Inconclusive`.
- `ProbeError = Transient | Corrupt | Unsupported | TracksNotFound`.
- `AcquireRequest { imdb_id: String, tmdb_id: u64, kind: MediaKind, season: Option<u32>, episode: Option<u32>, prefs: QualityPrefs }`.
- `AcquireOutcome = Acquired{hash} | Pending{hash} | NoAcceptableRelease | TemporarilyUnavailable`.

### 3.3 Repair refactor boundary
`reacquire::materialise` is the **only** shared piece. `repair.rs` keeps `RepairManager`, its health state machine (Healthy/Broken/Repairing/Failed, 30s cooldown, 3-attempt cap, rapid-loop prevention) and same-hash semantics; its `add_and_select_files` is reimplemented to call `materialise` with a selector that matches the old torrent's selected file paths (preserving today's behaviour, verified by the existing `repair_integration_test`). Acquisition calls `materialise` with a selector that picks the requested title's file(s) (movie → main video file; episode → the addon's `file_hint`/`file_idx`, falling back to the largest video file matching the S/E). `materialise` is provider-neutral and reuses the exact magnet form `format!("magnet:?xt=urn:btih:{}", hash)` and file-match-by-path logic that repair uses today.

## 4. Acquire flow

### 4.1 `acquire(req)`
1. `scraper.find(req.imdb_id, req.kind, season, episode)` → `parse` each → `score` against `req.prefs`; drop blacklisted hashes (lookup by `(tmdb_id, hash)`), drop those above the resolution ceiling; rank **cached-first, then score**.
2. If no candidates remain → `NoAcceptableRelease` (nothing added). If `scraper.find` errors → `TemporarilyUnavailable` (nothing added; retried when re-triggered).
3. Take the top candidate → `reacquire::materialise(hash, selector)`.
4. **Validate title identity (strict, §S8):** run the selected file's name through `identify_name`; accept only if it confidently resolves to `req.tmdb_id` (+ for series, the file's parsed season/episode equals the request). On mismatch / uncertain / unidentifiable / wrong-S·E → blacklist `(tmdb_id, hash)` reason `WrongTitle` + `delete_torrent` + next candidate (no `owned_hashes`/`authoritative_ids` recorded for a rejected hash) up to `MAX_ACQUIRE_ATTEMPTS`.
5. Validated → record `owned_hashes[hash] = {tmdb_id, kind, source: "manual", added_at, status: Pending}` and `authoritative_ids[hash] = MediaMetadata` (now safe — the file is confirmed to match `req.tmdb_id`). Validation is name-based, so it runs identically for cached and not-yet-downloaded picks.
6. **If `materialise` reported cached (`status == "downloaded"`):** resolve the file's CDN URL (`provider.resolve_url`) → `probe` → `verify`:
   - `Pass` → set `owned_hashes` status `Verified` → `Acquired{hash}`.
   - `FailAudio`/`FailSubtitle`/`Corrupt` → blacklist `(tmdb_id, hash)` with the reason → `delete_torrent` + drop `owned_hashes`/`authoritative_ids` for that hash → next candidate (we already hold the ranked list this call) up to `MAX_ACQUIRE_ATTEMPTS`.
   - `Inconclusive`/`Unsupported`/`TracksNotFound` → accept with a warning, status `Verified` → `Acquired{hash}`.
   - `Transient` probe error → retry with a fresh URL; if still failing, leave status `Pending` (re-probed by `observe`) → `Pending{hash}`.
7. **If not cached:** leave it downloading (add-and-react), status `Pending` → `Pending{hash}`. `observe` finishes it later.
8. If all attempts exhausted → `NoAcceptableRelease`.

### 4.2 `observe(&[Torrent])` — called each scan tick (wired into the existing scan loop)
For each service-owned torrent (present in `owned_hashes`):
- **`Pending` and now `downloaded`** → probe + verify (same handling as §4.1 step 4): `Pass`/inconclusive → `Verified`; `FailAudio`/`FailSubtitle`/`Corrupt` → blacklist + delete + re-acquire the title's next candidate (re-scrape).
- **`magnet_error`/`dead`/error status, or stuck with no progress past `STALL_TIMEOUT_SECS`** → blacklist(reason) + delete + re-acquire next candidate. Progress is tracked per owned torrent id in an in-memory map.
- A re-acquire here re-runs `acquire` for the title (re-scrape, blacklist now excludes the bad hash).

## 5. Data model — new redb tables (`store.rs` migration, `SCHEMA_VERSION` 1 → 2)

- `owned_hashes`: `hash → {tmdb_id: u64, kind, source, added_at, status: Pending|Verified}`. The guard that any delete/cleanup only ever touches torrents the engine added; also tracks probe completion.
- `authoritative_ids`: `hash → MediaMetadata` (the engine-resolved title/year/`tmdb:<id>`). Consulted by the scan loop **before** `identify_torrent`.
- `blacklist`: `(tmdb_id, hash) → {reason: Failed|Stalled|Dead|BadAudio|BadSubtitle|Corrupt|WrongTitle|Manual, at}`.

**Migration:** redb creates tables lazily on first `open_table`, so the v1→v2 step is a version bump plus the new typed accessors; `run_migrations` ensures the tables exist. Existing `matches` data is untouched (verified by a migration test).

**Identification hook** (`tasks.rs`, the only change to the identify path): before `identify_torrent`, consult the authoritative map —
```rust
let metadata = match store.authoritative_meta(&info.hash).await {
    Some(m) => m,
    None => identify_torrent(&info, &tmdb_client).await,
};
```
So our adds are always correctly identified; filename guessing remains only for externally-added torrents.

## 6. Probe design

- **Source:** ranged CDN GETs reusing the `ProxiedMediaFile::fetch_cdn_range` pattern (`Range: bytes=…` header, expect 206, invalidate + re-resolve on 403/410/5xx). MKV: read incrementally from the front until the EBML `Tracks` element is parsed (normally within the first MB, before clusters). MP4/MOV/M4V: read the box table from the front; if `moov` is not at the front, range-fetch the tail to locate it.
- **Parse:** extract only what's needed — MKV `TrackEntry` → `TrackType` (audio=2, subtitle=17) + `Language` (and `LanguageBCP47` if present); MP4 `trak` → `hdlr` (handler type) + `mdhd` (packed ISO-639-2 language).
- **Verify** against `LangReq` (resolved from `QualityPrefs`): audio requirement is always enforced (`original` resolved per-title from TMDB `original_language`, mapped 639-1→639-2); subtitle enforced **independently** unless `none` (the subtitle track must be present even when the same language as the required audio). Missing required audio → `FailAudio`; missing required subtitle → `FailSubtitle`.
- **Failure taxonomy** → §2 S7 / §4 handling.
- **Language codes:** verification needs an ISO 639-1 ↔ 639-2 mapping (TMDB uses `en`; Matroska/MP4 use `eng`).
- **Known limitation (explicit):** the probe is header-only and acquisition-time; **body-level corruption** (valid header, garbage/truncated data) is not automatically detectable — WebDAV gives the proxy reads, not decode results, so there is no automatic playback-failure signal. Such files are caught only by manual flagging in the SP4 web UI.

## 7. Config (env, SP1)

Small surface; full per-weight + DB-backed config arrives in SP4.

| Var | Default | Meaning |
|-----|---------|---------|
| `SCRAPER_ADDON_URL` | unset → auto-templated | **Optional override** of the scraper base URL. If unset, the service auto-builds the Torrentio URL from the active provider + token (`<provider>=<token>` in Torrentio's options path — Torrentio supports both RD and TorBox). Set it to point at a self-hosted Torrentio or a different addon (Comet/MediaFusion), used verbatim. In SP1 nothing acquires unless `--acquire` is run, so this never triggers acquisition on its own; existing deployments are unaffected. |
| `MAX_RESOLUTION` | `1080` | Hard ceiling; set `2160` for 4K. |
| `AUDIO_LANGUAGE` | `original` | A language code or `original`. |
| `SUBTITLE_LANGUAGE` | `none` | A language code or `none` (skip the subtitle check). |
| `PREFER_HEVC` | `true` | Soft ranking nudge. |
| `PREFER_HDR` | `false` | Soft ranking nudge. |
| `STALL_TIMEOUT_SECS` | `1800` | No-progress window before a downloading owned torrent is declared stalled. |
| `MAX_ACQUIRE_ATTEMPTS` | `5` | Candidates tried per title before giving up. |

These extend the SP0 `Config` struct (`from_env`/`from_parts`).

## 8. Temporary `--acquire` trigger

`--acquire <movie|series> <imdb-or-tmdb-id> [season episode]` — mirrors the `--healthcheck` early-exit pattern in `main.rs`: resolves the missing id (IMDB↔TMDB via new `tmdb_client` lookups — TMDB `/find/{imdb}` and `/{type}/{id}/external_ids`), builds an `AcquireRequest` from `Config`'s prefs, constructs the engine, runs `acquire` once, prints the `AcquireOutcome`, and exits. **Marked as a temporary dev/verification scaffold** (comment + a one-off log line) to be removed when SP2's reconciler / SP4's ad-hoc add land. The `#[ignore]` lifecycle test calls the library API directly rather than shelling out.

## 9. Scoring (`release.rs`)

- **Hard exclusions:** resolution > `MAX_RESOLUTION`; blacklisted hash for this title.
- **Soft weighted score (hardcoded weights in SP1), cached-first:** `cached` (dominant) › resolution (up to ceiling) › codec (HEVC when `PREFER_HEVC`) › HDR (when `PREFER_HDR`) › **verifiable container** (MKV/MP4/MOV/M4V small bonus) › seeders (availability / tie-break, matters for uncached) › size (mild — penalise tiny re-encodes and oversized remuxes).
- **Layer-1 language filter:** down-rank releases tagged as foreign-only dubs (best-effort from title/audio tags) before the authoritative probe.

## 10. Error handling, ownership, idempotency

- Scraper/addon unreachable → `TemporarilyUnavailable`, nothing added, never crash.
- No candidates / all blacklisted / all above ceiling / **none confidently matching the title under strict validation** → `NoAcceptableRelease` (the title is left unfilled by design — correctness over fill — and logged for visibility; a cleaner-named release later can fill it).
- `add_magnet`/`select_files` 429 or RD "too many active torrents" → back off (the shared `AdaptiveRateLimiter`) + retry; leave for the next cycle.
- `delete_torrent` cleanup failure → log + continue (scan-loop dedup-by-hash tolerates a transient leak).
- **Ownership safety:** the engine only ever `delete_torrent`s a hash present in `owned_hashes`; never touches externally-added torrents.
- **Idempotency:** a hash already in `owned_hashes` is not re-added; `acquire` for a title already satisfied is a no-op.

## 11. Testing (TDD; extends the parent spec's §8 three-layer strategy)

### Deterministic (CI, no tokens)
- `release_parse_test` — table-driven over committed real Torrentio stream-title fixtures: resolution/codec/HDR/audio/cached/container/file-hint parsed; ceiling excludes; verifiable-container preference; layer-1 dub down-rank.
- `probe_fixture_test` — committed small MKV + MP4/MOV header fixtures → `Track{kind, language}`; MP4 `moov`-at-end handled; AVI/unknown → `Unsupported`→Inconclusive; a deliberately-truncated MKV → `Corrupt`; `verify` Pass/FailAudio/FailSubtitle/Inconclusive incl. the audio-`original` + subtitle-`none` rules and 639-1↔639-2 mapping.
- `reacquire_test` — `materialise` with `MockProvider`: cached vs not-cached outcomes; selector chooses the right file ids; file-match-by-path.
- `acquire_integration_test` — `MockScraper` + `MockProvider`: cached-first pick within ceiling; owned + authoritative recorded only after validation; wrong-title / probe-fail / corrupt / stall / dead → blacklist + delete + next; idempotent re-acquire (already-owned skipped); inconclusive → accept; all-attempts-exhausted → `NoAcceptableRelease`.
- `title_validation_test` — selected file identifying as a different title, or (series) wrong season/episode, or unidentifiable under strict mode → rejected with `WrongTitle` + next, and **no** `authoritative_ids` written; a confidently-matching id (+ correct S/E) → accepted. Reuses the real `identify_name` against fixture filenames.
- `repair_regression` — existing repair unit tests + `repair_integration_test` pass unchanged after `add_and_select_files` is reimplemented on `materialise`.
- `store_migration_test` — a v1 DB (matches only) opens, migrates to v2 (the three tables created), `matches` rows preserved; the new tables round-trip; auto-recovery still holds.
- `backoff_429_test` — simulated 429 / "too many active torrents" → back off + retry (no failed acquire).
- `identification_override_test` — a hash present in `authoritative_ids` makes the scan loop use that `MediaMetadata` and skip `identify_torrent`.

### Live smoke (`#[ignore]`, token-gated, self-cleaning)
- `scraper_live_test` (needs `SCRAPER_ADDON_URL`) — query a known IMDB id → non-empty, parseable streams with cache flags.
- `probe_live_test` (needs a provider token) — ranged-probe a real cached file's tracks.
- **`lifecycle_test` extension** (cross-provider RD + TorBox) — acquire *Sintel by IMDB id via Torrentio* → appears in the VFS → delete → disappears, touching only the service-owned torrent; skips per provider when its token is unset.

New `MockScraper` seam mirrors `MockProvider`; `Scraper` is `Arc<dyn Scraper>` wherever the engine depends on it.

## 12. Out of scope / deferred

- Trakt (SP2), upgrades (SP3), web UI + manual blacklist UI + DB-backed config (SP4).
- Per-episode-within-pack authoritative identity stays filename-derived (anchored to the known show), per the parent spec's episode caveat.
- Full scoring-weight configurability (SP4).
- acquire↔upgrade primitive consolidation (revisit in SP3).
- **(Deployment, later)** Add a self-hosted **Torrentio + gluetun (VPN)** section to `docker-compose.yml`, so the default stack can run a local scraper (debrid key never leaves the network) with scraping traffic routed through a VPN; `SCRAPER_ADDON_URL` then points at the local instance. Not required for SP1 — the override already supports it.

## 13. Open implementation details (resolve in the plan, not blocking)

- Exact Torrentio options-path syntax for the default template (the `<provider>=<token>` placement, default sort, and the provider→option-name map for RD/TorBox) — verify against the current `torrentio.strem.fun/configure` output; the live `scraper_live_test` guards against drift.
- Exact ranged-read chunk sizes for the probe (initial fetch size for MKV `Tracks`; tail fetch size for MP4 `moov`).
- Precise EBML vint / element-id decoding and ISO-BMFF 64-bit box-size handling (TDD against fixtures).
- The `Scraper`/`reacquire`/`acquire` module placement of the in-memory progress map used by `observe`.
- Whether `observe` is a new method invoked from the existing scan tick or a thin wrapper the scan loop calls (favour the latter — minimal scan-loop change).
