//! Attachment byte pipeline (plan A6): drain `mail_attachments` rows whose
//! bytes are pending (blob_hash NULL, not skipped), fetch each blob through
//! the account's connected [`Syncer`], and store it content-addressed
//! (blake3 → blobs, dedup on hash).
//!
//! Classification is the whole trick:
//! - declared size > cap, or the server hands back more than cap → 'oversize'
//!   (permanent; the jmap blob id keeps the server as the byte source);
//! - HTTP 404 → 'missing' (permanent; the blob is gone server-side);
//! - anything else (network, auth, 5xx) → left pending for the next cycle —
//!   the pipeline runs every sync cycle, so it is self-healing by retry.

use anyhow::Result;
use hive_api::store::Store;
use jmap_sync::{SyncError, Syncer};

/// D8 default: 15 MiB per attachment.
const DEFAULT_MAX_ATTACHMENT_BYTES: u64 = 15_728_640;

/// Rows drained per cycle; a huge backlog spreads over successive cycles
/// instead of monopolizing the account task.
const FETCH_BATCH: i64 = 50;

pub(crate) async fn fetch_pending(
    store: &Store,
    syncer: &mut Syncer,
    account_id: &str,
) -> Result<()> {
    let cap = crate::env_u64(
        "HIVE_MAIL_MAX_ATTACHMENT_BYTES",
        DEFAULT_MAX_ATTACHMENT_BYTES,
    );
    let pending = store
        .mail_attachments_pending(account_id, FETCH_BATCH)
        .await?;
    if pending.is_empty() {
        return Ok(());
    }
    let (mut stored, mut skipped, mut deferred) = (0usize, 0usize, 0usize);
    for att in pending {
        // Declared-size precheck: don't download what we'd refuse anyway.
        if att.size > cap as i64 {
            store
                .mail_attachment_mark_skipped(&att.id, "oversize")
                .await?;
            skipped += 1;
            continue;
        }
        match syncer.fetch_blob(&att.jmap_blob_id, cap as usize).await {
            Ok(Some(bytes)) => {
                let hash = blake3::hash(&bytes).to_hex().to_string();
                store
                    .mail_attachment_store_blob(&att.id, &hash, &att.mime, &bytes)
                    .await?;
                stored += 1;
            }
            // The server declared one size and served another past the cap.
            Ok(None) => {
                store
                    .mail_attachment_mark_skipped(&att.id, "oversize")
                    .await?;
                skipped += 1;
            }
            Err(SyncError::NotFound(_)) => {
                store
                    .mail_attachment_mark_skipped(&att.id, "missing")
                    .await?;
                skipped += 1;
            }
            // Transient: stays pending; the next cycle retries.
            Err(e) => {
                tracing::warn!(account = %account_id, attachment = %att.id, error = %e, "attachment fetch deferred");
                deferred += 1;
            }
        }
    }
    tracing::debug!(account = %account_id, stored, skipped, deferred, "attachment pass");
    Ok(())
}
