// Shared domain types for hive — the state every Bee's Roadhouse AI config
// (Pia, Apis, Cera, peers) reads and writes: tasks, journal, notes, the
// knowledge-graph links between them, and the wire event log.
//
// This is the fun Node/Solid rewrite, so the model is a faithful-in-spirit
// distillation of the rust workspace rather than a column-for-column port.

export type TaskStatus = "todo" | "doing" | "blocked" | "done";
export type Priority = "low" | "normal" | "high" | "urgent";

/** The kinds of things that can live in the graph and be linked together. */
export type EntityKind = "task" | "note" | "journal" | "decision" | "project";

/** Lifecycle of a decision record (ADR-style). */
export type DecisionStatus = "proposed" | "accepted" | "rejected" | "superseded";

export interface Project {
  id: string;
  name: string;
  created_at: string;
}

export interface Task {
  id: string;
  project: string | null;
  title: string;
  body: string;
  status: TaskStatus;
  priority: Priority;
  tags: string[];
  created_at: string;
  updated_at: string;
}

export interface JournalEntry {
  id: string;
  project: string | null;
  body: string;
  tags: string[];
  created_at: string;
}

export interface Note {
  id: string;
  title: string;
  body: string;
  tags: string[];
  created_at: string;
  updated_at: string;
}

/**
 * A decision record. Captures *why* a choice was made so the Bees don't
 * relitigate it later: the context, the call, and what it commits us to.
 */
export interface Decision {
  id: string;
  title: string;
  context: string;
  decision: string;
  consequences: string;
  status: DecisionStatus;
  project: string | null;
  /** id of the decision this one supersedes, if any. */
  supersedes: string | null;
  tags: string[];
  created_at: string;
  updated_at: string;
}

/** A directed edge in the knowledge graph (e.g. task --relates--> note). */
export interface Link {
  id: string;
  source_kind: EntityKind;
  source_id: string;
  target_kind: EntityKind;
  target_id: string;
  rel: string;
  created_at: string;
}

/** Append-only event log every peer can tail to stay in sync. */
export interface WireEvent {
  id: string;
  kind: string;
  actor: string;
  payload: unknown;
  created_at: string;
}

/** A unified hit from the cross-entity search index. */
export interface SearchHit {
  kind: EntityKind;
  id: string;
  title: string;
  snippet: string;
  score: number;
}

// ---- request payloads (shared by api, web, cli) ----

export interface NewTask {
  title: string;
  body?: string;
  project?: string | null;
  status?: TaskStatus;
  priority?: Priority;
  tags?: string[];
}

export type TaskPatch = Partial<Omit<Task, "id" | "created_at" | "updated_at">>;

export interface NewNote {
  title: string;
  body?: string;
  tags?: string[];
}

export interface NewJournalEntry {
  body: string;
  project?: string | null;
  tags?: string[];
}

export interface NewDecision {
  title: string;
  context?: string;
  decision: string;
  consequences?: string;
  status?: DecisionStatus;
  project?: string | null;
  supersedes?: string | null;
  tags?: string[];
}

export type DecisionPatch = Partial<Omit<Decision, "id" | "created_at" | "updated_at">>;

export const TASK_STATUSES: TaskStatus[] = ["todo", "doing", "blocked", "done"];
export const PRIORITIES: Priority[] = ["low", "normal", "high", "urgent"];
export const DECISION_STATUSES: DecisionStatus[] = [
  "proposed",
  "accepted",
  "rejected",
  "superseded",
];
