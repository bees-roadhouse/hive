// Actor delete-cascade + merge with dryRun previews (store.ts `actors`).
//
// The cutover shape (D18): both operations PLAN a record set from the current
// index state — the recipes below — and the live run commits it as one batch
// (one fold transaction). Previews run the SAME planning code and simply
// discard the drafts, so the counts match the live run exactly (the record
// successor of Node's RollbackPreview).
//
// Record recipes:
//   remove(slug) =
//     per authored entry: [entity.update decisions.supersedes→null]* +
//       [tombstone task|decision|event]* (fold drops row+FTS+vectors) +
//       [tombstone inbox]* + [link.remove]* + tombstone journal (fold drops
//       row+anchors+FTS+vectors)
//     + entity.update assignee scrubs on surviving tasks/decisions/events
//     + entity.update mentions scrubs on surviving entries (journal is
//       entity.update-addressable per fold contract v2)
//     + tombstone inbox (to/from the actor) + tombstone custom entities with
//       their link.removes + tombstone profile + tombstone source* +
//       entity.update sources.notify→null + [mail: link.remove* + tombstone
//       inbox* + tombstone mail_account (fold cascades mailboxes → messages →
//       attachments and drops their FTS/vectors)] + entity.update
//       people.owner→null + tombstone person
//     Runtime (not records, live run only): cc_credentials rows named by the
//     accounts, orphaned blob_refs + blockstore blobs, the wire ring purge.
//   merge(from, into) =
//     entity.update journal.author + journal.mentions rewrites +
//     assignees rewrites + inbox recipient/"from" rewrites + tombstone inbox
//     (now-self-addressed) + entity.update entities.created_by/user_scope +
//     entity.update sources.owner/notify + entity.update people.owner +
//     profile move (tombstone from-card, or tombstone+create when renaming
//     onto a vacant into-card) + tombstone person(from).
//     Runtime: the wire ring reassigns authorship in place.

use std::collections::HashSet;

use anyhow::Result;
use hive_shared::{ActorDeleteResult, ActorMergeResult};
use serde_json::json;

use super::{now_iso, Core, Draft, Store};

/// kind → table for the entities that emerge from journal entries.
const EMERGED: &[(&str, &str)] = &[
    ("task", "tasks"),
    ("decision", "decisions"),
    ("event", "events"),
];

/// Everything a live remove needs beyond the record batch.
struct RemovePlan {
    acc: ActorDeleteResult,
    drafts: Vec<Draft>,
    /// cc_credentials rows to delete (runtime table).
    cred_ids: Vec<String>,
    /// blob_refs rows that orphan once the cascade lands (runtime table).
    orphan_blobs: Vec<String>,
}

impl Store {
    /// Preview a delete WITHOUT mutating: the full record plan is computed and
    /// discarded, so the numbers match the live run exactly.
    pub async fn actors_remove_preview(&self, slug: &str) -> Result<ActorDeleteResult> {
        let slug_s = slug.to_string();
        let wire = self.wire_ring_count(slug);
        let mut acc = self
            .run(move |core| Ok(remove_plan(core, &slug_s)?.acc))
            .await?;
        acc.wire = wire;
        acc.dry_run = true;
        Ok(acc)
    }

    /// Delete an actor and cascade ALL their data — one record batch.
    pub async fn actors_remove(&self, slug: &str) -> Result<ActorDeleteResult> {
        let slug_s = slug.to_string();
        let mut acc = self
            .run(move |core| {
                let plan = remove_plan(core, &slug_s)?;
                core.commit(plan.drafts)?;
                for cred in &plan.cred_ids {
                    core.conn().execute(
                        "DELETE FROM cc_credentials WHERE id = ?1",
                        rusqlite::params![cred],
                    )?;
                }
                for hash in &plan.orphan_blobs {
                    delete_blob(core, hash);
                }
                Ok(plan.acc)
            })
            .await?;
        acc.wire = self.wire_ring_purge(slug);
        self.emit(
            "actor.removed",
            "admin",
            json!({"actor": slug, "journal": acc.journal}),
        )
        .await?;
        Ok(acc)
    }

