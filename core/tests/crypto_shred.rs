// Crypto-shred end to end (PLAN.md PR 1.8; D19): an attachment blob goes in
// through the store's real byte pipeline, gets shredded through the redact
// path, and must be UNRECOVERABLE afterwards — from the live index, from the
// blockstore bytes on disk, and from a fresh index rebuilt by replaying the
// op log (the tombstone/redact records apply; nothing resurrects).
//
// What "shredded" means here, asserted piece by piece:
//   - the blob's blocks are gone from <data_dir>/blocks (belt), and the one
//     wrapped copy of its content key — the blob_refs row — is gone
//     (suspenders: without the wrapped key, surviving block copies are
//     noise, which is the load-bearing half of D19);
//   - FTS rows gone, vector rows gone (retrieval can never resurface it);
//   - the tombstone + redact records are IN the log, and a full replay
//     (index deleted, reopened) lands in the shredded state with zero
//     plaintext trace in the canonical dump.
//
// Hermetic: tempdir + MemoryKeySource + hash embedder; the store is opened
// directly (not via common::test_store) because the reopen path is the test.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use hive_core::blockstore::{BlobRef, BlockStore};
use hive_core::keys::MemoryKeySource;
use hive_core::oplog::{kind, LogReader};
use hive_core::store::mail::{MailIngestAttachment, MailIngestMessage};
use hive_core::store::Store;

const MASTER: [u8; 32] = [7u8; 32];
const SECRET_BYTES: &[u8] = b"TOP-SECRET waggle dance coordinates: 51.5007N 0.1246W";

fn keys() -> Arc<MemoryKeySource> {
    Arc::new(MemoryKeySource(MASTER))
}

fn open(dir: &Path) -> Store {
    Store::new(dir, keys(), Arc::new(hive_embed::HashEmbedder)).expect("open store")
}

async fn count(store: &Store, sql: &str, params: Vec<serde_json::Value>) -> i64 {
    store.raw_sql(sql, params).await.expect("count sql")[0][0]
        .as_i64()
        .expect("count")
}

async fn text_of(store: &Store, sql: &str, params: Vec<serde_json::Value>) -> Option<String> {
    let rows = store.raw_sql(sql, params).await.expect("text sql");
    rows.first().and_then(|r| r[0].as_str()).map(str::to_string)
}

/// Every regular file under <data_dir>/blocks (the encrypted block store).
fn block_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let root = dir.join("blocks");
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

