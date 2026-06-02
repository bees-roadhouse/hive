import type {
  Decision,
  DecisionPatch,
  JournalEntry,
  NewDecision,
  NewJournalEntry,
  NewNote,
  NewTask,
  Note,
  SearchHit,
  Task,
  TaskPatch,
  WireEvent,
} from "@hive/shared";

// Vite proxies /api → hive-api in dev (see vite.config.ts), so the browser
// only ever talks to its own origin.
const ACTOR_KEY = "hive.actor";
export const getActor = () => localStorage.getItem(ACTOR_KEY) ?? "nate";
export const setActor = (a: string) => localStorage.setItem(ACTOR_KEY, a);

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    headers: { "content-type": "application/json", "x-hive-actor": getActor(), ...init?.headers },
  });
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return (res.status === 204 ? undefined : await res.json()) as T;
}

export const api = {
  tasks: (q: { status?: string; project?: string } = {}) => {
    const p = new URLSearchParams(Object.entries(q).filter(([, v]) => v) as [string, string][]);
    return req<Task[]>(`/tasks?${p}`);
  },
  addTask: (t: NewTask) => req<Task>("/tasks", { method: "POST", body: JSON.stringify(t) }),
  patchTask: (id: string, p: TaskPatch) =>
    req<Task>(`/tasks/${id}`, { method: "PATCH", body: JSON.stringify(p) }),
  delTask: (id: string) => req<void>(`/tasks/${id}`, { method: "DELETE" }),

  notes: () => req<Note[]>("/notes"),
  addNote: (n: NewNote) => req<Note>("/notes", { method: "POST", body: JSON.stringify(n) }),

  journal: () => req<JournalEntry[]>("/journal"),
  addJournal: (e: NewJournalEntry) =>
    req<JournalEntry>("/journal", { method: "POST", body: JSON.stringify(e) }),

  decisions: (q: { status?: string } = {}) => {
    const p = new URLSearchParams(Object.entries(q).filter(([, v]) => v) as [string, string][]);
    return req<Decision[]>(`/decisions?${p}`);
  },
  addDecision: (d: NewDecision) =>
    req<Decision>("/decisions", { method: "POST", body: JSON.stringify(d) }),
  patchDecision: (id: string, p: DecisionPatch) =>
    req<Decision>(`/decisions/${id}`, { method: "PATCH", body: JSON.stringify(p) }),

  search: (query: string) => req<SearchHit[]>(`/search?q=${encodeURIComponent(query)}`),
  wire: () => req<WireEvent[]>("/wire"),
};
