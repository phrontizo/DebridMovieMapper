/// Seconds since the Unix epoch (saturating to 0 if the system clock is before 1970 — never panics).
/// The single canonical "now" used across the acquisition/upgrade/Trakt timestamp logic.
pub fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub mod acquire;
pub mod app_state;
pub mod config;
pub mod dav_fs;
pub mod enrolment;
pub mod error;
pub mod identification;
pub mod jellyfin_client;
pub mod probe;
pub mod provider;
pub mod ratelimit;
pub mod rd_client;
pub mod reacquire;
pub mod read_activity;
pub mod release;
pub mod repair;
pub mod scheduler;
pub mod scraper;
pub mod store;
pub mod tasks;
pub mod tmdb_client;
pub mod torbox_client;
pub mod trakt_client;
pub mod upgrade;
pub mod vfs;
pub mod wanted;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_unix_secs_is_recent_and_non_decreasing() {
        let a = now_unix_secs();
        // Sanity floor: well after 2023-11 (1_700_000_000). Catches a zeroed/garbage clock and the
        // saturating-to-0 path firing in normal operation. Also proves it does not panic.
        assert!(a > 1_700_000_000, "epoch seconds look wrong: {a}");
        let b = now_unix_secs();
        assert!(b >= a, "monotonic within a call sequence: {a} then {b}");
    }
}
