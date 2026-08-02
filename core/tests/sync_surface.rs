// The core sync read surface (PLAN-v2.1 PR 4.3, D29): keyless segment
// enumeration, header parse and frame walk against real segment files; the
// blockstore's bare-id block APIs; and the Store pass-throughs the desktop's
// push loop will call. Hermetic: tempdirs + MemoryKeySource + test_store()
// (ONE-SEAM), no network, no clock with meaning.
//
// The load-bearing test is the parity one: the wire is about to move segments
// as verbatim bytes and let a keyless peer verify them, so the keyless view
// must agree with the keyed reader's frame-for-frame — same hashes, same
// boundaries — or the two halves of replication disagree about what a segment
// contains. HOSTILE-DECODE coverage of the same parsers (adversarial bytes)
// lives in core/tests/hostile_decode.rs.

mod common;

use std::fs;
use std::path::Path;

use hive_core::blockstore::BlockStore;
use hive_core::keys::MemoryKeySource;
use hive_core::oplog::{self, kind, LogReader, LogWriter, Record};
use serde_json::json;

/// One key for the whole file, from the seam — the standalone logs and
/// blockstores below are read back both keyed and keyless, and a Store's own
/// data dir joins them in the last test.
fn keysource() -> MemoryKeySource {
    common::test_keys()
}

fn test_record(device: &str, seq: u64) -> Record {
    Record::new(
        device,
        seq,
        seq,
        "2026-07-20T09:00:00.000Z",
        "nate",
        kind::JOURNAL_APPEND,
        ciborium::Value::Map(vec![
            (
                ciborium::Value::Text("id".into()),
                ciborium::Value::Text(format!("jr-{seq:04}")),
            ),
            (
                ciborium::Value::Text("body".into()),
                ciborium::Value::Text("the quick brown fox jumps over the lazy dog ".repeat(6)),
            ),
        ]),
    )
}

/// A log with several segments: ~350-byte frames against a 2 KiB rotation
/// limit rotates every ~6 records (the oplog.rs recipe).
fn seeded_log(data_dir: &Path, device: &str, records: u64) -> Vec<[u8; 32]> {
    let mut w =
        LogWriter::open_with_segment_limit(data_dir, device, &keysource(), 2048).expect("writer");
    let batch: Vec<Record> = (1..=records).map(|i| test_record(device, i)).collect();
    w.append_batch(&batch).expect("append")
}

/// Every frame hash the keyed reader yields, in log order.
fn keyed_hashes(data_dir: &Path, device: &str) -> Vec<[u8; 32]> {
    LogReader::scan(data_dir, device, &keysource())
        .unwrap()
        .map(|item| item.unwrap().1)
        .collect()
}

#[test]
fn keyless_walk_agrees_with_the_keyed_reader() {
    let tmp = tempfile::tempdir().unwrap();
    let written = seeded_log(tmp.path(), "dev-a", 12);

    let segments = oplog::list_segments(tmp.path(), "dev-a").unwrap();
    assert!(segments.len() >= 2, "expected rotation, got {segments:?}");
    assert_eq!(segments[0].start_seq, 1);

    let mut walked = Vec::new();
    for (i, info) in segments.iter().enumerate() {
        let path = oplog::segment_path(tmp.path(), &info.device, info.start_seq);
        let bytes = fs::read(&path).unwrap();

        // Enumeration describes the file exactly: length, whole-file hash,
        // and "sealed" = every segment but the writer's current tail.
        assert_eq!(info.device, "dev-a");
        assert_eq!(info.len, bytes.len() as u64);
        assert_eq!(info.file_hash, *blake3::hash(&bytes).as_bytes());
        assert_eq!(info.sealed, i + 1 < segments.len(), "sealed flag at {i}");

        // The keyless walk sees the same header the writer wrote and stops
        // exactly at EOF — no key involved anywhere.
        let walk = oplog::walk_segment(&bytes).unwrap();
        assert_eq!(walk.header.device, "dev-a");
        assert_eq!(walk.header.start_seq, info.start_seq);
        assert_eq!(walk.header.version, 1);
        assert_eq!(walk.header.wrapped_key_len, 72);
        assert!(
            walk.is_whole(),
            "an intact segment ends on a frame boundary"
        );
        assert_eq!(walk.whole_end, info.len);
        assert_eq!(walk.frames[0].offset, walk.header.len);
        for pair in walk.frames.windows(2) {
            assert_eq!(
                pair[0].offset + pair[0].len,
                pair[1].offset,
                "frames are back to back"
            );
        }
        walked.extend(walk.frames.iter().map(|f| f.hash));
    }

    // Parity: same frames, same order, same hashes as the keyed reader — and
    // as the writer reported at append time.
    assert_eq!(walked.len(), 12);
    assert_eq!(walked, written);
    assert_eq!(walked, keyed_hashes(tmp.path(), "dev-a"));
}

