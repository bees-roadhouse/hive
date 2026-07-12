//! The ONLY module that may `use jmap_client`. Everything it returns is a
//! crate-local type, so replacing the dependency (dormant 2024-01..2025-09)
//! with a hand-rolled reqwest+serde client is a rewrite of this file alone.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use jmap_client::client::{Client, Credentials};
use jmap_client::core::error::MethodErrorType;
use jmap_client::core::query::{Filter as CoreFilter, QueryResponse};
use jmap_client::core::response::{
    EmailGetResponse, IdentityGetResponse, MailboxGetResponse, MethodResponse, TaggedMethodResponse,
};
use jmap_client::core::set::SetObject;
use jmap_client::email::{self, Email, EmailBodyPart, Property};
use jmap_client::email_submission::Address as SubmissionAddress;
use jmap_client::mailbox::{self, Role};
use jmap_client::{DataType, Error as JmapError};

use crate::{AttachmentMeta, MailboxInfo, OutgoingEmail, SyncConfig, SyncError};

pub(crate) type DoorbellStream = Pin<Box<dyn Stream<Item = Result<(), SyncError>> + Send>>;

/// A message as the server handed it to us, before plaintext extraction —
/// plain types only, so `extract.rs` stays free of jmap-client.
pub(crate) struct RawMessage {
    pub jmap_id: String,
    pub thread_id: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from: Vec<RawAddr>,
    pub to: Vec<RawAddr>,
    pub cc: Vec<RawAddr>,
    pub reply_to: Vec<RawAddr>,
    pub subject: Option<String>,
    pub received_at_epoch: Option<i64>,
    pub sent_at_epoch: Option<i64>,
    pub mailbox_ids: Vec<String>,
    pub keywords: Vec<String>,
    pub preview: Option<String>,
    pub size: u64,
    /// Resolved bodyValues for textBody parts, in order.
    pub text_parts: Vec<String>,
    /// Resolved bodyValues for htmlBody parts, in order.
    pub html_parts: Vec<String>,
    /// Any consulted body value was truncated at max_body_value_bytes.
    pub truncated: bool,
    pub attachments: Vec<AttachmentMeta>,
}

pub(crate) struct RawAddr {
    pub email: String,
    pub name: Option<String>,
}

pub(crate) struct QueryPage {
    pub ids: Vec<String>,
}

pub(crate) struct ChangeSet {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub destroyed: Vec<String>,
    pub new_state: String,
    pub has_more: bool,
}

