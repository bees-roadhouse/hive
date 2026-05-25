use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use serde::Deserialize;
use uuid::Uuid;

use hive_db::queries::messages;

use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/messages", get(list).post(add))
        .route("/messages/search", get(search_endpoint))
        .route("/messages/{id}", get(show))
        .route("/messages/{id}/read", post(mark_read))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    from: Option<String>,
    to: Option<String>,
    kind: Option<String>,
    #[serde(default)]
    unread_only: bool,
    in_reply_to: Option<Uuid>,
    limit: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<messages::Message>>, ApiError> {
    let filters = messages::ListFilters {
        from_ai: q.from,
        to_ai: q.to,
        kind: q.kind,
        in_reply_to: q.in_reply_to,
        unread_only: q.unread_only,
        limit: q.limit,
    };
    let rows = messages::list(&state.pool, &filters).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    sender_ai: String,
    recipient_ai: String,
    kind: Option<String>,
    body: String,
    in_reply_to: Option<Uuid>,
}

async fn add(
    State(state): State<AppState>,
    Json(b): Json<AddBody>,
) -> Result<Json<messages::Message>, ApiError> {
    // surface validation as 400 before hitting the db
    if b.sender_ai.trim().is_empty() {
        return Err(ApiError::BadRequest("sender_ai must not be empty".into()));
    }
    if b.recipient_ai.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "recipient_ai must not be empty".into(),
        ));
    }
    if b.body.trim().is_empty() {
        return Err(ApiError::BadRequest("body must not be empty".into()));
    }
    let m = messages::add(
        &state.pool,
        &b.sender_ai,
        &b.recipient_ai,
        b.kind.as_deref(),
        &b.body,
        b.in_reply_to,
    )
    .await?;
    state
        .emitter
        .emit(
            HiveEvent::now("message.sent", "messages", m.id).with_extra(serde_json::json!({
                "sender_ai": m.sender_ai,
                "recipient_ai": m.recipient_ai,
                "kind": m.kind,
                "in_reply_to": m.in_reply_to,
            })),
        );
    Ok(Json(m))
}

async fn show(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<messages::Message>, ApiError> {
    let m = messages::require(&state.pool, id).await?;
    Ok(Json(m))
}

async fn mark_read(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<messages::Message>, ApiError> {
    let m = messages::mark_read(&state.pool, id).await?;
    state
        .emitter
        .emit(
            HiveEvent::now("message.read", "messages", m.id).with_extra(serde_json::json!({
                "sender_ai": m.sender_ai,
                "recipient_ai": m.recipient_ai,
                "kind": m.kind,
            })),
        );
    Ok(Json(m))
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    20
}

async fn search_endpoint(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Vec<messages::MessageHit>>, ApiError> {
    let hits = messages::search(&state.pool, &q.q, q.limit).await?;
    Ok(Json(hits))
}
