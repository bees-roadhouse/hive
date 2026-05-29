use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use serde::Deserialize;
use uuid::Uuid;

use hive_db::enums::Ai;
use hive_db::queries::{journal, search};

use crate::auth::extractor::MaybeAuthUser;
use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/journal", get(list).post(add))
        .route("/journal/{id}", get(show))
        .route("/journal/search", get(search_endpoint))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    ai: Option<Ai>,
    from: Option<String>,
    to: Option<String>,
    tag: Option<String>,
    limit: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<hive_db::types::JournalEntry>>, ApiError> {
    let filters = journal::ListFilters {
        ai: q.ai,
        from_date: q.from,
        to_date: q.to,
        tag: q.tag,
        limit: q.limit,
    };
    // Phase 8 (§5.6): run the read inside an RLS-armed transaction so the
    // per-request `app.*` GUCs (visibility + handles for the resolved principal)
    // land on the same connection. Shadow-safe: with HIVE_RLS_ENFORCE off (or no
    // principal), the DB policies default-allow and this is behaviorally
    // identical to the prior `journal::list(&pool, ..)` call.
    let mut tx = state.rls_begin(auth.0.as_ref()).await?;
    let rows = journal::list_in(&mut *tx, &filters).await?;
    tx.commit()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    ai: Ai,
    /// YYYY-MM-DD; default today.
    date: Option<String>,
    title: Option<String>,
    body: String,
    tags: Option<String>,
}

async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddBody>,
) -> Result<Json<hive_db::types::JournalEntry>, ApiError> {
    let date = body.date.clone().unwrap_or_else(|| {
        chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    });
    let e = journal::add(
        &state.pool,
        body.ai,
        &date,
        body.title.as_deref(),
        &body.body,
        body.tags.as_deref(),
    )
    .await?;
    state.emitter.emit(
        HiveEvent::now("journal.created", "journal_entries", e.id).with_extra(serde_json::json!({
            "ai": e.ai,
            "title": e.title,
            "entry_date": e.entry_date,
            "tags": e.tags,
        })),
    );

    // Universal-mention pipeline (best-effort projection): extract prose
    // mentions + inline-task anchors from the body, resolve them, write rows
    // into `links` / `task_anchors`. Errors are logged in the hook itself ...
    // the entry creation already succeeded and is not rolled back.
    crate::mentions::project_body(&state.pool, "journal_entries", e.id, &e.body).await;

    Ok(Json(e))
}

async fn show(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<hive_db::types::JournalEntry>, ApiError> {
    let e = journal::require(&state.pool, id).await?;
    Ok(Json(e))
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
) -> Result<Json<Vec<search::JournalHit>>, ApiError> {
    let hits = search::journal(&state.pool, &q.q, q.limit).await?;
    Ok(Json(hits))
}
