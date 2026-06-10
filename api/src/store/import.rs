// Bulk historical import — legacy hive.db → this instance (store.ts importLegacy).
// Idempotent: rows keep their original ids + timestamps; existing ids are
// skipped (INSERT OR IGNORE). Unlike journal.append this does NOT fan out
// inbox/anchor/share side effects; it only persists + indexes.

use anyhow::Result;
use hive_shared::{is_ai, parse_mentions, snip, ActorKind, ImportResult, LegacyImport, TaskStatus};

use super::search::index_entity_conn;
use super::Store;

impl Store {
    pub async fn import_legacy(&self, payload: LegacyImport) -> Result<ImportResult> {
        let mut res = ImportResult::default();

        // people.ensure fans through the pool (it's idempotent and additive), so
        // run it before the transactional batch like Node effectively does.
        for e in payload.journal.as_deref().unwrap_or_default() {
            self.people_ensure(
                &e.author,
                if is_ai(&e.author) {
                    ActorKind::Ai
                } else {
                    ActorKind::Human
                },
            )
            .await?;
        }

        let mut tx = self.db().begin().await?;

        for p in payload.projects.as_deref().unwrap_or_default() {
            let r = sqlx::query(
                "INSERT OR IGNORE INTO projects (id, name, slug, created_at) VALUES (?, ?, ?, ?)",
            )
            .bind(&p.id)
            .bind(&p.name)
            .bind(&p.slug)
            .bind(&p.created_at)
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() > 0 {
                res.projects.inserted += 1;
            } else {
                res.projects.skipped += 1;
            }
        }

        for e in payload.journal.as_deref().unwrap_or_default() {
            let r = sqlx::query(
                "INSERT OR IGNORE INTO journal (id, author, body, tags, mentions, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&e.id)
            .bind(&e.author)
            .bind(&e.body)
            .bind(serde_json::to_string(&e.tags)?)
            .bind(serde_json::to_string(&parse_mentions(&e.body))?)
            .bind(&e.created_at)
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() > 0 {
                let title = format!("{}: {}", e.author, snip(&e.body, 50));
                index_entity_conn(&mut tx, "journal", &e.id, &title, &e.body, &e.tags).await?;
                res.journal.inserted += 1;
            } else {
                res.journal.skipped += 1;
            }
        }

        for t in payload.tasks.as_deref().unwrap_or_default() {
            let status = if TaskStatus::parse(&t.status).is_some() {
                t.status.as_str()
            } else {
                "todo"
            };
            let priority = if t.priority.is_empty() {
                "normal"
            } else {
                t.priority.as_str()
            };
            let r = sqlx::query(
                "INSERT OR IGNORE INTO tasks (id, project, title, body, status, priority, tags, assignees, due, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&t.id)
            .bind(&t.project)
            .bind(&t.title)
            .bind(&t.body)
            .bind(status)
            .bind(priority)
            .bind(serde_json::to_string(&t.tags)?)
            .bind(serde_json::to_string(&t.assignees)?)
            .bind(&t.due)
            .bind(&t.created_at)
            .bind(&t.updated_at)
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() > 0 {
                index_entity_conn(&mut tx, "task", &t.id, &t.title, &t.body, &t.tags).await?;
                res.tasks.inserted += 1;
            } else {
                res.tasks.skipped += 1;
            }
        }

        for l in payload.links.as_deref().unwrap_or_default() {
            let r = sqlx::query(
                "INSERT OR IGNORE INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&l.id)
            .bind(&l.source_kind)
            .bind(&l.source_id)
            .bind(&l.target_kind)
            .bind(&l.target_id)
            .bind(&l.rel)
            .bind(&l.created_at)
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() > 0 {
                res.links.inserted += 1;
            } else {
                res.links.skipped += 1;
            }
        }

        tx.commit().await?;
        Ok(res)
    }
}
