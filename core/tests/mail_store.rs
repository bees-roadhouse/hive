// Mail store invariants — the store/mail.rs in-src suite from the Postgres
// era, ported to the record-based cutover paths (the write surface is paused
// with the daemon, but every invariant must hold: replay-metadata-only,
// tombstone stickiness, redaction durability, the ingest gate, the byte
// pipeline against the blockstore, the actor cascade). Seeding that used raw
// SQL rides the raw_sql diagnostics seam or the real record paths.

mod common;

use hive_core::store::mail::{MailIngestAttachment, MailIngestMessage};
use hive_core::store::Store;

const NOW: &str = "2026-07-05T00:00:00.000Z";

async fn exec(store: &Store, sql: &str, params: Vec<serde_json::Value>) {
    store.raw_sql(sql, params).await.expect("seed sql");
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

/// Two accounts, one ingest-enabled inbox for alice, one message each — the
/// seeded_store() shape of the old in-src suite.
async fn seeded_store() -> Store {
    let store = common::test_store().await;
    for (acct, owner) in [("acct-alice", "alice"), ("acct-bob", "bob")] {
        exec(
            &store,
            "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            vec![
                acct.into(),
                owner.into(),
                format!("{owner}@example.test").into(),
                NOW.into(),
                NOW.into(),
            ],
        )
        .await;
    }
    exec(
        &store,
        "INSERT INTO mail_mailboxes (id, account_id, jmap_id, name, role, sort_order) VALUES (?, ?, ?, ?, ?, ?)",
        vec![
            "mbox-alice-inbox".into(),
            "acct-alice".into(),
            "inbox".into(),
            "Inbox".into(),
            "inbox".into(),
            0.into(),
        ],
    )
    .await;
    exec(
        &store,
        "INSERT INTO mail_messages (id, account_id, user_scope, jmap_thread_id, jmap_id, message_id_hdr, subject, from_name, from_addr, to_json, cc_json, received_at, keywords_json, snippet, body_text, has_attachments, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            "msg-alice-1".into(),
            "acct-alice".into(),
            "alice".into(),
            "thread-shared".into(),
            "jmap-alice-1".into(),
            "<alice-1@example.test>".into(),
            "Quarterly bees".into(),
            "Bee Ops".into(),
            "ops@example.test".into(),
            r#"[{"email":"alice@example.test"}]"#.into(),
            "[]".into(),
            "2026-07-04T12:00:00.000Z".into(),
            r##"{"$flagged":true,"Bee Roadhouse":true,"$seen":true}"##.into(),
            "nectar budget".into(),
            "The nectar budget has fictional hive details.".into(),
            false.into(),
            NOW.into(),
            NOW.into(),
        ],
    )
    .await;
    exec(
        &store,
        "INSERT INTO mail_messages (id, account_id, user_scope, jmap_thread_id, jmap_id, message_id_hdr, subject, from_name, from_addr, to_json, cc_json, received_at, snippet, body_text, has_attachments, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            "msg-bob-1".into(),
            "acct-bob".into(),
            "bob".into(),
            "thread-shared".into(),
            "jmap-bob-1".into(),
            "<bob-1@example.test>".into(),
            "Private swarm".into(),
            "Bob Ops".into(),
            "bobops@example.test".into(),
            r#"[{"email":"bob@example.test"}]"#.into(),
            "[]".into(),
            "2026-07-04T13:00:00.000Z".into(),
            "wax budget".into(),
            "The wax budget must stay in Bob's namespace.".into(),
            false.into(),
            NOW.into(),
            NOW.into(),
        ],
    )
    .await;
    store
}

#[tokio::test]
async fn mail_queries_read_unscoped() {
    let store = seeded_store().await;

    let accounts = store.mail_accounts_list().await.unwrap();
    assert_eq!(accounts.len(), 2, "single user sees every account");

    let messages = store.mail_messages_list(None, None, 20).await.unwrap();
    assert_eq!(messages.len(), 2);
    let alice = messages.iter().find(|m| m.id == "msg-alice-1").unwrap();
    assert_eq!(
        alice.labels,
        vec![
            "flagged".to_string(),
            "Bee Roadhouse".to_string(),
            "seen".to_string(),
        ]
    );

    let hits = store.mail_search("nectar budget", 20).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "msg-alice-1");

    let thread = store.mail_thread_get("thread-shared").await.unwrap();
    assert_eq!(thread.messages.len(), 2, "whole thread, no viewer gate");
}

