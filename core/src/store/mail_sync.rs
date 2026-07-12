//! The mail sync DRIVER: the loop the pre-pivot `hive-mail` daemon ran
//! (`mail/src/{lib,sink,attachments}.rs`, removed in the Node/worker teardown),
//! reconstructed over the current append-only [`Store`]. jmap-sync's engine
//! (backfill / delta / reconcile / doorbell) is intact and unchanged; only this
//! driver — the thing that decrypts a secret, connects a [`Syncer`], and feeds
//! the store — was deleted with the worker. This is that thing.
//!
//! Shape difference from the old daemon: the daemon was a third long-lived
//! binary with one re-spawned task per account and an in-cycle EventSource
//! doorbell wait. Here the app spawns a periodic TICK ([`Store::mail_sync_tick`],
//! wired in `app` like the embed backfill) and each tick runs ONE bounded pass
//! per due account, then returns — so the doorbell wait is dropped (the next
//! tick is the poll). Everything else — the pass ordering, the at-least-once
//! sink/cursor contract, the attachment byte pipeline, the backoff — is the
//! daemon's, faithfully.
//!
//! Where the network work runs: [`Syncer`] calls (connect, list_mailboxes,
//! backfill/delta pages, blob fetches) are plain async and happen OFF the
//! store's single writer thread; only the store methods the sink/cursor call
//! (`mail_ingest_batch`, `mail_cursor_save`, …) cross onto the writer. The tick
//! itself is spawned off the UI thread by the app.
//!
//! Live JMAP round-trips can only be exercised against a real server, so this
//! module's tests cover the store-facing scheduling/cursor logic; the
//! connect→ingest path is validated on-server (see the Slice A report).

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use hive_shared::InboxReason;
use jmap_sync::{
    BackfillOutcome, BackfillState, CursorStore, MailSink, MailboxInfo, NormalizedMessage,
    SyncConfig, SyncCursor, SyncError, Syncer,
};

use super::mail::{MailAccountSync, MailIngestAttachment, MailIngestMessage};
use super::Store;

// Re-export the send-message types so consumers (the app's compose UI) build an
// outgoing message without depending on jmap-sync directly — the store owns the
// send surface (`mail_send_enqueue`).
pub use jmap_sync::{Address as MailAddress, OutgoingEmail};

/// Backfill pages consumed per account per tick; the cursor commits per page,
/// so hitting the budget just resumes on the next tick. Keeps one giant
/// mailbox from monopolizing a tick for minutes (the old daemon's PAGE_BUDGET).
const PAGE_BUDGET: u64 = 50;

/// D8 default: 15 MiB per attachment (old `attachments.rs`).
const DEFAULT_MAX_ATTACHMENT_BYTES: u64 = 15_728_640;

/// Attachment rows drained per pass; a huge backlog spreads over successive
/// ticks instead of monopolizing one.
const FETCH_BATCH: i64 = 50;

/// The outbox kind the mail driver owns. The generic worker drainer explicitly
/// does NOT claim this (its WORKER_OUTBOX_KINDS list excludes it), so sends wait
/// for this driver instead of being swallowed as no-op successes.
const SEND_KIND: &str = "mail.send";

/// Send jobs flushed per driver tick. Household volume is tiny; this just keeps
/// one giant backlog from monopolizing a tick.
const SEND_BATCH: i64 = 20;

fn mail_env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn sink_err(e: anyhow::Error) -> SyncError {
    SyncError::Sink(format!("{e:#}"))
}

// ── the jmap-sync trait impls over the store (ported from mail/src/sink.rs) ──

