# Repair

## Synchronous instant repair (provider-neutral)

When a media file is read, `dav_fs.rs` resolves the locator via `provider.resolve_url` (cached, so free when healthy).
On `AppError::Unavailable`, it calls `repair_manager.try_instant_repair(&FileLocator) -> Result<FileLocator, _>` synchronously — re-adds the torrent by hash, selects the same files, and checks if the replacement is already cached.
Success is decided by the new torrent reaching `status == "downloaded"` **plus** a `file_path` match for the same file (there is no `InstantRepairResult` type, and no RD-specific link-count check — path matching is inherently safe, and the replacement file is located by matching `file_path` rather than the old positional index into RD's `links` array).
On success it returns a fresh `FileLocator` for that file — pairing the per-file restricted `link` by position among selected files for RD, or leaving `link: None` for providers that resolve by `(torrent_id, file_id)` (TorBox).
`dav_fs` installs the returned locator (invalidating the old resolution) and re-resolves inline, so playback continues after a brief delay.
For a new-id (Real-Debrid) repair, before it deletes the old torrent, the repair also records a redirect: the old torrent id mapped to each selected file's locator in the replacement (`RepairManager::repaired_locators`).
`DebridFileSystem::open` opens every media file through `RepairManager::redirect` (which follows chains of repairs), so until the next scan rebuilds the VFS, later opens of the same file or a sibling file don't resolve the deleted torrent and fail.
`try_instant_repair` consults the same redirect first, so a handle opened before the repair (a second reader of the file, or a long-lived sibling stream) whose read then fails gets the recorded copy instead of a doomed repair of the deleted torrent.
`prune_health_status` drops a redirect once the listing has neither the old torrent nor the copy its chain of redirects ends at, and that copy isn't a replacement still waiting for the scan loop, so a handle opened before the repair reaches the replacement for as long as it exists.
The repair is bounded to 30 s (`INSTANT_REPAIR_TIMEOUT`).
A repair cut short, by that timeout or by the read awaiting it being dropped, leaves the torrent `Healthy`/visible with its attempt budget reset and its cooldown restarted at that moment.
A new-id (RD) torrent it had added but not yet adopted is deleted in the background (`repair::UnadoptedAddGuard`): `reacquire::materialise` records the added id as soon as the add returns, and the repair takes it back before it adopts the add, so a repair cut short after that point never removes both copies.
The scan loop's same-hash dedup could not clean up such an un-adopted add, since it only collapses `downloaded` torrents and an un-selected RD torrent never downloads.
Once it has adopted the add, the repair records the replacement and updates the health map before the superseded torrent is deleted: on the cached path the new id gets the carried attempt budget and a fresh cooldown, and on the not-cached path the old torrent is marked Broken.
The delete then runs in a detached task (`RepairManager::delete_superseded`).
Awaited inside the 30 s bound, a slow or 5xx-ing Real-Debrid delete (it runs the full background retry budget) could time out a repair that had already succeeded, losing the replacement record and the carried budget.
The repair's three torrent lookups (the torrent under repair, the re-add, the final check) retry a 404 after 0.5 s and 1 s (`repair::REPAIR_INFO_NOT_FOUND_BACKOFF` via `provider::torrent_info_retrying_not_found`) for TorBox's spurious `mylist?id=` 404.
If **not** cached on a genuinely-new id (RD), the file fails, the new torrent is left to download, the old torrent is marked Broken, and the replacement is recorded (scan loop picks it up).

**Same-id re-add (TorBox):** TorBox's `createtorrent` recovers the existing torrent by infohash, so a repaired-in-place torrent keeps its **same** id (`new == old`); the repair therefore never deletes it (deleting would destroy the very torrent it repaired — and for mirror/manual content that would be permanent loss), records no replacement, and passes `protect_id` to `reacquire::materialise` so a transient post-add failure can't delete the real torrent either.

**Visible on no-replacement failures:** a repair that produces no usable replacement (a transient re-add/info error, or a same-id torrent that's merely re-downloading) leaves the torrent **`Healthy`/visible**, NOT `Broken` — because `read_bytes` short-circuits on `should_hide_torrent` *before* any I/O, a hidden torrent can never re-trigger repair, so marking it Broken with no replacement would trap it until restart.
`Broken` is set ONLY where a replacement IS recorded (the not-cached new-id path).

**Cooldown + attempt cap survive re-adds:** the repaired torrent's health carries the old entry's `repair_attempts` plus a fresh cooldown trigger to the new id (RD mints a new id each repair), so the 30s cooldown + max-3-attempt cap hold *across* re-adds — bounding a still-broken-on-CDN cached file to ≤3 re-adds rather than a per-read storm.
`prune_health_status` runs against the listing fetched at the start of the scan tick.
It keeps the new id's entry while its replacement is still waiting in `repair_replacements` (which `complete_repair` records before inserting that entry), so a repair that completes mid-tick keeps its carried budget.
A confirmed good byte read (`note_read_success`, called once per opened file) **resets** the budget, so the cap counts only *consecutive* failed repairs (a storm), not unrelated incidents over the deployment's lifetime (which would otherwise accumulate to a permanent `Failed`).

## Repair hides broken torrents (provider-dependent)

On the not-cached repair path with a genuinely-NEW id (Real-Debrid), the old torrent is marked Broken and hidden from WebDAV until the scan loop picks up the recorded replacement.
On a SAME-id re-add (TorBox's `createtorrent` recovers the existing torrent by infohash), and on any repair failure that records NO replacement, the torrent is instead kept `Healthy`/visible — because a `Broken`/hidden torrent can never re-trigger repair (reads short-circuit on `should_hide_torrent`), so hiding it with no replacement would trap it until restart (see the instant-repair design decision above).
A torrent mid-repair (`Repairing`) is NOT hidden from the VFS (`hidden_torrent_ids`, the VFS and repoint filter, holds only `Broken`/`Failed`), so a scan tick that lands inside a repair doesn't drop its files or tell Jellyfin they were deleted; `should_hide_torrent` still refuses its reads until the repair settles.
