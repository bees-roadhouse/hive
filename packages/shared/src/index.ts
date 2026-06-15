// hive domain — journal-first edition.
//
// The journal is the single, write-only input: people and AIs write entries in
// natural prose. Structured items (tasks, decisions, events) *emerge* from that
// prose: each is "anchored" to the exact span of text it came from, so we can
// always show the original sentence next to the structured card.
//
// Mentions (@pia, @nate, …) drive a per-actor inbox — humans and AIs each get
// one. The whole thing is MCP-first; the HTTP MCP server is the primary surface.

export type ActorKind = "human" | "ai";
export interface ActorInfo {
  name: string;
  kind: ActorKind;
}

// ---- people (the writers; kind human|ai) ----

export interface Person {
  id: string;
  slug: string;
  name: string;
  kind: ActorKind;
  /** For AI writers: the slug of their human owner. null for humans. */
  owner: string | null;
  /** Freeform identity profile — who they are / what they do. */
  bio: string | null;
  /** Short role/title, e.g. "VP of Technology". */
  role: string | null;
  created_at: string;
}

export type PersonPatch = Partial<Pick<Person, "name" | "kind" | "owner" | "bio" | "role">>;

// ---- external identities (platform user IDs → centralized actor) ----

export interface Identity {
  id: string;
  platform: string;
  platform_id: string;
  actor: string;
  created_at: string;
}

export interface NewIdentity {
  platform: string;
  platform_id: string;
  actor: string;
}

export type IdentityPatch = Partial<Pick<Identity, "actor">>;

// ---- shares ----

export type ShareScope = "entry" | "journal";

export interface Share {
  id: string;
  /** 'entry' → ref is a journal entry id; 'journal' → ref is an author slug. */
  scope: ShareScope;
  ref: string;
  /** Person slug the share is granted to. */
  viewer: string;
  created_at: string;
}

export interface NewShare {
  scope: ShareScope;
  ref: string;
  viewer: string;
}

// ---- journal writers (for filter UI) ----

export interface JournalWriter {
  slug: string;
  name: string;
  kind: ActorKind;
  owner: string | null;
}

/** The known cast. Mentions resolve against these to drive inboxes. */
export const ACTORS: ActorInfo[] = [
  { name: "nate", kind: "human" },
  { name: "maggie", kind: "human" },
  { name: "pia", kind: "ai" },
  { name: "apis", kind: "ai" },
  { name: "cera", kind: "ai" },
];
export const ACTOR_NAMES = ACTORS.map((a) => a.name);
export const isAi = (name: string) => ACTORS.find((a) => a.name === name)?.kind === "ai";

// ---- auth, users, onboarding (v0.1.1) ----

/** The app version that introduced auth + onboarding. The DB records the
 *  version that initialized it; databases created before this never show the
 *  onboarding wizard. */
export const APP_VERSION = "0.1.3";

export type UserRole = "admin" | "member";

/** A login account. `actor` is the person slug this user writes as — so the
 *  authenticated identity, not a spoofable header, drives the journal/inbox. */
export interface User {
  id: string;
  actor: string;
  email: string;
  name: string;
  role: UserRole;
  created_at: string;
  last_login_at: string | null;
}

/** A user without the password hash — the only shape that crosses the wire. */
export interface SafeUser {
  id: string;
  actor: string;
  email: string;
  name: string;
  role: UserRole;
}

/** A bearer token for programmatic clients (CLI, MCP, AI agents). The plaintext
 *  is shown once at creation; only its hash is stored. `kind='oauth'` tokens were
 *  minted via the OAuth consent flow and carry a client + expiry; `kind='pat'`
 *  (or null) are admin-minted personal tokens with no expiry. */
export interface ApiToken {
  id: string;
  actor: string;
  label: string;
  created_at: string;
  last_used_at: string | null;
  created_by: string;
  /** ISO expiry; null = legacy non-expiring token. Resolution rejects expired tokens. */
  expires_at: string | null;
  // OAuth-minted tokens (kind='oauth') carry a client + granting human + scope;
  // kind='pat' (or null) are admin-minted personal tokens.
  kind: "pat" | "oauth" | null;
  client_id: string | null;
  granted_by: string | null;
  scope: string | null;
}

/** API-token expiry policy. New tokens get DEFAULT days unless specified; never more than MAX. */
export const API_TOKEN_MAX_EXPIRY_DAYS = 365;
export const API_TOKEN_DEFAULT_EXPIRY_DAYS = 90;

/** A dynamically-registered OAuth client (RFC 7591). */
export interface OAuthClient {
  client_id: string;
  client_name: string;
  redirect_uris: string[];
  grant_types: string[];
  created_at: string;
}

