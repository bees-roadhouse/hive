//! JMAP mailbox sync: session discovery, resumable newest-first backfill,
//! `Email/changes` delta drive with `cannotCalculateChanges` reconciliation,
//! an EventSource doorbell, and plaintext extraction.
//!
//! Everything flows through two traits the consumer implements —
//! [`CursorStore`] (sync-state persistence) and [`MailSink`] (message
//! storage). The crate has no Hive types and no database dependency; the mail
//! sync driver (a Phase 3 module) implements both traits over the hive store.
//! The at-least-once contract: a cursor
//! is saved only after the sink call it corresponds to returned `Ok`, so a
//! crash between the two replays the batch, and sinks must make replays no-ops
//! (unique-key upserts).
//!
//! The `jmap-client` dependency is contained in `client.rs` — no type from it
//! escapes that module, so a hand-rolled reqwest+serde replacement stays a
//! one-module rewrite.

mod backfill;
mod client;
mod delta;
mod doorbell;
mod extract;
pub mod quote;

use std::collections::HashSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use extract::normalize_iso_millis;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("http: {0}")]
    Http(String),
    #[error("auth rejected: {0}")]
    Auth(String),
    #[error("jmap protocol: {0}")]
    Protocol(String),
    /// The server invalidated our state string (Stalwart does this after
    /// upgrades/compactions). The caller must run [`Syncer::reconcile`].
    #[error("server cannot calculate changes from the stored state string")]
    CannotCalculateChanges,
    /// The backfill anchor message vanished mid-backfill; the driver retries
    /// with a `before:` filter automatically — this surfaces only if that
    /// fallback also fails.
    #[error("backfill anchor lost: {0}")]
    AnchorLost(String),
    /// HTTP 404 — the resource (a blob, typically) is permanently gone from
    /// the server. Callers must not retry; [`Syncer::fetch_blob`] consumers
    /// use this to mark attachments 'missing' instead of retrying forever.
    #[error("not found: {0}")]
    NotFound(String),
    #[error("sink: {0}")]
    Sink(String),
    #[error("cursor store: {0}")]
    Cursor(String),
    #[error("config: {0}")]
    Config(String),
}

/// One mailbox participant (From/To/Cc/Reply-To entry).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Address {
    pub email: String,
    pub name: Option<String>,
}

/// A message the user is sending: what [`Syncer::send_email`] turns into a JMAP
/// `Email/set` create + `EmailSubmission/set` submit. Plaintext body only this
/// slice. Serializable so it round-trips through the durable outbox payload.
///
/// `drafts_mailbox_id` is the JMAP id of the account's Drafts mailbox (the
/// created email must live somewhere before it is submitted); `None` means the
/// send path resolves it from the server's mailbox list. `identity_id` likewise
/// may be pre-resolved or left for the send path to discover by From address.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingEmail {
    pub from_address: String,
    pub from_name: Option<String>,
    pub to: Vec<Address>,
    #[serde(default)]
    pub cc: Vec<Address>,
    #[serde(default)]
    pub bcc: Vec<Address>,
    pub subject: String,
    pub body_text: String,
    /// RFC Message-ID of the message being replied to (goes into In-Reply-To).
    #[serde(default)]
    pub in_reply_to: Option<String>,
    /// The References chain for a reply (already includes the parent's id).
    #[serde(default)]
    pub references: Vec<String>,
    /// JMAP id of the Drafts mailbox; resolved from the server when `None`.
    #[serde(default)]
    pub drafts_mailbox_id: Option<String>,
    /// JMAP Identity id to submit as; resolved by From address when `None`.
    #[serde(default)]
    pub identity_id: Option<String>,
}

impl OutgoingEmail {
    /// The MAIL FROM / rcpt-to recipients: To + Cc + Bcc, the whole envelope.
    pub fn envelope_rcpts(&self) -> Vec<String> {
        self.to
            .iter()
            .chain(self.cc.iter())
            .chain(self.bcc.iter())
            .map(|a| a.email.clone())
            .collect()
    }
}

/// How `body_text` was produced. Raw HTML is never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySource {
    Plain,
    Html2text,
}

impl BodySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            BodySource::Plain => "plain",
            BodySource::Html2text => "html2text",
        }
    }
}

/// Attachment metadata. Bytes are fetched separately (and capped) via
/// [`Syncer::fetch_blob`]; the JMAP blob id is always retained so oversize
/// parts keep the server as their byte source of record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentMeta {
    pub jmap_blob_id: String,
    pub filename: String,
    pub mime: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub disposition: Option<String>,
}

