// Per-actor inbox (store.ts `inbox`). Standalone adds are entity.create
// {kind:"inbox"} records; the mark-read path is the fold-contract v2
// entity.update {kind:"inbox", fields:{read_at}} record (the 1.5 report's
// flagged gap). journal.append's fan-out stays inside its own payload.

use anyhow::Result;
use hive_shared::{InboxItem, InboxReason};
use serde_json::json;

use super::{new_id, now_iso, snip140, Draft, Store};

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
        let draft = inbox_create_draft(&item);
        self.run(move |core| core.commit(vec![draft])).await?;
        self.emit(
            "inbox.delivered",
            from,
            json!({"to": recipient, "reason": item.reason.as_str(), "ref_kind": item.ref_kind.as_str(), "ref_id": ref_id}),
        )
        .await?;
        Ok(Some(item))
    }

    pub async fn inbox_list(&self, recipient: &str, unread_only: bool) -> Result<Vec<InboxItem>> {
        let recipient = recipient.to_string();
        self.run(move |core| {
            let sql = if unread_only {
                r#"SELECT id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at
                   FROM inbox WHERE recipient = ?1 AND read_at IS NULL ORDER BY created_at DESC"#
            } else {
                r#"SELECT id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at
                   FROM inbox WHERE recipient = ?1 ORDER BY created_at DESC"#
            };
            let mut stmt = core.conn().prepare(sql)?;
            let rows = stmt.query_map(rusqlite::params![recipient], row_to_inbox)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn inbox_mark_read(&self, item_id: &str) -> Result<u64> {
        let item_id = item_id.to_string();
        self.run(move |core| {
            // The command layer decides applicability (unread + exists); the
            // record then carries the final value.
            let unread: bool = core.conn().query_row(
                "SELECT EXISTS(SELECT 1 FROM inbox WHERE id = ?1 AND read_at IS NULL)",
                rusqlite::params![item_id],
                |r| r.get(0),
            )?;
            if !unread {
                return Ok(0);
            }
            let now = now_iso();
            core.commit(vec![Draft::new(
                crate::oplog::kind::ENTITY_UPDATE,
                "system",
                &now,
                json!({"kind": "inbox", "id": item_id, "fields": {"read_at": now}}),
            )])?;
            Ok(1)
        })
        .await
    }

    pub async fn inbox_mark_all_read(&self, recipient: &str) -> Result<u64> {
        let recipient = recipient.to_string();
        self.run(move |core| {
            let ids: Vec<String> = {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT id FROM inbox WHERE recipient = ?1 AND read_at IS NULL")?;
                let rows = stmt.query_map(rusqlite::params![recipient], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            if ids.is_empty() {
                return Ok(0);
            }
            let now = now_iso();
            let drafts: Vec<Draft> = ids
                .iter()
                .map(|id| {
                    Draft::new(
                        crate::oplog::kind::ENTITY_UPDATE,
                        "system",
                        &now,
                        json!({"kind": "inbox", "id": id, "fields": {"read_at": now}}),
                    )
                })
                .collect();
            let n = drafts.len() as u64;
            core.commit(drafts)?;
            Ok(n)
        })
        .await
    }

    pub async fn inbox_unread_count(&self, recipient: &str) -> Result<i64> {
        let recipient = recipient.to_string();
        self.run(move |core| {
            Ok(core.conn().query_row(
                "SELECT count(*) FROM inbox WHERE recipient = ?1 AND read_at IS NULL",
                rusqlite::params![recipient],
                |r| r.get(0),
            )?)
        })
        .await
    }
}

/// entity.create {kind:"inbox"} draft for a standalone notification.
pub(crate) fn inbox_create_draft(item: &InboxItem) -> Draft {
    Draft::new(
        crate::oplog::kind::ENTITY_CREATE,
        &item.from,
        &item.created_at,
        json!({"kind": "inbox", "id": item.id, "fields": {
            "recipient": item.recipient, "from": item.from,
            "reason": item.reason.as_str(), "ref_kind": item.ref_kind,
            "ref_id": item.ref_id, "entry_id": item.entry_id,
            "snippet": item.snippet, "created_at": item.created_at,
        }}),
    )
}

/// The journal.append `inbox` array item shape (pre-computed fan-out).
#[allow(clippy::too_many_arguments)]
pub(crate) fn inbox_payload_item(
    recipient: &str,
    from: &str,
    reason: InboxReason,
    ref_kind: &str,
    ref_id: &str,
    entry_id: Option<&str>,
    snippet: &str,
    created_at: &str,
) -> serde_json::Value {
    json!({
        "id": new_id("inb"),
        "recipient": recipient, "from": from, "reason": reason.as_str(),
        "ref_kind": ref_kind, "ref_id": ref_id, "entry_id": entry_id,
        "snippet": snip140(snippet), "created_at": created_at,
    })
}

/// ref_kind passes through as a string: with user-defined entity types an
/// enum-unknown kind is a valid row, and nothing mislabels without the lossy
/// default.
pub(crate) fn row_to_inbox(r: &rusqlite::Row) -> rusqlite::Result<InboxItem> {
    Ok(InboxItem {
        id: r.get("id")?,
        recipient: r.get("recipient")?,
        from: r.get("from")?,
        reason: InboxReason::from_str_lossy(r.get::<_, String>("reason")?.as_str()),
        ref_kind: r.get("ref_kind")?,
        ref_id: r.get("ref_id")?,
        entry_id: r.get("entry_id")?,
        snippet: r.get("snippet")?,
        created_at: r.get("created_at")?,
        read_at: r.get("read_at")?,
    })
}
