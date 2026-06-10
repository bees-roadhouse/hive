// Cross-platform identity mapping (Rust-branch addition): Discord/Telegram/
// Slack user ids → a people.slug.

use anyhow::{Context, Result};
use hive_shared::{ActorKind, Identity, IdentityPatch, NewIdentity};
use serde_json::json;
use sqlx::Row;

use super::{new_id, now_iso, Store};

impl Store {
    pub async fn identities_list(&self) -> Result<Vec<Identity>> {
        let rows = sqlx::query("SELECT * FROM identities ORDER BY platform, platform_id")
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_identity).collect()
    }

    pub async fn identities_get(&self, id: &str) -> Result<Option<Identity>> {
        let row = sqlx::query("SELECT * FROM identities WHERE id = ?")
            .bind(id)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_identity).transpose()
    }

    pub async fn identities_resolve(
        &self,
        platform: &str,
        platform_id: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT actor FROM identities WHERE platform = ? AND platform_id = ?",
        )
        .bind(platform)
        .bind(platform_id)
        .fetch_optional(self.db())
        .await?)
    }

    pub async fn identities_for_actor(&self, actor: &str) -> Result<Vec<Identity>> {
        let rows = sqlx::query("SELECT * FROM identities WHERE actor = ? ORDER BY platform")
            .bind(actor)
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_identity).collect()
    }

    pub async fn identities_create(&self, input: NewIdentity, by: &str) -> Result<Identity> {
        if self
            .identities_resolve(&input.platform, &input.platform_id)
            .await?
            .is_some()
        {
            let existing = self
                .identities_list()
                .await?
                .into_iter()
                .find(|i| i.platform == input.platform && i.platform_id == input.platform_id)
                .context("identity exists but not found in list")?;
            return Ok(existing);
        }
        let item = Identity {
            id: new_id("idm"),
            platform: input.platform,
            platform_id: input.platform_id,
            actor: input.actor,
            created_at: now_iso(),
        };
        sqlx::query(
            "INSERT INTO identities (id, platform, platform_id, actor, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&item.id)
        .bind(&item.platform)
        .bind(&item.platform_id)
        .bind(&item.actor)
        .bind(&item.created_at)
        .execute(self.db())
        .await?;
        self.emit(
            "identity.created",
            by,
            json!({"id": item.id, "platform": item.platform, "actor": item.actor}),
        )
        .await?;
        Ok(item)
    }

    /// Resolve, or create a person + identity for an unseen platform user.
    /// Returns (actor, identity, created).
    pub async fn identities_resolve_or_create(
        &self,
        platform: &str,
        platform_id: &str,
        display_name: &str,
        by: &str,
    ) -> Result<(String, Identity, bool)> {
        if let Some(actor) = self.identities_resolve(platform, platform_id).await? {
            let identity = self
                .identities_list()
                .await?
                .into_iter()
                .find(|i| i.platform == platform && i.platform_id == platform_id)
                .context("identity exists but not found")?;
            return Ok((actor, identity, false));
        }
        let person = self.people_ensure(display_name, ActorKind::Human).await?;
        let identity = self
            .identities_create(
                NewIdentity {
                    platform: platform.to_string(),
                    platform_id: platform_id.to_string(),
                    actor: person.slug.clone(),
                },
                by,
            )
            .await?;
        Ok((person.slug, identity, true))
    }

    pub async fn identities_update(
        &self,
        id: &str,
        patch: IdentityPatch,
        by: &str,
    ) -> Result<Option<Identity>> {
        let Some(cur) = self.identities_get(id).await? else {
            return Ok(None);
        };
        let actor = patch.actor.unwrap_or_else(|| cur.actor.clone());
        sqlx::query("UPDATE identities SET actor = ? WHERE id = ?")
            .bind(&actor)
            .bind(id)
            .execute(self.db())
            .await?;
        self.emit("identity.updated", by, json!({"id": id, "actor": actor}))
            .await?;
        Ok(Some(Identity { actor, ..cur }))
    }

    pub async fn identities_remove(&self, id: &str, by: &str) -> Result<bool> {
        let Some(cur) = self.identities_get(id).await? else {
            return Ok(false);
        };
        sqlx::query("DELETE FROM identities WHERE id = ?")
            .bind(id)
            .execute(self.db())
            .await?;
        self.emit(
            "identity.removed",
            by,
            json!({"id": id, "platform": cur.platform, "actor": cur.actor}),
        )
        .await?;
        Ok(true)
    }
}

fn row_to_identity(r: &sqlx::sqlite::SqliteRow) -> Result<Identity> {
    Ok(Identity {
        id: r.try_get("id")?,
        platform: r.try_get("platform")?,
        platform_id: r.try_get("platform_id")?,
        actor: r.try_get("actor")?,
        created_at: r.try_get("created_at")?,
    })
}
