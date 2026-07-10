// Writers: every human and AI that can author journal entries (store.ts
// `people`). Creates/updates are entity.create/entity.update records.

use anyhow::Result;
use hive_shared::{slugify, ActorKind, Person, PersonPatch};
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

use super::{new_id, now_iso, Core, Draft, Store};

impl Store {
    pub async fn people_list(&self) -> Result<Vec<Person>> {
        self.run(|core| {
            let mut stmt = core
                .conn()
                .prepare("SELECT * FROM people ORDER BY kind, slug")?;
            let rows = stmt.query_map([], row_to_person)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn people_get(&self, id_or_slug: &str) -> Result<Option<Person>> {
        let id_or_slug = id_or_slug.to_string();
        self.run(move |core| person_get(core.conn(), &id_or_slug))
            .await
    }

    pub async fn people_by_slug(&self, slug: &str) -> Result<Option<Person>> {
        let slug = slug.to_string();
        self.run(move |core| person_by_slug(core.conn(), &slug))
            .await
    }

    pub async fn people_ensure(&self, name: &str, kind: ActorKind) -> Result<Person> {
        let name = name.to_string();
        self.run(move |core| {
            let (p, draft) = person_ensure_plan(core, &name, kind)?;
            if let Some(draft) = draft {
                core.commit(vec![draft])?;
            }
            Ok(p)
        })
        .await
    }

    /// Insert under an explicit slug (importers / identity mapping). No-op if taken.
    pub async fn people_upsert(
        &self,
        slug: &str,
        name: &str,
        kind: ActorKind,
        owner: Option<&str>,
    ) -> Result<Person> {
        let (slug, name) = (slug.to_string(), name.to_string());
        let owner = owner.map(str::to_string);
        self.run(move |core| {
            if let Some(existing) = person_by_slug(core.conn(), &slug)? {
                return Ok(existing);
            }
            let p = Person {
                id: new_id("per"),
                slug: slug.clone(),
                name: name.clone(),
                kind,
                owner: owner.clone(),
                bio: None,
                role: None,
                created_at: now_iso(),
            };
            core.commit(vec![person_create_draft(&p)])?;
            Ok(p)
        })
        .await
    }

    pub async fn people_create(&self, name: &str, kind: ActorKind, actor: &str) -> Result<Person> {
        let p = self.people_ensure(name, kind).await?;
        self.emit(
            "person.created",
            actor,
            json!({"id": p.id, "name": p.name, "kind": p.kind.as_str()}),
        )
        .await?;
        Ok(p)
    }

    pub async fn people_update(
        &self,
        id_or_slug: &str,
        patch: PersonPatch,
        actor: &str,
    ) -> Result<Option<Person>> {
        let id_or_slug = id_or_slug.to_string();
        let actor_s = actor.to_string();
        let patch2 = patch.clone();
        let updated = self
            .run(move |core| {
                let Some(cur) = person_get(core.conn(), &id_or_slug)? else {
                    return Ok(None);
                };
                let name = patch2.name.clone().unwrap_or_else(|| cur.name.clone());
                let kind = patch2.kind.unwrap_or(cur.kind);
                let owner = match &patch2.owner {
                    Some(v) => v.clone(),
                    None => cur.owner.clone(),
                };
                let bio = match &patch2.bio {
                    Some(v) => v.clone(),
                    None => cur.bio.clone(),
                };
                let role = match &patch2.role {
                    Some(v) => v.clone(),
                    None => cur.role.clone(),
                };
                let slug = if patch2.name.is_some() {
                    slugify(&name)
                } else {
                    cur.slug.clone()
                };
                core.commit(vec![Draft::new(
                    crate::oplog::kind::ENTITY_UPDATE,
                    &actor_s,
                    &now_iso(),
                    json!({"kind": "person", "id": cur.id, "fields": {
                        "name": name, "slug": slug, "kind": kind.as_str(),
                        "owner": owner, "bio": bio, "role": role,
                    }}),
                )])?;
                Ok(Some(Person {
                    id: cur.id,
                    slug,
                    name,
                    kind,
                    owner,
                    bio,
                    role,
                    created_at: cur.created_at,
                }))
            })
            .await?;
        let Some(next) = updated else {
            return Ok(None);
        };

        // The profile card is the canonical identity store; mirror any bio/role
        // edit into it so every writer converges on one source of truth.
        if patch.bio.is_some() || patch.role.is_some() {
            let mut sections = std::collections::BTreeMap::new();
            if let Some(b) = &patch.bio {
                sections.insert("bio".to_string(), b.clone().unwrap_or_default());
            }
            if let Some(r) = &patch.role {
                sections.insert("role".to_string(), r.clone().unwrap_or_default());
            }
            self.profile_update(
                &next.slug,
                hive_shared::ProfilePatch {
                    display_name: Some(next.name.clone()),
                    kind: Some(next.kind),
                    sections: Some(sections),
                },
                actor,
            )
            .await?;
        }

        self.emit(
            "person.updated",
            actor,
            json!({"id": next.id, "name": next.name, "kind": next.kind.as_str()}),
        )
        .await?;
        Ok(Some(next))
    }
}

pub(crate) fn person_get(conn: &Connection, id_or_slug: &str) -> Result<Option<Person>> {
    Ok(conn
        .query_row(
            "SELECT * FROM people WHERE slug = ?1 OR id = ?1",
            rusqlite::params![id_or_slug],
            row_to_person,
        )
        .optional()?)
}

pub(crate) fn person_by_slug(conn: &Connection, slug: &str) -> Result<Option<Person>> {
    Ok(conn
        .query_row(
            "SELECT * FROM people WHERE slug = ?1",
            rusqlite::params![slug],
            row_to_person,
        )
        .optional()?)
}

pub(crate) fn person_create_draft(p: &Person) -> Draft {
    Draft::new(
        crate::oplog::kind::ENTITY_CREATE,
        "system",
        &p.created_at,
        json!({"kind": "person", "id": p.id, "fields": {
            "slug": p.slug, "name": p.name, "kind": p.kind.as_str(),
            "owner": p.owner, "bio": p.bio, "role": p.role,
            "created_at": p.created_at,
        }}),
    )
}

/// Find-or-create plan (see topics::topic_ensure_plan).
pub(crate) fn person_ensure_plan(
    core: &Core,
    name: &str,
    kind: ActorKind,
) -> Result<(Person, Option<Draft>)> {
    let slug = slugify(name);
    if let Some(existing) = person_by_slug(core.conn(), &slug)? {
        return Ok((existing, None));
    }
    let p = Person {
        id: new_id("per"),
        name: name.to_string(),
        slug,
        kind,
        owner: None,
        bio: None,
        role: None,
        created_at: now_iso(),
    };
    let draft = person_create_draft(&p);
    Ok((p, Some(draft)))
}

pub(crate) fn row_to_person(r: &rusqlite::Row) -> rusqlite::Result<Person> {
    Ok(Person {
        id: r.get("id")?,
        slug: r.get("slug")?,
        name: r.get("name")?,
        kind: ActorKind::from_str_lossy(r.get::<_, String>("kind")?.as_str()),
        owner: r.get("owner")?,
        bio: r.get("bio")?,
        role: r.get("role")?,
        created_at: r.get("created_at")?,
    })
}