#[tokio::test]
async fn account_lifecycle_create_toggle_resync_delete() {
    // Same constant every test uses; set_var is process-global but
    // idempotent here.
    std::env::set_var("HIVE_CRED_KEY", "mail-store-test-key");
    let store = common::test_store().await;

    let view = store
        .mail_account_create(
            "alice",
            "alice@example.test",
            "https://mail.example.test",
            Some("alice-login"),
            "jmap-acc-1",
            "hunter2",
        )
        .await
        .unwrap();
    assert_eq!(view.backfill_status, "pending");
    assert!(view.enabled);
    assert_eq!(view.jmap_account_id, "jmap-acc-1");

    // The credential landed in the vault, named by cred_id, and decrypts.
    let cred_id = text_of(
        &store,
        "SELECT cred_id FROM mail_accounts WHERE id = ?",
        vec![view.id.clone().into()],
    )
    .await
    .expect("cred_id");
    let secret = store.cc_cred_decrypt_by_id(&cred_id).await.unwrap();
    assert_eq!(secret.as_deref(), Some("hunter2"));

    // A second connect for the same owner+address refuses.
    assert!(store
        .mail_account_create(
            "alice",
            "alice@example.test",
            "https://mail.example.test",
            None,
            "jmap-acc-1",
            "hunter2",
        )
        .await
        .is_err());

    // Re-enabling clears the backoff bookkeeping.
    exec(
        &store,
        "UPDATE mail_accounts SET attempts = 5, next_attempt_at = '2099-01-01T00:00:00.000Z' WHERE id = ?",
        vec![view.id.clone().into()],
    )
    .await;
    assert!(store
        .mail_account_set_enabled(&view.id, true)
        .await
        .unwrap());
    assert_eq!(
        count(
            &store,
            "SELECT attempts FROM mail_accounts WHERE id = ?",
            vec![view.id.clone().into()]
        )
        .await,
        0
    );

    // Force-resync plants the sentinel that routes the next delta into
    // reconciliation.
    assert!(store.mail_account_force_resync(&view.id).await.unwrap());
    assert_eq!(
        text_of(
            &store,
            "SELECT email_state FROM mail_accounts WHERE id = ?",
            vec![view.id.clone().into()]
        )
        .await
        .as_deref(),
        Some("force-resync")
    );

    // Seed a message plus derived rows through the real paths, then delete
    // the account and assert nothing survives — including the vault row.
    exec(
        &store,
        "INSERT INTO mail_messages (id, account_id, user_scope, jmap_thread_id, jmap_id, subject, from_addr, to_json, cc_json, received_at, snippet, body_text, has_attachments, created_at, updated_at) \
         VALUES ('msg-cascade-1', ?, 'alice', 't1', 'j1', 's', 'a@b.test', '[]', '[]', ?, '', 'body', 1, ?, ?)",
        vec![view.id.clone().into(), NOW.into(), NOW.into(), NOW.into()],
    )
    .await;
    store
        .index_entity("mail", "msg-cascade-1", "s", "body", &[])
        .await
        .unwrap();

    assert!(store.mail_account_delete(&view.id).await.unwrap());
    for (what, sql) in [
        ("account", "SELECT COUNT(*) FROM mail_accounts WHERE id = ?"),
        (
            "messages",
            "SELECT COUNT(*) FROM mail_messages WHERE account_id = ?",
        ),
        (
            "search",
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND ref_id = 'msg-cascade-1' AND ? = ?",
        ),
    ] {
        let mut params: Vec<serde_json::Value> = vec![view.id.clone().into()];
        if sql.contains("? = ?") {
            params.push(view.id.clone().into());
        }
        assert_eq!(count(&store, sql, params).await, 0, "{what} survived");
    }
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM cc_credentials WHERE id = ?",
            vec![cred_id.into()]
        )
        .await,
        0,
        "vault credential survived the cascade"
    );
}

fn ingest_msg(jmap_id: &str, mailbox: &str, keywords: &[&str]) -> MailIngestMessage {
    MailIngestMessage {
        jmap_id: jmap_id.to_string(),
        thread_id: format!("t-{jmap_id}"),
        message_id_hdr: Some(format!("<{jmap_id}@example.test>")),
        in_reply_to: None,
        references_json: "[]".into(),
        from_addr: "sender@example.test".into(),
        from_name: Some("Sender".into()),
        to_json: "[]".into(),
        cc_json: "[]".into(),
        reply_to_json: "[]".into(),
        subject: format!("subject {jmap_id}"),
        sent_at: None,
        received_at: "2026-07-09T12:00:00.000Z".into(),
        mailbox_ids: vec![mailbox.to_string()],
        mailbox_ids_json: format!("[\"{mailbox}\"]"),
        keywords: keywords.iter().map(|s| s.to_string()).collect(),
        keywords_json: "{}".into(),
        body_text: format!("body of {jmap_id} with honeycomb"),
        body_source: "plain".into(),
        snippet: "snippet".into(),
        size: 100,
        has_attachments: false,
        attachments: Vec::new(),
        parse_error: None,
    }
}

fn ingest_att(blob_id: &str, filename: &str, size: i64) -> MailIngestAttachment {
    MailIngestAttachment {
        jmap_blob_id: blob_id.to_string(),
        filename: filename.to_string(),
        mime: "application/pdf".into(),
        size,
        content_id: None,
        disposition: Some("attachment".into()),
    }
}

