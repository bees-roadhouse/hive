// Hosted Claude Code workspaces — HTTP surface. Per-user CRUD + transcript, the
// runner's ingest path, and the INTERNAL runtime-auth that hands decrypted owner
// credentials to the runner. Auth + per-user scoping come from AuthCtx (middleware).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::{err, forbidden, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::cc_credentials::NewCcCredential;
use crate::store::workspaces::{NewCcMessage, NewCcSession};
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/workspaces", get(list).post(create))
        .route("/api/workspaces/{id}", get(get_one))
        .route(
            "/api/workspaces/{id}/messages",
            get(transcript).post(ingest),
        )
        .route("/api/workspaces/{id}/input", post(send_input))
        .route("/api/workspaces/{id}/archive", post(archive))
        .route("/api/workspaces/{id}/status", post(set_status))
        .route("/api/workspaces/{id}/runtime-auth", get(runtime_auth))
        .route("/api/cc-credentials", get(creds_list).post(creds_put))
        .route("/api/cc-credentials/{id}", delete(creds_delete))
}

/// Require a non-anon principal (the /api gate already enforces auth; defensive).
// The Err carries a full axum Response by design (callers `return Ok(r)` it); that
// trips result_large_err, which is not meaningful for a two-call-site guard.
#[allow(clippy::result_large_err)]
fn require_actor(ctx: &AuthCtx) -> Result<String, Response> {
    match ctx.actor.as_deref() {
        Some(a) if a != "anon" => Ok(a.to_string()),
        _ => Err(err(StatusCode::UNAUTHORIZED, "authentication required")),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
}

async fn list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    Ok(Json(s.workspace_list(&ctx.visibility(), limit).await?).into_response())
}

async fn create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(input): Json<NewCcSession>,
) -> ApiResult {
    if let Err(r) = require_actor(&ctx) {
        return Ok(r);
    }
    let owner = ctx.namespace_user().to_string();
    let created_by = ctx.actor().to_string();
    let prompt = input.prompt.clone();
    let session = s.workspace_create(&owner, &created_by, input).await?;
    // Persist the kickoff prompt as the first transcript message so the runner can
    // pick it up and the UI shows it immediately.
    if let Some(text) = prompt.filter(|t| !t.trim().is_empty()) {
        s.workspace_append_message(&session.id, input_message(&text))
            .await?;
    }
    Ok((StatusCode::CREATED, Json(session)).into_response())
}

async fn get_one(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    match s.workspace_get(&ctx.visibility(), &id).await? {
        Some(ws) => Ok(Json(ws).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct TranscriptQuery {
    after: Option<i64>,
    limit: Option<i64>,
}

async fn transcript(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> ApiResult {
    if s.workspace_get(&ctx.visibility(), &id).await?.is_none() {
        return Ok(not_found());
    }
    let msgs = s
        .workspace_transcript(
            &id,
            q.after.unwrap_or(0),
            q.limit.unwrap_or(1000).clamp(1, 5000),
        )
        .await?;
    Ok(Json(msgs).into_response())
}

/// Runner → API: append a transcript message. Allowed for the owner or an admin
/// (the runner authenticates as a service/admin principal).
async fn ingest(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(input): Json<NewCcMessage>,
) -> ApiResult {
    let Some(ws) = s.workspace_get_internal(&id).await? else {
        return Ok(not_found());
    };
    if !ctx.is_admin() && ctx.namespace_user() != ws.owner {
        return Ok(forbidden());
    }
    Ok((
        StatusCode::CREATED,
        Json(s.workspace_append_message(&id, input).await?),
    )
        .into_response())
}

#[derive(Deserialize)]
struct InputBody {
    text: String,
}

/// Human → session: record the input as a transcript message and signal the runner.
async fn send_input(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(body): Json<InputBody>,
) -> ApiResult {
    if s.workspace_get(&ctx.visibility(), &id).await?.is_none() {
        return Ok(not_found());
    }
    let msg = s
        .workspace_append_message(&id, input_message(&body.text))
        .await?;
    s.emit(
        "workspace.input",
        ctx.actor(),
        json!({"session_id": id, "seq": msg.seq, "text": body.text}),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(msg)).into_response())
}

async fn archive(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if s.workspace_get(&ctx.visibility(), &id).await?.is_none() {
        return Ok(not_found());
    }
    s.workspace_archive(&id).await?;
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

/// Runner/owner sets a session's lifecycle status (running/idle/completed/failed/…).
async fn set_status(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(body): Json<StatusBody>,
) -> ApiResult {
    let Some(ws) = s.workspace_get_internal(&id).await? else {
        return Ok(not_found());
    };
    if !ctx.is_admin() && ctx.namespace_user() != ws.owner {
        return Ok(forbidden());
    }
    s.workspace_set_status(&id, &body.status).await?;
    Ok(Json(json!({"ok": true, "status": body.status})).into_response())
}

/// INTERNAL: hand the decrypted owner credential to the runner. Admin/service only.
async fn runtime_auth(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    let Some(ws) = s.workspace_get_internal(&id).await? else {
        return Ok(not_found());
    };
    match s.cc_cred_decrypt_for_runtime(&ws.owner).await? {
        Some((kind, secret)) => {
            let meta = ws.meta.as_object();
            Ok(Json(json!({
                "owner": ws.owner,
                "kind": kind,
                "secret": secret,
                "workdir": ws.workdir,
                "runtime": meta
                    .and_then(|m| m.get("runtime"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("claude_code"),
                "provider": meta.and_then(|m| m.get("provider")).and_then(|v| v.as_str()),
                "model": ws.model,
            }))
            .into_response())
        }
        None => Ok(err(
            StatusCode::FAILED_DEPENDENCY,
            "owner has no Claude Code credential saved",
        )),
    }
}

// ---- per-user credentials (redacted; secret never returned) ----

async fn creds_list(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    Ok(Json(s.cc_cred_list(ctx.namespace_user()).await?).into_response())
}

async fn creds_put(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(input): Json<NewCcCredential>,
) -> ApiResult {
    if let Err(r) = require_actor(&ctx) {
        return Ok(r);
    }
    if input.secret.trim().is_empty() || input.kind.trim().is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "kind and secret are required"));
    }
    let owner = ctx.namespace_user().to_string();
    Ok((
        StatusCode::CREATED,
        Json(s.cc_cred_put(&owner, input).await?),
    )
        .into_response())
}

async fn creds_delete(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if s.cc_cred_delete(ctx.namespace_user(), &id).await? {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok(not_found())
    }
}

fn input_message(text: &str) -> NewCcMessage {
    NewCcMessage {
        role: "user".to_string(),
        kind: "input".to_string(),
        content: json!({ "text": text }),
        raw: json!({}),
        tokens_in: None,
        tokens_out: None,
        claude_session_id: None,
    }
}
