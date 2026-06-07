use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// Core domain types — ported from packages/shared/src/index.ts
// ============================================================================

pub type Id = String;
pub type Slug = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Ai,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorInfo {
    pub name: String,
    pub kind: ActorKind,
}

// ---- people ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Person {
    pub id: Id,
    pub slug: Slug,
    pub name: String,
    pub kind: ActorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Slug>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersonPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ActorKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Option<Slug>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bio: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Option<String>>,
}

// ---- identities (cross-platform user ID mapping) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: Id,
    pub platform: String,
    pub platform_id: String,
    pub actor: Slug,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewIdentity {
    pub platform: String,
    pub platform_id: String,
    pub actor: Slug,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<Slug>,
}

// ---- shares ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareScope {
    Entry,
    Journal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub id: Id,
    pub scope: ShareScope,
    pub r#ref: String,
    pub viewer: Slug,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewShare {
    pub scope: ShareScope,
    pub r#ref: String,
    pub viewer: Slug,
}

// ---- journal ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: Id,
    pub author: Slug,
    pub body: String,
    pub tags: Vec<String>,
    pub mentions: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewJournalEntry {
    pub author: Slug,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalWriter {
    pub slug: Slug,
    pub name: String,
    pub kind: ActorKind,
    pub owner: Option<Slug>,
}

// ---- anchors ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    Task,
    Decision,
    Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anchor {
    pub id: Id,
    pub entry_id: Id,
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub kind: AnchorKind,
    pub ref_id: Id,
    pub created_at: DateTime<Utc>,
}

// ---- structured entities ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    Doing,
    Blocked,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Normal,
    High,
    Urgent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Proposed,
    Accepted,
    Rejected,
    Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Id,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub assignees: Vec<Slug>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_entry_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_text: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<TaskStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<Slug>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<Option<Id>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<Option<Id>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<Option<DateTime<Utc>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: Id,
    pub title: String,
    pub context: String,
    pub decision: String,
    pub consequences: String,
    pub status: DecisionStatus,
    pub tags: Vec<String>,
    pub assignees: Vec<Slug>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_entry_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_text: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consequences: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<DecisionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Vec<Slug>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<Option<Id>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Option<Id>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventItem {
    pub id: Id,
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub tags: Vec<String>,
    pub assignees: Vec<Slug>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_entry_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor_text: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ---- projects, topics, phases ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Id,
    pub name: String,
    pub slug: Slug,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    pub id: Id,
    pub name: String,
    pub slug: Slug,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub id: Id,
    pub project: Id,
    pub name: String,
    pub position: i32,
    pub created_at: DateTime<Utc>,
}

// ---- inbox ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxReason {
    Mention,
    Assignment,
    Decision,
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: Id,
    pub recipient: Slug,
    pub from: Slug,
    pub reason: InboxReason,
    pub ref_kind: EntityKind,
    pub ref_id: Id,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<Id>,
    pub snippet: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_at: Option<DateTime<Utc>>,
}

// ---- profile cards ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub actor: Slug,
    pub kind: ActorKind,
    pub display_name: String,
    pub body: ProfileBody,
    pub source: ProfileSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileBody {
    #[serde(default)]
    pub sections: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    Manual,
    Derived,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfilePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ActorKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sections: Option<std::collections::HashMap<String, String>>,
}

