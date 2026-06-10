// Journal routes: GET/POST /api/journal, /api/journal/{id}, /api/journal/writers.
// Parity port of the server.ts journal section.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::{Extension, Router};
use hive_shared::NewJournalEntry;
use serde::Deserialize;

use crate::error::{err, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/journal/writers", get(writers))
        .route("/api/journal", get(list).post(append))
        .route("/api/journal/{id}", get(get_one))
}

#[derive(Deserialize)]
struct WritersQuery {
    viewer: Option<String>,
}

async fn writers(State(s): State<Store>, Query(q): Query<WritersQuery>) -> ApiResult {
    let Some(viewer) = q.viewer.filter(|v| !v.is_empty()) else {
        return Ok(err(StatusCode::BAD_REQUEST, "viewer required"));
    };
    Ok(Json(s.journal_writers(&viewer).await?).into_response())
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
    offset: Option<i64>,
    viewer: Option<String>,
    writers: Option<String>,
}

async fn list(State(s): State<Store>, Query(q): Query<ListQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    // Node truthiness: an empty viewer string falls through to the unscoped list.
    if let Some(viewer) = q.viewer.filter(|v| !v.is_empty()) {
        let writers: Option<Vec<String>> = q.writers.map(|w| {
            w.split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(String::from)
                .collect()
        });
        let entries = s
            .visible_journal(&viewer, writers.as_deref(), limit, offset)
            .await?;
        return Ok(Json(entries).into_response());
    }
    Ok(Json(s.journal_list(limit, offset).await?).into_response())
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
    let view = s.journal_append(input, Some(&actor)).await?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn get_one(State(s): State<Store>, Path(id): Path<String>) -> ApiResult {
    match s.journal_get(&id).await? {
        Some(e) => Ok(Json(e).into_response()),
        None => Ok(not_found()),
    }
}
