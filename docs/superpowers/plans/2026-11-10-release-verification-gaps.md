# Release Verification Gaps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop wrong-language / low-quality releases slipping into the library (a `fi`-original film held as a French dub, an example show's S01E04 served from a Russian-dub `.avi` pack) and let the upgrade engine replace the ones already there.

**Architecture:** Five independent fixes across the probe (`probe.rs`), release parsing/scoring (`release.rs`, `config.rs`), TMDB metadata plumbing (`tmdb_client.rs`, `store.rs`, `tasks.rs`, `acquire.rs`) and the upgrade engine (`upgrade.rs`), each test-first and committed separately.

**Tech Stack:** Rust (tokio, reqwest, redb, serde_json, regex). Tests: `cargo test` (unit, in-module `#[cfg(test)]`).

**Spec:** `docs/superpowers/specs/2026-11-10-release-verification-gaps-design.md`

## Global Constraints

- TDD: every behaviour change starts with a failing test (CLAUDE.md).
- Lint gate before every commit: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`.
- Test gate before every commit (CLAUDE.md, run sequentially):
  `cargo test && INTEGRATION_TEST_LIMIT=10 cargo test --test integration_test -- --ignored && INTEGRATION_TEST_LIMIT=10 cargo test --test repair_integration_test -- --ignored && cargo test --test lifecycle_test -- --ignored`
- Update `CLAUDE.md` and `README.md` in the same commit as any behaviour they describe.
- Commits authored `Kiril <54142726+phrontizo@users.noreply.github.com>` (repo-local config already set), ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- No new dependencies. Do not bump `Cargo.lock`.
- Serde back-compat: every new persisted field is `#[serde(default)]`; legacy records must decode.
- Never add 2-letter language tokens or subtitle-only markers (`vostfr`, `napisy`, `subs`) as audio tags.

## Review Focus

- A trailing `moov` larger than 8 MB, a size-0 (`to EOF`) `mdat`, or a 416 on the targeted read must fall back to the old suffix path — never a new `Transient` loop. (Task 1 tests `..._falls_back_...`.)
- A title whose TMDB `spoken_languages` is empty/missing behaves exactly as before (original only); an explicit `AUDIO_LANGUAGE` ignores spoken languages entirely. (Task 3 tests.)
- Legacy on-disk `OwnedRecord`s (no `spoken_languages`, no `unverified`) decode; the spoken-language backfill runs at most once per group and a TMDB failure leaves the record untouched for the next visit. (Task 3, Task 4 tests.)
- A pack that still serves another episode, or whose other episodes aren't covered elsewhere, is never pruned — including when the swap defers mid-way. (Task 5 tests.)
- `Multi Subs` and subtitle-only markers must not suppress or invent an audio-language penalty beyond the documented soft flag behaviour. (Task 2 tests.)

---

### Task 1: Probe a trailing MP4 `moov` at its exact offset

**Files:**
- Modify: `src/probe.rs` (`probe_tracks` MP4 `TracksNotFound` arm ≈ line 845; new helper near `front_moov_extent` ≈ line 685; tests module)
- Docs: `CLAUDE.md` `probe.rs` row (replace the "suffix rarely starts on a box boundary" caveat), README probe notes if present.

**Interfaces:**
- Produces: `fn front_mp4_next_box_offset(buf: &[u8]) -> Option<u64>` — walks top-level boxes from 0 with `read_box_header`; returns the declared absolute end of the FIRST box whose end exceeds `buf.len()`; `None` if no box overruns, a header is split/malformed, a size is 0 (to-EOF), or a box is `moov` (the front-moov case is handled elsewhere).

- [ ] **Step 1: Write failing unit tests** in the probe tests module using the existing `mp4_box`/`mp4_with` fixtures:
  - `front_mp4_next_box_offset_reports_end_of_an_overrunning_mdat`: buffer = `ftyp`(32 bytes) + `free`(8) + an `mdat` header declaring size `1_000_000` followed by 16 payload bytes → `Some(40 + 1_000_000)`.
  - `front_mp4_next_box_offset_handles_64bit_largesize_mdat`: `mdat` with size field `1` and largesize `5_000_000_000` at offset 40 → `Some(40 + 5_000_000_000)`.
  - `front_mp4_next_box_offset_none_for_size_zero_or_no_overrun`: size-0 `mdat` → `None`; a buffer of complete boxes only → `None`.
