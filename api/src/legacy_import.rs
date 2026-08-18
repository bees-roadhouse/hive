// Legacy hive.db reader — parity port of packages/api/src/legacy-import.ts.
// Reads a legacy hive.db (the old Python/Rust hive: independent journal/tasks/
// projects/links/messages tables) and maps it onto this instance's import
// payload. Opened READ-ONLY; each table is read defensively so a missing column
// or a single corrupt table (the old files have a known-bad wire_events page)
// can't abort the rest.

use anyhow::Result;
use hive_shared::{LegacyImport, LegacyJournalRow, LegacyLinkRow, LegacyProjectRow, LegacyTaskRow};
use sqlx::sqlite::{SqliteConnectOptions, SqliteRow};
use sqlx::{Connection, Row, SqliteConnection};

pub struct LegacyReadResult {
    pub payload: LegacyImport,
    pub warnings: Vec<String>,
}

const EPOCH: &str = "1970-01-01T00:00:00.000Z";

/// Node's local slugify: lowercase, [^a-z0-9]+ → "-", trim leading/trailing "-".
/// (Deliberately NOT hive_shared::slugify — the legacy importer folds
/// punctuation into hyphens.)
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.to_lowercase().trim().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Comma/whitespace-separated legacy tag string → clean array.
fn parse_tags(raw: &str) -> Vec<String> {
    raw.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// Legacy table name → this instance's link entity kind.
fn link_kind(table: &str) -> String {
    match table {
        "journal_entries" => "journal",
        "tasks" => "task",
        "projects" => "project",
        "notes" => "note",
        "messages" => "message",
        "wire_events" => "wire",
        other => other,
    }
    .to_string()
}

/// Node's `str()`: stringify whatever the legacy column holds; null/absent → "".
fn col_str(row: &SqliteRow, name: &str) -> String {
    if let Ok(v) = row.try_get::<Option<String>, _>(name) {
        return v.unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(name) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(name) {
        return v.map(|x| x.to_string()).unwrap_or_default();
    }
    String::new()
}

fn or_epoch(s: String) -> String {
    if s.is_empty() {
        EPOCH.to_string()
    } else {
        s
    }
}

async fn table_exists(conn: &mut SqliteConnection, name: &str) -> Result<bool> {
    Ok(
        sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?")
            .bind(name)
            .fetch_optional(&mut *conn)
            .await?
            .is_some(),
    )
}

/// Run a reader, swallowing errors (corrupt/absent table) so the rest still imports.
macro_rules! safe_read {
    ($label:expr, $warnings:expr, $read:expr) => {
        match $read {
            Ok(v) => v,
            Err(e) => {
                $warnings.push(format!("{}: {}", $label, e));
                Vec::new()
            }
        }
    };
}

pub async fn read_legacy_db(path: &std::path::Path) -> Result<LegacyReadResult> {
    let opts = SqliteConnectOptions::new().filename(path).read_only(true);
    let mut conn = SqliteConnection::connect_with(&opts).await?;
    let result = read_all(&mut conn).await;
    conn.close().await.ok();
    result
}

async fn read_all(conn: &mut SqliteConnection) -> Result<LegacyReadResult> {
    let mut warnings = Vec::new();

    // --- journal_entries → journal (title folds into the markdown body) ---
    let mut journal = if table_exists(conn, "journal_entries").await? {
        safe_read!(
            "journal_entries",
            warnings,
            read_journal_entries(conn).await
        )
    } else {
        Vec::new()
    };

    // --- messages → journal (no node equivalent; preserve as sender-authored notes) ---
    let messages = if table_exists(conn, "messages").await? {
        safe_read!("messages", warnings, read_messages(conn).await)
    } else {
        Vec::new()
    };
    journal.extend(messages);

    // --- projects ---
    let projects = if table_exists(conn, "projects").await? {
        safe_read!("projects", warnings, read_projects(conn).await)
    } else {
        Vec::new()
    };

    // --- tasks ---
    let tasks = if table_exists(conn, "tasks").await? {
        safe_read!("tasks", warnings, read_tasks(conn).await)
    } else {
        Vec::new()
    };

    // --- links ---
    let links = if table_exists(conn, "links").await? {
        safe_read!("links", warnings, read_links(conn).await)
    } else {
        Vec::new()
    };

    Ok(LegacyReadResult {
        payload: LegacyImport {
            journal: Some(journal),
            projects: Some(projects),
            tasks: Some(tasks),
            links: Some(links),
        },
        warnings,
    })
}

async fn read_journal_entries(conn: &mut SqliteConnection) -> Result<Vec<LegacyJournalRow>> {
    let rows = sqlx::query("SELECT * FROM journal_entries")
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let title = col_str(r, "title").trim().to_string();
            let body = col_str(r, "body");
            let ai = col_str(r, "ai");
            let created = col_str(r, "created_at");
            let entry_date = col_str(r, "entry_date");
            LegacyJournalRow {
                id: col_str(r, "id"),
                author: slugify(if ai.is_empty() { "unknown" } else { &ai }),
                body: if title.is_empty() {
                    body
                } else {
                    format!("# {title}\n\n{body}")
                },
                tags: parse_tags(&col_str(r, "tags")),
                created_at: or_epoch(if created.is_empty() {
                    entry_date
                } else {
                    created
                }),
            }
        })
        .collect())
}

