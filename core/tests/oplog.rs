// Op-log integration tests (PR 1.4). Hermetic: tempdir + MemoryKeySource,
// no Postgres, no network, no clock reads with meaning (timestamps are
// fixed strings — the envelope only freezes their shape).
//
// The golden-fixture test is the format freeze: the checked-in .bin files
// under tests/fixtures/oplog/ are the byte-exact CBOR encodings of one
// record per kind. If it fails, you changed the frozen record format —
// that is an envelope-version decision, not a fixture refresh. To
// regenerate deliberately: HIVE_UPDATE_GOLDENS=1 cargo test -p hive-core
// --test oplog (the run fails on purpose so it can't pass in regen mode).

use std::fs;
use std::path::{Path, PathBuf};

use ciborium::Value;
use hive_core::keys::MemoryKeySource;
use hive_core::oplog::{kind, ts_shape_ok, LogReader, LogWriter, Record};

fn keysource() -> MemoryKeySource {
    MemoryKeySource([7u8; 32])
}

fn t(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (t(k), v)).collect())
}

fn arr(vals: Vec<Value>) -> Value {
    Value::Array(vals)
}

/// One fixed record per kind, in kind::ALL order. Payloads are
/// representative, not schema-frozen (the fold owns payload semantics in
/// PR 1.5) — but they deliberately exercise every CBOR shape the envelope
/// can carry: text, unsigned/negative ints, bool, null, arrays, nested
/// maps, and byte strings.
fn fixture_records() -> Vec<Record> {
    let payloads: Vec<(&str, Value)> = vec![
        (
            kind::JOURNAL_APPEND,
            map(vec![
                ("id", t("jr-0001")),
                (
                    "body",
                    t("Kickoff. We must freeze the record format this week."),
                ),
                ("tags", arr(vec![t("p2p"), t("pivot")])),
                ("vis", Value::Null),
            ]),
        ),
        (
            kind::ENTITY_CREATE,
            map(vec![
                ("id", t("tasks-0001")),
                ("entity_type", t("task")),
                (
                    "fields",
                    map(vec![
                        ("title", t("Freeze the record format")),
                        ("priority", t("high")),
                        ("estimate", Value::from(3u64)),
                    ]),
                ),
            ]),
        ),
        (
            kind::ENTITY_UPDATE,
            map(vec![
                ("id", t("tasks-0001")),
                ("fields", map(vec![("status", t("done"))])),
                ("lww", Value::Bool(true)),
            ]),
        ),
        (
            kind::LINK_ADD,
            map(vec![
                ("source_table", t("journal")),
                ("source_id", t("jr-0001")),
                ("target_table", t("tasks")),
                ("target_id", t("tasks-0001")),
                ("link_type", t("spawned_in")),
            ]),
        ),
        (kind::LINK_REMOVE, map(vec![("link_id", t("links-0001"))])),
        (
            kind::TOMBSTONE,
            map(vec![
                ("target_table", t("tasks")),
                ("target_id", t("tasks-0001")),
                ("reason", Value::Null),
            ]),
        ),
        (
            kind::REDACT,
            map(vec![
                ("device", t("fixdev-01")),
                ("seq", Value::from(2u64)),
                ("fields", arr(vec![t("body")])),
            ]),
        ),
        (
            kind::CONFIG_SET,
            map(vec![
                ("key", t("sync.relay")),
                (
                    "value",
                    map(vec![
                        ("url", t("https://relay.beesroadhouse.com")),
                        ("port", Value::from(443u64)),
                        ("enabled", Value::Bool(true)),
                    ]),
                ),
            ]),
        ),
        (
            kind::MODULE_DOC,
            map(vec![
                ("module", t("mail")),
                ("doc_id", t("mail-0001")),
                (
                    "blob",
                    map(vec![
                        ("manifest_hash", Value::Bytes(vec![0xAB; 32])),
                        ("size", Value::from(2048u64)),
                        ("mime", t("text/plain")),
                    ]),
                ),
                ("offset", Value::from(-1i64)),
            ]),
        ),
        (
            kind::CURSOR_SET,
            map(vec![("module", t("mail")), ("cursor", t("jmap-state-017"))]),
        ),
        (
            kind::ALIAS,
            map(vec![
                ("from", t("blob-old-hash")),
                ("to", t("blob-new-hash")),
            ]),
        ),
    ];
    assert_eq!(payloads.len(), kind::ALL.len());
    payloads
        .into_iter()
        .enumerate()
        .map(|(i, (k, payload))| {
            assert_eq!(k, kind::ALL[i], "fixture order must match kind::ALL");
            let seq = (i + 1) as u64;
            let mut rec = Record::new(
                "fixdev-01",
                seq,
                100 + seq,
                &format!("2026-07-10T12:00:00.{:03}Z", i),
                "nate",
                k,
                payload,
            );
            rec.prev = if seq == 1 {
                [0u8; 32]
            } else {
                *blake3::hash(k.as_bytes()).as_bytes()
            };
            rec
        })
        .collect()
}

