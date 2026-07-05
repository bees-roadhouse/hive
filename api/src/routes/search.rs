// /api/search, /api/recall, /api/dashboard, /api/graph, /api/autocomplete,
// /api/embeddings — parity with server.ts. Owned by the search workstream.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::Deserialize;

use crate::error::{err, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::recall::RecallOptions;
use crate::store::semantic::SemanticOptions;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/search", get(search))
        .route("/api/recall", post(recall))
        .route("/api/dashboard", get(dashboard))
        .route("/api/graph", get(graph))
        .route("/api/autocomplete", get(autocomplete))
        .route("/api/embeddings", get(embeddings))
}

/// GET /api/search — ?mode=semantic|standard|precision routes to the semantic
/// engine (flags: hybrid/rerank/blanket/threshold/identity/peer); anything else
/// is FTS keyword search. Results are scoped to the AUTHENTICATED principal's
/// namespace (admins search unscoped); the client cannot widen this.
async fn search(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let query = q.get("q").cloned().unwrap_or_default();
    let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(25);
    // Admins search everything (viewer=None); everyone else is namespace-scoped.
    let viewer: Option<String> = if ctx.is_admin() {
        None
    } else {
        Some(ctx.namespace_user().to_string())
    };

    let mode = q.get("mode").map(String::as_str);
    if matches!(
        mode,
        Some("semantic") | Some("standard") | Some("precision")
    ) {
        let flag = |name: &str| matches!(q.get(name).map(String::as_str), Some("1") | Some("true"));
        let off = |name: &str| matches!(q.get(name).map(String::as_str), Some("0") | Some("false"));
        let opts = SemanticOptions {
            limit: Some(limit),
            mode: Some(if mode == Some("precision") {
                "precision".to_string()
            } else {
                "standard".to_string()
            }),
            hybrid: Some(!off("hybrid")),
            rerank: Some(flag("rerank")),
            blanket: if off("blanket") { Some(false) } else { None },
            threshold: q.get("threshold").and_then(|t| t.parse().ok()),
            viewer: viewer.clone(),
            identity: q.get("identity").filter(|v| !v.is_empty()).cloned(),
            peer: q.get("peer").filter(|v| !v.is_empty()).cloned(),
            kinds: None,
        };
        return Ok(Json(s.semantic_search(&query, opts).await?).into_response());
    }
    Ok(Json(s.search(&query, limit, viewer.as_deref()).await?).into_response())
}

#[derive(Deserialize, Default)]
struct RecallBody {
    identity: Option<String>,
    peer: Option<String>,
    query: Option<String>,
    budget: Option<usize>,
    threshold: Option<f64>,
}

/// POST /api/recall — identity defaults to the acting user.
async fn recall(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(body): Json<RecallBody>,
) -> ApiResult {
    let identity = body.identity.unwrap_or_else(|| ctx.actor().to_string());
    if identity.is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "identity required"));
    }
    if !crate::middleware::can_act_for_identity(&s, &ctx, &identity).await? {
        return Ok(err(StatusCode::FORBIDDEN, "not_your_identity"));
    }
    let viewer = if ctx.is_admin() {
        None
    } else {
        Some(ctx.namespace_user().to_string())
    };
    let result = s
        .recall(
            &identity,
            RecallOptions {
                peer: body.peer,
                query: body.query,
                budget: body.budget,
                threshold: body.threshold,
                viewer,
            },
        )
        .await?;
    Ok(Json(result).into_response())
}

// dashboard / graph / autocomplete are cross-namespace aggregate views (counts,
// the full knowledge graph, global typeahead). Until they're per-namespace they
// are admin-only so they can't leak other users' entries.
async fn dashboard(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(err(StatusCode::FORBIDDEN, "admin only"));
    }
    Ok(Json(s.dashboard().await?).into_response())
}

async fn graph(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(err(StatusCode::FORBIDDEN, "admin only"));
    }
    Ok(Json(s.graph().await?).into_response())
}

#[derive(Deserialize)]
struct AutocompleteQuery {
    q: Option<String>,
    kinds: Option<String>,
}

async fn autocomplete(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(params): Query<AutocompleteQuery>,
) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(err(StatusCode::FORBIDDEN, "admin only"));
    }
    let q = params.q.unwrap_or_default();
    let kinds = params
        .kinds
        .map(|k| k.split(',').map(|s| s.trim().to_string()).collect());
    Ok(Json(s.autocomplete(&q, kinds).await?).into_response())
}

async fn embeddings(State(s): State<Store>) -> ApiResult {
    Ok(Json(s.embedding_stats().await?).into_response())
}
