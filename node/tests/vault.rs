// Write-once kill-tests for the blind vault (PLAN-v2.1 PR 4.6; the
// "SegmentVault write-once" row of the ADVERSARIAL-SMOKE ledger,
// docs/TESTING-STRATEGY.md §4.4).
//
// The claim under test is the one that makes a blind node trustworthy without
// a key: **a node cannot be made to lose or change a byte it already holds,
// and it notices when someone tries.** Concretely —
//
//   * a differing re-upload of an existing (device, start_seq) is an
//     integrity ALARM and a refusal, never an overwrite — sealed or not;
//   * a segment may only extend at its end, with its byte prefix intact; a
//     write past the end (a hole) is refused;
//   * a sealed segment this node holds only part of can still be COMPLETED,
//     because sealing is a fact about the source, not about what arrived here
//     — refusing that would break the protocol's own resume path;
//   * an identical re-upload — the normal consequence of a lost ack — is a
//     silent no-op, because a vault that alarmed on honest retries would
//     train its operator to ignore alarms.
//
// Hermetic and ungated: no binaries, no sockets, no domain seam. The segment
// bytes are REAL frozen bytes, written by `hive_core::oplog::LogWriter` into
// a scratch dir under an in-memory key, because a hand-rolled header would
// test this file's idea of the format instead of the format. What the vault
// then sees is a byte string with no key attached — exactly what arrives over
// the wire.

use hive_core::keys::MemoryKeySource;
use hive_core::oplog::{self, kind, LogWriter, Record, SEGMENT_ROTATE_BYTES};
use hive_node::vault::{domain_dir, SegmentVault};
use hive_node::AlarmKind;
use hive_sync::SyncSink;

const TENANT: &str = "household";
const DOMAIN: &str = "bierlysmith.com";

/// The vault under test, in its own node root.
fn vault() -> (tempfile::TempDir, SegmentVault) {
    let root = tempfile::tempdir().expect("node root");
    let vault = SegmentVault::open(root.path(), TENANT, DOMAIN).expect("vault");
    (root, vault)
}

/// Real segment files for `device`, written by the frozen writer and read
/// back as bytes — the same thing a session carries. `rotate_at` forces
/// rotation, so a multi-segment fixture is one the writer actually produces
/// rather than one this test invented.
///
/// `salt` changes the payloads: two fixtures with the same identity tuple and
/// different salts are exactly the equivocation the vault must catch.
fn segments(device: &str, records: usize, salt: u8, rotate_at: u64) -> Vec<(u64, Vec<u8>)> {
    let dir = tempfile::tempdir().expect("scratch log dir");
    let keys = MemoryKeySource([7u8; 32]);
    let mut writer =
        LogWriter::open_with_segment_limit(dir.path(), device, &keys, rotate_at).expect("writer");
    for seq in 1..=records as u64 {
        let record = Record::new(
            device,
            seq,
            seq,
            "2026-07-25T12:00:00.000Z",
            "nate",
            kind::CONFIG_SET,
            ciborium::Value::Text(format!("record {seq} of {device} (salt {salt})")),
        );
        writer.append_batch(&[record]).expect("append");
    }
    drop(writer);

    let mut out = Vec::new();
    for info in oplog::list_segments(dir.path(), device).expect("enumerate the fixture") {
        let bytes = std::fs::read(oplog::segment_path(dir.path(), device, info.start_seq))
            .expect("segment bytes");
        // The fixture checks itself against the frozen parser: if this ever
        // stops being a real segment, the failure says so here rather than
        // pretending the vault rejected something meaningful.
        assert!(oplog::walk_segment(&bytes)
            .expect("fixture is a real segment")
            .is_whole());
        out.push((info.start_seq, bytes));
    }
    out
}

/// One un-rotated segment's bytes, `records` frames long.
fn segment_bytes(device: &str, records: usize, salt: u8) -> Vec<u8> {
    let mut files = segments(device, records, salt, SEGMENT_ROTATE_BYTES);
    assert_eq!(files.len(), 1, "the fixture must not have rotated");
    files.remove(0).1
}

