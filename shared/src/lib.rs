// hive domain — journal-first edition. Rust port of packages/shared/src/index.ts.
//
// The journal is the single, write-only input: people and AIs write entries in
// natural prose. Structured items (tasks, decisions, events) *emerge* from that
// prose: each is "anchored" to the exact span of text it came from.
//
// Parity rules for this crate:
// - Timestamps are plain ISO-8601 strings (JS `new Date().toISOString()` shape,
//   millisecond precision, trailing Z). Stored and served verbatim so they sort
//   lexicographically alongside rows written by the Node API.
// - Optional fields serialize as explicit nulls (no skip_serializing_if), the
//   same JSON the Node API emits.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const APP_VERSION: &str = "0.1.3";

// ---- actors ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    Human,
    Ai,
}

impl ActorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorKind::Human => "human",
            ActorKind::Ai => "ai",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        if s == "ai" {
            ActorKind::Ai
        } else {
            ActorKind::Human
        }
    }
}

/// The known cast. Mentions resolve against these to drive inboxes.
pub const ACTORS: &[(&str, ActorKind)] = &[
    ("nate", ActorKind::Human),
    ("maggie", ActorKind::Human),
    ("pia", ActorKind::Ai),
    ("apis", ActorKind::Ai),
    ("cera", ActorKind::Ai),
];

pub fn actor_names() -> Vec<&'static str> {
    ACTORS.iter().map(|(n, _)| *n).collect()
}

pub fn is_ai(name: &str) -> bool {
    ACTORS
        .iter()
        .any(|(n, k)| *n == name && *k == ActorKind::Ai)
}

// ---- people (the writers; kind human|ai) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub kind: ActorKind,
    /// For AI writers: the slug of their human owner. null for humans.
    pub owner: Option<String>,
    /// Freeform identity profile — who they are / what they do.
    pub bio: Option<String>,
    /// Short role/title, e.g. "VP of Technology".
    pub role: Option<String>,
    pub created_at: String,
}

/// Patch semantics: absent key = keep, explicit null = clear (double Option).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PersonPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<ActorKind>,
    #[serde(default, deserialize_with = "double_option")]
    pub owner: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub bio: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub role: Option<Option<String>>,
}

// ---- shares ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareScope {
    Entry,
    Journal,
}

impl ShareScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareScope::Entry => "entry",
            ShareScope::Journal => "journal",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        if s == "journal" {
            ShareScope::Journal
        } else {
            ShareScope::Entry
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub id: String,
    /// 'entry' → ref is a journal entry id; 'journal' → ref is an author slug.
    pub scope: ShareScope,
    #[serde(rename = "ref")]
    pub ref_: String,
    /// Person slug the share is granted to.
    pub viewer: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewShare {
    pub scope: ShareScope,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub viewer: String,
}

// ---- journal writers (for filter UI) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalWriter {
    pub slug: String,
    pub name: String,
    pub kind: ActorKind,
    pub owner: Option<String>,
}

// ---- auth, users, onboarding ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    Member,
}

impl UserRole {
    pub fn as_str(self) -> &'static str {
        match self {
            UserRole::Admin => "admin",
            UserRole::Member => "member",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        if s == "admin" {
            UserRole::Admin
        } else {
            UserRole::Member
        }
    }
}

/// A login account. `actor` is the person slug this user writes as.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub actor: String,
    pub email: String,
    pub name: String,
    pub role: UserRole,
    pub created_at: String,
    pub last_login_at: Option<String>,
}

/// A user without the password hash — the only shape that crosses the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeUser {
    pub id: String,
    pub actor: String,
    pub email: String,
    pub name: String,
    pub role: UserRole,
}

/// A bearer token for programmatic clients. kind='oauth' tokens were minted via
/// the OAuth consent flow; kind='pat' (or null) are admin-minted personal tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: String,
    pub actor: String,
    pub label: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub created_by: String,
    /// ISO expiry; null = legacy non-expiring token.
    pub expires_at: Option<String>,
    pub kind: Option<String>,
    pub client_id: Option<String>,
    pub granted_by: Option<String>,
    pub scope: Option<String>,
}

pub const API_TOKEN_MAX_EXPIRY_DAYS: i64 = 365;
pub const API_TOKEN_DEFAULT_EXPIRY_DAYS: i64 = 90;

