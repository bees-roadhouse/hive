// Chunked embedding backfill + the per-call embed budget — ported from the
// retired worker crate's cycle (PR 1.2 teardown). Callers (today the tests;
// soon the desktop shell's background task) invoke `backfill_embeddings()`
// repeatedly; each call is time-boxed so a large drain self-paces instead of
// starving whatever loop drives it.

use anyhow::Result;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use super::semantic::EmbeddableItem;
use super::{now_iso, Store};

/// Wall-clock budget for one backfill call: $HIVE_EMBED_STAGE_BUDGET_SECS,
/// default 20. Read per call so ops can tune it live.
fn embed_stage_budget() -> Duration {
    Duration::from_secs(
        std::env::var("HIVE_EMBED_STAGE_BUDGET_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20),
    )
}

impl Store {
    /// store.ts `embeddings.backfill()`, chunked: (re)embed every embeddable
    /// item whose stored rows are missing or have a stale content hash /
    /// model; returns how many items were (re)computed. Time-boxed: once the
    /// stage budget is spent, remaining stale items defer to the next call
    /// (large drains self-pace instead of starving the caller's loop).
    pub async fn backfill_embeddings(&self) -> Result<i64> {
        // Once the ONNX model latches to the hash fallback, every embed()
        // would return a 256-dim hash vector still stamped with the ONNX model
        // tag — pausing beats poisoning the corpus with vectors that never get
        // re-embedded. The latch clears on restart; retry next call.
        if hive_embed::transformers_latched() {
            warn!(
                "embedding backfill paused: transformers model unavailable (hash fallback latched)"
            );
            return Ok(0);
        }
        let model = hive_embed::embed_model();
        // Batched skip-map: ONE select per call instead of a per-item probe.
        // chunk_idx = 0 stands for the whole item — every chunk row carries
        // the same item-level hash. Model-filtered in SQL, so rows written
        // under another $HIVE_EMBED provider don't count as fresh.
        let stored: HashMap<String, String> = crate::pgq::query_as::<(String, String, String)>(
            "SELECT ref_kind, ref_id, hash FROM embeddings WHERE chunk_idx = 0 AND model = ?",
        )
        .bind(model)
        .fetch_all(self.db())
        .await?
        .into_iter()
        .map(|(kind, id, hash)| (format!("{kind}:{id}"), hash))
        .collect();

        let budget = embed_stage_budget();
        let started = Instant::now();
        let mut n: i64 = 0;
        let mut deferred: usize = 0;
        let mut warned_no_vec_v = false;
        for it in self.embeddable_items().await? {
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
            // The latch can trip inside any embed() call — stop the pass
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
        // pass, loudly.
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
        crate::pgq::query("DELETE FROM embeddings WHERE ref_kind = ? AND ref_id = ?")
            .bind(&it.kind)
            .bind(&it.id)
            .execute(&mut *tx)
            .await?;
        for (idx, v) in vecs.iter().enumerate() {
            let blob = hive_embed::to_blob(v);
            if write_vec_v {
                crate::pgq::query(
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
                crate::pgq::query(
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
}