- [ ] **Step 2: Write failing HTTP tests.** Add a test helper `async fn serve_ranges(file: Vec<u8>) -> String` (binds `127.0.0.1:0`, loops accepting connections, parses `Range: bytes=a-b` / `bytes=a-` / `bytes=-n`, replies `206` with `Content-Range` + exact `Content-Length`, `416` when `a >= len`; returns the base URL).
  - `probe_tracks_mp4_reads_trailing_moov_at_exact_offset`: file = `ftyp` + `free` + `mdat` of `5 * 1024 * 1024` zero bytes + `moov` taken from `mp4_with(&[(b"soun", packed("eng")), (b"subt", packed("ger"))])` (strip its `ftyp`). Assert `probe_tracks` returns tracks containing Audio `eng` and Subtitle `ger`.
  - `probe_tracks_mp4_trailing_moov_failure_falls_back_to_accept`: same layout but the `mdat` declares a size running past EOF (targeted read gets 416) and the tail is zero bytes → `Err(ProbeError::TracksNotFound)` (NOT `Transient`).
- [ ] **Step 3: Run** `cargo test --lib probe::` — expect the new tests to FAIL (helper undefined / trailing case returns `TracksNotFound`).
- [ ] **Step 4: Implement** `front_mp4_next_box_offset`, then in `probe_tracks`'s `Some(ContainerKind::Mp4)` → `Err(TracksNotFound)` arm: if `Some(off)`, `fetch_range(http, cdn_url, off, off + MAX_PROBE_FETCH as u64 - 1)`; on `Ok(buf)` and `Ok(tracks) = parse_mp4_tracks(&buf)` return `Ok(tracks)`. Any error from that fetch or parse falls through to the existing suffix code unchanged. Update the arm's comment and remove the `TODO(SP1+)`.
- [ ] **Step 5: Run** `cargo test --lib probe::` — all PASS.
- [ ] **Step 6:** Update `CLAUDE.md` probe row; run lint + test gates; commit `fix(probe): read a trailing MP4 moov at its exact offset`.

### Task 2: Recognise language abbreviations and Torrentio flags

**Files:**
- Modify: `src/release.rs` (`LANG_WORDS` ≈ line 298, `parse` language loop ≈ 258–276, tests)
- Docs: `CLAUDE.md` `release.rs` row (language tagging sentence).

