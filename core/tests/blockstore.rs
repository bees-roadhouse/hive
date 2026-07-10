// Blockstore integration tests (PR 1.4). Hermetic: tempdir +
// MemoryKeySource. Deterministic payloads come from the blake3 XOF, so
// multi-megabyte fixtures need no checked-in files and no randomness.

use std::path::Path;

use hive_core::blockstore::{BlockStore, CDC_MAX, SINGLE_BLOCK_CUTOFF};
use hive_core::keys::MemoryKeySource;

fn keysource() -> MemoryKeySource {
    MemoryKeySource([42u8; 32])
}

/// Deterministic pseudo-random bytes: blake3 XOF over a seed string.
fn seeded_bytes(len: usize, seed: &str) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed.as_bytes());
    hasher.finalize_xof().fill(&mut out);
    out
}

/// Count block files on disk (any regular file under blocks/, tmp included —
/// none should linger).
fn block_file_count(data_dir: &Path) -> usize {
    fn walk(dir: &Path, acc: &mut usize) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let ty = entry.file_type().unwrap();
            if ty.is_dir() {
                walk(&entry.path(), acc);
            } else {
                *acc += 1;
            }
        }
    }
    let mut n = 0;
    walk(&data_dir.join("blocks"), &mut n);
    n
}

#[test]
fn small_blob_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlockStore::open(tmp.path()).unwrap();
    let keys = keysource();
    let bytes = seeded_bytes(1024, "small blob");

    let blob = store.put(&keys, &bytes, Some("text/plain")).unwrap();
    assert_eq!(blob.size, 1024);
    assert_eq!(blob.mime.as_deref(), Some("text/plain"));
    assert_eq!(blob.plaintext_hash, *blake3::hash(&bytes).as_bytes());
    assert!(store.has(&blob));
    // One chunk block + one manifest block.
    assert_eq!(block_file_count(tmp.path()), 2);

    let back = store.get(&keys, &blob).unwrap();
    assert_eq!(back, bytes);
}

#[test]
fn empty_blob_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlockStore::open(tmp.path()).unwrap();
    let keys = keysource();
    let blob = store.put(&keys, &[], None).unwrap();
    assert_eq!(blob.size, 0);
    assert_eq!(blob.mime, None);
    assert_eq!(store.get(&keys, &blob).unwrap(), Vec::<u8>::new());
}

#[test]
fn multi_chunk_blob_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlockStore::open(tmp.path()).unwrap();
    let keys = keysource();
    // > 3 MiB, deterministic, well past the 256 KiB single-block cutoff.
    let len = 3 * 1024 * 1024 + 512 * 1024;
    assert!(len > SINGLE_BLOCK_CUTOFF);
    let bytes = seeded_bytes(len, "multi chunk blob");

    let blob = store
        .put(&keys, &bytes, Some("application/octet-stream"))
        .unwrap();
    assert_eq!(blob.size, len as u64);
    // FastCDC caps chunks at CDC_MAX (1 MiB): 3.5 MiB is at least 4 chunks,
    // plus the manifest block.
    let min_files = (len / CDC_MAX) + 2;
    let files = block_file_count(tmp.path());
    assert!(
        files >= min_files,
        "expected >= {min_files} chunk blocks + manifest, got {files}"
    );

    // get() reassembles and verifies plaintext_hash (a wrong hash must fail).
    let back = store.get(&keys, &blob).unwrap();
    assert_eq!(back, bytes);
    let mut tampered = blob.clone();
    tampered.plaintext_hash[0] ^= 1;
    assert!(store.get(&keys, &tampered).is_err());
}

#[test]
fn dedup_same_plaintext_is_one_set_of_blocks() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlockStore::open(tmp.path()).unwrap();
    let keys = keysource();
    let bytes = seeded_bytes(2 * 1024 * 1024, "dedup blob");

    let first = store.put(&keys, &bytes, Some("application/pdf")).unwrap();
    let files_after_first = block_file_count(tmp.path());

    let second = store.put(&keys, &bytes, Some("application/pdf")).unwrap();
    let files_after_second = block_file_count(tmp.path());

    // Identical BlobRef (put is a pure function of master+bytes+mime) and
    // not a single new block file.
    assert_eq!(first, second);
    assert_eq!(first.manifest_hash, second.manifest_hash);
    assert_eq!(files_after_first, files_after_second);
}

#[test]
fn shred_removes_blocks_and_leaves_neighbors_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let store = BlockStore::open(tmp.path()).unwrap();
    let keys = keysource();
    let doomed = seeded_bytes(1024 * 1024, "doomed blob");
    let survivor = seeded_bytes(4096, "survivor blob");

    let doomed_ref = store.put(&keys, &doomed, None).unwrap();
    let survivor_ref = store.put(&keys, &survivor, None).unwrap();
    let survivor_files = 2; // one chunk + one manifest
    assert!(block_file_count(tmp.path()) > survivor_files);

    store.delete(&keys, &doomed_ref).unwrap();
    assert!(!store.has(&doomed_ref));
    assert!(store.get(&keys, &doomed_ref).is_err());
    // Every one of the doomed blob's block files is gone…
    assert_eq!(block_file_count(tmp.path()), survivor_files);
    // …the neighbor is untouched…
    assert_eq!(store.get(&keys, &survivor_ref).unwrap(), survivor);
    // …and delete is idempotent.
    store.delete(&keys, &doomed_ref).unwrap();
}