/** A registered OAuth client plus live token stats, for the admin connected-apps view. */
export interface OAuthClientStatus {
  client_id: string;
  client_name: string;
  created_at: string;
  /** Count of this client's currently-active (non-expired) oauth tokens. */
  active_tokens: number;
  /** Most-recent last_used_at across this client's tokens (null = never used). */
  last_used_at: string | null;
}

/** An AI identity a signed-in human owns and may grant via the consent flow. */
export interface AiIdentity {
  slug: string;
  name: string;
}

/** Payload the consent screen reads to render the grant UI. */
export interface OAuthConsentContext {
  client_name: string;
  identities: AiIdentity[];
  csrf: string;
}

/** Public auth capabilities the SPA reads before login. */
export interface AuthConfig {
  oidc: boolean;
  instanceName: string | null;
}

/** Bulk historical import (legacy hive.db → this instance). Rows carry their original
 *  ids + timestamps; the importer is idempotent (existing ids are skipped). */
export interface LegacyImport {
  journal?: { id: string; author: string; body: string; tags: string[]; created_at: string }[];
  projects?: { id: string; name: string; slug: string; created_at: string }[];
  tasks?: {
    id: string;
    project: string | null;
    title: string;
    body: string;
    status: string;
    priority: string;
    tags: string[];
    assignees: string[];
    due: string | null;
    created_at: string;
    updated_at: string;
  }[];
  links?: {
    id: string;
    source_kind: string;
    source_id: string;
    target_kind: string;
    target_id: string;
    rel: string;
    created_at: string;
  }[];
}

export type ImportCounts = { inserted: number; skipped: number };
export interface ImportResult {
  journal: ImportCounts;
  projects: ImportCounts;
  tasks: ImportCounts;
  links: ImportCounts;
}

// ---- admin: actor delete + merge ----

/** Per-table counts from an actor delete cascade. `dryRun` reports the same
 *  shape WITHOUT mutating, so the UI can confirm "this will delete N…" first. */
export interface ActorDeleteResult {
  actor: string;
  dryRun: boolean;
  journal: number;
  tasks: number;
  decisions: number;
  events: number;
  anchors: number;
  links: number;
  embeddings: number;
  search: number;
  inbox: number;
  shares: number;
  profile: number;
  users: number;
  sessions: number;
  api_tokens: number;
  oauth_codes: number;
  wire: number;
  sources: number;
  people: number;
}

/** Per-table counts from folding `from` into `to` (reassignment, not deletion;
 *  `people`/`profile`/`users` for `from` are removed once their data is moved). */
export interface ActorMergeResult {
  from: string;
  into: string;
  dryRun: boolean;
  journal: number;
  tasks: number;
  decisions: number;
  events: number;
  inbox: number;
  shares: number;
  api_tokens: number;
  oauth_codes: number;
  wire: number;
  sources: number;
  people_owner: number;
  profile: number;
  users: number;
}

/** Public first-run state — the SPA reads this before anything else. */
export interface OnboardingStatus {
  completed: boolean;
  instanceName: string | null;
  version: string;
}

export interface OnboardingPayload {
  instanceName: string;
  adminName: string;
  adminEmail: string;
  password: string;
}

/** Who the caller is, resolved from a session cookie or bearer token. */
export interface AuthMe {
  user: SafeUser | null;
  principal: "session" | "token" | null;
}

export type TaskStatus = "todo" | "doing" | "blocked" | "done";
export type Priority = "low" | "normal" | "high" | "urgent";
export type DecisionStatus = "proposed" | "accepted" | "rejected" | "superseded";

/** The structured kinds that can be anchored into a journal entry. */
export type AnchorKind = "task" | "decision" | "event";
/** Everything addressable in search / inbox / links. */
export type EntityKind = AnchorKind | "journal" | "person" | "topic" | "project" | "phase";

export const TASK_STATUSES: TaskStatus[] = ["todo", "doing", "blocked", "done"];
export const PRIORITIES: Priority[] = ["low", "normal", "high", "urgent"];
export const DECISION_STATUSES: DecisionStatus[] = [
  "proposed",
  "accepted",
  "rejected",
  "superseded",
];
export const ANCHOR_KINDS: AnchorKind[] = ["task", "decision", "event"];

// ---- journal (the source of truth) ----

export interface JournalEntry {
  id: string;
  author: string;
  body: string;
  tags: string[];
  /** actors @mentioned in the body. */
  mentions: string[];
  /**
   * Memory namespace owner (the human the writing principal acts for).
   * null/absent = global/continuous history.
   */
  user_scope?: string | null;
  created_at: string;
}

/** A span of an entry's body that produced a structured entity. */
export interface Anchor {
  id: string;
  entry_id: string;
  start: number;
  end: number;
  text: string;
  kind: AnchorKind;
  ref_id: string;
  created_at: string;
}

// ---- structured entities (all carry their journal origin) ----

