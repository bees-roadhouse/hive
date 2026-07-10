// Chunked embedding backfill (store::embed_backfill) — the worker crate's
// chunked_backfill.rs + embed_budget.rs tests, adapted at the PR 1.6 cutover:
// the provider is an INJECTED 384-dim mock Embedder (no env latch, no ONNX),
// and the dual-write vec_v assertions died with the Postgres-native vector
// column — chunk rows now
// prove themselves through the embeddings table + the in-memory ANN.
// $HIVE_EMBED_STAGE_BUDGET_SECS is process-global, so the tests serialize on
// ENV_LOCK — the budget test's zero-budget window must never overlap another
// test's backfill call.

mod common;

use std::sync::Arc;

use hive_core::store::Store;
use tokio::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Deterministic + unit-length 384-dim engine: the 256-dim hash embedding
/// zero-padded to the BGE small dimension.
struct Mock384;

impl hive_embed::Embedder for Mock384 {
    fn model(&self) -> String {
        "mock-bge-384".to_string()
    }
    fn dim(&self) -> usize {
        384
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = hive_embed::embed_hash(text);
        v.resize(384, 0.0);
        v
    }
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.embed(text)
    }
    fn rerank_available(&self) -> bool {
        false
    }
    fn rerank(&self, _query: &str, _docs: &[String]) -> Option<Vec<f64>> {
        None
    }
    fn latched(&self) -> bool {
        false
    }
}

async fn test_setup() -> Store {
    common::test_store_with(Arc::new(Mock384))
}

async fn seed_journal(store: &Store, id: &str, body: &str) {
    store
        .raw_sql(
            "INSERT INTO journal (id, author, body, user_scope, created_at) \
             VALUES (?, 'nate', ?, 'nate', '2026-01-01T00:00:00.000Z')",
            vec![id.into(), body.into()],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn chunked_write_skip_and_atomic_replace() {
    let _env = ENV_LOCK.lock().await;
    let store = test_setup().await;

    // A body several times the 450-token (1800-char) chunk target → the item
    // must land as multiple chunk rows.
    let long_body = (0..40)
        .map(|i| format!("Paragraph {i:02}: {}", "chunked backfill notes ".repeat(8)))
        .collect::<Vec<_>>()
        .join("\n\n");
    seed_journal(&store, "jrnl_long", &long_body).await;

    let embedded = store.backfill_embeddings().await.expect("backfill 1");
    assert_eq!(embedded, 1, "one ITEM embedded (chunks don't inflate it)");

    let rows = store
        .raw_sql(
            "SELECT chunk_idx, owner, hash, vec IS NOT NULL, dim, model \
             FROM embeddings WHERE ref_kind = 'journal' AND ref_id = 'jrnl_long' ORDER BY chunk_idx",
            vec![],
        )
        .await
        .unwrap();
    assert!(rows.len() > 1, "long text must chunk: {} rows", rows.len());
    let item_hash = rows[0][2].as_str().unwrap().to_string();
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row[0].as_i64(), Some(i as i64), "contiguous chunk indexes");
        assert_eq!(row[1].as_str(), Some("nate"), "owner stamped on every row");
        assert_eq!(
            row[2].as_str(),
            Some(item_hash.as_str()),
            "item-level hash identical on every chunk"
        );
        assert_eq!(row[3].as_i64(), Some(1), "packed vec populated");
        assert_eq!(row[4].as_i64(), Some(384));
        assert_eq!(row[5].as_str(), Some("mock-bge-384"));
    }
    let n_chunks = rows.len() as i64;

    // Replay with an unchanged hash: the batched skip-map must skip it.
    let embedded = store.backfill_embeddings().await.expect("backfill 2");
    assert_eq!(embedded, 0, "unchanged item skips");
    let count = store
        .raw_sql(
            "SELECT count(*) FROM embeddings WHERE ref_id = 'jrnl_long'",
            vec![],
        )
        .await
        .unwrap()[0][0]
        .as_i64()
        .unwrap();
    assert_eq!(count, n_chunks, "skip leaves the chunk set untouched");

    // Shrink the body: the whole chunk set must be replaced atomically —
    // fewer rows, no stale high-index chunks, new hash everywhere.
    store
        .raw_sql(
            "UPDATE journal SET body = 'short now' WHERE id = 'jrnl_long'",
            vec![],
        )
        .await
        .unwrap();
    let embedded = store.backfill_embeddings().await.expect("backfill 3");
    assert_eq!(embedded, 1, "changed hash re-embeds");
    let rows = store
        .raw_sql(
            "SELECT chunk_idx, hash FROM embeddings WHERE ref_id = 'jrnl_long' ORDER BY chunk_idx",
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "shrunk text leaves exactly one chunk row");
    assert_eq!(rows[0][0].as_i64(), Some(0));
    assert_ne!(
        rows[0][1].as_str().unwrap(),
        item_hash,
        "hash rolled with the text"
    );
}

#[tokio::test]
async fn zero_budget_defers_items_to_the_next_call() {
    let _env = ENV_LOCK.lock().await;
    let store = test_setup().await;
    std::env::set_var("HIVE_EMBED_STAGE_BUDGET_SECS", "0");

    seed_journal(&store, "jrnl_budget", "body to embed later").await;

    // Zero budget: the stale item is seen but never started.
    let embedded = store
        .backfill_embeddings()
        .await
        .expect("backfill under zero budget");
    assert_eq!(embedded, 0, "zero budget must defer, not embed");
    let count = store
        .raw_sql("SELECT count(*) FROM embeddings", vec![])
        .await
        .unwrap()[0][0]
        .as_i64()
        .unwrap();
    assert_eq!(count, 0, "no rows written under a spent budget");

    // Budget restored (read fresh each call): the deferred item drains.
    std::env::set_var("HIVE_EMBED_STAGE_BUDGET_SECS", "20");
    let embedded = store.backfill_embeddings().await.expect("backfill drains");
    assert_eq!(embedded, 1, "deferred item embeds next call");
    std::env::remove_var("HIVE_EMBED_STAGE_BUDGET_SECS");
}
