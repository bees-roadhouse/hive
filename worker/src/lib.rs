// Background worker — parity port of packages/worker/src/index.ts. One cycle:
// heartbeat → poll due sources → drain outbox → backfill embeddings → maintain.
// The per-step implementations are owned by the worker-parity workstream; this
// skeleton keeps the tick loop, heartbeat, and last-run stats in the exact
// shape the Node worker writes (the GUI reads worker_status verbatim).

use anyhow::Result;
use serde_json::json;
use sqlx::SqlitePool;
use std::time::Duration;
use tracing::{info, warn};

pub struct Worker {
    db: SqlitePool,
}

#[derive(Debug, Default)]
pub struct CycleStats {
    pub polled: i64,
    pub ingested: i64,
    pub outbox: i64,
    pub embedded: i64,
    pub maintenance: Vec<String>,
}

fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

impl Worker {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    /// Tick seconds: $HIVE_WORKER_TICK, default 30 (Node parity).
    pub fn tick_secs() -> u64 {
        std::env::var("HIVE_WORKER_TICK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30)
    }

    /// Run forever on the tick interval.
    pub async fn run(&self) -> Result<()> {
        info!(tick_secs = Self::tick_secs(), "hive worker starting");
        let mut interval = tokio::time::interval(Duration::from_secs(Self::tick_secs()));
        let mut cycle_n: u64 = 0;
        loop {
            interval.tick().await;
            cycle_n += 1;
            if let Err(e) = self.cycle(cycle_n).await {
                warn!(error = %e, "worker cycle failed");
            }
        }
    }

    /// One cycle then return (the `--once` path; CI uses it).
    pub async fn run_once(&self) -> Result<()> {
        self.cycle(1).await
    }

    pub async fn cycle(&self, cycle_n: u64) -> Result<()> {
        self.set_heartbeat().await?;
        let mut stats = CycleStats::default();

        let (polled, ingested) = self.poll_sources().await?;
        stats.polled = polled;
        stats.ingested = ingested;
        stats.outbox = self.drain_outbox().await?;
        stats.embedded = self.backfill_embeddings().await?;
        stats.maintenance = self.maintain(cycle_n).await?;

        self.set_last_run(&stats).await?;
        info!(
            polled = stats.polled,
            ingested = stats.ingested,
            outbox = stats.outbox,
            embedded = stats.embedded,
            maintenance = stats.maintenance.join(","),
            "cycle"
        );
        Ok(())
    }

    async fn set_heartbeat(&self) -> Result<()> {
        sqlx::query(
            "INSERT INTO worker_status (id, heartbeat) VALUES (1, ?) \
             ON CONFLICT(id) DO UPDATE SET heartbeat = excluded.heartbeat",
        )
        .bind(now_iso())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn set_last_run(&self, stats: &CycleStats) -> Result<()> {
        let last_run = json!({
            "at": now_iso(),
            "polled": stats.polled,
            "ingested": stats.ingested,
            "outbox": stats.outbox,
            "embedded": stats.embedded,
            "maintenance": stats.maintenance,
        });
        sqlx::query(
            "INSERT INTO worker_status (id, last_run) VALUES (1, ?) \
             ON CONFLICT(id) DO UPDATE SET last_run = excluded.last_run",
        )
        .bind(last_run.to_string())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Poll due sources (10s timeout each, RSS guid dedup vs wire, scrape link
    /// extraction). Returns (polled, ingested). Worker-parity workstream.
    async fn poll_sources(&self) -> Result<(i64, i64)> {
        Ok((0, 0))
    }

    /// Claim up to 20 pending outbox jobs; webhooks POST with exponential
    /// backoff (2^attempts × 30s, cap 3600s), fail after 5 attempts.
    async fn drain_outbox(&self) -> Result<i64> {
        Ok(0)
    }

    /// Re-embed items whose content hash or model changed. Worker-parity workstream.
    async fn backfill_embeddings(&self) -> Result<i64> {
        Ok(0)
    }

    /// WAL checkpoint (TRUNCATE), FTS optimize, wire prune (keep 2000), VACUUM
    /// every 20 cycles. Worker-parity workstream.
    async fn maintain(&self, _cycle_n: u64) -> Result<Vec<String>> {
        Ok(vec![])
    }
}