#[tokio::test]
async fn mailbox_ingest_toggle_gates_retrieval() {
    let store = seeded_store().await;
    // Give alice's message mailbox membership + a search row, as the
    // sink would have.
    exec(
        &store,
        "UPDATE mail_messages SET mailbox_ids_json = '[\"inbox\"]', embed_state = 'pending' WHERE id = 'msg-alice-1'",
        vec![],
    )
    .await;
    store
        .index_entity(
            "mail",
            "msg-alice-1",
            "Quarterly bees",
            "nectar budget",
            &[],
        )
        .await
        .unwrap();

    // OFF: the mailbox's messages leave retrieval (search + embed queue)
    // but the rows stay (D6).
    assert!(store
        .mail_mailbox_set_ingest("mbox-alice-inbox", false)
        .await
        .unwrap());
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND ref_id = 'msg-alice-1'",
            vec![]
        )
        .await,
        0
    );
    assert_eq!(
        text_of(
            &store,
            "SELECT embed_state FROM mail_messages WHERE id = 'msg-alice-1'",
            vec![]
        )
        .await
        .as_deref(),
        Some("skip")
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM mail_messages WHERE id = 'msg-alice-1'",
            vec![]
        )
        .await,
        1
    );

    // ON: the account's backfill re-arms so history gets picked up.
    assert!(store
        .mail_mailbox_set_ingest("mbox-alice-inbox", true)
        .await
        .unwrap());
    assert_eq!(
        text_of(
            &store,
            "SELECT backfill_status FROM mail_accounts WHERE id = 'acct-alice'",
            vec![]
        )
        .await
        .as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn ingest_batch_is_idempotent_and_metadata_only_on_replay() {
    let store = seeded_store().await;
    exec(
        &store,
        "UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'",
        vec![],
    )
    .await;
    let (ingest, inbox) = store.mail_mailbox_sets("acct-alice").await.unwrap();
    assert!(ingest.contains("inbox") && inbox.contains("inbox"));

    let out = store
        .mail_ingest_batch(
            "acct-alice",
            "alice",
            &ingest,
            &inbox,
            vec![ingest_msg("j-new-1", "inbox", &[])],
        )
        .await
        .unwrap();
    assert_eq!(out.stored, 1);
    assert_eq!(out.notify.len(), 1, "new inbox-role message notifies");

    // FTS row exists.
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND title = 'subject j-new-1'",
            vec![]
        )
        .await,
        1
    );

    // Replay with changed metadata AND a (hostile) changed body: metadata
    // applies, the body must NOT rewrite — that invariant is what makes
    // admin redaction durable.
    let mut replay = ingest_msg("j-new-1", "inbox", &["$seen"]);
    replay.keywords_json = r#"{"$seen":true}"#.into();
    replay.body_text = "REWRITTEN".into();
    let out2 = store
        .mail_ingest_batch("acct-alice", "alice", &ingest, &inbox, vec![replay])
        .await
        .unwrap();
    assert!(out2.notify.is_empty(), "replays never re-notify");
    let body = text_of(
        &store,
        "SELECT body_text FROM mail_messages WHERE account_id = 'acct-alice' AND jmap_id = 'j-new-1'",
        vec![],
    )
    .await
    .unwrap();
    assert!(body.contains("honeycomb"), "body is immutable on conflict");
    let kw = text_of(
        &store,
        "SELECT keywords_json FROM mail_messages WHERE account_id = 'acct-alice' AND jmap_id = 'j-new-1'",
        vec![],
    )
    .await
    .unwrap();
    assert!(kw.contains("$seen"), "metadata updates apply");

    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM mail_messages WHERE account_id = 'acct-alice' AND jmap_id = 'j-new-1'",
            vec![]
        )
        .await,
        1,
        "the existing-row probe absorbed the replay"
    );
}

#[tokio::test]
async fn ingest_gates_junk_and_non_ingest_mailboxes() {
    let store = seeded_store().await;
    exec(
        &store,
        "UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'",
        vec![],
    )
    .await;
    let (ingest, inbox) = store.mail_mailbox_sets("acct-alice").await.unwrap();

    let out = store
        .mail_ingest_batch(
            "acct-alice",
            "alice",
            &ingest,
            &inbox,
            vec![
                ingest_msg("j-junk", "inbox", &["$junk"]),
                ingest_msg("j-elsewhere", "archive-box", &[]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(out.stored, 2);
    assert!(out.notify.is_empty(), "junk + non-ingest never notify");
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND (title = 'subject j-junk' OR title = 'subject j-elsewhere')",
            vec![]
        )
        .await,
        0,
        "neither row is searchable"
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM mail_messages WHERE jmap_id IN ('j-junk', 'j-elsewhere') AND embed_state = 'skip'",
            vec![]
        )
        .await,
        2
    );
}

