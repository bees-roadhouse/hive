// Tasks/decisions/events/topics/phases/projects-with-children/links/shares
// routes — parity port of the server.ts structured-entity sections.
// (GET /api/projects list lives in routes/mod.rs; autocomplete belongs to the
// search workstream.)

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::{Extension, Router};
use hive_shared::{DecisionPatch, NewShare, ShareScope, TaskPatch};
use serde::Deserialize;

use crate::error::{err, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::tasks::TaskFilter;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/tasks", get(tasks_list))
        .route("/api/tasks/{id}", get(tasks_get).patch(tasks_update))
        .route("/api/decisions", get(decisions_list))
        .route(
            "/api/decisions/{id}",
            get(decisions_get).patch(decisions_update),
        )
        .route("/api/events", get(events_list))
        .route("/api/events/{id}", get(events_get))
        .route("/api/shares", get(shares_list).post(shares_create))
        .route("/api/topics", get(topics_list))
        .route("/api/topics/{id}", get(topics_get))
        .route("/api/phases", get(phases_list))
        .route("/api/phases/{id}", get(phases_get))
        .route("/api/projects/{id}", get(projects_get))
        .route("/api/links/{id}", get(links_for_entity))
}

// ---- tasks (read + workflow only; creation flows via journal) ----

#[derive(Deserialize)]
struct TasksQuery {
    status: Option<String>,
    assignee: Option<String>,
    project: Option<String>,
}

async fn tasks_list(State(s): State<Store>, Query(q): Query<TasksQuery>) -> ApiResult {
    // Node truthiness: empty-string filters are skipped.
    let filter = TaskFilter {
        status: q.status.filter(|v| !v.is_empty()),
        assignee: q.assignee.filter(|v| !v.is_empty()),
        project: q.project.filter(|v| !v.is_empty()),
        phase: None,
    };
    Ok(Json(s.tasks_list(filter).await?).into_response())
}

async fn tasks_get(State(s): State<Store>, Path(id): Path<String>) -> ApiResult {
    match s.tasks_get(&id).await? {
        Some(t) => Ok(Json(t).into_response()),
        None => Ok(not_found()),
    }
}

async fn tasks_update(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(patch): Json<TaskPatch>,
) -> ApiResult {
    match s.tasks_update(&id, patch, ctx.actor()).await? {
        Some(t) => Ok(Json(t).into_response()),
        None => Ok(not_found()),
    }
}

// ---- decisions ----

#[derive(Deserialize)]
struct DecisionsQuery {
    status: Option<String>,
}

async fn decisions_list(State(s): State<Store>, Query(q): Query<DecisionsQuery>) -> ApiResult {
    let status = q.status.filter(|v| !v.is_empty());
    Ok(Json(s.decisions_list(status.as_deref()).await?).into_response())
}

async fn decisions_get(State(s): State<Store>, Path(id): Path<String>) -> ApiResult {
    match s.decisions_get(&id).await? {
        Some(d) => Ok(Json(d).into_response()),
        None => Ok(not_found()),
    }
}

async fn decisions_update(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(patch): Json<DecisionPatch>,
) -> ApiResult {
    match s.decisions_update(&id, patch, ctx.actor()).await? {
        Some(d) => Ok(Json(d).into_response()),
        None => Ok(not_found()),
    }
}

// ---- events ----

async fn events_list(State(s): State<Store>) -> ApiResult {
    Ok(Json(s.events_list().await?).into_response())
}

async fn events_get(State(s): State<Store>, Path(id): Path<String>) -> ApiResult {
    match s.events_get(&id).await? {
        Some(e) => Ok(Json(e).into_response()),
        None => Ok(not_found()),
    }
}

// ---- shares ----

#[derive(Deserialize)]
struct ShareBody {
    scope: Option<String>,
    #[serde(rename = "ref")]
    ref_: Option<String>,
    viewer: Option<String>,
}

async fn shares_create(State(s): State<Store>, Json(body): Json<ShareBody>) -> ApiResult {
    let (Some(scope), Some(ref_), Some(viewer)) = (
        body.scope.filter(|v| !v.is_empty()),
        body.ref_.filter(|v| !v.is_empty()),
        body.viewer.filter(|v| !v.is_empty()),
    ) else {
        return Ok(err(StatusCode::BAD_REQUEST, "scope, ref, viewer required"));
    };
    let share = s
        .shares_create(NewShare {
            scope: ShareScope::from_str_lossy(&scope),
            ref_,
            viewer,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(share)).into_response())
}

#[derive(Deserialize)]
struct SharesQuery {
    viewer: Option<String>,
}

async fn shares_list(State(s): State<Store>, Query(q): Query<SharesQuery>) -> ApiResult {
    let Some(viewer) = q.viewer.filter(|v| !v.is_empty()) else {
        return Ok(err(StatusCode::BAD_REQUEST, "viewer required"));
    };
    Ok(Json(s.shares_for_viewer(&viewer).await?).into_response())
}

// ---- misc ----

async fn topics_list(State(s): State<Store>) -> ApiResult {
    Ok(Json(s.topics_list().await?).into_response())
}

async fn topics_get(State(s): State<Store>, Path(id): Path<String>) -> ApiResult {
    match s.topics_get(&id).await? {
        Some(t) => Ok(Json(t).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct PhasesQuery {
    project: Option<String>,
}

async fn phases_list(State(s): State<Store>, Query(q): Query<PhasesQuery>) -> ApiResult {
    let project = q.project.filter(|v| !v.is_empty());
    Ok(Json(s.phases_list(project.as_deref()).await?).into_response())
}

async fn phases_get(State(s): State<Store>, Path(id): Path<String>) -> ApiResult {
    match s.phases_get(&id).await? {
        Some(ph) => Ok(Json(ph).into_response()),
        None => Ok(not_found()),
    }
}

/// Node `projects.withChildren` — the project plus its tasks and phases.
async fn projects_get(State(s): State<Store>, Path(id): Path<String>) -> ApiResult {
    let Some(p) = s.projects_get(&id).await? else {
        return Ok(not_found());
    };
    let tasks = s
        .tasks_list(TaskFilter {
            project: Some(id.clone()),
            ..TaskFilter::default()
        })
        .await?;
    let phases = s.phases_list(Some(&id)).await?;
    let mut v = serde_json::to_value(&p)?;
    v["tasks"] = serde_json::to_value(tasks)?;
    v["phases"] = serde_json::to_value(phases)?;
    Ok(Json(v).into_response())
}

async fn links_for_entity(State(s): State<Store>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(s.links_for_entity(&id).await?).into_response())
}