fn map_err(e: JmapError) -> SyncError {
    match e {
        JmapError::Method(m) => match m.error() {
            MethodErrorType::CannotCalculateChanges => SyncError::CannotCalculateChanges,
            MethodErrorType::AnchorNotFound => SyncError::AnchorLost(m.to_string()),
            _ => SyncError::Protocol(m.to_string()),
        },
        JmapError::Transport(t) => SyncError::Http(t.to_string()),
        JmapError::Problem(p) => {
            // 401/403 problem responses are how bad credentials surface;
            // 404 is a permanently-gone resource (blob downloads).
            let text = format!("{p:?}");
            if p.status() == Some(404) {
                SyncError::NotFound(text)
            } else if text.contains("401") || text.to_lowercase().contains("unauthorized") {
                SyncError::Auth(text)
            } else {
                SyncError::Protocol(text)
            }
        }
        // Non-problem+json error statuses arrive as Server("404 Not Found").
        JmapError::Server(s) if s.starts_with("404") => SyncError::NotFound(s),
        other => SyncError::Protocol(format!("{other:?}")),
    }
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

const MAIL_CAPABILITY: &str = "urn:ietf:params:jmap:mail";
/// Stalwart's maxObjectsInGet default is far higher, but 50 keeps individual
/// responses (with 256KB body values) at a polite size.
const GET_CHUNK: usize = 50;

pub(crate) struct RawClient {
    inner: Client,
}

impl RawClient {
    pub(crate) async fn connect(cfg: &SyncConfig) -> Result<Self, SyncError> {
        if cfg.jmap_url.is_empty() || cfg.username.is_empty() {
            return Err(SyncError::Config(
                "jmap_url and username are required".into(),
            ));
        }
        let base = cfg.jmap_url.trim_end_matches('/');
        let mut inner = Client::new()
            .credentials(Credentials::basic(&cfg.username, &cfg.secret))
            // Session discovery redirects (.well-known -> api host) must stay
            // on the host the operator configured.
            .follow_redirects([host_of(base)])
            .connect(base)
            .await
            .map_err(map_err)?;

        let account_id = match &cfg.account_id {
            Some(id) => id.clone(),
            None => {
                let session = inner.session();
                let found = session
                    .primary_accounts()
                    .find(|(cap, _)| cap.as_str() == MAIL_CAPABILITY)
                    .map(|(_, id)| id.clone())
                    .or_else(|| session.accounts().next().cloned());
                found.ok_or_else(|| SyncError::Protocol("session exposes no accounts".into()))?
            }
        };
        inner.set_default_account_id(account_id);
        Ok(RawClient { inner })
    }

    pub(crate) fn account_id(&self) -> &str {
        self.inner.default_account_id()
    }

    pub(crate) async fn list_mailboxes(&self) -> Result<(Vec<MailboxInfo>, String), SyncError> {
        let mut request = self.inner.build();
        request.get_mailbox().properties([
            mailbox::Property::Id,
            mailbox::Property::Name,
            mailbox::Property::Role,
            mailbox::Property::SortOrder,
        ]);
        let mut resp = request
            .send_single::<MailboxGetResponse>()
            .await
            .map_err(map_err)?;
        let state = resp.take_state();
        let boxes = resp
            .take_list()
            .into_iter()
            .filter_map(|mut m| {
                let jmap_id = m.take_id();
                if jmap_id.is_empty() {
                    return None;
                }
                Some(MailboxInfo {
                    name: m.name().unwrap_or(&jmap_id).to_string(),
                    role: role_str(m.role()),
                    sort_order: m.sort_order() as i64,
                    jmap_id,
                })
            })
            .collect();
        Ok((boxes, state))
    }

    /// One page of ids, newest-first, optionally scoped to mailboxes and/or
    /// bounded by `received_before` (epoch seconds), starting at `position`.
    pub(crate) async fn query_ids(
        &self,
        mailbox_ids: Option<&[String]>,
        received_before: Option<i64>,
        anchor: Option<&str>,
        position: i64,
        limit: usize,
    ) -> Result<QueryPage, SyncError> {
        let mut request = self.inner.build();
        {
            let q = request.query_email();
            if let Some(filter) = build_filter(mailbox_ids, received_before) {
                q.filter(filter);
            }
            q.sort([email::query::Comparator::received_at().descending()]);
            q.limit(limit);
            match anchor {
                Some(a) => {
                    q.anchor(a);
                    q.anchor_offset(1);
                }
                None => {
                    q.position(position as i32);
                }
            }
        }
        let mut resp = request
            .send_single::<QueryResponse>()
            .await
            .map_err(map_err)?;
        Ok(QueryPage {
            ids: resp.take_ids(),
        })
    }

    pub(crate) async fn changes(
        &self,
        since_state: &str,
        max: usize,
    ) -> Result<ChangeSet, SyncError> {
        let mut resp = self
            .inner
            .email_changes(since_state, Some(max))
            .await
            .map_err(|e| match e {
                // Stalwart (verified against v0.15.5 in the CI e2e) rejects a
                // state string its tokenizer can't even parse at the REQUEST
                // layer — HTTP 400 urn:ietf:params:jmap:error:notRequest —
                // rather than answering a method-level cannotCalculateChanges.
                // The state string is the only caller-variable part of this
                // request, so a parse reject means the stored state is garbage
                // (e.g. the deliberate 'force-resync' sentinel), and the
                // recovery is the same: full reconciliation. Without this
                // mapping a poisoned state would loop through backoff and
                // disable the account instead of resyncing.
                JmapError::Problem(ref p)
                    if matches!(
                        p.error(),
                        jmap_client::core::error::ProblemType::JMAP(
                            jmap_client::core::error::JMAPError::NotRequest
                        )
                    ) =>
                {
                    SyncError::CannotCalculateChanges
                }
                other => map_err(other),
            })?;
        Ok(ChangeSet {
            created: resp.take_created(),
            updated: resp.take_updated(),
            destroyed: resp.take_destroyed(),
            has_more: resp.has_more_changes(),
            new_state: resp.take_new_state(),
        })
    }

    /// The current Email state string, captured before backfill starts so the
    /// delta loop can replay anything that changes mid-backfill.
    pub(crate) async fn current_email_state(&self) -> Result<String, SyncError> {
        // A minimal Email/get (no ids) returns the account's state string.
        let mut request = self.inner.build();
        request.get_email().ids(Vec::<String>::new());
        let mut resp = request
            .send_single::<EmailGetResponse>()
            .await
            .map_err(map_err)?;
        Ok(resp.take_state())
    }

    /// Full messages with body values, chunked politely.
    pub(crate) async fn get_messages(
        &self,
        ids: &[String],
        max_body_bytes: usize,
    ) -> Result<Vec<RawMessage>, SyncError> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(GET_CHUNK) {
            let mut request = self.inner.build();
            {
                let get = request.get_email().ids(chunk.iter().map(|s| s.as_str()));
                get.properties([
                    Property::Id,
                    Property::ThreadId,
                    Property::MailboxIds,
                    Property::Keywords,
                    Property::Size,
                    Property::ReceivedAt,
                    Property::MessageId,
                    Property::InReplyTo,
                    Property::References,
                    Property::From,
                    Property::To,
                    Property::Cc,
                    Property::ReplyTo,
                    Property::Subject,
                    Property::SentAt,
                    Property::HasAttachment,
                    Property::Preview,
                    Property::BodyValues,
                    Property::TextBody,
                    Property::HtmlBody,
                    Property::Attachments,
                ]);
                let args = get.arguments();
                args.body_properties([
                    email::BodyProperty::PartId,
                    email::BodyProperty::BlobId,
                    email::BodyProperty::Size,
                    email::BodyProperty::Name,
                    email::BodyProperty::Type,
                    email::BodyProperty::Charset,
                    email::BodyProperty::Disposition,
                    email::BodyProperty::Cid,
                ]);
                args.fetch_text_body_values(true);
                args.fetch_html_body_values(true);
                args.max_body_value_bytes(max_body_bytes);
            }
            let mut resp = request
                .send_single::<EmailGetResponse>()
                .await
                .map_err(map_err)?;
            for email in resp.take_list() {
                out.push(raw_message(email));
            }
        }
        Ok(out)
    }

    pub(crate) async fn download(&self, blob_id: &str) -> Result<Vec<u8>, SyncError> {
        self.inner.download(blob_id).await.map_err(map_err)
    }

    /// Pick the submission Identity for a From address: exact email match
    /// (case-insensitive), else the first identity the account exposes. Errors
    /// only when the account has no identities at all.
    pub(crate) async fn resolve_identity(&self, from_address: &str) -> Result<String, SyncError> {
        // No `.ids()` → the server returns every identity on the account.
        let mut request = self.inner.build();
        request.get_identity();
        let mut resp = request
            .send_single::<IdentityGetResponse>()
            .await
            .map_err(map_err)?;
        let identities = resp.take_list();
        let want = from_address.to_ascii_lowercase();
        let chosen = identities
            .iter()
            .find(|i| {
                i.email()
                    .map(|e| e.eq_ignore_ascii_case(&want))
                    .unwrap_or(false)
            })
            .or_else(|| identities.first())
            .and_then(|i| i.id().map(|s| s.to_string()));
        chosen.ok_or_else(|| {
            SyncError::Protocol(format!(
                "no JMAP identity is configured to send as {from_address}"
            ))
        })
    }

    /// The two-method send: `Email/set` create then `EmailSubmission/set` create
    /// referencing it by `#creationId`, in one request. Returns the submission
    /// id. Surfaces `notCreated` set errors on either method (bad recipient,
    /// over quota, forbidden-from, …) as [`SyncError::Protocol`].
    pub(crate) async fn send_email(
        &self,
        msg: &OutgoingEmail,
        drafts_mailbox_id: &str,
        identity_id: &str,
    ) -> Result<String, SyncError> {
        // Body part id links the bodyValue to the textBody structure.
        const BODY_PART: &str = "text";

        let mut request = self.inner.build();

        // ── method s0: Email/set create (the draft) ──────────────────────────
        let email_create_id = {
            let email = request.set_email().create();
            email
                .mailbox_ids([drafts_mailbox_id])
                .keywords(["$draft", "$seen"])
                .from([email_address(&msg.from_address, msg.from_name.as_deref())])
                .to(msg.to.iter().map(email_address_of))
                .subject(msg.subject.clone())
                .body_value(BODY_PART.to_string(), msg.body_text.clone())
                .text_body(
                    EmailBodyPart::new()
                        .part_id(BODY_PART)
                        .content_type("text/plain"),
                );
            if !msg.cc.is_empty() {
                email.cc(msg.cc.iter().map(email_address_of));
            }
            if !msg.bcc.is_empty() {
                email.bcc(msg.bcc.iter().map(email_address_of));
            }
            if let Some(irt) = msg.in_reply_to.as_ref().filter(|s| !s.is_empty()) {
                email.in_reply_to([irt.clone()]);
            }
            if !msg.references.is_empty() {
                email.references(msg.references.clone());
            }
            email
                .create_id()
                .ok_or_else(|| SyncError::Protocol("email create id missing".into()))?
        };

        // ── method s1: EmailSubmission/set create, back-referencing #c0 ──────
        let submission_create_id = {
            let sub = request.set_email_submission().create();
            sub.email_id(format!("#{email_create_id}"))
                .identity_id(identity_id)
                .envelope(
                    SubmissionAddress::new(msg.from_address.clone()),
                    msg.envelope_rcpts().into_iter().map(SubmissionAddress::new),
                );
            sub.create_id()
                .ok_or_else(|| SyncError::Protocol("submission create id missing".into()))?
        };

        let response: jmap_client::core::response::Response<TaggedMethodResponse> =
            self.inner.send(&request).await.map_err(map_err)?;
        // Responses come back tagged by call id (s0, s1); pull each by position.
        let mut set_email = None;
        let mut set_submission = None;
        for tagged in response.unwrap_method_responses() {
            match tagged.unwrap_method_response() {
                MethodResponse::SetEmail(r) => set_email = Some(r),
                MethodResponse::SetEmailSubmission(r) => set_submission = Some(r),
                MethodResponse::Error(e) => return Err(map_err(e.into())),
                _ => {}
            }
        }

        // The email must have been created for the submission to reference it.
        let mut set_email =
            set_email.ok_or_else(|| SyncError::Protocol("no Email/set response".into()))?;
        set_email
            .created(&email_create_id)
            .map_err(|e| SyncError::Protocol(format!("draft not created: {e}")))?;

        let mut set_submission = set_submission
            .ok_or_else(|| SyncError::Protocol("no EmailSubmission/set response".into()))?;
        let submission = set_submission
            .created(&submission_create_id)
            .map_err(|e| SyncError::Protocol(format!("send rejected: {e}")))?;
        submission
            .id()
            .map(|s| s.to_string())
            .ok_or_else(|| SyncError::Protocol("submission created without an id".into()))
    }

    /// An SSE doorbell: yields `()` whenever the server reports an Email
    /// state change. Errors/termination surface once; the caller drops the
    /// stream and re-establishes on the next wait.
    pub(crate) async fn doorbell(&self) -> Result<DoorbellStream, SyncError> {
        let stream = self
            .inner
            .event_source(Some([DataType::Email]), false, Some(30), None)
            .await
            .map_err(map_err)?;
        Ok(Box::pin(stream.filter_map(|item| async move {
            match item {
                // Any Email change notification is a wake; contents don't
                // matter (state strings are the correctness mechanism).
                Ok(_) => Some(Ok(())),
                Err(e) => Some(Err(map_err(e))),
            }
        })))
    }
}

