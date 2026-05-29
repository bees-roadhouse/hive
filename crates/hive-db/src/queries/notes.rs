use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::enums::Author;
use crate::error::{Error, Result};
use crate::queries::projects;
use crate::slug::{derive_slug, resolve_collision};
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
    let slug = derive_note_slug(pool, title).await;
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

async fn derive_note_slug(pool: &PgPool, title: Option<&str>) -> String {
    let base_title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("note");
    let base = derive_slug(base_title, "note");
    resolve_collision(&base, |candidate| async move {
        let exists =
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM notes WHERE slug = $1)")
                .bind(candidate)
                .fetch_one(pool)
                .await
                .unwrap_or(false);
        !exists
    })
    .await
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Note>> {
    Ok(
        sqlx::query_as::<_, Note>(&format!("SELECT {SELECT_COLS} FROM notes WHERE id = $1"))
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn find_by_slug<'e, E>(executor: E, slug: &str) -> Result<Option<Note>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(
        sqlx::query_as::<_, Note>(&format!("SELECT {SELECT_COLS} FROM notes WHERE slug = $1"))
            .bind(slug)
            .fetch_optional(executor)
            .await?,
    )
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
