// Writers: every human and AI that can author journal entries (store.ts `people`).

use anyhow::Result;
use hive_shared::{slugify, ActorKind, Person, PersonPatch};
use serde_json::json;
use sqlx::Row;

use super::{new_id, now_iso, Store};

impl Store {
    pub async fn people_list(&self) -> Result<Vec<Person>> {
        let rows = crate::pgq::query("SELECT * FROM people ORDER BY kind, slug")
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_person).collect()
    }

    pub async fn people_get(&self, id_or_slug: &str) -> Result<Option<Person>> {
        let row = crate::pgq::query("SELECT * FROM people WHERE slug = ? OR id = ?")
            .bind(id_or_slug)
            .bind(id_or_slug)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_person).transpose()
    }

    pub async fn people_by_slug(&self, slug: &str) -> Result<Option<Person>> {
        let row = crate::pgq::query("SELECT * FROM people WHERE slug = ?")
            .bind(slug)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_person).transpose()
    }

    pub async fn people_ensure(&self, name: &str, kind: ActorKind) -> Result<Person> {
        let slug = slugify(name);
        if let Some(existing) = self.people_by_slug(&slug).await? {
            return Ok(existing);
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
        crate::pgq::query(
            "INSERT INTO people (id, name, slug, kind, owner, bio, role, created_at) \
             VALUES (?, ?, ?, ?, NULL, NULL, NULL, ?)",
        )
        .bind(&p.id)
        .bind(&p.name)
        .bind(&p.slug)
        .bind(p.kind.as_str())
        .bind(&p.created_at)
        .execute(self.db())
        .await?;
        Ok(p)
    }

    /// Insert under an explicit slug (importers / identity mapping). No-op if taken.
    pub async fn people_upsert(
        &self,
        slug: &str,
        name: &str,
        kind: ActorKind,
        owner: Option<&str>,
    ) -> Result<Person> {
        if let Some(existing) = self.people_by_slug(slug).await? {
            return Ok(existing);
        }
        let p = Person {
            id: new_id("per"),
            slug: slug.to_string(),
            name: name.to_string(),
            kind,
            owner: owner.map(String::from),
            bio: None,
            role: None,
            created_at: now_iso(),
        };
        crate::pgq::query(
            "INSERT INTO people (id, slug, name, kind, owner, bio, role, created_at) \
             VALUES (?, ?, ?, ?, ?, NULL, NULL, ?)",
        )
        .bind(&p.id)
        .bind(&p.slug)
        .bind(&p.name)
        .bind(p.kind.as_str())
        .bind(&p.owner)
        .bind(&p.created_at)
        .execute(self.db())
        .await?;
        Ok(p)
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
        let Some(cur) = self.people_get(id_or_slug).await? else {
            return Ok(None);
        };
        let name = patch.name.clone().unwrap_or_else(|| cur.name.clone());
        let kind = patch.kind.unwrap_or(cur.kind);
        let owner = match &patch.owner {
            Some(v) => v.clone(),
            None => cur.owner.clone(),
        };
        let bio = match &patch.bio {
            Some(v) => v.clone(),
            None => cur.bio.clone(),
        };
        let role = match &patch.role {
            Some(v) => v.clone(),
            None => cur.role.clone(),
        };
        let slug = if patch.name.is_some() {
            slugify(&name)
        } else {
            cur.slug.clone()
        };

        crate::pgq::query("UPDATE people SET name = ?, slug = ?, kind = ?, owner = ?, bio = ?, role = ? WHERE id = ?")
            .bind(&name)
            .bind(&slug)
            .bind(kind.as_str())
            .bind(&owner)
            .bind(&bio)
            .bind(&role)
            .bind(&cur.id)
            .execute(self.db())
            .await?;

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
                &slug,
                hive_shared::ProfilePatch {
                    display_name: Some(name.clone()),
                    kind: Some(kind),
                    sections: Some(sections),
                },
                actor,
            )
            .await?;
        }

        self.emit(
            "person.updated",
            actor,
            json!({"id": cur.id, "name": name, "kind": kind.as_str()}),
        )
        .await?;
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
    }
}

pub(crate) fn row_to_person(r: &sqlx::postgres::PgRow) -> Result<Person> {
    Ok(Person {
        id: r.try_get("id")?,
        slug: r.try_get("slug")?,
        name: r.try_get("name")?,
        kind: ActorKind::from_str_lossy(r.try_get::<String, _>("kind")?.as_str()),
        owner: r.try_get("owner")?,
        bio: r.try_get("bio")?,
        role: r.try_get("role")?,
        created_at: r.try_get("created_at")?,
    })
}