**Interfaces:**
- Produces: `fn flag_languages(text: &str) -> Vec<&'static str>` — maps each regional-indicator pair in `text` to an ISO 639-2/B code via a `const FLAG_LANGS: &[(&str, &str)]` keyed by the 2-letter country code: GB/US→eng, RU→rus, FR→fre, IT→ita, ES/MX→spa, DE→ger, PT/BR→por, PL→pol, CZ→cze, SK→slo, NL→dut, JP→jpn, KR→kor, CN/TW→chi, IN→hin, UA→ukr, HU→hun, BG→bul, RS→srp, HR→hrv, DK→dan, SE→swe, NO→nor, FI→fin, TR→tur, IL→heb, GR→gre, RO→rum, LT→lit, LV→lav, EE→est, SI→slv, SA→ara, IR→per, VN→vie, ID→ind, MY→may, TH→tha. Unknown flags ignored.
- `LANG_WORDS` additions (word → code): eng→eng, rus→rus, ita→ita, ger→ger, deu→ger, fre→fre, fra→fre, vff→fre, vfq→fre, vfi→fre, truefrench→fre, spa→spa, esp→spa, castellano→spa, latino→spa, por→por, dublado→por, pol→pol, polish→pol, jpn→jpn, kor→kor, hin→hin, ukr→ukr, ukrainian→ukr, dutch→dut, czech→cze, hungarian→hun, finnish→fin, swedish→swe, danish→dan, norwegian→nor, turkish→tur, greek→gre, hebrew→heb, arabic→ara, chinese→chi, thai→tha, vietnamese→vie.
- `parse` keeps de-duplicating codes. `mul` is pushed only when a `MULTI_RE` match is NOT immediately followed by `sub` (Torrentio's `Multi Subs`).

- [ ] **Step 1: Write failing tests** (release tests module, `raw(name, desc, hash, file)` helper):
  - `parse_tags_language_abbreviations`: `Show.s01e04.WEBRip.XviD.Rus.RuDub.tv.avi` → languages contain `rus`; `Film.2026.MULTi.VFF.1080p.WEBRip` → contain `fre` and `mul`; `Film (2026) [Bluray 1080p][Esp]` → contain `spa`; `Danza.con.il.diavolo.ITA.ENG.1080p` → contain `ita` and `eng`.
  - `parse_maps_torrentio_flags_to_languages`: desc `"👤 9 💾 1.7 GB ⚙️ EXT\n🇷🇺"` → `rus`; desc `"…\n🇬🇧 / 🇮🇹"` → `eng` + `ita`.
  - `multi_subs_marker_is_not_multi_audio`: desc `"Multi Subs / 🇷🇺"` → no `mul`, has `rus`; desc `"Multi Audio / 🇬🇧 / 🇫🇷"` → has `mul`.
  - `subtitle_only_markers_do_not_tag_audio`: `Show S01E04 - Saison 1 VOSTFR HDTV 2026` (no flag) → languages empty.
  - `score_penalises_russian_tagged_release_for_english_original`: with `prefs().audio = AudioReq::Lang("en")`, `score(Show.S01E04.1080p.Rus.ColdFilm)` < `score(Show.S01E04.1080p.WEB)` by ≥ 50_000.
- [ ] **Step 2: Run** `cargo test --lib release::` — new tests FAIL.
- [ ] **Step 3: Implement** the table additions, `flag_languages` (decode pairs of chars in `U+1F1E6..=U+1F1FF` to `A`–`Z`), the `Multi Subs` exclusion (inspect the text right after each `MULTI_RE` match end), and fold flag codes into `languages` (de-duplicated). Existing tests (`multi_is_separator_bounded_not_a_substring`, `language_words_are_separator_bounded_not_substrings`, etc.) must stay green.
- [ ] **Step 4: Run** `cargo test --lib` — all PASS.
- [ ] **Step 5:** Update `CLAUDE.md`; lint + test gates; commit `fix(release): tag language abbreviations and Torrentio flags`.

### Task 3: "Original" audio accepts TMDB's spoken languages

**Files:**
- Modify: `src/tmdb_client.rs` (`parse_details` ≈ 405, `details` ≈ 191, tests ≈ 815)
- Modify: `src/store.rs` (`AcquireRequest` ≈ 104)
- Modify: `src/config.rs` (`AudioReq` ≈ 73, `with_resolved_audio` ≈ 153, tests ≈ 1110)
- Modify: `src/release.rs` (`score` audio block ≈ 391)
- Modify: `src/probe.rs` (`LangReq` ≈ 46, `check_lang`/`verify` ≈ 300–345, tests)
- Modify: `src/acquire.rs` (ranking ≈ 425, `verify_file` LangReq ≈ 634)
- Modify: `src/tasks.rs` (`build_acquire_request` ≈ 1576)
- Modify: `src/upgrade.rs` (`run_upgrade_once` ≈ 32–80)
- Modify: every `AcquireRequest {` / `LangReq {` literal (≈41 / 11, mostly tests) — add the new field.
- Docs: `CLAUDE.md` + `README.md` `AUDIO_LANGUAGE` description ("original = TMDB original language plus its listed spoken languages").

**Interfaces:**
- Produces: `pub struct TitleDetails { pub title: String, pub year: Option<String>, pub original_language: Option<String>, pub spoken_languages: Vec<String> }`; `parse_details(v, kind) -> TitleDetails`; `TmdbClient::details(..) -> Result<TitleDetails, reqwest::Error>` (`spoken_languages[].iso_639_1`, lowercased, non-empty).
- Produces: `AcquireRequest.spoken_languages: Option<Vec<String>>` with `#[serde(default)]` (`None` = never fetched / legacy).
- Produces: `AudioReq::AnyOf(Vec<String>)` (never produced by `AudioReq::parse`); `QualityPrefs::with_resolved_audio(&self, original: Option<&str>, spoken: &[String]) -> QualityPrefs` — under `Original`, the de-duplicated lowercased set `original ∪ spoken`: empty → unchanged `Original`, one entry → `Lang(x)`, more → `AnyOf(set)` with original first. Explicit `Lang`/`AnyOf` unchanged.
- Produces: `probe::LangReq.spoken_languages: Vec<String>`; `verify` wants = `Lang(l)` → `[l]`, `AnyOf(v)` → `v`, `Original` → `original ∪ spoken`; `check_lang(tracks, kind, wants: &[String])` Pass if any track matches any want (via `lang_eq`).
- Produces (upgrade.rs): `async fn backfill_spoken_languages(app: &AppState, tmdb_id: u64, media_type: &MediaType, hashes: &[String], rec: &mut OwnedRecord)` — no-op unless `rec.request.spoken_languages.is_none()`; on `app.tmdb_client.details` Ok, sets `Some(list)` on `rec` and read-modify-writes every record in `hashes` via `get_owned`/`put_owned`; on Err logs at debug and leaves everything `None`. Called in `run_upgrade_once` for each title right before dispatch.

- [ ] **Step 1: Write failing tests:**
  - tmdb: `parse_details_captures_spoken_languages` (`{"title":"Film","release_date":"2026-07-01","original_language":"fi","spoken_languages":[{"iso_639_1":"en"},{"iso_639_1":"fi"},{"iso_639_1":"es"}]}` → `spoken_languages == ["en","fi","es"]`); missing key → empty vec. Adjust the two existing `parse_details` tests to the struct.
  - store: `acquire_request_without_spoken_languages_decodes_as_none` (legacy JSON of an `OwnedRecord` → `request.spoken_languages == None`).
  - config: `with_resolved_audio_unions_original_and_spoken` (`(Some("fi"), ["en","fi","es"])` → `AnyOf(["fi","en","es"])`); existing single-language assertions keep `Lang(..)` with `&[]`; explicit `Lang("de")` ignores spoken.
  - probe: `verify_original_accepts_a_spoken_language` (eng audio, original `fi`, spoken `[en,fi,es]` → `Pass`); `verify_original_without_spoken_still_requires_original` (eng audio, original `fi`, spoken `[]` → `FailAudio`); `verify_explicit_audio_ignores_spoken` (`Lang("de")`, spoken `[en]`, eng audio → `FailAudio`).
  - release: `score_any_of_does_not_penalise_a_spoken_language` (`AnyOf(["fi","en"])`, release tagged `English` → no 50_000 penalty; tagged `German` → penalised).
  - acquire: `observe_verifies_spoken_language_audio_under_original` — mirror the existing observe-probe tests (≈ line 3260): `CannedProber` returning eng audio, request `original_language: Some("fi")`, `spoken_languages: Some(vec!["en","fi"])`, prefs `Original` → record `Verified`, hash NOT blacklisted.
- [ ] **Step 2: Run** `cargo test --lib` — new tests FAIL / don't compile.
- [ ] **Step 3: Implement** in the order tmdb → store → config → release → probe → acquire → tasks (`build_acquire_request` fills `spoken_languages: Some(details.spoken_languages)`) → upgrade (`backfill_spoken_languages`). Fix every struct literal (tests: `spoken_languages: Some(vec![])` so no test reaches the TMDB backfill).
- [ ] **Step 4: Run** `cargo test` — all PASS.
- [ ] **Step 5:** Docs; lint + test gates; commit `fix(lang): accept TMDB spoken languages for AUDIO_LANGUAGE=original`.

### Task 4: Flag releases whose tracks couldn't be checked; upgrades adopt only checked releases

**Files:**
- Modify: `src/release.rs` (`QualitySummary` ≈ 424 + `of`; `Container::from_name` → `pub`; `is_target_improvement` ≈ 472)
- Modify: `src/acquire.rs` (`record_verified` ≈ 668 gains the verdict; `observe` call ≈ 1095)
- Modify: `src/tasks.rs` (`mirror_quality` ≈ 1169)
- Modify: `src/store.rs` (`SCHEMA_VERSION` 7→8, `run_migrations` step, doc comment)
- Modify: `src/upgrade.rs` (4 probe sites ≈ 707/1054/1206/1788; `rank_changes` ≈ 352; `rank_cached_pack_candidates` ≈ 891; staged `QualitySummary::of(..)` writes; test `app_with` default prober ≈ 2064)
- Modify: every `QualitySummary {` literal (≈61) — add `unverified: false`.
- Docs: `CLAUDE.md` (acquire/observe, upgrade, store schema v8 + migration bullet, account-mirror note), `README.md` upgrade section.

**Interfaces:**
- Produces: `QualitySummary.unverified: bool` (`#[serde(default)]`); `QualitySummary::of` always sets `false`.
- Produces: `pub fn Container::from_name(name: &str) -> Container`.
- Produces: `record_verified(&self, hash, req, info, selected_path, unverified: bool)` — sets `rec.quality.unverified = unverified` in the same read-modify-write; `observe` passes `true` for `VerifyResult::Accept`, `false` for `Pass`.
- Produces: `mirror_quality` sets `unverified = !Container::from_name(&name).is_verifiable()`.
- Produces: migration v7→v8 (`from_version < 8`, not for a fresh DB): for every owned record with `quality: Some`, if ANY `selection` entry pointing at its hash has a `file_path` whose `Container::from_name` isn't verifiable, set `quality.unverified = true`. One write transaction.
- Produces: `is_target_improvement` rule, placed after the two `cached` checks: `if current.unverified && cand_eff >= cur_eff && candidate.resolution >= current.resolution { return Improvement::Upgrade }`.
- Produces: upgrade staging — `VerifyResult::Accept` → delete the staged torrent, return `UpgradeSkip::NoChange("unverifiable release".into())`, no blacklist (all four sites). `rank_changes` and `rank_cached_pack_candidates` skip a candidate when `file_name.is_some() && !container.is_verifiable()`.
- Test infra: `app_with`'s default prober becomes `CannedProber(Ok(vec![Track { kind: TrackKind::Audio, language: None }]))` (verifies `Pass` for any requirement). Tests that need `Accept` use `app_with_prober(.., Err(ProbeError::Unsupported))`.

- [ ] **Step 1: Write failing tests:**
  - release: `unverified_current_is_improved_by_equal_score_candidate` (current cached, unverified, 1080p WEB score S; candidate cached 1080p WEB score S → `Upgrade`); `verified_current_equal_score_is_still_none`; `unverified_current_never_trades_resolution` (candidate lower res → not `Upgrade` via the new rule; over-ceiling downgrade semantics unchanged); `legacy_quality_summary_decodes_as_verified` (JSON without `unverified` → `false`).
  - acquire: `observe_flags_unverifiable_accept` (prober `Err(Unsupported)` → Verified record with `quality.unverified == true`); `observe_pass_is_not_flagged`.
  - tasks: extend `mirror_quality_requires_known_resolution_and_source`: `Movie.2020.1080p.BluRay.x264.avi` → `unverified == true`; `.mkv` → `false`.
  - store: `migrates_v7_db_flags_unverifiable_selected_files` — seed a v7 DB (follow `migrates_v6_db_clears_legacy_blacklist_but_keeps_owned`) with owned `a` (quality Some) selected at `x/Show.s01e04.avi` and owned `b` selected at `y/Movie.mkv`; after open: `a.quality.unverified == true`, `b == false`; owned/selection rows otherwise intact.
  - upgrade: `movie_upgrade_rejects_unverifiable_staged_candidate_without_blacklist` (prober `Unsupported`, a strictly better cached candidate → no swap, staged torrent deleted, hash not blacklisted); `movie_upgrade_replaces_unverified_copy_with_equal_score_verified` (owned unverified, candidate same score/tier/res, Pass prober → swapped + old pruned); `rank_changes_skips_known_unverifiable_containers` (candidate with `file_name` `X.avi` absent from the ranked list; candidate with no file name kept).
- [ ] **Step 2: Run** `cargo test --lib` — new tests FAIL.
- [ ] **Step 3: Implement** in the order release → acquire → tasks → store → upgrade; update every literal; switch the default test prober.
- [ ] **Step 4: Run** `cargo test` — all PASS (existing upgrade tests now run against a passing prober).
- [ ] **Step 5:** Docs; lint + test gates; commit `feat(upgrade): flag unverifiable releases and only adopt checked upgrades`.

### Task 5: Upgrade episodes served from multi-episode packs

**Files:**
- Modify: `src/upgrade.rs` (`upgrade_show_episodes_and_packs` ≈ 1305–1462: new phase between (a) and (b); new pure helper; tests)
- Docs: `CLAUDE.md` `upgrade.rs` row + SP3 bullet ("three layered stages" → four), `README.md` upgrade section.

**Interfaces:**
- Consumes: `rank_changes`, `stage_episode_candidate`, `apply_upgrade`, `StagedUpgrade`, `crate::store::episode_slot`, `downgrade_allowed_or_log` (all existing).
- Produces: `fn pack_prunable_after_repoint(pack_hash: &str, pack_provides: &[(u32, u32)], repointed: (u32, u32), served_by: &HashMap<(u32, u32), String>, other_provides: &[(u32, u32)]) -> bool` — pure. `served_by` maps each of the pack's episodes to the hash its selection slot currently points at. True iff, treating `repointed` as no longer served by the pack, no episode in `pack_provides` is served by `pack_hash`, AND every episode in `pack_provides` other than `repointed` appears in `other_provides` (the union of `provides` of every OTHER owned record in the group, re-read from the store just before the swap).
- Produces: phase (a2) loop — for each `(hash, rec)` with `rec.provides.len() > 1`, `rec.quality` `Some(q)`, `q.resolution <= ceiling`: collect served episodes (selection slot hash == `hash`, sorted). For each served `(s, e)` (index `i`): scrape + `rank_changes(.., &q, group_hashes)`; empty ranked list → `break` if `i == 0` (sample-first) else `continue`; otherwise stage candidates exactly like phase (a) (same idle checks, defer returns `true`, `NoChange` → next candidate, `Deferred` → next episode), passing `supersede = [hash]` when `pack_prunable_after_repoint(..)` else `[]`. After a successful repoint, update the local `served_by` map.

- [ ] **Step 1: Write failing unit tests for the helper:**
  - `pack_prunable_when_last_served_episode_moves_and_rest_covered` (pack E1–E4, served_by {E1:s1, E2:s2, E3:s3, E4:pack}, repointed E4, other_provides [E1,E2,E3,E4] → `true`).
  - `pack_kept_while_it_still_serves_another_episode` (served_by E3:pack too → `false`).
  - `pack_kept_when_an_unserved_episode_is_not_covered_elsewhere` (other_provides lacks E2 → `false`).
- [ ] **Step 2: Write failing integration-style tests** following the existing show upgrade tests (`MockProvider` + scripted scraper + `app_with`):
  - `pack_served_episode_is_upgraded_and_superseded_pack_pruned` — the example show's shape: singletons E1–E3 (1080p WEB, Verified, slots → singletons), pack P providing E1–E4 (resolution 0, WEB tier, `unverified: true`), slot E4 → P; scraper offers a cached 1080p single for E4 → slot E4 repointed to the new hash, P deleted from the provider and its owned record removed, E1–E3 untouched.
  - `pack_served_episode_upgrade_keeps_pack_serving_other_episodes` — pack serves E3 and E4, only E4 has a better single → E4 repointed, pack kept.
  - `pack_sample_first_skips_remaining_episodes_when_nothing_better` — pack serves E1–E3, scraper returns no improving candidate for E1 → exactly one scrape issued for that pack (count scraper calls).
  - `over_ceiling_pack_is_left_to_pack_correction` — pack with `resolution > ceiling` → phase (a2) issues no scrape for it.
- [ ] **Step 3: Run** `cargo test --lib upgrade::` — new tests FAIL.
- [ ] **Step 4: Implement** the helper and phase (a2) (extract the shared per-episode staging loop body from phase (a) into a private async fn if it avoids duplicating ~70 lines; keep behaviour of phase (a) identical — its existing tests must stay green).
- [ ] **Step 5: Run** `cargo test` — all PASS.
- [ ] **Step 6:** Docs; lint + test gates; commit `feat(upgrade): upgrade episodes served from multi-episode packs`.

### Task 6: Final verification and hand-off

- [ ] **Step 1:** `cargo fmt --check && cargo clippy --all-targets -- -D warnings` — clean.
- [ ] **Step 2:** Full test gate (Global Constraints) — all pass; record counts.
- [ ] **Step 3:** Confirm `CLAUDE.md`/`README.md` describe all five behaviours (grep for `spoken`, `unverified`, `schema v8`, `pack-served`/`multi-episode pack`, `flag`).
- [ ] **Step 4:** Ask the user before `git push origin v2` (CI publishes `:v2edge`) and before redeploying the service (interrupts playback). After a redeploy, watch the service logs for the v8 migration and the next upgrade tick.
