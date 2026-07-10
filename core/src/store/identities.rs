// Cross-platform identity mapping (Rust-branch addition): Discord/Telegram/
// Slack user ids → a people.slug. Fold-owned from contract v2 (the `identity`
// built-in kind): creates/updates/removes are records.

use anyhow::{Context, Result};
use hive_shared::{ActorKind, Identity, IdentityPatch, NewIdentity};
use rusqlite::OptionalExtension;
use serde_json::json;

use super::{new_id, now_iso, Draft, Store};

impl Store {
    pub async fn identities_list(&self) -> Result<Vec<Identity>> {
        self.run(|core| {
            let mut stmt = core
                .conn()
                .prepare("SELECT * FROM identities ORDER BY platform, platform_id")?;
            let rows = stmt.query_map([], row_to_identity)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn identities_get(&self, id: &str) -> Result<Option<Identity>> {
        let id = id.to_string();
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    "SELECT * FROM identities WHERE id = ?1",
                    rusqlite::params![id],
                    row_to_identity,
                )
                .optional()?)
        })
        .await
    }

    pub async fn identities_resolve(
        &self,
        platform: &str,
        platform_id: &str,
    ) -> Result<Option<String>> {
        let (platform, platform_id) = (platform.to_string(), platform_id.to_string());
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    "SELECT actor FROM identities WHERE platform = ?1 AND platform_id = ?2",
                    rusqlite::params![platform, platform_id],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
    }

    pub async fn identities_for_actor(&self, actor: &str) -> Result<Vec<Identity>> {
        let actor = actor.to_string();
        self.run(move |core| {
            let mut stmt = core
                .conn()
                .prepare("SELECT * FROM identities WHERE actor = ?1 ORDER BY platform")?;
            let rows = stmt.query_map(rusqlite::params![actor], row_to_identity)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
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
        let draft = Draft::new(
            crate::oplog::kind::ENTITY_CREATE,
            by,
            &item.created_at,
            json!({"kind": "identity", "id": item.id, "fields": {
                "platform": item.platform, "platform_id": item.platform_id,
                "actor": item.actor, "created_at": item.created_at,
            }}),
        );
        self.run(move |core| core.commit(vec![draft])).await?;
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
        let draft = Draft::new(
            crate::oplog::kind::ENTITY_UPDATE,
            by,
            &now_iso(),
            json!({"kind": "identity", "id": id, "fields": {"actor": actor}}),
        );
        self.run(move |core| core.commit(vec![draft])).await?;
        self.emit("identity.updated", by, json!({"id": id, "actor": actor}))
            .await?;
        Ok(Some(Identity { actor, ..cur }))
    }

    pub async fn identities_remove(&self, id: &str, by: &str) -> Result<bool> {
        let Some(cur) = self.identities_get(id).await? else {
            return Ok(false);
        };
        let draft = Draft::new(
            crate::oplog::kind::TOMBSTONE,
            by,
            &now_iso(),
            json!({"kind": "identity", "id": id}),
        );
        self.run(move |core| core.commit(vec![draft])).await?;
        self.emit(
            "identity.removed",
            by,
            json!({"id": id, "platform": cur.platform, "actor": cur.actor}),
        )
        .await?;
        Ok(true)
    }
}

fn row_to_identity(r: &rusqlite::Row) -> rusqlite::Result<Identity> {
    Ok(Identity {
        id: r.get("id")?,
        platform: r.get("platform")?,
        platform_id: r.get("platform_id")?,
        actor: r.get("actor")?,
        created_at: r.get("created_at")?,
    })
}
