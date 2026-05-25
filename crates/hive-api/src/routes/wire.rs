use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use hive_db::enums::Severity;
use hive_db::queries::wire;

use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/wire", get(list).post(add))
        .route("/wire/{id}/ack", post(ack))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    source: Option<String>,
    severity: Option<Severity>,
    #[serde(default)]
    unacknowledged: bool,
    limit: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<hive_db::types::WireEvent>>, ApiError> {
    let filters = wire::ListFilters {
        source: q.source,
        severity: q.severity,
        unacknowledged: q.unacknowledged,
        limit: q.limit,
    };
    let rows = wire::list(&state.pool, &filters).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    source: String,
    title: String,
    body: Option<String>,
    external_id: Option<String>,
    severity: Option<Severity>,
    affects: Option<String>,
    url: Option<String>,
    category: Option<String>,
}

async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let res = wire::add(
        &state.pool,
        wire::AddArgs {
            source: &body.source,
            title: &body.title,
            body: body.body.as_deref(),
            external_id: body.external_id.as_deref(),
            severity: body.severity,
            affects: body.affects.as_deref(),
            url: body.url.as_deref(),
            category: body.category.as_deref(),
        },
    )
    .await?;
    Ok(Json(match res {
        wire::AddResult::Added(e) => {
            state.emitter.emit(
                HiveEvent::now("wire.event", "wire_events", e.id).with_extra(serde_json::json!({
                    "source": e.source,
                    "title": e.title,
                    "severity": e.severity,
                    "category": e.category,
                    "url": e.url,
                    "affects": e.affects,
                })),
            );
            json!({"added": e})
        }
        wire::AddResult::AlreadySeen { id } => json!({"already_seen": {"id": id}}),
    }))
}

async fn ack(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    wire::ack(&state.pool, id).await?;
    state
        .emitter
        .emit(HiveEvent::now("wire.acked", "wire_events", id));
    Ok(Json(json!({"acknowledged": true})))
}