#[test]
fn header_parse_stops_before_the_key() {
    let tmp = tempfile::tempdir().unwrap();
    seeded_log(tmp.path(), "dev-h", 2);
    let path = oplog::segment_path(tmp.path(), "dev-h", 1);
    let mut bytes = fs::read(&path).unwrap();

    let header = oplog::parse_header(&bytes).unwrap();
    assert_eq!(header.device, "dev-h");
    assert_eq!(header.start_seq, 1);
    // The header ends after the wrapped key; frame 0 starts there.
    assert_eq!(header.len, (21 + "dev-h".len() + 72) as u64);

    // Corrupt the wrapped key itself: the keyless parse cannot care (it never
    // unwraps), while the keyed reader fails on exactly that step. This is
    // the "stops before key-unwrap" contract, stated as a behavioural
    // difference rather than a comment.
    let key_at = 21 + "dev-h".len();
    bytes[key_at] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();
    assert_eq!(oplog::parse_header(&bytes).unwrap(), header);
    let err = LogReader::scan(tmp.path(), "dev-h", &keysource())
        .unwrap()
        .next()
        .unwrap()
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("unwrapping segment key"),
        "{err}"
    );
}

#[test]
fn torn_tail_walk_stops_at_the_last_whole_frame() {
    let tmp = tempfile::tempdir().unwrap();
    let written = seeded_log(tmp.path(), "dev-t", 3);
    let path = oplog::segment_path(tmp.path(), "dev-t", 1);
    let whole = fs::read(&path).unwrap();
    let intact = oplog::walk_segment(&whole).unwrap();
    assert_eq!(intact.frames.len(), 3);

    // Tear the third frame in half — the crash-mid-append shape.
    let cut = intact.frames[2].offset + intact.frames[2].len / 2;
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(cut)
        .unwrap();

    let torn = fs::read(&path).unwrap();
    let walk = oplog::walk_segment(&torn).unwrap();
    assert_eq!(walk.frames.len(), 2, "only whole frames count");
    assert_eq!(
        walk.frames.iter().map(|f| f.hash).collect::<Vec<_>>(),
        written[..2].to_vec()
    );
    assert!(!walk.is_whole());
    // The resume point: whole_end is where a receiver re-requests from, and
    // it is exactly where the writer's own repair truncates to.
    assert_eq!(
        walk.whole_end,
        intact.frames[1].offset + intact.frames[1].len
    );
    assert_eq!(walk.partial_tail, cut - walk.whole_end);
    let mut w = LogWriter::open(tmp.path(), "dev-t", &keysource()).unwrap();
    assert_eq!(w.last_seq(), 2);
    assert_eq!(fs::metadata(&path).unwrap().len(), walk.whole_end);

    // A length word that survived the tear but describes nothing (a partial
    // write inside the header of the next frame) is a torn tail too, not an
    // error: the walk reports leftovers instead of refusing the file.
    w.append_batch(&[test_record("dev-t", 3)]).unwrap();
    drop(w);
    let mut garbled = fs::read(&path).unwrap();
    let last = oplog::walk_segment(&garbled).unwrap().frames[2].offset as usize;
    garbled[last..last + 4].copy_from_slice(&3u32.to_le_bytes()); // below the AEAD tag
    let walk = oplog::walk_segment(&garbled).unwrap();
    assert_eq!(walk.frames.len(), 2);
    assert_eq!(walk.whole_end, last as u64);
    assert!(walk.partial_tail > 0);
}

