use anyhow::Result;
use hive_worker::Worker;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            // hive_api included: the store's poll/outbox warns are the worker's
            // expected-transient log lines (#48).
            std::env::var("RUST_LOG").unwrap_or_else(|_| "hive_worker=info,hive_api=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Same open path as the api (HIVE_DB, default data/hive.db) plus schema
    // migrate — the way the Node worker gets both by importing @hive/api/db.
    let pool = hive_api::db::init().await?;

    let worker = Worker::new(pool);
    if std::env::args().any(|a| a == "--once" || a == "once") {
        worker.run_once().await?;
        tracing::info!("one-shot run done, exiting");
        Ok(())
    } else {
        worker.run().await
    }
}