/// [`CursorStore`] over `mail_cursor_load` / `mail_cursor_save`.
struct StoreCursor {
    store: Store,
    account_id: String,
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

/// [`MailSink`] over `mail_ingest_batch` / `mail_tombstone_batch` /
/// `mail_known_jmap_ids` / `mail_sync_mailboxes`, with the post-commit wire +
/// inbox emission suppressed during backfill (D10 — a 100k-message import must
/// not refetch-storm).
struct StoreSink {
    store: Store,
    account_id: String,
    owner: String,
    ingest_ids: HashSet<String>,
    inbox_ids: HashSet<String>,
    suppress_events: bool,
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
                // ids only on the wire (globally readable, pruned); the inbox
                // row carries the subject (inbox_list is viewer-gated).
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

// ── the attachment byte pipeline (ported from mail/src/attachments.rs) ───────

/// Drain `mail_attachments` rows whose bytes are pending, fetch each blob
/// through the connected [`Syncer`], and store it content-addressed.
/// Classification: declared/served oversize → 'oversize' (permanent); HTTP 404
/// → 'missing' (permanent); anything else → left pending for the next tick
/// (self-healing by retry).
async fn fetch_pending(store: &Store, syncer: &mut Syncer, account_id: &str) -> Result<()> {
    let cap = mail_env_u64(
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
            // Transient: stays pending; the next tick retries.
            Err(e) => {
                tracing::warn!(account = %account_id, attachment = %att.id, error = %e, "attachment fetch deferred");
                deferred += 1;
            }
        }
    }
    tracing::debug!(account = %account_id, stored, skipped, deferred, "attachment pass");
    Ok(())
}

// ── the per-account pass (ported from mail/src/lib.rs::sync_account) ─────────

/// One sync pass for one due account. Pass ordering (the daemon's, minus the
/// trailing doorbell wait the tick replaces):
///   decrypt secret → build SyncConfig → connect → confirm/set jmap_account_id
///   → list_mailboxes + mail_sync_mailboxes → load cursor, stamp mailbox_state,
///   save → recompute ingest/inbox sets → budgeted backfill (ingest+tombstone
///   via the sink, cursor committed per page) → run_delta → fetch pending
///   attachment bytes.
/// The cursor is saved (inside run_backfill/run_delta, via jmap-sync's `commit`)
/// only after the sink call it corresponds to returned Ok (at-least-once).
async fn sync_account(store: &Store, acct: MailAccountSync) -> Result<()> {
    let cred_id = acct
        .cred_id
        .as_deref()
        .ok_or_else(|| anyhow!("account has no stored credential"))?;
    let secret = store
        .cc_cred_decrypt_by_id(cred_id)
        .await
        .context("credential vault")?
        .ok_or_else(|| anyhow!("credential row {cred_id} is gone"))?;

    let username = acct
        .jmap_username
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| acct.address.clone());
    let mut cfg = SyncConfig::new(&acct.jmap_url, username, secret);
    if !acct.jmap_account_id.is_empty() {
        cfg.account_id = Some(acct.jmap_account_id.clone());
    }
    cfg.max_body_bytes =
        mail_env_u64("HIVE_MAIL_MAX_BODY_BYTES", cfg.max_body_bytes as u64) as usize;
    // Mostly a test knob: forces a multi-page backfill out of a small mailbox.
    cfg.page_size = mail_env_u64("HIVE_MAIL_PAGE_SIZE", cfg.page_size as u64) as usize;

    let (ingest, _) = store.mail_mailbox_sets(&acct.id).await?;
    cfg.ingest_mailbox_ids = ingest.into_iter().collect();
    let page_sleep = Duration::from_millis(cfg.page_sleep_ms);

    let mut syncer = Syncer::connect(cfg).await?;
    if acct.jmap_account_id.is_empty() {
        store
            .mail_account_set_jmap_id(&acct.id, syncer.account_id())
            .await?;
    }

    // Mailboxes refresh every pass (cheap at household N); new rows arrive with
    // ingest=FALSE — opting in is operator intent via the accounts UI.
    let (boxes, mailbox_state) = syncer.list_mailboxes().await?;
    let rows: Vec<(String, String, Option<String>, i64)> = boxes
        .into_iter()
        .map(|b| (b.jmap_id, b.name, b.role, b.sort_order))
        .collect();
    store.mail_sync_mailboxes(&acct.id, &rows).await?;

    let cursor_store = StoreCursor {
        store: store.clone(),
        account_id: acct.id.clone(),
    };
    let mut cursor = cursor_store.load().await?;
    cursor.mailbox_state = Some(mailbox_state);
    cursor_store.save(&cursor).await?;

    // The ingest set may have just gained its first mailboxes.
    let (ingest_ids, inbox_ids) = store.mail_mailbox_sets(&acct.id).await?;
    let backfilling = cursor.backfill != BackfillState::Complete;
    let sink = StoreSink {
        store: store.clone(),
        account_id: acct.id.clone(),
        owner: acct.owner.clone(),
        ingest_ids,
        inbox_ids,
        // Suppression holds for this whole pass even when backfill completes
        // mid-pass: the first delta drain replays whatever changed during
        // backfill, and notifying on that replay would still storm.
        suppress_events: backfilling,
    };

