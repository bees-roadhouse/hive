// Store-level tests for the fold-safe calendar slice: events_update round-trips
// a changed `at`/title over the EXISTING event columns, and events_delete
// tombstones the row (gone from events_list, gone from search) AND survives a
// full drop-the-index replay — proving the tombstone + link.remove records are
// durable, not just a live-index mutation. No schema/fold change is exercised.

mod common;

use std::sync::Arc;

use hive_core::keys::MemoryKeySource;
use hive_core::store::events::EventCreate;
use hive_core::store::Store;
use hive_shared::EventPatch;

fn keys() -> Arc<MemoryKeySource> {
    Arc::new(MemoryKeySource([7u8; 32]))
}

fn open(dir: &std::path::Path) -> Store {
    Store::new(dir, keys(), Arc::new(hive_embed::HashEmbedder)).expect("open store")
}

fn seed() -> EventCreate {
    EventCreate {
        title: "Kickoff".into(),
        body: "the first sync".into(),
        at: Some("2026-07-15".into()),
        tags: vec!["planning".into()],
        assignees: vec!["nate".into()],
        origin_entry_id: None,
        anchor_text: None,
    }
}

#[tokio::test]
async fn events_update_round_trips_at_and_title() {
    let store = common::test_store().await;
    let e = store.events_create(seed(), "nate").await.unwrap();

    // Change the title and reschedule (set a new `at`), leave the rest.
    let patch = EventPatch {
        title: Some("Kickoff (moved)".into()),
        at: Some(Some("2026-08-01T09:30:00.000Z".into())),
        ..Default::default()
    };
    let updated = store
        .events_update(&e.id, patch, "nate")
        .await
        .unwrap()
        .expect("event exists");
    assert_eq!(updated.title, "Kickoff (moved)");
    assert_eq!(updated.at.as_deref(), Some("2026-08-01T09:30:00.000Z"));
    // Untouched fields survive.
    assert_eq!(updated.body, "the first sync");
    assert_eq!(updated.assignees, vec!["nate".to_string()]);

    // The canonical row (re-read from the index the fold wrote) agrees.
    let got = store.events_get(&e.id).await.unwrap().expect("row");
    assert_eq!(got.title, "Kickoff (moved)");
    assert_eq!(got.at.as_deref(), Some("2026-08-01T09:30:00.000Z"));
    assert_eq!(got.tags, vec!["planning".to_string()]);

    // Clearing `at` (double Option null) drops it to unscheduled.
    let cleared = store
        .events_update(
            &e.id,
            EventPatch {
                at: Some(None),
                ..Default::default()
            },
            "nate",
        )
        .await
        .unwrap()
        .expect("event exists");
    assert_eq!(cleared.at, None, "explicit null clears `at`");
    let got = store.events_get(&e.id).await.unwrap().expect("row");
    assert_eq!(got.at, None, "cleared `at` persists to the row");
    assert_eq!(got.title, "Kickoff (moved)", "clearing at left title alone");

    // Updating a missing event is a clean None (no panic, no record).
    assert!(store
        .events_update("evt_missing", EventPatch::default(), "nate")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn events_delete_tombstones_and_survives_replay() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());

    let keep = store
        .events_create(
            EventCreate {
                title: "Retro".into(),
                ..seed()
            },
            "nate",
        )
        .await
        .unwrap();
    let doomed = store
        .events_create(
            EventCreate {
                title: "Cancelled offsite".into(),
                at: Some("2026-09-02".into()),
                ..seed()
            },
            "nate",
        )
        .await
        .unwrap();

    // Delete drops it from the list and from search immediately.
    store
        .events_delete(&doomed.id, "nate")
        .await
        .unwrap()
        .expect("deleted");
    assert!(store.events_get(&doomed.id).await.unwrap().is_none());
    let ids: Vec<String> = store
        .events_list()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert!(!ids.contains(&doomed.id), "tombstoned event left the list");
    assert!(ids.contains(&keep.id), "the other event stays");
    let hits = store.search("offsite", 25).await.unwrap();
    assert!(
        hits.iter().all(|h| h.id != doomed.id),
        "tombstoned event left the FTS index"
    );

    // Deleting an already-gone id is a clean None.
    assert!(store
        .events_delete(&doomed.id, "nate")
        .await
        .unwrap()
        .is_none());

    store.shutdown().await.unwrap();

    // Drop the ENTIRE derived index and reopen: the whole op log replays,
    // and the tombstone must still win (the deleted event does NOT resurrect).
    std::fs::remove_file(dir.path().join("index.db")).unwrap();
    let _ = std::fs::remove_file(dir.path().join("index.db-wal"));
    let _ = std::fs::remove_file(dir.path().join("index.db-shm"));

    let store = open(dir.path());
    assert!(
        store.events_get(&doomed.id).await.unwrap().is_none(),
        "tombstone survives a full replay — no resurrection"
    );
    let ids: Vec<String> = store
        .events_list()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert!(!ids.contains(&doomed.id));
    assert!(ids.contains(&keep.id), "the surviving event replays back");
    store.shutdown().await.unwrap();
}
