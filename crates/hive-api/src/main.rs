//! `hive-api` ... axum HTTP API exposing the same operations as the CLI.
//!
//! Routes mirror the CLI grammar 1:1. Auth-free; bind to 127.0.0.1 by
//! default ... external exposure is a reverse-proxy concern.

mod error;
mod routes;
mod state;

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::state::{AppState, EventEmitter};

#[derive(Debug, Parser)]
#[command(name = "hive-api", about = "Hive HTTP API")]
struct Args {
    /// Bind address (default: 127.0.0.1:7878)
    #[arg(long, env = "HIVE_API_BIND", default_value = "127.0.0.1:7878")]
    bind: SocketAddr,
    /// Postgres connection string (default: postgres://hive:hive@localhost:5432/hive)
    #[arg(
        long,
        env = "DATABASE_URL",
        default_value = "postgres://hive:hive@localhost:5432/hive"
    )]
    database_url: String,
    /// Directory for events.log (default: $HIVE_DIR or ~/.hive)
    #[arg(long, env = "HIVE_DIR")]
    hive_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer())
        .init();

    let args = Args::parse();
    tracing::info!(
        database_url = %scrub_password(&args.database_url),
        bind = %args.bind,
        "starting hive-api"
    );

    let pool = hive_db::open_pool(&args.database_url, 4).await?;

    let hive_dir = args
        .hive_dir
        .clone()
        .or_else(|| {
            directories::UserDirs::new()
                .map(|u| u.home_dir().join(".hive"))
        })
        .unwrap_or_else(|| PathBuf::from("/data"));
    let events_log = hive_dir.join("events.log");
    if let Some(parent) = events_log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let emitter = EventEmitter::new(events_log);
    let state = AppState { pool, emitter };

    let app: Router = Router::new()
        .merge(routes::projects::router())
        .merge(routes::tasks::router())
        .merge(routes::journal::router())
        .merge(routes::messages::router())
        .merge(routes::notes::router())
        .merge(routes::wire::router())
        .merge(routes::links::router())
        .merge(routes::graph::router())
        .merge(routes::search::router())
        .merge(routes::semantic::router())
        .merge(routes::events::router())
        .merge(routes::health::router())
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(tower_http::cors::CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Strip the password segment of a postgres URL so it doesn't leak into logs.
fn scrub_password(url: &str) -> String {
    // postgres://user:password@host:port/db -> postgres://user:***@host:port/db
    if let Some(scheme_idx) = url.find("://") {
        let (scheme, rest) = url.split_at(scheme_idx + 3);
        if let Some(at_idx) = rest.find('@') {
            let creds = &rest[..at_idx];
            let tail = &rest[at_idx..];
            if let Some(colon_idx) = creds.find(':') {
                let user = &creds[..colon_idx];
                return format!("{scheme}{user}:***{tail}");
            }
        }
    }
    url.to_string()
}