/// Bytes of the segment file as it sits in the vault.
fn on_disk(vault: &SegmentVault, device: &str, start_seq: u64) -> Vec<u8> {
    let path = oplog::segment_path(vault.dir(), device, start_seq);
    std::fs::read(path).unwrap_or_default()
}

/// The offset the first frame starts at — where a tail can be cut so the
/// prefix is a legal whole-frame landing point.
fn frame_end(bytes: &[u8], frames: usize) -> usize {
    let walk = oplog::walk_segment(bytes).expect("a real segment");
    let frame = &walk.frames[frames - 1];
    (frame.offset + frame.len) as usize
}

#[tokio::test]
async fn a_tail_extends_and_the_vault_remembers_what_it_holds() {
    let (_root, vault) = vault();
    let bytes = segment_bytes("dev-alpha", 4, 0);
    let cut = frame_end(&bytes, 2);

    let len = vault
        .extend_segment("dev-alpha", 1, 0, bytes[..cut].to_vec())
        .await
        .expect("first chunk lands");
    assert_eq!(len, cut as u64);

    let len = vault
        .extend_segment("dev-alpha", 1, cut as u64, bytes[cut..].to_vec())
        .await
        .expect("the tail extends");
    assert_eq!(len, bytes.len() as u64);
    assert_eq!(on_disk(&vault, "dev-alpha", 1), bytes, "verbatim");

    let row = vault
        .meta()
        .segment("dev-alpha", 1)
        .unwrap()
        .expect("a meta row");
    assert_eq!(row.len, bytes.len() as u64);
    assert_eq!(row.file_hash, *blake3::hash(&bytes).as_bytes());
    assert!(!row.sealed, "the only segment there is, is the tail");
    assert!(vault.meta().alarms().unwrap().is_empty(), "nothing hostile");
}

/// The lost-ack case. A sender that never heard "landed" re-sends from an
/// offset it already reached; those bytes are identical, so the vault must
/// accept the call, write nothing, and stay quiet.
#[tokio::test]
async fn an_identical_re_upload_is_a_silent_no_op() {
    let (_root, vault) = vault();
    let bytes = segment_bytes("dev-alpha", 3, 0);
    vault
        .extend_segment("dev-alpha", 1, 0, bytes.clone())
        .await
        .unwrap();

    let len = vault
        .extend_segment("dev-alpha", 1, 0, bytes.clone())
        .await
        .expect("a duplicate is not an error");
    assert_eq!(len, bytes.len() as u64);
    assert_eq!(on_disk(&vault, "dev-alpha", 1), bytes);
    assert!(
        vault.meta().alarms().unwrap().is_empty(),
        "an honest retry must not raise an alarm — that is how alarms get ignored"
    );
}

/// Equivocation: the same (device, start_seq) offered with different bytes.
/// A blind node catches this without a key, because it compares ciphertext to
/// ciphertext.
#[tokio::test]
async fn a_differing_re_upload_is_an_alarm_and_a_refusal() {
    let (_root, vault) = vault();
    let held = segment_bytes("dev-alpha", 3, 0);
    let forged = segment_bytes("dev-alpha", 3, 9);
    assert_ne!(held, forged, "different payloads, same identity tuple");

    vault
        .extend_segment("dev-alpha", 1, 0, held.clone())
        .await
        .unwrap();

    let err = vault
        .extend_segment("dev-alpha", 1, 0, forged)
        .await
        .expect_err("a differing re-upload must be refused");
    let text = format!("{err:#}");
    assert!(text.contains("differing re-upload"), "{text}");
    assert!(text.contains("integrity alarm"), "{text}");

    assert_eq!(
        on_disk(&vault, "dev-alpha", 1),
        held,
        "not one byte was overwritten"
    );
    let alarms = vault.meta().alarms().unwrap();
    assert_eq!(alarms.len(), 1);
    assert_eq!(alarms[0].kind, AlarmKind::SegmentDivergence.as_str());
    assert_eq!(alarms[0].device.as_deref(), Some("dev-alpha"));
    assert_eq!(alarms[0].start_seq, Some(1));
}

