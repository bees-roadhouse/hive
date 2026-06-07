/// Background worker: polls feeds, generates embeddings, runs scheduled tasks.
use hive_shared::*;
use sqlx::SqlitePool;
use std::time::Duration;
use tracing::info;

pub mod embed;

pub struct Worker {
    db: SqlitePool,
}

impl Worker {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        info!("Hive worker starting");

        let mut interval = tokio::time::interval(Duration::from_secs(60));

        loop {
            interval.tick().await;

            if let Err(e) = self.poll_sources().await {
                tracing::error!("poll_sources failed: {}", e);
            }

            if let Err(e) = self.backfill_embeddings().await {
                tracing::error!("backfill_embeddings failed: {}", e);
            }
        }
    }

    async fn poll_sources(&self) -> anyhow::Result<()> {
        let rows = sqlx::query("SELECT * FROM sources WHERE (last_run_at IS NULL OR datetime(last_run_at, '+' || interval_min || ' minutes') < datetime('now')) AND last_error IS NULL")
            .fetch_all(&self.db).await?;

        info!("Polling {} sources", rows.len());
        // TODO: implement RSS/Atom feed polling
        Ok(())
    }

    async fn backfill_embeddings(&self) -> anyhow::Result<()> {
        let pending: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT j.id, j.body FROM journal j
            LEFT JOIN embeddings e ON e.ref_id = j.id AND e.kind = 'journal'
            WHERE e.id IS NULL
            LIMIT 10
            "#
        )
        .fetch_all(&self.db)
        .await?;

        if !pending.is_empty() {
            info!("Backfilling {} embeddings", pending.len());
            // TODO: call embedding engine and store vectors
        }
        Ok(())
    }
}