    /// Preview a merge WITHOUT mutating.
    pub async fn actors_merge_preview(&self, from: &str, into: &str) -> Result<ActorMergeResult> {
        let (from_s, into_s) = (from.to_string(), into.to_string());
        let wire = self.wire_ring_count(from);
        let mut acc = self
            .run(move |core| Ok(merge_plan(core, &from_s, &into_s)?.0))
            .await?;
        acc.wire = wire;
        acc.dry_run = true;
        Ok(acc)
    }

    /// Fold `from` into `into`: reassign all authorship/ownership/refs, then
    /// remove the `from` people/profile rows — one record batch.
    pub async fn actors_merge(&self, from: &str, into: &str) -> Result<ActorMergeResult> {
        let (from_s, into_s) = (from.to_string(), into.to_string());
        let mut acc = self
            .run(move |core| {
                let (acc, drafts) = merge_plan(core, &from_s, &into_s)?;
                core.commit(drafts)?;
                Ok(acc)
            })
            .await?;
        acc.wire = self.wire_ring_reassign(from, into);
        self.emit(
            "actor.merged",
            "admin",
            json!({"from": from, "into": into, "journal": acc.journal}),
        )
        .await?;
        Ok(acc)
    }
}

fn bump_kind(acc: &mut ActorDeleteResult, kind: &str, n: i64) {
    match kind {
        "task" => acc.tasks += n,
        "decision" => acc.decisions += n,
        _ => acc.events += n,
    }
}

/// Remove `slug` from a JSON string-array column value; `None` when unchanged.
fn without_slug(json_arr: &str, slug: &str) -> Option<String> {
    let arr: Vec<String> = serde_json::from_str(json_arr).unwrap_or_default();
    let next: Vec<&String> = arr.iter().filter(|x| x.as_str() != slug).collect();
    if next.len() == arr.len() {
        return None;
    }
    Some(serde_json::to_string(&next).unwrap_or_else(|_| "[]".to_string()))
}

/// Replace `from`→`to` in a JSON string-array column, deduping; `None` if unchanged.
fn replace_slug(json_arr: &str, from: &str, to: &str) -> Option<String> {
    let arr: Vec<String> = serde_json::from_str(json_arr).unwrap_or_default();
    if !arr.iter().any(|x| x == from) {
        return None;
    }
    let mut next: Vec<String> = Vec::with_capacity(arr.len());
    for x in arr {
        let v = if x == from { to.to_string() } else { x };
        if !next.contains(&v) {
            next.push(v);
        }
    }
    Some(serde_json::to_string(&next).unwrap_or_else(|_| "[]".to_string()))
}

// ── shared read helpers ─────────────────────────────────────────────────────

