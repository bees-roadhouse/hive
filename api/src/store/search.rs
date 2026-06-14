// FTS5 index maintenance (store.ts indexEntity). The query side (keyword
// search with viewer ACL + the semantic engine) lives in semantic.rs and is
// owned by the search parity workstream.

use anyhow::Result;
use sqlx::PgConnection;

use super::Store;

impl Store {
    /// Replace the FTS row for an entity (delete + insert; body gets tags appended).
    pub async fn index_entity(
        &self,
        kind: &str,
        ref_id: &str,
        title: &str,
        body: &str,
        tags: &[String],
    ) -> Result<()> {
        let mut conn = self.db().acquire().await?;
        index_entity_conn(&mut conn, kind, ref_id, title, body, tags).await
    }
}

/// Connection-level variant so indexing can run inside a transaction
/// (`&mut *tx`) as the Node code does.
pub async fn index_entity_conn(
    conn: &mut PgConnection,
    kind: &str,
    ref_id: &str,
    title: &str,
    body: &str,
    tags: &[String],
) -> Result<()> {
    crate::pgq::query("DELETE FROM search WHERE kind = ? AND ref_id = ?")
        .bind(kind)
        .bind(ref_id)
        .execute(&mut *conn)
        .await?;
    crate::pgq::query("INSERT INTO search (kind, ref_id, title, body) VALUES (?, ?, ?, ?)")
        .bind(kind)
        .bind(ref_id)
        .bind(title)
        .bind(format!("{body} {}", tags.join(" ")))
        .execute(&mut *conn)
        .await?;
    Ok(())
}
