// Decisions list/get/update (store.ts `decisions`). Owned by core-stores.

use anyhow::Result;
use hive_shared::{Decision, DecisionPatch, DecisionStatus, EntityKind};
use serde_json::json;
use sqlx::Row;

use super::{json_vec, new_id, now_iso, to_json, Store};

/// Inputs for the internal creation path (store.ts `decisions.create` input shape).
#[derive(Debug, Clone)]
pub struct DecisionCreate {
    pub title: String,
    pub context: String,
    pub decision: String,
    pub consequences: String,
    pub status: DecisionStatus,
    pub tags: Vec<String>,
    pub assignees: Vec<String>,
    pub project: Option<String>,
    pub supersedes: Option<String>,
    pub origin_entry_id: Option<String>,
    pub anchor_text: Option<String>,
}

impl Store {
    pub async fn decisions_list(&self, status: Option<&str>) -> Result<Vec<Decision>> {
        let rows = crate::pgq::query("SELECT * FROM decisions ORDER BY created_at DESC")
            .fetch_all(self.db())
            .await?;
        let decisions: Vec<Decision> = rows
            .iter()
            .map(row_to_decision)
            .collect::<Result<Vec<_>>>()?;
        Ok(decisions
            .into_iter()
            .filter(|d| status.is_none_or(|s| d.status.as_str() == s))
            .collect())
    }

    pub async fn decisions_get(&self, decision_id: &str) -> Result<Option<Decision>> {
        let row = crate::pgq::query("SELECT * FROM decisions WHERE id = ?")
            .bind(decision_id)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_decision).transpose()
    }

    pub async fn decisions_create(&self, input: DecisionCreate, actor: &str) -> Result<Decision> {
        if let Some(project) = &input.project {
            if self.projects_get(project).await?.is_none() {
                self.projects_ensure(project).await?;
            }
        }
        let ts = now_iso();
        let d = Decision {
            id: new_id("dec"),
            title: input.title,
            context: input.context,
            decision: input.decision,
            consequences: input.consequences,
            status: input.status,
            tags: input.tags,
            assignees: input.assignees,
            project: input.project,
            supersedes: input.supersedes,
            origin_entry_id: input.origin_entry_id,
            anchor_text: input.anchor_text,
            created_at: ts.clone(),
            updated_at: ts,
        };
        crate::pgq::query(
            "INSERT INTO decisions (id, title, context, decision, consequences, status, tags, assignees, \
             project, supersedes, origin_entry_id, anchor_text, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&d.id)
        .bind(&d.title)
        .bind(&d.context)
        .bind(&d.decision)
        .bind(&d.consequences)
        .bind(d.status.as_str())
        .bind(to_json(&d.tags))
        .bind(to_json(&d.assignees))
        .bind(&d.project)
        .bind(&d.supersedes)
        .bind(&d.origin_entry_id)
        .bind(&d.anchor_text)
        .bind(&d.created_at)
        .bind(&d.updated_at)
        .execute(self.db())
        .await?;
        self.index_entity(
            "decision",
            &d.id,
            &d.title,
            &format!("{} {} {}", d.context, d.decision, d.consequences),
            &d.tags,
        )
        .await?;
        if let Some(supersedes) = &d.supersedes {
            if let Some(prior) = self.decisions_get(supersedes).await? {
                crate::pgq::query(
                    "UPDATE decisions SET status='superseded', updated_at=? WHERE id=?",
                )
                .bind(now_iso())
                .bind(&prior.id)
                .execute(self.db())
                .await?;
                self.links_create(
                    EntityKind::Decision,
                    &d.id,
                    EntityKind::Decision,
                    &prior.id,
                    "supersedes",
                )
                .await?;
            }
        }
        self.emit(
            "decision.created",
            actor,
            json!({"id": d.id, "title": d.title, "status": d.status.as_str()}),
        )
        .await?;
        Ok(d)
    }

    pub async fn decisions_update(
        &self,
        decision_id: &str,
        patch: DecisionPatch,
        actor: &str,
    ) -> Result<Option<Decision>> {
        let Some(current) = self.decisions_get(decision_id).await? else {
            return Ok(None);
        };
        let next = Decision {
            title: patch.title.unwrap_or(current.title),
            context: patch.context.unwrap_or(current.context),
            decision: patch.decision.unwrap_or(current.decision),
            consequences: patch.consequences.unwrap_or(current.consequences),
            status: patch.status.unwrap_or(current.status),
            tags: patch.tags.unwrap_or(current.tags),
            assignees: patch.assignees.unwrap_or(current.assignees),
            updated_at: now_iso(),
            ..current
        };
        crate::pgq::query(
            "UPDATE decisions SET title=?, context=?, decision=?, consequences=?, status=?, tags=?, assignees=?, updated_at=? WHERE id=?",
        )
        .bind(&next.title)
        .bind(&next.context)
        .bind(&next.decision)
        .bind(&next.consequences)
        .bind(next.status.as_str())
        .bind(to_json(&next.tags))
        .bind(to_json(&next.assignees))
        .bind(&next.updated_at)
        .bind(&next.id)
        .execute(self.db())
        .await?;
        self.index_entity(
            "decision",
            &next.id,
            &next.title,
            &format!("{} {} {}", next.context, next.decision, next.consequences),
            &next.tags,
        )
        .await?;
        self.emit(
            "decision.updated",
            actor,
            json!({"id": next.id, "status": next.status.as_str()}),
        )
        .await?;
        Ok(Some(next))
    }
}

pub(crate) fn row_to_decision(r: &sqlx::postgres::PgRow) -> Result<Decision> {
    Ok(Decision {
        id: r.try_get("id")?,
        title: r.try_get("title")?,
        context: r.try_get("context")?,
        decision: r.try_get("decision")?,
        consequences: r.try_get("consequences")?,
        status: DecisionStatus::from_str_lossy(r.try_get::<String, _>("status")?.as_str()),
        tags: json_vec(r.try_get::<String, _>("tags")?.as_str()),
        assignees: json_vec(r.try_get::<String, _>("assignees")?.as_str()),
        project: r.try_get("project")?,
        supersedes: r.try_get("supersedes")?,
        origin_entry_id: r.try_get("origin_entry_id")?,
        anchor_text: r.try_get("anchor_text")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}
