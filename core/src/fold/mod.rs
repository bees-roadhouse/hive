// The fold (PR 1.5, D18): a mechanical projector from op-log records
// (core/src/oplog) into the derived SQLite index (core/src/index). One
// handler per record kind — the closed 11-kind set in oplog::kind.
//
// DETERMINISM IS LAW. `apply` reads nothing but (tx, rec): no clocks, no
// RNG, no environment, no floating host state. Every id and every timestamp
// comes from the record (payload fields, or the record's own `ts`/`device`/
// `seq` envelope fields, which are part of the frozen bytes). Replaying the
// same records into a fresh index reproduces byte-identical derived state —
// core/tests/fold_replay.rs asserts it, and the grep fence in
// core/tests/determinism.rs makes the property reviewable.
//
// Watermark discipline: `apply` enforces the per-device gapless chain — a
// record only applies when `rec.seq == fold_meta.applied_seq + 1` for its
// device (or seq 1 with no row). Re-applying an already-applied seq is
// REJECTED here (documented choice: the strict arm keeps double-apply bugs
// loud); the batch replayer (SqliteIndex::fold) SKIPS records at or below
// the watermark before calling in, which is the idempotent crash-heal path
// (tail replay after a crash re-presents folded records; they no-op).
//
// ────────────────────────────────────────────────────────────────────────────
// PAYLOAD SCHEMAS (v1) — the contract the 1.6 command layer and the 1.7
// importer write to. Payloads are CBOR maps with text keys; FIELD NAMES ARE
// COLUMN NAMES of the corresponding derived tables (which themselves track
// the Postgres reference schema in core/src/db.rs). Unknown keys are errors:
// fail closed, so schema drift is caught at the first record, not at the
// 1.6 cutover. Where a shape was ambiguous the Postgres column set decides,
// noted per kind.
//
// journal.append — one immutable prose entry plus everything that emerged
// from it, pre-materialized by the command layer (matching the column sets
// store/journal.rs writes):
//   {
//     id, author, body,                        // journal columns (required)
//     tags: [..], mentions: [..],              // JSON arrays (default [])
//     created_at,                              // the entry timestamp
//     user_scope?,                             // NULL = global history
//     anchors?:  [{id, start, end, text, kind, ref_id, created_at?}],
//                                              // anchors columns; entry_id is
//                                              // the parent id; created_at
//                                              // defaults to the entry's
//     emerged?:  [{kind, id, fields{..}}, ..], // entity-creates, pre-
//                                              // materialized — each element
//                                              // is exactly an entity.create
//                                              // payload, applied inline
//     inbox?:    [{id, recipient, from, reason, ref_kind, ref_id,
//                  entry_id?, snippet?, created_at?}, ..],
//                                              // pre-computed fan-out; the
//                                              // fold NEVER derives fan-out
//   }
//   Links for anchors/tokens ride as separate link.add records in the same
//   batch (they are first-class records, not journal sub-objects).
//   FTS: title "author: " + snip(body, 50), body + " " + tags joined — the
//   exact store/journal.rs index_entity call.
//
// entity.create — one row in a built-in table, or a custom-entity instance:
//   { kind, id, fields{..} }
//   Built-in kinds → tables (fields = that table's columns, id column = id):
//     task → tasks, decision → decisions, event → events, topic → topics,
//     project → projects, phase → phases, person → people,
//     profile → profile (id IS the actor key),
//     entity_type → entity_types, entity_field → entity_fields,
//     identity_artifact → identity_artifacts, source → sources.
//   Any other kind is a custom entity-type slug → one `entities` row; fields
//   then carries the entities COLUMNS (type_id, title, fields{..},
//   user_scope?, origin_entry_id?, created_by, created_at, updated_at) — yes,
//   fields.fields: the inner map is the JSON `fields` column, keys = the
//   user-defined field slugs.
//   Strict INSERT: a duplicate id fails (find-or-create lives in the command
//   layer). An optional `inbox` array (same item shape as journal.append's)
//   carries pre-computed fan-out.
//   FTS on create: task (title, body, tags) / decision (title,
//   context+decision+consequences, tags) / event (title, body, tags) /
//   custom instance (title, searchable text of Text|Choice|Date fields in
//   entity_fields position order) — store/{tasks,decisions,events}.rs and
//   store/entity_validation.rs searchable_text, mirrored exactly.
//
// entity.update — last-writer-wins per field:
//   { kind, id, fields{..} }
//   Sets exactly the carried columns on the existing row (missing row =
//   error: the command layer knows current state and emits create first —
//   note profile: Postgres profile_set is an UPSERT with pre-merged
//   sections; the record model splits that into create-then-update, body
//   carried wholesale). For custom instances, fields.fields MERGES at the
//   top level into the JSON column — null removes the key — matching
//   store/entity_validation.rs merge_fields; other entities columns
//   (title, user_scope, updated_at, …) set directly.
//   FTS refreshes from the post-update row for the FTS kinds.
//
// link.add — one knowledge-graph edge (links columns):
//   { source_kind, source_id, rel?, target_kind, target_id, id?, created_at? }
//   rel defaults 'relates' (the column default); created_at defaults to the
//   record ts; id defaults to a deterministic per-record derivation
//   "link_<device>-<seq as 16-hex>" (the fold mints NOTHING random).
//
// link.remove —
//   { id } — delete that edge, or
//   { source_kind, source_id, target_kind, target_id, rel? } — delete every
//   matching edge (rel narrows when present). Absent rows no-op.
//
// tombstone — { kind, id }: the row leaves the derived state (D18: the
//   delete is itself a record; blob crypto-shredding happens elsewhere).
//   journal: row + its anchors rows + FTS + embeddings.
//   task/decision/event: row + FTS + embeddings.
//   topic/project/phase/person/profile/entity_type/entity_field/
//   identity_artifact/source: row. (entity_type does NOT cascade its fields
//   or instances — the command layer tombstones those explicitly.)
//   mail: SOFT delete — deleted_at := record ts, attachments metadata rows
//   deleted, FTS + embeddings dropped; the row keeps its (account_id,
//   jmap_id) key so a later sync replay cannot resurrect it (the
//   store/mail.rs rule).
//   mail_account: FTS + embeddings of its messages dropped, then the row —
//   mailboxes/messages/attachments go via ON DELETE CASCADE.
//   mail_mailbox: row.
//   custom slug: entities row + FTS + embeddings.
//   A tombstone for an id that is already gone is a no-op (delete-twice is
//   legal across devices).
//
// redact — { kind, id, fields? }: content leaves, structure stays. Every
//   kind has a fixed redactable column set; `fields` picks a subset (unknown
//   names are errors), absent means the whole set. NOT NULL text columns
//   clear to '' (JSON object columns to '{}'), nullable columns to NULL.
//   FTS re-indexes from the redacted row; embeddings rows drop (vectors of
//   redacted content must leave retrieval). Sets:
//     journal {body} · task {body, anchor_text} · decision {context,
//     decision, consequences, anchor_text} · event {body, anchor_text} ·
//     person {bio} · profile {body} · mail {subject, body_text, snippet} ·
//     custom slug {fields}.
//
// config.set — { key, value }: upsert into config; value is a string (the
//   Postgres column is TEXT); updated_at := record ts.
//
// module.doc — { module, doc_kind, id, fields{..} }: a module-owned document
//   row. v1 modules: module "mail" with doc_kind account → mail_accounts,
//   mailbox → mail_mailboxes, message → mail_messages, attachment →
//   mail_attachments (metadata only; bytes live in the 1.4 blockstore under
//   fields.blob_hash). Upsert-by-id: carried columns land; on a fresh row
//   absent columns take DDL defaults, on an existing row they keep their
//   values (sync deltas carry only what changed). Unknown module/doc_kind is
//   an error. Mail FTS rows are NOT fold-maintained in 1.5 — eligibility
//   policy (ingest mailboxes, junk) is command-layer business and lands with
//   the 1.6 mail port.
//
// cursor.set — { module, account, cursor{..} }: module sync state. v1:
//   module "mail"; cursor keys are the mail_accounts sync columns
//   (email_state, mailbox_state, backfill_status, backfill_cursor, attempts,
//   next_attempt_at, last_error, last_synced_at, last_status), set on the
//   existing account row (missing account = error); updated_at := record ts.
//
// alias — { from, to, namespace, created_at? }: identifier remapping (the
//   1.7 importer emits these for re-keyed blob hashes). Upsert into aliases
//   keyed (namespace, from); created_at defaults to the record ts.
//
// ── v2 (the PR 1.6 cutover; FOLD_VERSION 2) ────────────────────────────────
//
// The record KINDS are unchanged (frozen at 1.4); v2 widens what
// entity.create/entity.update/tombstone can address, because the cutover
// re-expresses every remaining UPDATE/DELETE path as records (D18):
//
//   - inbox is a built-in entity kind → the inbox table. entity.create
//     {kind:"inbox"} is the standalone inbox_add path (journal.append's
//     `inbox` array remains the fan-out path); entity.update {kind:"inbox",
//     fields:{read_at}} is the mark-read path; tombstone {kind:"inbox"}
//     removes a notification (actor cascade/merge cleanup).
//   - identity is a built-in entity kind → the identities table
//     (platform/platform_id/actor mappings; create/update/tombstone).
//   - journal is addressable by entity.update and ONLY entity.update —
//     creation stays journal.append (entity.create {kind:"journal"} is an
//     error), deletion stays tombstone. This carries actor merge/delete
//     (author reassignment, mentions scrubs) as records. Parity note: like
//     the Postgres merge path, a journal entity.update does NOT re-index FTS
//     (the search title keeps the append-time author).
//   - the FTS5 tokenizer becomes `porter unicode61` (index DDL): the golden
//     retrieval fixture was captured under Postgres `to_tsvector('english')`,
//     which stems — unstemmed unicode61 drops fixture hits ("tomatoes" must
//     match "tomato"). Cross-backend parity is the whole point of the oracle.
//
// Everything else in the v1 contract above is unchanged.
//
// ── v3 (the PR 1.7 importer; FOLD_VERSION 3) ───────────────────────────────
//
// One amendment, uniform across all 11 kinds: the TOP-LEVEL payload map may
// carry an optional `origin` key — provenance the 1.7 importer stamps on
// every record it writes ({source: "hosted-v0.6", table: <postgres table>}).
// The fold strips it before dispatch and projects nothing from it: the op
// log is where provenance lives; the derived state doesn't need it. Records
// without `origin` (all 1.6 command-layer writes) are untouched, so v2-built
// state replays identically under v3.
//
// Everything else in the v1/v2 contract above is unchanged.
// ────────────────────────────────────────────────────────────────────────────

