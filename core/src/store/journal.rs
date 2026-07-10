// Journal append/list/get + anchors + bracket-token refs. Parity port of
// store.ts `journal`, `anchorsFor`/`refsFor`, `materialiseAnchor`,
// `parseBracketTokens`, `journalWriters`. Single-user: reads are unscoped
// (the viewer ACL machinery died in the Phase 1 teardown); writes still
// stamp `user_scope` so old data stays shape-stable for the 1.6 cutover.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use hive_shared::{
    parse_mentions, slugify, snip, ActorKind, Anchor, AnchorFields, AnchorKind, DecisionStatus,
    EntityKind, InboxReason, JournalEntry, JournalEntryView, JournalRef, JournalWriter, NewAnchor,
    NewJournalEntry, Priority, ResolvedAnchor, TaskStatus, ACTORS,
};
use serde_json::json;
use sqlx::Row;

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

    pub async fn journal_get(&self, entry_id: &str) -> Result<Option<JournalEntryView>> {
        let row = crate::pgq::query("SELECT * FROM journal WHERE id = ?")
            .bind(entry_id)
            .fetch_optional(self.db())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(self.entry_view(row_to_entry(&row)?).await?))
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
        .bind(&entry.user_scope)
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
        self.links_create(
            EntityKind::Journal.as_str(),
            &entry.id,
            ref_kind.as_str(),
            &ref_id,
            "anchors",
        )
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
                "mail" => {
                    // [mail:<id>] cites an archived message: a links row only —
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
                    let visible: Option<String> = crate::pgq::query_scalar::<String>(
                        "SELECT id FROM mail_messages \
                         WHERE id = ? AND user_scope = ? AND deleted_at IS NULL",
                    )
                    .bind(token)
                    .bind(&effective_scope)
                    .fetch_optional(self.db())
                    .await?;
                    if let Some(mail_id) = visible {
                        self.links_create(
                            EntityKind::Journal.as_str(),
                            &entry.id,
                            EntityKind::Mail.as_str(),
                            &mail_id,
                            "cites",
                        )
                        .await?;
                    }
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
                // mail — id-addressed; the chip renders the subject. Live
                // rows only (tombstoned/redacted mail resolves to nothing and
                // the raw token stays visible — honest about a dead citation).
                "mail" => {
                    let row = crate::pgq::query(
                        "SELECT id, subject FROM mail_messages WHERE id = ? AND deleted_at IS NULL",
                    )
                    .bind(t.name.trim())
                    .fetch_optional(self.db())
                    .await?;
                    match row {
                        Some(r) => {
                            let id: String = r.try_get("id")?;
                            let subject: String = r.try_get("subject")?;
                            let name = if subject.trim().is_empty() {
                                "(no subject)".to_string()
                            } else {
                                subject
                            };
                            Some((id.clone(), id, name))
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

    async fn entry_view(&self, entry: JournalEntry) -> Result<JournalEntryView> {
        Ok(JournalEntryView {
            anchors: self.anchors_for(&entry.id).await?,
            refs: self.refs_for(&entry.body).await?,
            entry,
        })
    }

    /// Every journal author, with their people row when one exists (Node
    /// `journalWriters`, unscoped — single user sees all writers).
    pub async fn journal_writers(&self) -> Result<Vec<JournalWriter>> {
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
