// Projects (store.ts `projects`). Creates are entity.create records.

use anyhow::Result;
use hive_shared::{slugify, Project};
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

use super::{new_id, now_iso, Core, Draft, Store};

impl Store {
    pub async fn projects_list(&self) -> Result<Vec<Project>> {
        self.run(|core| {
            let mut stmt = core
                .conn()
                .prepare("SELECT * FROM projects ORDER BY name")?;
            let rows = stmt.query_map([], row_to_project)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn projects_get(&self, project_id: &str) -> Result<Option<Project>> {
        let project_id = project_id.to_string();
        self.run(move |core| project_get(core.conn(), &project_id))
            .await
    }

    pub async fn projects_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        let slug = slug.to_string();
        self.run(move |core| project_by_slug(core.conn(), &slug))
            .await
    }

    pub async fn projects_ensure(&self, name: &str) -> Result<Project> {
        let name = name.to_string();
        self.run(move |core| {
            let (p, draft) = project_ensure_plan(core, &name)?;
            if let Some(draft) = draft {
                core.commit(vec![draft])?;
            }
            Ok(p)
        })
        .await
    }
}

pub(crate) fn project_get(conn: &Connection, project_id: &str) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            "SELECT * FROM projects WHERE id = ?1",
            rusqlite::params![project_id],
            row_to_project,
        )
        .optional()?)
}

pub(crate) fn project_by_slug(conn: &Connection, slug: &str) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            "SELECT * FROM projects WHERE slug = ?1",
            rusqlite::params![slug],
            row_to_project,
        )
        .optional()?)
}

/// Find-or-create plan (see topics::topic_ensure_plan).
pub(crate) fn project_ensure_plan(core: &Core, name: &str) -> Result<(Project, Option<Draft>)> {
    let slug = slugify(name);
    if let Some(existing) = project_by_slug(core.conn(), &slug)? {
        return Ok((existing, None));
    }
    let p = Project {
        id: new_id("proj"),
        name: name.to_string(),
        slug,
        created_at: now_iso(),
    };
    let draft = Draft::new(
        crate::oplog::kind::ENTITY_CREATE,
        "system",
        &p.created_at,
        json!({"kind": "project", "id": p.id, "fields": {
            "name": p.name, "slug": p.slug, "created_at": p.created_at,
        }}),
    );
    Ok((p, Some(draft)))
}

pub(crate) fn row_to_project(r: &rusqlite::Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get("id")?,
        name: r.get("name")?,
        slug: r.get("slug")?,
        created_at: r.get("created_at")?,
    })
}
