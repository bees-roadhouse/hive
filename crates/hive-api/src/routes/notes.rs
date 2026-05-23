use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use hive_db::enums::Author;
use hive_db::queries::{notes, search};

use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/notes", get(list).post(add))
        .route("/notes/{id}", get(show))
        .route("/notes/search", get(search_endpoint))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    author: Option<Author>,
    project: Option<String>,
    tag: Option<String>,
    limit: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<hive_db::types::Note>>, ApiError> {
    let filters = notes::ListFilters {
        author: q.author,
        project: q.project,
        tag: q.tag,
        limit: q.limit,
    };
    let rows = notes::list(&state.pool, &filters).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    author: Author,
    title: Option<String>,
    body: String,
    project: Option<String>,
    tags: Option<String>,
}

async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddBody>,
) -> Result<Json<hive_db::types::Note>, ApiError> {
    let n = notes::add(
        &state.pool,
        body.author,
        body.title.as_deref(),
        &body.body,
        body.project.as_deref(),
        body.tags.as_deref(),
    )
    .await?;
    state.emitter.emit(
        HiveEvent::now("note.created", "notes", n.id).with_extra(serde_json::json!({
            "author": n.author,
            "title": n.title,
            "project": n.project,
            "tags": n.tags,
        })),
    );
    Ok(Json(n))
}

async fn show(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<hive_db::types::Note>, ApiError> {
    let n = notes::require(&state.pool, id).await?;
    Ok(Json(n))
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
) -> Result<Json<Vec<search::NoteHit>>, ApiError> {
    let hits = search::notes(&state.pool, &q.q, q.limit).await?;
    Ok(Json(hits))
}
