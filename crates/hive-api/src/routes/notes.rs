use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use serde::Deserialize;
use uuid::Uuid;

use hive_db::enums::Author;
use hive_db::queries::{notes, search};

use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/notes", get(list).post(add))
        .route("/notes/search", get(search_endpoint))
        // {id_or_slug} ... UUID first, slug fallback. /search matched above.
        .route("/notes/{id_or_slug}", get(show))
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
    state.guard_structured_write("POST /notes")?;
    let n = notes::add(
        &state.pool,
        body.author,
        body.title.as_deref(),
        &body.body,
        body.project.as_deref(),
        body.tags.as_deref(),
    )
    .await?;
    state
        .emitter
        .emit(
            HiveEvent::now("note.created", "notes", n.id).with_extra(serde_json::json!({
                "author": n.author,
                "title": n.title,
                "project": n.project,
                "tags": n.tags,
            })),
        );

    // Universal-mention pipeline: project links from prose in this note's
    // body. Best-effort (errors logged, never propagated).
    crate::mentions::project_body(&state.pool, "notes", n.id, &n.body).await;

    Ok(Json(n))
}

async fn show(
    State(state): State<AppState>,
    Path(id_or_slug): Path<String>,
) -> Result<Json<hive_db::types::Note>, ApiError> {
    if let Ok(id) = Uuid::parse_str(&id_or_slug)
        && let Some(n) = notes::get(&state.pool, id).await?
    {
        return Ok(Json(n));
    }
    let n = notes::find_by_slug(&state.pool, &id_or_slug)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("note {id_or_slug}")))?;
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
