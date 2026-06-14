// Projects (store.ts `projects`).

use anyhow::Result;
use hive_shared::{slugify, Project};
use sqlx::Row;

use super::{new_id, now_iso, Store};

impl Store {
    pub async fn projects_list(&self) -> Result<Vec<Project>> {
        let rows = sqlx::query("SELECT * FROM projects ORDER BY name")
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_project).collect()
    }

    pub async fn projects_get(&self, project_id: &str) -> Result<Option<Project>> {
        let row = sqlx::query("SELECT * FROM projects WHERE id = ?")
            .bind(project_id)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_project).transpose()
    }

    pub async fn projects_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        let row = sqlx::query("SELECT * FROM projects WHERE slug = ?")
            .bind(slug)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_project).transpose()
    }

    pub async fn projects_ensure(&self, name: &str) -> Result<Project> {
        let slug = slugify(name);
        if let Some(existing) = self.projects_by_slug(&slug).await? {
            return Ok(existing);
        }
        let p = Project {
            id: new_id("proj"),
            name: name.to_string(),
            slug,
            created_at: now_iso(),
        };
        sqlx::query("INSERT INTO projects (id, name, slug, created_at) VALUES (?, ?, ?, ?)")
            .bind(&p.id)
            .bind(&p.name)
            .bind(&p.slug)
            .bind(&p.created_at)
            .execute(self.db())
            .await?;
        Ok(p)
    }
}

pub(crate) fn row_to_project(r: &sqlx::sqlite::SqliteRow) -> Result<Project> {
    Ok(Project {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        slug: r.try_get("slug")?,
        created_at: r.try_get("created_at")?,
    })
}
