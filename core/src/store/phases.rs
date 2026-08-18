// Project phases (store.ts `phases`). Owned by core-stores.

use anyhow::Result;
use hive_shared::Phase;
use sqlx::Row;

use super::{new_id, now_iso, Store};

impl Store {
    pub async fn phases_list(&self, project_id: Option<&str>) -> Result<Vec<Phase>> {
        let rows = match project_id {
            Some(project) => {
                crate::pgq::query(
                    "SELECT * FROM phases WHERE project = ? ORDER BY position, created_at",
                )
                .bind(project)
                .fetch_all(self.db())
                .await?
            }
            None => {
                crate::pgq::query("SELECT * FROM phases ORDER BY project, position, created_at")
                    .fetch_all(self.db())
                    .await?
            }
        };
        rows.iter().map(row_to_phase).collect()
    }

    pub async fn phases_get(&self, phase_id: &str) -> Result<Option<Phase>> {
        let row = crate::pgq::query("SELECT * FROM phases WHERE id = ?")
            .bind(phase_id)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_phase).transpose()
    }

    pub async fn phases_ensure(&self, project_id: &str, name: &str) -> Result<Phase> {
        let existing =
            crate::pgq::query("SELECT * FROM phases WHERE project = ? AND LOWER(name) = LOWER(?)")
                .bind(project_id)
                .bind(name)
                .fetch_optional(self.db())
                .await?;
        if let Some(row) = existing {
            return row_to_phase(&row);
        }
        let pos: i64 = crate::pgq::query_scalar(
            "SELECT COALESCE(MAX(position)+1, 0) FROM phases WHERE project = ?",
        )
        .bind(project_id)
        .fetch_one(self.db())
        .await?;
        let ph = Phase {
            id: new_id("ph"),
            project: project_id.to_string(),
            name: name.to_string(),
            position: pos,
            created_at: now_iso(),
        };
        crate::pgq::query(
            "INSERT INTO phases (id, project, name, position, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&ph.id)
        .bind(&ph.project)
        .bind(&ph.name)
        .bind(ph.position)
        .bind(&ph.created_at)
        .execute(self.db())
        .await?;
        Ok(ph)
    }
}

pub(crate) fn row_to_phase(r: &sqlx::postgres::PgRow) -> Result<Phase> {
    Ok(Phase {
        id: r.try_get("id")?,
        project: r.try_get("project")?,
        name: r.try_get("name")?,
        position: r.try_get("position")?,
        created_at: r.try_get("created_at")?,
    })
}
