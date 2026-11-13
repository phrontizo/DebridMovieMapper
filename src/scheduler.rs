//! Background-job scheduler (SP2 Task 10). Splits the single `run_scan_loop` spawn into
//! cooperating periodic tasks over a shared [`AppState`]:
//!
//! - **Scan task** = `run_scan_loop`: per tick, sharing one `get_torrents`, the account sync (VFS
//!   mirror) + acquisition verification (`engine.observe`), plus (SP3) `record_mirror_owned` and
//!   `dedup_owned`. Cadence: `SCAN_INTERVAL_SECS`.
//! - **Trakt cycle task** = `sync_trakt` THEN `reconcile_wanted`, sequentially each tick (so the
//!   reconciler sees the just-synced wanted set). Cadence: `TRAKT_SYNC_INTERVAL_SECS`.
//! - **Episode monitor task** = `monitor_episodes`. Cadence: `TRAKT_EPISODE_CHECK_INTERVAL_SECS`.
//! - **Upgrade task** (SP3) = `run_upgrade_once` (daily quality-upgrade + full-season consolidation).
//!   Spawned ONLY when `config.upgrade.enabled()` (`UPGRADE_INTERVAL_SECS > 0`). Cadence:
//!   `UPGRADE_INTERVAL_SECS`; the first pass waits out the read-activity start-up grace
//!   ([`upgrade_first_run_delay`]), during which it could do nothing.
//!
//! The Trakt cycle + monitor tasks are spawned ONLY when `trakt_jobs_enabled(&app)` — i.e. both a
//! Trakt client and `config.trakt` are present; otherwise the service runs exactly as before.

use crate::app_state::AppState;
use crate::tasks::{monitor_episodes, reconcile_wanted, run_scan_loop, sync_trakt, ScanConfig};
use crate::upgrade::run_upgrade_once;
use std::time::Duration;
use tokio::sync::watch;
use tracing::info;

/// Run `job` immediately, then sleep `interval` after EACH tick completes (a fixed-DELAY cadence, so
/// the effective period is `tick_duration + interval`), until `shutdown` flips to true. Fixed-delay
/// (not fixed-rate) is deliberate: it can never overlap two ticks of the same job, and for the
/// daily/15-min cadences here the tick-duration drift is negligible.
pub async fn periodic<F, Fut>(interval: Duration, mut shutdown: watch::Receiver<bool>, mut job: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    loop {
        if *shutdown.borrow() {
            return;
        }
        // Run the job, but cancel it promptly if shutdown fires MID-tick. A long tick (e.g. an
        // upgrade budget staging many candidates, each polling the provider for seconds) must not
        // delay graceful shutdown past the container's stop grace (→ SIGKILL) — the scan loop
        // already bails mid-work, so the periodic jobs must too. Dropping the job future at an await
        // point is safe: the jobs are idempotent and all persistence is via ACID redb writes, so the
        // worst case is an added-but-unrecorded provider torrent, which the account-mirror/dedup pass
        // reclaims on the next run.
        // Run the job under `catch_unwind` so a panic in ONE tick is logged and the schedule
        // continues, rather than unwinding `periodic` and silently disabling this subsystem for the
        // process lifetime (a `JoinError` would otherwise surface only at shutdown). `catch_unwind`
        // (not `tokio::spawn`) is used so the mid-tick cancellation below is preserved — spawning
        // would detach the task on shutdown instead of dropping it. A caught panic leaves at worst an
        // added-but-unrecorded provider torrent (same as the drop-on-shutdown case), reclaimed next run.
        // The `job()` invocation itself is wrapped in the caught async block (not evaluated eagerly as
        // `catch_unwind(job())` would), so a panic while *constructing* a tick's future is caught too,
        // not just a panic while polling it.
        use futures_util::FutureExt;
        tokio::select! {
            res = std::panic::AssertUnwindSafe(async { job().await }).catch_unwind() => {
                if res.is_err() {
                    tracing::error!("periodic job panicked; tick aborted, schedule continues");
                }
            }
            // shutdown signalled (Ok) or sender dropped (Err) — either way, stop.
            _ = shutdown.changed() => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            // `changed()` resolving means either a shutdown signal OR the sender was dropped
            // (Err). Both are terminal here: treat a dropped sender as shutdown so we exit rather
            // than hot-spin re-running `job()` with no delay (a dropped sender resolves `changed()`
            // immediately and forever while `borrow()` stays false).
            res = shutdown.changed() => {
                if res.is_err() {
                    return;
                }
            }
        }
        if *shutdown.borrow() {
            return;
        }
    }
}

