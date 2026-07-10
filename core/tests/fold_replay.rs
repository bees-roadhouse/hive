// Fold integration tests (PR 1.5). Hermetic: tempdir + MemoryKeySource, no
// Postgres, no network; timestamps are fixed strings (the envelope freezes
// only their shape). The scripted log covers EVERY record kind and runs
// through the real LogWriter/LogReader pair, so these tests also prove the
// op-log → fold seam end to end.

use ciborium::Value as Cb;
use hive_core::index::SqliteIndex;
use hive_core::keys::MemoryKeySource;
use hive_core::oplog::{kind, LogReader, LogWriter, Record};
use hive_core::{fold, index};

fn keysource() -> MemoryKeySource {
    MemoryKeySource([7u8; 32])
}

fn t(s: &str) -> Cb {
    Cb::Text(s.to_string())
}

fn map(entries: Vec<(&str, Cb)>) -> Cb {
    Cb::Map(entries.into_iter().map(|(k, v)| (t(k), v)).collect())
}

fn arr(vals: Vec<Cb>) -> Cb {
    Cb::Array(vals)
}

fn int(v: i64) -> Cb {
    Cb::from(v)
}

fn ts(i: usize) -> String {
    format!("2026-07-10T12:{:02}:{:02}.000Z", i / 60, i % 60)
}

const DEVICE: &str = "dev-1";

/// Build the scripted records (seq assigned in order, ts strictly
/// increasing) from (kind, payload) pairs.
fn records(pairs: Vec<(&str, Cb)>) -> Vec<Record> {
    pairs
        .into_iter()
        .enumerate()
        .map(|(i, (k, payload))| {
            Record::new(
                DEVICE,
                i as u64 + 1,
                i as u64 + 1,
                &ts(i),
                "nate",
                k,
                payload,
            )
        })
        .collect()
}