async fn read_messages(conn: &mut SqliteConnection) -> Result<Vec<LegacyJournalRow>> {
    let rows = sqlx::query("SELECT * FROM messages")
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let sender = col_str(r, "sender_ai");
            LegacyJournalRow {
                id: col_str(r, "id"),
                author: slugify(if sender.is_empty() {
                    "unknown"
                } else {
                    &sender
                }),
                body: format!(
                    "**Message → @{} ({})**\n\n{}",
                    col_str(r, "recipient_ai"),
                    col_str(r, "kind"),
                    col_str(r, "body")
                ),
                tags: vec!["legacy-message".to_string()],
                created_at: or_epoch(col_str(r, "sent_at")),
            }
        })
        .collect())
}

async fn read_projects(conn: &mut SqliteConnection) -> Result<Vec<LegacyProjectRow>> {
    let rows = sqlx::query("SELECT * FROM projects")
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let name = col_str(r, "name");
            LegacyProjectRow {
                id: col_str(r, "id"),
                slug: slugify(&name),
                name,
                created_at: or_epoch(col_str(r, "created_at")),
            }
        })
        .collect())
}

async fn read_tasks(conn: &mut SqliteConnection) -> Result<Vec<LegacyTaskRow>> {
    let rows = sqlx::query("SELECT * FROM tasks")
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            // due/block_reason/closed_at have no node column → footnote into the body.
            let mut notes = Vec::new();
            let block_reason = col_str(r, "block_reason");
            if !block_reason.is_empty() {
                notes.push(format!("blocked: {block_reason}"));
            }
            let closed_at = col_str(r, "closed_at");
            if !closed_at.is_empty() {
                notes.push(format!("closed: {closed_at}"));
            }
            let body = format!(
                "{}{}",
                col_str(r, "body"),
                if notes.is_empty() {
                    String::new()
                } else {
                    format!("\n\n_{}_", notes.join(" · "))
                }
            );
            let owner = col_str(r, "owner").trim().to_string();
            let status = col_str(r, "status");
            let priority = col_str(r, "priority");
            let project = col_str(r, "project");
            let due = col_str(r, "due");
            let created = or_epoch(col_str(r, "created_at"));
            let updated = col_str(r, "updated_at");
            LegacyTaskRow {
                id: col_str(r, "id"),
                project: if project.is_empty() {
                    None
                } else {
                    Some(project)
                },
                title: col_str(r, "title"),
                body,
                status: if status.is_empty() {
                    "todo".to_string()
                } else {
                    status
                },
                priority: if priority.is_empty() {
                    "normal".to_string()
                } else {
                    priority
                },
                tags: Vec::new(),
                assignees: if owner.is_empty() {
                    Vec::new()
                } else {
                    vec![slugify(&owner)]
                },
                due: if due.is_empty() { None } else { Some(due) },
                updated_at: if updated.is_empty() {
                    created.clone()
                } else {
                    updated
                },
                created_at: created,
            }
        })
        .collect())
}

async fn read_links(conn: &mut SqliteConnection) -> Result<Vec<LegacyLinkRow>> {
    let rows = sqlx::query("SELECT * FROM links")
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let rel = col_str(r, "link_type");
            LegacyLinkRow {
                id: col_str(r, "id"),
                source_kind: link_kind(&col_str(r, "source_table")),
                source_id: col_str(r, "source_id"),
                target_kind: link_kind(&col_str(r, "target_table")),
                target_id: col_str(r, "target_id"),
                rel: if rel.is_empty() {
                    "relates".to_string()
                } else {
                    rel
                },
                created_at: or_epoch(col_str(r, "created_at")),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_folds_punctuation() {
        assert_eq!(slugify("Pia's Project"), "pia-s-project");
        assert_eq!(slugify("  Hello  World!! "), "hello-world");
        assert_eq!(slugify("---x---"), "x");
    }

    #[test]
    fn parse_tags_splits_commas_and_whitespace() {
        assert_eq!(
            parse_tags("a, b  c,,d"),
            vec!["a".to_string(), "b".into(), "c".into(), "d".into()]
        );
        assert!(parse_tags("").is_empty());
    }
}
