// The embed stage time-box ($HIVE_EMBED_STAGE_BUDGET_SECS): a spent budget
// stops new items from starting and defers them to the next cycle. Own test
// file: the env var is process-global, so this must not run beside tests that
// expect the default budget.

use hive_api::store::Store;

#[tokio::test]
async fn zero_budget_defers_items_to_the_next_cycle() {
    std::env::set_var("HIVE_EMBED", "hash");
    std::env::set_var("HIVE_EMBED_STAGE_BUDGET_SECS", "0");
    let pool = hive_api::db::test_pool().await;
    let store = Store::new(pool.clone());
    let worker = hive_worker::Worker::new(pool.clone());

    sqlx::query(
        "INSERT INTO journal (id, author, body, created_at) \
         VALUES ('jrnl_budget', 'pia', 'body to embed later', '2026-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Zero budget: the stale item is seen but never started.
    worker.cycle(1).await.expect("cycle under zero budget");
    let run = store.worker_status().await.unwrap().last_run.unwrap();
    assert_eq!(run.embedded, 0, "zero budget must defer, not embed");
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM embeddings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "no rows written under a spent budget");

    // Budget restored (read fresh each cycle): the deferred item drains.
    std::env::set_var("HIVE_EMBED_STAGE_BUDGET_SECS", "20");
    worker.cycle(2).await.expect("cycle with budget");
    let run = store.worker_status().await.unwrap().last_run.unwrap();
    assert_eq!(run.embedded, 1, "deferred item embeds next cycle");
}
