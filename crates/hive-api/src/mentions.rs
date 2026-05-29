//! Universal mention pipeline: prose mentions (`@slug`, `[[type:identifier]]`,
//! `[[type:identifier|alias]]`, `[[slug-or-title]]`) in a journal entry /
//! note body get resolved to entities and projected into the `links` table
//! as `link_type='mention'` rows.
//!
//! Resolver shape (post-0014):
//!
//! - `@slug` ... checks `ai.slug` first (pia/apis/cera live there now), then
//!   `people.slug` (humans). The `target_table` on the resolved row is
//!   `'ai'` or `'people'` and that's the discriminator downstream. Identity
//!   slugs stay UNIQUE so the existing single-row lookup is fine.
//! - `[[type:identifier]]` ... typed lookup. If the identifier parses as a
//!   UUID (the canonical anchor the compose picker writes), look up by id.
//!   Otherwise fall back to slug. Slug is no longer UNIQUE on content
//!   tables, so multi-row matches pick newest-by-created_at.
//! - `[[slug-or-title]]` ... fuzzy. UUID identifiers bind directly to the
//!   first matching content table; non-UUID inputs go through the four-
//!   table slug scan. Single-table hit wins; multi-table tie ⇒ unresolved
//!   (UI prompts the human to disambiguate).
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

    // Bucket lookups by table. Two parallel buckets:
    //   - `ids`: UUIDs to look up by primary key (the canonical anchor).
    //   - `slugs`: fall-back slugs (newest-on-collision since slug is no
    //     longer UNIQUE on content tables).
    //
    // Routing:
    //   - `@slug` (Person) → ai (preferred) + people (fallback) by slug.
    //     Identity slugs stay UNIQUE so a plain slug match is enough.
    //   - Typed(t) → that one entity table. UUID-first, slug fallback.
    //   - Fuzzy → the four entity tables (tasks/notes/journal_entries/
    //     wire_events). UUID-first against each; if no UUID, slug fallback.
    //     Single-table hit wins; tie on slug ⇒ unresolved.
    let mut wanted_slugs: HashMap<&'static str, Vec<String>> = HashMap::new();
    let mut wanted_ids: HashMap<&'static str, Vec<Uuid>> = HashMap::new();
    let mut want_slug = |table: &'static str, slug: &str| {
        wanted_slugs
            .entry(table)
            .or_default()
            .push(slug.to_string());
    };
    let mut want_id = |table: &'static str, id: Uuid| {
        wanted_ids.entry(table).or_default().push(id);
    };

    for m in mentions {
        match m.kind {
            MentionKind::Person => {
                // No UUID flow for `@slug` ... identity grammar is slug-only.
                want_slug("ai", &m.slug);
                want_slug("people", &m.slug);
            }
            MentionKind::Typed(t) => {
                let table = table_for_typed(t);
                if let Some(id) = m.target_id {
                    want_id(table, id);
                } else {
                    want_slug(table, &m.slug);
                }
            }
            MentionKind::Fuzzy => {
                if let Some(id) = m.target_id {
                    // UUID-shaped freeform: try each content table by id.
                    for table in ["tasks", "notes", "journal_entries", "wire_events"] {
                        want_id(table, id);
                    }
                } else {
                    for table in ["tasks", "notes", "journal_entries", "wire_events"] {
                        want_slug(table, &m.slug);
                    }
                }
            }
        }
    }

    // Run one query per bucket. Slug queries return Vec<(slug, id)> with
    // newest-on-tie; id queries return Vec<Uuid> (just the matching ids).
    let mut found_by_slug: HashMap<(&'static str, String), Uuid> = HashMap::new();
    let mut found_by_id: HashMap<(&'static str, Uuid), ()> = HashMap::new();

    for (table, slugs) in wanted_slugs {
        // De-dup slugs to keep the query small.
        let mut uniq = slugs;
        uniq.sort();
        uniq.dedup();
        match query_slugs(pool, table, &uniq).await {
            Ok(rows) => {
                for (slug, id) in rows {
                    found_by_slug.insert((table, slug), id);
                }
            }
            Err(e) => {
                warn!(
                    table,
                    error = %e,
                    "mention resolver slug query failed; treating mentions for this table as unresolved"
                );
            }
        }
    }
    for (table, ids) in wanted_ids {
        let mut uniq = ids;
        uniq.sort();
        uniq.dedup();
        match query_ids(pool, table, &uniq).await {
            Ok(rows) => {
                for id in rows {
                    found_by_id.insert((table, id), ());
                }
            }
            Err(e) => {
                warn!(
                    table,
                    error = %e,
                    "mention resolver id query failed; treating mentions for this table as unresolved"
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
                if let Some(id) = found_by_slug.get(&("ai", m.slug.clone())) {
                    Some(ResolvedTarget {
                        table: "ai",
                        id: *id,
                    })
                } else {
                    found_by_slug
                        .get(&("people", m.slug.clone()))
                        .map(|id| ResolvedTarget {
                            table: "people",
                            id: *id,
                        })
                }
            }
            MentionKind::Typed(t) => {
                let table = table_for_typed(t);
                if let Some(id) = m.target_id {
                    if found_by_id.contains_key(&(table, id)) {
                        Some(ResolvedTarget { table, id })
                    } else {
                        None
                    }
                } else {
                    found_by_slug
                        .get(&(table, m.slug.clone()))
                        .map(|id| ResolvedTarget { table, id: *id })
                }
            }
            MentionKind::Fuzzy => {
                if let Some(id) = m.target_id {
                    // UUID freeform: take the FIRST content table that
                    // matches. UUIDs are globally unique, so the order is
                    // deterministic but ambiguity is impossible in practice.
                    let mut hit: Option<ResolvedTarget> = None;
                    for table in ["tasks", "notes", "journal_entries", "wire_events"] {
                        if found_by_id.contains_key(&(table, id)) {
                            hit = Some(ResolvedTarget { table, id });
                            break;
                        }
                    }
                    hit
                } else {
                    let mut hits: Vec<ResolvedTarget> = Vec::new();
                    for table in ["tasks", "notes", "journal_entries", "wire_events"] {
                        if let Some(id) = found_by_slug.get(&(table, m.slug.clone())) {
                            hits.push(ResolvedTarget { table, id: *id });
                        }
                    }
                    // Exactly-one rule: ambiguous fuzzy slug lookups are
                    // intentionally left unresolved. UI can prompt the human
                    // to disambiguate.
                    if hits.len() == 1 {
                        Some(hits.into_iter().next().unwrap())
                    } else {
                        None
                    }
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
    //
    // Newest-on-collision: with slug uniqueness dropped on content tables
    // (migration 0014), two rows can share a slug. `DISTINCT ON (slug)` plus
    // an `ORDER BY slug, created_at DESC` picks the newest per slug. Tables
    // without a `created_at` column (e.g. identity tables that happen to be
    // queried via this same path ... they aren't today, but be defensive)
    // are handled by the identity-only `ai` / `people` paths upstream.
    let sql = format!(
        "SELECT DISTINCT ON (slug) slug, id \
         FROM {table} \
         WHERE slug = ANY($1) \
         ORDER BY slug, created_at DESC, id DESC"
    );
    let rows = sqlx::query(&sql).bind(slugs).fetch_all(pool).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let slug: String = r.try_get("slug")?;
        let id: Uuid = r.try_get("id")?;
        out.push((slug, id));
    }
    Ok(out)
}

async fn query_ids(pool: &PgPool, table: &'static str, ids: &[Uuid]) -> sqlx::Result<Vec<Uuid>> {
    // Table is a static, fixed enum (see callers). Safe to interpolate.
    let sql = format!("SELECT id FROM {table} WHERE id = ANY($1)");
    let rows = sqlx::query(&sql).bind(ids).fetch_all(pool).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id: Uuid = r.try_get("id")?;
        out.push(id);
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
