mod auth;
mod db;
mod mcp;
mod routes;
mod store;

use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "hive_api=info,tower_http=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:hive.db".to_string());
    let pool = db::init_db(&database_url).await?;

    let store = store::Store::new(pool);
    let state = Arc::new(routes::AppStateInner::new(store));

    let app = routes::router(state);

    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(7878);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;

    tracing::info!("Hive API listening on http://0.0.0.0:{}", port);
    axum::serve(listener, app).await?;

    Ok(())
}
