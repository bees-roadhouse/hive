//! Steady-state sync: `Email/changes` drains from the stored state string;
//! `cannotCalculateChanges` (Stalwart invalidates states after upgrades or
//! compactions) falls into a full reconciliation. An unimplemented resync
//! path would mean an account that silently stalls forever — this one is
//! exercised in CI against a real Stalwart container.

use std::collections::HashSet;

use crate::{
    commit, extract, Batch, CursorStore, DeltaOutcome, MailSink, SyncCursor, SyncError, Syncer,
};

impl Syncer {
    /// Drain all pending changes. Each `Email/changes` response commits as
    /// one unit (upserts + tombstones + its new state string), so a crash
    /// mid-drain replays only the uncommitted tail.
    pub async fn run_delta(
        &mut self,
        cursor_store: &dyn CursorStore,
        sink: &dyn MailSink,
    ) -> Result<DeltaOutcome, SyncError> {
        let mut cursor = cursor_store.load().await?;
        let mut state = match cursor.email_state.clone() {
            Some(s) => s,
            None => {
                // No state yet (fresh account whose backfill snapshot hasn't
                // run): capture one so the next call has a baseline.
                let s = self.raw.current_email_state().await?;
                cursor.email_state = Some(s);
                cursor_store.save(&cursor).await?;
                return Ok(DeltaOutcome::default());
            }
        };

        let mut out = DeltaOutcome::default();
        loop {
            let set = match self.raw.changes(&state, self.cfg.page_size).await {
                Ok(set) => set,
                Err(SyncError::CannotCalculateChanges) => {
                    let reconciled = self.reconcile(cursor_store, sink).await?;
                    out.created += reconciled.created;
                    out.destroyed += reconciled.destroyed;
                    out.resynced = true;
                    return Ok(out);
                }
                Err(e) => return Err(e),
            };

            let mut fetch: Vec<String> = Vec::with_capacity(set.created.len() + set.updated.len());
            let mut seen = HashSet::new();
            for id in set.created.iter().chain(set.updated.iter()) {
                if seen.insert(id.clone()) {
                    fetch.push(id.clone());
                }
            }
            let msgs = if fetch.is_empty() {
                Vec::new()
            } else {
                self.raw
                    .get_messages(&fetch, self.cfg.max_body_bytes)
                    .await?
                    .into_iter()
                    .map(extract::normalize)
                    .collect()
            };

            out.created += set.created.len();
            out.updated += set.updated.len();
            out.destroyed += set.destroyed.len();

            state = set.new_state.clone();
            cursor.email_state = Some(state.clone());
            commit(
                sink,
                cursor_store,
                Batch {
                    upserts: msgs,
                    tombstones: set.destroyed,
                },
                cursor.clone(),
            )
            .await?;

            if !set.has_more {
                return Ok(out);
            }
        }
    }

    /// Full reconciliation against the server, for when state strings die.
    ///
    /// Two id sets, both paged and ids-only (cheap):
    /// - the UNFILTERED account id set is the tombstone diff base — messages
    ///   we hold that the server no longer has anywhere are dead. Filtering
    ///   this set to ingest mailboxes would wrongly tombstone rows whose
    ///   message merely moved out of ingest (D6 keeps those).
    /// - the INGEST-FILTERED id set is the fetch base — ids we've never seen
    ///   get fetched and upserted. Ids in `known_jmap_ids` are never
    ///   re-fetched, which is also what keeps admin-redacted rows redacted.
    pub async fn reconcile(
        &mut self,
        cursor_store: &dyn CursorStore,
        sink: &dyn MailSink,
    ) -> Result<DeltaOutcome, SyncError> {
        // Capture the authoritative state BEFORE reading the id sets:
        // anything that changes during reconcile replays through the delta
        // loop afterwards, and replays are no-ops by unique key.
        let fresh_state = self.raw.current_email_state().await?;
        let known = sink.known_jmap_ids().await?;

        let server_all = self.collect_ids(None).await?;
        let to_fetch: Vec<String> = self
            .collect_ids(Some(&self.cfg.ingest_mailbox_ids.clone()))
            .await?
            .into_iter()
            .filter(|id| !known.contains(id))
            .collect();

        let mut out = DeltaOutcome {
            resynced: true,
            ..Default::default()
        };

        // Upserts stream in chunks without touching the cursor — a crash
        // mid-reconcile just reruns reconcile, and the unique key absorbs it.
        for chunk in to_fetch.chunks(self.cfg.page_size) {
            let msgs: Vec<_> = self
                .raw
                .get_messages(chunk, self.cfg.max_body_bytes)
                .await?
                .into_iter()
                .map(extract::normalize)
                .collect();
            out.created += msgs.len();
            if !msgs.is_empty() {
                sink.upsert_batch(msgs).await?;
            }
        }

        let dead: Vec<String> = known
            .iter()
            .filter(|id| !server_all.contains(*id))
            .cloned()
            .collect();
        out.destroyed = dead.len();

        let cursor = cursor_store.load().await?;
        let next = SyncCursor {
            email_state: Some(fresh_state),
            ..cursor
        };
        commit(
            sink,
            cursor_store,
            Batch {
                upserts: Vec::new(),
                tombstones: dead,
            },
            next,
        )
        .await?;
        Ok(out)
    }

    async fn collect_ids(
        &mut self,
        mailbox_ids: Option<&[String]>,
    ) -> Result<HashSet<String>, SyncError> {
        let mailbox_ids = match mailbox_ids {
            Some([]) => return Ok(HashSet::new()),
            other => other,
        };
        let mut ids = HashSet::new();
        let mut position = 0i64;
        loop {
            let page = self
                .raw
                .query_ids(mailbox_ids, None, None, position, self.cfg.page_size)
                .await?;
            let fetched = page.ids.len();
            position += fetched as i64;
            ids.extend(page.ids);
            if fetched < self.cfg.page_size {
                return Ok(ids);
            }
        }
    }
}
