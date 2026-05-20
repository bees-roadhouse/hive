use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::Json;
use serde::Deserialize;

use hive_db::enums::{Owner, TaskStatus};
use hive_db::queries::tasks;

use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks", get(list).post(add))
        .route("/tasks/{id}", get(show).patch(update))
        .route("/tasks/{id}/done", post(done))
        .route("/tasks/{id}/block", post(block))
        .route("/tasks/{id}/drop", post(drop))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    project: Option<String>,
    owner: Option<Owner>,
    status: Option<TaskStatus>,
    #[serde(default)]
    all: bool,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<hive_db::types::Task>>, ApiError> {
    let filters = tasks::ListFilters {
        project: q.project,
        owner: q.owner,
        status: q.status,
        all: q.all,
    };
    let rows = tasks::list(&state.pool, &filters).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    project: String,
    title: String,
    body: Option<String>,
    owner: Owner,
    priority: Option<String>,
    due: Option<String>,
}

async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddBody>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    let t = tasks::add(
        &state.pool,
        &body.project,
        &body.title,
        body.body.as_deref(),
        body.owner,
        body.priority.as_deref(),
        body.due.as_deref(),
    )
    .await?;
    state.emitter.emit(
        HiveEvent::now("task.created", "tasks", t.id).with_extra(serde_json::json!({
            "title": t.title,
            "owner": t.owner,
            "project": t.project,
            "priority": t.priority,
        })),
    );
    Ok(Json(t))
}

async fn show(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    let t = tasks::require(&state.pool, id).await?;
    Ok(Json(t))
}

#[derive(Debug, Deserialize)]
struct UpdateBody {
    status: Option<TaskStatus>,
    /// `priority`: outer absent = no change; inner null = clear; string = set.
    #[serde(default, deserialize_with = "deserialize_optional_optional")]
    priority: Option<Option<String>>,
    owner: Option<Owner>,
    #[serde(default, deserialize_with = "deserialize_optional_optional")]
    due: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional")]
    body: Option<Option<String>>,
    title: Option<String>,
}

fn deserialize_optional_optional<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(Some(Option::<String>::deserialize(d)?))
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    let fields = tasks::UpdateFields {
        status: body.status,
        priority: body.priority,
        owner: body.owner,
        due: body.due,
        body: body.body,
        title: body.title,
        block_reason: None,
    };
    tasks::update(&state.pool, id, &fields).await?;
    let t = tasks::require(&state.pool, id).await?;
    state.emitter.emit(
        HiveEvent::now("task.updated", "tasks", t.id).with_extra(serde_json::json!({
            "title": t.title,
            "owner": t.owner,
            "status": t.status,
            "priority": t.priority,
        })),
    );
    Ok(Json(t))
}

async fn done(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    tasks::mark_done(&state.pool, id).await?;
    let t = tasks::require(&state.pool, id).await?;
    state.emitter.emit(
        HiveEvent::now("task.done", "tasks", t.id).with_extra(serde_json::json!({
            "title": t.title,
            "owner": t.owner,
        })),
    );
    Ok(Json(t))
}

#[derive(Debug, Deserialize)]
struct BlockBody {
    reason: String,
}

async fn block(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<BlockBody>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    let reason = body.reason.clone();
    tasks::mark_blocked(&state.pool, id, &body.reason).await?;
    let t = tasks::require(&state.pool, id).await?;
    state.emitter.emit(
        HiveEvent::now("task.blocked", "tasks", t.id).with_extra(serde_json::json!({
            "title": t.title,
            "owner": t.owner,
            "reason": reason,
        })),
    );
    Ok(Json(t))
}

async fn drop(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    tasks::mark_dropped(&state.pool, id).await?;
    let t = tasks::require(&state.pool, id).await?;
    state.emitter.emit(
        HiveEvent::now("task.dropped", "tasks", t.id).with_extra(serde_json::json!({
            "title": t.title,
            "owner": t.owner,
        })),
    );
    Ok(Json(t))
}
