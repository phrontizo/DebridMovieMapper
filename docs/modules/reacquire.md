# `reacquire.rs`

`materialise` — re-adds a torrent by hash, **polls** for the file list (retrying every `settle` up to a caller-set `max_wait`, so slow-to-resolve uncached metadata isn't given up on after one check; its only caller, the instant-repair path, passes `settle == max_wait`, a single poll, since the re-added hash is known-good), and selects the target files.
It records the id the add returned in an optional caller slot (`added`; never the protected same-id torrent), so a caller that may drop it mid-await can delete that torrent.
Its post-add lookup retries a 404 on the repair schedule.
When `materialise` fails after its add, it deletes the added torrent; a delete that fails is logged at error with the torrent id, and the torrent is left in the account.
Used by the **instant-repair path only** — the SP3 acquisition engine no longer materialises (it adds optimistically in `acquire` and resolves the verdict in `observe`)