use anyhow::{bail, Context, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::Value as Json;

use crate::oplog::{kind, Record};

/// Version of the fold semantics (payload interpretation + derived DDL).
/// Stored in the index's `PRAGMA user_version`; bumping it makes every
/// existing index rebuild by replay at next open. Bump on ANY change to the
/// schemas above, the DDL, or handler behavior.
///
/// v2 = the PR 1.6 cutover set: inbox/identity built-ins, journal
/// entity.update, and the porter FTS tokenizer (see the v2 header section).
/// v3 = the PR 1.7 importer's optional top-level `origin` provenance key,
/// accepted on every kind and ignored (see the v3 header section).
pub const FOLD_VERSION: u32 = 3;

/// Apply one record to the derived state inside the caller's transaction,
/// advancing the per-device watermark in the same transaction. See the
/// module header for the watermark rules and every payload schema.
pub fn apply(tx: &Transaction, rec: &Record) -> Result<()> {
    let applied: Option<i64> = tx
        .query_row(
            "SELECT applied_seq FROM fold_meta WHERE device = ?1",
            params![rec.device],
            |r| r.get(0),
        )
        .optional()?;
    let expect = applied.unwrap_or(0) as u64 + 1;
    if rec.seq != expect {
        bail!(
            "fold rejects {}#{}: watermark expects seq {expect} (re-applying an \
             applied seq is rejected; the batch replayer skips those instead)",
            rec.device,
            rec.seq
        );
    }

    let mut payload: Json = serde_json::to_value(&rec.payload).with_context(|| {
        format!(
            "record {}#{} payload is not JSON-representable",
            rec.device, rec.seq
        )
    })?;
    // v3: strip the optional top-level `origin` provenance key (the 1.7
    // importer stamps it) before the fail-closed field checks below — the
    // op log keeps it; the projection ignores it.
    if let Some(m) = payload.as_object_mut() {
        m.remove("origin");
    }

    match rec.kind.as_str() {
        kind::JOURNAL_APPEND => journal_append(tx, rec, &payload)?,
        kind::ENTITY_CREATE => entity_create(tx, rec, &payload)?,
        kind::ENTITY_UPDATE => entity_update(tx, &payload)?,
        kind::LINK_ADD => link_add(tx, rec, &payload)?,
        kind::LINK_REMOVE => link_remove(tx, &payload)?,
        kind::TOMBSTONE => tombstone(tx, rec, &payload)?,
        kind::REDACT => redact(tx, &payload)?,
        kind::CONFIG_SET => config_set(tx, rec, &payload)?,
        kind::MODULE_DOC => module_doc(tx, &payload)?,
        kind::CURSOR_SET => cursor_set(tx, rec, &payload)?,
        kind::ALIAS => alias(tx, rec, &payload)?,
        other => bail!("fold has no handler for record kind {other:?}"),
    }

    tx.execute(
        "INSERT INTO fold_meta (device, applied_seq) VALUES (?1, ?2) \
         ON CONFLICT (device) DO UPDATE SET applied_seq = excluded.applied_seq",
        params![rec.device, rec.seq as i64],
    )?;
    Ok(())
}

// ── column specs ─────────────────────────────────────────────────────────────

/// How a JSON payload value binds into a column.
#[derive(Clone, Copy, PartialEq)]
enum Ty {
    /// TEXT: string or null.
    Text,
    /// JSON TEXT: object/array serialized compactly; a string passes through
    /// pre-serialized.
    JsonText,
    /// INTEGER: i64 or null.
    Int,
    /// BOOLEAN: bool (or 0/1) stored as 0/1.
    Bool,
}

type Spec = &'static [(&'static str, Ty)];