    if backfilling {
        let mut pages = 0u64;
        loop {
            match syncer.run_backfill(&cursor_store, &sink).await? {
                BackfillOutcome::Complete => {
                    store
                        .emit(
                            "mail.backfill.completed",
                            &acct.owner,
                            serde_json::json!({"account_id": acct.id}),
                        )
                        .await?;
                    break;
                }
                BackfillOutcome::Page { fetched } => {
                    pages += 1;
                    tracing::debug!(account = %acct.id, pages, fetched, "backfill page");
                    if pages >= PAGE_BUDGET {
                        // Cursor is committed per page — resume next tick. Drain
                        // this budget's attachment bytes first so blobs trail
                        // the backfill instead of waiting for its end.
                        fetch_pending(store, &mut syncer, &acct.id).await?;
                        return Ok(());
                    }
                    tokio::time::sleep(page_sleep).await;
                }
            }
        }
    }

    let outcome = syncer.run_delta(&cursor_store, &sink).await?;
    if outcome.resynced {
        tracing::info!(account = %acct.id, created = outcome.created, destroyed = outcome.destroyed, "full reconciliation ran");
    }

    // Once per pass regardless of what the delta brought: the pending scan
    // re-selects anything a previous pass deferred (transient fetch errors,
    // fresh backfill leftovers) — self-healing by retry.
    fetch_pending(store, &mut syncer, &acct.id).await?;
    Ok(())
}

/// Top-level per-account supervision: success resets the backoff; failure
/// applies it, and the 8th consecutive failure disables the account and
/// notifies its owner (fail loud, never retry-forever silently). Ported from
/// the daemon's `account_task`.
async fn run_account_pass(store: &Store, acct: MailAccountSync) {
    let id = acct.id.clone();
    let owner = acct.owner.clone();
    let address = acct.address.clone();
    match sync_account(store, acct).await {
        Ok(()) => {
            if let Err(e) = store.mail_account_mark_ok(&id).await {
                tracing::error!(account = %id, error = %format!("{e:#}"), "mark_ok failed");
            }
        }
        Err(e) => {
            // The error is stored via mark_failed (clipped) and logged; it is
            // built from anyhow context, never from the secret.
            let error = format!("{e:#}");
            tracing::warn!(account = %id, %address, %error, "account sync failed");
            match store.mail_account_mark_failed(&id, &error).await {
                Ok(true) => {
                    tracing::error!(account = %id, %address, "disabled after repeated failures");
                    let _ = store
                        .inbox_add(
                            &owner,
                            "hive-mail",
                            InboxReason::Mail,
                            "mail_account",
                            &id,
                            None,
                            &format!(
                                "mail account {address} disabled after repeated sync failures"
                            ),
                        )
                        .await;
                    let _ = store
                        .emit(
                            "mail.account.disabled",
                            &owner,
                            serde_json::json!({"id": id}),
                        )
                        .await;
                }
                Ok(false) => {}
                Err(e2) => {
                    tracing::error!(account = %id, error = %format!("{e2:#}"), "mark_failed failed")
                }
            }
        }
    }
}

// ── the outbound send path (Slice C1): enqueue → flush via the outbox ────────
//
// A compose/reply hands the store an `OutgoingEmail`; `mail_send_enqueue`
// serializes it into a durable `mail.send` outbox job. The driver tick then
// flushes queued sends: connect the account (decrypt its vault credential),
// `send_email` (Email/set create + EmailSubmission/set submit in one request),
// then `outbox_complete` on success or `outbox_fail` (with the existing
// exponential backoff) on error. The network work is a plain async task off the
// writer; only the store transitions (`outbox_*`) cross onto the writer.

/// The serialized `mail.send` job body. `msg` carries the whole message; the
/// account id names which mailbox authenticates + sends it. Kept minimal and
/// versioned-by-shape (serde round-trips the struct).
#[derive(serde::Serialize, serde::Deserialize)]
struct SendJob {
    account_id: String,
    msg: OutgoingEmail,
}

/// The send seam. The driver flush loop is generic over this so the
/// claim→send→complete/fail transitions can be exercised offline (a mock
/// transport); the real transport does the JMAP round-trip, which only a live
/// server can validate.
#[async_trait]
trait SendTransport {
    /// Send one queued message, returning the server submission id. The error
    /// string is stored on the job (never a secret — it is built from the JMAP
    /// error, and the credential never appears in it).
    async fn send(&self, account_id: &str, msg: &OutgoingEmail) -> Result<String, String>;
}

/// The production transport: decrypt the account's credential, connect a
/// [`Syncer`], resolve the Drafts mailbox from local state when known, and
/// `send_email`.
struct JmapSendTransport<'a> {
    store: &'a Store,
}

