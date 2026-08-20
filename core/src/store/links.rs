// Knowledge-graph links (store.ts `links`). Owned by core-stores.

use anyhow::Result;
use hive_shared::Link;
use sqlx::{PgConnection, Row};

use super::{new_id, now_iso, Store};

impl Store {
    /// store.ts links.create — Node takes (and ignores) an actor arg; no emit.
    pub async fn links_create(
        &self,
        source_kind: &str,
        source_id: &str,
        target_kind: &str,
        target_id: &str,
        rel: &str,
    ) -> Result<Link> {
        let mut conn = self.db().acquire().await?;
        links_create_conn(
            &mut conn,
            source_kind,
            source_id,
            target_kind,
            target_id,
            rel,
        )
        .await
    }

    pub async fn links_for_entity(&self, ref_id: &str) -> Result<Vec<Link>> {
        let rows = crate::pgq::query(
            "SELECT * FROM links WHERE source_id = ? OR target_id = ? ORDER BY created_at DESC",
        )
        .bind(ref_id)
        .bind(ref_id)
        .fetch_all(self.db())
        .await?;
        rows.iter().map(row_to_link).collect()
    }
}

/// Connection-level variant so link creation can ride a caller's transaction
/// (the journal write path). Never emits — Node's links.create doesn't either.
pub(crate) async fn links_create_conn(
    conn: &mut PgConnection,
    source_kind: &str,
    source_id: &str,
    target_kind: &str,
    target_id: &str,
    rel: &str,
) -> Result<Link> {
    let l = Link {
        id: new_id("link"),
        source_kind: source_kind.to_string(),
        source_id: source_id.to_string(),
        target_kind: target_kind.to_string(),
        target_id: target_id.to_string(),
        rel: rel.to_string(),
        created_at: now_iso(),
    };
    crate::pgq::query(
        "INSERT INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&l.id)
    .bind(&l.source_kind)
    .bind(&l.source_id)
    .bind(&l.target_kind)
    .bind(&l.target_id)
    .bind(&l.rel)
    .bind(&l.created_at)
    .execute(&mut *conn)
    .await?;
    Ok(l)
}

/// Kinds pass through as strings: with user-defined entity types an
/// enum-unknown kind is a VALID row (a custom slug), not a hazard — nothing
/// mislabels now that the lossy default is gone.
pub(crate) fn row_to_link(r: &sqlx::postgres::PgRow) -> Result<Link> {
    Ok(Link {
        id: r.try_get("id")?,
        source_kind: r.try_get("source_kind")?,
        source_id: r.try_get("source_id")?,
        target_kind: r.try_get("target_kind")?,
        target_id: r.try_get("target_id")?,
        rel: r.try_get("rel")?,
        created_at: r.try_get("created_at")?,
    })
}
