// Topics taxonomy (store.ts `topics`). Creates are entity.create records;
// find-or-create stays in the command layer (the fold does strict inserts).

use anyhow::Result;
use hive_shared::{slugify, Topic};
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

use super::{new_id, now_iso, Core, Draft, Store};

impl Store {
    pub async fn topics_list(&self) -> Result<Vec<Topic>> {
        self.run(|core| {
            let mut stmt = core.conn().prepare("SELECT * FROM topics ORDER BY name")?;
            let rows = stmt.query_map([], row_to_topic)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn topics_get(&self, topic_id: &str) -> Result<Option<Topic>> {
        let topic_id = topic_id.to_string();
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    "SELECT * FROM topics WHERE id = ?1",
                    rusqlite::params![topic_id],
                    row_to_topic,
                )
                .optional()?)
        })
        .await
    }

    pub async fn topics_by_slug(&self, slug: &str) -> Result<Option<Topic>> {
        let slug = slug.to_string();
        self.run(move |core| topic_by_slug(core.conn(), &slug))
            .await
    }

    pub async fn topics_ensure(&self, name: &str) -> Result<Topic> {
        let name = name.to_string();
        self.run(move |core| {
            let (topic, draft) = topic_ensure_plan(core, &name)?;
            if let Some(draft) = draft {
                core.commit(vec![draft])?;
            }
            Ok(topic)
        })
        .await
    }
}

pub(crate) fn topic_by_slug(conn: &Connection, slug: &str) -> Result<Option<Topic>> {
    Ok(conn
        .query_row(
            "SELECT * FROM topics WHERE slug = ?1",
            rusqlite::params![slug],
            row_to_topic,
        )
        .optional()?)
}

/// Find-or-create plan: the existing row, or a fresh Topic plus the
/// entity.create draft that materializes it (the caller commits — journal
/// emergence folds these into its own batch).
pub(crate) fn topic_ensure_plan(core: &Core, name: &str) -> Result<(Topic, Option<Draft>)> {
    let slug = slugify(name);
    if let Some(existing) = topic_by_slug(core.conn(), &slug)? {
        return Ok((existing, None));
    }
    let t = Topic {
        id: new_id("top"),
        name: name.to_string(),
        slug,
        created_at: now_iso(),
    };
    let draft = Draft::new(
        crate::oplog::kind::ENTITY_CREATE,
        "system",
        &t.created_at,
        json!({"kind": "topic", "id": t.id, "fields": {
            "name": t.name, "slug": t.slug, "created_at": t.created_at,
        }}),
    );
    Ok((t, Some(draft)))
}

pub(crate) fn row_to_topic(r: &rusqlite::Row) -> rusqlite::Result<Topic> {
    Ok(Topic {
        id: r.get("id")?,
        name: r.get("name")?,
        slug: r.get("slug")?,
        created_at: r.get("created_at")?,
    })
}
