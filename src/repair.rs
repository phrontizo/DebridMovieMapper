use crate::provider::DebridProvider;
use crate::rd_client::TorrentInfo;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Minimum seconds between repair triggers for one torrent — bounds a per-read repair storm on a
/// still-broken-on-CDN cached file (a fresh re-add each read would otherwise hammer the provider).
const REPAIR_COOLDOWN_SECS: u64 = 30;
/// Maximum consecutive failed repairs before a torrent is marked permanently `Failed`. Counts only
/// *consecutive* failures — a confirmed good read (`note_read_success`) resets the budget.
const MAX_REPAIR_ATTEMPTS: u32 = 3;
/// Upper bound on one instant repair. It runs inline in a WebDAV read, but the provider calls inside
/// it use the background retry budgets (RD: 10 attempts with up to 30 s backoff on a 5xx; either
/// client: a 60 s per-request timeout), so during a provider outage it could otherwise hold the read
/// — and a connection permit — for minutes. A normal repair takes a few seconds.
const INSTANT_REPAIR_TIMEOUT: Duration = Duration::from_secs(30);
/// The waits before each retry of a 404 on one of the instant repair's torrent lookups (the torrent
/// under repair, the re-add right after it is added, the final check). TorBox's `mylist?id=`
/// sometimes answers 404 for a torrent that exists (21 times in ten minutes on 2026-11-13; see the
/// upgrade engine's longer `STAGED_INFO_BACKOFF`). Kept short because the read is waiting, and on
/// Real-Debrid a 404 means the torrent is gone: at most 1.5 s a lookup, 4.5 s a repair, well inside
/// `INSTANT_REPAIR_TIMEOUT`.
pub(crate) const REPAIR_INFO_NOT_FOUND_BACKOFF: [Duration; 2] =
    [Duration::from_millis(500), Duration::from_secs(1)];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairState {
    Healthy,
    Broken,
    Repairing,
    Failed,
}

#[derive(Debug, Clone)]
pub struct TorrentHealth {
    pub torrent_id: String,
    pub state: RepairState,
    pub repair_attempts: u32,
    /// When the cooldown (`REPAIR_COOLDOWN_SECS`) started. A tokio `Instant` (the same as
    /// `std::time::Instant` outside a paused test clock) so tests can run the cooldown against
    /// `INSTANT_REPAIR_TIMEOUT` on tokio's clock.
    pub last_repair_trigger: Option<tokio::time::Instant>,
}

/// Why an instant-repair attempt did not yield a usable cached replacement. Distinguishes the
/// NORMAL lapsed-cache outcome (the re-added torrent is (re)downloading — the scan loop surfaces it
/// once it caches) from an actual failure, so callers can pick the right log level without matching
/// on a human-readable message.
#[derive(Debug)]
pub enum RepairFailure {
    /// The re-added torrent is not cached yet — it is downloading. Not an error: `dav_fs` logs at
    /// info level and leaves it to the scan loop.
    NotCached(String),
    /// A TRANSIENT outcome that gives no verdict on the file: the re-add failed (an add HTTP error,
    /// or its file list not resolving in the poll window), or its result hadn't settled (the
    /// repaired torrent's `files` list momentarily lacked our path, a ~500ms settle blip, or the
    /// final info fetch failed), or the whole repair hit `INSTANT_REPAIR_TIMEOUT`. In each case the
    /// torrent stays `Healthy`/visible with its attempt budget reset and a later read retries, so
    /// `dav_fs` logs this at info level (like `NotCached`), NOT warn.
    Deferred(String),
    /// An actual repair failure (rate-limited, max attempts exceeded, a provider torrent-info error).
    /// `dav_fs` logs this at warn level.
    Failed(String),
}

impl std::fmt::Display for RepairFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepairFailure::NotCached(m) | RepairFailure::Deferred(m) | RepairFailure::Failed(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl From<String> for RepairFailure {
    /// An error string from `check_and_begin_repair` (a cooldown, attempt-cap, already-repairing
    /// or permanently-failed refusal), consumed via `?`, is an actual failure. (The transient
    /// `add_and_select_files` / final-info-fetch blips are mapped explicitly to `Deferred` at their
    /// call sites instead.)
    fn from(m: String) -> Self {
        RepairFailure::Failed(m)
    }
}

#[derive(Debug)]
pub struct RepairManager {
    health_status: Arc<RwLock<HashMap<String, TorrentHealth>>>,
    /// The active debrid provider (Real-Debrid or TorBox) — repair is provider-neutral.
    provider: Arc<dyn DebridProvider>,
    /// Maps new_torrent_id -> old_torrent_id for every new-id (Real-Debrid) repair: the cached one
    /// and the not-cached one whose re-add is left to download.
    /// The scan loop consumes this to reuse old TMDB identifications.
    repair_replacements: Arc<RwLock<HashMap<String, String>>>,
    /// Old torrent id -> (file path -> that file's locator in the torrent that replaced it), for a
    /// successful NEW-id (Real-Debrid) instant repair, which deletes the old torrent. The VFS keeps
    /// the old torrent's locators until the next scan rebuilds it, so `dav_fs` opens a file through
    /// [`RepairManager::redirect`]; otherwise every later open of any file of that torrent would
    /// resolve the deleted torrent and fail. A handle opened before the repair keeps the old locator
    /// past that rebuild, so `try_instant_repair` consults the redirect too. Dropped by
    /// `prune_health_status` once the listing has neither the old torrent nor the copy its chain of
    /// redirects ends at (followed at most `MAX_REDIRECT_HOPS` hops), and that copy isn't a
    /// replacement still waiting for the scan loop.
    repaired_locators: Arc<RwLock<HashMap<String, HashMap<String, crate::provider::FileLocator>>>>,
}

/// How many repairs in a row [`RepairManager::redirect`] follows. Each Real-Debrid re-add is a new
/// id, and a redirect lives as long as the copy its chain ends at is listed, so a chain can span
/// repairs separated by good reads (`note_read_success` resets the attempt budget, so
/// `MAX_REPAIR_ATTEMPTS` doesn't bound it). This cap does: `redirect` follows at most this many
/// hops. `prune_health_status` keeps a redirect while its old torrent is listed, or while the copy
/// this many hops along its chain (or the chain's end, if nearer) is listed or is a replacement
/// still waiting for the scan loop; otherwise it drops it.
const MAX_REDIRECT_HOPS: usize = 8;

/// RAII guard that makes the `Repairing` transition in `try_instant_repair` cancellation-safe.
///
/// `check_and_begin_repair` transitions a torrent to `Repairing`. `try_instant_repair` is awaited
/// INSIDE the WebDAV read future, so if the player/HTTP connection drops mid-repair the future is
/// dropped at an intermediate await before any outcome arm resets the state — leaving the torrent
/// stranded `Repairing`, whose reads `should_hide_torrent` refuses. Such a torrent can never
/// re-trigger repair (reads short-circuit on `should_hide_torrent`) and is not pruned while it is
/// still listed, so it would be trapped until process restart. On drop while
/// still armed, the guard resets a *still-`Repairing`* torrent back to `Healthy`, resets its
/// attempt budget (`repair_attempts` → 0) and restarts the cooldown, like a timed-out repair
/// (`note_abandoned_repair`): a repair that never finished is no evidence the file is broken,
/// and counting it would let three cancellations (with no good read between them) trap the torrent
/// `Failed`/hidden. A normal completion calls [`RepairInProgressGuard::disarm`], so on completion the
/// guard is a no-op (every exit path already sets a terminal state) and it only fires on
/// cancellation.
struct RepairInProgressGuard {
    health_status: Arc<RwLock<HashMap<String, TorrentHealth>>>,
    torrent_id: String,
    armed: bool,
}

impl RepairInProgressGuard {
    fn armed(
        health_status: Arc<RwLock<HashMap<String, TorrentHealth>>>,
        torrent_id: String,
    ) -> Self {
        Self {
            health_status,
            torrent_id,
            armed: true,
        }
    }

    /// Disarm the guard after a normal (non-cancelled) completion, whose own arms already set a
    /// terminal state.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RepairInProgressGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let health_status = Arc::clone(&self.health_status);
        let torrent_id = std::mem::take(&mut self.torrent_id);
        // `Drop` cannot await, so hand the reset to a detached task. It resets ONLY a still
        // `Repairing` entry, so a racing state change (another repair began, or a Broken/Failed
        // transition) is never clobbered. If there is no current runtime (e.g. during shutdown) the
        // in-memory health map no longer matters, so a failed spawn is harmless.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut map = health_status.write().await;
                if let Some(h) = map.get_mut(&torrent_id) {
                    if h.state == RepairState::Repairing {
                        h.state = RepairState::Healthy;
                        h.repair_attempts = 0;
                        // Restart the cooldown now, so it spaces the next attempt from when this
                        // one was abandoned (see `note_abandoned_repair`).
                        h.last_repair_trigger = Some(tokio::time::Instant::now());
                    }
                }
            });
        }
    }
}

/// Deletes the torrent an instant repair added but never adopted, when the repair is cut short (its
/// timeout fires, or the read awaiting it is dropped) while `materialise` or the steps after it are
/// mid-await. Their own failure cleanup only runs if the future completes, and a Real-Debrid
/// torrent left un-selected sits in `waiting_files_selection` for good: the scan loop's same-hash
/// dedup only collapses `downloaded` torrents.
///
/// `materialise` fills `slot` with the new id as soon as the add returns (never a same-id TorBox
/// re-add, which IS the torrent under repair). The repair takes the id back out of `slot` before it
/// adopts the add (records it as the replacement and has the superseded torrent deleted), so a
/// repair cut short after that point can't remove both copies; a repair that runs to completion
/// empties the slot too, having settled the add itself.
struct UnadoptedAddGuard {
    provider: Arc<dyn DebridProvider>,
    slot: Arc<std::sync::Mutex<Option<String>>>,
    /// The torrent under repair: never deleted here.
    keep: String,
}

impl Drop for UnadoptedAddGuard {
    fn drop(&mut self) {
        let Some(id) = take_unadopted(&self.slot) else {
            return;
        };
        if id == self.keep {
            return;
        }
        // `Drop` cannot await, so delete from a detached task (the read has given up already).
        // Without a runtime (shutdown) the delete is skipped and the add is left behind.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let provider = Arc::clone(&self.provider);
            handle.spawn(async move {
                warn!(
                    "Instant repair was cut short; deleting the torrent {} it added",
                    id
                );
                if let Err(e) = provider.delete_torrent(&id).await {
                    error!(
                        "Failed to delete torrent {} after a cut-short repair: {}",
                        id, e
                    );
                }
            });
        }
    }
}

/// Empty an [`UnadoptedAddGuard`] slot, returning the id it held.
fn take_unadopted(slot: &std::sync::Mutex<Option<String>>) -> Option<String> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
}

