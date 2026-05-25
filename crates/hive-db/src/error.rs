use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
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

    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migrate error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// True when the underlying sqlx error is a unique/primary-key violation.
    /// Mirrors the old rusqlite ConstraintViolation branch we used to match on.
    pub fn is_unique_violation(&self) -> bool {
        match self {
            Error::Sqlx(sqlx::Error::Database(db)) => {
                // Postgres SQLSTATE 23505 = unique_violation
                db.code().map(|c| c.as_ref() == "23505").unwrap_or(false)
            }
            _ => false,
        }
    }
}
