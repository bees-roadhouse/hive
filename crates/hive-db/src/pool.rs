//! Postgres connection pool + migrations bootstrap.
//!
//! sqlx's `PgPool` is the async, postgres-native replacement for the old
//! r2d2 + rusqlite setup. The pool is cheap to clone and is intended to
//! live in `AppState`. Migrations live in `crates/hive-db/migrations/` and
//! are applied via `sqlx::migrate!` on first open.

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::error::Result;

/// Default connection string, used when the caller (hive-api / hive-cli)
/// doesn't pass `--database-url` or `DATABASE_URL`.
pub const DEFAULT_DATABASE_URL: &str = "postgres://hive:hive@localhost:5432/hive";

/// Resolve the database URL: `DATABASE_URL` env override, else the default.
pub fn default_database_url() -> String {
    std::env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// Open a `PgPool` against `url` with `max_size` connections, then run
/// pending migrations from `./migrations` (compiled in via the
/// `sqlx::migrate!` macro).
pub async fn open_pool(url: &str, max_size: u32) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(max_size)
        .connect(url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
