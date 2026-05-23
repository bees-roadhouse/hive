//! Row types matching the postgres schema (see `migrations/0001_initial.sql`).
//!
//! Timestamps are `chrono::DateTime<Utc>` (TIMESTAMPTZ). `entry_date` and
//! `due` stay TEXT (YYYY-MM-DD) because the python CLI writes them in that
//! shape and we want byte-stable cross-tool reads.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub status: String,
    pub owner: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Task {
    pub id: i64,
    pub project: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub owner: String,
    pub status: String,
    pub priority: Option<String>,
    pub due: Option<String>,
    pub block_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct JournalEntry {
    pub id: i64,
    pub ai: String,
    pub entry_date: String,
    pub title: Option<String>,
    pub body: String,
    pub tags: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Note {
    pub id: i64,
    pub author: String,
    pub title: Option<String>,
    pub body: String,
    pub tags: Option<String>,
    pub project: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
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
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TaskAnchor {
    pub task_id: i64,
    pub journal_entry_id: i64,
    pub block_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Person {
    pub id: i64,
    pub slug: String,
    pub display_name: String,
    /// `ai` or `human` (CHECK-constrained at the schema level).
    pub kind: String,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Link {
    pub id: i64,
    pub source_table: String,
    pub source_id: i64,
    pub target_table: String,
    pub target_id: i64,
    pub link_type: Option<String>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
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