/// The full script: every kind in the closed set at least once, with the
/// interesting payload shapes (anchors + emerged + inbox, custom entities,
/// JSON merges, soft mail tombstones, upserts, redaction).
fn script() -> Vec<Record> {
    records(vec![
        // 1 — journal with anchors, a pre-materialized emerged task, and
        // explicit inbox fan-out.
        (
            kind::JOURNAL_APPEND,
            map(vec![
                ("id", t("jrnl_1")),
                ("author", t("nate")),
                ("body", t("Ship the SQLite fold this week [person: Pia]")),
                ("tags", arr(vec![t("p2p"), t("storage")])),
                ("mentions", arr(vec![t("pia")])),
                ("user_scope", Cb::Null),
                ("created_at", t(&ts(0))),
                (
                    "anchors",
                    arr(vec![map(vec![
                        ("id", t("anc_1")),
                        ("start", int(0)),
                        ("end", int(24)),
                        ("text", t("Ship the SQLite fold")),
                        ("kind", t("task")),
                        ("ref_id", t("task_1")),
                    ])]),
                ),
                (
                    "emerged",
                    arr(vec![map(vec![
                        ("kind", t("task")),
                        ("id", t("task_1")),
                        (
                            "fields",
                            map(vec![
                                ("title", t("Ship the SQLite fold")),
                                ("body", t("Fold, FTS5, ANN — additive.")),
                                ("status", t("todo")),
                                ("priority", t("high")),
                                ("tags", arr(vec![t("p2p")])),
                                ("assignees", arr(vec![t("nate"), t("pia")])),
                                ("origin_entry_id", t("jrnl_1")),
                                ("anchor_text", t("Ship the SQLite fold")),
                                ("created_at", t(&ts(0))),
                                ("updated_at", t(&ts(0))),
                            ]),
                        ),
                    ])]),
                ),
                (
                    "inbox",
                    arr(vec![map(vec![
                        ("id", t("inb_1")),
                        ("recipient", t("pia")),
                        ("from", t("nate")),
                        ("reason", t("assignment")),
                        ("ref_kind", t("task")),
                        ("ref_id", t("task_1")),
                        ("entry_id", t("jrnl_1")),
                        ("snippet", t("Ship the SQLite fold")),
                    ])]),
                ),
            ]),
        ),
        // 2 — the anchors link the command layer emits alongside.
        (
            kind::LINK_ADD,
            map(vec![
                ("id", t("link_1")),
                ("source_kind", t("journal")),
                ("source_id", t("jrnl_1")),
                ("rel", t("anchors")),
                ("target_kind", t("task")),
                ("target_id", t("task_1")),
                ("created_at", t(&ts(0))),
            ]),
        ),
        // 3 — link without id/rel/created_at: deterministic fallbacks.
        (
            kind::LINK_ADD,
            map(vec![
                ("source_kind", t("journal")),
                ("source_id", t("jrnl_1")),
                ("target_kind", t("person")),
                ("target_id", t("person_pia")),
            ]),
        ),
        // 4..6 — built-in creates: person, decision, event.
        (
            kind::ENTITY_CREATE,
            map(vec![
                ("kind", t("person")),
                ("id", t("person_pia")),
                (
                    "fields",
                    map(vec![
                        ("slug", t("pia")),
                        ("name", t("Pia")),
                        ("kind", t("ai")),
                        ("created_at", t(&ts(3))),
                    ]),
                ),
            ]),
        ),
        (
            kind::ENTITY_CREATE,
            map(vec![
                ("kind", t("decision")),
                ("id", t("dec_1")),
                (
                    "fields",
                    map(vec![
                        ("title", t("Adopt append-only storage")),
                        ("context", t("The mutable mirror rewrote history")),
                        ("decision", t("Every write becomes a record")),
                        ("consequences", t("Replay rebuilds all derived state")),
                        ("status", t("accepted")),
                        ("tags", arr(vec![t("architecture")])),
                        ("assignees", arr(vec![])),
                        ("created_at", t(&ts(4))),
                        ("updated_at", t(&ts(4))),
                    ]),
                ),
            ]),
        ),
        (
            kind::ENTITY_CREATE,
            map(vec![
                ("kind", t("event")),
                ("id", t("ev_1")),
                (
                    "fields",
                    map(vec![
                        ("title", t("Hive apiary inspection")),
                        ("body", t("Check the brood frames")),
                        ("at", t("2026-07-12T09:00:00.000Z")),
                        ("tags", arr(vec![])),
                        ("assignees", arr(vec![t("nate")])),
                        ("created_at", t(&ts(5))),
                    ]),
                ),
            ]),
        ),
        // 7..9 — custom entity registry + instance.
        (
            kind::ENTITY_CREATE,
            map(vec![
                ("kind", t("entity_type")),
                ("id", t("etype_recipe")),
                (
                    "fields",
                    map(vec![
                        ("slug", t("recipe")),
                        ("name", t("Recipe")),
                        ("name_plural", t("Recipes")),
                        ("created_by", t("nate")),
                        ("created_at", t(&ts(6))),
                        ("updated_at", t(&ts(6))),
                    ]),
                ),
            ]),
        ),
        (
            kind::ENTITY_CREATE,
            map(vec![
                ("kind", t("entity_field")),
                ("id", t("efield_notes")),
                (
                    "fields",
                    map(vec![
                        ("type_id", t("etype_recipe")),
                        ("slug", t("notes")),
                        ("label", t("Notes")),
                        ("field_type", t("text")),
                        ("required", Cb::Bool(false)),
                        ("position", int(0)),
                        ("created_at", t(&ts(7))),
                        ("updated_at", t(&ts(7))),
                    ]),
                ),
            ]),
        ),
        (
            kind::ENTITY_CREATE,
            map(vec![
                ("kind", t("recipe")),
                ("id", t("ent_cake")),
                (
                    "fields",
                    map(vec![
                        ("type_id", t("etype_recipe")),
                        ("title", t("Honey cake")),
                        (
                            "fields",
                            map(vec![("notes", t("double the honey")), ("rating", int(4))]),
                        ),
                        ("created_by", t("nate")),
                        ("created_at", t(&ts(8))),
                        ("updated_at", t(&ts(8))),
                    ]),
                ),
            ]),
        ),
        // 10..11 — LWW updates: built-in column set, custom JSON merge
        // (null removes a key).
        (
            kind::ENTITY_UPDATE,
            map(vec![
                ("kind", t("task")),
                ("id", t("task_1")),
                (
                    "fields",
                    map(vec![("status", t("done")), ("updated_at", t(&ts(9)))]),
                ),
            ]),
        ),
        (
            kind::ENTITY_UPDATE,
            map(vec![
                ("kind", t("recipe")),
                ("id", t("ent_cake")),
                (
                    "fields",
                    map(vec![
                        (
                            "fields",
                            map(vec![("notes", t("triple the honey")), ("rating", Cb::Null)]),
                        ),
                        ("updated_at", t(&ts(10))),
                    ]),
                ),
            ]),
        ),
        // 12..15 — the mail module: account, mailbox, message, attachment.
        (
            kind::MODULE_DOC,
            map(vec![
                ("module", t("mail")),
                ("doc_kind", t("account")),
                ("id", t("acct_1")),
                (
                    "fields",
                    map(vec![
                        ("owner", t("nate")),
                        ("address", t("nate@example.com")),
                        ("jmap_url", t("https://mail.example.com/jmap")),
                        ("created_at", t(&ts(11))),
                        ("updated_at", t(&ts(11))),
                    ]),
                ),
            ]),
        ),
        (
            kind::MODULE_DOC,
            map(vec![
                ("module", t("mail")),
                ("doc_kind", t("mailbox")),
                ("id", t("mbox_inbox")),
                (
                    "fields",
                    map(vec![
                        ("account_id", t("acct_1")),
                        ("jmap_id", t("mb-1")),
                        ("name", t("Inbox")),
                        ("role", t("inbox")),
                        ("ingest", Cb::Bool(true)),
                        ("sort_order", int(0)),
                    ]),
                ),
            ]),
        ),
        (
            kind::MODULE_DOC,
            map(vec![
                ("module", t("mail")),
                ("doc_kind", t("message")),
                ("id", t("mail_1")),
                (
                    "fields",
                    map(vec![
                        ("account_id", t("acct_1")),
                        ("jmap_id", t("jm-1")),
                        ("jmap_thread_id", t("jt-1")),
                        ("from_addr", t("alice@example.com")),
                        ("from_name", t("Alice")),
                        ("to_json", arr(vec![t("nate@example.com")])),
                        ("subject", t("Quarterly bees")),
                        ("received_at", t("2026-07-09T08:00:00.000Z")),
                        ("mailbox_ids_json", arr(vec![t("mb-1")])),
                        ("body_text", t("nectar budget attached")),
                        ("snippet", t("nectar budget…")),
                        ("size", int(2048)),
                        ("has_attachments", Cb::Bool(true)),
                        ("embed_state", t("pending")),
                        ("user_scope", t("nate")),
                        ("created_at", t(&ts(13))),
                        ("updated_at", t(&ts(13))),
                    ]),
                ),
            ]),
        ),
        (
            kind::MODULE_DOC,
            map(vec![
                ("module", t("mail")),
                ("doc_kind", t("attachment")),
                ("id", t("att_1")),
                (
                    "fields",
                    map(vec![
                        ("message_id", t("mail_1")),
                        ("blob_hash", t("b3-deadbeef")),
                        ("jmap_blob_id", t("blob-1")),
                        ("filename", t("budget.pdf")),
                        ("mime", t("application/pdf")),
                        ("size", int(1024)),
                        ("created_at", t(&ts(14))),
                    ]),
                ),
            ]),
        ),
        // 16 — message delta upsert: keywords + updated_at only (sync shape).
        (
            kind::MODULE_DOC,
            map(vec![
                ("module", t("mail")),
                ("doc_kind", t("message")),
                ("id", t("mail_1")),
                (
                    "fields",
                    map(vec![
                        ("keywords_json", map(vec![("$seen", Cb::Bool(true))])),
                        ("updated_at", t(&ts(15))),
                    ]),
                ),
            ]),
        ),
        // 17 — sync cursor onto the account row.
        (
            kind::CURSOR_SET,
            map(vec![
                ("module", t("mail")),
                ("account", t("acct_1")),
                (
                    "cursor",
                    map(vec![
                        ("email_state", t("s-100")),
                        ("mailbox_state", t("s-7")),
                        ("backfill_status", t("running")),
                        ("backfill_cursor", map(vec![("upto", t("2026-01-01"))])),
                        ("attempts", int(0)),
                    ]),
                ),
            ]),
        ),
        // 18..19 — config upsert, twice (second wins).
        (
            kind::CONFIG_SET,
            map(vec![("key", t("app.version")), ("value", t("0.6.9"))]),
        ),
        (
            kind::CONFIG_SET,
            map(vec![("key", t("app.version")), ("value", t("0.7.0"))]),
        ),
        // 20 — alias (the importer's re-keyed blob hashes).
        (
            kind::ALIAS,
            map(vec![
                ("from", t("sha256-old")),
                ("to", t("b3-new")),
                ("namespace", t("blob")),
            ]),
        ),
        // 21 — a second journal entry, then redact it.
        (
            kind::JOURNAL_APPEND,
            map(vec![
                ("id", t("jrnl_2")),
                ("author", t("nate")),
                ("body", t("The passphrase is swordfish")),
                ("tags", arr(vec![])),
                ("mentions", arr(vec![])),
                ("created_at", t(&ts(20))),
            ]),
        ),
        (
            kind::REDACT,
            map(vec![("kind", t("journal")), ("id", t("jrnl_2"))]),
        ),
        // 23 — redact a subset of decision columns.
        (
            kind::REDACT,
            map(vec![
                ("kind", t("decision")),
                ("id", t("dec_1")),
                ("fields", arr(vec![t("context"), t("consequences")])),
            ]),
        ),
        // 24..25 — tombstones: hard (event), soft (mail).
        (
            kind::TOMBSTONE,
            map(vec![("kind", t("event")), ("id", t("ev_1"))]),
        ),
        (
            kind::TOMBSTONE,
            map(vec![("kind", t("mail")), ("id", t("mail_1"))]),
        ),
        // 26 — drop the explicit link.
        (kind::LINK_REMOVE, map(vec![("id", t("link_1"))])),
    ])
}

