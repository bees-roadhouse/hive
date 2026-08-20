// The journal write path is ONE transaction: the entry and everything it
// emerges (anchors, tasks, links, inbox items, shares, search rows, wire
// rows) commits or rolls back as a unit, and SSE fan-out happens only after
// the commit lands. Before this, each step took its own pool checkout, so a
// mid-append failure persisted an entry with its emergence half-applied.
//
// Error injection is a BEFORE INSERT trigger on inbox (installed owner-side —
// hive_app owns no tables) that rejects one sentinel recipient: a deterministic
// database failure AFTER the entry row and the emerged task were written, which
// is exactly the crash shape the transaction has to survive. The sentinel must
// be a KNOWN actor — parse_mentions only fans out @mentions of ACTORS entries.

use std::pin::Pin;
use std::time::Duration;

use futures_core::Stream;
use hive_core::db::{self, DEFAULT_ORG_ID};
use hive_core::store::Store;
use hive_core::{acting, ActingScope};
use hive_shared::{ActorKind, AnchorKind, NewAnchor, NewJournalEntry, WireEvent};

async fn test_store() -> (Store, db::TestDb) {
    let test_db = db::test_pool().await;
    let store = Store::new(test_db.pool.clone());
    (store, test_db)
}

async fn count(pool: &sqlx::PgPool, table: &str) -> i64 {
    hive_core::pgq::query_scalar::<i64>(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn next_event(stream: &mut Pin<Box<impl Stream<Item = WireEvent>>>) -> WireEvent {
    tokio::time::timeout(
        Duration::from_secs(5),
        std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)),
    )
    .await
    .expect("an event must arrive within 5s")
    .expect("the bus stays open")
}

async fn assert_silence(stream: &mut Pin<Box<impl Stream<Item = WireEvent>>>, what: &str) {
    let got = tokio::time::timeout(
        Duration::from_millis(300),
        std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)),
    )
    .await;
    assert!(got.is_err(), "no event may be published {what}");
}

#[tokio::test]
async fn append_rolls_back_everything_when_a_step_fails() {
    let (store, test_db) = test_store().await;
    let admin = db::test_admin_pool(&test_db.pool).await;

    // The injected failure: delivering an inbox item to 'cera' raises. With a
    // task anchor ahead of the mention loop, the failing step runs AFTER the
    // entry insert, the search index, and the emerged task.
    sqlx::raw_sql(
        "CREATE FUNCTION fail_on_cera() RETURNS trigger AS $$
         BEGIN
           IF NEW.recipient = 'cera' THEN
             RAISE EXCEPTION 'injected inbox failure';
           END IF;
           RETURN NEW;
         END;
         $$ LANGUAGE plpgsql;
         CREATE TRIGGER fail_on_cera BEFORE INSERT ON inbox
           FOR EACH ROW EXECUTE FUNCTION fail_on_cera();",
    )
    .execute(&admin)
    .await
    .expect("install error-injection trigger");

    let entry = NewJournalEntry {
        author: None,
        body: "Ship the atomic write path. @cera should see this.".to_string(),
        tags: None,
        anchors: Some(vec![NewAnchor {
            start: 0,
            end: 26,
            kind: AnchorKind::Task,
            fields: None,
        }]),
    };

    // Scoped so that any (buggy) fan-out WOULD be tagged for our subscriber —
    // the silence assertion below then proves nothing escaped the rollback.
    let mut stream = Box::pin(store.subscribe(DEFAULT_ORG_ID));
    let err = acting::scope(
        ActingScope::new(DEFAULT_ORG_ID, "nate", true),
        store.journal_append(entry, Some("nate"), None),
    )
    .await
    .expect_err("the injected inbox failure must abort the append");
    assert!(
        format!("{err:#}").contains("injected inbox failure"),
        "unexpected error: {err:#}"
    );

    // Nothing persists — not the entry, not anything it emerged, not the wire
    // events the emerged writes persisted before the failure.
    for table in [
        "journal", "search", "tasks", "anchors", "links", "inbox", "shares", "wire",
    ] {
        assert_eq!(
            count(&test_db.pool, table).await,
            0,
            "{table} must be empty after the rollback"
        );
    }
    assert_silence(&mut stream, "for a rolled-back append").await;
}

#[tokio::test]
async fn append_commits_emergence_then_publishes() {
    let (store, test_db) = test_store().await;
    store
        .people_upsert("nate", "Nate", ActorKind::Human, None)
        .await
        .unwrap();
    store
        .people_upsert("pia", "Pia", ActorKind::Ai, Some("nate"))
        .await
        .unwrap();

    let mut stream = Box::pin(store.subscribe(DEFAULT_ORG_ID));

    let entry = NewJournalEntry {
        author: None,
        body: "Ship the atomic write path with @pia. [task: update the docs]".to_string(),
        tags: None,
        anchors: Some(vec![NewAnchor {
            start: 0,
            end: 26,
            kind: AnchorKind::Task,
            fields: None,
        }]),
    };
    let view = acting::scope(
        ActingScope::new(DEFAULT_ORG_ID, "nate", true),
        store.journal_append(entry, Some("nate"), None),
    )
    .await
    .expect("append");

    // The commit is visible: entry, anchor-resolved task, bracket task,
    // mention inbox item, auto-share, search rows, wire rows.
    assert_eq!(view.entry.mentions, vec!["pia".to_string()]);
    assert_eq!(view.anchors.len(), 1);
    assert_eq!(
        view.anchors[0].entity["title"], "Ship the atomic write path",
        "the anchor resolved to its emerged task"
    );
    assert!(
        view.refs.iter().any(|r| r.name == "update the docs"),
        "the [task:] token resolves against the committed row: {:?}",
        view.refs
    );
    assert_eq!(count(&test_db.pool, "journal").await, 1);
    assert_eq!(count(&test_db.pool, "tasks").await, 2);
    assert_eq!(count(&test_db.pool, "anchors").await, 1);
    assert_eq!(count(&test_db.pool, "links").await, 2);
    assert_eq!(count(&test_db.pool, "inbox").await, 1);
    assert_eq!(count(&test_db.pool, "shares").await, 1);
    assert_eq!(count(&test_db.pool, "search").await, 3);
    assert_eq!(count(&test_db.pool, "wire").await, 5);

    let inbox = store.inbox_list("pia", false).await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].entry_id.as_deref(), Some(view.entry.id.as_str()));

    // The outbox publishes in persist order once the commit is durable:
    // both emerged tasks, the mention delivery, the share, then the entry.
    let mut kinds = Vec::new();
    for _ in 0..5 {
        kinds.push(next_event(&mut stream).await.kind);
    }
    assert_eq!(
        kinds,
        vec![
            "task.created",
            "task.created",
            "inbox.delivered",
            "share.created",
            "journal.created",
        ]
    );
    assert_silence(&mut stream, "twice — the outbox publishes once").await;
}