/// Write-once is about CHANGING bytes, not about appending to an old segment
/// — and the difference is load-bearing. A session killed mid-segment leaves a
/// prefix here; if the device rotates before the next session, the resume that
/// completes it is a write to a segment that is by then sealed. A vault that
/// refused it would strand those bytes forever on the machine whose job is to
/// hold them, so this asserts the resume path works and stays quiet.
#[tokio::test]
async fn a_sealed_segment_this_node_holds_only_part_of_can_still_be_completed() {
    let (_root, vault) = vault();
    // A log that really rotated: two genuine segment files from the frozen
    // writer, the second one proof that the first is final at the source.
    let files = segments("dev-alpha", 8, 0, 512);
    assert!(files.len() >= 2, "the fixture must have rotated");
    let (first_seq, first) = files[0].clone();
    let (second_seq, second) = files[1].clone();

    let cut = frame_end(&first, 1);
    vault
        .extend_segment("dev-alpha", first_seq, 0, first[..cut].to_vec())
        .await
        .unwrap();
    // Rotation lands: a higher start_seq appears, which is the only event
    // that seals anything (oplog/writer.rs never returns to an old segment).
    vault
        .extend_segment("dev-alpha", second_seq, 0, second)
        .await
        .unwrap();
    assert!(
        vault
            .meta()
            .segment("dev-alpha", first_seq)
            .unwrap()
            .unwrap()
            .sealed,
        "the node knows the earlier segment is sealed"
    );

    let len = vault
        .extend_segment("dev-alpha", first_seq, cut as u64, first[cut..].to_vec())
        .await
        .expect("the resume completes it");
    assert_eq!(len, first.len() as u64);
    assert_eq!(on_disk(&vault, "dev-alpha", first_seq), first, "verbatim");
    assert!(
        vault.meta().alarms().unwrap().is_empty(),
        "finishing what was interrupted is not an integrity event"
    );
}

/// ...and the write-once rule still bites on that same sealed segment: what
/// may follow is more bytes, never different ones.
#[tokio::test]
async fn a_differing_re_upload_of_a_sealed_segment_is_still_an_alarm() {
    let (_root, vault) = vault();
    let files = segments("dev-alpha", 8, 0, 512);
    let (first_seq, first) = files[0].clone();
    let (second_seq, second) = files[1].clone();
    vault
        .extend_segment("dev-alpha", first_seq, 0, first.clone())
        .await
        .unwrap();
    vault
        .extend_segment("dev-alpha", second_seq, 0, second)
        .await
        .unwrap();

    // A second history for the same identity tuple, offered after sealing.
    let forged = segments("dev-alpha", 8, 9, 512)[0].1.clone();
    let err = vault
        .extend_segment("dev-alpha", first_seq, 0, forged)
        .await
        .expect_err("a sealed segment may not be rewritten");
    assert!(
        format!("{err:#}").contains("differing re-upload"),
        "{err:#}"
    );
    assert_eq!(on_disk(&vault, "dev-alpha", first_seq), first);
    assert_eq!(
        vault.meta().alarms().unwrap()[0].kind,
        AlarmKind::SegmentDivergence.as_str()
    );
}

/// The belt to that check's braces: if the bytes themselves VANISH — a lost
/// or truncated file — and come back different, the hash this node recorded
/// still catches it, and none of the impostor bytes are kept.
#[tokio::test]
async fn bytes_that_vanish_and_return_different_are_caught_by_the_recorded_hash() {
    let (_root, vault) = vault();
    let held = segment_bytes("dev-alpha", 3, 0);
    let forged = segment_bytes("dev-alpha", 3, 9);
    vault
        .extend_segment("dev-alpha", 1, 0, held.clone())
        .await
        .unwrap();

    // The file is gone; node-meta.db still remembers what it was.
    std::fs::remove_file(oplog::segment_path(vault.dir(), "dev-alpha", 1)).unwrap();

    let err = vault
        .extend_segment("dev-alpha", 1, 0, forged)
        .await
        .expect_err("not the bytes this node attested");
    assert!(format!("{err:#}").contains("integrity alarm"), "{err:#}");
    assert!(
        on_disk(&vault, "dev-alpha", 1).is_empty(),
        "nothing unvouched-for is kept"
    );
    assert_eq!(vault.meta().alarms().unwrap().len(), 1);

    // The real bytes are still welcome back.
    vault
        .extend_segment("dev-alpha", 1, 0, held.clone())
        .await
        .expect("a genuine restore is not an attack");
    assert_eq!(on_disk(&vault, "dev-alpha", 1), held);
}