const TASKS: Spec = &[
    ("project", Ty::Text),
    ("phase", Ty::Text),
    ("due", Ty::Text),
    ("title", Ty::Text),
    ("body", Ty::Text),
    ("status", Ty::Text),
    ("priority", Ty::Text),
    ("tags", Ty::JsonText),
    ("assignees", Ty::JsonText),
    ("origin_entry_id", Ty::Text),
    ("anchor_text", Ty::Text),
    ("created_at", Ty::Text),
    ("updated_at", Ty::Text),
];

const DECISIONS: Spec = &[
    ("title", Ty::Text),
    ("context", Ty::Text),
    ("decision", Ty::Text),
    ("consequences", Ty::Text),
    ("status", Ty::Text),
    ("tags", Ty::JsonText),
    ("assignees", Ty::JsonText),
    ("project", Ty::Text),
    ("supersedes", Ty::Text),
    ("origin_entry_id", Ty::Text),
    ("anchor_text", Ty::Text),
    ("created_at", Ty::Text),
    ("updated_at", Ty::Text),
];

const EVENTS: Spec = &[
    ("title", Ty::Text),
    ("body", Ty::Text),
    ("at", Ty::Text),
    ("tags", Ty::JsonText),
    ("assignees", Ty::JsonText),
    ("origin_entry_id", Ty::Text),
    ("anchor_text", Ty::Text),
    ("created_at", Ty::Text),
];

const TOPICS: Spec = &[
    ("name", Ty::Text),
    ("slug", Ty::Text),
    ("created_at", Ty::Text),
];

const PROJECTS: Spec = &[
    ("name", Ty::Text),
    ("slug", Ty::Text),
    ("created_at", Ty::Text),
];

const PHASES: Spec = &[
    ("project", Ty::Text),
    ("name", Ty::Text),
    ("position", Ty::Int),
    ("created_at", Ty::Text),
];

const PEOPLE: Spec = &[
    ("slug", Ty::Text),
    ("name", Ty::Text),
    ("kind", Ty::Text),
    ("owner", Ty::Text),
    ("bio", Ty::Text),
    ("role", Ty::Text),
    ("created_at", Ty::Text),
];

const PROFILE: Spec = &[
    ("kind", Ty::Text),
    ("display_name", Ty::Text),
    ("body", Ty::JsonText),
    ("source", Ty::Text),
    ("derived_at", Ty::Text),
    ("updated_at", Ty::Text),
];

const ENTITY_TYPES: Spec = &[
    ("slug", Ty::Text),
    ("name", Ty::Text),
    ("name_plural", Ty::Text),
    ("description", Ty::Text),
    ("icon", Ty::Text),
    ("color", Ty::Text),
    ("board_field", Ty::Text),
    ("archived", Ty::Bool),
    ("created_by", Ty::Text),
    ("created_at", Ty::Text),
    ("updated_at", Ty::Text),
];

const ENTITY_FIELDS: Spec = &[
    ("type_id", Ty::Text),
    ("slug", Ty::Text),
    ("label", Ty::Text),
    ("field_type", Ty::Text),
    ("required", Ty::Bool),
    ("position", Ty::Int),
    ("options", Ty::JsonText),
    ("ref_kind", Ty::Text),
    ("archived", Ty::Bool),
    ("created_at", Ty::Text),
    ("updated_at", Ty::Text),
];

const IDENTITY_ARTIFACTS: Spec = &[
    ("actor", Ty::Text),
    ("kind", Ty::Text),
    ("name", Ty::Text),
    ("content", Ty::Text),
    ("description", Ty::Text),
    ("enabled", Ty::Bool),
    ("created_at", Ty::Text),
    ("updated_at", Ty::Text),
];

const SOURCES: Spec = &[
    ("name", Ty::Text),
    ("url", Ty::Text),
    ("kind", Ty::Text),
    ("category", Ty::Text),
    ("severity", Ty::Text),
    ("interval_secs", Ty::Int),
    ("notify", Ty::Text),
    ("enabled", Ty::Bool),
    ("owner", Ty::Text),
    ("last_polled_at", Ty::Text),
    ("last_status", Ty::Text),
    ("created_at", Ty::Text),
];

const ENTITIES: Spec = &[
    ("type_id", Ty::Text),
    ("title", Ty::Text),
    ("fields", Ty::JsonText),
    ("user_scope", Ty::Text),
    ("origin_entry_id", Ty::Text),
    ("created_by", Ty::Text),
    ("created_at", Ty::Text),
    ("updated_at", Ty::Text),
];

/// v2: standalone inbox rows (journal.append's `inbox` array stays the
/// fan-out path; this spec serves entity.create/update/tombstone).
const INBOX: Spec = &[
    ("recipient", Ty::Text),
    ("from", Ty::Text),
    ("reason", Ty::Text),
    ("ref_kind", Ty::Text),
    ("ref_id", Ty::Text),
    ("entry_id", Ty::Text),
    ("snippet", Ty::Text),
    ("created_at", Ty::Text),
    ("read_at", Ty::Text),
];

/// v2: platform identity mappings (Discord/Telegram/Slack ids → actor slug).
const IDENTITIES: Spec = &[
    ("platform", Ty::Text),
    ("platform_id", Ty::Text),
    ("actor", Ty::Text),
    ("created_at", Ty::Text),
];

/// v2: journal columns entity.update may set (actor merge/delete recipes:
/// author reassignment, mentions scrubs). Creation is journal.append ONLY.
const JOURNAL: Spec = &[
    ("author", Ty::Text),
    ("body", Ty::Text),
    ("tags", Ty::JsonText),
    ("mentions", Ty::JsonText),
    ("user_scope", Ty::Text),
    ("created_at", Ty::Text),
];

