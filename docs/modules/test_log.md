# `test_log.rs`

`#[cfg(test)]` log capture shared by every module's tests, like `store::fail_writes`: `capture(level, f)` runs a closure and `capture_async(level, fut)` a future under a plain-text `tracing` subscriber, and each returns what was logged at `level` or above (`capture_async` alongside the future's output).
`config` uses `capture`; `enrolment`, `jellyfin_client`, `reacquire` and `torbox_client` use `capture_async`.
The first capture also registers a dispatcher with no subscriber that stays for the life of the test process (`ANCHOR`).
While only one dispatcher is registered, tracing-core decides whether a log line is enabled by asking just the thread that hits it first, so a line another test's thread hit first during a capture was cached as disabled and the capture missed it.
