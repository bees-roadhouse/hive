// Background worker — parity port of packages/worker/src/index.ts. One cycle:
// heartbeat → poll due sources → drain outbox → backfill embeddings → maintain.
// Source polling, outbox drain, and worker-status writes reuse the api crate's
// Store (the Node worker imports store.ts the same way); embeddings backfill
// and db maintenance live here because the api store doesn't expose them.

use anyhow::Result;
use hive_api::store::Store;
use hive_shared::WorkerLastRun;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use tracing::{info, warn};

pub struct Worker {
    store: Store,
}

fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

impl Worker {
    pub fn new(db: PgPool) -> Self {
        Self {
            store: Store::new(db),
        }
    }

    fn db(&self) -> &PgPool {
        self.store.db()
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
        let tick = Self::tick_secs();
        info!(mode = "loop", tick_secs = tick, "starting");
        self.store
            .emit(
                "worker.started",
                "worker",
                json!({ "once": false, "tick": tick }),
            )
            .await?;
        let mut interval = tokio::time::interval(Duration::from_secs(tick));
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
        info!(mode = "once", "starting");
        self.store
            .emit(
                "worker.started",
                "worker",
                json!({ "once": true, "tick": Self::tick_secs() }),
            )
            .await?;
        self.cycle(1).await
    }

    pub async fn cycle(&self, cycle_n: u64) -> Result<()> {
        self.store.worker_set_heartbeat().await?;
        let poll = self.store.poll_sources(None).await?;
        let outbox = self.store.drain_outbox().await?;
        let embedded = self.backfill_embeddings().await?;
        let maintenance = self.maintain(cycle_n).await?;

        let stats = WorkerLastRun {
            at: now_iso(),
            polled: poll.polled,
            ingested: poll.ingested,
            outbox,
            embedded,
            maintenance,
        };
        self.store.worker_set_last_run(&stats).await?;
        let joined = stats.maintenance.join(",");
        info!(
            polled = stats.polled,
            ingested = stats.ingested,
            outbox = stats.outbox,
            embedded = stats.embedded,
            maintenance = if joined.is_empty() { "none" } else { &joined },
            "cycle complete"
        );
        Ok(())
    }

    /// store.ts `embeddings.backfill()`: (re)embed every embeddable item whose
    /// stored row is missing or has a stale content hash / model; returns how
    /// many were (re)computed.
    async fn backfill_embeddings(&self) -> Result<i64> {
        // Once the ONNX model latches to the hash fallback, every embed()
        // would return a 256-dim hash vector still stamped with the ONNX model
        // tag — pausing beats poisoning the corpus with vectors that never get
        // re-embedded. The latch clears on restart; retry next cycle.
        if hive_embed::transformers_latched() {
            warn!(
                "embedding backfill paused: transformers model unavailable (hash fallback latched)"
            );
            return Ok(0);
        }
        let mut n = 0;
        // The api store owns embeddableItems (same source Node's worker imports).
        for it in self.store.embeddable_items().await? {
            if self
                .embed_upsert(&it.kind, &it.id, it.embed_text, it.hash)
                .await?
            {
                n += 1;
            }
            // The latch can trip inside any embed() call — stop the cycle
            // rather than hammer the fallback for the rest of the corpus.
            if hive_embed::transformers_latched() {
                warn!(
                    embedded = n,
                    "embedding backfill paused mid-cycle: transformers model unavailable"
                );
                break;
            }
        }
        Ok(n)
    }

    /// store.ts `embeddings.upsert`: skip when the stored hash AND model both
    /// match — flipping $HIVE_EMBED re-embeds even unchanged rows. Vector is a
    /// packed little-endian f32 BLOB (hive_embed::to_blob), PK (ref_kind, ref_id).
    async fn embed_upsert(
        &self,
        ref_kind: &str,
        ref_id: &str,
        embed_text: String,
        hash: String,
    ) -> Result<bool> {
        let model = hive_embed::embed_model();
        let existing: Option<(String, String)> = hive_api::pgq::query_as(
            "SELECT hash, model FROM embeddings WHERE ref_kind = ? AND ref_id = ?",
        )
        .bind(ref_kind)
        .bind(ref_id)
        .fetch_optional(self.db())
        .await?;
        if matches!(&existing, Some((h, m)) if *h == hash && m == model) {
            return Ok(false);
        }
        // embed() is sync + potentially slow (ONNX) — keep it off the async runtime.
        let vec = tokio::task::spawn_blocking(move || hive_embed::embed(&embed_text)).await?;
        // If the model failed (possibly during this very call), `vec` is the
        // hash fallback but `model` still names the ONNX repo: a row written
        // now would be wrong-dim, mislabeled, and skipped by every future
        // backfill. Drop it; the paused backfill picks the item up on restart.
        if hive_embed::transformers_latched() {
            return Ok(false);
        }
        hive_api::pgq::query(
            "INSERT INTO embeddings (ref_kind, ref_id, model, dim, vec, hash, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(ref_kind, ref_id) DO UPDATE SET model=excluded.model, dim=excluded.dim, vec=excluded.vec, hash=excluded.hash, created_at=excluded.created_at",
        )
        .bind(ref_kind)
        .bind(ref_id)
        .bind(model)
        .bind(vec.len() as i64)
        .bind(hive_embed::to_blob(&vec))
        .bind(&hash)
        .bind(now_iso())
        .execute(self.db())
        .await?;
        Ok(true)
    }

    /// Worker `maintain()`: prune the wire log to the newest 2000 rows. On
    /// Postgres the SQLite-era housekeeping is handled by the server — WAL
    /// checkpointing is automatic, the GIN full-text index self-maintains (no
    /// FTS5 `optimize`), and autovacuum reclaims space (no manual `VACUUM`).
    async fn maintain(&self, _cycle_n: u64) -> Result<Vec<String>> {
        let db = self.db();
        let mut did: Vec<String> = Vec::new();
        let pruned = hive_api::pgq::query(
            "DELETE FROM wire WHERE id NOT IN (SELECT id FROM wire ORDER BY created_at DESC LIMIT 2000)",
        )
        .execute(db)
        .await?
        .rows_affected();
        if pruned > 0 {
            did.push(format!("pruned-wire({pruned})"));
        }
        let swept = self.sweep_conversations().await?;
        if swept > 0 {
            did.push(format!("swept-conversations({swept})"));
        }
        Ok(did)
    }

    /// Conversation retention: when $HIVE_CONVERSATION_RETENTION_DAYS is set,
    /// hard-delete archived hosted sessions whose updated_at is older than the
    /// cutoff (transcript + conversation graph links go too — journal mirrors
    /// are history and stay). Unset = keep forever, the default.
    /// TODO: also sweep origin='captured' AND reflected_at IS NOT NULL once the
    /// conversation-capture columns land.
    async fn sweep_conversations(&self) -> Result<u64> {
        let Some(days) = std::env::var("HIVE_CONVERSATION_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|d| *d >= 0)
            .and_then(chrono::Duration::try_days)
        else {
            return Ok(0);
        };
        // updated_at is ISO-8601 UTC text (now_iso), so string compare == time compare.
        let cutoff = (chrono::Utc::now() - days)
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let ids: Vec<String> = hive_api::pgq::query_scalar::<String>(
            "SELECT id FROM cc_sessions WHERE status = 'archived' AND updated_at < ?",
        )
        .bind(&cutoff)
        .fetch_all(self.db())
        .await?;
        let mut swept = 0u64;
        for id in &ids {
            if self.store.workspace_delete(id).await? {
                swept += 1;
            }
        }
        Ok(swept)
    }
}
