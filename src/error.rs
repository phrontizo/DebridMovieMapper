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
}
