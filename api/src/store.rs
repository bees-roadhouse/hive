use hive_shared::*;
use sqlx::{SqlitePool, Row};
use anyhow::{Result, Context};
use chrono::{DateTime, Utc};
use tracing::info;

pub struct Store {
    db: SqlitePool,
}

impl Store {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &SqlitePool {
        &self.db
    }
}

// ---- id generation ----

fn nanoid(prefix: &str) -> String {
    format!("{}_{}", prefix, nanoid::nanoid!(12))
}

fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

// ---- JSON helpers for SQLite ----

fn to_json<T: serde::Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?)
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_str(s)?)
}

// ---- wire events ----

impl Store {
    pub async fn emit(&self, kind: &str, actor: &str, payload: serde_json::Value) -> Result<WireEvent> {
        let id = nanoid("wire");
        let created_at = now_iso();
        sqlx::query("INSERT INTO wire (id, kind, actor, payload, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(kind)
            .bind(actor)
            .bind(payload.to_string())
            .bind(&created_at)
            .execute(&self.db)
            .await?;
        Ok(WireEvent { id, kind: kind.to_string(), actor: actor.to_string(), payload, created_at: parse_iso(&created_at)? })
    }

    pub async fn wire_log(&self, limit: i64) -> Result<Vec<WireEvent>> {
        let rows = sqlx::query("SELECT id, kind, actor, payload, created_at FROM wire ORDER BY created_at DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.db)
            .await?;
        rows.iter()
            .map(|r| Ok(WireEvent {
                id: r.try_get("id")?,
                kind: r.try_get("kind")?,
                actor: r.try_get("actor")?,
                payload: serde_json::from_str(r.try_get::<String, _>("payload")?.as_str())?,
                created_at: parse_iso(r.try_get("created_at")?)?,
            }))
            .collect()
    }
}

// ---- people ----

impl Store {
    pub async fn people_list(&self) -> Result<Vec<Person>> {
        let rows = sqlx::query("SELECT * FROM people ORDER BY kind, slug").fetch_all(&self.db).await?;
        rows.iter().map(row_to_person).collect()
    }

    pub async fn people_get(&self, id_or_slug: &str) -> Result<Option<Person>> {
        let row = sqlx::query("SELECT * FROM people WHERE slug = ? OR id = ?")
            .bind(id_or_slug)
            .bind(id_or_slug)
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(row_to_person).transpose()
    }

    pub async fn people_by_slug(&self, slug: &str) -> Result<Option<Person>> {
        let row = sqlx::query("SELECT * FROM people WHERE slug = ?")
            .bind(slug)
            .fetch_optional(&self.db)
            .await?;
        row.as_ref().map(row_to_person).transpose()
    }

    pub async fn people_ais_owned_by(&self, owner: &str) -> Result<Vec<Person>> {
        let rows = sqlx::query("SELECT * FROM people WHERE kind = 'ai' AND owner = ? ORDER BY slug")
            .bind(owner)
            .fetch_all(&self.db)
            .await?;
        rows.iter().map(row_to_person).collect()
    }

    pub async fn people_ensure(&self, name: &str, kind: ActorKind) -> Result<Person> {
        let slug = slugify(name);
        if let Some(p) = self.people_by_slug(&slug).await? {
            return Ok(p);
        }
        let id = nanoid("per");
        let created_at = now_iso();
        sqlx::query("INSERT INTO people (id, slug, name, kind, owner, bio, role, created_at) VALUES (?, ?, ?, ?, NULL, NULL, NULL, ?)")
            .bind(&id)
            .bind(&slug)
            .bind(name)
            .bind(kind_to_str(kind))
            .bind(&created_at)
            .execute(&self.db)
            .await?;
        Ok(Person { id, slug, name: name.to_string(), kind, owner: None, bio: None, role: None, created_at: parse_iso(&created_at)? })
    }

    pub async fn people_create(&self, name: &str, kind: ActorKind, actor: &str) -> Result<Person> {
        let p = self.people_ensure(name, kind).await?;
        self.emit("person.created", actor, serde_json::json!({"id": &p.id, "name": &p.name, "kind": kind_to_str(kind)})).await?;
        Ok(p)
    }

    pub async fn people_update(&self, id_or_slug: &str, patch: PersonPatch, by: &str) -> Result<Option<Person>> {
        let cur = match self.people_get(id_or_slug).await? {
            Some(p) => p,
            None => return Ok(None),
        };
        let name = patch.name.unwrap_or_else(|| cur.name.clone());
        let kind = patch.kind.unwrap_or(cur.kind);
        let owner = patch.owner.unwrap_or_else(|| cur.owner.clone());
        let bio = patch.bio.unwrap_or_else(|| cur.bio.clone());
        let role = patch.role.unwrap_or_else(|| cur.role.clone());
        let slug = if patch.name.is_some() { slugify(&name) } else { cur.slug.clone() };

        sqlx::query("UPDATE people SET name = ?, slug = ?, kind = ?, owner = ?, bio = ?, role = ? WHERE id = ?")
            .bind(&name)
            .bind(&slug)
            .bind(kind_to_str(kind))
            .bind(&owner)
            .bind(&bio)
            .bind(&role)
            .bind(&cur.id)
            .execute(&self.db)
            .await?;

        self.emit("person.updated", by, serde_json::json!({"id": &cur.id, "name": &name, "kind": kind_to_str(kind)})).await?;
        Ok(Some(Person { id: cur.id, slug, name, kind, owner, bio, role, created_at: cur.created_at }))
    }
}

fn row_to_person(r: &sqlx::sqlite::SqliteRow) -> Result<Person> {
    Ok(Person {
        id: r.try_get("id")?,
        slug: r.try_get("slug")?,
        name: r.try_get("name")?,
        kind: str_to_kind(r.try_get::<String, _>("kind")?.as_str()),
        owner: r.try_get("owner")?,
        bio: r.try_get("bio")?,
        role: r.try_get("role")?,
        created_at: parse_iso(r.try_get("created_at")?)?,
    })
}

// ---- identities ----

impl Store {
    pub async fn identities_list(&self) -> Result<Vec<Identity>> {
        let rows = sqlx::query("SELECT * FROM identities ORDER BY platform, platform_id").fetch_all(&self.db).await?;
        rows.iter().map(row_to_identity).collect()
    }

    pub async fn identities_get(&self, id: &str) -> Result<Option<Identity>> {
        let row = sqlx::query("SELECT * FROM identities WHERE id = ?").bind(id).fetch_optional(&self.db).await?;
        row.as_ref().map(row_to_identity).transpose()
    }

    pub async fn identities_resolve(&self, platform: &str, platform_id: &str) -> Result<Option<String>> {
        let row: Option<String> = sqlx::query_scalar("SELECT actor FROM identities WHERE platform = ? AND platform_id = ?")
            .bind(platform)
            .bind(platform_id)
            .fetch_optional(&self.db)
            .await?;
        Ok(row)
    }

    pub async fn identities_for_actor(&self, actor: &str) -> Result<Vec<Identity>> {
        let rows = sqlx::query("SELECT * FROM identities WHERE actor = ? ORDER BY platform").bind(actor).fetch_all(&self.db).await?;
        rows.iter().map(row_to_identity).collect()
    }

    pub async fn identities_create(&self, input: NewIdentity, by: &str) -> Result<Identity> {
        if let Some(actor) = self.identities_resolve(&input.platform, &input.platform_id).await? {
            let existing = self.identities_list().await?.into_iter()
                .find(|i| i.platform == input.platform && i.platform_id == input.platform_id)
                .context("identity exists but not found in list")?;
            return Ok(existing);
        }
        let id = nanoid("idm");
        let created_at = now_iso();
        sqlx::query("INSERT INTO identities (id, platform, platform_id, actor, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(&input.platform)
            .bind(&input.platform_id)
            .bind(&input.actor)
            .bind(&created_at)
            .execute(&self.db)
            .await?;
        let item = Identity { id: id.clone(), platform: input.platform, platform_id: input.platform_id, actor: input.actor, created_at: parse_iso(&created_at)? };
        self.emit("identity.created", by, serde_json::json!({"id": &id, "platform": &item.platform, "actor": &item.actor})).await?;
        Ok(item)
    }

    pub async fn identities_resolve_or_create(&self, platform: &str, platform_id: &str, display_name: &str, by: &str) -> Result<(String, Identity, bool)> {
        if let Some(actor) = self.identities_resolve(platform, platform_id).await? {
            let identity = self.identities_list().await?.into_iter()
                .find(|i| i.platform == platform && i.platform_id == platform_id)
                .context("identity exists but not found")?;
            return Ok((actor, identity, false));
        }
        let person = self.people_ensure(display_name, ActorKind::Human).await?;
        let identity = self.identities_create(NewIdentity { platform: platform.to_string(), platform_id: platform_id.to_string(), actor: person.slug.clone() }, by).await?;
        Ok((person.slug, identity, true))
    }

    pub async fn identities_update(&self, id: &str, patch: IdentityPatch, by: &str) -> Result<Option<Identity>> {
        let cur = match self.identities_get(id).await? {
            Some(i) => i,
            None => return Ok(None),
        };
        let actor = patch.actor.unwrap_or_else(|| cur.actor.clone());
        sqlx::query("UPDATE identities SET actor = ? WHERE id = ?").bind(&actor).bind(id).execute(&self.db).await?;
        let next = Identity { id: id.to_string(), platform: cur.platform, platform_id: cur.platform_id, actor, created_at: cur.created_at };
        self.emit("identity.updated", by, serde_json::json!({"id": id, "actor": &next.actor})).await?;
        Ok(Some(next))
    }

    pub async fn identities_remove(&self, id: &str, by: &str) -> Result<bool> {
        let cur = match self.identities_get(id).await? {
            Some(i) => i,
            None => return Ok(false),
        };
        sqlx::query("DELETE FROM identities WHERE id = ?").bind(id).execute(&self.db).await?;
        self.emit("identity.removed", by, serde_json::json!({"id": id, "platform": &cur.platform, "actor": &cur.actor})).await?;
        Ok(true)
    }
}

fn row_to_identity(r: &sqlx::sqlite::SqliteRow) -> Result<Identity> {
    Ok(Identity {
        id: r.try_get("id")?,
        platform: r.try_get("platform")?,
        platform_id: r.try_get("platform_id")?,
        actor: r.try_get("actor")?,
        created_at: parse_iso(r.try_get("created_at")?)?,
    })
}

// ---- profiles ----

impl Store {
    pub async fn profile_get(&self, actor: &str) -> Result<Option<Profile>> {
        let row = sqlx::query("SELECT * FROM profile WHERE actor = ?").bind(actor).fetch_optional(&self.db).await?;
        row.as_ref().map(row_to_profile).transpose()
    }

    pub async fn profile_update(&self, actor: &str, patch: ProfilePatch, by: &str) -> Result<Profile> {
        let cur = self.profile_get(actor).await?;
        let sections = if let Some(new_sections) = patch.sections {
            let mut merged = cur.as_ref().map(|p| p.body.sections.clone()).unwrap_or_default();
            merged.extend(new_sections);
            merged
        } else {
            cur.as_ref().map(|p| p.body.sections.clone()).unwrap_or_default()
        };
        let kind = patch.kind.unwrap_or_else(|| cur.as_ref().map(|p| p.kind).unwrap_or(ActorKind::Human));
        let display_name = patch.display_name.unwrap_or_else(|| cur.as_ref().map(|p| p.display_name.clone()).unwrap_or_default());
        let updated_at = now_iso();
        let body = serde_json::to_string(&ProfileBody { sections })?;

        sqlx::query("INSERT INTO profile (actor, kind, display_name, body, source, derived_at, updated_at) VALUES (?, ?, ?, ?, 'manual', NULL, ?) ON CONFLICT(actor) DO UPDATE SET kind = excluded.kind, display_name = excluded.display_name, body = excluded.body, source = excluded.source, derived_at = excluded.derived_at, updated_at = excluded.updated_at")
            .bind(actor)
            .bind(kind_to_str(kind))
            .bind(&display_name)
            .bind(&body)
            .bind(&updated_at)
            .execute(&self.db)
            .await?;

        let next = Profile {
            actor: actor.to_string(),
            kind,
            display_name,
            body: serde_json::from_str(&body)?,
            source: ProfileSource::Manual,
            derived_at: None,
            updated_at: parse_iso(&updated_at)?,
        };
        self.emit("profile.updated", by, serde_json::json!({"actor": actor, "source": "manual"})).await?;
        Ok(next)
    }
}

fn row_to_profile(r: &sqlx::sqlite::SqliteRow) -> Result<Profile> {
    Ok(Profile {
        actor: r.try_get("actor")?,
        kind: str_to_kind(r.try_get::<String, _>("kind")?.as_str()),
        display_name: r.try_get("display_name")?,
        body: serde_json::from_str(r.try_get::<String, _>("body")?.as_str())?,
        source: if r.try_get::<String, _>("source")?.as_str() == "derived" { ProfileSource::Derived } else { ProfileSource::Manual },
        derived_at: r.try_get::<Option<String>, _>("derived_at")?.map(|s| parse_iso(&s)).transpose()?,
        updated_at: parse_iso(r.try_get("updated_at")?)?,
    })
}

// ---- projects ----

impl Store {
    pub async fn projects_list(&self) -> Result<Vec<Project>> {
        let rows = sqlx::query("SELECT * FROM projects ORDER BY name").fetch_all(&self.db).await?;
        rows.iter().map(row_to_project).collect()
    }

    pub async fn projects_get(&self, id: &str) -> Result<Option<Project>> {
        let row = sqlx::query("SELECT * FROM projects WHERE id = ?").bind(id).fetch_optional(&self.db).await?;
        row.as_ref().map(row_to_project).transpose()
    }

    pub async fn projects_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        let row = sqlx::query("SELECT * FROM projects WHERE slug = ?").bind(slug).fetch_optional(&self.db).await?;
        row.as_ref().map(row_to_project).transpose()
    }

    pub async fn projects_ensure(&self, name: &str) -> Result<Project> {
        let slug = slugify(name);
        if let Some(p) = self.projects_by_slug(&slug).await? {
            return Ok(p);
        }
        let id = nanoid("proj");
        let created_at = now_iso();
        sqlx::query("INSERT INTO projects (id, name, slug, created_at) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(name)
            .bind(&slug)
            .bind(&created_at)
            .execute(&self.db)
            .await?;
        Ok(Project { id, name: name.to_string(), slug, created_at: parse_iso(&created_at)? })
    }
}

fn row_to_project(r: &sqlx::sqlite::SqliteRow) -> Result<Project> {
    Ok(Project {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        slug: r.try_get("slug")?,
        created_at: parse_iso(r.try_get("created_at")?)?,
    })
}

// ---- journal ----

impl Store {
    pub async fn journal_list(&self, limit: i64, author: Option<&str>) -> Result<Vec<JournalEntry>> {
        let rows = if let Some(a) = author {
            sqlx::query("SELECT * FROM journal WHERE author = ? ORDER BY created_at DESC LIMIT ?")
                .bind(a)
                .bind(limit)
                .fetch_all(&self.db)
                .await?
        } else {
            sqlx::query("SELECT * FROM journal ORDER BY created_at DESC LIMIT ?")
                .bind(limit)
                .fetch_all(&self.db)
                .await?
        };
        rows.iter().map(row_to_journal_entry).collect()
    }

    pub async fn journal_get(&self, id: &str) -> Result<Option<JournalEntry>> {
        let row = sqlx::query("SELECT * FROM journal WHERE id = ?").bind(id).fetch_optional(&self.db).await?;
        row.as_ref().map(row_to_journal_entry).transpose()
    }

    pub async fn journal_create(&self, entry: NewJournalEntry) -> Result<JournalEntry> {
        let id = nanoid("jrn");
        let created_at = now_iso();
        let tags_json = serde_json::to_string(&entry.tags)?;
        let mentions = parse_mentions(&entry.body);
        let mentions_json = serde_json::to_string(&mentions)?;

        sqlx::query("INSERT INTO journal (id, author, body, tags, mentions, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(&entry.author)
            .bind(&entry.body)
            .bind(&tags_json)
            .bind(&mentions_json)
            .bind(&created_at)
            .execute(&self.db)
            .await?;

        // Index for search
        let title = derive_title(&entry.body);
        sqlx::query("INSERT INTO search (kind, ref_id, title, body) VALUES ('journal', ?, ?, ?)")
            .bind(&id)
            .bind(&title)
            .bind(&entry.body)
            .execute(&self.db)
            .await?;

        // Emit inbox items for mentions
        for mention in &mentions {
            if mention != &entry.author {
                self.inbox_add(mention, &entry.author, InboxReason::Mention, EntityKind::Journal, &id, Some(&id), &entry.body).await?;
            }
        }

        Ok(JournalEntry {
            id,
            author: entry.author,
            body: entry.body,
            tags: entry.tags,
            mentions,
            created_at: parse_iso(&created_at)?,
        })
    }
}

fn row_to_journal_entry(r: &sqlx::sqlite::SqliteRow) -> Result<JournalEntry> {
    Ok(JournalEntry {
        id: r.try_get("id")?,
        author: r.try_get("author")?,
        body: r.try_get("body")?,
        tags: serde_json::from_str(r.try_get::<String, _>("tags")?.as_str())?,
        mentions: serde_json::from_str(r.try_get::<String, _>("mentions")?.as_str())?,
        created_at: parse_iso(r.try_get("created_at")?)?,
    })
}

fn derive_title(body: &str) -> String {
    let first_line = body.lines().next().unwrap_or(body);
    if first_line.len() > 80 {
        format!("{}...", &first_line[..77])
    } else {
        first_line.to_string()
    }
}

// ---- inbox ----

impl Store {
    pub async fn inbox_add(&self, recipient: &str, from: &str, reason: InboxReason, ref_kind: EntityKind, ref_id: &str, entry_id: Option<&str>, snippet: &str) -> Result<Option<InboxItem>> {
        if recipient == from {
            return Ok(None);
        }
        let id = nanoid("inb");
        let created_at = now_iso();
        let snippet = if snippet.len() > 140 { format!("{}…", &snippet[..137]) } else { snippet.to_string() };
        sqlx::query(r#"INSERT INTO inbox (id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)"#)
            .bind(&id)
            .bind(recipient)
            .bind(from)
            .bind(entity_kind_to_str(ref_kind))
            .bind(ref_id)
            .bind(entry_id)
            .bind(&snippet)
            .bind(&created_at)
            .execute(&self.db)
            .await?;
        self.emit("inbox.delivered", from, serde_json::json!({"to": recipient, "reason": entity_kind_to_str(ref_kind), "ref_kind": entity_kind_to_str(ref_kind), "ref_id": ref_id})).await?;
        Ok(Some(InboxItem { id, recipient: recipient.to_string(), from: from.to_string(), reason, ref_kind, ref_id: ref_id.to_string(), entry_id: entry_id.map(|s| s.to_string()), snippet, created_at: parse_iso(&created_at)?, read_at: None }))
    }

    pub async fn inbox_list(&self, recipient: &str, unread_only: bool) -> Result<Vec<InboxItem>> {
        let sql = if unread_only {
            r#"SELECT id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at FROM inbox WHERE recipient = ? AND read_at IS NULL ORDER BY created_at DESC"#
        } else {
            r#"SELECT id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at FROM inbox WHERE recipient = ? ORDER BY created_at DESC"#
        };
        let rows = sqlx::query(sql).bind(recipient).fetch_all(&self.db).await?;
        rows.iter().map(row_to_inbox).collect()
    }

    pub async fn inbox_mark_read(&self, item_id: &str) -> Result<bool> {
        let now = now_iso();
        let result = sqlx::query("UPDATE inbox SET read_at = ? WHERE id = ? AND read_at IS NULL")
            .bind(&now)
            .bind(item_id)
            .execute(&self.db)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn inbox_mark_all_read(&self, recipient: &str) -> Result<u64> {
        let now = now_iso();
        let result = sqlx::query("UPDATE inbox SET read_at = ? WHERE recipient = ? AND read_at IS NULL")
            .bind(&now)
            .bind(recipient)
            .execute(&self.db)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn inbox_unread_count(&self, recipient: &str) -> Result<i64> {
        let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM inbox WHERE recipient = ? AND read_at IS NULL"#)
            .bind(recipient)
            .fetch_one(&self.db)
            .await?;
        Ok(count)
    }
}

fn row_to_inbox(r: &sqlx::sqlite::SqliteRow) -> Result<InboxItem> {
    Ok(InboxItem {
        id: r.try_get("id")?,
        recipient: r.try_get("recipient")?,
        from: r.try_get("from")?,
        reason: str_to_inbox_reason(r.try_get::<String, _>("reason")?.as_str()),
        ref_kind: str_to_entity_kind(r.try_get::<String, _>("ref_kind")?.as_str()),
        ref_id: r.try_get("ref_id")?,
        entry_id: r.try_get("entry_id")?,
        snippet: r.try_get("snippet")?,
        created_at: parse_iso(r.try_get("created_at")?)?,
        read_at: r.try_get::<Option<String>, _>("read_at")?.map(|s| parse_iso(&s)).transpose()?,
    })
}

// ---- tasks ----

impl Store {
    pub async fn tasks_list(&self, project: Option<&str>, status: Option<TaskStatus>) -> Result<Vec<Task>> {
        let mut sql = "SELECT * FROM tasks WHERE 1=1".to_string();
        if project.is_some() { sql += " AND project = ?"; }
        if status.is_some() { sql += " AND status = ?"; }
        sql += " ORDER BY updated_at DESC";
        let mut q = sqlx::query(&sql);
        if let Some(p) = project { q = q.bind(p); }
        if let Some(s) = status { q = q.bind(task_status_to_str(s)); }
        let rows = q.fetch_all(&self.db).await?;
        rows.iter().map(row_to_task).collect()
    }

    pub async fn tasks_get(&self, id: &str) -> Result<Option<Task>> {
        let row = sqlx::query("SELECT * FROM tasks WHERE id = ?").bind(id).fetch_optional(&self.db).await?;
        row.as_ref().map(row_to_task).transpose()
    }

    pub async fn tasks_create(&self, title: &str, body: &str, actor: &str) -> Result<Task> {
        let id = nanoid("tsk");
        let created_at = now_iso();
        let updated_at = created_at.clone();
        sqlx::query("INSERT INTO tasks (id, title, body, status, priority, tags, assignees, project, phase, due, origin_entry_id, anchor_text, created_at, updated_at) VALUES (?, ?, ?, 'todo', 'normal', '[]', '[]', NULL, NULL, NULL, NULL, NULL, ?, ?)")
            .bind(&id)
            .bind(title)
            .bind(body)
            .bind(&created_at)
            .bind(&updated_at)
            .execute(&self.db)
            .await?;
        self.emit("task.created", actor, serde_json::json!({"id": &id, "title": title})).await?;
        row_to_task(&sqlx::query("SELECT * FROM tasks WHERE id = ?").bind(&id).fetch_one(&self.db).await?)
    }

    pub async fn tasks_update(&self, id: &str, patch: TaskPatch, actor: &str) -> Result<Option<Task>> {
        let cur = match self.tasks_get(id).await? {
            Some(t) => t,
            None => return Ok(None),
        };
        let title = patch.title.unwrap_or_else(|| cur.title.clone());
        let body = patch.body.unwrap_or_else(|| cur.body.clone());
        let status = patch.status.unwrap_or(cur.status);
        let priority = patch.priority.unwrap_or(cur.priority);
        let tags = patch.tags.unwrap_or_else(|| cur.tags.clone());
        let assignees = patch.assignees.unwrap_or_else(|| cur.assignees.clone());
        let project = patch.project.unwrap_or_else(|| cur.project.clone());
        let phase = patch.phase.unwrap_or_else(|| cur.phase.clone());
        let due = patch.due.unwrap_or_else(|| cur.due.clone());
        let updated_at = now_iso();

        sqlx::query("UPDATE tasks SET title = ?, body = ?, status = ?, priority = ?, tags = ?, assignees = ?, project = ?, phase = ?, due = ?, updated_at = ? WHERE id = ?")
            .bind(&title)
            .bind(&body)
            .bind(task_status_to_str(status))
            .bind(priority_to_str(priority))
            .bind(serde_json::to_string(&tags)?)
            .bind(serde_json::to_string(&assignees)?)
            .bind(&project)
            .bind(&phase)
            .bind(due.map(|d| d.to_rfc3339()))
            .bind(&updated_at)
            .bind(id)
            .execute(&self.db)
            .await?;

        self.emit("task.updated", actor, serde_json::json!({"id": id, "title": &title, "status": task_status_to_str(status)})).await?;
        self.tasks_get(id).await
    }
}

fn row_to_task(r: &sqlx::sqlite::SqliteRow) -> Result<Task> {
    Ok(Task {
        id: r.try_get("id")?,
        title: r.try_get("title")?,
        body: r.try_get("body")?,
        status: str_to_task_status(r.try_get::<String, _>("status")?.as_str()),
        priority: str_to_priority(r.try_get::<String, _>("priority")?.as_str()),
        tags: serde_json::from_str(r.try_get::<String, _>("tags")?.as_str())?,
        assignees: serde_json::from_str(r.try_get::<String, _>("assignees")?.as_str())?,
        project: r.try_get("project")?,
        phase: r.try_get("phase")?,
        due: r.try_get::<Option<String>, _>("due")?.map(|s| parse_iso(&s)).transpose()?,
        origin_entry_id: r.try_get("origin_entry_id")?,
        anchor_text: r.try_get("anchor_text")?,
        created_at: parse_iso(r.try_get("created_at")?)?,
        updated_at: parse_iso(r.try_get("updated_at")?)?,
    })
}

// ---- decisions ----

impl Store {
    pub async fn decisions_list(&self) -> Result<Vec<Decision>> {
        let rows = sqlx::query("SELECT * FROM decisions ORDER BY updated_at DESC").fetch_all(&self.db).await?;
        rows.iter().map(row_to_decision).collect()
    }

    pub async fn decisions_get(&self, id: &str) -> Result<Option<Decision>> {
        let row = sqlx::query("SELECT * FROM decisions WHERE id = ?").bind(id).fetch_optional(&self.db).await?;
        row.as_ref().map(row_to_decision).transpose()
    }
}

fn row_to_decision(r: &sqlx::sqlite::SqliteRow) -> Result<Decision> {
    Ok(Decision {
        id: r.try_get("id")?,
        title: r.try_get("title")?,
        context: r.try_get("context")?,
        decision: r.try_get("decision")?,
        consequences: r.try_get("consequences")?,
        status: str_to_decision_status(r.try_get::<String, _>("status")?.as_str()),
        tags: serde_json::from_str(r.try_get::<String, _>("tags")?.as_str())?,
        assignees: serde_json::from_str(r.try_get::<String, _>("assignees")?.as_str())?,
        project: r.try_get("project")?,
        supersedes: r.try_get("supersedes")?,
        origin_entry_id: r.try_get("origin_entry_id")?,
        anchor_text: r.try_get("anchor_text")?,
        created_at: parse_iso(r.try_get("created_at")?)?,
        updated_at: parse_iso(r.try_get("updated_at")?)?,
    })
}

// ---- search ----

impl Store {
    pub async fn search(&self, q: &str, limit: i64) -> Result<Vec<SearchHit>> {
        let rows = sqlx::query(r#"SELECT kind, ref_id, title, snippet(search, 0, '<b>', '</b>', '…', 30) AS snippet FROM search WHERE body MATCH ? ORDER BY rank LIMIT ?"#)
            .bind(q)
            .bind(limit)
            .fetch_all(&self.db)
            .await?;
        rows.iter().map(|r| Ok(SearchHit {
            kind: str_to_entity_kind(r.try_get::<String, _>("kind")?.as_str()),
            id: r.try_get("ref_id")?,
            title: r.try_get("title")?,
            snippet: r.try_get("snippet")?,
            score: 0.0,
        })).collect()
    }
}

// ---- users ----

impl Store {
    pub async fn users_count(&self) -> Result<i64> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(&self.db).await?;
        Ok(count)
    }

    pub async fn users_list(&self) -> Result<Vec<SafeUser>> {
        let rows = sqlx::query("SELECT id, actor, email, name, role FROM users ORDER BY created_at").fetch_all(&self.db).await?;
        rows.iter().map(|r| Ok(SafeUser {
            id: r.try_get("id")?,
            actor: r.try_get("actor")?,
            email: r.try_get("email")?,
            name: r.try_get("name")?,
            role: str_to_user_role(r.try_get::<String, _>("role")?.as_str()),
        })).collect()
    }

    pub async fn users_by_email(&self, email: &str) -> Result<Option<(User, String)>> {
        let row = sqlx::query("SELECT * FROM users WHERE email = ?").bind(email.trim().to_lowercase()).fetch_optional(&self.db).await?;
        row.as_ref().map(|r| Ok((User {
            id: r.try_get("id")?,
            actor: r.try_get("actor")?,
            email: r.try_get("email")?,
            name: r.try_get("name")?,
            role: str_to_user_role(r.try_get::<String, _>("role")?.as_str()),
            created_at: parse_iso(r.try_get("created_at")?)?,
            last_login_at: r.try_get::<Option<String>, _>("last_login_at")?.map(|s| parse_iso(&s)).transpose()?,
        }, r.try_get::<String, _>("password_hash")?))).transpose()
    }

    pub async fn users_create(&self, name: &str, email: &str, password: &str, role: UserRole, actor_hint: Option<&str>, by: &str) -> Result<SafeUser> {
        let person = self.people_ensure(actor_hint.unwrap_or(name), ActorKind::Human).await?;
        let id = nanoid("usr");
        let created_at = now_iso();
        let password_hash = crate::auth::hash_password(password)?;
        sqlx::query("INSERT INTO users (id, actor, email, name, role, password_hash, created_at, last_login_at) VALUES (?, ?, ?, ?, ?, ?, ?, NULL)")
            .bind(&id)
            .bind(&person.slug)
            .bind(email.trim().to_lowercase())
            .bind(name)
            .bind(user_role_to_str(role))
            .bind(&password_hash)
            .bind(&created_at)
            .execute(&self.db)
            .await?;
        let user = SafeUser { id, actor: person.slug, email: email.to_string(), name: name.to_string(), role };
        self.emit("user.created", by, serde_json::json!({"id": &user.id, "actor": &user.actor, "role": user_role_to_str(role)})).await?;
        Ok(user)
    }

    pub async fn users_authenticate(&self, email: &str, password: &str) -> Result<Option<User>> {
        let (user, hash) = match self.users_by_email(email).await? {
            Some(uh) => uh,
            None => return Ok(None),
        };
        if !crate::auth::verify_password(password, &hash) {
            return Ok(None);
        }
        let now = now_iso();
        sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&user.id)
            .execute(&self.db)
            .await?;
        Ok(Some(user))
    }
}

// ---- sessions ----

impl Store {
    pub async fn sessions_create(&self, user_id: &str) -> Result<String> {
        let token = crate::auth::generate_token("ses");
        let id = nanoid("ses");
        let created_at = now_iso();
        let expires_at = crate::auth::expire(30).to_rfc3339();
        let hash = crate::auth::token_hash(&token);
        sqlx::query("INSERT INTO sessions (id, token_hash, user_id, created_at, expires_at, last_seen) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(&hash)
            .bind(user_id)
            .bind(&created_at)
            .bind(&expires_at)
            .bind(&created_at)
            .execute(&self.db)
            .await?;
        Ok(token)
    }

    pub async fn sessions_resolve(&self, token: &str) -> Result<Option<User>> {
        let hash = crate::auth::token_hash(token);
        let row = sqlx::query("SELECT s.id, s.user_id, s.expires_at FROM sessions s WHERE s.token_hash = ?")
            .bind(&hash)
            .fetch_optional(&self.db)
            .await?;
        let (session_id, user_id, expires_at) = match row {
            Some(r) => (r.try_get::<String, _>("id")?, r.try_get::<String, _>("user_id")?, r.try_get::<String, _>("expires_at")?),
            None => return Ok(None),
        };
        if parse_iso(&expires_at)? < Utc::now() {
            sqlx::query("DELETE FROM sessions WHERE id = ?").bind(&session_id).execute(&self.db).await?;
            return Ok(None);
        }
        let now = now_iso();
        sqlx::query("UPDATE sessions SET last_seen = ? WHERE id = ?").bind(&now).bind(&session_id).execute(&self.db).await?;
        let user_row = sqlx::query("SELECT * FROM users WHERE id = ?").bind(&user_id).fetch_one(&self.db).await?;
        Ok(Some(User {
            id: user_row.try_get("id")?,
            actor: user_row.try_get("actor")?,
            email: user_row.try_get("email")?,
            name: user_row.try_get("name")?,
            role: str_to_user_role(user_row.try_get::<String, _>("role")?.as_str()),
            created_at: parse_iso(user_row.try_get("created_at")?)?,
            last_login_at: user_row.try_get::<Option<String>, _>("last_login_at")?.map(|s| parse_iso(&s)).transpose()?,
        }))
    }

    pub async fn sessions_destroy(&self, token: &str) -> Result<()> {
        let hash = crate::auth::token_hash(token);
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?").bind(&hash).execute(&self.db).await?;
        Ok(())
    }
}

// ---- api tokens ----

impl Store {
    pub async fn tokens_list(&self) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query("SELECT id, actor, label, created_by, created_at, last_used_at, kind, client_id, granted_by, scope, expires_at FROM api_tokens ORDER BY created_at DESC").fetch_all(&self.db).await?;
        rows.iter().map(row_to_token).collect()
    }

    pub async fn tokens_resolve(&self, token: &str) -> Result<Option<String>> {
        let hash = crate::auth::token_hash(token);
        let row: Option<String> = sqlx::query_scalar("SELECT actor FROM api_tokens WHERE id = ? AND (expires_at IS NULL OR expires_at > ?)")
            .bind(&hash)
            .bind(Utc::now().to_rfc3339())
            .fetch_optional(&self.db)
            .await?;
        Ok(row)
    }
}

fn row_to_token(r: &sqlx::sqlite::SqliteRow) -> Result<ApiToken> {
    Ok(ApiToken {
        id: r.try_get("id")?,
        actor: r.try_get("actor")?,
        label: r.try_get("label")?,
        created_at: parse_iso(r.try_get("created_at")?)?,
        last_used_at: r.try_get::<Option<String>, _>("last_used_at")?.map(|s| parse_iso(&s)).transpose()?,
        created_by: r.try_get("created_by")?,
        expires_at: r.try_get::<Option<String>, _>("expires_at")?.map(|s| parse_iso(&s)).transpose()?,
        kind: r.try_get::<Option<String>, _>("kind")?.map(|s| if s == "oauth" { TokenKind::OAuth } else { TokenKind::Pat }),
        client_id: r.try_get("client_id")?,
        granted_by: r.try_get("granted_by")?,
        scope: r.try_get("scope")?,
    })
}

// ---- config ----

impl Store {
    pub async fn config_get(&self, key: &str) -> Result<Option<String>> {
        let value: Option<String> = sqlx::query_scalar("SELECT value FROM config WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.db)
            .await?;
        Ok(value)
    }

    pub async fn config_set(&self, key: &str, value: &str) -> Result<()> {
        let updated_at = now_iso();
        sqlx::query("INSERT INTO config (key, value, updated_at) VALUES (?, ?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at")
            .bind(key)
            .bind(value)
            .bind(&updated_at)
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

// ---- recall ----

impl Store {
    pub async fn recall(&self, identity: &str, peer: Option<&str>, peer_platform: Option<&str>, peer_platform_id: Option<&str>, query: Option<&str>, budget: Option<usize>) -> Result<RecallResult> {
        let budget = budget.unwrap_or(RECALL_DEFAULT_BUDGET);
        let peer = if let Some(p) = peer {
            Some(p.to_string())
        } else if let (Some(platform), Some(pid)) = (peer_platform, peer_platform_id) {
            self.identities_resolve(platform, pid).await?
        } else {
            None
        };

        let mut profiles = vec![];
        if let Some(card) = self.profile_get(identity).await? {
            profiles.push(card);
        }
        if let Some(ref p) = peer {
            if let Some(card) = self.profile_get(p).await? {
                profiles.push(card);
            }
        }

        let tasks = self.tasks_list(None, Some(TaskStatus::Todo)).await?;
        let inbox = self.inbox_list(identity, true).await?;
        let journal = self.journal_list(20, peer.as_deref()).await?.into_iter().map(|e| RecallJournalHit { entry: e, anchors: vec![], similarity: None }).collect();
        let events = vec![]; // TODO: implement events store
        let projects = self.projects_list().await?;

        let brief = format!(
            "# Session brief for @{identity}\n\n## Profile\n{}\n\n## Open tasks ({}): {}\n\n## Unread inbox ({}): {}\n\n## Recent journal: {} entries\n\n## Projects: {}\n",
            profiles.iter().map(|p| format!("- {}: {}", p.display_name, p.body.sections.get("role").cloned().unwrap_or_default())).collect::<Vec<_>>().join("\n"),
            tasks.len(),
            tasks.iter().map(|t| format!("- [{}] {}", task_status_to_str(t.status), t.title)).collect::<Vec<_>>().join("\n"),
            inbox.len(),
            inbox.iter().map(|i| format!("- [{}] {}", entity_kind_to_str(i.ref_kind), i.snippet)).collect::<Vec<_>>().join("\n"),
            journal.len(),
            projects.iter().map(|p| format!("- {}", p.name)).collect::<Vec<_>>().join("\n"),
        );

        Ok(RecallResult { profile: profiles, tasks, inbox, journal, events, projects, brief })
    }
}

// ---- helpers ----

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .replace(|c: char| c.is_whitespace(), "-")
        .replace(|c: char| !c.is_alphanumeric() && c != '-', "")
}

fn parse_iso(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))
}

fn kind_to_str(k: ActorKind) -> &'static str {
    match k { ActorKind::Human => "human", ActorKind::Ai => "ai" }
}

fn str_to_kind(s: &str) -> ActorKind {
    match s { "ai" => ActorKind::Ai, _ => ActorKind::Human }
}

fn task_status_to_str(s: TaskStatus) -> &'static str {
    match s { TaskStatus::Todo => "todo", TaskStatus::Doing => "doing", TaskStatus::Blocked => "blocked", TaskStatus::Done => "done" }
}

fn str_to_task_status(s: &str) -> TaskStatus {
    match s { "doing" => TaskStatus::Doing, "blocked" => TaskStatus::Blocked, "done" => TaskStatus::Done, _ => TaskStatus::Todo }
}

fn priority_to_str(p: Priority) -> &'static str {
    match p { Priority::Low => "low", Priority::Normal => "normal", Priority::High => "high", Priority::Urgent => "urgent" }
}

fn str_to_priority(s: &str) -> Priority {
    match s { "low" => Priority::Low, "high" => Priority::High, "urgent" => Priority::Urgent, _ => Priority::Normal }
}

fn decision_status_to_str(s: DecisionStatus) -> &'static str {
    match s { DecisionStatus::Proposed => "proposed", DecisionStatus::Accepted => "accepted", DecisionStatus::Rejected => "rejected", DecisionStatus::Superseded => "superseded" }
}

fn str_to_decision_status(s: &str) -> DecisionStatus {
    match s { "accepted" => DecisionStatus::Accepted, "rejected" => DecisionStatus::Rejected, "superseded" => DecisionStatus::Superseded, _ => DecisionStatus::Proposed }
}

fn inbox_reason_to_str(r: InboxReason) -> &'static str {
    match r { InboxReason::Mention => "mention", InboxReason::Assignment => "assignment", InboxReason::Decision => "decision", InboxReason::Event => "event" }
}

fn str_to_inbox_reason(s: &str) -> InboxReason {
    match s { "assignment" => InboxReason::Assignment, "decision" => InboxReason::Decision, "event" => InboxReason::Event, _ => InboxReason::Mention }
}

fn entity_kind_to_str(k: EntityKind) -> &'static str {
    match k {
        EntityKind::Task => "task", EntityKind::Decision => "decision", EntityKind::Event => "event",
        EntityKind::Journal => "journal", EntityKind::Person => "person", EntityKind::Topic => "topic",
        EntityKind::Project => "project", EntityKind::Phase => "phase",
    }
}

fn str_to_entity_kind(s: &str) -> EntityKind {
    match s {
        "decision" => EntityKind::Decision, "event" => EntityKind::Event, "journal" => EntityKind::Journal,
        "person" => EntityKind::Person, "topic" => EntityKind::Topic, "project" => EntityKind::Project,
        "phase" => EntityKind::Phase, _ => EntityKind::Task,
    }
}

fn user_role_to_str(r: UserRole) -> &'static str {
    match r { UserRole::Admin => "admin", UserRole::Member => "member" }
}

fn str_to_user_role(s: &str) -> UserRole {
    match s { "admin" => UserRole::Admin, _ => UserRole::Member }
}
