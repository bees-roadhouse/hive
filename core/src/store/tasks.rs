// Tasks list/get/update + anchor emergence (store.ts `tasks`). Creates and
// updates are entity.create/entity.update records (the fold maintains FTS).

use anyhow::Result;
use hive_shared::{Priority, Task, TaskPatch, TaskStatus};
use rusqlite::OptionalExtension;
use serde_json::json;

use super::{json_vec, new_id, now_iso, to_json, Core, Draft, Store};

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
        let tasks: Vec<Task> = self
            .run(|core| {
                let mut stmt = core.conn().prepare(
                    "SELECT * FROM tasks ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
                )?;
                let rows = stmt.query_map([], row_to_task)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
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
        let task_id = task_id.to_string();
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    "SELECT * FROM tasks WHERE id = ?1",
                    rusqlite::params![task_id],
                    row_to_task,
                )
                .optional()?)
        })
        .await
    }

    pub async fn tasks_create(&self, input: TaskCreate, actor: &str) -> Result<Task> {
        let actor_s = actor.to_string();
        let t = self
            .run(move |core| {
                let (t, drafts) = task_create_plan(core, input, &actor_s)?;
                core.commit(drafts)?;
                Ok(t)
            })
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
        let task_id_s = task_id.to_string();
        let actor_s = actor.to_string();
        let next = self
            .run(move |core| {
                let Some(current) = core
                    .conn()
                    .query_row(
                        "SELECT * FROM tasks WHERE id = ?1",
                        rusqlite::params![task_id_s],
                        row_to_task,
                    )
                    .optional()?
                else {
                    return Ok(None);
                };
                let next = Task {
                    title: patch.title.unwrap_or(current.title),
                    body: patch.body.unwrap_or(current.body),
                    status: patch.status.unwrap_or(current.status),
                    priority: patch.priority.unwrap_or(current.priority),
                    tags: patch.tags.unwrap_or(current.tags),
                    assignees: patch.assignees.unwrap_or(current.assignees),
                    // Double Option (like EventPatch::at): absent keeps, null
                    // clears to NULL, value sets. The fold's bind_value maps a
                    // JSON null to SQL NULL, so an explicit clear round-trips.
                    project: match &patch.project {
                        Some(v) => v.clone(),
                        None => current.project,
                    },
                    due: match &patch.due {
                        Some(v) => v.clone(),
                        None => current.due,
                    },
                    updated_at: now_iso(),
                    ..current
                };
                // Emit only the fields this update touches. `project`/`due` are
                // included ONLY when the patch specifies them (Some(_)) — so a
                // task whose list/due wasn't edited produces the byte-identical
                // record it always did. A specified clear emits JSON null →
                // SQL NULL via the generic entity_update → update_row path.
                let mut fields = json!({
                    "title": next.title, "body": next.body,
                    "status": next.status.as_str(), "priority": next.priority.as_str(),
                    "tags": to_json(&next.tags), "assignees": to_json(&next.assignees),
                    "updated_at": next.updated_at,
                });
                let map = fields.as_object_mut().expect("fields is an object");
                if patch.project.is_some() {
                    map.insert("project".into(), json!(next.project));
                }
                if patch.due.is_some() {
                    map.insert("due".into(), json!(next.due));
                }
                core.commit(vec![Draft::new(
                    crate::oplog::kind::ENTITY_UPDATE,
                    &actor_s,
                    &next.updated_at,
                    json!({"kind": "task", "id": next.id, "fields": fields}),
                )])?;
                Ok(Some(next))
            })
            .await?;
        let Some(next) = next else { return Ok(None) };
        self.emit(
            "task.updated",
            actor,
            json!({"id": next.id, "status": next.status.as_str()}),
        )
        .await?;
        Ok(Some(next))
    }
}

/// The entity.create payload for one task (also the `emerged` element shape
/// journal.append pre-materializes).
pub(crate) fn task_create_payload(t: &Task) -> serde_json::Value {
    json!({"kind": "task", "id": t.id, "fields": {
        "project": t.project, "phase": t.phase, "due": t.due,
        "title": t.title, "body": t.body,
        "status": t.status.as_str(), "priority": t.priority.as_str(),
        "tags": to_json(&t.tags), "assignees": to_json(&t.assignees),
        "origin_entry_id": t.origin_entry_id, "anchor_text": t.anchor_text,
        "created_at": t.created_at, "updated_at": t.updated_at,
    }})
}

/// Build the Task + its record drafts (project ensure-by-name first when the
/// value isn't a known project id — matching the Postgres path).
pub(crate) fn task_create_plan(
    core: &Core,
    input: TaskCreate,
    actor: &str,
) -> Result<(Task, Vec<Draft>)> {
    let mut drafts: Vec<Draft> = Vec::new();
    if let Some(project) = &input.project {
        if super::projects::project_get(core.conn(), project)?.is_none() {
            let (_p, draft) = super::projects::project_ensure_plan(core, project)?;
            if let Some(d) = draft {
                drafts.push(d);
            }
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
        updated_at: ts.clone(),
    };
    drafts.push(Draft::new(
        crate::oplog::kind::ENTITY_CREATE,
        actor,
        &ts,
        task_create_payload(&t),
    ));
    Ok((t, drafts))
}

pub(crate) fn row_to_task(r: &rusqlite::Row) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get("id")?,
        title: r.get("title")?,
        body: r.get("body")?,
        status: TaskStatus::from_str_lossy(r.get::<_, String>("status")?.as_str()),
        priority: Priority::from_str_lossy(r.get::<_, String>("priority")?.as_str()),
        tags: json_vec(r.get::<_, String>("tags")?.as_str()),
        assignees: json_vec(r.get::<_, String>("assignees")?.as_str()),
        project: r.get("project")?,
        phase: r.get("phase")?,
        due: r.get("due")?,
        origin_entry_id: r.get("origin_entry_id")?,
        anchor_text: r.get("anchor_text")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}
