use std::path::PathBuf;

/// Everything the store can fail with.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("Database not found: {0}")]
    NotFound(PathBuf),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = DbError> = std::result::Result<T, E>;

impl DbError {
    pub(crate) fn other(msg: impl Into<String>) -> Self {
        DbError::Other(msg.into())
    }
}
