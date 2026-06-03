# TorBox Support via Provider Abstraction + Durable Catalogue

- **Date:** 2026-06-03
- **Status:** Approved (design)
- **Author:** Kiril Dunn (with Claude Code)

## Summary

Add TorBox as an alternative debrid provider alongside Real-Debrid (RD). Exactly
one provider is active per deployment, selected at startup by which API token is
set. Introduce a `DebridProvider` trait so the rest of the system is
provider-agnostic, and make the media library **durable**: once a film is
identified it remains in the library indefinitely and is re-acquired from the
provider on demand if its cached copy has been purged.

Full TorBox parity with RD is the goal — including on-demand re-acquisition.

## Background & Motivation

DebridMovieMapper currently bridges Real-Debrid to media servers (Jellyfin/Plex)
over WebDAV. `RealDebridClient` is used **concretely** throughout — `main.rs`,
`tasks.rs`, `repair.rs`, and `dav_fs.rs` all hold `Arc<RealDebridClient>`; there
is no provider abstraction.

Two facts drive this design:

1. **TorBox's API model differs from RD's.** TorBox lists torrents with their
   files inline (`/torrents/mylist`), and resolves a streamable URL by
   addressing a file as `(torrent_id, file_id)` via `/torrents/requestdl` — there
   is **no two-step "unrestrict"** of a restricted link string. It also exposes a
   real cached-availability endpoint (`/torrents/checkcached`).

2. **RD gives library durability by accident; TorBox does not.** RD keeps
   torrents in the account list even when their links break, so a
   scan-loop-mirrors-provider design never loses library entries. TorBox
   **removes** purged items from `mylist` after 30 days of inactivity (any access
   resets the 30-day timer). A naive port would make films silently disappear
   from the library over time. The requirement is that a film, once shown in
   Jellyfin, remains playable a year later.

## Requirements & Decisions

Captured from the brainstorming session:

1. **One provider active per deployment.** `RD_API_TOKEN` → Real-Debrid;
   `TORBOX_API_KEY` → TorBox.
   - Both tokens set → **hard error at startup**, refuse to run.
   - Neither set → error (as today).
   - Exactly one set → run that provider.
   - Consequence: no cross-provider deduplication is needed (only one provider
     ever runs).
2. **Provider abstraction** via a `DebridProvider` trait; a single implementation
   is selected at startup and passed everywhere as `Arc<dyn DebridProvider>`.
   Chosen over enum dispatch primarily because it enables a `MockProvider` for
   the repo's mandated TDD, and gives cleaner module isolation. Adds the
   `async_trait` crate.
3. **Durable library.** The VFS is built from a **persistent catalogue** of
   everything ever identified, *not* from the provider's live torrent list. The
   library never shrinks automatically.
4. **On-demand re-acquisition.** When a selected file's torrent is no longer
   available/cached on the provider, re-add it by stored hash, re-cache/download,
   and serve. This generalises the existing repair machinery across both
   providers.
5. **Full TorBox parity**, including re-acquisition (not a deferred/no-op).

### Known limitation (accepted)

