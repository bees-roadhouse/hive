use sqlx::PgPool;

use crate::error::Result;
use crate::types::Person;

const SELECT_COLS: &str =
    "id, slug, display_name, kind, notes, created_at, updated_at";

pub async fn list(pool: &PgPool) -> Result<Vec<Person>> {
    let rows = sqlx::query_as::<_, Person>(&format!(
        "SELECT {SELECT_COLS} FROM people ORDER BY kind, slug"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get_by_slug(pool: &PgPool, slug: &str) -> Result<Option<Person>> {
    Ok(sqlx::query_as::<_, Person>(&format!(
        "SELECT {SELECT_COLS} FROM people WHERE slug = $1"
    ))
    .bind(slug)
    .fetch_optional(pool)
    .await?)
}

/// Insert-if-missing, return current row either way. `default_display_name`
/// and `kind` are used only on insert; existing rows are returned unmodified.
pub async fn ensure(
    pool: &PgPool,
    slug: &str,
    default_display_name: &str,
    kind: &str,
) -> Result<Person> {
    let row = sqlx::query_as::<_, Person>(
        "INSERT INTO people (slug, display_name, kind) VALUES ($1, $2, $3) \
         ON CONFLICT (slug) DO UPDATE SET slug = EXCLUDED.slug \
         RETURNING id, slug, display_name, kind, notes, created_at, updated_at",
    )
    .bind(slug)
    .bind(default_display_name)
    .bind(kind)
    .fetch_one(pool)
    .await?;
    Ok(row)
}
