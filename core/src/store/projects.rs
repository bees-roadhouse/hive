// Projects (store.ts `projects`).

use anyhow::Result;
use hive_shared::{slugify, Project};
use sqlx::{PgConnection, Row};

use super::{new_id, now_iso, Store};

impl Store {
    pub async fn projects_list(&self) -> Result<Vec<Project>> {
        let rows = crate::pgq::query("SELECT * FROM projects ORDER BY name")
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_project).collect()
    }

    pub async fn projects_get(&self, project_id: &str) -> Result<Option<Project>> {
        let mut conn = self.db().acquire().await?;
        projects_get_conn(&mut conn, project_id).await
    }

    pub async fn projects_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        let mut conn = self.db().acquire().await?;
        projects_by_slug_conn(&mut conn, slug).await
    }

    pub async fn projects_ensure(&self, name: &str) -> Result<Project> {
        let mut conn = self.db().acquire().await?;
        projects_ensure_conn(&mut conn, name).await
    }
}

pub(crate) async fn projects_get_conn(
    conn: &mut PgConnection,
    project_id: &str,
) -> Result<Option<Project>> {
    let row = crate::pgq::query("SELECT * FROM projects WHERE id = ?")
        .bind(project_id)
        .fetch_optional(&mut *conn)
        .await?;
    row.as_ref().map(row_to_project).transpose()
}

pub(crate) async fn projects_by_slug_conn(
    conn: &mut PgConnection,
    slug: &str,
) -> Result<Option<Project>> {
    let row = crate::pgq::query("SELECT * FROM projects WHERE slug = ?")
        .bind(slug)
        .fetch_optional(&mut *conn)
        .await?;
    row.as_ref().map(row_to_project).transpose()
}

/// Connection-level variant so project emergence can ride a caller's
/// transaction (the journal write path).
pub(crate) async fn projects_ensure_conn(conn: &mut PgConnection, name: &str) -> Result<Project> {
    let slug = slugify(name);
    if let Some(existing) = projects_by_slug_conn(conn, &slug).await? {
        return Ok(existing);
    }
    let p = Project {
        id: new_id("proj"),
        name: name.to_string(),
        slug,
        created_at: now_iso(),
    };
    crate::pgq::query("INSERT INTO projects (id, name, slug, created_at) VALUES (?, ?, ?, ?)")
        .bind(&p.id)
        .bind(&p.name)
        .bind(&p.slug)
        .bind(&p.created_at)
        .execute(&mut *conn)
        .await?;
    Ok(p)
}

pub(crate) fn row_to_project(r: &sqlx::postgres::PgRow) -> Result<Project> {
    Ok(Project {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        slug: r.try_get("slug")?,
        created_at: r.try_get("created_at")?,
    })
}