/// [`periodic`], but the first run waits `first_delay` (returning early, without running the job, if
/// `shutdown` fires meanwhile). A zero delay is exactly `periodic`.
pub async fn periodic_after<F, Fut>(
    first_delay: Duration,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    job: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if !first_delay.is_zero() {
        tokio::select! {
            _ = tokio::time::sleep(first_delay) => {}
            // shutdown signalled or sender dropped — stop either way.
            _ = shutdown.changed() => return,
        }
    }
    periodic(interval, shutdown, job).await
}

/// How long the upgrade job waits before its first pass: what is left of the read-activity start-up
/// grace (`UPGRADE_IDLE_SECS` after process start, during which the library is never idle). A pass
/// inside the grace would defer every title — its swaps and prunes are idle-gated, and so is the
/// per-title loop — and the next pass only comes `UPGRADE_INTERVAL_SECS` later (fixed delay), so a
/// service restarted at least that often would never upgrade anything. Zero when there is no grace
/// left, so the first pass then runs immediately, as before the grace existed.
pub(crate) fn upgrade_first_run_delay(app: &AppState) -> Duration {
    app.read_activity
        .startup_grace_remaining(Duration::from_secs(app.config.upgrade.idle_secs))
}

/// Returns `true` when both a Trakt client and Trakt configuration are present — the
/// condition that gates the Trakt cycle + episode-monitor jobs in `run`. Exposed as
/// `pub(crate)` so tests can assert the disabled path without spawning tasks.
pub(crate) fn trakt_jobs_enabled(app: &AppState) -> bool {
    app.trakt_client.is_some() && app.config.trakt.is_some()
}

/// Backoff before restarting a panicked scan loop — long enough to avoid a tight respawn storm on a
/// deterministic panic, short enough that the library refreshes again promptly.
const SCAN_RESTART_BACKOFF: Duration = Duration::from_secs(5);

/// Run the future produced by `make` on its own task; if it panics, log and re-run it after
/// `backoff` (respecting `shutdown`). A clean return (`Ok(())`) or a non-panic `JoinError`
/// (cancellation) ends supervision. Generic over the loop factory so it can be unit-tested without
/// the real scan loop.
async fn supervise<F, Fut>(mut shutdown: watch::Receiver<bool>, backoff: Duration, mut make: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    loop {
        if *shutdown.borrow() {
            return;
        }
        match tokio::spawn(make()).await {
            Ok(()) => return,
            Err(e) if e.is_panic() => {
                if *shutdown.borrow() {
                    return;
                }
                tracing::error!(
                    "Supervised task panicked ({:?}); restarting in {}s",
                    e,
                    backoff.as_secs()
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    // shutdown signalled or sender dropped — stop supervising either way.
                    _ = shutdown.changed() => return,
                }
            }
            Err(e) => {
                tracing::error!("Supervised task ended abnormally (not a panic): {:?}", e);
                return;
            }
        }
    }
}

