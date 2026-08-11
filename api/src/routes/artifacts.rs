// Claude Code artifacts (skills / agents / slash-commands) per AI identity.
//
// The sync endpoint (`GET /api/identity/artifacts`) returns the ENABLED
// artifacts for the AUTHENTICATED identity (`ctx.actor()`) — this is what the
// Claude Code plugin calls with the AI-identity token to pull THAT identity's
// artifacts. Keyed on the AI actor, not the per-user memory namespace.
//
// The management routes (list-incl-disabled, upsert, delete) are gated through
// the shared identity gate (middleware::can_act_for_identity): an admin, the
// identity itself, or — for sessions — the logged-in owner of that AI. Managed
// via REST/MCP this release; no UI surface.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get};
use axum::{Extension, Router};
use serde::Deserialize;

use crate::error::{err, forbidden, not_found, ApiResult};
use crate::middleware::{can_act_for_identity, AuthCtx};
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/identity/artifacts", get(sync))
        .route("/api/actors/{actor}/artifacts", get(list).post(upsert))
        .route("/api/artifacts/{id}", delete(remove))
}

/// The sync endpoint: enabled artifacts for the authenticated identity. The
/// plugin calls this with the AI-identity token; `ctx.actor()` is that identity.
async fn sync(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    let items = s.artifacts_for_actor(ctx.actor()).await?;
    Ok(Json(items).into_response())
}

/// Management listing: ALL artifacts (incl. disabled) for `{actor}`.
async fn list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(actor): Path<String>,
) -> ApiResult {
    if !can_act_for_identity(&s, &ctx, &actor).await? {
        return Ok(forbidden());
    }
    Ok(Json(s.artifacts_list(&actor).await?).into_response())
}

#[derive(Deserialize)]
struct UpsertBody {
    kind: Option<String>,
    name: Option<String>,
    content: Option<String>,
    description: Option<String>,
    enabled: Option<bool>,
}

const VALID_KINDS: &[&str] = &["skill", "agent", "command"];

/// Upsert an artifact for `{actor}` (keyed on kind+name). Owner-or-admin gated.
async fn upsert(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(actor): Path<String>,
    Json(body): Json<UpsertBody>,
) -> ApiResult {
    if !can_act_for_identity(&s, &ctx, &actor).await? {
        return Ok(forbidden());
    }
    let Some(kind) = body.kind.filter(|k| VALID_KINDS.contains(&k.as_str())) else {
        return Ok(err(
            StatusCode::BAD_REQUEST,
            "kind required (one of: skill, agent, command)",
        ));
    };
    let Some(name) = body.name.filter(|n| !n.trim().is_empty()) else {
        return Ok(err(StatusCode::BAD_REQUEST, "name required"));
    };
    let Some(content) = body.content.filter(|c| !c.is_empty()) else {
        return Ok(err(StatusCode::BAD_REQUEST, "content required"));
    };
    let artifact = s
        .artifacts_upsert(
            &actor,
            &kind,
            &name,
            &content,
            body.description.as_deref().unwrap_or(""),
            body.enabled.unwrap_or(true),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(artifact)).into_response())
}

/// Delete an artifact by id. Owner-or-admin gated (resolved via the row's actor).
async fn remove(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    let Some(artifact) = s.artifacts_get(&id).await? else {
        return Ok(not_found());
    };
    if !can_act_for_identity(&s, &ctx, &artifact.actor).await? {
        return Ok(forbidden());
    }
    if s.artifacts_remove(&id).await? {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok(not_found())
    }
}
