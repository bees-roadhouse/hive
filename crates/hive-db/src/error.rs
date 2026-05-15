use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database not found at {0}. run: hive init")]
    DbNotFound(std::path::PathBuf),

    #[error("not found: {kind} {id}")]
    NotFound { kind: &'static str, id: String },

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("invalid {field} '{value}'. valid: {valid}")]
    InvalidEnum {
        field: &'static str,
        value: String,
        valid: String,
    },

    #[error("invalid {field} '{value}'. expected {expected}")]
    InvalidFormat {
        field: &'static str,
        value: String,
        expected: &'static str,
    },

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("pool error: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
