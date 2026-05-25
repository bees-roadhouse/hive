use rusqlite::{Connection, OptionalExtension, params};

use crate::enums::{Owner, TaskStatus};
use crate::error::{Error, Result};
use crate::queries::projects;
use crate::types::Task;

#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    pub project: Option<String>,
    pub owner: Option<Owner>,
    pub status: Option<TaskStatus>,
    /// When `status` is None and `all` is false, restrict to active statuses
    /// (open + in_progress) to mirror the python default.
    pub all: bool,
}

#[derive(Debug, Default, Clone)]
pub struct UpdateFields {
    pub status: Option<TaskStatus>,
    pub priority: Option<Option<String>>, // outer Option = "field present", inner = NULL clearing
    pub owner: Option<Owner>,
    pub due: Option<Option<String>>,
    pub body: Option<Option<String>>,
    pub title: Option<String>,
    pub block_reason: Option<Option<String>>,
}

impl UpdateFields {
    pub fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.priority.is_none()
            && self.owner.is_none()
            && self.due.is_none()
            && self.body.is_none()
            && self.title.is_none()
            && self.block_reason.is_none()
    }
}

pub fn add(
    conn: &Connection,
    project: &str,
    title: &str,
    body: Option<&str>,
    owner: Owner,
    priority: Option<&str>,
    due: Option<&str>,
) -> Result<Task> {
    projects::require(conn, project)?;
    conn.execute(
        "INSERT INTO tasks (project, title, body, owner, priority, due) VALUES (?, ?, ?, ?, ?, ?)",
        params![project, title, body, owner, priority, due],
    )?;
    let id = conn.last_insert_rowid();
    Ok(get(conn, id)?.ok_or_else(|| Error::NotFound {
        kind: "task",
        id: id.to_string(),
    })?)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Task>> {
    Ok(conn
        .query_row(
            "SELECT id, project, title, body, owner, status, priority, due, block_reason, \
                    created_at, updated_at, closed_at \
             FROM tasks WHERE id = ?",
            [id],
            Task::from_row,
        )
        .optional()?)
}

pub fn require(conn: &Connection, id: i64) -> Result<Task> {
    get(conn, id)?.ok_or_else(|| Error::NotFound {
        kind: "task",
        id: id.to_string(),
    })
}

pub fn list(conn: &Connection, filters: &ListFilters) -> Result<Vec<Task>> {
    let mut sql = String::from(
        "SELECT id, project, title, body, owner, status, priority, due, block_reason, \
                created_at, updated_at, closed_at FROM tasks WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(p) = &filters.project {
        sql.push_str(" AND project = ?");
        params.push(Box::new(p.clone()));
    }
    if let Some(o) = filters.owner {
        sql.push_str(" AND owner = ?");
        params.push(Box::new(o.as_str().to_string()));
    }
    if let Some(s) = filters.status {
        sql.push_str(" AND status = ?");
        params.push(Box::new(s.as_str().to_string()));
    } else if !filters.all {
        sql.push_str(" AND status IN ('in_progress', 'open')");
    }
    sql.push_str(" ORDER BY project, status, COALESCE(due, '9999-99-99'), id");

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| &**p as &dyn rusqlite::ToSql).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(refs.iter().copied()), Task::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn update(conn: &Connection, id: i64, fields: &UpdateFields) -> Result<()> {
    if fields.is_empty() {
        return Err(Error::InvalidFormat {
            field: "update",
            value: "(none)".into(),
            expected: "at least one of --status / --priority / --owner / --due / --body / --title",
        });
    }
    let existing = require(conn, id)?;

    let mut sets: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(s) = fields.status {
        sets.push("status = ?".into());
        params.push(Box::new(s.as_str().to_string()));
    }
    if let Some(p) = &fields.priority {
        sets.push("priority = ?".into());
        params.push(Box::new(p.clone()));
    }
    if let Some(o) = fields.owner {
        sets.push("owner = ?".into());
        params.push(Box::new(o.as_str().to_string()));
    }
    if let Some(d) = &fields.due {
        sets.push("due = ?".into());
        params.push(Box::new(d.clone()));
    }
    if let Some(b) = &fields.body {
        sets.push("body = ?".into());
        params.push(Box::new(b.clone()));
    }
    if let Some(t) = &fields.title {
        sets.push("title = ?".into());
        params.push(Box::new(t.clone()));
    }
    if let Some(r) = &fields.block_reason {
        sets.push("block_reason = ?".into());
        params.push(Box::new(r.clone()));
    }
    sets.push("updated_at = datetime('now')".into());

    if let Some(new_status) = fields.status {
        let was_closed = matches!(existing.status.as_str(), "done" | "dropped");
        let now_closed = new_status.is_closed();
        if now_closed && !was_closed {
            sets.push("closed_at = datetime('now')".into());
        } else if !now_closed && was_closed {
            sets.push("closed_at = NULL".into());
        }
    }

    let sql = format!("UPDATE tasks SET {} WHERE id = ?", sets.join(", "));
    params.push(Box::new(id));
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| &**p as &dyn rusqlite::ToSql).collect();
    conn.execute(&sql, rusqlite::params_from_iter(refs.iter().copied()))?;
    Ok(())
}

pub fn mark_done(conn: &Connection, id: i64) -> Result<()> {
    update(
        conn,
        id,
        &UpdateFields {
            status: Some(TaskStatus::Done),
            ..Default::default()
        },
    )
}

pub fn mark_dropped(conn: &Connection, id: i64) -> Result<()> {
    update(
        conn,
        id,
        &UpdateFields {
            status: Some(TaskStatus::Dropped),
            ..Default::default()
        },
    )
}

pub fn mark_blocked(conn: &Connection, id: i64, reason: &str) -> Result<()> {
    update(
        conn,
        id,
        &UpdateFields {
            status: Some(TaskStatus::Blocked),
            block_reason: Some(Some(reason.to_string())),
            ..Default::default()
        },
    )
}
