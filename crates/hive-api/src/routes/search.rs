use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::Json;
use serde::{Deserialize, Serialize};

use hive_db::queries::search;

use crate::error::ApiError;
use crate::state::AppState;
use crate::with_conn;

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
    let limit = q.limit;
    let query = q.q;
    let hits = with_conn(&state, move |c| {
        let j = search::journal(c, &query, limit)?;
        let n = search::notes(c, &query, limit)?;
        Ok::<_, hive_db::Error>(CombinedHits { journal: j, notes: n })
    })
    .await?;
    Ok(Json(hits))
}