/// A tail may extend, but only from where it ends. A write past the end is a
/// hole, and the frozen format has no way to fill one later.
#[tokio::test]
async fn a_write_past_the_end_is_refused_without_an_alarm() {
    let (_root, vault) = vault();
    let bytes = segment_bytes("dev-alpha", 3, 0);
    let cut = frame_end(&bytes, 1);
    vault
        .extend_segment("dev-alpha", 1, 0, bytes[..cut].to_vec())
        .await
        .unwrap();

    let err = vault
        .extend_segment("dev-alpha", 1, cut as u64 + 1, bytes[cut..].to_vec())
        .await
        .expect_err("a gap is refused");
    assert!(format!("{err:#}").contains("no holes"), "{err:#}");
    assert!(
        vault.meta().alarms().unwrap().is_empty(),
        "a confused sender is not an attacker"
    );
}

/// Bytes may not be filed under a name that is not theirs: the first chunk of
/// a new segment is parsed keylessly and must say what it was offered as.
#[tokio::test]
async fn bytes_must_carry_the_header_they_were_offered_under() {
    let (_root, vault) = vault();
    let bytes = segment_bytes("dev-alpha", 2, 0);
    let err = vault
        .extend_segment("dev-beta", 1, 0, bytes)
        .await
        .expect_err("the header names dev-alpha");
    assert!(format!("{err:#}").contains("dev-alpha"), "{err:#}");
    assert!(on_disk(&vault, "dev-beta", 1).is_empty());
}

/// A device id is a directory name here. It is checked at the door.
#[tokio::test]
async fn a_hostile_device_id_never_becomes_a_path() {
    let (_root, vault) = vault();
    let err = vault
        .extend_segment("../../etc", 1, 0, vec![0u8; 8])
        .await
        .expect_err("outside the allowlist");
    assert!(format!("{err:#}").contains("allowlist"), "{err:#}");
}

/// The write-once evidence survives a restart: the alarm history and the
/// segment rows are in `node-meta.db`, not in a session's memory.
#[tokio::test]
async fn the_evidence_outlives_the_process() {
    let root = tempfile::tempdir().unwrap();
    let held = segment_bytes("dev-alpha", 3, 0);
    let forged = segment_bytes("dev-alpha", 3, 9);
    {
        let vault = SegmentVault::open(root.path(), TENANT, DOMAIN).unwrap();
        vault
            .extend_segment("dev-alpha", 1, 0, held.clone())
            .await
            .unwrap();
        vault
            .extend_segment("dev-alpha", 1, 0, forged.clone())
            .await
            .unwrap_err();
    }

    // A fresh open — a restarted node.
    let vault = SegmentVault::open(root.path(), TENANT, DOMAIN).unwrap();
    assert_eq!(vault.meta().alarms().unwrap().len(), 1);
    let err = vault
        .extend_segment("dev-alpha", 1, 0, forged)
        .await
        .expect_err("still refused after a restart");
    assert!(
        format!("{err:#}").contains("differing re-upload"),
        "{err:#}"
    );
    assert_eq!(vault.meta().alarms().unwrap().len(), 2, "and alarmed again");
    assert_eq!(on_disk(&vault, "dev-alpha", 1), held);
}

/// A vault directory that arrived by ordinary means (rsync, a ZFS send, a
/// restored snapshot — D36's cheap multi-node story) still gets its
/// bookkeeping: the files are the truth, node-meta.db is the memory of them.
#[tokio::test]
async fn a_copied_in_tree_is_reconciled_at_open() {
    let root = tempfile::tempdir().unwrap();
    let bytes = segment_bytes("dev-alpha", 3, 0);
    let path = oplog::segment_path(&domain_dir(root.path(), TENANT, DOMAIN), "dev-alpha", 1);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &bytes).unwrap();

    let vault = SegmentVault::open(root.path(), TENANT, DOMAIN).unwrap();
    let row = vault.meta().segment("dev-alpha", 1).unwrap().unwrap();
    assert_eq!(row.len, bytes.len() as u64);
    assert_eq!(row.file_hash, *blake3::hash(&bytes).as_bytes());

    // And the write-once rule now covers bytes this node never received.
    let forged = segment_bytes("dev-alpha", 3, 9);
    vault
        .extend_segment("dev-alpha", 1, 0, forged)
        .await
        .unwrap_err();
    assert_eq!(vault.meta().alarms().unwrap().len(), 1);
}

