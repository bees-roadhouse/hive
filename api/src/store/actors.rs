// Actor delete-cascade + merge with dryRun previews (store.ts `actors`).
// Both run inside one transaction so a mid-operation failure rolls back fully.
// Previews run the SAME mutating code and then roll the transaction back
// (Node's RollbackPreview), so the counts match the live run exactly.
//
// Decoupling: journal/tasks/decisions/events/shares live in concurrently-built
// modules, so everything here is raw SQL on the transaction connection — which
// is what Node does anyway (one big SQL transaction).

use anyhow::Result;
use hive_shared::{ActorDeleteResult, ActorMergeResult};
use serde_json::json;
use sqlx::{PgConnection, Row};

use super::Store;

/// kind → table for the entities that emerge from journal entries.
const EMERGED: &[(&str, &str)] = &[
    ("task", "tasks"),
    ("decision", "decisions"),
    ("event", "events"),
];

impl Store {
    /// Preview a delete WITHOUT mutating: the full cascade runs in a transaction
    /// that always rolls back, so the numbers match the live run exactly.
    pub async fn actors_remove_preview(&self, slug: &str) -> Result<ActorDeleteResult> {
        let mut tx = self.db().begin().await?;
        let mut acc = remove_in_tx(&mut tx, slug).await?;
        tx.rollback().await?;
        acc.dry_run = true;
        Ok(acc)
    }

    /// Delete an actor and cascade ALL their data. Transactional.
    pub async fn actors_remove(&self, slug: &str) -> Result<ActorDeleteResult> {
        let mut tx = self.db().begin().await?;
        let acc = remove_in_tx(&mut tx, slug).await?;
        tx.commit().await?;
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
        let mut tx = self.db().begin().await?;
        let mut acc = merge_in_tx(&mut tx, from, into).await?;
        tx.rollback().await?;
        acc.dry_run = true;
        Ok(acc)
    }

