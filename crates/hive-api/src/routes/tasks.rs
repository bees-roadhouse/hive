use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::Json;
use serde::Deserialize;

use hive_db::enums::{Owner, TaskStatus};
use hive_db::queries::tasks;

use crate::error::ApiError;
use crate::state::AppState;
use crate::with_conn;

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
    let rows = with_conn(&state, move |c| tasks::list(c, &filters)).await?;
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
    let t = with_conn(&state, move |c| {
        tasks::add(
            c,
            &body.project,
            &body.title,
            body.body.as_deref(),
            body.owner,
            body.priority.as_deref(),
            body.due.as_deref(),
        )
    })
    .await?;
    Ok(Json(t))
}

async fn show(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    let t = with_conn(&state, move |c| tasks::require(c, id)).await?;
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
    with_conn(&state, move |c| {
        tasks::update(c, id, &fields)?;
        tasks::require(c, id)
    })
    .await
    .map(Json)
}

async fn done(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    with_conn(&state, move |c| {
        tasks::mark_done(c, id)?;
        tasks::require(c, id)
    })
    .await
    .map(Json)
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
    with_conn(&state, move |c| {
        tasks::mark_blocked(c, id, &body.reason)?;
        tasks::require(c, id)
    })
    .await
    .map(Json)
}

async fn drop(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    with_conn(&state, move |c| {
        tasks::mark_dropped(c, id)?;
        tasks::require(c, id)
    })
    .await
    .map(Json)
}
