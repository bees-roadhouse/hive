// Project phases (store.ts `phases`). Creates are entity.create records.

use anyhow::Result;
use hive_shared::Phase;
use rusqlite::OptionalExtension;
use serde_json::json;

use super::{new_id, now_iso, Core, Draft, Store};

impl Store {
    pub async fn phases_list(&self, project_id: Option<&str>) -> Result<Vec<Phase>> {
        let project_id = project_id.map(str::to_string);
        self.run(move |core| {
            let conn = core.conn();
            let rows = match &project_id {
                Some(project) => {
                    let mut stmt = conn.prepare(
                        "SELECT * FROM phases WHERE project = ?1 ORDER BY position, created_at",
                    )?;
                    let rows = stmt.query_map(rusqlite::params![project], row_to_phase)?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                }
                None => {
                    let mut stmt = conn
                        .prepare("SELECT * FROM phases ORDER BY project, position, created_at")?;
                    let rows = stmt.query_map([], row_to_phase)?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                }
            };
            Ok(rows)
        })
        .await
    }

    pub async fn phases_get(&self, phase_id: &str) -> Result<Option<Phase>> {
        let phase_id = phase_id.to_string();
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    "SELECT * FROM phases WHERE id = ?1",
                    rusqlite::params![phase_id],
                    row_to_phase,
                )
                .optional()?)
        })
        .await
    }

    pub async fn phases_ensure(&self, project_id: &str, name: &str) -> Result<Phase> {
        let (project_id, name) = (project_id.to_string(), name.to_string());
        self.run(move |core| {
            let (ph, draft) = phase_ensure_plan(core, &project_id, &name)?;
            if let Some(draft) = draft {
                core.commit(vec![draft])?;
            }
            Ok(ph)
        })
        .await
    }
}

/// Find-or-create plan (case-insensitive name match within the project).
pub(crate) fn phase_ensure_plan(
    core: &Core,
    project_id: &str,
    name: &str,
) -> Result<(Phase, Option<Draft>)> {
    let existing = core
        .conn()
        .query_row(
            "SELECT * FROM phases WHERE project = ?1 AND LOWER(name) = LOWER(?2)",
            rusqlite::params![project_id, name],
            row_to_phase,
        )
        .optional()?;
    if let Some(ph) = existing {
        return Ok((ph, None));
    }
    let pos: i64 = core.conn().query_row(
        "SELECT COALESCE(MAX(position)+1, 0) FROM phases WHERE project = ?1",
        rusqlite::params![project_id],
        |r| r.get(0),
    )?;
    let ph = Phase {
        id: new_id("ph"),
        project: project_id.to_string(),
        name: name.to_string(),
        position: pos,
        created_at: now_iso(),
    };
    let draft = Draft::new(
        crate::oplog::kind::ENTITY_CREATE,
        "system",
        &ph.created_at,
        json!({"kind": "phase", "id": ph.id, "fields": {
            "project": ph.project, "name": ph.name, "position": ph.position,
            "created_at": ph.created_at,
        }}),
    );
    Ok((ph, Some(draft)))
}

pub(crate) fn row_to_phase(r: &rusqlite::Row) -> rusqlite::Result<Phase> {
    Ok(Phase {
        id: r.get("id")?,
        project: r.get("project")?,
        name: r.get("name")?,
        position: r.get("position")?,
        created_at: r.get("created_at")?,
    })
}
