// Worker-cycle parity smoke over a temp db: heartbeat + last_run shape,
// maintenance labels (Node's exact strings + vacuum cadence), and embeddings
// backfill with hash/model dedup.

use hive_api::store::Store;

async fn test_pool() -> (sqlx::PgPool, ()) {
    // Hash embedder: deterministic + offline (set before any embed call; the
    // provider choice is latched once per process).
    std::env::set_var("HIVE_EMBED", "hash");
    // Isolated Postgres schema per test (uses DATABASE_URL / local dev default).
    let pool = hive_api::db::test_pool().await;
    (pool, ())
}

#[tokio::test]
async fn cycle_writes_status_and_node_maintenance_labels() {
    let (pool, _dir) = test_pool().await;
    let store = Store::new(pool.clone());
    let worker = hive_worker::Worker::new(pool);

    // Postgres handles WAL/GIN/autovacuum itself, so the worker's only
    // maintenance is pruning the wire log — and only when there's surplus.
    worker.cycle(1).await.expect("cycle 1");
    let status = store.worker_status().await.expect("status");
    assert!(status.heartbeat.is_some(), "heartbeat stamped");
    let run = status.last_run.expect("last_run written");
    assert_eq!(run.polled, 0);
    assert_eq!(run.ingested, 0);
    assert_eq!(run.outbox, 0);
    assert!(
        run.maintenance.is_empty(),
        "wire is empty → nothing pruned: {:?}",
        run.maintenance
    );
}

#[tokio::test]
async fn backfill_embeds_new_skips_unchanged_reembeds_changed() {
    let (pool, _dir) = test_pool().await;
    let store = Store::new(pool.clone());
    let worker = hive_worker::Worker::new(pool.clone());

    sqlx::query(
        "INSERT INTO journal (id, author, body, created_at) \
         VALUES ('jrnl_test1', 'pia', 'first entry body', '2026-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    worker.cycle(2).await.expect("cycle embeds new item");
    let run = store.worker_status().await.unwrap().last_run.unwrap();
    assert_eq!(run.embedded, 1, "new journal entry embedded");

    worker.cycle(3).await.expect("cycle skips unchanged");
    let run = store.worker_status().await.unwrap().last_run.unwrap();
    assert_eq!(run.embedded, 0, "unchanged hash + model → skip");

    sqlx::query("UPDATE journal SET body = 'edited body' WHERE id = 'jrnl_test1'")
        .execute(&pool)
        .await
        .unwrap();
    worker.cycle(4).await.expect("cycle re-embeds changed");
    let run = store.worker_status().await.unwrap().last_run.unwrap();
    assert_eq!(run.embedded, 1, "changed hash → re-embed");

    let (model, dim): (String, i64) = sqlx::query_as(
        "SELECT model, dim FROM embeddings WHERE ref_kind = 'journal' AND ref_id = 'jrnl_test1'",
    )
    .fetch_one(&pool)
    .await
    .expect("embedding row stored");
    assert_eq!(model, hive_embed::embed_model());
    assert!(dim > 0);
}
