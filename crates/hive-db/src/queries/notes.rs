use rusqlite::{Connection, OptionalExtension, params};

use crate::enums::Author;
use crate::error::{Error, Result};
use crate::queries::projects;
use crate::types::Note;

#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    pub author: Option<Author>,
    pub project: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<i64>,
}

pub fn add(
    conn: &Connection,
    author: Author,
    title: Option<&str>,
    body: &str,
    project: Option<&str>,
    tags: Option<&str>,
) -> Result<Note> {
    if let Some(p) = project {
        projects::require(conn, p)?;
    }
    conn.execute(
        "INSERT INTO notes (author, title, body, tags, project) VALUES (?, ?, ?, ?, ?)",
        params![author, title, body, tags, project],
    )?;
    let id = conn.last_insert_rowid();
    Ok(get(conn, id)?.ok_or_else(|| Error::NotFound {
        kind: "note",
        id: id.to_string(),
    })?)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Note>> {
    Ok(conn
        .query_row(
            "SELECT id, author, title, body, tags, project, created_at, updated_at \
             FROM notes WHERE id = ?",
            [id],
            Note::from_row,
        )
        .optional()?)
}

pub fn require(conn: &Connection, id: i64) -> Result<Note> {
    get(conn, id)?.ok_or_else(|| Error::NotFound {
        kind: "note",
        id: id.to_string(),
    })
}

pub fn list(conn: &Connection, filters: &ListFilters) -> Result<Vec<Note>> {
    let mut sql = String::from(
        "SELECT id, author, title, body, tags, project, created_at, updated_at \
         FROM notes WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(a) = filters.author {
        sql.push_str(" AND author = ?");
        params.push(Box::new(a.as_str().to_string()));
    }
    if let Some(p) = &filters.project {
        sql.push_str(" AND project = ?");
        params.push(Box::new(p.clone()));
    }
    if let Some(t) = &filters.tag {
        sql.push_str(" AND tags LIKE ?");
        params.push(Box::new(format!("%{t}%")));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC");
    if let Some(l) = filters.limit {
        sql.push_str(" LIMIT ?");
        params.push(Box::new(l));
    }

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| &**p as &dyn rusqlite::ToSql).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(refs.iter().copied()), Note::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
