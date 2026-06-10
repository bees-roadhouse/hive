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

    let path = std::env::var("HIVE_DB").unwrap_or_else(|_| "data/hive.db".to_string());
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(30));
    let pool = sqlx::SqlitePool::connect_with(opts).await?;

    let worker = Worker::new(pool);
    if std::env::args().any(|a| a == "--once" || a == "once") {
        worker.run_once().await
    } else {
        worker.run().await
    }
}
