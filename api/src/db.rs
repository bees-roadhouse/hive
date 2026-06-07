use anyhow::Result;
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::str::FromStr;
use tracing::info;

const MIGRATIONS_SQL: &str = include_str!("../migrations/001_initial.sql");

pub async fn init_db(database_url: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(30));

    let pool = SqlitePool::connect_with(opts).await?;

    // Run migrations manually since sqlx migrate isn't easily embedded
    for stmt in MIGRATIONS_SQL.split(';').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        sqlx::query(stmt).execute(&pool).await?;
    }

    info!("Database initialized at {}", database_url);
    Ok(pool)
}

pub type Db = SqlitePool;
