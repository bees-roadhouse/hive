// Migration 0002 (pgvector embeddings reshape) over both bases the hybrid
// convention promises to tolerate: a fresh database (inline DDL builds the
// final shape, the migration no-ops) and an old-shape database (2-col PK,
// NOT NULL vec, no chunk_idx/owner/vec_v — the migration wipes + reshapes).
// Below it, migration 0004 (cc_credentials.kdf_version) gets the same
// both-bases treatment.

use sqlx::PgPool;

async fn column_exists(pool: &PgPool, column: &str) -> bool {
    sqlx::query_scalar::<_, i32>(
        "SELECT 1 FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'embeddings' AND column_name = $1",
    )
    .bind(column)
    .fetch_optional(pool)
    .await
    .expect("column probe")
    .is_some()
}

async fn pk_columns(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT k.column_name \
         FROM information_schema.table_constraints c \
         JOIN information_schema.key_column_usage k \
           ON k.constraint_name = c.constraint_name \
          AND k.table_schema = c.table_schema AND k.table_name = c.table_name \
         WHERE c.table_schema = current_schema() AND c.table_name = 'embeddings' \
           AND c.constraint_type = 'PRIMARY KEY' \
         ORDER BY k.ordinal_position",
    )
    .fetch_all(pool)
    .await
    .expect("pk probe")
}

async fn assert_final_embeddings_shape(pool: &PgPool) {
    for col in ["chunk_idx", "owner", "vec_v"] {
        assert!(column_exists(pool, col).await, "missing column {col}");
    }
    let vec_nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'embeddings' AND column_name = 'vec'",
    )
    .fetch_one(pool)
    .await
    .expect("vec nullability");
    assert_eq!(vec_nullable, "YES", "vec must have dropped NOT NULL");
    assert_eq!(
        pk_columns(pool).await,
        ["ref_kind", "ref_id", "chunk_idx"],
        "PK must be the 3-column chunked key"
    );
    let hnsw: Option<String> = sqlx::query_scalar(
        "SELECT indexname FROM pg_indexes \
         WHERE schemaname = current_schema() AND tablename = 'embeddings' \
           AND indexname = 'embeddings_vec_hnsw'",
    )
    .fetch_optional(pool)
    .await
    .expect("index probe");
    assert!(hnsw.is_some(), "HNSW index missing");
}

#[tokio::test]
async fn fresh_database_lands_on_final_embeddings_shape() {
    // test_pool runs the migrator + the inline DDL, exactly like a boot.
    let test_db = hive_api::db::test_pool().await;
    let pool = test_db.pool.clone();
    assert_final_embeddings_shape(&pool).await;

    // The native vector column round-trips through the pgvector binding, and
    // chunked rows insert under the new PK.
    let v = pgvector::Vector::from(vec![0.5f32; 384]);
    sqlx::query(
        "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec, vec_v, hash, created_at) \
         VALUES ('journal', 'jrnl_v', 1, 'bge-test', 384, 'nate', $1, $2, 'h1', '2026-01-01T00:00:00.000Z')",
    )
    .bind(hive_embed::to_blob(&[0.5f32; 384]))
    .bind(&v)
    .execute(&pool)
    .await
    .expect("chunked insert with vec_v");
    let (got, owner): (pgvector::Vector, Option<String>) = sqlx::query_as(
        "SELECT vec_v, owner FROM embeddings WHERE ref_kind = 'journal' AND ref_id = 'jrnl_v' AND chunk_idx = 1",
    )
    .fetch_one(&pool)
    .await
    .expect("vec_v readback");
    assert_eq!(got.as_slice().len(), 384);
    assert_eq!(owner.as_deref(), Some("nate"));

    // CHECK: a row with neither vector representation must be rejected.
    let err = sqlx::query(
        "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, hash, created_at) \
         VALUES ('journal', 'jrnl_v', 2, 'bge-test', 384, 'h1', '2026-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await;
    assert!(err.is_err(), "vec-less + vec_v-less row must violate CHECK");
}

#[tokio::test]
async fn migration_reshapes_and_wipes_old_embeddings_table() {
    // An empty schema with migrate() NOT run: this test lays down the old
    // shape by hand and migrates over it.
    let test_db = hive_api::db::test_pool_unmigrated().await;
    let pool = test_db.pool.clone();

    // The pre-0.6 shape, verbatim from the old inline DDL, plus one legacy
    // row that must NOT survive (chunking changes row identity — wipe and
    // re-embed is the migration decision).
    sqlx::raw_sql(
        "CREATE TABLE embeddings (
           ref_kind   TEXT NOT NULL,
           ref_id     TEXT NOT NULL,
           model      TEXT NOT NULL,
           dim        BIGINT NOT NULL,
           vec        BYTEA NOT NULL,
           hash       TEXT NOT NULL,
           created_at TEXT NOT NULL,
           PRIMARY KEY (ref_kind, ref_id)
         )",
    )
    .execute(&pool)
    .await
    .expect("old-shape table");
    sqlx::query(
        "INSERT INTO embeddings (ref_kind, ref_id, model, dim, vec, hash, created_at) \
         VALUES ('journal', 'jrnl_legacy', 'hash-ngram-v1', 4, $1, 'stale', '2025-01-01T00:00:00.000Z')",
    )
    .bind(vec![0u8; 16])
    .execute(&pool)
    .await
    .expect("legacy row");

    hive_api::db::migrate(&pool).await.expect("migrate");

    assert_final_embeddings_shape(&pool).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM embeddings")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count, 0, "old-shape vectors must be wiped for re-embed");

    // And migrate() must be re-runnable on the now-final shape (the inline
    // DDL + a second boot racing are the everyday case).
    hive_api::db::migrate(&pool).await.expect("re-migrate");
    assert_final_embeddings_shape(&pool).await;
}

