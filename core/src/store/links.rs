// Knowledge-graph links (store.ts `links`). Creates are link.add records.

use anyhow::Result;
use hive_shared::Link;
use serde_json::json;

use super::{new_id, now_iso, Draft, Store};

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
        let l = Link {
            id: new_id("link"),
            source_kind: source_kind.to_string(),
            source_id: source_id.to_string(),
            target_kind: target_kind.to_string(),
            target_id: target_id.to_string(),
            rel: rel.to_string(),
            created_at: now_iso(),
        };
        let draft = link_add_draft(&l);
        let out = l.clone();
        self.run(move |core| {
            core.commit(vec![draft])?;
            Ok(out)
        })
        .await?;
        Ok(l)
    }

    pub async fn links_for_entity(&self, ref_id: &str) -> Result<Vec<Link>> {
        let ref_id = ref_id.to_string();
        self.run(move |core| {
            let mut stmt = core.conn().prepare(
                "SELECT * FROM links WHERE source_id = ?1 OR target_id = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(rusqlite::params![ref_id], row_to_link)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }
}

pub(crate) fn link_add_draft(l: &Link) -> Draft {
    Draft::new(
        crate::oplog::kind::LINK_ADD,
        "system",
        &l.created_at,
        json!({
            "id": l.id,
            "source_kind": l.source_kind, "source_id": l.source_id,
            "target_kind": l.target_kind, "target_id": l.target_id,
            "rel": l.rel, "created_at": l.created_at,
        }),
    )
}

/// A link.add draft straight from parts (journal emergence batches these).
pub(crate) fn link_draft(
    source_kind: &str,
    source_id: &str,
    target_kind: &str,
    target_id: &str,
    rel: &str,
    ts: &str,
) -> Draft {
    Draft::new(
        crate::oplog::kind::LINK_ADD,
        "system",
        ts,
        json!({
            "id": new_id("link"),
            "source_kind": source_kind, "source_id": source_id,
            "target_kind": target_kind, "target_id": target_id,
            "rel": rel, "created_at": ts,
        }),
    )
}

/// A link.remove draft addressing one edge by id.
pub(crate) fn link_remove_draft(id: &str, ts: &str) -> Draft {
    Draft::new(
        crate::oplog::kind::LINK_REMOVE,
        "system",
        ts,
        json!({"id": id}),
    )
}

/// Kinds pass through as strings: with user-defined entity types an
/// enum-unknown kind is a VALID row (a custom slug), not a hazard — nothing
/// mislabels now that the lossy default is gone.
pub(crate) fn row_to_link(r: &rusqlite::Row) -> rusqlite::Result<Link> {
    Ok(Link {
        id: r.get("id")?,
        source_kind: r.get("source_kind")?,
        source_id: r.get("source_id")?,
        target_kind: r.get("target_kind")?,
        target_id: r.get("target_id")?,
        rel: r.get("rel")?,
        created_at: r.get("created_at")?,
    })
}