export interface Task {
  id: string;
  title: string;
  body: string;
  status: TaskStatus;
  priority: Priority;
  tags: string[];
  assignees: string[];
  project: string | null;
  phase: string | null;
  due: string | null;
  origin_entry_id: string | null;
  anchor_text: string | null;
  created_at: string;
  updated_at: string;
}

export interface Decision {
  id: string;
  title: string;
  context: string;
  decision: string;
  consequences: string;
  status: DecisionStatus;
  tags: string[];
  assignees: string[];
  project: string | null;
  supersedes: string | null;
  origin_entry_id: string | null;
  anchor_text: string | null;
  created_at: string;
  updated_at: string;
}

/** A happening pulled from prose — a meeting, a ship, a deadline. */
export interface EventItem {
  id: string;
  title: string;
  body: string;
  /** when it happens/happened, ISO-ish, free-form. */
  at: string | null;
  tags: string[];
  assignees: string[];
  origin_entry_id: string | null;
  anchor_text: string | null;
  created_at: string;
}

// ---- inbox (per actor, humans + AIs) ----

export type InboxReason = "mention" | "assignment" | "decision" | "event";

export interface InboxItem {
  id: string;
  recipient: string;
  from: string;
  reason: InboxReason;
  ref_kind: EntityKind;
  ref_id: string;
  entry_id: string | null;
  snippet: string;
  created_at: string;
  read_at: string | null;
}

// ---- supporting ----

export interface Project {
  id: string;
  name: string;
  slug: string;
  created_at: string;
}

export interface Person {
  id: string;
  name: string;
  slug: string;
  kind: "human" | "ai";
  created_at: string;
}

export interface Topic {
  id: string;
  name: string;
  slug: string;
  created_at: string;
}

export interface Phase {
  id: string;
  project: string;
  name: string;
  position: number;
  created_at: string;
}

/** A resolved bracket token reference in a journal entry body. */
export interface JournalRef {
  kind: "person" | "topic" | "project" | "phase" | "task";
  id: string;
  slug: string;
  name: string;
  /** char offset of `[` in the body */
  start: number;
  /** char offset one past `]` in the body */
  end: number;
}

/** Autocomplete candidate for the journal editor. */
export interface AutocompleteItem {
  kind: "person" | "topic" | "project" | "phase" | "task";
  id: string;
  slug: string;
  label: string;
}

export interface Link {
  id: string;
  source_kind: EntityKind;
  source_id: string;
  target_kind: EntityKind;
  target_id: string;
  rel: string;
  created_at: string;
}

export interface WireEvent {
  id: string;
  kind: string;
  actor: string;
  payload: unknown;
  created_at: string;
}

export interface SearchHit {
  kind: EntityKind;
  id: string;
  title: string;
  snippet: string;
  score: number;
}

// ---- knowledge graph ----

/** A node in the knowledge graph; `id` is the `kind:ref_id` composite key. */
export interface GraphNode {
  id: string;
  kind: EntityKind;
  title: string;
}

/** A directed edge; `source`/`target` are `kind:ref_id` keys into the nodes. */
export interface GraphEdge {
  source: string;
  target: string;
  rel: string;
}