#[tokio::test]
async fn tombstone_removes_retrieval_in_the_same_batch_and_stays_dead() {
    let store = seeded_store().await;
    exec(
        &store,
        "UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'",
        vec![],
    )
    .await;
    let (ingest, inbox) = store.mail_mailbox_sets("acct-alice").await.unwrap();
    store
        .mail_ingest_batch(
            "acct-alice",
            "alice",
            &ingest,
            &inbox,
            vec![ingest_msg("j-dead", "inbox", &[])],
        )
        .await
        .unwrap();

    let n = store
        .mail_tombstone_batch("acct-alice", &["j-dead".to_string()])
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND title = 'subject j-dead'",
            vec![]
        )
        .await,
        0,
        "deleted mail leaves search in the same batch"
    );

    // A cannotCalculateChanges replay re-upserts the id; the tombstone
    // must hold (no search resurrection).
    store
        .mail_ingest_batch(
            "acct-alice",
            "alice",
            &ingest,
            &inbox,
            vec![ingest_msg("j-dead", "inbox", &[])],
        )
        .await
        .unwrap();
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND title = 'subject j-dead'",
            vec![]
        )
        .await,
        0,
        "replay must not resurrect a tombstoned row"
    );
    // known_jmap_ids still reports it, so reconcile never re-fetches it.
    assert!(store
        .mail_known_jmap_ids("acct-alice")
        .await
        .unwrap()
        .contains("j-dead"));
}

#[tokio::test]
async fn mail_token_links_gate_by_scope_and_resolve_subjects() {
    let store = seeded_store().await;
    // seeded: msg-alice-1 (user_scope 'alice', subject "Quarterly bees").

    // Alice-scoped entry citing alice's mail → a 'cites' link + a subject chip.
    let entry = store
        .journal_append(
            hive_shared::NewJournalEntry {
                author: Some("alice".into()),
                body: "Following up on [mail:msg-alice-1] before Thursday.".into(),
                tags: None,
                anchors: None,
            },
            Some("alice"),
            Some("alice"),
        )
        .await
        .unwrap();
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM links WHERE source_kind = 'journal' AND source_id = ? \
             AND target_kind = 'mail' AND target_id = 'msg-alice-1' AND rel = 'cites'",
            vec![entry.entry.id.clone().into()]
        )
        .await,
        1
    );
    let refs = store.refs_for(&entry.entry.body).await.unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "Quarterly bees");
    assert_eq!(refs[0].id, "msg-alice-1");

    // Bob-scoped entry citing alice's mail → the token simply doesn't link.
    let cross = store
        .journal_append(
            hive_shared::NewJournalEntry {
                author: Some("bob".into()),
                body: "Trying to cite [mail:msg-alice-1] across namespaces.".into(),
                tags: None,
                anchors: None,
            },
            Some("bob"),
            Some("bob"),
        )
        .await
        .unwrap();
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM links WHERE source_id = ? AND target_kind = 'mail'",
            vec![cross.entry.id.clone().into()]
        )
        .await,
        0,
        "cross-namespace citation must not link"
    );

    // Tombstoned mail stops resolving (the raw token stays visible).
    exec(
        &store,
        "UPDATE mail_messages SET deleted_at = '2026-07-09T00:00:00.000Z' WHERE id = 'msg-alice-1'",
        vec![],
    )
    .await;
    let dead_refs = store.refs_for(&entry.entry.body).await.unwrap();
    assert!(dead_refs.is_empty(), "dead citations resolve to nothing");
}

#[tokio::test]
async fn backoff_disables_after_eight_failures() {
    std::env::set_var("HIVE_CRED_KEY", "mail-store-test-key");
    let store = common::test_store().await;
    let view = store
        .mail_account_create(
            "alice",
            "backoff@example.test",
            "https://mail.example.test",
            None,
            "acc-b",
            "pw",
        )
        .await
        .unwrap();
    for i in 1..=7 {
        let disabled = store
            .mail_account_mark_failed(&view.id, "connect refused")
            .await
            .unwrap();
        assert!(!disabled, "attempt {i} must only back off");
    }
    assert!(
        store
            .mail_account_mark_failed(&view.id, "connect refused")
            .await
            .unwrap(),
        "the 8th failure disables the account"
    );
    let due = store.mail_accounts_due().await.unwrap();
    assert!(
        !due.iter().any(|a| a.id == view.id),
        "disabled accounts never come due"
    );
}

#[tokio::test]
async fn ingest_writes_attachment_metadata_idempotently() {
    let store = seeded_store().await;
    exec(
        &store,
        "UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'",
        vec![],
    )
    .await;
    let (ingest, inbox) = store.mail_mailbox_sets("acct-alice").await.unwrap();

    let mut msg = ingest_msg("j-att-1", "inbox", &[]);
    msg.attachments = vec![
        ingest_att("blob-a", "report.pdf", 1000),
        ingest_att("blob-b", "photo.jpg", 2000),
    ];
    msg.has_attachments = true;
    store
        .mail_ingest_batch("acct-alice", "alice", &ingest, &inbox, vec![msg.clone()])
        .await
        .unwrap();

    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM mail_attachments WHERE blob_hash IS NULL AND skipped_reason IS NULL",
            vec![]
        )
        .await,
        2,
        "metadata rows land with bytes pending"
    );

    // Replay: the existing-row probe absorbs it — no duplicate rows.
    store
        .mail_ingest_batch("acct-alice", "alice", &ingest, &inbox, vec![msg])
        .await
        .unwrap();
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM mail_attachments", vec![]).await,
        2,
        "replay must not duplicate attachment rows"
    );
}

