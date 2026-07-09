use anyhow::{anyhow, Result};
use serde::Serialize;

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

#[derive(Debug, Clone, Serialize)]
pub struct MailThreadMessage {
    #[serde(flatten)]
    pub summary: MailMessageSummary,
    pub body_text: String,
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
        let messages: Vec<MailThreadMessage> =
            rows.into_iter().map(MailThreadMessage::from).collect();
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
}
