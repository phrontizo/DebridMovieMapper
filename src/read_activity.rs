//! In-memory proxy read-activity tracker (SP3). The WebDAV read path stamps a path on every
//! `read_bytes`; the upgrade engine consults `is_idle` before swapping/pruning a slot so a file
//! that is being streamed is never pulled out from under a player. Best-effort and in-memory only:
//! after a restart everything reads idle (there are no pre-existing open handles to disturb).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

#[derive(Clone, Debug, Default)]
pub struct ReadActivity {
    last_read: Arc<RwLock<HashMap<String, Instant>>>,
}

impl ReadActivity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `path` was just read. Cheap; called on every proxy read.
    pub async fn touch(&self, path: &str) {
        self.last_read
            .write()
            .await
            .insert(path.to_string(), Instant::now());
    }

    /// `true` if `path` has had no read within `window` (never-read counts as idle).
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
}
