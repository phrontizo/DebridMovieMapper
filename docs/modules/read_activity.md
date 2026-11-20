# `read_activity.rs`

In-memory proxy read-activity tracker (SP3), keyed by torrent info hash (lowercased).
`ReadActivity::touch(hash)` is stamped by `dav_fs` on every proxy byte read with the file's `FileLocator.hash`.
The upgrade engine asks `torrents_idle(hashes, window)` (none of these torrents read within the window) about the torrents a swap or prune changes; the other destructive passes (dedup removals, `execute_remove`, the scan loop's same-hash duplicate deletion, selection repoints) ask `all_idle(window)`, which is library-wide (a read of any torrent within the window defers the pass).
Best-effort in-memory only, so a restart loses the read history: to keep that from reading as "idle", a tracker built by `ReadActivity::new()`/`Default` (production, created at start-up in `main.rs`) has a **start-up grace** — `all_idle(window)` and `torrents_idle` are `false` until one `window` (`UPGRADE_IDLE_SECS`) has elapsed since creation (`startup_grace_remaining(window)` reports what is left of it — the upgrade job sleeps that before its first pass).
Tests that need an already-idle tracker use the test-only `ReadActivity::without_startup_grace()` (`#[cfg(test)] pub(crate)`), so production can only build a tracker with the grace.