/// Write the script through the real LogWriter, read it back with the strict
/// LogReader, and return the recovered records.
fn round_trip_through_log(dir: &std::path::Path) -> Vec<Record> {
    let keys = keysource();
    let mut w = LogWriter::open(dir, DEVICE, &keys).unwrap();
    w.append_batch(&script()).unwrap();
    LogReader::scan(dir, DEVICE, &keys)
        .unwrap()
        .map(|item| item.unwrap().0)
        .collect()
}

fn fresh_index(dir: &std::path::Path) -> SqliteIndex {
    SqliteIndex::open(dir, &keysource()).unwrap()
}

// ── 1. determinism: replay twice, byte-identical dumps ──────────────────────

#[test]
fn scripted_replay_into_two_indexes_is_byte_identical() {
    let log_dir = tempfile::tempdir().unwrap();
    let recs = round_trip_through_log(log_dir.path());
    assert_eq!(recs.len(), script().len(), "log round-trip lost records");

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let mut a = fresh_index(dir_a.path());
    let mut b = fresh_index(dir_b.path());
    assert_eq!(a.fold(&recs).unwrap(), recs.len());
    assert_eq!(b.fold(&recs).unwrap(), recs.len());

    let dump_a = a.canonical_dump().unwrap();
    let dump_b = b.canonical_dump().unwrap();
    assert!(!dump_a.is_empty());
    assert_eq!(dump_a, dump_b, "two replays of the same log diverged");
    a.fts_integrity_check().unwrap();
    b.fts_integrity_check().unwrap();
}

