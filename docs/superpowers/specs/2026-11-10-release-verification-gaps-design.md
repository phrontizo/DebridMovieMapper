# Release verification gaps — design

**Date:** 2026-11-10 · **Status:** approved ("do all 5") · **Branch:** `v2`

## Problem (live evidence from a deployment's database)

**A `fi`-original film** is held as an SD `Film.2026.Multi.Vff.WEBRIP.mp4`-shaped release (French dub
+ a `fin`-tagged VO track, French subtitles only). TMDB says `original_language = fi`, but the film is
mostly English. With the default `AUDIO_LANGUAGE=original` every correctly `eng`-tagged release was
blacklisted `BadAudio`, at acquisition and again by the upgrade pass. Two French releases were
blacklisted `BadSubtitle` (a subtitle requirement is configured). The owned SD file — same group, same
track layout — was accepted only because it is a non-faststart MP4 (`ftyp`,`free`, a several-hundred-MB
`mdat`, then a `moov` of a few MB at EOF): the 4 MB suffix probe can't parse it → `TracksNotFound` →
Accept.

**An example show's S01E04** is served from a `Show.S01E01-E04.WEBRip.XviD.Rus`-shaped release (a
Russian-dub SD `.avi` pack) acquired on the episode's air date, while E01–E03 are 1080p ENG/ITA WEB-DLs.
The name's `Rus` and Torrentio's 🇷🇺 flag aren't recognised (no ranking penalty), `.avi` is
`Unsupported` → Accept (no language check), and the upgrade engine never revisits it: per-episode
upgrades only cover single-episode torrents, pack correction only over-ceiling packs, consolidation
only full-season packs.

## Design

### 1. Probe a trailing MP4 `moov` at its exact offset (`probe.rs`)
When the front 4 MB parses as MP4 with no `moov`, walk the front's top-level boxes; the first box whose
declared end overruns the front buffer (the `mdat`) gives the absolute offset of the next top-level box.
Fetch `[offset, offset + MAX_PROBE_FETCH)` (8 MB; a 206 shorter than requested is fine) and parse it
with `parse_mp4_tracks`. On success return those tracks. On any failure of the targeted read or parse
(moov > 8 MB, size-0 "to EOF" box, 416, garbage) fall back to the existing suffix attempt unchanged —
so the new step can never introduce an endless defer.

### 2. "Original" audio accepts TMDB's spoken languages
TMDB details (`movie`/`tv`) `spoken_languages[].iso_639_1` is captured alongside `original_language`.
`AcquireRequest` gains `spoken_languages: Option<Vec<String>>` (`None` = never fetched, i.e. a legacy
record). Under `AUDIO_LANGUAGE=original` the acceptable audio set is `original_language ∪
spoken_languages` for both ranking (`with_resolved_audio` → new `AudioReq::AnyOf`) and the probe
(`probe::verify`). An explicit `AUDIO_LANGUAGE` is unchanged. Trade-off (accepted): a dub in a
secondary spoken language passes too; ranking still puts the cleanly-tagged releases first.
Legacy owned records (`None`) are backfilled once from TMDB at the start of each upgrade visit
(best-effort: on failure leave `None`, retry next visit) and persisted on every record of the group.

### 3. Recognise language abbreviations and Torrentio flags in release names (`release.rs`)
Add separator-bounded tokens (`eng`, `rus`, `ita`, `ger`/`deu`, `fre`/`fra`/`vff`/`vfq`/`vfi`/
`truefrench`, `spa`/`esp`/`castellano`/`latino`, `por`/`dublado`, `pol`, `jpn`, `kor`, `hin`, `ukr`,
plus more full names) and map Torrentio's regional-indicator flag emoji (🇷🇺→`rus`, 🇬🇧→`eng`, …) to
ISO 639-2/B codes. Never add 2-letter tokens or subtitle-only markers (`vostfr`, `napisy`, `subs`).
`Multi Subs` (Torrentio's multi-subtitle marker) must not tag `mul` (multi-audio wildcard). Flags can
name subtitle languages (e.g. VOSTFR → 🇫🇷); the penalty is a soft ranking hit, not an exclusion, so a
mis-tagged release is still acquirable and the probe is the real gate.

### 4. Upgrade episodes served from multi-episode packs (`upgrade.rs`)
A new phase between the singleton pass (a) and over-ceiling pack correction (b): for every owned
within-ceiling record with `provides.len() > 1`, for each episode whose selection slot points at it,
run the same rank → stage (true singleton only) → idle-gated swap as phase (a), baselined on the pack's
quality. Sample-first cost bound: if the pack's first served episode has no improving candidate, skip
the rest of that pack this visit. The pack is pruned (via `apply_upgrade`'s supersede list) only on the
repoint that leaves it serving no slot AND every episode it provides is provided by another owned
record; otherwise it is kept. Over-ceiling packs stay with phase (b). Manual-provenance groups are
already skipped. Accepted consequence: a lower-quality full-season pack can be split into better
singles; consolidation later folds them into a pack when a same-or-better one exists.

### 5. Flag releases whose tracks couldn't be checked
`QualitySummary` gains `#[serde(default)] unverified: bool`.
- Set by `observe` from the verdict: `Accept` (Unsupported / TracksNotFound / no tracks) → `true`,
  `Pass` → `false`. Mirror records: `true` iff the representative file isn't `.mkv`/`.mp4` (mirror
  MKV/MP4 files were never probed but are assumed fine — flagging them would churn the whole library).
- Schema v8 migration backfills `true` on owned records whose selected file (any `selection` entry
  pointing at the hash) has a non-MKV/MP4 extension.
- `is_target_improvement`: an unverified cached `current` is also improved by a cached candidate with
  `effective_score >= current` at `resolution >= current` (no category jump needed). Converges: the
  replacement is verified, so the rule can't fire again.
- Every upgrade staging site (movie, per-episode, pack correction, consolidation) adopts only a
  `VerifyResult::Pass`; `Accept` → delete the staged torrent, `NoChange("unverifiable")`, no
  blacklist. `rank_changes` / pack ranking skip candidates whose file name is known and isn't
  MKV/MP4 (they can never pass). Effect: an upgrade never swaps a checked copy for an unchecked one.
- Acquisition policy is unchanged: when nothing verifiable exists an unverifiable release is still
  accepted (now flagged so the upgrade pass replaces it).

## Out of scope
Prioritising unverified records in the upgrade round-robin; pack-for-pack (non-ceiling) upgrades;
clearing existing blacklist rows (the untried releases suffice); changing acquisition's accept policy.
