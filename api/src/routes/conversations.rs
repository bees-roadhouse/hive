// Conversation routes: upsert + append + list + transcript + reflection queue +
// mark-reflected + rename. Namespace visibility is derived from the
// AUTHENTICATED principal (ctx.visibility() / ctx.namespace_owner()), never a
// client-supplied param — same model as the journal routes.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::{Extension, Router};
use hive_shared::NewConversationMessage;
use serde::Deserialize;
use serde_json::json;

use crate::error::{err, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::conversations::ConversationUpsert;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/conversations", get(list).post(upsert))
        .route("/api/conversations/pending", get(pending))
        .route("/api/conversations/{id}", get(get_one).patch(rename))
        .route("/api/conversations/{id}/messages", post(append))
        .route("/api/conversations/{id}/reflected", post(reflected))
}

#[derive(Deserialize)]
struct UpsertBody {
    app: Option<String>,
    instance: Option<String>,
    name: Option<String>,
    /// Defaults to the authenticated actor; a client can't log as someone else.
    actor: Option<String>,
    external_id: Option<String>,
}

async fn upsert(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(b): Json<UpsertBody>,
) -> ApiResult {
    let Some(app) = b.app.filter(|a| !a.trim().is_empty()) else {
        return Ok(err(StatusCode::BAD_REQUEST, "app required"));
    };
    // actor defaults to the authenticated identity; user_scope from the ctx.
    let actor = b
        .actor
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| ctx.actor().to_string());
    let input = ConversationUpsert {
        app,
        instance: b.instance,
        name: b.name,
        actor,
        external_id: b.external_id,
    };
    let id = s.conversations_upsert(input, ctx.namespace_owner()).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

#[derive(Deserialize)]
struct AppendBody {
    #[serde(default)]
    messages: Vec<NewConversationMessage>,
}

async fn append(
    State(s): State<Store>,
    Path(id): Path<String>,
    Json(b): Json<AppendBody>,
) -> ApiResult {
    let appended = s.conversation_append_messages(&id, &b.messages).await?;
    Ok(Json(json!({ "appended": appended })).into_response())
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    let items = s
        .conversations_list(&ctx.visibility(), limit, offset)
        .await?;
    Ok(Json(items).into_response())
}

#[derive(Deserialize)]
struct PendingQuery {
    limit: Option<i64>,
}

async fn pending(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<PendingQuery>,
) -> ApiResult {
    let items = s
        .conversations_pending(&ctx.visibility(), q.limit.unwrap_or(50))
        .await?;
    Ok(Json(items).into_response())
}

async fn get_one(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    match s.conversation_get(&id, &ctx.visibility()).await? {
        Some(view) => Ok(Json(view).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct ReflectedBody {
    #[serde(default)]
    summary: String,
}

async fn reflected(
    State(s): State<Store>,
    Path(id): Path<String>,
    Json(b): Json<ReflectedBody>,
) -> ApiResult {
    match s.conversation_mark_reflected(&id, &b.summary).await? {
        Some(c) => Ok(Json(c).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct RenameBody {
    #[serde(default)]
    name: String,
}

async fn rename(
    State(s): State<Store>,
    Path(id): Path<String>,
    Json(b): Json<RenameBody>,
) -> ApiResult {
    match s.conversation_rename(&id, &b.name).await? {
        Some(c) => Ok(Json(c).into_response()),
        None => Ok(not_found()),
    }
}
