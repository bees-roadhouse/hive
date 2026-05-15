use rusqlite::{Connection, OptionalExtension, params};

use crate::enums::Severity;
use crate::error::{Error, Result};
use crate::types::WireEvent;

#[derive(Debug, Default, Clone)]
pub struct ListFilters {
    pub source: Option<String>,
    pub severity: Option<Severity>,
    pub unacknowledged: bool,
    pub limit: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct AddArgs<'a> {
    pub source: &'a str,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub external_id: Option<&'a str>,
    pub severity: Option<Severity>,
    pub affects: Option<&'a str>,
    pub url: Option<&'a str>,
    pub category: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub enum AddResult {
    Added(WireEvent),
    AlreadySeen { id: i64 },
}

pub fn add(conn: &Connection, args: AddArgs<'_>) -> Result<AddResult> {
    let res = conn.execute(
        "INSERT INTO wire_events (source, category, external_id, title, body, url, severity, affects) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            args.source,
            args.category,
            args.external_id,
            args.title,
            args.body,
            args.url,
            args.severity.map(|s| s.as_str().to_string()),
            args.affects,
        ],
    );
    match res {
        Ok(_) => {
            let id = conn.last_insert_rowid();
            let event = get(conn, id)?.ok_or_else(|| Error::NotFound {
                kind: "wire_event",
                id: id.to_string(),
            })?;
            Ok(AddResult::Added(event))
        }
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation && args.external_id.is_some() =>
        {
            let row = conn
                .query_row(
                    "SELECT id FROM wire_events WHERE external_id = ?",
                    [args.external_id],
                    |r| r.get::<_, i64>("id"),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound {
                    kind: "wire_event",
                    id: args.external_id.unwrap_or("").to_string(),
                })?;
            conn.execute(
                "UPDATE wire_events SET last_seen_at = datetime('now') WHERE id = ?",
                [row],
            )?;
            Ok(AddResult::AlreadySeen { id: row })
        }
        Err(e) => Err(e.into()),
    }
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<WireEvent>> {
    Ok(conn
        .query_row(
            "SELECT id, source, category, external_id, title, body, url, severity, affects, \
                    acknowledged, pinged_discord, first_seen_at, last_seen_at \
             FROM wire_events WHERE id = ?",
            [id],
            WireEvent::from_row,
        )
        .optional()?)
}

pub fn list(conn: &Connection, filters: &ListFilters) -> Result<Vec<WireEvent>> {
    let mut sql = String::from(
        "SELECT id, source, category, external_id, title, body, url, severity, affects, \
                acknowledged, pinged_discord, first_seen_at, last_seen_at \
         FROM wire_events WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(s) = &filters.source {
        sql.push_str(" AND source = ?");
        params.push(Box::new(s.clone()));
    }
    if let Some(s) = filters.severity {
        sql.push_str(" AND severity = ?");
        params.push(Box::new(s.as_str().to_string()));
    }
    if filters.unacknowledged {
        sql.push_str(" AND acknowledged = 0");
    }
    sql.push_str(" ORDER BY last_seen_at DESC, id DESC");
    if let Some(l) = filters.limit {
        sql.push_str(" LIMIT ?");
        params.push(Box::new(l));
    }

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| &**p as &dyn rusqlite::ToSql).collect();
    let rows = stmt
        .query_map(rusqlite::params_from_iter(refs.iter().copied()), WireEvent::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn ack(conn: &Connection, id: i64) -> Result<()> {
    let n = conn.execute(
        "UPDATE wire_events SET acknowledged = 1 WHERE id = ?",
        [id],
    )?;
    if n == 0 {
        return Err(Error::NotFound {
            kind: "wire_event",
            id: id.to_string(),
        });
    }
    Ok(())
}
