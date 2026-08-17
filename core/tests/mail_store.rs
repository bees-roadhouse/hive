// Mail store coverage that the in-src suite does not carry.
//
// core/src/store/mail.rs has its own `#[cfg(test)] mod tests` block, and that
// block is the live descendant of the old core/tests/mail_store.rs: 13 of that
// file's 19 cases have exact-name twins there, in better shape (the reader
// tests are viewer-gated now, which the old file predates). Restoring the
// whole file would have duplicated them.
//
// What was genuinely lost is here. Each case below drives a `mail.rs` function
// that no other test in the repo calls:
//
//   - mail_cursor_save / mail_cursor_load  (the sync cursor round-trip)
//   - mail_attachment_serve                (bytes back out of the blob store)
//   - mail_accounts_due + mail_account_mark_ok (the due-scan gate)
//
// Dropped rather than contorted, because the mechanism is gone, not the test:
//
//   - mailbox_list_counts_and_membership: asserted MailMailboxView.total and
//     .unread, which the struct no longer has, and mail_messages_by_mailbox,
//     which exists nowhere in the tree.
//   - mail_fts_rebuilds_from_replay: a documenting pin on rebuild-by-replay.
//     There is no op-log to replay.

use hive_core::db;
use hive_core::store::Store;

const NOW: &str = "2026-07-05T00:00:00Z";

/// Same constant the in-src suite uses. set_var is process-global but
/// idempotent here.
async fn test_store() -> (Store, db::TestDb) {
    std::env::set_var("HIVE_CRED_KEY", "mail-store-test-key");
    let test_db = db::test_pool().await;
    let store = Store::new(test_db.pool.clone());
    (store, test_db)
}

/// The cursor round-trip: what mail_cursor_save persists, mail_cursor_load
/// returns. Covers the in-progress backfill anchor and both JMAP state
/// strings, which is the whole cursor (DIRECTION.md D5).
#[tokio::test]
async fn cursor_roundtrips_through_the_store() {
    let (store, _test_db) = test_store().await;
    let view = store
        .mail_account_create(
            "alice",
            "cursor@example.test",
            "https://mail.example.test",
            None,
            "acc-cur",
            "pw",
        )
        .await
        .unwrap();

    // A fresh account: no email/mailbox state, backfill pending, no anchor.
    let (email, mailbox, status, cursor) = store.mail_cursor_load(&view.id).await.unwrap();
    assert_eq!(email, None);
    assert_eq!(mailbox, None);
    assert_eq!(status, "pending");
    assert!(cursor.is_none());

    // The shape written mid-backfill: both state strings plus the resume
    // anchor.
    let anchor = serde_json::json!({
        "phase": "in_progress",
        "received_at": "2026-07-04T00:00:00.000Z",
        "jmap_id": "j-anchor"
    });
    store
        .mail_cursor_save(
            &view.id,
            Some("email-state-1"),
            Some("mailbox-state-1"),
            "in_progress",
            Some(&anchor),
        )
        .await
        .unwrap();

    let (email, mailbox, status, cursor) = store.mail_cursor_load(&view.id).await.unwrap();
    assert_eq!(email.as_deref(), Some("email-state-1"));
    assert_eq!(mailbox.as_deref(), Some("mailbox-state-1"));
    assert_eq!(status, "in_progress");
    assert_eq!(cursor, Some(anchor));

    // Completing the backfill clears the anchor.
    store
        .mail_cursor_save(&view.id, Some("email-state-2"), None, "complete", None)
        .await
        .unwrap();
    let (email, _mailbox, status, cursor) = store.mail_cursor_load(&view.id).await.unwrap();
    assert_eq!(email.as_deref(), Some("email-state-2"));
    assert_eq!(status, "complete");
    assert!(cursor.is_none(), "a complete cursor carries no anchor");
}