impl RepairManager {
    pub fn new(provider: Arc<dyn DebridProvider>) -> Self {
        Self {
            health_status: Arc::new(RwLock::new(HashMap::new())),
            provider,
            repair_replacements: Arc::new(RwLock::new(HashMap::new())),
            repaired_locators: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The locator that now serves `locator`'s file when an instant repair replaced its torrent
    /// under a new id (Real-Debrid) since the VFS was built, following repeated repairs to the
    /// latest copy. `None` when the torrent wasn't replaced: the locator is still current.
    pub async fn redirect(
        &self,
        locator: &crate::provider::FileLocator,
    ) -> Option<crate::provider::FileLocator> {
        let map = self.repaired_locators.read().await;
        let mut current: Option<&crate::provider::FileLocator> = None;
        let mut torrent_id = locator.torrent_id.as_str();
        for _ in 0..MAX_REDIRECT_HOPS {
            match map
                .get(torrent_id)
                .and_then(|files| files.get(&locator.file_path))
            {
                Some(next) => {
                    torrent_id = next.torrent_id.as_str();
                    current = Some(next);
                }
                None => break,
            }
        }
        current.cloned()
    }

    /// Record that `new_info` (downloaded) replaced the torrent `old_torrent_id`, so
    /// [`Self::redirect`] maps each of its selected files onto `new_info`.
    async fn record_redirect(&self, old_torrent_id: &str, new_info: &TorrentInfo, hash: &str) {
        let files: HashMap<String, crate::provider::FileLocator> = new_info
            .files
            .iter()
            .filter(|f| f.selected == 1)
            .filter_map(|f| {
                Self::locator_for_file(new_info, hash, &f.path).map(|l| (f.path.clone(), l))
            })
            .collect();
        self.repaired_locators
            .write()
            .await
            .insert(old_torrent_id.to_string(), files);
    }

    /// Drains and returns the repair replacements map (new_id -> old_id).
    /// After calling this, the internal map is empty.
    pub async fn take_repair_replacements(&self) -> HashMap<String, String> {
        let mut map = self.repair_replacements.write().await;
        std::mem::take(&mut *map)
    }

    /// Put back replacements [`Self::take_repair_replacements`] returned to a scan tick that then
    /// used none of them (its listing failed, or was a suspect empty one), so the next tick still
    /// reuses each old identification. An entry recorded for the same new id since the take wins.
    pub async fn restore_repair_replacements(&self, taken: HashMap<String, String>) {
        if taken.is_empty() {
            return;
        }
        let mut map = self.repair_replacements.write().await;
        for (new_id, old_id) in taken {
            map.entry(new_id).or_insert(old_id);
        }
    }

    /// Delete a torrent that was created by add_magnet but whose repair failed.
    /// Prevents duplicate torrents from accumulating in the debrid account.
    async fn cleanup_leaked_torrent(&self, new_torrent_id: &str) {
        warn!(
            "Cleaning up leaked torrent {} after failed repair",
            new_torrent_id
        );
        if let Err(e) = self.provider.delete_torrent(new_torrent_id).await {
            error!(
                "Failed to clean up leaked torrent {}: {}",
                new_torrent_id, e
            );
        }
    }

    /// Called when a byte read SUCCEEDS, confirming the file genuinely works. Resets the torrent's
    /// repair budget (`repair_attempts` → 0, cooldown cleared) so the 3-attempt cap counts only
    /// CONSECUTIVE failed repairs (an actual storm on a still-broken file), not unrelated repair
    /// incidents accumulated over a long-running deployment. `complete_repair` carries
    /// `repair_attempts` forward across re-adds to bound a storm; without this reset on a confirmed
    /// good read, that carry would become a permanent lifetime cap — a file legitimately repaired 3
    /// separate times would be wrongly promoted to `Failed`/hidden on its 4th genuine incident. A
    /// genuinely-broken file never gets a successful read, so it still converges to `Failed`.
    ///
    /// Fast-path read lock: the common case (a clean torrent with no repair history) takes no write
    /// lock, so calling this per opened file is cheap.
    pub async fn note_read_success(&self, torrent_id: &str) {
        {
            let health_map = self.health_status.read().await;
            match health_map.get(torrent_id) {
                // Only reset from `Healthy`. Skipping `Repairing` avoids clearing an in-flight
                // repair's guard/cooldown (its own completion path resets the budget). Skipping
                // `Broken`/`Failed` is the important guard: a read that resolved BEFORE the non-cached
                // repair path deleted+marked-Broken the old id could complete successfully and call
                // this — flipping Broken→Healthy would UN-HIDE a torrent that no longer exists on the
                // provider (`should_hide_torrent`→false → next read re-resolves a deleted id → repair
                // churn). The state must never be resurrected here.
                Some(h)
                    if h.state == RepairState::Healthy
                        && (h.repair_attempts > 0 || h.last_repair_trigger.is_some()) => {}
                _ => return, // no entry, not Healthy, or already a clean budget — nothing to reset
            }
        }
        let mut health_map = self.health_status.write().await;
        if let Some(h) = health_map.get_mut(torrent_id) {
            // Re-check under the write lock: another task may have transitioned the state (begun a
            // repair, or marked it Broken) between the read and write locks.
            if h.state != RepairState::Healthy {
                return;
            }
            h.repair_attempts = 0;
            h.last_repair_trigger = None;
        }
    }

    /// Reset a torrent to `Healthy` after a repair attempt (or path-mismatch) that produced NO usable
    /// replacement to swap in — a transient re-add/info/listing error, the re-added file list not
    /// resolving, or a `downloaded` re-add whose file path momentarily isn't listed. It keeps the
    /// torrent VISIBLE (`read_bytes` short-circuits on `should_hide_torrent` *before* any I/O, so a
    /// `Broken`/`Repairing`/`Failed` torrent can never re-trigger repair — leaving it hidden with no
    /// recorded replacement traps it until restart) AND resets `repair_attempts` to 0: a transient
    /// failure is not evidence the file is broken, so it must not accumulate toward the 3-attempt
    /// `Failed` cap. Without the reset, three cooldown-spaced reads with no good read between would
    /// promote a same-id (TorBox) torrent to `Failed`/hidden — unrecoverable (hidden → no reads → no
    /// `note_read_success` → never reset; still listed → never pruned), the very trap the cached and
    /// uncached same-id resets prevent. The 30s cooldown (`last_repair_trigger`) is PRESERVED, so
    /// retries stay rate-limited to one per 30s even though the budget no longer climbs.
    ///
    /// (`Broken`/`Failed` is set ONLY where a replacement IS recorded for the scan loop to swap in —
    /// the not-cached, new-id path — so the hidden original is correctly superseded.)
    async fn note_transient_repair_failure(&self, torrent_id: &str) {
        let mut health_map = self.health_status.write().await;
        if let Some(health) = health_map.get_mut(torrent_id) {
            health.state = RepairState::Healthy;
            health.repair_attempts = 0;
        }
    }

    /// [`Self::note_transient_repair_failure`] for a repair cut short by `INSTANT_REPAIR_TIMEOUT`,
    /// which also RESTARTS the cooldown. The trigger was stamped when the repair began, and the
    /// timeout is as long as the cooldown, so keeping it would leave no cooldown at all: during an
    /// outage each retry of the failed read (rclone retries a 500) would at once hold another full
    /// timeout on the hung provider, and spend another re-add. The guard on a dropped read does the
    /// same (`RepairInProgressGuard`).
    async fn note_abandoned_repair(&self, torrent_id: &str) {
        let mut health_map = self.health_status.write().await;
        if let Some(health) = health_map.get_mut(torrent_id) {
            health.state = RepairState::Healthy;
            health.repair_attempts = 0;
            health.last_repair_trigger = Some(tokio::time::Instant::now());
        }
    }

    /// Check pre-repair guards (Failed/Repairing/rate-limited) and transition to Repairing state.
    /// Returns the attempt number on success, or Err with the reason why repair cannot proceed.
    async fn check_and_begin_repair(&self, torrent_id: &str) -> Result<u32, String> {
        // Read-side guard: check state without holding write lock
        {
            let health_map = self.health_status.read().await;
            if let Some(health) = health_map.get(torrent_id) {
                if health.state == RepairState::Failed {
                    debug!(
                        "Torrent {} has permanently failed repair, skipping",
                        torrent_id
                    );
                    return Err("Torrent permanently failed".to_string());
                }
                if health.state == RepairState::Repairing {
                    debug!(
                        "Repair already in progress for torrent {}, skipping",
                        torrent_id
                    );
                    return Err("Repair already in progress".to_string());
                }
                if let Some(last_trigger) = health.last_repair_trigger {
                    if last_trigger.elapsed().as_secs() < REPAIR_COOLDOWN_SECS {
                        debug!(
                            "Repair recently triggered for torrent {} ({}s ago), skipping",
                            torrent_id,
                            last_trigger.elapsed().as_secs()
                        );
                        return Err("Repair rate limited".to_string());
                    }
                }
            }
        }

        // Write-side: set state to Repairing and increment attempt count
        let mut health_map = self.health_status.write().await;
        let attempt_num = if let Some(health) = health_map.get_mut(torrent_id) {
            // Double-check all guards FIRST: another task might have changed state or started a
            // repair between the read and write lock acquisitions. These re-checks must precede the
            // attempts-cap promotion below — otherwise a concurrent racer that finds
            // `repair_attempts == MAX` while another task's repair is legitimately in-flight
            // (`Repairing`) would spuriously flip that in-flight torrent to `Failed`, which the
            // state machine reserves for MAX *completed* failed attempts.
            if health.state == RepairState::Failed {
                return Err("Torrent permanently failed".to_string());
            }
            if health.state == RepairState::Repairing {
                return Err("Repair already in progress".to_string());
            }
            if let Some(last_trigger) = health.last_repair_trigger {
                if last_trigger.elapsed().as_secs() < REPAIR_COOLDOWN_SECS {
                    return Err("Repair rate limited".to_string());
                }
            }
            if health.repair_attempts >= MAX_REPAIR_ATTEMPTS {
                error!(
                    "Torrent {} has failed repair {} times, marking as permanently FAILED",
                    torrent_id, MAX_REPAIR_ATTEMPTS
                );
                health.state = RepairState::Failed;
                return Err("Maximum repair attempts exceeded".to_string());
            }
            health.state = RepairState::Repairing;
            health.repair_attempts += 1;
            health.last_repair_trigger = Some(tokio::time::Instant::now());
            health.repair_attempts
        } else {
            health_map.insert(
                torrent_id.to_string(),
                TorrentHealth {
                    torrent_id: torrent_id.to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 1,
                    last_repair_trigger: Some(tokio::time::Instant::now()),
                },
            );
            1
        };

        Ok(attempt_num)
    }

    /// Add magnet, wait for the provider to process, get new torrent info, match files by path, select them.
    /// Returns (new_torrent_id, new_torrent_info) on success.
    /// On failure, `reacquire::materialise` deletes a leaked NEW-id torrent (never `old_torrent_id`,
    /// passed as `protect_id`), and the old torrent is kept visible (`Healthy`) with its attempt
    /// budget reset (`note_transient_repair_failure`). The new id goes into `added` as soon as the add
    /// returns, for [`UnadoptedAddGuard`].
    async fn add_and_select_files(
        &self,
        old_torrent_id: &str,
        old_info: &TorrentInfo,
        wait_duration: Duration,
        added: &std::sync::Mutex<Option<String>>,
    ) -> Result<(String, TorrentInfo), String> {
        // Selector: match the new torrent's files to the old torrent's previously-selected paths.
        let old_info_cl = old_info.clone();
        let select = move |new_info: &TorrentInfo| -> Vec<u32> {
            old_info_cl
                .files
                .iter()
                .filter(|f| f.selected == 1)
                .filter_map(|of| {
                    new_info
                        .files
                        .iter()
                        .find(|nf| nf.path == of.path)
                        .map(|nf| nf.id)
                })
                .collect()
        };
        // Repair keeps the single-poll behaviour (max_wait == settle): the re-added torrent is a
        // known-good hash whose metadata should already be available. `protect_id = old_torrent_id`
        // so that on a same-id re-add (TorBox recovers the existing torrent by infohash) a transient
        // post-add failure NEVER deletes the real torrent.
        match crate::reacquire::materialise(
            &*self.provider,
            &old_info.hash,
            wait_duration,
            wait_duration,
            Some(old_torrent_id),
            Some(added),
            select,
        )
        .await
        {
            Ok(pair) => Ok(pair),
            Err(e) => {
                // A materialise failure is transient (re-add HTTP error, or the file list not
                // resolving within the poll window) and produced NO replacement to swap in — keep
                // the torrent VISIBLE (Healthy) so the next read can re-attempt repair under the
                // cooldown. Marking it Broken would hide it, and a hidden torrent can never
                // re-trigger repair (reads short-circuit on `should_hide_torrent`) → it would be
                // trapped until restart. A transient error is not a "broken file" signal, so reset
                // the attempt budget too — critical for a same-id (TorBox) re-add where `old` IS the
                // real, present torrent and a Failed cap-out would be unrecoverable.
                self.note_transient_repair_failure(old_torrent_id).await;
                Err(e.to_string())
            }
        }
    }

    /// Finalise a successful repair: (for a genuinely new torrent) record the replacement, update the
    /// health map to track the repaired torrent, then delete the old one off the timed repair
    /// ([`Self::delete_superseded`]).
    ///
    /// `old == new` when the provider's re-add-by-hash returns the SAME torrent id — TorBox's
    /// `createtorrent` recovers the existing entry by infohash, so a repaired-in-place torrent keeps
    /// its id. In that case we must NOT delete it (that would destroy the very torrent we repaired)
    /// and there is no replacement to record.
    ///
    /// The repaired torrent's health carries the old entry's `repair_attempts` and a FRESH cooldown
    /// trigger forward, so the 30s cooldown and 3-attempt cap survive across re-adds. Real-Debrid
    /// mints a NEW id on every re-add; without carrying state, each "successful" repair of a
    /// still-broken-on-CDN cached file would land on a fresh, trigger-less, attempts-0 health entry,
    /// so the cooldown never applied → an unbounded re-add/delete storm on every byte read. On a
    /// genuinely-fixed file there is no further 5xx, so the carried cooldown is harmless.
    async fn complete_repair(&self, old_torrent_id: &str, new_torrent_id: &str) {
        let same = old_torrent_id == new_torrent_id;
        if !same {
            // Record the replacement so the scan loop reuses the old TMDB identification. Recorded
            // BEFORE the new id's health entry: while the scan loop hasn't taken it,
            // `prune_health_status` keeps that entry even though its listing predates the new id.
            self.repair_replacements
                .write()
                .await
                .insert(new_torrent_id.to_string(), old_torrent_id.to_string());
        }

        let mut health_map = self.health_status.write().await;
        // Carry the attempt budget forward ONLY for a new-id (Real-Debrid) re-add, so the 30s
        // cooldown + 3-attempt cap bound RD's per-read new-id storm. For a same-id (TorBox) re-add
        // this success path is reached only when the file IS present and cached (`locator_for_file`
        // matched), so a still-failing byte read is a transient CDN issue, not missing content —
        // RESET the budget so it can never accumulate to the Failed/hidden state, which is
        // unrecoverable for a same-id torrent (hidden → no reads → `note_read_success` never fires).
        // The fresh `last_repair_trigger` below still rate-limits re-adds to one per 30s.
        let carried_attempts = if same {
            0
        } else {
            health_map
                .get(old_torrent_id)
                .map(|h| h.repair_attempts)
                .unwrap_or(0)
        };
        if !same {
            health_map.remove(old_torrent_id);
        }
        health_map.insert(
            new_torrent_id.to_string(),
            TorrentHealth {
                torrent_id: new_torrent_id.to_string(),
                state: RepairState::Healthy,
                repair_attempts: carried_attempts,
                last_repair_trigger: Some(tokio::time::Instant::now()),
            },
        );
        drop(health_map);
        if !same {
            self.delete_superseded(old_torrent_id);
        }
    }

    /// Delete the torrent a new-id (Real-Debrid) repair replaced, from a detached task, once the
    /// repair has recorded the replacement and settled the health map. Awaiting the delete inside
    /// the repair would put it under `INSTANT_REPAIR_TIMEOUT`, and Real-Debrid's delete runs the
    /// full background retry budget: a slow or 5xx-ing delete could then cut off a repair that had
    /// already adopted its add, losing the replacement record and the attempt budget carried to the
    /// new id, and failing a read the new copy could serve. A failed delete only logs, as it did
    /// when the repair awaited it.
    fn delete_superseded(&self, old_torrent_id: &str) {
        let provider = Arc::clone(&self.provider);
        let old = old_torrent_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = provider.delete_torrent(&old).await {
                warn!("Failed to delete old torrent {}: {}", old, e);
            }
        });
    }

    /// Build a `FileLocator` for the file at `file_path` within `info`
    /// (`acquire::locator_for_selected`, over the one copy of the pairing,
    /// `vfs::selected_files_with_links`). The per-file restricted link is paired by position among
    /// selected files (Real-Debrid); for providers with no per-file link array (TorBox) `links` is
    /// empty so `link` is `None` and the file is addressed by `(torrent_id, file_id)`. Returns
    /// `None` if no selected file matches `file_path`.
    fn locator_for_file(
        info: &TorrentInfo,
        hash: &str,
        file_path: &str,
    ) -> Option<crate::provider::FileLocator> {
        crate::acquire::locator_for_selected(info, hash, file_path)
    }

