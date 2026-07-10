// Chunked embedding backfill + the per-call embed budget. Embedding is
// derived state ABOVE the fold: vectors are not records — the pipeline writes
// SqliteIndex::{remove_embeddings, upsert_embedding} directly on the writer
// thread. Chunk embedding itself (slow, possibly ONNX) runs OFF the thread
// via spawn_blocking; the per-item write closure is atomic from every
// reader's perspective because nothing interleaves with the writer thread. A
// crash mid-item leaves a missing/stale hash, which the next backfill heals.

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
        // Once a real model latches to the hash fallback, every embed()
        // would return a 256-dim hash vector still stamped with the ONNX model
        // tag — pausing beats poisoning the corpus with vectors that never get
        // re-embedded. The latch clears on restart; retry next call.
        if self.embedder().latched() {
            warn!(
                "embedding backfill paused: transformers model unavailable (hash fallback latched)"
            );
            return Ok(0);
        }
        let model = self.embedder().model();
        // Batched skip-map: ONE select per call instead of a per-item probe.
        // chunk_idx = 0 stands for the whole item — every chunk row carries
        // the same item-level hash. Model-filtered in SQL, so rows written
        // under another provider don't count as fresh.
        let model_q = model.clone();
        let stored: HashMap<String, String> = self
            .run(move |core| {
                let mut stmt = core.conn().prepare(
                    "SELECT ref_kind, ref_id, hash FROM embeddings WHERE chunk_idx = 0 AND model = ?1",
                )?;
                let rows = stmt.query_map(rusqlite::params![model_q], |r| {
                    Ok((
                        format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                        r.get::<_, String>(2)?,
                    ))
                })?;
                Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
            })
            .await?;

        let budget = embed_stage_budget();
        let started = Instant::now();
        let mut n: i64 = 0;
        let mut deferred: usize = 0;
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
            if self.embed_item(&it, &model).await? {
                n += 1;
            }
            // The latch can trip inside any embed() call — stop the pass
            // rather than hammer the fallback for the rest of the corpus.
            if self.embedder().latched() {
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

    /// (Re)embed one item as its full chunk set. Chunks embed OUTSIDE the
    /// writer thread — embed() is sync + potentially slow (ONNX), and the
    /// latch is re-checked between chunks so a mid-item model failure drops
    /// the item (no partial/mislabeled rows), same contract as the old
    /// single-row path. The write replaces the whole chunk set in one writer-
    /// thread closure, so readers never see a half-replaced item.
    async fn embed_item(&self, it: &EmbeddableItem, model: &str) -> Result<bool> {
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
            let embedder = self.embedder().clone();
            let v = tokio::task::spawn_blocking(move || embedder.embed(&chunk)).await?;
            // If the model failed (possibly during this very call), `v` is the
            // hash fallback still stamped with the real model tag — drop the
            // whole item; the paused backfill picks it up after restart.
            if self.embedder().latched() {
                return Ok(false);
            }
            vecs.push(v);
        }

        let now = now_iso();
        let (kind, id, owner, hash) = (
            it.kind.clone(),
            it.id.clone(),
            it.owner.clone(),
            it.hash.clone(),
        );
        let model = model.to_string();
        self.run(move |core| {
            core.index.remove_embeddings(&kind, &id)?;
            for (idx, v) in vecs.iter().enumerate() {
                core.index.upsert_embedding(
                    &kind,
                    &id,
                    idx as i64,
                    &model,
                    owner.as_deref(),
                    v,
                    &hash,
                    &now,
                )?;
            }
            Ok(())
        })
        .await?;
        Ok(true)
    }
}