// ── 2. replay = incremental; watermarks; idempotent-or-rejected ─────────────

#[test]
fn batched_fold_equals_one_shot_and_watermark_advances() {
    let log_dir = tempfile::tempdir().unwrap();
    let recs = round_trip_through_log(log_dir.path());

    let dir_a = tempfile::tempdir().unwrap();
    let mut all_at_once = fresh_index(dir_a.path());
    all_at_once.fold(&recs).unwrap();

    let dir_b = tempfile::tempdir().unwrap();
    let mut batched = fresh_index(dir_b.path());
    for chunk in recs.chunks(3) {
        batched.fold(chunk).unwrap();
    }

    assert_eq!(
        all_at_once.canonical_dump().unwrap(),
        batched.canonical_dump().unwrap(),
        "batch-at-a-time fold diverged from all-at-once"
    );
    assert_eq!(
        batched.applied_seq(DEVICE).unwrap(),
        Some(recs.len() as u64)
    );
}

#[test]
fn reapply_is_skipped_by_fold_and_rejected_by_apply() {
    let log_dir = tempfile::tempdir().unwrap();
    let recs = round_trip_through_log(log_dir.path());
    let dir = tempfile::tempdir().unwrap();
    let mut idx = fresh_index(dir.path());
    idx.fold(&recs).unwrap();
    let dump = idx.canonical_dump().unwrap();

    // The batch replayer treats already-applied seqs as no-ops (crash heal).
    assert_eq!(idx.fold(&recs).unwrap(), 0);
    assert_eq!(idx.canonical_dump().unwrap(), dump);

    // Direct apply of an applied seq is REJECTED (the documented strict arm).
    let tx = idx.conn_mut().transaction().unwrap();
    let err = fold::apply(&tx, &recs[0]).unwrap_err();
    assert!(err.to_string().contains("watermark"), "{err}");
    drop(tx);

    // A seq gap is rejected too: nothing from a partial future may land.
    let mut future = recs[0].clone();
    future.seq = recs.len() as u64 + 5;
    let tx = idx.conn_mut().transaction().unwrap();
    assert!(fold::apply(&tx, &future).is_err());
    drop(tx);
}

