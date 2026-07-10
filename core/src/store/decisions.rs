// Decisions list/get/update (store.ts `decisions`). Creates/updates are
// records; the supersedes side effect (prior decision → superseded + link)
// rides the same batch as separate records.

use anyhow::Result;
use hive_shared::{Decision, DecisionPatch, DecisionStatus, EntityKind};
use rusqlite::OptionalExtension;
use serde_json::json;

use super::{json_vec, new_id, now_iso, to_json, Core, Draft, Store};

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
        let status = status.map(str::to_string);
        self.run(move |core| {
            let mut stmt = core
                .conn()
                .prepare("SELECT * FROM decisions ORDER BY created_at DESC")?;
            let rows = stmt.query_map([], row_to_decision)?;
            let decisions = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(decisions
                .into_iter()
                .filter(|d| status.as_deref().is_none_or(|s| d.status.as_str() == s))
                .collect())
        })
        .await
    }

    pub async fn decisions_get(&self, decision_id: &str) -> Result<Option<Decision>> {
        let decision_id = decision_id.to_string();
        self.run(move |core| decision_get(core, &decision_id)).await
    }

    pub async fn decisions_create(&self, input: DecisionCreate, actor: &str) -> Result<Decision> {
        let actor_s = actor.to_string();
        let d = self
            .run(move |core| {
                let (d, drafts) = decision_create_plan(core, input, &actor_s)?;
                core.commit(drafts)?;
                Ok(d)
            })
            .await?;
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
        let decision_id_s = decision_id.to_string();
        let actor_s = actor.to_string();
        let next = self
            .run(move |core| {
                let Some(current) = decision_get(core, &decision_id_s)? else {
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
                core.commit(vec![Draft::new(
                    crate::oplog::kind::ENTITY_UPDATE,
                    &actor_s,
                    &next.updated_at,
                    json!({"kind": "decision", "id": next.id, "fields": {
                        "title": next.title, "context": next.context,
                        "decision": next.decision, "consequences": next.consequences,
                        "status": next.status.as_str(),
                        "tags": to_json(&next.tags), "assignees": to_json(&next.assignees),
                        "updated_at": next.updated_at,
                    }}),
                )])?;
                Ok(Some(next))
            })
            .await?;
        let Some(next) = next else { return Ok(None) };
        self.emit(
            "decision.updated",
            actor,
            json!({"id": next.id, "status": next.status.as_str()}),
        )
        .await?;
        Ok(Some(next))
    }
}

pub(crate) fn decision_get(core: &Core, decision_id: &str) -> Result<Option<Decision>> {
    Ok(core
        .conn()
        .query_row(
            "SELECT * FROM decisions WHERE id = ?1",
            rusqlite::params![decision_id],
            row_to_decision,
        )
        .optional()?)
}

/// The entity.create payload for one decision (also journal.append's
/// `emerged` element shape).
pub(crate) fn decision_create_payload(d: &Decision) -> serde_json::Value {
    json!({"kind": "decision", "id": d.id, "fields": {
        "title": d.title, "context": d.context, "decision": d.decision,
        "consequences": d.consequences, "status": d.status.as_str(),
        "tags": to_json(&d.tags), "assignees": to_json(&d.assignees),
        "project": d.project, "supersedes": d.supersedes,
        "origin_entry_id": d.origin_entry_id, "anchor_text": d.anchor_text,
        "created_at": d.created_at, "updated_at": d.updated_at,
    }})
}

/// Build the Decision + record drafts: optional project ensure, the create,
/// and — when `supersedes` names an existing decision — the prior decision's
/// status flip plus the supersedes link, all in one batch.
pub(crate) fn decision_create_plan(
    core: &Core,
    input: DecisionCreate,
    actor: &str,
) -> Result<(Decision, Vec<Draft>)> {
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
        updated_at: ts.clone(),
    };
    drafts.push(Draft::new(
        crate::oplog::kind::ENTITY_CREATE,
        actor,
        &ts,
        decision_create_payload(&d),
    ));
    if let Some(supersedes) = &d.supersedes {
        if let Some(prior) = decision_get(core, supersedes)? {
            let ts2 = now_iso();
            drafts.push(Draft::new(
                crate::oplog::kind::ENTITY_UPDATE,
                actor,
                &ts2,
                json!({"kind": "decision", "id": prior.id, "fields": {
                    "status": "superseded", "updated_at": ts2,
                }}),
            ));
            drafts.push(super::links::link_draft(
                EntityKind::Decision.as_str(),
                &d.id,
                EntityKind::Decision.as_str(),
                &prior.id,
                "supersedes",
                &ts2,
            ));
        }
    }
    Ok((d, drafts))
}

pub(crate) fn row_to_decision(r: &rusqlite::Row) -> rusqlite::Result<Decision> {
    Ok(Decision {
        id: r.get("id")?,
        title: r.get("title")?,
        context: r.get("context")?,
        decision: r.get("decision")?,
        consequences: r.get("consequences")?,
        status: DecisionStatus::from_str_lossy(r.get::<_, String>("status")?.as_str()),
        tags: json_vec(r.get::<_, String>("tags")?.as_str()),
        assignees: json_vec(r.get::<_, String>("assignees")?.as_str()),
        project: r.get("project")?,
        supersedes: r.get("supersedes")?,
        origin_entry_id: r.get("origin_entry_id")?,
        anchor_text: r.get("anchor_text")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}
