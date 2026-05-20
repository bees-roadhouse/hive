//! Inter-AI messages. Mirrors `messages` table:
//! id PK, sender_ai, recipient_ai, kind, body, in_reply_to,
//! sent_at (default now()), read_at, fts (tsvector).
//!
//! sender_ai / recipient_ai are stored as free-text (not the Ai enum) so
//! future participants don't require an enum change to send.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};

use crate::error::{Error, Result};

const SELECT_COLS: &str =
    "id, sender_ai, recipient_ai, kind, body, in_reply_to, sent_at, read_at";

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Message {
    pub id: i64,
    pub sender_ai: String,
    pub recipient_ai: String,
    pub kind: Option<String>,
    pub body: String,
    pub in_reply_to: Option<i64>,
    pub sent_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MessageHit {
    pub id: i64,
    pub sender_ai: String,
    pub recipient_ai: String,
    pub kind: Option<String>,
    pub sent_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
    pub snippet: String,
}

#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    pub from_ai: Option<String>,
    pub to_ai: Option<String>,
    pub kind: Option<String>,
    pub in_reply_to: Option<i64>,
    pub unread_only: bool,
    pub limit: Option<i64>,
}

pub async fn add(
    pool: &PgPool,
    sender_ai: &str,
    recipient_ai: &str,
    kind: Option<&str>,
    body: &str,
    in_reply_to: Option<i64>,
) -> Result<Message> {
    // validation: non-empty sender/recipient/body
    if sender_ai.trim().is_empty() {
        return Err(Error::InvalidFormat {
            field: "sender_ai",
            value: sender_ai.to_string(),
            expected: "non-empty",
        });
    }
    if recipient_ai.trim().is_empty() {
        return Err(Error::InvalidFormat {
            field: "recipient_ai",
            value: recipient_ai.to_string(),
            expected: "non-empty",
        });
    }
    if body.trim().is_empty() {
        return Err(Error::InvalidFormat {
            field: "body",
            value: String::new(),
            expected: "non-empty",
        });
    }
    // if in_reply_to set, parent must exist
    if let Some(parent_id) = in_reply_to {
        require(pool, parent_id).await?;
    }

    let row = sqlx::query_as::<_, Message>(
        "INSERT INTO messages (sender_ai, recipient_ai, kind, body, in_reply_to) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, sender_ai, recipient_ai, kind, body, in_reply_to, sent_at, read_at",
    )
    .bind(sender_ai)
    .bind(recipient_ai)
    .bind(kind)
    .bind(body)
    .bind(in_reply_to)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<Message>> {
    Ok(sqlx::query_as::<_, Message>(&format!(
        "SELECT {SELECT_COLS} FROM messages WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn require(pool: &PgPool, id: i64) -> Result<Message> {
    get(pool, id).await?.ok_or_else(|| Error::NotFound {
        kind: "message",
        id: id.to_string(),
    })
}

pub async fn list(pool: &PgPool, filters: &ListFilters) -> Result<Vec<Message>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT {SELECT_COLS} FROM messages WHERE 1=1"
    ));

    if let Some(s) = &filters.from_ai {
        qb.push(" AND sender_ai = ").push_bind(s.clone());
    }
    if let Some(r) = &filters.to_ai {
        qb.push(" AND recipient_ai = ").push_bind(r.clone());
    }
    if let Some(k) = &filters.kind {
        qb.push(" AND kind = ").push_bind(k.clone());
    }
    if let Some(parent) = filters.in_reply_to {
        qb.push(" AND in_reply_to = ").push_bind(parent);
    }
    if filters.unread_only {
        qb.push(" AND read_at IS NULL");
    }
    qb.push(" ORDER BY sent_at DESC, id DESC");
    if let Some(l) = filters.limit {
        qb.push(" LIMIT ").push_bind(l);
    }

    let rows = qb.build_query_as::<Message>().fetch_all(pool).await?;
    Ok(rows)
}

/// Idempotent: leaves read_at untouched if already set.
pub async fn mark_read(pool: &PgPool, id: i64) -> Result<Message> {
    let existing = require(pool, id).await?;
    if existing.read_at.is_none() {
        sqlx::query("UPDATE messages SET read_at = now() WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await?;
    }
    require(pool, id).await
}

/// Postgres tsvector full-text search over `messages.fts`. ts_headline
/// produces the bracketed snippet (matching the old FTS5 contract).
pub async fn search(pool: &PgPool, query: &str, limit: i64) -> Result<Vec<MessageHit>> {
    let rows = sqlx::query_as::<_, MessageHit>(
        "SELECT m.id, m.sender_ai, m.recipient_ai, m.kind, m.sent_at, m.read_at, \
                ts_headline('english', m.body, plainto_tsquery('english', $1), \
                            'StartSel=[, StopSel=], MaxFragments=1, MaxWords=20') AS snippet \
         FROM messages m \
         WHERE m.fts @@ plainto_tsquery('english', $1) \
         ORDER BY ts_rank(m.fts, plainto_tsquery('english', $1)) DESC \
         LIMIT $2",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
