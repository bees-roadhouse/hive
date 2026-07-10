// Journal append/list/get + anchors + bracket-token refs. Parity port of
// store.ts `journal`, `anchorsFor`/`refsFor`, `materialiseAnchor`,
// `parseBracketTokens`, `journalWriters`.
//
// The cutover shape (D18): journal_append is ONE logical write = ONE record
// batch. The command layer parses emergence here — bracket tokens, anchor
// materialisation, mention fan-out — and pre-computes EVERYTHING into the
// journal.append payload (anchors, emerged entity-creates, inbox rows), with
// links and side-effect updates (decision supersedes) as separate records in
// the same batch. The fold applies it all in one SQLite transaction: the
// atomicity the Postgres path never had. Find-or-create consults the pending
// batch as well as the index, so two [topic: X] tokens in one entry emerge
// one topic.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use hive_shared::{
    parse_mentions, slugify, ActorKind, Anchor, AnchorFields, AnchorKind, DecisionStatus,
    EntityKind, InboxReason, JournalEntry, JournalEntryView, JournalRef, JournalWriter, NewAnchor,
    NewJournalEntry, Person, Priority, ResolvedAnchor, TaskStatus, ACTORS,
};
use rusqlite::OptionalExtension;
use serde_json::{json, Value as Json};

use super::{json_vec, new_id, now_iso, Core, Draft, Store};

/// The in-flight state of one journal_append: everything emerging from the
/// entry, accumulated before the single commit.
struct Emergence {
    /// Pre-materialized entity.create payloads (the `emerged` array).
    emerged: Vec<Json>,
    /// Pre-computed inbox fan-out rows (the `inbox` array).
    inbox: Vec<Json>,
    /// link.add / entity.update records that ride the same batch.
    extra: Vec<Draft>,
    /// Anchor rows (the `anchors` array).
    anchors: Vec<Json>,
    /// Pending find-or-create results, keyed by slug (or composite), so a
    /// second token in the same entry reuses the first's id.
    topics: HashMap<String, (String, String)>, // slug -> (id, name)
    projects: HashMap<String, (String, String)>, // slug -> (id, name)
    phases: HashMap<(String, String), String>,   // (project id, lower name) -> id
    people: HashMap<String, Person>,             // slug -> person
    phase_next_pos: HashMap<String, i64>,        // project id -> next position
}

