//! Universal mention pipeline: prose mentions (`@slug`, `[[type:slug]]`,
//! `[[slug-or-title]]`) in a journal entry / note body get resolved to
//! entities and projected into the `links` table as `link_type='mention'`
//! rows.
//!
//! Resolver shape (post-0013):
//!
//! - `@slug` ... checks `ai.slug` first (pia/apis/cera live there now), then
//!   `people.slug` (humans). The `target_table` on the resolved row is
//!   `'ai'` or `'people'` and that's the discriminator downstream.
//! - `[[type:slug]]` ... typed lookup against the right table. The parser
//!   accepts a title after the type prefix and slugifies it before we get
//!   here, so the resolver only sees a slug.
//! - `[[slug-or-title]]` ... fuzzy. The parser pre-slugifies a non-slug
//!   inner so we only do exact slug matches at this layer. Tries the four
//!   entity tables (`tasks`, `notes`, `journal_entries`, `wire_events`);
//!   single-table hit wins; multi-table tie ⇒ unresolved.
//!
//! Wiring lives in the `add` handlers for journal + notes. Errors here are
//! LOGGED, never propagated ... the hook is a projection, not a constraint.
//! Re-running on the same body produces no duplicates (ON CONFLICT DO NOTHING
//! on the unique source/target tuple).

use std::collections::HashMap;

use hive_md::{EntityMention, MentionKind, TypedKind};
use sqlx::{PgPool, Row};
use tracing::warn;
use uuid::Uuid;

use hive_db::queries::anchors;

/// A mention that resolved to a concrete entity (or didn't).
#[derive(Debug, Clone)]
pub struct ResolvedMention {
    pub mention: EntityMention,
    pub target: Option<ResolvedTarget>,
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub table: &'static str,
    pub id: Uuid,
}

/// Batch-resolve a slice of mentions against the DB. Returns one entry per
/// input mention, in order, with `target: None` for unresolved cases (no row
/// matches, or a fuzzy lookup hit multiple tables).
pub async fn resolve_mentions(pool: &PgPool, mentions: &[EntityMention]) -> Vec<ResolvedMention> {
    if mentions.is_empty() {
        return Vec::new();
    }

    // Bucket slugs by the table we need to query.
    //
    // - `@slug` (Person) → ai (preferred) + people (fallback). Tried in
    //   that order at resolve time.
    // - Typed(t) → that one entity table.
    // - Fuzzy → the four entity tables (tasks/notes/journal_entries/
    //   wire_events). Persons/ais are NOT included in fuzzy: they have
    //   their own grammar (`@slug`). Single-table hit wins; tie ⇒
    //   unresolved.
    let mut wanted: HashMap<&'static str, Vec<String>> = HashMap::new();
    let mut want = |table: &'static str, slug: &str| {
        wanted.entry(table).or_default().push(slug.to_string());
    };

    for m in mentions {
        match m.kind {
            MentionKind::Person => {
                want("ai", &m.slug);
                want("people", &m.slug);
            }
            MentionKind::Typed(t) => want(table_for_typed(t), &m.slug),
            MentionKind::Fuzzy => {
                want("tasks", &m.slug);
                want("notes", &m.slug);
                want("journal_entries", &m.slug);
                want("wire_events", &m.slug);
            }
        }
    }

    // Run one query per bucket. Each returns Vec<(slug, id)>.
    let mut found: HashMap<(&'static str, String), Uuid> = HashMap::new();
    for (table, slugs) in wanted {
        // De-dup slugs to keep the query small.
        let mut uniq = slugs;
        uniq.sort();
        uniq.dedup();
        match query_slugs(pool, table, &uniq).await {
            Ok(rows) => {
                for (slug, id) in rows {
                    found.insert((table, slug), id);
                }
            }
            Err(e) => {
                // Don't fan out warnings if the issue is "column doesn't
                // exist yet" ... that's the temporary state while the slug
                // migration is in flight. Log once per table per call.
                warn!(
                    table,
                    error = %e,
                    "mention resolver query failed; treating mentions for this table as unresolved"
                );
            }
        }
    }

    let mut out = Vec::with_capacity(mentions.len());
    for m in mentions {
        let target = match m.kind {
            MentionKind::Person => {
                // AI first (pia/apis/cera live there post-0013), then humans.
                // The two tables share the `@slug` grammar but are queried
                // independently ... `target_table` carries the discriminator.
                if let Some(id) = found.get(&("ai", m.slug.clone())) {
                    Some(ResolvedTarget {
                        table: "ai",
                        id: *id,
                    })
                } else {
                    found
                        .get(&("people", m.slug.clone()))
                        .map(|id| ResolvedTarget {
                            table: "people",
                            id: *id,
                        })
                }
            }
            MentionKind::Typed(t) => {
                let table = table_for_typed(t);
                found
                    .get(&(table, m.slug.clone()))
                    .map(|id| ResolvedTarget { table, id: *id })
            }
            MentionKind::Fuzzy => {
                let mut hits: Vec<ResolvedTarget> = Vec::new();
                for table in ["tasks", "notes", "journal_entries", "wire_events"] {
                    if let Some(id) = found.get(&(table, m.slug.clone())) {
                        hits.push(ResolvedTarget { table, id: *id });
                    }
                }
                // Exactly-one rule: ambiguous fuzzy lookups are intentionally
                // left unresolved. UI can prompt the human to disambiguate.
                if hits.len() == 1 {
                    Some(hits.into_iter().next().unwrap())
                } else {
                    None
                }
            }
        };
        out.push(ResolvedMention {
            mention: m.clone(),
            target,
        });
    }

    out
}

