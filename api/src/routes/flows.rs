// The flow registry over HTTP (docs/FLOWS.md D3/D6). Read surface for every
// authenticated member (the SPA /flows page and external clients inspect the
// same registry MCP serves); enable/disable is admin, like every other
// registry-shaping write. Install (module upload → artifact storage →
// register) is F3 and deliberately absent here.

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;

use crate::error::{forbidden, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/flows", get(list))
        .route("/api/flows/{slug}", get(get_one).patch(patch_one))
        .route("/api/flows/{slug}/runs", get(runs))
}

/// Every registered flow, disabled included — the management view. Ensures
/// the builtins exist first, so the registry is self-seeding per org.
async fn list(State(s): State<Store>) -> ApiResult {
    s.flows_ensure_builtins().await?;
    Ok(Json(s.flows_list().await?).into_response())
}

async fn get_one(State(s): State<Store>, Path(slug): Path<String>) -> ApiResult {
    s.flows_ensure_builtins().await?;
    match s.flows_get(&slug).await? {
        Some(flow) => Ok(Json(flow).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct FlowPatch {
    enabled: Option<bool>,
}

/// Admin: flip a flow on or off. A disabled flow keeps its row and history;
/// its MCP tools stop being served (docs/FLOWS.md D6).
async fn patch_one(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(slug): Path<String>,
    Json(body): Json<FlowPatch>,
) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    let Some(enabled) = body.enabled else {
        return Ok(crate::error::err(
            axum::http::StatusCode::BAD_REQUEST,
            "enabled required",
        ));
    };
    match s.flows_set_enabled(&slug, enabled, ctx.actor()).await? {
        Some(flow) => Ok(Json(flow).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct RunsQuery {
    limit: Option<i64>,
}

/// Run history for one flow, newest first — what the F5 visualization reads.
async fn runs(
    State(s): State<Store>,
    Path(slug): Path<String>,
    Query(q): Query<RunsQuery>,
) -> ApiResult {
    let Some(flow) = s.flows_get(&slug).await? else {
        return Ok(not_found());
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    Ok(Json(s.flow_runs_list(&flow.id, limit).await?).into_response())
}
