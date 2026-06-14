// Journal routes: GET/POST /api/journal, /api/journal/{id}, /api/journal/writers,
// POST /api/journal/reassign-scope (admin). Parity port of the server.ts journal
// section, with per-user-namespace visibility derived from the AUTHENTICATED
// principal (never a client-supplied ?viewer= param).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::{Extension, Router};
use hive_shared::NewJournalEntry;
use serde::Deserialize;
use serde_json::json;

use crate::error::{err, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/journal/writers", get(writers))
        .route("/api/journal", get(list).post(append))
        .route("/api/journal/reassign-scope", post(reassign_scope))
        .route("/api/journal/{id}", get(get_one))
}

async fn writers(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    Ok(Json(s.journal_writers(&ctx.visibility()).await?).into_response())
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    writers: Option<String>,
}

async fn list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    let writers: Option<Vec<String>> = q.writers.map(|w| {
        w.split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    });
    // Visibility is the authenticated principal's, not a client param.
    let entries = s
        .visible_journal(&ctx.visibility(), writers.as_deref(), limit, offset)
        .await?;
    Ok(Json(entries).into_response())
}

async fn append(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(raw): Json<serde_json::Value>,
) -> ApiResult {
    let has_body = raw
        .get("body")
        .and_then(|v| v.as_str())
        .is_some_and(|b| !b.trim().is_empty());
    if !has_body {
        return Ok(err(StatusCode::BAD_REQUEST, "body required"));
    }
    let mut input: NewJournalEntry = serde_json::from_value(raw)?;
    // Author is the authenticated identity — a client can't write as someone else.
    let actor = ctx.actor().to_string();
    input.author = Some(actor.clone());
    // The entry lands in the writing principal's namespace (their granting user).
    let view = s
        .journal_append(input, Some(&actor), ctx.namespace_owner())
        .await?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn get_one(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    match s.journal_get(&id, &ctx.visibility()).await? {
        Some(e) => Ok(Json(e).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct ReassignBody {
    #[serde(default)]
    match_unscoped: bool,
    from_user: Option<String>,
    author: Option<String>,
    /// New owner; null/omitted makes the matched entries global.
    to: Option<String>,
}

/// Admin: bulk-change the namespace owner of journal entries.
async fn reassign_scope(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(b): Json<ReassignBody>,
) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(err(StatusCode::FORBIDDEN, "admin only"));
    }
    let changed = s
        .journal_reassign_scope(
            b.match_unscoped,
            b.from_user.as_deref(),
            b.author.as_deref(),
            b.to.as_deref(),
        )
        .await?;
    Ok(Json(json!({ "changed": changed })).into_response())
}
