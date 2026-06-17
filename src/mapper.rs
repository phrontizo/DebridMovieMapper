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
