use rusqlite::{Connection, OptionalExtension, params};

use crate::enums::LinkTable;
use crate::error::{Error, Result};
use crate::types::Link;

/// Reference to a hive entity, e.g. `tasks:53` or `projects:hive-rs`.
#[derive(Debug, Clone)]
pub struct EntityRef {
    pub table: LinkTable,
    /// Tasks/journal/notes/wire/projects all key on integer `id` post-task-8.
    pub id: i64,
}

impl EntityRef {
    pub fn parse(spec: &str, label: &'static str) -> Result<Self> {
        let (table_str, ident) = spec.split_once(':').ok_or(Error::InvalidFormat {
            field: label,
            value: spec.to_string(),
            expected: "<table>:<id>",
        })?;
        if ident.is_empty() {
            return Err(Error::InvalidFormat {
                field: label,
                value: spec.to_string(),
                expected: "<table>:<id> (id missing)",
            });
        }
        let table = LinkTable::parse_short(table_str)?;
        let id = ident.parse::<i64>().map_err(|_| Error::InvalidFormat {
            field: label,
            value: ident.to_string(),
            expected: "integer id",
        })?;
        Ok(EntityRef { table, id })
    }
}

/// `<table>:<id>[:<link_type>]` ... used by --link on add commands.
#[derive(Debug, Clone)]
pub struct LinkSpec {
    pub target: EntityRef,
    pub link_type: Option<String>,
}

impl LinkSpec {
    pub fn parse(spec: &str) -> Result<Self> {
        let mut parts = spec.splitn(3, ':');
        let table_str = parts.next().ok_or(Error::InvalidFormat {
            field: "--link",
            value: spec.to_string(),
            expected: "<table>:<id>[:<link_type>]",
        })?;
        let ident = parts.next().ok_or(Error::InvalidFormat {
            field: "--link",
            value: spec.to_string(),
            expected: "<table>:<id>[:<link_type>]",
        })?;
        let link_type = parts.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
        let table = LinkTable::parse_short(table_str)?;
        if ident.is_empty() {
            return Err(Error::InvalidFormat {
                field: "--link",
                value: spec.to_string(),
                expected: "<table>:<id>[:<link_type>] (id missing)",
            });
        }
        let id = ident.parse::<i64>().map_err(|_| Error::InvalidFormat {
            field: "--link",
            value: ident.to_string(),
            expected: "integer id",
        })?;
        Ok(LinkSpec {
            target: EntityRef { table, id },
            link_type,
        })
    }
}

/// Result of an entity-label lookup ... mirrors the python `LINK_TABLES` map
/// where `(pk, label_col)` was per table.
pub fn label_for(conn: &Connection, target: &EntityRef) -> Result<Option<String>> {
    let (table, label_col) = match target.table {
        LinkTable::Tasks => ("tasks", "title"),
        LinkTable::JournalEntries => ("journal_entries", "title"),
        LinkTable::Notes => ("notes", "title"),
        LinkTable::WireEvents => ("wire_events", "title"),
        LinkTable::Projects => ("projects", "name"),
    };
    let sql = format!("SELECT {label_col} AS label FROM {table} WHERE id = ?");
    Ok(conn
        .query_row(&sql, [target.id], |r| r.get::<_, Option<String>>("label"))
        .optional()?
        .flatten())
}

pub fn require_exists(conn: &Connection, target: &EntityRef, label: &'static str) -> Result<String> {
    let title = label_for(conn, target)?.ok_or_else(|| Error::NotFound {
        kind: label,
        id: format!("{}:{}", target.table, target.id),
    })?;
    Ok(title)
}

pub fn add(
    conn: &Connection,
    source: &EntityRef,
    target: &EntityRef,
    link_type: Option<&str>,
    note: Option<&str>,
) -> Result<Option<i64>> {
    let res = conn.execute(
        "INSERT INTO links (source_table, source_id, target_table, target_id, link_type, note) \
         VALUES (?, ?, ?, ?, ?, ?)",
        params![source.table, source.id, target.table, target.id, link_type, note],
    );
    match res {
        Ok(_) => Ok(Some(conn.last_insert_rowid())),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Ok(None) // duplicate per UNIQUE constraint
        }
        Err(e) => Err(e.into()),
    }
}

pub fn outgoing(conn: &Connection, source: &EntityRef) -> Result<Vec<Link>> {
    let mut stmt = conn.prepare(
        "SELECT id, source_table, source_id, target_table, target_id, link_type, note, created_at \
         FROM links WHERE source_table = ? AND source_id = ? ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![source.table, source.id], Link::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn incoming(conn: &Connection, target: &EntityRef) -> Result<Vec<Link>> {
    let mut stmt = conn.prepare(
        "SELECT id, source_table, source_id, target_table, target_id, link_type, note, created_at \
         FROM links WHERE target_table = ? AND target_id = ? ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![target.table, target.id], Link::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn remove(conn: &Connection, id: i64) -> Result<()> {
    let n = conn.execute("DELETE FROM links WHERE id = ?", [id])?;
    if n == 0 {
        return Err(Error::NotFound {
            kind: "link",
            id: id.to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct LinkTypeCount {
    pub link_type: String,
    pub count: i64,
}

pub fn type_counts(conn: &Connection) -> Result<Vec<LinkTypeCount>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(link_type, '(none)') AS link_type, COUNT(*) AS n \
         FROM links GROUP BY link_type ORDER BY n DESC, link_type",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(LinkTypeCount {
                link_type: r.get("link_type")?,
                count: r.get("n")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Bulk-attach `link_specs` from a CLI add command to a freshly-inserted
/// source row. Mirrors python `attach_links_from_args`. Returns one message
/// per spec for the caller to print.
pub fn attach_from_specs(
    conn: &Connection,
    source: &EntityRef,
    specs: &[LinkSpec],
) -> Result<Vec<String>> {
    let mut messages = Vec::new();
    for spec in specs {
        require_exists(conn, &spec.target, "--link target")?;
        let lid = add(
            conn,
            source,
            &spec.target,
            spec.link_type.as_deref(),
            None,
        )?;
        let lt = spec.link_type.as_deref().unwrap_or("-");
        match lid {
            Some(lid) => messages.push(format!(
                "linked #{lid}: {}:{} -> {}:{} ({lt})",
                source.table, source.id, spec.target.table, spec.target.id
            )),
            None => messages.push(format!(
                "link already exists: {}:{} -> {}:{} (type={})",
                source.table,
                source.id,
                spec.target.table,
                spec.target.id,
                spec.link_type.as_deref().unwrap_or("NULL")
            )),
        }
    }
    Ok(messages)
}
