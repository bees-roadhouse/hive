// Dashboard stats + knowledge graph + typeahead autocomplete — parity port of
// store.ts `dashboard`/`graph`/`autocomplete`. Private SQL for journal/tasks/
// decisions/links/topics/phases per the decoupling rule (people/projects go
// through their owned row mappers). `recent` reads the in-memory wire ring
// (the wire table died with Postgres); mail blob bytes come from the
// blob_refs runtime table.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use hive_shared::{
    slugify, AuthorCount, AuthorEntryCount, AutocompleteItem, DashboardStats, DayCount,
    DecisionCounts, EntityKind, GraphData, GraphEdge, GraphNode, InboxStat, MailDashboardStats,
    PersonCallout, TaskCounts, TaskStatus, TaskWithDue, ACTORS,
};

use super::{json_vec, Core, Store};

fn count(core: &Core, sql: &str) -> Result<i64> {
    Ok(core.conn().query_row(sql, [], |r| r.get(0))?)
}

fn count_by(core: &Core, sql: &str, arg: &str) -> Result<i64> {
    Ok(core
        .conn()
        .query_row(sql, rusqlite::params![arg], |r| r.get(0))?)
}

impl Store {
    pub async fn dashboard(&self) -> Result<DashboardStats> {
        let recent = self.wire_log(12).await?;
        self.run(move |core| {
            let tasks = TaskCounts {
                total: count(core, "SELECT count(*) FROM tasks")?,
                todo: count_by(core, "SELECT count(*) FROM tasks WHERE status=?1", "todo")?,
                doing: count_by(core, "SELECT count(*) FROM tasks WHERE status=?1", "doing")?,
                blocked: count_by(
                    core,
                    "SELECT count(*) FROM tasks WHERE status=?1",
                    "blocked",
                )?,
                done: count_by(core, "SELECT count(*) FROM tasks WHERE status=?1", "done")?,
            };
            let decisions = DecisionCounts {
                total: count(core, "SELECT count(*) FROM decisions")?,
                proposed: count_by(
                    core,
                    "SELECT count(*) FROM decisions WHERE status=?1",
                    "proposed",
                )?,
                accepted: count_by(
                    core,
                    "SELECT count(*) FROM decisions WHERE status=?1",
                    "accepted",
                )?,
                rejected: count_by(
                    core,
                    "SELECT count(*) FROM decisions WHERE status=?1",
                    "rejected",
                )?,
                superseded: count_by(
                    core,
                    "SELECT count(*) FROM decisions WHERE status=?1",
                    "superseded",
                )?,
            };

            let by_author: Vec<AuthorCount> = {
                let mut stmt = core.conn().prepare(
                    "SELECT author, count(*) AS entries FROM journal GROUP BY author ORDER BY entries DESC",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(AuthorCount {
                        author: r.get(0)?,
                        entries: r.get(1)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };

            let mut inbox: Vec<InboxStat> = Vec::with_capacity(ACTORS.len());
            for (name, kind) in ACTORS {
                inbox.push(InboxStat {
                    recipient: name.to_string(),
                    kind: *kind,
                    unread: count_by(
                        core,
                        "SELECT count(*) FROM inbox WHERE recipient=?1 AND read_at IS NULL",
                        name,
                    )?,
                    total: count_by(core, "SELECT count(*) FROM inbox WHERE recipient=?1", name)?,
                });
            }

            // Open tasks with a due date (for calendar overlay).
            let tasks_with_due: Vec<TaskWithDue> = {
                let mut stmt = core.conn().prepare(
                    "SELECT id, title, due, status, assignees FROM tasks WHERE due IS NOT NULL AND status != 'done' ORDER BY due ASC",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(TaskWithDue {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        due: r.get(2)?,
                        status: TaskStatus::from_str_lossy(r.get::<_, String>(3)?.as_str()),
                        assignees: json_vec(&r.get::<_, String>(4)?),
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };

            // Entry counts per day for last 30 days (substr gives YYYY-MM-DD).
            // created_at is an ISO-8601 TEXT column, so compare against a
            // Rust-computed cutoff string (lexicographic order matches chronological).
            let cutoff = (chrono::Utc::now() - chrono::Duration::days(30))
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string();
            let entries_by_day: Vec<DayCount> = {
                let mut stmt = core.conn().prepare(
                    "SELECT substr(created_at, 1, 10) AS day, count(*) AS count \
                     FROM journal \
                     WHERE created_at >= ?1 \
                     GROUP BY day ORDER BY day ASC",
                )?;
                let rows = stmt.query_map(rusqlite::params![cutoff], |r| {
                    Ok(DayCount {
                        day: r.get(0)?,
                        count: r.get(1)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };

            let entries_by_author: Vec<AuthorEntryCount> = {
                let mut stmt = core.conn().prepare(
                    "SELECT author, count(*) AS count FROM journal GROUP BY author ORDER BY count DESC",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(AuthorEntryCount {
                        author: r.get(0)?,
                        count: r.get(1)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };

            // Callouts: how often each person is referenced via links.
            let callout_rows: Vec<(String, i64)> = {
                let mut stmt = core.conn().prepare(
                    "SELECT target_id, count(*) AS count FROM links WHERE target_kind = 'person' \
                     GROUP BY target_id ORDER BY count DESC",
                )?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut callouts_by_person: Vec<PersonCallout> = Vec::new();
            for (target_id, n) in &callout_rows {
                if let Some(p) = super::people::person_get(core.conn(), target_id)? {
                    callouts_by_person.push(PersonCallout {
                        name: p.name,
                        slug: p.slug,
                        count: *n,
                    });
                }
            }

            // Mail archive totals — cheap COUNT/SUM scans, all zero pre-mail.
            // blob bytes now live in the blockstore; blob_refs carries sizes.
            let mail = MailDashboardStats {
                messages: count(
                    core,
                    "SELECT count(*) FROM mail_messages WHERE deleted_at IS NULL",
                )?,
                accounts: count(core, "SELECT count(*) FROM mail_accounts")?,
                blob_bytes: count(core, "SELECT COALESCE(SUM(size), 0) FROM blob_refs")?,
                search: count(core, "SELECT count(*) FROM search WHERE kind = 'mail'")?,
            };

            Ok(DashboardStats {
                entries: count(core, "SELECT count(*) FROM journal")?,
                events: count(core, "SELECT count(*) FROM events")?,
                tasks,
                decisions,
                inbox,
                by_author,
                recent,
                tasks_with_due,
                entries_by_day,
                entries_by_author,
                callouts_by_person,
                mail,
            })
        })
        .await
    }

    /// The whole knowledge graph (store.ts `graph`): every linked entity as a
    /// node, every link as an edge, plus derived edges — per-author journal
    /// chains, project→task, project→phase, phase→task.
    pub async fn graph(&self) -> Result<GraphData> {
        self.run(|core| {
            // Title resolution: embeddable items + people/topics/projects/phases.
            let mut title_of: HashMap<String, String> = HashMap::new();
            for it in super::semantic::embeddable_items_core(core)? {
                title_of.insert(format!("{}:{}", it.kind, it.id), it.title);
            }
            {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT id, name FROM people ORDER BY kind, slug")?;
                let rows = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, name) = row?;
                    title_of.insert(format!("person:{id}"), name);
                }
            }
            {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT id, name FROM topics ORDER BY name")?;
                let rows = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, name) = row?;
                    title_of.insert(format!("topic:{id}"), name);
                }
            }
            {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT id, name FROM projects ORDER BY name")?;
                let rows = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, name) = row?;
                    title_of.insert(format!("project:{id}"), name);
                }
            }
            let phases: Vec<(String, String, String)> = {
                let mut stmt = core.conn().prepare(
                    "SELECT id, project, name FROM phases ORDER BY project, position, created_at",
                )?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (id, _project, name) in &phases {
                title_of.insert(format!("phase:{id}"), name.clone());
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

            let link_rows: Vec<(String, String, String, String, String)> = {
                let mut stmt = core.conn().prepare(
                    "SELECT source_kind, source_id, target_kind, target_id, rel FROM links ORDER BY created_at",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                    ))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut edges: Vec<GraphEdge> = Vec::new();
            for (sk, si, tk, ti, rel) in &link_rows {
                // Fail closed: skip links whose endpoint kinds this build doesn't
                // know rather than graphing them under a wrong kind.
                let (Some(sk), Some(tk)) = (EntityKind::parse(sk), EntityKind::parse(tk)) else {
                    continue;
                };
                add_node(&mut nodes, sk, si);
                add_node(&mut nodes, tk, ti);
                edges.push(GraphEdge {
                    source: format!("{}:{si}", sk.as_str()),
                    target: format!("{}:{ti}", tk.as_str()),
                    rel: rel.clone(),
                });
            }

            // Derived: per-author journal chain edges.
            let journal_rows: Vec<(String, String)> = {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT id, author FROM journal ORDER BY author, created_at ASC")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut prev_author: Option<String> = None;
            let mut prev_id: Option<String> = None;
            for (id, author) in &journal_rows {
                if prev_author.as_deref() == Some(author.as_str()) {
                    if let Some(prev) = &prev_id {
                        add_node(&mut nodes, EntityKind::Journal, prev);
                        add_node(&mut nodes, EntityKind::Journal, id);
                        edges.push(GraphEdge {
                            source: format!("journal:{prev}"),
                            target: format!("journal:{id}"),
                            rel: "chain".to_string(),
                        });
                    }
                } else {
                    prev_author = Some(author.clone());
                }
                prev_id = Some(id.clone());
            }

            // Derived: project→task and project→phase edges from column values.
            let task_rows: Vec<(String, Option<String>, Option<String>)> = {
                let mut stmt = core.conn().prepare(
                    "SELECT id, project, phase FROM tasks ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
                )?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (id, project, phase) in &task_rows {
                if let Some(project) = project {
                    add_node(&mut nodes, EntityKind::Project, project);
                    add_node(&mut nodes, EntityKind::Task, id);
                    edges.push(GraphEdge {
                        source: format!("project:{project}"),
                        target: format!("task:{id}"),
                        rel: "has_task".to_string(),
                    });
                }
                if let Some(phase) = phase {
                    add_node(&mut nodes, EntityKind::Phase, phase);
                    add_node(&mut nodes, EntityKind::Task, id);
                    edges.push(GraphEdge {
                        source: format!("phase:{phase}"),
                        target: format!("task:{id}"),
                        rel: "has_task".to_string(),
                    });
                }
            }
            for (id, project, _name) in &phases {
                add_node(&mut nodes, EntityKind::Project, project);
                add_node(&mut nodes, EntityKind::Phase, id);
                edges.push(GraphEdge {
                    source: format!("project:{project}"),
                    target: format!("phase:{id}"),
                    rel: "has_phase".to_string(),
                });
            }

            Ok(GraphData { nodes, edges })
        })
        .await
    }

    /// Typeahead autocomplete (store.ts `autocomplete`): matching people, open
    /// tasks, projects, topics, phases — ≤8 results.
    pub async fn autocomplete(
        &self,
        q: &str,
        kinds: Option<Vec<String>>,
    ) -> Result<Vec<AutocompleteItem>> {
        let lower = q.to_lowercase();
        self.run(move |core| {
            let want = kinds.unwrap_or_else(|| {
                ["person", "task", "project", "topic", "phase"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
            let wants = |k: &str| want.iter().any(|w| w == k);
            let mut results: Vec<AutocompleteItem> = Vec::new();

            if wants("person") {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT id, slug, name FROM people ORDER BY kind, slug")?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                for row in rows {
                    let (id, slug, name) = row?;
                    if name.to_lowercase().contains(&lower) {
                        results.push(AutocompleteItem {
                            kind: EntityKind::Person,
                            id,
                            slug,
                            label: name,
                        });
                    }
                }
            }
            if wants("project") {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT id, slug, name FROM projects ORDER BY name")?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                for row in rows {
                    let (id, slug, name) = row?;
                    if name.to_lowercase().contains(&lower) {
                        results.push(AutocompleteItem {
                            kind: EntityKind::Project,
                            id,
                            slug,
                            label: name,
                        });
                    }
                }
            }
            if wants("topic") {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT id, name, slug FROM topics ORDER BY name")?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                for row in rows {
                    let (id, name, slug) = row?;
                    if name.to_lowercase().contains(&lower) {
                        results.push(AutocompleteItem {
                            kind: EntityKind::Topic,
                            id,
                            slug,
                            label: name,
                        });
                    }
                }
            }
            if wants("phase") {
                let mut stmt = core.conn().prepare(
                    "SELECT id, name FROM phases ORDER BY project, position, created_at",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (id, name) = row?;
                    if name.to_lowercase().contains(&lower) {
                        results.push(AutocompleteItem {
                            kind: EntityKind::Phase,
                            id,
                            slug: slugify(&name),
                            label: name,
                        });
                    }
                }
            }
            if wants("task") {
                // Node: tasks.list({status:'todo'}).concat(tasks.list({status:'doing'})).
                for status in ["todo", "doing"] {
                    let mut stmt = core.conn().prepare(
                        "SELECT id, title FROM tasks WHERE status = ?1 ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
                    )?;
                    let rows = stmt.query_map(rusqlite::params![status], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?;
                    for row in rows {
                        let (id, title) = row?;
                        if title.to_lowercase().contains(&lower) {
                            results.push(AutocompleteItem {
                                kind: EntityKind::Task,
                                id,
                                slug: slugify(&title),
                                label: title,
                            });
                        }
                    }
                }
            }

            results.truncate(8);
            Ok(results)
        })
        .await
    }
}
