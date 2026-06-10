//! In-memory proxy read-activity tracker (SP3). The WebDAV read path stamps a path on every
//! `read_bytes`; the upgrade engine consults `all_idle` (a library-wide check) before
//! swapping/pruning a slot so a file that is being streamed is never pulled out from under a
//! player. Best-effort and in-memory only: after a restart everything reads idle (there are no
//! pre-existing open handles to disturb).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Soft cap on tracked paths. The active set (files currently streaming) is tiny; this only bounds
/// the accumulation of paths that were streamed once and never again — e.g. a file renamed by an
/// upgrade/repair. Without it the map grows for every distinct path *ever* streamed.
const MAX_TRACKED: usize = 4096;

#[derive(Clone, Debug, Default)]
pub struct ReadActivity {
    last_read: Arc<RwLock<HashMap<String, Instant>>>,
}

impl ReadActivity {
    pub fn new() -> Self {
        Self::default()
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
        map.insert(path.to_string(), Instant::now());
        if map.len() > MAX_TRACKED {
            let excess = map.len() - WATERMARK;
            let mut entries: Vec<(String, Instant)> =
                map.iter().map(|(k, v)| (k.clone(), *v)).collect();
            // Partition the `excess` oldest entries to the front in O(n) (no full sort needed).
            entries.select_nth_unstable_by_key(excess, |(_, t)| *t);
            for (k, _) in entries.into_iter().take(excess) {
                map.remove(&k);
            }
        }
    }

    /// `true` if `path` has had no read within `window` (never-read counts as idle). The per-path
    /// sibling of [`all_idle`](Self::all_idle); the upgrade engine deliberately gates on `all_idle`
    /// (library-wide) rather than this, so `is_idle` is retained as the per-path query for tests and
    /// a future per-slot idle gate. Kept intentionally; not dead code.
    pub async fn is_idle(&self, path: &str, window: Duration) -> bool {
        match self.last_read.read().await.get(path) {
            Some(t) => t.elapsed() >= window,
            None => true,
        }
    }

    /// The most recent read across all paths, if any.
    pub async fn most_recent(&self) -> Option<Instant> {
        self.last_read.read().await.values().copied().max()
    }

    /// `true` if NO path has been read within `window`.
    pub async fn all_idle(&self, window: Duration) -> bool {
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
        let ra = ReadActivity::new();
        assert!(ra.is_idle("Movies/X/x.mkv", Duration::from_secs(300)).await);
    }

    #[tokio::test]
    async fn just_touched_is_not_idle_then_becomes_idle() {
        let ra = ReadActivity::new();
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
        let ra = ReadActivity::new();
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

    #[tokio::test]
    async fn map_is_bounded_and_eviction_preserves_all_idle() {
        let ra = ReadActivity::new();
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
