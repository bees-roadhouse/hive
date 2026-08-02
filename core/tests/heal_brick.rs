// Heal-brick regression pins (PLAN-v2.1 PR 4.1; TESTING-STRATEGY §7.3).
//
// These tests pin TODAY'S FAILING BEHAVIOR on purpose. A foreign device log
// dropped into a data dir (exactly the shape replication will land, and what
// a hand-copied restore produces today) is scanned by heal at the next open;
// when its records collide with existing derived state the fold errors, so
// `open_core` fails, `Store::new` errors, and the app boots to Boot::Failed —
// the store is BRICKED by data. Two collision classes brick it today:
//
//   1. an entity.create whose UNIQUE slug already exists (independent
//      emergence of the same topic name on two devices);
//   2. an entity.update whose target row does not exist (update-before-
//      create — device B edited something whose create lives in a segment
//      that has not arrived).
//
// PR 4.11 (FOLD_VERSION 4: deterministic slug-collision resolution, ghost
// creates) must flip these asserts CONSCIOUSLY. If one of these tests fails
// because open started SUCCEEDING, that is the 4.11 work landing, not a
// regression — rewrite the test to pin the new convergent outcome (loser
// slug suffixed / ghost row completed) instead of deleting it.
//
// Hermetic: tempdir + MemoryKeySource. The foreign log is written with the
// real LogWriter under the same master (a domain shares one master key), the
// store reopen is the real Store::new — nothing simulated. Reopen-style
// dirs follow the cutover.rs shape (the one place tests hold their own dir).

use std::path::Path;
use std::sync::Arc;

use ciborium::Value as Cb;
use hive_core::keys::MemoryKeySource;
use hive_core::oplog::{kind, LogWriter, Record};
use hive_core::store::Store;

fn keys() -> Arc<MemoryKeySource> {
    Arc::new(MemoryKeySource([7u8; 32]))
}

fn open(dir: &Path) -> anyhow::Result<Store> {
    Store::new(dir, keys(), Arc::new(hive_embed::HashEmbedder))
}

fn t(s: &str) -> Cb {
    Cb::Text(s.to_string())
}

fn map(entries: Vec<(&str, Cb)>) -> Cb {
    Cb::Map(entries.into_iter().map(|(k, v)| (t(k), v)).collect())
}

/// Land records under a FOREIGN device id in `<dir>/log/<device>/` — the
/// verbatim-segment shape a future sync ingests and heal already scans.
/// Seq assigned 1..n; the writer owns the prev chain.
fn append_foreign(dir: &Path, device: &str, payloads: Vec<(&'static str, Cb)>) {
    let ks = keys();
    let mut log = LogWriter::open(dir, device, ks.as_ref()).expect("open foreign log");
    let records: Vec<Record> = payloads
        .into_iter()
        .enumerate()
        .map(|(i, (k, payload))| {
            let seq = i as u64 + 1;
            Record::new(
                device,
                seq,
                seq,
                &format!("2026-07-20T10:00:{:02}.000Z", i % 60),
                "maggie",
                k,
                payload,
            )
        })
        .collect();
    log.append_batch(&records).expect("append foreign records");
}

#[tokio::test]
async fn pin_foreign_unique_slug_collision_bricks_store_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).unwrap();
    // Local emergence: topic "bees" holds the UNIQUE topics.slug.
    let topic = store.topics_ensure("Bees").await.unwrap();
    store.shutdown().await.unwrap();

    // A foreign device independently emerged the same topic name → same
    // slug, different id. Heal folds it at next open and the UNIQUE
    // constraint aborts the fold — there is no arbitration rule yet.
    append_foreign(
        dir.path(),
        "dev-foreign",
        vec![(
            kind::ENTITY_CREATE,
            map(vec![
                ("kind", t("topic")),
                ("id", t("top_foreign00001")),
                (
                    "fields",
                    map(vec![
                        ("name", t("Bees")),
                        ("slug", t(&topic.slug)),
                        ("created_at", t("2026-07-20T10:00:00.000Z")),
                    ]),
                ),
            ]),
        )],
    );

    let err = match open(dir.path()) {
        Ok(_) => panic!(
            "TODAY a foreign UNIQUE-slug collision must brick open — if this \
             started succeeding, PR 4.11's slug resolution landed: re-pin the \
             convergent outcome instead"
        ),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("UNIQUE constraint failed: topics.slug"),
        "the brick is the raw constraint surfacing through heal: {msg}"
    );
}

#[tokio::test]
async fn pin_foreign_update_before_create_bricks_store_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).unwrap();
    store.shutdown().await.unwrap();

    // A foreign edit to a task whose create has not arrived (out-of-order
    // segment delivery). The fold's strict update refuses missing rows, so
    // heal — and with it the whole open — fails.
    append_foreign(
        dir.path(),
        "dev-foreign",
        vec![(
            kind::ENTITY_UPDATE,
            map(vec![
                ("kind", t("task")),
                ("id", t("tsk_never_created")),
                ("fields", map(vec![("status", t("done"))])),
            ]),
        )],
    );

    let err = match open(dir.path()) {
        Ok(_) => panic!(
            "TODAY update-before-create must brick open — if this started \
             succeeding, PR 4.11's ghost-create tolerance landed: re-pin the \
             ghost-row outcome instead"
        ),
        Err(e) => e,
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("targets missing tasks row") && msg.contains("create first"),
        "the brick is the fold's strict-update refusal surfacing through heal: {msg}"
    );
}

/// The flip side, so the pins above can't be read as "foreign logs are
/// refused": a NON-colliding foreign log folds cleanly at open (this is
/// today's only restore path — drop files, heal — and PR 4.2's dir-copy
/// smoke formalizes it). Only the collisions brick.
#[tokio::test]
async fn non_colliding_foreign_log_heals_cleanly_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).unwrap();
    store.topics_ensure("Bees").await.unwrap();
    store.shutdown().await.unwrap();

    append_foreign(
        dir.path(),
        "dev-foreign",
        vec![(
            kind::ENTITY_CREATE,
            map(vec![
                ("kind", t("topic")),
                ("id", t("top_foreign00002")),
                (
                    "fields",
                    map(vec![
                        ("name", t("Wasps")),
                        ("slug", t("wasps")),
                        ("created_at", t("2026-07-20T10:00:00.000Z")),
                    ]),
                ),
            ]),
        )],
    );

    let store = open(dir.path()).expect("a clean foreign log must fold at open");
    let topics = store.topics_list().await.unwrap();
    let mut slugs: Vec<&str> = topics.iter().map(|t| t.slug.as_str()).collect();
    slugs.sort_unstable();
    assert_eq!(slugs, vec!["bees", "wasps"]);
    store.shutdown().await.unwrap();
}