const MAIL_ACCOUNTS: Spec = &[
    ("owner", Ty::Text),
    ("address", Ty::Text),
    ("jmap_url", Ty::Text),
    ("jmap_username", Ty::Text),
    ("jmap_account_id", Ty::Text),
    ("cred_id", Ty::Text),
    ("email_state", Ty::Text),
    ("mailbox_state", Ty::Text),
    ("backfill_status", Ty::Text),
    ("backfill_cursor", Ty::JsonText),
    ("attempts", Ty::Int),
    ("next_attempt_at", Ty::Text),
    ("last_error", Ty::Text),
    ("last_synced_at", Ty::Text),
    ("last_status", Ty::Text),
    ("enabled", Ty::Bool),
    ("created_at", Ty::Text),
    ("updated_at", Ty::Text),
];

const MAIL_MAILBOXES: Spec = &[
    ("account_id", Ty::Text),
    ("jmap_id", Ty::Text),
    ("name", Ty::Text),
    ("role", Ty::Text),
    ("ingest", Ty::Bool),
    ("sort_order", Ty::Int),
];

const MAIL_MESSAGES: Spec = &[
    ("account_id", Ty::Text),
    ("jmap_id", Ty::Text),
    ("jmap_thread_id", Ty::Text),
    ("message_id_hdr", Ty::Text),
    ("in_reply_to", Ty::Text),
    ("references_json", Ty::JsonText),
    ("from_addr", Ty::Text),
    ("from_name", Ty::Text),
    ("to_json", Ty::JsonText),
    ("cc_json", Ty::JsonText),
    ("reply_to_json", Ty::JsonText),
    ("subject", Ty::Text),
    ("sent_at", Ty::Text),
    ("received_at", Ty::Text),
    ("mailbox_ids_json", Ty::JsonText),
    ("keywords_json", Ty::JsonText),
    ("body_text", Ty::Text),
    ("body_source", Ty::Text),
    ("snippet", Ty::Text),
    ("size", Ty::Int),
    ("has_attachments", Ty::Bool),
    ("embed_state", Ty::Text),
    ("user_scope", Ty::Text),
    ("deleted_at", Ty::Text),
    ("created_at", Ty::Text),
    ("updated_at", Ty::Text),
];

const MAIL_ATTACHMENTS: Spec = &[
    ("message_id", Ty::Text),
    ("blob_hash", Ty::Text),
    ("jmap_blob_id", Ty::Text),
    ("filename", Ty::Text),
    ("mime", Ty::Text),
    ("size", Ty::Int),
    ("content_id", Ty::Text),
    ("disposition", Ty::Text),
    ("skipped_reason", Ty::Text),
    ("created_at", Ty::Text),
];

/// The mail_accounts columns cursor.set may touch.
const MAIL_CURSOR_COLS: &[&str] = &[
    "email_state",
    "mailbox_state",
    "backfill_status",
    "backfill_cursor",
    "attempts",
    "next_attempt_at",
    "last_error",
    "last_synced_at",
    "last_status",
];

/// Built-in entity kinds for entity.create/entity.update: kind → (table,
/// id column, column spec). Anything else routes to the `entities` table as
/// a custom-instance slug. These names are therefore RESERVED against
/// custom entity-type slugs.
const BUILTIN_ENTITIES: &[(&str, &str, &str, Spec)] = &[
    ("task", "tasks", "id", TASKS),
    ("decision", "decisions", "id", DECISIONS),
    ("event", "events", "id", EVENTS),
    ("topic", "topics", "id", TOPICS),
    ("project", "projects", "id", PROJECTS),
    ("phase", "phases", "id", PHASES),
    ("person", "people", "id", PEOPLE),
    ("profile", "profile", "actor", PROFILE),
    ("entity_type", "entity_types", "id", ENTITY_TYPES),
    ("entity_field", "entity_fields", "id", ENTITY_FIELDS),
    (
        "identity_artifact",
        "identity_artifacts",
        "id",
        IDENTITY_ARTIFACTS,
    ),
    ("source", "sources", "id", SOURCES),
    // v2 additions (see the header's v2 section).
    ("inbox", "inbox", "id", INBOX),
    ("identity", "identities", "id", IDENTITIES),
    ("journal", "journal", "id", JOURNAL),
];

fn builtin(kind: &str) -> Option<(&'static str, &'static str, Spec)> {
    BUILTIN_ENTITIES
        .iter()
        .find(|(k, _, _, _)| *k == kind)
        .map(|(_, table, id_col, spec)| (*table, *id_col, *spec))
}

// ── payload access helpers ───────────────────────────────────────────────────

fn obj<'a>(v: &'a Json, what: &str) -> Result<&'a serde_json::Map<String, Json>> {
    v.as_object()
        .with_context(|| format!("{what} must be a map"))
}

fn need_str<'a>(m: &'a serde_json::Map<String, Json>, key: &str) -> Result<&'a str> {
    m.get(key)
        .and_then(Json::as_str)
        .with_context(|| format!("payload field {key:?} must be a string"))
}

fn opt_str<'a>(m: &'a serde_json::Map<String, Json>, key: &str) -> Result<Option<&'a str>> {
    match m.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s)),
        Some(_) => bail!("payload field {key:?} must be a string or null"),
    }
}

fn reject_unknown(m: &serde_json::Map<String, Json>, allowed: &[&str], what: &str) -> Result<()> {
    for k in m.keys() {
        if !allowed.contains(&k.as_str()) {
            bail!("{what} carries unknown field {k:?} (fail closed)");
        }
    }
    Ok(())
}

/// Bind one JSON value per the column type. Null always binds SQL NULL (a
/// NOT NULL column then fails at execute — fail closed).
fn bind_value(name: &str, ty: Ty, v: &Json) -> Result<SqlValue> {
    if v.is_null() {
        return Ok(SqlValue::Null);
    }
    Ok(match ty {
        Ty::Text => match v {
            Json::String(s) => SqlValue::Text(s.clone()),
            _ => bail!("field {name:?} must be a string"),
        },
        Ty::JsonText => match v {
            Json::String(s) => SqlValue::Text(s.clone()),
            other => SqlValue::Text(serde_json::to_string(other)?),
        },
        Ty::Int => match v.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => bail!("field {name:?} must be an integer"),
        },
        Ty::Bool => match v {
            Json::Bool(b) => SqlValue::Integer(*b as i64),
            Json::Number(n) if n.as_i64() == Some(0) || n.as_i64() == Some(1) => {
                SqlValue::Integer(n.as_i64().unwrap())
            }
            _ => bail!("field {name:?} must be a boolean"),
        },
    })
}

/// (columns, values) for the fields present in `m`, in SPEC order (stable
/// SQL text), rejecting unknown fields.
fn spec_bindings(
    m: &serde_json::Map<String, Json>,
    spec: Spec,
    what: &str,
) -> Result<(Vec<&'static str>, Vec<SqlValue>)> {
    for k in m.keys() {
        if !spec.iter().any(|(name, _)| name == k) {
            bail!("{what} carries unknown column {k:?} (fail closed)");
        }
    }
    let mut cols = Vec::new();
    let mut vals = Vec::new();
    for (name, ty) in spec {
        if let Some(v) = m.get(*name) {
            cols.push(*name);
            vals.push(bind_value(name, *ty, v)?);
        }
    }
    Ok((cols, vals))
}

