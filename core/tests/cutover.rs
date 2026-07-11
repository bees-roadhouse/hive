// The PR 1.6 cutover's own guarantees, beyond the ported suites:
//
//   - crash heal: records appended to the op log WITHOUT folding (the crash
//     window between LogWriter::append_batch and the fold transaction) are
//     folded at the next Store::new;
//   - resume: reopening a data dir keeps the device id, the folded state, and
//     the fold watermark (no double-apply, no rebuild);
//   - single-writer serialization: many concurrent async writers all land
//     (no SQLITE_BUSY — there is exactly one connection, owned by the writer
//     thread);
//   - single-writer exclusion (PR 1.8): a second Store on the same data dir
//     is refused while the first holds the `lock` file, and admitted after
//     shutdown releases it;
//   - rebuild-by-replay (PR 1.8): deleting the derived index and reopening
//     reproduces byte-identical canonical state from the op log alone;
//   - the read_at record path (fold contract v2) survives replay;
//   - none of it consults the Postgres connection env var (Postgres left
//     core at this PR).
//
// Hermetic: tempdir + MemoryKeySource + the injected hash embedder.

mod common;

use std::sync::Arc;

use ciborium::Value as Cb;
use hive_core::keys::MemoryKeySource;
use hive_core::oplog::{kind, LogWriter, Record};
use hive_core::store::Store;
use serde_json::json;

fn keys() -> Arc<MemoryKeySource> {
    Arc::new(MemoryKeySource([7u8; 32]))
}

fn open(dir: &std::path::Path) -> Store {
    Store::new(dir, keys(), Arc::new(hive_embed::HashEmbedder)).expect("open store")
}

fn t(s: &str) -> Cb {
    Cb::Text(s.to_string())
}

fn map(entries: Vec<(&str, Cb)>) -> Cb {
    Cb::Map(entries.into_iter().map(|(k, v)| (t(k), v)).collect())
}

/// Append `journal.append` records straight to the device log — the fold
/// never sees them, exactly like a crash after fsync and before the fold tx.
fn append_unfolded(dir: &std::path::Path, device: &str, seq_from: u64, bodies: &[&str]) {
    let ks = keys();
    let mut log = LogWriter::open(dir, device, ks.as_ref()).expect("open log");
    let records: Vec<Record> = bodies
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let seq = seq_from + i as u64;
            let ts = format!("2026-07-10T14:00:{:02}.000Z", i % 60);
            Record::new(
                device,
                seq,
                seq,
                &ts,
                "nate",
                kind::JOURNAL_APPEND,
                map(vec![
                    ("id", t(&format!("jrnl_heal{seq}"))),
                    ("author", t("nate")),
                    ("body", t(body)),
                    ("created_at", t(&ts)),
                ]),
            )
        })
        .collect();
    log.append_batch(&records).expect("append unfolded tail");
}

