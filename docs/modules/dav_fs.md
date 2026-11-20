# `dav_fs.rs`

Maps VFS to WebDAV; on read resolves a `FileLocator` to a CDN URL via `provider.resolve_url` (cached per provider: ~1h Real-Debrid, ~3h TorBox), attempts synchronous instant repair on `AppError::Unavailable` (resolve-time) or a persistent CDN **5xx** (byte-fetch time, via the shared `attempt_instant_repair`).
An opened file's locator first goes through `RepairManager::redirect`, so a Real-Debrid new-id repair reaches every later open before the VFS is rebuilt.
The byte fetch is **window-bounded**: a Range-ignoring CDN that replies `200 OK` streaming the WHOLE multi-GB object from byte 0 (**TorBox** can do this; normally it honours ranges and replies `206`) is read only for the requested `[pos, range_end]` window then has its connection dropped — it is accepted at `pos == 0` and serves correctly, NOT rejected as "oversized" on its full-file `Content-Length` (mirrors `probe::read_body`).
A `200` at any `pos > 0` is still rejected (it would be the wrong bytes), as is a `206` whose `Content-Range` start disagrees with `pos`.
A connection error while sending the request OR reading the body (a mid-body reset, a closed pooled keep-alive connection, the 30 s client timeout firing during the body) is retried once on the same URL without invalidating the resolution; a 5xx is likewise retried on the same URL before escalating to instant repair, and only a stale 4xx / non-compliant response re-resolves.

**Connection reuse:** a bounded `206` (Content-Length ≤ the requested window) is instead read to its natural end so reqwest returns the TCP+TLS socket to the keep-alive pool — the next read-ahead window reuses it rather than re-handshaking per 2 MB.

**First-read window:** the first fetch while the read-ahead buffer is empty — the FIRST fetch per opened file, and again after an instant-repair buffer reset — uses the larger `CDN_FIRST_READ_MB` window (`with_first_read_bytes`) to front-load the header/early index; later (warm-buffer) fetches use the 2 MB `BUFFER_SIZE`.
