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
    BackfillOutcome, BackfillState, CursorStore, EmailPatch, MailSink, MailboxInfo,
    NormalizedMessage, SyncConfig, SyncCursor, SyncError, Syncer,
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

/// The outbox kind the mail ACTION path owns (read/flag/label/move/archive/
/// delete write-back). Like `mail.send`, it is deliberately absent from the
/// generic worker's `WORKER_OUTBOX_KINDS`, so actions wait for this driver
/// instead of being swallowed as no-op successes. `pub(crate)` so the enqueue
/// helpers in `mail.rs` name the same kind.
pub(crate) const ACTION_KIND: &str = "mail.action";

/// Action jobs flushed per driver tick; actions are cheap single `Email/set`
/// calls, but this bounds a burst (e.g. flag-then-move-then-archive) per tick.
const ACTION_BATCH: i64 = 50;

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

// ── the message-action path (Slice C2): optimistic patch → flush via the outbox ─
//
// A reader action (mark read/unread, flag, edit labels, move, archive, delete)
// calls `mail_enqueue_action`, which in ONE writer tx both patches the derived
// `mail_messages` row (so the UI reflects it now) and serializes a durable
// `mail.action` outbox job. The driver tick then flushes queued actions via the
// SAME shape as sends: `connect_syncer` (decrypt the vault credential + connect),
// `patch_email` (one `Email/set`), then `outbox_complete`/`outbox_fail` (backoff).
// Network work stays a plain async task off the writer.

/// The change one `mail.action` job applies, server-side. Each variant reduces
/// to an [`EmailPatch`]; the store has already applied the matching optimistic
/// patch to the derived row. Serde-tagged so the payload is self-describing and
/// versioned by shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum MailAction {
    /// Toggle one keyword (`$seen` = read, `$flagged` = flag).
    SetKeyword { keyword: String, on: bool },
    /// Replace the whole mailbox-membership set (move/archive/trash = one id;
    /// label add/remove = the store-recomputed resulting set).
    SetMailboxes { ids: Vec<String> },
    /// Permanent delete (`Email/set destroy`).
    Destroy,
}

impl MailAction {
    /// The JMAP patch this action becomes. Mirrors the optimistic patch the
    /// store already applied to the derived row.
    fn to_patch(&self) -> EmailPatch {
        match self {
            MailAction::SetKeyword { keyword, on } => EmailPatch {
                keywords: vec![(keyword.clone(), *on)],
                ..Default::default()
            },
            MailAction::SetMailboxes { ids } => EmailPatch {
                mailbox_ids: Some(ids.clone()),
                ..Default::default()
            },
            MailAction::Destroy => EmailPatch {
                destroy: true,
                ..Default::default()
            },
        }
    }
}

/// The serialized `mail.action` job body: which account authenticates it, the
/// server message id it targets, and the change. No secret (the credential is
/// resolved at flush time from the account's vault entry).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ActionJob {
    pub account_id: String,
    pub jmap_id: String,
    pub action: MailAction,
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

/// Decrypt an account's vault credential and connect a [`Syncer`] for it — the
/// preamble every outbound JMAP write shares (C1 send + C2 actions). Looks up
/// the account's sync fields, decrypts the credential by id, builds the
/// `SyncConfig` (username falls back to the address; the JMAP account id is
/// pinned when known), and connects. The secret lives only inside the returned
/// connection — it is never logged and never lands in an error string.
async fn connect_syncer(store: &Store, account_id: &str) -> Result<Syncer> {
    let acct = store
        .mail_account_sync_get(account_id)
        .await?
        .ok_or_else(|| anyhow!("mail account {account_id} is gone"))?;
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
    Ok(Syncer::connect(cfg).await?)
}

impl JmapSendTransport<'_> {
    async fn send_inner(&self, account_id: &str, msg: &OutgoingEmail) -> Result<String> {
        let mut syncer = connect_syncer(self.store, account_id).await?;

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

/// The action seam, mirroring [`SendTransport`]: the flush loop is generic over
/// it so the claim→apply→complete/fail transitions run offline against a mock,
/// while the real transport does the JMAP `Email/set`, which only a live server
/// validates.
#[async_trait]
trait ActionTransport {
    /// Apply one queued action's patch to the server message. The error string
    /// is stored on the job (never a secret — it is the JMAP error text).
    async fn apply(
        &self,
        account_id: &str,
        jmap_id: &str,
        patch: &EmailPatch,
    ) -> Result<(), String>;
}

/// The production action transport: connect the account (via the shared
/// [`connect_syncer`]) and `patch_email`.
struct JmapActionTransport<'a> {
    store: &'a Store,
}

