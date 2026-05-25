use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use serde::{Deserialize, Serialize};

use hive_db::queries::search;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/search", get(search_endpoint))
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    10
}

#[derive(Debug, Serialize)]
struct CombinedHits {
    journal: Vec<search::JournalHit>,
    notes: Vec<search::NoteHit>,
}

async fn search_endpoint(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<CombinedHits>, ApiError> {
    let j = search::journal(&state.pool, &q.q, q.limit).await?;
    let n = search::notes(&state.pool, &q.q, q.limit).await?;
    Ok(Json(CombinedHits {
        journal: j,
        notes: n,
    }))
}
