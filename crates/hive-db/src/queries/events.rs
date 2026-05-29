//! `events` ... date-anchored first-class entity. Meetings, household
//! occurrences, milestones. Mirror of `journal.rs`'s shape so the mention
//! resolver and the UI sidecars can rely on a consistent surface.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::slug::derive_slug;
use crate::types::Event;

const SELECT_COLS: &str = "id, slug, title, body, starts_at, ends_at, location, tags, \
     created_at, updated_at";

#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
}

/// Insert an event. If `slug` is `Some`, accept it as-is; if `None`, derive
/// from `title`. Slug is no longer UNIQUE on this table (post-0014); we
/// accept collisions and the resolver picks newest-on-tie.
#[allow(clippy::too_many_arguments)]
pub async fn add(
    pool: &PgPool,
    slug: Option<&str>,
    title: &str,
    body: Option<&str>,
    starts_at: DateTime<Utc>,
    ends_at: Option<DateTime<Utc>>,
    location: Option<&str>,
    tags: Option<&str>,
) -> Result<Event> {
    let resolved_slug = match slug {
        Some(s) => s.to_string(),
        None => derive_slug(title, "event"),
    };
    let row = sqlx::query_as::<_, Event>(
        "INSERT INTO events (slug, title, body, starts_at, ends_at, location, tags) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING id, slug, title, body, starts_at, ends_at, location, tags, \
                   created_at, updated_at",
    )
    .bind(&resolved_slug)
    .bind(title)
    .bind(body)
    .bind(starts_at)
    .bind(ends_at)
    .bind(location)
    .bind(tags)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn get<'e, E>(executor: E, id: Uuid) -> Result<Option<Event>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(
        sqlx::query_as::<_, Event>(&format!("SELECT {SELECT_COLS} FROM events WHERE id = $1"))
            .bind(id)
            .fetch_optional(executor)
            .await?,
    )
}

pub async fn require(pool: &PgPool, id: Uuid) -> Result<Event> {
    get(pool, id).await?.ok_or_else(|| Error::NotFound {
        kind: "event",
        id: id.to_string(),
    })
}

/// Slug-based lookup. Post-0014, slug is no longer UNIQUE on this table, so
/// a slug can match multiple rows. We return the newest match.
pub async fn find_by_slug<'e, E>(executor: E, slug: &str) -> Result<Option<Event>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    find_latest_by_slug(executor, slug).await
}

pub async fn find_latest_by_slug<'e, E>(executor: E, slug: &str) -> Result<Option<Event>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(sqlx::query_as::<_, Event>(&format!(
        "SELECT {SELECT_COLS} FROM events WHERE slug = $1 \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    ))
    .bind(slug)
    .fetch_optional(executor)
    .await?)
}

pub async fn list(pool: &PgPool, filters: &ListFilters) -> Result<Vec<Event>> {
    list_in(pool, filters).await
}

pub async fn list_in<'e, E>(executor: E, filters: &ListFilters) -> Result<Vec<Event>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!("SELECT {SELECT_COLS} FROM events WHERE 1=1"));

    if let Some(f) = filters.from {
        qb.push(" AND starts_at >= ").push_bind(f);
    }
    if let Some(t) = filters.to {
        qb.push(" AND starts_at <= ").push_bind(t);
    }
    if let Some(t) = &filters.tag {
        qb.push(" AND tags LIKE ").push_bind(format!("%{t}%"));
    }
    qb.push(" ORDER BY starts_at DESC, id DESC");
    if let Some(l) = filters.limit {
        qb.push(" LIMIT ").push_bind(l);
    }

    let rows = qb.build_query_as::<Event>().fetch_all(executor).await?;
    Ok(rows)
}
