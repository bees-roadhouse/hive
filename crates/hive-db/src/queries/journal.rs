use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::enums::Ai;
use crate::error::{Error, Result};
use crate::types::JournalEntry;

const SELECT_COLS: &str =
    "id, ai, entry_date, title, body, tags, created_at, updated_at";

#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    pub ai: Option<Ai>,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
}

pub async fn add(
    pool: &PgPool,
    ai: Ai,
    entry_date: &str,
    title: Option<&str>,
    body: &str,
    tags: Option<&str>,
) -> Result<JournalEntry> {
    let row = sqlx::query_as::<_, JournalEntry>(
        "INSERT INTO journal_entries (ai, entry_date, title, body, tags) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, ai, entry_date, title, body, tags, created_at, updated_at",
    )
    .bind(ai.as_str())
    .bind(entry_date)
    .bind(title)
    .bind(body)
    .bind(tags)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<JournalEntry>> {
    Ok(sqlx::query_as::<_, JournalEntry>(&format!(
        "SELECT {SELECT_COLS} FROM journal_entries WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn require(pool: &PgPool, id: i64) -> Result<JournalEntry> {
    get(pool, id).await?.ok_or_else(|| Error::NotFound {
        kind: "journal_entry",
        id: id.to_string(),
    })
}

pub async fn list(pool: &PgPool, filters: &ListFilters) -> Result<Vec<JournalEntry>> {
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

    let rows = qb.build_query_as::<JournalEntry>().fetch_all(pool).await?;
    Ok(rows)
}