/// A message normalized to plaintext with all timestamps in the exact
/// `%Y-%m-%dT%H:%M:%S%.3fZ` shape (lexicographic ordering holds).
#[derive(Debug, Clone)]
pub struct NormalizedMessage {
    pub jmap_id: String,
    pub thread_id: String,
    pub message_id_hdr: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from_addr: String,
    pub from_name: Option<String>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub subject: String,
    pub sent_at: Option<String>,
    pub received_at: String,
    pub mailbox_ids: Vec<String>,
    /// JMAP keywords that are set (e.g. `$seen`, `$junk`).
    pub keywords: Vec<String>,
    pub body_text: String,
    pub body_source: BodySource,
    pub snippet: String,
    pub size: u64,
    pub attachments: Vec<AttachmentMeta>,
    /// Set when normalization hit a per-message failure; the sink stores a
    /// stub row and the cursor advances (one poisoned message must never
    /// wedge the account's replay loop).
    pub parse_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MailboxInfo {
    pub jmap_id: String,
    pub name: String,
    /// Lowercase JMAP role (`inbox`, `junk`, `sent`, …) when the server
    /// assigns one.
    pub role: Option<String>,
    pub sort_order: i64,
}

/// Backfill progress. `InProgress` carries the committed resume anchor:
/// the (received_at, jmap_id) of the oldest message already persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum BackfillState {
    Pending,
    InProgress {
        received_at: String,
        jmap_id: String,
    },
    Complete,
}

/// Everything the syncer needs persisted per account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCursor {
    pub email_state: Option<String>,
    pub mailbox_state: Option<String>,
    pub backfill: BackfillState,
}

impl SyncCursor {
    pub fn fresh() -> Self {
        SyncCursor {
            email_state: None,
            mailbox_state: None,
            backfill: BackfillState::Pending,
        }
    }
}

/// Persistence for [`SyncCursor`], one instance per account.
#[async_trait]
pub trait CursorStore: Send + Sync {
    async fn load(&self) -> Result<SyncCursor, SyncError>;
    async fn save(&self, cursor: &SyncCursor) -> Result<(), SyncError>;
}

/// Message storage, one instance per account.
#[async_trait]
pub trait MailSink: Send + Sync {
    /// Idempotent on `jmap_id`; atomic per call; MUST be durable before
    /// returning `Ok` (the cursor is saved on the strength of it).
    async fn upsert_batch(&self, batch: Vec<NormalizedMessage>) -> Result<(), SyncError>;
    /// JMAP destroys. Implementations must drop retrieval rows (search,
    /// embeddings) in the same transaction — deleted mail must not stay
    /// searchable until a sweep.
    async fn tombstone(&self, jmap_ids: Vec<String>) -> Result<(), SyncError>;
    /// Every stored jmap_id including tombstoned rows — the reconciliation
    /// diff base. Reconcile never re-fetches ids in this set, which is also
    /// what keeps admin-redacted rows redacted.
    async fn known_jmap_ids(&self) -> Result<HashSet<String>, SyncError>;
    /// Upsert mailbox names/roles. Must never flip an existing row's ingest
    /// flag (that is operator intent, not server state).
    async fn sync_mailboxes(&self, boxes: Vec<MailboxInfo>) -> Result<(), SyncError>;
}

#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// JMAP base URL; session discovery appends `/.well-known/jmap`.
    pub jmap_url: String,
    pub username: String,
    pub secret: String,
    /// JMAP account id; discovered from the session's primary mail account
    /// when unset.
    pub account_id: Option<String>,
    /// The per-mailbox opt-in set (the spam gate). Empty means nothing
    /// backfills.
    pub ingest_mailbox_ids: Vec<String>,
    pub page_size: usize,
    pub max_body_bytes: usize,
    /// Politeness pause between backfill pages; the caller sleeps (the
    /// driver returns after each page so the cursor stays committed).
    pub page_sleep_ms: u64,
}

impl SyncConfig {
    pub fn new(
        jmap_url: impl Into<String>,
        username: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        SyncConfig {
            jmap_url: jmap_url.into(),
            username: username.into(),
            secret: secret.into(),
            account_id: None,
            ingest_mailbox_ids: Vec::new(),
            page_size: 200,
            max_body_bytes: 262_144,
            page_sleep_ms: 250,
        }
    }
}

#[derive(Debug)]
pub enum BackfillOutcome {
    /// One page persisted and the cursor committed; call again (after the
    /// politeness sleep) for the next page.
    Page {
        fetched: usize,
    },
    Complete,
}

