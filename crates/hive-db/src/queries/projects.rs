use sqlx::PgPool;

use crate::enums::{Owner, ProjectStatus};
use crate::error::{Error, Result};
use crate::types::Project;

const SELECT_COLS: &str = "id, name, description, status, owner, created_at, updated_at";

pub async fn add(
    pool: &PgPool,
    name: &str,
    description: Option<&str>,
    owner: Owner,
) -> Result<Project> {
    let res = sqlx::query_as::<_, Project>(
        "INSERT INTO projects (name, description, owner) \
         VALUES ($1, $2, $3) \
         RETURNING id, name, description, status, owner, created_at, updated_at",
    )
    .bind(name)
    .bind(description)
    .bind(owner.as_str())
    .fetch_one(pool)
    .await;

    match res {
        Ok(p) => Ok(p),
        Err(e) => {
            let err: Error = e.into();
            if err.is_unique_violation() {
                Err(Error::AlreadyExists(format!("project '{name}'")))
            } else {
                Err(err)
            }
        }
    }
}

pub async fn get(pool: &PgPool, name: &str) -> Result<Option<Project>> {
    Ok(sqlx::query_as::<_, Project>(&format!(
        "SELECT {SELECT_COLS} FROM projects WHERE name = $1"
    ))
    .bind(name)
    .fetch_optional(pool)
    .await?)
}

pub async fn require(pool: &PgPool, name: &str) -> Result<Project> {
    get(pool, name).await?.ok_or_else(|| Error::NotFound {
        kind: "project",
        id: name.to_string(),
    })
}

pub async fn list(pool: &PgPool, status: Option<ProjectStatus>) -> Result<Vec<Project>> {
    let rows = match status {
        Some(s) => {
            sqlx::query_as::<_, Project>(&format!(
                "SELECT {SELECT_COLS} FROM projects WHERE status = $1 ORDER BY status, name"
            ))
            .bind(s.as_str())
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as::<_, Project>(&format!(
                "SELECT {SELECT_COLS} FROM projects ORDER BY status, name"
            ))
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows)
}

pub async fn archive(pool: &PgPool, name: &str) -> Result<()> {
    let res =
        sqlx::query("UPDATE projects SET status = 'archived', updated_at = now() WHERE name = $1")
            .bind(name)
            .execute(pool)
            .await?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound {
            kind: "project",
            id: name.to_string(),
        });
    }
    Ok(())
}