/// Spawn all background jobs over `app`, returning when all have stopped (after shutdown).
pub async fn run(app: AppState, shutdown: watch::Receiver<bool>) {
    let mut handles = Vec::new();

    // Scan task (account sync + acquisition verification, both inlined in run_scan_loop) —
    // unchanged behaviour, own internal cadence.
    // Supervised so a panic in a single tick can't silently freeze the most important subsystem
    // (VFS refresh + observe + repair-replacement processing + account-mirror/dedup) for the process
    // lifetime — it is restarted after a short backoff, mirroring the per-tick `catch_unwind`
    // resilience the `periodic` jobs already have. (The scan loop maintains cross-tick state and its
    // own shutdown-aware awaits, so it isn't built on `periodic`; supervision gives the same guarantee
    // without restructuring the loop body.)
    let scan_app = app.clone();
    let scan_shutdown = shutdown.clone();
    handles.push(tokio::spawn(async move {
        supervise(scan_shutdown.clone(), SCAN_RESTART_BACKOFF, move || {
            run_scan_loop(
                ScanConfig {
                    app: scan_app.clone(),
                },
                scan_shutdown.clone(),
            )
        })
        .await;
    }));

    if trakt_jobs_enabled(&app) {
        // trakt_jobs_enabled guarantees config.trakt is Some
        if let Some(t) = app.config.trakt.as_ref() {
            let trakt_secs = t.sync_interval_secs;
            let episode_secs = t.episode_check_interval_secs;
            info!(
                "Trakt sync enabled: sync every {}s, episode check every {}s",
                trakt_secs, episode_secs
            );

            // Trakt cycle: sync_trakt -> reconcile_wanted (sequential, so reconcile sees the fresh set).
            let trakt_app = app.clone();
            handles.push(tokio::spawn(periodic(
                Duration::from_secs(trakt_secs),
                shutdown.clone(),
                move || {
                    let app = trakt_app.clone();
                    async move {
                        if let Some(trakt) = &app.trakt_client {
                            let catchup = app
                                .config
                                .trakt
                                .as_ref()
                                .and_then(|t| t.catchup_lookback_secs);
                            let summary = sync_trakt(
                                trakt,
                                &app.tmdb_client,
                                &app.store,
                                catchup,
                                app.config.remove_finished_shows,
                            )
                            .await;
                            // Only let this tick's reconcile REMOVE content if the sync actually
                            // confirmed the `wanted` set (≥1 account synced, or none enrolled) —
                            // otherwise a transiently-empty set (e.g. just after a migration cleared
                            // it) could Trigger-B-delete engine-acquired content. Acquires are
                            // unaffected; removals resume next cycle once a sync succeeds.
                            reconcile_wanted(
                                app.engine.as_ref(),
                                &app.provider,
                                &app.tmdb_client,
                                &app.store,
                                &app.read_activity,
                                Duration::from_secs(app.config.upgrade.idle_secs),
                                summary.removals_trustworthy(),
                            )
                            .await;
                        }
                    }
                },
            )));

            // Episode monitor.
            let monitor_app = app.clone();
            handles.push(tokio::spawn(periodic(
                Duration::from_secs(episode_secs),
                shutdown.clone(),
                move || {
                    let app = monitor_app.clone();
                    async move {
                        monitor_episodes(
                            app.engine.as_ref(),
                            &app.provider,
                            &app.tmdb_client,
                            &app.store,
                            &app.read_activity,
                            Duration::from_secs(app.config.upgrade.idle_secs),
                            app.config.remove_finished_shows,
                        )
                        .await;
                    }
                },
            )));
        }
    } else {
        info!("Trakt sync disabled (no Trakt client configured)");
    }

    if app.config.upgrade.enabled() {
        let secs = app.config.upgrade.interval_secs;
        let first = upgrade_first_run_delay(&app);
        info!(
            "Upgrade engine enabled: re-scoring owned titles every {}s (first pass in {}s, after the start-up idle grace)",
            secs,
            first.as_secs()
        );
        let upgrade_app = app.clone();
        handles.push(tokio::spawn(periodic_after(
            first,
            Duration::from_secs(secs),
            shutdown.clone(),
            move || {
                let app = upgrade_app.clone();
                async move {
                    run_upgrade_once(&app).await;
                }
            },
        )));
    } else {
        info!("Upgrade engine disabled (UPGRADE_INTERVAL_SECS=0)");
    }

    for h in handles {
        if let Err(e) = h.await {
            tracing::error!("Background task ended abnormally: {:?}", e);
        }
    }
}

/// Helper used exclusively by tests — builds a minimal `AppState` configured either
/// with or without Trakt, without touching the process environment.
#[cfg(test)]
fn make_test_app(with_trakt: bool) -> crate::app_state::AppState {
    use crate::app_state::AppState;
    use crate::config::Config;
    use crate::provider::{DebridProvider, MockProvider, ProviderKind};
    use crate::repair::RepairManager;
    use crate::store::Store;
    use crate::tmdb_client::TmdbClient;
    use crate::vfs::DebridVfs;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
    let db = Arc::new(
        redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .unwrap(),
    );
    let store = Store::from_database(db).unwrap();
    let mut config = Config::from_parts(
        None,
        Some("tb".to_string()),
        Some("k".to_string()),
        None,
        None,
        None,
    )
    .unwrap();
    let trakt_client: Option<Arc<dyn crate::trakt_client::TraktClient>> = if with_trakt {
        config.trakt = crate::config::TraktConfig::from_parts(
            Some("client_id".to_string()),
            Some("client_secret".to_string()),
            None,
            None,
            None,
            None,
        );
        Some(Arc::new(crate::trakt_client::MockTrakt::default()))
    } else {
        None
    };
    let scraper: Arc<dyn crate::scraper::Scraper> =
        Arc::new(crate::scraper::TorrentioScraper::new(
            None,
            ProviderKind::TorBox,
            "tok",
            reqwest::Client::new(),
        ));
    let validator: Arc<dyn crate::acquire::TitleValidator> =
        Arc::new(crate::acquire::TmdbTitleValidator {
            tmdb: Arc::new(TmdbClient::new("k".to_string()).unwrap()),
        });
    let prober: Arc<dyn crate::acquire::Prober> = Arc::new(crate::acquire::HttpProber {
        http: reqwest::Client::new(),
    });
    let engine = Arc::new(crate::acquire::AcquisitionEngine::new(
        provider.clone(),
        scraper.clone(),
        validator,
        prober,
        store.clone(),
        crate::config::AcquisitionConfig::default().prefs,
        5,
        std::time::Duration::from_secs(1800),
        std::time::Duration::from_secs(600),
    ));
    AppState {
        provider: provider.clone(),
        tmdb_client: Arc::new(TmdbClient::new("k".to_string()).unwrap()),
        vfs: Arc::new(RwLock::new(DebridVfs::new())),
        store,
        repair_manager: Arc::new(RepairManager::new(provider)),
        config: Arc::new(config),
        jellyfin_client: None,
        http_client: reqwest::Client::new(),
        scraper,
        engine,
        trakt_client,
        read_activity: Arc::new(crate::read_activity::ReadActivity::without_startup_grace()),
    }
}

