use sqlx::{PgPool, Postgres};

use crate::error::Result;
use crate::types::Person;

const SELECT_COLS: &str = "id, slug, display_name, notes, created_at, updated_at";

pub async fn list(pool: &PgPool) -> Result<Vec<Person>> {
    let rows =
        sqlx::query_as::<_, Person>(&format!("SELECT {SELECT_COLS} FROM people ORDER BY slug"))
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

pub async fn get<'e, E>(executor: E, id: uuid::Uuid) -> Result<Option<Person>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(
        sqlx::query_as::<_, Person>(&format!("SELECT {SELECT_COLS} FROM people WHERE id = $1"))
            .bind(id)
            .fetch_optional(executor)
            .await?,
    )
}

pub async fn find_by_slug<'e, E>(executor: E, slug: &str) -> Result<Option<Person>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(
        sqlx::query_as::<_, Person>(&format!("SELECT {SELECT_COLS} FROM people WHERE slug = $1"))
            .bind(slug)
            .fetch_optional(executor)
            .await?,
    )
}

/// Back-compat shim for the old `get_by_slug` name. Kept so callers in other
/// crates don't need to be touched in the same change.
pub async fn get_by_slug(pool: &PgPool, slug: &str) -> Result<Option<Person>> {
    find_by_slug(pool, slug).await
}

/// Insert-if-missing, return current row either way. `default_display_name`
/// is used only on insert; existing rows are returned unmodified.
pub async fn ensure(pool: &PgPool, slug: &str, default_display_name: &str) -> Result<Person> {
    let row = sqlx::query_as::<_, Person>(
        "INSERT INTO people (slug, display_name) VALUES ($1, $2) \
         ON CONFLICT (slug) DO UPDATE SET slug = EXCLUDED.slug \
         RETURNING id, slug, display_name, notes, created_at, updated_at",
    )
    .bind(slug)
    .bind(default_display_name)
    .fetch_one(pool)
    .await?;
    Ok(row)
}
