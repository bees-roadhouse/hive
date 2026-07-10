// FTS content-table maintenance. For the fold-owned kinds (journal, task,
// decision, event, custom instances) the fold maintains `search` from records
// — nothing here to do. These helpers remain for MAIL rows, whose FTS
// membership is command-layer policy (ingest mailboxes, junk) per the 1.5
// decision: direct writes to `search` (the FTS5 shadow follows via triggers).
// Mail search rows are therefore NOT rebuilt by log replay — they return with
// the Phase 3 mail module's resync.

use anyhow::Result;
use rusqlite::Connection;

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
        let (kind, ref_id, title, body) = (
            kind.to_string(),
            ref_id.to_string(),
            title.to_string(),
            body.to_string(),
        );
        let tags = tags.to_vec();
        self.run(move |core| index_entity_conn(core.conn(), &kind, &ref_id, &title, &body, &tags))
            .await
    }

    /// Drop the FTS row for an entity (deletion must leave search immediately).
    pub async fn unindex_entity(&self, kind: &str, ref_id: &str) -> Result<()> {
        let (kind, ref_id) = (kind.to_string(), ref_id.to_string());
        self.run(move |core| {
            core.conn().execute(
                "DELETE FROM search WHERE kind = ?1 AND ref_id = ?2",
                rusqlite::params![kind, ref_id],
            )?;
            Ok(())
        })
        .await
    }
}

/// Connection-level variant so mail ingest can index inside its own pass.
pub(crate) fn index_entity_conn(
    conn: &Connection,
    kind: &str,
    ref_id: &str,
    title: &str,
    body: &str,
    tags: &[String],
) -> Result<()> {
    conn.execute(
        "DELETE FROM search WHERE kind = ?1 AND ref_id = ?2",
        rusqlite::params![kind, ref_id],
    )?;
    conn.execute(
        "INSERT INTO search (kind, ref_id, title, body) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![kind, ref_id, title, format!("{body} {}", tags.join(" "))],
    )?;
    Ok(())
}
