//! `hive-ui` ... server-rendered HTML UI for the hive shared-state DB.
//!
//! Renders directly off the local `hive-db` (no API hop ... the SSR server
//! already has DB access). The graph view fetches its payload from
//! `/graph` rendered inline so the page is interactive without WASM
//! hydration.
//!
//! v1 is HTML SSR only. The DESIGN.md leptos+WASM hydration story is
//! deferred to a follow-up; the user-visible result of v1 is the same
//! lists + nav + graph view the python+svelte UI ships today.

mod pages;

use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::pages::AppState;

#[derive(Debug, Parser)]
#[command(name = "hive-ui", about = "Hive web UI")]
struct Args {
    #[arg(long, env = "HIVE_UI_BIND", default_value = "127.0.0.1:8080")]
    bind: SocketAddr,
    #[arg(long, env = "HIVE_DB")]
    db: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer())
        .init();

    let args = Args::parse();
    let db_path = args.db.clone().unwrap_or_else(hive_db::default_db_path);
    tracing::info!(db = %db_path.display(), bind = %args.bind, "starting hive-ui");

    let pool = hive_db::open_pool(&db_path, false, 4)?;
    let state = AppState { pool };

    let app: Router = Router::new()
        .route("/", get(pages::home::view))
        .route("/tasks", get(pages::tasks::view))
        .route("/journal", get(pages::journal::view))
        .route("/notes", get(pages::notes::view))
        .route("/wire", get(pages::wire::view))
        .route("/graph", get(pages::graph::view))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