    /// Attempt instant repair (re-acquire) for a broken/uncached file. Re-adds the
    /// torrent by hash; if the replacement is immediately available, returns a fresh
    /// `FileLocator` for the SAME file (matched by `file_path`). Returns `Err` if the
    /// torrent needs downloading or the repair fails.
    ///
    /// When the locator's torrent was already replaced by an earlier new-id repair (a handle opened
    /// before another handle's Real-Debrid repair still holds the old locator), the copy recorded
    /// for that file ([`Self::redirect`]) is returned without repairing the deleted torrent again.
    /// If that copy is itself broken, the next failure carries its id, which has no redirect, and
    /// is repaired under the usual cooldown and attempt cap.
    pub async fn try_instant_repair(
        &self,
        locator: &crate::provider::FileLocator,
    ) -> Result<crate::provider::FileLocator, RepairFailure> {
        if let Some(next) = self.redirect(locator).await {
            info!(
                "Instant repair: torrent {} was already repaired as {}; using the repaired copy",
                locator.torrent_id, next.torrent_id
            );
            return Ok(next);
        }
        let torrent_id = locator.torrent_id.as_str();
        let attempt_num = self.check_and_begin_repair(torrent_id).await?;
        info!(
            "Instant repair attempt #{} for torrent {}",
            attempt_num, torrent_id
        );

        // Cancellation safety: `check_and_begin_repair` transitioned the torrent to `Repairing`.
        // Because this future is awaited inside the WebDAV read path, a dropped connection would
        // otherwise strand it `Repairing`/hidden with no way back. The guard resets a still
        // `Repairing` torrent on drop; a normal completion disarms it (see `RepairInProgressGuard`).
        let mut guard =
            RepairInProgressGuard::armed(Arc::clone(&self.health_status), torrent_id.to_string());
        // The same cancellation (or the timeout below) can strand a NEW-id torrent the repair added
        // but never adopted; this guard deletes it on drop (see `UnadoptedAddGuard`).
        let added = Arc::new(std::sync::Mutex::new(None));
        let _add_guard = UnadoptedAddGuard {
            provider: Arc::clone(&self.provider),
            slot: Arc::clone(&added),
            keep: torrent_id.to_string(),
        };
        // Bound the whole repair: the read that triggered it is waiting (see
        // `INSTANT_REPAIR_TIMEOUT`). On timeout the inner future is dropped at an await, which is
        // the same cancellation the guards cover: `_add_guard` deletes a new-id torrent added but not
        // adopted when this function returns, and the attempt budget is reset here, as the state
        // guard does on a dropped read, so repeated timeouts during an outage can't climb to
        // `MAX_REPAIR_ATTEMPTS` and trap the torrent `Failed`/hidden. The cooldown restarts too, so
        // the next read waits a full `REPAIR_COOLDOWN_SECS` before trying again
        // (`note_abandoned_repair`).
        let result = match tokio::time::timeout(
            INSTANT_REPAIR_TIMEOUT,
            self.instant_repair_inner(locator, torrent_id, &added),
        )
        .await
        {
            Ok(result) => {
                // Ran to completion, so it settled any torrent it added itself (adopted it, or
                // deleted it on failure): nothing for the guard to clean up.
                take_unadopted(&added);
                result
            }
            Err(_) => {
                self.note_abandoned_repair(torrent_id).await;
                Err(RepairFailure::Deferred(format!(
                    "instant repair timed out after {}s",
                    INSTANT_REPAIR_TIMEOUT.as_secs()
                )))
            }
        };
        guard.disarm();
        result
    }

