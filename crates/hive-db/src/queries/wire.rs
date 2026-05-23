use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::enums::Severity;
use crate::error::{Error, Result};
use crate::types::WireEvent;

const SELECT_COLS: &str =
    "id, source, category, external_id, title, body, url, severity, affects, \
     acknowledged, pinged_discord, first_seen_at, last_seen_at";

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
    AlreadySeen { id: Uuid },
}

pub async fn add(pool: &PgPool, args: AddArgs<'_>) -> Result<AddResult> {
    let res = sqlx::query_as::<_, WireEvent>(
        "INSERT INTO wire_events (source, category, external_id, title, body, url, severity, affects) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING id, source, category, external_id, title, body, url, severity, affects, \
                   acknowledged, pinged_discord, first_seen_at, last_seen_at",
    )
    .bind(args.source)
    .bind(args.category)
    .bind(args.external_id)
    .bind(args.title)
    .bind(args.body)
    .bind(args.url)
    .bind(args.severity.map(|s| s.as_str().to_string()))
    .bind(args.affects)
    .fetch_one(pool)
    .await;

    match res {
        Ok(event) => Ok(AddResult::Added(event)),
        Err(e) => {
            let err: Error = e.into();
            if err.is_unique_violation() && args.external_id.is_some() {
                // Re-emit AlreadySeen with the existing row's id; bump last_seen_at.
                let ext = args.external_id.unwrap();
                let row: Option<(Uuid,)> =
                    sqlx::query_as("SELECT id FROM wire_events WHERE external_id = $1")
                        .bind(ext)
                        .fetch_optional(pool)
                        .await?;
                let id = row
                    .ok_or_else(|| Error::NotFound {
                        kind: "wire_event",
                        id: ext.to_string(),
                    })?
                    .0;
                sqlx::query("UPDATE wire_events SET last_seen_at = now() WHERE id = $1")
                    .bind(id)
                    .execute(pool)
                    .await?;
                Ok(AddResult::AlreadySeen { id })
            } else {
                Err(err)
            }
        }
    }
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<WireEvent>> {
    Ok(sqlx::query_as::<_, WireEvent>(&format!(
        "SELECT {SELECT_COLS} FROM wire_events WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn list(pool: &PgPool, filters: &ListFilters) -> Result<Vec<WireEvent>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT {SELECT_COLS} FROM wire_events WHERE 1=1"
    ));

    if let Some(s) = &filters.source {
        qb.push(" AND source = ").push_bind(s.clone());
    }
    if let Some(s) = filters.severity {
        qb.push(" AND severity = ").push_bind(s.as_str().to_string());
    }
    if filters.unacknowledged {
        qb.push(" AND acknowledged = false");
    }
    qb.push(" ORDER BY last_seen_at DESC, id DESC");
    if let Some(l) = filters.limit {
        qb.push(" LIMIT ").push_bind(l);
    }

    let rows = qb.build_query_as::<WireEvent>().fetch_all(pool).await?;
    Ok(rows)
}

pub async fn ack(pool: &PgPool, id: Uuid) -> Result<()> {
    let res = sqlx::query("UPDATE wire_events SET acknowledged = true WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound {
            kind: "wire_event",
            id: id.to_string(),
        });
    }
    Ok(())
}
