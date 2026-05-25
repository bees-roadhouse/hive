use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use hive_db::queries::links::{self, EntityRef};

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/links", get(outgoing_or_all).post(add))
        .route("/links/incoming", get(incoming))
        .route("/links/types", get(types))
        .route("/links/{id}", delete(remove))
}

#[derive(Debug, Deserialize)]
struct OutgoingQuery {
    /// `<table>:<id>` ... required.
    source: String,
}

async fn outgoing_or_all(
    State(state): State<AppState>,
    Query(q): Query<OutgoingQuery>,
) -> Result<Json<Vec<hive_db::types::Link>>, ApiError> {
    let src =
        EntityRef::parse(&q.source, "source").map_err(|e| ApiError::BadRequest(e.to_string()))?;
    links::require_exists(&state.pool, &src, "source").await?;
    let rows = links::outgoing(&state.pool, &src).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct IncomingQuery {
    target: String,
}

async fn incoming(
    State(state): State<AppState>,
    Query(q): Query<IncomingQuery>,
) -> Result<Json<Vec<hive_db::types::Link>>, ApiError> {
    let tgt =
        EntityRef::parse(&q.target, "target").map_err(|e| ApiError::BadRequest(e.to_string()))?;
    links::require_exists(&state.pool, &tgt, "target").await?;
    let rows = links::incoming(&state.pool, &tgt).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    source: String,
    target: String,
    link_type: Option<String>,
    note: Option<String>,
}

async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let src = EntityRef::parse(&body.source, "source")
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let tgt = EntityRef::parse(&body.target, "target")
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    links::require_exists(&state.pool, &src, "source").await?;
    links::require_exists(&state.pool, &tgt, "target").await?;
    let id = links::add(
        &state.pool,
        &src,
        &tgt,
        body.link_type.as_deref(),
        body.note.as_deref(),
    )
    .await?;
    match id {
        Some(id) => Ok(Json(json!({"id": id}))),
        None => Err(ApiError::Conflict(format!(
            "link already exists: {} -> {}",
            body.source, body.target
        ))),
    }
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    links::remove(&state.pool, id).await?;
    Ok(Json(json!({"removed": true})))
}

async fn types(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let rows = links::type_counts(&state.pool).await?;
    let payload: Vec<_> = rows
        .into_iter()
        .map(|r| json!({"link_type": r.link_type, "count": r.count}))
        .collect();
    Ok(Json(json!(payload)))
}
