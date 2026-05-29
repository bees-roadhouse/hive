use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use serde::Deserialize;
use uuid::Uuid;

use hive_db::enums::Ai;
use hive_db::queries::{journal, search, tasks};

use crate::auth::extractor::MaybeAuthUser;
use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/journal", get(list).post(add))
        .route("/journal/search", get(search_endpoint))
        .route("/journal/{id}/tasks", get(tasks_for_entry))
        // {id_or_slug} ... UUID parsed first, slug fallback. /search is matched
        // above so it doesn't fall into this catch-all.
        .route("/journal/{id_or_slug}", get(show))
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
    let entry_body = assign_missing_task_block_ids(&body.body);
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
        &entry_body,
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
    Path(id_or_slug): Path<String>,
) -> Result<Json<hive_db::types::JournalEntry>, ApiError> {
    if let Ok(id) = Uuid::parse_str(&id_or_slug)
        && let Some(e) = journal::get(&state.pool, id).await?
    {
        return Ok(Json(e));
    }
    let e = journal::find_by_slug(&state.pool, &id_or_slug)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("journal_entry {id_or_slug}")))?;
    Ok(Json(e))
}

fn assign_missing_task_block_ids(body: &str) -> String {
    let parsed = hive_md::parse(body);
    let mut next = parsed
        .tasks
        .iter()
        .filter_map(|t| t.block_id.as_deref())
        .filter_map(|id| id.strip_prefix("task"))
        .filter_map(|n| n.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1;

    hive_md::assign_block_ids(body, || {
        let id = format!("task{next}");
        next += 1;
        id
    })
}

#[cfg(test)]
mod tests {
    use super::assign_missing_task_block_ids;

    #[test]
    fn assigns_missing_task_block_ids_without_rewriting_existing_ones() {
        let body = "- [ ] first\n- [x] second ^task4\n- [ ] third";
        let out = assign_missing_task_block_ids(body);

        assert_eq!(
            out,
            "- [ ] first ^task5\n- [x] second ^task4\n- [ ] third ^task6"
        );
    }
}

async fn tasks_for_entry(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<hive_db::types::Task>>, ApiError> {
    let rows = tasks::list_for_journal_entry(&state.pool, id).await?;
    Ok(Json(rows))
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
