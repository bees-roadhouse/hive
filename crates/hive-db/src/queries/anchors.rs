use sqlx::PgPool;

use crate::error::Result;
use crate::types::TaskAnchor;

const SELECT_COLS: &str = "task_id, journal_entry_id, block_id, created_at";

pub async fn list_for_entry(pool: &PgPool, journal_entry_id: i64) -> Result<Vec<TaskAnchor>> {
    let rows = sqlx::query_as::<_, TaskAnchor>(&format!(
        "SELECT {SELECT_COLS} FROM task_anchors \
         WHERE journal_entry_id = $1 ORDER BY block_id"
    ))
    .bind(journal_entry_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_for_task(pool: &PgPool, task_id: i64) -> Result<Vec<TaskAnchor>> {
    let rows = sqlx::query_as::<_, TaskAnchor>(&format!(
        "SELECT {SELECT_COLS} FROM task_anchors \
         WHERE task_id = $1 ORDER BY journal_entry_id, block_id"
    ))
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Insert-or-rewrite. The PK is `(journal_entry_id, block_id)` ... if the
/// block id moves to a different task on the same entry, the row's `task_id`
/// is updated in place.
pub async fn upsert(
    pool: &PgPool,
    task_id: i64,
    journal_entry_id: i64,
    block_id: &str,
) -> Result<TaskAnchor> {
    let row = sqlx::query_as::<_, TaskAnchor>(
        "INSERT INTO task_anchors (task_id, journal_entry_id, block_id) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (journal_entry_id, block_id) DO UPDATE SET task_id = EXCLUDED.task_id \
         RETURNING task_id, journal_entry_id, block_id, created_at",
    )
    .bind(task_id)
    .bind(journal_entry_id)
    .bind(block_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn delete(pool: &PgPool, journal_entry_id: i64, block_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM task_anchors WHERE journal_entry_id = $1 AND block_id = $2")
        .bind(journal_entry_id)
        .bind(block_id)
        .execute(pool)
        .await?;
    Ok(())
}