// ── 3. per-kind application ─────────────────────────────────────────────────

fn folded() -> (tempfile::TempDir, SqliteIndex) {
    let log_dir = tempfile::tempdir().unwrap();
    let recs = round_trip_through_log(log_dir.path());
    let dir = tempfile::tempdir().unwrap();
    let mut idx = fresh_index(dir.path());
    idx.fold(&recs).unwrap();
    (dir, idx)
}

fn one<T: rusqlite::types::FromSql>(idx: &SqliteIndex, sql: &str) -> T {
    idx.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn journal_append_projects_rows_anchors_emerged_and_fts() {
    let (_dir, idx) = folded();
    let body: String = one(&idx, "SELECT body FROM journal WHERE id = 'jrnl_1'");
    assert!(body.starts_with("Ship the SQLite fold"));
    let tags: String = one(&idx, "SELECT tags FROM journal WHERE id = 'jrnl_1'");
    assert_eq!(tags, r#"["p2p","storage"]"#);
    let anchor_ref: String = one(&idx, "SELECT ref_id FROM anchors WHERE id = 'anc_1'");
    assert_eq!(anchor_ref, "task_1");
    // The emerged task materialized with the carried columns…
    let status: String = one(&idx, "SELECT status FROM tasks WHERE id = 'task_1'");
    assert_eq!(status, "done"); // updated later in the script
                                // …and both journal and task are findable by keyword.
    let hits = idx.keyword_search("sqlite fold", None, 10).unwrap();
    let kinds: Vec<&str> = hits.iter().map(|h| h.kind.as_str()).collect();
    assert!(kinds.contains(&"journal"), "hits: {kinds:?}");
    assert!(kinds.contains(&"task"), "hits: {kinds:?}");
}

#[test]
fn inbox_rows_only_when_carried() {
    let (_dir, idx) = folded();
    // Exactly the one explicitly carried item — the fold derives no fan-out
    // (jrnl_1 also @mentions pia; no mention row appears without a payload).
    let n: i64 = one(&idx, "SELECT count(*) FROM inbox");
    assert_eq!(n, 1);
    let recipient: String = one(&idx, "SELECT recipient FROM inbox WHERE id = 'inb_1'");
    assert_eq!(recipient, "pia");
    let read_at: Option<String> = one(&idx, "SELECT read_at FROM inbox WHERE id = 'inb_1'");
    assert_eq!(read_at, None);
}

#[test]
fn custom_entity_create_and_update_merge() {
    let (_dir, idx) = folded();
    let fields: String = one(&idx, "SELECT fields FROM entities WHERE id = 'ent_cake'");
    let parsed: serde_json::Value = serde_json::from_str(&fields).unwrap();
    assert_eq!(parsed["notes"], "triple the honey");
    assert!(
        parsed.get("rating").is_none(),
        "null in the patch must remove the key (merge_fields parity): {parsed}"
    );
    // json_extract works over the TEXT column (the 1.6 query path).
    let via_json: String = one(
        &idx,
        "SELECT json_extract(fields, '$.notes') FROM entities WHERE id = 'ent_cake'",
    );
    assert_eq!(via_json, "triple the honey");
    // FTS carries the searchable text of Text|Choice|Date fields.
    let hits = idx
        .keyword_search("triple honey", Some(&["recipe"]), 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "ent_cake");
    assert_eq!(hits[0].title, "Honey cake");
}

#[test]
fn module_doc_mail_rows_and_delta_upsert() {
    // The shared script tombstones mail_1 at the end (which deletes its
    // attachment rows), so this test folds only the mail prefix: account,
    // mailbox, message, attachment, delta (script records 12–16).
    let prefix: Vec<Record> = script().into_iter().take(16).collect();
    let dir = tempfile::tempdir().unwrap();
    let mut idx = fresh_index(dir.path());
    idx.fold(&prefix).unwrap();

    let owner: String = one(&idx, "SELECT owner FROM mail_accounts WHERE id = 'acct_1'");
    assert_eq!(owner, "nate");
    let ingest: bool = one(
        &idx,
        "SELECT ingest FROM mail_mailboxes WHERE id = 'mbox_inbox'",
    );
    assert!(ingest);
    // The delta upsert set keywords and updated_at, and kept every other
    // column from the first write.
    let keywords: String = one(
        &idx,
        "SELECT keywords_json FROM mail_messages WHERE id = 'mail_1'",
    );
    assert_eq!(keywords, r#"{"$seen":true}"#);
    let subject: String = one(
        &idx,
        "SELECT subject FROM mail_messages WHERE id = 'mail_1'",
    );
    assert_eq!(subject, "Quarterly bees");
    let updated: String = one(
        &idx,
        "SELECT updated_at FROM mail_messages WHERE id = 'mail_1'",
    );
    assert_eq!(updated, ts(15));
    let att_blob: String = one(
        &idx,
        "SELECT blob_hash FROM mail_attachments WHERE id = 'att_1'",
    );
    assert_eq!(att_blob, "b3-deadbeef");
}

#[test]
fn cursor_set_round_trips_account_sync_state() {
    let (_dir, idx) = folded();
    let (email_state, backfill_status, attempts): (String, String, i64) = idx
        .conn()
        .query_row(
            "SELECT email_state, backfill_status, attempts FROM mail_accounts WHERE id = 'acct_1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(email_state, "s-100");
    assert_eq!(backfill_status, "running");
    assert_eq!(attempts, 0);
    let upto: String = one(
        &idx,
        "SELECT json_extract(backfill_cursor, '$.upto') FROM mail_accounts WHERE id = 'acct_1'",
    );
    assert_eq!(upto, "2026-01-01");
}

#[test]
fn config_set_upserts_and_alias_lands() {
    let (_dir, idx) = folded();
    let v: String = one(&idx, "SELECT value FROM config WHERE key = 'app.version'");
    assert_eq!(v, "0.7.0", "second config.set must win");
    let to: String = one(
        &idx,
        "SELECT \"to\" FROM aliases WHERE namespace = 'blob' AND \"from\" = 'sha256-old'",
    );
    assert_eq!(to, "b3-new");
}

#[test]
fn link_fallback_id_is_deterministic_and_remove_deletes() {
    let (_dir, idx) = folded();
    // link_1 was removed by the script's link.remove.
    let n: i64 = one(&idx, "SELECT count(*) FROM links WHERE id = 'link_1'");
    assert_eq!(n, 0);
    // The id-less link got the documented derivation (record seq 3) and the
    // rel/created_at defaults.
    let (id, rel, created_at): (String, String, String) = idx
        .conn()
        .query_row(
            "SELECT id, rel, created_at FROM links WHERE target_id = 'person_pia'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(id, format!("link_{DEVICE}-{:016x}", 3));
    assert_eq!(rel, "relates");
    assert_eq!(created_at, ts(2)); // the record's ts (seq 3 = index 2)
}

#[test]
fn tombstone_removes_row_fts_and_soft_deletes_mail() {
    let (_dir, idx) = folded();
    // Hard tombstone: the event row and its search row are gone.
    let n: i64 = one(&idx, "SELECT count(*) FROM events WHERE id = 'ev_1'");
    assert_eq!(n, 0);
    let n: i64 = one(
        &idx,
        "SELECT count(*) FROM search WHERE kind = 'event' AND ref_id = 'ev_1'",
    );
    assert_eq!(n, 0);
    assert!(idx
        .keyword_search("brood frames", None, 10)
        .unwrap()
        .is_empty());
    // Soft mail tombstone: row survives with deleted_at, key intact, and the
    // attachment metadata is gone.
    let deleted_at: Option<String> = one(
        &idx,
        "SELECT deleted_at FROM mail_messages WHERE id = 'mail_1'",
    );
    assert!(deleted_at.is_some());
    let n: i64 = one(
        &idx,
        "SELECT count(*) FROM mail_attachments WHERE message_id = 'mail_1'",
    );
    assert_eq!(n, 0);
}

#[test]
fn redact_clears_fields_and_reindexes() {
    let (_dir, idx) = folded();
    // Whole-set redaction: journal body cleared, search no longer finds it.
    let body: String = one(&idx, "SELECT body FROM journal WHERE id = 'jrnl_2'");
    assert_eq!(body, "");
    assert!(idx
        .keyword_search("swordfish", None, 10)
        .unwrap()
        .is_empty());
    // The row itself and its metadata survive.
    let author: String = one(&idx, "SELECT author FROM journal WHERE id = 'jrnl_2'");
    assert_eq!(author, "nate");
    // Subset redaction: named decision columns cleared, the rest intact.
    let (context, decision, consequences): (String, String, String) = idx
        .conn()
        .query_row(
            "SELECT context, decision, consequences FROM decisions WHERE id = 'dec_1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(context, "");
    assert_eq!(consequences, "");
    assert_eq!(decision, "Every write becomes a record");
    // FTS reflects the post-redaction text: cleared column misses, kept hits.
    assert!(idx
        .keyword_search("mutable mirror", None, 10)
        .unwrap()
        .is_empty());
    assert!(!idx
        .keyword_search("becomes a record", None, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn unknown_payload_fields_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = fresh_index(dir.path());
    let bad = records(vec![(
        kind::CONFIG_SET,
        map(vec![
            ("key", t("k")),
            ("value", t("v")),
            ("surprise", t("x")),
        ]),
    )]);
    let err = idx.fold(&bad).unwrap_err();
    assert!(format!("{err:#}").contains("unknown"), "{err:#}");
    // Nothing committed: the config row did not land.
    let n: i64 = one(&idx, "SELECT count(*) FROM config");
    assert_eq!(n, 0);
}

// ── 6. fold-version bump resets ─────────────────────────────────────────────

#[test]
fn fold_version_bump_resets_tables_and_watermark() {
    let log_dir = tempfile::tempdir().unwrap();
    let recs = round_trip_through_log(log_dir.path());
    let dir = tempfile::tempdir().unwrap();
    {
        let mut idx = fresh_index(dir.path());
        idx.fold(&recs).unwrap();
        assert!(idx.applied_seq(DEVICE).unwrap().is_some());
    }
    // Reopen at a bumped fold version: derived state and watermark reset.
    let keys = keysource();
    let mut idx =
        SqliteIndex::open_with_fold_version(dir.path(), &keys, fold::FOLD_VERSION + 1).unwrap();
    assert_eq!(idx.applied_seq(DEVICE).unwrap(), None);
    let n: i64 = one(&idx, "SELECT count(*) FROM journal");
    assert_eq!(n, 0);
    assert!(idx.keyword_search("sqlite", None, 10).unwrap().is_empty());
    // Replay rebuilds — that is the design.
    idx.fold(&recs).unwrap();
    assert_eq!(idx.applied_seq(DEVICE).unwrap(), Some(recs.len() as u64));
    let n: i64 = one(&idx, "SELECT count(*) FROM journal");
    assert_eq!(n, 2);
}

// ── 7. SQLCipher at rest ────────────────────────────────────────────────────

#[test]
fn index_db_is_encrypted_and_wrong_key_fails() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut idx = fresh_index(dir.path());
        idx.fold(&records(vec![(
            kind::CONFIG_SET,
            map(vec![("key", t("k")), ("value", t("sensitive"))]),
        )]))
        .unwrap();
    }
    let db_path = dir.path().join(index::INDEX_DB_FILE);
    let head = std::fs::read(&db_path).unwrap();
    assert!(
        !head.starts_with(b"SQLite format 3\0"),
        "index.db is plaintext SQLite"
    );
    assert!(
        !head.windows(b"sensitive".len()).any(|w| w == b"sensitive"),
        "plaintext content visible in index.db"
    );
    // A different master key must not open it.
    let wrong = MemoryKeySource([8u8; 32]);
    let err = match SqliteIndex::open(dir.path(), &wrong) {
        Ok(_) => panic!("wrong master key opened the index"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("key check"), "{err}");
    // The right key still does.
    let idx = fresh_index(dir.path());
    let v: String = one(&idx, "SELECT value FROM config WHERE key = 'k'");
    assert_eq!(v, "sensitive");
}
