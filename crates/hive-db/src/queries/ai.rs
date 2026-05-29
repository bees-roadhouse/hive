//! Queries against the `ai` directory table (migration 0013). This is the
//! AI-side companion to `people`: pia/apis/cera live here, nate/maggie live
//! in `people`. The auth-side `ai_identities` table is a different concept
//! and lives behind `crate::auth::ai` in `hive-api`.

use sqlx::{PgPool, Postgres};
use uuid::Uuid;

use crate::error::Result;
use crate::types::Ai;

const SELECT_COLS: &str = "id, slug, display_name, kind, notes, created_at, updated_at";

pub async fn list(pool: &PgPool, limit: Option<i64>) -> Result<Vec<Ai>> {
    let sql = match limit {
        Some(_) => format!("SELECT {SELECT_COLS} FROM ai ORDER BY slug LIMIT $1"),
        None => format!("SELECT {SELECT_COLS} FROM ai ORDER BY slug"),
    };
    let mut q = sqlx::query_as::<_, Ai>(&sql);
    if let Some(l) = limit {
        q = q.bind(l);
    }
    Ok(q.fetch_all(pool).await?)
}

pub async fn get<'e, E>(executor: E, id: Uuid) -> Result<Option<Ai>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(
        sqlx::query_as::<_, Ai>(&format!("SELECT {SELECT_COLS} FROM ai WHERE id = $1"))
            .bind(id)
            .fetch_optional(executor)
            .await?,
    )
}

pub async fn find_by_slug<'e, E>(executor: E, slug: &str) -> Result<Option<Ai>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(
        sqlx::query_as::<_, Ai>(&format!("SELECT {SELECT_COLS} FROM ai WHERE slug = $1"))
            .bind(slug)
            .fetch_optional(executor)
            .await?,
    )
}
