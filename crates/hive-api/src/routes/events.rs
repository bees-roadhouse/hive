//! `/events` ... CRUD for the date-anchored events entity.
//!
//! Mirrors `routes/journal.rs`: list / get-by-id-or-slug / create. The
//! `GET /events/{id_or_slug}` form accepts either a UUID or a slug so the
//! mention resolver can `<a href="/events/birthday-2026">` straight to the
//! row without the UI having to know UUIDs.
//!
//! The SSE stream that used to live at `/events` is now at `/events/stream`
//! (see `routes/stream.rs`).

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use hive_db::queries::events;

use crate::error::ApiError;
use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/events", get(list).post(add))
        .route("/events/{id_or_slug}", get(show))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    tag: Option<String>,
    limit: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<hive_db::types::Event>>, ApiError> {
    let filters = events::ListFilters {
        from: q.from,
        to: q.to,
        tag: q.tag,
        limit: q.limit,
    };
    let rows = events::list(&state.pool, &filters).await?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
struct AddBody {
    title: String,
    body: Option<String>,
    starts_at: DateTime<Utc>,
    ends_at: Option<DateTime<Utc>>,
    location: Option<String>,
    tags: Option<String>,
    /// Optional explicit slug. Omit to derive from title.
    slug: Option<String>,
}

async fn add(
    State(state): State<AppState>,
    Json(body): Json<AddBody>,
) -> Result<Json<hive_db::types::Event>, ApiError> {
    let e = events::add(
        &state.pool,
        body.slug.as_deref(),
        &body.title,
        body.body.as_deref(),
        body.starts_at,
        body.ends_at,
        body.location.as_deref(),
        body.tags.as_deref(),
    )
    .await?;
    state
        .emitter
        .emit(
            HiveEvent::now("event.created", "events", e.id).with_extra(serde_json::json!({
                "title": e.title,
                "slug": e.slug,
                "starts_at": e.starts_at,
                "ends_at": e.ends_at,
                "location": e.location,
                "tags": e.tags,
            })),
        );
    Ok(Json(e))
}

async fn show(
    State(state): State<AppState>,
    Path(id_or_slug): Path<String>,
) -> Result<Json<hive_db::types::Event>, ApiError> {
    let row = resolve(&state, &id_or_slug).await?;
    Ok(Json(row))
}

/// Try UUID parse first; on failure (or no row), fall through to slug lookup.
/// Returns NotFound if neither matches.
async fn resolve(state: &AppState, id_or_slug: &str) -> Result<hive_db::types::Event, ApiError> {
    if let Ok(uuid) = Uuid::parse_str(id_or_slug)
        && let Some(row) = events::get(&state.pool, uuid).await?
    {
        return Ok(row);
    }
    events::find_by_slug(&state.pool, id_or_slug)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("event {id_or_slug}")))
}
