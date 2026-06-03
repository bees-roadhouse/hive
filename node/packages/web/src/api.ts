import type {
  AutocompleteItem,
  DashboardStats,
  Decision,
  EmbeddingStats,
  EventItem,
  GraphData,
  InboxItem,
  JournalEntryView,
  NewJournalEntry,
  NewSource,
  OutboxJob,
  Person,
  PersonPatch,
  Phase,
  Project,
  SearchHit,
  Source,
  SourceKind,
  SourcePatch,
  Task,
  TaskPatch,
  Topic,
  WireEvent,
  WorkerStatus,
} from "@hive/shared";

// Vite proxies /api → hive-api in dev (see vite.config.ts).
const ACTOR_KEY = "hive.actor";
export const getActor = () => localStorage.getItem(ACTOR_KEY) ?? "nate";
export const setActor = (a: string) => localStorage.setItem(ACTOR_KEY, a);

// Done-retention: how long (in hours) a DONE task stays visible before it's
// hidden by default. The Tasks board respects this unless "show done" is toggled.
const DONE_RETENTION_KEY = "hive.doneRetentionHours";
const DONE_RETENTION_DEFAULT = 24;
export const getDoneRetentionHours = (): number => {
  const raw = localStorage.getItem(DONE_RETENTION_KEY);
  const n = raw !== null ? Number(raw) : NaN;
  // Sentinel: Infinity means "always show" (never hide by age).
  return Number.isFinite(n) && n >= 0 ? n : DONE_RETENTION_DEFAULT;
};
export const setDoneRetentionHours = (hours: number): void =>
  localStorage.setItem(DONE_RETENTION_KEY, String(hours));

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    headers: { "content-type": "application/json", "x-hive-actor": getActor(), ...init?.headers },
  });
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return (res.status === 204 ? undefined : await res.json()) as T;
}

export const api = {
  journal: (limit = 50, offset = 0) =>
    req<JournalEntryView[]>(`/journal?limit=${limit}&offset=${offset}`),
  append: (e: NewJournalEntry) =>
    req<JournalEntryView>("/journal", { method: "POST", body: JSON.stringify(e) }),

  tasks: (q: { status?: string; assignee?: string } = {}) => {
    const p = new URLSearchParams(Object.entries(q).filter(([, v]) => v) as [string, string][]);
    return req<Task[]>(`/tasks?${p}`);
  },
  patchTask: (id: string, p: TaskPatch) =>
    req<Task>(`/tasks/${id}`, { method: "PATCH", body: JSON.stringify(p) }),

  decisions: () => req<Decision[]>("/decisions"),
  events: () => req<EventItem[]>("/events"),

  inbox: (recipient: string, unread = false) =>
    req<InboxItem[]>(`/inbox/${recipient}?unread=${unread ? 1 : 0}`),
  markRead: (id: string) => req<{ marked: boolean }>(`/inbox/item/${id}/read`, { method: "POST" }),
  markAllRead: (recipient: string) =>
    req<{ marked: number }>(`/inbox/${recipient}/read`, { method: "POST" }),

  search: (query: string, mode: "keyword" | "semantic" = "keyword") =>
    req<SearchHit[]>(`/search?q=${encodeURIComponent(query)}&mode=${mode}`),
  wire: () => req<WireEvent[]>("/wire"),
  dashboard: () => req<DashboardStats>("/dashboard"),
  graph: () => req<GraphData>("/graph"),
  embeddings: () => req<EmbeddingStats>("/embeddings"),

  sources: (owner?: string) =>
    req<Source[]>(`/sources${owner ? `?owner=${encodeURIComponent(owner)}` : ""}`),
  addSource: (s: NewSource & { scope?: "global" | "me" }) =>
    req<Source>("/sources", { method: "POST", body: JSON.stringify(s) }),
  patchSource: (id: string, p: SourcePatch) =>
    req<Source>(`/sources/${id}`, { method: "PATCH", body: JSON.stringify(p) }),
  delSource: (id: string) => req<void>(`/sources/${id}`, { method: "DELETE" }),
  worker: () => req<WorkerStatus>("/worker"),
  outbox: () => req<OutboxJob[]>("/outbox"),

  autocomplete: (q: string, kinds: string[]) =>
    req<AutocompleteItem[]>(
      `/autocomplete?q=${encodeURIComponent(q)}&kinds=${kinds.join(",")}`,
    ),

  people: () => req<Person[]>("/people"),
  addPerson: (p: { name: string; kind?: "human" | "ai" }) =>
    req<Person>("/people", { method: "POST", body: JSON.stringify(p) }),
  patchPerson: (id: string, p: PersonPatch) =>
    req<Person>(`/people/${id}`, { method: "PATCH", body: JSON.stringify(p) }),

  topics: () => req<Topic[]>("/topics"),
  projects: () => req<Project[]>("/projects"),
  projectById: (id: string) =>
    req<Project & { tasks: Task[]; phases: Phase[] }>(`/projects/${id}`),
};