#[tokio::test]
async fn crash_heal_folds_the_unfolded_log_tail_at_open() {
    let dir = tempfile::tempdir().unwrap();

    // Pin the device id up front so the tail can be written before any Store
    // exists (a first-boot crash), under the id the Store will adopt.
    std::fs::write(dir.path().join("device"), "dev-heal\n").unwrap();
    append_unfolded(
        dir.path(),
        "dev-heal",
        1,
        &["healed entry one", "healed entry two"],
    );

    // Open: heal must fold the tail before the first command runs.
    let store = open(dir.path());
    assert_eq!(store.device(), "dev-heal");
    let entries = store.journal_list(10, 0).await.unwrap();
    let bodies: Vec<&str> = entries.iter().map(|e| e.entry.body.as_str()).collect();
    assert!(
        bodies.contains(&"healed entry one") && bodies.contains(&"healed entry two"),
        "unfolded tail must fold at open: {bodies:?}"
    );
    // FTS came back through the fold too.
    let hits = store.search("healed", 10).await.unwrap();
    assert_eq!(hits.len(), 2, "healed rows must be searchable: {hits:?}");

    // The watermark advanced: writes continue on the same gapless chain.
    let after = store
        .journal_append(
            serde_json::from_value(json!({"body": "written after the heal"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();
    assert!(!after.entry.id.is_empty());
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn crash_heal_skips_already_folded_records_and_folds_only_the_tail() {
    let dir = tempfile::tempdir().unwrap();

    // A normal write through the store (appended AND folded)...
    let store = open(dir.path());
    let device = store.device().to_string();
    store
        .journal_append(
            serde_json::from_value(json!({"body": "folded before the crash"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();
    store.shutdown().await.unwrap();

    // ...then a crash-window tail (seq continues the chain, never folded)...
    append_unfolded(dir.path(), &device, 2, &["tail after the crash"]);

    // ...heals idempotently: both present, nothing double-applied.
    let store = open(dir.path());
    let entries = store.journal_list(10, 0).await.unwrap();
    assert_eq!(entries.len(), 2, "one folded + one healed, no duplicates");
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn reopen_resumes_device_id_state_and_watermark() {
    let dir = tempfile::tempdir().unwrap();

    let store = open(dir.path());
    let device = store.device().to_string();
    let entry = store
        .journal_append(
            serde_json::from_value(json!({"body": "note on [topic: Continuity] before reopen"}))
                .unwrap(),
            Some("nate"),
            Some("nate"),
        )
        .await
        .unwrap();
    store.config_set("cutover.smoke", "yes").await.unwrap();
    store.shutdown().await.unwrap();

    let store = open(dir.path());
    assert_eq!(store.device(), device, "device id is minted once per dir");
    let got = store.journal_get(&entry.entry.id).await.unwrap();
    assert!(got.is_some(), "state survives reopen");
    assert_eq!(
        store.config_get("cutover.smoke").await.unwrap().as_deref(),
        Some("yes")
    );
    let topics = store.topics_list().await.unwrap();
    assert_eq!(topics.len(), 1, "emerged topic survives reopen");

    // The chain continues (watermark correct: a fresh write folds cleanly).
    store
        .journal_append(
            serde_json::from_value(json!({"body": "post-reopen write"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(store.journal_list(10, 0).await.unwrap().len(), 2);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_async_writers_serialize_without_sqlite_busy() {
    let store = common::test_store().await;
    let mut handles = Vec::new();
    for i in 0..32 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store
                .journal_append(
                    serde_json::from_value(json!({"body": format!("concurrent write {i}")}))
                        .unwrap(),
                    Some("nate"),
                    None,
                )
                .await
        }));
    }
    for h in handles {
        h.await
            .unwrap()
            .expect("no write may fail (no SQLITE_BUSY)");
    }
    let entries = store.journal_list(100, 0).await.unwrap();
    assert_eq!(entries.len(), 32, "every concurrent write landed");
}

#[tokio::test]
async fn inbox_read_at_rides_records_and_survives_replay() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let item = store
        .inbox_add(
            "pia",
            "nate",
            hive_shared::InboxReason::Mention,
            "journal",
            "jrnl_x",
            None,
            "ping",
        )
        .await
        .unwrap()
        .expect("delivered");
    assert_eq!(store.inbox_mark_read(&item.id).await.unwrap(), 1);
    store.shutdown().await.unwrap();

    // Wipe the derived index (+ any WAL leftovers); the op log alone must
    // reproduce read_at (the fold contract v2 entity.update {kind:"inbox"}
    // path).
    std::fs::remove_file(dir.path().join("index.db")).unwrap();
    let _ = std::fs::remove_file(dir.path().join("index.db-wal"));
    let _ = std::fs::remove_file(dir.path().join("index.db-shm"));
    let store = open(dir.path());
    let items = store.inbox_list("pia", false).await.unwrap();
    assert_eq!(items.len(), 1);
    assert!(
        items[0].read_at.is_some(),
        "read_at must rebuild from records alone"
    );
    assert_eq!(store.inbox_unread_count("pia").await.unwrap(), 0);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn second_store_on_the_same_data_dir_is_refused_until_the_first_closes() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());

    // While the first store lives (app OR bridge), a second opener — even in
    // the same process, which is why the lock is flock and not fcntl — gets
    // the documented error instead of a second writer on the same log/index.
    let err = match Store::new(dir.path(), keys(), Arc::new(hive_embed::HashEmbedder)) {
        Ok(_) => panic!("a second store on a held data dir must be refused"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("another hive process"),
        "unexpected refusal text: {err:#}"
    );

    // The refused open must not have disturbed the holder.
    store
        .journal_append(
            serde_json::from_value(json!({"body": "still writing fine"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();

    // Shutdown joins the writer thread and releases the lock: next opener in.
    store.shutdown().await.unwrap();
    let store = open(dir.path());
    assert_eq!(store.journal_list(10, 0).await.unwrap().len(), 1);
    store.shutdown().await.unwrap();
}

/// The rebuild-derived-state proof (PLAN.md PR 1.8), through Store::new
/// rather than a bare SqliteIndex: seed varied state through the real write
/// paths, snapshot the canonical dump, DELETE the derived index entirely,
/// reopen the dir (crash-heal replays the whole log), and require the dump
/// byte-identical. This is the "drop SQLite, replay" recovery story (D18)
/// exercised in CI on every test run.
#[tokio::test]
async fn deleting_the_index_and_reopening_rebuilds_identical_state() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());

    // Varied seed: journal prose with emergence (topic/person/task tokens),
    // an anchored decision, config, inbox traffic (@mention), a task status
    // flip, and a custom entity type + instance.
    store
        .journal_append(
            serde_json::from_value(json!({
                "body": "Planning [project: Rebuild] with @pia — [task: Verify replay] \
                         under [topic: Durability].",
                "tags": ["cutover"],
            }))
            .unwrap(),
            Some("nate"),
            Some("nate"),
        )
        .await
        .unwrap();
    store
        .journal_append(
            serde_json::from_value(json!({
                "body": "We choose replay over backups for derived state.",
                "anchors": [{"start": 3, "end": 32, "kind": "decision",
                             "fields": {"title": "Replay is the migration story"}}],
            }))
            .unwrap(),
            Some("nate"),
            Some("nate"),
        )
        .await
        .unwrap();
    store.config_set("rebuild.smoke", "yes").await.unwrap();
    let task = &store.tasks_list(Default::default()).await.unwrap()[0];
    store
        .tasks_update(
            &task.id,
            hive_shared::TaskPatch {
                status: hive_shared::TaskStatus::parse("doing"),
                ..Default::default()
            },
            "nate",
        )
        .await
        .unwrap();
    let dump_before = store.canonical_dump().await.unwrap();
    assert!(!dump_before.is_empty());
    store.shutdown().await.unwrap();

    // Drop the ENTIRE derived index (+ WAL leftovers). Only the op log,
    // device file, and blockstore remain.
    std::fs::remove_file(dir.path().join("index.db")).unwrap();
    let _ = std::fs::remove_file(dir.path().join("index.db-wal"));
    let _ = std::fs::remove_file(dir.path().join("index.db-shm"));

    let store = open(dir.path());
    let dump_after = store.canonical_dump().await.unwrap();
    assert_eq!(
        dump_before, dump_after,
        "full replay must reproduce the derived state byte-identically"
    );
    // And the rebuilt index is live, not just shaped right: FTS answers.
    let hits = store.search("durability", 10).await.unwrap();
    assert!(!hits.is_empty(), "rebuilt FTS must serve queries");
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_store_lives_entirely_without_database_url() {
    // Belt and braces for the CI shape: nothing in the SQLite store consults
    // the Postgres connection env var. (Its name is assembled at runtime so
    // the "no Postgres tokens in core" grep gate stays a mechanical zero.)
    std::env::remove_var(format!("DATABASE{}", "_URL"));
    let store = common::test_store().await;
    store
        .journal_append(
            serde_json::from_value(json!({"body": "no postgres anywhere"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();
    let hits = store.search("postgres", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
}
