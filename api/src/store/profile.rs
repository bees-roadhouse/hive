// Mutable per-actor card — the durable-identity write target (store.ts `profiles`).

use anyhow::Result;
use hive_shared::{is_ai, ActorKind, Profile, ProfileBody, ProfilePatch, ProfileSource};
use serde_json::json;
use sqlx::Row;

use super::{now_iso, Store};

impl Store {
    pub async fn profile_get(&self, actor: &str) -> Result<Option<Profile>> {
        let row = crate::pgq::query("SELECT * FROM profile WHERE actor = ?")
            .bind(actor)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_profile).transpose()
    }

    /// Deep-merge `sections` into body.sections (per-key replace), stamp
    /// updated_at, source='manual'. Creates the card on first write.
    pub async fn profile_update(
        &self,
        actor: &str,
        patch: ProfilePatch,
        by: &str,
    ) -> Result<Profile> {
        let cur = self.profile_get(actor).await?;
        let mut sections = cur
            .as_ref()
            .map(|p| p.body.sections.clone())
            .unwrap_or_default();
        if let Some(new_sections) = patch.sections {
            sections.extend(new_sections);
        }
        let next = Profile {
            actor: actor.to_string(),
            kind: patch
                .kind
                .or(cur.as_ref().map(|p| p.kind))
                .unwrap_or(if is_ai(actor) {
                    ActorKind::Ai
                } else {
                    ActorKind::Human
                }),
            display_name: patch
                .display_name
                .or(cur.as_ref().map(|p| p.display_name.clone()))
                .unwrap_or_default(),
            body: ProfileBody { sections },
            source: ProfileSource::Manual,
            derived_at: cur.as_ref().and_then(|p| p.derived_at.clone()),
            updated_at: now_iso(),
        };
        crate::pgq::query(
            "INSERT INTO profile (actor, kind, display_name, body, source, derived_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(actor) DO UPDATE SET kind=excluded.kind, display_name=excluded.display_name, \
               body=excluded.body, source=excluded.source, derived_at=excluded.derived_at, updated_at=excluded.updated_at",
        )
        .bind(&next.actor)
        .bind(next.kind.as_str())
        .bind(&next.display_name)
        .bind(serde_json::to_string(&next.body)?)
        .bind(next.source.as_str())
        .bind(&next.derived_at)
        .bind(&next.updated_at)
        .execute(self.db())
        .await?;
        self.emit(
            "profile.updated",
            by,
            json!({"actor": actor, "source": next.source.as_str()}),
        )
        .await?;
        Ok(next)
    }

    /// One-time reconciliation (#31 → #37): fold legacy people.bio/role into the
    /// canonical profile card as sections.bio/sections.role. Idempotent — only
    /// fills a section that's missing/blank. Safe to run on every boot.
    pub async fn backfill_identity_cards(&self) -> Result<i64> {
        let rows = crate::pgq::query(
            "SELECT slug, name, kind, bio, role FROM people WHERE bio IS NOT NULL OR role IS NOT NULL",
        )
        .fetch_all(self.db())
        .await?;
        let mut migrated = 0;
        for r in rows {
            let slug: String = r.try_get("slug")?;
            let name: String = r.try_get("name")?;
            let kind = ActorKind::from_str_lossy(r.try_get::<String, _>("kind")?.as_str());
            let bio: Option<String> = r.try_get("bio")?;
            let role: Option<String> = r.try_get("role")?;

            let card = self.profile_get(&slug).await?;
            let mut sections = std::collections::BTreeMap::new();
            let card_has = |key: &str| {
                card.as_ref()
                    .map(|c| {
                        c.body
                            .sections
                            .get(key)
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            };
            if let Some(b) = bio.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
                if !card_has("bio") {
                    sections.insert("bio".to_string(), b.to_string());
                }
            }
            if let Some(ro) = role.as_deref().map(str::trim).filter(|ro| !ro.is_empty()) {
                if !card_has("role") {
                    sections.insert("role".to_string(), ro.to_string());
                }
            }
            if sections.is_empty() {
                continue;
            }
            let display_name = card
                .as_ref()
                .map(|c| c.display_name.clone())
                .filter(|d| !d.is_empty())
                .unwrap_or(name);
            self.profile_update(
                &slug,
                ProfilePatch {
                    display_name: Some(display_name),
                    kind: Some(kind),
                    sections: Some(sections),
                },
                "migration",
            )
            .await?;
            migrated += 1;
        }
        Ok(migrated)
    }
}

fn row_to_profile(r: &sqlx::postgres::PgRow) -> Result<Profile> {
    let body: String = r.try_get("body")?;
    let parsed: ProfileBody = serde_json::from_str(&body).unwrap_or_default();
    Ok(Profile {
        actor: r.try_get("actor")?,
        kind: ActorKind::from_str_lossy(r.try_get::<String, _>("kind")?.as_str()),
        display_name: r.try_get("display_name")?,
        body: parsed,
        source: ProfileSource::from_str_lossy(r.try_get::<String, _>("source")?.as_str()),
        derived_at: r.try_get("derived_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}