#[async_trait]
impl ActionTransport for JmapActionTransport<'_> {
    async fn apply(
        &self,
        account_id: &str,
        jmap_id: &str,
        patch: &EmailPatch,
    ) -> Result<(), String> {
        async {
            let mut syncer = connect_syncer(self.store, account_id).await?;
            syncer.patch_email(jmap_id, patch).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await
        .map_err(|e| format!("{e:#}"))
    }
}

impl Store {
    /// Run ONE full sync pass for this account right now — the "Sync now"
    /// button. Unlike the background tick, it returns the outcome to the caller
    /// so the UI shows success or the EXACT failure immediately (no waiting on
    /// the ~30s tick, no reading logs). It persists status exactly like the
    /// driver (`mark_ok` clears the backoff on success; `mark_failed` records
    /// the clipped `last_error`), so the row's status line and error update too.
    /// Never carries a secret in its error — the message is anyhow context.
    pub async fn mail_account_sync_now(&self, id: &str) -> Result<()> {
        let acct = self
            .mail_account_sync_get(id)
            .await?
            .ok_or_else(|| anyhow!("mail account not found"))?;
        // Log the same shape as the background driver so a manual "Sync now"
        // failure is diagnosable in the journal, not only in the UI row.
        let address = acct.address.clone();
        match sync_account(self, acct).await {
            Ok(()) => {
                tracing::info!(account = %id, %address, "sync-now ok");
                self.mail_account_mark_ok(id).await?;
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(account = %id, %address, error = %msg, "sync-now failed");
                // Persist the failure like the driver, then surface it verbatim.
                let _ = self.mail_account_mark_failed(id, &msg).await;
                Err(anyhow!("{msg}"))
            }
        }
    }

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

    /// Flush queued `mail.action` jobs through the given transport: claim due
    /// jobs, rebuild each [`EmailPatch`] from its `MailAction` payload, apply,
    /// complete on success / fail-with-backoff on error. Generic over the
    /// transport so tests drive it without a network. Returns how many completed.
    /// The derived row was already patched optimistically at enqueue time; this
    /// only reconciles the server (a later sync delta is authoritative).
    async fn flush_action_jobs<T: ActionTransport>(
        &self,
        transport: &T,
        limit: i64,
    ) -> Result<i64> {
        let mut done = 0;
        for job in self.outbox_claim(&[ACTION_KIND], limit).await? {
            let parsed: Result<ActionJob, _> = serde_json::from_value(job.payload.clone());
            let applied: Result<(), String> = match &parsed {
                Ok(aj) => {
                    transport
                        .apply(&aj.account_id, &aj.jmap_id, &aj.action.to_patch())
                        .await
                }
                // A malformed payload can never succeed; fail it (backoff then
                // permanent) rather than looping on a parse error forever.
                Err(e) => Err(format!("unreadable mail.action payload: {e}")),
            };
            match applied {
                Ok(()) => {
                    tracing::info!(job = %job.id, "mail action applied");
                    self.outbox_complete(&job.id).await?;
                    done += 1;
                }
                Err(reason) => {
                    tracing::warn!(job = %job.id, attempt = job.attempts + 1, %reason, "mail action failed, will retry");
                    self.outbox_fail(&job.id, &reason, job.attempts + 1).await?;
                }
            }
        }
        Ok(done)
    }

