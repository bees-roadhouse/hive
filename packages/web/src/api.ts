import type {
  ActorDeleteResult,
  ActorMergeResult,
  AutocompleteItem,
  DashboardStats,
  Decision,
  EmbeddingStats,
  EventItem,
  GraphData,
  ImportResult,
  InboxItem,
  JournalEntryView,
  JournalWriter,
  NewJournalEntry,
  NewShare,
  NewSource,
  OutboxJob,
  Person,
  PersonPatch,
  Phase,
  Project,
  SearchHit,
  Share,
  Source,
  SourceKind,
  SourcePatch,
  Task,
  TaskPatch,
  Topic,
  WireEvent,
  WorkerStatus,
  ApiToken,
  AuthConfig,
  AuthMe,
  OAuthConsentContext,
  OnboardingPayload,
  OnboardingStatus,
  SafeUser,
  UserRole,
} from "@hive/shared";

// Vite proxies /api → hive-api in dev (see vite.config.ts).
// Identity is the authenticated user (v0.1.1) — set once auth resolves, read by
// the journal/inbox views. No more spoofable localStorage actor.
let currentUser: SafeUser | null = null;
export const setCurrentUser = (u: SafeUser | null) => {
  currentUser = u;
};
export const getCurrentUser = () => currentUser;
export const getActor = () => currentUser?.actor ?? "nate";

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

async function req<T>(path: string, init?: RequestInit, timeoutMs = 15000): Promise<T> {
  // Bound every call so a slow/cold API (e.g. just-restarted hive-api) can't hang
  // the UI indefinitely — the caller gets a rejection it can retry.
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(new Error("request timed out")), timeoutMs);
  try {
    const res = await fetch(`/api${path}`, {
      ...init,
      credentials: "include", // send the session cookie
      signal: ctrl.signal,
      headers: { "content-type": "application/json", ...init?.headers },
    });
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    return (res.status === 204 ? undefined : await res.json()) as T;
  } finally {
    clearTimeout(timer);
  }
}

export const api = {
  journal: (limit = 50, offset = 0) =>
    req<JournalEntryView[]>(`/journal?limit=${limit}&offset=${offset}`),
  journalScoped: (viewer: string, writers?: string[], limit = 50, offset = 0) => {
    const p = new URLSearchParams({ viewer, limit: String(limit), offset: String(offset) });
    if (writers && writers.length > 0) p.set("writers", writers.join(","));
    return req<JournalEntryView[]>(`/journal?${p}`);
  },
  journalWriters: (viewer: string) =>
    req<JournalWriter[]>(`/journal/writers?viewer=${encodeURIComponent(viewer)}`),
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
  // Trigger an immediate source poll (worker normally polls on a schedule).
  // The backend endpoint may not exist yet — callers should catch and fall
  // back to a plain wire refetch.
  pollSources: (id?: string) =>
    req<{ polled: number; ingested: number }>("/sources/poll", {
      method: "POST",
      body: JSON.stringify(id ? { id } : {}),
    }),
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
  patchPerson: (slug: string, patch: PersonPatch) =>
    req<Person>(`/people/${slug}`, { method: "PATCH", body: JSON.stringify(patch) }),

  // admin: actor delete-with-cascade + merge. dryRun returns the per-table blast
  // radius without mutating, so the UI can confirm before the real run.
  previewDeleteActor: (slug: string) =>
    req<ActorDeleteResult>(`/actors/${encodeURIComponent(slug)}?dryRun=1`, { method: "DELETE" }),
  deleteActor: (slug: string) =>
    req<ActorDeleteResult>(`/actors/${encodeURIComponent(slug)}`, { method: "DELETE" }),
  previewMergeActor: (slug: string, into: string) =>
    req<ActorMergeResult>(`/actors/${encodeURIComponent(slug)}/merge?dryRun=1`, {
      method: "POST",
      body: JSON.stringify({ into }),
    }),
  mergeActor: (slug: string, into: string) =>
    req<ActorMergeResult>(`/actors/${encodeURIComponent(slug)}/merge`, {
      method: "POST",
      body: JSON.stringify({ into }),
    }),

  topics: () => req<Topic[]>("/topics"),
  projects: () => req<Project[]>("/projects"),
  projectById: (id: string) =>
    req<Project & { tasks: Task[]; phases: Phase[] }>(`/projects/${id}`),

  createShare: (share: NewShare) =>
    req<Share>("/shares", { method: "POST", body: JSON.stringify(share) }),
  shares: (viewer: string) =>
    req<Share[]>(`/shares?viewer=${encodeURIComponent(viewer)}`),

  // ---- auth + onboarding (v0.1.1) ----
  onboardingStatus: () => req<OnboardingStatus>("/onboarding/status"),
  onboard: (p: OnboardingPayload) =>
    req<{ user: SafeUser }>("/onboarding", { method: "POST", body: JSON.stringify(p) }),
  login: (email: string, password: string) =>
    req<{ user: SafeUser }>("/auth/login", { method: "POST", body: JSON.stringify({ email, password }) }),
  logout: () => req<{ ok: boolean }>("/auth/logout", { method: "POST" }),
  me: () => req<AuthMe>("/auth/me"),
  authConfig: () => req<AuthConfig>("/auth/config"),

  // OAuth consent (AI identity grant). These hit /oauth/* (not under /api).
  oauthContext: (clientId: string) =>
    fetch(`/oauth/authorize/context?client_id=${encodeURIComponent(clientId)}`, { credentials: "include" }).then(
      async (r) => {
        if (!r.ok) throw new Error(String(r.status));
        return (await r.json()) as OAuthConsentContext;
      },
    ),
  oauthGrant: (body: {
    client_id: string;
    redirect_uri: string;
    code_challenge: string;
    state: string;
    scope: string;
    ai_actor: string;
    csrf: string;
  }) =>
    fetch("/oauth/authorize/grant", {
      method: "POST",
      credentials: "include",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }).then(async (r) => {
      if (!r.ok) throw new Error(`${r.status} ${await r.text()}`);
      return (await r.json()) as { redirect: string };
    }),

  // admin: users + API tokens
  users: () => req<SafeUser[]>("/users"),
  addUser: (u: { name: string; email: string; password: string; role?: UserRole; kind?: "human" | "ai" }) =>
    req<SafeUser>("/users", { method: "POST", body: JSON.stringify(u) }),
  apiTokens: () => req<ApiToken[]>("/tokens"),
  createToken: (actor: string, label: string, expiresInDays?: number) =>
    req<{ token: string; record: ApiToken }>("/tokens", {
      method: "POST",
      body: JSON.stringify({ actor, label, expiresInDays }),
    }),
  deleteToken: (id: string) => req<void>(`/tokens/${id}`, { method: "DELETE" }),

  // admin: bulk import from a legacy hive.db (SQLite). Multipart upload — we let the
  // browser set the content-type/boundary, so this bypasses the JSON `req` helper.
  importSqlite: async (file: File): Promise<ImportResult & { warnings: string[] }> => {
    const fd = new FormData();
    fd.append("db", file);
    const res = await fetch("/api/import/sqlite", { method: "POST", credentials: "include", body: fd });
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    return res.json() as Promise<ImportResult & { warnings: string[] }>;
  },
};
