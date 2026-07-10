// The one store-construction seam for every hive-core integration test.
//
// PR 1.6 (the SQLite cutover) swaps THIS function to a tempdir SQLite store +
// mock keys — test bodies stay unchanged. That only works if no test body
// touches Postgres construction directly, so: every integration test builds
// its store through `test_store()`, and anything inherently Postgres-shaped
// (the migration-reshape harness below) lives here, clearly marked to die
// with the cutover.

use hive_core::store::Store;

/// A Store over a fresh, isolated database (today: a uniquely-named Postgres
/// schema via `db::test_pool()`; after PR 1.6: tempdir SQLite).
pub async fn test_store() -> Store {
    Store::new(hive_core::db::test_pool().await)
}

/// A raw pool pinned to a brand-new schema WITHOUT running migrate() — the
/// migration upgrade-path test lays down the old shape by hand first.
/// Postgres-specific by nature; dies with migrations.rs at the 1.6 cutover.
#[allow(dead_code)]
pub async fn raw_schema_pool() -> sqlx::PgPool {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    let url = hive_core::db::database_url();
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect admin");
    sqlx::raw_sql(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .expect("create schema");
    admin.close().await;
    let opts: PgConnectOptions = url.parse().expect("parse DATABASE_URL");
    let opts = opts.options([("search_path", format!("{schema},public"))]);
    PgPoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .expect("connect pool")
}
