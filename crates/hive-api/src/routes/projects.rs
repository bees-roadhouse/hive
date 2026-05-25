use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use serde::Deserialize;

use hive_db::enums::{Owner, ProjectStatus};
use hive_db::queries::projects;

use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/projects", get(list).post(add))
        .route("/projects/{name}/archive", post(archive))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    status: Option<ProjectStatus>,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<hive_db::types::Project>>, ApiError> {
    let rows = projects::list(&state.pool, q.status).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    name: String,
    description: Option<String>,
    owner: Owner,
}

async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddBody>,
) -> Result<Json<hive_db::types::Project>, ApiError> {
    let p = projects::add(
        &state.pool,
        &body.name,
        body.description.as_deref(),
        body.owner,
    )
    .await?;
    state.emitter.emit(
        HiveEvent::now("project.created", "projects", p.id).with_extra(serde_json::json!({
            "name": p.name,
            "owner": p.owner,
            "description": p.description,
        })),
    );
    Ok(Json(p))
}

async fn archive(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    projects::archive(&state.pool, &name).await?;
    let p = projects::require(&state.pool, &name).await?;
    state.emitter.emit(
        HiveEvent::now("project.archived", "projects", p.id).with_extra(serde_json::json!({
            "name": name,
            "owner": p.owner,
        })),
    );
    Ok(Json(serde_json::json!({"archived": true})))
}
