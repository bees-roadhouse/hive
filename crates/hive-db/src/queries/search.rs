//! FTS5 search over journal_entries and notes. Mirrors python
//! `cmd_journal_search` / `cmd_notes_search` / `cmd_search` (non-hybrid path).
//! Hybrid search lives in the `hive-embed` crate.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalHit {
    pub id: i64,
    pub ai: String,
    pub entry_date: String,
    pub title: Option<String>,
    pub tags: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteHit {
    pub id: i64,
    pub author: String,
    pub project: Option<String>,
    pub title: Option<String>,
    pub tags: Option<String>,
    pub snippet: String,
}

pub fn journal(conn: &Connection, query: &str, limit: i64) -> Result<Vec<JournalHit>> {
    let mut stmt = conn.prepare(
        "SELECT j.id, j.ai, j.entry_date, j.title, j.tags, \
                snippet(journal_fts, 1, '[', ']', '...', 12) AS snip \
         FROM journal_fts \
         JOIN journal_entries j ON j.id = journal_fts.rowid \
         WHERE journal_fts MATCH ? \
         ORDER BY rank \
         LIMIT ?",
    )?;
    let rows = stmt
        .query_map(params![query, limit], |r| {
            Ok(JournalHit {
                id: r.get("id")?,
                ai: r.get("ai")?,
                entry_date: r.get("entry_date")?,
                title: r.get("title")?,
                tags: r.get("tags")?,
                snippet: r.get("snip")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn notes(conn: &Connection, query: &str, limit: i64) -> Result<Vec<NoteHit>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.author, n.project, n.title, n.tags, \
                snippet(notes_fts, 1, '[', ']', '...', 12) AS snip \
         FROM notes_fts \
         JOIN notes n ON n.id = notes_fts.rowid \
         WHERE notes_fts MATCH ? \
         ORDER BY rank \
         LIMIT ?",
    )?;
    let rows = stmt
        .query_map(params![query, limit], |r| {
            Ok(NoteHit {
                id: r.get("id")?,
                author: r.get("author")?,
                project: r.get("project")?,
                title: r.get("title")?,
                tags: r.get("tags")?,
                snippet: r.get("snip")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
