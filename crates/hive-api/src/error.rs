use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    BadRequest(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    code: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        let body = ErrorBody {
            error: self.to_string(),
            code,
        };
        (status, Json(body)).into_response()
    }
}

impl From<hive_db::Error> for ApiError {
    fn from(e: hive_db::Error) -> Self {
        match e {
            hive_db::Error::NotFound { kind, id } => ApiError::NotFound(format!("{kind} {id}")),
            hive_db::Error::AlreadyExists(s) => ApiError::Conflict(s),
            hive_db::Error::InvalidEnum { .. } | hive_db::Error::InvalidFormat { .. } => {
                ApiError::BadRequest(e.to_string())
            }
            hive_db::Error::Sqlx(_) | hive_db::Error::Migrate(_) | hive_db::Error::Io(_) => {
                ApiError::Internal(e.to_string())
            }
        }
    }
}

/// The auth store error (incl. the Phase-8 RLS GUC plumbing) folds to Internal —
/// a SET LOCAL / txn failure is a server fault, not a client one.
impl From<crate::auth::store::StoreError> for ApiError {
    fn from(e: crate::auth::store::StoreError) -> Self {
        ApiError::Internal(e.to_string())
    }
}
