//! Embedding storage and retrieval.
//!
//! The `hive-embed` crate owns the model client (encoding, reranking).
//! This module owns the postgres IO: read existing rows, write embeddings,
//! list staleness. Embeddings are stored as `pgvector::Vector` against the
//! `vector` column type.

use std::collections::HashMap;

use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::error::Result;

pub const VALID_SOURCE_TABLES: &[&str] = &["journal_entries", "notes"];

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SourceRow {
    pub id: Uuid,
    pub title: Option<String>,
    pub body: Option<String>,
    pub tags: Option<String>,
}

pub async fn fetch_source_rows(pool: &PgPool, table: &str) -> Result<Vec<SourceRow>> {
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
    let rows = sqlx::query_as::<_, SourceRow>(sql).fetch_all(pool).await?;
    Ok(rows)
}

pub async fn fetch_one_source_row(
    pool: &PgPool,
    table: &str,
    source_id: Uuid,
) -> Result<Option<SourceRow>> {
    let sql = match table {
        "journal_entries" => "SELECT id, title, body, tags FROM journal_entries WHERE id = $1",
        "notes" => "SELECT id, title, body, tags FROM notes WHERE id = $1",
        other => {
            return Err(crate::error::Error::InvalidEnum {
                field: "source_table",
                value: other.to_string(),
                valid: VALID_SOURCE_TABLES.join(", "),
            });
        }
    };
    let row = sqlx::query_as::<_, SourceRow>(sql)
        .bind(source_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Map of `source_id -> content_hash` for an (table, model) pair. Used to
/// determine which rows need (re-)embedding.
pub async fn existing_index(
    pool: &PgPool,
    table: &str,
    model: &str,
) -> Result<HashMap<Uuid, String>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT source_id, content_hash FROM embeddings WHERE source_table = $1 AND model = $2",
    )
    .bind(table)
    .bind(model)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusCount {
    pub table: String,
    pub total: i64,
    pub embedded: i64,
}

pub async fn status(pool: &PgPool, model: &str) -> Result<Vec<StatusCount>> {
    let mut out = Vec::new();
    for &table in VALID_SOURCE_TABLES {
        let total: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await?;
        let embedded: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM embeddings WHERE source_table = $1 AND model = $2",
        )
        .bind(table)
        .bind(model)
        .fetch_one(pool)
        .await?;
        out.push(StatusCount {
            table: table.to_string(),
            total: total.0,
            embedded: embedded.0,
        });
    }
    Ok(out)
}

/// Insert or update an embedding row. `embedding` is the raw f32 slice the
/// callers already produce via fastembed; we wrap it in `pgvector::Vector`
/// to send it across the wire.
pub async fn upsert(
    pool: &PgPool,
    table: &str,
    source_id: Uuid,
    model: &str,
    dim: i32,
    embedding: &[f32],
    content_hash: &str,
) -> Result<()> {
    let vec = Vector::from(embedding.to_vec());
    sqlx::query(
        "INSERT INTO embeddings (source_table, source_id, model, dim, embedding, content_hash) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (source_table, source_id, model) DO UPDATE SET \
           embedding = EXCLUDED.embedding, \
           dim = EXCLUDED.dim, \
           content_hash = EXCLUDED.content_hash, \
           created_at = now()",
    )
    .bind(table)
    .bind(source_id)
    .bind(model)
    .bind(dim)
    .bind(vec)
    .bind(content_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Load every embedding for a (table, model) pair as `(source_ids, vectors)`.
/// Returns the embeddings as raw `Vec<f32>` so existing call sites that worked
/// against the old sqlite blob format only need a one-line change (skip the
/// `bytemuck::cast_slice` step).
pub async fn load_all(
    pool: &PgPool,
    table: &str,
    model: &str,
) -> Result<(Vec<Uuid>, Vec<Vec<f32>>)> {
    let rows: Vec<(Uuid, Vector)> = sqlx::query_as(
        "SELECT source_id, embedding FROM embeddings WHERE source_table = $1 AND model = $2",
    )
    .bind(table)
    .bind(model)
    .fetch_all(pool)
    .await?;
    let mut ids = Vec::with_capacity(rows.len());
    let mut vecs = Vec::with_capacity(rows.len());
    for (sid, v) in rows {
        ids.push(sid);
        vecs.push(v.to_vec());
    }
    Ok((ids, vecs))
}