#[async_trait]
impl SendTransport for JmapSendTransport<'_> {
    async fn send(&self, account_id: &str, msg: &OutgoingEmail) -> Result<String, String> {
        self.send_inner(account_id, msg)
            .await
            .map_err(|e| format!("{e:#}"))
    }
}

impl JmapSendTransport<'_> {
    async fn send_inner(&self, account_id: &str, msg: &OutgoingEmail) -> Result<String> {
        let acct = self
            .store
            .mail_account_sync_get(account_id)
            .await?
            .ok_or_else(|| anyhow!("mail account {account_id} is gone"))?;
        let cred_id = acct
            .cred_id
            .as_deref()
            .ok_or_else(|| anyhow!("account has no stored credential"))?;
        let secret = self
            .store
            .cc_cred_decrypt_by_id(cred_id)
            .await
            .context("credential vault")?
            .ok_or_else(|| anyhow!("credential row {cred_id} is gone"))?;

        let username = acct
            .jmap_username
            .clone()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| acct.address.clone());
        let mut cfg = SyncConfig::new(&acct.jmap_url, username, secret);
        if !acct.jmap_account_id.is_empty() {
            cfg.account_id = Some(acct.jmap_account_id.clone());
        }
        let mut syncer = Syncer::connect(cfg).await?;

        // Fill the Drafts mailbox from local state when we already synced it;
        // otherwise send_email resolves it from the server's mailbox list.
        let mut msg = msg.clone();
        if msg.drafts_mailbox_id.is_none() {
            msg.drafts_mailbox_id = self
                .store
                .mail_mailbox_jmap_by_role(account_id, "drafts")
                .await?;
        }
        let submission_id = syncer.send_email(&msg).await?;
        Ok(submission_id)
    }
}

impl Store {
    /// Queue a message to send. Serializes it into a durable `mail.send` outbox
    /// job (kind the mail driver owns); the next driver tick flushes it. Returns
    /// the outbox job id the UI tracks. NO fold change — the outbox is runtime.
    ///
    /// The message is validated shallowly here (a From, at least one recipient);
    /// the real rejection (bad address, quota, auth) surfaces on the flush and
    /// lands in the job's `last_error`.
    pub async fn mail_send_enqueue(&self, account_id: &str, msg: OutgoingEmail) -> Result<String> {
        if msg.from_address.trim().is_empty() {
            return Err(anyhow!("a From address is required"));
        }
        if msg.to.is_empty() && msg.cc.is_empty() && msg.bcc.is_empty() {
            return Err(anyhow!("at least one recipient is required"));
        }
        // Attribute the job to the account's owner (never a secret in the
        // payload — OutgoingEmail carries no credential).
        let owner = self
            .mail_account_owner(account_id)
            .await?
            .ok_or_else(|| anyhow!("mail account {account_id} not found"))?;
        let payload = serde_json::to_value(SendJob {
            account_id: account_id.to_string(),
            msg,
        })?;
        let job = self
            .outbox_enqueue(SEND_KIND, payload, None, &owner)
            .await?;
        Ok(job.id)
    }

    /// Outbox counts for the compose UI's pending/failed send indicator. A thin
    /// alias over `outbox_counts` (which is global; sends dominate it in
    /// practice, and the count is advisory).
    pub async fn mail_outbox_status(&self) -> Result<hive_shared::WorkerOutboxCounts> {
        self.outbox_counts().await
    }

    /// Flush queued `mail.send` jobs through the given transport: claim due
    /// jobs, send each, complete on success / fail-with-backoff on error.
    /// Generic over the transport so tests drive it without a network. Returns
    /// how many sends completed.
    async fn flush_send_jobs<T: SendTransport>(&self, transport: &T, limit: i64) -> Result<i64> {
        let mut done = 0;
        for job in self.outbox_claim(&[SEND_KIND], limit).await? {
            let parsed: Result<SendJob, _> = serde_json::from_value(job.payload.clone());
            let send: Result<String, String> = match &parsed {
                Ok(sj) => transport.send(&sj.account_id, &sj.msg).await,
                // A malformed payload can never succeed; fail it (backoff then
                // permanent) rather than looping on a parse error forever.
                Err(e) => Err(format!("unreadable mail.send payload: {e}")),
            };
            match send {
                Ok(submission_id) => {
                    tracing::info!(job = %job.id, submission = %submission_id, "mail sent");
                    self.outbox_complete(&job.id).await?;
                    done += 1;
                }
                Err(reason) => {
                    tracing::warn!(job = %job.id, attempt = job.attempts + 1, %reason, "mail send failed, will retry");
                    self.outbox_fail(&job.id, &reason, job.attempts + 1).await?;
                }
            }
        }
        Ok(done)
    }

