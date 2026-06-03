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
export type PersonPatch = Partial<Pick<Person, "name" | "kind">>;

/** Pull @mentions of known actors out of prose. */
export function parseMentions(text: string): string[] {
  const found = new Set<string>();
  for (const m of text.matchAll(/@([a-z][a-z0-9_-]*)/gi)) {
    const name = m[1].toLowerCase();
    if (ACTOR_NAMES.includes(name)) found.add(name);
  }
  return [...found];
}
