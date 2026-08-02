// First tests on the broadcast bus (PLAN-v2.1 PR 4.1). `Store::subscribe`
// has zero production subscribers today, but two planned surfaces sit on
// this channel — replication fold-applied events (PR 4.10) and the web
// head's view invalidation (Phase 5) — so the commit→event contract gets
// pinned before anything depends on it: a subscriber present at write time
// receives each write's event; a late subscriber gets history from the ring
// (`wire_log`), not the channel; and no subscriber state may ever fail the
// mutation path. Hermetic: tempdir + MemoryKeySource + hash embedder via
// test_store() (ONE-SEAM).

mod common;

use hive_core::store::tasks::TaskCreate;
use serde_json::json;
use tokio::sync::broadcast::error::TryRecvError;

#[tokio::test]
async fn subscriber_receives_the_event_each_write_emits() {
    let store = common::test_store().await;
    let mut rx = store.subscribe();

    // journal_append → journal.created, carrying the entry id and author.
    let view = store
        .journal_append(
            serde_json::from_value(json!({"body": "bus test entry"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();
    let ev = rx.recv().await.expect("subscriber must receive the event");
    assert_eq!(ev.kind, "journal.created");
    assert_eq!(ev.actor, "nate");
    assert_eq!(
        ev.payload.get("id").and_then(|v| v.as_str()),
        Some(view.entry.id.as_str())
    );

    // tasks_create → task.created; events arrive in write order.
    let task = store
        .tasks_create(
            TaskCreate {
                title: "wire the bus".into(),
                ..Default::default()
            },
            "nate",
        )
        .await
        .unwrap();
    let ev = rx.recv().await.expect("second event");
    assert_eq!(ev.kind, "task.created");
    assert_eq!(
        ev.payload.get("id").and_then(|v| v.as_str()),
        Some(task.id.as_str())
    );

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn every_live_subscriber_gets_its_own_copy() {
    let store = common::test_store().await;
    let mut rx_a = store.subscribe();
    let mut rx_b = store.subscribe();

    store
        .journal_append(
            serde_json::from_value(json!({"body": "fan-out"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();

    let (a, b) = (rx_a.recv().await.unwrap(), rx_b.recv().await.unwrap());
    assert_eq!(a.id, b.id, "broadcast: one event, every subscriber");
    assert_eq!(a.kind, "journal.created");

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn late_subscriber_misses_the_channel_but_the_ring_remembers() {
    let store = common::test_store().await;

    store
        .journal_append(
            serde_json::from_value(json!({"body": "before anyone listened"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();

    // The broadcast channel is fire-and-forget: subscribing after the write
    // yields nothing…
    let mut rx = store.subscribe();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

    // …but the in-memory ring (session-scoped by design — the wire table
    // died with Postgres) still serves it, newest first.
    let ring = store.wire_log(10).await.unwrap();
    assert_eq!(ring.len(), 1);
    assert_eq!(ring[0].kind, "journal.created");

    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn absent_and_dropped_subscribers_never_fail_the_write_path() {
    let store = common::test_store().await;

    // Zero subscribers: writes succeed (today's production reality).
    store
        .journal_append(
            serde_json::from_value(json!({"body": "no listeners"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();

    // A subscriber that hung up must not fail later writes either.
    let rx = store.subscribe();
    drop(rx);
    store
        .journal_append(
            serde_json::from_value(json!({"body": "listener hung up"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();

    assert_eq!(store.wire_log(10).await.unwrap().len(), 2);
    store.shutdown().await.unwrap();
}
