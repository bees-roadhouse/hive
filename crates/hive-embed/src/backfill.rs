//! Backfill embeddings for existing rows in journal_entries / notes / tasks.
//!
//! For each scoped table, walk rows, compute `sha256(title + body)`, and
//! skip when the stored content_hash matches. Otherwise embed + upsert via
//! `store_embedding`. Returns the number of rows actually re-embedded.

use anyhow::{Context, Result};
use hive_db::PgPool;
use sqlx::Row;
use uuid::Uuid;

use crate::{Embedder, content_hash, store_embedding};

/// Which source tables to backfill.
#[derive(Debug, Clone, Copy)]
pub enum BackfillScope {
    Journal,
    Tasks,
    Notes,
    All,
}

impl BackfillScope {
    fn tables(&self) -> &'static [&'static str] {
        match self {
            BackfillScope::Journal => &["journal_entries"],
            BackfillScope::Notes => &["notes"],
            BackfillScope::Tasks => &["tasks"],
            BackfillScope::All => &["journal_entries", "notes", "tasks"],
        }
    }
}

/// Per-row payload used to compute content_hash + the text to embed.
struct SourceRow {
    id: Uuid,
    text: String,
}

async fn fetch_rows(pool: &PgPool, table: &str) -> Result<Vec<SourceRow>> {
    let sql = match table {
        "journal_entries" | "notes" | "tasks" => format!(
            "SELECT id, COALESCE(title, '') AS title, COALESCE(body, '') AS body FROM {table}"
        ),
        other => anyhow::bail!("unsupported backfill table: {other}"),
    };
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    let out = rows
        .into_iter()
        .map(|r| {
            let id: Uuid = r.try_get("id")?;
            let title: String = r.try_get("title")?;
            let body: String = r.try_get("body")?;
            let text = if title.is_empty() {
                body
            } else if body.is_empty() {
                title
            } else {
                format!("{title}\n{body}")
            };
            Ok::<SourceRow, sqlx::Error>(SourceRow { id, text })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(out)
}

/// Iterate the in-scope tables, embed rows that are missing or stale,
/// upsert via `store_embedding`. Returns the count of rows actually
/// (re-)embedded. Skips rows with empty text.
pub async fn backfill_embeddings(
    pool: &PgPool,
    embedder: &Embedder,
    scope: BackfillScope,
) -> Result<usize> {
    let model = embedder.model_id;
    let mut total = 0usize;

    for table in scope.tables() {
        let rows = fetch_rows(pool, table)
            .await
            .with_context(|| format!("fetch rows for backfill: {table}"))?;

        // Pull existing (id -> hash) map once per table to avoid N+1 lookups.
        let existing = hive_db::queries::embeddings::existing_index(pool, table, model)
            .await
            .with_context(|| format!("existing_index for {table}"))?;

        // Build the work list first so we can batch-embed.
        let mut pending: Vec<(Uuid, String, String)> = Vec::new(); // (id, text, hash)
        for r in rows {
            if r.text.is_empty() {
                continue;
            }
            let h = content_hash(&r.text);
            match existing.get(&r.id) {
                Some(existing_h) if existing_h == &h => continue,
                _ => pending.push((r.id, r.text, h)),
            }
        }
        if pending.is_empty() {
            continue;
        }
        // Batch embed.
        let texts: Vec<&str> = pending.iter().map(|(_, t, _)| t.as_str()).collect();
        let vectors = embedder
            .embed_batch(&texts)
            .with_context(|| format!("embed_batch failed for table {table}"))?;
        if vectors.len() != pending.len() {
            anyhow::bail!(
                "embedder returned {} vectors for {} inputs ({table})",
                vectors.len(),
                pending.len()
            );
        }
        for ((id, _text, hash), vec) in pending.into_iter().zip(vectors) {
            store_embedding(pool, table, id, model, &vec, &hash)
                .await
                .with_context(|| format!("store_embedding {table}:{id}"))?;
            total += 1;
        }
    }
    Ok(total)
}