    /// Fold `from` into `into`: reassign all authorship/ownership/refs, then
    /// remove the `from` people/profile/users rows. Transactional.
    pub async fn actors_merge(&self, from: &str, into: &str) -> Result<ActorMergeResult> {
        let mut tx = self.db().begin().await?;
        let acc = merge_in_tx(&mut tx, from, into).await?;
        tx.commit().await?;
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

async fn exec_count(conn: &mut PgConnection, sql: &str, binds: &[&str]) -> Result<i64> {
    let mut q = crate::pgq::query(sql);
    for b in binds {
        q = q.bind(*b);
    }
    Ok(q.execute(&mut *conn).await?.rows_affected() as i64)
}

/// Strip the search-index + embeddings + link rows pointing at a structured
/// entity or journal entry. Shared by entity and entry deletes.
async fn purge_entity_indexes(
    conn: &mut PgConnection,
    kind: &str,
    ref_id: &str,
) -> Result<(i64, i64, i64)> {
    let search = exec_count(
        conn,
        "DELETE FROM search WHERE kind = ? AND ref_id = ?",
        &[kind, ref_id],
    )
    .await?;
    let embeddings = exec_count(
        conn,
        "DELETE FROM embeddings WHERE ref_kind = ? AND ref_id = ?",
        &[kind, ref_id],
    )
    .await?;
    // Links are undirected for cleanup: any edge that touches this id, either end.
    let links = exec_count(
        conn,
        "DELETE FROM links WHERE (source_kind = ? AND source_id = ?) OR (target_kind = ? AND target_id = ?)",
        &[kind, ref_id, kind, ref_id],
    )
    .await?;
    Ok((search, embeddings, links))
}

/// Delete one anchored entity (task/decision/event) and everything that points
/// at it: its anchors row, inbox items, search/embeddings/links, and any
/// decision that supersedes it (the supersedes pointer is cleared).
async fn delete_entity(
    conn: &mut PgConnection,
    kind: &str,
    table: &str,
    ref_id: &str,
    acc: &mut ActorDeleteResult,
) -> Result<()> {
    if kind == "decision" {
        exec_count(
            conn,
            "UPDATE decisions SET supersedes = NULL WHERE supersedes = ?",
            &[ref_id],
        )
        .await?;
    }
    let n = exec_count(
        conn,
        &format!("DELETE FROM {table} WHERE id = ?"),
        &[ref_id],
    )
    .await?;
    bump_kind(acc, kind, n);
    acc.anchors += exec_count(
        conn,
        "DELETE FROM anchors WHERE ref_id = ? AND kind = ?",
        &[ref_id, kind],
    )
    .await?;
    acc.inbox += exec_count(
        conn,
        "DELETE FROM inbox WHERE ref_id = ? AND ref_kind = ?",
        &[ref_id, kind],
    )
    .await?;
    let (search, embeddings, links) = purge_entity_indexes(conn, kind, ref_id).await?;
    acc.search += search;
    acc.embeddings += embeddings;
    acc.links += links;
    Ok(())
}

/// Delete a journal entry and cascade everything that emerged from it.
/// Must run inside the caller's transaction.
async fn delete_journal_entry(
    conn: &mut PgConnection,
    entry_id: &str,
    acc: &mut ActorDeleteResult,
) -> Result<()> {
    // Entities anchored to spans of this entry — the cascade's core rule.
    let anchored: Vec<(String, String)> =
        crate::pgq::query_as("SELECT DISTINCT kind, ref_id FROM anchors WHERE entry_id = ?")
            .bind(entry_id)
            .fetch_all(&mut *conn)
            .await?;
    for (kind, ref_id) in &anchored {
        if let Some((k, table)) = EMERGED.iter().find(|(k, _)| k == kind) {
            delete_entity(conn, k, table, ref_id, acc).await?;
        }
    }
    // Entities whose origin_entry_id is this entry but that weren't anchored
    // (bracket-token tasks link via "anchors" rel but always carry origin_entry_id;
    // belt-and-suspenders so nothing emerged from this entry is left orphaned).
    for (kind, table) in EMERGED {
        let ids: Vec<(String,)> =
            crate::pgq::query_as(&format!("SELECT id FROM {table} WHERE origin_entry_id = ?"))
                .bind(entry_id)
                .fetch_all(&mut *conn)
                .await?;
        for (id,) in &ids {
            delete_entity(conn, kind, table, id, acc).await?;
        }
    }
    // The entry's own dependents.
    acc.anchors += exec_count(conn, "DELETE FROM anchors WHERE entry_id = ?", &[entry_id]).await?;
    acc.inbox += exec_count(
        conn,
        "DELETE FROM inbox WHERE entry_id = ? OR (ref_kind = 'journal' AND ref_id = ?)",
        &[entry_id, entry_id],
    )
    .await?;
    acc.shares += exec_count(
        conn,
        "DELETE FROM shares WHERE scope = 'entry' AND ref = ?",
        &[entry_id],
    )
    .await?;
    let (search, embeddings, links) = purge_entity_indexes(conn, "journal", entry_id).await?;
    acc.search += search;
    acc.embeddings += embeddings;
    acc.links += links;
    acc.journal += exec_count(conn, "DELETE FROM journal WHERE id = ?", &[entry_id]).await?;
    Ok(())
}

/// The mutating body of actors.remove, run inside the caller's transaction.
async fn remove_in_tx(conn: &mut PgConnection, slug: &str) -> Result<ActorDeleteResult> {
    let mut acc = ActorDeleteResult {
        actor: slug.to_string(),
        ..Default::default()
    };

    // 1. Every journal entry the actor authored — cascade each (this folds in the
    //    anchored tasks/decisions/events + their indexes).
    let entries: Vec<(String,)> = crate::pgq::query_as("SELECT id FROM journal WHERE author = ?")
        .bind(slug)
        .fetch_all(&mut *conn)
        .await?;
    for (entry_id,) in &entries {
        delete_journal_entry(conn, entry_id, &mut acc).await?;
    }

    // 2. assignee scrub on entities NOT already deleted above: drop the slug from
    //    tasks/decisions/events assignee arrays so nothing assigns to a ghost.
    let like = format!("%\"{slug}\"%");
    for (_, table) in EMERGED {
        let rows: Vec<(String, String)> = crate::pgq::query_as(&format!(
            "SELECT id, assignees FROM {table} WHERE assignees LIKE ?"
        ))
        .bind(&like)
        .fetch_all(&mut *conn)
        .await?;
        for (id, assignees) in &rows {
            if let Some(next) = without_slug(assignees, slug) {
                crate::pgq::query(&format!("UPDATE {table} SET assignees = ? WHERE id = ?"))
                    .bind(&next)
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
            }
        }
    }

    // 3. journal.mentions scrub on surviving entries (other authors who @mentioned slug).
    let mention_rows: Vec<(String, String)> =
        crate::pgq::query_as("SELECT id, mentions FROM journal WHERE mentions LIKE ?")
            .bind(&like)
            .fetch_all(&mut *conn)
            .await?;
    for (id, mentions) in &mention_rows {
        if let Some(next) = without_slug(mentions, slug) {
            crate::pgq::query("UPDATE journal SET mentions = ? WHERE id = ?")
                .bind(&next)
                .bind(id)
                .execute(&mut *conn)
                .await?;
        }
    }

    // 4. Inbox: anything to/from the actor (remaining items not tied to a deleted entry).
    acc.inbox += exec_count(
        conn,
        r#"DELETE FROM inbox WHERE recipient = ? OR "from" = ?"#,
        &[slug, slug],
    )
    .await?;

    // 5. Shares: as viewer, or journal-scoped shares OF this author's stream.
    acc.shares += exec_count(
        conn,
        "DELETE FROM shares WHERE viewer = ? OR (scope = 'journal' AND ref = ?)",
        &[slug, slug],
    )
    .await?;

    // 5b. Custom entities: rows this actor authored or holds privately go,
    // with their search/embeddings/field-mirror links (kind = the type slug).
    // Global rows created by OTHERS that merely reference the actor keep
    // their (now dangling) ref values — the next touch of that field 400s.
    let ent_rows: Vec<(String, String)> = crate::pgq::query_as::<(String, String)>(
        "SELECT e.id, t.slug FROM entities e JOIN entity_types t ON t.id = e.type_id \
         WHERE e.created_by = ? OR e.user_scope = ?",
    )
    .bind(slug)
    .bind(slug)
    .fetch_all(&mut *conn)
    .await?;
    for (ent_id, type_slug) in &ent_rows {
        let (search, embeddings, links) = purge_entity_indexes(conn, type_slug, ent_id).await?;
        acc.search += search;
        acc.embeddings += embeddings;
        acc.links += links;
        acc.entities += exec_count(conn, "DELETE FROM entities WHERE id = ?", &[ent_id]).await?;
    }

    // 6. Profile card.
    acc.profile += exec_count(conn, "DELETE FROM profile WHERE actor = ?", &[slug]).await?;

    // 7. Login + credentials. Sessions hang off the user row (user_id), so reap
    //    them first, then the user, then any bearer tokens for this actor.
    let usr_rows: Vec<(String,)> = crate::pgq::query_as("SELECT id FROM users WHERE actor = ?")
        .bind(slug)
        .fetch_all(&mut *conn)
        .await?;
    for (user_id,) in &usr_rows {
        acc.sessions +=
            exec_count(conn, "DELETE FROM sessions WHERE user_id = ?", &[user_id]).await?;
    }
    acc.users += exec_count(conn, "DELETE FROM users WHERE actor = ?", &[slug]).await?;
    acc.api_tokens += exec_count(
        conn,
        "DELETE FROM api_tokens WHERE actor = ? OR created_by = ? OR granted_by = ?",
        &[slug, slug, slug],
    )
    .await?;
    acc.oauth_codes += exec_count(
        conn,
        "DELETE FROM oauth_auth_codes WHERE ai_actor = ? OR granted_by = ?",
        &[slug, slug],
    )
    .await?;

    // 8. Wire events authored by the actor.
    acc.wire += exec_count(conn, "DELETE FROM wire WHERE actor = ?", &[slug]).await?;

    // 9. Sources the actor owns; null out a `notify` that pointed at them.
    acc.sources += exec_count(conn, "DELETE FROM sources WHERE owner = ?", &[slug]).await?;
    exec_count(
        conn,
        "UPDATE sources SET notify = NULL WHERE notify = ?",
        &[slug],
    )
    .await?;

    // 10. people.owner pointers (AIs this actor owned) → null, then the row itself.
    exec_count(
        conn,
        "UPDATE people SET owner = NULL WHERE owner = ?",
        &[slug],
    )
    .await?;
    acc.people += exec_count(conn, "DELETE FROM people WHERE slug = ?", &[slug]).await?;

    Ok(acc)
}

/// The mutating body of actors.merge, run inside the caller's transaction.
async fn merge_in_tx(conn: &mut PgConnection, from: &str, to: &str) -> Result<ActorMergeResult> {
    if from == to {
        anyhow::bail!("cannot merge an actor into itself");
    }
    let mut acc = ActorMergeResult {
        from: from.to_string(),
        into: to.to_string(),
        ..Default::default()
    };

    // Reassign authorship + scrub the slug everywhere it acts as an identity.
    acc.journal += exec_count(
        conn,
        "UPDATE journal SET author = ? WHERE author = ?",
        &[to, from],
    )
    .await?;

    // journal.mentions: rewrite from→to (dedupe) on every entry that mentioned from.
    let like = format!("%\"{from}\"%");
    let mention_rows: Vec<(String, String)> =
        crate::pgq::query_as("SELECT id, mentions FROM journal WHERE mentions LIKE ?")
            .bind(&like)
            .fetch_all(&mut *conn)
            .await?;
    for (id, mentions) in &mention_rows {
        if let Some(next) = replace_slug(mentions, from, to) {
            crate::pgq::query("UPDATE journal SET mentions = ? WHERE id = ?")
                .bind(&next)
                .bind(id)
                .execute(&mut *conn)
                .await?;
        }
    }

    // Assignees on tasks/decisions/events: from→to (dedupe).
    for (kind, table) in EMERGED {
        let rows: Vec<(String, String)> = crate::pgq::query_as(&format!(
            "SELECT id, assignees FROM {table} WHERE assignees LIKE ?"
        ))
        .bind(&like)
        .fetch_all(&mut *conn)
        .await?;
        for (id, assignees) in &rows {
            if let Some(next) = replace_slug(assignees, from, to) {
                crate::pgq::query(&format!("UPDATE {table} SET assignees = ? WHERE id = ?"))
                    .bind(&next)
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
                match *kind {
                    "task" => acc.tasks += 1,
                    "decision" => acc.decisions += 1,
                    _ => acc.events += 1,
                }
            }
        }
    }

    // Inbox: recipient + "from".
    acc.inbox += exec_count(
        conn,
        "UPDATE inbox SET recipient = ? WHERE recipient = ?",
        &[to, from],
    )
    .await?;
    acc.inbox += exec_count(
        conn,
        r#"UPDATE inbox SET "from" = ? WHERE "from" = ?"#,
        &[to, from],
    )
    .await?;
    // Drop any now-self-addressed items the move created (recipient === from).
    exec_count(conn, r#"DELETE FROM inbox WHERE recipient = "from""#, &[]).await?;

    // Shares: viewer + journal-scoped ref. A move can collide with an existing
    // (scope,ref,viewer) row (unique index), so reassign only where no twin exists,
    // and delete the leftover duplicates.
    for (col, where_sql) in [
        ("viewer", "viewer = ?"),
        ("ref", "scope = 'journal' AND ref = ?"),
    ] {
        let rows = crate::pgq::query(&format!(
            "SELECT id, scope, ref, viewer FROM shares WHERE {where_sql}"
        ))
        .bind(from)
        .fetch_all(&mut *conn)
        .await?;
        for r in &rows {
            let id: String = r.try_get("id")?;
            let scope: String = r.try_get("scope")?;
            let s_ref: String = r.try_get("ref")?;
            let viewer: String = r.try_get("viewer")?;
            let next_ref = if col == "ref" { to } else { s_ref.as_str() };
            let next_viewer = if col == "viewer" { to } else { viewer.as_str() };
            let twin = crate::pgq::query(
                "SELECT 1 FROM shares WHERE scope = ? AND ref = ? AND viewer = ? AND id != ?",
            )
            .bind(&scope)
            .bind(next_ref)
            .bind(next_viewer)
            .bind(&id)
            .fetch_optional(&mut *conn)
            .await?;
            if twin.is_some() {
                exec_count(conn, "DELETE FROM shares WHERE id = ?", &[&id]).await?;
            } else {
                exec_count(
                    conn,
                    &format!("UPDATE shares SET {col} = ? WHERE id = ?"),
                    &[to, &id],
                )
                .await?;
                acc.shares += 1;
            }
        }
    }

    // Tokens + oauth codes: re-point actor + the granting/creating columns.
    acc.api_tokens += exec_count(
        conn,
        "UPDATE api_tokens SET actor = ? WHERE actor = ?",
        &[to, from],
    )
    .await?;
    exec_count(
        conn,
        "UPDATE api_tokens SET created_by = ? WHERE created_by = ?",
        &[to, from],
    )
    .await?;
    exec_count(
        conn,
        "UPDATE api_tokens SET granted_by = ? WHERE granted_by = ?",
        &[to, from],
    )
    .await?;
    acc.oauth_codes += exec_count(
        conn,
        "UPDATE oauth_auth_codes SET ai_actor = ? WHERE ai_actor = ?",
        &[to, from],
    )
    .await?;
    exec_count(
        conn,
        "UPDATE oauth_auth_codes SET granted_by = ? WHERE granted_by = ?",
        &[to, from],
    )
    .await?;

    // Wire authorship.
    acc.entities += exec_count(
        conn,
        "UPDATE entities SET created_by = ? WHERE created_by = ?",
        &[to, from],
    )
    .await?;
    acc.entities += exec_count(
        conn,
        "UPDATE entities SET user_scope = ? WHERE user_scope = ?",
        &[to, from],
    )
    .await?;
    acc.wire += exec_count(
        conn,
        "UPDATE wire SET actor = ? WHERE actor = ?",
        &[to, from],
    )
    .await?;

    // Sources owned by / notifying from.
    acc.sources += exec_count(
        conn,
        "UPDATE sources SET owner = ? WHERE owner = ?",
        &[to, from],
    )
    .await?;
    exec_count(
        conn,
        "UPDATE sources SET notify = ? WHERE notify = ?",
        &[to, from],
    )
    .await?;

    // people.owner pointers (AIs the `from` human owned) now point at `to`.
    acc.people_owner += exec_count(
        conn,
        "UPDATE people SET owner = ? WHERE owner = ?",
        &[to, from],
    )
    .await?;

    // Profile/identity + login: the `to` card/account wins. Move the `from` card
    // only if `to` has none, else drop the `from` card; same for the user account.
    let to_has_profile = crate::pgq::query("SELECT 1 FROM profile WHERE actor = ?")
        .bind(to)
        .fetch_optional(&mut *conn)
        .await?
        .is_some();
    if to_has_profile {
        acc.profile += exec_count(conn, "DELETE FROM profile WHERE actor = ?", &[from]).await?;
    } else {
        acc.profile += exec_count(
            conn,
            "UPDATE profile SET actor = ? WHERE actor = ?",
            &[to, from],
        )
        .await?;
    }
    let to_has_user = crate::pgq::query("SELECT 1 FROM users WHERE actor = ?")
        .bind(to)
        .fetch_optional(&mut *conn)
        .await?
        .is_some();
    if to_has_user {
        // `to` already logs in; drop the `from` account + its sessions.
        let usr_rows: Vec<(String,)> = crate::pgq::query_as("SELECT id FROM users WHERE actor = ?")
            .bind(from)
            .fetch_all(&mut *conn)
            .await?;
        for (user_id,) in &usr_rows {
            exec_count(conn, "DELETE FROM sessions WHERE user_id = ?", &[user_id]).await?;
        }
        acc.users += exec_count(conn, "DELETE FROM users WHERE actor = ?", &[from]).await?;
    } else {
        acc.users += exec_count(
            conn,
            "UPDATE users SET actor = ? WHERE actor = ?",
            &[to, from],
        )
        .await?;
    }

    // Finally remove the folded-away people row.
    exec_count(conn, "DELETE FROM people WHERE slug = ?", &[from]).await?;

    Ok(acc)
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
