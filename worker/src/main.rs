use anyhow::Result;
use hive_worker::Worker;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "hive_worker=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:hive.db".to_string());
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&database_url.replace("sqlite:", ""))
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(opts).await?;

    let worker = Worker::new(pool);
    worker.run().await
}