#[tokio::test]
async fn attachments_pending_excludes_skipped_stored_and_deleted() {
    let store = seeded_store().await;
    exec(
        &store,
        "UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'",
        vec![],
    )
    .await;
    let (ingest, inbox) = store.mail_mailbox_sets("acct-alice").await.unwrap();
    let mut msg = ingest_msg("j-pend", "inbox", &[]);
    msg.attachments = vec![
        ingest_att("blob-pending", "pending.pdf", 100),
        ingest_att("blob-oversize", "huge.iso", 100),
        ingest_att("blob-stored", "done.pdf", 100),
    ];
    store
        .mail_ingest_batch("acct-alice", "alice", &ingest, &inbox, vec![msg])
        .await
        .unwrap();

    let att_id = |blob: &str| {
        let store = store.clone();
        let blob = blob.to_string();
        async move {
            text_of(
                &store,
                "SELECT id FROM mail_attachments WHERE jmap_blob_id = ?",
                vec![blob.into()],
            )
            .await
            .unwrap()
        }
    };
    store
        .mail_attachment_mark_skipped(&att_id("blob-oversize").await, "oversize")
        .await
        .unwrap();
    store
        .mail_attachment_store_blob(&att_id("blob-stored").await, "hash-1", "text/plain", b"x")
        .await
        .unwrap();

    let pending = store
        .mail_attachments_pending("acct-alice", 50)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "skipped + stored rows leave the queue");
    assert_eq!(pending[0].jmap_blob_id, "blob-pending");

    // Tombstoning the message drops its attachments out of the queue too.
    store
        .mail_tombstone_batch("acct-alice", &["j-pend".to_string()])
        .await
        .unwrap();
    assert!(store
        .mail_attachments_pending("acct-alice", 50)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn attachment_store_blob_dedups_by_hash_and_serves_bytes() {
    let store = seeded_store().await;
    for (att, blob) in [("att-d1", "b1"), ("att-d2", "b2")] {
        exec(
            &store,
            "INSERT INTO mail_attachments (id, message_id, jmap_blob_id, created_at) \
             VALUES (?, 'msg-alice-1', ?, ?)",
            vec![att.into(), blob.into(), NOW.into()],
        )
        .await;
    }

    // Same bytes fetched twice (e.g. the same PDF on two messages).
    let bytes = b"identical attachment bytes";
    let hash = blake3::hash(bytes).to_hex().to_string();
    store
        .mail_attachment_store_blob("att-d1", &hash, "application/pdf", bytes)
        .await
        .unwrap();
    store
        .mail_attachment_store_blob("att-d2", &hash, "application/pdf", bytes)
        .await
        .unwrap();

    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM blob_refs", vec![]).await,
        1,
        "identical bytes share one blob pointer"
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM mail_attachments WHERE blob_hash = ?",
            vec![hash.clone().into()]
        )
        .await,
        2,
        "both attachments point at the shared blob"
    );

    // The thread payload reports them stored, and the bytes round-trip
    // through the blockstore.
    let thread = store.mail_thread_get("thread-shared").await.unwrap();
    let atts = &thread
        .messages
        .iter()
        .find(|m| m.summary.id == "msg-alice-1")
        .unwrap()
        .attachments;
    assert_eq!(atts.len(), 2);
    assert!(atts.iter().all(|a| a.stored));
    let served = store
        .mail_attachment_serve("att-d1")
        .await
        .unwrap()
        .expect("serve");
    assert_eq!(served.data.as_deref(), Some(bytes.as_slice()));
    assert_eq!(served.user_scope, "alice");
}