#[test]
fn heads_cover_every_device_and_survive_a_cbor_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    seeded_log(tmp.path(), "dev-a", 12);
    seeded_log(tmp.path(), "dev-b", 2);

    let heads = oplog::heads(tmp.path()).unwrap();
    assert_eq!(heads.devices(), vec!["dev-a", "dev-b"]);
    assert_eq!(oplog::list_devices(tmp.path()).unwrap(), ["dev-a", "dev-b"]);
    assert_eq!(
        heads.segments,
        [
            oplog::list_segments(tmp.path(), "dev-a").unwrap(),
            oplog::list_segments(tmp.path(), "dev-b").unwrap(),
        ]
        .concat(),
        "a snapshot is ordered by (device, start_seq), never by readdir order"
    );
    assert_eq!(heads.segment("dev-b", 1).unwrap().start_seq, 1);
    assert!(heads.segment("dev-b", 7).is_none());
    assert_eq!(
        heads.total_len(),
        heads.segments.iter().map(|s| s.len).sum::<u64>()
    );

    // The snapshot IS the heads-exchange payload (PR 4.4): it round-trips
    // through CBOR, and the file hash rides as a byte string, not an array of
    // 32 integers (the encoding the frozen record envelope also uses).
    let mut encoded = Vec::new();
    ciborium::into_writer(&heads, &mut encoded).unwrap();
    let back: oplog::HeadsSnapshot = ciborium::from_reader(encoded.as_slice()).unwrap();
    assert_eq!(back, heads);
    assert!(
        encoded.windows(2).any(|w| w == [0x58, 0x20]),
        "file_hash must encode as a 32-byte CBOR byte string"
    );

    // An empty data dir has no heads at all (a store that never committed).
    let empty = tempfile::tempdir().unwrap();
    assert!(oplog::heads(empty.path()).unwrap().segments.is_empty());
    assert!(oplog::list_devices(empty.path()).unwrap().is_empty());
    assert!(oplog::list_segments(empty.path(), "dev-none")
        .unwrap()
        .is_empty());
    assert!(oplog::list_segments(empty.path(), "../escape").is_err());
}

/// Deterministic pseudo-random bytes (the blockstore.rs recipe).
fn seeded_bytes(len: usize, seed: &str) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed.as_bytes());
    hasher.finalize_xof().fill(&mut out);
    out
}