    /// The driver's send flush with the production JMAP transport. Called once
    /// per tick after the account passes.
    async fn flush_sends(&self) -> Result<i64> {
        let transport = JmapSendTransport { store: self };
        self.flush_send_jobs(&transport, SEND_BATCH).await
    }

    /// ONE driver tick: every due account gets one bounded sync pass. The app
    /// spawns this on an interval (like the embed backfill). Sequential across
    /// accounts — household N is small, and the store's single writer serializes
    /// their writes regardless; the JMAP/network work is plain async off the
    /// writer. Returns how many accounts it ran (0 = nothing due).
    ///
    /// Never errors out of the loop: a per-account failure is captured by that
    /// account's backoff (`run_account_pass`), so one broken server can't wedge
    /// the others. The scan itself failing is logged and skipped this tick.
    ///
    /// Queued sends flush every tick regardless of whether any account was due
    /// for a poll — a send must not wait on the account's poll cadence. Send
    /// failures are captured by each job's own outbox backoff, so a flaky send
    /// can't wedge the poll either.
    pub async fn mail_sync_tick(&self) -> usize {
        if let Err(e) = self.flush_sends().await {
            tracing::warn!(error = %format!("{e:#}"), "mail send flush failed");
        }
        let due = match self.mail_accounts_due().await {
            Ok(due) => due,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "mail account scan failed");
                return 0;
            }
        };
        let n = due.len();
        for acct in due {
            run_account_pass(self, acct).await;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::keys::MemoryKeySource;
    use jmap_sync::Address;

    fn test_store() -> Store {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the tempdir so the data dir outlives the store's writer thread
        // for the duration of the test (mirrors the integration common helper).
        let path = dir.keep();
        Store::new(
            &path,
            Arc::new(MemoryKeySource([9u8; 32])),
            Arc::new(hive_embed::HashEmbedder),
        )
        .expect("open test store")
    }

    fn outgoing() -> OutgoingEmail {
        OutgoingEmail {
            from_address: "alice@example.test".into(),
            from_name: Some("Alice".into()),
            to: vec![Address {
                email: "bob@example.test".into(),
                name: None,
            }],
            cc: vec![],
            bcc: vec![],
            subject: "Re: bees".into(),
            body_text: "on it".into(),
            in_reply_to: Some("<prev@example.test>".into()),
            references: vec!["<prev@example.test>".into()],
            drafts_mailbox_id: None,
            identity_id: None,
        }
    }

    /// A transport that records the sends it is asked to make and returns a
    /// scripted Ok/Err, so the flush loop's transitions can be checked with no
    /// network.
    struct MockTransport {
        calls: Arc<Mutex<Vec<(String, OutgoingEmail)>>>,
        fail: bool,
        seq: AtomicUsize,
    }

    impl MockTransport {
        fn ok() -> Self {
            MockTransport {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail: false,
                seq: AtomicUsize::new(0),
            }
        }
        fn failing() -> Self {
            MockTransport {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail: true,
                seq: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl SendTransport for MockTransport {
        async fn send(&self, account_id: &str, msg: &OutgoingEmail) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((account_id.to_string(), msg.clone()));
            if self.fail {
                Err("server said no".into())
            } else {
                let n = self.seq.fetch_add(1, Ordering::SeqCst);
                Ok(format!("submission-{n}"))
            }
        }
    }

    async fn seed_account(store: &Store) {
        store
            .raw_sql(
                "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) \
                 VALUES ('acct-a', 'alice', 'alice@example.test', '2026-07-05T00:00:00.000Z', '2026-07-05T00:00:00.000Z')",
                vec![],
            )
            .await
            .expect("seed account");
    }

    /// The whole payload round-trips: the enqueued job is a claimable `mail.send`
    /// whose JSON deserializes back to the exact OutgoingEmail + account.
    #[tokio::test]
    async fn enqueue_writes_claimable_roundtripping_job() {
        let store = test_store();
        seed_account(&store).await;
        let msg = outgoing();

        let job_id = store
            .mail_send_enqueue("acct-a", msg.clone())
            .await
            .unwrap();
        assert!(job_id.starts_with("out"));

        let claimed = store.outbox_claim(&[SEND_KIND], 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, job_id);
        assert_eq!(claimed[0].kind, SEND_KIND);

        let decoded: SendJob = serde_json::from_value(claimed[0].payload.clone()).unwrap();
        assert_eq!(decoded.account_id, "acct-a");
        assert_eq!(decoded.msg, msg);

        // The generic worker drainer must NOT claim mail.send.
        assert_eq!(store.drain_outbox().await.unwrap(), 0);
        // Still pending after the foreign drainer ran.
        assert_eq!(store.outbox_claim(&[SEND_KIND], 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn enqueue_rejects_no_from_or_no_recipients() {
        let store = test_store();
        seed_account(&store).await;

        let mut no_from = outgoing();
        no_from.from_address = "  ".into();
        assert!(store.mail_send_enqueue("acct-a", no_from).await.is_err());

        let mut no_rcpt = outgoing();
        no_rcpt.to.clear();
        assert!(store.mail_send_enqueue("acct-a", no_rcpt).await.is_err());

        // Nothing landed in the outbox from the rejected enqueues.
        assert_eq!(store.outbox_claim(&[SEND_KIND], 10).await.unwrap().len(), 0);
    }

    /// A successful flush completes the job and calls the transport with the
    /// exact message.
    #[tokio::test]
    async fn flush_completes_on_success() {
        let store = test_store();
        seed_account(&store).await;
        store.mail_send_enqueue("acct-a", outgoing()).await.unwrap();

        let transport = MockTransport::ok();
        let done = store.flush_send_jobs(&transport, SEND_BATCH).await.unwrap();
        assert_eq!(done, 1);

        // The transport saw exactly one send for the right account. Copy out
        // and drop the guard before any await (no lock held across await).
        let calls = {
            let guard = transport.calls.lock().unwrap();
            guard.clone()
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "acct-a");
        assert_eq!(calls[0].1.subject, "Re: bees");

        // Job is done, no longer claimable, counts reflect it.
        assert_eq!(store.outbox_claim(&[SEND_KIND], 10).await.unwrap().len(), 0);
        let counts = store.mail_outbox_status().await.unwrap();
        assert_eq!(counts.done, 1);
        assert_eq!(counts.pending, 0);
    }

    /// A failed send stays queued with backoff: attempts incremented, run_after
    /// pushed into the future, still pending (not failed until 5 attempts).
    #[tokio::test]
    async fn flush_fails_with_backoff_and_stays_queued() {
        let store = test_store();
        seed_account(&store).await;
        store.mail_send_enqueue("acct-a", outgoing()).await.unwrap();

        let transport = MockTransport::failing();
        let done = store.flush_send_jobs(&transport, SEND_BATCH).await.unwrap();
        assert_eq!(done, 0);

        // Not immediately re-claimable (run_after is in the future now).
        assert_eq!(store.outbox_claim(&[SEND_KIND], 10).await.unwrap().len(), 0);

        // But the row is still pending with the error recorded and one attempt.
        let listed = store.outbox_list(10).await.unwrap();
        let job = listed.iter().find(|j| j.kind == SEND_KIND).unwrap();
        assert_eq!(job.attempts, 1);
        assert_eq!(job.status, hive_shared::OutboxStatus::Pending);
        assert_eq!(job.last_error.as_deref(), Some("server said no"));
        assert!(job.run_after.as_str() > "2026-07-05T00:00:00.000Z");

        let counts = store.mail_outbox_status().await.unwrap();
        assert_eq!(counts.pending, 1);
        assert_eq!(counts.failed, 0);
    }

    /// An unreadable payload can never send; it fails (with backoff) rather than
    /// looping on a parse error, and never touches the transport.
    #[tokio::test]
    async fn flush_fails_unreadable_payload() {
        let store = test_store();
        // Enqueue a bogus mail.send job directly (not via mail_send_enqueue).
        store
            .outbox_enqueue(SEND_KIND, serde_json::json!({"nope": true}), None, "alice")
            .await
            .unwrap();

        let transport = MockTransport::ok();
        let done = store.flush_send_jobs(&transport, SEND_BATCH).await.unwrap();
        assert_eq!(done, 0);
        assert!(transport.calls.lock().unwrap().is_empty());

        let listed = store.outbox_list(10).await.unwrap();
        let job = listed.iter().find(|j| j.kind == SEND_KIND).unwrap();
        assert_eq!(job.attempts, 1);
        assert!(job
            .last_error
            .as_deref()
            .unwrap()
            .contains("unreadable mail.send payload"));
    }
}