    /// The body of [`RepairManager::try_instant_repair`] after the `Repairing` transition, split out
    /// so the outer method can wrap it in a cancellation-safety guard. Every exit path sets a
    /// terminal health state, so the guard is a no-op unless this future is cancelled mid-await.
    /// `added` holds a new-id torrent the repair added and has not adopted yet (see
    /// `UnadoptedAddGuard`); it is emptied before each step that adopts the add.
    async fn instant_repair_inner(
        &self,
        locator: &crate::provider::FileLocator,
        torrent_id: &str,
        added: &std::sync::Mutex<Option<String>>,
    ) -> Result<crate::provider::FileLocator, RepairFailure> {
        // Fetch old torrent info to know which files were selected (for re-selection). Every lookup
        // here retries TorBox's spurious 404 briefly (`REPAIR_INFO_NOT_FOUND_BACKOFF`).
        let old_info = match crate::provider::torrent_info_retrying_not_found(
            &*self.provider,
            torrent_id,
            &REPAIR_INFO_NOT_FOUND_BACKOFF,
        )
        .await
        {
            Ok(info) => info,
            Err(e) => {
                // Transient torrent-info failure, no replacement produced — keep the torrent VISIBLE
                // (Healthy) so the next read can re-attempt repair (Broken would hide it with no way
                // back). Reset the attempt budget (a transient error is not a broken-file signal),
                // preserving the cooldown; see `note_transient_repair_failure`.
                self.note_transient_repair_failure(torrent_id).await;
                return Err(RepairFailure::Failed(format!(
                    "Failed to get torrent info: {}",
                    e
                )));
            }
        };

        info!("Instant repair: adding magnet for hash {}", old_info.hash);
        let (new_torrent_id, _new_info) = match self
            .add_and_select_files(torrent_id, &old_info, Duration::from_millis(500), added)
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                // A materialise failure (re-add HTTP error, or the file list not resolving in the
                // poll window) is transient and produced NO replacement; `add_and_select_files`
                // already reset the attempt budget and kept the torrent visible. Classify as
                // `Deferred` (dav_fs logs it at info) — like the settle-blip / info-fetch arms below
                // — rather than `Failed` (warn): a self-healing retry isn't a genuine failure.
                return Err(RepairFailure::Deferred(e));
            }
        };
        info!("Instant repair: new torrent ID {}", new_torrent_id);

        // TorBox re-adds by infohash return the SAME torrent id (createtorrent recovers the existing
        // entry), so the re-added torrent IS the one under repair — never delete/clean it up as a
        // "leaked" duplicate. Real-Debrid mints a fresh id, so its cleanup/delete paths are real.
        let same_torrent = new_torrent_id == torrent_id;

        // Brief wait for the provider to process file selection.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let final_info = match crate::provider::torrent_info_retrying_not_found(
            &*self.provider,
            &new_torrent_id,
            &REPAIR_INFO_NOT_FOUND_BACKOFF,
        )
        .await
        {
            Ok(info) => info,
            Err(e) => {
                // The re-add already succeeded; only the final info fetch blipped (transient). There
                // is NO usable replacement to swap in, so keep the original torrent VISIBLE (Healthy)
                // and reset the attempt budget (preserving the cooldown) — Broken would hide it with
                // no replacement, and accumulating toward Failed on a transient error would trap a
                // same-id torrent. For a genuinely-new id (RD) also delete the leaked replacement.
                if !same_torrent {
                    self.cleanup_leaked_torrent(&new_torrent_id).await;
                }
                self.note_transient_repair_failure(torrent_id).await;
                // Transient (the re-add succeeded; only the final info fetch blipped) with the budget
                // reset and the torrent left visible — classify as `Deferred` (info), matching the
                // settle-blip / materialise arms, not `Failed` (warn).
                return Err(RepairFailure::Deferred(format!(
                    "Failed to get final torrent info: {}",
                    e
                )));
            }
        };

        if final_info.status == "downloaded" {
            // Match the SAME file by path (provider-neutral; no positional link index).
            match Self::locator_for_file(&final_info, &locator.hash, &locator.file_path) {
                Some(new_locator) => {
                    // Adopting the add: from here a cut-short repair must not delete it, since
                    // `complete_repair` records it as the replacement and has the superseded
                    // torrent deleted.
                    take_unadopted(added);
                    if !same_torrent {
                        // Before the old torrent is deleted, so no open in between resolves it.
                        self.record_redirect(torrent_id, &final_info, &locator.hash)
                            .await;
                    }
                    self.complete_repair(torrent_id, &new_torrent_id).await;
                    info!(
                        "Instant repair SUCCEEDED for torrent {} -> new ID {} (file {})",
                        torrent_id, new_torrent_id, locator.file_path
                    );
                    Ok(new_locator)
                }
                None => {
                    // The re-added torrent is `downloaded` but our `file_path` isn't in it. This is
                    // usually a TRANSIENT settle blip — `final_info` is fetched only ~500ms after the
                    // add, and TorBox's `files` field is loose/momentarily-empty — not genuinely
                    // missing content. Keep the ORIGINAL torrent VISIBLE and RESET its attempt budget
                    // (`note_transient_repair_failure`), exactly like the other transient arms
                    // (info-fetch / materialise failure): trapping it `Failed`/hidden with no
                    // replacement is unrecoverable (hidden → no reads → `note_read_success` never
                    // fires → never reset; still listed → never pruned), and a SAME-id (TorBox)
                    // re-add does NOT itself reset attempts (only a confirmed good read via
                    // `note_read_success` does), so three cooldown-spaced reads would still hit the
                    // attempt cap and trap the real torrent. The cooldown still rate-limits retries.
                    // Accepted downside for a genuinely-NEW-id provider (RD): a PERSISTENT
                    // path-mismatch (rare — this is normally a settle blip) never accumulates toward
                    // the give-up cap, so it re-adds+deletes once per cooldown indefinitely rather
                    // than giving up. The alternative (a new-id-only give-up counter) would trade that
                    // rare churn for the unrecoverable Failed/hidden trap above on the COMMON transient
                    // case, which is strictly worse — so the reset is kept.
                    // For a genuinely-new id (RD) also delete the
                    // leaked replacement (TorBox's same-id re-add IS the real torrent — never delete).
                    if !same_torrent {
                        self.cleanup_leaked_torrent(&new_torrent_id).await;
                    }
                    self.note_transient_repair_failure(torrent_id).await;
                    // A settle blip, not a genuine failure (budget reset above, a later read
                    // retries) — classify as Deferred so `dav_fs` logs it at info, not warn.
                    Err(RepairFailure::Deferred(format!(
                        "Repaired torrent missing file path {} (not yet settled)",
                        locator.file_path
                    )))
                }
            }
        } else {
            // Not cached -- torrent needs actual download.
            info!(
                "Torrent {} not cached (status: {}), leaving new torrent {} to download",
                torrent_id, final_info.status, new_torrent_id
            );

            // For TorBox (same id) the re-add IS this torrent — deleting it would remove the very
            // download we're leaving to complete. Only the genuinely-new-id (RD) case deletes the
            // old broken torrent and records a replacement for the scan loop.
            if !same_torrent {
                // The add becomes the replacement left to download: a cut-short repair must not
                // delete it as well as the superseded torrent deleted below.
                take_unadopted(added);
                self.repair_replacements
                    .write()
                    .await
                    .insert(new_torrent_id.to_string(), torrent_id.to_string());
            }

            let mut health_map = self.health_status.write().await;
            if let Some(health) = health_map.get_mut(torrent_id) {
                if same_torrent {
                    // TorBox (same id): this IS the real, re-downloading torrent. Keep it Healthy so
                    // the normal scan surfaces it once it caches. A re-download in progress is NOT a
                    // failed repair, so RESET the attempt budget — otherwise a slow re-cache (a large
                    // 4K file taking >~90s) accumulates `repair_attempts` across the cooldown-spaced
                    // read retries and `check_and_begin_repair` promotes it to `Failed`/hidden, which
                    // is UNRECOVERABLE: hidden → no reads → no `note_read_success`, and a `Failed`
                    // state short-circuits `check_and_begin_repair` (so no Healthy-setter is ever
                    // reached) while the still-listed torrent is never pruned. The 30s cooldown
                    // (`last_repair_trigger`, preserved) still bounds the retry rate.
                    health.state = RepairState::Healthy;
                    health.repair_attempts = 0;
                } else {
                    // RD (new id): the old torrent is superseded → Broken (hidden until the scan loop
                    // picks up the recorded replacement).
                    health.state = RepairState::Broken;
                }
            }
            drop(health_map);
            if !same_torrent {
                // Only now, with the replacement recorded and the old torrent Broken, and off the
                // timed repair (see `delete_superseded`): a timeout during an awaited delete would
                // have lost the replacement and flipped the old torrent back to Healthy
                // (`note_abandoned_repair`).
                self.delete_superseded(torrent_id);
            }

            Err(RepairFailure::NotCached(format!(
                "Torrent not cached (status: {}), needs download",
                final_info.status
            )))
        }
    }

    /// Check if a torrent should be hidden from WebDAV
    pub async fn should_hide_torrent(&self, torrent_id: &str) -> bool {
        let health_map = self.health_status.read().await;
        if let Some(health) = health_map.get(torrent_id) {
            matches!(
                health.state,
                RepairState::Broken | RepairState::Repairing | RepairState::Failed
            )
        } else {
            false
        }
    }

    /// Return the set of torrent IDs the VFS leaves out (`Broken` or `Failed`); the scan loop also
    /// keeps them out of the selection repoint's candidates. This acquires the read lock once
    /// instead of per-torrent, which is significantly faster when filtering hundreds of torrents
    /// during VFS updates.
    ///
    /// Unlike [`Self::should_hide_torrent`], a torrent mid-repair (`Repairing`) is NOT included:
    /// a repair takes a few seconds (up to `INSTANT_REPAIR_TIMEOUT`), and a scan tick landing inside
    /// it would otherwise drop the torrent's files from the library until the next tick, telling
    /// Jellyfin they were deleted, even when the repair succeeds a moment later. Its reads are still
    /// refused while the repair runs (`should_hide_torrent`).
    pub async fn hidden_torrent_ids(&self) -> std::collections::HashSet<String> {
        let health_map = self.health_status.read().await;
        health_map
            .values()
            .filter(|h| matches!(h.state, RepairState::Broken | RepairState::Failed))
            .map(|h| h.torrent_id.clone())
            .collect()
    }

    /// Summary of repair status as `(healthy, repairing, failed)`, where "repairing" counts both
    /// `Broken` and `Repairing`. Used by `repair_integration_test` to assert post-repair state.
    pub async fn get_status_summary(&self) -> (usize, usize, usize) {
        let health_map = self.health_status.read().await;
        let healthy = health_map
            .values()
            .filter(|h| h.state == RepairState::Healthy)
            .count();
        let repairing = health_map
            .values()
            .filter(|h| matches!(h.state, RepairState::Broken | RepairState::Repairing))
            .count();
        let failed = health_map
            .values()
            .filter(|h| h.state == RepairState::Failed)
            .count();
        (healthy, repairing, failed)
    }

    /// Remove health_status entries for torrent IDs that are no longer active.
    /// This prevents unbounded growth of the health_status map over time.
    ///
    /// `active_torrent_ids` is the listing the scan loop fetched at the start of its tick, so a
    /// Real-Debrid repair that completed since then has a new id it lacks. That entry carries the
    /// cooldown and the attempt budget across the re-add, so it is kept while its replacement is
    /// still waiting for the scan loop (`take_repair_replacements`, which runs before the next
    /// listing). A repair redirect is dropped once the listing has neither its old torrent nor the
    /// copy its chain of redirects ends at (followed at most `MAX_REDIRECT_HOPS` hops), and that
    /// copy isn't a replacement still waiting for the scan loop. The VFS the scan loop builds from
    /// the listing no longer has the old locators, but a handle opened before the repair (one long
    /// stream of a season pack's episode) still does, and must reach the replacement for as long as
    /// it exists.
    pub async fn prune_health_status(&self, active_torrent_ids: &std::collections::HashSet<&str>) {
        let mut health_map = self.health_status.write().await;
        let before = health_map.len();
        let pending: std::collections::HashSet<String> = {
            // Read under the health lock: `complete_repair` records the replacement before it
            // inserts the new id's entry, so every entry this retain sees has its replacement
            // recorded already. (Nothing holds the two locks the other way round.)
            let pending = self.repair_replacements.read().await;
            health_map.retain(|id, _| {
                active_torrent_ids.contains(id.as_str()) || pending.contains_key(id)
            });
            pending.keys().cloned().collect()
        };
        let pruned = before - health_map.len();
        drop(health_map);
        if pruned > 0 {
            info!("Pruned {} stale entries from repair health_status", pruned);
        }
        let mut locators = self.repaired_locators.write().await;
        let keep: std::collections::HashSet<String> = locators
            .iter()
            .filter(|(old_id, files)| {
                if active_torrent_ids.contains(old_id.as_str()) {
                    return true;
                }
                // Every locator of an entry points at the same replacement torrent: follow the
                // chain to its last copy, as `redirect` does.
                let Some(first) = files.values().next() else {
                    return false;
                };
                let mut target = first.torrent_id.as_str();
                for _ in 1..MAX_REDIRECT_HOPS {
                    match locators.get(target).and_then(|f| f.values().next()) {
                        Some(next) => target = next.torrent_id.as_str(),
                        None => break,
                    }
                }
                active_torrent_ids.contains(target) || pending.contains(target)
            })
            .map(|(old_id, _)| old_id.clone())
            .collect();
        locators.retain(|old_id, _| keep.contains(old_id));
    }

    /// Force a torrent into the `Broken` state (hidden from WebDAV until a replacement is
    /// surfaced). TEST-SUPPORT ONLY (also consumed by the `repair_integration_test` binary, so it
    /// stays `pub` rather than `#[cfg(test)]`; `#[doc(hidden)]` keeps it out of the public API
    /// surface): the live playback path no longer calls this — a 503/`Unavailable` now drives
    /// synchronous instant repair (`try_instant_repair`) instead, which records a replacement before
    /// hiding the original. Marking a torrent `Broken` with NO recorded replacement would trap it
    /// hidden until restart (reads short-circuit on `should_hide_torrent`, so it can never
    /// re-trigger repair), so this must NEVER be re-wired into a production path.
    #[doc(hidden)]
    pub async fn mark_broken(&self, torrent_id: &str) {
        let mut health_map = self.health_status.write().await;

        warn!("Marking torrent {} as BROKEN", torrent_id);

        // Preserve previous repair attempts and trigger time to prevent rapid repair loops.
        // If mark_broken cleared last_repair_trigger, a torrent that breaks immediately after
        // repair would bypass the 30-second cooldown and enter a rapid repair cycle.
        let (previous_attempts, previous_trigger) = health_map
            .get(torrent_id)
            .map(|h| (h.repair_attempts, h.last_repair_trigger))
            .unwrap_or((0, None));

        health_map.insert(
            torrent_id.to_string(),
            TorrentHealth {
                torrent_id: torrent_id.to_string(),
                state: RepairState::Broken,
                repair_attempts: previous_attempts,
                last_repair_trigger: previous_trigger,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FileLocator;
    use std::collections::HashSet;

    #[test]
    fn repair_manager_accepts_trait_object() {
        use crate::provider::{DebridProvider, MockProvider};
        let provider: std::sync::Arc<dyn DebridProvider> =
            std::sync::Arc::new(MockProvider::default());
        let _manager = RepairManager::new(provider);
    }

    #[tokio::test]
    async fn should_hide_torrent_for_each_state() {
        let manager = make_test_manager();

        // Healthy: should NOT hide
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "healthy".to_string(),
                TorrentHealth {
                    torrent_id: "healthy".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
        }
        assert!(
            !manager.should_hide_torrent("healthy").await,
            "Healthy torrent should not be hidden"
        );

        // Broken: should hide
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "broken".to_string(),
                TorrentHealth {
                    torrent_id: "broken".to_string(),
                    state: RepairState::Broken,
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
        }
        assert!(
            manager.should_hide_torrent("broken").await,
            "Broken torrent should be hidden"
        );

        // Repairing: should hide
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "repairing".to_string(),
                TorrentHealth {
                    torrent_id: "repairing".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
        }
        assert!(
            manager.should_hide_torrent("repairing").await,
            "Repairing torrent should be hidden"
        );

        // Failed: should hide
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "failed".to_string(),
                TorrentHealth {
                    torrent_id: "failed".to_string(),
                    state: RepairState::Failed,
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }
        assert!(
            manager.should_hide_torrent("failed").await,
            "Failed torrent should be hidden"
        );

        // Unknown torrent: should NOT hide
        assert!(
            !manager.should_hide_torrent("unknown").await,
            "Unknown torrent should not be hidden"
        );
    }

    /// A manager over an in-memory provider: a unit test must never reach a live API (a real client
    /// here sent `complete_repair`'s DELETE to api.real-debrid.com).
    fn make_test_manager() -> RepairManager {
        RepairManager::new(Arc::new(crate::provider::MockProvider::default()))
    }

    /// `locator_for_file` advances `link_idx` ONLY for `selected == 1` files, pairing RD's
    /// per-file restricted links positionally among the SELECTED files. With a SELECTED file
    /// AND an UNSELECTED file both preceding the target, the target is the 2nd selected file
    /// (`link_idx == 1`) and must pair `links[1]`; the unselected file must NOT consume an index.
    /// The single-selected-file repair tests never reach `link_idx > 0`, so this guards it.
    #[test]
    fn locator_for_file_pairs_link_by_position_among_selected_files() {
        use crate::rd_client::{TorrentFile, TorrentInfo};

        let info = TorrentInfo {
            id: "tid".to_string(),
            hash: "H".to_string(),
            status: "downloaded".to_string(),
            files: vec![
                // Selected, but not the target → pushes link_idx to 1.
                TorrentFile {
                    id: 10,
                    path: "/SelectedFirst.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                },
                // Unselected → must be skipped WITHOUT consuming a link index.
                TorrentFile {
                    id: 20,
                    path: "/Unselected.mkv".to_string(),
                    bytes: 500,
                    selected: 0,
                },
                // The target: the 2nd selected file → must pair links[1].
                TorrentFile {
                    id: 30,
                    path: "/Target.mkv".to_string(),
                    bytes: 2000,
                    selected: 1,
                },
            ],
            // RD emits one link per SELECTED file, in selected order: [SelectedFirst, Target].
            links: vec![
                "https://rd/selected-first".to_string(),
                "https://rd/target".to_string(),
            ],
            ..Default::default()
        };

        let loc = RepairManager::locator_for_file(&info, "H", "/Target.mkv")
            .expect("target file should resolve to a locator");

        assert_eq!(loc.file_id, 30);
        assert_eq!(loc.file_path, "/Target.mkv");
        assert_eq!(loc.hash, "H");
        assert_eq!(loc.torrent_id, "tid");
        assert_eq!(
            loc.link.as_deref(),
            Some("https://rd/target"),
            "the 2nd selected file must pair links[1]; the unselected file must not consume an index"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_cached_returns_new_locator() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // MockProvider returns a "downloaded" torrent containing the target file with a link.
        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = crate::provider::MockProvider {
            deleted: deleted.clone(),
            add_magnet: Some(AddMagnetResponse {
                id: "new_tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new_tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec!["https://rd/newlink".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));

        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "old_tid".to_string(),
            file_id: 1,
            file_path: "/Movie.mkv".to_string(),
            link: Some("https://rd/oldlink".to_string()),
        };
        let new = manager
            .try_instant_repair(&old)
            .await
            .expect("repair should succeed");
        assert_eq!(new.torrent_id, "new_tid");
        assert_eq!(new.file_id, 5);
        assert_eq!(new.file_path, "/Movie.mkv");
        assert_eq!(new.link.as_deref(), Some("https://rd/newlink"));
        assert_eq!(new.hash, "H");
        // The destructive half of the RD new-id success path: only the superseded OLD torrent is
        // deleted (never the replacement now serving the file), and new→old is recorded so the scan
        // loop reuses the old identification for the replacement. The delete runs detached.
        assert!(
            yield_until(|| !deleted.lock().unwrap().is_empty()).await,
            "the superseded torrent's delete is issued"
        );
        assert_eq!(
            *deleted.lock().unwrap(),
            vec!["old_tid".to_string()],
            "only the superseded old torrent is deleted"
        );
        assert_eq!(
            manager
                .take_repair_replacements()
                .await
                .get("new_tid")
                .map(String::as_str),
            Some("old_tid"),
            "a cached new-id repair records new→old for the scan loop"
        );
    }

    /// R1 regression: `try_instant_repair` is awaited inside the WebDAV read future. If that future
    /// is dropped mid-repair (the player/HTTP connection disconnects) after the torrent was moved to
    /// `Repairing`, the cancellation-safety guard must reset it to `Healthy` — otherwise it stays
    /// `Repairing`/hidden with nothing able to re-trigger repair, trapped until restart.
    #[tokio::test(start_paused = true)]
    async fn cancelling_in_flight_repair_does_not_strand_torrent_hidden() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // With the clock paused, `try_instant_repair`'s internal `sleep(500ms)` awaits never fire,
        // so the future parks mid-repair with the torrent in `Repairing`.
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec![],
                ..Default::default()
            }),
            ..Default::default()
        };
        let manager = std::sync::Arc::new(RepairManager::new(std::sync::Arc::new(mock)));

        let loc = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 5,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };

        let m = manager.clone();
        let handle = tokio::spawn(async move { m.try_instant_repair(&loc).await });

        // Let the task advance to its first internal await; by then it must have set `Repairing`.
        let mut hidden = false;
        for _ in 0..200 {
            tokio::task::yield_now().await;
            if manager.should_hide_torrent("tid").await {
                hidden = true;
                break;
            }
        }
        assert!(
            hidden,
            "repair should have transitioned the torrent to Repairing (hidden)"
        );

        // Simulate the WebDAV reader disconnecting: drop the repair future mid-await.
        handle.abort();

        // The guard's detached reset must un-hide the torrent without a restart.
        let mut recovered = false;
        for _ in 0..200 {
            tokio::task::yield_now().await;
            if !manager.should_hide_torrent("tid").await {
                recovered = true;
                break;
            }
        }
        assert!(
            recovered,
            "a cancelled repair must not leave the torrent stranded Repairing/hidden"
        );
    }

    /// A provider whose `get_torrent_info` never returns: stands in for an API that accepts the
    /// connection and then hangs, or one deep in its (slow, background-sized) retry backoff.
    #[derive(Debug)]
    struct HangingInfoProvider;

    #[async_trait::async_trait]
    impl DebridProvider for HangingInfoProvider {
        fn name(&self) -> &'static str {
            "hanging"
        }
        async fn get_torrents(&self) -> Result<Vec<crate::rd_client::Torrent>, reqwest::Error> {
            Ok(vec![])
        }
        async fn get_torrent_info(&self, _id: &str) -> Result<TorrentInfo, reqwest::Error> {
            std::future::pending().await
        }
        async fn add_magnet(
            &self,
            _magnet: &str,
        ) -> Result<crate::rd_client::AddMagnetResponse, reqwest::Error> {
            std::future::pending().await
        }
        async fn select_files(&self, _t: &str, _f: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn delete_torrent(&self, _t: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn resolve_url(&self, _l: &FileLocator) -> Result<String, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn invalidate(&self, _l: &FileLocator) {}
    }

    /// The repair runs inline in a WebDAV read, so it must give up within `INSTANT_REPAIR_TIMEOUT`
    /// even when the provider hangs, rather than holding the read (and a connection permit) for the
    /// minutes its background retry budgets allow. A timed-out repair produced no replacement, so the
    /// torrent stays visible with its attempt budget reset, like the other transient arms, and its
    /// cooldown restarts at the timeout.
    #[tokio::test(start_paused = true)]
    async fn try_instant_repair_gives_up_when_the_provider_hangs() {
        let manager = RepairManager::new(Arc::new(HangingInfoProvider));
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 2,
                    last_repair_trigger: Some(
                        tokio::time::Instant::now() - Duration::from_secs(31),
                    ),
                },
            );
        }
        let loc = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_path: "/Movie.mkv".to_string(),
            ..Default::default()
        };
        let started = tokio::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(600), manager.try_instant_repair(&loc))
            .await
            .expect("the inline repair must bound itself, not hang the read");
        assert!(
            matches!(r, Err(RepairFailure::Deferred(_))),
            "a timed-out repair is a transient deferral, got {r:?}"
        );
        assert!(
            started.elapsed() <= INSTANT_REPAIR_TIMEOUT + Duration::from_secs(1),
            "gave up after {:?}",
            started.elapsed()
        );
        assert!(
            !manager.should_hide_torrent("tid").await,
            "a timed-out repair must leave the torrent visible"
        );
        {
            let hm = manager.health_status.read().await;
            let h = hm.get("tid").expect("health entry present");
            assert_eq!(h.state, RepairState::Healthy);
            assert_eq!(
                h.repair_attempts, 0,
                "a timeout is not a broken-file signal, so it must not climb toward Failed"
            );
        }
        // The cooldown runs from when the repair was abandoned, not from when it began: the timeout
        // is as long as the cooldown, so measured from the start it would already be over and the
        // next read (e.g. rclone retrying the failed one) would hold another 30 s on the hung
        // provider at once.
        let retried = tokio::time::Instant::now();
        let r = manager.try_instant_repair(&loc).await;
        assert!(
            matches!(&r, Err(RepairFailure::Failed(m)) if m == "Repair rate limited"),
            "the next read right after a timed-out repair must be rate limited, got {r:?}"
        );
        assert_eq!(retried.elapsed(), Duration::ZERO, "refused without waiting");
    }

    /// R2 regression: the write-side attempts-cap promotion must run AFTER the `Repairing` re-check,
    /// so an at-cap torrent whose repair is legitimately in-flight is not spuriously flipped to
    /// `Failed`.
    #[tokio::test]
    async fn at_cap_repairing_torrent_is_not_flipped_to_failed() {
        let manager = make_test_manager();
        {
            let mut m = manager.health_status.write().await;
            m.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: MAX_REPAIR_ATTEMPTS,
                    last_repair_trigger: Some(tokio::time::Instant::now()),
                },
            );
        }
        let err = manager
            .check_and_begin_repair("tid")
            .await
            .expect_err("a second repair must not begin while one is in-flight");
        assert_eq!(err, "Repair already in progress");
        let state = manager
            .health_status
            .read()
            .await
            .get("tid")
            .unwrap()
            .state
            .clone();
        assert_eq!(
            state,
            RepairState::Repairing,
            "an in-flight Repairing torrent must not be promoted to Failed by the cap check"
        );
    }

    /// The reordered write-side cap check still fires: an at-cap, non-in-flight, cooldown-expired
    /// torrent is correctly promoted to `Failed`.
    #[tokio::test]
    async fn at_cap_healthy_torrent_is_marked_failed() {
        let manager = make_test_manager();
        {
            let mut m = manager.health_status.write().await;
            m.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: MAX_REPAIR_ATTEMPTS,
                    last_repair_trigger: None,
                },
            );
        }
        let err = manager
            .check_and_begin_repair("tid")
            .await
            .expect_err("an at-cap torrent must not begin another repair");
        assert_eq!(err, "Maximum repair attempts exceeded");
        let state = manager
            .health_status
            .read()
            .await
            .get("tid")
            .unwrap()
            .state
            .clone();
        assert_eq!(state, RepairState::Failed);
    }

    #[tokio::test]
    async fn try_instant_repair_torbox_same_id_does_not_delete_the_repaired_torrent() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // TorBox: re-add by infohash returns the SAME torrent id ("tid") as the one under repair,
        // and it is downloaded (cached). Repair must succeed WITHOUT deleting "tid" — deleting it
        // would destroy the very torrent it repaired (data loss for mirror/manual content).
        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(), // SAME id as the locator's torrent — TorBox behaviour
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec![], // TorBox has no per-file links
                ..Default::default()
            }),
            deleted: deleted.clone(),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));

        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 5,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let new = manager
            .try_instant_repair(&old)
            .await
            .expect("repair should succeed");
        assert_eq!(new.torrent_id, "tid");
        assert_eq!(new.file_path, "/Movie.mkv");
        assert!(
            deleted.lock().unwrap().is_empty(),
            "the repaired-in-place torrent (same id) must NOT be deleted, got: {:?}",
            deleted.lock().unwrap()
        );
        assert!(
            manager.take_repair_replacements().await.is_empty(),
            "a same-id repair has no replacement to record"
        );
        assert!(
            manager.redirect(&old).await.is_none(),
            "a same-id repair leaves the VFS's locators valid, so it redirects nothing"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_torbox_same_id_uncached_leaves_torrent_visible() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // TorBox: re-add returns the SAME id, but the torrent is NOT yet downloaded (re-downloading).
        // The repair must fail this read WITHOUT trapping the torrent hidden — it is the real torrent
        // and will be surfaced once it downloads. It must stay non-hidden and not be deleted.
        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(), // same id as the locator — TorBox behaviour
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloading".to_string(), // not cached
                files: vec![TorrentFile {
                    id: 0,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                ..Default::default()
            }),
            deleted: deleted.clone(),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "uncached repair returns Err for this read");
        assert!(
            deleted.lock().unwrap().is_empty(),
            "the same-id re-downloading torrent must not be deleted"
        );
        assert!(
            !manager.should_hide_torrent("tid").await,
            "the same-id re-downloading torrent must stay VISIBLE (not trapped Broken/hidden)"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_torbox_same_id_uncached_resets_attempt_budget() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // A slow-to-recache TorBox same-id torrent must NOT accumulate toward the 3-attempt Failed
        // cap — a re-download in progress is not a failed repair. Pre-seed 2 prior attempts with an
        // expired cooldown; after a same-id uncached repair the budget resets to 0 and the torrent
        // stays Healthy/visible, so a slow re-cache can never be trapped Failed/hidden (which would be
        // unrecoverable). Without the reset, this read would push attempts to 3 and the next would
        // promote it to Failed.
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloading".to_string(), // not cached
                files: vec![TorrentFile {
                    id: 0,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        {
            let mut health = manager.health_status.write().await;
            health.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 2,
                    last_repair_trigger: Some(
                        tokio::time::Instant::now() - std::time::Duration::from_secs(31),
                    ),
                },
            );
        }
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "uncached repair returns Err for this read");
        let health = manager.health_status.read().await;
        let h = health.get("tid").expect("health entry present");
        assert_eq!(
            h.repair_attempts, 0,
            "a same-id re-download must reset the attempt budget, not accumulate toward Failed"
        );
        assert_eq!(h.state, RepairState::Healthy);
    }

    #[tokio::test]
    async fn try_instant_repair_same_id_materialise_failure_leaves_torrent_visible() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentInfo};

        // Same-id (TorBox) re-add whose torrent transiently reports no selectable files →
        // `materialise` fails (with protect_id, NOT deleting the real torrent) → the materialise
        // failure path must leave the real torrent VISIBLE (Healthy), not hide it as Broken with no
        // recovery (a hidden torrent can never re-trigger repair).
        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(), // same id — TorBox behaviour
                uri: String::new(),
            }),
            // No files → the path-match selector finds nothing → materialise errors.
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                ..Default::default()
            }),
            deleted: deleted.clone(),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "materialise failure returns Err for this read");
        assert!(
            deleted.lock().unwrap().is_empty(),
            "protect_id must prevent deleting the real same-id torrent on materialise failure"
        );
        assert!(
            !manager.should_hide_torrent("tid").await,
            "a same-id materialise failure must leave the real torrent VISIBLE (not Broken/hidden)"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_new_id_missing_file_stays_visible_not_failed() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};
        // RD mints a NEW id on re-add. The re-added torrent is `downloaded` but does NOT contain our
        // file_path (usually a transient ~500ms settle blip). The OLD torrent must stay VISIBLE
        // (Healthy) — NOT trapped Failed/hidden with no replacement — and the leaked NEW torrent must
        // be deleted. (Regression for the iter-19 fix: this arm previously called set_repair_failed.)
        let deleted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "new_rd_id".to_string(), // NEW id (!= old) — Real-Debrid behaviour
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new_rd_id".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                files: vec![TorrentFile {
                    id: 0,
                    path: "/OtherFile.mkv".to_string(), // NOT the requested /Movie.mkv
                    bytes: 1000,
                    selected: 1,
                }],
                ..Default::default()
            }),
            deleted: deleted.clone(),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "old_tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "a missing file returns Err for this read");
        assert!(
            matches!(r, Err(RepairFailure::Deferred(_))),
            "a settle-blip path mismatch is a transient DEFERRAL (info-logged), not a hard Failed (warn)"
        );
        assert!(
            !manager.should_hide_torrent("old_tid").await,
            "an RD new-id path-mismatch must leave the OLD torrent VISIBLE (not trapped Failed/hidden)"
        );
        assert!(
            deleted.lock().unwrap().iter().any(|id| id == "new_rd_id"),
            "the leaked new RD torrent must be cleaned up"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_transient_failure_resets_attempt_budget() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentInfo};

        // A TRANSIENT repair failure (here: a same-id re-add whose torrent reports no selectable
        // files → `materialise` errors) must RESET the attempt budget, not accumulate toward the
        // 3-attempt Failed cap. Pre-seed 2 prior attempts with an expired cooldown; without the
        // reset this read would push attempts to 3 and the next would promote the (same-id) torrent
        // to Failed/hidden — unrecoverable. The 30s cooldown must still be preserved.
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "tid".to_string(), // same id — TorBox behaviour
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "tid".to_string(),
                hash: "H".to_string(),
                status: "downloaded".to_string(),
                ..Default::default() // no files → materialise errors (transient)
            }),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        {
            let mut health = manager.health_status.write().await;
            health.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 2,
                    last_repair_trigger: Some(
                        tokio::time::Instant::now() - std::time::Duration::from_secs(31),
                    ),
                },
            );
        }
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 0,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        };
        let r = manager.try_instant_repair(&old).await;
        assert!(r.is_err(), "transient failure returns Err for this read");
        let health = manager.health_status.read().await;
        let h = health.get("tid").expect("health entry present");
        assert_eq!(
            h.repair_attempts, 0,
            "a transient repair failure must reset the attempt budget (never trap a same-id torrent)"
        );
        assert_eq!(h.state, RepairState::Healthy);
        assert!(
            h.last_repair_trigger.is_some(),
            "the 30s cooldown must be preserved so retries stay rate-limited"
        );
    }

    #[tokio::test]
    async fn complete_repair_carries_attempts_and_cooldown_forward() {
        // Real-Debrid mints a NEW id per repair. The new health entry must carry the old entry's
        // repair_attempts and a fresh cooldown trigger, so the 30s cooldown / 3-attempt cap survive
        // across re-adds (otherwise a still-broken cached file storms re-add/delete on every read).
        let manager = make_test_manager();
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "old".to_string(),
                TorrentHealth {
                    torrent_id: "old".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 2,
                    last_repair_trigger: None,
                },
            );
        }
        manager.complete_repair("old", "new").await;
        let hm = manager.health_status.read().await;
        assert!(hm.get("old").is_none(), "old id removed");
        let new = hm.get("new").expect("new id tracked");
        assert_eq!(new.state, RepairState::Healthy);
        assert_eq!(
            new.repair_attempts, 2,
            "attempts carried forward across the re-add"
        );
        assert!(
            new.last_repair_trigger.is_some(),
            "a fresh cooldown trigger is set so the next read is rate-limited (storm bound)"
        );
    }

    #[tokio::test]
    async fn note_read_success_resets_repair_budget() {
        // A confirmed good read must reset the carried repair budget, so the 3-attempt cap bounds
        // only CONSECUTIVE failed repairs (a storm) — not separate incidents over the deployment's
        // lifetime (which the carry-forward would otherwise accumulate into a permanent Failed).
        let manager = make_test_manager();
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "t".to_string(),
                TorrentHealth {
                    torrent_id: "t".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 2,
                    last_repair_trigger: Some(tokio::time::Instant::now()),
                },
            );
        }
        manager.note_read_success("t").await;
        let hm = manager.health_status.read().await;
        let h = hm.get("t").expect("entry present");
        assert_eq!(
            h.repair_attempts, 0,
            "budget reset on a confirmed good read"
        );
        assert!(
            h.last_repair_trigger.is_none(),
            "cooldown cleared on a confirmed good read"
        );
        assert_eq!(h.state, RepairState::Healthy);
    }

    #[tokio::test]
    async fn note_read_success_is_a_noop_for_clean_or_absent_torrent() {
        let manager = make_test_manager();
        // Absent torrent: no panic, no entry created.
        manager.note_read_success("absent").await;
        assert!(manager.health_status.read().await.get("absent").is_none());
    }

    #[tokio::test]
    async fn note_read_success_skips_while_repairing() {
        // A confirmed good read on one handle must NOT clear an in-flight repair's Repairing
        // state/cooldown on another handle — doing so would let a third reader launch a redundant
        // re-add. The in-flight repair's completion path is responsible for resetting the budget.
        let manager = make_test_manager();
        let trigger = tokio::time::Instant::now();
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "t".to_string(),
                TorrentHealth {
                    torrent_id: "t".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 1,
                    last_repair_trigger: Some(trigger),
                },
            );
        }
        manager.note_read_success("t").await;
        let hm = manager.health_status.read().await;
        let h = hm.get("t").expect("entry present");
        assert_eq!(
            h.state,
            RepairState::Repairing,
            "in-flight repair untouched"
        );
        assert_eq!(
            h.repair_attempts, 1,
            "attempt budget not cleared mid-repair"
        );
        assert!(
            h.last_repair_trigger.is_some(),
            "cooldown not cleared mid-repair"
        );
    }

    #[tokio::test]
    async fn note_read_success_does_not_resurrect_a_broken_torrent() {
        // A read that resolved BEFORE the non-cached repair path deleted + marked the old id Broken
        // can still complete successfully and call note_read_success on that id. It must NOT flip
        // Broken→Healthy — un-hiding a torrent that no longer exists on the provider would let the
        // next read re-resolve a deleted id and churn repair.
        let manager = make_test_manager();
        for state in [RepairState::Broken, RepairState::Failed] {
            {
                let mut hm = manager.health_status.write().await;
                hm.insert(
                    "t".to_string(),
                    TorrentHealth {
                        torrent_id: "t".to_string(),
                        state: state.clone(),
                        repair_attempts: 2,
                        last_repair_trigger: Some(tokio::time::Instant::now()),
                    },
                );
            }
            manager.note_read_success("t").await;
            let hm = manager.health_status.read().await;
            let h = hm.get("t").expect("entry present");
            assert_eq!(
                h.state, state,
                "a {state:?} torrent must not be resurrected"
            );
            assert_eq!(
                h.repair_attempts, 2,
                "budget untouched on a non-Healthy state"
            );
        }
    }

    #[tokio::test]
    async fn complete_repair_same_id_resets_attempts() {
        // TorBox re-adds by infohash recover the SAME id. On the cached-success path the file is
        // present, so a still-failing byte read is transient CDN trouble, not missing content: the
        // attempt budget MUST reset so a same-id torrent can never accumulate to Failed/hidden
        // (unrecoverable — hidden torrents never re-trigger repair). RD's new-id carry-forward
        // (covered by `complete_repair_carries_attempts_and_cooldown_forward`) is unaffected.
        let manager = make_test_manager();
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 2,
                    last_repair_trigger: None,
                },
            );
        }
        manager.complete_repair("tid", "tid").await;
        let hm = manager.health_status.read().await;
        let h = hm.get("tid").expect("same id still tracked");
        assert_eq!(h.state, RepairState::Healthy);
        assert_eq!(
            h.repair_attempts, 0,
            "a same-id (TorBox) repair resets the attempt budget so it can never trap as Failed"
        );
        assert!(
            h.last_repair_trigger.is_some(),
            "a fresh cooldown trigger still bounds re-adds to one per 30s"
        );
    }

    #[tokio::test]
    async fn try_instant_repair_rate_limited_within_30s() {
        let manager = make_test_manager();
        // Pre-populate health with a recent repair trigger
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "torrent1".to_string(),
                TorrentHealth {
                    torrent_id: "torrent1".to_string(),
                    state: RepairState::Broken,
                    repair_attempts: 1,
                    last_repair_trigger: Some(tokio::time::Instant::now()),
                },
            );
        }

        let result = manager
            .try_instant_repair(&FileLocator {
                torrent_id: "torrent1".to_string(),
                link: Some("some_link".to_string()),
                ..Default::default()
            })
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "Repair rate limited");
    }

    #[tokio::test]
    async fn try_instant_repair_max_attempts_exceeded() {
        let manager = make_test_manager();
        // Pre-populate health with 3 prior attempts (no recent trigger, so rate limit passes)
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "torrent2".to_string(),
                TorrentHealth {
                    torrent_id: "torrent2".to_string(),
                    state: RepairState::Broken,
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let result = manager
            .try_instant_repair(&FileLocator {
                torrent_id: "torrent2".to_string(),
                link: Some("some_link".to_string()),
                ..Default::default()
            })
            .await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "Maximum repair attempts exceeded"
        );

        // Verify it was marked as Failed
        let health_map = manager.health_status.read().await;
        assert_eq!(
            health_map.get("torrent2").unwrap().state,
            RepairState::Failed
        );
    }

    #[tokio::test]
    async fn try_instant_repair_skips_already_repairing() {
        let manager = make_test_manager();
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "torrent3".to_string(),
                TorrentHealth {
                    torrent_id: "torrent3".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
        }

        let result = manager
            .try_instant_repair(&FileLocator {
                torrent_id: "torrent3".to_string(),
                link: Some("some_link".to_string()),
                ..Default::default()
            })
            .await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "Repair already in progress"
        );
    }

    #[tokio::test]
    async fn repair_replacements_records_and_returns() {
        let manager = make_test_manager();

        // Manually insert replacements (simulating successful repairs)
        {
            let mut map = manager.repair_replacements.write().await;
            map.insert("new_id_1".to_string(), "old_id_1".to_string());
            map.insert("new_id_2".to_string(), "old_id_2".to_string());
        }

        let replacements = manager.take_repair_replacements().await;
        assert_eq!(replacements.len(), 2);
        assert_eq!(replacements.get("new_id_1").unwrap(), "old_id_1");
        assert_eq!(replacements.get("new_id_2").unwrap(), "old_id_2");
    }

    #[tokio::test]
    async fn take_repair_replacements_drains_map() {
        let manager = make_test_manager();

        {
            let mut map = manager.repair_replacements.write().await;
            map.insert("new_id".to_string(), "old_id".to_string());
        }

        let first_call = manager.take_repair_replacements().await;
        assert_eq!(first_call.len(), 1);

        let second_call = manager.take_repair_replacements().await;
        assert!(second_call.is_empty());
    }

    // The TOCTOU guards in check_and_begin_repair (re-checking Failed / Repairing /
    // last_repair_trigger on the write side) are covered behaviourally by
    // `concurrent_check_and_begin_repair_only_one_succeeds` (only one of ten racers wins →
    // Repairing guard) and `check_and_begin_repair_write_side_rate_limits` (the
    // last_repair_trigger re-check rejects), rather than by asserting on source text.

    #[tokio::test]
    async fn concurrent_check_and_begin_repair_only_one_succeeds() {
        // Verify that when multiple tasks call check_and_begin_repair concurrently
        // for the same torrent, only one succeeds and the others are rejected.
        let manager = Arc::new(make_test_manager());

        // Spawn 10 concurrent repair attempts
        let mut handles = Vec::new();
        for _ in 0..10 {
            let mgr = manager.clone();
            handles.push(tokio::spawn(async move {
                mgr.check_and_begin_repair("concurrent_torrent").await
            }));
        }

        let mut successes = 0;
        let mut failures = 0;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(_) => successes += 1,
                Err(_) => failures += 1,
            }
        }

        // Exactly one task should succeed; all others should be rejected
        assert_eq!(successes, 1, "Exactly one concurrent repair should succeed");
        assert_eq!(
            failures, 9,
            "All other concurrent repairs should be rejected"
        );

        // Verify the torrent is in Repairing state
        let health_map = manager.health_status.read().await;
        let health = health_map.get("concurrent_torrent").unwrap();
        assert_eq!(health.state, RepairState::Repairing);
        assert_eq!(health.repair_attempts, 1);
    }

    #[tokio::test]
    async fn check_and_begin_repair_write_side_rate_limits() {
        // Verify the write-side rate limit re-check works: if another task
        // completes repair (setting last_repair_trigger) between our read
        // and write locks, we should be rejected.
        let manager = make_test_manager();

        // First repair succeeds
        let result = manager.check_and_begin_repair("rate_test").await;
        assert!(result.is_ok());

        // Simulate repair completing (back to Broken, but last_repair_trigger is recent)
        {
            let mut health_map = manager.health_status.write().await;
            let health = health_map.get_mut("rate_test").unwrap();
            health.state = RepairState::Broken;
            // last_repair_trigger was set by check_and_begin_repair, leave it as-is
        }

        // Second attempt should be rate-limited (within 30s)
        let result = manager.check_and_begin_repair("rate_test").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Repair rate limited");
    }

    #[tokio::test]
    async fn try_instant_repair_skips_permanently_failed() {
        let manager = make_test_manager();
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "torrent4".to_string(),
                TorrentHealth {
                    torrent_id: "torrent4".to_string(),
                    state: RepairState::Failed,
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let result = manager
            .try_instant_repair(&FileLocator {
                torrent_id: "torrent4".to_string(),
                link: Some("some_link".to_string()),
                ..Default::default()
            })
            .await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "Torrent permanently failed"
        );
    }

    #[tokio::test]
    async fn mark_broken_preserves_repair_attempts() {
        let manager = make_test_manager();

        // First, set up a torrent with 2 prior repair attempts
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "torrent_preserve".to_string(),
                TorrentHealth {
                    torrent_id: "torrent_preserve".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 2,
                    last_repair_trigger: Some(tokio::time::Instant::now()),
                },
            );
        }

        // Mark it as broken again
        manager.mark_broken("torrent_preserve").await;

        // Verify repair_attempts is preserved
        let health_map = manager.health_status.read().await;
        let health = health_map.get("torrent_preserve").unwrap();
        assert_eq!(health.state, RepairState::Broken);
        assert_eq!(
            health.repair_attempts, 2,
            "mark_broken must preserve previous repair_attempts count"
        );
        // last_repair_trigger should be preserved to prevent rapid repair loops
        assert!(
            health.last_repair_trigger.is_some(),
            "mark_broken must preserve last_repair_trigger to prevent rapid repair loops"
        );
    }

    #[tokio::test]
    async fn get_status_summary_counts_correctly() {
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "h1".to_string(),
                TorrentHealth {
                    torrent_id: "h1".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "b1".to_string(),
                TorrentHealth {
                    torrent_id: "b1".to_string(),
                    state: RepairState::Broken,
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "r1".to_string(),
                TorrentHealth {
                    torrent_id: "r1".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "f1".to_string(),
                TorrentHealth {
                    torrent_id: "f1".to_string(),
                    state: RepairState::Failed,
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let (healthy, repairing, failed) = manager.get_status_summary().await;
        assert_eq!(healthy, 1, "Should have 1 healthy torrent");
        // get_status_summary counts Broken + Repairing together as "repairing"
        assert_eq!(repairing, 2, "Should have 2 repairing (broken + repairing)");
        assert_eq!(failed, 1, "Should have 1 failed torrent");
    }

    #[tokio::test]
    async fn hidden_torrent_ids_returns_broken_and_failed_only() {
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "healthy1".to_string(),
                TorrentHealth {
                    torrent_id: "healthy1".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "broken1".to_string(),
                TorrentHealth {
                    torrent_id: "broken1".to_string(),
                    state: RepairState::Broken,
                    repair_attempts: 0,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "repairing1".to_string(),
                TorrentHealth {
                    torrent_id: "repairing1".to_string(),
                    state: RepairState::Repairing,
                    repair_attempts: 1,
                    last_repair_trigger: None,
                },
            );
            health_map.insert(
                "failed1".to_string(),
                TorrentHealth {
                    torrent_id: "failed1".to_string(),
                    state: RepairState::Failed,
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let hidden = manager.hidden_torrent_ids().await;
        assert_eq!(
            hidden.len(),
            2,
            "Should have 2 hidden torrents (broken, failed)"
        );
        assert!(
            !hidden.contains("healthy1"),
            "Healthy torrent should not be hidden"
        );
        assert!(
            hidden.contains("broken1"),
            "Broken torrent should be hidden"
        );
        assert!(
            !hidden.contains("repairing1"),
            "a torrent mid-repair stays in the VFS (only its reads are refused)"
        );
        assert!(
            hidden.contains("failed1"),
            "Failed torrent should be hidden"
        );
    }

    #[tokio::test]
    async fn hidden_torrent_ids_consistent_with_should_hide() {
        // `hidden_torrent_ids` (what the VFS leaves out) and `should_hide_torrent` (what reads refuse)
        // agree, except on a torrent mid-repair: it stays listed, so a scan tick landing inside a
        // repair doesn't drop its files from the library (telling Jellyfin they were deleted) until
        // the next tick, while its reads are still refused until the repair settles.
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            for (id, state) in [
                ("a", RepairState::Healthy),
                ("b", RepairState::Broken),
                ("c", RepairState::Repairing),
                ("d", RepairState::Failed),
            ] {
                health_map.insert(
                    id.to_string(),
                    TorrentHealth {
                        torrent_id: id.to_string(),
                        state,
                        repair_attempts: 0,
                        last_repair_trigger: None,
                    },
                );
            }
        }

        let hidden_set = manager.hidden_torrent_ids().await;
        for id in ["a", "b", "d"] {
            assert_eq!(
                hidden_set.contains(id),
                manager.should_hide_torrent(id).await,
                "hidden_torrent_ids and should_hide_torrent must agree for torrent '{}'",
                id
            );
        }
        assert!(
            !hidden_set.contains("c"),
            "a Repairing torrent stays in the VFS"
        );
        assert!(
            manager.should_hide_torrent("c").await,
            "reads of a Repairing torrent are still refused"
        );
    }

    #[tokio::test]
    async fn non_cached_repair_marks_broken_and_records_replacement() {
        use crate::provider::FileLocator;
        use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

        // MockProvider returns a re-added torrent that is NOT yet downloaded (cache lapsed),
        // driving try_instant_repair into the non-cached branch.
        let mock = crate::provider::MockProvider {
            add_magnet: Some(AddMagnetResponse {
                id: "new_tid".to_string(),
                uri: String::new(),
            }),
            torrent_info: Some(TorrentInfo {
                id: "new_tid".to_string(),
                hash: "H".to_string(),
                status: "downloading".to_string(),
                files: vec![TorrentFile {
                    id: 5,
                    path: "/Movie.mkv".to_string(),
                    bytes: 1000,
                    selected: 1,
                }],
                links: vec![],
                ..Default::default()
            }),
            ..Default::default()
        };
        let manager = RepairManager::new(std::sync::Arc::new(mock));
        let old = FileLocator {
            hash: "H".to_string(),
            torrent_id: "old_tid".to_string(),
            file_id: 1,
            file_path: "/Movie.mkv".to_string(),
            link: Some("https://rd/oldlink".to_string()),
        };

        // The non-cached branch returns an error (no fresh locator to serve).
        assert!(manager.try_instant_repair(&old).await.is_err());

        // It must record new_tid -> old_tid so the scan loop reuses the old identification.
        let replacements = manager.take_repair_replacements().await;
        assert_eq!(
            replacements.get("new_tid").map(String::as_str),
            Some("old_tid"),
            "non-cached repair must record the new->old replacement mapping"
        );

        // And it must mark the old torrent Broken so it is hidden until the scan swaps it in.
        assert!(
            manager.should_hide_torrent("old_tid").await,
            "non-cached repair must mark the old torrent Broken"
        );
    }

    #[tokio::test]
    async fn prune_health_status_removes_stale_keeps_active() {
        let manager = make_test_manager();

        // Populate health_status with several entries
        {
            let mut health_map = manager.health_status.write().await;
            for id in ["active1", "active2", "stale1", "stale2", "stale3"] {
                health_map.insert(
                    id.to_string(),
                    TorrentHealth {
                        torrent_id: id.to_string(),
                        state: RepairState::Healthy,
                        repair_attempts: 0,
                        last_repair_trigger: None,
                    },
                );
            }
        }

        // Only "active1" and "active2" are still active
        let active_ids: HashSet<&str> = ["active1", "active2"].into_iter().collect();
        manager.prune_health_status(&active_ids).await;

        let health_map = manager.health_status.read().await;
        assert_eq!(
            health_map.len(),
            2,
            "Should only have 2 active entries after pruning"
        );
        assert!(
            health_map.contains_key("active1"),
            "active1 should be retained"
        );
        assert!(
            health_map.contains_key("active2"),
            "active2 should be retained"
        );
        assert!(
            !health_map.contains_key("stale1"),
            "stale1 should be pruned"
        );
        assert!(
            !health_map.contains_key("stale2"),
            "stale2 should be pruned"
        );
        assert!(
            !health_map.contains_key("stale3"),
            "stale3 should be pruned"
        );
    }

    #[tokio::test]
    async fn prune_health_status_no_op_when_all_active() {
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            for id in ["t1", "t2"] {
                health_map.insert(
                    id.to_string(),
                    TorrentHealth {
                        torrent_id: id.to_string(),
                        state: RepairState::Healthy,
                        repair_attempts: 0,
                        last_repair_trigger: None,
                    },
                );
            }
        }

        let active_ids: HashSet<&str> = ["t1", "t2"].into_iter().collect();
        manager.prune_health_status(&active_ids).await;

        let health_map = manager.health_status.read().await;
        assert_eq!(
            health_map.len(),
            2,
            "All entries should be retained when all are active"
        );
    }

    #[tokio::test]
    async fn prune_health_status_empty_active_removes_all() {
        let manager = make_test_manager();

        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "orphan".to_string(),
                TorrentHealth {
                    torrent_id: "orphan".to_string(),
                    state: RepairState::Failed,
                    repair_attempts: 3,
                    last_repair_trigger: None,
                },
            );
        }

        let active_ids: HashSet<&str> = HashSet::new();
        manager.prune_health_status(&active_ids).await;

        let health_map = manager.health_status.read().await;
        assert!(
            health_map.is_empty(),
            "All entries should be pruned when active set is empty"
        );
    }

    #[tokio::test]
    async fn mark_broken_preserves_last_repair_trigger_preventing_rapid_loops() {
        let manager = make_test_manager();

        // Simulate a torrent that was just repaired (has recent last_repair_trigger)
        let recent_trigger = tokio::time::Instant::now();
        {
            let mut health_map = manager.health_status.write().await;
            health_map.insert(
                "rapid_torrent".to_string(),
                TorrentHealth {
                    torrent_id: "rapid_torrent".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: 1,
                    last_repair_trigger: Some(recent_trigger),
                },
            );
        }

        // Torrent breaks again immediately
        manager.mark_broken("rapid_torrent").await;

        // The 30-second cooldown should still be in effect because
        // mark_broken preserves last_repair_trigger
        let result = manager.check_and_begin_repair("rapid_torrent").await;
        assert!(
            result.is_err(),
            "Repair should be rate-limited after mark_broken preserves trigger"
        );
        assert_eq!(result.unwrap_err(), "Repair rate limited");
    }

    // ── repairs cut short (a timeout or a dropped read), and TorBox's 404 blips ─────────────────

    /// Yield until `done` holds (detached tasks such as a guard's cleanup get to run), up to 200
    /// times; returns whether it did.
    async fn yield_until(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if done() {
                return true;
            }
            tokio::task::yield_now().await;
        }
        done()
    }

    /// The torrent under repair, `tid`: one selected file, `/Movie.mkv`.
    fn old_movie_info() -> TorrentInfo {
        TorrentInfo {
            id: "tid".to_string(),
            hash: "H".to_string(),
            status: "downloaded".to_string(),
            files: vec![crate::rd_client::TorrentFile {
                id: 5,
                path: "/Movie.mkv".to_string(),
                bytes: 1000,
                selected: 1,
            }],
            ..Default::default()
        }
    }

    /// Real-Debrid's re-add of `tid`'s hash: a NEW id, `new`, with the same file, in `status`.
    fn new_movie_info(status: &str) -> TorrentInfo {
        TorrentInfo {
            id: "new".to_string(),
            status: status.to_string(),
            ..old_movie_info()
        }
    }

    fn tid_locator() -> FileLocator {
        FileLocator {
            hash: "H".to_string(),
            torrent_id: "tid".to_string(),
            file_id: 5,
            file_path: "/Movie.mkv".to_string(),
            link: None,
        }
    }

    /// A provider scripted per torrent id. `add_magnet` returns the new id `new` (Real-Debrid
    /// behaviour); `get_torrent_info` answers from `infos`, never returns for an id in `hang_info`,
    /// and with `flaky_lookups` answers every other lookup of each id (the 1st, 3rd, ...) with a 404;
    /// `delete_torrent` records the id, then never returns for one in `hang_delete`.
    #[derive(Debug, Default)]
    struct ScriptedProvider {
        infos: HashMap<String, TorrentInfo>,
        hang_info: HashSet<String>,
        hang_delete: HashSet<String>,
        flaky_lookups: bool,
        lookups: std::sync::Mutex<HashMap<String, usize>>,
        adds: std::sync::atomic::AtomicUsize,
        deleted: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedProvider {
        fn deleted(&self) -> Vec<String> {
            self.deleted.lock().unwrap().clone()
        }
        fn lookups_of(&self, id: &str) -> usize {
            self.lookups.lock().unwrap().get(id).copied().unwrap_or(0)
        }
    }

    #[async_trait::async_trait]
    impl DebridProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }
        async fn get_torrents(&self) -> Result<Vec<crate::rd_client::Torrent>, reqwest::Error> {
            Ok(vec![])
        }
        async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
            let n = {
                let mut lookups = self.lookups.lock().unwrap();
                let n = lookups.entry(id.to_string()).or_default();
                *n += 1;
                *n
            };
            if self.hang_info.contains(id) {
                return std::future::pending().await;
            }
            if self.flaky_lookups && n % 2 == 1 {
                return Err(crate::provider::not_found_error());
            }
            Ok(self.infos.get(id).cloned().unwrap_or_default())
        }
        async fn add_magnet(
            &self,
            _magnet: &str,
        ) -> Result<crate::rd_client::AddMagnetResponse, reqwest::Error> {
            self.adds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::rd_client::AddMagnetResponse {
                id: "new".to_string(),
                uri: String::new(),
            })
        }
        async fn select_files(&self, _t: &str, _f: &str) -> Result<(), reqwest::Error> {
            Ok(())
        }
        async fn delete_torrent(&self, id: &str) -> Result<(), reqwest::Error> {
            self.deleted.lock().unwrap().push(id.to_string());
            if self.hang_delete.contains(id) {
                return std::future::pending().await;
            }
            Ok(())
        }
        async fn resolve_url(&self, _l: &FileLocator) -> Result<String, crate::error::AppError> {
            Err(crate::error::AppError::Unavailable)
        }
        async fn invalidate(&self, _l: &FileLocator) {}
    }

    /// A repair cut short by a dropped read reached no verdict on the file, so (like a timed-out
    /// one) it must not count toward `MAX_REPAIR_ATTEMPTS`; otherwise three cancellations with no
    /// good read between them trap the torrent `Failed`/hidden until restart. The cooldown restarts
    /// when the repair is abandoned.
    #[tokio::test(start_paused = true)]
    async fn cancelling_in_flight_repair_resets_the_attempt_budget() {
        // The clock is paused and the test task keeps yielding, so the repair parks mid-way with
        // the torrent `Repairing`.
        let provider = Arc::new(ScriptedProvider {
            infos: [("tid".to_string(), old_movie_info())]
                .into_iter()
                .collect(),
            hang_info: ["new".to_string()].into_iter().collect(),
            ..Default::default()
        });
        let manager = Arc::new(RepairManager::new(provider));
        {
            let mut hm = manager.health_status.write().await;
            hm.insert(
                "tid".to_string(),
                TorrentHealth {
                    torrent_id: "tid".to_string(),
                    state: RepairState::Healthy,
                    repair_attempts: MAX_REPAIR_ATTEMPTS - 1,
                    last_repair_trigger: Some(
                        tokio::time::Instant::now() - Duration::from_secs(REPAIR_COOLDOWN_SECS + 1),
                    ),
                },
            );
        }
        let m = manager.clone();
        let handle = tokio::spawn(async move { m.try_instant_repair(&tid_locator()).await });
        let mut hidden = false;
        for _ in 0..200 {
            tokio::task::yield_now().await;
            if manager.should_hide_torrent("tid").await {
                hidden = true;
                break;
            }
        }
        assert!(hidden, "the repair should have begun (Repairing = hidden)");
        assert!(!handle.is_finished(), "the repair must still be in flight");
        // The read gives up 20 s into the repair (short of `INSTANT_REPAIR_TIMEOUT`).
        tokio::time::advance(Duration::from_secs(20)).await;
        assert!(!handle.is_finished(), "the repair must still be in flight");
        handle.abort();
        let mut visible = false;
        for _ in 0..200 {
            tokio::task::yield_now().await;
            if !manager.should_hide_torrent("tid").await {
                visible = true;
                break;
            }
        }
        assert!(visible, "the guard must un-hide the torrent");

        {
            let hm = manager.health_status.read().await;
            let h = hm.get("tid").expect("health entry present");
            assert_eq!(h.state, RepairState::Healthy);
            assert_eq!(
                h.repair_attempts, 0,
                "a cancelled repair is not a broken-file signal, so it must not climb toward Failed"
            );
            assert!(
                h.last_repair_trigger
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(1)),
                "the cooldown restarts when the repair is abandoned, not when it began"
            );
        }
        // Once the cooldown is over, the next read may repair again rather than hitting the cap.
        manager
            .health_status
            .write()
            .await
            .get_mut("tid")
            .unwrap()
            .last_repair_trigger =
            Some(tokio::time::Instant::now() - Duration::from_secs(REPAIR_COOLDOWN_SECS + 1));
        assert!(manager.check_and_begin_repair("tid").await.is_ok());
    }

    /// A repair that times out after Real-Debrid's re-add minted a new id must delete that torrent:
    /// it was never selected, so it sits in `waiting_files_selection` forever and the scan loop's
    /// same-hash dedup (which only collapses `downloaded` torrents) never removes it.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_repair_deletes_the_torrent_it_added() {
        let provider = Arc::new(ScriptedProvider {
            infos: [("tid".to_string(), old_movie_info())]
                .into_iter()
                .collect(),
            // The re-added torrent's lookup hangs: the repair times out between add and select.
            hang_info: ["new".to_string()].into_iter().collect(),
            ..Default::default()
        });
        let manager = RepairManager::new(provider.clone());

        let r = manager.try_instant_repair(&tid_locator()).await;

        assert!(
            matches!(r, Err(RepairFailure::Deferred(_))),
            "a timed-out repair is a deferral, got {r:?}"
        );
        assert!(
            yield_until(|| !provider.deleted().is_empty()).await,
            "the unadopted add must be deleted"
        );
        assert_eq!(
            provider.deleted(),
            vec!["new".to_string()],
            "only the unadopted add is deleted, never the torrent under repair"
        );
        assert!(!manager.should_hide_torrent("tid").await);
    }

    /// The same when the read that triggered the repair is dropped (the player disconnects) after
    /// the re-add.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_repair_deletes_the_torrent_it_added() {
        let provider = Arc::new(ScriptedProvider {
            infos: [("tid".to_string(), old_movie_info())]
                .into_iter()
                .collect(),
            hang_info: ["new".to_string()].into_iter().collect(),
            ..Default::default()
        });
        let manager = Arc::new(RepairManager::new(provider.clone()));
        let m = manager.clone();
        let handle = tokio::spawn(async move { m.try_instant_repair(&tid_locator()).await });
        assert!(
            yield_until(|| provider.adds.load(std::sync::atomic::Ordering::SeqCst) > 0).await,
            "the repair should have re-added the hash"
        );
        assert!(!handle.is_finished(), "the repair must still be in flight");

        handle.abort();

        assert!(
            yield_until(|| !provider.deleted().is_empty()).await,
            "the unadopted add must be deleted"
        );
        assert_eq!(provider.deleted(), vec!["new".to_string()]);
    }

    /// Once the repair has adopted its add (here: the not-cached path, which leaves the new torrent
    /// to download in place of the superseded one), a delete of the superseded torrent that hangs
    /// past `INSTANT_REPAIR_TIMEOUT` must neither delete the add as well (both copies would be gone)
    /// nor cut the repair's bookkeeping short: the delete runs off the timed repair, so the
    /// replacement is recorded for the scan loop and the superseded torrent is hidden.
    #[tokio::test(start_paused = true)]
    async fn a_slow_delete_of_the_superseded_torrent_keeps_the_uncached_replacement() {
        let provider = Arc::new(ScriptedProvider {
            infos: [
                ("tid".to_string(), old_movie_info()),
                ("new".to_string(), new_movie_info("downloading")),
            ]
            .into_iter()
            .collect(),
            hang_delete: ["tid".to_string()].into_iter().collect(),
            ..Default::default()
        });
        let manager = RepairManager::new(provider.clone());

        let r = manager.try_instant_repair(&tid_locator()).await;

        assert!(matches!(r, Err(RepairFailure::NotCached(_))), "got {r:?}");
        assert!(
            yield_until(|| !provider.deleted().is_empty()).await,
            "the superseded torrent's delete is still issued"
        );
        yield_until(|| false).await; // let any other detached cleanup run
        assert_eq!(
            provider.deleted(),
            vec!["tid".to_string()],
            "the adopted add must survive (only the superseded torrent's delete was attempted)"
        );
        assert_eq!(
            manager.take_repair_replacements().await,
            HashMap::from([("new".to_string(), "tid".to_string())]),
            "the replacement is recorded for the scan loop"
        );
        assert!(
            manager.hidden_torrent_ids().await.contains("tid"),
            "the superseded torrent stays hidden (Broken) until the scan loop swaps the new one in"
        );
    }

    /// The cached twin: a cached re-add serves the read, and a hanging delete of the superseded
    /// torrent doesn't turn that into a deferral that loses the replacement record and the attempt
    /// budget carried to the new id (whose cooldown bounds a re-add storm on a still-broken file).
    #[tokio::test(start_paused = true)]
    async fn a_slow_delete_of_the_superseded_torrent_keeps_the_cached_replacement() {
        let provider = Arc::new(ScriptedProvider {
            infos: [
                ("tid".to_string(), old_movie_info()),
                ("new".to_string(), new_movie_info("downloaded")),
            ]
            .into_iter()
            .collect(),
            hang_delete: ["tid".to_string()].into_iter().collect(),
            ..Default::default()
        });
        let manager = RepairManager::new(provider.clone());

        let new = manager
            .try_instant_repair(&tid_locator())
            .await
            .expect("the cached re-add serves the read");

        assert_eq!(new.torrent_id, "new");
        assert!(
            yield_until(|| !provider.deleted().is_empty()).await,
            "the superseded torrent's delete is still issued"
        );
        assert_eq!(provider.deleted(), vec!["tid".to_string()]);
        assert_eq!(
            manager.take_repair_replacements().await,
            HashMap::from([("new".to_string(), "tid".to_string())]),
            "the replacement is recorded for the scan loop"
        );
        {
            let hm = manager.health_status.read().await;
            let h = hm.get("new").expect("the new id carries a health entry");
            assert_eq!(h.state, RepairState::Healthy);
            assert_eq!(
                h.repair_attempts, 1,
                "the attempt budget is carried forward"
            );
            assert!(!hm.contains_key("tid"), "the superseded id's entry is gone");
        }
        assert!(
            matches!(
                manager.try_instant_repair(&new).await,
                Err(RepairFailure::Failed(_))
            ),
            "a repair of the new id inside the carried cooldown is refused"
        );
    }

    /// TorBox's `mylist?id=` sometimes answers 404 for a torrent that exists. Each of the repair's
    /// three lookups (the torrent under repair, the re-add right after it is added, and the final
    /// check) retries such a 404 briefly instead of failing the read.
    #[tokio::test(start_paused = true)]
    async fn instant_repair_retries_a_404_on_its_torrent_lookups() {
        let provider = Arc::new(ScriptedProvider {
            infos: [
                ("tid".to_string(), old_movie_info()),
                ("new".to_string(), new_movie_info("downloaded")),
            ]
            .into_iter()
            .collect(),
            flaky_lookups: true,
            ..Default::default()
        });
        let manager = RepairManager::new(provider.clone());

        let new = manager
            .try_instant_repair(&tid_locator())
            .await
            .expect("each 404 is retried, so the repair succeeds");

        assert_eq!(new.torrent_id, "new");
        assert_eq!(new.file_path, "/Movie.mkv");
        assert_eq!(provider.lookups_of("tid"), 2, "one 404, then found");
        assert_eq!(
            provider.lookups_of("new"),
            4,
            "the post-add and the final lookup each 404 once, then find it"
        );
        assert!(yield_until(|| !provider.deleted().is_empty()).await);
        assert_eq!(provider.deleted(), vec!["tid".to_string()]);
    }

    // ── what a new-id repair leaves for the next scan ───────────────────────────────────────────

    /// A two-episode pack, `tid` before the repair and `new` (Real-Debrid's re-add) after it, with
    /// an unselected sample between the episodes so the per-file links pair by position among the
    /// SELECTED files.
    fn pack_info(id: &str, ids: [u32; 3], links: &[&str]) -> TorrentInfo {
        let file = |id: u32, path: &str, selected: u32| crate::rd_client::TorrentFile {
            id,
            path: path.to_string(),
            bytes: 1000,
            selected,
        };
        TorrentInfo {
            id: id.to_string(),
            hash: "H".to_string(),
            status: "downloaded".to_string(),
            files: vec![
                file(ids[0], "/Show.S01E01.mkv", 1),
                file(ids[1], "/Sample.mkv", 0),
                file(ids[2], "/Show.S01E02.mkv", 1),
            ],
            links: links.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        }
    }

    fn pack_locator(torrent_id: &str, file_id: u32, path: &str, link: &str) -> FileLocator {
        FileLocator {
            hash: "H".to_string(),
            torrent_id: torrent_id.to_string(),
            file_id,
            file_path: path.to_string(),
            link: Some(link.to_string()),
        }
    }

    /// Real-Debrid re-adds the hash under a new id and the repair deletes the old torrent, but the
    /// VFS keeps the old torrent's locators until the next scan rebuilds it. Until then `redirect`
    /// maps every selected file of the old torrent onto the repaired one (by path, each with its own
    /// link), so a later open of the same file, or of a sibling episode, doesn't resolve the deleted
    /// torrent and fail. A handle opened before the repair holds the old locator past the VFS
    /// rebuild, so the redirect is kept while a listing has the old torrent or the copy its chain
    /// of redirects ends at, or while that copy is a replacement still waiting for the scan loop.
    /// It is dropped once none of those holds.
    #[tokio::test(start_paused = true)]
    async fn a_new_id_repair_redirects_every_file_of_the_old_torrent() {
        let provider = Arc::new(ScriptedProvider {
            infos: [
                (
                    "tid".to_string(),
                    pack_info("tid", [1, 2, 3], &["old1", "old2"]),
                ),
                (
                    "new".to_string(),
                    pack_info("new", [5, 6, 7], &["new1", "new2"]),
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        });
        let manager = RepairManager::new(provider.clone());
        let e01 = pack_locator("tid", 1, "/Show.S01E01.mkv", "old1");
        let e02 = pack_locator("tid", 3, "/Show.S01E02.mkv", "old2");

        assert!(
            manager.redirect(&e01).await.is_none(),
            "nothing repaired yet"
        );
        let repaired = manager
            .try_instant_repair(&e01)
            .await
            .expect("cached re-add");
        assert_eq!(repaired.torrent_id, "new");

        assert_eq!(
            manager.redirect(&e01).await,
            Some(pack_locator("new", 5, "/Show.S01E01.mkv", "new1"))
        );
        assert_eq!(
            manager.redirect(&e02).await,
            Some(pack_locator("new", 7, "/Show.S01E02.mkv", "new2")),
            "a sibling file of the repaired torrent is redirected too"
        );
        let other = FileLocator {
            torrent_id: "other".to_string(),
            ..e01.clone()
        };
        assert!(manager.redirect(&other).await.is_none());

        // The scan tick whose listing predates the repair still lists `tid`: its VFS has the old
        // locators, so the redirect is kept.
        manager
            .prune_health_status(&["tid"].into_iter().collect())
            .await;
        assert!(manager.redirect(&e02).await.is_some());
        // The next tick takes the replacement, and its listing lacks `tid` (deleted, or left out
        // as a same-hash duplicate of the newer copy): the VFS is rebuilt on the new torrent, but a
        // handle opened before the repair still holds the old locator, so the redirect lives as
        // long as the copy it points at is listed.
        assert_eq!(manager.take_repair_replacements().await.len(), 1);
        manager
            .prune_health_status(&["new"].into_iter().collect())
            .await;
        assert_eq!(
            manager.redirect(&e02).await,
            Some(pack_locator("new", 7, "/Show.S01E02.mkv", "new2")),
            "kept while the replacement is listed"
        );
        // Once neither torrent is listed it goes.
        manager.prune_health_status(&HashSet::new()).await;
        assert!(manager.redirect(&e02).await.is_none());
    }

    /// A handle opened before another handle's new-id repair still holds the old torrent's locator.
    /// When its read fails, the repair hands it the copy already recorded for that file instead of
    /// re-repairing the deleted torrent (a fresh attempt whose lookup 404s, failing the read).
    #[tokio::test(start_paused = true)]
    async fn a_repair_of_an_already_repaired_torrent_uses_the_recorded_copy() {
        let provider = Arc::new(ScriptedProvider {
            infos: [
                (
                    "tid".to_string(),
                    pack_info("tid", [1, 2, 3], &["old1", "old2"]),
                ),
                (
                    "new".to_string(),
                    pack_info("new", [5, 6, 7], &["new1", "new2"]),
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        });
        let manager = RepairManager::new(provider.clone());
        let e01 = pack_locator("tid", 1, "/Show.S01E01.mkv", "old1");
        let e02 = pack_locator("tid", 3, "/Show.S01E02.mkv", "old2");
        manager
            .try_instant_repair(&e01)
            .await
            .expect("cached re-add");
        let adds = provider.adds.load(std::sync::atomic::Ordering::SeqCst);
        let lookups = provider.lookups_of("tid");

        let repaired = manager
            .try_instant_repair(&e02)
            .await
            .expect("the recorded copy of the sibling file");
        assert_eq!(repaired, pack_locator("new", 7, "/Show.S01E02.mkv", "new2"));
        assert_eq!(
            provider.adds.load(std::sync::atomic::Ordering::SeqCst),
            adds,
            "no re-add"
        );
        assert_eq!(
            provider.lookups_of("tid"),
            lookups,
            "no lookup of the deleted torrent"
        );
        assert!(
            !manager.health_status.read().await.contains_key("tid"),
            "no health entry for the deleted torrent"
        );
    }

    /// A torrent repaired twice (a still-broken file on a storm of re-adds, each a new id) is
    /// redirected to the latest copy, not to the deleted one in between.
    #[tokio::test]
    async fn redirect_follows_a_chain_of_repairs() {
        let manager = make_test_manager();
        manager
            .record_redirect("a", &pack_info("b", [5, 6, 7], &["b1", "b2"]), "H")
            .await;
        manager
            .record_redirect("b", &pack_info("c", [8, 9, 10], &["c1", "c2"]), "H")
            .await;
        assert_eq!(
            manager
                .redirect(&pack_locator("a", 3, "/Show.S01E02.mkv", "a2"))
                .await,
            Some(pack_locator("c", 10, "/Show.S01E02.mkv", "c2"))
        );
    }

    /// A chain of redirects is kept while its LAST copy is listed (a handle opened on the first
    /// torrent still reaches it), and dropped as a whole once nothing in it is listed.
    #[tokio::test]
    async fn a_redirect_chain_is_kept_while_its_last_copy_is_listed() {
        let manager = make_test_manager();
        manager
            .record_redirect("a", &pack_info("b", [5, 6, 7], &["b1", "b2"]), "H")
            .await;
        manager
            .record_redirect("b", &pack_info("c", [8, 9, 10], &["c1", "c2"]), "H")
            .await;
        let a2 = pack_locator("a", 3, "/Show.S01E02.mkv", "a2");
        manager
            .prune_health_status(&["c"].into_iter().collect())
            .await;
        assert_eq!(
            manager.redirect(&a2).await,
            Some(pack_locator("c", 10, "/Show.S01E02.mkv", "c2")),
            "the whole chain survives while `c` is listed"
        );
        manager.prune_health_status(&HashSet::new()).await;
        assert!(manager.redirect(&a2).await.is_none());
        assert!(manager.repaired_locators.read().await.is_empty());
    }

    /// A redirect whose replacement the scan loop hasn't taken yet is kept even by a listing that
    /// lacks both torrents (one fetched before the add and after the old torrent's delete).
    #[tokio::test]
    async fn a_redirect_to_a_replacement_still_waiting_for_the_scan_loop_is_kept() {
        let manager = make_test_manager();
        manager
            .record_redirect("old", &pack_info("new", [5, 6, 7], &["n1", "n2"]), "H")
            .await;
        manager
            .repair_replacements
            .write()
            .await
            .insert("new".to_string(), "old".to_string());
        manager.prune_health_status(&HashSet::new()).await;
        assert!(manager
            .redirect(&pack_locator("old", 3, "/Show.S01E02.mkv", "o2"))
            .await
            .is_some());
    }

    /// Replacements the scan loop took for a tick it then skipped (a failed or suspect listing) are
    /// put back for the next tick; one recorded since the take wins over the restored entry.
    #[tokio::test]
    async fn restored_repair_replacements_are_taken_again_without_overwriting_newer_ones() {
        let manager = make_test_manager();
        manager
            .repair_replacements
            .write()
            .await
            .insert("new1".to_string(), "old1".to_string());
        let taken = manager.take_repair_replacements().await;
        // A repair recorded meanwhile for the same new id (cannot really happen, but it must win).
        manager
            .repair_replacements
            .write()
            .await
            .insert("new1".to_string(), "newer".to_string());
        manager
            .repair_replacements
            .write()
            .await
            .insert("new2".to_string(), "old2".to_string());
        manager.restore_repair_replacements(taken).await;
        let again = manager.take_repair_replacements().await;
        assert_eq!(again.len(), 2);
        assert_eq!(again.get("new1").map(String::as_str), Some("newer"));
        assert_eq!(again.get("new2").map(String::as_str), Some("old2"));
    }

    /// The scan loop prunes the health map against the listing it fetched at the start of its tick.
    /// A Real-Debrid repair that completes during the tick has a new id that listing lacks, and its
    /// entry carries the cooldown and the attempt budget across the re-add, so it must survive the
    /// prune until the scan loop has taken the replacement (the next tick's listing has the new id).
    #[tokio::test]
    async fn prune_health_status_keeps_a_repair_the_listing_predates() {
        let manager = make_test_manager();
        manager.health_status.write().await.insert(
            "old".to_string(),
            TorrentHealth {
                torrent_id: "old".to_string(),
                state: RepairState::Repairing,
                repair_attempts: 2,
                last_repair_trigger: Some(tokio::time::Instant::now()),
            },
        );
        manager.complete_repair("old", "new").await;

        manager
            .prune_health_status(&["old"].into_iter().collect())
            .await;
        {
            let hm = manager.health_status.read().await;
            let h = hm
                .get("new")
                .expect("the repaired torrent's entry survives a listing that predates it");
            assert_eq!(h.repair_attempts, 2, "the attempt budget is carried");
            assert!(h.last_repair_trigger.is_some(), "the cooldown is carried");
        }

        // The next tick takes the replacement, then prunes against a listing with the new id.
        assert_eq!(manager.take_repair_replacements().await.len(), 1);
        manager
            .prune_health_status(&["new"].into_iter().collect())
            .await;
        assert!(manager.health_status.read().await.contains_key("new"));
        // A torrent gone from the listing after that is pruned as before.
        manager.prune_health_status(&HashSet::new()).await;
        assert!(manager.health_status.read().await.is_empty());
    }
}
