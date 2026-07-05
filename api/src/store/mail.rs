use anyhow::Result;
use serde::Serialize;

use super::Store;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MailAccount {
    pub id: String,
    pub label: String,
    pub address: String,
    pub provider: Option<String>,
    pub last_synced_at: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct MailMessageRow {
    pub id: String,
    pub account_id: String,
    pub mailbox: Option<String>,
    pub thread_id: String,
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
    pub mailbox: Option<String>,
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
            mailbox: row.mailbox,
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
            clauses.push("(m.subject ILIKE ? OR m.from_addr ILIKE ? OR COALESCE(m.from_name, '') ILIKE ? OR m.snippet ILIKE ? OR m.body_text ILIKE ?)");
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
                .bind(needle);
        }
        Ok(q.bind(limit).fetch_all(self.db()).await?)
    }
}

fn mail_message_select(suffix: &str) -> String {
    format!(
        "SELECT m.id, m.account_id, NULL::TEXT AS mailbox, m.jmap_thread_id AS thread_id, \
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
            "INSERT INTO mail_messages (id, account_id, user_scope, jmap_thread_id, jmap_id, message_id_hdr, subject, from_name, from_addr, to_json, cc_json, received_at, snippet, body_text, has_attachments, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
}
