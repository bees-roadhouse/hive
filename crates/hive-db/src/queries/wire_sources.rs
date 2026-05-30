use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::enums::Severity;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WireSource {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub enabled: bool,
    pub poll_interval_secs: i32,
    pub source_tag: String,
    pub category: Option<String>,
    pub affects: Option<String>,
    pub default_severity: Option<String>,
    pub last_fetched_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const SELECT_COLS: &str = "id, name, kind, url, enabled, poll_interval_secs, source_tag, \
     category, affects, default_severity, last_fetched_at, last_error, created_at, updated_at";

pub async fn list(pool: &PgPool, enabled_only: bool) -> Result<Vec<WireSource>> {
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!("SELECT {SELECT_COLS} FROM wire_sources WHERE 1=1"));
    if enabled_only {
        qb.push(" AND enabled = true");
    }
    qb.push(" ORDER BY name");
    Ok(qb.build_query_as::<WireSource>().fetch_all(pool).await?)
}

/// Sources due for a poll: enabled and never fetched or past their interval.
pub async fn due_for_poll(pool: &PgPool) -> Result<Vec<WireSource>> {
    Ok(sqlx::query_as::<_, WireSource>(&format!(
        "SELECT {SELECT_COLS} FROM wire_sources \
         WHERE enabled \
           AND (last_fetched_at IS NULL \
                OR last_fetched_at + (poll_interval_secs * interval '1 second') <= now()) \
         ORDER BY last_fetched_at NULLS FIRST, name"
    ))
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, Clone)]
pub struct AddArgs<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub url: &'a str,
    pub poll_interval_secs: i32,
    pub source_tag: &'a str,
    pub category: Option<&'a str>,
    pub affects: Option<&'a str>,
    pub default_severity: Option<Severity>,
}

pub async fn add(pool: &PgPool, args: AddArgs<'_>) -> Result<WireSource> {
    Ok(sqlx::query_as::<_, WireSource>(&format!(
        "INSERT INTO wire_sources \
         (name, kind, url, poll_interval_secs, source_tag, category, affects, default_severity) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING {SELECT_COLS}"
    ))
    .bind(args.name)
    .bind(args.kind)
    .bind(args.url)
    .bind(args.poll_interval_secs)
    .bind(args.source_tag)
    .bind(args.category)
    .bind(args.affects)
    .bind(args.default_severity.map(|s| s.as_str().to_string()))
    .fetch_one(pool)
    .await?)
}

#[derive(Debug, Default, Clone)]
pub struct UpdateFields {
    pub url: Option<String>,
    pub enabled: Option<bool>,
    pub poll_interval_secs: Option<i32>,
    pub category: Option<Option<String>>,
    pub affects: Option<Option<String>>,
    pub default_severity: Option<Option<Severity>>,
}

pub async fn update(pool: &PgPool, id: Uuid, fields: &UpdateFields) -> Result<WireSource> {
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new("UPDATE wire_sources SET updated_at = now()");
    let mut first = true;
    let push = |qb: &mut QueryBuilder<Postgres>, first: &mut bool| {
        if *first {
            *first = false;
        } else {
            qb.push(", ");
        }
    };
    if let Some(url) = &fields.url {
        push(&mut qb, &mut first);
        qb.push("url = ").push_bind(url.clone());
    }
    if let Some(enabled) = fields.enabled {
        push(&mut qb, &mut first);
        qb.push("enabled = ").push_bind(enabled);
    }
    if let Some(secs) = fields.poll_interval_secs {
        push(&mut qb, &mut first);
        qb.push("poll_interval_secs = ").push_bind(secs);
    }
    if let Some(category) = &fields.category {
        push(&mut qb, &mut first);
        qb.push("category = ").push_bind(category.clone());
    }
    if let Some(affects) = &fields.affects {
        push(&mut qb, &mut first);
        qb.push("affects = ").push_bind(affects.clone());
    }
    if let Some(severity) = &fields.default_severity {
        push(&mut qb, &mut first);
        qb.push("default_severity = ")
            .push_bind(severity.as_ref().map(|s| s.as_str().to_string()));
    }
    if first {
        return Err(Error::InvalidFormat {
            field: "update",
            value: "(none)".into(),
            expected: "at least one field to update",
        });
    }
    qb.push(" WHERE id = ").push_bind(id);
    qb.push(format!(" RETURNING {SELECT_COLS}"));
    Ok(qb.build_query_as::<WireSource>().fetch_one(pool).await?)
}

pub async fn remove(pool: &PgPool, id: Uuid) -> Result<()> {
    let res = sqlx::query("DELETE FROM wire_sources WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound {
            kind: "wire_source",
            id: id.to_string(),
        });
    }
    Ok(())
}

pub async fn mark_fetched(pool: &PgPool, id: Uuid, error: Option<&str>) -> Result<()> {
    sqlx::query(
        "UPDATE wire_sources SET last_fetched_at = now(), last_error = $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<WireSource>> {
    Ok(sqlx::query_as::<_, WireSource>(&format!(
        "SELECT {SELECT_COLS} FROM wire_sources WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}
