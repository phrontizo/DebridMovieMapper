# `test_loopback.rs`

`#[cfg(test)]` scripted loopback HTTP/1.1 server shared by the HTTP clients' tests (Jellyfin, Trakt, TorBox, Real-Debrid), like `test_log`.
`serve(steps, then)` runs one `Step` per accepted connection (`Reply`, `DropBeforeRead`, `DropAfterRead`, `HangUntilClosed`; the last waits on a read rather than a sleep, so a paused test clock can't end it), then gives every later connection `then`, or stops accepting when it is `None`.
`serve_fn(respond)` answers each request with a reply built from its text.
Both count connections and record each request, head and body (`Loopback::heads` returns the heads only).
`read_request` and `write_reply` are shared too, e.g. by `trakt_client::test_http`.
