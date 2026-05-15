//! Row types matching the post-task-8 schema (see SCHEMA.md).

use rusqlite::Row;
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub owner: String,
    pub created_at: String,
    pub updated_at: String,
}

impl Project {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Project {
            id: row.get("id")?,
            name: row.get("name")?,
            description: row.get("description")?,
            status: row.get("status")?,
            owner: row.get("owner")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub body: Option<String>,
    pub owner: String,
    pub status: String,
    pub priority: Option<String>,
    pub due: Option<String>,
    pub block_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

impl Task {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Task {
            id: row.get("id")?,
            project: row.get("project")?,
            title: row.get("title")?,
            body: row.get("body")?,
            owner: row.get("owner")?,
            status: row.get("status")?,
            priority: row.get("priority")?,
            due: row.get("due")?,
            block_reason: row.get("block_reason")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            closed_at: row.get("closed_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: i64,
    pub ai: String,
    pub entry_date: String,
    pub title: Option<String>,
    pub body: String,
    pub tags: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl JournalEntry {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(JournalEntry {
            id: row.get("id")?,
            ai: row.get("ai")?,
            entry_date: row.get("entry_date")?,
            title: row.get("title")?,
            body: row.get("body")?,
            tags: row.get("tags")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: i64,
    pub author: String,
    pub title: Option<String>,
    pub body: String,
    pub tags: Option<String>,
    pub project: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Note {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Note {
            id: row.get("id")?,
            author: row.get("author")?,
            title: row.get("title")?,
            body: row.get("body")?,
            tags: row.get("tags")?,
            project: row.get("project")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEvent {
    pub id: i64,
    pub source: String,
    pub category: Option<String>,
    pub external_id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub url: Option<String>,
    pub severity: Option<String>,
    pub affects: Option<String>,
    pub acknowledged: bool,
    pub pinged_discord: bool,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

impl WireEvent {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let ack: i64 = row.get("acknowledged")?;
        let pinged: i64 = row.get("pinged_discord")?;
        Ok(WireEvent {
            id: row.get("id")?,
            source: row.get("source")?,
            category: row.get("category")?,
            external_id: row.get("external_id")?,
            title: row.get("title")?,
            body: row.get("body")?,
            url: row.get("url")?,
            severity: row.get("severity")?,
            affects: row.get("affects")?,
            acknowledged: ack != 0,
            pinged_discord: pinged != 0,
            first_seen_at: row.get("first_seen_at")?,
            last_seen_at: row.get("last_seen_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: i64,
    pub source_table: String,
    pub source_id: i64,
    pub target_table: String,
    pub target_id: i64,
    pub link_type: Option<String>,
    pub note: Option<String>,
    pub created_at: String,
}

impl Link {
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Link {
            id: row.get("id")?,
            source_table: row.get("source_table")?,
            source_id: row.get("source_id")?,
            target_table: row.get("target_table")?,
            target_id: row.get("target_id")?,
            link_type: row.get("link_type")?,
            note: row.get("note")?,
            created_at: row.get("created_at")?,
        })
    }
}

/// Compose the canonical text used for embedding hashes (see python
/// `hive_semantic.compose_text`). Stable across the rust port.
pub fn compose_embed_text(
    title: Option<&str>,
    body: Option<&str>,
    tags: Option<&str>,
) -> String {
    let mut pieces = Vec::with_capacity(3);
    if let Some(t) = title.map(str::trim).filter(|s| !s.is_empty()) {
        pieces.push(t.to_string());
    }
    if let Some(b) = body.map(str::trim).filter(|s| !s.is_empty()) {
        pieces.push(b.to_string());
    }
    if let Some(t) = tags.map(str::trim).filter(|s| !s.is_empty()) {
        pieces.push(format!("tags: {t}"));
    }
    pieces.join("\n\n")
}

/// SHA-256 of `title || body || tags` joined with `||`. Matches python
/// `hive_semantic.content_hash` byte-for-byte.
pub fn content_hash(title: Option<&str>, body: Option<&str>, tags: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let parts = format!(
        "{}||{}||{}",
        title.unwrap_or(""),
        body.unwrap_or(""),
        tags.unwrap_or("")
    );
    let digest = Sha256::digest(parts.as_bytes());
    hex::encode(digest)
}

/// Split a comma-separated tag string into trimmed tags. Empty input → empty.
pub fn split_tags(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[allow(dead_code)]
fn _ensure_result_used() -> Result<()> {
    Ok(())
}
