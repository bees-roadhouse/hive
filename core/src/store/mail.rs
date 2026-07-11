// Mail archive: read paths on the index; write paths as records per the fold
// contract — module.doc (account/mailbox/message/attachment upserts),
// cursor.set (sync state), tombstone (JMAP destroys; soft delete in the
// fold), redact (admin scrub). The sync daemon is PAUSED until the Phase 3
// mail module, but every path compiles and is fold-tested.
//
// Command-layer responsibilities the fold refuses on purpose:
//   - mail FTS membership (ingest mailboxes ∩ not junk): direct writes to the
//     `search` table here (mail search rows do NOT rebuild from replay; the
//     Phase 3 resync re-creates them);
//   - embeddings drops (via SqliteIndex so the ANN forgets too);
//   - attachment BYTES: the blockstore, with blob_refs (runtime table)
//     holding hash → serialized BlobRef. mail_attachments.blob_hash names
//     rows there.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::json;

use super::cc_credentials::NewCcCredential;
use super::{new_id, now_iso, Core, Draft, Store};

#[derive(Debug, Clone, Serialize)]
pub struct MailAccount {
    pub id: String,
    pub label: String,
    pub address: String,
    pub provider: Option<String>,
    pub last_synced_at: Option<String>,
}

/// Management view: sync state + error surface, never secrets.
#[derive(Debug, Clone, Serialize)]
pub struct MailAccountAdminView {
    pub id: String,
    pub owner: String,
    pub address: String,
    pub jmap_url: String,
    pub jmap_username: Option<String>,
    pub jmap_account_id: String,
    pub backfill_status: String,
    pub enabled: bool,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub last_synced_at: Option<String>,
    pub last_status: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MailMailboxView {
    pub id: String,
    pub jmap_id: String,
    pub name: String,
    pub role: Option<String>,
    pub sort_order: i64,
    pub ingest: bool,
}

#[derive(Debug, Clone)]
struct MailMessageRow {
    pub id: String,
    pub account_id: String,
    pub thread_id: String,
    pub labels_json: String,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_email: String,
    pub to_json: String,
    pub cc_json: String,
    pub received_at: String,
    pub snippet: String,
    pub body_text: String,
    pub has_attachments: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MailMessageSummary {
    pub id: String,
    pub thread_id: String,
    pub account_id: String,
    pub labels: Vec<String>,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub snippet: Option<String>,
    pub received_at: String,
    pub has_attachments: bool,
}

/// Lightweight attachment row for thread payloads: enough to render a chip
/// and link the serving path. `stored` = bytes are in the local blockstore
/// (false = oversize/missing/pending — a fetch would 404).
#[derive(Debug, Clone, Serialize)]
pub struct MailAttachmentChip {
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub size: i64,
    pub stored: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MailThreadMessage {
    #[serde(flatten)]
    pub summary: MailMessageSummary,
    pub body_text: String,
    pub attachments: Vec<MailAttachmentChip>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MailThread {
    pub thread_id: String,
    pub subject: String,
    pub messages: Vec<MailThreadMessage>,
}

impl From<MailMessageRow> for MailThreadMessage {
    fn from(row: MailMessageRow) -> Self {
        let from = row
            .from_name
            .filter(|name| !name.trim().is_empty())
            .map(|name| format!("{name} <{}>", row.from_email))
            .unwrap_or_else(|| row.from_email.clone());
        let summary = MailMessageSummary {
            id: row.id,
            thread_id: row.thread_id,
            account_id: row.account_id,
            labels: mail_labels(&row.labels_json),
            from,
            to: json_string_array(&row.to_json),
            cc: json_string_array(&row.cc_json),
            subject: row.subject,
            snippet: if row.snippet.is_empty() {
                None
            } else {
                Some(row.snippet)
            },
            received_at: row.received_at,
            has_attachments: row.has_attachments,
        };
        Self {
            summary,
            body_text: row.body_text,
            attachments: Vec::new(),
        }
    }
}

fn mail_labels(raw: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let mut labels: Vec<String> = match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(label_display))
            .collect(),
        serde_json::Value::Object(map) => map
            .into_iter()
            .filter_map(|(k, v)| {
                if v.as_bool().unwrap_or(!v.is_null()) {
                    Some(label_display(&k))
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    labels.sort_by_key(|label| (label_rank(label), label.to_lowercase()));
    labels.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    labels
}

fn label_display(raw: &str) -> String {
    match raw.trim() {
        "$seen" | "seen" => "seen".to_string(),
        "$draft" | "draft" => "draft".to_string(),
        "$flagged" | "flagged" => "flagged".to_string(),
        "$answered" | "answered" => "answered".to_string(),
        "$forwarded" | "forwarded" => "forwarded".to_string(),
        other => other.trim_start_matches('$').replace(['_', '-'], " "),
    }
}

fn label_rank(label: &str) -> u8 {
    match label {
        "flagged" => 0,
        "draft" => 1,
        "answered" | "forwarded" => 2,
        "seen" => 9,
        _ => 4,
    }
}

fn json_string_array(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw)
        .or_else(|_| {
            serde_json::from_str::<Vec<serde_json::Value>>(raw).map(|items| {
                items
                    .into_iter()
                    .filter_map(|v| {
                        v.as_str().map(ToOwned::to_owned).or_else(|| {
                            v.get("email")
                                .and_then(|e| e.as_str())
                                .map(ToOwned::to_owned)
                        })
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn row_to_message(r: &rusqlite::Row) -> rusqlite::Result<MailMessageRow> {
    Ok(MailMessageRow {
        id: r.get("id")?,
        account_id: r.get("account_id")?,
        thread_id: r.get("thread_id")?,
        labels_json: r.get("labels_json")?,
        subject: r.get("subject")?,
        from_name: r.get("from_name")?,
        from_email: r.get("from_email")?,
        to_json: r.get("to_json")?,
        cc_json: r.get("cc_json")?,
        received_at: r.get("received_at")?,
        snippet: r.get("snippet")?,
        body_text: r.get("body_text")?,
        has_attachments: r.get("has_attachments")?,
    })
}

fn row_to_admin_view(r: &rusqlite::Row) -> rusqlite::Result<MailAccountAdminView> {
    Ok(MailAccountAdminView {
        id: r.get("id")?,
        owner: r.get("owner")?,
        address: r.get("address")?,
        jmap_url: r.get("jmap_url")?,
        jmap_username: r.get("jmap_username")?,
        jmap_account_id: r.get("jmap_account_id")?,
        backfill_status: r.get("backfill_status")?,
        enabled: r.get("enabled")?,
        attempts: r.get("attempts")?,
        last_error: r.get("last_error")?,
        last_synced_at: r.get("last_synced_at")?,
        last_status: r.get("last_status")?,
        created_at: r.get("created_at")?,
    })
}

impl Store {
    pub async fn mail_accounts_list(&self) -> Result<Vec<MailAccount>> {
        self.run(|core| {
            let mut stmt = core.conn().prepare(
                "SELECT id, address AS label, address, 'jmap' AS provider, last_synced_at \
                 FROM mail_accounts ORDER BY owner ASC, address ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(MailAccount {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    address: r.get(2)?,
                    provider: r.get(3)?,
                    last_synced_at: r.get(4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn mail_messages_list(
        &self,
        query: Option<&str>,
        account_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<MailMessageSummary>> {
        let query = query.map(str::to_string);
        let account_id = account_id.map(str::to_string);
        self.run(move |core| {
            let rows = mail_message_rows(core, query.as_deref(), account_id.as_deref(), limit)?;
            Ok(rows
                .into_iter()
                .map(MailThreadMessage::from)
                .map(|m| m.summary)
                .collect())
        })
        .await
    }

    pub async fn mail_search(&self, query: &str, limit: i64) -> Result<Vec<MailMessageSummary>> {
        self.mail_messages_list(Some(query), None, limit).await
    }

    pub async fn mail_thread_get(&self, thread_id: &str) -> Result<MailThread> {
        let thread_id = thread_id.to_string();
        self.run(move |core| {
            let rows: Vec<MailMessageRow> = {
                let sql =
                    mail_message_select("WHERE m.jmap_thread_id = ?1 ORDER BY m.received_at ASC");
                let mut stmt = core.conn().prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params![thread_id], row_to_message)?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut messages: Vec<MailThreadMessage> =
                rows.into_iter().map(MailThreadMessage::from).collect();
            attach_chips(core, &mut messages)?;
            let subject = messages
                .first()
                .map(|m| m.summary.subject.clone())
                .unwrap_or_default();
            Ok(MailThread {
                thread_id,
                subject,
                messages,
            })
        })
        .await
    }

    // ---- account management (the connect surface; the mail sync driver —
    // a Phase 3 module — owns sync) ----

    /// Register a mail account: the credential lands in the AES-GCM vault
    /// (which hard-requires HIVE_CRED_KEY) and the account row starts
    /// 'pending' for mail sync to pick up. The caller has already validated
    /// the credential against the server and captured `jmap_account_id`.
    #[allow(clippy::too_many_arguments)]
    pub async fn mail_account_create(
        &self,
        owner: &str,
        address: &str,
        jmap_url: &str,
        jmap_username: Option<&str>,
        jmap_account_id: &str,
        secret: &str,
    ) -> Result<MailAccountAdminView> {
        let exists = {
            let (owner_s, address_s) = (owner.to_string(), address.to_string());
            self.run(move |core| {
                Ok(core
                    .conn()
                    .query_row(
                        "SELECT id FROM mail_accounts WHERE owner = ?1 AND address = ?2",
                        rusqlite::params![owner_s, address_s],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .await?
        };
        if exists.is_some() {
            return Err(anyhow!(
                "mail account {address} is already connected for {owner}"
            ));
        }
        let cred = self
            .cc_cred_put(
                owner,
                NewCcCredential {
                    kind: "password".to_string(),
                    runtime: Some("jmap".to_string()),
                    provider: Some("stalwart".to_string()),
                    label: Some(address.to_string()),
                    secret: secret.to_string(),
                },
            )
            .await?;
        let id = new_id("macct");
        let ts = now_iso();
        let draft = Draft::new(
            crate::oplog::kind::MODULE_DOC,
            owner,
            &ts,
            json!({"module": "mail", "doc_kind": "account", "id": id, "fields": {
                "owner": owner, "address": address, "jmap_url": jmap_url,
                "jmap_username": jmap_username, "jmap_account_id": jmap_account_id,
                "cred_id": cred.id, "created_at": ts, "updated_at": ts,
            }}),
        );
        let id_c = id.clone();
        let view = self
            .run(move |core| {
                core.commit(vec![draft])?;
                mail_account_admin_view_core(core, &id_c)
            })
            .await?
            .ok_or_else(|| anyhow!("account {id} vanished after insert"))?;
        // ids only on the wire: it is globally readable (D10).
        self.emit(
            "mail.account.connected",
            owner,
            serde_json::json!({"id": id}),
        )
        .await?;
        Ok(view)
    }

    pub async fn mail_account_admin_view(&self, id: &str) -> Result<Option<MailAccountAdminView>> {
        let id = id.to_string();
        self.run(move |core| mail_account_admin_view_core(core, &id))
            .await
    }

    /// All accounts, with sync state.
    pub async fn mail_accounts_admin_list(&self) -> Result<Vec<MailAccountAdminView>> {
        self.run(|core| {
            let mut stmt = core.conn().prepare(&format!(
                "{MAIL_ACCOUNT_ADMIN_SELECT} ORDER BY owner ASC, address ASC"
            ))?;
            let rows = stmt.query_map([], row_to_admin_view)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn mail_account_owner(&self, id: &str) -> Result<Option<String>> {
        let id = id.to_string();
        self.run(move |core| mail_account_owner_core(core, &id))
            .await
    }

    /// Delete an account and everything derived from it. The account
    /// tombstone's fold cascade drops mailboxes → messages → attachments and
    /// their FTS/vector rows; inbox/link rows and the vault credential and
    /// orphaned blobs go explicitly — one batch.
    pub async fn mail_account_delete(&self, id: &str) -> Result<bool> {
        let id_s = id.to_string();
        let owner = self
            .run(move |core| {
                let Some(owner) = mail_account_owner_core(core, &id_s)? else {
                    return Ok(None);
                };
                let ts = now_iso();
                let mut drafts: Vec<Draft> = Vec::new();
                let msg_ids: Vec<String> = {
                    let mut stmt = core
                        .conn()
                        .prepare("SELECT id FROM mail_messages WHERE account_id = ?1")?;
                    let rows = stmt.query_map(rusqlite::params![id_s], |r| r.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                for mid in &msg_ids {
                    let inbox_ids: Vec<String> = {
                        let mut stmt = core.conn().prepare(
                            "SELECT id FROM inbox WHERE ref_kind = 'mail' AND ref_id = ?1",
                        )?;
                        let rows = stmt.query_map(rusqlite::params![mid], |r| r.get(0))?;
                        rows.collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    for iid in inbox_ids {
                        drafts.push(Draft::new(
                            crate::oplog::kind::TOMBSTONE,
                            &owner,
                            &ts,
                            json!({"kind": "inbox", "id": iid}),
                        ));
                    }
                    let link_ids: Vec<String> = {
                        let mut stmt = core.conn().prepare(
                            "SELECT id FROM links WHERE (source_kind = 'mail' AND source_id = ?1) \
                             OR (target_kind = 'mail' AND target_id = ?1)",
                        )?;
                        let rows = stmt.query_map(rusqlite::params![mid], |r| r.get(0))?;
                        rows.collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    for lid in link_ids {
                        drafts.push(super::links::link_remove_draft(&lid, &ts));
                    }
                }
                let cred_id: Option<String> = core
                    .conn()
                    .query_row(
                        "SELECT cred_id FROM mail_accounts WHERE id = ?1",
                        rusqlite::params![id_s],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                drafts.push(Draft::new(
                    crate::oplog::kind::TOMBSTONE,
                    &owner,
                    &ts,
                    json!({"kind": "mail_account", "id": id_s}),
                ));
                core.commit(drafts)?;
                if let Some(cred) = cred_id {
                    core.conn().execute(
                        "DELETE FROM cc_credentials WHERE id = ?1",
                        rusqlite::params![cred],
                    )?;
                }
                gc_orphan_blobs(core, None)?;
                Ok(Some(owner))
            })
            .await?;
        let Some(owner) = owner else {
            return Ok(false);
        };
        self.emit(
            "mail.account.deleted",
            &owner,
            serde_json::json!({"id": id}),
        )
        .await?;
        Ok(true)
    }

    /// Enabling clears the backoff so mail sync picks the account up on its
    /// next tick instead of waiting out a stale next_attempt_at.
    pub async fn mail_account_set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let id = id.to_string();
        self.run(move |core| {
            if mail_account_owner_core(core, &id)?.is_none() {
                return Ok(false);
            }
            let ts = now_iso();
            let mut drafts = vec![Draft::new(
                crate::oplog::kind::MODULE_DOC,
                "admin",
                &ts,
                json!({"module": "mail", "doc_kind": "account", "id": id, "fields": {
                    "enabled": enabled, "updated_at": ts,
                }}),
            )];
            drafts.push(Draft::new(
                crate::oplog::kind::CURSOR_SET,
                "admin",
                &ts,
                if enabled {
                    json!({"module": "mail", "account": id, "cursor": {
                        "attempts": 0, "next_attempt_at": null,
                    }})
                } else {
                    json!({"module": "mail", "account": id, "cursor": {
                        "next_attempt_at": null,
                    }})
                },
            ));
            core.commit(drafts)?;
            Ok(true)
        })
        .await
    }

    /// Force a full reconciliation: the sentinel state string makes the next
    /// Email/changes call fail cannotCalculateChanges, which is the resync
    /// path (a bogus state is the ONLY way to route there deliberately —
    /// clearing the state would just capture a fresh one and silently skip
    /// interim changes).
    pub async fn mail_account_force_resync(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.run(move |core| {
            if mail_account_owner_core(core, &id)?.is_none() {
                return Ok(false);
            }
            core.commit(vec![Draft::new(
                crate::oplog::kind::CURSOR_SET,
                "admin",
                &now_iso(),
                json!({"module": "mail", "account": id, "cursor": {
                    "email_state": "force-resync", "attempts": 0,
                    "next_attempt_at": null, "last_error": null,
                }}),
            )])?;
            Ok(true)
        })
        .await
    }

    pub async fn mail_mailbox_owner(&self, mailbox_id: &str) -> Result<Option<String>> {
        let mailbox_id = mailbox_id.to_string();
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    "SELECT a.owner FROM mail_mailboxes b JOIN mail_accounts a ON a.id = b.account_id \
                     WHERE b.id = ?1",
                    rusqlite::params![mailbox_id],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
    }

    pub async fn mail_mailboxes_list(&self, account_id: &str) -> Result<Vec<MailMailboxView>> {
        let account_id = account_id.to_string();
        self.run(move |core| {
            let mut stmt = core.conn().prepare(
                "SELECT id, jmap_id, name, role, sort_order, ingest FROM mail_mailboxes \
                 WHERE account_id = ?1 ORDER BY sort_order ASC, name ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![account_id], |r| {
                Ok(MailMailboxView {
                    id: r.get(0)?,
                    jmap_id: r.get(1)?,
                    name: r.get(2)?,
                    role: r.get(3)?,
                    sort_order: r.get(4)?,
                    ingest: r.get(5)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// The per-mailbox opt-in (the spam gate). Turning a mailbox ON resets
    /// the account's backfill to 'pending' so history gets picked up (the
    /// unique key makes the re-run duplicate-free). Turning it OFF drops the
    /// mailbox's messages out of retrieval immediately (D6 semantics: rows
    /// stay, search/embedding membership goes).
    pub async fn mail_mailbox_set_ingest(&self, mailbox_id: &str, ingest: bool) -> Result<bool> {
        let mailbox_id = mailbox_id.to_string();
        self.run(move |core| {
            let row: Option<(String, String)> = core
                .conn()
                .query_row(
                    "SELECT account_id, jmap_id FROM mail_mailboxes WHERE id = ?1",
                    rusqlite::params![mailbox_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((account_id, jmap_id)) = row else {
                return Ok(false);
            };
            let ts = now_iso();
            let mut drafts = vec![Draft::new(
                crate::oplog::kind::MODULE_DOC,
                "admin",
                &ts,
                json!({"module": "mail", "doc_kind": "mailbox", "id": mailbox_id, "fields": {
                    "ingest": ingest,
                }}),
            )];
            let mut affected: Vec<String> = Vec::new();
            if ingest {
                drafts.push(Draft::new(
                    crate::oplog::kind::CURSOR_SET,
                    "admin",
                    &ts,
                    json!({"module": "mail", "account": account_id, "cursor": {
                        "backfill_status": "pending",
                    }}),
                ));
            } else {
                // mailbox_ids_json is a JSON array of jmap ids; the quoted-id
                // containment match is exact enough (ids are server-issued and
                // never substrings of each other in practice).
                let needle = format!("%\"{jmap_id}\"%");
                affected = {
                    let mut stmt = core.conn().prepare(
                        "SELECT id FROM mail_messages WHERE account_id = ?1 AND mailbox_ids_json LIKE ?2",
                    )?;
                    let rows =
                        stmt.query_map(rusqlite::params![account_id, needle], |r| r.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                for mid in &affected {
                    drafts.push(Draft::new(
                        crate::oplog::kind::MODULE_DOC,
                        "admin",
                        &ts,
                        json!({"module": "mail", "doc_kind": "message", "id": mid, "fields": {
                            "embed_state": "skip",
                        }}),
                    ));
                }
            }
            core.commit(drafts)?;
            if !ingest {
                // Retrieval drop is command-layer business (mail FTS is not
                // fold-maintained): search rows + vectors leave immediately.
                for mid in &affected {
                    core.conn().execute(
                        "DELETE FROM search WHERE kind = 'mail' AND ref_id = ?1",
                        rusqlite::params![mid],
                    )?;
                    core.index.remove_embeddings("mail", mid)?;
                }
            }
            Ok(true)
        })
        .await
    }
}

const MAIL_ACCOUNT_ADMIN_SELECT: &str =
    "SELECT id, owner, address, jmap_url, jmap_username, jmap_account_id, backfill_status, \
     enabled, attempts, last_error, last_synced_at, last_status, created_at FROM mail_accounts";

fn mail_message_select(suffix: &str) -> String {
    format!(
        "SELECT m.id, m.account_id, m.jmap_thread_id AS thread_id, m.keywords_json AS labels_json, \
         m.subject, m.from_name, m.from_addr AS from_email, m.to_json, m.cc_json, \
         m.received_at, m.snippet, m.body_text, m.has_attachments \
         FROM mail_messages m \
         JOIN mail_accounts a ON a.id = m.account_id {suffix}"
    )
}

fn mail_account_owner_core(core: &Core, id: &str) -> Result<Option<String>> {
    Ok(core
        .conn()
        .query_row(
            "SELECT owner FROM mail_accounts WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .optional()?)
}

fn mail_account_admin_view_core(core: &Core, id: &str) -> Result<Option<MailAccountAdminView>> {
    Ok(core
        .conn()
        .query_row(
            &format!("{MAIL_ACCOUNT_ADMIN_SELECT} WHERE id = ?1"),
            rusqlite::params![id],
            row_to_admin_view,
        )
        .optional()?)
}

/// Fill the attachment chips for a set of thread messages (one query).
fn attach_chips(core: &Core, messages: &mut [MailThreadMessage]) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = messages.iter().map(|m| m.summary.id.clone()).collect();
    let sql = format!(
        "SELECT id, message_id, filename, mime, size, (blob_hash IS NOT NULL) AS stored \
         FROM mail_attachments WHERE message_id IN ({}) ORDER BY created_at ASC, id ASC",
        super::placeholders_or_never(ids.len())
    );
    let mut stmt = core.conn().prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, bool>(5)?,
        ))
    })?;
    for row in rows {
        let (id, message_id, filename, mime, size, stored) = row?;
        if let Some(m) = messages.iter_mut().find(|m| m.summary.id == message_id) {
            m.attachments.push(MailAttachmentChip {
                id,
                filename,
                mime,
                size,
                stored,
            });
        }
    }
    Ok(())
}

fn mail_message_rows(
    core: &Core,
    query: Option<&str>,
    account_id: Option<&str>,
    limit: i64,
) -> Result<Vec<MailMessageRow>> {
    let limit = limit.clamp(1, 200);
    let mut clauses: Vec<&str> = Vec::new();
    if account_id.is_some() {
        clauses.push("m.account_id = ?");
    }
    let trimmed = query.map(str::trim).filter(|q| !q.is_empty());
    if trimmed.is_some() {
        // SQLite LIKE is ASCII case-insensitive by default — the ILIKE port.
        clauses.push("(m.subject LIKE ? OR m.from_addr LIKE ? OR COALESCE(m.from_name, '') LIKE ? OR m.snippet LIKE ? OR m.body_text LIKE ? OR m.keywords_json LIKE ?)");
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", clauses.join(" AND "))
    };
    let deleted_filter = if where_sql.is_empty() {
        "WHERE m.deleted_at IS NULL "
    } else {
        "AND m.deleted_at IS NULL "
    };
    let sql = mail_message_select(&format!(
        "{where_sql}{deleted_filter}ORDER BY m.received_at DESC LIMIT ?"
    ));
    let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(account_id) = account_id {
        binds.push(Box::new(account_id.to_string()));
    }
    if let Some(term) = trimmed {
        let needle = format!("%{term}%");
        for _ in 0..6 {
            binds.push(Box::new(needle.clone()));
        }
    }
    binds.push(Box::new(limit));
    let mut stmt = core.conn().prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())),
        row_to_message,
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---- ingest (the mail-sync sink; DIRECTION.md D6/D10) --------------------
//
// The mail sync driver (the retired daemon, then the Phase 3 module)
// implements jmap-sync's MailSink/CursorStore by delegating here, so every
// write stays in the store layer. MailIngestMessage mirrors jmap-sync's
// NormalizedMessage as plain fields.

/// Attachment metadata as mail sync hands it over (mirrors jmap-sync's
/// AttachmentMeta). Bytes come later via the fetch pipeline; the jmap blob id
/// keeps the server as the byte source of record for oversize parts.
#[derive(Debug, Clone)]
pub struct MailIngestAttachment {
    pub jmap_blob_id: String,
    pub filename: String,
    pub mime: String,
    pub size: i64,
    pub content_id: Option<String>,
    pub disposition: Option<String>,
}

/// One message as mail sync hands it to the store. JSON-typed fields arrive
/// pre-serialized (the sync driver owns the address/keyword shapes).
#[derive(Debug, Clone)]
pub struct MailIngestMessage {
    pub jmap_id: String,
    pub thread_id: String,
    pub message_id_hdr: Option<String>,
    pub in_reply_to: Option<String>,
    pub references_json: String,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub to_json: String,
    pub cc_json: String,
    pub reply_to_json: String,
    pub subject: String,
    pub sent_at: Option<String>,
    pub received_at: String,
    pub mailbox_ids: Vec<String>,
    pub mailbox_ids_json: String,
    pub keywords: Vec<String>,
    pub keywords_json: String,
    pub body_text: String,
    pub body_source: String,
    pub snippet: String,
    pub size: i64,
    pub has_attachments: bool,
    pub attachments: Vec<MailIngestAttachment>,
    pub parse_error: Option<String>,
}

/// What the daemon emits post-commit (wire + inbox), suppressed during
/// backfill per D10.
#[derive(Debug, Default)]
pub struct MailIngestOutcome {
    pub stored: usize,
    /// (mail id, subject) of NEW messages that live in an inbox-role mailbox.
    pub notify: Vec<(String, String)>,
}

/// Everything the daemon needs to sync one account, minus the secret.
#[derive(Debug, Clone)]
pub struct MailAccountSync {
    pub id: String,
    pub owner: String,
    pub address: String,
    pub jmap_url: String,
    pub jmap_username: Option<String>,
    pub jmap_account_id: String,
    pub cred_id: Option<String>,
    pub email_state: Option<String>,
    pub mailbox_state: Option<String>,
    pub backfill_status: String,
    pub backfill_cursor: Option<serde_json::Value>,
    pub attempts: i64,
}

/// FTS input has practical size limits a large newsletter can hit; clip on a
/// char boundary well below them (DIRECTION.md seam 2).
pub fn fts_clip(body: &str, max_bytes: usize) -> &str {
    if body.len() <= max_bytes {
        return body;
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

/// The one clip size for mail FTS bodies — shared with the 1.7 importer so
/// imported and synced messages index identically.
pub const FTS_CLIP_BYTES: usize = 200_000;

/// THE mail embed-eligibility predicate — the single contract between the
/// embed drain (which embeds rows matching it) and the reaper (which deletes
/// embedding rows NOT matching it). Both sides MUST use this exact SQL: if
/// the two ever diverge, the drain embeds a row the reaper considers
/// ineligible (or vice versa) and they fight forever — embed, reap, re-embed,
/// every cycle, silently burning the whole embed budget. Never inline a
/// variant of this predicate anywhere else.
///
/// A message is embed-eligible iff ALL of:
///   1. live — `deleted_at IS NULL` (tombstones/redactions leave retrieval);
///   2. in at least one ingest-enabled mailbox of its account — the same
///      quoted-id `mailbox_ids_json LIKE '%"<jmap_id>"%'` containment match
///      `mail_mailbox_set_ingest`'s OFF-path uses (jmap ids are server-issued
///      and never substrings of each other in practice);
///   3. not junk — `keywords_json NOT LIKE '%"$junk"%'`;
///   4. within the newest-N window of its account (`HIVE_MAIL_EMBED_LIMIT`,
///      default 5000 — the DIRECTION.md D8 gate), ranked by `received_at`
///      over the *otherwise-eligible* rows only, so junk/tombstoned/
///      non-ingest mail never consumes a window slot. Newest-N is a moving
///      predicate: new mail arriving pushes old mail out with no event fired,
///      which is why the reaper sweep is the ONLY aging mechanism.
///
/// Usage contract: the `mail_messages` row under test must be aliased `m`,
/// and the fragment takes exactly ONE bind — the per-account window limit N
/// (as an i64; N <= 0 means an empty window, i.e. nothing is eligible).
pub const MAIL_EMBED_ELIGIBLE_SQL: &str = "m.deleted_at IS NULL \
     AND m.keywords_json NOT LIKE '%\"$junk\"%' \
     AND EXISTS (SELECT 1 FROM mail_mailboxes gate \
         WHERE gate.account_id = m.account_id AND gate.ingest \
         AND m.mailbox_ids_json LIKE ('%\"' || gate.jmap_id || '\"%')) \
     AND m.id IN (SELECT win.id FROM ( \
         SELECT i.id, ROW_NUMBER() OVER ( \
             PARTITION BY i.account_id ORDER BY i.received_at DESC, i.id DESC) AS rn \
         FROM mail_messages i \
         WHERE i.deleted_at IS NULL \
           AND i.keywords_json NOT LIKE '%\"$junk\"%' \
           AND EXISTS (SELECT 1 FROM mail_mailboxes wgate \
               WHERE wgate.account_id = i.account_id AND wgate.ingest \
               AND i.mailbox_ids_json LIKE ('%\"' || wgate.jmap_id || '\"%')) \
         ) win WHERE win.rn <= ?)";

impl Store {
    /// Enabled accounts whose backoff window has elapsed.
    pub async fn mail_accounts_due(&self) -> Result<Vec<MailAccountSync>> {
        self.run(|core| {
            let mut stmt = core.conn().prepare(
                "SELECT id, owner, address, jmap_url, jmap_username, jmap_account_id, cred_id, \
                 email_state, mailbox_state, backfill_status, backfill_cursor, attempts \
                 FROM mail_accounts WHERE enabled AND (next_attempt_at IS NULL OR next_attempt_at <= ?1) \
                 ORDER BY id",
            )?;
            let rows = stmt.query_map(rusqlite::params![now_iso()], |r| {
                Ok(MailAccountSync {
                    id: r.get(0)?,
                    owner: r.get(1)?,
                    address: r.get(2)?,
                    jmap_url: r.get(3)?,
                    jmap_username: r.get(4)?,
                    jmap_account_id: r.get(5)?,
                    cred_id: r.get(6)?,
                    email_state: r.get(7)?,
                    mailbox_state: r.get(8)?,
                    backfill_status: r.get(9)?,
                    backfill_cursor: r
                        .get::<_, Option<String>>(10)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    attempts: r.get(11)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn mail_account_set_jmap_id(&self, id: &str, jmap_account_id: &str) -> Result<()> {
        let (id, jmap_account_id) = (id.to_string(), jmap_account_id.to_string());
        self.run(move |core| {
            core.commit(vec![Draft::new(
                crate::oplog::kind::MODULE_DOC,
                "admin",
                &now_iso(),
                json!({"module": "mail", "doc_kind": "account", "id": id, "fields": {
                    "jmap_account_id": jmap_account_id, "updated_at": now_iso(),
                }}),
            )])
        })
        .await
    }

    /// The persisted sync cursor: (email_state, mailbox_state,
    /// backfill_status, backfill_cursor).
    pub async fn mail_cursor_load(
        &self,
        id: &str,
    ) -> Result<(
        Option<String>,
        Option<String>,
        String,
        Option<serde_json::Value>,
    )> {
        let id = id.to_string();
        self.run(move |core| {
            let row = core
                .conn()
                .query_row(
                    "SELECT email_state, mailbox_state, backfill_status, backfill_cursor \
                     FROM mail_accounts WHERE id = ?1",
                    rusqlite::params![id],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| anyhow!("mail account {id} not found"))?;
            Ok((
                row.0,
                row.1,
                row.2,
                row.3.and_then(|s| serde_json::from_str(&s).ok()),
            ))
        })
        .await
    }

    /// Persist sync state. Backfill status/cursor and the two JMAP state
    /// strings are the whole cursor (DIRECTION.md D5) — one cursor.set record.
    pub async fn mail_cursor_save(
        &self,
        id: &str,
        email_state: Option<&str>,
        mailbox_state: Option<&str>,
        backfill_status: &str,
        backfill_cursor: Option<&serde_json::Value>,
    ) -> Result<()> {
        let payload = json!({"module": "mail", "account": id, "cursor": {
            "email_state": email_state,
            "mailbox_state": mailbox_state,
            "backfill_status": backfill_status,
            "backfill_cursor": backfill_cursor.map(|v| v.to_string()),
        }});
        self.run(move |core| {
            core.commit(vec![Draft::new(
                crate::oplog::kind::CURSOR_SET,
                "system",
                &now_iso(),
                payload,
            )])
        })
        .await
    }

    pub async fn mail_account_mark_ok(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.run(move |core| {
            core.commit(vec![Draft::new(
                crate::oplog::kind::CURSOR_SET,
                "system",
                &now_iso(),
                json!({"module": "mail", "account": id, "cursor": {
                    "attempts": 0, "next_attempt_at": null, "last_error": null,
                    "last_status": "ok", "last_synced_at": now_iso(),
                }}),
            )])
        })
        .await
    }

    /// Outbox-style backoff at the account level; after 8 attempts the
    /// account disables itself and the caller notifies its owner loudly
    /// (D5: sources' silent retry-forever is the known-bad pattern).
    pub async fn mail_account_mark_failed(&self, id: &str, error: &str) -> Result<bool> {
        let (id, error) = (id.to_string(), error.to_string());
        self.run(move |core| {
            let prior: i64 = core
                .conn()
                .query_row(
                    "SELECT attempts FROM mail_accounts WHERE id = ?1",
                    rusqlite::params![id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| anyhow!("mail account {id} not found"))?;
            let attempts = prior + 1;
            let ts = now_iso();
            let mut cursor = json!({
                "attempts": attempts,
                "last_error": fts_clip(&error, 2000),
                "last_status": "error",
            });
            let disable = attempts >= 8;
            let mut drafts = Vec::new();
            if disable {
                cursor["next_attempt_at"] = serde_json::Value::Null;
                drafts.push(Draft::new(
                    crate::oplog::kind::MODULE_DOC,
                    "system",
                    &ts,
                    json!({"module": "mail", "doc_kind": "account", "id": id, "fields": {
                        "enabled": false, "updated_at": ts,
                    }}),
                ));
            } else {
                let delay = super::outbox::backoff_secs(attempts);
                let next = (chrono::Utc::now() + chrono::Duration::seconds(delay))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                cursor["next_attempt_at"] = serde_json::Value::String(next);
            }
            drafts.insert(
                0,
                Draft::new(
                    crate::oplog::kind::CURSOR_SET,
                    "system",
                    &ts,
                    json!({"module": "mail", "account": id, "cursor": cursor}),
                ),
            );
            core.commit(drafts)?;
            Ok(disable)
        })
        .await
    }

    /// Upsert mailbox names/roles; never flips an existing row's ingest flag
    /// (that is operator intent, not server state).
    pub async fn mail_sync_mailboxes(
        &self,
        account_id: &str,
        boxes: &[(String, String, Option<String>, i64)],
    ) -> Result<()> {
        let account_id = account_id.to_string();
        let boxes = boxes.to_vec();
        self.run(move |core| {
            let ts = now_iso();
            let mut drafts = Vec::new();
            for (jmap_id, name, role, sort_order) in &boxes {
                let existing: Option<String> = core
                    .conn()
                    .query_row(
                        "SELECT id FROM mail_mailboxes WHERE account_id = ?1 AND jmap_id = ?2",
                        rusqlite::params![account_id, jmap_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                match existing {
                    // Existing row: name/role/sort_order refresh; ingest is
                    // operator intent and is deliberately NOT carried.
                    Some(id) => drafts.push(Draft::new(
                        crate::oplog::kind::MODULE_DOC,
                        "system",
                        &ts,
                        json!({"module": "mail", "doc_kind": "mailbox", "id": id, "fields": {
                            "name": name, "role": role, "sort_order": sort_order,
                        }}),
                    )),
                    None => drafts.push(Draft::new(
                        crate::oplog::kind::MODULE_DOC,
                        "system",
                        &ts,
                        json!({"module": "mail", "doc_kind": "mailbox", "id": new_id("mbox"), "fields": {
                            "account_id": account_id, "jmap_id": jmap_id,
                            "name": name, "role": role, "sort_order": sort_order,
                        }}),
                    )),
                }
            }
            core.commit(drafts)
        })
        .await
    }

    /// (ingest-enabled jmap ids, inbox-role jmap ids) for one account.
    pub async fn mail_mailbox_sets(
        &self,
        account_id: &str,
    ) -> Result<(HashSet<String>, HashSet<String>)> {
        let account_id = account_id.to_string();
        self.run(move |core| {
            let mut stmt = core.conn().prepare(
                "SELECT jmap_id, role, ingest FROM mail_mailboxes WHERE account_id = ?1",
            )?;
            let rows = stmt.query_map(rusqlite::params![account_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })?;
            let mut ingest = HashSet::new();
            let mut inbox = HashSet::new();
            for row in rows {
                let (jmap_id, role, ing) = row?;
                if ing {
                    ingest.insert(jmap_id.clone());
                }
                if role.as_deref() == Some("inbox") {
                    inbox.insert(jmap_id);
                }
            }
            Ok((ingest, inbox))
        })
        .await
    }

    /// Every stored jmap_id including tombstoned rows — the reconciliation
    /// diff base (never re-fetching known ids is what keeps redaction sticky).
    pub async fn mail_known_jmap_ids(&self, account_id: &str) -> Result<HashSet<String>> {
        let account_id = account_id.to_string();
        self.run(move |core| {
            let mut stmt = core
                .conn()
                .prepare("SELECT jmap_id FROM mail_messages WHERE account_id = ?1")?;
            let rows = stmt.query_map(rusqlite::params![account_id], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<HashSet<String>>>()?)
        })
        .await
    }

    /// The sink's upsert, as records: one batch per call; idempotent on
    /// (account_id, jmap_id) via the command layer's existing-row probe. On
    /// replays the record carries ONLY mutable metadata (mailbox_ids,
    /// keywords) so bodies never rewrite, moves/flags apply (D6), and admin
    /// redaction can never be re-hydrated by sync. FTS membership re-evaluates
    /// in the same pass: ingest-enabled ∩ not-junk rows are searchable the
    /// moment the batch lands, everything else leaves search AND embeddings
    /// immediately.
    pub async fn mail_ingest_batch(
        &self,
        account_id: &str,
        owner: &str,
        ingest_ids: &HashSet<String>,
        inbox_ids: &HashSet<String>,
        msgs: Vec<MailIngestMessage>,
    ) -> Result<MailIngestOutcome> {
        if msgs.is_empty() {
            return Ok(MailIngestOutcome::default());
        }
        let account_id = account_id.to_string();
        let owner = owner.to_string();
        let ingest_ids = ingest_ids.clone();
        let inbox_ids = inbox_ids.clone();
        self.run(move |core| {
            let mut out = MailIngestOutcome::default();
            let ts = now_iso();
            let mut drafts: Vec<Draft> = Vec::new();
            // (id, live_eligible, inserted, subject, clipped body) per message,
            // applied to search after the fold lands the rows.
            let mut post: Vec<(String, bool, String, String)> = Vec::new();
            for m in &msgs {
                let eligible = m.mailbox_ids.iter().any(|id| ingest_ids.contains(id))
                    && !m.keywords.iter().any(|k| k == "$junk");
                let existing: Option<(String, Option<String>, String)> = core
                    .conn()
                    .query_row(
                        "SELECT id, deleted_at, embed_state FROM mail_messages \
                         WHERE account_id = ?1 AND jmap_id = ?2",
                        rusqlite::params![account_id, m.jmap_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                let (id, deleted, embed_state, inserted) = match &existing {
                    Some((id, deleted, embed_state)) => {
                        (id.clone(), deleted.clone(), embed_state.clone(), false)
                    }
                    None => (new_id("mail"), None, String::new(), true),
                };
                let live_eligible = eligible && deleted.is_none();

                if inserted {
                    let embed_on_insert = if eligible && m.parse_error.is_none() {
                        "pending"
                    } else {
                        "skip"
                    };
                    drafts.push(Draft::new(
                        crate::oplog::kind::MODULE_DOC,
                        &owner,
                        &ts,
                        json!({"module": "mail", "doc_kind": "message", "id": id, "fields": {
                            "account_id": account_id, "user_scope": owner,
                            "jmap_id": m.jmap_id, "jmap_thread_id": m.thread_id,
                            "message_id_hdr": m.message_id_hdr, "in_reply_to": m.in_reply_to,
                            "references_json": m.references_json,
                            "from_addr": m.from_addr, "from_name": m.from_name,
                            "to_json": m.to_json, "cc_json": m.cc_json,
                            "reply_to_json": m.reply_to_json,
                            "subject": m.subject, "sent_at": m.sent_at,
                            "received_at": m.received_at,
                            "mailbox_ids_json": m.mailbox_ids_json,
                            "keywords_json": m.keywords_json,
                            "body_text": m.body_text, "body_source": m.body_source,
                            "snippet": m.snippet, "size": m.size,
                            "has_attachments": m.has_attachments,
                            "embed_state": embed_on_insert,
                            "created_at": m.received_at, "updated_at": ts,
                        }}),
                    ));
                } else {
                    // Replay/delta: mutable metadata ONLY — bodies are
                    // immutable on conflict (redaction durability).
                    let mut fields = json!({
                        "mailbox_ids_json": m.mailbox_ids_json,
                        "keywords_json": m.keywords_json,
                        "updated_at": ts,
                    });
                    if live_eligible && embed_state == "skip" {
                        // A move back INTO ingest re-queues a previously
                        // skipped row.
                        fields["embed_state"] = json!("pending");
                    } else if !live_eligible && embed_state != "skip" {
                        fields["embed_state"] = json!("skip");
                    }
                    drafts.push(Draft::new(
                        crate::oplog::kind::MODULE_DOC,
                        &owner,
                        &ts,
                        json!({"module": "mail", "doc_kind": "message", "id": id, "fields": fields}),
                    ));
                }

                // Attachment metadata rows; blob_hash stays NULL (= bytes
                // pending) until the fetch pipeline stores them. Replays are
                // absorbed by the existing-row probe (the Postgres NULLS-NOT-
                // DISTINCT DO NOTHING). Tombstoned rows (incl. admin-redacted
                // ones, whose attachment rows were deleted) get nothing back —
                // otherwise a metadata replay would re-queue redacted bytes.
                if deleted.is_none() {
                    for att in &m.attachments {
                        let dup: bool = core.conn().query_row(
                            "SELECT EXISTS(SELECT 1 FROM mail_attachments WHERE message_id = ?1 \
                             AND jmap_blob_id = ?2 AND COALESCE(content_id, '') = COALESCE(?3, ''))",
                            rusqlite::params![id, att.jmap_blob_id, att.content_id],
                            |r| r.get(0),
                        )?;
                        if dup {
                            continue;
                        }
                        drafts.push(Draft::new(
                            crate::oplog::kind::MODULE_DOC,
                            &owner,
                            &ts,
                            json!({"module": "mail", "doc_kind": "attachment", "id": new_id("matt"), "fields": {
                                "message_id": id, "jmap_blob_id": att.jmap_blob_id,
                                "filename": att.filename, "mime": att.mime,
                                "size": att.size, "content_id": att.content_id,
                                "disposition": att.disposition, "created_at": ts,
                            }}),
                        ));
                    }
                }

                if inserted && live_eligible && m.mailbox_ids.iter().any(|x| inbox_ids.contains(x))
                {
                    out.notify.push((id.clone(), m.subject.clone()));
                }
                post.push((
                    id,
                    live_eligible,
                    if m.subject.trim().is_empty() {
                        "(no subject)".to_string()
                    } else {
                        m.subject.clone()
                    },
                    fts_clip(&m.body_text, FTS_CLIP_BYTES).to_string(),
                ));
                out.stored += 1;
            }
            core.commit(drafts)?;

            // FTS membership (command-layer business — see module header).
            for (id, live_eligible, title, body) in &post {
                if *live_eligible {
                    super::search::index_entity_conn(core.conn(), "mail", id, title, body, &[])?;
                } else {
                    core.conn().execute(
                        "DELETE FROM search WHERE kind = 'mail' AND ref_id = ?1",
                        rusqlite::params![id],
                    )?;
                    core.index.remove_embeddings("mail", id)?;
                }
            }
            Ok(out)
        })
        .await
    }

    /// JMAP destroys: tombstone records (the fold soft-deletes the row, drops
    /// its attachments metadata, FTS, and vectors — D6: deleted mail must not
    /// stay searchable until a sweep).
    pub async fn mail_tombstone_batch(
        &self,
        account_id: &str,
        jmap_ids: &[String],
    ) -> Result<usize> {
        if jmap_ids.is_empty() {
            return Ok(0);
        }
        let account_id = account_id.to_string();
        let jmap_ids = jmap_ids.to_vec();
        self.run(move |core| {
            let ts = now_iso();
            let mut drafts = Vec::new();
            let mut doomed: Vec<String> = Vec::new();
            for jmap_id in &jmap_ids {
                let id: Option<String> = core
                    .conn()
                    .query_row(
                        "SELECT id FROM mail_messages WHERE account_id = ?1 AND jmap_id = ?2 AND deleted_at IS NULL",
                        rusqlite::params![account_id, jmap_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(id) = id else { continue };
                drafts.push(Draft::new(
                    crate::oplog::kind::TOMBSTONE,
                    "system",
                    &ts,
                    json!({"kind": "mail", "id": id}),
                ));
                doomed.push(id);
            }
            let n = doomed.len();
            core.commit(drafts)?;
            // The fold dropped the persisted vectors; forget the ANN entries too.
            for id in &doomed {
                core.index.remove_embeddings("mail", id)?;
            }
            Ok(n)
        })
        .await
    }
}

// ---- attachments (byte pipeline + serving + GC; plan A6) ------------------

/// One attachment awaiting bytes, as the fetch pipeline consumes it.
#[derive(Debug, Clone)]
pub struct MailAttachmentPending {
    pub id: String,
    pub jmap_blob_id: String,
    pub mime: String,
    /// Declared (server-reported) size — the pre-download oversize check.
    pub size: i64,
}

/// Everything the serving path needs, resolved in one query + a blockstore
/// read. `data` rides along because household-scale attachments are ≤ the
/// fetch cap anyway.
#[derive(Debug, Clone)]
pub struct MailAttachmentServe {
    pub user_scope: String,
    pub filename: String,
    pub mime: String,
    pub blob_hash: Option<String>,
    pub data: Option<Vec<u8>>,
}

impl Store {
    /// Attachments still awaiting bytes for one account: blob not stored, not
    /// skipped, message still live. Oldest first so a big backlog drains in
    /// arrival order.
    pub async fn mail_attachments_pending(
        &self,
        account_id: &str,
        limit: i64,
    ) -> Result<Vec<MailAttachmentPending>> {
        let account_id = account_id.to_string();
        self.run(move |core| {
            let mut stmt = core.conn().prepare(
                "SELECT t.id, t.jmap_blob_id, t.mime, t.size FROM mail_attachments t \
                 JOIN mail_messages m ON m.id = t.message_id \
                 WHERE m.account_id = ?1 AND m.deleted_at IS NULL \
                 AND t.blob_hash IS NULL AND t.skipped_reason IS NULL \
                 ORDER BY t.created_at ASC, t.id ASC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![account_id, limit.clamp(1, 500)], |r| {
                Ok(MailAttachmentPending {
                    id: r.get(0)?,
                    jmap_blob_id: r.get(1)?,
                    mime: r.get(2)?,
                    size: r.get(3)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Permanently park an attachment ('oversize' | 'missing'); it leaves the
    /// pending queue and its chip renders dimmed. Never overwrites stored
    /// bytes.
    pub async fn mail_attachment_mark_skipped(&self, att_id: &str, reason: &str) -> Result<()> {
        let (att_id, reason) = (att_id.to_string(), reason.to_string());
        self.run(move |core| {
            let unstored: bool = core.conn().query_row(
                "SELECT EXISTS(SELECT 1 FROM mail_attachments WHERE id = ?1 AND blob_hash IS NULL)",
                rusqlite::params![att_id],
                |r| r.get(0),
            )?;
            if !unstored {
                return Ok(());
            }
            core.commit(vec![Draft::new(
                crate::oplog::kind::MODULE_DOC,
                "system",
                &now_iso(),
                json!({"module": "mail", "doc_kind": "attachment", "id": att_id, "fields": {
                    "skipped_reason": reason,
                }}),
            )])
        })
        .await
    }

    /// Store fetched bytes content-addressed: the blockstore dedups blocks by
    /// construction and blob_refs dedups on hash (identical attachments across
    /// messages share one entry); the attachment metadata then points at it.
    pub async fn mail_attachment_store_blob(
        &self,
        att_id: &str,
        hash: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let (att_id, hash, mime) = (att_id.to_string(), hash.to_string(), mime.to_string());
        let bytes = bytes.to_vec();
        self.run(move |core| {
            let keys = core.keys.clone();
            let blob = core.blocks.put(keys.as_ref(), &bytes, Some(&mime))?;
            let mut raw = Vec::new();
            ciborium::into_writer(&blob, &mut raw).map_err(|e| anyhow!("encoding BlobRef: {e}"))?;
            core.conn().execute(
                "INSERT OR IGNORE INTO blob_refs (hash, ref, size, mime, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![hash, raw, bytes.len() as i64, mime, now_iso()],
            )?;
            core.commit(vec![Draft::new(
                crate::oplog::kind::MODULE_DOC,
                "system",
                &now_iso(),
                json!({"module": "mail", "doc_kind": "attachment", "id": att_id, "fields": {
                    "blob_hash": hash, "skipped_reason": null,
                }}),
            )])?;
            Ok(())
        })
        .await
    }

    /// The serving path's lookup: attachment joined to its owning message
    /// for the user_scope check, bytes fetched from the blockstore (None =
    /// not stored). By id only — blobs are NEVER addressable by hash from the
    /// outside.
    pub async fn mail_attachment_serve(&self, att_id: &str) -> Result<Option<MailAttachmentServe>> {
        let att_id = att_id.to_string();
        self.run(move |core| {
            let row: Option<(String, String, String, Option<String>)> = core
                .conn()
                .query_row(
                    "SELECT m.user_scope, t.filename, t.mime, t.blob_hash \
                     FROM mail_attachments t \
                     JOIN mail_messages m ON m.id = t.message_id \
                     WHERE t.id = ?1",
                    rusqlite::params![att_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((user_scope, filename, mime, blob_hash)) = row else {
                return Ok(None);
            };
            let data = match &blob_hash {
                Some(hash) => read_blob(core, hash),
                None => None,
            };
            Ok(Some(MailAttachmentServe {
                user_scope,
                filename,
                mime,
                blob_hash,
                data,
            }))
        })
        .await
    }

    /// Admin redaction (plan A6), as records: redact (columns clear, FTS +
    /// vectors drop) + tombstone (soft delete + attachment rows drop) +
    /// module.doc restoring the visible "[redacted]" subject and flags, then
    /// runtime blob cleanup. Durability is guaranteed by the ingest replay
    /// path (metadata-only on existing rows — body columns never rewritten),
    /// the tombstone check gating attachment re-inserts, and reconcile never
    /// re-fetching known jmap ids. Returns the owning namespace for the
    /// caller's post-commit wire event; None = no such message.
    pub async fn mail_message_redact(&self, id: &str) -> Result<Option<String>> {
        let id = id.to_string();
        self.run(move |core| {
            let owner: Option<String> = core
                .conn()
                .query_row(
                    "SELECT user_scope FROM mail_messages WHERE id = ?1",
                    rusqlite::params![id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(owner) = owner else {
                return Ok(None);
            };
            let hashes: Vec<String> = {
                let mut stmt = core.conn().prepare(
                    "SELECT DISTINCT blob_hash FROM mail_attachments \
                     WHERE message_id = ?1 AND blob_hash IS NOT NULL",
                )?;
                let rows = stmt.query_map(rusqlite::params![id], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let ts = now_iso();
            core.commit(vec![
                Draft::new(
                    crate::oplog::kind::REDACT,
                    "admin",
                    &ts,
                    json!({"kind": "mail", "id": id}),
                ),
                Draft::new(
                    crate::oplog::kind::TOMBSTONE,
                    "admin",
                    &ts,
                    json!({"kind": "mail", "id": id}),
                ),
                Draft::new(
                    crate::oplog::kind::MODULE_DOC,
                    "admin",
                    &ts,
                    json!({"module": "mail", "doc_kind": "message", "id": id, "fields": {
                        "subject": "[redacted]", "has_attachments": false,
                        "embed_state": "skip", "updated_at": ts,
                    }}),
                ),
            ])?;
            core.index.remove_embeddings("mail", &id)?;
            for hash in &hashes {
                gc_blob_if_orphan(core, hash)?;
            }
            Ok(Some(owner))
        })
        .await
    }

    /// Refcount blob GC (weekly when sync runs): delete blob_refs +
    /// blockstore blobs no attachment points at, but only ones older than
    /// 24h — the grace window covers a fetch pipeline that has stored bytes
    /// but not yet committed the attachment pointer.
    pub async fn mail_blobs_gc(&self) -> Result<u64> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        self.run(move |core| gc_orphan_blobs(core, Some(&cutoff)))
            .await
    }
}

/// Read one blob's bytes back from the blockstore via its blob_refs pointer.
fn read_blob(core: &Core, hash: &str) -> Option<Vec<u8>> {
    let raw: Option<Vec<u8>> = core
        .conn()
        .query_row(
            "SELECT ref FROM blob_refs WHERE hash = ?1",
            rusqlite::params![hash],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let blob: crate::blockstore::BlobRef = ciborium::from_reader(raw?.as_slice()).ok()?;
    core.blocks.get(core.keys.clone().as_ref(), &blob).ok()
}

/// Delete `hash` from blob_refs + blockstore when nothing references it.
fn gc_blob_if_orphan(core: &mut Core, hash: &str) -> Result<bool> {
    let referenced: bool = core.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM mail_attachments WHERE blob_hash = ?1)",
        rusqlite::params![hash],
        |r| r.get(0),
    )?;
    if referenced {
        return Ok(false);
    }
    let raw: Option<Vec<u8>> = core
        .conn()
        .query_row(
            "SELECT ref FROM blob_refs WHERE hash = ?1",
            rusqlite::params![hash],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(raw) = raw {
        if let Ok(blob) = ciborium::from_reader::<crate::blockstore::BlobRef, _>(raw.as_slice()) {
            let keys = core.keys.clone();
            if let Err(e) = core.blocks.delete(keys.as_ref(), &blob) {
                tracing::warn!(hash, "blockstore delete failed during blob GC: {e}");
            }
        }
        core.conn().execute(
            "DELETE FROM blob_refs WHERE hash = ?1",
            rusqlite::params![hash],
        )?;
        return Ok(true);
    }
    Ok(false)
}

/// Sweep every orphaned blob_refs row (optionally only those created before
/// `cutoff`). Returns how many were removed.
fn gc_orphan_blobs(core: &mut Core, cutoff: Option<&str>) -> Result<u64> {
    let sql = match cutoff {
        Some(_) => {
            "SELECT b.hash FROM blob_refs b WHERE b.created_at < ?1 AND NOT EXISTS \
             (SELECT 1 FROM mail_attachments a WHERE a.blob_hash = b.hash)"
        }
        None => {
            "SELECT b.hash FROM blob_refs b WHERE NOT EXISTS \
             (SELECT 1 FROM mail_attachments a WHERE a.blob_hash = b.hash)"
        }
    };
    let hashes: Vec<String> = {
        let mut stmt = core.conn().prepare(sql)?;
        match cutoff {
            Some(c) => {
                let rows = stmt.query_map(rusqlite::params![c], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            }
        }
    };
    let mut n = 0u64;
    for hash in &hashes {
        if gc_blob_if_orphan(core, hash)? {
            n += 1;
        }
    }
    Ok(n)
}