/// A JMAP `EmailAddress` (the `Get`-state value the `Email<Set>` header setters
/// accept) built from an email + optional display name. An empty/absent name
/// serializes as no `name`, so the server emits a bare `<addr>`.
fn email_address(email: &str, name: Option<&str>) -> email::EmailAddress {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => (n.to_string(), email.to_string()).into(),
        None => email.to_string().into(),
    }
}

fn email_address_of(addr: &crate::Address) -> email::EmailAddress {
    email_address(&addr.email, addr.name.as_deref())
}

fn build_filter(
    mailbox_ids: Option<&[String]>,
    received_before: Option<i64>,
) -> Option<CoreFilter<email::query::Filter>> {
    let mailbox_or = mailbox_ids.filter(|ids| !ids.is_empty()).map(|ids| {
        CoreFilter::or(
            ids.iter()
                .map(|id| email::query::Filter::in_mailbox(id.as_str())),
        )
    });
    match (mailbox_or, received_before) {
        (Some(m), Some(b)) => Some(CoreFilter::and([m, email::query::Filter::before(b).into()])),
        (Some(m), None) => Some(m),
        (None, Some(b)) => Some(email::query::Filter::before(b).into()),
        (None, None) => None,
    }
}

fn role_str(role: Role) -> Option<String> {
    match role {
        Role::Archive => Some("archive".into()),
        Role::Drafts => Some("drafts".into()),
        Role::Important => Some("important".into()),
        Role::Inbox => Some("inbox".into()),
        Role::Junk => Some("junk".into()),
        Role::Sent => Some("sent".into()),
        Role::Trash => Some("trash".into()),
        Role::Other(s) => Some(s.to_lowercase()),
        Role::None => None,
    }
}