    /// The driver's action flush with the production JMAP transport. Called once
    /// per tick beside the send flush.
    async fn flush_actions(&self) -> Result<i64> {
        let transport = JmapActionTransport { store: self };
        self.flush_action_jobs(&transport, ACTION_BATCH).await
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
    /// Queued sends AND actions flush every tick regardless of whether any
    /// account was due for a poll — neither must wait on the account's poll
    /// cadence. Failures are captured by each job's own outbox backoff, so a
    /// flaky send/action can't wedge the poll either.
    pub async fn mail_sync_tick(&self) -> usize {
        if let Err(e) = self.flush_sends().await {
            tracing::warn!(error = %format!("{e:#}"), "mail send flush failed");
        }
        if let Err(e) = self.flush_actions().await {
            tracing::warn!(error = %format!("{e:#}"), "mail action flush failed");
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

    // ── message actions (Slice C2) ──────────────────────────────────────────

    /// Records the (account, jmap_id, patch) tuples it is asked to apply and
    /// returns a scripted Ok/Err, so the action flush loop's transitions run
    /// with no network.
    struct MockActionTransport {
        calls: Arc<Mutex<Vec<(String, String, EmailPatch)>>>,
        fail: bool,
    }

    impl MockActionTransport {
        fn ok() -> Self {
            MockActionTransport {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail: false,
            }
        }
        fn failing() -> Self {
            MockActionTransport {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail: true,
            }
        }
    }

    #[async_trait]
    impl ActionTransport for MockActionTransport {
        async fn apply(
            &self,
            account_id: &str,
            jmap_id: &str,
            patch: &EmailPatch,
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push((
                account_id.to_string(),
                jmap_id.to_string(),
                patch.clone(),
            ));
            if self.fail {
                Err("server said no".into())
            } else {
                Ok(())
            }
        }
    }

    /// Seed one account, its inbox + archive + trash mailboxes, and one message
    /// sitting in the inbox (unread, unflagged). Returns the mail row id.
    async fn seed_message(store: &Store) -> String {
        seed_account(store).await;
        for (id, jmap, role) in [
            ("mb-inbox", "jmbox-inbox", "inbox"),
            ("mb-arch", "jmbox-arch", "archive"),
            ("mb-trash", "jmbox-trash", "trash"),
        ] {
            store
                .raw_sql(
                    "INSERT INTO mail_mailboxes (id, account_id, jmap_id, name, role, ingest) \
                     VALUES (?, 'acct-a', ?, ?, ?, TRUE)",
                    vec![id.into(), jmap.into(), jmap.into(), role.into()],
                )
                .await
                .expect("seed mailbox");
        }
        store
            .raw_sql(
                "INSERT INTO mail_messages \
                 (id, account_id, user_scope, jmap_id, jmap_thread_id, mailbox_ids_json, \
                  keywords_json, received_at, created_at, updated_at) \
                 VALUES ('mail-1', 'acct-a', 'alice', 'jm-1', 'jt-1', '[\"jmbox-inbox\"]', \
                  '{}', '2026-07-05T00:00:00.000Z', '2026-07-05T00:00:00.000Z', '2026-07-05T00:00:00.000Z')",
                vec![],
            )
            .await
            .expect("seed message");
        "mail-1".to_string()
    }

    /// The derived row's (keywords_json, mailbox_ids_json, deleted?) as the UI
    /// re-reads it.
    async fn row_state(store: &Store, id: &str) -> (String, String, bool) {
        let rows = store
            .raw_sql(
                "SELECT keywords_json, mailbox_ids_json, deleted_at FROM mail_messages WHERE id = ?",
                vec![id.into()],
            )
            .await
            .unwrap();
        let r = &rows[0];
        (
            r[0].as_str().unwrap().to_string(),
            r[1].as_str().unwrap().to_string(),
            !r[2].is_null(),
        )
    }

    fn action_of(payload: &serde_json::Value) -> MailAction {
        let job: ActionJob = serde_json::from_value(payload.clone()).unwrap();
        job.action
    }

    /// Marking read optimistically sets `$seen` on the derived row AND enqueues a
    /// claimable, round-tripping `mail.action` the generic drainer ignores.
    #[tokio::test]
    async fn mark_read_patches_row_and_enqueues() {
        let store = test_store();
        let id = seed_message(&store).await;

        let job_id = store.mail_mark_read(&id, true).await.unwrap();
        assert!(job_id.starts_with("out"));

        // Optimistic patch is visible immediately.
        let (kw, mboxes, deleted) = row_state(&store, &id).await;
        assert!(kw.contains("$seen"));
        assert_eq!(mboxes, r#"["jmbox-inbox"]"#); // membership untouched
        assert!(!deleted);

        // A claimable mail.action with the right payload.
        let claimed = store.outbox_claim(&[ACTION_KIND], 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, job_id);
        let job: ActionJob = serde_json::from_value(claimed[0].payload.clone()).unwrap();
        assert_eq!(job.account_id, "acct-a");
        assert_eq!(job.jmap_id, "jm-1");
        assert_eq!(
            job.action,
            MailAction::SetKeyword {
                keyword: "$seen".into(),
                on: true
            }
        );

        // The generic worker drainer must NOT claim mail.action.
        assert_eq!(store.drain_outbox().await.unwrap(), 0);
        assert_eq!(
            store.outbox_claim(&[ACTION_KIND], 10).await.unwrap().len(),
            1
        );
    }

    /// Flag toggles `$flagged`; unflag drops it (idempotent double-apply).
    #[tokio::test]
    async fn flag_and_unflag_patch_keyword() {
        let store = test_store();
        let id = seed_message(&store).await;

        store.mail_set_flagged(&id, true).await.unwrap();
        assert!(row_state(&store, &id).await.0.contains("$flagged"));

        store.mail_set_flagged(&id, false).await.unwrap();
        assert!(!row_state(&store, &id).await.0.contains("$flagged"));

        // Two enqueued jobs, both SetKeyword $flagged.
        let claimed = store.outbox_claim(&[ACTION_KIND], 10).await.unwrap();
        assert_eq!(claimed.len(), 2);
        for job in &claimed {
            match action_of(&job.payload) {
                MailAction::SetKeyword { keyword, .. } => assert_eq!(keyword, "$flagged"),
                other => panic!("unexpected action {other:?}"),
            }
        }
    }

    /// Move replaces the whole membership set with the single target, and the
    /// payload carries exactly that set.
    #[tokio::test]
    async fn move_replaces_mailboxes() {
        let store = test_store();
        let id = seed_message(&store).await;

        store.mail_move(&id, "jmbox-arch").await.unwrap();
        assert_eq!(row_state(&store, &id).await.1, r#"["jmbox-arch"]"#);

        let claimed = store.outbox_claim(&[ACTION_KIND], 10).await.unwrap();
        assert_eq!(
            action_of(&claimed[0].payload),
            MailAction::SetMailboxes {
                ids: vec!["jmbox-arch".into()]
            }
        );
    }

    /// Label add/remove recompute the set and enqueue the FULL resulting set, so
    /// the server patch matches the optimistic row.
    #[tokio::test]
    async fn label_add_then_remove_recompute_set() {
        let store = test_store();
        let id = seed_message(&store).await;

        store.mail_add_label(&id, "jmbox-arch").await.unwrap();
        let (_, mboxes, _) = row_state(&store, &id).await;
        let ids: Vec<String> = serde_json::from_str(&mboxes).unwrap();
        assert_eq!(ids, vec!["jmbox-inbox", "jmbox-arch"]);
        let claimed = store.outbox_claim(&[ACTION_KIND], 10).await.unwrap();
        assert_eq!(
            action_of(&claimed[0].payload),
            MailAction::SetMailboxes {
                ids: vec!["jmbox-inbox".into(), "jmbox-arch".into()]
            }
        );
        store.outbox_complete(&claimed[0].id).await.unwrap();

        store.mail_remove_label(&id, "jmbox-arch").await.unwrap();
        assert_eq!(row_state(&store, &id).await.1, r#"["jmbox-inbox"]"#);
    }

    /// Archive resolves the role=archive mailbox and moves there.
    #[tokio::test]
    async fn archive_moves_to_archive_role() {
        let store = test_store();
        let id = seed_message(&store).await;
        store.mail_archive(&id).await.unwrap();
        assert_eq!(row_state(&store, &id).await.1, r#"["jmbox-arch"]"#);
    }

    /// Delete (Apple soft delete) moves to role=trash.
    #[tokio::test]
    async fn delete_moves_to_trash_role() {
        let store = test_store();
        let id = seed_message(&store).await;
        store.mail_delete(&id).await.unwrap();
        assert_eq!(row_state(&store, &id).await.1, r#"["jmbox-trash"]"#);
    }

    /// A missing role mailbox surfaces a clean error (no panic), and nothing is
    /// enqueued.
    #[tokio::test]
    async fn archive_without_archive_mailbox_errors_cleanly() {
        let store = test_store();
        seed_account(&store).await;
        // A message but NO archive mailbox for the account.
        store
            .raw_sql(
                "INSERT INTO mail_messages \
                 (id, account_id, user_scope, jmap_id, jmap_thread_id, mailbox_ids_json, \
                  keywords_json, received_at, created_at, updated_at) \
                 VALUES ('mail-x', 'acct-a', 'alice', 'jm-x', 'jt-x', '[]', '{}', \
                  '2026-07-05T00:00:00.000Z', '2026-07-05T00:00:00.000Z', '2026-07-05T00:00:00.000Z')",
                vec![],
            )
            .await
            .unwrap();
        let err = store.mail_archive("mail-x").await.unwrap_err();
        assert!(format!("{err:#}").contains("archive mailbox"));
        assert_eq!(
            store.outbox_claim(&[ACTION_KIND], 10).await.unwrap().len(),
            0
        );
    }

    /// Permanent delete tombstones the local row (drops it from the reader) AND
    /// enqueues a Destroy.
    #[tokio::test]
    async fn permanent_delete_tombstones_and_enqueues_destroy() {
        let store = test_store();
        let id = seed_message(&store).await;

        store.mail_delete_permanently(&id).await.unwrap();
        // Row is soft-deleted (gone from the reader's live per-mailbox list,
        // which filters deleted_at).
        assert!(row_state(&store, &id).await.2);
        assert!(store
            .mail_messages_by_mailbox("mb-inbox", 50)
            .await
            .unwrap()
            .is_empty());

        let claimed = store.outbox_claim(&[ACTION_KIND], 10).await.unwrap();
        assert_eq!(action_of(&claimed[0].payload), MailAction::Destroy);
    }

    /// Acting on a gone message errors cleanly and enqueues nothing.
    #[tokio::test]
    async fn action_on_missing_message_errors() {
        let store = test_store();
        seed_account(&store).await;
        assert!(store.mail_mark_read("nope", true).await.is_err());
        assert_eq!(
            store.outbox_claim(&[ACTION_KIND], 10).await.unwrap().len(),
            0
        );
    }

    /// A successful action flush completes the job and hands the transport the
    /// rebuilt patch.
    #[tokio::test]
    async fn action_flush_completes_on_success() {
        let store = test_store();
        let id = seed_message(&store).await;
        store.mail_mark_read(&id, true).await.unwrap();

        let transport = MockActionTransport::ok();
        let done = store
            .flush_action_jobs(&transport, ACTION_BATCH)
            .await
            .unwrap();
        assert_eq!(done, 1);

        let calls = {
            let g = transport.calls.lock().unwrap();
            g.clone()
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "acct-a");
        assert_eq!(calls[0].1, "jm-1");
        assert_eq!(calls[0].2.keywords, vec![("$seen".to_string(), true)]);
        assert!(calls[0].2.mailbox_ids.is_none());
        assert!(!calls[0].2.destroy);

        // Job done, no longer claimable.
        assert_eq!(
            store.outbox_claim(&[ACTION_KIND], 10).await.unwrap().len(),
            0
        );
    }

    /// A failed action flush stays queued with backoff (attempts++, run_after
    /// pushed out, still pending).
    #[tokio::test]
    async fn action_flush_fails_with_backoff_and_stays_queued() {
        let store = test_store();
        let id = seed_message(&store).await;
        store.mail_set_flagged(&id, true).await.unwrap();

        let transport = MockActionTransport::failing();
        let done = store
            .flush_action_jobs(&transport, ACTION_BATCH)
            .await
            .unwrap();
        assert_eq!(done, 0);
        // Not immediately re-claimable (run_after in the future).
        assert_eq!(
            store.outbox_claim(&[ACTION_KIND], 10).await.unwrap().len(),
            0
        );
        let listed = store.outbox_list(10).await.unwrap();
        let job = listed.iter().find(|j| j.kind == ACTION_KIND).unwrap();
        assert_eq!(job.attempts, 1);
        assert_eq!(job.status, hive_shared::OutboxStatus::Pending);
        assert_eq!(job.last_error.as_deref(), Some("server said no"));
    }

    /// An unreadable mail.action payload fails (with backoff), never touching the
    /// transport.
    #[tokio::test]
    async fn action_flush_fails_unreadable_payload() {
        let store = test_store();
        store
            .outbox_enqueue(
                ACTION_KIND,
                serde_json::json!({"nope": true}),
                None,
                "alice",
            )
            .await
            .unwrap();

        let transport = MockActionTransport::ok();
        let done = store
            .flush_action_jobs(&transport, ACTION_BATCH)
            .await
            .unwrap();
        assert_eq!(done, 0);
        assert!(transport.calls.lock().unwrap().is_empty());

        let listed = store.outbox_list(10).await.unwrap();
        let job = listed.iter().find(|j| j.kind == ACTION_KIND).unwrap();
        assert_eq!(job.attempts, 1);
        assert!(job
            .last_error
            .as_deref()
            .unwrap()
            .contains("unreadable mail.action payload"));
    }
}
