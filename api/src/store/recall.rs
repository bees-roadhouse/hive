// Recall — store.ts `recall`: the read/inject composition (profile cards +
// scoped retrieval), assembled into a deterministic markdown brief trimmed to
// ~budget tokens. Private SQL for tasks/events/journal per the decoupling rule.

use anyhow::Result;
use hive_shared::{
    snip, EventItem, Priority, Profile, ProjectRef, RecallData, RecallJournalHit, RecallResult,
    SearchHit, Task, TaskStatus, RECALL_DEFAULT_BUDGET,
};
use sqlx::Row;

use super::semantic::SemanticOptions;
use super::{json_vec, Store};

/// Rough token estimate (~4 chars/token; JS `s.length` = UTF-16 units).
fn est_tokens(s: &str) -> usize {
    s.encode_utf16().count().div_ceil(4)
}

/// Append sections until the next would exceed the budget (first always fits).
fn assemble_brief(sections: &[String], budget: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for sec in sections {
        let cost = est_tokens(sec) + 1;
        if !out.is_empty() && used + cost > budget {
            break;
        }
        out.push(sec);
        used += cost;
    }
    out.join("\n\n")
}

fn profile_card(p: &Profile) -> String {
    let name = if p.display_name.is_empty() {
        &p.actor
    } else {
        &p.display_name
    };
    let mut lines = vec![format!("## {} ({})", name, p.kind.as_str())];
    for (k, v) in &p.body.sections {
        if v.trim().is_empty() {
            continue;
        }
        lines.push(format!("**{}:** {}", k.replace('_', " "), v.trim()));
    }
    lines.join("\n")
}

/// Journal entries have no stored title — derive one from the prose: the first
/// Markdown heading, else the first non-empty line, truncated to 80.
fn derive_journal_title(body: &str) -> String {
    for raw in body.split('\n') {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let title = heading_text(line).unwrap_or(line);
        return snip(title.trim(), 80);
    }
    "(untitled)".to_string()
}

/// `^#{1,6}\s+(.*)$` without a regex dep.
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    let trimmed = rest.trim_start();
    if trimmed.len() == rest.len() {
        return None; // no whitespace after the hashes
    }
    Some(trimmed)
}

/// Options for `recall` (store.ts `recall` opts).
#[derive(Debug, Clone, Default)]
pub struct RecallOptions {
    pub peer: Option<String>,
    pub query: Option<String>,
    pub budget: Option<usize>,
}

