// The three cases from the old core/tests/store_smoke.rs that covered live
// code and were not migrated anywhere.
//
// Most of that file WAS migrated: the onboarding/journal/recall flow became
// api/tests/parity_smoke.rs::onboarding_gate_then_full_flow, the artifact
// cases have exact-name twins in api/tests/identity_artifacts.rs, and
// user_scope stamping is asserted in api/tests/conversations.rs. These three
// were not, and each drives a function no other test calls.
//
// The self-merge guard is the one worth having. Without it merge_plan
// reauthors everything onto the same slug and then tombstones it, so the actor
// deletes itself. api/tests/parity_smoke.rs only covers the dryRun preview of
// two DISTINCT actors, so neither the guard nor a committing merge was tested.

use hive_core::db;
use hive_core::store::Store;
use hive_shared::ActorKind;

async fn test_store() -> (Store, db::TestDb) {
    let test_db = db::test_pool().await;
    let store = Store::new(test_db.pool.clone());
    (store, test_db)
}

/// A merge (and its preview) must REFUSE `from == into`. A genuine two-actor
/// merge still runs.
#[tokio::test]
async fn actors_merge_rejects_self_merge() {
    let (store, _test_db) = test_store().await;
    store.people_ensure("nate", ActorKind::Human).await.unwrap();
    store.people_ensure("apis", ActorKind::Ai).await.unwrap();

    // Refused before any record is drafted, on both the live run and the
    // preview.
    let live = store.actors_merge("nate", "nate").await;
    let msg = format!("{:#}", live.expect_err("self-merge must error"));
    assert!(
        msg.contains("cannot merge an actor into itself"),
        "unexpected error: {msg}"
    );
    let prev = store.actors_merge_preview("nate", "nate").await;
    let msg = format!("{:#}", prev.expect_err("self-merge preview must error"));
    assert!(
        msg.contains("cannot merge an actor into itself"),
        "unexpected error: {msg}"
    );
    // Nothing was destroyed.
    assert!(store.people_get("nate").await.unwrap().is_some());

    // A real merge of two DISTINCT actors still works: apis folds into nate.
    let res = store.actors_merge("apis", "nate").await.unwrap();
    assert_eq!(res.from, "apis");
    assert_eq!(res.into, "nate");
    assert!(!res.dry_run, "a live merge is not a dry run");
    assert!(
        store.people_get("apis").await.unwrap().is_none(),
        "the folded-away actor's people row must be removed"
    );
    assert!(store.people_get("nate").await.unwrap().is_some());
}

/// The Identities pane "Claim as mine" flow: a `writer:` slug is materialised
/// into a real AI Person owned by the owner, and re-claiming is a no-op that
/// keeps the existing row rather than clobbering its owner.
#[tokio::test]
async fn claim_materialises_writer_slug_owned_by_owner() {
    let (store, _test_db) = test_store().await;
    assert!(
        store.people_get("ghostwriter").await.unwrap().is_none(),
        "precondition: the writer has no people row yet"
    );

    let owner = "nate";
    let claimed = store
        .people_upsert("ghostwriter", "Ghostwriter", ActorKind::Ai, Some(owner))
        .await
        .unwrap();
    assert_eq!(claimed.slug, "ghostwriter", "slug is preserved verbatim");
    assert_eq!(claimed.kind, ActorKind::Ai);
    assert_eq!(claimed.owner.as_deref(), Some(owner), "owned by the owner");

    let got = store.people_get("ghostwriter").await.unwrap().unwrap();
    assert_eq!(got.owner.as_deref(), Some(owner));

    // Re-claiming is idempotent: the existing row wins.
    let again = store
        .people_upsert(
            "ghostwriter",
            "Ghostwriter",
            ActorKind::Ai,
            Some("someone-else"),
        )
        .await
        .unwrap();
    assert_eq!(again.id, claimed.id, "same row returned, not a new one");
    assert_eq!(
        again.owner.as_deref(),
        Some(owner),
        "owner unchanged on a no-op upsert"
    );
}

/// Delivery, the read counters, and the self-notification skip. journal.rs
/// relies on that skip in a comment, and nothing asserted it.
#[tokio::test]
async fn inbox_roundtrip_and_self_notification_skip() {
    let (store, _test_db) = test_store().await;

    let none = store
        .inbox_add(
            "nate",
            "nate",
            hive_shared::InboxReason::Mention,
            hive_shared::EntityKind::Journal.as_str(),
            "jrnl_x",
            None,
            "self ping",
        )
        .await
        .unwrap();
    assert!(none.is_none(), "don't notify yourself");

    let item = store
        .inbox_add(
            "pia",
            "nate",
            hive_shared::InboxReason::Mention,
            hive_shared::EntityKind::Journal.as_str(),
            "jrnl_y",
            None,
            "a snippet",
        )
        .await
        .unwrap()
        .expect("delivered");
    assert_eq!(store.inbox_unread_count("pia").await.unwrap(), 1);

    // Mark by id; a second mark and a missing id both report zero rows.
    assert_eq!(store.inbox_mark_read(&item.id).await.unwrap(), 1);
    assert_eq!(store.inbox_mark_read(&item.id).await.unwrap(), 0);
    assert_eq!(store.inbox_mark_read("inb_missing").await.unwrap(), 0);
    assert_eq!(store.inbox_unread_count("pia").await.unwrap(), 0);

    // Mark-all clears the remaining unread.
    store
        .inbox_add(
            "pia",
            "nate",
            hive_shared::InboxReason::Mention,
            hive_shared::EntityKind::Journal.as_str(),
            "jrnl_z",
            None,
            "another",
        )
        .await
        .unwrap();
    assert_eq!(store.inbox_mark_all_read("pia").await.unwrap(), 1);
    assert_eq!(store.inbox_unread_count("pia").await.unwrap(), 0);
}