#[test]
fn golden_record_encoding_is_frozen() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oplog");
    let regen = std::env::var_os("HIVE_UPDATE_GOLDENS").is_some();
    for rec in fixture_records() {
        assert!(ts_shape_ok(&rec.ts), "fixture ts must be shape-valid");
        let bytes = rec.to_cbor_bytes().unwrap();
        let path = dir.join(format!("{}.bin", rec.kind));
        if regen {
            fs::create_dir_all(&dir).unwrap();
            fs::write(&path, &bytes).unwrap();
            continue;
        }
        let want = fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "missing/unreadable golden fixture {} ({e}); if this is a brand-new kind, \
                 regenerate deliberately with HIVE_UPDATE_GOLDENS=1",
                path.display()
            )
        });
        assert_eq!(
            bytes, want,
            "FROZEN record encoding changed for kind {:?} — this is a format break, \
             not a fixture refresh (see core/src/oplog/mod.rs)",
            rec.kind
        );
        // Decode-back: the fixture bytes must reproduce the record exactly.
        let back = Record::from_cbor_bytes(&want).unwrap();
        assert_eq!(back, rec, "decode-back mismatch for kind {:?}", rec.kind);
    }
    assert!(
        !regen,
        "golden fixtures were regenerated; rerun without HIVE_UPDATE_GOLDENS to verify"
    );
}

fn test_record(device: &str, seq: u64) -> Record {
    Record::new(
        device,
        seq,
        seq, // lc: any monotone value will do for these tests
        "2026-07-10T09:00:00.000Z",
        "nate",
        kind::JOURNAL_APPEND,
        map(vec![
            ("id", t(&format!("jr-{seq:04}"))),
            (
                "body",
                t(&"the quick brown fox jumps over the lazy dog ".repeat(6)),
            ),
        ]),
    )
}

fn segment_files(data_dir: &Path, device: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(data_dir.join("log").join(device))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn scan_all(data_dir: &Path, device: &str) -> Vec<(Record, [u8; 32])> {
    LogReader::scan(data_dir, device, &keysource())
        .unwrap()
        .collect::<anyhow::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn segment_rotation_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = keysource();
    // ~350-byte frames against a 2 KiB limit: rotation every ~6 records.
    let mut w = LogWriter::open_with_segment_limit(tmp.path(), "dev-a", &keys, 2048).unwrap();
    assert_eq!(w.last_seq(), 0);
    assert_eq!(w.last_frame_hash(), [0u8; 32]);

    let first: Vec<Record> = (1..=8).map(|i| test_record("dev-a", i)).collect();
    let second: Vec<Record> = (9..=12).map(|i| test_record("dev-a", i)).collect();
    let mut hashes = w.append_batch(&first).unwrap();
    hashes.extend(w.append_batch(&second).unwrap());
    assert_eq!(hashes.len(), 12);
    assert_eq!(w.last_seq(), 12);
    assert_eq!(w.last_frame_hash(), hashes[11]);

    // Rotation happened: several segments, the first named for seq 1.
    let names = segment_files(tmp.path(), "dev-a");
    assert!(names.len() >= 2, "expected rotation, got {names:?}");
    assert_eq!(names[0], "0000000000000001.seg");

    // Scan returns everything, in order, chain intact across the boundary.
    let items = scan_all(tmp.path(), "dev-a");
    assert_eq!(items.len(), 12);
    for (i, (rec, hash)) in items.iter().enumerate() {
        assert_eq!(rec.seq, (i + 1) as u64);
        assert_eq!(*hash, hashes[i], "frame hash mismatch at seq {}", rec.seq);
        let want_prev = if i == 0 { [0u8; 32] } else { hashes[i - 1] };
        assert_eq!(rec.prev, want_prev, "prev chain broken at seq {}", rec.seq);
        assert_eq!(rec.kind, kind::JOURNAL_APPEND);
    }

    // Reopen: chain state survives, appends continue seamlessly.
    drop(w);
    let mut w2 = LogWriter::open_with_segment_limit(tmp.path(), "dev-a", &keys, 2048).unwrap();
    assert_eq!(w2.last_seq(), 12);
    assert_eq!(w2.last_frame_hash(), hashes[11]);
    let h13 = w2.append_batch(&[test_record("dev-a", 13)]).unwrap();
    let items = scan_all(tmp.path(), "dev-a");
    assert_eq!(items.len(), 13);
    assert_eq!(items[12].0.prev, hashes[11]);
    assert_eq!(items[12].1, h13[0]);
}

#[test]
fn writer_validates_the_envelope() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = keysource();
    let mut w = LogWriter::open(tmp.path(), "dev-v", &keys).unwrap();

    // Gap in seq.
    let err = w.append_batch(&[test_record("dev-v", 2)]).unwrap_err();
    assert!(err.to_string().contains("gapless"), "{err}");
    // Wrong device.
    let err = w.append_batch(&[test_record("other", 1)]).unwrap_err();
    assert!(err.to_string().contains("device"), "{err}");
    // Malformed ts.
    let mut bad_ts = test_record("dev-v", 1);
    bad_ts.ts = "2026-07-10T09:00:00Z".into();
    let err = w.append_batch(&[bad_ts]).unwrap_err();
    assert!(err.to_string().contains("ISO shape"), "{err}");
    // Unknown kind.
    let mut bad_kind = test_record("dev-v", 1);
    bad_kind.kind = "journal.rewrite".into();
    let err = w.append_batch(&[bad_kind]).unwrap_err();
    assert!(err.to_string().contains("unknown kind"), "{err}");
    // Validation failures poison nothing: a good append still lands.
    w.append_batch(&[test_record("dev-v", 1)]).unwrap();
    assert_eq!(w.last_seq(), 1);
}