export interface GraphData {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

// ---- embeddings admin ----

export interface EmbeddingStats {
  total: number;
  model: string;
  /** How many items are currently embeddable (the backfill target). */
  embeddable: number;
  /** Embeddable items whose stored embedding is missing or stale. */
  pending: number;
  byKind: { kind: string; count: number }[];
  byModel: { model: string; dim: number; count: number }[];
}

// ---- worker: sources, outbound queue, status ----

export type SourceKind = "rss" | "scrape";
export type Severity = "critical" | "high" | "medium" | "low" | "info";
export const SEVERITIES: Severity[] = ["critical", "high", "medium", "low", "info"];

/** An external feed the worker polls into wire events. */
export interface Source {
  id: string;
  name: string;
  url: string;
  kind: SourceKind;
  category: string | null;
  severity: Severity;
  interval_secs: number;
  /** actor to ping in their inbox on new items, or null. */
  notify: string | null;
  enabled: boolean;
  /** null = global (all actors see it); actor name = personal. */
  owner: string | null;
  last_polled_at: string | null;
  last_status: string | null;
  created_at: string;
}

export interface NewSource {
  name: string;
  url: string;
  kind?: SourceKind;
  category?: string | null;
  severity?: Severity;
  interval_secs?: number;
  notify?: string | null;
  enabled?: boolean;
  owner?: string | null;
}
export type SourcePatch = Partial<Omit<Source, "id" | "created_at" | "last_polled_at" | "last_status">>;

export type OutboxStatus = "pending" | "done" | "failed";
export interface OutboxJob {
  id: string;
  kind: string;
  payload: unknown;
  status: OutboxStatus;
  attempts: number;
  last_error: string | null;
  run_after: string;
  created_at: string;
  completed_at: string | null;
}

export interface WorkerStatus {
  heartbeat: string | null;
  last_run: {
    at: string;
    polled: number;
    ingested: number;
    outbox: number;
    embedded: number;
    maintenance: string[];
  } | null;
  sources: { total: number; enabled: number };
  outbox: { pending: number; failed: number; done: number };
  embeddings: { count: number; model: string };
}

// ---- views (server resolves anchors → their entities for the client) ----

export type ResolvedAnchor = Anchor & { entity: Task | Decision | EventItem | null };
export interface JournalEntryView extends JournalEntry {
  anchors: ResolvedAnchor[];
  /** Resolved bracket-token references — renderer uses start/end to substitute display names. */
  refs: JournalRef[];
}

export interface DashboardStats {
  entries: number;
  events: number;
  tasks: { total: number } & Record<TaskStatus, number>;
  decisions: { total: number } & Record<DecisionStatus, number>;
  inbox: { recipient: string; kind: ActorKind; unread: number; total: number }[];
  byAuthor: { author: string; entries: number }[];
  recent: WireEvent[];
  /** Open tasks (status != done) that have a due date — for the calendar overlay. */
  tasksWithDue: { id: string; title: string; due: string; status: TaskStatus; assignees: string[] }[];
  /** Journal entry counts per day for the last ~30 days. */
  entriesByDay: { day: string; count: number }[];
  /** Journal entry counts per author (same data as byAuthor but in count form). */
  entriesByAuthor: { author: string; count: number }[];
  /** How often each person is referenced via links (target_kind='person'), most to least. */
  calloutsByPerson: { name: string; slug: string; count: number }[];
}

// ---- profile (the mutable per-actor card; humans + AIs) ----

/** Where a card's facts came from: hand-written vs synthesised from the journal. */
export type ProfileSource = "manual" | "derived";

/** Durable, mutable "who they are" card for an actor — distinct from the
 *  immutable journal. `sections` holds free-form prose blocks (identity,
 *  preferences, working_style, relationships, …) keyed by section name. */
export interface Profile {
  /** people.slug — the PK. */
  actor: string;
  kind: ActorKind;
  display_name: string;
  body: { sections: Record<string, string> };
  source: ProfileSource;
  derived_at: string | null;
  updated_at: string;
}

export interface ProfilePatch {
  display_name?: string;
  kind?: ActorKind;
  /** Section blocks to deep-merge into body.sections (replace per key). */
  sections?: Record<string, string>;
}

// ---- recall (the read/inject composition) ----

/** A journal hit returned by recall — a search hit plus the author + snippet. */
export interface RecallJournalHit extends SearchHit {
  author: string;
  created_at: string;
}

/** A project touched by the recalled material. */
export interface ProjectRef {
  id: string;
  name: string;
  slug: string;
}

/** Everything recall composed, structured so adapters can render their own format. */
export interface RecallData {
  profiles: Profile[];
  journal: RecallJournalHit[];
  tasks: Task[];
  inbox: InboxItem[];
  events: EventItem[];
  projects: ProjectRef[];
}

/** Default brief budget in (approximate) tokens. */
export const RECALL_DEFAULT_BUDGET = 1500;

export interface RecallResult {
  /** Ready-to-inject markdown, trimmed to ~budget tokens. */
  brief: string;
  data: RecallData;
}

// ---- write payloads ----

/** Fields the author may attach when anchoring a span. All optional. */
export interface AnchorFields {
  title?: string;
  status?: TaskStatus | DecisionStatus;
  priority?: Priority;
  assignees?: string[];
  tags?: string[];
  project?: string | null;
  // decision-specific
  context?: string;
  decision?: string;
  consequences?: string;
  supersedes?: string | null;
  // event-specific
  at?: string | null;
}

export interface NewAnchor {
  start: number;
  end: number;
  kind: AnchorKind;
  fields?: AnchorFields;
}

export interface NewJournalEntry {
  author: string;
  body: string;
  tags?: string[];
  anchors?: NewAnchor[];
}

export type TaskPatch = Partial<Pick<Task, "status" | "priority" | "assignees" | "title" | "body" | "tags">>;
export type DecisionPatch = Partial<Pick<Decision, "status" | "title" | "context" | "decision" | "consequences" | "tags" | "assignees">>;

/** Pull @mentions of known actors out of prose. */
export function parseMentions(text: string): string[] {
  const found = new Set<string>();
  for (const m of text.matchAll(/@([a-z][a-z0-9_-]*)/gi)) {
    const name = m[1].toLowerCase();
    if (ACTOR_NAMES.includes(name)) found.add(name);
  }
  return [...found];
}
