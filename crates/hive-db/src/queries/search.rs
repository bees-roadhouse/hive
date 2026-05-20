//! tsvector full-text search over `journal_entries.fts` and `notes.fts`.
//! Mirrors python `cmd_journal_search` / `cmd_notes_search` / `cmd_search`
//! (non-hybrid path). Hybrid semantic search lives in the `hive-embed` crate.

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct JournalHit {
    pub id: i64,
    pub ai: String,
    pub entry_date: String,
    pub title: Option<String>,
    pub tags: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct NoteHit {
    pub id: i64,
    pub author: String,
    pub project: Option<String>,
    pub title: Option<String>,
    pub tags: Option<String>,
    pub snippet: String,
}

pub async fn journal(pool: &PgPool, query: &str, limit: i64) -> Result<Vec<JournalHit>> {
    let rows = sqlx::query_as::<_, JournalHit>(
        "SELECT j.id, j.ai, j.entry_date, j.title, j.tags, \
                ts_headline('english', j.body, plainto_tsquery('english', $1), \
                            'StartSel=[, StopSel=], MaxFragments=1, MaxWords=20') AS snippet \
         FROM journal_entries j \
         WHERE j.fts @@ plainto_tsquery('english', $1) \
         ORDER BY ts_rank(j.fts, plainto_tsquery('english', $1)) DESC \
         LIMIT $2",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn notes(pool: &PgPool, query: &str, limit: i64) -> Result<Vec<NoteHit>> {
    let rows = sqlx::query_as::<_, NoteHit>(
        "SELECT n.id, n.author, n.project, n.title, n.tags, \
                ts_headline('english', n.body, plainto_tsquery('english', $1), \
                            'StartSel=[, StopSel=], MaxFragments=1, MaxWords=20') AS snippet \
         FROM notes n \
         WHERE n.fts @@ plainto_tsquery('english', $1) \
         ORDER BY ts_rank(n.fts, plainto_tsquery('english', $1)) DESC \
         LIMIT $2",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