// ---- auth / users ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    Admin,
    Member,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Id,
    pub actor: Slug,
    pub email: String,
    pub name: String,
    pub role: UserRole,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_login_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeUser {
    pub id: Id,
    pub actor: Slug,
    pub email: String,
    pub name: String,
    pub role: UserRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: Id,
    pub actor: Slug,
    pub label: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_by: Slug,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<TokenKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<Slug>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Pat,
    OAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiIdentity {
    pub slug: Slug,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConsentContext {
    pub client_name: String,
    pub identities: Vec<AiIdentity>,
    pub csrf: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub oidc: bool,
    pub instance_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMe {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<SafeUser>,
    pub principal: Option<Principal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Principal {
    Session,
    Token,
}

// ---- links, search, graph ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: Id,
    pub source_kind: EntityKind,
    pub source_id: Id,
    pub target_kind: EntityKind,
    pub target_id: Id,
    pub rel: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub kind: EntityKind,
    pub id: Id,
    pub title: String,
    pub snippet: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: EntityKind,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub rel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

// ---- wire, sources, import ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEvent {
    pub id: Id,
    pub kind: String,
    pub actor: Slug,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: Id,
    pub slug: Slug,
    pub name: String,
    pub url: String,
    pub kind: String,
    pub interval_min: i32,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourcePatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_min: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyImport {
    #[serde(default)]
    pub journal: Vec<LegacyJournalEntry>,
    #[serde(default)]
    pub projects: Vec<LegacyProject>,
    #[serde(default)]
    pub tasks: Vec<LegacyTask>,
    #[serde(default)]
    pub links: Vec<LegacyLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyJournalEntry {
    pub id: Id,
    pub author: Slug,
    pub body: String,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyProject {
    pub id: Id,
    pub name: String,
    pub slug: Slug,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyTask {
    pub id: Id,
    pub project: Option<Id>,
    pub title: String,
    pub body: String,
    pub status: String,
    pub priority: String,
    pub tags: Vec<String>,
    pub assignees: Vec<Slug>,
    pub due: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyLink {
    pub id: Id,
    pub source_kind: String,
    pub source_id: Id,
    pub target_kind: String,
    pub target_id: Id,
    pub rel: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportCounts {
    pub inserted: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub journal: ImportCounts,
    pub projects: ImportCounts,
    pub tasks: ImportCounts,
    pub links: ImportCounts,
}

// ---- recall ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResult {
    pub profile: Vec<Profile>,
    pub tasks: Vec<Task>,
    pub inbox: Vec<InboxItem>,
    pub journal: Vec<RecallJournalHit>,
    pub events: Vec<EventItem>,
    pub projects: Vec<Project>,
    pub brief: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallJournalHit {
    pub entry: JournalEntry,
    pub anchors: Vec<Anchor>,
    pub similarity: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallData {
    pub profile: Vec<Profile>,
    pub tasks: Vec<Task>,
    pub inbox: Vec<InboxItem>,
    pub journal: Vec<RecallJournalHit>,
    pub events: Vec<EventItem>,
    pub projects: Vec<Project>,
}

// ---- admin: actor delete + merge ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorDeleteResult {
    pub actor: Slug,
    pub dry_run: bool,
    pub journal: usize,
    pub tasks: usize,
    pub decisions: usize,
    pub events: usize,
    pub anchors: usize,
    pub links: usize,
    pub embeddings: usize,
    pub search: usize,
    pub inbox: usize,
    pub shares: usize,
    pub profile: usize,
    pub users: usize,
    pub sessions: usize,
    pub api_tokens: usize,
    pub oauth_codes: usize,
    pub wire: usize,
    pub sources: usize,
    pub people: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorMergeResult {
    pub from: Slug,
    pub into: Slug,
    pub dry_run: bool,
    pub journal: usize,
    pub tasks: usize,
    pub decisions: usize,
    pub events: usize,
    pub inbox: usize,
    pub shares: usize,
    pub api_tokens: usize,
    pub oauth_codes: usize,
    pub wire: usize,
    pub sources: usize,
    pub people_owner: usize,
    pub profile: usize,
    pub users: usize,
}

// ---- onboarding ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnboardingStatus {
    pub completed: bool,
    pub instance_name: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnboardingPayload {
    pub instance_name: String,
    pub admin_name: String,
    pub admin_email: String,
    pub password: String,
}

// ---- constants ----

pub const APP_VERSION: &str = "0.2.0";
pub const RECALL_DEFAULT_BUDGET: usize = 4000;
pub const API_TOKEN_MAX_EXPIRY_DAYS: i64 = 365;
pub const API_TOKEN_DEFAULT_EXPIRY_DAYS: i64 = 90;

pub fn is_ai(name: &str) -> bool {
    matches!(name, "pia" | "apis" | "cera")
}

pub fn parse_mentions(body: &str) -> Vec<String> {
    body.split_whitespace()
        .filter_map(|word| {
            let word = word.trim_matches(|c: char| !c.is_alphanumeric());
            word.strip_prefix('@').map(|s| s.to_lowercase().to_string())
        })
        .collect()
}