/// THE redaction invariant (plan A6): after mail_message_redact, a full
/// ingest replay of the same jmap_id (reconcile/delta metadata update)
/// must not resurrect body, subject, search, or attachment rows.
#[tokio::test]
async fn redact_scrubs_everything_and_replay_cannot_resurrect() {
    let store = seeded_store().await;
    exec(
        &store,
        "UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'",
        vec![],
    )
    .await;
    let (ingest, inbox) = store.mail_mailbox_sets("acct-alice").await.unwrap();
    let mut msg = ingest_msg("j-redact", "inbox", &[]);
    msg.attachments = vec![ingest_att("blob-r", "secret.pdf", 10)];
    msg.has_attachments = true;
    store
        .mail_ingest_batch("acct-alice", "alice", &ingest, &inbox, vec![msg.clone()])
        .await
        .unwrap();
    let mail_id = text_of(
        &store,
        "SELECT id FROM mail_messages WHERE jmap_id = 'j-redact'",
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
    store
        .mail_attachment_store_blob(&att_id, "redact-hash", "application/pdf", b"secret")
        .await
        .unwrap();
    store
        .upsert_embedding_raw("mail", &mail_id, 0, "hash", None, vec![0.0; 4], "h")
        .await
        .unwrap();

    let owner = store.mail_message_redact(&mail_id).await.unwrap();
    assert_eq!(owner.as_deref(), Some("alice"));

    let row = store
        .raw_sql(
            "SELECT body_text, snippet, subject, has_attachments, deleted_at, embed_state \
             FROM mail_messages WHERE id = ?",
            vec![mail_id.clone().into()],
        )
        .await
        .unwrap();
    assert_eq!(row[0][0].as_str(), Some(""));
    assert_eq!(row[0][1].as_str(), Some(""));
    assert_eq!(row[0][2].as_str(), Some("[redacted]"));
    assert_eq!(row[0][3].as_i64(), Some(0));
    assert!(row[0][4].as_str().is_some(), "tombstoned");
    assert_eq!(row[0][5].as_str(), Some("skip"));
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
            "{what} rows survived redaction"
        );
    }
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM blob_refs", vec![]).await,
        0,
        "orphaned blob survived redaction"
    );

    // The replay: same jmap_id, hostile body + attachments. The metadata-only
    // replay arm and the tombstone gate mean nothing comes back.
    let mut replay = msg;
    replay.body_text = "RESURRECTED BODY".into();
    replay.subject = "resurrected subject".into();
    store
        .mail_ingest_batch("acct-alice", "alice", &ingest, &inbox, vec![replay])
        .await
        .unwrap();
    let after = store
        .raw_sql(
            "SELECT body_text, subject, deleted_at FROM mail_messages WHERE id = ?",
            vec![mail_id.clone().into()],
        )
        .await
        .unwrap();
    assert_eq!(
        after[0][0].as_str(),
        Some(""),
        "replay must not restore the body"
    );
    assert_eq!(
        after[0][1].as_str(),
        Some("[redacted]"),
        "replay must not restore the subject"
    );
    assert!(
        after[0][2].as_str().is_some(),
        "replay must not clear the tombstone"
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND ref_id = ?",
            vec![mail_id.clone().into()]
        )
        .await,
        0,
        "replay must not re-index a redacted row"
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM mail_attachments WHERE message_id = ?",
            vec![mail_id.clone().into()]
        )
        .await,
        0,
        "replay must not re-queue redacted attachments"
    );
}

#[tokio::test]
async fn blob_gc_deletes_only_unreferenced_aged_blobs() {
    let store = seeded_store().await;
    // Three blobs through the real pipeline: one referenced, two orphaned
    // (one aged, one fresh). Age is blob_refs.created_at.
    exec(
        &store,
        "INSERT INTO mail_attachments (id, message_id, jmap_blob_id, created_at) \
         VALUES ('att-gc', 'msg-alice-1', 'bgc', ?)",
        vec![NOW.into()],
    )
    .await;
    store
        .mail_attachment_store_blob("att-gc", "gc-old-referenced", "text/plain", b"keep")
        .await
        .unwrap();
    // Orphaned pointers (their attachments are gone — e.g. a racing fetch
    // pipeline): seed blob_refs rows directly, one aged, one fresh. The `ref`
    // blob doesn't need to decode for the sweep to remove the pointer.
    for (hash, created) in [
        ("gc-old-orphan", "2020-01-01T00:00:00.000Z"),
        ("gc-fresh-orphan", "2099-01-01T00:00:00.000Z"),
    ] {
        exec(
            &store,
            "INSERT INTO blob_refs (hash, ref, size, mime, created_at) VALUES (?, x'00', 1, 'text/plain', ?)",
            vec![hash.into(), created.into()],
        )
        .await;
    }
    // Age the referenced pointer past the 24h grace window too — reference
    // counting, not age, must be what keeps it.
    exec(
        &store,
        "UPDATE blob_refs SET created_at = '2020-01-01T00:00:00.000Z' WHERE hash = 'gc-old-referenced'",
        vec![],
    )
    .await;

    let swept = store.mail_blobs_gc().await.unwrap();
    assert_eq!(swept, 1, "exactly the aged orphan goes");
    let left = store
        .raw_sql("SELECT hash FROM blob_refs ORDER BY hash", vec![])
        .await
        .unwrap();
    let hashes: Vec<&str> = left.iter().filter_map(|r| r[0].as_str()).collect();
    assert_eq!(
        hashes,
        vec!["gc-fresh-orphan", "gc-old-referenced"],
        "referenced + in-grace blobs survive"
    );
}

