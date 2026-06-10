// Actors delete/merge, bulk import, sources, worker status, outbox, fixtures
// (server.ts admin + worker sections). Owned by the admin workstream.
// NOTE: GET /api/embeddings is registered elsewhere (semantic workstream).

use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, patch, post};
use axum::{Extension, Router};
use hive_shared::{LegacyImport, NewSource, SourcePatch, UserRole};
use serde::Deserialize;

use crate::error::{err, forbidden, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/actors/{slug}", delete(actors_remove))
        .route("/api/actors/{slug}/merge", post(actors_merge))
        .route("/api/import", post(import_json))
        .route("/api/import/sqlite", post(import_sqlite))
        .route("/api/sources", get(sources_list).post(sources_create))
        .route(
            "/api/sources/{id}",
            patch(sources_update).delete(sources_remove),
        )
        .route("/api/sources/poll", post(sources_poll))
        .route("/api/worker", get(worker_status))
        .route("/api/outbox", get(outbox_list))
        .route("/api/_fixtures/sample.xml", get(fixture_xml))
        .route("/api/_fixtures/sample.html", get(fixture_html))
        // Uploaded legacy .db files can be tens of MB (server.ts relied on
        // Node's unbounded body; 64 MB is the Rust branch's explicit cap).
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
}

/// Node requireAdminActor: session admin, or a Bearer token whose actor maps to
/// an admin user (sessions carry role directly; tokens don't, so resolve via
/// the user record).
async fn require_admin_actor(s: &Store, ctx: &AuthCtx) -> Result<bool, crate::error::ApiError> {
    if ctx.is_admin() {
        return Ok(true);
    }
    Ok(s.users_list()
        .await?
        .iter()
        .any(|u| u.actor == ctx.actor() && u.role == UserRole::Admin))
}

// ---- actor lifecycle (admin): delete-with-cascade + merge ----
// Both are destructive and admin-gated. Pass ?dryRun=1 to get the per-table
// counts WITHOUT mutating — the UI shows the blast radius before confirm.

#[derive(Deserialize)]
struct DryRunQuery {
    #[serde(rename = "dryRun")]
    dry_run: Option<String>,
}

impl DryRunQuery {
    fn is_dry(&self) -> bool {
        matches!(self.dry_run.as_deref(), Some("1") | Some("true"))
    }
}

async fn actors_remove(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(slug): Path<String>,
    Query(q): Query<DryRunQuery>,
) -> ApiResult {
    if !require_admin_actor(&s, &ctx).await? {
        return Ok(forbidden());
    }
    if s.people_get(&slug).await?.is_none() {
        return Ok(not_found());
    }
    let result = if q.is_dry() {
        s.actors_remove_preview(&slug).await?
    } else {
        s.actors_remove(&slug).await?
    };
    Ok(Json(result).into_response())
}

#[derive(Deserialize)]
struct MergeBody {
    into: Option<String>,
}

async fn actors_merge(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(slug): Path<String>,
    Query(q): Query<DryRunQuery>,
    body: Option<Json<MergeBody>>,
) -> ApiResult {
    if !require_admin_actor(&s, &ctx).await? {
        return Ok(forbidden());
    }
    let into = body
        .and_then(|Json(b)| b.into)
        .filter(|v| !v.trim().is_empty());
    let Some(into) = into else {
        return Ok(err(
            StatusCode::BAD_REQUEST,
            "into (target actor slug) required",
        ));
    };
    if slug == into {
        return Ok(err(
            StatusCode::BAD_REQUEST,
            "cannot merge an actor into itself",
        ));
    }
    if s.people_get(&slug).await?.is_none() {
        return Ok(err(
            StatusCode::NOT_FOUND,
            &format!("from actor '{slug}' not found"),
        ));
    }
    if s.people_get(&into).await?.is_none() {
        return Ok(err(
            StatusCode::NOT_FOUND,
            &format!("into actor '{into}' not found"),
        ));
    }
    let result = if q.is_dry() {
        s.actors_merge_preview(&slug, &into).await?
    } else {
        s.actors_merge(&slug, &into).await?
    };
    Ok(Json(result).into_response())
}

// ---- bulk historical import (admin) ----
// Backfill from a legacy hive.db. Idempotent (existing ids skipped). Admin-only;
// an admin's Bearer token qualifies (e.g. a one-shot programmatic migration).

async fn import_json(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(payload): Json<LegacyImport>,
) -> ApiResult {
    if !require_admin_actor(&s, &ctx).await? {
        return Ok(forbidden());
    }
    Ok(Json(s.import_legacy(payload).await?).into_response())
}

