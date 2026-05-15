use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::Json;
use serde::Deserialize;

use hive_db::enums::{Owner, ProjectStatus};
use hive_db::queries::projects;

use crate::error::ApiError;
use crate::state::AppState;
use crate::with_conn;

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
    let rows = with_conn(&state, move |c| projects::list(c, q.status)).await?;
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
    let p = with_conn(&state, move |c| {
        projects::add(c, &body.name, body.description.as_deref(), body.owner)
    })
    .await?;
    Ok(Json(p))
}

async fn archive(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_conn(&state, move |c| projects::archive(c, &name)).await?;
    Ok(Json(serde_json::json!({"archived": true})))
}
