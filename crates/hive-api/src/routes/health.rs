use axum::Router;
use axum::routing::get;
use serde_json::json;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/healthz", get(|| async { axum::Json(json!({"ok": true})) }))
}