/// A dynamically-registered OAuth client (RFC 7591).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub created_at: String,
}

/// A registered OAuth client plus live token stats, for the admin
/// connected-apps view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClientStatus {
    pub client_id: String,
    pub client_name: String,
    pub created_at: String,
    /// Count of this client's currently-active (non-expired) oauth tokens.
    pub active_tokens: i64,
    /// Most-recent `last_used_at` across this client's tokens (null = never used).
    pub last_used_at: Option<String>,
}

/// An AI identity a signed-in human owns and may grant via the consent flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiIdentity {
    pub slug: String,
    pub name: String,
}

/// Payload the consent screen reads to render the grant UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConsentContext {
    pub client_name: String,
    pub identities: Vec<AiIdentity>,
    pub csrf: String,
}

/// Public auth capabilities the SPA reads before login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub oidc: bool,
    #[serde(rename = "instanceName")]
    pub instance_name: Option<String>,
}

// ---- bulk historical import ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyImport {
    #[serde(default)]
    pub journal: Option<Vec<LegacyJournalRow>>,
    #[serde(default)]
    pub projects: Option<Vec<LegacyProjectRow>>,
    #[serde(default)]
    pub tasks: Option<Vec<LegacyTaskRow>>,
    #[serde(default)]
    pub links: Option<Vec<LegacyLinkRow>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyJournalRow {
    pub id: String,
    pub author: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyProjectRow {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyTaskRow {
    pub id: String,
    pub project: Option<String>,
    pub title: String,
    pub body: String,
    pub status: String,
    pub priority: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    pub due: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyLinkRow {
    pub id: String,
    pub source_kind: String,
    pub source_id: String,
    pub target_kind: String,
    pub target_id: String,
    pub rel: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ImportCounts {
    pub inserted: i64,
    pub skipped: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportResult {
    pub journal: ImportCounts,
    pub projects: ImportCounts,
    pub tasks: ImportCounts,
    pub links: ImportCounts,
}

// ---- admin: actor delete + merge ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActorDeleteResult {
    pub actor: String,
    #[serde(rename = "dryRun")]
    pub dry_run: bool,
    pub journal: i64,
    pub tasks: i64,
    pub decisions: i64,
    pub events: i64,
    pub anchors: i64,
    pub links: i64,
    pub embeddings: i64,
    pub search: i64,
    pub inbox: i64,
    pub shares: i64,
    pub profile: i64,
    pub users: i64,
    pub sessions: i64,
    pub api_tokens: i64,
    pub oauth_codes: i64,
    pub wire: i64,
    pub sources: i64,
    pub people: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActorMergeResult {
    pub from: String,
    pub into: String,
    #[serde(rename = "dryRun")]
    pub dry_run: bool,
    pub journal: i64,
    pub tasks: i64,
    pub decisions: i64,
    pub events: i64,
    pub inbox: i64,
    pub shares: i64,
    pub api_tokens: i64,
    pub oauth_codes: i64,
    pub wire: i64,
    pub sources: i64,
    pub people_owner: i64,
    pub profile: i64,
    pub users: i64,
}

/// Public first-run state — the SPA reads this before anything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnboardingStatus {
    pub completed: bool,
    #[serde(rename = "instanceName")]
    pub instance_name: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OnboardingPayload {
    #[serde(rename = "instanceName")]
    pub instance_name: String,
    #[serde(rename = "adminName")]
    pub admin_name: String,
    #[serde(rename = "adminEmail")]
    pub admin_email: String,
    pub password: String,
}

/// Who the caller is, resolved from a session cookie or bearer token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMe {
    pub user: Option<SafeUser>,
    pub principal: Option<String>,
}

// ---- enums ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Todo,
    Doing,
    Blocked,
    Done,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::Doing => "doing",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "todo" => Some(TaskStatus::Todo),
            "doing" => Some(TaskStatus::Doing),
            "blocked" => Some(TaskStatus::Blocked),
            "done" => Some(TaskStatus::Done),
            _ => None,
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        Self::parse(s).unwrap_or(TaskStatus::Todo)
    }
}

pub const TASK_STATUSES: &[TaskStatus] = &[
    TaskStatus::Todo,
    TaskStatus::Doing,
    TaskStatus::Blocked,
    TaskStatus::Done,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    Normal,
    High,
    Urgent,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
            Priority::Urgent => "urgent",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "low" => Priority::Low,
            "high" => Priority::High,
            "urgent" => Priority::Urgent,
            _ => Priority::Normal,
        }
    }
}

