use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::enums::LinkTable;
use crate::error::{Error, Result};
use crate::types::Link;

const SELECT_COLS: &str =
    "id, source_table, source_id, target_table, target_id, link_type, note, created_at";

/// Reference to a hive entity, e.g. `tasks:0190fae8-...` or
/// `projects:0190fae8-...`. Post-task-5, every PK in the hive schema is a
/// UUIDv7; references parse the second half as a uuid.
#[derive(Debug, Clone)]
pub struct EntityRef {
    pub table: LinkTable,
    pub id: Uuid,
}

impl EntityRef {
    pub fn parse(spec: &str, label: &'static str) -> Result<Self> {
        let (table_str, ident) = spec.split_once(':').ok_or(Error::InvalidFormat {
            field: label,
            value: spec.to_string(),
            expected: "<table>:<uuid>",
        })?;
        if ident.is_empty() {
            return Err(Error::InvalidFormat {
                field: label,
                value: spec.to_string(),
                expected: "<table>:<uuid> (id missing)",
            });
        }
        let table = LinkTable::parse_short(table_str)?;
        let id = Uuid::parse_str(ident).map_err(|_| Error::InvalidFormat {
            field: label,
            value: ident.to_string(),
            expected: "uuid",
        })?;
        Ok(EntityRef { table, id })
    }
}

/// `<table>:<uuid>[:<link_type>]` ... used by --link on add commands.
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
            expected: "<table>:<uuid>[:<link_type>]",
        })?;
        let ident = parts.next().ok_or(Error::InvalidFormat {
            field: "--link",
            value: spec.to_string(),
            expected: "<table>:<uuid>[:<link_type>]",
        })?;
        let link_type = parts.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
        let table = LinkTable::parse_short(table_str)?;
        if ident.is_empty() {
            return Err(Error::InvalidFormat {
                field: "--link",
                value: spec.to_string(),
                expected: "<table>:<uuid>[:<link_type>] (id missing)",
            });
        }
        let id = Uuid::parse_str(ident).map_err(|_| Error::InvalidFormat {
            field: "--link",
            value: ident.to_string(),
            expected: "uuid",
        })?;
        Ok(LinkSpec {
            target: EntityRef { table, id },
            link_type,
        })
    }
}

/// Result of an entity-label lookup ... mirrors the python `LINK_TABLES` map
/// where `(pk, label_col)` was per table.
pub async fn label_for(pool: &PgPool, target: &EntityRef) -> Result<Option<String>> {
    let (table, label_col) = match target.table {
        LinkTable::Tasks => ("tasks", "title"),
        LinkTable::JournalEntries => ("journal_entries", "title"),
        LinkTable::Notes => ("notes", "title"),
        LinkTable::WireEvents => ("wire_events", "title"),
        LinkTable::Projects => ("projects", "name"),
    };
    let sql = format!("SELECT {label_col} AS label FROM {table} WHERE id = $1");
    let row: Option<(Option<String>,)> = sqlx::query_as(&sql)
        .bind(target.id)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|(label,)| label))
}

pub async fn require_exists(
    pool: &PgPool,
    target: &EntityRef,
    label: &'static str,
) -> Result<String> {
    let title = label_for(pool, target).await?.ok_or_else(|| Error::NotFound {
        kind: label,
        id: format!("{}:{}", target.table, target.id),
    })?;
    Ok(title)
}

pub async fn add(
    pool: &PgPool,
    source: &EntityRef,
    target: &EntityRef,
    link_type: Option<&str>,
    note: Option<&str>,
) -> Result<Option<Uuid>> {
    let res = sqlx::query_as::<_, (Uuid,)>(
        "INSERT INTO links (source_table, source_id, target_table, target_id, link_type, note) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(source.table.as_str())
    .bind(source.id)
    .bind(target.table.as_str())
    .bind(target.id)
    .bind(link_type)
    .bind(note)
    .fetch_one(pool)
    .await;

    match res {
        Ok((id,)) => Ok(Some(id)),
        Err(e) => {
            let err: Error = e.into();
            if err.is_unique_violation() {
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

pub async fn outgoing(pool: &PgPool, source: &EntityRef) -> Result<Vec<Link>> {
    let rows = sqlx::query_as::<_, Link>(&format!(
        "SELECT {SELECT_COLS} \
         FROM links WHERE source_table = $1 AND source_id = $2 ORDER BY id"
    ))
    .bind(source.table.as_str())
    .bind(source.id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn incoming(pool: &PgPool, target: &EntityRef) -> Result<Vec<Link>> {
    let rows = sqlx::query_as::<_, Link>(&format!(
        "SELECT {SELECT_COLS} \
         FROM links WHERE target_table = $1 AND target_id = $2 ORDER BY id"
    ))
    .bind(target.table.as_str())
    .bind(target.id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn remove(pool: &PgPool, id: Uuid) -> Result<()> {
    let res = sqlx::query("DELETE FROM links WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound {
            kind: "link",
            id: id.to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LinkTypeCount {
    pub link_type: String,
    pub count: i64,
}

pub async fn type_counts(pool: &PgPool) -> Result<Vec<LinkTypeCount>> {
    let rows = sqlx::query_as::<_, LinkTypeCount>(
        "SELECT COALESCE(link_type, '(none)') AS link_type, COUNT(*) AS count \
         FROM links GROUP BY link_type ORDER BY count DESC, link_type",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Bulk-attach `link_specs` from a CLI add command to a freshly-inserted
/// source row. Mirrors python `attach_links_from_args`. Returns one message
/// per spec for the caller to print.
pub async fn attach_from_specs(
    pool: &PgPool,
    source: &EntityRef,
    specs: &[LinkSpec],
) -> Result<Vec<String>> {
    let mut messages = Vec::new();
    for spec in specs {
        require_exists(pool, &spec.target, "--link target").await?;
        let lid = add(pool, source, &spec.target, spec.link_type.as_deref(), None).await?;
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
