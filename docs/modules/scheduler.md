# `scheduler.rs`

`periodic` primitive (+ `periodic_after`, the same with a delayed first run) + `run` that spawns the cooperating periodic jobs over `AppState`; the Trakt cycle + episode monitor are gated on `trakt_jobs_enabled`; the upgrade job's first pass waits `upgrade_first_run_delay` (what is left of the read-activity start-up grace — `ReadActivity::startup_grace_remaining`).
`run` also creates the episode monitor's `tasks::AssumedCoverage` once and passes it to every `monitor_episodes` tick.
`run_scan_loop` takes the `AppState` directly.
