use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use serde::Deserialize;
use uuid::Uuid;

use hive_db::enums::{Owner, TaskStatus};
use hive_db::queries::tasks;

use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks", get(list).post(add))
        // GET takes UUID or slug; PATCH still requires UUID (mutations resolve
        // the slug client-side first if needed).
        .route("/tasks/{id_or_slug}", get(show).patch(update))
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
    state.guard_structured_write("POST /tasks")?;
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
    state
        .emitter
        .emit(
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
    Path(id_or_slug): Path<String>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    if let Ok(id) = Uuid::parse_str(&id_or_slug)
        && let Some(t) = tasks::get(&state.pool, id).await?
    {
        return Ok(Json(t));
    }
    let t = tasks::find_by_slug(&state.pool, &id_or_slug)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("task {id_or_slug}")))?;
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
    Path(id_or_slug): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    state.guard_structured_write("PATCH /tasks/{id}")?;
    let id = resolve_task_id(&state, &id_or_slug).await?;
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
    state
        .emitter
        .emit(
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
    Path(id): Path<Uuid>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    state.guard_structured_write("POST /tasks/{id}/done")?;
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
    Path(id): Path<Uuid>,
    Json(body): Json<BlockBody>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    state.guard_structured_write("POST /tasks/{id}/block")?;
    let reason = body.reason.clone();
    tasks::mark_blocked(&state.pool, id, &body.reason).await?;
    let t = tasks::require(&state.pool, id).await?;
    state
        .emitter
        .emit(
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
    Path(id): Path<Uuid>,
) -> Result<Json<hive_db::types::Task>, ApiError> {
    state.guard_structured_write("POST /tasks/{id}/drop")?;
    tasks::mark_dropped(&state.pool, id).await?;
    let t = tasks::require(&state.pool, id).await?;
    state
        .emitter
        .emit(
            HiveEvent::now("task.dropped", "tasks", t.id).with_extra(serde_json::json!({
                "title": t.title,
                "owner": t.owner,
            })),
        );
    Ok(Json(t))
}

/// Resolve a `{id_or_slug}` path param to a task id. UUID parse first,
/// `find_by_slug` fallback. Returns NotFound if neither matches.
async fn resolve_task_id(state: &AppState, id_or_slug: &str) -> Result<Uuid, ApiError> {
    if let Ok(id) = Uuid::parse_str(id_or_slug)
        && tasks::get(&state.pool, id).await?.is_some()
    {
        return Ok(id);
    }
    let t = tasks::find_by_slug(&state.pool, id_or_slug)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("task {id_or_slug}")))?;
    Ok(t.id)
}
