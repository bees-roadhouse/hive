use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, patch};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use hive_db::enums::Severity;
use hive_db::queries::wire_sources::{self, UpdateFields};

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/wire/sources", get(list).post(add))
        .route("/wire/sources/{id}", patch(update).delete(remove))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(default)]
    enabled_only: bool,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<wire_sources::WireSource>>, ApiError> {
    let rows = wire_sources::list(&state.pool, q.enabled_only).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    name: String,
    #[serde(default = "default_kind")]
    kind: String,
    url: String,
    #[serde(default = "default_interval")]
    poll_interval_secs: i32,
    source_tag: String,
    category: Option<String>,
    affects: Option<String>,
    default_severity: Option<Severity>,
}

fn default_kind() -> String {
    "rss".into()
}

fn default_interval() -> i32 {
    3600
}

async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddBody>,
) -> Result<Json<wire_sources::WireSource>, ApiError> {
    let row = wire_sources::add(
        &state.pool,
        wire_sources::AddArgs {
            name: &body.name,
            kind: &body.kind,
            url: &body.url,
            poll_interval_secs: body.poll_interval_secs,
            source_tag: &body.source_tag,
            category: body.category.as_deref(),
            affects: body.affects.as_deref(),
            default_severity: body.default_severity,
        },
    )
    .await?;
    Ok(Json(row))
}

#[derive(Debug, Deserialize, Default)]
struct PatchBody {
    url: Option<String>,
    enabled: Option<bool>,
    poll_interval_secs: Option<i32>,
    category: Option<String>,
    affects: Option<String>,
    default_severity: Option<Severity>,
    clear_category: Option<bool>,
    clear_affects: Option<bool>,
    clear_default_severity: Option<bool>,
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<PatchBody>,
) -> Result<Json<wire_sources::WireSource>, ApiError> {
    let fields = UpdateFields {
        url: body.url,
        enabled: body.enabled,
        poll_interval_secs: body.poll_interval_secs,
        category: body
            .clear_category
            .filter(|&v| v)
            .map(|_| None)
            .or_else(|| body.category.map(Some)),
        affects: body
            .clear_affects
            .filter(|&v| v)
            .map(|_| None)
            .or_else(|| body.affects.map(Some)),
        default_severity: body
            .clear_default_severity
            .filter(|&v| v)
            .map(|_| None)
            .or_else(|| body.default_severity.map(Some)),
    };
    let row = wire_sources::update(&state.pool, id, &fields).await?;
    Ok(Json(row))
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    wire_sources::remove(&state.pool, id).await?;
    Ok(Json(json!({"removed": true})))
}
