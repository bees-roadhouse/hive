// One error type for every handler: `?` on any anyhow-compatible error becomes
// a 500 `{"error": "..."}` — the same JSON shape Hono's error paths produce.
// Expected non-200s (404, 403, …) are returned as explicit responses, not errors.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;

pub struct ApiError(pub anyhow::Error);

pub type ApiResult = Result<Response, ApiError>;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

/// `{"error": msg}` with a status — the Node API's error idiom.
pub fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// 404 `{"error": "not found"}`.
pub fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}

/// 403 `{"error": "forbidden"}`.
pub fn forbidden() -> Response {
    err(StatusCode::FORBIDDEN, "forbidden")
}