impl Store {
    /// Compose the ready-to-inject memory brief for `identity` (store.ts
    /// `recall`): profile cards, open tasks, unread inbox, relevant journal,
    /// recent events, touched projects. Deterministic assembly, no LLM.
    pub async fn recall(&self, identity: &str, opts: RecallOptions) -> Result<RecallResult> {
        let peer = opts.peer.as_deref();
        let budget = opts.budget.unwrap_or(RECALL_DEFAULT_BUDGET);

        let mut profile_list: Vec<Profile> = Vec::new();
        if let Some(card) = self.profile_get(identity).await? {
            profile_list.push(card);
        }
        if let Some(p) = peer {
            if let Some(card) = self.profile_get(p).await? {
                profile_list.push(card);
            }
        }

        let open_tasks = self.recall_open_tasks(identity).await?;
        let unread = self.inbox_list(identity, true).await?;

        // Default query: the actors in focus plus the open-task titles.
        let query = match opts.query.as_deref().map(str::trim) {
            Some(q) if !q.is_empty() => q.to_string(),
            _ => {
                let mut parts: Vec<&str> = vec![identity];
                if let Some(p) = peer {
                    parts.push(p);
                }
                parts.extend(open_tasks.iter().take(5).map(|t| t.title.as_str()));
                parts.join(" ")
            }
        };

        // Precision mode — degrades to the standard blend when no reranker.
        let raw_hits: Vec<SearchHit> = if query.is_empty() {
            vec![]
        } else {
            self.semantic_search(
                &query,
                SemanticOptions {
                    limit: Some(8),
                    identity: Some(identity.to_string()),
                    peer: peer.map(String::from),
                    mode: Some("precision".to_string()),
                    ..Default::default()
                },
            )
            .await?
        };
        let mut journal_hits: Vec<RecallJournalHit> = Vec::new();
        for h in raw_hits
            .into_iter()
            .filter(|h| h.kind == hive_shared::EntityKind::Journal)
        {
            let row =
                crate::pgq::query("SELECT author, body, created_at FROM journal WHERE id = ?")
                    .bind(&h.id)
                    .fetch_optional(self.db())
                    .await?;
            let Some(r) = row else { continue };
            let body: String = r.try_get("body")?;
            journal_hits.push(RecallJournalHit {
                hit: SearchHit {
                    title: derive_journal_title(&body),
                    ..h
                },
                author: r.try_get("author")?,
                created_at: r.try_get("created_at")?,
            });
        }

        let recent_events = self.recall_recent_events(5).await?;

        // Projects touched by the identity's open tasks.
        let mut proj_ids: Vec<String> = Vec::new();
        for t in &open_tasks {
            if let Some(p) = &t.project {
                if !proj_ids.contains(p) {
                    proj_ids.push(p.clone());
                }
            }
        }
        let mut touched_projects: Vec<ProjectRef> = Vec::new();
        for pid in &proj_ids {
            if let Some(p) = self.projects_get(pid).await? {
                touched_projects.push(ProjectRef {
                    id: p.id,
                    name: p.name,
                    slug: p.slug,
                });
            }
        }

        // Deterministic markdown brief — cards first, then the working sections.
        let mut sections: Vec<String> = vec![format!(
            "# Recall for {identity}{}",
            peer.map(|p| format!(" · focus: {p}")).unwrap_or_default()
        )];
        for p in &profile_list {
            sections.push(profile_card(p));
        }
        if !open_tasks.is_empty() {
            sections.push(format!(
                "## Open tasks ({identity})\n{}",
                open_tasks
                    .iter()
                    .map(|t| format!(
                        "- [{}] {}{}",
                        t.status.as_str(),
                        t.title,
                        t.due
                            .as_deref()
                            .map(|d| format!(" (due {d})"))
                            .unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !unread.is_empty() {
            sections.push(format!(
                "## Unread inbox\n{}",
                unread
                    .iter()
                    .map(|i| format!("- from {} ({}): {}", i.from, i.reason.as_str(), i.snippet))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !journal_hits.is_empty() {
            sections.push(format!(
                "## Recent relevant journal\n{}",
                journal_hits
                    .iter()
                    .map(|h| format!("- {}: {}", h.author, h.hit.title))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !recent_events.is_empty() {
            sections.push(format!(
                "## Recent events\n{}",
                recent_events
                    .iter()
                    .map(|e| format!(
                        "- {}{}",
                        e.title,
                        e.at.as_deref()
                            .map(|a| format!(" ({a})"))
                            .unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !touched_projects.is_empty() {
            sections.push(format!(
                "## Projects\n{}",
                touched_projects
                    .iter()
                    .map(|p| format!("- {}", p.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        Ok(RecallResult {
            brief: assemble_brief(&sections, budget),
            data: RecallData {
                profiles: profile_list,
                journal: journal_hits,
                tasks: open_tasks,
                inbox: unread,
                events: recent_events,
                projects: touched_projects,
            },
        })
    }

    /// Node: `tasks.list({ assignee: identity }).filter(t => t.status !== "done")`
    /// — priority order (urgent→low), then created_at DESC.
    async fn recall_open_tasks(&self, assignee: &str) -> Result<Vec<Task>> {
        let rows = crate::pgq::query(
            "SELECT * FROM tasks ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
        )
        .fetch_all(self.db())
        .await?;
        let mut out = Vec::new();
        for r in &rows {
            let task = Task {
                id: r.try_get("id")?,
                title: r.try_get("title")?,
                body: r.try_get("body")?,
                status: TaskStatus::from_str_lossy(r.try_get::<String, _>("status")?.as_str()),
                priority: Priority::from_str_lossy(r.try_get::<String, _>("priority")?.as_str()),
                tags: json_vec(&r.try_get::<String, _>("tags")?),
                assignees: json_vec(&r.try_get::<String, _>("assignees")?),
                project: r.try_get("project")?,
                phase: r.try_get("phase")?,
                due: r.try_get("due")?,
                origin_entry_id: r.try_get("origin_entry_id")?,
                anchor_text: r.try_get("anchor_text")?,
                created_at: r.try_get("created_at")?,
                updated_at: r.try_get("updated_at")?,
            };
            if task.status != TaskStatus::Done && task.assignees.iter().any(|a| a == assignee) {
                out.push(task);
            }
        }
        Ok(out)
    }

    /// Node: `events.list().slice(0, 5)` — COALESCE(at, created_at) DESC.
    async fn recall_recent_events(&self, limit: i64) -> Result<Vec<EventItem>> {
        let rows = crate::pgq::query(
            "SELECT * FROM events ORDER BY COALESCE(at, created_at) DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(self.db())
        .await?;
        rows.iter()
            .map(|r| -> Result<EventItem> {
                Ok(EventItem {
                    id: r.try_get("id")?,
                    title: r.try_get("title")?,
                    body: r.try_get("body")?,
                    at: r.try_get("at")?,
                    tags: json_vec(&r.try_get::<String, _>("tags")?),
                    assignees: json_vec(&r.try_get::<String, _>("assignees")?),
                    origin_entry_id: r.try_get("origin_entry_id")?,
                    anchor_text: r.try_get("anchor_text")?,
                    created_at: r.try_get("created_at")?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_title_from_heading_or_first_line() {
        assert_eq!(derive_journal_title("# Big Win\n\nbody"), "Big Win");
        assert_eq!(derive_journal_title("\n\nfirst line\nsecond"), "first line");
        assert_eq!(
            derive_journal_title("###no-space heading"),
            "###no-space heading"
        );
        assert_eq!(derive_journal_title("   \n\t\n"), "(untitled)");
        let long = "x".repeat(100);
        assert_eq!(derive_journal_title(&long).encode_utf16().count(), 81); // 80 + '…'
    }

    #[test]
    fn brief_respects_budget_but_keeps_first_section() {
        let sections = vec!["a".repeat(400), "b".repeat(400), "c".repeat(400)];
        // Each section ≈ 100 tokens (+1). Budget 150 → first two? 101 + 101 > 150
        // after the first, so only the first survives.
        let brief = assemble_brief(&sections, 150);
        assert!(brief.contains('a') && !brief.contains('b'));
        // First section always included even when over budget on its own.
        let brief = assemble_brief(&sections, 1);
        assert!(brief.contains('a'));
    }
}