impl Emergence {
    fn new() -> Self {
        Emergence {
            emerged: Vec::new(),
            inbox: Vec::new(),
            extra: Vec::new(),
            anchors: Vec::new(),
            topics: HashMap::new(),
            projects: HashMap::new(),
            phases: HashMap::new(),
            people: HashMap::new(),
            phase_next_pos: HashMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_inbox(
        &mut self,
        recipient: &str,
        from: &str,
        reason: InboxReason,
        ref_kind: &str,
        ref_id: &str,
        entry_id: Option<&str>,
        snippet: &str,
    ) {
        if recipient == from {
            return; // don't notify yourself (inbox_add parity)
        }
        self.inbox.push(super::inbox::inbox_payload_item(
            recipient,
            from,
            reason,
            ref_kind,
            ref_id,
            entry_id,
            snippet,
            &now_iso(),
        ));
    }
}

impl Store {
    pub async fn journal_list(&self, limit: i64, offset: i64) -> Result<Vec<JournalEntryView>> {
        self.run(move |core| {
            let entries: Vec<JournalEntry> = {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT * FROM journal ORDER BY created_at DESC LIMIT ?1 OFFSET ?2")?;
                let rows = stmt.query_map(rusqlite::params![limit, offset], row_to_entry)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            entries.into_iter().map(|e| entry_view(core, e)).collect()
        })
        .await
    }

    pub async fn journal_get(&self, entry_id: &str) -> Result<Option<JournalEntryView>> {
        let entry_id = entry_id.to_string();
        self.run(move |core| {
            let entry = core
                .conn()
                .query_row(
                    "SELECT * FROM journal WHERE id = ?1",
                    rusqlite::params![entry_id],
                    row_to_entry,
                )
                .optional()?;
            entry.map(|e| entry_view(core, e)).transpose()
        })
        .await
    }

    /// The one write path. Persist immutable prose, then materialise each anchored
    /// span into a structured entity and fan out inbox notifications. Also parses
    /// inline [person:], [topic:], [project:], [phase:], [task:] tokens to
    /// emerge/link entities and feed inboxes. One record batch, one fold
    /// transaction.
    pub async fn journal_append(
        &self,
        input: NewJournalEntry,
        actor_override: Option<&str>,
        user_scope: Option<&str>,
    ) -> Result<JournalEntryView> {
        let author = actor_override
            .map(String::from)
            .or_else(|| input.author.clone())
            .ok_or_else(|| anyhow!("author required"))?;
        let user_scope = user_scope.map(String::from);
        let author_for_emit = author.clone();

        let view = self
            .run(move |core| {
                let mentions = parse_mentions(&input.body);
                let entry = JournalEntry {
                    id: new_id("jrnl"),
                    author: author.clone(),
                    body: input.body.clone(),
                    tags: input.tags.clone().unwrap_or_default(),
                    mentions: mentions.clone(),
                    user_scope: user_scope.clone(),
                    created_at: now_iso(),
                };

                let mut em = Emergence::new();
                let mut assigned: HashSet<String> = HashSet::new();
                for a in input.anchors.as_deref().unwrap_or_default() {
                    materialise_anchor(core, &entry, a, &author, &mut assigned, &mut em)?;
                }
                parse_bracket_tokens_into(core, &entry, &author, &mut assigned, &mut em)?;

                // Anyone @mentioned but not already pulled into an anchor gets a
                // plain "mention" inbox item — humans and AIs alike.
                for m in &mentions {
                    if !assigned.contains(m) {
                        em.add_inbox(
                            m,
                            &author,
                            InboxReason::Mention,
                            EntityKind::Journal.as_str(),
                            &entry.id,
                            Some(&entry.id),
                            &input.body,
                        );
                    }
                }

                let payload = json!({
                    "id": entry.id,
                    "author": entry.author,
                    "body": entry.body,
                    "tags": entry.tags,
                    "mentions": entry.mentions,
                    "user_scope": entry.user_scope,
                    "created_at": entry.created_at,
                    "anchors": em.anchors,
                    "emerged": em.emerged,
                    "inbox": em.inbox,
                });
                let mut batch = vec![Draft::new(
                    crate::oplog::kind::JOURNAL_APPEND,
                    &author,
                    &entry.created_at,
                    payload,
                )];
                batch.extend(em.extra);
                core.commit(batch)?;

                entry_view(core, entry)
            })
            .await?;

        self.emit(
            "journal.created",
            &author_for_emit,
            json!({"id": view.entry.id, "anchors": view.anchors.len()}),
        )
        .await?;
        Ok(view)
    }

    /// Anchors for an entry, each with its resolved entity (Node `anchorsFor`).
    pub async fn anchors_for(&self, entry_id: &str) -> Result<Vec<ResolvedAnchor>> {
        let entry_id = entry_id.to_string();
        self.run(move |core| anchors_for_conn(core, &entry_id))
            .await
    }

    /// Resolve bracket tokens in a body string against the DB at read time
    /// (Node `refsFor`).
    pub async fn refs_for(&self, body: &str) -> Result<Vec<JournalRef>> {
        let body = body.to_string();
        self.run(move |core| refs_for_conn(core, &body)).await
    }

    /// Every journal author, with their people row when one exists (Node
    /// `journalWriters`, unscoped — single user sees all writers).
    pub async fn journal_writers(&self) -> Result<Vec<JournalWriter>> {
        self.run(|core| {
            let slugs: Vec<String> = {
                let mut stmt = core.conn().prepare("SELECT DISTINCT author FROM journal")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut result = Vec::with_capacity(slugs.len());
            for slug in &slugs {
                match super::people::person_get(core.conn(), slug)? {
                    Some(p) => result.push(JournalWriter {
                        slug: p.slug,
                        name: p.name,
                        kind: p.kind,
                        owner: p.owner,
                    }),
                    // Author may not be in the people table — return a minimal record.
                    None => result.push(JournalWriter {
                        slug: slug.clone(),
                        name: slug.clone(),
                        kind: ActorKind::Human,
                        owner: None,
                    }),
                }
            }
            result.sort_by(|a, b| a.slug.cmp(&b.slug));
            Ok(result)
        })
        .await
    }
}

// ── emergence (command layer; everything lands in the Emergence acc) ────────

fn materialise_anchor(
    core: &Core,
    entry: &JournalEntry,
    a: &NewAnchor,
    author: &str,
    assigned: &mut HashSet<String>,
    em: &mut Emergence,
) -> Result<()> {
    let text = js_slice_utf16(&entry.body, a.start, a.end)
        .trim()
        .to_string();
    if text.is_empty() {
        return Ok(());
    }
    let f: AnchorFields = a.fields.clone().unwrap_or_default();
    let span_mentions = parse_mentions(&text);
    // Auto-assign to the entry author when no explicit assignees and no @mentions in the span.
    let raw_assignees = f.assignees.clone().unwrap_or_else(|| {
        if span_mentions.is_empty() {
            vec![author.to_string()]
        } else {
            span_mentions
        }
    });
    let assignees: Vec<String> = raw_assignees
        .iter()
        .filter(|x| x.as_str() != author)
        .cloned()
        .collect();
    let assignees_for_task = if raw_assignees.is_empty() {
        vec![author.to_string()]
    } else {
        raw_assignees.clone()
    };
    let title_src = f.title.clone().unwrap_or_else(|| {
        text.split(['.', '\n'])
            .next()
            .unwrap_or_default()
            .to_string()
    });
    let title = js_slice_utf16(&title_src, 0, 120).trim().to_string();

    let (ref_id, reason, ref_kind) = match a.kind {
        AnchorKind::Task => {
            let project = resolve_project_value(core, em, f.project.clone().flatten())?;
            let ts = now_iso();
            let t = hive_shared::Task {
                id: new_id("task"),
                title,
                body: text.clone(),
                status: f
                    .status
                    .as_deref()
                    .map(TaskStatus::from_str_lossy)
                    .unwrap_or(TaskStatus::Todo),
                priority: f.priority.unwrap_or(Priority::Normal),
                tags: f.tags.clone().unwrap_or_default(),
                assignees: assignees_for_task.clone(),
                project,
                phase: None,
                due: None,
                origin_entry_id: Some(entry.id.clone()),
                anchor_text: Some(text.clone()),
                created_at: ts.clone(),
                updated_at: ts,
            };
            em.emerged.push(super::tasks::task_create_payload(&t));
            (t.id, InboxReason::Assignment, EntityKind::Task)
        }
        AnchorKind::Decision => {
            let project = resolve_project_value(core, em, f.project.clone().flatten())?;
            let ts = now_iso();
            let d = hive_shared::Decision {
                id: new_id("dec"),
                title,
                context: f.context.clone().unwrap_or_default(),
                decision: f.decision.clone().unwrap_or_else(|| text.clone()),
                consequences: f.consequences.clone().unwrap_or_default(),
                status: f
                    .status
                    .as_deref()
                    .map(DecisionStatus::from_str_lossy)
                    .unwrap_or(DecisionStatus::Proposed),
                tags: f.tags.clone().unwrap_or_default(),
                assignees: assignees.clone(),
                project,
                supersedes: f.supersedes.clone().flatten(),
                origin_entry_id: Some(entry.id.clone()),
                anchor_text: Some(text.clone()),
                created_at: ts.clone(),
                updated_at: ts,
            };
            em.emerged
                .push(super::decisions::decision_create_payload(&d));
            // Supersedes side effect: prior decision flips + link, same batch.
            if let Some(supersedes) = &d.supersedes {
                if let Some(prior) = super::decisions::decision_get(core, supersedes)? {
                    let ts2 = now_iso();
                    em.extra.push(Draft::new(
                        crate::oplog::kind::ENTITY_UPDATE,
                        author,
                        &ts2,
                        json!({"kind": "decision", "id": prior.id, "fields": {
                            "status": "superseded", "updated_at": ts2,
                        }}),
                    ));
                    em.extra.push(super::links::link_draft(
                        EntityKind::Decision.as_str(),
                        &d.id,
                        EntityKind::Decision.as_str(),
                        &prior.id,
                        "supersedes",
                        &ts2,
                    ));
                }
            }
            (d.id, InboxReason::Decision, EntityKind::Decision)
        }
        AnchorKind::Event => {
            let e = hive_shared::EventItem {
                id: new_id("evt"),
                title,
                body: text.clone(),
                at: f.at.clone().flatten(),
                tags: f.tags.clone().unwrap_or_default(),
                assignees: assignees.clone(),
                origin_entry_id: Some(entry.id.clone()),
                anchor_text: Some(text.clone()),
                created_at: now_iso(),
            };
            em.emerged.push(super::events::event_create_payload(&e));
            (e.id, InboxReason::Event, EntityKind::Event)
        }
    };

    em.anchors.push(json!({
        "id": new_id("anc"),
        "start": a.start, "end": a.end, "text": text,
        "kind": a.kind.as_str(), "ref_id": ref_id,
        "created_at": now_iso(),
    }));
    em.extra.push(super::links::link_draft(
        EntityKind::Journal.as_str(),
        &entry.id,
        ref_kind.as_str(),
        &ref_id,
        "anchors",
        &now_iso(),
    ));

    // For inbox delivery use the full assignee list (including author when auto-assigned).
    let recipients = if a.kind == AnchorKind::Task {
        &assignees_for_task
    } else {
        &assignees
    };
    for who in recipients {
        assigned.insert(who.clone());
        em.add_inbox(
            who,
            author,
            reason,
            ref_kind.as_str(),
            &ref_id,
            Some(&entry.id),
            &text,
        );
    }
    Ok(())
}

/// Anchor `fields.project`: a known project id passes through; anything else
/// find-or-creates by name (batch-aware).
fn resolve_project_value(
    core: &Core,
    em: &mut Emergence,
    project: Option<String>,
) -> Result<Option<String>> {
    let Some(project) = project else {
        return Ok(None);
    };
    if super::projects::project_get(core.conn(), &project)?.is_some()
        || em.projects.values().any(|(id, _)| *id == project)
    {
        return Ok(Some(project));
    }
    let (id, _name) = ensure_project(core, em, &project)?;
    Ok(Some(id))
}

/// Batch-aware find-or-create: pending map → index → mint into `emerged`.
fn ensure_project(core: &Core, em: &mut Emergence, name: &str) -> Result<(String, String)> {
    let slug = slugify(name);
    if let Some(hit) = em.projects.get(&slug) {
        return Ok(hit.clone());
    }
    if let Some(p) = super::projects::project_by_slug(core.conn(), &slug)? {
        let hit = (p.id.clone(), p.name.clone());
        em.projects.insert(slug, hit.clone());
        return Ok(hit);
    }
    let id = new_id("proj");
    let ts = now_iso();
    em.emerged
        .push(json!({"kind": "project", "id": id, "fields": {
            "name": name, "slug": slug, "created_at": ts,
        }}));
    let hit = (id, name.to_string());
    em.projects.insert(slug, hit.clone());
    Ok(hit)
}

fn ensure_topic(core: &Core, em: &mut Emergence, name: &str) -> Result<(String, String)> {
    let slug = slugify(name);
    if let Some(hit) = em.topics.get(&slug) {
        return Ok(hit.clone());
    }
    if let Some(t) = super::topics::topic_by_slug(core.conn(), &slug)? {
        let hit = (t.id.clone(), t.name.clone());
        em.topics.insert(slug, hit.clone());
        return Ok(hit);
    }
    let id = new_id("top");
    let ts = now_iso();
    em.emerged
        .push(json!({"kind": "topic", "id": id, "fields": {
            "name": name, "slug": slug, "created_at": ts,
        }}));
    let hit = (id, name.to_string());
    em.topics.insert(slug, hit.clone());
    Ok(hit)
}

fn ensure_phase(core: &Core, em: &mut Emergence, project_id: &str, name: &str) -> Result<String> {
    let key = (project_id.to_string(), name.to_lowercase());
    if let Some(id) = em.phases.get(&key) {
        return Ok(id.clone());
    }
    let existing: Option<String> = core
        .conn()
        .query_row(
            "SELECT id FROM phases WHERE project = ?1 AND LOWER(name) = LOWER(?2)",
            rusqlite::params![project_id, name],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        em.phases.insert(key, id.clone());
        return Ok(id);
    }
    let pos = match em.phase_next_pos.get(project_id) {
        Some(p) => *p,
        None => core.conn().query_row(
            "SELECT COALESCE(MAX(position)+1, 0) FROM phases WHERE project = ?1",
            rusqlite::params![project_id],
            |r| r.get(0),
        )?,
    };
    em.phase_next_pos.insert(project_id.to_string(), pos + 1);
    let id = new_id("ph");
    let ts = now_iso();
    em.emerged
        .push(json!({"kind": "phase", "id": id, "fields": {
            "project": project_id, "name": name, "position": pos, "created_at": ts,
        }}));
    em.phases.insert(key, id.clone());
    Ok(id)
}

fn ensure_person(core: &Core, em: &mut Emergence, name: &str, kind: ActorKind) -> Result<Person> {
    let slug = slugify(name);
    if let Some(p) = em.people.get(&slug) {
        return Ok(p.clone());
    }
    if let Some(p) = super::people::person_by_slug(core.conn(), &slug)? {
        em.people.insert(slug, p.clone());
        return Ok(p);
    }
    let p = Person {
        id: new_id("per"),
        name: name.to_string(),
        slug: slug.clone(),
        kind,
        owner: None,
        bio: None,
        role: None,
        created_at: now_iso(),
    };
    em.emerged
        .push(json!({"kind": "person", "id": p.id, "fields": {
            "slug": p.slug, "name": p.name, "kind": p.kind.as_str(),
            "owner": null, "bio": null, "role": null, "created_at": p.created_at,
        }}));
    em.people.insert(slug, p.clone());
    Ok(p)
}

/// Parse [person:], [topic:], [project:], [phase:], [task:] tokens from an
/// entry body. Find-or-create each entity, add a links record, and fan to
/// inboxes where relevant. Context tracking: if the entry mentions a
/// [project:] and/or [phase:], any [task:] that emerges is related to it.
fn parse_bracket_tokens_into(
    core: &Core,
    entry: &JournalEntry,
    author: &str,
    assigned: &mut HashSet<String>,
    em: &mut Emergence,
) -> Result<()> {
    let tokens = scan_tokens(&entry.body);

    // First pass: collect context (project + phase referenced in this entry).
    let mut ctx_project: Option<String> = None;
    let mut ctx_phase: Option<String> = None;
    for t in &tokens {
        match t.kind {
            "project" => {
                let (id, _) = ensure_project(core, em, &t.name)?;
                ctx_project = Some(id);
            }
            "phase" => {
                if let Some(pid) = &ctx_project {
                    let id = ensure_phase(core, em, pid, &t.name)?;
                    ctx_phase = Some(id);
                }
            }
            _ => {}
        }
    }

    // Second pass: process all tokens.
    for t in &tokens {
        match t.kind {
            "person" => {
                // Resolve against ACTORS first (known actors), then ensure as a people row.
                let slug = slugify(&t.name);
                let actor_match = ACTORS
                    .iter()
                    .find(|(n, _)| *n == slug || slugify(n) == slug);
                let person = match actor_match {
                    Some((n, k)) => ensure_person(core, em, &capitalize(n), *k)?,
                    None => ensure_person(core, em, &t.name, ActorKind::Human)?,
                };
                em.extra.push(super::links::link_draft(
                    EntityKind::Journal.as_str(),
                    &entry.id,
                    EntityKind::Person.as_str(),
                    &person.id,
                    "mentions",
                    &now_iso(),
                ));
                // Fan to inbox if this person is a known actor (same as @mention).
                if let Some((n, _)) = actor_match {
                    assigned.insert((*n).to_string());
                    em.add_inbox(
                        n,
                        author,
                        InboxReason::Mention,
                        EntityKind::Journal.as_str(),
                        &entry.id,
                        Some(&entry.id),
                        &entry.body,
                    );
                }
            }
            "topic" => {
                let (topic_id, _) = ensure_topic(core, em, &t.name)?;
                em.extra.push(super::links::link_draft(
                    EntityKind::Journal.as_str(),
                    &entry.id,
                    EntityKind::Topic.as_str(),
                    &topic_id,
                    "tagged",
                    &now_iso(),
                ));
            }
            "project" => {
                let (proj_id, _) = ensure_project(core, em, &t.name)?;
                em.extra.push(super::links::link_draft(
                    EntityKind::Journal.as_str(),
                    &entry.id,
                    EntityKind::Project.as_str(),
                    &proj_id,
                    "about",
                    &now_iso(),
                ));
            }
            "phase" => {
                if let Some(pid) = &ctx_project {
                    let ph_id = ensure_phase(core, em, pid, &t.name)?;
                    em.extra.push(super::links::link_draft(
                        EntityKind::Journal.as_str(),
                        &entry.id,
                        EntityKind::Phase.as_str(),
                        &ph_id,
                        "about",
                        &now_iso(),
                    ));
                }
            }
            "task" => {
                // Emerge a task anchored to this entry, auto-assigned to the author.
                let ts = now_iso();
                let task = hive_shared::Task {
                    id: new_id("task"),
                    title: t.name.clone(),
                    body: String::new(),
                    status: TaskStatus::Todo,
                    priority: Priority::Normal,
                    tags: Vec::new(),
                    assignees: vec![author.to_string()],
                    project: ctx_project.clone(),
                    phase: ctx_phase.clone(),
                    due: None,
                    origin_entry_id: Some(entry.id.clone()),
                    anchor_text: Some(t.name.clone()),
                    created_at: ts.clone(),
                    updated_at: ts,
                };
                em.emerged.push(super::tasks::task_create_payload(&task));
                em.extra.push(super::links::link_draft(
                    EntityKind::Journal.as_str(),
                    &entry.id,
                    EntityKind::Task.as_str(),
                    &task.id,
                    "anchors",
                    &now_iso(),
                ));
                // author is assigned; add_inbox silently skips self-notification.
                em.add_inbox(
                    author,
                    author,
                    InboxReason::Assignment,
                    EntityKind::Task.as_str(),
                    &task.id,
                    Some(&entry.id),
                    &t.name,
                );
            }
            "mail" => {
                // [mail:<id>] cites an archived message: a links record only —
                // no entity emerges, no anchor (anchors stay journal spans;
                // a task cites the ENTRY, never the email), no inbox fan.
                // Write-time scope gate: you can only cite mail whose owner
                // matches the entry's effective scope (its user_scope, or
                // the author's namespace for global entries) — a token
                // naming someone else's mail simply doesn't link (D9:
                // owner-only, no piercing).
                let token = t.name.trim();
                let effective_scope = entry
                    .user_scope
                    .clone()
                    .unwrap_or_else(|| author.to_string());
                let visible: Option<String> = core
                    .conn()
                    .query_row(
                        "SELECT id FROM mail_messages \
                         WHERE id = ?1 AND user_scope = ?2 AND deleted_at IS NULL",
                        rusqlite::params![token, effective_scope],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(mail_id) = visible {
                    em.extra.push(super::links::link_draft(
                        EntityKind::Journal.as_str(),
                        &entry.id,
                        EntityKind::Mail.as_str(),
                        &mail_id,
                        "cites",
                        &now_iso(),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ── read-side composition ────────────────────────────────────────────────────

pub(crate) fn anchors_for_conn(core: &Core, entry_id: &str) -> Result<Vec<ResolvedAnchor>> {
    struct Raw {
        anchor: Anchor,
        kind_str: String,
        ref_id: String,
    }
    let raws: Vec<Raw> = {
        let mut stmt = core.conn().prepare(
            r#"SELECT id, entry_id, start, "end", text, kind, ref_id, created_at FROM anchors WHERE entry_id = ?1 ORDER BY start"#,
        )?;
        let rows = stmt.query_map(rusqlite::params![entry_id], |r| {
            let kind_str: String = r.get("kind")?;
            let ref_id: String = r.get("ref_id")?;
            Ok(Raw {
                anchor: Anchor {
                    id: r.get("id")?,
                    entry_id: r.get("entry_id")?,
                    start: r.get("start")?,
                    end: r.get("end")?,
                    text: r.get("text")?,
                    kind: AnchorKind::parse(&kind_str).unwrap_or(AnchorKind::Task),
                    ref_id: ref_id.clone(),
                    created_at: r.get("created_at")?,
                },
                kind_str,
                ref_id,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut out = Vec::with_capacity(raws.len());
    for raw in raws {
        let entity = entity_by_id(core, &raw.kind_str, &raw.ref_id)?;
        out.push(ResolvedAnchor {
            anchor: raw.anchor,
            entity,
        });
    }
    Ok(out)
}

/// Node `entityById` — Task | Decision | EventItem | null as JSON.
fn entity_by_id(core: &Core, kind: &str, ref_id: &str) -> Result<Json> {
    let conn = core.conn();
    Ok(match kind {
        "task" => conn
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                rusqlite::params![ref_id],
                super::tasks::row_to_task,
            )
            .optional()?
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Json::Null),
        "decision" => conn
            .query_row(
                "SELECT * FROM decisions WHERE id = ?1",
                rusqlite::params![ref_id],
                super::decisions::row_to_decision,
            )
            .optional()?
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Json::Null),
        "event" => conn
            .query_row(
                "SELECT * FROM events WHERE id = ?1",
                rusqlite::params![ref_id],
                super::events::row_to_event,
            )
            .optional()?
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or(Json::Null),
        _ => Json::Null,
    })
}

pub(crate) fn refs_for_conn(core: &Core, body: &str) -> Result<Vec<JournalRef>> {
    let conn = core.conn();
    let mut refs = Vec::new();
    for t in scan_tokens(body) {
        let start = utf16_len(&body[..t.start_byte]);
        let end = utf16_len(&body[..t.end_byte]);
        let resolved: Option<(String, String, String)> = match t.kind {
            "person" => super::people::person_by_slug(conn, &slugify(&t.name))?
                .map(|p| (p.id, p.slug, p.name)),
            "topic" => super::topics::topic_by_slug(conn, &slugify(&t.name))?
                .map(|x| (x.id, x.slug, x.name)),
            "project" => super::projects::project_by_slug(conn, &slugify(&t.name))?
                .map(|x| (x.id, x.slug, x.name)),
            "phase" => {
                // phase resolution without a project context: find by name across all phases
                conn.query_row(
                    "SELECT id, name FROM phases WHERE LOWER(name) = LOWER(?1) LIMIT 1",
                    rusqlite::params![t.name],
                    |r| {
                        let id: String = r.get(0)?;
                        let name: String = r.get(1)?;
                        Ok((id, slugify(&name), name))
                    },
                )
                .optional()?
            }
            // mail — id-addressed; the chip renders the subject. Live
            // rows only (tombstoned/redacted mail resolves to nothing and
            // the raw token stays visible — honest about a dead citation).
            "mail" => conn
                .query_row(
                    "SELECT id, subject FROM mail_messages WHERE id = ?1 AND deleted_at IS NULL",
                    rusqlite::params![t.name.trim()],
                    |r| {
                        let id: String = r.get(0)?;
                        let subject: String = r.get(1)?;
                        Ok((id, subject))
                    },
                )
                .optional()?
                .map(|(id, subject)| {
                    let name = if subject.trim().is_empty() {
                        "(no subject)".to_string()
                    } else {
                        subject
                    };
                    (id.clone(), id, name)
                }),
            // task — find the most recent task with matching title
            _ => conn
                .query_row(
                    "SELECT id, title FROM tasks WHERE LOWER(title) = LOWER(?1) ORDER BY created_at DESC LIMIT 1",
                    rusqlite::params![t.name],
                    |r| {
                        let id: String = r.get(0)?;
                        let title: String = r.get(1)?;
                        Ok((id, slugify(&title), title))
                    },
                )
                .optional()?,
        };
        if let Some((id, slug, name)) = resolved {
            // TOKEN_KINDS is a subset of EntityKind strings, so this never
            // skips today; parse keeps it fail-closed if they ever drift.
            if let Some(kind) = EntityKind::parse(t.kind) {
                refs.push(JournalRef {
                    kind,
                    id,
                    slug,
                    name,
                    start,
                    end,
                });
            }
        }
    }
    Ok(refs)
}

pub(crate) fn entry_view(core: &Core, entry: JournalEntry) -> Result<JournalEntryView> {
    Ok(JournalEntryView {
        anchors: anchors_for_conn(core, &entry.id)?,
        refs: refs_for_conn(core, &entry.body)?,
        entry,
    })
}

pub(crate) fn row_to_entry(r: &rusqlite::Row) -> rusqlite::Result<JournalEntry> {
    Ok(JournalEntry {
        id: r.get("id")?,
        author: r.get("author")?,
        body: r.get("body")?,
        tags: json_vec(r.get::<_, String>("tags")?.as_str()),
        mentions: json_vec(r.get::<_, String>("mentions")?.as_str()),
        user_scope: r.get("user_scope")?,
        created_at: r.get("created_at")?,
    })
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// JS `String.prototype.slice` over UTF-16 code units — anchor offsets come from
/// the browser, so they index UTF-16, not bytes or chars.
fn js_slice_utf16(s: &str, start: i64, end: i64) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    let len = units.len() as i64;
    let norm = |i: i64| -> usize {
        let v = if i < 0 { len + i } else { i };
        v.clamp(0, len) as usize
    };
    let (a, b) = (norm(start), norm(end));
    if a >= b {
        return String::new();
    }
    String::from_utf16_lossy(&units[a..b])
}

fn utf16_len(s: &str) -> i64 {
    s.encode_utf16().count() as i64
}

/// One bracket token: `[kind:name]`. Byte offsets into the body.
struct BracketToken {
    kind: &'static str,
    /// Trimmed name (Node trims `m[2]`).
    name: String,
    start_byte: usize,
    end_byte: usize,
}

const TOKEN_KINDS: &[&str] = &["person", "topic", "project", "phase", "task", "mail"];

/// Node TOKEN_RE, plus the Rust-side `mail` addition (id-addressed):
/// /\[(person|topic|project|phase|task|mail):([^\]]+)\]/g
fn scan_tokens(body: &str) -> Vec<BracketToken> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(tok) = match_token_at(body, i) {
                i = tok.end_byte;
                out.push(tok);
                continue;
            }
        }
        i += 1;
    }
    out
}

fn match_token_at(body: &str, open: usize) -> Option<BracketToken> {
    let rest = &body[open + 1..];
    for kind in TOKEN_KINDS {
        if let Some(after) = rest.strip_prefix(kind).and_then(|r| r.strip_prefix(':')) {
            let close = after.find(']')?;
            if close == 0 {
                // `[^\]]+` needs at least one char — no other alternative can
                // match here either (the kind prefixes are mutually exclusive).
                return None;
            }
            let name = after[..close].trim().to_string();
            let end_byte = open + 1 + kind.len() + 1 + close + 1;
            return Some(BracketToken {
                kind,
                name,
                start_byte: open,
                end_byte,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_bracket_tokens() {
        let toks = scan_tokens("ship [project: Hive Rust] phase [phase:port] with [person: Nate]");
        let got: Vec<(&str, &str)> = toks.iter().map(|t| (t.kind, t.name.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ("project", "Hive Rust"),
                ("phase", "port"),
                ("person", "Nate")
            ]
        );
    }

    #[test]
    fn token_requires_name_and_close() {
        assert!(scan_tokens("[topic:]").is_empty());
        assert!(scan_tokens("[topic: unterminated").is_empty());
        assert!(scan_tokens("[unknown: x]").is_empty());
        // a failed open bracket doesn't swallow a later valid token
        let toks = scan_tokens("[nope [task: do the thing]");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].name, "do the thing");
    }

    #[test]
    fn js_slice_is_utf16_indexed() {
        // '😀' is 2 UTF-16 units; JS "a😀b".slice(1, 3) === "😀"
        assert_eq!(js_slice_utf16("a😀b", 1, 3), "😀");
        assert_eq!(js_slice_utf16("hello", 0, 120), "hello");
        assert_eq!(js_slice_utf16("hello", 3, 2), "");
        assert_eq!(js_slice_utf16("hello", -3, 5), "llo");
    }
}
