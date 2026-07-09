// Router composition. Each resource area owns its file and exposes
// `router() -> Router<Store>`; this module merges them and layers the shared
// CORS + auth middleware. Route paths mirror server.ts exactly.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use serde_json::json;

use crate::middleware::auth_and_cors;
use crate::store::Store;

pub mod admin;
pub mod artifacts;
pub mod auth;
pub mod custom;
pub mod entities;
pub mod journal;
pub mod mail;
pub mod mcp;
pub mod oauth;
pub mod people;
pub mod search;
pub mod spa;
pub mod stream;
pub mod workspaces;

pub fn router(store: Store) -> Router {
    Router::new()
        .route("/api/healthz", get(healthz))
        .route("/api/wire", get(wire))
        .route("/api/projects", get(projects_list))
        .merge(auth::router())
        .merge(people::router())
        .merge(journal::router())
        .merge(mail::router())
        .merge(entities::router())
        .merge(custom::router())
        .merge(search::router())
        .merge(oauth::router())
        .merge(admin::router())
        .merge(artifacts::router())
        .merge(workspaces::router())
        .merge(mcp::router())
        .merge(stream::router())
        .merge(spa::router())
        .layer(axum::middleware::from_fn_with_state(
            store.clone(),
            auth_and_cors,
        ))
        .with_state(store)
}

async fn healthz() -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "service": "hive-rust",
        "mcp": "/mcp",
        "ts": crate::auth::now_iso(),
    }))
}

async fn wire(
    axum::extract::State(s): axum::extract::State<Store>,
    axum::extract::Query(q): axum::extract::Query<WireQuery>,
) -> crate::error::ApiResult {
    let events = s.wire_log(q.limit.unwrap_or(100)).await?;
    Ok(Json(events).into_response())
}

#[derive(serde::Deserialize)]
struct WireQuery {
    limit: Option<i64>,
}

async fn projects_list(
    axum::extract::State(s): axum::extract::State<Store>,
) -> crate::error::ApiResult {
    let list = s.projects_list().await?;
    Ok(Json(list).into_response())
}

/// Standard JSON 404 body (Node's `{error: "not found"}` shape).
pub fn json_404() -> axum::response::Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
}