// Upload a legacy hive.db (SQLite) straight from the dashboard. We persist it to
// a temp file (SQLite needs a path), read it READ-ONLY, map → import, then delete.
async fn import_sqlite(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    mut multipart: Multipart,
) -> ApiResult {
    if !require_admin_actor(&s, &ctx).await? {
        return Ok(forbidden());
    }
    let mut file = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
    {
        if field.name() == Some("db") {
            file = Some(
                field
                    .bytes()
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?,
            );
            break;
        }
    }
    let Some(bytes) = file else {
        return Ok(err(
            StatusCode::BAD_REQUEST,
            "multipart field 'db' (the .db file) required",
        ));
    };

    let dir = std::env::temp_dir().join(format!("hive-import-{}", nanoid::nanoid!(12)));
    let outcome: anyhow::Result<serde_json::Value> = async {
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join("legacy.db");
        std::fs::write(&db_path, &bytes)?;
        let read = crate::legacy_import::read_legacy_db(&db_path).await?;
        let result = s.import_legacy(read.payload).await?;
        let mut v = serde_json::to_value(&result)?;
        v["warnings"] = serde_json::json!(read.warnings);
        Ok(v)
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    match outcome {
        Ok(v) => Ok(Json(v).into_response()),
        Err(e) => Ok(err(StatusCode::BAD_REQUEST, &format!("import failed: {e}"))),
    }
}

// ---- worker config: sources (GUI + MCP configurable) ----

#[derive(Deserialize)]
struct OwnerQuery {
    owner: Option<String>,
}

async fn sources_list(State(s): State<Store>, Query(q): Query<OwnerQuery>) -> ApiResult {
    // ?owner=<actor> returns global + that actor's; omit (or empty) for all.
    let owner = q.owner.filter(|o| !o.is_empty());
    Ok(Json(s.sources_list(owner.as_deref()).await?).into_response())
}

async fn sources_create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult {
    let has = |k: &str| {
        body.get(k)
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
    };
    if !has("name") || !has("url") {
        return Ok(err(StatusCode::BAD_REQUEST, "name and url required"));
    }
    // scope:"me" → owner = acting identity; scope:"global" or absent → body.owner ?? null.
    let scope_me = body.get("scope").and_then(|v| v.as_str()) == Some("me");
    let mut input: NewSource = serde_json::from_value(body)?;
    if scope_me {
        input.owner = Some(ctx.actor().to_string());
    }
    let src = s.sources_create(input, ctx.actor()).await?;
    Ok((StatusCode::CREATED, Json(src)).into_response())
}

async fn sources_update(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(patch): Json<SourcePatch>,
) -> ApiResult {
    match s.sources_update(&id, patch, ctx.actor()).await? {
        Some(src) => Ok(Json(src).into_response()),
        None => Ok(not_found()),
    }
}

async fn sources_remove(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if s.sources_remove(&id, ctx.actor()).await? {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok(not_found())
    }
}

#[derive(Deserialize)]
struct PollBody {
    id: Option<String>,
}

// On-demand poll (the GUI "refresh now"). Admin-gated like the worker/import
// routes. Body optional { id } polls one source; omitted polls all due sources.
// Shares the worker's poll_sources() — feed.item/scrape.item events fan out over SSE.
async fn sources_poll(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    body: Option<Json<PollBody>>,
) -> ApiResult {
    if !require_admin_actor(&s, &ctx).await? {
        return Ok(forbidden());
    }
    let id = body.and_then(|Json(b)| b.id);
    if let Some(id) = &id {
        if s.sources_get(id).await?.is_none() {
            return Ok(not_found());
        }
    }
    Ok(Json(s.poll_sources(id.as_deref()).await?).into_response())
}

// ---- worker status + outbox ----

async fn worker_status(State(s): State<Store>) -> ApiResult {
    Ok(Json(s.worker_status().await?).into_response())
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

async fn outbox_list(State(s): State<Store>, Query(q): Query<LimitQuery>) -> ApiResult {
    Ok(Json(s.outbox_list(q.limit.unwrap_or(50)).await?).into_response())
}

// ---- fixtures ----
// A locally-served sample RSS feed so feed ingestion is real (and demoable)
// without depending on outbound network in the sandbox.

async fn fixture_xml() -> impl IntoResponse {
    let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Bee feed</title><item><guid>bee-rss-1</guid><title>pgvector 0.8 released</title><link>https://example.com/bee-rss-1</link><description>Postgres vector search gets faster ANN indexes.</description></item><item><guid>bee-rss-2</guid><title>Solid 2.0 roadmap</title><link>https://example.com/bee-rss-2</link><description>Fine-grained reactivity, same tiny runtime.</description></item><item><guid>bee-rss-3</guid><title>SQLite ships native JSON5</title><link>https://example.com/bee-rss-3</link><description>Looser JSON parsing lands in the amalgamation.</description></item></channel></rss>"#;
    ([(header::CONTENT_TYPE, "application/rss+xml")], xml)
}

// A locally-served sample HTML page so scrape ingestion is demoable without
// depending on outbound network.
async fn fixture_html() -> impl IntoResponse {
    let html = r#"<!DOCTYPE html><html><head><title>Bee scrape fixture</title></head><body>
<h1>Bee's Roadhouse dev feed</h1>
<h2>Latest picks</h2>
<ul>
  <li><a href="https://example.com/bee-scrape-1">Hono v4 ships — faster routing, smaller core</a></li>
  <li><a href="https://example.com/bee-scrape-2">SolidJS fine-grained signals land in v2</a></li>
  <li><a href="https://example.com/bee-scrape-3">better-sqlite3 adds WAL2 support</a></li>
</ul>
<nav><a href="/">home</a> <a href="/about">about</a></nav>
</body></html>"#;
    ([(header::CONTENT_TYPE, "text/html")], html)
}
