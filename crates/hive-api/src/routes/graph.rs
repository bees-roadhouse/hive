use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::routing::get;
use serde::Deserialize;

use hive_db::queries::graph::{self, GraphOptions, GraphPayload};

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/graph", get(get_graph))
}

#[derive(Debug, Deserialize)]
struct GraphQuery {
    #[serde(default = "default_min")]
    min: i64,
    #[serde(default = "default_tags")]
    tags: i64,
    #[serde(default = "default_nodes")]
    nodes: i64,
    #[serde(default)]
    include_meta: bool,
}

fn default_min() -> i64 {
    2
}
fn default_tags() -> i64 {
    80
}
fn default_nodes() -> i64 {
    600
}

async fn get_graph(
    State(state): State<AppState>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<GraphPayload>, ApiError> {
    let opts = GraphOptions {
        min_tag_count: q.min,
        limit_tags: q.tags,
        limit_nodes: q.nodes,
        include_meta: q.include_meta,
    };
    let payload = graph::build(&state.pool, opts).await?;
    Ok(Json(payload))
}
