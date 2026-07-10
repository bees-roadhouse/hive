// Chunked backfill under a 384-dim "transformers" provider (a mock ONNX
// engine — offline + deterministic like the hash path, but dimension-eligible
// for the native vector column): chunk rows, item-level hash on every row,
// owner stamping, DUAL-WRITE of vec (BYTEA) + vec_v (pgvector), skip-on-replay,
// and atomic chunk-set replacement when the text changes.
//
// Own integration-test file on purpose: the provider choice ($HIVE_EMBED) and
// the engine are latched once per process, so this must not share a binary
// with the hash-provider tests.

use hive_core::store::Store;

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

async fn test_setup() -> (sqlx::PgPool, Store, hive_worker::Worker) {
    // Must beat the first embed call: the provider choice latches once per
    // process, and installing the mock first keeps the lazy default ort
    // engine (real model download) from ever wiring itself in.
    std::env::set_var("HIVE_EMBED", "transformers");
    hive_embed::set_onnx_provider(Box::new(Mock384));
    let pool = hive_core::db::test_pool().await;
    (
        pool.clone(),
        Store::new(pool.clone()),
        hive_worker::Worker::new(pool),
    )
}

#[tokio::test]
async fn chunked_dual_write_skip_and_atomic_replace() {
    let (pool, store, worker) = test_setup().await;
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

    worker.cycle(1).await.expect("cycle 1");
    let run = store.worker_status().await.unwrap().last_run.unwrap();
    assert_eq!(
        run.embedded, 1,
        "one ITEM embedded (chunks don't inflate it)"
    );
    assert!(!run.latched, "mock engine must not latch");

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
    worker.cycle(2).await.expect("cycle 2");
    let run = store.worker_status().await.unwrap().last_run.unwrap();
    assert_eq!(run.embedded, 0, "unchanged item skips");
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
    worker.cycle(3).await.expect("cycle 3");
    let run = store.worker_status().await.unwrap().last_run.unwrap();
    assert_eq!(run.embedded, 1, "changed hash re-embeds");
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
