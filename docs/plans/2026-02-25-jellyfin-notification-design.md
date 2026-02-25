# Jellyfin Library Notification

When the VFS changes (new content, deletions, repairs), notify Jellyfin via `POST /Library/Media/Updated` so it scans only the affected paths instead of requiring a full library rescan.

## Configuration

Three optional env vars. Feature disabled if any are missing:

- `JELLYFIN_URL` — e.g. `http://jellyfin:8096`
- `JELLYFIN_API_KEY` — Jellyfin API key
- `JELLYFIN_RCLONE_MOUNT_PATH` — rclone mount prefix, e.g. `/mnt/debrid`

## VFS Diff (`vfs.rs`)

```rust
pub enum UpdateType { Created, Modified, Deleted }

pub struct VfsChange {
    pub path: String,             // e.g. "Shows/Breaking Bad/Season 03"
    pub update_type: UpdateType,
}
```

`diff_trees(old: &VfsNode, new: &VfsNode) -> Vec<VfsChange>` walks both `BTreeMap`s in sorted merge-join order:

- Key only in new → `Created` (report deepest new directory, not every file)
- Key only in old → `Deleted` (report parent of removed subtree)
- Key in both, both directories → recurse deeper
- Key in both, leaf nodes differ (`rd_link` or content changed) → `Modified` on parent

## Jellyfin Client (`jellyfin_client.rs`)

Small HTTP client holding `url`, `api_key`, `mount_path`.

Single method: `notify_changes(changes: &[VfsChange])`.

- POSTs to `{url}/Library/Media/Updated`
- Header: `X-Emby-Token: {api_key}`
- Body: `{"Updates": [{"Path": "{mount_path}/{path}", "UpdateType": "Created"}, ...]}`
- All changes batched into one request
- Fire-and-forget with error logging (never blocks scan loop)

## Integration (`tasks.rs::update_vfs`)

1. Build new VFS (existing)
2. Diff old root vs new root → `Vec<VfsChange>`
3. Swap VFS (existing)
4. If changes non-empty and Jellyfin client configured, `tokio::spawn` notification

Jellyfin client passed as `Option<Arc<JellyfinClient>>`. `None` when unconfigured.

## Change scenarios covered

All go through `update_vfs`, so notification is automatic:

| Scenario | What happens |
|----------|-------------|
| New torrent identified | New folders appear → `Created` |
| Torrent deleted from RD | Folders disappear → `Deleted` |
| Repair completes | Next scan rebuilds VFS with new links → `Modified` |
| Re-identification | Old folder `Deleted`, new folder `Created` |
| Torrent hidden during repair | Filtered out → `Deleted`; repair done → next cycle → `Created` |
