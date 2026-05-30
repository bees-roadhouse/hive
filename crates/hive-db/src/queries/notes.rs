use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::enums::Author;
use crate::error::{Error, Result};
use crate::queries::projects;
use crate::slug::derive_slug;
use crate::types::Note;

const SELECT_COLS: &str = "id, author, title, body, tags, project, created_at, updated_at, slug";

#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    pub author: Option<Author>,
    pub project: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
}

pub async fn add(
    pool: &PgPool,
    author: Author,
    title: Option<&str>,
    body: &str,
    project: Option<&str>,
    tags: Option<&str>,
) -> Result<Note> {
    if let Some(p) = project {
        projects::require(pool, p).await?;
    }
    // Slug is informational post-0014 (no UNIQUE constraint); derive once
    // from the title (or fall back to "note") and accept collisions.
    let base_title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("note");
    let slug = derive_slug(base_title, "note");
    let row = sqlx::query_as::<_, Note>(
        "INSERT INTO notes (author, title, body, tags, project, slug) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING id, author, title, body, tags, project, created_at, updated_at, slug",
    )
    .bind(author.as_str())
    .bind(title)
    .bind(body)
    .bind(tags)
    .bind(project)
    .bind(&slug)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Note>> {
    Ok(
        sqlx::query_as::<_, Note>(&format!("SELECT {SELECT_COLS} FROM notes WHERE id = $1"))
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Slug-based lookup. Post-0014, slug is no longer UNIQUE on this table, so
/// a slug can match multiple rows. We return the newest match.
pub async fn find_by_slug<'e, E>(executor: E, slug: &str) -> Result<Option<Note>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    find_latest_by_slug(executor, slug).await
}

pub async fn find_latest_by_slug<'e, E>(executor: E, slug: &str) -> Result<Option<Note>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(sqlx::query_as::<_, Note>(&format!(
        "SELECT {SELECT_COLS} FROM notes WHERE slug = $1 \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    ))
    .bind(slug)
    .fetch_optional(executor)
    .await?)
}

pub async fn require(pool: &PgPool, id: Uuid) -> Result<Note> {
    get(pool, id).await?.ok_or_else(|| Error::NotFound {
        kind: "note",
        id: id.to_string(),
    })
}

pub async fn list(pool: &PgPool, filters: &ListFilters) -> Result<Vec<Note>> {
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!("SELECT {SELECT_COLS} FROM notes WHERE 1=1"));

    if let Some(a) = filters.author {
        qb.push(" AND author = ").push_bind(a.as_str().to_string());
    }
    if let Some(p) = &filters.project {
        qb.push(" AND project = ").push_bind(p.clone());
    }
    if let Some(t) = &filters.tag {
        qb.push(" AND tags LIKE ").push_bind(format!("%{t}%"));
    }
    qb.push(" ORDER BY created_at DESC, id DESC");
    if let Some(l) = filters.limit {
        qb.push(" LIMIT ").push_bind(l);
    }

    let rows = qb.build_query_as::<Note>().fetch_all(pool).await?;
    Ok(rows)
}

pub async fn list_for_journal_entry(pool: &PgPool, journal_entry_id: Uuid) -> Result<Vec<Note>> {
    Ok(sqlx::query_as::<_, Note>(&format!(
        "SELECT n.{cols} \
         FROM links l \
         JOIN notes n ON n.id = l.target_id \
         WHERE l.source_table = 'journal_entries' AND l.source_id = $1 \
           AND l.target_table = 'notes' AND l.link_type = 'spawned_in' \
         ORDER BY n.created_at, n.id",
        cols = SELECT_COLS.replace(", ", ", n.")
    ))
    .bind(journal_entry_id)
    .fetch_all(pool)
    .await?)
}
