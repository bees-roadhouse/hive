//! `/search/semantic` ... vector search endpoint skeleton. fastembed-rs
//! integration is in flight on a parallel branch; this returns 501 so clients
//! can target it now and the markov-blanket boost logic is verified pre-fastembed.

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use hive_db::queries::graph;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/search/semantic", get(semantic_endpoint))
}

#[derive(Debug, Deserialize)]
struct SemanticQuery {
    #[allow(dead_code)] // wired through once fastembed lands
    q: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default = "default_scope")]
    #[allow(dead_code)]
    scope: String,
    #[serde(default = "default_limit")]
    #[allow(dead_code)]
    limit: i64,
    /// `table:id` form, e.g. `journal_entries:418` or `tasks:47`.
    context: Option<String>,
    #[serde(default = "default_context_mode")]
    #[allow(dead_code)]
    context_mode: String,
    #[serde(default = "default_context_depth")]
    context_depth: u32,
}

fn default_mode() -> String {
    "precision".into()
}
fn default_scope() -> String {
    "all".into()
}
fn default_limit() -> i64 {
    10
}
fn default_context_mode() -> String {
    "boost".into()
}
fn default_context_depth() -> u32 {
    2
}

#[derive(Debug, Serialize)]
struct SemanticHit {
    source_table: String,
    source_id: Uuid,
    score: f64,
    title: Option<String>,
    snippet: String,
    blanket_match: bool,
}

#[derive(Debug, Serialize)]
struct SemanticResponse {
    mode: String,
    results: Vec<SemanticHit>,
    blanket_size: usize,
    implemented: bool,
    note: &'static str,
}

async fn semantic_endpoint(
    State(state): State<AppState>,
    Query(q): Query<SemanticQuery>,
) -> Result<(StatusCode, Json<SemanticResponse>), ApiError> {
    let mode = q.mode.clone();
    // Parse context spec ... `<table>:<id>`. If malformed, surface as bad request.
    let context = match q.context.as_deref() {
        Some(spec) => Some(parse_context(spec)?),
        None => None,
    };
    let depth = q.context_depth;

    // Compute the blanket even though semantic results aren't wired ... proves
    // the BFS path works against the real `links` table pre-fastembed.
    let blanket_size = if let Some((table, id)) = context {
        graph::markov_blanket(&state.pool, &table, id, depth)
            .await
            .map(|v| v.len())?
    } else {
        0
    };

    let body = SemanticResponse {
        mode,
        results: Vec::new(),
        blanket_size,
        implemented: false,
        note: "fastembed integration pending",
    };
    Ok((StatusCode::NOT_IMPLEMENTED, Json(body)))
}

fn parse_context(spec: &str) -> Result<(String, Uuid), ApiError> {
    let (t, i) = spec.split_once(':').ok_or_else(|| {
        ApiError::BadRequest(format!("context must be <table>:<uuid>, got '{spec}'"))
    })?;
    if t.is_empty() {
        return Err(ApiError::BadRequest("context table missing".into()));
    }
    let id = Uuid::parse_str(i)
        .map_err(|_| ApiError::BadRequest(format!("context id not a uuid: '{i}'")))?;
    Ok((t.to_string(), id))
}
