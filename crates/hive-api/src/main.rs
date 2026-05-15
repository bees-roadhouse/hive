//! `hive-api` ... axum HTTP API exposing the same operations as the CLI.
//!
//! Routes mirror the CLI grammar 1:1. Auth-free; bind to 127.0.0.1 by
//! default ... external exposure is a reverse-proxy concern.

mod error;
mod routes;
mod state;

use std::net::SocketAddr;

use axum::Router;
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::state::AppState;

#[derive(Debug, Parser)]
#[command(name = "hive-api", about = "Hive HTTP API")]
struct Args {
    /// Bind address (default: 127.0.0.1:7878)
    #[arg(long, env = "HIVE_API_BIND", default_value = "127.0.0.1:7878")]
    bind: SocketAddr,
    /// DB path override (default: $HIVE_DB or ~/.hive/hive.db)
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
    let db_path = args
        .db
        .clone()
        .unwrap_or_else(hive_db::default_db_path);
    tracing::info!(db = %db_path.display(), bind = %args.bind, "starting hive-api");

    let pool = hive_db::open_pool(&db_path, false, 4)?;
    let state = AppState { pool };

    let app: Router = Router::new()
        .merge(routes::projects::router())
        .merge(routes::tasks::router())
        .merge(routes::journal::router())
        .merge(routes::notes::router())
        .merge(routes::wire::router())
        .merge(routes::links::router())
        .merge(routes::graph::router())
        .merge(routes::search::router())
        .merge(routes::health::router())
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::cors::CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Helper so each handler can borrow a connection from the pool inside
/// `tokio::task::spawn_blocking` without worrying about send-bounds on
/// `rusqlite::Connection`.
pub(crate) async fn with_conn<F, T>(state: &AppState, f: F) -> Result<T, error::ApiError>
where
    F: FnOnce(&hive_db::Connection) -> hive_db::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let pool = state.pool.clone();
    tokio::task::spawn_blocking(move || -> hive_db::Result<T> {
        let conn = pool.get().map_err(hive_db::Error::from)?;
        f(&conn)
    })
    .await
    .map_err(|e| error::ApiError::Internal(format!("join error: {e}")))?
    .map_err(error::ApiError::from)
}
