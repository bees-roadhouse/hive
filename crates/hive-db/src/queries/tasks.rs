use sqlx::{PgPool, QueryBuilder, Postgres};

use crate::enums::{Owner, TaskStatus};
use crate::error::{Error, Result};
use crate::queries::projects;
use crate::types::Task;

const SELECT_COLS: &str =
    "id, project, title, body, owner, status, priority, due, block_reason, \
     created_at, updated_at, closed_at";

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

pub async fn add(
    pool: &PgPool,
    project: &str,
    title: &str,
    body: Option<&str>,
    owner: Owner,
    priority: Option<&str>,
    due: Option<&str>,
) -> Result<Task> {
    projects::require(pool, project).await?;
    let task = sqlx::query_as::<_, Task>(
        "INSERT INTO tasks (project, title, body, owner, priority, due) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         RETURNING id, project, title, body, owner, status, priority, due, block_reason, \
                   created_at, updated_at, closed_at",
    )
    .bind(project)
    .bind(title)
    .bind(body)
    .bind(owner.as_str())
    .bind(priority)
    .bind(due)
    .fetch_one(pool)
    .await?;
    Ok(task)
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<Task>> {
    Ok(sqlx::query_as::<_, Task>(&format!(
        "SELECT {SELECT_COLS} FROM tasks WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn require(pool: &PgPool, id: i64) -> Result<Task> {
    get(pool, id).await?.ok_or_else(|| Error::NotFound {
        kind: "task",
        id: id.to_string(),
    })
}

pub async fn list(pool: &PgPool, filters: &ListFilters) -> Result<Vec<Task>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT {SELECT_COLS} FROM tasks WHERE 1=1"
    ));

    if let Some(p) = &filters.project {
        qb.push(" AND project = ").push_bind(p.clone());
    }
    if let Some(o) = filters.owner {
        qb.push(" AND owner = ").push_bind(o.as_str().to_string());
    }
    if let Some(s) = filters.status {
        qb.push(" AND status = ").push_bind(s.as_str().to_string());
    } else if !filters.all {
        qb.push(" AND status IN ('in_progress', 'open')");
    }
    qb.push(" ORDER BY project, status, COALESCE(due, '9999-99-99'), id");

    let rows = qb.build_query_as::<Task>().fetch_all(pool).await?;
    Ok(rows)
}

pub async fn update(pool: &PgPool, id: i64, fields: &UpdateFields) -> Result<()> {
    if fields.is_empty() {
        return Err(Error::InvalidFormat {
            field: "update",
            value: "(none)".into(),
            expected: "at least one of --status / --priority / --owner / --due / --body / --title",
        });
    }
    let existing = require(pool, id).await?;

    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE tasks SET ");
    let mut first = true;
    let mut push_set = |qb: &mut QueryBuilder<Postgres>, first: &mut bool| {
        if *first {
            *first = false;
        } else {
            qb.push(", ");
        }
    };

    if let Some(s) = fields.status {
        push_set(&mut qb, &mut first);
        qb.push("status = ").push_bind(s.as_str().to_string());
    }
    if let Some(p) = &fields.priority {
        push_set(&mut qb, &mut first);
        qb.push("priority = ").push_bind(p.clone());
    }
    if let Some(o) = fields.owner {
        push_set(&mut qb, &mut first);
        qb.push("owner = ").push_bind(o.as_str().to_string());
    }
    if let Some(d) = &fields.due {
        push_set(&mut qb, &mut first);
        qb.push("due = ").push_bind(d.clone());
    }
    if let Some(b) = &fields.body {
        push_set(&mut qb, &mut first);
        qb.push("body = ").push_bind(b.clone());
    }
    if let Some(t) = &fields.title {
        push_set(&mut qb, &mut first);
        qb.push("title = ").push_bind(t.clone());
    }
    if let Some(r) = &fields.block_reason {
        push_set(&mut qb, &mut first);
        qb.push("block_reason = ").push_bind(r.clone());
    }
    push_set(&mut qb, &mut first);
    qb.push("updated_at = now()");

    if let Some(new_status) = fields.status {
        let was_closed = matches!(existing.status.as_str(), "done" | "dropped");
        let now_closed = new_status.is_closed();
        if now_closed && !was_closed {
            qb.push(", closed_at = now()");
        } else if !now_closed && was_closed {
            qb.push(", closed_at = NULL");
        }
    }

    qb.push(" WHERE id = ").push_bind(id);
    qb.build().execute(pool).await?;
    Ok(())
}

pub async fn mark_done(pool: &PgPool, id: i64) -> Result<()> {
    update(
        pool,
        id,
        &UpdateFields {
            status: Some(TaskStatus::Done),
            ..Default::default()
        },
    )
    .await
}

pub async fn mark_dropped(pool: &PgPool, id: i64) -> Result<()> {
    update(
        pool,
        id,
        &UpdateFields {
            status: Some(TaskStatus::Dropped),
            ..Default::default()
        },
    )
    .await
}

pub async fn mark_blocked(pool: &PgPool, id: i64, reason: &str) -> Result<()> {
    update(
        pool,
        id,
        &UpdateFields {
            status: Some(TaskStatus::Blocked),
            block_reason: Some(Some(reason.to_string())),
            ..Default::default()
        },
    )
    .await
}
