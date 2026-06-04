# TorBox Support via Provider Abstraction

- **Created:** 2026-06-03
- **Status:** Phase 1 complete (merged to `main`). Phases 2–5 revised — see below.

## Revision history

- **2026-06-03 — v1.** Initial design: provider abstraction + a **durable hash-keyed
  catalogue** decoupled from the provider's live list (with a redb migration) +
  on-demand re-acquire + a TorBox client. Premise: TorBox *removes* purged torrents
  from `mylist` after 30 days, so a list-driven design would make films vanish.
- **2026-06-04 — v2 (this document).** Phase 1 (provider abstraction) implemented and
  merged. **Live TorBox API verification disproved the v1 premise:** TorBox does *not*
  remove purged torrents from `mylist` — it keeps the entry listed and flips it to an
  *uncached/expired* state (`cached:false`, `download_state` changes; an `expires_at`
  field exists). This mirrors Real-Debrid, which keeps a torrent in its list even when
  its links break (503). Consequently:
  - The **durable hash-keyed catalogue and its redb migration are dropped** — they were
    unnecessary, and a never-remove catalogue would have broken the user's frequent
    *delete-a-bad-torrent-and-add-a-different-one* workflow (the deleted torrent would
    linger and, for a movie, the largest copy wins — possibly the dead one).
  - The library stays **list-driven** (as today). Durability for "watch it a year later"
    comes from the provider *retaining the entry* plus **on-demand re-acquire/re-cache on
    playback**.
  - Remaining phases reorganised (trait reshape → re-acquire → TorBox client → docs).

## Summary

Add TorBox as an alternative debrid provider alongside Real-Debrid (RD). Exactly one
provider is active per deployment, selected at startup by which API token is set
(**done in Phase 1**). The rest of the system depends on a `DebridProvider` trait
(**done in Phase 1**). The remaining work reshapes that trait to a provider-neutral
file-resolution model, generalises the existing repair machinery into on-demand
re-acquire, and adds the TorBox implementation.

Full TorBox parity with RD is the goal — including on-demand re-acquisition so an
owned-but-uncached film plays after a brief re-cache.

## Background & motivation

DebridMovieMapper bridges a debrid account to media servers (Jellyfin/Plex) over
WebDAV. After Phase 1, all components depend on `Arc<dyn DebridProvider>`; `RealDebridClient`
is the sole implementation and is selected at startup.

Two facts drive the remaining work:

1. **TorBox's resolution model differs from RD's.** TorBox lists torrents with their
   files inline (`/torrents/mylist`) and resolves a streamable URL by addressing a file
   as `(torrent_id, file_id)` via `/torrents/requestdl` — there is **no two-step
   "unrestrict"** of a per-file restricted link. File ids are arbitrary integers (not a
   positional index). It also exposes a real cached-availability endpoint
   (`/torrents/checkcached`).

2. **Both providers retain entries until the user deletes them (corrected).** RD keeps a
   torrent in its list even when its links break (503 on `unrestrict`). TorBox keeps a
   torrent in `mylist` even after its cache expires — the entry persists with
   `cached:false`/`download_present:false` and an updated `download_state`. So for both
   providers, *absent from the live list ⇒ the user deleted it*. The current list-driven
   design therefore already gives durability and correctly handles deliberate
   delete-and-replace. What's missing for TorBox is (a) showing owned-but-uncached items
   and (b) re-caching them on playback.

## Requirements & decisions

1. **One provider active per deployment** — `RD_API_TOKEN` → RD; `TORBOX_API_KEY` → TorBox;
   both set → startup error; neither → error. *(Done in Phase 1.)*
2. **Provider abstraction** via a `DebridProvider` trait; one implementation selected at
   startup, passed everywhere as `Arc<dyn DebridProvider>`. *(Done in Phase 1.)*
3. **List-driven library (unchanged).** The VFS is built from the provider's current list
   plus cached TMDB identifications (redb, keyed by torrent id). Deleting a torrent on the
   provider removes it from the library on the next scan; this preserves the user's
   delete-a-bad-torrent-and-add-a-different-one workflow. **No durable catalogue, no redb
   migration.**
