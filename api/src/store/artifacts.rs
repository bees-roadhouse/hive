// Claude Code artifacts (skills / agents / slash-commands) stored per AI
// identity. The plugin pulls an identity's ENABLED artifacts via the sync
// endpoint, authenticated by the AI-identity token — keyed on the AI actor
// (people.slug), NOT the per-user memory namespace.
//
// Skills are modeled as a single SKILL.md `content` for v1; multi-file skills
// (bundled scripts / references) are out of scope for now.

use anyhow::Result;
use hive_shared::IdentityArtifact;
use serde_json::json;
use sqlx::Row;

use super::{new_id, now_iso, Store};

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
        let now = now_iso();
        let id = new_id("iart");
        let row = crate::pgq::query(&format!(
            "INSERT INTO identity_artifacts (id, actor, kind, name, content, description, enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (actor, kind, name) DO UPDATE SET \
               content = excluded.content, \
               description = excluded.description, \
               enabled = excluded.enabled, \
               updated_at = excluded.updated_at \
             RETURNING {ARTIFACT_COLS}"
        ))
        .bind(&id)
        .bind(actor)
        .bind(kind)
        .bind(name)
        .bind(content)
        .bind(description)
        .bind(enabled)
        .bind(&now)
        .bind(&now)
        .fetch_one(self.db())
        .await?;
        let artifact = row_to_artifact(&row)?;
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
        let rows = crate::pgq::query(&format!(
            "SELECT {ARTIFACT_COLS} FROM identity_artifacts \
             WHERE actor = ? AND enabled = TRUE ORDER BY kind, name"
        ))
        .bind(actor)
        .fetch_all(self.db())
        .await?;
        rows.iter().map(row_to_artifact).collect()
    }

    /// All artifacts for an actor (including disabled) — the management listing.
    pub async fn artifacts_list(&self, actor: &str) -> Result<Vec<IdentityArtifact>> {
        let rows = crate::pgq::query(&format!(
            "SELECT {ARTIFACT_COLS} FROM identity_artifacts \
             WHERE actor = ? ORDER BY kind, name"
        ))
        .bind(actor)
        .fetch_all(self.db())
        .await?;
        rows.iter().map(row_to_artifact).collect()
    }

    pub async fn artifacts_get(&self, id: &str) -> Result<Option<IdentityArtifact>> {
        let row = crate::pgq::query(&format!(
            "SELECT {ARTIFACT_COLS} FROM identity_artifacts WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        row.as_ref().map(row_to_artifact).transpose()
    }

    pub async fn artifacts_remove(&self, id: &str) -> Result<bool> {
        let res = crate::pgq::query("DELETE FROM identity_artifacts WHERE id = ?")
            .bind(id)
            .execute(self.db())
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

fn row_to_artifact(r: &sqlx::postgres::PgRow) -> Result<IdentityArtifact> {
    Ok(IdentityArtifact {
        id: r.try_get("id")?,
        actor: r.try_get("actor")?,
        kind: r.try_get("kind")?,
        name: r.try_get("name")?,
        content: r.try_get("content")?,
        description: r.try_get("description")?,
        enabled: r.try_get("enabled")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}
