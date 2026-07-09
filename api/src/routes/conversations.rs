// Conversation capture — HTTP surface for SessionEnd ingest of local agent
// sessions plus the reflection queue, over the same cc_* tables as the hosted
// workspaces (origin='captured' rows only). Gating mirrors the workspaces
// ingest pattern: owner-or-admin on writes, visibility on reads, with owner/
// namespace derived from the AUTHENTICATED principal (never a body param).
//
// Deliberately NO journal mirroring anywhere on this surface (unlike hosted
// ingest): reflection summarizes captured transcripts into the journal later;
// mirroring here would double-write every turn.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use hive_shared::{ConversationReflected, NewCapturedConversation, NewConversationMessages};
use serde::Deserialize;
use serde_json::json;

use crate::error::{err, forbidden, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/conversations", post(upsert))
        .route("/api/conversations/pending", get(pending))
        .route("/api/conversations/{id}", get(get_one))
        .route("/api/conversations/{id}/messages", post(messages))
        .route("/api/conversations/{id}/reflected", post(reflected))
}

/// Require a non-anon principal (the /api gate already enforces auth; defensive).
#[allow(clippy::result_large_err)]
fn require_actor(ctx: &AuthCtx) -> Result<String, Response> {
    match ctx.actor.as_deref() {
        Some(a) if a != "anon" => Ok(a.to_string()),
        _ => Err(err(StatusCode::UNAUTHORIZED, "authentication required")),
    }
}

/// Idempotent capture upsert keyed on (runtime, external_id) for
/// origin='captured'. Owner is the authenticated namespace; status is always
/// 'captured' so the runner claim loop (which only picks up 'provisioning')
/// can never drive these sessions.
async fn upsert(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(input): Json<NewCapturedConversation>,
) -> ApiResult {
    if let Err(r) = require_actor(&ctx) {
        return Ok(r);
    }
    if input.external_id.trim().is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "external_id required"));
    }
    let owner = ctx.namespace_user().to_string();
    let created_by = ctx.actor().to_string();
    match s
        .conversation_upsert_captured(&owner, &created_by, input)
        .await?
    {
        Some(id) => Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response()),
        // The capture key belongs to a different owner's session.
        None => Ok(forbidden()),
    }
}

/// Transcript write: replace=true swaps the whole stored transcript (resumed
/// sessions re-send the FULL transcript), else appends. Owner-or-admin, same
/// gate as the workspaces ingest path. Any write re-queues reflection.
async fn messages(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(body): Json<NewConversationMessages>,
) -> ApiResult {
    let Some(conv) = s.conversation_get_captured(&id).await? else {
        return Ok(not_found());
    };
    if !ctx.is_admin() && ctx.namespace_user() != conv.owner {
        return Ok(forbidden());
    }
    let appended = s
        .conversation_replace_messages(&id, &body.messages, body.replace)
        .await?;
    Ok(Json(json!({ "appended": appended, "replaced": body.replace })).into_response())
}

#[derive(Deserialize)]
struct PendingQuery {
    limit: Option<i64>,
}

/// The reflection queue: captured conversations with reflected_at IS NULL,
/// oldest first. Visibility-gated (owner sees own; admins see all).
async fn pending(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<PendingQuery>,
) -> ApiResult {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    Ok(Json(s.conversations_pending(&ctx.visibility(), limit).await?).into_response())
}

/// A conversation + its transcript flattened to plain text (what the
/// reflector consumes). Hidden as 404 outside the viewer's namespace.
async fn get_one(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    match s.conversation_get_flat(&ctx.visibility(), &id).await? {
        Some(view) => Ok(Json(view).into_response()),
        None => Ok(not_found()),
    }
}

/// Stamp the reflection cursor (+ optional rolling summary). Owner-or-admin.
async fn reflected(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(body): Json<ConversationReflected>,
) -> ApiResult {
    let Some(conv) = s.conversation_get_captured(&id).await? else {
        return Ok(not_found());
    };
    if !ctx.is_admin() && ctx.namespace_user() != conv.owner {
        return Ok(forbidden());
    }
    match s
        .conversation_mark_reflected(&id, body.summary.as_deref())
        .await?
    {
        Some(c) => Ok(Json(c).into_response()),
        None => Ok(not_found()),
    }
}