If a magnet has **no seeders** when the film is finally selected, no system can
produce the bytes — the catalogue entry remains but cannot play until seeders
return. For popular content (usually in TorBox's global cache) re-acquire is
near-instant.

## Architecture

### 1. Canonical model — `src/provider.rs` (new)

Provider-neutral types that both clients map into. The trait methods return
these, never RD- or TorBox-specific shapes.

```rust
pub struct TorrentInfo {
    pub id: String,
    pub hash: String,
    pub filename: String,
    pub bytes: u64,
    pub status: String,
    pub added: String,
    pub ended: Option<String>,
    pub files: Vec<TorrentFile>,
}

pub struct TorrentFile {
    pub id: u32,                 // provider file id / index
    pub path: String,
    pub bytes: u64,
    pub selected: bool,
    pub link: Option<String>,    // RD restricted link; None for TorBox
}

/// What the VFS stores for a media file and what the catalogue persists.
/// Stable identity = (hash, file_path). torrent_id / file_id / link are
/// mutable and re-derived on re-acquire.
pub struct FileLocator {
    pub hash: String,
    pub torrent_id: String,
    pub file_id: u32,
    pub file_path: String,
    pub link: Option<String>,
}
```

### 2. The `DebridProvider` trait — `src/provider.rs`

```rust
#[async_trait]
pub trait DebridProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn list_torrents(&self) -> Result<Vec<TorrentInfo>, AppError>;
    async fn resolve_url(&self, loc: &FileLocator) -> Result<String, AppError>;
    async fn check_cached(&self, hash: &str) -> Result<bool, AppError>;
    async fn add_by_hash(&self, hash: &str) -> Result<TorrentInfo, AppError>;
    async fn delete_torrent(&self, id: &str) -> Result<(), AppError>;
    async fn invalidate(&self, loc: &FileLocator);
}
```

- `rd_client.rs` and the new `torbox_client.rs` each implement the trait.
- `DavFs`, `RepairManager`, and `ScanConfig` swap their `Arc<RealDebridClient>`
  field for `Arc<dyn DebridProvider>`.
- A `MockProvider` (test-only) implements the trait for unit tests of scan / VFS
  / re-acquire / dav_fs logic without network access.

`resolve_url` must distinguish a **transient/recoverable failure** (signal
"needs re-acquire": RD 503 on unrestrict, TorBox not-cached / 404) from other
errors. This is surfaced via a dedicated `AppError` variant so the re-acquire
orchestration can trigger.

### 3. Durable catalogue (redb)

New table keyed by **hash**:

```text
catalogue: hash -> CatalogueEntry {
    torrent_snapshot,     // file structure: paths, sizes, file ids (at identification)
    media_metadata,       // TMDB identification (title, year, type, external_id)
    current_torrent_id,   // mutable: provider's current torrent id for this hash
}
```

- **The VFS is rebuilt from the catalogue (all entries) each scan**, not from the
  live list.
- Scan loop polls the live list and **upserts**: a hash not in the catalogue is
  identified via TMDB and inserted; an existing hash refreshes
  `current_torrent_id` and per-file links/ids.
- **Entries absent from the live list are retained.** This is the durability
  guarantee. On RD the catalogue ≈ the live list (no behaviour change); on TorBox
  the library survives the 30-day purge.
- **Migration:** the current store is keyed by `torrent_id` holding
  `(TorrentInfo, MediaMetadata)`. Migrate to the hash-keyed catalogue (TorrentInfo
  already carries `hash`). Approach: introduce the new table and backfill from the
  old one on first run, then read exclusively from the new table. The migration
  is the riskiest piece of the change and gets dedicated tests.

### 4. Generalised re-acquire — refactor `src/repair.rs`

On a resolve failure that signals unavailability:

1. `invalidate(loc)` — drop the cached resolution.
2. `add_by_hash(hash)` — RD: `addMagnet` + `selectFiles`; TorBox: `createtorrent`.
3. `check_cached(hash)` / inspect status for "downloaded".
4. Match the file by `file_path` in the returned `TorrentInfo` → new
   `file_id`/`link` → fresh `FileLocator`.
5. `resolve_url(new_loc)`.
6. Update the VFS node + catalogue entry (`current_torrent_id`, file ids/links);
   delete the stale provider torrent.
7. If not cached: fail this read; the torrent downloads and the next scan/read
   picks it up (preserves RD's current non-cached semantics).

The existing health state machine (Healthy→Broken→Repairing→Failed, 30s cooldown,
max 3 attempts) is preserved but calls the trait rather than concrete RD methods.
TorBox uses `check_cached` instead of RD's poll-and-guess.

### 5. TorBox client — `src/torbox_client.rs` (new)

| Trait method | TorBox endpoint | Notes |
|---|---|---|
| `list_torrents` | `GET /torrents/mylist` | Files inline; no per-torrent fetch needed |
| `resolve_url` | `GET /torrents/requestdl?token&torrent_id&file_id` | Returns direct URL (~3h TTL); cache keyed by `(torrent_id,file_id)` |
| `check_cached` | `GET /torrents/checkcached?hash=` | Real cached-availability check |
| `add_by_hash` | `POST /torrents/createtorrent` | Magnet built from hash; rate-limited 60/h |
| `delete_torrent` | `POST /torrents/controltorrent` | Body `{ torrent_id, operation: "delete" }` |

- Base URL `https://api.torbox.app/v1/api`; auth `Authorization: Bearer <API_KEY>`.
- Reuses the adaptive token-bucket rate-limiter pattern from `rd_client.rs`.
  `createtorrent` (60/h) needs a conservative cap distinct from read endpoints.
- Resolution cache TTL ~3h (matching TorBox link expiry) vs RD's ~1h.
- Integer `torrent_id`/`file_id` are stringified into the canonical model.

### 6. Provider selection — `src/main.rs`

```text
rd  = env RD_API_TOKEN  (trimmed, non-empty?)
tb  = env TORBOX_API_KEY (trimmed, non-empty?)
match (rd, tb):
    (Some, Some) -> error "Set only one of RD_API_TOKEN / TORBOX_API_KEY", exit
    (Some, None) -> Arc<dyn DebridProvider> = RealDebridClient::new(rd)
    (None, Some) -> Arc<dyn DebridProvider> = TorBoxClient::new(tb)
    (None, None) -> error "Set RD_API_TOKEN or TORBOX_API_KEY", exit
```

### 7. Errors — `src/error.rs`

Add `AppError` variants:
- a config error (both tokens set / neither set),
- a "provider resource unavailable / needs re-acquire" signal used by
  `resolve_url` and consumed by the re-acquire orchestration.

## Module changes

| File | Change |
|---|---|
| `provider.rs` *(new)* | Canonical types, `FileLocator`, `DebridProvider` trait, `MockProvider` (test) |
| `torbox_client.rs` *(new)* | TorBox implementation of the trait |
| `rd_client.rs` | Implement the trait; map RD responses → canonical types |
| `tasks.rs` | Build catalogue + VFS from the persistent store; provider-agnostic |
| `dav_fs.rs` | Hold `Arc<dyn DebridProvider>`; resolve via trait; trigger re-acquire |
| `repair.rs` | Generalised to the trait; provider-agnostic re-acquire |
| `main.rs` | Provider selection (xor tokens) |
| `mapper.rs` | Declare new modules |
| `error.rs` | New variants (config, needs-re-acquire) |
| redb schema | New hash-keyed catalogue table + migration |

## Testing strategy (TDD)

- **Unit:** `MockProvider` drives scan-loop, VFS-build-from-catalogue, and
  re-acquire orchestration tests without network. TorBox client parse tests from
  fixture JSON for `mylist`/`requestdl`/`checkcached`/`createtorrent`.
- **Migration:** dedicated tests for torrent-id-keyed → hash-keyed redb migration.
- **Integration:** TorBox integration tests mirroring the RD suite, gated on
  `TORBOX_API_KEY`. Update the pre-commit gate to run the active provider's suite.
- Run `cargo test` after every change; keep green.

## Documentation

Update `CLAUDE.md` and `README.md`:
- New modules (`provider.rs`, `torbox_client.rs`), new env var `TORBOX_API_KEY`.
- Single-provider selection rule (both-set is an error).
- Durable catalogue (VFS from persistent store, not live list).
- Generalised on-demand re-acquisition across providers.

## Implementation phasing

Each phase lands test-green with no regression to RD behaviour.

1. **Abstraction (pure refactor):** canonical model + `DebridProvider` trait + RD
   implements it + `MockProvider` + provider-selection (both-tokens error). No
   behaviour change.
2. **Durable catalogue:** hash-keyed catalogue table + migration + build VFS from
   the catalogue. RD gains durability; no regression.
3. **Generalised re-acquire:** refactor `repair.rs` to the trait.
4. **TorBox client:** implement the trait against the TorBox API + unit and
   integration tests.
5. **Docs:** update `CLAUDE.md` and `README.md`.

## Open questions / risks

- **redb migration** is the riskiest item; mitigated by phasing it separately
  (phase 2) with dedicated tests and a backfill-then-read-new approach.
- TorBox `createtorrent` file selection: confirm whether it auto-selects all
  files or requires an explicit selection step during implementation (affects
  `add_by_hash`).
- Confirm exact TorBox JSON field names against the live API during phase 4
  (captured as fixtures for parse tests).
