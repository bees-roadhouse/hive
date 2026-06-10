// Worker heartbeat + status composition (store.ts setHeartbeat/setLastRun/
// workerStatus). Owned by the admin workstream.

use anyhow::Result;
use hive_shared::{WorkerEmbeddingCounts, WorkerLastRun, WorkerSourceCounts, WorkerStatus};

use super::{now_iso, Store};

impl Store {
    pub async fn worker_set_heartbeat(&self) -> Result<()> {
        sqlx::query(
            "INSERT INTO worker_status (id, heartbeat) VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET heartbeat = excluded.heartbeat",
        )
        .bind(now_iso())
        .execute(self.db())
        .await?;
        Ok(())
    }

    pub async fn worker_set_last_run(&self, stats: &WorkerLastRun) -> Result<()> {
        sqlx::query(
            "INSERT INTO worker_status (id, last_run) VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET last_run = excluded.last_run",
        )
        .bind(serde_json::to_string(stats)?)
        .execute(self.db())
        .await?;
        Ok(())
    }

    pub async fn worker_status(&self) -> Result<WorkerStatus> {
        let row: Option<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT heartbeat, last_run FROM worker_status WHERE id = 1")
                .fetch_optional(self.db())
                .await?;
        let (heartbeat, last_run_raw) = row.unwrap_or((None, None));
        let all = self.sources_list(None).await?;
        let outbox = self.outbox_counts().await?;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM embeddings")
            .fetch_one(self.db())
            .await?;
        Ok(WorkerStatus {
            heartbeat,
            last_run: last_run_raw.and_then(|s| serde_json::from_str(&s).ok()),
            sources: WorkerSourceCounts {
                total: all.len() as i64,
                enabled: all.iter().filter(|s| s.enabled).count() as i64,
            },
            outbox,
            embeddings: WorkerEmbeddingCounts {
                count,
                model: hive_embed::embed_model().to_string(),
            },
        })
    }
}