#[derive(Debug, Default)]
pub struct DeltaOutcome {
    pub created: usize,
    pub updated: usize,
    pub destroyed: usize,
    /// True when the state string was invalid and a full reconciliation ran
    /// instead of an incremental drain.
    pub resynced: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DoorbellWake {
    /// The server signalled an Email state change.
    Change,
    Timeout,
    /// The EventSource stream is down; the caller polls on the timeout
    /// cadence and the next wait re-establishes the stream.
    Disconnected,
}

/// One connected JMAP account. Drivers live in `backfill.rs` / `delta.rs` /
/// `doorbell.rs`.
pub struct Syncer {
    raw: client::RawClient,
    cfg: SyncConfig,
    doorbell: Option<client::DoorbellStream>,
}

impl Syncer {
    pub async fn connect(cfg: SyncConfig) -> Result<Self, SyncError> {
        let raw = client::RawClient::connect(&cfg).await?;
        Ok(Syncer {
            raw,
            cfg,
            doorbell: None,
        })
    }

    /// The JMAP account id in use (discovered at connect when not configured).
    pub fn account_id(&self) -> &str {
        self.raw.account_id()
    }

    pub fn config(&self) -> &SyncConfig {
        &self.cfg
    }

    /// All mailboxes plus the Mailbox state string they correspond to.
    pub async fn list_mailboxes(&mut self) -> Result<(Vec<MailboxInfo>, String), SyncError> {
        self.raw.list_mailboxes().await
    }

