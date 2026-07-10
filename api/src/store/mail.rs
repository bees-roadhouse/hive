use anyhow::{anyhow, Result};
use serde::Serialize;
use sqlx::Row;

use super::cc_credentials::NewCcCredential;
use super::{new_id, now_iso, Store};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MailAccount {
    pub id: String,
    pub label: String,
    pub address: String,
    pub provider: Option<String>,
    pub last_synced_at: Option<String>,
}

/// Management view: sync state + error surface, never secrets.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
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

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MailMailboxView {
    pub id: String,
    pub jmap_id: String,
    pub name: String,
    pub role: Option<String>,
    pub sort_order: i64,
    pub ingest: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
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

/// Lightweight attachment row for thread payloads: enough for the SPA to
/// render a chip and link the serving route. `stored` = bytes are in the
/// local blob store (false = oversize/missing/pending — link would 404).
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

impl Store {
    pub async fn mail_accounts_list(&self, viewer: Option<&str>) -> Result<Vec<MailAccount>> {
        let rows = match viewer {
            Some(viewer) => {
                crate::pgq::query_as::<MailAccount>(
                    "SELECT id, address AS label, address, 'jmap' AS provider, last_synced_at \
                 FROM mail_accounts WHERE owner = ? ORDER BY address ASC",
                )
                .bind(viewer)
                .fetch_all(self.db())
                .await?
            }
            None => {
                crate::pgq::query_as::<MailAccount>(
                    "SELECT id, address AS label, address, 'jmap' AS provider, last_synced_at \
                 FROM mail_accounts ORDER BY owner ASC, address ASC",
                )
                .fetch_all(self.db())
                .await?
            }
        };
        Ok(rows)
    }

    pub async fn mail_messages_list(
        &self,
        viewer: Option<&str>,
        query: Option<&str>,
        account_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<MailMessageSummary>> {
        let rows = self
            .mail_message_rows(viewer, query, account_id, limit)
            .await?;
        Ok(rows
            .into_iter()
            .map(MailThreadMessage::from)
            .map(|m| m.summary)
            .collect())
    }

    pub async fn mail_search(
        &self,
        query: &str,
        viewer: Option<&str>,
        limit: i64,
    ) -> Result<Vec<MailMessageSummary>> {
        self.mail_messages_list(viewer, Some(query), None, limit)
            .await
    }

    pub async fn mail_thread_get(
        &self,
        thread_id: &str,
        viewer: Option<&str>,
    ) -> Result<MailThread> {
        let rows =
            match viewer {
                Some(viewer) => crate::pgq::query_as::<MailMessageRow>(&mail_message_select(
                    "WHERE m.user_scope = ? AND m.jmap_thread_id = ? ORDER BY m.received_at ASC",
                ))
                .bind(viewer)
                .bind(thread_id)
                .fetch_all(self.db())
                .await?,
                None => {
                    crate::pgq::query_as::<MailMessageRow>(&mail_message_select(
                        "WHERE m.jmap_thread_id = ? ORDER BY m.received_at ASC",
                    ))
                    .bind(thread_id)
                    .fetch_all(self.db())
                    .await?
                }
            };
        let mut messages: Vec<MailThreadMessage> =
            rows.into_iter().map(MailThreadMessage::from).collect();
        self.attach_chips(&mut messages).await?;
        let subject = messages
            .first()
            .map(|m| m.summary.subject.clone())
            .unwrap_or_default();
        Ok(MailThread {
            thread_id: thread_id.to_string(),
            subject,
            messages,
        })
    }

    /// Fill the attachment chips for a set of thread messages (one query).
    async fn attach_chips(&self, messages: &mut [MailThreadMessage]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        #[derive(sqlx::FromRow)]
        struct ChipRow {
            id: String,
            message_id: String,
            filename: String,
            mime: String,
            size: i64,
            stored: bool,
        }
        let ids: Vec<String> = messages.iter().map(|m| m.summary.id.clone()).collect();
        let rows = crate::pgq::query_as::<ChipRow>(
            "SELECT id, message_id, filename, mime, size, (blob_hash IS NOT NULL) AS stored \
             FROM mail_attachments WHERE message_id = ANY(?) ORDER BY created_at ASC, id ASC",
        )
        .bind(&ids)
        .fetch_all(self.db())
        .await?;
        for row in rows {
            if let Some(m) = messages.iter_mut().find(|m| m.summary.id == row.message_id) {
                m.attachments.push(MailAttachmentChip {
                    id: row.id,
                    filename: row.filename,
                    mime: row.mime,
                    size: row.size,
                    stored: row.stored,
                });
            }
        }
        Ok(())
    }

    async fn mail_message_rows(
        &self,
        viewer: Option<&str>,
        query: Option<&str>,
        account_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<MailMessageRow>> {
        let limit = limit.clamp(1, 200);
        let mut clauses: Vec<&str> = Vec::new();
        if viewer.is_some() {
            clauses.push("m.user_scope = ?");
        }
        if account_id.is_some() {
            clauses.push("m.account_id = ?");
        }
        let trimmed = query.map(str::trim).filter(|q| !q.is_empty());
        if trimmed.is_some() {
            clauses.push("(m.subject ILIKE ? OR m.from_addr ILIKE ? OR COALESCE(m.from_name, '') ILIKE ? OR m.snippet ILIKE ? OR m.body_text ILIKE ? OR m.keywords_json::text ILIKE ?)");
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
        let mut q = crate::pgq::query_as::<MailMessageRow>(&sql);
        if let Some(viewer) = viewer {
            q = q.bind(viewer);
        }
        if let Some(account_id) = account_id {
            q = q.bind(account_id);
        }
        if let Some(term) = trimmed {
            let needle = format!("%{term}%");
            q = q
                .bind(needle.clone())
                .bind(needle.clone())
                .bind(needle.clone())
                .bind(needle.clone())
                .bind(needle.clone())
                .bind(needle);
        }
        Ok(q.bind(limit).fetch_all(self.db()).await?)
    }

    // ---- account management (the connect surface; hive-mail owns sync) ----

    /// Register a mail account: the credential lands in the AES-GCM vault
    /// (which hard-requires HIVE_CRED_KEY) and the account row starts
    /// 'pending' for hive-mail to pick up. The caller (route) has already
    /// validated the credential against the server via session discovery and
    /// captured `jmap_account_id`.
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
        let exists: Option<String> = crate::pgq::query_scalar::<String>(
            "SELECT id FROM mail_accounts WHERE owner = ? AND address = ?",
        )
        .bind(owner)
        .bind(address)
        .fetch_optional(self.db())
        .await?;
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
        crate::pgq::query(
            "INSERT INTO mail_accounts (id, owner, address, jmap_url, jmap_username, jmap_account_id, cred_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(owner)
        .bind(address)
        .bind(jmap_url)
        .bind(jmap_username)
        .bind(jmap_account_id)
        .bind(&cred.id)
        .bind(&ts)
        .bind(&ts)
        .execute(self.db())
        .await?;
        // ids only on the wire: it is globally readable (D10).
        self.emit(
            "mail.account.connected",
            owner,
            serde_json::json!({"id": id}),
        )
        .await?;
        self.mail_account_admin_view(&id)
            .await?
            .ok_or_else(|| anyhow!("account {id} vanished after insert"))
    }

    pub async fn mail_account_admin_view(&self, id: &str) -> Result<Option<MailAccountAdminView>> {
        Ok(crate::pgq::query_as::<MailAccountAdminView>(&format!(
            "{MAIL_ACCOUNT_ADMIN_SELECT} WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(self.db())
        .await?)
    }

    /// All accounts (admin) or the viewer's own, with sync state.
    pub async fn mail_accounts_admin_list(
        &self,
        viewer: Option<&str>,
    ) -> Result<Vec<MailAccountAdminView>> {
        let rows = match viewer {
            Some(viewer) => {
                crate::pgq::query_as::<MailAccountAdminView>(&format!(
                    "{MAIL_ACCOUNT_ADMIN_SELECT} WHERE owner = ? ORDER BY address ASC"
                ))
                .bind(viewer)
                .fetch_all(self.db())
                .await?
            }
            None => {
                crate::pgq::query_as::<MailAccountAdminView>(&format!(
                    "{MAIL_ACCOUNT_ADMIN_SELECT} ORDER BY owner ASC, address ASC"
                ))
                .fetch_all(self.db())
                .await?
            }
        };
        Ok(rows)
    }

    pub async fn mail_account_owner(&self, id: &str) -> Result<Option<String>> {
        Ok(
            crate::pgq::query_scalar::<String>("SELECT owner FROM mail_accounts WHERE id = ?")
                .bind(id)
                .fetch_optional(self.db())
                .await?,
        )
    }

    /// Delete an account and everything derived from it. Messages and
    /// attachments go via FK cascade; search/embeddings/inbox/links rows and
    /// the vault credential go explicitly, then orphaned blobs. One
    /// transaction — an account is never left half-deleted.
    pub async fn mail_account_delete(&self, id: &str) -> Result<bool> {
        let Some(owner) = self.mail_account_owner(id).await? else {
            return Ok(false);
        };
        let mut tx = self.db().begin().await?;
        for sql in [
            "DELETE FROM search WHERE kind = 'mail' AND ref_id IN (SELECT id FROM mail_messages WHERE account_id = ?)",
            "DELETE FROM embeddings WHERE ref_kind = 'mail' AND ref_id IN (SELECT id FROM mail_messages WHERE account_id = ?)",
            "DELETE FROM inbox WHERE ref_kind = 'mail' AND ref_id IN (SELECT id FROM mail_messages WHERE account_id = ?)",
            "DELETE FROM links WHERE (source_kind = 'mail' AND source_id IN (SELECT id FROM mail_messages WHERE account_id = ?)) \
             OR (target_kind = 'mail' AND target_id IN (SELECT id FROM mail_messages WHERE account_id = ?))",
        ] {
            let mut q = crate::pgq::query(sql).bind(id);
            if sql.contains("target_kind") {
                q = q.bind(id);
            }
            q.execute(&mut *tx).await?;
        }
        crate::pgq::query(
            "DELETE FROM cc_credentials WHERE id = (SELECT cred_id FROM mail_accounts WHERE id = ?)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        crate::pgq::query("DELETE FROM mail_accounts WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        crate::pgq::query(
            "DELETE FROM blobs b WHERE NOT EXISTS \
             (SELECT 1 FROM mail_attachments a WHERE a.blob_hash = b.hash)",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.emit(
            "mail.account.deleted",
            &owner,
            serde_json::json!({"id": id}),
        )
        .await?;
        Ok(true)
    }

    /// Enabling clears the backoff so hive-mail picks the account up on its
    /// next tick instead of waiting out a stale next_attempt_at.
    pub async fn mail_account_set_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let n = crate::pgq::query(
            "UPDATE mail_accounts SET enabled = ?, attempts = CASE WHEN ? THEN 0 ELSE attempts END, \
             next_attempt_at = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(enabled)
        .bind(enabled)
        .bind(now_iso())
        .bind(id)
        .execute(self.db())
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    /// Force a full reconciliation: the sentinel state string makes the next
    /// Email/changes call fail cannotCalculateChanges, which is the resync
    /// path (a bogus state is the ONLY way to route there deliberately —
    /// clearing the state would just capture a fresh one and silently skip
    /// interim changes).
    pub async fn mail_account_force_resync(&self, id: &str) -> Result<bool> {
        let n = crate::pgq::query(
            "UPDATE mail_accounts SET email_state = 'force-resync', attempts = 0, \
             next_attempt_at = NULL, last_error = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(now_iso())
        .bind(id)
        .execute(self.db())
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    pub async fn mail_mailbox_owner(&self, mailbox_id: &str) -> Result<Option<String>> {
        Ok(crate::pgq::query_scalar::<String>(
            "SELECT a.owner FROM mail_mailboxes b JOIN mail_accounts a ON a.id = b.account_id \
             WHERE b.id = ?",
        )
        .bind(mailbox_id)
        .fetch_optional(self.db())
        .await?)
    }

    pub async fn mail_mailboxes_list(&self, account_id: &str) -> Result<Vec<MailMailboxView>> {
        Ok(crate::pgq::query_as::<MailMailboxView>(
            "SELECT id, jmap_id, name, role, sort_order, ingest FROM mail_mailboxes \
             WHERE account_id = ? ORDER BY sort_order ASC, name ASC",
        )
        .bind(account_id)
        .fetch_all(self.db())
        .await?)
    }

    /// The per-mailbox opt-in (the spam gate). Turning a mailbox ON resets
    /// the account's backfill to 'pending' so history gets picked up (the
    /// unique key makes the re-run duplicate-free). Turning it OFF drops the
    /// mailbox's messages out of retrieval immediately (D6 semantics: rows
    /// stay, search/embedding membership goes).
    pub async fn mail_mailbox_set_ingest(&self, mailbox_id: &str, ingest: bool) -> Result<bool> {
        #[derive(sqlx::FromRow)]
        struct BoxRow {
            account_id: String,
            jmap_id: String,
        }
        let Some(row) = crate::pgq::query_as::<BoxRow>(
            "SELECT account_id, jmap_id FROM mail_mailboxes WHERE id = ?",
        )
        .bind(mailbox_id)
        .fetch_optional(self.db())
        .await?
        else {
            return Ok(false);
        };
        let mut tx = self.db().begin().await?;
        crate::pgq::query("UPDATE mail_mailboxes SET ingest = ? WHERE id = ?")
            .bind(ingest)
            .bind(mailbox_id)
            .execute(&mut *tx)
            .await?;
        if ingest {
            crate::pgq::query(
                "UPDATE mail_accounts SET backfill_status = 'pending', updated_at = ? WHERE id = ?",
            )
            .bind(now_iso())
            .bind(&row.account_id)
            .execute(&mut *tx)
            .await?;
        } else {
            // mailbox_ids_json is a JSON array of jmap ids; the quoted-id
            // containment match is exact enough (ids are server-issued and
            // never substrings of each other in practice).
            let needle = format!("%\"{}\"%", row.jmap_id);
            for sql in [
                "DELETE FROM search WHERE kind = 'mail' AND ref_id IN \
                 (SELECT id FROM mail_messages WHERE account_id = ? AND mailbox_ids_json LIKE ?)",
                "DELETE FROM embeddings WHERE ref_kind = 'mail' AND ref_id IN \
                 (SELECT id FROM mail_messages WHERE account_id = ? AND mailbox_ids_json LIKE ?)",
                "UPDATE mail_messages SET embed_state = 'skip' \
                 WHERE account_id = ? AND mailbox_ids_json LIKE ?",
            ] {
                crate::pgq::query(sql)
                    .bind(&row.account_id)
                    .bind(&needle)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(true)
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

// ---- ingest (the hive-mail sink; DIRECTION.md D6/D10) --------------------
//
// hive-mail implements jmap-sync's MailSink/CursorStore by delegating here, so
// every write stays in the store layer (and under test_pool). The api crate
// deliberately does NOT depend on jmap-sync — MailIngestMessage mirrors its
// NormalizedMessage as plain fields.

/// Attachment metadata as hive-mail hands it over (mirrors jmap-sync's
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

/// One message as hive-mail hands it to the store. JSON-typed fields arrive
/// pre-serialized (the daemon owns the address/keyword shapes).
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
#[derive(Debug, Clone, sqlx::FromRow)]
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

/// tsvector input has a hard ~1MB limit a large newsletter can hit; clip on a
/// char boundary well below it (DIRECTION.md seam 2).
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

const FTS_CLIP_BYTES: usize = 200_000;

impl Store {
    /// Enabled accounts whose backoff window has elapsed.
    pub async fn mail_accounts_due(&self) -> Result<Vec<MailAccountSync>> {
        Ok(crate::pgq::query_as::<MailAccountSync>(
            "SELECT id, owner, address, jmap_url, jmap_username, jmap_account_id, cred_id, \
             email_state, mailbox_state, backfill_status, backfill_cursor, attempts \
             FROM mail_accounts WHERE enabled AND (next_attempt_at IS NULL OR next_attempt_at <= ?) \
             ORDER BY id",
        )
        .bind(now_iso())
        .fetch_all(self.db())
        .await?)
    }

    pub async fn mail_account_set_jmap_id(&self, id: &str, jmap_account_id: &str) -> Result<()> {
        crate::pgq::query(
            "UPDATE mail_accounts SET jmap_account_id = ?, updated_at = ? WHERE id = ?",
        )
        .bind(jmap_account_id)
        .bind(now_iso())
        .bind(id)
        .execute(self.db())
        .await?;
        Ok(())
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
        #[derive(sqlx::FromRow)]
        struct Row {
            email_state: Option<String>,
            mailbox_state: Option<String>,
            backfill_status: String,
            backfill_cursor: Option<serde_json::Value>,
        }
        let row = crate::pgq::query_as::<Row>(
            "SELECT email_state, mailbox_state, backfill_status, backfill_cursor \
             FROM mail_accounts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.db())
        .await?
        .ok_or_else(|| anyhow!("mail account {id} not found"))?;
        Ok((
            row.email_state,
            row.mailbox_state,
            row.backfill_status,
            row.backfill_cursor,
        ))
    }

    /// Persist sync state. Backfill status/cursor and the two JMAP state
    /// strings are the whole cursor (DIRECTION.md D5).
    pub async fn mail_cursor_save(
        &self,
        id: &str,
        email_state: Option<&str>,
        mailbox_state: Option<&str>,
        backfill_status: &str,
        backfill_cursor: Option<&serde_json::Value>,
    ) -> Result<()> {
        crate::pgq::query(
            "UPDATE mail_accounts SET email_state = ?, mailbox_state = ?, backfill_status = ?, \
             backfill_cursor = ?, updated_at = ? WHERE id = ?",
        )
        .bind(email_state)
        .bind(mailbox_state)
        .bind(backfill_status)
        .bind(backfill_cursor)
        .bind(now_iso())
        .bind(id)
        .execute(self.db())
        .await?;
        Ok(())
    }

    pub async fn mail_account_mark_ok(&self, id: &str) -> Result<()> {
        crate::pgq::query(
            "UPDATE mail_accounts SET attempts = 0, next_attempt_at = NULL, last_error = NULL, \
             last_status = 'ok', last_synced_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(now_iso())
        .bind(now_iso())
        .bind(id)
        .execute(self.db())
        .await?;
        Ok(())
    }

    /// Outbox-style backoff at the account level; after 8 attempts the
    /// account disables itself and the caller notifies its owner loudly
    /// (D5: sources' silent retry-forever is the known-bad pattern).
    pub async fn mail_account_mark_failed(&self, id: &str, error: &str) -> Result<bool> {
        let attempts: i64 = crate::pgq::query_scalar::<i64>(
            "UPDATE mail_accounts SET attempts = attempts + 1, last_error = ?, \
             last_status = 'error', updated_at = ? WHERE id = ? RETURNING attempts",
        )
        .bind(fts_clip(error, 2000))
        .bind(now_iso())
        .bind(id)
        .fetch_one(self.db())
        .await?;
        if attempts >= 8 {
            crate::pgq::query(
                "UPDATE mail_accounts SET enabled = FALSE, next_attempt_at = NULL WHERE id = ?",
            )
            .bind(id)
            .execute(self.db())
            .await?;
            return Ok(true);
        }
        let delay = super::outbox::backoff_secs(attempts);
        let next = (chrono::Utc::now() + chrono::Duration::seconds(delay))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        crate::pgq::query("UPDATE mail_accounts SET next_attempt_at = ? WHERE id = ?")
            .bind(next)
            .bind(id)
            .execute(self.db())
            .await?;
        Ok(false)
    }

    /// Upsert mailbox names/roles; never flips an existing row's ingest flag
    /// (that is operator intent, not server state).
    pub async fn mail_sync_mailboxes(
        &self,
        account_id: &str,
        boxes: &[(String, String, Option<String>, i64)],
    ) -> Result<()> {
        let mut tx = self.db().begin().await?;
        for (jmap_id, name, role, sort_order) in boxes {
            crate::pgq::query(
                "INSERT INTO mail_mailboxes (id, account_id, jmap_id, name, role, sort_order) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (account_id, jmap_id) DO UPDATE SET name = excluded.name, \
                 role = excluded.role, sort_order = excluded.sort_order",
            )
            .bind(new_id("mbox"))
            .bind(account_id)
            .bind(jmap_id)
            .bind(name)
            .bind(role)
            .bind(sort_order)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// (ingest-enabled jmap ids, inbox-role jmap ids) for one account.
    pub async fn mail_mailbox_sets(
        &self,
        account_id: &str,
    ) -> Result<(
        std::collections::HashSet<String>,
        std::collections::HashSet<String>,
    )> {
        #[derive(sqlx::FromRow)]
        struct Row {
            jmap_id: String,
            role: Option<String>,
            ingest: bool,
        }
        let rows = crate::pgq::query_as::<Row>(
            "SELECT jmap_id, role, ingest FROM mail_mailboxes WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_all(self.db())
        .await?;
        let mut ingest = std::collections::HashSet::new();
        let mut inbox = std::collections::HashSet::new();
        for r in rows {
            if r.ingest {
                ingest.insert(r.jmap_id.clone());
            }
            if r.role.as_deref() == Some("inbox") {
                inbox.insert(r.jmap_id);
            }
        }
        Ok((ingest, inbox))
    }

    /// Every stored jmap_id including tombstoned rows — the reconciliation
    /// diff base (never re-fetching known ids is what keeps redaction sticky).
    pub async fn mail_known_jmap_ids(
        &self,
        account_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        let rows = crate::pgq::query("SELECT jmap_id FROM mail_messages WHERE account_id = ?")
            .bind(account_id)
            .fetch_all(self.db())
            .await?;
        let mut out = std::collections::HashSet::with_capacity(rows.len());
        for r in &rows {
            out.insert(r.try_get::<String, _>("jmap_id")?);
        }
        Ok(out)
    }

    /// The sink's upsert: one transaction per batch; idempotent on
    /// (account_id, jmap_id); the conflict arm touches ONLY mutable metadata
    /// (mailbox_ids, keywords) so replays are no-ops, moves/flags apply (D6),
    /// and admin redaction can never be re-hydrated by sync. FTS membership
    /// re-evaluates in the same transaction: ingest-enabled ∩ not-junk rows
    /// are searchable the moment they commit, everything else leaves search
    /// AND embeddings immediately.
    pub async fn mail_ingest_batch(
        &self,
        account_id: &str,
        owner: &str,
        ingest_ids: &std::collections::HashSet<String>,
        inbox_ids: &std::collections::HashSet<String>,
        msgs: Vec<MailIngestMessage>,
    ) -> Result<MailIngestOutcome> {
        let mut out = MailIngestOutcome::default();
        if msgs.is_empty() {
            return Ok(out);
        }
        let mut tx = self.db().begin().await?;
        for m in &msgs {
            let eligible = m.mailbox_ids.iter().any(|id| ingest_ids.contains(id))
                && !m.keywords.iter().any(|k| k == "$junk");
            let embed_on_insert = if eligible && m.parse_error.is_none() {
                "pending"
            } else {
                "skip"
            };
            #[derive(sqlx::FromRow)]
            struct Upserted {
                id: String,
                inserted: bool,
            }
            // xmax = 0 exposes insert-vs-update through ON CONFLICT.
            let row = crate::pgq::query_as::<Upserted>(
                "INSERT INTO mail_messages (id, account_id, user_scope, jmap_id, jmap_thread_id, \
                 message_id_hdr, in_reply_to, references_json, from_addr, from_name, to_json, \
                 cc_json, reply_to_json, subject, sent_at, received_at, mailbox_ids_json, \
                 keywords_json, body_text, body_source, snippet, size, has_attachments, \
                 embed_state, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (account_id, jmap_id) DO UPDATE SET \
                 mailbox_ids_json = excluded.mailbox_ids_json, \
                 keywords_json = excluded.keywords_json, \
                 updated_at = excluded.updated_at \
                 RETURNING id, (xmax = 0) AS inserted",
            )
            .bind(new_id("mail"))
            .bind(account_id)
            .bind(owner)
            .bind(&m.jmap_id)
            .bind(&m.thread_id)
            .bind(&m.message_id_hdr)
            .bind(&m.in_reply_to)
            .bind(&m.references_json)
            .bind(&m.from_addr)
            .bind(&m.from_name)
            .bind(&m.to_json)
            .bind(&m.cc_json)
            .bind(&m.reply_to_json)
            .bind(&m.subject)
            .bind(&m.sent_at)
            .bind(&m.received_at)
            .bind(&m.mailbox_ids_json)
            .bind(&m.keywords_json)
            .bind(&m.body_text)
            .bind(&m.body_source)
            .bind(&m.snippet)
            .bind(m.size)
            .bind(m.has_attachments)
            .bind(embed_on_insert)
            .bind(&m.received_at)
            .bind(now_iso())
            .fetch_one(&mut *tx)
            .await?;

            // Tombstoned rows keep their (account_id, jmap_id) key, so a
            // replay or move lands on the conflict arm — never resurrect
            // them into search.
            let deleted: Option<String> = crate::pgq::query_scalar::<String>(
                "SELECT deleted_at FROM mail_messages WHERE id = ? AND deleted_at IS NOT NULL",
            )
            .bind(&row.id)
            .fetch_optional(&mut *tx)
            .await?;
            let live_eligible = eligible && deleted.is_none();

            // Attachment metadata rows; blob_hash stays NULL (= bytes
            // pending) until the fetch pipeline stores them. Replays land on
            // DO NOTHING via the NULLS-NOT-DISTINCT unique key. Tombstoned
            // rows (incl. admin-redacted ones, whose attachment rows were
            // deleted) get nothing back — otherwise a metadata replay would
            // re-queue redacted bytes for download.
            if deleted.is_none() {
                for att in &m.attachments {
                    crate::pgq::query(
                        "INSERT INTO mail_attachments (id, message_id, jmap_blob_id, filename, \
                         mime, size, content_id, disposition, created_at) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                         ON CONFLICT (message_id, jmap_blob_id, content_id) DO NOTHING",
                    )
                    .bind(new_id("matt"))
                    .bind(&row.id)
                    .bind(&att.jmap_blob_id)
                    .bind(&att.filename)
                    .bind(&att.mime)
                    .bind(att.size)
                    .bind(&att.content_id)
                    .bind(&att.disposition)
                    .bind(now_iso())
                    .execute(&mut *tx)
                    .await?;
                }
            }

            if live_eligible {
                let title = if m.subject.trim().is_empty() {
                    "(no subject)"
                } else {
                    m.subject.as_str()
                };
                super::search::index_entity_conn(
                    &mut tx,
                    "mail",
                    &row.id,
                    title,
                    fts_clip(&m.body_text, FTS_CLIP_BYTES),
                    &[],
                )
                .await?;
                // A move back INTO ingest re-queues a previously skipped row.
                crate::pgq::query(
                    "UPDATE mail_messages SET embed_state = 'pending' \
                     WHERE id = ? AND embed_state = 'skip'",
                )
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
            } else {
                crate::pgq::query("DELETE FROM search WHERE kind = 'mail' AND ref_id = ?")
                    .bind(&row.id)
                    .execute(&mut *tx)
                    .await?;
                crate::pgq::query("DELETE FROM embeddings WHERE ref_kind = 'mail' AND ref_id = ?")
                    .bind(&row.id)
                    .execute(&mut *tx)
                    .await?;
                crate::pgq::query("UPDATE mail_messages SET embed_state = 'skip' WHERE id = ?")
                    .bind(&row.id)
                    .execute(&mut *tx)
                    .await?;
            }

            if row.inserted
                && live_eligible
                && m.mailbox_ids.iter().any(|id| inbox_ids.contains(id))
            {
                out.notify.push((row.id.clone(), m.subject.clone()));
            }
            out.stored += 1;
        }
        tx.commit().await?;
        Ok(out)
    }

    /// JMAP destroys: tombstone + drop retrieval rows in the SAME transaction
    /// (D6 — deleted mail must not stay searchable until a sweep).
    pub async fn mail_tombstone_batch(
        &self,
        account_id: &str,
        jmap_ids: &[String],
    ) -> Result<usize> {
        if jmap_ids.is_empty() {
            return Ok(0);
        }
        let mut tx = self.db().begin().await?;
        let mut n = 0usize;
        for jmap_id in jmap_ids {
            let Some(id) = crate::pgq::query_scalar::<String>(
                "UPDATE mail_messages SET deleted_at = ?, embed_state = 'skip' \
                 WHERE account_id = ? AND jmap_id = ? AND deleted_at IS NULL RETURNING id",
            )
            .bind(now_iso())
            .bind(account_id)
            .bind(jmap_id)
            .fetch_optional(&mut *tx)
            .await?
            else {
                continue;
            };
            crate::pgq::query("DELETE FROM search WHERE kind = 'mail' AND ref_id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            crate::pgq::query("DELETE FROM embeddings WHERE ref_kind = 'mail' AND ref_id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            n += 1;
        }
        tx.commit().await?;
        Ok(n)
    }
}

// ---- attachments (byte pipeline + serving + GC; plan A6) ------------------

/// One attachment awaiting bytes, as the fetch pipeline consumes it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MailAttachmentPending {
    pub id: String,
    pub jmap_blob_id: String,
    pub mime: String,
    /// Declared (server-reported) size — the pre-download oversize check.
    pub size: i64,
}

/// Everything the serving route needs, resolved in one query. `data` rides
/// along because household-scale attachments are ≤ the fetch cap anyway.
#[derive(Debug, Clone, sqlx::FromRow)]
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
        Ok(crate::pgq::query_as::<MailAttachmentPending>(
            "SELECT t.id, t.jmap_blob_id, t.mime, t.size FROM mail_attachments t \
             JOIN mail_messages m ON m.id = t.message_id \
             WHERE m.account_id = ? AND m.deleted_at IS NULL \
             AND t.blob_hash IS NULL AND t.skipped_reason IS NULL \
             ORDER BY t.created_at ASC, t.id ASC LIMIT ?",
        )
        .bind(account_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(self.db())
        .await?)
    }

    /// Permanently park an attachment ('oversize' | 'missing'); it leaves the
    /// pending queue and its chip renders dimmed. Never overwrites stored
    /// bytes.
    pub async fn mail_attachment_mark_skipped(&self, att_id: &str, reason: &str) -> Result<()> {
        crate::pgq::query(
            "UPDATE mail_attachments SET skipped_reason = ? WHERE id = ? AND blob_hash IS NULL",
        )
        .bind(reason)
        .bind(att_id)
        .execute(self.db())
        .await?;
        Ok(())
    }

    /// Store fetched bytes content-addressed: blob insert dedups on hash
    /// (identical attachments across messages share one row), then the
    /// attachment points at it. One transaction so a crash can't leave the
    /// pointer without the bytes.
    pub async fn mail_attachment_store_blob(
        &self,
        att_id: &str,
        hash: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let mut tx = self.db().begin().await?;
        crate::pgq::query(
            "INSERT INTO blobs (hash, size, mime, data, created_at) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (hash) DO NOTHING",
        )
        .bind(hash)
        .bind(bytes.len() as i64)
        .bind(mime)
        .bind(bytes)
        .bind(now_iso())
        .execute(&mut *tx)
        .await?;
        crate::pgq::query(
            "UPDATE mail_attachments SET blob_hash = ?, skipped_reason = NULL WHERE id = ?",
        )
        .bind(hash)
        .bind(att_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The serving route's lookup: attachment joined to its owning message
    /// for the user_scope ACL, bytes left-joined (NULL = not stored). By id
    /// only — blobs are NEVER addressable by hash from the outside.
    pub async fn mail_attachment_serve(&self, att_id: &str) -> Result<Option<MailAttachmentServe>> {
        Ok(crate::pgq::query_as::<MailAttachmentServe>(
            "SELECT m.user_scope, t.filename, t.mime, t.blob_hash, b.data \
             FROM mail_attachments t \
             JOIN mail_messages m ON m.id = t.message_id \
             LEFT JOIN blobs b ON b.hash = t.blob_hash \
             WHERE t.id = ?",
        )
        .bind(att_id)
        .fetch_optional(self.db())
        .await?)
    }

    /// Admin redaction (plan A6): scrub the body columns, tombstone, and drop
    /// every derived row (search, embeddings, attachments, now-orphaned
    /// blobs) in one transaction. Durability is guaranteed by the ingest
    /// conflict arm (metadata-only — body columns never rewritten), the
    /// tombstone check gating attachment re-inserts, and reconcile never
    /// re-fetching known jmap ids. Returns the owning namespace for the
    /// caller's post-commit wire event; None = no such message.
    pub async fn mail_message_redact(&self, id: &str) -> Result<Option<String>> {
        let mut tx = self.db().begin().await?;
        let ts = now_iso();
        let Some(owner) = crate::pgq::query_scalar::<String>(
            "UPDATE mail_messages SET body_text = '', snippet = '', subject = '[redacted]', \
             has_attachments = FALSE, deleted_at = ?, embed_state = 'skip', updated_at = ? \
             WHERE id = ? RETURNING user_scope",
        )
        .bind(&ts)
        .bind(&ts)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(None);
        };
        crate::pgq::query("DELETE FROM search WHERE kind = 'mail' AND ref_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        crate::pgq::query("DELETE FROM embeddings WHERE ref_kind = 'mail' AND ref_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let hashes: Vec<String> = crate::pgq::query_scalar::<String>(
            "SELECT DISTINCT blob_hash FROM mail_attachments \
             WHERE message_id = ? AND blob_hash IS NOT NULL",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        crate::pgq::query("DELETE FROM mail_attachments WHERE message_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for hash in &hashes {
            crate::pgq::query(
                "DELETE FROM blobs b WHERE b.hash = ? AND NOT EXISTS \
                 (SELECT 1 FROM mail_attachments a WHERE a.blob_hash = b.hash)",
            )
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(Some(owner))
    }

    /// Refcount blob GC (hive-mail runs it weekly): delete blobs no
    /// attachment points at, but only ones older than 24h — the grace window
    /// covers a fetch pipeline that has inserted the blob but not yet
    /// committed the attachment pointer in a racing transaction.
    pub async fn mail_blobs_gc(&self) -> Result<u64> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        Ok(crate::pgq::query(
            "DELETE FROM blobs b WHERE b.created_at < ? AND NOT EXISTS \
             (SELECT 1 FROM mail_attachments a WHERE a.blob_hash = b.hash)",
        )
        .bind(cutoff)
        .execute(self.db())
        .await?
        .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn seeded_store() -> Store {
        let pool = db::test_pool().await;
        let store = Store::new(pool);
        let now = "2026-07-05T00:00:00Z";

        crate::pgq::query(
            "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("acct-alice")
        .bind("alice")
        .bind("alice@example.test")
        .bind(now)
        .bind(now)
        .execute(store.db())
        .await
        .unwrap();
        crate::pgq::query(
            "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("acct-bob")
        .bind("bob")
        .bind("bob@example.test")
        .bind(now)
        .bind(now)
        .execute(store.db())
        .await
        .unwrap();
        crate::pgq::query(
            "INSERT INTO mail_mailboxes (id, account_id, jmap_id, name, role, sort_order) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("mbox-alice-inbox")
        .bind("acct-alice")
        .bind("inbox")
        .bind("Inbox")
        .bind("inbox")
        .bind(0_i64)
        .execute(store.db())
        .await
        .unwrap();
        crate::pgq::query(
            "INSERT INTO mail_messages (id, account_id, user_scope, jmap_thread_id, jmap_id, message_id_hdr, subject, from_name, from_addr, to_json, cc_json, received_at, keywords_json, snippet, body_text, has_attachments, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("msg-alice-1")
        .bind("acct-alice")
        .bind("alice")
        .bind("thread-shared")
        .bind("jmap-alice-1")
        .bind("<alice-1@example.test>")
        .bind("Quarterly bees")
        .bind("Bee Ops")
        .bind("ops@example.test")
        .bind(r#"[{"email":"alice@example.test"}]"#)
        .bind("[]")
        .bind("2026-07-04T12:00:00Z")
        .bind(r##"{"$flagged":true,"Bee Roadhouse":true,"$seen":true}"##)
        .bind("nectar budget")
        .bind("The nectar budget has fictional hive details.")
        .bind(false)
        .bind(now)
        .bind(now)
        .execute(store.db())
        .await
        .unwrap();
        crate::pgq::query(
            "INSERT INTO mail_messages (id, account_id, user_scope, jmap_thread_id, jmap_id, message_id_hdr, subject, from_name, from_addr, to_json, cc_json, received_at, snippet, body_text, has_attachments, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("msg-bob-1")
        .bind("acct-bob")
        .bind("bob")
        .bind("thread-shared")
        .bind("jmap-bob-1")
        .bind("<bob-1@example.test>")
        .bind("Private swarm")
        .bind("Bob Ops")
        .bind("bobops@example.test")
        .bind(r#"[{"email":"bob@example.test"}]"#)
        .bind("[]")
        .bind("2026-07-04T13:00:00Z")
        .bind("wax budget")
        .bind("The wax budget must stay in Bob's namespace.")
        .bind(false)
        .bind(now)
        .bind(now)
        .execute(store.db())
        .await
        .unwrap();

        store
    }

    #[tokio::test]
    async fn mail_queries_are_viewer_gated() {
        let store = seeded_store().await;

        let alice_accounts = store.mail_accounts_list(Some("alice")).await.unwrap();
        assert_eq!(alice_accounts.len(), 1);
        assert_eq!(alice_accounts[0].id, "acct-alice");

        let alice_messages = store
            .mail_messages_list(Some("alice"), None, None, 20)
            .await
            .unwrap();
        assert_eq!(alice_messages.len(), 1);
        assert_eq!(alice_messages[0].id, "msg-alice-1");
        assert_eq!(
            alice_messages[0].labels,
            vec![
                "flagged".to_string(),
                "Bee Roadhouse".to_string(),
                "seen".to_string(),
            ]
        );

        let hits = store
            .mail_search("budget", Some("alice"), 20)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "msg-alice-1");

        let thread = store
            .mail_thread_get("thread-shared", Some("alice"))
            .await
            .unwrap();
        assert_eq!(thread.messages.len(), 1);
        assert_eq!(thread.messages[0].summary.id, "msg-alice-1");

        let admin_thread = store.mail_thread_get("thread-shared", None).await.unwrap();
        assert_eq!(admin_thread.messages.len(), 2);
    }

    #[tokio::test]
    async fn account_lifecycle_create_toggle_resync_delete() {
        // Same constant every test uses; set_var is process-global but
        // idempotent here.
        std::env::set_var("HIVE_CRED_KEY", "mail-store-test-key");
        let pool = db::test_pool().await;
        let store = Store::new(pool);

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
        let cred_id: Option<String> =
            crate::pgq::query_scalar::<String>("SELECT cred_id FROM mail_accounts WHERE id = ?")
                .bind(&view.id)
                .fetch_optional(store.db())
                .await
                .unwrap();
        let secret = store
            .cc_cred_decrypt_by_id(cred_id.as_deref().unwrap())
            .await
            .unwrap();
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
        crate::pgq::query(
            "UPDATE mail_accounts SET attempts = 5, next_attempt_at = '2099-01-01T00:00:00.000Z' WHERE id = ?",
        )
        .bind(&view.id)
        .execute(store.db())
        .await
        .unwrap();
        assert!(store
            .mail_account_set_enabled(&view.id, true)
            .await
            .unwrap());
        let attempts: Option<i64> =
            crate::pgq::query_scalar::<i64>("SELECT attempts FROM mail_accounts WHERE id = ?")
                .bind(&view.id)
                .fetch_optional(store.db())
                .await
                .unwrap();
        assert_eq!(attempts, Some(0));

        // Force-resync plants the sentinel that routes the next delta into
        // reconciliation.
        assert!(store.mail_account_force_resync(&view.id).await.unwrap());
        let state: Option<String> = crate::pgq::query_scalar::<String>(
            "SELECT email_state FROM mail_accounts WHERE id = ?",
        )
        .bind(&view.id)
        .fetch_optional(store.db())
        .await
        .unwrap();
        assert_eq!(state.as_deref(), Some("force-resync"));

        // Seed a message plus every derived row, then delete the account and
        // assert nothing survives — including the vault row and orphan blob.
        let now = "2026-07-09T00:00:00.000Z";
        crate::pgq::query(
            "INSERT INTO mail_messages (id, account_id, user_scope, jmap_thread_id, jmap_id, subject, from_addr, to_json, cc_json, received_at, snippet, body_text, has_attachments, created_at, updated_at) \
             VALUES (?, ?, 'alice', 't1', 'j1', 's', 'a@b.c', '[]', '[]', ?, '', 'body', TRUE, ?, ?)",
        )
        .bind("msg-cascade-1")
        .bind(&view.id)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(store.db())
        .await
        .unwrap();
        store
            .index_entity("mail", "msg-cascade-1", "s", "body", &[])
            .await
            .unwrap();
        crate::pgq::query("INSERT INTO blobs (hash, size, mime, data, created_at) VALUES ('h1', 1, 'text/plain', ?, ?)")
            .bind(vec![0u8])
            .bind(now)
            .execute(store.db())
            .await
            .unwrap();
        crate::pgq::query(
            "INSERT INTO mail_attachments (id, message_id, blob_hash, jmap_blob_id, created_at) VALUES ('att1', 'msg-cascade-1', 'h1', 'b1', ?)",
        )
        .bind(now)
        .execute(store.db())
        .await
        .unwrap();

        assert!(store.mail_account_delete(&view.id).await.unwrap());
        for (what, sql) in [
            ("account", "SELECT COUNT(*) FROM mail_accounts WHERE id = ?"),
            ("messages", "SELECT COUNT(*) FROM mail_messages WHERE account_id = ?"),
            ("search", "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND ref_id = 'msg-cascade-1' AND ? = ?"),
        ] {
            let mut q = crate::pgq::query_scalar::<i64>(sql).bind(&view.id);
            if sql.contains("? = ?") {
                q = q.bind(&view.id);
            }
            let n = q.fetch_one(store.db()).await.unwrap();
            assert_eq!(n, 0, "{what} rows survived the cascade");
        }
        let creds: i64 =
            crate::pgq::query_scalar::<i64>("SELECT COUNT(*) FROM cc_credentials WHERE id = ?")
                .bind(cred_id.as_deref().unwrap())
                .fetch_one(store.db())
                .await
                .unwrap();
        assert_eq!(creds, 0, "vault credential survived the cascade");
        let blobs: i64 = crate::pgq::query_scalar::<i64>("SELECT COUNT(*) FROM blobs")
            .fetch_one(store.db())
            .await
            .unwrap();
        assert_eq!(blobs, 0, "orphan blob survived the cascade");
    }

    #[tokio::test]
    async fn mailbox_ingest_toggle_gates_retrieval() {
        let store = seeded_store().await;
        // Give alice's message mailbox membership + a search row, as the
        // sink would have.
        crate::pgq::query(
            "UPDATE mail_messages SET mailbox_ids_json = '[\"inbox\"]', embed_state = 'pending' WHERE id = 'msg-alice-1'",
        )
        .execute(store.db())
        .await
        .unwrap();
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
        let search_rows: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND ref_id = 'msg-alice-1'",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(search_rows, 0);
        let embed_state: Option<String> = crate::pgq::query_scalar::<String>(
            "SELECT embed_state FROM mail_messages WHERE id = 'msg-alice-1'",
        )
        .fetch_optional(store.db())
        .await
        .unwrap();
        assert_eq!(embed_state.as_deref(), Some("skip"));
        let row_still_there: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM mail_messages WHERE id = 'msg-alice-1'",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(row_still_there, 1);

        // ON: the account's backfill re-arms so history gets picked up.
        assert!(store
            .mail_mailbox_set_ingest("mbox-alice-inbox", true)
            .await
            .unwrap());
        let status: Option<String> = crate::pgq::query_scalar::<String>(
            "SELECT backfill_status FROM mail_accounts WHERE id = 'acct-alice'",
        )
        .fetch_optional(store.db())
        .await
        .unwrap();
        assert_eq!(status.as_deref(), Some("pending"));
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
    async fn ingest_batch_is_idempotent_and_metadata_only_on_replay() {
        let store = seeded_store().await;
        crate::pgq::query("UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'")
            .execute(store.db())
            .await
            .unwrap();
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

        // FTS row exists, embed queued.
        let fts: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND title = 'subject j-new-1'",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(fts, 1);

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
        #[derive(sqlx::FromRow)]
        struct Row {
            body_text: String,
            keywords_json: String,
        }
        let row = crate::pgq::query_as::<Row>(
            "SELECT body_text, keywords_json FROM mail_messages WHERE account_id = 'acct-alice' AND jmap_id = 'j-new-1'",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert!(
            row.body_text.contains("honeycomb"),
            "body is immutable on conflict"
        );
        assert!(
            row.keywords_json.contains("$seen"),
            "metadata updates apply"
        );

        let count: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM mail_messages WHERE account_id = 'acct-alice' AND jmap_id = 'j-new-1'",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(count, 1, "unique key absorbed the replay");
    }

    #[tokio::test]
    async fn ingest_gates_junk_and_non_ingest_mailboxes() {
        let store = seeded_store().await;
        crate::pgq::query("UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'")
            .execute(store.db())
            .await
            .unwrap();
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
        let fts: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND (title = 'subject j-junk' OR title = 'subject j-elsewhere')",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(fts, 0, "neither row is searchable");
        let skipped: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM mail_messages WHERE jmap_id IN ('j-junk', 'j-elsewhere') AND embed_state = 'skip'",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(skipped, 2);
    }

    #[tokio::test]
    async fn tombstone_removes_retrieval_in_the_same_batch_and_stays_dead() {
        let store = seeded_store().await;
        crate::pgq::query("UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'")
            .execute(store.db())
            .await
            .unwrap();
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
        let fts: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND title = 'subject j-dead'",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(fts, 0, "deleted mail leaves search in the same batch");

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
        let fts_after: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND title = 'subject j-dead'",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(fts_after, 0, "replay must not resurrect a tombstoned row");
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
        let links: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM links WHERE source_kind = 'journal' AND source_id = ? \
             AND target_kind = 'mail' AND target_id = 'msg-alice-1' AND rel = 'cites'",
        )
        .bind(&entry.entry.id)
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(links, 1);
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
        let cross_links: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM links WHERE source_id = ? AND target_kind = 'mail'",
        )
        .bind(&cross.entry.id)
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(cross_links, 0, "cross-namespace citation must not link");

        // Tombstoned mail stops resolving (the raw token stays visible).
        crate::pgq::query(
            "UPDATE mail_messages SET deleted_at = '2026-07-09T00:00:00.000Z' WHERE id = 'msg-alice-1'",
        )
        .execute(store.db())
        .await
        .unwrap();
        let dead_refs = store.refs_for(&entry.entry.body).await.unwrap();
        assert!(dead_refs.is_empty(), "dead citations resolve to nothing");
    }

    #[tokio::test]
    async fn backoff_disables_after_eight_failures() {
        std::env::set_var("HIVE_CRED_KEY", "mail-store-test-key");
        let pool = db::test_pool().await;
        let store = Store::new(pool);
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
        crate::pgq::query("UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'")
            .execute(store.db())
            .await
            .unwrap();
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

        let rows: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM mail_attachments WHERE blob_hash IS NULL AND skipped_reason IS NULL",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(rows, 2, "metadata rows land with bytes pending");

        // Replay: the unique key absorbs it — no duplicate rows.
        store
            .mail_ingest_batch("acct-alice", "alice", &ingest, &inbox, vec![msg])
            .await
            .unwrap();
        let rows_after: i64 =
            crate::pgq::query_scalar::<i64>("SELECT COUNT(*) FROM mail_attachments")
                .fetch_one(store.db())
                .await
                .unwrap();
        assert_eq!(rows_after, 2, "replay must not duplicate attachment rows");
    }

    #[tokio::test]
    async fn attachments_pending_excludes_skipped_stored_and_deleted() {
        let store = seeded_store().await;
        crate::pgq::query("UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'")
            .execute(store.db())
            .await
            .unwrap();
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
                crate::pgq::query_scalar::<String>(
                    "SELECT id FROM mail_attachments WHERE jmap_blob_id = ?",
                )
                .bind(blob)
                .fetch_one(store.db())
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
    async fn attachment_store_blob_dedups_by_hash() {
        let store = seeded_store().await;
        let now = "2026-07-09T00:00:00.000Z";
        for (att, blob) in [("att-d1", "b1"), ("att-d2", "b2")] {
            crate::pgq::query(
                "INSERT INTO mail_attachments (id, message_id, jmap_blob_id, created_at) \
                 VALUES (?, 'msg-alice-1', ?, ?)",
            )
            .bind(att)
            .bind(blob)
            .bind(now)
            .execute(store.db())
            .await
            .unwrap();
        }

        // Same bytes fetched twice (e.g. the same PDF on two messages).
        let bytes = b"identical attachment bytes";
        let hash = blake3_hex_for_tests(bytes);
        store
            .mail_attachment_store_blob("att-d1", &hash, "application/pdf", bytes)
            .await
            .unwrap();
        store
            .mail_attachment_store_blob("att-d2", &hash, "application/pdf", bytes)
            .await
            .unwrap();

        let blobs: i64 = crate::pgq::query_scalar::<i64>("SELECT COUNT(*) FROM blobs")
            .fetch_one(store.db())
            .await
            .unwrap();
        assert_eq!(blobs, 1, "identical bytes share one blob row");
        let pointed: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM mail_attachments WHERE blob_hash = ?",
        )
        .bind(&hash)
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(pointed, 2, "both attachments point at the shared blob");

        // The thread payload now reports them stored.
        let thread = store
            .mail_thread_get("thread-shared", Some("alice"))
            .await
            .unwrap();
        let atts = &thread.messages[0].attachments;
        assert_eq!(atts.len(), 2);
        assert!(atts.iter().all(|a| a.stored));
    }

    /// THE redaction invariant (plan A6): after mail_message_redact, a full
    /// ingest replay of the same jmap_id (reconcile/delta metadata update)
    /// must not resurrect body, subject, search, or attachment rows.
    #[tokio::test]
    async fn redact_scrubs_everything_and_replay_cannot_resurrect() {
        let store = seeded_store().await;
        crate::pgq::query("UPDATE mail_mailboxes SET ingest = TRUE WHERE id = 'mbox-alice-inbox'")
            .execute(store.db())
            .await
            .unwrap();
        let (ingest, inbox) = store.mail_mailbox_sets("acct-alice").await.unwrap();
        let mut msg = ingest_msg("j-redact", "inbox", &[]);
        msg.attachments = vec![ingest_att("blob-r", "secret.pdf", 10)];
        msg.has_attachments = true;
        store
            .mail_ingest_batch("acct-alice", "alice", &ingest, &inbox, vec![msg.clone()])
            .await
            .unwrap();
        let mail_id: String = crate::pgq::query_scalar::<String>(
            "SELECT id FROM mail_messages WHERE jmap_id = 'j-redact'",
        )
        .fetch_one(store.db())
        .await
        .unwrap();
        let att_id: String = crate::pgq::query_scalar::<String>(
            "SELECT id FROM mail_attachments WHERE message_id = ?",
        )
        .bind(&mail_id)
        .fetch_one(store.db())
        .await
        .unwrap();
        store
            .mail_attachment_store_blob(&att_id, "redact-hash", "application/pdf", b"secret")
            .await
            .unwrap();
        crate::pgq::query(
            "INSERT INTO embeddings (ref_kind, ref_id, model, dim, vec, hash, created_at) \
             VALUES ('mail', ?, 'hash', 4, ?, 'h', '2026-07-09T00:00:00.000Z')",
        )
        .bind(&mail_id)
        .bind(vec![0u8; 16])
        .execute(store.db())
        .await
        .unwrap();

        let owner = store.mail_message_redact(&mail_id).await.unwrap();
        assert_eq!(owner.as_deref(), Some("alice"));

        #[derive(sqlx::FromRow)]
        struct Row {
            body_text: String,
            snippet: String,
            subject: String,
            has_attachments: bool,
            deleted_at: Option<String>,
            embed_state: String,
        }
        let row = crate::pgq::query_as::<Row>(
            "SELECT body_text, snippet, subject, has_attachments, deleted_at, embed_state \
             FROM mail_messages WHERE id = ?",
        )
        .bind(&mail_id)
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(row.body_text, "");
        assert_eq!(row.snippet, "");
        assert_eq!(row.subject, "[redacted]");
        assert!(!row.has_attachments);
        assert!(row.deleted_at.is_some());
        assert_eq!(row.embed_state, "skip");
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
            let n: i64 = crate::pgq::query_scalar::<i64>(sql)
                .bind(&mail_id)
                .fetch_one(store.db())
                .await
                .unwrap();
            assert_eq!(n, 0, "{what} rows survived redaction");
        }
        let blobs: i64 = crate::pgq::query_scalar::<i64>("SELECT COUNT(*) FROM blobs")
            .fetch_one(store.db())
            .await
            .unwrap();
        assert_eq!(blobs, 0, "orphaned blob survived redaction");

        // The replay: same jmap_id, hostile body + attachments. The conflict
        // arm is metadata-only and the tombstone gates attachments, so
        // nothing comes back.
        let mut replay = msg;
        replay.body_text = "RESURRECTED BODY".into();
        replay.subject = "resurrected subject".into();
        store
            .mail_ingest_batch("acct-alice", "alice", &ingest, &inbox, vec![replay])
            .await
            .unwrap();
        let after = crate::pgq::query_as::<Row>(
            "SELECT body_text, snippet, subject, has_attachments, deleted_at, embed_state \
             FROM mail_messages WHERE id = ?",
        )
        .bind(&mail_id)
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(after.body_text, "", "replay must not restore the body");
        assert_eq!(
            after.subject, "[redacted]",
            "replay must not restore the subject"
        );
        assert!(
            after.deleted_at.is_some(),
            "replay must not clear the tombstone"
        );
        let search: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM search WHERE kind = 'mail' AND ref_id = ?",
        )
        .bind(&mail_id)
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(search, 0, "replay must not re-index a redacted row");
        let atts: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT COUNT(*) FROM mail_attachments WHERE message_id = ?",
        )
        .bind(&mail_id)
        .fetch_one(store.db())
        .await
        .unwrap();
        assert_eq!(atts, 0, "replay must not re-queue redacted attachments");
    }

    #[tokio::test]
    async fn blob_gc_deletes_only_unreferenced_aged_blobs() {
        let store = seeded_store().await;
        let old = "2020-01-01T00:00:00.000Z";
        let fresh = (chrono::Utc::now())
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        for (hash, created) in [
            ("gc-old-orphan", old),
            ("gc-old-referenced", old),
            ("gc-fresh-orphan", fresh.as_str()),
        ] {
            crate::pgq::query(
                "INSERT INTO blobs (hash, size, mime, data, created_at) VALUES (?, 1, 'text/plain', ?, ?)",
            )
            .bind(hash)
            .bind(vec![0u8])
            .bind(created)
            .execute(store.db())
            .await
            .unwrap();
        }
        crate::pgq::query(
            "INSERT INTO mail_attachments (id, message_id, blob_hash, jmap_blob_id, created_at) \
             VALUES ('att-gc', 'msg-alice-1', 'gc-old-referenced', 'bgc', ?)",
        )
        .bind(old)
        .execute(store.db())
        .await
        .unwrap();

        let swept = store.mail_blobs_gc().await.unwrap();
        assert_eq!(swept, 1, "exactly the aged orphan goes");
        let left: Vec<String> =
            crate::pgq::query_scalar::<String>("SELECT hash FROM blobs ORDER BY hash")
                .fetch_all(store.db())
                .await
                .unwrap();
        assert_eq!(
            left,
            vec![
                "gc-fresh-orphan".to_string(),
                "gc-old-referenced".to_string()
            ],
            "referenced + in-grace blobs survive"
        );
    }

    /// blake3 lives in hive-mail (bytes are hashed before they reach the
    /// store), so tests hash with a tiny fixed stand-in — the store treats
    /// hashes as opaque content addresses.
    fn blake3_hex_for_tests(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }
}