pub const PRIORITIES: &[Priority] = &[
    Priority::Low,
    Priority::Normal,
    Priority::High,
    Priority::Urgent,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    Proposed,
    Accepted,
    Rejected,
    Superseded,
}

impl DecisionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionStatus::Proposed => "proposed",
            DecisionStatus::Accepted => "accepted",
            DecisionStatus::Rejected => "rejected",
            DecisionStatus::Superseded => "superseded",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "accepted" => DecisionStatus::Accepted,
            "rejected" => DecisionStatus::Rejected,
            "superseded" => DecisionStatus::Superseded,
            _ => DecisionStatus::Proposed,
        }
    }
}

pub const DECISION_STATUSES: &[DecisionStatus] = &[
    DecisionStatus::Proposed,
    DecisionStatus::Accepted,
    DecisionStatus::Rejected,
    DecisionStatus::Superseded,
];

/// The structured kinds that can be anchored into a journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnchorKind {
    Task,
    Decision,
    Event,
}

impl AnchorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AnchorKind::Task => "task",
            AnchorKind::Decision => "decision",
            AnchorKind::Event => "event",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "task" => Some(AnchorKind::Task),
            "decision" => Some(AnchorKind::Decision),
            "event" => Some(AnchorKind::Event),
            _ => None,
        }
    }
}

pub const ANCHOR_KINDS: &[AnchorKind] =
    &[AnchorKind::Task, AnchorKind::Decision, AnchorKind::Event];

/// Everything addressable in search / inbox / links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntityKind {
    Task,
    Decision,
    Event,
    Journal,
    Person,
    Topic,
    Project,
    Phase,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntityKind::Task => "task",
            EntityKind::Decision => "decision",
            EntityKind::Event => "event",
            EntityKind::Journal => "journal",
            EntityKind::Person => "person",
            EntityKind::Topic => "topic",
            EntityKind::Project => "project",
            EntityKind::Phase => "phase",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "decision" => EntityKind::Decision,
            "event" => EntityKind::Event,
            "journal" => EntityKind::Journal,
            "person" => EntityKind::Person,
            "topic" => EntityKind::Topic,
            "project" => EntityKind::Project,
            "phase" => EntityKind::Phase,
            _ => EntityKind::Task,
        }
    }
}

// ---- journal (the source of truth) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: String,
    pub author: String,
    pub body: String,
    pub tags: Vec<String>,
    /// actors @mentioned in the body.
    pub mentions: Vec<String>,
    /// Memory namespace owner (the human the writing principal acts for). `None`
    /// = global/continuous history (a system/worker write).
    #[serde(default)]
    pub user_scope: Option<String>,
    pub created_at: String,
}

/// A span of an entry's body that produced a structured entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anchor {
    pub id: String,
    pub entry_id: String,
    pub start: i64,
    pub end: i64,
    pub text: String,
    pub kind: AnchorKind,
    pub ref_id: String,
    pub created_at: String,
}

// ---- structured entities (all carry their journal origin) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub assignees: Vec<String>,
    pub project: Option<String>,
    pub phase: Option<String>,
    pub due: Option<String>,
    pub origin_entry_id: Option<String>,
    pub anchor_text: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub title: String,
    pub context: String,
    pub decision: String,
    pub consequences: String,
    pub status: DecisionStatus,
    pub tags: Vec<String>,
    pub assignees: Vec<String>,
    pub project: Option<String>,
    pub supersedes: Option<String>,
    pub origin_entry_id: Option<String>,
    pub anchor_text: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A happening pulled from prose — a meeting, a ship, a deadline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventItem {
    pub id: String,
    pub title: String,
    pub body: String,
    /// when it happens/happened, ISO-ish, free-form.
    pub at: Option<String>,
    pub tags: Vec<String>,
    pub assignees: Vec<String>,
    pub origin_entry_id: Option<String>,
    pub anchor_text: Option<String>,
    pub created_at: String,
}

// ---- inbox (per actor, humans + AIs) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InboxReason {
    Mention,
    Assignment,
    Decision,
    Event,
}