fn quoted(cols: &[&str]) -> Vec<String> {
    cols.iter().map(|c| format!("\"{c}\"")).collect()
}

/// Strict INSERT of (id + carried fields).
fn insert_row(
    tx: &Transaction,
    table: &str,
    id_col: &str,
    id: &str,
    fields: &serde_json::Map<String, Json>,
    spec: Spec,
) -> Result<()> {
    let (mut cols, mut vals) = spec_bindings(fields, spec, table)?;
    cols.insert(0, "__id__"); // placeholder; replaced by the real id column
    vals.insert(0, SqlValue::Text(id.to_string()));
    let mut names = quoted(&cols);
    names[0] = format!("\"{id_col}\"");
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({})",
        names.join(", "),
        vec!["?"; names.len()].join(", ")
    );
    tx.execute(&sql, rusqlite::params_from_iter(vals))
        .with_context(|| format!("inserting {table} row {id:?}"))?;
    Ok(())
}

/// Strict UPDATE of the carried fields; the row must exist.
fn update_row(
    tx: &Transaction,
    table: &str,
    id_col: &str,
    id: &str,
    fields: &serde_json::Map<String, Json>,
    spec: Spec,
) -> Result<()> {
    let (cols, mut vals) = spec_bindings(fields, spec, table)?;
    if cols.is_empty() {
        bail!("entity.update for {table} row {id:?} carries no fields");
    }
    let sets: Vec<String> = quoted(&cols).iter().map(|c| format!("{c} = ?")).collect();
    vals.push(SqlValue::Text(id.to_string()));
    let sql = format!(
        "UPDATE {table} SET {} WHERE \"{id_col}\" = ?",
        sets.join(", ")
    );
    let n = tx
        .execute(&sql, rusqlite::params_from_iter(vals))
        .with_context(|| format!("updating {table} row {id:?}"))?;
    if n == 0 {
        bail!("entity.update targets missing {table} row {id:?} (create first)");
    }
    Ok(())
}

/// Upsert-by-id (module.doc): carried columns land, absent ones keep DDL
/// defaults (fresh row) or their current values (existing row). Existence is
/// probed first rather than using ON CONFLICT: a delta document carries only
/// the changed columns, and SQLite raises NOT NULL violations on the insert
/// attempt BEFORE the conflict clause could route to the update arm.
fn upsert_row(
    tx: &Transaction,
    table: &str,
    id_col: &str,
    id: &str,
    fields: &serde_json::Map<String, Json>,
    spec: Spec,
) -> Result<()> {
    let exists: bool = tx.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE \"{id_col}\" = ?1)"),
        params![id],
        |r| r.get(0),
    )?;
    if exists {
        let (cols, mut vals) = spec_bindings(fields, spec, table)?;
        if cols.is_empty() {
            return Ok(()); // nothing carried; nothing to change
        }
        let sets: Vec<String> = quoted(&cols).iter().map(|c| format!("{c} = ?")).collect();
        vals.push(SqlValue::Text(id.to_string()));
        tx.execute(
            &format!(
                "UPDATE {table} SET {} WHERE \"{id_col}\" = ?",
                sets.join(", ")
            ),
            rusqlite::params_from_iter(vals),
        )
        .with_context(|| format!("upserting (update arm) {table} row {id:?}"))?;
        Ok(())
    } else {
        insert_row(tx, table, id_col, id, fields, spec)
            .with_context(|| format!("upserting (insert arm) {table} row {id:?}"))
    }
}

// ── FTS maintenance (five kinds: journal, task, decision, event, entities) ──

/// DELETE+INSERT on the `search` content table (the Postgres path's shape);
/// the schema triggers keep the search_fts shadow in lockstep. Body gets the
/// tags appended exactly like store/search.rs index_entity.
fn fts_replace(
    tx: &Transaction,
    kind_s: &str,
    ref_id: &str,
    title: &str,
    body: &str,
    tags: &[String],
) -> Result<()> {
    tx.execute(
        "DELETE FROM search WHERE kind = ?1 AND ref_id = ?2",
        params![kind_s, ref_id],
    )?;
    tx.execute(
        "INSERT INTO search (kind, ref_id, title, body) VALUES (?1, ?2, ?3, ?4)",
        params![kind_s, ref_id, title, format!("{body} {}", tags.join(" "))],
    )?;
    Ok(())
}

fn fts_delete(tx: &Transaction, kind_s: &str, ref_id: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM search WHERE kind = ?1 AND ref_id = ?2",
        params![kind_s, ref_id],
    )?;
    Ok(())
}

/// Drop the persisted vector rows for one item (tombstone/redact): the
/// content is leaving retrieval. The in-memory ANN reconciles at reopen (or
/// tolerates the orphan — see index::SqliteIndex::ann_candidates).
fn embeddings_delete(tx: &Transaction, kind_s: &str, ref_id: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM embeddings WHERE ref_kind = ?1 AND ref_id = ?2",
        params![kind_s, ref_id],
    )?;
    tx.execute(
        "DELETE FROM ann_keys WHERE ref_kind = ?1 AND ref_id = ?2",
        params![kind_s, ref_id],
    )?;
    Ok(())
}