/// Stored attachment bytes come back out with the owning namespace attached,
/// which is what the serving route gates on. Two attachments sharing one hash
/// share one blob row, and both serve the same bytes.
#[tokio::test]
async fn attachment_serve_returns_the_stored_bytes() {
    let (store, _test_db) = test_store().await;

    hive_core::pgq::query(
        "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind("acct-alice")
    .bind("alice")
    .bind("alice@example.test")
    .bind(NOW)
    .bind(NOW)
    .execute(store.db())
    .await
    .unwrap();
    hive_core::pgq::query(
        "INSERT INTO mail_messages (id, account_id, user_scope, jmap_thread_id, jmap_id, \
         subject, from_addr, to_json, cc_json, received_at, snippet, body_text, \
         has_attachments, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind("msg-alice-1")
    .bind("acct-alice")
    .bind("alice")
    .bind("thread-shared")
    .bind("jmap-alice-1")
    .bind("Quarterly bees")
    .bind("ops@example.test")
    .bind("[]")
    .bind("[]")
    .bind("2026-07-04T12:00:00Z")
    .bind("nectar budget")
    .bind("The nectar budget has fictional hive details.")
    .bind(true)
    .bind(NOW)
    .bind(NOW)
    .execute(store.db())
    .await
    .unwrap();
    for (att, blob) in [("att-d1", "b1"), ("att-d2", "b2")] {
        hive_core::pgq::query(
            "INSERT INTO mail_attachments (id, message_id, jmap_blob_id, filename, mime, created_at) \
             VALUES (?, 'msg-alice-1', ?, ?, ?, ?)",
        )
        .bind(att)
        .bind(blob)
        .bind("invoice.pdf")
        .bind("application/pdf")
        .bind(NOW)
        .execute(store.db())
        .await
        .unwrap();
    }

    // The same bytes on two messages, e.g. one PDF sent twice. blake3 lives
    // with the sync driver, so the hash here is a fixed stand-in ... the store
    // treats hashes as opaque content addresses.
    let bytes = b"identical attachment bytes";
    let hash = "0f".repeat(32);
    for att in ["att-d1", "att-d2"] {
        store
            .mail_attachment_store_blob(att, &hash, "application/pdf", bytes)
            .await
            .unwrap();
    }

    let blobs = hive_core::pgq::query_scalar::<i64>("SELECT COUNT(*) FROM blobs WHERE hash = ?")
        .bind(&hash)
        .fetch_one(store.db())
        .await
        .unwrap();
    assert_eq!(blobs, 1, "identical bytes share one blob row");

    let served = store
        .mail_attachment_serve("att-d1")
        .await
        .unwrap()
        .expect("att-d1 serves");
    assert_eq!(served.data.as_deref(), Some(bytes.as_slice()));
    assert_eq!(served.user_scope, "alice", "serving gates on the namespace");
    assert_eq!(served.filename, "invoice.pdf");
    // The type DECLARED on the attachment row, which is what ingest recorded.
    // store_blob's mime lands on the blob and serving never reads it back, so
    // seeding the row without one serves application/octet-stream.
    assert_eq!(served.mime, "application/pdf");
    assert_eq!(served.blob_hash.as_deref(), Some(hash.as_str()));

    // The second attachment resolves the same bytes through the shared blob.
    let also = store
        .mail_attachment_serve("att-d2")
        .await
        .unwrap()
        .expect("att-d2 serves");
    assert_eq!(also.data.as_deref(), Some(bytes.as_slice()));

    // An unknown attachment is absent, not an error.
    assert!(store
        .mail_attachment_serve("att-missing")
        .await
        .unwrap()
        .is_none());
}

async fn is_due(store: &Store, id: &str) -> bool {
    store
        .mail_accounts_due()
        .await
        .unwrap()
        .iter()
        .any(|a| a.id == id)
}

/// The due-scan honors enabled + the per-account backoff window: a disabled
/// account and one whose next_attempt_at is still in the future are both
/// skipped, and mail_account_mark_ok clears the window so it comes due again.
#[tokio::test]
async fn due_scan_honors_enabled_and_the_backoff_window() {
    let (store, _test_db) = test_store().await;
    let view = store
        .mail_account_create(
            "alice",
            "due@example.test",
            "https://mail.example.test",
            None,
            "acc-due",
            "pw",
        )
        .await
        .unwrap();

    // Fresh + enabled: due immediately, next_attempt_at is NULL.
    assert!(
        is_due(&store, &view.id).await,
        "a fresh enabled account is due"
    );

    // Disabled: never due, even with the window clear.
    assert!(store
        .mail_account_set_enabled(&view.id, false)
        .await
        .unwrap());
    assert!(
        !is_due(&store, &view.id).await,
        "a disabled account is skipped"
    );

    // Re-enabled, but inside its backoff window: still skipped.
    assert!(store
        .mail_account_set_enabled(&view.id, true)
        .await
        .unwrap());
    hive_core::pgq::query("UPDATE mail_accounts SET next_attempt_at = ? WHERE id = ?")
        .bind("2099-01-01T00:00:00.000Z")
        .bind(&view.id)
        .execute(store.db())
        .await
        .unwrap();
    assert!(
        !is_due(&store, &view.id).await,
        "an account inside its backoff window is not due"
    );

    // A successful sync clears the window, so the account comes due again
    // without waiting it out.
    store.mail_account_mark_ok(&view.id).await.unwrap();
    assert!(
        is_due(&store, &view.id).await,
        "mark_ok clears the backoff window"
    );
    let admin = store
        .mail_account_admin_view(&view.id)
        .await
        .unwrap()
        .expect("account still there");
    assert_eq!(admin.attempts, 0);
    assert_eq!(admin.last_status.as_deref(), Some("ok"));
    assert!(admin.last_error.is_none());
}