/// Actor delete must cascade the whole mail footprint: accounts, messages,
/// attachments, blob pointers, vault credentials, and every derived row
/// (search/embeddings/inbox/links) — and the preview must report the same
/// counts without deleting anything. (The store/actors.rs in-src test from
/// the Postgres era, on the record recipes.)
#[tokio::test]
async fn actor_remove_cascades_mail_with_zero_orphans() {
    std::env::set_var("HIVE_CRED_KEY", "actors-cascade-test-key");
    let store = common::test_store().await;

    let view = store
        .mail_account_create(
            "casc-alice",
            "casc@example.test",
            "https://mail.example.test",
            None,
            "jmap-casc",
            "hunter2",
        )
        .await
        .unwrap();
    let cred_id = text_of(
        &store,
        "SELECT cred_id FROM mail_accounts WHERE id = ?",
        vec![view.id.clone().into()],
    )
    .await
    .unwrap();
    exec(
        &store,
        "INSERT INTO mail_messages (id, account_id, user_scope, jmap_thread_id, jmap_id, subject, from_addr, to_json, cc_json, received_at, snippet, body_text, has_attachments, created_at, updated_at) \
         VALUES ('msg-casc', ?, 'casc-alice', 't1', 'j1', 'cascade subject', 'a@b.test', '[]', '[]', ?, '', 'cascade body', 1, ?, ?)",
        vec![view.id.clone().into(), NOW.into(), NOW.into(), NOW.into()],
    )
    .await;
    store
        .index_entity("mail", "msg-casc", "cascade subject", "cascade body", &[])
        .await
        .unwrap();
    store
        .upsert_embedding_raw("mail", "msg-casc", 0, "hash", None, vec![0.0; 4], "h")
        .await
        .unwrap();
    exec(
        &store,
        "INSERT INTO inbox (id, recipient, \"from\", reason, ref_kind, ref_id, snippet, created_at) \
         VALUES ('inb-casc', 'someone-else', 'mail-sync', 'mail', 'mail', 'msg-casc', 's', ?)",
        vec![NOW.into()],
    )
    .await;
    exec(
        &store,
        "INSERT INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at) \
         VALUES ('lnk-casc', 'journal', 'entry-x', 'mail', 'msg-casc', 'cites', ?)",
        vec![NOW.into()],
    )
    .await;
    exec(
        &store,
        "INSERT INTO mail_attachments (id, message_id, blob_hash, jmap_blob_id, created_at) \
         VALUES ('att-casc', 'msg-casc', 'blob-casc', 'b1', ?)",
        vec![NOW.into()],
    )
    .await;
    store
        .mail_attachment_store_blob("att-casc", "blob-casc", "text/plain", b"x")
        .await
        .unwrap();

    // Dry run first: counts flow, nothing deletes.
    let preview = store.actors_remove_preview("casc-alice").await.unwrap();
    assert!(preview.dry_run);
    assert_eq!(preview.mail_accounts, 1);
    assert_eq!(preview.mail_messages, 1);
    assert_eq!(preview.blobs, 1);
    assert_eq!(
        count(&store, "SELECT COUNT(*) FROM mail_accounts", vec![]).await,
        1,
        "preview must not delete"
    );

    let acc = store.actors_remove("casc-alice").await.unwrap();
    assert!(!acc.dry_run);
    assert_eq!(acc.mail_accounts, 1);
    assert_eq!(acc.mail_messages, 1);
    assert_eq!(acc.blobs, 1);

    for (what, sql) in [
        ("mail_accounts", "SELECT COUNT(*) FROM mail_accounts"),
        ("mail_messages", "SELECT COUNT(*) FROM mail_messages"),
        ("mail_attachments", "SELECT COUNT(*) FROM mail_attachments"),
        ("blob_refs", "SELECT COUNT(*) FROM blob_refs"),
        (
            "cc_credentials",
            "SELECT COUNT(*) FROM cc_credentials WHERE kind = 'password'",
        ),
        ("search", "SELECT COUNT(*) FROM search WHERE kind = 'mail'"),
        (
            "embeddings",
            "SELECT COUNT(*) FROM embeddings WHERE ref_kind = 'mail'",
        ),
        (
            "inbox",
            "SELECT COUNT(*) FROM inbox WHERE ref_kind = 'mail'",
        ),
        (
            "links",
            "SELECT COUNT(*) FROM links WHERE source_kind = 'mail' OR target_kind = 'mail'",
        ),
    ] {
        assert_eq!(
            count(&store, sql, vec![]).await,
            0,
            "{what} rows survived the actor cascade"
        );
    }
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM cc_credentials WHERE id = ?",
            vec![cred_id.into()]
        )
        .await,
        0,
        "vault credential survived the actor cascade"
    );
}

// ── mail sync DRIVER scheduling / cursor logic (Slice A) ─────────────────────
//
// These exercise the store-facing pieces the reconstructed driver
// (store/mail_sync.rs) leans on WITHOUT a live JMAP server: which accounts come
// due (enabled ∩ backoff), the cursor round-trip the CursorStore impl performs,
// and force-resync's cursor reset. The connect→ingest network path is validated
// on a real server (Slice A report), not here.

