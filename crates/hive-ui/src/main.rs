//! `hive-ui` — leptos SSR for the hive shared-state DB.
//!
//! v1 is render-only: axum serves leptos `view!` macros via `render_to_string`.
//! WASM hydration + reactive client state are deferred to v1.5 once the
//! cargo-leptos pipeline is wired up. See DESIGN-UI.md for the staged plan.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::{
    Router,
    extract::{Path, Query, State},
    response::{Html, IntoResponse},
    routing::get,
};
use hive_db::queries::{journal, notes, search, tasks, wire};
use hive_db::{Pool, default_db_path, open_pool};
use serde::Deserialize;

mod views;

#[derive(Clone)]
struct AppState {
    pool: Arc<Pool>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,hive_ui=debug".into()),
        )
        .init();

    let db_path = default_db_path();
    tracing::info!(?db_path, "opening hive db (read-write, no-create)");
    let pool = open_pool(&db_path, false, 4).context("open hive.db pool")?;
    let state = AppState { pool: Arc::new(pool) };

    let app = Router::new()
        .route("/", get(home))
        .route("/journal", get(journal_list))
        .route("/journal/{id}", get(journal_detail))
        .route("/tasks", get(tasks_list))
        .route("/notes", get(notes_list))
        .route("/wire", get(wire_list))
        .route("/search", get(search_handler))
        .route("/healthz", get(healthz))
        .with_state(state);

    let port: u16 = std::env::var("HIVE_UI_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8091);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "hive-ui listening on http://localhost:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn home(State(state): State<AppState>) -> Html<String> {
    let pool = state.pool.clone();
    let entries = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = pool.get()?;
        let filters = journal::ListFilters {
            limit: Some(30),
            ..Default::default()
        };
        Ok(journal::list(&conn, &filters)?)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "blocking task panicked");
        Ok(Vec::new())
    })
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "journal list failed");
        Vec::new()
    });

    Html(views::render_home(entries))
}

#[derive(Debug, Deserialize, Default)]
struct JournalFilterQuery {
    tag: Option<String>,
    ai: Option<String>,
}

async fn journal_list(
    State(state): State<AppState>,
    Query(q): Query<JournalFilterQuery>,
) -> Html<String> {
    let pool = state.pool.clone();
    let tag = q.tag.clone();
    let ai = q.ai.clone();
    let entries = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = pool.get()?;
        let parsed_ai = ai
            .as_deref()
            .and_then(|s| s.parse::<hive_db::enums::Ai>().ok());
        let filters = journal::ListFilters {
            limit: Some(100),
            tag: tag.clone(),
            ai: parsed_ai,
            ..Default::default()
        };
        Ok(journal::list(&conn, &filters)?)
    })
    .await
    .unwrap_or_else(|_| Ok(Vec::new()))
    .unwrap_or_default();
    Html(views::render_journal_page(entries, q.tag, q.ai))
}

#[derive(Debug, Deserialize, Default)]
struct SearchQuery {
    q: Option<String>,
}

async fn search_handler(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Html<String> {
    let q = query.q.unwrap_or_default();
    let q_clone = q.clone();
    if q.trim().is_empty() {
        return Html(views::render_search_page(String::new(), Vec::new(), Vec::new()));
    }
    let pool = state.pool.clone();
    let (journal_hits, note_hits) = tokio::task::spawn_blocking(move || {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(_) => return (Vec::new(), Vec::new()),
        };
        let j = search::journal(&conn, &q_clone, 30).unwrap_or_default();
        let n = search::notes(&conn, &q_clone, 30).unwrap_or_default();
        (j, n)
    })
    .await
    .unwrap_or_default();
    Html(views::render_search_page(q, journal_hits, note_hits))
}

async fn healthz() -> impl IntoResponse {
    "ok"
}

async fn journal_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Html<String> {
    let pool = state.pool.clone();
    let entry = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = pool.get()?;
        Ok(journal::get(&conn, id)?)
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .flatten();
    Html(views::render_journal_detail(id, entry))
}

async fn tasks_list(State(state): State<AppState>) -> Html<String> {
    let pool = state.pool.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = pool.get()?;
        let filters = tasks::ListFilters::default();
        Ok(tasks::list(&conn, &filters)?)
    })
    .await
    .unwrap_or_else(|_| Ok(Vec::new()))
    .unwrap_or_default();
    Html(views::render_tasks_page(rows))
}

async fn notes_list(State(state): State<AppState>) -> Html<String> {
    let pool = state.pool.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = pool.get()?;
        let filters = notes::ListFilters::default();
        Ok(notes::list(&conn, &filters)?)
    })
    .await
    .unwrap_or_else(|_| Ok(Vec::new()))
    .unwrap_or_default();
    Html(views::render_notes_page(rows))
}

async fn wire_list(State(state): State<AppState>) -> Html<String> {
    let pool = state.pool.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = pool.get()?;
        let filters = wire::ListFilters {
            limit: Some(50),
            ..Default::default()
        };
        Ok(wire::list(&conn, &filters)?)
    })
    .await
    .unwrap_or_else(|_| Ok(Vec::new()))
    .unwrap_or_default();
    Html(views::render_wire_page(rows))
}
