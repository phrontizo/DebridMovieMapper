//! In-memory proxy read-activity tracker (SP3). The WebDAV read path stamps a path on every
//! `read_bytes`; every destructive pass (dedup removals, upgrade swaps/prunes, `execute_remove`, the
//! scan loop's same-hash duplicate deletion) consults `all_idle` (a library-wide check) first so a
//! file that is being streamed is never pulled out from under a player. Best-effort and in-memory
//! only — so after a restart there is no read history at all. To keep that from reading as "idle"
//! (and firing every idle-gated deletion on the first tick, as on 2026-11-11), a tracker created by
//! [`ReadActivity::new`] (start-up) reports NOT idle for the first idle window: a player that was
//! streaming before the restart gets a full window to reconnect and show up as activity.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Soft cap on tracked paths. The active set (files currently streaming) is tiny; this only bounds
/// the accumulation of paths that were streamed once and never again — e.g. a file renamed by an
/// upgrade/repair. Without it the map grows for every distinct path *ever* streamed.
const MAX_TRACKED: usize = 4096;

/// Evict the oldest entries from `map` until it is no larger than `watermark`, but ALWAYS keep at
/// least the single newest entry (so `all_idle`/`most_recent`, which key off the newest read, are
/// never disturbed). Split out from `touch` so the pivot-index bounds are unit-testable independently
/// of the shipped `MAX_TRACKED`: `select_nth_unstable_by_key(k, …)` panics if `k >= len`, and a
/// `watermark` of 0 (which `MAX_TRACKED <= 1` would produce) would otherwise make `excess == len`.
fn evict_to_watermark(map: &mut HashMap<String, Instant>, watermark: usize) {
    if map.len() <= watermark {
        return;
    }
    // Cap the number removed at len-1 so the newest entry survives and the pivot index stays
    // in-bounds even when `watermark == 0`.
    let excess = (map.len() - watermark).min(map.len() - 1);
    if excess == 0 {
        return;
    }
    let mut entries: Vec<(String, Instant)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    // Partition the `excess` oldest entries to the front in O(n) (no full sort needed).
    entries.select_nth_unstable_by_key(excess, |(_, t)| *t);
    for (k, _) in entries.into_iter().take(excess) {
        map.remove(&k);
    }
}

#[derive(Clone, Debug)]
pub struct ReadActivity {
    last_read: Arc<RwLock<HashMap<String, Instant>>>,
    /// When the tracker was created (≈ process start), or `None` for no start-up grace. Until one
    /// idle window has elapsed since then the library is never idle (see [`ReadActivity::new`]).
    started: Option<Instant>,
}

impl Default for ReadActivity {
    /// The production constructor — WITH the start-up grace (see [`ReadActivity::new`]).
    fn default() -> Self {
        Self::new()
    }
}

impl ReadActivity {
    /// A tracker for a freshly started process: for the first idle window after creation
    /// (`UPGRADE_IDLE_SECS`, the window callers pass to [`all_idle`](Self::all_idle)) the library
    /// counts as NOT idle, so dedup removals, upgrade swaps/prunes and `execute_remove` all defer
    /// rather than firing on the first tick after a restart (when the in-memory history is empty).
    pub fn new() -> Self {
        Self {
            last_read: Arc::default(),
            started: Some(Instant::now()),
        }
    }

    /// A tracker with NO start-up grace: idle from the start until something is read. For tests (and
    /// any caller that must treat a brand-new tracker as idle).
    pub fn without_startup_grace() -> Self {
        Self {
            last_read: Arc::default(),
            started: None,
        }
    }

    /// `true` while the start-up grace for `window` is still running.
    fn in_startup_grace(&self, window: Duration) -> bool {
        self.started.is_some_and(|t| t.elapsed() < window)
    }

    /// How much of the start-up grace for `window` is left (zero with no grace, or once it is over).
    /// A job that cannot do anything useful while the library is never idle — the daily upgrade pass,
    /// whose next tick would otherwise come a full interval later — sleeps this before its first run.
    pub fn startup_grace_remaining(&self, window: Duration) -> Duration {
        self.started
            .map_or(Duration::ZERO, |t| window.saturating_sub(t.elapsed()))
    }

