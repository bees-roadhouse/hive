import type {
  DashboardStats,
  Decision,
  EventItem,
  InboxItem,
  JournalEntryView,
  NewJournalEntry,
  SearchHit,
  Task,
  TaskPatch,
  WireEvent,
} from "@hive/shared";

// Vite proxies /api → hive-api in dev (see vite.config.ts).
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
  journal: (limit = 50) => req<JournalEntryView[]>(`/journal?limit=${limit}`),
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

  search: (query: string) => req<SearchHit[]>(`/search?q=${encodeURIComponent(query)}`),
  wire: () => req<WireEvent[]>("/wire"),
  dashboard: () => req<DashboardStats>("/dashboard"),
};