fn ids_where(core: &Core, sql: &str, binds: &[&str]) -> Result<Vec<String>> {
    let mut stmt = core.conn().prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(binds.iter()), |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn count_where(core: &Core, sql: &str, binds: &[&str]) -> Result<i64> {
    Ok(core
        .conn()
        .query_row(sql, rusqlite::params_from_iter(binds.iter()), |r| r.get(0))?)
}

/// The mutating context one remove plan accumulates.
struct RemoveCtx {
    acc: ActorDeleteResult,
    drafts: Vec<Draft>,
    deleted_entries: HashSet<String>,
    deleted_entities: HashSet<String>,
    deleted_inbox: HashSet<String>,
    removed_links: HashSet<String>,
    ts: String,
}

impl RemoveCtx {
    fn tombstone(&mut self, kind: &str, id: &str) {
        self.drafts.push(Draft::new(
            crate::oplog::kind::TOMBSTONE,
            "admin",
            &self.ts,
            json!({"kind": kind, "id": id}),
        ));
    }

    fn update(&mut self, kind: &str, id: &str, fields: serde_json::Value) {
        self.drafts.push(Draft::new(
            crate::oplog::kind::ENTITY_UPDATE,
            "admin",
            &self.ts,
            json!({"kind": kind, "id": id, "fields": fields}),
        ));
    }

    fn tombstone_inbox_ids(&mut self, ids: Vec<String>) -> i64 {
        let mut n = 0;
        for id in ids {
            if self.deleted_inbox.insert(id.clone()) {
                self.tombstone("inbox", &id);
                n += 1;
            }
        }
        n
    }

    fn remove_link_ids(&mut self, ids: Vec<String>) -> i64 {
        let mut n = 0;
        let ts = self.ts.clone();
        for id in ids {
            if self.removed_links.insert(id.clone()) {
                self.drafts.push(super::links::link_remove_draft(&id, &ts));
                n += 1;
            }
        }
        n
    }
}

/// Count + plan the search/embeddings/link purges pointing at one entity or
/// entry. Search/embeddings deletions ride the tombstone in the fold; links
/// need explicit link.remove records.
fn purge_entity_indexes(core: &Core, ctx: &mut RemoveCtx, kind: &str, ref_id: &str) -> Result<()> {
    ctx.acc.search += count_where(
        core,
        "SELECT count(*) FROM search WHERE kind = ?1 AND ref_id = ?2",
        &[kind, ref_id],
    )?;
    ctx.acc.embeddings += count_where(
        core,
        "SELECT count(*) FROM embeddings WHERE ref_kind = ?1 AND ref_id = ?2",
        &[kind, ref_id],
    )?;
    // Links are undirected for cleanup: any edge that touches this id, either end.
    let link_ids = ids_where(
        core,
        "SELECT id FROM links WHERE (source_kind = ?1 AND source_id = ?2) OR (target_kind = ?1 AND target_id = ?2)",
        &[kind, ref_id],
    )?;
    ctx.acc.links += ctx.remove_link_ids(link_ids);
    Ok(())
}

/// Plan the deletion of one anchored entity (task/decision/event) and
/// everything that points at it.
fn delete_entity(core: &Core, ctx: &mut RemoveCtx, kind: &str, ref_id: &str) -> Result<()> {
    let key = format!("{kind}:{ref_id}");
    if !ctx.deleted_entities.insert(key) {
        return Ok(());
    }
    if kind == "decision" {
        // Clear supersedes pointers on SURVIVING decisions.
        let pointing = ids_where(
            core,
            "SELECT id FROM decisions WHERE supersedes = ?1",
            &[ref_id],
        )?;
        for id in pointing {
            if !ctx.deleted_entities.contains(&format!("decision:{id}")) {
                ctx.update(
                    "decision",
                    &id,
                    json!({"supersedes": null, "updated_at": ctx.ts}),
                );
            }
        }
    }
    let table = EMERGED.iter().find(|(k, _)| *k == kind).map(|(_, t)| *t);
    let Some(table) = table else { return Ok(()) };
    let n = count_where(
        core,
        &format!("SELECT count(*) FROM {table} WHERE id = ?1"),
        &[ref_id],
    )?;
    bump_kind(&mut ctx.acc, kind, n);
    ctx.tombstone(kind, ref_id);
    // Anchor rows die with their host entries' journal tombstones; count them
    // here for parity with the Postgres cascade's accounting.
    ctx.acc.anchors += count_where(
        core,
        "SELECT count(*) FROM anchors WHERE ref_id = ?1 AND kind = ?2",
        &[ref_id, kind],
    )?;
    let inbox_ids = ids_where(
        core,
        "SELECT id FROM inbox WHERE ref_id = ?1 AND ref_kind = ?2",
        &[ref_id, kind],
    )?;
    ctx.acc.inbox += ctx.tombstone_inbox_ids(inbox_ids);
    purge_entity_indexes(core, ctx, kind, ref_id)?;
    Ok(())
}

/// Plan the deletion of a journal entry and everything that emerged from it.
fn delete_journal_entry(core: &Core, ctx: &mut RemoveCtx, entry_id: &str) -> Result<()> {
    if !ctx.deleted_entries.insert(entry_id.to_string()) {
        return Ok(());
    }
    // Entities anchored to spans of this entry — the cascade's core rule.
    let anchored: Vec<(String, String)> = {
        let mut stmt = core
            .conn()
            .prepare("SELECT DISTINCT kind, ref_id FROM anchors WHERE entry_id = ?1")?;
        let rows = stmt.query_map(rusqlite::params![entry_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (kind, ref_id) in &anchored {
        if EMERGED.iter().any(|(k, _)| k == kind) {
            delete_entity(core, ctx, kind, ref_id)?;
        }
    }
    // Entities whose origin_entry_id is this entry but that weren't anchored
    // (bracket-token tasks link via "anchors" rel but always carry origin_entry_id;
    // belt-and-suspenders so nothing emerged from this entry is left orphaned).
    for (kind, table) in EMERGED {
        let ids = ids_where(
            core,
            &format!("SELECT id FROM {table} WHERE origin_entry_id = ?1"),
            &[entry_id],
        )?;
        for id in &ids {
            delete_entity(core, ctx, kind, id)?;
        }
    }
    // The entry's own dependents. Anchors die inside the journal tombstone.
    ctx.acc.anchors += count_where(
        core,
        "SELECT count(*) FROM anchors WHERE entry_id = ?1",
        &[entry_id],
    )?;
    let inbox_ids = ids_where(
        core,
        "SELECT id FROM inbox WHERE entry_id = ?1 OR (ref_kind = 'journal' AND ref_id = ?1)",
        &[entry_id],
    )?;
    ctx.acc.inbox += ctx.tombstone_inbox_ids(inbox_ids);
    purge_entity_indexes(core, ctx, "journal", entry_id)?;
    ctx.tombstone("journal", entry_id);
    ctx.acc.journal += 1;
    Ok(())
}

/// The planning body of actors.remove (see the module-header recipe).
fn remove_plan(core: &Core, slug: &str) -> Result<RemovePlan> {
    let mut ctx = RemoveCtx {
        acc: ActorDeleteResult {
            actor: slug.to_string(),
            ..Default::default()
        },
        drafts: Vec::new(),
        deleted_entries: HashSet::new(),
        deleted_entities: HashSet::new(),
        deleted_inbox: HashSet::new(),
        removed_links: HashSet::new(),
        ts: now_iso(),
    };

    // 1. Every journal entry the actor authored — cascade each (this folds in the
    //    anchored tasks/decisions/events + their indexes).
    let entries = ids_where(core, "SELECT id FROM journal WHERE author = ?1", &[slug])?;
    for entry_id in &entries {
        delete_journal_entry(core, &mut ctx, entry_id)?;
    }

    // 2. assignee scrub on entities NOT already deleted above: drop the slug from
    //    tasks/decisions/events assignee arrays so nothing assigns to a ghost.
    let like = format!("%\"{slug}\"%");
    for (kind, table) in EMERGED {
        let rows: Vec<(String, String)> = {
            let mut stmt = core.conn().prepare(&format!(
                "SELECT id, assignees FROM {table} WHERE assignees LIKE ?1"
            ))?;
            let rows = stmt.query_map(rusqlite::params![like], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, assignees) in &rows {
            if ctx.deleted_entities.contains(&format!("{kind}:{id}")) {
                continue;
            }
            if let Some(next) = without_slug(assignees, slug) {
                let ts = ctx.ts.clone();
                ctx.update(kind, id, json!({"assignees": next, "updated_at": ts}));
            }
        }
    }

    // 3. journal.mentions scrub on surviving entries (other authors who @mentioned slug).
    let mention_rows: Vec<(String, String)> = {
        let mut stmt = core
            .conn()
            .prepare("SELECT id, mentions FROM journal WHERE mentions LIKE ?1")?;
        let rows = stmt.query_map(rusqlite::params![like], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, mentions) in &mention_rows {
        if ctx.deleted_entries.contains(id) {
            continue;
        }
        if let Some(next) = without_slug(mentions, slug) {
            ctx.update("journal", id, json!({"mentions": next}));
        }
    }

    // 4. Inbox: anything to/from the actor (remaining items not tied to a deleted entry).
    let inbox_ids = ids_where(
        core,
        r#"SELECT id FROM inbox WHERE recipient = ?1 OR "from" = ?1"#,
        &[slug],
    )?;
    ctx.acc.inbox += ctx.tombstone_inbox_ids(inbox_ids);

    // 5. Custom entities: rows this actor authored or holds privately go,
    // with their search/embeddings/link rows (kind = the type slug).
    // Global rows created by OTHERS that merely reference the actor keep
    // their (now dangling) ref values — the next touch of that field 400s.
    let ent_rows: Vec<(String, String)> = {
        let mut stmt = core.conn().prepare(
            "SELECT e.id, t.slug FROM entities e JOIN entity_types t ON t.id = e.type_id \
             WHERE e.created_by = ?1 OR e.user_scope = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![slug], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (ent_id, type_slug) in &ent_rows {
        purge_entity_indexes(core, &mut ctx, type_slug, ent_id)?;
        ctx.tombstone(type_slug, ent_id);
        ctx.acc.entities += 1;
    }

    // 6. Profile card.
    let has_profile: bool = core.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM profile WHERE actor = ?1)",
        rusqlite::params![slug],
        |r| r.get(0),
    )?;
    if has_profile {
        ctx.tombstone("profile", slug);
        ctx.acc.profile += 1;
    }

    // 7. Wire events authored by the actor: the in-memory ring (counted and
    //    purged by the caller — no table since the cutover).

    // 8. Sources the actor owns; null out a `notify` that pointed at them.
    let source_ids = ids_where(core, "SELECT id FROM sources WHERE owner = ?1", &[slug])?;
    for id in &source_ids {
        ctx.tombstone("source", id);
        ctx.acc.sources += 1;
    }
    let notify_ids = ids_where(core, "SELECT id FROM sources WHERE notify = ?1", &[slug])?;
    for id in notify_ids {
        if !source_ids.contains(&id) {
            ctx.update("source", &id, json!({"notify": null}));
        }
    }

    // 9. Mail: derived rows for the owner's messages (search/embeddings/
    //     inbox/links) die inside the account tombstone's fold cascade; the
    //     vault credentials and orphaned blobs are runtime cleanup.
    let msg_ids = ids_where(
        core,
        "SELECT m.id FROM mail_messages m JOIN mail_accounts a ON a.id = m.account_id WHERE a.owner = ?1",
        &[slug],
    )?;
    for mid in &msg_ids {
        ctx.acc.search += count_where(
            core,
            "SELECT count(*) FROM search WHERE kind = 'mail' AND ref_id = ?1",
            &[mid],
        )?;
        ctx.acc.embeddings += count_where(
            core,
            "SELECT count(*) FROM embeddings WHERE ref_kind = 'mail' AND ref_id = ?1",
            &[mid],
        )?;
        let inbox_ids = ids_where(
            core,
            "SELECT id FROM inbox WHERE ref_kind = 'mail' AND ref_id = ?1",
            &[mid],
        )?;
        ctx.acc.inbox += ctx.tombstone_inbox_ids(inbox_ids);
        let link_ids = ids_where(
            core,
            "SELECT id FROM links WHERE (source_kind = 'mail' AND source_id = ?1) \
             OR (target_kind = 'mail' AND target_id = ?1)",
            &[mid],
        )?;
        ctx.acc.links += ctx.remove_link_ids(link_ids);
    }
    let cred_ids = ids_where(
        core,
        "SELECT cred_id FROM mail_accounts WHERE owner = ?1 AND cred_id IS NOT NULL",
        &[slug],
    )?;
    ctx.acc.mail_messages += msg_ids.len() as i64;
    let account_ids = ids_where(
        core,
        "SELECT id FROM mail_accounts WHERE owner = ?1",
        &[slug],
    )?;
    for id in &account_ids {
        ctx.tombstone("mail_account", id);
        ctx.acc.mail_accounts += 1;
    }
    // blob_refs rows orphaned once these accounts' attachments cascade away:
    // referenced by NO attachment outside the doomed message set.
    let orphan_blobs: Vec<String> = {
        let mut stmt = core.conn().prepare(
            "SELECT br.hash FROM blob_refs br WHERE NOT EXISTS (\
               SELECT 1 FROM mail_attachments a \
               JOIN mail_messages m ON m.id = a.message_id \
               JOIN mail_accounts acct ON acct.id = m.account_id \
               WHERE a.blob_hash = br.hash AND acct.owner != ?1)",
        )?;
        let rows = stmt.query_map(rusqlite::params![slug], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    ctx.acc.blobs += orphan_blobs.len() as i64;

    // 10. people.owner pointers (AIs this actor owned) → null, then the row itself.
    let owned_people = ids_where(core, "SELECT id FROM people WHERE owner = ?1", &[slug])?;
    for id in owned_people {
        ctx.update("person", &id, json!({"owner": null}));
    }
    if let Some(p) = super::people::person_by_slug(core.conn(), slug)? {
        ctx.tombstone("person", &p.id);
        ctx.acc.people += 1;
    }

    Ok(RemovePlan {
        acc: ctx.acc,
        drafts: ctx.drafts,
        cred_ids,
        orphan_blobs,
    })
}

/// Best-effort blockstore + blob_refs cleanup (runtime; live run only).
fn delete_blob(core: &mut Core, hash: &str) {
    let blob_ref: Option<Vec<u8>> = core
        .conn()
        .query_row(
            "SELECT ref FROM blob_refs WHERE hash = ?1",
            rusqlite::params![hash],
            |r| r.get(0),
        )
        .ok();
    if let Some(raw) = blob_ref {
        if let Ok(blob) = ciborium::from_reader::<crate::blockstore::BlobRef, _>(raw.as_slice()) {
            let keys = core.keys.clone();
            if let Err(e) = core.blocks.delete(keys.as_ref(), &blob) {
                tracing::warn!(hash, "blockstore delete failed during actor cascade: {e}");
            }
        }
    }
    let _ = core.conn().execute(
        "DELETE FROM blob_refs WHERE hash = ?1",
        rusqlite::params![hash],
    );
}

/// Draft accumulator for merge planning.
struct MergeCtx {
    drafts: Vec<Draft>,
    ts: String,
}

impl MergeCtx {
    fn update(&mut self, kind: &str, id: &str, fields: serde_json::Value) {
        self.drafts.push(Draft::new(
            crate::oplog::kind::ENTITY_UPDATE,
            "admin",
            &self.ts,
            json!({"kind": kind, "id": id, "fields": fields}),
        ));
    }
}

/// The planning body of actors.merge (see the module-header recipe).
fn merge_plan(core: &Core, from: &str, to: &str) -> Result<(ActorMergeResult, Vec<Draft>)> {
    if from == to {
        anyhow::bail!("cannot merge an actor into itself");
    }
    let mut acc = ActorMergeResult {
        from: from.to_string(),
        into: to.to_string(),
        ..Default::default()
    };
    let ts = now_iso();
    let mut m = MergeCtx {
        drafts: Vec::new(),
        ts,
    };

    // Reassign authorship + scrub the slug everywhere it acts as an identity.
    let authored = ids_where(core, "SELECT id FROM journal WHERE author = ?1", &[from])?;
    for id in &authored {
        m.update("journal", id, json!({"author": to}));
        acc.journal += 1;
    }

    // journal.mentions: rewrite from→to (dedupe) on every entry that mentioned from.
    let like = format!("%\"{from}\"%");
    let mention_rows: Vec<(String, String)> = {
        let mut stmt = core
            .conn()
            .prepare("SELECT id, mentions FROM journal WHERE mentions LIKE ?1")?;
        let rows = stmt.query_map(rusqlite::params![like], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, mentions) in &mention_rows {
        if let Some(next) = replace_slug(mentions, from, to) {
            m.update("journal", id, json!({"mentions": next}));
        }
    }

    // Assignees on tasks/decisions/events: from→to (dedupe).
    for (kind, table) in EMERGED {
        let rows: Vec<(String, String)> = {
            let mut stmt = core.conn().prepare(&format!(
                "SELECT id, assignees FROM {table} WHERE assignees LIKE ?1"
            ))?;
            let rows = stmt.query_map(rusqlite::params![like], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (id, assignees) in &rows {
            if let Some(next) = replace_slug(assignees, from, to) {
                m.update(kind, id, json!({"assignees": next, "updated_at": m.ts}));
                match *kind {
                    "task" => acc.tasks += 1,
                    "decision" => acc.decisions += 1,
                    _ => acc.events += 1,
                }
            }
        }
    }

    // Inbox: recipient + "from", then drop anything now self-addressed
    // (recipient == from post-rewrite — including pre-existing rows, matching
    // the Postgres DELETE that ran after the updates).
    let inbox_rows: Vec<(String, String, String)> = {
        let mut stmt = core
            .conn()
            .prepare(r#"SELECT id, recipient, "from" FROM inbox"#)?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, recipient, sender) in &inbox_rows {
        let new_recipient = if recipient == from { to } else { recipient };
        let new_sender = if sender == from { to } else { sender };
        if recipient == from {
            m.update("inbox", id, json!({"recipient": to}));
            acc.inbox += 1;
        }
        if sender == from {
            m.update("inbox", id, json!({"from": to}));
            acc.inbox += 1;
        }
        if new_recipient == new_sender {
            m.drafts.push(Draft::new(
                crate::oplog::kind::TOMBSTONE,
                "admin",
                &m.ts.clone(),
                json!({"kind": "inbox", "id": id}),
            ));
        }
    }
    // Custom entity authorship + scope.
    let created_rows: Vec<(String, String)> = {
        let mut stmt = core.conn().prepare(
            "SELECT e.id, t.slug FROM entities e JOIN entity_types t ON t.id = e.type_id WHERE e.created_by = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![from], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, slug) in &created_rows {
        m.update(slug, id, json!({"created_by": to}));
        acc.entities += 1;
    }
    let scoped_rows: Vec<(String, String)> = {
        let mut stmt = core.conn().prepare(
            "SELECT e.id, t.slug FROM entities e JOIN entity_types t ON t.id = e.type_id WHERE e.user_scope = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![from], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, slug) in &scoped_rows {
        m.update(slug, id, json!({"user_scope": to}));
        acc.entities += 1;
    }

    // Wire authorship: the in-memory ring, reassigned by the caller.

    // Sources owned by / notifying from.
    let owned_sources = ids_where(core, "SELECT id FROM sources WHERE owner = ?1", &[from])?;
    for id in &owned_sources {
        m.update("source", id, json!({"owner": to}));
        acc.sources += 1;
    }
    let notify_sources = ids_where(core, "SELECT id FROM sources WHERE notify = ?1", &[from])?;
    for id in &notify_sources {
        m.update("source", id, json!({"notify": to}));
    }

    // people.owner pointers (AIs the `from` human owned) now point at `to`.
    let owned_people = ids_where(core, "SELECT id FROM people WHERE owner = ?1", &[from])?;
    for id in &owned_people {
        m.update("person", id, json!({"owner": to}));
        acc.people_owner += 1;
    }

    // Profile card: the `to` card wins. Move the `from` card only if `to` has
    // none, else drop the `from` card. (A "move" is tombstone + re-create
    // under the new actor key — profile is keyed by actor.)
    let from_card = super::profile::profile_get_conn(core.conn(), from)?;
    let to_has_profile: bool = core.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM profile WHERE actor = ?1)",
        rusqlite::params![to],
        |r| r.get(0),
    )?;
    if let Some(card) = from_card {
        m.drafts.push(Draft::new(
            crate::oplog::kind::TOMBSTONE,
            "admin",
            &m.ts.clone(),
            json!({"kind": "profile", "id": from}),
        ));
        if !to_has_profile {
            m.drafts.push(Draft::new(
                crate::oplog::kind::ENTITY_CREATE,
                "admin",
                &m.ts.clone(),
                json!({"kind": "profile", "id": to, "fields": {
                    "kind": card.kind.as_str(),
                    "display_name": card.display_name,
                    "body": serde_json::to_string(&card.body)?,
                    "source": card.source.as_str(),
                    "derived_at": card.derived_at,
                    "updated_at": card.updated_at,
                }}),
            ));
        }
        acc.profile += 1;
    }

    // Finally remove the folded-away people row.
    if let Some(p) = super::people::person_by_slug(core.conn(), from)? {
        m.drafts.push(Draft::new(
            crate::oplog::kind::TOMBSTONE,
            "admin",
            &m.ts.clone(),
            json!({"kind": "person", "id": p.id}),
        ));
    }

    Ok((acc, m.drafts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_slug_removes_only_matches() {
        assert_eq!(
            without_slug(r#"["pia","apis"]"#, "pia").as_deref(),
            Some(r#"["apis"]"#)
        );
        assert_eq!(without_slug(r#"["apis"]"#, "pia"), None);
    }

    #[test]
    fn replace_slug_dedupes() {
        assert_eq!(
            replace_slug(r#"["pia","apis"]"#, "pia", "apis").as_deref(),
            Some(r#"["apis"]"#)
        );
        assert_eq!(
            replace_slug(r#"["pia"]"#, "pia", "cera").as_deref(),
            Some(r#"["cera"]"#)
        );
        assert_eq!(replace_slug(r#"["apis"]"#, "pia", "cera"), None);
    }
}
