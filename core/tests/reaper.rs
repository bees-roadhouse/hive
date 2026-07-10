// Embedding-corpus reaper (store/maintenance.rs) — the in-src suite from the
// Postgres era, ported to the cutover store: the sweeps must delete exactly
// the ineligible vectors (orphans, past-window journal, ineligible mail,
// typeless custom slugs) and never fight the drain's shared eligibility
// predicate. Seeding rides the raw_sql + upsert_embedding_raw seams.

mod common;

use std::collections::{HashMap, HashSet};

use hive_core::store::Store;

const NOW: &str = "2026-07-09T00:00:00.000Z";

async fn exec(store: &Store, sql: &str, params: Vec<serde_json::Value>) {
    store.raw_sql(sql, params).await.expect("seed sql");
}

async fn seed_embedding(store: &Store, kind: &str, id: &str, chunk_idx: i64) {
    store
        .upsert_embedding_raw(kind, id, chunk_idx, "test-model", None, vec![0.0; 4], "h")
        .await
        .expect("seed embedding");
}

async fn embedding_ids(store: &Store, kind: &str) -> HashSet<String> {
    store
        .raw_sql(
            "SELECT DISTINCT ref_id FROM embeddings WHERE ref_kind = ?",
            vec![kind.into()],
        )
        .await
        .unwrap()
        .into_iter()
        .filter_map(|r| r[0].as_str().map(str::to_string))
        .collect()
}

fn counts(reaped: &[(String, u64)]) -> HashMap<&str, u64> {
    reaped.iter().map(|(k, n)| (k.as_str(), *n)).collect()
}

async fn seed_journal(store: &Store, id: &str, created_at: &str) {
    exec(
        store,
        "INSERT INTO journal (id, author, body, created_at) VALUES (?, 'pia', 'body', ?)",
        vec![id.into(), created_at.into()],
    )
    .await;
}

#[tokio::test]
async fn orphaned_vectors_reaped_live_ones_kept() {
    let store = common::test_store().await;

    // Live rows for every anchored kind + a custom type, each with a vector.
    seed_journal(&store, "j-live", NOW).await;
    exec(
        &store,
        "INSERT INTO tasks (id, title, created_at, updated_at) VALUES ('t-live', 'requeen', ?, ?)",
        vec![NOW.into(), NOW.into()],
    )
    .await;
    exec(
        &store,
        "INSERT INTO decisions (id, title, decision, created_at, updated_at) \
         VALUES ('d-live', 'split hive', 'yes', ?, ?)",
        vec![NOW.into(), NOW.into()],
    )
    .await;
    exec(
        &store,
        "INSERT INTO events (id, title, created_at) VALUES ('e-live', 'inspection', ?)",
        vec![NOW.into()],
    )
    .await;
    exec(
        &store,
        "INSERT INTO entity_types (id, slug, name, created_by, created_at, updated_at) \
         VALUES ('ty-widget', 'widget', 'Widget', 'pia', ?, ?)",
        vec![NOW.into(), NOW.into()],
    )
    .await;
    exec(
        &store,
        "INSERT INTO entities (id, type_id, title, created_by, created_at, updated_at) \
         VALUES ('w-live', 'ty-widget', 'smoker', 'pia', ?, ?)",
        vec![NOW.into(), NOW.into()],
    )
    .await;

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
    let store = common::test_store().await;
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
        exec(
            store,
            "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                acct.into(),
                owner.into(),
                format!("{owner}@example.test").into(),
                NOW.into(),
                NOW.into(),
            ],
        )
        .await;
    }
    for (id, acct, jmap, ingest) in [
        ("mb-a-inbox", "acct-a", "inbox", true),
        ("mb-a-arch", "acct-a", "archive", false),
        ("mb-b-inbox", "acct-b", "inbox", true),
    ] {
        exec(
            store,
            "INSERT INTO mail_mailboxes (id, account_id, jmap_id, name, ingest) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                id.into(),
                acct.into(),
                jmap.into(),
                jmap.into(),
                ingest.into(),
            ],
        )
        .await;
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
        exec(
            store,
            "INSERT INTO mail_messages (id, account_id, user_scope, jmap_id, jmap_thread_id, \
             received_at, mailbox_ids_json, keywords_json, deleted_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                id.into(),
                acct.into(),
                owner.into(),
                format!("jmap-{id}").into(),
                format!("t-{id}").into(),
                received.into(),
                boxes.into(),
                kw.into(),
                if dead {
                    NOW.into()
                } else {
                    serde_json::Value::Null
                },
                NOW.into(),
                NOW.into(),
            ],
        )
        .await;
        seed_embedding(store, "mail", id, 0).await;
    }
    // Orphan vector: message row gone entirely.
    seed_embedding(store, "mail", "m-ghost", 0).await;
}

#[tokio::test]
async fn mail_ineligible_vectors_reaped_eligible_kept() {
    let store = common::test_store().await;
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
    let store = common::test_store().await;
    seed_mail_scenario(&store).await;

    let sql = format!(
        "SELECT m.id FROM mail_messages m WHERE {}",
        hive_core::store::mail::MAIL_EMBED_ELIGIBLE_SQL
    );
    let eligible: HashSet<String> = store
        .raw_sql(&sql, vec![2.into()])
        .await
        .unwrap()
        .into_iter()
        .filter_map(|r| r[0].as_str().map(str::to_string))
        .collect();
    assert!(!eligible.is_empty(), "scenario must have eligible rows");

    store.embeddings_reap(2).await.unwrap();
    assert_eq!(embedding_ids(&store, "mail").await, eligible);
}

#[tokio::test]
async fn mail_window_zero_or_negative_means_gate_closed() {
    let store = common::test_store().await;
    seed_mail_scenario(&store).await;
    let reaped = store.embeddings_reap(0).await.unwrap();
    assert_eq!(
        counts(&reaped)["mail"],
        8,
        "empty window reaps every mail vector"
    );
    assert!(embedding_ids(&store, "mail").await.is_empty());
}
