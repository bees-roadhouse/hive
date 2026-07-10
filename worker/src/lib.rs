// Background worker — parity port of packages/worker/src/index.ts. One cycle:
// heartbeat → poll due sources → drain outbox → backfill embeddings → maintain.
// Source polling, outbox drain, and worker-status writes reuse the api crate's
// Store (the Node worker imports store.ts the same way); embeddings backfill
// and db maintenance live here because the api store doesn't expose them.

use anyhow::Result;
use hive_core::store::semantic::EmbeddableItem;
use hive_core::store::Store;
use hive_shared::WorkerLastRun;
use serde_json::json;
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::{Duration, Instant};
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
            // Persisted so the api process (a different process — its own
            // latch state says nothing about ours) can surface it in
            // /api/worker's payload.
            latched: hive_embed::transformers_latched(),
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

    /// Wall-clock budget for one backfill stage: $HIVE_EMBED_STAGE_BUDGET_SECS,
    /// default 20 (of the 30s tick). Read per cycle so ops can tune it live.
    fn embed_stage_budget() -> Duration {
        Duration::from_secs(
            std::env::var("HIVE_EMBED_STAGE_BUDGET_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
        )
    }

    /// store.ts `embeddings.backfill()`, chunked: (re)embed every embeddable
    /// item whose stored rows are missing or have a stale content hash /
    /// model; returns how many items were (re)computed. Time-boxed: once the
    /// stage budget is spent, remaining stale items defer to the next cycle
    /// (large drains self-pace instead of starving the rest of the loop).
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
        let model = hive_embed::embed_model();
        // Batched skip-map: ONE select per cycle instead of a per-item probe.
        // chunk_idx = 0 stands for the whole item — every chunk row carries
        // the same item-level hash. Model-filtered in SQL, so rows written
        // under another $HIVE_EMBED provider don't count as fresh.
        let stored: HashMap<String, String> = hive_core::pgq::query_as::<(String, String, String)>(
            "SELECT ref_kind, ref_id, hash FROM embeddings WHERE chunk_idx = 0 AND model = ?",
        )
        .bind(model)
        .fetch_all(self.db())
        .await?
        .into_iter()
        .map(|(kind, id, hash)| (format!("{kind}:{id}"), hash))
        .collect();

        let budget = Self::embed_stage_budget();
        let started = Instant::now();
        let mut n: i64 = 0;
        let mut deferred: usize = 0;
        let mut warned_no_vec_v = false;
        // The api store owns embeddableItems (same source Node's worker imports).
        for it in self.store.embeddable_items().await? {
            if stored.get(&format!("{}:{}", it.kind, it.id)) == Some(&it.hash) {
                continue;
            }
            // Time-box: stop STARTING items once the budget is spent; keep
            // scanning (cheap map lookups) only to count what got deferred.
            if started.elapsed() >= budget {
                deferred += 1;
                continue;
            }
            if self.embed_item(&it, model, &mut warned_no_vec_v).await? {
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
        if deferred > 0 {
            info!(
                embedded = n,
                deferred,
                budget_secs = budget.as_secs(),
                "embed stage budget spent; deferred items wait for the next cycle"
            );
        }
        Ok(n)
    }

    /// (Re)embed one item as its full chunk set. Chunks embed OUTSIDE any
    /// transaction — embed() is sync + potentially slow (ONNX), and the latch
    /// is re-checked between chunks so a mid-item model failure drops the item
    /// (no partial/mislabeled rows), same contract as the old single-row path.
    /// The write is atomic: DELETE every chunk row for (kind, id) + INSERT the
    /// new set in one tx, so readers never see a half-replaced item and stale
    /// high-index chunks can't outlive a shrunk text.
    async fn embed_item(
        &self,
        it: &EmbeddableItem,
        model: &str,
        warned_no_vec_v: &mut bool,
    ) -> Result<bool> {
        let chunks = hive_embed::chunk_text(
            &it.embed_text,
            hive_embed::CHUNK_TARGET_TOKENS,
            hive_embed::CHUNK_OVERLAP_TOKENS,
            hive_embed::CHUNK_MAX_CHUNKS,
        );
        // embed_text always has the "[kind] title" prefix, so this only trips
        // on pathological whitespace — embed it whole rather than skip forever.
        let chunks = if chunks.is_empty() {
            vec![it.embed_text.clone()]
        } else {
            chunks
        };
        let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let v = tokio::task::spawn_blocking(move || hive_embed::embed(&chunk)).await?;
            // If the model failed (possibly during this very call), `v` is the
            // hash fallback still stamped with the ONNX model tag — drop the
            // whole item; the paused backfill picks it up after restart.
            if hive_embed::transformers_latched() {
                return Ok(false);
            }
            vecs.push(v);
        }

        // Dual-write vec (BYTEA brute-force path, what semantic_search reads
        // today) AND vec_v (pgvector, the HNSW ANN column) while the ANN path
        // soaks — but vec_v is vector(384), so only 384-dim models qualify.
        // The 256-dim hash provider (dev/CI) is by design BYTEA-only; any
        // other real model dimension means no ANN coverage: say so, once per
        // cycle, loudly.
        let write_vec_v = hive_embed::embed_dim() == 384;
        if !write_vec_v && model != hive_embed::HASH_MODEL && !*warned_no_vec_v {
            *warned_no_vec_v = true;
            warn!(
                model,
                dim = hive_embed::embed_dim(),
                "embedding model is not 384-dim: writing BYTEA only, rows get no ANN index coverage"
            );
        }

        let now = now_iso();
        let mut tx = self.db().begin().await?;
        hive_core::pgq::query("DELETE FROM embeddings WHERE ref_kind = ? AND ref_id = ?")
            .bind(&it.kind)
            .bind(&it.id)
            .execute(&mut *tx)
            .await?;
        for (idx, v) in vecs.iter().enumerate() {
            let blob = hive_embed::to_blob(v);
            if write_vec_v {
                hive_core::pgq::query(
                    "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec, vec_v, hash, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&it.kind)
                .bind(&it.id)
                .bind(idx as i32)
                .bind(model)
                .bind(v.len() as i64)
                .bind(&it.owner)
                .bind(blob)
                .bind(pgvector::Vector::from(v.clone()))
                .bind(&it.hash)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            } else {
                hive_core::pgq::query(
                    "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec, hash, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&it.kind)
                .bind(&it.id)
                .bind(idx as i32)
                .bind(model)
                .bind(v.len() as i64)
                .bind(&it.owner)
                .bind(blob)
                .bind(&it.hash)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Embedding-reaper cadence: every 20th cycle ≈ 10 min at the 30s tick.
    const REAP_EVERY_CYCLES: u64 = 20;

    /// Per-account newest-N mail embed window: $HIVE_MAIL_EMBED_LIMIT, default
    /// 5000 (the DIRECTION.md D8 gate). Read per reap so ops can tune it live;
    /// the SAME value must gate the embed drain, or drain and reaper fight.
    fn mail_embed_limit() -> i64 {
        std::env::var("HIVE_MAIL_EMBED_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5000)
    }

    /// Worker `maintain()`: prune the wire log to the newest 2000 rows, and
    /// every 20th cycle run the embedding reaper (orphans, the journal embed
    /// window, mail eligibility — see api store/maintenance.rs). On Postgres
    /// the SQLite-era housekeeping is handled by the server — WAL
    /// checkpointing is automatic, the GIN full-text index self-maintains (no
    /// FTS5 `optimize`), and autovacuum reclaims space (no manual `VACUUM`).
    async fn maintain(&self, cycle_n: u64) -> Result<Vec<String>> {
        let db = self.db();
        let mut did: Vec<String> = Vec::new();
        let pruned = hive_core::pgq::query(
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
        // The reaper is the safety net behind hive-mail's synchronous deletes
        // and the ONLY aging mechanism for the moving newest-N windows —
        // nothing "events" a message (or journal entry) out of its window.
        if cycle_n % Self::REAP_EVERY_CYCLES == 0 {
            let mut total: u64 = 0;
            for (label, n) in self.store.embeddings_reap(Self::mail_embed_limit()).await? {
                total += n;
                if n > 0 {
                    did.push(format!("reaped-{label}({n})"));
                }
            }
            // Silence marker: shows the reaper RAN and found nothing, so a
            // quiet maintenance vec is distinguishable from a reaper that
            // never fired.
            if total == 0 {
                did.push("reaped-total(0)".to_string());
            }
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
        let ids: Vec<String> = hive_core::pgq::query_scalar::<String>(
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
