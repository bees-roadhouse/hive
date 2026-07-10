// Mutable per-actor card — the durable-identity write target (store.ts
// `profiles`). The Postgres path's UPSERT splits into create-then-update
// records per the fold contract (body carried wholesale, pre-merged here).

use anyhow::Result;
use hive_shared::{is_ai, ActorKind, Profile, ProfileBody, ProfilePatch, ProfileSource};
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

use super::{now_iso, Draft, Store};

impl Store {
    pub async fn profile_get(&self, actor: &str) -> Result<Option<Profile>> {
        let actor = actor.to_string();
        self.run(move |core| profile_get_conn(core.conn(), &actor))
            .await
    }

    /// Deep-merge `sections` into body.sections (per-key replace), stamp
    /// updated_at, source='manual'. Creates the card on first write.
    pub async fn profile_update(
        &self,
        actor: &str,
        patch: ProfilePatch,
        by: &str,
    ) -> Result<Profile> {
        let actor_s = actor.to_string();
        let by_s = by.to_string();
        let next = self
            .run(move |core| {
                let cur = profile_get_conn(core.conn(), &actor_s)?;
                let mut sections = cur
                    .as_ref()
                    .map(|p| p.body.sections.clone())
                    .unwrap_or_default();
                if let Some(new_sections) = patch.sections {
                    sections.extend(new_sections);
                }
                let next = Profile {
                    actor: actor_s.clone(),
                    kind: patch.kind.or(cur.as_ref().map(|p| p.kind)).unwrap_or(
                        if is_ai(&actor_s) {
                            ActorKind::Ai
                        } else {
                            ActorKind::Human
                        },
                    ),
                    display_name: patch
                        .display_name
                        .or(cur.as_ref().map(|p| p.display_name.clone()))
                        .unwrap_or_default(),
                    body: ProfileBody { sections },
                    source: ProfileSource::Manual,
                    derived_at: cur.as_ref().and_then(|p| p.derived_at.clone()),
                    updated_at: now_iso(),
                };
                let fields = json!({
                    "kind": next.kind.as_str(),
                    "display_name": next.display_name,
                    "body": serde_json::to_string(&next.body)?,
                    "source": next.source.as_str(),
                    "derived_at": next.derived_at,
                    "updated_at": next.updated_at,
                });
                let record_kind = if cur.is_some() {
                    crate::oplog::kind::ENTITY_UPDATE
                } else {
                    crate::oplog::kind::ENTITY_CREATE
                };
                core.commit(vec![Draft::new(
                    record_kind,
                    &by_s,
                    &next.updated_at,
                    json!({"kind": "profile", "id": next.actor, "fields": fields}),
                )])?;
                Ok(next)
            })
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
        #[derive(Clone)]
        struct Row {
            slug: String,
            name: String,
            kind: ActorKind,
            bio: Option<String>,
            role: Option<String>,
        }
        let rows: Vec<Row> = self
            .run(|core| {
                let mut stmt = core.conn().prepare(
                    "SELECT slug, name, kind, bio, role FROM people WHERE bio IS NOT NULL OR role IS NOT NULL",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(Row {
                        slug: r.get("slug")?,
                        name: r.get("name")?,
                        kind: ActorKind::from_str_lossy(r.get::<_, String>("kind")?.as_str()),
                        bio: r.get("bio")?,
                        role: r.get("role")?,
                    })
                })?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        let mut migrated = 0;
        for r in rows {
            let card = self.profile_get(&r.slug).await?;
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
            if let Some(b) = r.bio.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
                if !card_has("bio") {
                    sections.insert("bio".to_string(), b.to_string());
                }
            }
            if let Some(ro) = r.role.as_deref().map(str::trim).filter(|ro| !ro.is_empty()) {
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
                .unwrap_or(r.name);
            self.profile_update(
                &r.slug,
                ProfilePatch {
                    display_name: Some(display_name),
                    kind: Some(r.kind),
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

pub(crate) fn profile_get_conn(conn: &Connection, actor: &str) -> Result<Option<Profile>> {
    Ok(conn
        .query_row(
            "SELECT * FROM profile WHERE actor = ?1",
            rusqlite::params![actor],
            row_to_profile,
        )
        .optional()?)
}

fn row_to_profile(r: &rusqlite::Row) -> rusqlite::Result<Profile> {
    let body: String = r.get("body")?;
    let parsed: ProfileBody = serde_json::from_str(&body).unwrap_or_default();
    Ok(Profile {
        actor: r.get("actor")?,
        kind: ActorKind::from_str_lossy(r.get::<_, String>("kind")?.as_str()),
        display_name: r.get("display_name")?,
        body: parsed,
        source: ProfileSource::from_str_lossy(r.get::<_, String>("source")?.as_str()),
        derived_at: r.get("derived_at")?,
        updated_at: r.get("updated_at")?,
    })
}
