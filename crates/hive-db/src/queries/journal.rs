use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::enums::Ai;
use crate::error::{Error, Result};
use crate::slug::derive_slug;
use crate::types::JournalEntry;

const SELECT_COLS: &str = "id, ai, entry_date, title, body, tags, created_at, updated_at, slug";

#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    pub ai: Option<Ai>,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
}

/// Insert a journal entry. Derives a slug from the title (falling back to
/// `<ai>-entry`). Slug is no longer UNIQUE on this table (post-0014); we
/// accept collisions and the resolver picks newest-on-tie. Callers that
/// want an explicit slug should use `add_with_slug`.
pub async fn add(
    pool: &PgPool,
    ai: Ai,
    entry_date: &str,
    title: Option<&str>,
    body: &str,
    tags: Option<&str>,
) -> Result<JournalEntry> {
    let slug = derive_journal_slug(ai, title);
    add_with_slug(pool, ai, entry_date, title, body, tags, &slug).await
}

pub async fn add_with_slug(
    pool: &PgPool,
    ai: Ai,
    entry_date: &str,
    title: Option<&str>,
    body: &str,
    tags: Option<&str>,
    slug: &str,
) -> Result<JournalEntry> {
    let row = sqlx::query_as::<_, JournalEntry>(
        "INSERT INTO journal_entries (ai, entry_date, title, body, tags, slug) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING id, ai, entry_date, title, body, tags, created_at, updated_at, slug",
    )
    .bind(ai.as_str())
    .bind(entry_date)
    .bind(title)
    .bind(body)
    .bind(tags)
    .bind(slug)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

fn derive_journal_slug(ai: Ai, title: Option<&str>) -> String {
    let base_title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}-entry", ai.as_str()));
    derive_slug(&base_title, "entry")
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<JournalEntry>> {
    Ok(sqlx::query_as::<_, JournalEntry>(&format!(
        "SELECT {SELECT_COLS} FROM journal_entries WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Slug-based lookup. Post-0014, slug is no longer UNIQUE on this table, so
/// a slug can match multiple rows. We return the newest match.
pub async fn find_by_slug<'e, E>(executor: E, slug: &str) -> Result<Option<JournalEntry>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    find_latest_by_slug(executor, slug).await
}

pub async fn find_latest_by_slug<'e, E>(executor: E, slug: &str) -> Result<Option<JournalEntry>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(sqlx::query_as::<_, JournalEntry>(&format!(
        "SELECT {SELECT_COLS} FROM journal_entries WHERE slug = $1 \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    ))
    .bind(slug)
    .fetch_optional(executor)
    .await?)
}

pub async fn require(pool: &PgPool, id: Uuid) -> Result<JournalEntry> {
    get(pool, id).await?.ok_or_else(|| Error::NotFound {
        kind: "journal_entry",
        id: id.to_string(),
    })
}

pub async fn list(pool: &PgPool, filters: &ListFilters) -> Result<Vec<JournalEntry>> {
    list_in(pool, filters).await
}

/// `list`, but against any executor — a `&PgPool` or a `&mut Transaction`. The
/// transaction form lets the caller run the query under per-request RLS GUCs
/// (Phase 8, §5.6) on the same connection the `SET LOCAL` was issued on. Behaves
/// identically to `list` when handed a pool.
pub async fn list_in<'e, E>(executor: E, filters: &ListFilters) -> Result<Vec<JournalEntry>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT {SELECT_COLS} FROM journal_entries WHERE 1=1"
    ));

    if let Some(a) = filters.ai {
        qb.push(" AND ai = ").push_bind(a.as_str().to_string());
    }
    if let Some(d) = &filters.from_date {
        qb.push(" AND entry_date >= ").push_bind(d.clone());
    }
    if let Some(d) = &filters.to_date {
        qb.push(" AND entry_date <= ").push_bind(d.clone());
    }
    if let Some(t) = &filters.tag {
        qb.push(" AND tags LIKE ").push_bind(format!("%{t}%"));
    }
    qb.push(" ORDER BY entry_date DESC, id DESC");
    if let Some(l) = filters.limit {
        qb.push(" LIMIT ").push_bind(l);
    }

    let rows = qb
        .build_query_as::<JournalEntry>()
        .fetch_all(executor)
        .await?;
    Ok(rows)
}

#[derive(Debug, Default, Clone)]
pub struct UpdateFields {
    pub title: Option<Option<String>>,
    pub body: Option<String>,
    pub tags: Option<Option<String>>,
    pub entry_date: Option<String>,
}

pub async fn update(pool: &PgPool, id: Uuid, fields: &UpdateFields) -> Result<JournalEntry> {
    if fields.title.is_none()
        && fields.body.is_none()
        && fields.tags.is_none()
        && fields.entry_date.is_none()
    {
        return Err(Error::InvalidFormat {
            field: "update",
            value: "(none)".into(),
            expected: "at least one field to update",
        });
    }

    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new("UPDATE journal_entries SET updated_at = now()");
    let mut first = true;
    let push = |qb: &mut QueryBuilder<Postgres>, first: &mut bool| {
        if *first {
            *first = false;
        } else {
            qb.push(", ");
        }
    };
    if let Some(title) = &fields.title {
        push(&mut qb, &mut first);
        qb.push("title = ").push_bind(title.clone());
    }
    if let Some(body) = &fields.body {
        push(&mut qb, &mut first);
        qb.push("body = ").push_bind(body.clone());
    }
    if let Some(tags) = &fields.tags {
        push(&mut qb, &mut first);
        qb.push("tags = ").push_bind(tags.clone());
    }
    if let Some(entry_date) = &fields.entry_date {
        push(&mut qb, &mut first);
        qb.push("entry_date = ").push_bind(entry_date.clone());
    }
    qb.push(" WHERE id = ").push_bind(id);
    qb.push(" RETURNING id, ai, entry_date, title, body, tags, created_at, updated_at, slug");
    Ok(qb.build_query_as::<JournalEntry>().fetch_one(pool).await?)
}
