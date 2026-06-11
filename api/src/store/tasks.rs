// Tasks list/get/update + anchor emergence (store.ts `tasks`).
// Owned by the core-stores workstream.

use anyhow::Result;
use hive_shared::{Priority, Task, TaskPatch, TaskStatus};
use serde_json::json;
use sqlx::Row;

use super::{json_vec, new_id, now_iso, to_json, Store};

/// Inputs for the internal creation path (store.ts `tasks.create` input shape).
/// Tasks only ever emerge from journal anchors / bracket tokens.
#[derive(Debug, Clone)]
pub struct TaskCreate {
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub assignees: Vec<String>,
    pub project: Option<String>,
    pub phase: Option<String>,
    pub due: Option<String>,
    pub origin_entry_id: Option<String>,
    pub anchor_text: Option<String>,
}

impl Default for TaskCreate {
    fn default() -> Self {
        Self {
            title: String::new(),
            body: String::new(),
            status: TaskStatus::Todo,
            priority: Priority::Normal,
            tags: Vec::new(),
            assignees: Vec::new(),
            project: None,
            phase: None,
            due: None,
            origin_entry_id: None,
            anchor_text: None,
        }
    }
}

/// store.ts tasks.list filter — falsy (absent) filters are skipped.
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub project: Option<String>,
    pub phase: Option<String>,
}

impl Store {
    /// Priority-then-recency sort, filters applied after the fetch (as Node does).
    pub async fn tasks_list(&self, filter: TaskFilter) -> Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT * FROM tasks ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
        )
        .fetch_all(self.db())
        .await?;
        let tasks: Vec<Task> = rows.iter().map(row_to_task).collect::<Result<Vec<_>>>()?;
        Ok(tasks
            .into_iter()
            .filter(|t| {
                filter
                    .status
                    .as_deref()
                    .is_none_or(|s| t.status.as_str() == s)
            })
            .filter(|t| {
                filter
                    .project
                    .as_deref()
                    .is_none_or(|p| t.project.as_deref() == Some(p))
            })
            .filter(|t| {
                filter
                    .phase
                    .as_deref()
                    .is_none_or(|p| t.phase.as_deref() == Some(p))
            })
            .filter(|t| {
                filter
                    .assignee
                    .as_deref()
                    .is_none_or(|a| t.assignees.iter().any(|x| x == a))
            })
            .collect())
    }

    pub async fn tasks_get(&self, task_id: &str) -> Result<Option<Task>> {
        let row = sqlx::query("SELECT * FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_task).transpose()
    }

    pub async fn tasks_create(&self, input: TaskCreate, actor: &str) -> Result<Task> {
        // Only ensure-by-name when the project value is not already a known project id.
        if let Some(project) = &input.project {
            if self.projects_get(project).await?.is_none() {
                self.projects_ensure(project).await?;
            }
        }
        let ts = now_iso();
        let t = Task {
            id: new_id("task"),
            title: input.title,
            body: input.body,
            status: input.status,
            priority: input.priority,
            tags: input.tags,
            assignees: input.assignees,
            project: input.project,
            phase: input.phase,
            due: input.due,
            origin_entry_id: input.origin_entry_id,
            anchor_text: input.anchor_text,
            created_at: ts.clone(),
            updated_at: ts,
        };
        sqlx::query(
            "INSERT INTO tasks (id, project, phase, due, title, body, status, priority, tags, assignees, origin_entry_id, anchor_text, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&t.id)
        .bind(&t.project)
        .bind(&t.phase)
        .bind(&t.due)
        .bind(&t.title)
        .bind(&t.body)
        .bind(t.status.as_str())
        .bind(t.priority.as_str())
        .bind(to_json(&t.tags))
        .bind(to_json(&t.assignees))
        .bind(&t.origin_entry_id)
        .bind(&t.anchor_text)
        .bind(&t.created_at)
        .bind(&t.updated_at)
        .execute(self.db())
        .await?;
        self.index_entity("task", &t.id, &t.title, &t.body, &t.tags)
            .await?;
        self.emit("task.created", actor, json!({"id": t.id, "title": t.title}))
            .await?;
        Ok(t)
    }

    pub async fn tasks_update(
        &self,
        task_id: &str,
        patch: TaskPatch,
        actor: &str,
    ) -> Result<Option<Task>> {
        let Some(current) = self.tasks_get(task_id).await? else {
            return Ok(None);
        };
        let next = Task {
            title: patch.title.unwrap_or(current.title),
            body: patch.body.unwrap_or(current.body),
            status: patch.status.unwrap_or(current.status),
            priority: patch.priority.unwrap_or(current.priority),
            tags: patch.tags.unwrap_or(current.tags),
            assignees: patch.assignees.unwrap_or(current.assignees),
            updated_at: now_iso(),
            ..current
        };
        sqlx::query(
            "UPDATE tasks SET title=?, body=?, status=?, priority=?, tags=?, assignees=?, updated_at=? WHERE id=?",
        )
        .bind(&next.title)
        .bind(&next.body)
        .bind(next.status.as_str())
        .bind(next.priority.as_str())
        .bind(to_json(&next.tags))
        .bind(to_json(&next.assignees))
        .bind(&next.updated_at)
        .bind(&next.id)
        .execute(self.db())
        .await?;
        self.index_entity("task", &next.id, &next.title, &next.body, &next.tags)
            .await?;
        self.emit(
            "task.updated",
            actor,
            json!({"id": next.id, "status": next.status.as_str()}),
        )
        .await?;
        Ok(Some(next))
    }
}

pub(crate) fn row_to_task(r: &sqlx::sqlite::SqliteRow) -> Result<Task> {
    Ok(Task {
        id: r.try_get("id")?,
        title: r.try_get("title")?,
        body: r.try_get("body")?,
        status: TaskStatus::from_str_lossy(r.try_get::<String, _>("status")?.as_str()),
        priority: Priority::from_str_lossy(r.try_get::<String, _>("priority")?.as_str()),
        tags: json_vec(r.try_get::<String, _>("tags")?.as_str()),
        assignees: json_vec(r.try_get::<String, _>("assignees")?.as_str()),
        project: r.try_get("project")?,
        phase: r.try_get("phase")?,
        due: r.try_get("due")?,
        origin_entry_id: r.try_get("origin_entry_id")?,
        anchor_text: r.try_get("anchor_text")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}
