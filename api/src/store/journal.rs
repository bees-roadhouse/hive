// Journal append/list/get + anchors + bracket-token refs + visibleJournal ACL.
// Parity port of store.ts `journal`, `anchorsFor`/`refsFor`, `materialiseAnchor`,
// `parseBracketTokens`, `journalWriters`, `visibleJournal`, `visibleEntryIds`.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use hive_shared::{
    parse_mentions, slugify, snip, ActorKind, Anchor, AnchorFields, AnchorKind, DecisionStatus,
    EntityKind, InboxReason, JournalEntry, JournalEntryView, JournalRef, JournalWriter, NewAnchor,
    NewJournalEntry, NewShare, Priority, ResolvedAnchor, ShareScope, TaskStatus, ACTORS,
};
use serde_json::json;
use sqlx::Row;

use crate::middleware::Visibility;

use super::decisions::DecisionCreate;
use super::events::EventCreate;
use super::tasks::TaskCreate;
use super::{json_vec, new_id, now_iso, to_json, Store};

impl Store {
    pub async fn journal_list(&self, limit: i64, offset: i64) -> Result<Vec<JournalEntryView>> {
        let rows =
            crate::pgq::query("SELECT * FROM journal ORDER BY created_at DESC LIMIT ? OFFSET ?")
                .bind(limit)
                .bind(offset)
                .fetch_all(self.db())
                .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let entry = row_to_entry(row)?;
            out.push(self.entry_view(entry).await?);
        }
        Ok(out)
    }

    pub async fn journal_get(
        &self,
        entry_id: &str,
        vis: &Visibility,
    ) -> Result<Option<JournalEntryView>> {
        let row = crate::pgq::query("SELECT * FROM journal WHERE id = ?")
            .bind(entry_id)
            .fetch_optional(self.db())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        // Namespace gate: non-admins get an entry only if it's global, in their
        // own namespace, or explicitly shared/@mentioned to them. Hidden as 404.
        if let Visibility::Namespace(u) = vis {
            let scope: Option<String> = row.try_get("user_scope")?;
            let own_or_global = scope.as_deref().map(|s| s == u).unwrap_or(true);
            if !own_or_global {
                let visible = self.visible_entry_ids(vis).await?.unwrap_or_default();
                if !visible.contains(entry_id) {
                    return Ok(None);
                }
            }
        }
        Ok(Some(self.entry_view(row_to_entry(&row)?).await?))
    }

    /// Admin bulk-reassignment of journal namespace ownership. Filters (ANDed,
    /// all optional) pick the entries: `match_unscoped` = currently global,
    /// `from_user` = currently owned by that user, `author` = written by that
    /// actor. `to` is the new owner (None = make global). Returns rows changed.
    /// With no filters it reassigns every entry (admin-only, deliberate).
    pub async fn journal_reassign_scope(
        &self,
        match_unscoped: bool,
        from_user: Option<&str>,
        author: Option<&str>,
        to: Option<&str>,
    ) -> Result<u64> {
        let mut clauses: Vec<String> = Vec::new();
        if match_unscoped {
            clauses.push("user_scope IS NULL".to_string());
        }
        if from_user.is_some() {
            clauses.push("user_scope = ?".to_string());
        }
        if author.is_some() {
            clauses.push("author = ?".to_string());
        }
        let where_ = if clauses.is_empty() {
            "TRUE".to_string()
        } else {
            clauses.join(" AND ")
        };
        let sql = format!("UPDATE journal SET user_scope = ? WHERE {where_}");
        let mut q = crate::pgq::query(&sql).bind(to);
        if let Some(u) = from_user {
            q = q.bind(u);
        }
        if let Some(a) = author {
            q = q.bind(a);
        }
        Ok(q.execute(self.db()).await?.rows_affected())
    }

    /// The one write path. Persist immutable prose, then materialise each anchored
    /// span into a structured entity and fan out inbox notifications. Also parses
    /// inline [person:], [topic:], [project:], [phase:], [task:] tokens to
    /// emerge/link entities and feed inboxes.
    ///
    /// Node wraps this in a SQLite transaction; here the steps run sequentially on
    /// the pool because emit/inbox/ensure helpers are pool-level (a wrapping write
    /// transaction would deadlock against them under WAL's single-writer rule).
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
        let mentions = parse_mentions(&input.body);
        let entry = JournalEntry {
            id: new_id("jrnl"),
            author: author.clone(),
            body: input.body.clone(),
            tags: input.tags.clone().unwrap_or_default(),
            mentions: mentions.clone(),
            user_scope: user_scope.map(String::from),
            created_at: now_iso(),
        };
        // Namespace owner: the human the writing principal acts for (None = a
        // system/worker write → global/continuous history).
        crate::pgq::query(
            "INSERT INTO journal (id, author, body, tags, mentions, user_scope, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&entry.id)
        .bind(&entry.author)
        .bind(&entry.body)
        .bind(to_json(&entry.tags))
        .bind(to_json(&entry.mentions))
        .bind(user_scope)
        .bind(&entry.created_at)
        .execute(self.db())
        .await?;
        self.index_entity(
            "journal",
            &entry.id,
            &format!("{author}: {}", snip(&input.body, 50)),
            &input.body,
            &entry.tags,
        )
        .await?;

        let mut assigned: HashSet<String> = HashSet::new();
        for a in input.anchors.as_deref().unwrap_or_default() {
            self.materialise_anchor(&entry, a, &author, &mut assigned)
                .await?;
        }

        // Parse bracket tokens: emerge/link entities, fan to inboxes.
        self.parse_bracket_tokens(&entry, &author, &mut assigned)
            .await?;

        // Anyone @mentioned but not already pulled into an anchor gets a plain
        // "mention" inbox item — humans and AIs alike.
        for m in &mentions {
            if !assigned.contains(m) {
                self.inbox_add(
                    m,
                    &author,
                    InboxReason::Mention,
                    EntityKind::Journal.as_str(),
                    &entry.id,
                    Some(&entry.id),
                    &input.body,
                )
                .await?;
            }
        }

        // Auto-share: every @mentioned actor gets an entry-level share so the
        // entry is visible in their scoped journal view.
        for m in &mentions {
            if m != &author {
                self.shares_create(NewShare {
                    scope: ShareScope::Entry,
                    ref_: entry.id.clone(),
                    viewer: m.clone(),
                })
                .await?;
            }
        }

        self.emit(
            "journal.created",
            &author,
            json!({"id": entry.id, "anchors": input.anchors.as_ref().map_or(0, Vec::len)}),
        )
        .await?;
        self.entry_view(entry).await
    }

    async fn materialise_anchor(
        &self,
        entry: &JournalEntry,
        a: &NewAnchor,
        author: &str,
        assigned: &mut HashSet<String>,
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
                let t = self
                    .tasks_create(
                        TaskCreate {
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
                            project: f.project.clone().flatten(),
                            origin_entry_id: Some(entry.id.clone()),
                            anchor_text: Some(text.clone()),
                            ..TaskCreate::default()
                        },
                        author,
                    )
                    .await?;
                (t.id, InboxReason::Assignment, EntityKind::Task)
            }
            AnchorKind::Decision => {
                let d = self
                    .decisions_create(
                        DecisionCreate {
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
                            project: f.project.clone().flatten(),
                            supersedes: f.supersedes.clone().flatten(),
                            origin_entry_id: Some(entry.id.clone()),
                            anchor_text: Some(text.clone()),
                        },
                        author,
                    )
                    .await?;
                (d.id, InboxReason::Decision, EntityKind::Decision)
            }
            AnchorKind::Event => {
                let e = self
                    .events_create(
                        EventCreate {
                            title,
                            body: text.clone(),
                            at: f.at.clone().flatten(),
                            tags: f.tags.clone().unwrap_or_default(),
                            assignees: assignees.clone(),
                            origin_entry_id: Some(entry.id.clone()),
                            anchor_text: Some(text.clone()),
                        },
                        author,
                    )
                    .await?;
                (e.id, InboxReason::Event, EntityKind::Event)
            }
        };

        crate::pgq::query(
            r#"INSERT INTO anchors (id, entry_id, start, "end", text, kind, ref_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(new_id("anc"))
        .bind(&entry.id)
        .bind(a.start)
        .bind(a.end)
        .bind(&text)
        .bind(a.kind.as_str())
        .bind(&ref_id)
        .bind(now_iso())
        .execute(self.db())
        .await?;
        self.links_create(EntityKind::Journal.as_str(), &entry.id, ref_kind.as_str(), &ref_id, "anchors")
            .await?;

        // For inbox delivery use the full assignee list (including author when auto-assigned).
        let recipients = if a.kind == AnchorKind::Task {
            &assignees_for_task
        } else {
            &assignees
        };
        for who in recipients {
            assigned.insert(who.clone());
            self.inbox_add(
                who,
                author,
                reason,
                ref_kind.as_str(),
                &ref_id,
                Some(&entry.id),
                &text,
            )
            .await?;
        }
        Ok(())
    }

    /// Parse [person:], [topic:], [project:], [phase:], [task:] tokens from an
    /// entry body. Find-or-create each entity, create a links row, and fan to
    /// inboxes where relevant. Context tracking: if the entry mentions a
    /// [project:] and/or [phase:], any [task:] that emerges is related to it.
    async fn parse_bracket_tokens(
        &self,
        entry: &JournalEntry,
        author: &str,
        assigned: &mut HashSet<String>,
    ) -> Result<()> {
        let tokens = scan_tokens(&entry.body);

        // First pass: collect context (project + phase referenced in this entry).
        let mut ctx_project: Option<String> = None;
        let mut ctx_phase: Option<String> = None;
        for t in &tokens {
            match t.kind {
                "project" => {
                    let p = self.projects_ensure(&t.name).await?;
                    ctx_project = Some(p.id);
                }
                "phase" => {
                    if let Some(pid) = &ctx_project {
                        let ph = self.phases_ensure(pid, &t.name).await?;
                        ctx_phase = Some(ph.id);
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
                        Some((n, k)) => self.people_ensure(&capitalize(n), *k).await?,
                        None => self.people_ensure(&t.name, ActorKind::Human).await?,
                    };
                    self.links_create(
                        EntityKind::Journal.as_str(),
                        &entry.id,
                        EntityKind::Person.as_str(),
                        &person.id,
                        "mentions",
                    )
                    .await?;
                    // Fan to inbox if this person is a known actor (same as @mention).
                    if let Some((n, _)) = actor_match {
                        assigned.insert((*n).to_string());
                        self.inbox_add(
                            n,
                            author,
                            InboxReason::Mention,
                            EntityKind::Journal.as_str(),
                            &entry.id,
                            Some(&entry.id),
                            &entry.body,
                        )
                        .await?;
                    }
                }
                "topic" => {
                    let topic = self.topics_ensure(&t.name).await?;
                    self.links_create(
                        EntityKind::Journal.as_str(),
                        &entry.id,
                        EntityKind::Topic.as_str(),
                        &topic.id,
                        "tagged",
                    )
                    .await?;
                }
                "project" => {
                    let proj = self.projects_ensure(&t.name).await?;
                    self.links_create(
                        EntityKind::Journal.as_str(),
                        &entry.id,
                        EntityKind::Project.as_str(),
                        &proj.id,
                        "about",
                    )
                    .await?;
                }
                "phase" => {
                    if let Some(pid) = &ctx_project {
                        let ph = self.phases_ensure(pid, &t.name).await?;
                        self.links_create(
                            EntityKind::Journal.as_str(),
                            &entry.id,
                            EntityKind::Phase.as_str(),
                            &ph.id,
                            "about",
                        )
                        .await?;
                    }
                }
                "task" => {
                    // Emerge a task anchored to this entry, auto-assigned to the author.
                    let task = self
                        .tasks_create(
                            TaskCreate {
                                title: t.name.clone(),
                                body: String::new(),
                                assignees: vec![author.to_string()],
                                project: ctx_project.clone(),
                                phase: ctx_phase.clone(),
                                origin_entry_id: Some(entry.id.clone()),
                                anchor_text: Some(t.name.clone()),
                                ..TaskCreate::default()
                            },
                            author,
                        )
                        .await?;
                    self.links_create(
                        EntityKind::Journal.as_str(),
                        &entry.id,
                        EntityKind::Task.as_str(),
                        &task.id,
                        "anchors",
                    )
                    .await?;
                    // author is assigned; inbox_add silently skips self-notification.
                    self.inbox_add(
                        author,
                        author,
                        InboxReason::Assignment,
                        EntityKind::Task.as_str(),
                        &task.id,
                        Some(&entry.id),
                        &t.name,
                    )
                    .await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Anchors for an entry, each with its resolved entity (Node `anchorsFor`).
    pub async fn anchors_for(&self, entry_id: &str) -> Result<Vec<ResolvedAnchor>> {
        let rows = crate::pgq::query(
            r#"SELECT id, entry_id, start, "end", text, kind, ref_id, created_at FROM anchors WHERE entry_id = ? ORDER BY start"#,
        )
        .bind(entry_id)
        .fetch_all(self.db())
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let kind_str: String = r.try_get("kind")?;
            let ref_id: String = r.try_get("ref_id")?;
            let anchor = Anchor {
                id: r.try_get("id")?,
                entry_id: r.try_get("entry_id")?,
                start: r.try_get("start")?,
                end: r.try_get("end")?,
                text: r.try_get("text")?,
                kind: AnchorKind::parse(&kind_str).unwrap_or(AnchorKind::Task),
                ref_id: ref_id.clone(),
                created_at: r.try_get("created_at")?,
            };
            let entity = self.entity_by_id(&kind_str, &ref_id).await?;
            out.push(ResolvedAnchor { anchor, entity });
        }
        Ok(out)
    }

    /// Node `entityById` — Task | Decision | EventItem | null as JSON.
    async fn entity_by_id(&self, kind: &str, ref_id: &str) -> Result<serde_json::Value> {
        Ok(match kind {
            "task" => self
                .tasks_get(ref_id)
                .await?
                .map(serde_json::to_value)
                .transpose()?
                .unwrap_or(serde_json::Value::Null),
            "decision" => self
                .decisions_get(ref_id)
                .await?
                .map(serde_json::to_value)
                .transpose()?
                .unwrap_or(serde_json::Value::Null),
            "event" => self
                .events_get(ref_id)
                .await?
                .map(serde_json::to_value)
                .transpose()?
                .unwrap_or(serde_json::Value::Null),
            _ => serde_json::Value::Null,
        })
    }

    /// Resolve bracket tokens in a body string against the DB at read time
    /// (Node `refsFor`).
    pub async fn refs_for(&self, body: &str) -> Result<Vec<JournalRef>> {
        let mut refs = Vec::new();
        for t in scan_tokens(body) {
            let start = utf16_len(&body[..t.start_byte]);
            let end = utf16_len(&body[..t.end_byte]);
            let resolved: Option<(String, String, String)> = match t.kind {
                "person" => self
                    .people_by_slug(&slugify(&t.name))
                    .await?
                    .map(|p| (p.id, p.slug, p.name)),
                "topic" => self
                    .topics_by_slug(&slugify(&t.name))
                    .await?
                    .map(|x| (x.id, x.slug, x.name)),
                "project" => self
                    .projects_by_slug(&slugify(&t.name))
                    .await?
                    .map(|x| (x.id, x.slug, x.name)),
                "phase" => {
                    // phase resolution without a project context: find by name across all phases
                    let row = crate::pgq::query(
                        "SELECT * FROM phases WHERE LOWER(name) = LOWER(?) LIMIT 1",
                    )
                    .bind(&t.name)
                    .fetch_optional(self.db())
                    .await?;
                    match row {
                        Some(r) => {
                            let name: String = r.try_get("name")?;
                            Some((r.try_get("id")?, slugify(&name), name))
                        }
                        None => None,
                    }
                }
                // task — find the most recent task with matching title
                _ => {
                    let row = crate::pgq::query(
                        "SELECT id, title FROM tasks WHERE LOWER(title) = LOWER(?) ORDER BY created_at DESC LIMIT 1",
                    )
                    .bind(&t.name)
                    .fetch_optional(self.db())
                    .await?;
                    match row {
                        Some(r) => {
                            let title: String = r.try_get("title")?;
                            Some((r.try_get("id")?, slugify(&title), title))
                        }
                        None => None,
                    }
                }
            };
            if let Some((id, slug, name)) = resolved {
                refs.push(JournalRef {
                    kind: EntityKind::parse(t.kind).expect("TOKEN_KINDS entries always parse"),
                    id,
                    slug,
                    name,
                    start,
                    end,
                });
            }
        }
        Ok(refs)
    }

    async fn entry_view(&self, entry: JournalEntry) -> Result<JournalEntryView> {
        Ok(JournalEntryView {
            anchors: self.anchors_for(&entry.id).await?,
            refs: self.refs_for(&entry.body).await?,
            entry,
        })
    }

    /// Authors whose journal streams a viewer can see by ownership/relationship:
    /// themselves, AIs they own, AIs that linked or @mentioned them.
    async fn viewer_base_authors(&self, viewer: &str) -> Result<Vec<String>> {
        let mut authors: Vec<String> = vec![viewer.to_string()];
        let push = |list: Vec<String>, authors: &mut Vec<String>| {
            for a in list {
                if !authors.contains(&a) {
                    authors.push(a);
                }
            }
        };

        let owned: Vec<String> =
            crate::pgq::query_scalar("SELECT slug FROM people WHERE kind='ai' AND owner=?")
                .bind(viewer)
                .fetch_all(self.db())
                .await?;
        push(owned, &mut authors);

        // AI authors that have written inside this user's namespace belong to
        // this user's memory view even if the AI person row predates ownership.
        let namespace_ai_authors: Vec<String> = crate::pgq::query_scalar(
            "SELECT DISTINCT j.author FROM journal j \
             WHERE j.user_scope = ? \
               AND EXISTS (SELECT 1 FROM people WHERE slug=j.author AND kind='ai')",
        )
        .bind(viewer)
        .fetch_all(self.db())
        .await?;
        push(namespace_ai_authors, &mut authors);

        // AI authors that referenced viewer via links (target_kind='person', target_id=viewer).
        let linked: Vec<String> = crate::pgq::query_scalar(
            "SELECT DISTINCT j.author FROM journal j \
             JOIN links l ON l.source_kind='journal' AND l.source_id=j.id \
             WHERE l.target_kind='person' AND l.target_id=? \
               AND EXISTS (SELECT 1 FROM people WHERE slug=j.author AND kind='ai')",
        )
        .bind(viewer)
        .fetch_all(self.db())
        .await?;
        push(linked, &mut authors);

        // AI authors that @mentioned viewer in any entry.
        let mentioned: Vec<String> = crate::pgq::query_scalar(
            "SELECT DISTINCT j.author FROM journal j \
             WHERE j.mentions LIKE ? \
               AND EXISTS (SELECT 1 FROM people WHERE slug=j.author AND kind='ai')",
        )
        .bind(mention_like(viewer))
        .fetch_all(self.db())
        .await?;
        push(mentioned, &mut authors);
        Ok(authors)
    }

    /// Journal entries visible to a principal, optionally filtered to specific
    /// writers (Node `visibleJournal`, now per-user-namespace). Admins see every
    /// entry; everyone else sees global + own-namespace entries surfaced by the
    /// author/share/mention ACL.
    pub async fn visible_journal(
        &self,
        vis: &Visibility,
        writers: Option<&[String]>,
        scope: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JournalEntryView>> {
        let viewer = match vis {
            Visibility::All => return self.journal_all(writers, scope, limit, offset).await,
            Visibility::Namespace(u) => u.clone(),
        };
        let viewer = viewer.as_str();
        // Base authors (self + owned/related AIs) are NAMESPACE-GATED.
        let mut base_authors = self.viewer_base_authors(viewer).await?;
        // Journal-scope shares grant a whole author stream and PIERCE the
        // namespace (explicit "open my journal to you").
        let mut shared_authors: Vec<String> =
            crate::pgq::query_scalar("SELECT ref FROM shares WHERE scope='journal' AND viewer=?")
                .bind(viewer)
                .fetch_all(self.db())
                .await?;
        // Entry shares + @mentions are explicit per-entry grants that PIERCE.
        let shared_entry_ids: Vec<String> =
            crate::pgq::query_scalar("SELECT ref FROM shares WHERE scope='entry' AND viewer=?")
                .bind(viewer)
                .fetch_all(self.db())
                .await?;
        let mentioned_ids: Vec<String> =
            crate::pgq::query_scalar("SELECT id FROM journal WHERE mentions LIKE ?")
                .bind(mention_like(viewer))
                .fetch_all(self.db())
                .await?;
        let mut extra_ids: Vec<String> = Vec::new();
        for id in shared_entry_ids.into_iter().chain(mentioned_ids) {
            if !extra_ids.contains(&id) {
                extra_ids.push(id);
            }
        }

        // Optional writers filter: intersect with both author lists.
        let writers = writers.filter(|w| !w.is_empty());
        if let Some(w) = writers {
            base_authors.retain(|a| w.contains(a));
            shared_authors.retain(|a| w.contains(a));
        }

        let base_ph = placeholders_or_never(base_authors.len());
        let shared_ph = placeholders_or_never(shared_authors.len());
        let extra_ph = placeholders_or_never(extra_ids.len());
        let writers_filter = match writers {
            Some(w) => format!("AND j.author IN ({})", placeholders_or_never(w.len())),
            None => String::new(),
        };
        // Namespace gate applies to the base-author branch ONLY; shared streams,
        // entry shares, and @mentions pierce it. The optional `scope` filter only
        // NARROWS this already-permitted set (it never widens visibility): it is
        // ANDed onto the whole WHERE.
        let scope_filter = scope_clause(scope);
        let sql = format!(
            "SELECT j.* FROM journal j WHERE (\
               (j.author IN ({base_ph}) AND (j.user_scope IS NULL OR j.user_scope = ?)) \
               OR j.author IN ({shared_ph}) \
               OR (j.id IN ({extra_ph}) {writers_filter})\
             ){scope_filter} \
             ORDER BY j.created_at DESC LIMIT ? OFFSET ?"
        );
        let mut q = crate::pgq::query(&sql);
        for a in &base_authors {
            q = q.bind(a);
        }
        q = q.bind(viewer);
        for a in &shared_authors {
            q = q.bind(a);
        }
        for id in &extra_ids {
            q = q.bind(id);
        }
        if let Some(w) = writers {
            for x in w {
                q = q.bind(x);
            }
        }
        q = bind_scope(q, scope);
        let rows = q.bind(limit).bind(offset).fetch_all(self.db()).await?;
        self.hydrate_entries(&rows).await
    }

    /// Admin path: every entry, optionally filtered to writers, no namespace gate.
    /// The optional `scope` filter lets an admin pivot the feed to a single
    /// namespace (or the global/continuous stream).
    async fn journal_all(
        &self,
        writers: Option<&[String]>,
        scope: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<JournalEntryView>> {
        let writers = writers.filter(|w| !w.is_empty());
        let scope_filter = scope_clause(scope);
        let sql = match writers {
            Some(w) => format!(
                "SELECT j.* FROM journal j WHERE j.author IN ({}){scope_filter} \
                 ORDER BY j.created_at DESC LIMIT ? OFFSET ?",
                placeholders_or_never(w.len())
            ),
            None => format!(
                "SELECT j.* FROM journal j WHERE TRUE{scope_filter} \
                 ORDER BY j.created_at DESC LIMIT ? OFFSET ?"
            ),
        };
        let mut q = crate::pgq::query(&sql);
        if let Some(w) = writers {
            for x in w {
                q = q.bind(x);
            }
        }
        q = bind_scope(q, scope);
        let rows = q.bind(limit).bind(offset).fetch_all(self.db()).await?;
        self.hydrate_entries(&rows).await
    }

    async fn hydrate_entries(
        &self,
        rows: &[sqlx::postgres::PgRow],
    ) -> Result<Vec<JournalEntryView>> {
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let entry = row_to_entry(row)?;
            // Parity with Node: visibleJournal calls refsFor(r.id) — the entry ID,
            // not the body — so refs always resolve empty on this path.
            let refs = self.refs_for(&entry.id).await?;
            out.push(JournalEntryView {
                anchors: self.anchors_for(&entry.id).await?,
                refs,
                entry,
            });
        }
        Ok(out)
    }

    /// Writers visible to a principal: their own + related AIs (Node
    /// `journalWriters`). Admins get every author.
    pub async fn journal_writers(&self, vis: &Visibility) -> Result<Vec<JournalWriter>> {
        let viewer = match vis {
            Visibility::All => {
                return self.all_writers().await;
            }
            Visibility::Namespace(u) => u.clone(),
        };
        let viewer = viewer.as_str();
        let mut slugs = self.viewer_base_authors(viewer).await?;
        let journal_shared: Vec<String> =
            crate::pgq::query_scalar("SELECT ref FROM shares WHERE scope='journal' AND viewer=?")
                .bind(viewer)
                .fetch_all(self.db())
                .await?;
        for a in journal_shared {
            if !slugs.contains(&a) {
                slugs.push(a);
            }
        }

        let mut result = Vec::with_capacity(slugs.len());
        for slug in &slugs {
            match self.people_get(slug).await? {
                Some(p) => result.push(JournalWriter {
                    slug: p.slug,
                    name: p.name,
                    kind: p.kind,
                    owner: p.owner,
                }),
                // Viewer may not be in the people table yet — return a minimal record.
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
    }

    /// Admin path: every journal author as a writer.
    async fn all_writers(&self) -> Result<Vec<JournalWriter>> {
        let slugs: Vec<String> = crate::pgq::query_scalar("SELECT DISTINCT author FROM journal")
            .fetch_all(self.db())
            .await?;
        let mut result = Vec::with_capacity(slugs.len());
        for slug in &slugs {
            match self.people_get(slug).await? {
                Some(p) => result.push(JournalWriter {
                    slug: p.slug,
                    name: p.name,
                    kind: p.kind,
                    owner: p.owner,
                }),
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
    }

    /// Journal entry ids visible to a principal — the permission boundary every
    /// read (feed, search, entity reads) filters through (Node `visibleEntryIds`,
    /// now per-user-namespace). Returns `None` for an admin (sees everything — no
    /// id filter). For a non-admin the candidate set (author streams + shares +
    /// @mentions) is hard-gated to the principal's namespace: only entries that
    /// are global (`user_scope IS NULL`) or owned by their namespace user.
    pub async fn visible_entry_ids(&self, vis: &Visibility) -> Result<Option<HashSet<String>>> {
        let Visibility::Namespace(viewer) = vis else {
            return Ok(None);
        };
        let viewer = viewer.as_str();
        let base_authors = self.viewer_base_authors(viewer).await?;
        let shared_authors: Vec<String> =
            crate::pgq::query_scalar("SELECT ref FROM shares WHERE scope='journal' AND viewer=?")
                .bind(viewer)
                .fetch_all(self.db())
                .await?;

        let mut ids: HashSet<String> = HashSet::new();
        // Explicit entry shares + @mentions pierce the namespace (a deliberate
        // "relates to you" signal across users).
        let shared: Vec<String> =
            crate::pgq::query_scalar("SELECT ref FROM shares WHERE scope='entry' AND viewer=?")
                .bind(viewer)
                .fetch_all(self.db())
                .await?;
        ids.extend(shared);
        let mentioned: Vec<String> =
            crate::pgq::query_scalar("SELECT id FROM journal WHERE mentions LIKE ?")
                .bind(mention_like(viewer))
                .fetch_all(self.db())
                .await?;
        ids.extend(mentioned);

        // Base author streams are namespace-gated: global or own-namespace only.
        if !base_authors.is_empty() {
            let sql = format!(
                "SELECT id FROM journal WHERE author IN ({}) \
                 AND (user_scope IS NULL OR user_scope = ?)",
                placeholders_or_never(base_authors.len())
            );
            let mut q = crate::pgq::query_scalar::<String>(&sql);
            for a in &base_authors {
                q = q.bind(a);
            }
            q = q.bind(viewer);
            ids.extend(q.fetch_all(self.db()).await?);
        }

        // Journal-scope-shared author streams pierce the namespace.
        if !shared_authors.is_empty() {
            let sql = format!(
                "SELECT id FROM journal WHERE author IN ({})",
                placeholders_or_never(shared_authors.len())
            );
            let mut q = crate::pgq::query_scalar::<String>(&sql);
            for a in &shared_authors {
                q = q.bind(a);
            }
            ids.extend(q.fetch_all(self.db()).await?);
        }

        Ok(Some(ids))
    }
}

fn row_to_entry(r: &sqlx::postgres::PgRow) -> Result<JournalEntry> {
    Ok(JournalEntry {
        id: r.try_get("id")?,
        author: r.try_get("author")?,
        body: r.try_get("body")?,
        tags: json_vec(r.try_get::<String, _>("tags")?.as_str()),
        mentions: json_vec(r.try_get::<String, _>("mentions")?.as_str()),
        user_scope: r.try_get("user_scope")?,
        created_at: r.try_get("created_at")?,
    })
}

/// `%"viewer"%` — Node's LIKE probe into the mentions JSON column.
fn mention_like(viewer: &str) -> String {
    format!("%\"{viewer}\"%")
}

/// Sentinel scope value meaning "the global / continuous (un-owned) stream"
/// — i.e. `user_scope IS NULL`. Any other `Some(slug)` matches that exact owner.
pub const GLOBAL_SCOPE: &str = "__global__";

/// SQL fragment ANDed onto the feed query for the optional namespace filter.
/// `None` → no extra clause; `Some(GLOBAL_SCOPE)` → only un-owned (global)
/// entries; `Some(slug)` → only entries owned by `slug`. This only ever NARROWS
/// the already-permitted set.
fn scope_clause(scope: Option<&str>) -> &'static str {
    match scope {
        None => "",
        Some(GLOBAL_SCOPE) => " AND j.user_scope IS NULL",
        Some(_) => " AND j.user_scope = ?",
    }
}

/// Bind the placeholder used by `scope_clause` (a no-op unless `scope` is a
/// concrete owner slug).
fn bind_scope<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    scope: Option<&'q str>,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match scope {
        Some(s) if s != GLOBAL_SCOPE => q.bind(s),
        _ => q,
    }
}

/// `?,?,?` for n binds, or the never-matching literal Node uses when a set is empty.
fn placeholders_or_never(n: usize) -> String {
    if n == 0 {
        "'__never__'".to_string()
    } else {
        vec!["?"; n].join(",")
    }
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

const TOKEN_KINDS: &[&str] = &["person", "topic", "project", "phase", "task"];

/// Node TOKEN_RE: /\[(person|topic|project|phase|task):([^\]]+)\]/g
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