    /// Record that `path` was just read. Cheap; called on every proxy read. When the map exceeds
    /// `MAX_TRACKED`, the least-recently-read entries are batch-evicted down to a lower watermark
    /// (3/4 of the cap) so the eviction pass amortizes over many inserts rather than re-running on
    /// every distinct path once at the cap. Eviction removes only the *oldest* entries, so
    /// `most_recent`/`all_idle` (which key off the newest read) are provably unaffected, and an
    /// evicted entry only ever reads *more* idle, never less.
    pub async fn touch(&self, path: &str) {
        const WATERMARK: usize = MAX_TRACKED * 3 / 4;
        let mut map = self.last_read.write().await;
        // The common case under rclone read-ahead is re-touching an already-tracked path: update it
        // in place (no `String` allocation, no eviction check). Only a brand-new path falls through.
        if let Some(t) = map.get_mut(path) {
            *t = Instant::now();
            return;
        }
        map.insert(path.to_string(), Instant::now());
        if map.len() > MAX_TRACKED {
            evict_to_watermark(&mut map, WATERMARK);
        }
    }

    /// `true` if `path` has had no read within `window` (never-read counts as idle once the start-up
    /// grace is over). The per-path
    /// sibling of [`all_idle`](Self::all_idle); the upgrade engine deliberately gates on `all_idle`
    /// (library-wide) rather than this. Currently exercised only by tests (all its callers are
    /// in-crate `#[cfg(test)]` modules), so it is gated `#[cfg(test)]` to keep the production API
    /// surface honest — promote it back to `pub` when a real per-slot idle gate lands.
    #[cfg(test)]
    pub async fn is_idle(&self, path: &str, window: Duration) -> bool {
        if self.in_startup_grace(window) {
            return false;
        }
        match self.last_read.read().await.get(path) {
            Some(t) => t.elapsed() >= window,
            None => true,
        }
    }

    /// The most recent read across all paths, if any.
    pub async fn most_recent(&self) -> Option<Instant> {
        self.last_read.read().await.values().copied().max()
    }

