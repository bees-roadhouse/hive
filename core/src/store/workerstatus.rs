// Worker heartbeat + status composition (store.ts setHeartbeat/setLastRun/
// workerStatus). worker_status is RUNTIME state — written directly, never
// through the fold (see index/mod.rs).

use anyhow::Result;
use hive_shared::{WorkerEmbeddingCounts, WorkerLastRun, WorkerSourceCounts, WorkerStatus};
use rusqlite::OptionalExtension;

use super::{now_iso, Store};

impl Store {
    pub async fn worker_set_heartbeat(&self) -> Result<()> {
        self.run(move |core| {
            core.conn().execute(
                "INSERT INTO worker_status (id, heartbeat) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET heartbeat = excluded.heartbeat",
                rusqlite::params![now_iso()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn worker_set_last_run(&self, stats: &WorkerLastRun) -> Result<()> {
        let raw = serde_json::to_string(stats)?;
        self.run(move |core| {
            core.conn().execute(
                "INSERT INTO worker_status (id, last_run) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET last_run = excluded.last_run",
                rusqlite::params![raw],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn worker_status(&self) -> Result<WorkerStatus> {
        let (heartbeat, last_run_raw, count) = self
            .run(|core| {
                let row: Option<(Option<String>, Option<String>)> = core
                    .conn()
                    .query_row(
                        "SELECT heartbeat, last_run FROM worker_status WHERE id = 1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let (heartbeat, last_run_raw) = row.unwrap_or((None, None));
                let count: i64 =
                    core.conn()
                        .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))?;
                Ok((heartbeat, last_run_raw, count))
            })
            .await?;
        let all = self.sources_list(None).await?;
        let outbox = self.outbox_counts().await?;
        let last_run: Option<WorkerLastRun> =
            last_run_raw.and_then(|s| serde_json::from_str(&s).ok());
        // The worker persists its latch per cycle (a separate process — its
        // in-memory latch is invisible here); OR in this process's own latch
        // so a query-time model failure surfaces too.
        let latched = last_run.as_ref().is_some_and(|r| r.latched) || self.embedder().latched();
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
                model: self.embedder().model(),
            },
            latched,
        })
    }
}
