// Knowledge-graph links (store.ts `links`). Owned by core-stores.

use anyhow::Result;
use hive_shared::{EntityKind, Link};
use sqlx::Row;

use super::{new_id, now_iso, Store};

impl Store {
    /// store.ts links.create — Node takes (and ignores) an actor arg; no emit.
    pub async fn links_create(
        &self,
        source_kind: EntityKind,
        source_id: &str,
        target_kind: EntityKind,
        target_id: &str,
        rel: &str,
    ) -> Result<Link> {
        let l = Link {
            id: new_id("link"),
            source_kind,
            source_id: source_id.to_string(),
            target_kind,
            target_id: target_id.to_string(),
            rel: rel.to_string(),
            created_at: now_iso(),
        };
        crate::pgq::query(
            "INSERT INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&l.id)
        .bind(l.source_kind.as_str())
        .bind(&l.source_id)
        .bind(l.target_kind.as_str())
        .bind(&l.target_id)
        .bind(&l.rel)
        .bind(&l.created_at)
        .execute(self.db())
        .await?;
        Ok(l)
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

pub(crate) fn row_to_link(r: &sqlx::postgres::PgRow) -> Result<Link> {
    Ok(Link {
        id: r.try_get("id")?,
        source_kind: EntityKind::from_str_lossy(r.try_get::<String, _>("source_kind")?.as_str()),
        source_id: r.try_get("source_id")?,
        target_kind: EntityKind::from_str_lossy(r.try_get::<String, _>("target_kind")?.as_str()),
        target_id: r.try_get("target_id")?,
        rel: r.try_get("rel")?,
        created_at: r.try_get("created_at")?,
    })
}