impl InboxReason {
    pub fn as_str(self) -> &'static str {
        match self {
            InboxReason::Mention => "mention",
            InboxReason::Assignment => "assignment",
            InboxReason::Decision => "decision",
            InboxReason::Event => "event",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "assignment" => InboxReason::Assignment,
            "decision" => InboxReason::Decision,
            "event" => InboxReason::Event,
            _ => InboxReason::Mention,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: String,
    pub recipient: String,
    pub from: String,
    pub reason: InboxReason,
    pub ref_kind: EntityKind,
    pub ref_id: String,
    pub entry_id: Option<String>,
    pub snippet: String,
    pub created_at: String,
    pub read_at: Option<String>,
}

// ---- supporting ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub id: String,
    pub project: String,
    pub name: String,
    pub position: i64,
    pub created_at: String,
}

/// A resolved bracket token reference in a journal entry body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalRef {
    pub kind: EntityKind,
    pub id: String,
    pub slug: String,
    pub name: String,
    /// char offset of `[` in the body
    pub start: i64,
    /// char offset one past `]` in the body
    pub end: i64,
}

/// Autocomplete candidate for the journal editor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutocompleteItem {
    pub kind: EntityKind,
    pub id: String,
    pub slug: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: String,
    pub source_kind: EntityKind,
    pub source_id: String,
    pub target_kind: EntityKind,
    pub target_id: String,
    pub rel: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEvent {
    pub id: String,
    pub kind: String,
    pub actor: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub kind: EntityKind,
    pub id: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
}

// ---- knowledge graph ----

/// A node in the knowledge graph; `id` is the `kind:ref_id` composite key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: EntityKind,
    pub title: String,
}

/// A directed edge; `source`/`target` are `kind:ref_id` keys into the nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub rel: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

// ---- embeddings admin ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingKindCount {
    pub kind: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelCount {
    pub model: String,
    pub dim: i64,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingStats {
    pub total: i64,
    pub model: String,
    /// How many items are currently embeddable (the backfill target).
    pub embeddable: i64,
    /// Embeddable items whose stored embedding is missing or stale.
    pub pending: i64,
    #[serde(rename = "byKind")]
    pub by_kind: Vec<EmbeddingKindCount>,
    #[serde(rename = "byModel")]
    pub by_model: Vec<EmbeddingModelCount>,
}

// ---- worker: sources, outbound queue, status ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Rss,
    Scrape,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Rss => "rss",
            SourceKind::Scrape => "scrape",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        if s == "scrape" {
            SourceKind::Scrape
        } else {
            SourceKind::Rss
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::Info => "info",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "critical" => Severity::Critical,
            "high" => Severity::High,
            "medium" => Severity::Medium,
            "low" => Severity::Low,
            _ => Severity::Info,
        }
    }
}

pub const SEVERITIES: &[Severity] = &[
    Severity::Critical,
    Severity::High,
    Severity::Medium,
    Severity::Low,
    Severity::Info,
];

