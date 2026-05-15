//! Embedding storage and retrieval.
//!
//! The `hive-embed` crate owns the model client (encoding, reranking).
//! This module owns the sqlite IO: read existing rows, write embeddings,
//! list staleness.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;

pub const VALID_SOURCE_TABLES: &[&str] = &["journal_entries", "notes"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRow {
    pub id: i64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub tags: Option<String>,
}

pub fn fetch_source_rows(conn: &Connection, table: &str) -> Result<Vec<SourceRow>> {
    let sql = match table {
        "journal_entries" => "SELECT id, title, body, tags FROM journal_entries",
        "notes" => "SELECT id, title, body, tags FROM notes",
        other => {
            return Err(crate::error::Error::InvalidEnum {
                field: "source_table",
                value: other.to_string(),
                valid: VALID_SOURCE_TABLES.join(", "),
            });
        }
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(SourceRow {
                id: r.get("id")?,
                title: r.get("title")?,
                body: r.get("body")?,
                tags: r.get("tags")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn fetch_one_source_row(
    conn: &Connection,
    table: &str,
    source_id: i64,
) -> Result<Option<SourceRow>> {
    let sql = match table {
        "journal_entries" => "SELECT id, title, body, tags FROM journal_entries WHERE id = ?",
        "notes" => "SELECT id, title, body, tags FROM notes WHERE id = ?",
        other => {
            return Err(crate::error::Error::InvalidEnum {
                field: "source_table",
                value: other.to_string(),
                valid: VALID_SOURCE_TABLES.join(", "),
            });
        }
    };
    Ok(conn
        .query_row(sql, [source_id], |r| {
            Ok(SourceRow {
                id: r.get("id")?,
                title: r.get("title")?,
                body: r.get("body")?,
                tags: r.get("tags")?,
            })
        })
        .optional()?)
}

/// Map of `source_id -> content_hash` for an (table, model) pair. Used to
/// determine which rows need (re-)embedding.
pub fn existing_index(
    conn: &Connection,
    table: &str,
    model: &str,
) -> Result<HashMap<i64, String>> {
    let mut stmt = conn.prepare(
        "SELECT source_id, content_hash FROM embeddings WHERE source_table = ? AND model = ?",
    )?;
    let mut map = HashMap::new();
    for row in stmt.query_map(params![table, model], |r| {
        Ok((r.get::<_, i64>("source_id")?, r.get::<_, String>("content_hash")?))
    })? {
        let (sid, ch) = row?;
        map.insert(sid, ch);
    }
    Ok(map)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusCount {
    pub table: String,
    pub total: i64,
    pub embedded: i64,
}

pub fn status(conn: &Connection, model: &str) -> Result<Vec<StatusCount>> {
    let mut out = Vec::new();
    for &table in VALID_SOURCE_TABLES {
        let total: i64 =
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
        let embedded: i64 = conn.query_row(
            "SELECT COUNT(*) FROM embeddings WHERE source_table = ? AND model = ?",
            params![table, model],
            |r| r.get(0),
        )?;
        out.push(StatusCount {
            table: table.to_string(),
            total,
            embedded,
        });
    }
    Ok(out)
}

pub fn upsert(
    conn: &Connection,
    table: &str,
    source_id: i64,
    model: &str,
    dim: i64,
    embedding: &[u8],
    content_hash: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO embeddings (source_table, source_id, model, dim, embedding, content_hash) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT (source_table, source_id, model) DO UPDATE SET \
           embedding = excluded.embedding, \
           dim = excluded.dim, \
           content_hash = excluded.content_hash, \
           created_at = datetime('now')",
        params![table, source_id, model, dim, embedding, content_hash],
    )?;
    Ok(())
}

/// Load every embedding for a (table, model) pair as `(source_ids, raw blobs)`.
/// Callers convert the blobs to f32 vectors themselves (typically via
/// `bytemuck::cast_slice`).
pub fn load_all(
    conn: &Connection,
    table: &str,
    model: &str,
) -> Result<(Vec<i64>, Vec<Vec<u8>>)> {
    let mut stmt = conn.prepare(
        "SELECT source_id, embedding FROM embeddings WHERE source_table = ? AND model = ?",
    )?;
    let mut ids = Vec::new();
    let mut blobs = Vec::new();
    for row in stmt.query_map(params![table, model], |r| {
        Ok((r.get::<_, i64>("source_id")?, r.get::<_, Vec<u8>>("embedding")?))
    })? {
        let (sid, blob) = row?;
        ids.push(sid);
        blobs.push(blob);
    }
    Ok((ids, blobs))
}