fn json_arr_column(tx: &Transaction, table: &str, id: &str, col: &str) -> Result<Vec<String>> {
    let raw: String = tx.query_row(
        &format!("SELECT \"{col}\" FROM {table} WHERE id = ?1"),
        params![id],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

/// Re-index one FTS-covered row from its CURRENT (post-write) table state,
/// mirroring each store module's index_entity call exactly.
fn fts_refresh(tx: &Transaction, kind_s: &str, id: &str) -> Result<()> {
    match kind_s {
        "journal" => {
            let (author, body): (String, String) = tx.query_row(
                "SELECT author, body FROM journal WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let tags = json_arr_column(tx, "journal", id, "tags")?;
            let title = format!("{author}: {}", hive_shared::snip(&body, 50));
            fts_replace(tx, "journal", id, &title, &body, &tags)
        }
        "task" => {
            let (title, body): (String, String) = tx.query_row(
                "SELECT title, body FROM tasks WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let tags = json_arr_column(tx, "tasks", id, "tags")?;
            fts_replace(tx, "task", id, &title, &body, &tags)
        }
        "decision" => {
            let (title, context, decision, consequences): (String, String, String, String) = tx
                .query_row(
                    "SELECT title, context, decision, consequences FROM decisions WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;
            let tags = json_arr_column(tx, "decisions", id, "tags")?;
            let body = format!("{context} {decision} {consequences}");
            fts_replace(tx, "decision", id, &title, &body, &tags)
        }
        "event" => {
            let (title, body): (String, String) = tx.query_row(
                "SELECT title, body FROM events WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let tags = json_arr_column(tx, "events", id, "tags")?;
            fts_replace(tx, "event", id, &title, &body, &tags)
        }
        // Custom entity instance: searchable text is the Text|Choice|Date
        // field values in entity_fields position order (store/
        // entity_validation.rs searchable_text), joined by newlines.
        custom => {
            let (type_id, title, fields_raw): (String, String, String) = tx.query_row(
                "SELECT type_id, title, fields FROM entities WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let fields: serde_json::Map<String, Json> =
                serde_json::from_str(&fields_raw).unwrap_or_default();
            let mut stmt = tx.prepare(
                "SELECT slug FROM entity_fields WHERE type_id = ?1 \
                 AND field_type IN ('text', 'choice', 'date') \
                 ORDER BY position, created_at",
            )?;
            let slugs: Vec<String> = stmt
                .query_map(params![type_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut parts = Vec::new();
            for slug in &slugs {
                if let Some(s) = fields.get(slug).and_then(Json::as_str) {
                    if !s.is_empty() {
                        parts.push(s.to_string());
                    }
                }
            }
            fts_replace(tx, custom, id, &title, &parts.join("\n"), &[])
        }
    }
}

// ── inbox fan-out (explicit payloads only) ──────────────────────────────────

/// Insert pre-computed inbox rows. The command layer decides recipients,
/// reasons, and snippets — the fold only lands what the record carries.
fn inbox_items(tx: &Transaction, rec: &Record, items: &Json) -> Result<()> {
    let items = items
        .as_array()
        .context("payload field \"inbox\" must be an array")?;
    for item in items {
        let m = obj(item, "inbox item")?;
        reject_unknown(
            m,
            &[
                "id",
                "recipient",
                "from",
                "reason",
                "ref_kind",
                "ref_id",
                "entry_id",
                "snippet",
                "created_at",
            ],
            "inbox item",
        )?;
        tx.execute(
            "INSERT INTO inbox (id, recipient, \"from\", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
            params![
                need_str(m, "id")?,
                need_str(m, "recipient")?,
                need_str(m, "from")?,
                need_str(m, "reason")?,
                need_str(m, "ref_kind")?,
                need_str(m, "ref_id")?,
                opt_str(m, "entry_id")?,
                opt_str(m, "snippet")?.unwrap_or(""),
                opt_str(m, "created_at")?.unwrap_or(&rec.ts),
            ],
        )?;
    }
    Ok(())
}

// ── handlers, one per kind ──────────────────────────────────────────────────

fn journal_append(tx: &Transaction, rec: &Record, payload: &Json) -> Result<()> {
    let m = obj(payload, "journal.append payload")?;
    reject_unknown(
        m,
        &[
            "id",
            "author",
            "body",
            "tags",
            "mentions",
            "user_scope",
            "created_at",
            "anchors",
            "emerged",
            "inbox",
        ],
        "journal.append",
    )?;
    let id = need_str(m, "id")?;
    let created_at = need_str(m, "created_at")?;
    let tags: Vec<String> = m
        .get("tags")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .context("journal.append tags must be a string array")?
        .unwrap_or_default();
    let mentions: Vec<String> = m
        .get("mentions")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .context("journal.append mentions must be a string array")?
        .unwrap_or_default();
    let body = need_str(m, "body")?;
    let author = need_str(m, "author")?;
    tx.execute(
        "INSERT INTO journal (id, author, body, tags, mentions, user_scope, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            author,
            body,
            serde_json::to_string(&tags)?,
            serde_json::to_string(&mentions)?,
            opt_str(m, "user_scope")?,
            created_at
        ],
    )
    .with_context(|| format!("inserting journal row {id:?}"))?;

    let title = format!("{author}: {}", hive_shared::snip(body, 50));
    fts_replace(tx, "journal", id, &title, body, &tags)?;

    if let Some(anchors) = m.get("anchors") {
        let anchors = anchors
            .as_array()
            .context("journal.append anchors must be an array")?;
        for a in anchors {
            let am = obj(a, "anchor")?;
            reject_unknown(
                am,
                &["id", "start", "end", "text", "kind", "ref_id", "created_at"],
                "anchor",
            )?;
            tx.execute(
                r#"INSERT INTO anchors (id, entry_id, start, "end", text, kind, ref_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
                params![
                    need_str(am, "id")?,
                    id,
                    am.get("start").and_then(Json::as_i64).context("anchor start must be an integer")?,
                    am.get("end").and_then(Json::as_i64).context("anchor end must be an integer")?,
                    need_str(am, "text")?,
                    need_str(am, "kind")?,
                    need_str(am, "ref_id")?,
                    opt_str(am, "created_at")?.unwrap_or(created_at),
                ],
            )?;
        }
    }

    if let Some(emerged) = m.get("emerged") {
        let emerged = emerged
            .as_array()
            .context("journal.append emerged must be an array")?;
        for e in emerged {
            entity_create_inner(tx, e).context("applying emerged entity-create")?;
        }
    }

    if let Some(items) = m.get("inbox") {
        inbox_items(tx, rec, items)?;
    }
    Ok(())
}

/// The shared create path: top-level entity.create records AND the
/// pre-materialized `emerged` elements of journal.append.
fn entity_create_inner(tx: &Transaction, payload: &Json) -> Result<()> {
    let m = obj(payload, "entity.create payload")?;
    reject_unknown(m, &["kind", "id", "fields", "inbox"], "entity.create")?;
    let kind_s = need_str(m, "kind")?;
    if kind_s == "journal" {
        bail!("journal rows are created by journal.append, never entity.create");
    }
    let id = need_str(m, "id")?;
    let empty = serde_json::Map::new();
    let fields = match m.get("fields") {
        Some(v) => obj(v, "entity.create fields")?,
        None => &empty,
    };
    match builtin(kind_s) {
        Some((table, id_col, spec)) => {
            insert_row(tx, table, id_col, id, fields, spec)?;
            if matches!(kind_s, "task" | "decision" | "event") {
                fts_refresh(tx, kind_s, id)?;
            }
        }
        None => {
            insert_row(tx, "entities", "id", id, fields, ENTITIES)?;
            fts_refresh(tx, kind_s, id)?;
        }
    }
    Ok(())
}

fn entity_create(tx: &Transaction, rec: &Record, payload: &Json) -> Result<()> {
    entity_create_inner(tx, payload)?;
    if let Some(items) = obj(payload, "entity.create payload")?.get("inbox") {
        inbox_items(tx, rec, items)?;
    }
    Ok(())
}

fn entity_update(tx: &Transaction, payload: &Json) -> Result<()> {
    let m = obj(payload, "entity.update payload")?;
    reject_unknown(m, &["kind", "id", "fields"], "entity.update")?;
    let kind_s = need_str(m, "kind")?;
    let id = need_str(m, "id")?;
    let fields = obj(
        m.get("fields").context("entity.update requires fields")?,
        "entity.update fields",
    )?;
    match builtin(kind_s) {
        Some((table, id_col, spec)) => {
            update_row(tx, table, id_col, id, fields, spec)?;
            if matches!(kind_s, "task" | "decision" | "event") {
                fts_refresh(tx, kind_s, id)?;
            }
        }
        None => {
            // Custom instance: the inner JSON column merges per field (null
            // removes a key — merge_fields parity); other columns set direct.
            let mut direct = fields.clone();
            let patch = direct.remove("fields");
            if let Some(patch) = patch {
                let patch = patch
                    .as_object()
                    .context("entity.update fields.fields must be a map")?
                    .clone();
                let current_raw: String = tx
                    .query_row(
                        "SELECT fields FROM entities WHERE id = ?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .optional()?
                    .with_context(|| {
                        format!("entity.update targets missing entities row {id:?} (create first)")
                    })?;
                let mut merged: serde_json::Map<String, Json> =
                    serde_json::from_str(&current_raw).unwrap_or_default();
                for (k, v) in patch {
                    if v.is_null() {
                        merged.remove(&k);
                    } else {
                        merged.insert(k, v);
                    }
                }
                direct.insert(
                    "fields".into(),
                    Json::String(serde_json::to_string(&merged)?),
                );
            }
            update_row(tx, "entities", "id", id, &direct, ENTITIES)?;
            fts_refresh(tx, kind_s, id)?;
        }
    }
    Ok(())
}

fn link_add(tx: &Transaction, rec: &Record, payload: &Json) -> Result<()> {
    let m = obj(payload, "link.add payload")?;
    reject_unknown(
        m,
        &[
            "id",
            "source_kind",
            "source_id",
            "rel",
            "target_kind",
            "target_id",
            "created_at",
        ],
        "link.add",
    )?;
    let fallback_id = format!("link_{}-{:016x}", rec.device, rec.seq);
    tx.execute(
        "INSERT INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            opt_str(m, "id")?.unwrap_or(&fallback_id),
            need_str(m, "source_kind")?,
            need_str(m, "source_id")?,
            need_str(m, "target_kind")?,
            need_str(m, "target_id")?,
            opt_str(m, "rel")?.unwrap_or("relates"),
            opt_str(m, "created_at")?.unwrap_or(&rec.ts),
        ],
    )?;
    Ok(())
}

fn link_remove(tx: &Transaction, payload: &Json) -> Result<()> {
    let m = obj(payload, "link.remove payload")?;
    reject_unknown(
        m,
        &[
            "id",
            "source_kind",
            "source_id",
            "rel",
            "target_kind",
            "target_id",
        ],
        "link.remove",
    )?;
    if let Some(id) = opt_str(m, "id")? {
        tx.execute("DELETE FROM links WHERE id = ?1", params![id])?;
        return Ok(());
    }
    match opt_str(m, "rel")? {
        Some(rel) => tx.execute(
            "DELETE FROM links WHERE source_kind = ?1 AND source_id = ?2 \
             AND target_kind = ?3 AND target_id = ?4 AND rel = ?5",
            params![
                need_str(m, "source_kind")?,
                need_str(m, "source_id")?,
                need_str(m, "target_kind")?,
                need_str(m, "target_id")?,
                rel
            ],
        )?,
        None => tx.execute(
            "DELETE FROM links WHERE source_kind = ?1 AND source_id = ?2 \
             AND target_kind = ?3 AND target_id = ?4",
            params![
                need_str(m, "source_kind")?,
                need_str(m, "source_id")?,
                need_str(m, "target_kind")?,
                need_str(m, "target_id")?
            ],
        )?,
    };
    Ok(())
}

fn tombstone(tx: &Transaction, rec: &Record, payload: &Json) -> Result<()> {
    let m = obj(payload, "tombstone payload")?;
    reject_unknown(m, &["kind", "id"], "tombstone")?;
    let kind_s = need_str(m, "kind")?;
    let id = need_str(m, "id")?;
    match kind_s {
        "journal" => {
            tx.execute("DELETE FROM journal WHERE id = ?1", params![id])?;
            tx.execute("DELETE FROM anchors WHERE entry_id = ?1", params![id])?;
            fts_delete(tx, "journal", id)?;
            embeddings_delete(tx, "journal", id)?;
        }
        "task" | "decision" | "event" => {
            let table = builtin(kind_s).expect("built-in").0;
            tx.execute(&format!("DELETE FROM {table} WHERE id = ?1"), params![id])?;
            fts_delete(tx, kind_s, id)?;
            embeddings_delete(tx, kind_s, id)?;
        }
        "topic" | "project" | "phase" | "person" | "entity_type" | "entity_field"
        | "identity_artifact" | "source" | "inbox" | "identity" => {
            let table = builtin(kind_s).expect("built-in").0;
            tx.execute(&format!("DELETE FROM {table} WHERE id = ?1"), params![id])?;
        }
        "profile" => {
            tx.execute("DELETE FROM profile WHERE actor = ?1", params![id])?;
        }
        // Mail message: SOFT delete — the (account_id, jmap_id) key must
        // survive so a sync replay lands on the existing row instead of
        // resurrecting content (store/mail.rs rule). Content leaves
        // retrieval: attachments metadata, FTS, vectors.
        "mail" => {
            tx.execute(
                "UPDATE mail_messages SET deleted_at = ?1, updated_at = ?1 WHERE id = ?2",
                params![rec.ts, id],
            )?;
            tx.execute(
                "DELETE FROM mail_attachments WHERE message_id = ?1",
                params![id],
            )?;
            fts_delete(tx, "mail", id)?;
            embeddings_delete(tx, "mail", id)?;
        }
        "mail_account" => {
            // Search/vector rows of its messages first; the account row's
            // deletion then cascades mailboxes → messages → attachments.
            let mut stmt = tx.prepare("SELECT id FROM mail_messages WHERE account_id = ?1")?;
            let ids: Vec<String> = stmt
                .query_map(params![id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(stmt);
            for mid in &ids {
                fts_delete(tx, "mail", mid)?;
                embeddings_delete(tx, "mail", mid)?;
            }
            tx.execute("DELETE FROM mail_accounts WHERE id = ?1", params![id])?;
        }
        "mail_mailbox" => {
            tx.execute("DELETE FROM mail_mailboxes WHERE id = ?1", params![id])?;
        }
        custom => {
            tx.execute("DELETE FROM entities WHERE id = ?1", params![id])?;
            fts_delete(tx, custom, id)?;
            embeddings_delete(tx, custom, id)?;
        }
    }
    Ok(())
}

/// Redactable columns per kind: (column, cleared value). See module header.
fn redact_spec(
    kind_s: &str,
) -> (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
) {
    match kind_s {
        "journal" => ("journal", "id", &[("body", "''")]),
        "task" => ("tasks", "id", &[("body", "''"), ("anchor_text", "NULL")]),
        "decision" => (
            "decisions",
            "id",
            &[
                ("context", "''"),
                ("decision", "''"),
                ("consequences", "''"),
                ("anchor_text", "NULL"),
            ],
        ),
        "event" => ("events", "id", &[("body", "''"), ("anchor_text", "NULL")]),
        "person" => ("people", "id", &[("bio", "NULL")]),
        "profile" => ("profile", "actor", &[("body", "'{}'")]),
        "mail" => (
            "mail_messages",
            "id",
            &[("subject", "''"), ("body_text", "''"), ("snippet", "''")],
        ),
        _ => ("entities", "id", &[("fields", "'{}'")]),
    }
}

fn redact(tx: &Transaction, payload: &Json) -> Result<()> {
    let m = obj(payload, "redact payload")?;
    reject_unknown(m, &["kind", "id", "fields"], "redact")?;
    let kind_s = need_str(m, "kind")?;
    let id = need_str(m, "id")?;
    let (table, id_col, spec) = redact_spec(kind_s);

    let chosen: Vec<&(&str, &str)> = match m.get("fields") {
        None | Some(Json::Null) => spec.iter().collect(),
        Some(v) => {
            let names = v
                .as_array()
                .context("redact fields must be an array of column names")?;
            let mut picked = Vec::new();
            for n in names {
                let n = n
                    .as_str()
                    .context("redact fields entries must be strings")?;
                let hit = spec.iter().find(|(col, _)| *col == n).with_context(|| {
                    format!("column {n:?} is not redactable for kind {kind_s:?}")
                })?;
                picked.push(hit);
            }
            picked
        }
    };
    if chosen.is_empty() {
        bail!("redact resolved to no columns");
    }
    let sets: Vec<String> = chosen
        .iter()
        .map(|(col, cleared)| format!("\"{col}\" = {cleared}"))
        .collect();
    let n = tx.execute(
        &format!(
            "UPDATE {table} SET {} WHERE \"{id_col}\" = ?1",
            sets.join(", ")
        ),
        params![id],
    )?;
    if n == 0 {
        bail!("redact targets missing {table} row {id:?}");
    }
    // Redacted content leaves retrieval: vectors always; FTS re-indexed from
    // the cleared row for fold-maintained kinds, dropped outright for mail
    // (its FTS rows are not fold-maintained in 1.5).
    embeddings_delete(tx, kind_s, id)?;
    match kind_s {
        "journal" | "task" | "decision" | "event" => fts_refresh(tx, kind_s, id)?,
        "person" | "profile" => {}
        "mail" => fts_delete(tx, "mail", id)?,
        custom => fts_refresh(tx, custom, id)?,
    }
    Ok(())
}

fn config_set(tx: &Transaction, rec: &Record, payload: &Json) -> Result<()> {
    let m = obj(payload, "config.set payload")?;
    reject_unknown(m, &["key", "value"], "config.set")?;
    tx.execute(
        "INSERT INTO config (key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![need_str(m, "key")?, need_str(m, "value")?, rec.ts],
    )?;
    Ok(())
}

fn module_doc(tx: &Transaction, payload: &Json) -> Result<()> {
    let m = obj(payload, "module.doc payload")?;
    reject_unknown(m, &["module", "doc_kind", "id", "fields"], "module.doc")?;
    let module = need_str(m, "module")?;
    let doc_kind = need_str(m, "doc_kind")?;
    let id = need_str(m, "id")?;
    let empty = serde_json::Map::new();
    let fields = match m.get("fields") {
        Some(v) => obj(v, "module.doc fields")?,
        None => &empty,
    };
    let (table, spec) = match (module, doc_kind) {
        ("mail", "account") => ("mail_accounts", MAIL_ACCOUNTS),
        ("mail", "mailbox") => ("mail_mailboxes", MAIL_MAILBOXES),
        ("mail", "message") => ("mail_messages", MAIL_MESSAGES),
        ("mail", "attachment") => ("mail_attachments", MAIL_ATTACHMENTS),
        _ => bail!("module.doc has no route for module {module:?} doc_kind {doc_kind:?}"),
    };
    upsert_row(tx, table, "id", id, fields, spec)
}

fn cursor_set(tx: &Transaction, rec: &Record, payload: &Json) -> Result<()> {
    let m = obj(payload, "cursor.set payload")?;
    reject_unknown(m, &["module", "account", "cursor"], "cursor.set")?;
    let module = need_str(m, "module")?;
    if module != "mail" {
        bail!("cursor.set has no route for module {module:?}");
    }
    let account = need_str(m, "account")?;
    let cursor = obj(
        m.get("cursor").context("cursor.set requires cursor")?,
        "cursor.set cursor",
    )?;
    let mut sets = Vec::new();
    let mut vals: Vec<SqlValue> = Vec::new();
    for (key, v) in cursor {
        if !MAIL_CURSOR_COLS.contains(&key.as_str()) {
            bail!("cursor.set carries unknown cursor key {key:?}");
        }
        let ty = MAIL_ACCOUNTS
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, ty)| *ty)
            .expect("cursor cols are account cols");
        sets.push(format!("\"{key}\" = ?"));
        vals.push(bind_value(key, ty, v)?);
    }
    if sets.is_empty() {
        bail!("cursor.set carries an empty cursor");
    }
    sets.push("updated_at = ?".to_string());
    vals.push(SqlValue::Text(rec.ts.clone()));
    vals.push(SqlValue::Text(account.to_string()));
    let n = tx.execute(
        &format!("UPDATE mail_accounts SET {} WHERE id = ?", sets.join(", ")),
        rusqlite::params_from_iter(vals),
    )?;
    if n == 0 {
        bail!("cursor.set targets missing mail account {account:?}");
    }
    Ok(())
}

fn alias(tx: &Transaction, rec: &Record, payload: &Json) -> Result<()> {
    let m = obj(payload, "alias payload")?;
    reject_unknown(m, &["from", "to", "namespace", "created_at"], "alias")?;
    tx.execute(
        "INSERT INTO aliases (namespace, \"from\", \"to\", created_at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (namespace, \"from\") DO UPDATE SET \"to\" = excluded.\"to\", created_at = excluded.created_at",
        params![
            need_str(m, "namespace")?,
            need_str(m, "from")?,
            need_str(m, "to")?,
            opt_str(m, "created_at")?.unwrap_or(&rec.ts),
        ],
    )?;
    Ok(())
}