#[cfg(test)]
mod trakt_gate_tests {
    use super::*;

    /// Without a Trakt client (no Trakt config), `trakt_jobs_enabled` returns false — the
    /// scheduler runs ONLY the scan task, exactly as before Trakt was introduced.
    #[test]
    fn disabled_when_no_trakt_client() {
        let app = make_test_app(false);
        assert!(
            !trakt_jobs_enabled(&app),
            "Trakt gate must be false when trakt_client is None"
        );
    }

    /// With both a Trakt client and a Trakt config present, `trakt_jobs_enabled` returns true.
    #[test]
    fn enabled_when_trakt_configured() {
        let app = make_test_app(true);
        assert!(
            trakt_jobs_enabled(&app),
            "Trakt gate must be true when both client and config are present"
        );
    }

    /// The upgrade job's first pass waits out the read-activity start-up grace: during it the
    /// library is never idle, so a pass at start-up would defer every title and — on the daily
    /// fixed-delay cadence — the next pass would only come one `UPGRADE_INTERVAL_SECS` later (a
    /// service restarted at least daily would never upgrade anything). With no grace (or once it is
    /// over) the first pass runs immediately, as before.
    #[test]
    fn upgrade_first_run_waits_out_the_startup_grace() {
        let mut app = make_test_app(false);
        assert_eq!(
            upgrade_first_run_delay(&app),
            Duration::ZERO,
            "no start-up grace → the first pass runs immediately"
        );
        app.read_activity = std::sync::Arc::new(crate::read_activity::ReadActivity::new());
        let idle = Duration::from_secs(app.config.upgrade.idle_secs);
        let delay = upgrade_first_run_delay(&app);
        assert!(
            delay <= idle && delay > idle - Duration::from_secs(5),
            "the first pass waits the rest of the grace (~{idle:?}), got {delay:?}"
        );
        assert!(delay < Duration::from_secs(app.config.upgrade.interval_secs));
    }

    /// `upgrade_gate_follows_config`: default `Config` has the upgrade job enabled (daily);
    /// swapping in an `UpgradeConfig` with `interval_secs = 0` disables it.
    #[test]
    fn upgrade_gate_follows_config() {
        let mut app = make_test_app(false);
        // default config has upgrade enabled (daily)
        assert!(app.config.upgrade.enabled());
        // a config with interval 0 disables it
        let mut cfg = (*app.config).clone();
        cfg.upgrade = crate::config::UpgradeConfig::from_parts(Some("0".into()), None, None);
        app.config = std::sync::Arc::new(cfg);
        assert!(!app.config.upgrade.enabled());
    }

