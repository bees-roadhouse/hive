//! AI directory routes (migration 0013). Read-only, mirror of `/people`.
//!
//! `/ai` is the directory: who are pia/apis/cera? It's intentionally separate
//! from the auth-side `/ai-identities` routes (`routes::ai_identities`), which
//! are about grant-shape (which human granted which scopes to which AI). A
//! row here can mention OR be mentioned without ever owning a grant; the two
//! tables exist for different reasons.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use serde::Deserialize;
use uuid::Uuid;

use hive_db::queries::ai;
use hive_db::types::Ai;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ai", get(list))
        // {id_or_slug} ... UUID first, slug fallback. Mirror of /people/{...}.
        .route("/ai/{id_or_slug}", get(show))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Ai>>, ApiError> {
    let rows = ai::list(&state.pool, q.limit).await?;
    Ok(Json(rows))
}

async fn show(
    State(state): State<AppState>,
    Path(id_or_slug): Path<String>,
) -> Result<Json<Ai>, ApiError> {
    if let Ok(id) = Uuid::parse_str(&id_or_slug)
        && let Some(a) = ai::get(&state.pool, id).await?
    {
        return Ok(Json(a));
    }
    let a = ai::find_by_slug(&state.pool, &id_or_slug)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("ai {id_or_slug}")))?;
    Ok(Json(a))
}
