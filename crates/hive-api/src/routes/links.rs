use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use hive_db::enums::LinkTable;
use hive_db::queries::links::{self, EntityRef};
use hive_db::types::Link;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/links", get(get_links).post(add))
        .route("/links/incoming", get(incoming))
        .route("/links/types", get(types))
        .route("/links/{id}", delete(remove))
}

/// Combined GET /links query: accepts either the legacy `?source=<t>:<id>` /
/// `?target=<t>:<id>` shapes OR the new `?source_table=&source_id=` /
/// `?target_table=&target_id=` shapes.
///
/// The new shape returns *enriched* rows: each row includes the source and
/// target titles where the underlying entity carries one (notes/journal/tasks
/// use `title`, projects use `name`, people use `display_name`). The legacy
/// `source=`/`target=` shape returns the bare row for back-compat ... callers
/// (`hive-cli`, prior UI) parse the flat Link shape.
#[derive(Debug, Deserialize)]
struct LinksQuery {
    /// Legacy `<table>:<uuid>` for outgoing.
    source: Option<String>,
    /// Legacy `<table>:<uuid>` for incoming.
    target: Option<String>,

    source_table: Option<String>,
    source_id: Option<Uuid>,
    target_table: Option<String>,
    target_id: Option<Uuid>,
}

async fn get_links(
    State(state): State<AppState>,
    Query(q): Query<LinksQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Source-side queries (outgoing).
    if let Some(spec) = q.source.as_deref() {
        let src =
            EntityRef::parse(spec, "source").map_err(|e| ApiError::BadRequest(e.to_string()))?;
        links::require_exists(&state.pool, &src, "source").await?;
        let rows = links::outgoing(&state.pool, &src).await?;
        return Ok(Json(serde_json::to_value(rows).unwrap_or(json!([]))));
    }
    if let (Some(table), Some(id)) = (q.source_table.as_deref(), q.source_id) {
        let src = entity_ref_from(table, id)?;
        links::require_exists(&state.pool, &src, "source").await?;
        let rows = links::outgoing(&state.pool, &src).await?;
        let enriched = enrich(&state.pool, rows).await?;
        return Ok(Json(serde_json::to_value(enriched).unwrap_or(json!([]))));
    }

    // Target-side queries (incoming).
    if let Some(spec) = q.target.as_deref() {
        let tgt =
            EntityRef::parse(spec, "target").map_err(|e| ApiError::BadRequest(e.to_string()))?;
        links::require_exists(&state.pool, &tgt, "target").await?;
        let rows = links::incoming(&state.pool, &tgt).await?;
        return Ok(Json(serde_json::to_value(rows).unwrap_or(json!([]))));
    }
    if let (Some(table), Some(id)) = (q.target_table.as_deref(), q.target_id) {
        let tgt = entity_ref_from(table, id)?;
        links::require_exists(&state.pool, &tgt, "target").await?;
        let rows = links::incoming(&state.pool, &tgt).await?;
        let enriched = enrich(&state.pool, rows).await?;
        return Ok(Json(serde_json::to_value(enriched).unwrap_or(json!([]))));
    }

    Err(ApiError::BadRequest(
        "specify one of: source, target, source_table+source_id, target_table+target_id"
            .to_string(),
    ))
}

fn entity_ref_from(table: &str, id: Uuid) -> Result<EntityRef, ApiError> {
    let table = LinkTable::parse_short(table).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(EntityRef { table, id })
}

#[derive(Debug, Clone, Serialize)]
pub struct EnrichedLink {
    pub id: Uuid,
    pub source_table: String,
    pub source_id: Uuid,
    pub source_title: Option<String>,
    pub target_table: String,
    pub target_id: Uuid,
    pub target_title: Option<String>,
    pub link_type: Option<String>,
    pub note: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Batch-fetch entity titles for the source/target sides of `rows` and fold
/// them into `EnrichedLink`. One query per distinct (table, id-batch) bucket
/// instead of N queries per row.
async fn enrich(pool: &PgPool, rows: Vec<Link>) -> Result<Vec<EnrichedLink>, ApiError> {
    let mut buckets: HashMap<String, Vec<Uuid>> = HashMap::new();
    for r in &rows {
        buckets
            .entry(r.source_table.clone())
            .or_default()
            .push(r.source_id);
        buckets
            .entry(r.target_table.clone())
            .or_default()
            .push(r.target_id);
    }

    let mut labels: HashMap<(String, Uuid), String> = HashMap::new();
    for (table, mut ids) in buckets {
        ids.sort();
        ids.dedup();
        let label_col = match label_column_for(&table) {
            Some(c) => c,
            None => continue, // unknown table ... no title to fetch
        };
        // Table + col are from a closed set; safe to interpolate.
        let sql = format!("SELECT id, {label_col} AS label FROM {table} WHERE id = ANY($1)");
        let res = sqlx::query(&sql).bind(&ids).fetch_all(pool).await;
        let q_rows = match res {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(table, error = %e, "links enrichment label fetch failed");
                continue;
            }
        };
        for row in q_rows {
            let id: Uuid = row
                .try_get("id")
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let label: Option<String> = row
                .try_get("label")
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            if let Some(label) = label {
                labels.insert((table.clone(), id), label);
            }
        }
    }

    Ok(rows
        .into_iter()
        .map(|r| EnrichedLink {
            source_title: labels.get(&(r.source_table.clone(), r.source_id)).cloned(),
            target_title: labels.get(&(r.target_table.clone(), r.target_id)).cloned(),
            id: r.id,
            source_table: r.source_table,
            source_id: r.source_id,
            target_table: r.target_table,
            target_id: r.target_id,
            link_type: r.link_type,
            note: r.note,
            created_at: r.created_at,
        })
        .collect())
}

fn label_column_for(table: &str) -> Option<&'static str> {
    match table {
        "tasks" | "notes" | "journal_entries" | "wire_events" => Some("title"),
        "projects" => Some("name"),
        "people" => Some("display_name"),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct IncomingQuery {
    target: String,
}

async fn incoming(
    State(state): State<AppState>,
    Query(q): Query<IncomingQuery>,
) -> Result<Json<Vec<hive_db::types::Link>>, ApiError> {
    let tgt =
        EntityRef::parse(&q.target, "target").map_err(|e| ApiError::BadRequest(e.to_string()))?;
    links::require_exists(&state.pool, &tgt, "target").await?;
    let rows = links::incoming(&state.pool, &tgt).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    source: String,
    target: String,
    link_type: Option<String>,
    note: Option<String>,
}

async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.guard_structured_write("POST /links")?;
    let src = EntityRef::parse(&body.source, "source")
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let tgt = EntityRef::parse(&body.target, "target")
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    links::require_exists(&state.pool, &src, "source").await?;
    links::require_exists(&state.pool, &tgt, "target").await?;
    let id = links::add(
        &state.pool,
        &src,
        &tgt,
        body.link_type.as_deref(),
        body.note.as_deref(),
    )
    .await?;
    match id {
        Some(id) => Ok(Json(json!({"id": id}))),
        None => Err(ApiError::Conflict(format!(
            "link already exists: {} -> {}",
            body.source, body.target
        ))),
    }
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.guard_structured_write("DELETE /links/{id}")?;
    links::remove(&state.pool, id).await?;
    Ok(Json(json!({"removed": true})))
}

async fn types(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let rows = links::type_counts(&state.pool).await?;
    let payload: Vec<_> = rows
        .into_iter()
        .map(|r| json!({"link_type": r.link_type, "count": r.count}))
        .collect();
    Ok(Json(json!(payload)))
}