    /// Download a blob, refusing anything over `cap` bytes (`Ok(None)`).
    pub async fn fetch_blob(
        &mut self,
        blob_id: &str,
        cap: usize,
    ) -> Result<Option<Vec<u8>>, SyncError> {
        let bytes = self.raw.download(blob_id).await?;
        if bytes.len() > cap {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    /// Create the message as a draft and submit it in ONE JMAP request:
    /// `Email/set` create (into the account's Drafts, keywords `$draft`+`$seen`)
    /// followed by `EmailSubmission/set` create that back-references the new
    /// email via `#creationId` and carries the SMTP envelope. Returns the server
    /// EmailSubmission id.
    ///
    /// Resolving what the message itself doesn't carry:
    ///   - Drafts mailbox: `msg.drafts_mailbox_id`, else the mailbox the server
    ///     tags with the `drafts` role;
    ///   - submission identity: `msg.identity_id`, else the Identity whose email
    ///     equals the From address (case-insensitive), else the first identity.
    ///
    /// Network work only — no store, no secret (the driver decrypts and connects
    /// before calling this).
    pub async fn send_email(&mut self, msg: &OutgoingEmail) -> Result<String, SyncError> {
        let drafts = match &msg.drafts_mailbox_id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => self.resolve_drafts_mailbox().await?,
        };
        let identity = match &msg.identity_id {
            Some(id) if !id.is_empty() => id.clone(),
            _ => self.raw.resolve_identity(&msg.from_address).await?,
        };
        self.raw.send_email(msg, &drafts, &identity).await
    }

    /// Find the Drafts mailbox jmap id from the server's mailbox list.
    async fn resolve_drafts_mailbox(&mut self) -> Result<String, SyncError> {
        let (boxes, _) = self.raw.list_mailboxes().await?;
        boxes
            .into_iter()
            .find(|b| b.role.as_deref() == Some("drafts"))
            .map(|b| b.jmap_id)
            .ok_or_else(|| {
                SyncError::Protocol("account has no Drafts mailbox to compose into".into())
            })
    }
}

/// What a commit persists before the cursor advances. A delta response can
/// carry both creations and destroys; they belong to one state transition, so
/// they commit as one unit.
#[derive(Default)]
pub(crate) struct Batch {
    pub upserts: Vec<NormalizedMessage>,
    pub tombstones: Vec<String>,
}

impl Batch {
    pub(crate) fn upserts(msgs: Vec<NormalizedMessage>) -> Self {
        Batch {
            upserts: msgs,
            tombstones: Vec::new(),
        }
    }
}

/// The single choke point that enforces the at-least-once contract: every
/// sink call must succeed before the cursor is saved. Every driver commits
/// through here.
pub(crate) async fn commit(
    sink: &dyn MailSink,
    cursor: &dyn CursorStore,
    batch: Batch,
    next: SyncCursor,
) -> Result<(), SyncError> {
    if !batch.upserts.is_empty() {
        sink.upsert_batch(batch.upserts).await?;
    }
    if !batch.tombstones.is_empty() {
        sink.tombstone(batch.tombstones).await?;
    }
    cursor.save(&next).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<&'static str>>>);

    struct MockSink {
        log: Log,
        fail: bool,
    }

    #[async_trait]
    impl MailSink for MockSink {
        async fn upsert_batch(&self, _batch: Vec<NormalizedMessage>) -> Result<(), SyncError> {
            self.log.0.lock().unwrap().push("sink");
            if self.fail {
                Err(SyncError::Sink("boom".into()))
            } else {
                Ok(())
            }
        }
        async fn tombstone(&self, _ids: Vec<String>) -> Result<(), SyncError> {
            self.log.0.lock().unwrap().push("tombstone");
            if self.fail {
                Err(SyncError::Sink("boom".into()))
            } else {
                Ok(())
            }
        }
        async fn known_jmap_ids(&self) -> Result<HashSet<String>, SyncError> {
            Ok(HashSet::new())
        }
        async fn sync_mailboxes(&self, _boxes: Vec<MailboxInfo>) -> Result<(), SyncError> {
            Ok(())
        }
    }

    struct MockCursor {
        log: Log,
    }

    #[async_trait]
    impl CursorStore for MockCursor {
        async fn load(&self) -> Result<SyncCursor, SyncError> {
            Ok(SyncCursor::fresh())
        }
        async fn save(&self, _cursor: &SyncCursor) -> Result<(), SyncError> {
            self.log.0.lock().unwrap().push("save");
            Ok(())
        }
    }

    fn msg(id: &str) -> NormalizedMessage {
        NormalizedMessage {
            jmap_id: id.into(),
            thread_id: "t1".into(),
            message_id_hdr: None,
            in_reply_to: None,
            references: vec![],
            from_addr: "a@example.test".into(),
            from_name: None,
            to: vec![],
            cc: vec![],
            reply_to: vec![],
            subject: "s".into(),
            sent_at: None,
            received_at: "2026-07-09T00:00:00.000Z".into(),
            mailbox_ids: vec![],
            keywords: vec![],
            body_text: "b".into(),
            body_source: BodySource::Plain,
            snippet: "b".into(),
            size: 1,
            attachments: vec![],
            parse_error: None,
        }
    }

    /// The at-least-once contract: cursor.save happens only after the sink
    /// returns Ok, and never when it fails.
    #[tokio::test]
    async fn cursor_saves_only_after_sink_ok() {
        let log = Log::default();
        let sink = MockSink {
            log: log.clone(),
            fail: false,
        };
        let cursor = MockCursor { log: log.clone() };
        commit(
            &sink,
            &cursor,
            Batch::upserts(vec![msg("m1")]),
            SyncCursor::fresh(),
        )
        .await
        .unwrap();
        assert_eq!(*log.0.lock().unwrap(), vec!["sink", "save"]);
    }

    /// Mixed batches (a delta response with creates AND destroys) must
    /// complete every sink call before the state string is saved.
    #[tokio::test]
    async fn mixed_batch_orders_sink_tombstone_save() {
        let log = Log::default();
        let sink = MockSink {
            log: log.clone(),
            fail: false,
        };
        let cursor = MockCursor { log: log.clone() };
        commit(
            &sink,
            &cursor,
            Batch {
                upserts: vec![msg("m1")],
                tombstones: vec!["m2".into()],
            },
            SyncCursor::fresh(),
        )
        .await
        .unwrap();
        assert_eq!(*log.0.lock().unwrap(), vec!["sink", "tombstone", "save"]);
    }

    #[tokio::test]
    async fn cursor_never_saves_after_sink_error() {
        let log = Log::default();
        let sink = MockSink {
            log: log.clone(),
            fail: true,
        };
        let cursor = MockCursor { log: log.clone() };
        let err = commit(
            &sink,
            &cursor,
            Batch::upserts(vec![msg("m1")]),
            SyncCursor::fresh(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SyncError::Sink(_)));
        assert_eq!(*log.0.lock().unwrap(), vec!["sink"]);
    }

    #[tokio::test]
    async fn empty_batches_still_advance_the_cursor() {
        // Delta drains with only state movement (e.g. flag changes already
        // applied) must persist the new state string.
        let log = Log::default();
        let sink = MockSink {
            log: log.clone(),
            fail: true, // would fail IF called — it must not be
        };
        let cursor = MockCursor { log: log.clone() };
        commit(&sink, &cursor, Batch::default(), SyncCursor::fresh())
            .await
            .unwrap();
        assert_eq!(*log.0.lock().unwrap(), vec!["save"]);
    }

    #[test]
    fn cursor_serde_roundtrip() {
        let c = SyncCursor {
            email_state: Some("s1".into()),
            mailbox_state: None,
            backfill: BackfillState::InProgress {
                received_at: "2026-07-09T00:00:00.000Z".into(),
                jmap_id: "m1".into(),
            },
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: SyncCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }
}