#[tokio::test]
async fn shredded_attachment_is_unrecoverable_everywhere_including_replay() {
    std::env::set_var("HIVE_CRED_KEY", "crypto-shred-test-key");
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());

    // ── in: account + message + attachment bytes, all through record paths ──
    let account = store
        .mail_account_create(
            "alice",
            "alice@example.test",
            "https://mail.example.test",
            None,
            "jmap-acc-1",
            "hunter2",
        )
        .await
        .unwrap();
    let ingest: HashSet<String> = ["inbox".to_string()].into();
    let msg = MailIngestMessage {
        jmap_id: "j-shred".into(),
        thread_id: "t-shred".into(),
        message_id_hdr: Some("<shred@example.test>".into()),
        in_reply_to: None,
        references_json: "[]".into(),
        from_addr: "sender@example.test".into(),
        from_name: Some("Sender".into()),
        to_json: "[]".into(),
        cc_json: "[]".into(),
        reply_to_json: "[]".into(),
        subject: "waggle dance map enclosed".into(),
        sent_at: None,
        received_at: "2026-07-09T12:00:00.000Z".into(),
        mailbox_ids: vec!["inbox".into()],
        mailbox_ids_json: "[\"inbox\"]".into(),
        keywords: Vec::new(),
        keywords_json: "{}".into(),
        body_text: "the waggle dance coordinates are attached".into(),
        body_source: "plain".into(),
        snippet: "waggle".into(),
        size: 100,
        has_attachments: true,
        attachments: vec![MailIngestAttachment {
            jmap_blob_id: "blob-shred".into(),
            filename: "waggle-map.pdf".into(),
            mime: "application/pdf".into(),
            size: SECRET_BYTES.len() as i64,
            content_id: None,
            disposition: Some("attachment".into()),
        }],
        parse_error: None,
    };
    store
        .mail_ingest_batch(&account.id, "alice", &ingest, &HashSet::new(), vec![msg])
        .await
        .unwrap();
    let mail_id = text_of(
        &store,
        "SELECT id FROM mail_messages WHERE jmap_id = 'j-shred'",
        vec![],
    )
    .await
    .unwrap();
    let att_id = text_of(
        &store,
        "SELECT id FROM mail_attachments WHERE message_id = ?",
        vec![mail_id.clone().into()],
    )
    .await
    .unwrap();
    let hash = blake3::hash(SECRET_BYTES).to_hex().to_string();
    store
        .mail_attachment_store_blob(&att_id, &hash, "application/pdf", SECRET_BYTES)
        .await
        .unwrap();
    // A vector row for the message (the shred must drop it with everything
    // else; seeded via the raw test seam like the ported mail suite does).
    store
        .upsert_embedding_raw("mail", &mail_id, 0, "hash", None, vec![0.5; 4], "h")
        .await
        .unwrap();

    // ── retrievable before the shred ────────────────────────────────────────
    let served = store.mail_attachment_serve(&att_id).await.unwrap().unwrap();
    assert_eq!(served.data.as_deref(), Some(SECRET_BYTES));
    assert_eq!(store.mail_search("waggle", 10).await.unwrap().len(), 1);
    assert!(!block_files(dir.path()).is_empty(), "blocks on disk");
    // Capture the BlobRef (the wrapped content key) exactly as blob_refs
    // holds it, to prove decryption works now and fails after.
    let ref_hex = text_of(
        &store,
        "SELECT ref FROM blob_refs WHERE hash = ?",
        vec![hash.clone().into()],
    )
    .await
    .expect("blob_refs row");
    let blob: BlobRef = ciborium::from_reader(
        data_encoding::HEXLOWER
            .decode(ref_hex.as_bytes())
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    let blocks = BlockStore::open(dir.path()).unwrap();
    assert_eq!(
        blocks.get(keys().as_ref(), &blob).unwrap(),
        SECRET_BYTES.to_vec(),
        "sanity: the captured BlobRef decrypts the blob today"
    );

    // ── shred ───────────────────────────────────────────────────────────────
    let owner = store.mail_message_redact(&mail_id).await.unwrap();
    assert_eq!(owner.as_deref(), Some("alice"));

    // Bytes unrecoverable: blocks deleted, so even the captured wrapped key
    // opens nothing…
    assert!(
        blocks.get(keys().as_ref(), &blob).is_err(),
        "blob get must fail after the shred"
    );
    assert!(
        block_files(dir.path()).is_empty(),
        "no block files may remain"
    );
    // …and the stored wrapped-key copy is destroyed (the load-bearing half).
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM blob_refs", vec![]).await,
        0,
        "the wrapped content key must be destroyed"
    );
    // FTS and vectors gone; the serving path finds nothing.
    assert_eq!(store.mail_search("waggle", 10).await.unwrap().len(), 0);
    for (what, sql) in [
        (
            "search",
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND ref_id = ?",
        ),
        (
            "embeddings",
            "SELECT COUNT(*) FROM embeddings WHERE ref_kind = 'mail' AND ref_id = ?",
        ),
        (
            "attachments",
            "SELECT COUNT(*) FROM mail_attachments WHERE message_id = ?",
        ),
    ] {
        assert_eq!(
            count(&store, sql, vec![mail_id.clone().into()]).await,
            0,
            "{what} rows survived the shred"
        );
    }
    let device = store.device().to_string();
    store.shutdown().await.unwrap();

    // ── the log tells the story: tombstone + redact records present ─────────
    let ks = keys();
    let kinds: Vec<String> = LogReader::scan(dir.path(), &device, ks.as_ref())
        .unwrap()
        .map(|item| item.unwrap().0.kind)
        .collect();
    assert!(
        kinds.iter().any(|k| k == kind::TOMBSTONE),
        "tombstone record must be in the log: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == kind::REDACT),
        "redact record must be in the log: {kinds:?}"
    );

    // ── fresh replay: delete the index, reopen, still shredded ──────────────
    std::fs::remove_file(dir.path().join("index.db")).unwrap();
    let _ = std::fs::remove_file(dir.path().join("index.db-wal"));
    let _ = std::fs::remove_file(dir.path().join("index.db-shm"));
    let store = open(dir.path());
    let dump = store.canonical_dump().await.unwrap();
    for trace in ["waggle", "TOP-SECRET", "51.5007"] {
        assert!(
            !dump.contains(trace),
            "replayed canonical state leaks {trace:?}"
        );
    }
    // The tombstoned, redacted row is what replay produces — not the content.
    let row = store
        .raw_sql(
            "SELECT subject, body_text, deleted_at FROM mail_messages WHERE id = ?",
            vec![mail_id.clone().into()],
        )
        .await
        .unwrap();
    assert_eq!(row[0][0].as_str(), Some("[redacted]"));
    assert_eq!(row[0][1].as_str(), Some(""));
    assert!(row[0][2].as_str().is_some(), "tombstone survives replay");
    assert_eq!(store.mail_search("waggle", 10).await.unwrap().len(), 0);
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM blob_refs", vec![]).await,
        0,
        "replay must not resurrect a wrapped key"
    );
    assert!(
        blocks.get(keys().as_ref(), &blob).is_err(),
        "blob stays unrecoverable after replay"
    );
    store.shutdown().await.unwrap();
}