    /// Source-level guard: confirms the temporary `--acquire` CLI trigger has been removed.
    /// This test will fail if anyone accidentally reintroduces the flag.
    #[test]
    fn acquire_cli_is_removed() {
        let main_src = include_str!("main.rs");
        assert!(
            !main_src.contains("--acquire"),
            "The --acquire CLI trigger must not appear in main.rs; \
             remove the temporary SP1 block if this assertion fails"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// `periodic` runs the job immediately, then once per interval, until shutdown.
    /// Paused-clock: each manual `advance(interval)` fires exactly one timer → one tick, so the
    /// counter reaches `1 (immediate) + 3 (ticks) = 4`. Asserted as a tolerant `4..=5` range to
    /// absorb any paused-clock scheduling slack.
    #[tokio::test(start_paused = true)]
    async fn periodic_runs_immediately_then_on_cadence() {
        let counter = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = watch::channel(false);

        let c = counter.clone();
        let handle = tokio::spawn(periodic(Duration::from_secs(60), rx, move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        }));

        // Let the immediate run happen.
        tokio::task::yield_now().await;

        // Advance through three intervals, one at a time, yielding after each so the task is
        // polled and re-arms its next sleep before the following advance.
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }

        // Signal shutdown and advance past one more interval so the task observes it and exits.
        tx.send(true).unwrap();
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;

        let _ = handle.await;

        let n = counter.load(Ordering::SeqCst);
        assert!(
            (4..=5).contains(&n),
            "expected ~4 runs (initial + 3 ticks), got {}",
            n
        );
    }

    /// A panicking tick must NOT kill the schedule: `periodic` runs each tick under
    /// `catch_unwind` (unlike a plain `job().await`, which would unwind `periodic` and silently
    /// disable the subsystem for the process lifetime). The job PANICS on its first invocation,
    /// then increments `after_panic` on every later tick — so `after_panic` advancing past the
    /// panicking first tick is direct proof the schedule continued.
    #[tokio::test(start_paused = true)]
    async fn periodic_continues_after_a_panicking_tick() {
        let calls = Arc::new(AtomicUsize::new(0));
        let after_panic = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = watch::channel(false);

        let calls_c = calls.clone();
        let after_c = after_panic.clone();
        let handle = tokio::spawn(periodic(Duration::from_secs(60), rx, move || {
            let calls_c = calls_c.clone();
            let after_c = after_c.clone();
            async move {
                let n = calls_c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    panic!("simulated panic on the first tick");
                }
                after_c.fetch_add(1, Ordering::SeqCst);
            }
        }));

        // Immediate run → panics (caught by `catch_unwind`); the schedule must survive it.
        tokio::task::yield_now().await;

