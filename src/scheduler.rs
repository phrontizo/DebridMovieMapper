//! Background-job scheduler (SP2 Task 10). Splits the single `run_scan_loop` spawn into
//! cooperating periodic tasks over a shared [`AppState`]:
//!
//! - **Scan task** = `run_scan_loop` (UNCHANGED): `sync_account` (VFS mirror) + `verify_acquisitions`
//!   (`engine.observe`), sharing one `get_torrents` per tick. Cadence: `SCAN_INTERVAL_SECS`.
//! - **Trakt cycle task** = `sync_trakt` THEN `reconcile_wanted`, sequentially each tick (so the
//!   reconciler sees the just-synced wanted set). Cadence: `TRAKT_SYNC_INTERVAL_SECS`.
//! - **Episode monitor task** = `monitor_episodes`. Cadence: `EPISODE_CHECK_INTERVAL_SECS`.
//!
//! The Trakt cycle + monitor tasks are spawned ONLY when a Trakt client is present
//! (`app.trakt_client.is_some()`); otherwise the service runs exactly as before.

use crate::app_state::AppState;
use crate::tasks::{monitor_episodes, reconcile_wanted, run_scan_loop, sync_trakt, ScanConfig};
use std::time::Duration;
use tokio::sync::watch;
use tracing::info;

/// Run `job` immediately, then once every `interval`, until `shutdown` flips to true.
pub async fn periodic<F, Fut>(interval: Duration, mut shutdown: watch::Receiver<bool>, mut job: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    loop {
        if *shutdown.borrow() {
            return;
        }
        job().await;
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {}
        }
        if *shutdown.borrow() {
            return;
        }
    }
}

/// Spawn all background jobs over `app`, returning when all have stopped (after shutdown).
pub async fn run(app: AppState, shutdown: watch::Receiver<bool>) {
    let mut handles = Vec::new();

    // Scan task (sync_account + verify_acquisitions) — unchanged behaviour, own internal cadence.
    handles.push(tokio::spawn(run_scan_loop(
        ScanConfig { app: app.clone() },
        shutdown.clone(),
    )));

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
                        sync_trakt(trakt, &app.tmdb_client, &app.store).await;
                        reconcile_wanted(
                            app.engine.as_ref(),
                            &app.provider,
                            &app.tmdb_client,
                            &app.store,
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
                    )
                    .await;
                }
            },
        )));
    } else {
        info!("Trakt sync disabled (no Trakt client configured)");
    }

    for h in handles {
        if let Err(e) = h.await {
            tracing::error!("Background task ended abnormally: {:?}", e);
        }
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
        assert!(n >= 4 && n <= 5, "expected ~4 runs (initial + 3 ticks), got {}", n);
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
}