/// An external feed the worker polls into wire events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub url: String,
    pub kind: SourceKind,
    pub category: Option<String>,
    pub severity: Severity,
    pub interval_secs: i64,
    /// actor to ping in their inbox on new items, or null.
    pub notify: Option<String>,
    pub enabled: bool,
    /// null = global (all actors see it); actor name = personal.
    pub owner: Option<String>,
    pub last_polled_at: Option<String>,
    pub last_status: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewSource {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub kind: Option<SourceKind>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub severity: Option<Severity>,
    #[serde(default)]
    pub interval_secs: Option<i64>,
    #[serde(default)]
    pub notify: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourcePatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub kind: Option<SourceKind>,
    #[serde(default, deserialize_with = "double_option")]
    pub category: Option<Option<String>>,
    #[serde(default)]
    pub severity: Option<Severity>,
    #[serde(default)]
    pub interval_secs: Option<i64>,
    #[serde(default, deserialize_with = "double_option")]
    pub notify: Option<Option<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    pub owner: Option<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboxStatus {
    Pending,
    Done,
    Failed,
}

impl OutboxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxStatus::Pending => "pending",
            OutboxStatus::Done => "done",
            OutboxStatus::Failed => "failed",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "done" => OutboxStatus::Done,
            "failed" => OutboxStatus::Failed,
            _ => OutboxStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxJob {
    pub id: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub status: OutboxStatus,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub run_after: String,
    pub created_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerLastRun {
    pub at: String,
    pub polled: i64,
    pub ingested: i64,
    pub outbox: i64,
    pub embedded: i64,
    pub maintenance: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSourceCounts {
    pub total: i64,
    pub enabled: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerOutboxCounts {
    pub pending: i64,
    pub failed: i64,
    pub done: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEmbeddingCounts {
    pub count: i64,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub heartbeat: Option<String>,
    pub last_run: Option<WorkerLastRun>,
    pub sources: WorkerSourceCounts,
    pub outbox: WorkerOutboxCounts,
    pub embeddings: WorkerEmbeddingCounts,
}

// ---- views (server resolves anchors → their entities for the client) ----

/// Anchor plus its resolved entity (Task | Decision | EventItem | null).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedAnchor {
    #[serde(flatten)]
    pub anchor: Anchor,
    pub entity: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntryView {
    #[serde(flatten)]
    pub entry: JournalEntry,
    pub anchors: Vec<ResolvedAnchor>,
    /// Resolved bracket-token references — renderer substitutes display names.
    pub refs: Vec<JournalRef>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCounts {
    pub total: i64,
    pub todo: i64,
    pub doing: i64,
    pub blocked: i64,
    pub done: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionCounts {
    pub total: i64,
    pub proposed: i64,
    pub accepted: i64,
    pub rejected: i64,
    pub superseded: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxStat {
    pub recipient: String,
    pub kind: ActorKind,
    pub unread: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorCount {
    pub author: String,
    pub entries: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskWithDue {
    pub id: String,
    pub title: String,
    pub due: String,
    pub status: TaskStatus,
    pub assignees: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayCount {
    pub day: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorEntryCount {
    pub author: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonCallout {
    pub name: String,
    pub slug: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardStats {
    pub entries: i64,
    pub events: i64,
    pub tasks: TaskCounts,
    pub decisions: DecisionCounts,
    pub inbox: Vec<InboxStat>,
    #[serde(rename = "byAuthor")]
    pub by_author: Vec<AuthorCount>,
    pub recent: Vec<WireEvent>,
    #[serde(rename = "tasksWithDue")]
    pub tasks_with_due: Vec<TaskWithDue>,
    #[serde(rename = "entriesByDay")]
    pub entries_by_day: Vec<DayCount>,
    #[serde(rename = "entriesByAuthor")]
    pub entries_by_author: Vec<AuthorEntryCount>,
    #[serde(rename = "calloutsByPerson")]
    pub callouts_by_person: Vec<PersonCallout>,
}

// ---- profile (the mutable per-actor card; humans + AIs) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileSource {
    Manual,
    Derived,
}

impl ProfileSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileSource::Manual => "manual",
            ProfileSource::Derived => "derived",
        }
    }
    pub fn from_str_lossy(s: &str) -> Self {
        if s == "derived" {
            ProfileSource::Derived
        } else {
            ProfileSource::Manual
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileBody {
    #[serde(default)]
    pub sections: BTreeMap<String, String>,
}

/// Durable, mutable "who they are" card for an actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// people.slug — the PK.
    pub actor: String,
    pub kind: ActorKind,
    pub display_name: String,
    pub body: ProfileBody,
    pub source: ProfileSource,
    pub derived_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfilePatch {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub kind: Option<ActorKind>,
    /// Section blocks to deep-merge into body.sections (replace per key).
    #[serde(default)]
    pub sections: Option<BTreeMap<String, String>>,
}

// ---- recall (the read/inject composition) ----

/// A journal hit returned by recall — a search hit plus the author + timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallJournalHit {
    #[serde(flatten)]
    pub hit: SearchHit,
    pub author: String,
    pub created_at: String,
}

/// A project touched by the recalled material.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRef {
    pub id: String,
    pub name: String,
    pub slug: String,
}

/// Everything recall composed, structured so adapters can render their own format.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecallData {
    pub profiles: Vec<Profile>,
    pub journal: Vec<RecallJournalHit>,
    pub tasks: Vec<Task>,
    pub inbox: Vec<InboxItem>,
    pub events: Vec<EventItem>,
    pub projects: Vec<ProjectRef>,
}

/// Default brief budget in (approximate) tokens.
pub const RECALL_DEFAULT_BUDGET: usize = 1500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResult {
    /// Ready-to-inject markdown, trimmed to ~budget tokens.
    pub brief: String,
    pub data: RecallData,
}

// ---- write payloads ----

/// Fields the author may attach when anchoring a span. All optional.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnchorFields {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub priority: Option<Priority>,
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub project: Option<Option<String>>,
    // decision-specific
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub consequences: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub supersedes: Option<Option<String>>,
    // event-specific
    #[serde(default, deserialize_with = "double_option")]
    pub at: Option<Option<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewAnchor {
    pub start: i64,
    pub end: i64,
    pub kind: AnchorKind,
    #[serde(default)]
    pub fields: Option<AnchorFields>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewJournalEntry {
    /// Overridden server-side with the authenticated identity on the REST path.
    #[serde(default)]
    pub author: Option<String>,
    pub body: String,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub anchors: Option<Vec<NewAnchor>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaskPatch {
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub priority: Option<Priority>,
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DecisionPatch {
    #[serde(default)]
    pub status: Option<DecisionStatus>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub decision: Option<String>,
    #[serde(default)]
    pub consequences: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
}

// ---- identities (cross-platform identity mapping; Rust-branch addition) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub platform: String,
    pub platform_id: String,
    pub actor: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewIdentity {
    pub platform: String,
    pub platform_id: String,
    pub actor: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IdentityPatch {
    #[serde(default)]
    pub actor: Option<String>,
}

// ---- helpers ----

/// Pull @mentions of known actors out of prose (parity with shared parseMentions:
/// `@([a-z][a-z0-9_-]*)` case-insensitive, matched against the known cast).
pub fn parse_mentions(text: &str) -> Vec<String> {
    let names = actor_names();
    let mut found: Vec<String> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len()
                && ((bytes[end] as char).is_ascii_alphanumeric()
                    || bytes[end] == b'_'
                    || bytes[end] == b'-')
            {
                end += 1;
            }
            if end > start && (bytes[start] as char).is_ascii_alphabetic() {
                let name = text[start..end].to_lowercase();
                if names.contains(&name.as_str()) && !found.contains(&name) {
                    found.push(name);
                }
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    found
}

/// lowercase, whitespace runs→'-', strip non [a-z0-9-] (parity with store slugify).
pub fn slugify(s: &str) -> String {
    let lowered = s.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut in_ws = false;
    for c in lowered.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push('-');
                in_ws = true;
            }
        } else {
            in_ws = false;
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                out.push(c);
            }
        }
    }
    out
}

/// Node `snip`: truncate to n UTF-16 code units (JS `.length`/`.slice`
/// semantics — emoji count as 2) with a `…` suffix.
pub fn snip(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() > n {
        format!("{}…", String::from_utf16_lossy(&units[..n]))
    } else {
        s.to_string()
    }
}

/// Deserialize a JSON field where absent = None, null = Some(None), value = Some(Some(v)).
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_match_known_actors_only() {
        assert_eq!(
            parse_mentions("ping @pia and @nate about @unknown"),
            vec!["pia", "nate"]
        );
        assert_eq!(parse_mentions("@PIA caps fold"), vec!["pia"]);
        assert_eq!(
            parse_mentions("email a@pia.example is still a mention in JS"),
            vec!["pia"]
        );
        assert!(parse_mentions("no mentions here").is_empty());
    }

    #[test]
    fn slugify_matches_node() {
        assert_eq!(slugify("Bee's Roadhouse"), "bees-roadhouse");
        assert_eq!(slugify("MiXeD 123"), "mixed-123");
    }

    #[test]
    fn snip_counts_utf16_units_like_js() {
        assert_eq!(snip("short", 140), "short");
        let long = "é".repeat(200);
        let s = snip(&long, 140);
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), 141);
        // '😀' is 2 UTF-16 units: JS snip("😀".repeat(80), 140) keeps 70 emoji.
        let emoji = "😀".repeat(80);
        let s = snip(&emoji, 140);
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), 71);
        // At exactly n units JS does not truncate.
        assert_eq!(snip(&"😀".repeat(70), 140), "😀".repeat(70));
    }

    #[test]
    fn person_patch_distinguishes_null_from_absent() {
        let p: PersonPatch = serde_json::from_str(r#"{"owner": null}"#).unwrap();
        assert_eq!(p.owner, Some(None));
        let p: PersonPatch = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(p.owner, None);
    }
}
