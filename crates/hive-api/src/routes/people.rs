//! `/people` ... read-only directory over the `people` table.
//!
//! `GET /people` and `GET /people/{id_or_slug}` cover both AIs (`kind = 'ai'`)
//! and humans (`kind = 'human'`). The optional `?kind=ai|human` filter lets
//! the UI render separate `/people` and `/ai` listings off the same endpoint
//! until the split-ai migration carves AIs into their own table.
//!
//! Writes still go through the resolver's `ensure(...)` flow ... there's no
//! POST/PATCH here.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use serde::Deserialize;
use uuid::Uuid;

use hive_db::queries::people;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/people", get(list))
        .route("/people/{id_or_slug}", get(show))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    /// `ai` or `human`. Omit for both.
    kind: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<hive_db::types::Person>>, ApiError> {
    let kind = q.kind.as_deref().filter(|s| !s.is_empty());
    let rows = people::list_filtered(&state.pool, kind).await?;
    Ok(Json(rows))
}

async fn show(
    State(state): State<AppState>,
    Path(id_or_slug): Path<String>,
) -> Result<Json<hive_db::types::Person>, ApiError> {
    // UUID first, slug fallback ... matches the shape of /tasks, /events.
    if let Ok(_id) = Uuid::parse_str(&id_or_slug) {
        // No get-by-id helper yet; fall through to slug. Slug is unique, and
        // every person row has one (NOT NULL in the schema).
    }
    let p = people::get_by_slug(&state.pool, &id_or_slug)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("person {id_or_slug}")))?;
    Ok(Json(p))
}