#[test]
fn blocks_move_by_bare_id_and_a_manifest_alone_is_not_a_blob() {
    let src_dir = tempfile::tempdir().unwrap();
    let peer_dir = tempfile::tempdir().unwrap();
    let src = BlockStore::open(src_dir.path()).unwrap();
    // The peer is BLIND: it opens the same shape and never sees a key.
    let peer = BlockStore::open(peer_dir.path()).unwrap();
    let keys = keysource();

    let bytes = seeded_bytes(3 * 1024 * 1024, "sync surface blob");
    let blob = src
        .put(&keys, &bytes, Some("application/octet-stream"))
        .unwrap();
    let ids = src.blob_block_ids(&keys, &blob).unwrap();
    assert!(ids.len() >= 3, "multi-chunk blob plus its manifest");
    assert_eq!(*ids.last().unwrap(), blob.manifest_hash, "manifest last");
    assert!(ids.iter().all(|id| src.has_block(id)));
    let mut all = src.list_block_ids().unwrap();
    let mut want = ids.clone();
    want.sort();
    all.sort();
    assert_eq!(all, want, "enumeration sees exactly this blob's blocks");

    // Manifest first, chunks withheld: `has()` says yes (it probes the
    // manifest alone) but the blob is NOT complete — completeness is the
    // per-block, keyed-side judgment.
    peer.put_block(
        &blob.manifest_hash,
        &src.get_block(&blob.manifest_hash).unwrap(),
    )
    .unwrap();
    assert!(peer.has(&blob), "the manifest block is present");
    assert!(
        !ids.iter().all(|id| peer.has_block(id)),
        "a manifest alone must never read as a complete blob"
    );
    assert!(peer.get(&keys, &blob).is_err());

    // Now the chunks, by bare id, verified on write with no key in sight.
    for id in &ids {
        peer.put_block(id, &src.get_block(id).unwrap()).unwrap();
    }
    assert!(ids.iter().all(|id| peer.has_block(id)));
    assert_eq!(peer.get(&keys, &blob).unwrap(), bytes);
    assert_eq!(peer.blob_block_ids(&keys, &blob).unwrap(), ids);

    // Misaddressed bytes are refused, and nothing lands.
    let mut lie = src.get_block(&ids[0]).unwrap();
    lie[0] ^= 0x01;
    let err = peer.put_block(&[0u8; 32], &lie).unwrap_err();
    assert!(format!("{err:#}").contains("misaddressed"), "{err}");
    assert!(!peer.has_block(&[0u8; 32]));
    // An id nobody stored is simply absent, never an error.
    assert!(!peer.has_block(&[9u8; 32]));
    assert!(peer.get_block(&[9u8; 32]).is_err());
}

#[tokio::test]
async fn store_surface_serves_heads_and_blocks() {
    let store = common::test_store().await;
    store
        .journal_append(
            serde_json::from_value(json!({"body": "sync surface entry"})).unwrap(),
            Some("nate"),
            None,
        )
        .await
        .unwrap();

    // Heads: this store's own device, one open (unsealed) segment, described
    // exactly as it sits on disk.
    let heads = store.sync_heads().await.unwrap();
    assert_eq!(heads.devices(), vec![store.device()]);
    let seg = &heads.segments[0];
    assert!(!seg.sealed, "the only segment is the writer's tail");
    let path = oplog::segment_path(store.data_dir(), store.device(), seg.start_seq);
    let raw = fs::read(&path).unwrap();
    assert_eq!(seg.len, raw.len() as u64);
    assert_eq!(seg.file_hash, *blake3::hash(&raw).as_bytes());
    assert_eq!(
        store.sync_segments(store.device()).await.unwrap(),
        heads.segments
    );
    assert!(oplog::walk_segment(&raw).unwrap().is_whole());

    // Blocks by bare id, through the writer thread.
    let bytes = seeded_bytes(4096, "store surface block");
    let id = *blake3::hash(&bytes).as_bytes();
    assert!(!store.sync_has_block(id).await.unwrap());
    store.sync_put_block(id, bytes.clone()).await.unwrap();
    assert!(store.sync_has_block(id).await.unwrap());
    assert_eq!(store.sync_get_block(id).await.unwrap(), bytes);
    assert!(store.sync_block_ids().await.unwrap().contains(&id));
    assert!(store.sync_put_block([1u8; 32], bytes).await.is_err());

    // The keyed enumeration over a blob that landed in this store's own
    // blockstore (the restore shape: bytes beside the store, read through
    // it). The seam owns the master key, so the test never re-types it.
    let blob_bytes = seeded_bytes(512 * 1024, "store surface blob");
    let blob = BlockStore::open(store.data_dir())
        .unwrap()
        .put(&keysource(), &blob_bytes, None)
        .unwrap();
    let ids = store.sync_blob_block_ids(blob.clone()).await.unwrap();
    assert_eq!(*ids.last().unwrap(), blob.manifest_hash);
    for id in &ids {
        assert!(store.sync_has_block(*id).await.unwrap());
        assert_eq!(
            *blake3::hash(&store.sync_get_block(*id).await.unwrap()).as_bytes(),
            *id,
            "every served block is its own content address"
        );
    }
}