    /// `true` if NO path has been read within `window` — and the start-up grace (the first `window`
    /// after the tracker was created by [`ReadActivity::new`]) is over.
    pub async fn all_idle(&self, window: Duration) -> bool {
        if self.in_startup_grace(window) {
            return false;
        }
        match self.most_recent().await {
            Some(t) => t.elapsed() >= window,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn never_read_is_idle() {
        let ra = ReadActivity::without_startup_grace();
        assert!(ra.is_idle("Movies/X/x.mkv", Duration::from_secs(300)).await);
    }

    /// After a restart nothing has been read yet, so without a grace the whole library would count
    /// as idle on the very first tick and every idle-gated deletion would fire at once. For the first
    /// idle window after the tracker is created (process start) the library counts as NOT idle.
    #[tokio::test]
    async fn library_is_not_idle_during_the_startup_grace() {
        let ra = ReadActivity::new();
        assert!(
            !ra.all_idle(Duration::from_secs(300)).await,
            "within the first idle window after start-up the library is not idle"
        );
        assert!(!ra.is_idle("Movies/X/x.mkv", Duration::from_secs(300)).await);
        // The grace is exactly one idle window: a zero window has already elapsed.
        assert!(ra.all_idle(Duration::from_secs(0)).await);
        // Default is the production constructor (with the grace).
        assert!(
            !ReadActivity::default()
                .all_idle(Duration::from_secs(300))
                .await
        );
    }

    #[tokio::test]
    async fn startup_grace_ends_after_one_idle_window() {
        let ra = ReadActivity::new();
        let window = Duration::from_millis(50);
        assert!(!ra.all_idle(window).await);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            ra.all_idle(window).await,
            "idle once the window has passed with no reads"
        );
        // A read still resets idleness as usual.
        ra.touch("p").await;
        assert!(!ra.all_idle(window).await);
    }

    #[tokio::test]
    async fn without_startup_grace_is_idle_from_the_start() {
        let ra = ReadActivity::without_startup_grace();
        assert!(ra.all_idle(Duration::from_secs(300)).await);
    }

    /// `startup_grace_remaining` is what is left of the grace: about one window for a fresh tracker,
    /// zero without a grace or once it has run out — and waiting it out ends the grace, so a job
    /// that sleeps it before its first run (the upgrade job) finds the library idle.
    #[tokio::test]
    async fn startup_grace_remaining_counts_down_to_idle() {
        let window = Duration::from_secs(300);
        let left = ReadActivity::new().startup_grace_remaining(window);
        assert!(
            left <= window && left > window - Duration::from_secs(5),
            "a fresh tracker has about one window of grace left, got {left:?}"
        );
        assert_eq!(
            ReadActivity::without_startup_grace().startup_grace_remaining(window),
            Duration::ZERO
        );

        let ra = ReadActivity::new();
        let window = Duration::from_millis(50);
        let left = ra.startup_grace_remaining(window);
        assert!(!left.is_zero());
        assert!(!ra.all_idle(window).await);
        tokio::time::sleep(left).await;
        assert!(
            ra.all_idle(window).await,
            "after sleeping the remaining grace the library is idle"
        );
        assert_eq!(ra.startup_grace_remaining(window), Duration::ZERO);
    }

    #[tokio::test]
    async fn just_touched_is_not_idle_then_becomes_idle() {
        let ra = ReadActivity::without_startup_grace();
        ra.touch("p").await;
        assert!(
            !ra.is_idle("p", Duration::from_secs(300)).await,
            "just-read is active"
        );
        // A zero-length window makes any elapsed time count as idle.
        assert!(ra.is_idle("p", Duration::from_secs(0)).await);
    }

    #[tokio::test]
    async fn all_idle_is_library_wide_mirroring_is_idle() {
        let ra = ReadActivity::without_startup_grace();
        // No reads anywhere ⇒ the whole library is idle.
        assert!(ra.all_idle(Duration::from_secs(300)).await);
        assert!(ra.most_recent().await.is_none());

        // A read on ANY path makes the whole library active for a non-zero window…
        ra.touch("Movies/anything.mkv").await;
        assert!(
            !ra.all_idle(Duration::from_secs(300)).await,
            "a recent read anywhere is active"
        );
        assert!(ra.most_recent().await.is_some());
        // …and a zero-length window makes any elapsed time count as idle again.
        assert!(ra.all_idle(Duration::from_secs(0)).await);
    }

    #[test]
    fn eviction_does_not_panic_for_tiny_watermark() {
        // Latent-trap guard: if MAX_TRACKED is ever lowered so WATERMARK computes to 0, the pivot
        // index passed to select_nth_unstable_by_key must still stay < len. A watermark of 0 must
        // not panic and must keep at least one (the newest) entry rather than emptying the map.
        let mut two: HashMap<String, Instant> = HashMap::new();
        two.insert("a".into(), Instant::now());
        two.insert("b".into(), Instant::now());
        evict_to_watermark(&mut two, 0);
        assert_eq!(
            two.len(),
            1,
            "keeps exactly one entry (the newest); never panics or empties"
        );

        // A single-entry map with watermark 0 has nothing safe to evict → unchanged, no panic.
        let mut one: HashMap<String, Instant> = HashMap::new();
        one.insert("solo".into(), Instant::now());
        evict_to_watermark(&mut one, 0);
        assert_eq!(one.len(), 1);

        // Normal case still evicts down to the watermark exactly.
        let mut many: HashMap<String, Instant> = HashMap::new();
        for i in 0..10 {
            many.insert(format!("k{i}"), Instant::now());
        }
        evict_to_watermark(&mut many, 4);
        assert_eq!(many.len(), 4, "evicts down to the watermark");
    }

    #[tokio::test]
    async fn map_is_bounded_and_eviction_preserves_all_idle() {
        let ra = ReadActivity::without_startup_grace();
        // Stream far more distinct paths than the cap; the most-recent one is "live".
        for i in 0..(MAX_TRACKED + 500) {
            ra.touch(&format!("Movies/{i}/file.mkv")).await;
        }
        let len = ra.last_read.read().await.len();
        assert!(
            len <= MAX_TRACKED,
            "map must stay bounded at the cap, got {len}"
        );
        // The newest read is never evicted ⇒ the library still reads active for a non-zero window.
        assert!(
            !ra.all_idle(Duration::from_secs(300)).await,
            "the most-recent read must survive eviction so all_idle is unaffected"
        );
    }
}