fn addrs(list: Option<&[email::EmailAddress]>) -> Vec<RawAddr> {
    list.map(|l| {
        l.iter()
            .map(|a| RawAddr {
                email: a.email().to_string(),
                name: a.name().map(|n| n.to_string()),
            })
            .collect()
    })
    .unwrap_or_default()
}

fn raw_message(email: Email) -> RawMessage {
    // Resolve text/html part ids against the bodyValues map. Parts without a
    // resolvable value (e.g. attachments listed in textBody) are skipped.
    let mut text_parts = Vec::new();
    let mut html_parts = Vec::new();
    let mut truncated = false;
    let mut collect =
        |parts: Option<&[email::EmailBodyPart]>, out: &mut Vec<String>, want_html: bool| {
            for part in parts.unwrap_or_default() {
                let is_html = part
                    .content_type()
                    .map(|t| t.eq_ignore_ascii_case("text/html"))
                    .unwrap_or(false);
                if is_html != want_html {
                    continue;
                }
                if let Some(value) = part.part_id().and_then(|pid| email.body_value(pid)) {
                    truncated |= value.is_truncated();
                    out.push(value.value().to_string());
                }
            }
        };
    collect(email.text_body(), &mut text_parts, false);
    collect(email.html_body(), &mut html_parts, true);

    let attachments = email
        .attachments()
        .unwrap_or_default()
        .iter()
        .filter_map(|part| {
            let blob_id = part.blob_id()?.to_string();
            Some(AttachmentMeta {
                jmap_blob_id: blob_id,
                filename: part
                    .name()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "attachment".to_string()),
                mime: part
                    .content_type()
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                size: part.size() as u64,
                content_id: part
                    .content_id()
                    .map(|c| c.trim_matches(['<', '>']).to_string()),
                disposition: part.content_disposition().map(|d| d.to_string()),
            })
        })
        .collect();

    RawMessage {
        jmap_id: email.id().unwrap_or_default().to_string(),
        thread_id: email.thread_id().unwrap_or_default().to_string(),
        message_id: email
            .message_id()
            .and_then(|m| m.first())
            .map(|s| s.to_string()),
        in_reply_to: email
            .in_reply_to()
            .and_then(|m| m.first())
            .map(|s| s.to_string()),
        references: email.references().map(|r| r.to_vec()).unwrap_or_default(),
        from: addrs(email.from()),
        to: addrs(email.to()),
        cc: addrs(email.cc()),
        reply_to: addrs(email.reply_to()),
        subject: email.subject().map(|s| s.to_string()),
        received_at_epoch: email.received_at(),
        sent_at_epoch: email.sent_at(),
        mailbox_ids: email
            .mailbox_ids()
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
        keywords: email
            .keywords()
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
        preview: email.preview().map(|s| s.to_string()),
        size: email.size() as u64,
        text_parts,
        html_parts,
        truncated,
        attachments,
    }
}