4. **Show owned-but-uncached items.** For TorBox, `list_torrents` returns downloaded *and*
   expired/uncached-but-owned torrents, so a film the user owns still appears in Jellyfin
   even when its cache has lapsed.
5. **On-demand re-acquire/re-cache** is the durability mechanism. Playing an item whose
   bytes aren't currently available (RD 503 / TorBox uncached or `requestdl` failure)
   triggers a re-acquire: re-add by stored hash, `check_cached`, then resolve. This makes
   "watch it a year later" work.
6. **Full TorBox parity**, including re-acquisition.

### Known limitation (accepted)

If a magnet has **no seeders** when the film is finally selected, no system can produce the
bytes — the entry remains in the library but cannot play until seeders return. For popular
content (usually in TorBox's global cache) re-acquire is near-instant.

## Verified TorBox API facts (live, 2026-06-04)

Captured by adding/removing a Creative-Commons test torrent (Sintel) against a real account.

- **Base / auth:** `https://api.torbox.app/v1/api`, header `Authorization: Bearer <API_KEY>`.
- **`POST /torrents/createtorrent`** (multipart form, field `magnet`) →
  `{ success, detail, data: { hash, torrent_id: <int>, auth_id } }`. A globally-cached magnet
  returns instantly (`detail: "Found Cached Torrent. Using Cached Torrent."`). Rate limit
  ~60/hour on creation endpoints.
- **`GET /torrents/mylist?bypass_cache=true`** → `{ success, detail, data: [ Torrent ] }`.
  Torrent fields include: `id` (int), `hash`, `name`, `magnet`, `size`, `files` (inline),
  `cached` (bool), `download_present` (bool), `download_finished` (bool),
  `download_state` (str, e.g. `"cached"`), `active` (bool — **means "currently transferring",
  NOT availability**; a complete cached torrent is `active:false`), `expires_at`, `progress`,
  `availability`, `download_path`, timestamps, etc.
  File fields: `id` (int, **arbitrary, not path-ordered** — e.g. Sintel's `.mp4` is `id:10`),
  `name` (full path, e.g. `"Sintel/Sintel.mp4"`), `short_name`, `size`, `mimetype`, `s3_path`,
  `absolute_path`, `md5`, `infected`, `zipped`, `opensubtitles_hash`.
- **`GET /torrents/checkcached?hash=<h>&format=object&list_files=true`** →
  `{ data: { "<hash>": { name, size, hash, files: [ { id, name, short_name, size, mimetype } ] } } }`
  (empty object/list if not cached).
- **`GET /torrents/requestdl?token=<key>&torrent_id=<int>&file_id=<int>`** →
  `{ success, detail, data: "<direct CDN URL string>" }`. URL valid ~3 hours. (`&redirect=true`
  yields a 302 permalink instead.)
- **`POST /torrents/controltorrent`** (JSON body `{ "torrent_id": <int>, "operation": "delete" }`) →
  `{ success, detail }`. Operations also include reannounce/pause/resume.

**Availability mapping:** a TorBox torrent is "in the library and playable now" when
`download_present:true`/`cached:true`; "owned but needs re-cache" when those are false (expired);
RD's equivalent of the latter is a 503 on `unrestrict`.

## Architecture (remaining work)

### 1. Canonical model — `src/provider.rs` (extend)

```rust
pub struct TorrentInfo {
    pub id: String,
    pub hash: String,
    pub filename: String,
    pub bytes: u64,
    pub status: String,        // normalized: "downloaded" (playable) or "uncached" (owned, needs re-cache)
    pub added: String,
    pub ended: Option<String>,
    pub files: Vec<TorrentFile>,
}

pub struct TorrentFile {
    pub id: u32,               // provider file id (RD: index/id; TorBox: arbitrary id)
    pub path: String,
    pub bytes: u64,
    pub selected: bool,
    pub link: Option<String>,  // RD restricted link; None for TorBox
}

/// What the VFS stores for a media file. Stable identity = (hash, file_path);
/// torrent_id / file_id / link are re-derived on re-acquire.
pub struct FileLocator {
    pub hash: String,
    pub torrent_id: String,
    pub file_id: u32,
    pub file_path: String,
    pub link: Option<String>,
}
```

### 2. Reshaped `DebridProvider` trait

The Phase 1 trait mirrors RD's methods (`unrestrict_link`, etc.). Reshape it to be
provider-neutral:

```rust
#[async_trait::async_trait]
pub trait DebridProvider: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &'static str;
    /// All library-relevant torrents WITH files (RD: list + per-torrent info, links paired
    /// into files[].link; TorBox: mylist incl. uncached, files[].link = None).
    async fn list_torrents(&self) -> Result<Vec<TorrentInfo>, AppError>;
    /// Resolve a file to a streamable CDN URL (RD: unrestrict link; TorBox: requestdl).
    /// Returns a recoverable "needs re-acquire" error on 503 / uncached.
    async fn resolve_url(&self, loc: &FileLocator) -> Result<String, AppError>;
    async fn check_cached(&self, hash: &str) -> Result<bool, AppError>;
    /// Re-add by hash (RD: addMagnet + selectFiles; TorBox: createtorrent), returns fresh info.
    async fn add_by_hash(&self, hash: &str) -> Result<TorrentInfo, AppError>;
    async fn delete_torrent(&self, id: &str) -> Result<(), AppError>;
    async fn invalidate(&self, loc: &FileLocator);
}
```

`resolve_url` distinguishes a **recoverable** failure (RD 503, TorBox uncached/`requestdl`
failure) from other errors via a dedicated `AppError` variant, so the re-acquire
orchestration can trigger.

### 3. Persistence (unchanged) & VFS

Keep the existing list-driven scan loop and redb cache (TMDB identifications keyed by torrent
id). The only VFS change: the `MediaFile` node stores a `FileLocator` instead of
`(rd_link, rd_torrent_id)`, so `resolve_url` works for both providers and re-acquire can
re-derive the locator. `vfs.rs::build` continues to take `Vec<(TorrentInfo, MediaMetadata)>`.

### 4. Generalised re-acquire — refactor `src/repair.rs`

On a `resolve_url` recoverable failure: `invalidate` → `add_by_hash(hash)` →
`check_cached`/status → match file by `file_path` → fresh `FileLocator` → `resolve_url` →
update the VFS node → optionally delete the stale provider torrent. The existing health state
machine (cooldown, max attempts) is preserved but calls the trait. RD behaviour is unchanged;
TorBox uses `check_cached`.

### 5. TorBox client — `src/torbox_client.rs` (new)

Implements the trait against the verified API (see the API facts above). `list_torrents`
includes uncached/expired owned torrents (mapping `download_present`/`cached` →
`status`). Reuses the adaptive rate-limiter pattern (creation endpoints ~60/h). Resolution
cache TTL ~3h.

## Implementation phasing

Phase 1 is complete and merged. Remaining phases each land test-green with no regression to RD.

1. ~~**Provider abstraction + selection.**~~ **Done** (merged to `main`).
2. **Trait reshape + `FileLocator`.** Introduce the canonical `FileLocator`/per-file `link`;
   reshape the trait to `list_torrents`/`resolve_url`/`check_cached`/`add_by_hash`/`invalidate`;
   RD implements them; the VFS `MediaFile` node stores a `FileLocator`; `dav_fs` resolves via
   `resolve_url`. List-driven persistence unchanged. **No durable catalogue, no migration.**
3. **Generalised re-acquire.** Refactor `repair.rs` to the reshaped trait.
4. **TorBox client.** Implement `DebridProvider` for TorBox; include uncached/expired items in
   the library; unit + integration tests.
5. **Docs.** Update `CLAUDE.md` and `README.md`; TorBox integration tests in the pre-commit gate.

## Risks

- **Trait reshape touches `vfs.rs` (node shape) and `dav_fs.rs` (resolution + repair trigger).**
  Mitigated by phasing and the existing test suite; RD resolve path stays behaviourally identical.
- **TorBox status mapping** (which `download_state`/`cached` combinations count as "owned but
  uncached" vs "still downloading" vs "failed") — pin down during Phase 4 against live responses;
  the verified field list above is the reference.
