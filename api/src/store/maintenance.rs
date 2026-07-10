// Embedding-corpus reaper — the safety net behind every synchronous delete
// path, and the ONLY mechanism for window aging. hive-mail drops search +
// embedding rows in the same transaction as tombstones/moves (D6), and actor
// deletion cascades its own rows, but nothing "events" a message out of the
// newest-N window or a journal entry past the embed window — those are moving
// predicates, so a periodic sweep is structural, not a workaround. The worker
// calls `embeddings_reap` from `maintain()` every 20th cycle.

use anyhow::Result;

use super::mail::MAIL_EMBED_ELIGIBLE_SQL;
use super::semantic::{JOURNAL_EMBED_WINDOW, ORIGIN_TABLE};
use super::Store;

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
    async fn embeddings_reap_with(
        &self,
        journal_window: i64,
        mail_embed_limit: i64,
    ) -> Result<Vec<(String, u64)>> {
        let mut out: Vec<(String, u64)> = Vec::new();

        // Journal: one sweep covers both orphans (entry deleted, e.g. the
        // actor-removal cascade missed a row) and the newest-N window
        // (`embeddable_items` only embeds the newest JOURNAL_EMBED_WINDOW
        // entries; vectors beyond it used to leak forever). Same ORDER BY +
        // tiebreak as embeddable_items — the boundary must be deterministic.
        let n = crate::pgq::query(
            "DELETE FROM embeddings WHERE ref_kind = 'journal' AND ref_id NOT IN \
             (SELECT id FROM journal ORDER BY created_at DESC, id DESC LIMIT ?)",
        )
        .bind(journal_window)
        .execute(self.db())
        .await?
        .rows_affected();
        out.push(("journal".to_string(), n));

        // Anchored built-ins: vectors whose ref row no longer exists.
        for (kind, table) in ORIGIN_TABLE {
            let sql = format!(
                "DELETE FROM embeddings e WHERE e.ref_kind = '{kind}' AND NOT EXISTS \
                 (SELECT 1 FROM {table} t WHERE t.id = e.ref_id)"
            );
            let n = crate::pgq::query(&sql)
                .execute(self.db())
                .await?
                .rows_affected();
            out.push((kind.to_string(), n));
        }

        // Mail: everything not matching the shared eligibility predicate —
        // orphans (message row gone) fall out of the same NOT EXISTS. This is
        // the net behind hive-mail's synchronous D6 deletes AND the only
        // window-aging path (new mail silently pushes old mail past newest-N).
        let sql = format!(
            "DELETE FROM embeddings e WHERE e.ref_kind = 'mail' AND NOT EXISTS \
             (SELECT 1 FROM mail_messages m WHERE m.id = e.ref_id AND {MAIL_EMBED_ELIGIBLE_SQL})"
        );
        let n = crate::pgq::query(&sql)
            .bind(mail_embed_limit)
            .execute(self.db())
            .await?
            .rows_affected();
        out.push(("mail".to_string(), n));

        // Custom kinds: ref_kind is an entity-type slug; reap rows with no
        // matching entities row. Every EntityKind built-in is excluded, not
        // just the kinds swept above — person/topic/project/phase never get
        // embeddings today, and if a future writer adds them, silently reaping
        // its rows as "custom orphans" is the wrong failure mode.
        let n = crate::pgq::query(
            "DELETE FROM embeddings e WHERE e.ref_kind NOT IN \
             ('journal', 'task', 'decision', 'event', 'mail', 'person', 'topic', 'project', 'phase') \
             AND NOT EXISTS \
             (SELECT 1 FROM entities x JOIN entity_types ty ON ty.id = x.type_id \
              WHERE x.id = e.ref_id AND ty.slug = e.ref_kind)",
        )
        .execute(self.db())
        .await?
        .rows_affected();
        out.push(("custom".to_string(), n));

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::db;

    const NOW: &str = "2026-07-09T00:00:00.000Z";

    async fn seed_embedding(store: &Store, kind: &str, id: &str, chunk_idx: i32) {
        crate::pgq::query(
            "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec, hash, created_at) \
             VALUES (?, ?, ?, 'test-model', 4, NULL, ?, 'h', ?)",
        )
        .bind(kind)
        .bind(id)
        .bind(chunk_idx)
        .bind(vec![0u8; 16])
        .bind(NOW)
        .execute(store.db())
        .await
        .unwrap();
    }

    async fn embedding_ids(store: &Store, kind: &str) -> HashSet<String> {
        crate::pgq::query_scalar::<String>(
            "SELECT DISTINCT ref_id FROM embeddings WHERE ref_kind = ?",
        )
        .bind(kind)
        .fetch_all(store.db())
        .await
        .unwrap()
        .into_iter()
        .collect()
    }

    fn counts(reaped: &[(String, u64)]) -> HashMap<&str, u64> {
        reaped.iter().map(|(k, n)| (k.as_str(), *n)).collect()
    }

    async fn seed_journal(store: &Store, id: &str, created_at: &str) {
        crate::pgq::query(
            "INSERT INTO journal (id, author, body, created_at) VALUES (?, 'pia', 'body', ?)",
        )
        .bind(id)
        .bind(created_at)
        .execute(store.db())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn orphaned_vectors_reaped_live_ones_kept() {
        let store = Store::new(db::test_pool().await);

        // Live rows for every anchored kind + a custom type, each with a vector.
        seed_journal(&store, "j-live", NOW).await;
        crate::pgq::query(
            "INSERT INTO tasks (id, title, created_at, updated_at) VALUES ('t-live', 'requeen', ?, ?)",
        )
        .bind(NOW)
        .bind(NOW)
        .execute(store.db())
        .await
        .unwrap();
        crate::pgq::query(
            "INSERT INTO decisions (id, title, decision, created_at, updated_at) \
             VALUES ('d-live', 'split hive', 'yes', ?, ?)",
        )
        .bind(NOW)
        .bind(NOW)
        .execute(store.db())
        .await
        .unwrap();
        crate::pgq::query(
            "INSERT INTO events (id, title, created_at) VALUES ('e-live', 'inspection', ?)",
        )
        .bind(NOW)
        .execute(store.db())
        .await
        .unwrap();
        crate::pgq::query(
            "INSERT INTO entity_types (id, slug, name, created_by, created_at, updated_at) \
             VALUES ('ty-widget', 'widget', 'Widget', 'pia', ?, ?)",
        )
        .bind(NOW)
        .bind(NOW)
        .execute(store.db())
        .await
        .unwrap();
        crate::pgq::query(
            "INSERT INTO entities (id, type_id, title, created_by, created_at, updated_at) \
             VALUES ('w-live', 'ty-widget', 'smoker', 'pia', ?, ?)",
        )
        .bind(NOW)
        .bind(NOW)
        .execute(store.db())
        .await
        .unwrap();

        for (kind, id) in [
            ("journal", "j-live"),
            ("task", "t-live"),
            ("decision", "d-live"),
            ("event", "e-live"),
            ("widget", "w-live"),
        ] {
            seed_embedding(&store, kind, id, 0).await;
        }
        // Orphans: ref row gone (or never existed). The task orphan carries a
        // second chunk — the whole chunk set must go.
        seed_embedding(&store, "journal", "j-ghost", 0).await;
        seed_embedding(&store, "task", "t-ghost", 0).await;
        seed_embedding(&store, "task", "t-ghost", 1).await;
        seed_embedding(&store, "decision", "d-ghost", 0).await;
        seed_embedding(&store, "event", "e-ghost", 0).await;
        // Custom orphans: known slug without its entity row, and a slug with
        // no entity type at all (e.g. the type was deleted).
        seed_embedding(&store, "widget", "w-ghost", 0).await;
        seed_embedding(&store, "gadget", "g-ghost", 0).await;
        // A built-in that never gets embeddings: spared by the custom sweep
        // even with no backing row (fail-safe for future writers).
        seed_embedding(&store, "person", "p-1", 0).await;

        let reaped = store.embeddings_reap(5000).await.unwrap();
        let by = counts(&reaped);
        assert_eq!(by["journal"], 1);
        assert_eq!(by["task"], 2, "both chunks of the orphaned task go");
        assert_eq!(by["decision"], 1);
        assert_eq!(by["event"], 1);
        assert_eq!(by["mail"], 0);
        assert_eq!(by["custom"], 2, "orphaned slug + typeless slug both go");

        assert_eq!(
            embedding_ids(&store, "journal").await,
            ["j-live".to_string()].into()
        );
        assert_eq!(
            embedding_ids(&store, "task").await,
            ["t-live".to_string()].into()
        );
        assert_eq!(
            embedding_ids(&store, "decision").await,
            ["d-live".to_string()].into()
        );
        assert_eq!(
            embedding_ids(&store, "event").await,
            ["e-live".to_string()].into()
        );
        assert_eq!(
            embedding_ids(&store, "widget").await,
            ["w-live".to_string()].into()
        );
        assert_eq!(
            embedding_ids(&store, "person").await,
            ["p-1".to_string()].into(),
            "non-embedded built-ins are never swept as custom orphans"
        );

        // Idempotent: a second pass finds nothing.
        let again = store.embeddings_reap(5000).await.unwrap();
        assert!(again.iter().all(|(_, n)| *n == 0), "{again:?}");
    }

    #[tokio::test]
    async fn journal_window_sweep_reaps_beyond_newest_n() {
        let store = Store::new(db::test_pool().await);
        // Five entries, j1 oldest … j5 newest; j1 chunked into two rows.
        for i in 1..=5 {
            seed_journal(
                &store,
                &format!("j{i}"),
                &format!("2026-07-0{i}T00:00:00.000Z"),
            )
            .await;
            seed_embedding(&store, "journal", &format!("j{i}"), 0).await;
        }
        seed_embedding(&store, "journal", "j1", 1).await;

        // Window of 3 (production pins JOURNAL_EMBED_WINDOW; parameterized
        // here so the test doesn't need 1001 rows).
        let reaped = store.embeddings_reap_with(3, 5000).await.unwrap();
        assert_eq!(counts(&reaped)["journal"], 3, "j1 (2 chunks) + j2");
        assert_eq!(
            embedding_ids(&store, "journal").await,
            ["j3".to_string(), "j4".to_string(), "j5".to_string()].into(),
            "newest-3 window kept"
        );
    }

    /// Accounts, mailboxes, messages, and one vector per message (plus one
    /// orphan vector). With a window of 2: alice's a-new1/a-new2 and bob's
    /// b-one are eligible; everything else must reap — and crucially the
    /// tombstoned/junk/out-of-ingest rows are NEWER than the eligible ones,
    /// proving ineligible mail never consumes a window slot.
    async fn seed_mail_scenario(store: &Store) {
        for (acct, owner) in [("acct-a", "alice"), ("acct-b", "bob")] {
            crate::pgq::query(
                "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(acct)
            .bind(owner)
            .bind(format!("{owner}@example.test"))
            .bind(NOW)
            .bind(NOW)
            .execute(store.db())
            .await
            .unwrap();
        }
        for (id, acct, jmap, ingest) in [
            ("mb-a-inbox", "acct-a", "inbox", true),
            ("mb-a-arch", "acct-a", "archive", false),
            ("mb-b-inbox", "acct-b", "inbox", true),
        ] {
            crate::pgq::query(
                "INSERT INTO mail_mailboxes (id, account_id, jmap_id, name, ingest) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(acct)
            .bind(jmap)
            .bind(jmap)
            .bind(ingest)
            .execute(store.db())
            .await
            .unwrap();
        }
        struct Msg(
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            &'static str,
            bool,
        );
        for Msg(id, acct, owner, received, boxes_kw, dead) in [
            // Eligible under window 2 (newest two otherwise-eligible of acct-a):
            Msg(
                "a-new1",
                "acct-a",
                "alice",
                "2026-07-09T12:05:00.000Z",
                r#"["inbox"]|{}"#,
                false,
            ),
            Msg(
                "a-new2",
                "acct-a",
                "alice",
                "2026-07-09T12:04:00.000Z",
                r#"["inbox"]|{}"#,
                false,
            ),
            // Outside the newest-2 window → reap.
            Msg(
                "a-old",
                "acct-a",
                "alice",
                "2026-07-09T12:03:00.000Z",
                r#"["inbox"]|{}"#,
                false,
            ),
            // Tombstoned (newest of all — must not hold a window slot) → reap.
            Msg(
                "a-tomb",
                "acct-a",
                "alice",
                "2026-07-09T12:10:00.000Z",
                r#"["inbox"]|{}"#,
                true,
            ),
            // Junk → reap, no slot.
            Msg(
                "a-junk",
                "acct-a",
                "alice",
                "2026-07-09T12:09:00.000Z",
                r#"["inbox"]|{"$junk":true}"#,
                false,
            ),
            // Moved out of every ingest-enabled mailbox → reap, no slot.
            Msg(
                "a-out",
                "acct-a",
                "alice",
                "2026-07-09T12:08:00.000Z",
                r#"["archive"]|{}"#,
                false,
            ),
            // Other account: eligible in ITS OWN window despite being oldest.
            Msg(
                "b-one",
                "acct-b",
                "bob",
                "2026-07-09T11:00:00.000Z",
                r#"["inbox"]|{}"#,
                false,
            ),
        ] {
            let (boxes, kw) = boxes_kw.split_once('|').unwrap();
            crate::pgq::query(
                "INSERT INTO mail_messages (id, account_id, user_scope, jmap_id, jmap_thread_id, \
                 received_at, mailbox_ids_json, keywords_json, deleted_at, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(acct)
            .bind(owner)
            .bind(format!("jmap-{id}"))
            .bind(format!("t-{id}"))
            .bind(received)
            .bind(boxes)
            .bind(kw)
            .bind(if dead { Some(NOW) } else { None })
            .bind(NOW)
            .bind(NOW)
            .execute(store.db())
            .await
            .unwrap();
            seed_embedding(store, "mail", id, 0).await;
        }
        // Orphan vector: message row gone entirely.
        seed_embedding(store, "mail", "m-ghost", 0).await;
    }

    #[tokio::test]
    async fn mail_ineligible_vectors_reaped_eligible_kept() {
        let store = Store::new(db::test_pool().await);
        seed_mail_scenario(&store).await;

        let reaped = store.embeddings_reap(2).await.unwrap();
        // a-old + a-tomb + a-junk + a-out + m-ghost.
        assert_eq!(counts(&reaped)["mail"], 5);
        assert_eq!(
            embedding_ids(&store, "mail").await,
            [
                "a-new1".to_string(),
                "a-new2".to_string(),
                "b-one".to_string()
            ]
            .into(),
            "window is per account; ineligible rows hold no slots"
        );
    }

    /// The drain/reaper contract: after a reap, the surviving mail vectors are
    /// EXACTLY the rows `MAIL_EMBED_ELIGIBLE_SQL` selects — a row satisfying
    /// the shared predicate is never reaped, and nothing outside it survives.
    /// If this ever fails, the drain and the reaper have diverged and will
    /// fight forever (embed → reap → re-embed every cycle).
    #[tokio::test]
    async fn rows_matching_the_shared_eligibility_predicate_are_never_reaped() {
        let store = Store::new(db::test_pool().await);
        seed_mail_scenario(&store).await;

        let sql = format!("SELECT m.id FROM mail_messages m WHERE {MAIL_EMBED_ELIGIBLE_SQL}");
        let eligible: HashSet<String> = crate::pgq::query_scalar::<String>(&sql)
            .bind(2_i64)
            .fetch_all(store.db())
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert!(!eligible.is_empty(), "scenario must have eligible rows");

        store.embeddings_reap(2).await.unwrap();
        assert_eq!(embedding_ids(&store, "mail").await, eligible);
    }

    #[tokio::test]
    async fn mail_window_zero_or_negative_means_gate_closed() {
        let store = Store::new(db::test_pool().await);
        seed_mail_scenario(&store).await;
        let reaped = store.embeddings_reap(0).await.unwrap();
        assert_eq!(
            counts(&reaped)["mail"],
            8,
            "empty window reaps every mail vector"
        );
        assert!(embedding_ids(&store, "mail").await.is_empty());
    }
}