/// Parse the plaintext structure of a segment file: header end offset plus
/// each frame's (start, end) byte range. Mirrors the frozen layout in
/// core/src/oplog/mod.rs — lengths are plaintext, so no keys needed.
fn frame_ranges(seg: &[u8]) -> (usize, Vec<(usize, usize)>) {
    assert_eq!(&seg[..8], b"HIVELOG1");
    let dlen = u16::from_le_bytes([seg[9], seg[10]]) as usize;
    let wlen_at = 11 + dlen + 8;
    let wlen = u16::from_le_bytes([seg[wlen_at], seg[wlen_at + 1]]) as usize;
    let header_end = wlen_at + 2 + wlen;
    let mut ranges = Vec::new();
    let mut off = header_end;
    while off < seg.len() {
        let clen = u32::from_le_bytes(seg[off..off + 4].try_into().unwrap()) as usize;
        let end = off + 4 + 24 + clen;
        assert!(end <= seg.len(), "test parsed past EOF");
        ranges.push((off, end));
        off = end;
    }
    (header_end, ranges)
}

fn only_segment(tmp: &Path, device: &str) -> PathBuf {
    let names = segment_files(tmp, device);
    assert_eq!(
        names.len(),
        1,
        "test expects a single segment, got {names:?}"
    );
    tmp.join("log").join(device).join(&names[0])
}

#[test]
fn torn_tail_truncates_to_last_valid_frame_and_appends_continue() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = keysource();
    let mut w = LogWriter::open(tmp.path(), "dev-t", &keys).unwrap();
    let records: Vec<Record> = (1..=3).map(|i| test_record("dev-t", i)).collect();
    let hashes = w.append_batch(&records).unwrap();
    drop(w);

    // Tear the third frame in half — the canonical crash-mid-append shape.
    let seg_path = only_segment(tmp.path(), "dev-t");
    let seg = fs::read(&seg_path).unwrap();
    let (_, ranges) = frame_ranges(&seg);
    assert_eq!(ranges.len(), 3);
    let (start, end) = ranges[2];
    let cut = start + (end - start) / 2;
    let f = fs::OpenOptions::new().write(true).open(&seg_path).unwrap();
    f.set_len(cut as u64).unwrap();
    drop(f);

    // Reopen: recovery truncates to the last complete frame.
    let mut w = LogWriter::open(tmp.path(), "dev-t", &keys).unwrap();
    assert_eq!(w.last_seq(), 2);
    assert_eq!(w.last_frame_hash(), hashes[1]);
    assert_eq!(fs::metadata(&seg_path).unwrap().len() as usize, ranges[1].1);

    // Scan agrees: exactly the two complete records survive.
    let items = scan_all(tmp.path(), "dev-t");
    assert_eq!(items.len(), 2);
    assert_eq!(items[1].0.seq, 2);

    // Appends still work, chaining off the surviving frame.
    let h3 = w.append_batch(&[test_record("dev-t", 3)]).unwrap();
    let items = scan_all(tmp.path(), "dev-t");
    assert_eq!(items.len(), 3);
    assert_eq!(items[2].0.seq, 3);
    assert_eq!(items[2].0.prev, hashes[1]);
    assert_eq!(items[2].1, h3[0]);
}

#[test]
fn scan_reports_corruption_at_the_flipped_frame() {
    let tmp = tempfile::tempdir().unwrap();
    let keys = keysource();
    let mut w = LogWriter::open(tmp.path(), "dev-c", &keys).unwrap();
    let records: Vec<Record> = (1..=3).map(|i| test_record("dev-c", i)).collect();
    w.append_batch(&records).unwrap();
    drop(w);

    // Flip one ciphertext byte in the second frame.
    let seg_path = only_segment(tmp.path(), "dev-c");
    let mut seg = fs::read(&seg_path).unwrap();
    let (_, ranges) = frame_ranges(&seg);
    let ct_byte = ranges[1].0 + 4 + 24 + 5; // 5 bytes into frame 2's ciphertext
    seg[ct_byte] ^= 0x01;
    fs::write(&seg_path, &seg).unwrap();

    let mut it = LogReader::scan(tmp.path(), "dev-c", &keys).unwrap();
    // Frame 1 is fine.
    let first = it.next().unwrap().unwrap();
    assert_eq!(first.0.seq, 1);
    // Frame 2 is reported as corrupt, naming the frame.
    let err = it.next().unwrap().unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("seq 2"), "error should name the frame: {msg}");
    assert!(msg.contains("authentication failed"), "{msg}");
    // The scan fuses after the error.
    assert!(it.next().is_none());
}
