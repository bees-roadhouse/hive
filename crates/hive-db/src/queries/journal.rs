use rusqlite::{Connection, OptionalExtension, params};

use crate::enums::Ai;
use crate::error::{Error, Result};
use crate::types::JournalEntry;

#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    pub ai: Option<Ai>,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
}

pub fn add(
    conn: &Connection,
    ai: Ai,
    entry_date: &str,
    title: Option<&str>,
    body: &str,
    tags: Option<&str>,
) -> Result<JournalEntry> {
    conn.execute(
        "INSERT INTO journal_entries (ai, entry_date, title, body, tags) VALUES (?, ?, ?, ?, ?)",
        params![ai, entry_date, title, body, tags],
    )?;
    let id = conn.last_insert_rowid();
    Ok(get(conn, id)?.ok_or_else(|| Error::NotFound {
        kind: "journal_entry",
        id: id.to_string(),
    })?)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<JournalEntry>> {
    Ok(conn
        .query_row(
            "SELECT id, ai, entry_date, title, body, tags, created_at, updated_at \
             FROM journal_entries WHERE id = ?",
            [id],
            JournalEntry::from_row,
        )
        .optional()?)
}

pub fn require(conn: &Connection, id: i64) -> Result<JournalEntry> {
    get(conn, id)?.ok_or_else(|| Error::NotFound {
        kind: "journal_entry",
        id: id.to_string(),
    })
}

pub fn list(conn: &Connection, filters: &ListFilters) -> Result<Vec<JournalEntry>> {
    let mut sql = String::from(
        "SELECT id, ai, entry_date, title, body, tags, created_at, updated_at \
         FROM journal_entries WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(a) = filters.ai {
        sql.push_str(" AND ai = ?");
        params.push(Box::new(a.as_str().to_string()));
    }
    if let Some(d) = &filters.from_date {
        sql.push_str(" AND entry_date >= ?");
        params.push(Box::new(d.clone()));
    }
    if let Some(d) = &filters.to_date {
        sql.push_str(" AND entry_date <= ?");
        params.push(Box::new(d.clone()));
    }
    if let Some(t) = &filters.tag {
        sql.push_str(" AND tags LIKE ?");
        params.push(Box::new(format!("%{t}%")));
    }
    sql.push_str(" ORDER BY entry_date DESC, id DESC");
    if let Some(l) = filters.limit {
        sql.push_str(" LIMIT ?");
        params.push(Box::new(l));
    }

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| &**p as &dyn rusqlite::ToSql).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(refs.iter().copied()), JournalEntry::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