/// mail_sync_tick's due-scan (`mail_accounts_due`) honors enabled + the
/// per-account backoff window: a disabled account and one whose next_attempt_at
/// is still in the future are both skipped; clearing the window brings it back.
#[tokio::test]
async fn driver_due_honors_enabled_and_backoff() {
    std::env::set_var("HIVE_CRED_KEY", "mail-store-test-key");
    let store = common::test_store().await;
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

    // Fresh + enabled → due immediately (next_attempt_at NULL).
    assert!(
        store
            .mail_accounts_due()
            .await
            .unwrap()
            .iter()
            .any(|a| a.id == view.id),
        "a fresh enabled account is due"
    );

    // Disabled → never due (even with the window clear).
    assert!(store
        .mail_account_set_enabled(&view.id, false)
        .await
        .unwrap());
    assert!(
        !store
            .mail_accounts_due()
            .await
            .unwrap()
            .iter()
            .any(|a| a.id == view.id),
        "a disabled account is skipped"
    );

    // Re-enable, then push the backoff window into the future → skipped until it
    // elapses.
    assert!(store
        .mail_account_set_enabled(&view.id, true)
        .await
        .unwrap());
    exec(
        &store,
        "UPDATE mail_accounts SET next_attempt_at = '2099-01-01T00:00:00.000Z' WHERE id = ?",
        vec![view.id.clone().into()],
    )
    .await;
    assert!(
        !store
            .mail_accounts_due()
            .await
            .unwrap()
            .iter()
            .any(|a| a.id == view.id),
        "an account inside its backoff window is not due"
    );

    // An elapsed window (past timestamp) is due again.
    exec(
        &store,
        "UPDATE mail_accounts SET next_attempt_at = '2000-01-01T00:00:00.000Z' WHERE id = ?",
        vec![view.id.clone().into()],
    )
    .await;
    assert!(
        store
            .mail_accounts_due()
            .await
            .unwrap()
            .iter()
            .any(|a| a.id == view.id),
        "an elapsed backoff window makes the account due again"
    );
}

/// The CursorStore round-trip: what mail_cursor_save persists, mail_cursor_load
/// returns — including an in_progress backfill cursor (the resume anchor) and
/// both JMAP state strings. This is exactly the load/save the driver's
/// StoreCursor performs between backfill pages.
#[tokio::test]
async fn driver_cursor_roundtrips_through_the_store() {
    std::env::set_var("HIVE_CRED_KEY", "mail-store-test-key");
    let store = common::test_store().await;
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

    // A fresh account: no email/mailbox state, backfill pending.
    let (email, mailbox, status, cursor) = store.mail_cursor_load(&view.id).await.unwrap();
    assert_eq!(email, None);
    assert_eq!(mailbox, None);
    assert_eq!(status, "pending");
    assert!(cursor.is_none());

    // Save an in-progress backfill cursor with both state strings (the shape
    // StoreCursor writes mid-backfill).
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

/// Force-resync plants the sentinel state that routes the next delta into
/// reconciliation AND clears the backoff, so a previously-failing account comes
/// due again on the next tick. (Complements the lifecycle test, focused on the
/// driver's re-arm contract.)
#[tokio::test]
async fn driver_force_resync_resets_cursor_and_rearms() {
    std::env::set_var("HIVE_CRED_KEY", "mail-store-test-key");
    let store = common::test_store().await;
    let view = store
        .mail_account_create(
            "alice",
            "resync@example.test",
            "https://mail.example.test",
            None,
            "acc-rs",
            "pw",
        )
        .await
        .unwrap();

    // Simulate two prior failures (attempts + a future backoff window) and a
    // captured email state.
    store
        .mail_account_mark_failed(&view.id, "connect refused")
        .await
        .unwrap();
    store
        .mail_account_mark_failed(&view.id, "connect refused")
        .await
        .unwrap();
    exec(
        &store,
        "UPDATE mail_accounts SET email_state = 'live-state' WHERE id = ?",
        vec![view.id.clone().into()],
    )
    .await;
    assert!(
        !store
            .mail_accounts_due()
            .await
            .unwrap()
            .iter()
            .any(|a| a.id == view.id),
        "the backed-off account is not due before resync"
    );

    assert!(store.mail_account_force_resync(&view.id).await.unwrap());

    // Sentinel state string (the ONLY deliberate route into reconcile),
    // attempts cleared, window cleared → due again.
    assert_eq!(
        text_of(
            &store,
            "SELECT email_state FROM mail_accounts WHERE id = ?",
            vec![view.id.clone().into()]
        )
        .await
        .as_deref(),
        Some("force-resync")
    );
    assert_eq!(
        count(
            &store,
            "SELECT attempts FROM mail_accounts WHERE id = ?",
            vec![view.id.clone().into()]
        )
        .await,
        0,
        "force-resync clears the attempt counter"
    );
    assert!(
        store
            .mail_accounts_due()
            .await
            .unwrap()
            .iter()
            .any(|a| a.id == view.id),
        "force-resync re-arms the account so the next tick runs it"
    );
}
