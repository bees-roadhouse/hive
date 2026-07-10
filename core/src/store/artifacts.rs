// Claude Code artifacts (skills / agents / slash-commands) stored per AI
// identity, keyed on the AI actor (people.slug). Upserts split into
// entity.create / entity.update records in the command layer (the fold does
// strict inserts); (actor, kind, name) uniqueness is command-layer law.

use anyhow::Result;
use hive_shared::IdentityArtifact;
use rusqlite::OptionalExtension;
use serde_json::json;

use super::{new_id, now_iso, Draft, Store};

const ARTIFACT_COLS: &str =
    "id, actor, kind, name, content, description, enabled, created_at, updated_at";

impl Store {
    /// Insert-or-update an artifact keyed on (actor, kind, name). On conflict the
    /// content/description/enabled are refreshed and `updated_at` bumped; the
    /// original `id` and `created_at` are preserved. Returns the stored row.
    pub async fn artifacts_upsert(
        &self,
        actor: &str,
        kind: &str,
        name: &str,
        content: &str,
        description: &str,
        enabled: bool,
    ) -> Result<IdentityArtifact> {
        let (actor_s, kind_s, name_s, content_s, desc_s) = (
            actor.to_string(),
            kind.to_string(),
            name.to_string(),
            content.to_string(),
            description.to_string(),
        );
        let artifact = self
            .run(move |core| {
                let now = now_iso();
                let existing = core
                    .conn()
                    .query_row(
                        &format!(
                            "SELECT {ARTIFACT_COLS} FROM identity_artifacts \
                             WHERE actor = ?1 AND kind = ?2 AND name = ?3"
                        ),
                        rusqlite::params![actor_s, kind_s, name_s],
                        row_to_artifact,
                    )
                    .optional()?;
                match existing {
                    Some(prior) => {
                        let next = IdentityArtifact {
                            content: content_s.clone(),
                            description: desc_s.clone(),
                            enabled,
                            updated_at: now.clone(),
                            ..prior
                        };
                        core.commit(vec![Draft::new(
                            crate::oplog::kind::ENTITY_UPDATE,
                            &actor_s,
                            &now,
                            json!({"kind": "identity_artifact", "id": next.id, "fields": {
                                "content": next.content, "description": next.description,
                                "enabled": next.enabled, "updated_at": next.updated_at,
                            }}),
                        )])?;
                        Ok(next)
                    }
                    None => {
                        let next = IdentityArtifact {
                            id: new_id("iart"),
                            actor: actor_s.clone(),
                            kind: kind_s.clone(),
                            name: name_s.clone(),
                            content: content_s.clone(),
                            description: desc_s.clone(),
                            enabled,
                            created_at: now.clone(),
                            updated_at: now.clone(),
                        };
                        core.commit(vec![Draft::new(
                            crate::oplog::kind::ENTITY_CREATE,
                            &actor_s,
                            &now,
                            json!({"kind": "identity_artifact", "id": next.id, "fields": {
                                "actor": next.actor, "kind": next.kind, "name": next.name,
                                "content": next.content, "description": next.description,
                                "enabled": next.enabled,
                                "created_at": next.created_at, "updated_at": next.updated_at,
                            }}),
                        )])?;
                        Ok(next)
                    }
                }
            })
            .await?;
        self.emit(
            "artifact.upserted",
            actor,
            json!({"id": artifact.id, "actor": actor, "kind": kind, "name": name}),
        )
        .await?;
        Ok(artifact)
    }

    /// The sync payload: every ENABLED artifact for an actor, ordered for stable
    /// output. This is what the plugin pulls with the identity token.
    pub async fn artifacts_for_actor(&self, actor: &str) -> Result<Vec<IdentityArtifact>> {
        let actor = actor.to_string();
        self.run(move |core| {
            let mut stmt = core.conn().prepare(&format!(
                "SELECT {ARTIFACT_COLS} FROM identity_artifacts \
                 WHERE actor = ?1 AND enabled = TRUE ORDER BY kind, name"
            ))?;
            let rows = stmt.query_map(rusqlite::params![actor], row_to_artifact)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// All artifacts for an actor (including disabled) — the management listing.
    pub async fn artifacts_list(&self, actor: &str) -> Result<Vec<IdentityArtifact>> {
        let actor = actor.to_string();
        self.run(move |core| {
            let mut stmt = core.conn().prepare(&format!(
                "SELECT {ARTIFACT_COLS} FROM identity_artifacts \
                 WHERE actor = ?1 ORDER BY kind, name"
            ))?;
            let rows = stmt.query_map(rusqlite::params![actor], row_to_artifact)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn artifacts_get(&self, id: &str) -> Result<Option<IdentityArtifact>> {
        let id = id.to_string();
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    &format!("SELECT {ARTIFACT_COLS} FROM identity_artifacts WHERE id = ?1"),
                    rusqlite::params![id],
                    row_to_artifact,
                )
                .optional()?)
        })
        .await
    }

    pub async fn artifacts_remove(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.run(move |core| {
            let exists: bool = core.conn().query_row(
                "SELECT EXISTS(SELECT 1 FROM identity_artifacts WHERE id = ?1)",
                rusqlite::params![id],
                |r| r.get(0),
            )?;
            if !exists {
                return Ok(false);
            }
            core.commit(vec![Draft::new(
                crate::oplog::kind::TOMBSTONE,
                "system",
                &now_iso(),
                json!({"kind": "identity_artifact", "id": id}),
            )])?;
            Ok(true)
        })
        .await
    }
}

fn row_to_artifact(r: &rusqlite::Row) -> rusqlite::Result<IdentityArtifact> {
    Ok(IdentityArtifact {
        id: r.get("id")?,
        actor: r.get("actor")?,
        kind: r.get("kind")?,
        name: r.get("name")?,
        content: r.get("content")?,
        description: r.get("description")?,
        enabled: r.get("enabled")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}
