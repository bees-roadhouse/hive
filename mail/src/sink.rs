//! jmap-sync trait implementations over the hive store. All SQL lives in
//! api/src/store/mail.rs (tested under test_pool); this module only maps
//! shapes and emits the post-commit events.

use std::collections::HashSet;

use async_trait::async_trait;
use hive_api::store::mail::{MailIngestAttachment, MailIngestMessage};
use hive_api::store::Store;
use hive_shared::InboxReason;
use jmap_sync::{
    BackfillState, CursorStore, MailSink, MailboxInfo, NormalizedMessage, SyncCursor, SyncError,
};

fn sink_err(e: anyhow::Error) -> SyncError {
    SyncError::Sink(format!("{e:#}"))
}

pub(crate) struct StoreCursor {
    pub store: Store,
    pub account_id: String,
}

#[async_trait]
impl CursorStore for StoreCursor {
    async fn load(&self) -> Result<SyncCursor, SyncError> {
        let (email_state, mailbox_state, status, cursor) = self
            .store
            .mail_cursor_load(&self.account_id)
            .await
            .map_err(|e| SyncError::Cursor(format!("{e:#}")))?;
        let backfill = match status.as_str() {
            "complete" => BackfillState::Complete,
            "in_progress" => match cursor
                .as_ref()
                .and_then(|c| serde_json::from_value::<BackfillState>(c.clone()).ok())
            {
                Some(state @ BackfillState::InProgress { .. }) => state,
                // A half-written cursor restarts the backfill; the unique key
                // makes the re-run duplicate-free.
                _ => BackfillState::Pending,
            },
            _ => BackfillState::Pending,
        };
        Ok(SyncCursor {
            email_state,
            mailbox_state,
            backfill,
        })
    }

    async fn save(&self, cursor: &SyncCursor) -> Result<(), SyncError> {
        let (status, backfill_cursor) = match &cursor.backfill {
            BackfillState::Pending => ("pending", None),
            BackfillState::Complete => ("complete", None),
            state @ BackfillState::InProgress { .. } => (
                "in_progress",
                Some(serde_json::to_value(state).map_err(|e| SyncError::Cursor(e.to_string()))?),
            ),
        };
        self.store
            .mail_cursor_save(
                &self.account_id,
                cursor.email_state.as_deref(),
                cursor.mailbox_state.as_deref(),
                status,
                backfill_cursor.as_ref(),
            )
            .await
            .map_err(|e| SyncError::Cursor(format!("{e:#}")))
    }
}

pub(crate) struct StoreSink {
    pub store: Store,
    pub account_id: String,
    pub owner: String,
    pub ingest_ids: HashSet<String>,
    pub inbox_ids: HashSet<String>,
    /// D10: wire + inbox emission are suppressed during backfill — a
    /// 100k-message import must not refetch-storm every open SPA client.
    pub suppress_events: bool,
}

fn to_ingest(m: NormalizedMessage) -> MailIngestMessage {
    let attachments: Vec<MailIngestAttachment> = m
        .attachments
        .into_iter()
        .map(|a| MailIngestAttachment {
            jmap_blob_id: a.jmap_blob_id,
            filename: a.filename,
            mime: a.mime,
            size: a.size as i64,
            content_id: a.content_id,
            disposition: a.disposition,
        })
        .collect();
    MailIngestMessage {
        references_json: serde_json::to_string(&m.references).unwrap_or_else(|_| "[]".into()),
        to_json: serde_json::to_string(&m.to).unwrap_or_else(|_| "[]".into()),
        cc_json: serde_json::to_string(&m.cc).unwrap_or_else(|_| "[]".into()),
        reply_to_json: serde_json::to_string(&m.reply_to).unwrap_or_else(|_| "[]".into()),
        mailbox_ids_json: serde_json::to_string(&m.mailbox_ids).unwrap_or_else(|_| "[]".into()),
        keywords_json: keywords_object(&m.keywords),
        jmap_id: m.jmap_id,
        thread_id: m.thread_id,
        message_id_hdr: m.message_id_hdr,
        in_reply_to: m.in_reply_to,
        from_addr: m.from_addr,
        from_name: m.from_name,
        subject: m.subject,
        sent_at: m.sent_at,
        received_at: m.received_at,
        mailbox_ids: m.mailbox_ids,
        keywords: m.keywords,
        body_text: m.body_text,
        body_source: m.body_source.as_str().to_string(),
        snippet: m.snippet,
        size: m.size as i64,
        has_attachments: !attachments.is_empty(),
        attachments,
        parse_error: m.parse_error,
    }
}

/// Keywords store in the JMAP object shape (`{"$seen": true}`) — the read
/// side's label parser already speaks it.
fn keywords_object(keywords: &[String]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = keywords
        .iter()
        .map(|k| (k.clone(), serde_json::Value::Bool(true)))
        .collect();
    serde_json::Value::Object(map).to_string()
}

#[async_trait]
impl MailSink for StoreSink {
    async fn upsert_batch(&self, batch: Vec<NormalizedMessage>) -> Result<(), SyncError> {
        let msgs: Vec<MailIngestMessage> = batch.into_iter().map(to_ingest).collect();
        let outcome = self
            .store
            .mail_ingest_batch(
                &self.account_id,
                &self.owner,
                &self.ingest_ids,
                &self.inbox_ids,
                msgs,
            )
            .await
            .map_err(sink_err)?;
        if !self.suppress_events {
            for (mail_id, subject) in outcome.notify {
                // ids only on the wire (globally readable, pruned to 2000);
                // the inbox row may carry the subject now that inbox_list is
                // viewer-gated (Phase 0).
                self.store
                    .emit(
                        "mail.received",
                        &self.owner,
                        serde_json::json!({"id": mail_id, "owner": self.owner}),
                    )
                    .await
                    .map_err(sink_err)?;
                self.store
                    .inbox_add(
                        &self.owner,
                        "hive-mail",
                        InboxReason::Mail,
                        "mail",
                        &mail_id,
                        None,
                        &subject,
                    )
                    .await
                    .map_err(sink_err)?;
            }
        }
        Ok(())
    }

    async fn tombstone(&self, jmap_ids: Vec<String>) -> Result<(), SyncError> {
        self.store
            .mail_tombstone_batch(&self.account_id, &jmap_ids)
            .await
            .map(|_| ())
            .map_err(sink_err)
    }

    async fn known_jmap_ids(&self) -> Result<HashSet<String>, SyncError> {
        self.store
            .mail_known_jmap_ids(&self.account_id)
            .await
            .map_err(sink_err)
    }

    async fn sync_mailboxes(&self, boxes: Vec<MailboxInfo>) -> Result<(), SyncError> {
        let rows: Vec<(String, String, Option<String>, i64)> = boxes
            .into_iter()
            .map(|b| (b.jmap_id, b.name, b.role, b.sort_order))
            .collect();
        self.store
            .mail_sync_mailboxes(&self.account_id, &rows)
            .await
            .map_err(sink_err)
    }
}