// ---- migration 0004: cc_credentials.kdf_version ----

async fn assert_final_cc_credentials_shape(pool: &PgPool) {
    let (data_type, is_nullable, default): (String, String, Option<String>) = sqlx::query_as(
        "SELECT data_type, is_nullable, column_default FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'cc_credentials' \
           AND column_name = 'kdf_version'",
    )
    .fetch_one(pool)
    .await
    .expect("kdf_version probe");
    assert_eq!(data_type, "integer");
    assert_eq!(is_nullable, "NO");
    // DEFAULT 1 is the backfill: rows that predate the column were sealed
    // under the SHA-256 derivation, so the default labels them correctly.
    assert_eq!(default.as_deref(), Some("1"));
}

#[tokio::test]
async fn fresh_database_lands_on_final_cc_credentials_shape() {
    // test_pool runs the migrator + the inline DDL, exactly like a boot.
    let test_db = hive_api::db::test_pool().await;
    assert_final_cc_credentials_shape(&test_db.pool).await;
}

#[tokio::test]
async fn migration_labels_legacy_cc_credentials_rows_v1() {
    // An empty schema with migrate() NOT run: lay down the old shape by hand
    // and migrate over it.
    let test_db = hive_api::db::test_pool_unmigrated().await;
    let pool = test_db.pool.clone();

    // The pre-kdf_version shape, verbatim from the old inline DDL, plus one
    // legacy row that must come out the other side labeled kdf_version 1.
    sqlx::raw_sql(
        "CREATE TABLE cc_credentials (
           id           TEXT PRIMARY KEY,
           owner        TEXT NOT NULL,
           kind         TEXT NOT NULL,
           runtime      TEXT NOT NULL DEFAULT 'claude_code',
           provider     TEXT,
           label        TEXT NOT NULL DEFAULT '',
           ciphertext   TEXT NOT NULL,
           nonce        TEXT NOT NULL,
           tail         TEXT NOT NULL DEFAULT '',
           created_at   TEXT NOT NULL,
           last_used_at TEXT
         )",
    )
    .execute(&pool)
    .await
    .expect("old-shape table");
    sqlx::query(
        "INSERT INTO cc_credentials (id, owner, kind, ciphertext, nonce, created_at) \
         VALUES ('cred-legacy', 'nate', 'api_key', 'b3BhYXVl', 'bm9uY2U=', '2025-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .expect("legacy row");

    hive_api::db::migrate(&pool).await.expect("migrate");

    assert_final_cc_credentials_shape(&pool).await;

    // The legacy row is labeled v1. The migrated table is FORCE-RLS and this
    // pool is the owner role with no scope stamping, so read it through a
    // transaction-local scope (org backfill targets DEFAULT_ORG_ID).
    let mut tx = pool.begin().await.expect("tx");
    sqlx::query(
        "SELECT set_config('hive.acting_org', $1, true), \
                set_config('hive.acting_admin', 'on', true)",
    )
    .bind(hive_api::db::DEFAULT_ORG_ID.to_string())
    .execute(&mut *tx)
    .await
    .expect("local scope");
    let version: i32 =
        sqlx::query_scalar("SELECT kdf_version FROM cc_credentials WHERE id = 'cred-legacy'")
            .fetch_one(&mut *tx)
            .await
            .expect("legacy row readable in its org");
    assert_eq!(version, 1, "legacy rows must be labeled v1 (SHA-256)");
    tx.commit().await.expect("commit");

    // And migrate() must be re-runnable on the now-final shape.
    hive_api::db::migrate(&pool).await.expect("re-migrate");
    assert_final_cc_credentials_shape(&pool).await;
}