/// Blocks are content-addressed, so a second put of the same id is the same
/// bytes by construction — no write-once question to ask, and misaddressed
/// content is refused by the address itself.
#[tokio::test]
async fn blocks_are_their_own_write_once_rule() {
    let (_root, vault) = vault();
    let bytes = b"ciphertext block".to_vec();
    let id = *blake3::hash(&bytes).as_bytes();

    assert!(!vault.has_block(id).await.unwrap());
    vault.put_block(id, bytes.clone()).await.unwrap();
    assert!(vault.has_block(id).await.unwrap());
    vault.put_block(id, bytes.clone()).await.unwrap();
    assert_eq!(
        vault.meta().block_size(&id).unwrap(),
        Some(bytes.len() as u64)
    );

    let err = vault
        .put_block(id, b"different bytes".to_vec())
        .await
        .expect_err("misaddressed content is refused");
    assert!(
        format!("{err:#}").contains("refusing misaddressed"),
        "{err:#}"
    );
    assert!(
        vault.meta().alarms().unwrap().is_empty(),
        "not an equivocation"
    );
}

/// The heads snapshot a vault reports is the one a device would report over
/// the same files — which is what makes "restore is a copy" true.
#[tokio::test]
async fn the_vault_reports_the_same_heads_a_store_would() {
    let (_root, vault) = vault();
    let alpha = segment_bytes("dev-alpha", 2, 0);
    let beta = segment_bytes("dev-beta", 1, 1);
    vault
        .extend_segment("dev-alpha", 1, 0, alpha.clone())
        .await
        .unwrap();
    vault
        .extend_segment("dev-beta", 1, 0, beta.clone())
        .await
        .unwrap();

    let heads = vault.heads().unwrap();
    assert_eq!(heads.devices(), vec!["dev-alpha", "dev-beta"]);
    let seg = heads.segment("dev-alpha", 1).unwrap();
    assert_eq!(seg.len, alpha.len() as u64);
    assert_eq!(seg.file_hash, *blake3::hash(&alpha).as_bytes());
    assert_eq!(heads.total_len(), (alpha.len() + beta.len()) as u64);
}

/// `landed` is the resume point, and it is always a whole-frame offset: a
/// torn tail (a session killed mid-frame) is cut back before it is reported,
/// and the node's memory follows the file.
#[tokio::test]
async fn a_torn_tail_is_cut_back_to_a_landing_point() {
    let (_root, vault) = vault();
    let bytes = segment_bytes("dev-alpha", 3, 0);
    let whole = frame_end(&bytes, 2);
    let torn = whole + 5;
    vault
        .extend_segment("dev-alpha", 1, 0, bytes[..torn].to_vec())
        .await
        .unwrap();

    let landed = vault
        .landed("dev-alpha", 1)
        .await
        .unwrap()
        .expect("something landed");
    assert_eq!(landed.len, whole as u64, "whole frames only");
    assert_eq!(on_disk(&vault, "dev-alpha", 1).len(), whole);
    assert_eq!(
        vault.meta().segment("dev-alpha", 1).unwrap().unwrap().len,
        whole as u64,
        "the memory follows the file"
    );

    // And the sender resumes from exactly there.
    vault
        .extend_segment("dev-alpha", 1, whole as u64, bytes[whole..].to_vec())
        .await
        .unwrap();
    assert_eq!(on_disk(&vault, "dev-alpha", 1), bytes);
}

/// `landed` for a segment nobody has sent is `None`, not an error — the
/// receiver asks about everything the offer named.
#[tokio::test]
async fn an_unseen_segment_has_no_landing_point() {
    let (_root, vault) = vault();
    assert!(vault.landed("dev-alpha", 1).await.unwrap().is_none());
}
