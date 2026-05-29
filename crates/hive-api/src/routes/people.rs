//! People (humans) directory routes. Mirror of `/ai` from migration 0013.
//!
//! Read-only listing + show. After 0013 the `people` table is humans only:
//! nate, maggie. AI directory rows moved to the `ai` table; their routes
//! live in `routes::ai`.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use uuid::Uuid;

use hive_db::queries::people;
use hive_db::types::Person;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/people", get(list))
        // {id_or_slug} ... UUID first, slug fallback. Mirror of /ai/{...}.
        .route("/people/{id_or_slug}", get(show))
}

async fn list(State(state): State<AppState>) -> Result<Json<Vec<Person>>, ApiError> {
    let rows = people::list(&state.pool).await?;
    Ok(Json(rows))
}

async fn show(
    State(state): State<AppState>,
    Path(id_or_slug): Path<String>,
) -> Result<Json<Person>, ApiError> {
    if let Ok(id) = Uuid::parse_str(&id_or_slug)
        && let Some(p) = people::get(&state.pool, id).await?
    {
        return Ok(Json(p));
    }
    let p = people::find_by_slug(&state.pool, &id_or_slug)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("person {id_or_slug}")))?;
    Ok(Json(p))
}
