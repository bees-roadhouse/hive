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
async fn outbox_claim_leaves_foreign_kinds_for_their_own_drainer() {
    let (pool, _dir) = test_pool().await;
    let store = Store::new(pool.clone());
    let worker = hive_worker::Worker::new(pool.clone());

    store
        .outbox_enqueue("log", serde_json::json!({}), None, "test")
        .await
        .expect("enqueue log");
    store
        .outbox_enqueue(
            "mail.send",
            serde_json::json!({"to": "someone@example.com"}),
            None,
            "test",
        )
        .await
        .expect("enqueue mail.send");

    worker.cycle(1).await.expect("cycle drains");
    let run = store.worker_status().await.unwrap().last_run.unwrap();
    assert_eq!(run.outbox, 1, "only the worker-owned kind drains");

    let (log_status,): (String,) = sqlx::query_as("SELECT status FROM outbox WHERE kind = 'log'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(log_status, "done");
    // The foreign kind must stay pending — a completed no-op here would
    // silently swallow every Phase 2 mail send.
    let (mail_status,): (String,) =
        sqlx::query_as("SELECT status FROM outbox WHERE kind = 'mail.send'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mail_status, "pending");
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

#[tokio::test]
async fn backfill_stamps_owner_and_stays_bytea_for_hash_provider() {
    let (pool, _dir) = test_pool().await;
    let worker = hive_worker::Worker::new(pool.clone());

    // A scoped journal entry, a task anchored to it, and a free-floating task:
    // journal rows carry their own user_scope; anchored entities inherit the
    // origin entry's; no origin means global (owner NULL).
    sqlx::query(
        "INSERT INTO journal (id, author, body, user_scope, created_at) \
         VALUES ('jrnl_scoped', 'nate', 'private brood notes', 'nate', '2026-01-02T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tasks (id, title, origin_entry_id, created_at, updated_at) \
         VALUES ('task_anchored', 'requeen hive 2', 'jrnl_scoped', '2026-01-02T00:00:00.000Z', '2026-01-02T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO tasks (id, title, created_at, updated_at) \
         VALUES ('task_floating', 'paint supers', '2026-01-02T00:00:00.000Z', '2026-01-02T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();

    worker.cycle(1).await.expect("cycle embeds");

    let owner_of = |kind: &str, id: &str| {
        let pool = pool.clone();
        let (kind, id) = (kind.to_string(), id.to_string());
        async move {
            let (owner,): (Option<String>,) = sqlx::query_as(
                "SELECT owner FROM embeddings WHERE ref_kind = $1 AND ref_id = $2 AND chunk_idx = 0",
            )
            .bind(kind)
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("embedding row");
            owner
        }
    };
    assert_eq!(
        owner_of("journal", "jrnl_scoped").await.as_deref(),
        Some("nate"),
        "journal embedding carries the entry's user_scope"
    );
    assert_eq!(
        owner_of("task", "task_anchored").await.as_deref(),
        Some("nate"),
        "anchored task inherits its origin entry's scope"
    );
    assert_eq!(
        owner_of("task", "task_floating").await,
        None,
        "task without an origin entry stays global"
    );

    // 256-dim hash provider: BYTEA only — vec populated, vec_v never written
    // (the vector(384) column is for the BGE model; dual-write is dim-gated).
    let (bytea_only,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM embeddings WHERE vec IS NULL OR vec_v IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(bytea_only, 0, "hash rows must be vec-only");
}
