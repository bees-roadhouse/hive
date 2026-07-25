// Unix-only, like the tier itself (the scenarios spawn processes, bind unix
// sockets, and copy dirs with unix permissions).
#![cfg(unix)]

// Smoke scenario 1 — dir-copy restore (docs/TESTING-STRATEGY.md §4;
// PLAN-v2.1 PR 4.2).
//
// Today's ONLY restore story is "copy the files back and open the store": the
// op log is the durable truth, the derived index is rebuildable, and heal
// folds whatever it finds at open. A restore CLI will formalise that later
// (Stage B); this pins the path it must keep working, at the level a restore
// actually happens — a whole data dir moving to a NEW location, with a second
// store already alive on the original.
//
// Two shapes, both of which a real backup produces:
//
//   1. the whole dir (log + blocks + device id + the derived index), the shape
//      `cp -a`, restic, or a ZFS snapshot rollback hands back;
//   2. log + blocks + device only, the shape you get when the index was
//      excluded (it is derived) or lost — restore then means REPLAY.
//
// `core/tests/cutover.rs` already pins the in-place version of (2) — delete
// index.db, reopen, dump is byte-identical. What is new here is the copy: a
// different path, a second live store, and writes continuing on the restored
// dir afterwards. The oracle is `canonical_dump()` byte-equality (the
// CONVERGE-ORACLE assert shape), plus a live FTS query so "the rows are there"
// cannot pass while the index is inert.
//
// Hermetic: one `test_domain()` tempdir, the domain's raw-hex master key, the
// deterministic hash embedder — no binaries, no network, no keychain.

use std::path::Path;

use hive_core::store::Store;
use hive_smoke_support::{require_smoke, test_domain, TestDomain};
use serde_json::json;

/// Enough state that a broken restore cannot pass by accident: prose that the
/// FTS indexes, emerged entities (topic/project/task/decision from anchors and
/// bracket tokens), a config row, and a second entry so the log spans more
/// than one record batch.
async fn seed(store: &Store) {
    store
        .journal_append(
            serde_json::from_value(json!({
                "body": "Durability review with @pia — [project: Restore] under \
                         [topic: Backups], [task: Verify a copied data dir opens].",
                "tags": ["restore"],
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
                "body": "We restore by copying files and replaying the log.",
                "anchors": [{"start": 3, "end": 40, "kind": "decision",
                             "fields": {"title": "Files plus replay is the restore path"}}],
            }))
            .unwrap(),
            Some("nate"),
            Some("nate"),
        )
        .await
        .unwrap();
    store.config_set("restore.smoke", "yes").await.unwrap();
}

/// Recursive copy — the "restore" half of a file-level backup. Deliberately
/// dumb (no rsync, no reflink): if a copied dir ever stops opening, the
/// failure must be about hive, not about a clever copy.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap_or_else(|e| panic!("mkdir {}: {e}", to.display()));
    for entry in
        std::fs::read_dir(from).unwrap_or_else(|e| panic!("read dir {}: {e}", from.display()))
    {
        let entry = entry.expect("dir entry");
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst)
                .unwrap_or_else(|e| panic!("copy {} → {}: {e}", src.display(), dst.display()));
        }
    }
}

/// Copy just the named top-level entries of a data dir (files or dirs).
fn copy_entries(domain: &TestDomain, from: &str, to: &str, entries: &[&str]) {
    let src_dir = domain.device_dir(from);
    let dst_dir = domain.device_dir(to);
    std::fs::create_dir_all(&dst_dir)
        .unwrap_or_else(|e| panic!("mkdir {}: {e}", dst_dir.display()));
    for name in entries {
        let src = src_dir.join(name);
        let dst = dst_dir.join(name);
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst)
                .unwrap_or_else(|e| panic!("copy {} → {}: {e}", src.display(), dst.display()));
        }
    }
}

/// (1) The whole-dir copy: same state, same device identity, still writable,
/// and the original is untouched by any of it.
#[tokio::test]
async fn a_copied_data_dir_restores_to_the_same_state() {
    require_smoke!();
    let domain = test_domain();

    let live = domain.open_device("alpha");
    seed(&live).await;
    let dump = live.canonical_dump().await.unwrap();
    let device = live.device().to_string();
    assert!(!dump.is_empty(), "the seed must produce derived state");
    // Cold backup: shut down first, so the copy is not racing the writer
    // thread (a hot copy of a WAL database is its own problem, and not one
    // the restore path claims to solve).
    live.shutdown().await.unwrap();

    copy_tree(&domain.device_dir("alpha"), &domain.device_dir("restored"));

    let restored = domain.open_device("restored");
    assert_eq!(
        restored.canonical_dump().await.unwrap(),
        dump,
        "a restored dir must serve exactly the state that was backed up"
    );
    assert_eq!(
        restored.device(),
        device,
        "the device id travels with the dir — a restore is the SAME device \
         resuming, not a new one (which is why restoring onto a box where the \
         original still runs forks the log: a restore-CLI concern, Stage B)"
    );
    assert!(
        !restored.search("durability", 10).await.unwrap().is_empty(),
        "the restored index must ANSWER, not just hold rows"
    );

    // And it is a live store, not a museum: the log writer picks the tail up
    // and keeps appending under the restored device id.
    restored
        .journal_append(
            serde_json::from_value(json!({"body": "First entry after the restore."})).unwrap(),
            Some("nate"),
            Some("nate"),
        )
        .await
        .unwrap();
    let after = restored.canonical_dump().await.unwrap();
    assert_ne!(after, dump, "the post-restore write must land");
    restored.shutdown().await.unwrap();

    // Reopening the restored dir proves the appended tail is on disk (and not
    // just in the index), and the original dir is exactly where we left it.
    let restored = domain.open_device("restored");
    assert_eq!(restored.canonical_dump().await.unwrap(), after);
    restored.shutdown().await.unwrap();

    let live = domain.open_device("alpha");
    assert_eq!(
        live.canonical_dump().await.unwrap(),
        dump,
        "copying a data dir must not touch the dir it was copied from"
    );
    live.shutdown().await.unwrap();
}

/// (2) The index-less copy: log + blocks + device only. Heal replays the whole
/// log at open and must rebuild the derived state byte-identically — the
/// "derived state is disposable" claim, tested where it matters (a fresh path
/// with an empty index, not an in-place delete).
#[tokio::test]
async fn a_copy_without_the_derived_index_rebuilds_by_replay() {
    require_smoke!();
    let domain = test_domain();

    let live = domain.open_device("alpha");
    seed(&live).await;
    let dump = live.canonical_dump().await.unwrap();
    live.shutdown().await.unwrap();

    // Everything durable, nothing derived: no index.db (nor its -wal/-shm),
    // no lock file.
    copy_entries(&domain, "alpha", "replayed", &["log", "blocks", "device"]);
    let replayed_dir = domain.device_dir("replayed");
    assert!(
        !replayed_dir.join("index.db").exists(),
        "the point of this scenario is an absent index"
    );

    let replayed = domain.open_device("replayed");
    assert_eq!(
        replayed.canonical_dump().await.unwrap(),
        dump,
        "full replay of a restored log must reproduce the derived state \
         byte-identically"
    );
    assert!(
        !replayed.search("durability", 10).await.unwrap().is_empty(),
        "the rebuilt FTS must serve queries"
    );
    replayed.shutdown().await.unwrap();
}
