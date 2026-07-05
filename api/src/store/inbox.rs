// Per-actor inbox (store.ts `inbox`).

use anyhow::Result;
use hive_shared::{InboxItem, InboxReason};
use serde_json::json;
use sqlx::Row;

use super::{new_id, now_iso, snip140, Store};

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub async fn inbox_add(
        &self,
        recipient: &str,
        from: &str,
        reason: InboxReason,
        ref_kind: &str,
        ref_id: &str,
        entry_id: Option<&str>,
        snippet: &str,
    ) -> Result<Option<InboxItem>> {
        if recipient == from {
            return Ok(None); // don't notify yourself
        }
        let item = InboxItem {
            id: new_id("inb"),
            recipient: recipient.to_string(),
            from: from.to_string(),
            reason,
            ref_kind: ref_kind.to_string(),
            ref_id: ref_id.to_string(),
            entry_id: entry_id.map(String::from),
            snippet: snip140(snippet),
            created_at: now_iso(),
            read_at: None,
        };
        crate::pgq::query(
            r#"INSERT INTO inbox (id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)"#,
        )
        .bind(&item.id)
        .bind(&item.recipient)
        .bind(&item.from)
        .bind(item.reason.as_str())
        .bind(&item.ref_kind)
        .bind(&item.ref_id)
        .bind(&item.entry_id)
        .bind(&item.snippet)
        .bind(&item.created_at)
        .execute(self.db())
        .await?;
        self.emit(
            "inbox.delivered",
            from,
            json!({"to": recipient, "reason": item.reason.as_str(), "ref_kind": item.ref_kind.as_str(), "ref_id": ref_id}),
        )
        .await?;
        Ok(Some(item))
    }

    pub async fn inbox_list(&self, recipient: &str, unread_only: bool) -> Result<Vec<InboxItem>> {
        let sql = if unread_only {
            r#"SELECT id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at
               FROM inbox WHERE recipient = ? AND read_at IS NULL ORDER BY created_at DESC"#
        } else {
            r#"SELECT id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at
               FROM inbox WHERE recipient = ? ORDER BY created_at DESC"#
        };
        let rows = crate::pgq::query(sql)
            .bind(recipient)
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_inbox).collect()
    }

    pub async fn inbox_mark_read(&self, item_id: &str) -> Result<u64> {
        let res =
            crate::pgq::query("UPDATE inbox SET read_at = ? WHERE id = ? AND read_at IS NULL")
                .bind(now_iso())
                .bind(item_id)
                .execute(self.db())
                .await?;
        Ok(res.rows_affected())
    }

    pub async fn inbox_mark_all_read(&self, recipient: &str) -> Result<u64> {
        let res = crate::pgq::query(
            "UPDATE inbox SET read_at = ? WHERE recipient = ? AND read_at IS NULL",
        )
        .bind(now_iso())
        .bind(recipient)
        .execute(self.db())
        .await?;
        Ok(res.rows_affected())
    }

    pub async fn inbox_unread_count(&self, recipient: &str) -> Result<i64> {
        Ok(crate::pgq::query_scalar(
            "SELECT count(*) FROM inbox WHERE recipient = ? AND read_at IS NULL",
        )
        .bind(recipient)
        .fetch_one(self.db())
        .await?)
    }
}

pub(crate) fn row_to_inbox(r: &sqlx::postgres::PgRow) -> Result<InboxItem> {
    Ok(InboxItem {
        id: r.try_get("id")?,
        recipient: r.try_get("recipient")?,
        from: r.try_get("from")?,
        reason: InboxReason::from_str_lossy(r.try_get::<String, _>("reason")?.as_str()),
        ref_kind: r.try_get("ref_kind")?,
        ref_id: r.try_get("ref_id")?,
        entry_id: r.try_get("entry_id")?,
        snippet: r.try_get("snippet")?,
        created_at: r.try_get("created_at")?,
        read_at: r.try_get("read_at")?,
    })
}
