// Embedding-corpus reaper — the safety net behind every synchronous delete
// path, and the ONLY mechanism for window aging. Tombstones/redactions drop
// search + embedding rows in the fold, and actor deletion emits its own
// records, but nothing "events" a message out of the newest-N window or a
// journal entry past the embed window — those are moving predicates, so a
// periodic sweep is structural, not a workaround. Deletions go through
// SqliteIndex::remove_embeddings so the in-memory ANN forgets the rows too.

use anyhow::Result;

use super::mail::MAIL_EMBED_ELIGIBLE_SQL;
use super::semantic::{JOURNAL_EMBED_WINDOW, ORIGIN_TABLE};
use super::{Core, Store};

impl Store {
    /// Delete embedding rows that no longer belong in the corpus. Returns
    /// `(label, rows_affected)` per sweep (zeros included — the caller decides
    /// what's worth reporting). `mail_embed_limit` is the per-account newest-N
    /// window (`HIVE_MAIL_EMBED_LIMIT`); it feeds `MAIL_EMBED_ELIGIBLE_SQL`,
    /// the SAME predicate the embed drain uses — the reaper must never decide
    /// eligibility on its own or the two fight forever (drain embeds, reaper
    /// deletes, every cycle).
    pub async fn embeddings_reap(&self, mail_embed_limit: i64) -> Result<Vec<(String, u64)>> {
        self.embeddings_reap_with(JOURNAL_EMBED_WINDOW, mail_embed_limit)
            .await
    }

    /// Inner form with the journal window as a parameter so tests can exercise
    /// the sweep without seeding 1000+ entries. Production always goes through
    /// `embeddings_reap`, which pins `JOURNAL_EMBED_WINDOW`.
    #[doc(hidden)]
    pub async fn embeddings_reap_with(
        &self,
        journal_window: i64,
        mail_embed_limit: i64,
    ) -> Result<Vec<(String, u64)>> {
        self.run(move |core| {
            let mut out: Vec<(String, u64)> = Vec::new();

            // Journal: one sweep covers both orphans (entry deleted, e.g. the
            // actor-removal cascade missed a row) and the newest-N window
            // (`embeddable_items` only embeds the newest JOURNAL_EMBED_WINDOW
            // entries; vectors beyond it used to leak forever). Same ORDER BY +
            // tiebreak as embeddable_items — the boundary must be deterministic.
            let n = reap_matching(
                core,
                "SELECT ref_kind, ref_id, count(*) FROM embeddings \
                 WHERE ref_kind = 'journal' AND ref_id NOT IN \
                 (SELECT id FROM journal ORDER BY created_at DESC, id DESC LIMIT ?1) \
                 GROUP BY ref_kind, ref_id",
                rusqlite::params![journal_window],
            )?;
            out.push(("journal".to_string(), n));

            // Anchored built-ins: vectors whose ref row no longer exists.
            for (kind, table) in ORIGIN_TABLE {
                let sql = format!(
                    "SELECT ref_kind, ref_id, count(*) FROM embeddings e \
                     WHERE e.ref_kind = '{kind}' AND NOT EXISTS \
                     (SELECT 1 FROM {table} t WHERE t.id = e.ref_id) \
                     GROUP BY ref_kind, ref_id"
                );
                let n = reap_matching(core, &sql, rusqlite::params![])?;
                out.push((kind.to_string(), n));
            }

            // Mail: everything not matching the shared eligibility predicate —
            // orphans (message row gone) fall out of the same NOT EXISTS. This is
            // the net behind mail sync's synchronous D6 deletes AND the only
            // window-aging path (new mail silently pushes old mail past newest-N).
            let sql = format!(
                "SELECT ref_kind, ref_id, count(*) FROM embeddings e \
                 WHERE e.ref_kind = 'mail' AND NOT EXISTS \
                 (SELECT 1 FROM mail_messages m WHERE m.id = e.ref_id AND {MAIL_EMBED_ELIGIBLE_SQL}) \
                 GROUP BY ref_kind, ref_id"
            );
            let n = reap_matching(core, &sql, rusqlite::params![mail_embed_limit])?;
            out.push(("mail".to_string(), n));

            // Custom kinds: ref_kind is an entity-type slug; reap rows with no
            // matching entities row. Every EntityKind built-in is excluded, not
            // just the kinds swept above — person/topic/project/phase never get
            // embeddings today, and if a future writer adds them, silently reaping
            // its rows as "custom orphans" is the wrong failure mode.
            let n = reap_matching(
                core,
                "SELECT ref_kind, ref_id, count(*) FROM embeddings e \
                 WHERE e.ref_kind NOT IN \
                 ('journal', 'task', 'decision', 'event', 'mail', 'person', 'topic', 'project', 'phase') \
                 AND NOT EXISTS \
                 (SELECT 1 FROM entities x JOIN entity_types ty ON ty.id = x.type_id \
                  WHERE x.id = e.ref_id AND ty.slug = e.ref_kind) \
                 GROUP BY ref_kind, ref_id",
                rusqlite::params![],
            )?;
            out.push(("custom".to_string(), n));

            Ok(out)
        })
        .await
    }
}

/// Select (ref_kind, ref_id, chunk count) groups with `sql`, then remove each
/// item's whole chunk set (rows + ann_keys + in-memory ANN). Returns rows
/// removed (chunks, matching the old DELETE's rows_affected).
fn reap_matching<P: rusqlite::Params>(core: &mut Core, sql: &str, params: P) -> Result<u64> {
    let doomed: Vec<(String, String, u64)> = {
        let mut stmt = core.conn().prepare(sql)?;
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut n = 0u64;
    for (kind, id, count) in doomed {
        core.index.remove_embeddings(&kind, &id)?;
        n += count;
    }
    Ok(n)
}
