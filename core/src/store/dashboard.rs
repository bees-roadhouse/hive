// Dashboard stats + knowledge graph + typeahead autocomplete — parity port of
// store.ts `dashboard`/`graph`/`autocomplete`. Private SQL for journal/tasks/
// decisions/links/topics/phases per the decoupling rule (people/projects go
// through their owned store modules).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use hive_shared::{
    slugify, AuthorCount, AuthorEntryCount, AutocompleteItem, DashboardStats, DayCount,
    DecisionCounts, EntityKind, GraphData, GraphEdge, GraphNode, InboxStat, MailDashboardStats,
    PersonCallout, TaskCounts, TaskStatus, TaskWithDue, ACTORS,
};
use sqlx::Row;

use super::{json_vec, Store};

impl Store {
    pub async fn dashboard(&self) -> Result<DashboardStats> {
        let count = |sql: &'static str| async move {
            crate::pgq::query_scalar::<i64>(sql)
                .fetch_one(self.db())
                .await
        };
        let count_by = |sql: &'static str, arg: String| async move {
            crate::pgq::query_scalar::<i64>(sql)
                .bind(arg)
                .fetch_one(self.db())
                .await
        };

        let tasks = TaskCounts {
            total: count("SELECT count(*) FROM tasks").await?,
            todo: count_by("SELECT count(*) FROM tasks WHERE status=?", "todo".into()).await?,
            doing: count_by("SELECT count(*) FROM tasks WHERE status=?", "doing".into()).await?,
            blocked: count_by(
                "SELECT count(*) FROM tasks WHERE status=?",
                "blocked".into(),
            )
            .await?,
            done: count_by("SELECT count(*) FROM tasks WHERE status=?", "done".into()).await?,
        };
        let decisions = DecisionCounts {
            total: count("SELECT count(*) FROM decisions").await?,
            proposed: count_by(
                "SELECT count(*) FROM decisions WHERE status=?",
                "proposed".into(),
            )
            .await?,
            accepted: count_by(
                "SELECT count(*) FROM decisions WHERE status=?",
                "accepted".into(),
            )
            .await?,
            rejected: count_by(
                "SELECT count(*) FROM decisions WHERE status=?",
                "rejected".into(),
            )
            .await?,
            superseded: count_by(
                "SELECT count(*) FROM decisions WHERE status=?",
                "superseded".into(),
            )
            .await?,
        };

        let by_author = crate::pgq::query(
            "SELECT author, count(*) AS entries FROM journal GROUP BY author ORDER BY entries DESC",
        )
        .fetch_all(self.db())
        .await?
        .iter()
        .map(|r| -> Result<AuthorCount> {
            Ok(AuthorCount {
                author: r.try_get("author")?,
                entries: r.try_get("entries")?,
            })
        })
        .collect::<Result<_>>()?;

        let mut inbox: Vec<InboxStat> = Vec::with_capacity(ACTORS.len());
        for (name, kind) in ACTORS {
            inbox.push(InboxStat {
                recipient: name.to_string(),
                kind: *kind,
                unread: count_by(
                    "SELECT count(*) FROM inbox WHERE recipient=? AND read_at IS NULL",
                    name.to_string(),
                )
                .await?,
                total: count_by(
                    "SELECT count(*) FROM inbox WHERE recipient=?",
                    name.to_string(),
                )
                .await?,
            });
        }

        // Open tasks with a due date (for calendar overlay).
        let tasks_with_due = crate::pgq::query(
            "SELECT id, title, due, status, assignees FROM tasks WHERE due IS NOT NULL AND status != 'done' ORDER BY due ASC",
        )
        .fetch_all(self.db())
        .await?
        .iter()
        .map(|r| -> Result<TaskWithDue> {
            Ok(TaskWithDue {
                id: r.try_get("id")?,
                title: r.try_get("title")?,
                due: r.try_get("due")?,
                status: TaskStatus::from_str_lossy(r.try_get::<String, _>("status")?.as_str()),
                assignees: json_vec(&r.try_get::<String, _>("assignees")?),
            })
        })
        .collect::<Result<_>>()?;

        // Entry counts per day for last 30 days (substr gives YYYY-MM-DD).
        // created_at is an ISO-8601 TEXT column, so compare against a
        // Rust-computed cutoff string (lexicographic order matches chronological).
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(30))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let entries_by_day = crate::pgq::query(
            "SELECT substr(created_at, 1, 10) AS day, count(*) AS count \
             FROM journal \
             WHERE created_at >= ? \
             GROUP BY day ORDER BY day ASC",
        )
        .bind(&cutoff)
        .fetch_all(self.db())
        .await?
        .iter()
        .map(|r| -> Result<DayCount> {
            Ok(DayCount {
                day: r.try_get("day")?,
                count: r.try_get("count")?,
            })
        })
        .collect::<Result<_>>()?;

        let entries_by_author = crate::pgq::query(
            "SELECT author, count(*) AS count FROM journal GROUP BY author ORDER BY count DESC",
        )
        .fetch_all(self.db())
        .await?
        .iter()
        .map(|r| -> Result<AuthorEntryCount> {
            Ok(AuthorEntryCount {
                author: r.try_get("author")?,
                count: r.try_get("count")?,
            })
        })
        .collect::<Result<_>>()?;

        // Callouts: how often each person is referenced via links.
        let callout_rows = crate::pgq::query(
            "SELECT target_id, count(*) AS count FROM links WHERE target_kind = 'person' \
             GROUP BY target_id ORDER BY count DESC",
        )
        .fetch_all(self.db())
        .await?;
        let mut callouts_by_person: Vec<PersonCallout> = Vec::new();
        for r in &callout_rows {
            let target_id: String = r.try_get("target_id")?;
            if let Some(p) = self.people_get(&target_id).await? {
                callouts_by_person.push(PersonCallout {
                    name: p.name,
                    slug: p.slug,
                    count: r.try_get("count")?,
                });
            }
        }

        // Mail archive totals — cheap COUNT/SUM scans, all zero pre-mail.
        let mail = MailDashboardStats {
            messages: count("SELECT count(*) FROM mail_messages WHERE deleted_at IS NULL").await?,
            accounts: count("SELECT count(*) FROM mail_accounts").await?,
            blob_bytes: count("SELECT COALESCE(SUM(size), 0)::BIGINT FROM blobs").await?,
            search: count("SELECT count(*) FROM search WHERE kind = 'mail'").await?,
        };

        Ok(DashboardStats {
            entries: count("SELECT count(*) FROM journal").await?,
            events: count("SELECT count(*) FROM events").await?,
            tasks,
            decisions,
            inbox,
            by_author,
            recent: self.wire_log(12).await?,
            tasks_with_due,
            entries_by_day,
            entries_by_author,
            callouts_by_person,
            mail,
        })
    }

    /// The whole knowledge graph (store.ts `graph`): every linked entity as a
    /// node, every link as an edge, plus derived edges — per-author journal
    /// chains, project→task, project→phase, phase→task.
    pub async fn graph(&self) -> Result<GraphData> {
        // Title resolution: embeddable items + people/topics/projects/phases.
        let mut title_of: HashMap<String, String> = HashMap::new();
        for it in self.embeddable_items().await? {
            title_of.insert(format!("{}:{}", it.kind, it.id), it.title);
        }
        for p in self.people_list().await? {
            title_of.insert(format!("person:{}", p.id), p.name);
        }
        let topics = crate::pgq::query("SELECT id, name FROM topics ORDER BY name")
            .fetch_all(self.db())
            .await?;
        for r in &topics {
            title_of.insert(
                format!("topic:{}", r.try_get::<String, _>("id")?),
                r.try_get("name")?,
            );
        }
        for p in self.projects_list().await? {
            title_of.insert(format!("project:{}", p.id), p.name);
        }
        let phases = crate::pgq::query(
            "SELECT id, project, name FROM phases ORDER BY project, position, created_at",
        )
        .fetch_all(self.db())
        .await?;
        for r in &phases {
            title_of.insert(
                format!("phase:{}", r.try_get::<String, _>("id")?),
                r.try_get("name")?,
            );
        }

        let mut nodes: Vec<GraphNode> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut add_node = |nodes: &mut Vec<GraphNode>, kind: EntityKind, ref_id: &str| {
            let key = format!("{}:{ref_id}", kind.as_str());
            if seen.insert(key.clone()) {
                nodes.push(GraphNode {
                    id: key.clone(),
                    kind: kind.as_str().to_string(),
                    title: title_of
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| ref_id.to_string()),
                });
            }
        };

        let link_rows = crate::pgq::query(
            "SELECT source_kind, source_id, target_kind, target_id, rel FROM links ORDER BY created_at",
        )
        .fetch_all(self.db())
        .await?;
        let mut edges: Vec<GraphEdge> = Vec::new();
        for r in &link_rows {
            let sk: String = r.try_get("source_kind")?;
            let si: String = r.try_get("source_id")?;
            let tk: String = r.try_get("target_kind")?;
            let ti: String = r.try_get("target_id")?;
            // Fail closed: skip links whose endpoint kinds this build doesn't
            // know rather than graphing them under a wrong kind.
            let (Some(sk), Some(tk)) = (EntityKind::parse(&sk), EntityKind::parse(&tk)) else {
                continue;
            };
            add_node(&mut nodes, sk, &si);
            add_node(&mut nodes, tk, &ti);
            edges.push(GraphEdge {
                source: format!("{}:{si}", sk.as_str()),
                target: format!("{}:{ti}", tk.as_str()),
                rel: r.try_get("rel")?,
            });
        }

        // Derived: per-author journal chain edges.
        let journal_rows =
            crate::pgq::query("SELECT id, author FROM journal ORDER BY author, created_at ASC")
                .fetch_all(self.db())
                .await?;
        let mut prev_author: Option<String> = None;
        let mut prev_id: Option<String> = None;
        for r in &journal_rows {
            let id: String = r.try_get("id")?;
            let author: String = r.try_get("author")?;
            if prev_author.as_deref() == Some(author.as_str()) {
                if let Some(prev) = &prev_id {
                    add_node(&mut nodes, EntityKind::Journal, prev);
                    add_node(&mut nodes, EntityKind::Journal, &id);
                    edges.push(GraphEdge {
                        source: format!("journal:{prev}"),
                        target: format!("journal:{id}"),
                        rel: "chain".to_string(),
                    });
                }
            } else {
                prev_author = Some(author);
            }
            prev_id = Some(id);
        }

        // Derived: project→task and project→phase edges from column values.
        let task_rows = crate::pgq::query(
            "SELECT id, project, phase FROM tasks ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
        )
        .fetch_all(self.db())
        .await?;
        for r in &task_rows {
            let id: String = r.try_get("id")?;
            if let Some(project) = r.try_get::<Option<String>, _>("project")? {
                add_node(&mut nodes, EntityKind::Project, &project);
                add_node(&mut nodes, EntityKind::Task, &id);
                edges.push(GraphEdge {
                    source: format!("project:{project}"),
                    target: format!("task:{id}"),
                    rel: "has_task".to_string(),
                });
            }
            if let Some(phase) = r.try_get::<Option<String>, _>("phase")? {
                add_node(&mut nodes, EntityKind::Phase, &phase);
                add_node(&mut nodes, EntityKind::Task, &id);
                edges.push(GraphEdge {
                    source: format!("phase:{phase}"),
                    target: format!("task:{id}"),
                    rel: "has_task".to_string(),
                });
            }
        }
        for r in &phases {
            let id: String = r.try_get("id")?;
            let project: String = r.try_get("project")?;
            add_node(&mut nodes, EntityKind::Project, &project);
            add_node(&mut nodes, EntityKind::Phase, &id);
            edges.push(GraphEdge {
                source: format!("project:{project}"),
                target: format!("phase:{id}"),
                rel: "has_phase".to_string(),
            });
        }

        Ok(GraphData { nodes, edges })
    }

    /// Typeahead autocomplete (store.ts `autocomplete`): matching people, open
    /// tasks, projects, topics, phases — ≤8 results.
    pub async fn autocomplete(
        &self,
        q: &str,
        kinds: Option<Vec<String>>,
    ) -> Result<Vec<AutocompleteItem>> {
        let lower = q.to_lowercase();
        let want = kinds.unwrap_or_else(|| {
            ["person", "task", "project", "topic", "phase"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
        let wants = |k: &str| want.iter().any(|w| w == k);
        let mut results: Vec<AutocompleteItem> = Vec::new();

        if wants("person") {
            for p in self.people_list().await? {
                if p.name.to_lowercase().contains(&lower) {
                    results.push(AutocompleteItem {
                        kind: EntityKind::Person,
                        id: p.id,
                        slug: p.slug,
                        label: p.name,
                    });
                }
            }
        }
        if wants("project") {
            for p in self.projects_list().await? {
                if p.name.to_lowercase().contains(&lower) {
                    results.push(AutocompleteItem {
                        kind: EntityKind::Project,
                        id: p.id,
                        slug: p.slug,
                        label: p.name,
                    });
                }
            }
        }
        if wants("topic") {
            let rows = crate::pgq::query("SELECT id, name, slug FROM topics ORDER BY name")
                .fetch_all(self.db())
                .await?;
            for r in &rows {
                let name: String = r.try_get("name")?;
                if name.to_lowercase().contains(&lower) {
                    results.push(AutocompleteItem {
                        kind: EntityKind::Topic,
                        id: r.try_get("id")?,
                        slug: r.try_get("slug")?,
                        label: name,
                    });
                }
            }
        }
        if wants("phase") {
            let rows = crate::pgq::query(
                "SELECT id, name FROM phases ORDER BY project, position, created_at",
            )
            .fetch_all(self.db())
            .await?;
            for r in &rows {
                let name: String = r.try_get("name")?;
                if name.to_lowercase().contains(&lower) {
                    results.push(AutocompleteItem {
                        kind: EntityKind::Phase,
                        id: r.try_get("id")?,
                        slug: slugify(&name),
                        label: name,
                    });
                }
            }
        }
        if wants("task") {
            // Node: tasks.list({status:'todo'}).concat(tasks.list({status:'doing'})).
            for status in ["todo", "doing"] {
                let rows = crate::pgq::query(
                    "SELECT id, title FROM tasks WHERE status = ? ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
                )
                .bind(status)
                .fetch_all(self.db())
                .await?;
                for r in &rows {
                    let title: String = r.try_get("title")?;
                    if title.to_lowercase().contains(&lower) {
                        results.push(AutocompleteItem {
                            kind: EntityKind::Task,
                            id: r.try_get("id")?,
                            slug: slugify(&title),
                            label: title,
                        });
                    }
                }
            }
        }

        results.truncate(8);
        Ok(results)
    }
}
