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
pub mod conversations;
pub mod custom;
pub mod entities;
pub mod flows;
pub mod identity_artifacts;
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
        .merge(agents_gated(conversations::router()))
        .merge(mail::router())
        .merge(entities::router())
        .merge(custom::router())
        .merge(search::router())
        .merge(oauth::router())
        .merge(admin::router())
        .merge(artifacts::router())
        .merge(identity_artifacts::router())
        .merge(agents_gated(workspaces::router()))
        .merge(flows::router())
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

/// The retired hosted-agent surface (docs/FLOWS.md D9): every route in the
/// wrapped router answers the standard 404 unless `HIVE_AGENTS_ENABLED` flips
/// it back on. `route_layer` so unmatched paths keep their normal handling.
fn agents_gated(r: Router<Store>) -> Router<Store> {
    r.route_layer(axum::middleware::from_fn(
        |req: axum::extract::Request, next: axum::middleware::Next| async move {
            if workspaces::agents_enabled() {
                next.run(req).await
            } else {
                json_404()
            }
        },
    ))
}