fn table_for_typed(t: TypedKind) -> &'static str {
    match t {
        TypedKind::Task => "tasks",
        TypedKind::Note => "notes",
        TypedKind::Event => "wire_events",
        TypedKind::Journal => "journal_entries",
    }
}

async fn query_slugs(
    pool: &PgPool,
    table: &'static str,
    slugs: &[String],
) -> sqlx::Result<Vec<(String, Uuid)>> {
    // Table is a static, fixed enum (see callers). Safe to interpolate.
    let sql = format!("SELECT slug, id FROM {table} WHERE slug = ANY($1)");
    let rows = sqlx::query(&sql).bind(slugs).fetch_all(pool).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let slug: String = r.try_get("slug")?;
        let id: Uuid = r.try_get("id")?;
        out.push((slug, id));
    }
    Ok(out)
}

/// Upsert one `links` row per resolved mention. Idempotent: the existing
/// `(source_table, source_id, target_table, target_id, link_type)` uniqueness
/// constraint takes care of dupes; we use ON CONFLICT DO NOTHING so a
/// re-run of the projection is a no-op.
pub async fn upsert_mention_links(
    pool: &PgPool,
    source_table: &str,
    source_id: Uuid,
    resolved: &[ResolvedMention],
) -> sqlx::Result<usize> {
    let mut inserted = 0usize;
    for r in resolved {
        let Some(target) = &r.target else {
            continue; // unresolved mention ... skip per the design doc
        };
        // Truncate the raw string so a pasted blob can't blow the `note`
        // column ... links.note is TEXT, but a 64KB blob there is ugly.
        let note = truncate_raw(&r.mention.raw, 256);
        let res = sqlx::query(
            "INSERT INTO links (source_table, source_id, target_table, target_id, link_type, note) \
             VALUES ($1, $2, $3, $4, 'mention', $5) \
             ON CONFLICT (source_table, source_id, target_table, target_id, link_type) \
             DO NOTHING",
        )
        .bind(source_table)
        .bind(source_id)
        .bind(target.table)
        .bind(target.id)
        .bind(&note)
        .execute(pool)
        .await;
        match res {
            Ok(r) if r.rows_affected() > 0 => inserted += 1,
            Ok(_) => {} // existed already, fine
            Err(e) => {
                warn!(
                    source_table,
                    %source_id,
                    target_table = target.table,
                    target_id = %target.id,
                    error = %e,
                    "mention link upsert failed"
                );
            }
        }
    }
    Ok(inserted)
}

fn truncate_raw(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Project inline tasks parsed from a journal entry into `task_anchors` rows.
/// Each parsed task that has a `block_id` AND a real task row (matched by
/// title for now) gets an anchor.
///
/// TODO(handoff): this is the half-shipped version. We project anchors for
/// tasks that ALREADY exist with a matching title; we do NOT yet create new
/// task rows from inline `- [ ]` lines. The full pipeline ... auto-create
/// tasks for new inline checkboxes, with project derived from the surrounding
/// context ... needs:
///
///   1. A project default for journal-derived tasks (likely `journal-inbox`).
///   2. Title de-dup heuristics so editing a line text doesn't create a new
///      task; the block_id is the stable anchor, but linking by title on
///      first sight is the bootstrap path.
///   3. UI affordance for "this inline task became real task #X" so Nate sees
///      the projection happen.
///
/// Leaving this minimal until the slug / project bits stabilize. The
/// `task_anchors` upsert path itself IS idempotent and exercised below ... so
/// once we start creating tasks, the anchor write happens here without
/// further changes.
pub async fn upsert_task_anchors(
    pool: &PgPool,
    journal_entry_id: Uuid,
    parsed: &hive_md::ParsedBody,
) -> sqlx::Result<usize> {
    let mut written = 0usize;
    for t in &parsed.tasks {
        let Some(block_id) = &t.block_id else {
            continue;
        };
        // Look up an existing task by exact title. This is the bootstrap
        // path; once the full slug+project pipeline ships, we'll create the
        // task row here too.
        let row: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM tasks WHERE title = $1 ORDER BY created_at LIMIT 1")
                .bind(&t.text)
                .fetch_optional(pool)
                .await?;
        let Some((task_id,)) = row else {
            continue;
        };
        if let Err(e) = anchors::upsert(pool, task_id, journal_entry_id, block_id).await {
            warn!(
                %journal_entry_id,
                block_id,
                %task_id,
                error = %e,
                "task_anchors upsert failed"
            );
        } else {
            written += 1;
        }
    }
    Ok(written)
}

/// Run the full post-write projection for a freshly-inserted journal entry
/// or note body. Errors are logged, never propagated. The caller already
/// committed the entity row; this is a best-effort projection.
pub async fn project_body(pool: &PgPool, source_table: &str, source_id: Uuid, body: &str) {
    let parsed = hive_md::parse(body);

    let resolved = resolve_mentions(pool, &parsed.entity_mentions).await;
    if let Err(e) = upsert_mention_links(pool, source_table, source_id, &resolved).await {
        warn!(source_table, %source_id, error = %e, "mention link projection failed");
    }

    // task_anchors only applies to journal_entries (the table where inline
    // tasks live with their block ids).
    if source_table == "journal_entries"
        && let Err(e) = upsert_task_anchors(pool, source_id, &parsed).await
    {
        warn!(%source_id, error = %e, "task_anchors projection failed");
    }
}
