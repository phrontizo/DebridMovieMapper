use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Database error: {0}")]
    Db(#[from] redb::Error),

    #[error("Repair failed: {0}")]
    Repair(String),

    #[error("Invalid configuration: {0}")]
    Config(String),

    #[error("Background task failed: {0}")]
    Task(String),

    #[error("Debrid resource temporarily unavailable")]
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_variant_displays() {
        let e = AppError::Unavailable;
        assert_eq!(e.to_string(), "Debrid resource temporarily unavailable");
    }

    #[test]
    fn string_variants_display_with_their_payload() {
        // The hand-written `#[error(...)]` format strings must interpolate the payload, since these
        // messages reach logs and the enrolment page; a regression here would hide the cause.
        assert_eq!(
            AppError::Repair("no candidate".into()).to_string(),
            "Repair failed: no candidate"
        );
        assert_eq!(
            AppError::Config("both tokens set".into()).to_string(),
            "Invalid configuration: both tokens set"
        );
        assert_eq!(
            AppError::Task("scheduler panicked".into()).to_string(),
            "Background task failed: scheduler panicked"
        );
    }

    #[test]
    fn db_error_converts_via_from_and_displays() {
        // The `#[from] redb::Error` conversion is what lets `store.rs` use `?` on redb errors; verify
        // a redb error maps to the `Db` variant and its Display carries the source message.
        let redb_err = redb::Error::from(redb::TableError::TableDoesNotExist("missing".into()));
        let app: AppError = redb_err.into();
        assert!(matches!(app, AppError::Db(_)));
        assert!(app.to_string().starts_with("Database error: "), "got {app}");
    }
}