        // Advance through three intervals, one at a time, yielding so the task re-arms each sleep.
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }

        // Signal shutdown and advance once more so the task observes it and exits.
        tx.send(true).unwrap();
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        let _ = handle.await;

        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "the job must be invoked again after the panicking first tick (got {})",
            calls.load(Ordering::SeqCst)
        );
        assert!(
            after_panic.load(Ordering::SeqCst) >= 1,
            "ticks after the panic must run — the counter must advance past the panic (got {})",
            after_panic.load(Ordering::SeqCst)
        );
    }

    /// `periodic` returns promptly when shutdown is signalled, without waiting the full interval.
    /// The job runs exactly once (the immediate run) before shutdown is observed.
    #[tokio::test]
    async fn periodic_stops_on_shutdown() {
        let counter = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = watch::channel(false);

        let c = counter.clone();
        let handle = tokio::spawn(periodic(Duration::from_secs(3600), rx, move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
            }
        }));

        // Let the immediate run happen and the task park on the long sleep.
        tokio::task::yield_now().await;

        // Signal shutdown; the task must return without waiting out the (long) interval.
        tx.send(true).unwrap();

        // The test completing (the handle resolving) is the core assertion.
        handle.await.unwrap();

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "job runs exactly once (the immediate run) before shutdown"
        );
    }

    /// `supervise` restarts a panicking loop until it returns cleanly (the panic-resilience the
    /// scan task relies on). The factory panics on its first two runs, then returns `Ok(())`.
    #[tokio::test(start_paused = true)]
    async fn supervise_restarts_after_panic_until_clean_return() {
        let runs = Arc::new(AtomicUsize::new(0));
        let (_tx, rx) = watch::channel(false);
        let r = runs.clone();
        supervise(rx, Duration::from_millis(1), move || {
            let r = r.clone();
            async move {
                let n = r.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    panic!("simulated scan-tick panic");
                }
                // third run returns cleanly → supervisor stops
            }
        })
        .await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            3,
            "loop should run 3 times: two panics then a clean return"
        );
    }

    /// `supervise` never starts the loop when shutdown is already signalled.
    #[tokio::test(start_paused = true)]
    async fn supervise_does_not_start_after_shutdown() {
        let runs = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = watch::channel(false);
        let r = runs.clone();
        tx.send(true).unwrap();
        supervise(rx, Duration::from_secs(3600), move || {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                panic!("always panics");
            }
        })
        .await;
        // With shutdown already set, the loop guard returns before running at all.
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    /// A shutdown that arrives while `supervise` waits out the restart backoff after a panic ends
    /// supervision at once: no restart, and no waiting out the (long) backoff.
    #[tokio::test(start_paused = true)]
    async fn supervise_stops_when_shutdown_arrives_during_backoff() {
        let runs = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = watch::channel(false);
        let r = runs.clone();
        let handle = tokio::spawn(supervise(rx, Duration::from_secs(3600), move || {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                panic!("always panics");
            }
        }));
        settle().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "one panicked run, then parked in the backoff"
        );
        tx.send(true).unwrap();
        // The paused clock auto-advances to the earliest timer, so a supervisor still sleeping out
        // its 3600s backoff would let this 1s timeout fire first.
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("shutdown during the backoff must end supervision without waiting it out")
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no restart after shutdown");
    }

    /// A loop that panics after shutdown was signalled mid-run is not restarted.
    #[tokio::test(start_paused = true)]
    async fn supervise_does_not_restart_a_loop_that_panics_after_shutdown() {
        let runs = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let (tx, rx) = watch::channel(false);
        let (r, rel) = (runs.clone(), release.clone());
        let handle = tokio::spawn(supervise(rx, Duration::from_secs(3600), move || {
            let (r, rel) = (r.clone(), rel.clone());
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                rel.notified().await;
                panic!("panics on its way out");
            }
        }));
        settle().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the loop is running");
        tx.send(true).unwrap();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("a panic after shutdown must end supervision")
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no restart after shutdown");
    }

    /// Yield a few times so a task woken by a paused-clock `advance` gets polled to its next await.
    async fn settle() {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    /// `periodic_after` runs nothing during `first_delay`, runs the job right when it ends (not one
    /// `interval` later), then continues on the fixed-delay cadence.
    #[tokio::test(start_paused = true)]
    async fn periodic_after_runs_when_the_first_delay_ends_then_on_cadence() {
        let counter = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = watch::channel(false);
        let c = counter.clone();
        let handle = tokio::spawn(periodic_after(
            Duration::from_secs(300),
            Duration::from_secs(86_400),
            rx,
            move || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                }
            },
        ));
        settle().await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "nothing during the delay"
        );
        tokio::time::advance(Duration::from_secs(299)).await;
        settle().await;
        assert_eq!(counter.load(Ordering::SeqCst), 0, "still inside the delay");
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "the first run comes as the delay ends"
        );
        tokio::time::advance(Duration::from_secs(86_399)).await;
        settle().await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(counter.load(Ordering::SeqCst), 2, "then once per interval");
        tx.send(true).unwrap();
        handle.await.unwrap();
    }

    /// A shutdown during `periodic_after`'s first delay returns promptly without running the job.
    #[tokio::test(start_paused = true)]
    async fn periodic_after_stops_on_shutdown_during_the_first_delay() {
        let counter = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = watch::channel(false);
        let c = counter.clone();
        let handle = tokio::spawn(periodic_after(
            Duration::from_secs(3600),
            Duration::from_secs(60),
            rx,
            move || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                }
            },
        ));
        settle().await;
        tx.send(true).unwrap();
        handle.await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    /// A shutdown signalled MID-tick cancels the in-flight job promptly, rather than waiting for a
    /// long-running tick to finish (which would defeat graceful shutdown under a container stop grace).
    #[tokio::test(start_paused = true)]
    async fn periodic_cancels_in_flight_job_on_shutdown() {
        use std::sync::atomic::AtomicBool;
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (tx, rx) = watch::channel(false);

        let s = started.clone();
        let f = finished.clone();
        let handle = tokio::spawn(periodic(Duration::from_secs(60), rx, move || {
            let s = s.clone();
            let f = f.clone();
            async move {
                s.store(true, Ordering::SeqCst);
                // A long-running tick. If shutdown didn't cancel it, the task would block here.
                tokio::time::sleep(Duration::from_secs(3600)).await;
                f.store(true, Ordering::SeqCst);
            }
        }));

        // Let the job start and park on its long sleep.
        tokio::task::yield_now().await;
        assert!(
            started.load(Ordering::SeqCst),
            "the job should have started"
        );

        // Signal shutdown mid-job; the task must return WITHOUT the 1h tick completing.
        tx.send(true).unwrap();
        handle.await.unwrap();

        assert!(
            !finished.load(Ordering::SeqCst),
            "an in-flight job must be cancelled on shutdown, not run to completion"
        );
    }
}
