//! Newest-first, resumable backfill. One page per call: the caller owns the
//! politeness sleep and can interleave delta/doorbell work between pages.

use std::collections::HashMap;

use chrono::DateTime;

use crate::client::QueryPage;
use crate::{
    commit, extract, BackfillOutcome, BackfillState, Batch, CursorStore, MailSink, SyncCursor,
    SyncError, Syncer,
};

impl Syncer {
    /// Fetch, persist, and commit ONE backfill page. Returns
    /// [`BackfillOutcome::Complete`] when the query runs dry (or nothing is
    /// opted into ingest).
    pub async fn run_backfill(
        &self,
        cursor_store: &dyn CursorStore,
        sink: &dyn MailSink,
    ) -> Result<BackfillOutcome, SyncError> {
        let mut cursor = cursor_store.load().await?;
        if cursor.backfill == BackfillState::Complete {
            return Ok(BackfillOutcome::Complete);
        }
        if self.cfg.ingest_mailbox_ids.is_empty() {
            // The spam gate: no mailbox opted in, nothing to backfill.
            let next = SyncCursor {
                backfill: BackfillState::Complete,
                ..cursor
            };
            commit(sink, cursor_store, Batch::default(), next).await?;
            return Ok(BackfillOutcome::Complete);
        }

        // Snapshot the Email state string BEFORE the first page: the delta
        // loop later replays anything that changed mid-backfill, and the
        // unique key makes those replays no-ops.
        if cursor.email_state.is_none() {
            cursor.email_state = Some(self.raw.current_email_state().await?);
            cursor_store.save(&cursor).await?;
        }

        let ingest = &self.cfg.ingest_mailbox_ids;
        let page = self.query_page(&cursor, ingest).await?;

        if page.ids.is_empty() {
            let next = SyncCursor {
                backfill: BackfillState::Complete,
                ..cursor
            };
            commit(sink, cursor_store, Batch::default(), next).await?;
            return Ok(BackfillOutcome::Complete);
        }

        let raws = self
            .raw
            .get_messages(&page.ids, self.cfg.max_body_bytes)
            .await?;
        // Preserve the query's newest-first order; Email/get output order is
        // not guaranteed. Messages destroyed between query and get simply
        // drop out.
        let mut by_id: HashMap<String, crate::client::RawMessage> = raws
            .into_iter()
            .map(|raw| (raw.jmap_id.clone(), raw))
            .collect();
        let msgs: Vec<_> = page
            .ids
            .iter()
            .filter_map(|id| by_id.remove(id))
            .map(extract::normalize)
            .collect();
        let fetched = msgs.len();

        // The resume anchor: the page's LAST id (anchor+offset resumes after
        // it even if we failed to fetch it), with the oldest fetched
        // received_at as the `before:` fallback bound. If the whole page
        // vanished between query and get, carry the previous timestamp
        // forward.
        let anchor_id = page.ids.last().cloned().unwrap_or_default();
        let anchor_ts = msgs
            .last()
            .map(|m| m.received_at.clone())
            .or(match &cursor.backfill {
                BackfillState::InProgress { received_at, .. } => Some(received_at.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "9999-12-31T23:59:59.999Z".to_string());

        let next = SyncCursor {
            backfill: BackfillState::InProgress {
                received_at: anchor_ts,
                jmap_id: anchor_id,
            },
            ..cursor
        };
        commit(sink, cursor_store, Batch::upserts(msgs), next).await?;
        Ok(BackfillOutcome::Page { fetched })
    }

    /// Query the next page: anchor-resume when a cursor exists, falling back
    /// to a `before:` filter when the anchor message no longer exists.
    async fn query_page(
        &self,
        cursor: &SyncCursor,
        ingest: &[String],
    ) -> Result<QueryPage, SyncError> {
        match &cursor.backfill {
            BackfillState::InProgress {
                jmap_id,
                received_at,
            } => {
                match self
                    .raw
                    .query_ids(Some(ingest), None, Some(jmap_id), 0, self.cfg.page_size)
                    .await
                {
                    Ok(page) => Ok(page),
                    Err(SyncError::AnchorLost(_)) => {
                        let epoch = DateTime::parse_from_rfc3339(received_at)
                            .map_err(|e| {
                                SyncError::Cursor(format!(
                                    "backfill cursor timestamp {received_at:?} unparseable: {e}"
                                ))
                            })?
                            .timestamp();
                        self.raw
                            .query_ids(Some(ingest), Some(epoch), None, 0, self.cfg.page_size)
                            .await
                    }
                    Err(e) => Err(e),
                }
            }
            _ => {
                self.raw
                    .query_ids(Some(ingest), None, None, 0, self.cfg.page_size)
                    .await
            }
        }
    }
}
