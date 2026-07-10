// Chunked embedding backfill (store::embed_backfill) — the worker crate's
// chunked_backfill.rs + embed_budget.rs tests, adapted to drive
// `Store::backfill_embeddings()` directly (PR 1.2 moved the code here and
// deleted the worker).
//
// Both tests run under a 384-dim "transformers" provider (a mock ONNX engine —
// offline + deterministic like the hash path, but dimension-eligible for the
// native vector column): the provider choice ($HIVE_EMBED) and the engine are
// latched once per process, so one binary means ONE provider for every test in
// it. $HIVE_EMBED_STAGE_BUDGET_SECS is process-global too, so the tests
// serialize on ENV_LOCK — the budget test's zero-budget window must never
// overlap another test's backfill call.

mod common;

use hive_core::store::Store;
use tokio::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct Mock384;
impl hive_embed::OnnxProvider for Mock384 {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        // Deterministic + unit-length: the 256-dim hash embedding zero-padded
        // to the BGE small dimension.
        let mut v = hive_embed::embed_hash(text);
        v.resize(384, 0.0);
        Ok(v)
    }
    fn rerank(&self, _query: &str, _docs: &[String]) -> anyhow::Result<Vec<f64>> {
        anyhow::bail!("no reranker in the mock")
    }
    fn supports_rerank(&self) -> bool {
        false
    }
}

async fn test_setup() -> (sqlx::PgPool, Store) {
    // Must beat the first embed call: the provider choice latches once per
    // process, and installing the mock first keeps the lazy default ort
    // engine (real model download) from ever wiring itself in.
    std::env::set_var("HIVE_EMBED", "transformers");
    hive_embed::set_onnx_provider(Box::new(Mock384));
    let store = common::test_store().await;
    (store.db().clone(), store)
}

#[tokio::test]
async fn chunked_dual_write_skip_and_atomic_replace() {
    let _env = ENV_LOCK.lock().await;
    let (pool, store) = test_setup().await;
    assert_eq!(hive_embed::embed_dim(), 384, "mock must be vec_v-eligible");

    // A body several times the 450-token (1800-char) chunk target → the item
    // must land as multiple chunk rows.
    let long_body = (0..40)
        .map(|i| format!("Paragraph {i:02}: {}", "chunked backfill notes ".repeat(8)))
        .collect::<Vec<_>>()
        .join("\n\n");
    sqlx::query(
        "INSERT INTO journal (id, author, body, user_scope, created_at) \
         VALUES ('jrnl_long', 'nate', $1, 'nate', '2026-01-01T00:00:00.000Z')",
    )
    .bind(&long_body)
    .execute(&pool)
    .await
    .unwrap();

    let embedded = store.backfill_embeddings().await.expect("backfill 1");
    assert_eq!(embedded, 1, "one ITEM embedded (chunks don't inflate it)");
    assert!(
        !hive_embed::transformers_latched(),
        "mock engine must not latch"
    );

    let rows: Vec<(i32, Option<String>, String, bool, bool, i64)> = sqlx::query_as(
        "SELECT chunk_idx, owner, hash, vec IS NOT NULL, vec_v IS NOT NULL, dim \
         FROM embeddings WHERE ref_kind = 'journal' AND ref_id = 'jrnl_long' ORDER BY chunk_idx",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(rows.len() > 1, "long text must chunk: {} rows", rows.len());
    let item_hash = &rows[0].2;
    for (i, (chunk_idx, owner, hash, has_vec, has_vec_v, dim)) in rows.iter().enumerate() {
        assert_eq!(*chunk_idx, i as i32, "contiguous chunk indexes");
        assert_eq!(owner.as_deref(), Some("nate"), "owner stamped on every row");
        assert_eq!(hash, item_hash, "item-level hash identical on every chunk");
        assert!(has_vec, "dual-write keeps BYTEA vec populated");
        assert!(has_vec_v, "384-dim model writes the native vector too");
        assert_eq!(*dim, 384);
    }
    // The native column really holds 384-dim vectors (not just non-NULL).
    let (v,): (pgvector::Vector,) = sqlx::query_as(
        "SELECT vec_v FROM embeddings WHERE ref_kind = 'journal' AND ref_id = 'jrnl_long' AND chunk_idx = 0",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(v.as_slice().len(), 384);
    let n_chunks = rows.len() as i64;

    // Replay with an unchanged hash: the batched skip-map must skip it.
    let embedded = store.backfill_embeddings().await.expect("backfill 2");
    assert_eq!(embedded, 0, "unchanged item skips");
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM embeddings WHERE ref_id = 'jrnl_long'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, n_chunks, "skip leaves the chunk set untouched");

    // Shrink the body: the whole chunk set must be replaced atomically —
    // fewer rows, no stale high-index chunks, new hash everywhere.
    sqlx::query("UPDATE journal SET body = 'short now' WHERE id = 'jrnl_long'")
        .execute(&pool)
        .await
        .unwrap();
    let embedded = store.backfill_embeddings().await.expect("backfill 3");
    assert_eq!(embedded, 1, "changed hash re-embeds");
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT chunk_idx, hash FROM embeddings WHERE ref_id = 'jrnl_long' ORDER BY chunk_idx",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "shrunk text leaves exactly one chunk row");
    assert_eq!(rows[0].0, 0);
    assert_ne!(&rows[0].1, item_hash, "hash rolled with the text");
}

#[tokio::test]
async fn zero_budget_defers_items_to_the_next_call() {
    let _env = ENV_LOCK.lock().await;
    let (pool, store) = test_setup().await;
    std::env::set_var("HIVE_EMBED_STAGE_BUDGET_SECS", "0");

    sqlx::query(
        "INSERT INTO journal (id, author, body, created_at) \
         VALUES ('jrnl_budget', 'pia', 'body to embed later', '2026-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Zero budget: the stale item is seen but never started.
    let embedded = store
        .backfill_embeddings()
        .await
        .expect("backfill under zero budget");
    assert_eq!(embedded, 0, "zero budget must defer, not embed");
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM embeddings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no rows written under a spent budget");

    // Budget restored (read fresh each call): the deferred item drains.
    std::env::set_var("HIVE_EMBED_STAGE_BUDGET_SECS", "20");
    let embedded = store.backfill_embeddings().await.expect("backfill drains");
    assert_eq!(embedded, 1, "deferred item embeds next call");
    std::env::remove_var("HIVE_EMBED_STAGE_BUDGET_SECS");
}
