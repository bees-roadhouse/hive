// Worker status composition (store.ts workerStatus). Owned by the admin
// workstream. The WRITE half is gone with the Node worker that was the only
// caller: `heartbeat` and `last_run` read as NULL until something schedules
// work again and brings its own writer.

use anyhow::Result;
use hive_shared::{WorkerEmbeddingCounts, WorkerLastRun, WorkerSourceCounts, WorkerStatus};

use super::Store;

impl Store {
    pub async fn worker_status(&self) -> Result<WorkerStatus> {
        let row: Option<(Option<String>, Option<String>)> =
            crate::pgq::query_as("SELECT heartbeat, last_run FROM worker_status WHERE id = 1")
                .fetch_optional(self.db())
                .await?;
        let (heartbeat, last_run_raw) = row.unwrap_or((None, None));
        let all = self.sources_list(None).await?;
        let outbox = self.outbox_counts().await?;
        let count: i64 = crate::pgq::query_scalar("SELECT count(*) FROM embeddings")
            .fetch_one(self.db())
            .await?;
        let last_run: Option<WorkerLastRun> =
            last_run_raw.and_then(|s| serde_json::from_str(&s).ok());
        // The worker persists its latch per cycle (a separate process — its
        // in-memory latch is invisible here); OR in this process's own latch
        // so a query-time model failure surfaces too.
        let latched =
            last_run.as_ref().is_some_and(|r| r.latched) || hive_embed::transformers_latched();
        Ok(WorkerStatus {
            heartbeat,
            last_run,
            sources: WorkerSourceCounts {
                total: all.len() as i64,
                enabled: all.iter().filter(|s| s.enabled).count() as i64,
            },
            outbox,
            embeddings: WorkerEmbeddingCounts {
                count,
                model: hive_embed::embed_model().to_string(),
            },
            latched,
        })
    }
}
