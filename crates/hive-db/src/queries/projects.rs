use rusqlite::{Connection, OptionalExtension, params};

use crate::enums::{Owner, ProjectStatus};
use crate::error::{Error, Result};
use crate::types::Project;

pub fn add(
    conn: &Connection,
    name: &str,
    description: Option<&str>,
    owner: Owner,
) -> Result<Project> {
    let res = conn.execute(
        "INSERT INTO projects (name, description, owner) VALUES (?, ?, ?)",
        params![name, description, owner],
    );
    match res {
        Ok(_) => Ok(get(conn, name)?
            .ok_or_else(|| Error::NotFound { kind: "project", id: name.to_string() })?),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(Error::AlreadyExists(format!("project '{name}'")))
        }
        Err(e) => Err(e.into()),
    }
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<Project>> {
    Ok(conn
        .query_row(
            "SELECT id, name, description, status, owner, created_at, updated_at \
             FROM projects WHERE name = ?",
            [name],
            Project::from_row,
        )
        .optional()?)
}

pub fn require(conn: &Connection, name: &str) -> Result<Project> {
    get(conn, name)?.ok_or_else(|| Error::NotFound {
        kind: "project",
        id: name.to_string(),
    })
}

pub fn list(conn: &Connection, status: Option<ProjectStatus>) -> Result<Vec<Project>> {
    let (sql, params): (&str, Vec<&dyn rusqlite::ToSql>) = match status {
        Some(ref s) => (
            "SELECT id, name, description, status, owner, created_at, updated_at \
             FROM projects WHERE status = ? ORDER BY status, name",
            vec![s as &dyn rusqlite::ToSql],
        ),
        None => (
            "SELECT id, name, description, status, owner, created_at, updated_at \
             FROM projects ORDER BY status, name",
            vec![],
        ),
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter().copied()), Project::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn archive(conn: &Connection, name: &str) -> Result<()> {
    let n = conn.execute(
        "UPDATE projects SET status = 'archived', updated_at = datetime('now') WHERE name = ?",
        [name],
    )?;
    if n == 0 {
        return Err(Error::NotFound {
            kind: "project",
            id: name.to_string(),
        });
    }
    Ok(())
}
