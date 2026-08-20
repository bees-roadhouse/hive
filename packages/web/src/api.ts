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
  MailAccount,
  MailAccountAdminView,
  MailMailboxView,
  MailMessageSummary,
  MailThread,
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
  OAuthClientStatus,
  OnboardingPayload,
  OnboardingStatus,
  SafeUser,
  UserRole,
  CustomEntity,
  CustomEntityPatch,
  EntityTypePatch,
  EntityTypeView,
  NewCustomEntity,
  NewEntityType,
  Flow,
  FlowRun,
} from "@hive/shared";

// Vite proxies /api → hive-api in dev (see vite.config.ts).
// Identity is the authenticated user (v0.1.1) — set once auth resolves, read by
// the journal/inbox views. No more spoofable localStorage actor.
import { READS, idbClear, idbGet, idbPut } from "./idb.ts";
import {
  enqueue,
  parseConflictRow,
  setIdentityGetter,
  type OutboxKind,
  type OutboxOp,
} from "./outbox.ts";

let currentUser: SafeUser | null = null;
export const setCurrentUser = (u: SafeUser | null) => {
  currentUser = u;
};
export const getCurrentUser = () => currentUser;
export const getActor = () => currentUser?.actor ?? "nate";
setIdentityGetter(() => currentUser?.id);

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

// ---- offline layer (docs/DECISION-offline-conflict-model.md) ----
//
// Reads: network-first; every successful GET populates the IndexedDB read
// cache, and when the network is gone the cache answers instead. A cache, not
// local-first — no TTLs, no optimistic writes into it; SSE bumps still drive
// refetch exactly as before.
//
// Writes: the queueable surface below enqueues on a NETWORK failure (never on
// a 4xx, which is the server answering, not the network failing) and replay is
// the outbox's job. Creates mint their id here at call time — the first
// attempt already carries it, so a lost response followed by a replay can
// never double-land. Patches carry the base updated_at the caller read. An
// online 409 is the same conflict a replayed write hits, so it goes to the
// same human surface rather than throwing at the call site.

export const isOffline = (): boolean => !navigator.onLine;

/** Thrown (in place of a result) when a write was queued for later replay or
 *  parked as a conflict. Call sites catch it and treat the write as accepted. */
export class QueuedWrite extends Error {
  op: OutboxOp;
  constructor(op: OutboxOp) {
    super(`queued for replay: ${op.label}`);
    this.op = op;
  }
}
export const isQueuedWrite = (e: unknown): e is QueuedWrite => e instanceof QueuedWrite;

// The server mints ids as `{prefix}_{nanoid(12)}` (core/src/store/mod.rs);
// client-minted ids match that shape so server-side validation can pin the
// endpoint's namespace. The alphabet is nanoid's default 64-char url-safe set.
const NANOID_ALPHABET = "_-0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
function nanoid(size = 12): string {
  const bytes = crypto.getRandomValues(new Uint8Array(size));
  let id = "";
  for (let i = 0; i < size; i++) id += NANOID_ALPHABET[bytes[i] & 63];
  return id;
}

interface QueueRoute {
  kind: OutboxKind;
  /** Creates: the id namespace this endpoint owns. */
  idPrefix?: string;
  label: (path: string, body: Record<string, unknown> | undefined) => string;
}

const tail = (p: string) => p.split("/").pop() ?? p;
const snippet = (v: unknown, n = 48): string => {
  if (typeof v !== "string") return "";
  const oneLine = v.replace(/\s+/g, " ").trim();
  return oneLine.length > n ? `${oneLine.slice(0, n)}…` : oneLine;
};
const patchKeys = (b: Record<string, unknown> | undefined): string =>
  Object.keys(b ?? {}).filter((k) => k !== "base_updated_at" && k !== "id").join(", ") || "update";

// What the offline queue replays. Admin, mail, workspace, and credential
// writes are deliberately absent: they stay online-only per the decision doc
// (and secrets never sit in IndexedDB).
const QUEUE_ROUTES: { method: string; re: RegExp; route: QueueRoute }[] = [
  {
    method: "POST",
    re: /^\/journal$/,
    route: {
      kind: "create",
      idPrefix: "jrnl",
      label: (_p, b) => `journal entry — “${snippet(b?.body)}”`,
    },
  },
  {
    method: "PATCH",
    re: /^\/tasks\/[^/]+$/,
    route: { kind: "patch", label: (p, b) => `task ${tail(p)} — set ${patchKeys(b)}` },
  },
  {
    method: "PATCH",
    re: /^\/decisions\/[^/]+$/,
    route: { kind: "patch", label: (p, b) => `decision ${tail(p)} — set ${patchKeys(b)}` },
  },
  {
    method: "POST",
    re: /^\/inbox\/item\/[^/]+\/read$/,
    route: { kind: "action", label: (p) => `mark inbox item ${tail(p.split("/")[3] ?? p)} read` },
  },
  {
    method: "POST",
    re: /^\/inbox\/[^/]+\/read$/,
    route: { kind: "action", label: (p) => `mark all read for ${p.split("/")[2] ?? "inbox"}` },
  },
  {
    method: "POST",
    re: /^\/people$/,
    route: { kind: "create", idPrefix: "per", label: (_p, b) => `add person “${snippet(b?.name, 24)}”` },
  },
  {
    method: "PATCH",
    re: /^\/people\/[^/]+$/,
    route: { kind: "patch", label: (p, b) => `person ${tail(p)} — set ${patchKeys(b)}` },
  },
  {
    method: "POST",
    re: /^\/sources$/,
    route: { kind: "create", idPrefix: "src", label: (_p, b) => `add source “${snippet(b?.name, 24)}”` },
  },
  {
    method: "PATCH",
    re: /^\/sources\/[^/]+$/,
    route: { kind: "patch", label: (p, b) => `source ${tail(p)} — set ${patchKeys(b)}` },
  },
  {
    method: "DELETE",
    re: /^\/sources\/[^/]+$/,
    route: { kind: "delete", label: (p) => `delete source ${tail(p)}` },
  },
  {
    method: "POST",
    re: /^\/shares$/,
    route: { kind: "create", idPrefix: "shr", label: (_p, b) => `share with ${snippet(b?.viewer, 24)}` },
  },
  {
    method: "POST",
    re: /^\/entities$/,
    route: { kind: "create", idPrefix: "ent", label: (_p, b) => `add ${snippet(b?.type, 16)} “${snippet(b?.title, 24)}”` },
  },
  {
    method: "PATCH",
    re: /^\/entities\/[^/]+$/,
    route: { kind: "patch", label: (p, b) => `entity ${tail(p)} — set ${patchKeys(b)}` },
  },
  {
    method: "DELETE",
    re: /^\/entities\/[^/]+$/,
    route: { kind: "delete", label: (p) => `delete entity ${tail(p)}` },
  },
];

function matchQueueRoute(method: string, path: string): QueueRoute | null {
  // Query strings never appear on the write surface, but match on the bare
  // path so a future caller can't slip past the allowlist with one.
  const bare = path.split("?")[0];
  for (const r of QUEUE_ROUTES) {
    if (r.method === method && r.re.test(bare)) return r.route;
  }
  return null;
}

async function cacheGet<T>(path: string): Promise<T | undefined> {
  try {
    return await idbGet<T>(READS, path);
  } catch {
    return undefined;
  }
}

function cachePut(path: string, value: unknown): void {
  idbPut(READS, value, path).catch((e) => console.warn("read-cache put failed", e));
}

/** Logout hygiene: the cache is per-browser, so drop it when the session ends
 *  rather than leave one user's journal readable by the next. */
export async function clearReadCache(): Promise<void> {
  try {
    await idbClear(READS);
  } catch (e) {
    console.warn("read-cache clear failed", e);
  }
}

async function doFetch(path: string, init: RequestInit | undefined, timeoutMs: number): Promise<Response> {
  // Bound every call so a slow/cold API (e.g. just-restarted hive-api) can't hang
  // the UI indefinitely — the caller gets a rejection it can retry.
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(new Error("request timed out")), timeoutMs);
  try {
    return await fetch(`/api${path}`, {
      ...init,
      credentials: "include", // send the session cookie
      signal: ctrl.signal,
      headers: { "content-type": "application/json", ...init?.headers },
    });
  } finally {
    clearTimeout(timer);
  }
}

async function reqGet<T>(path: string, timeoutMs: number): Promise<T> {
  if (navigator.onLine) {
    let res: Response;
    try {
      res = await doFetch(path, undefined, timeoutMs);
    } catch (e) {
      // The network failed (or onLine lied): answer from the cache if we can.
      const hit = await cacheGet<T>(path);
      if (hit !== undefined) return hit;
      throw e;
    }
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    const data = (await res.json()) as T;
    cachePut(path, data);
    return data;
  }
  const hit = await cacheGet<T>(path);
  if (hit !== undefined) return hit;
  // Offline with nothing cached: try the wire anyway — onLine is advisory.
  const res = await doFetch(path, undefined, timeoutMs);
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  const data = (await res.json()) as T;
  cachePut(path, data);
  return data;
}

async function reqWrite<T>(path: string, init: RequestInit, timeoutMs: number): Promise<T> {
  const method = (init.method ?? "POST").toUpperCase();
  const route = matchQueueRoute(method, path);
  let bodyText = typeof init.body === "string" ? init.body : undefined;
  let bodyObj: Record<string, unknown> | undefined;
  if (bodyText) {
    try {
      bodyObj = JSON.parse(bodyText) as Record<string, unknown>;
    } catch {
      bodyObj = undefined;
    }
  }
  if (route?.idPrefix && bodyObj && bodyObj.id === undefined) {
    bodyObj.id = `${route.idPrefix}_${nanoid(12)}`;
    bodyText = JSON.stringify(bodyObj);
  }

  let res: Response;
  try {
    res = await doFetch(path, { ...init, body: bodyText }, timeoutMs);
  } catch (e) {
    // Network error or timeout — never an HTTP status: the server didn't
    // answer, so queue the write (if this endpoint is queueable) instead of
    // losing it. A 4xx/5xx below still throws normally.
    if (route) {
      const op = await enqueue({
        kind: route.kind,
        method,
        path,
        body: bodyObj,
        label: route.label(path, bodyObj),
      });
      throw new QueuedWrite(op);
    }
    throw e;
  }

  if (!res.ok) {
    const text = await res.text();
    if (route && res.status === 409) {
      // Base precondition failed while online — park it as a conflict with the
      // server's current row, exactly as a replayed write would surface.
      const op = await enqueue({
        kind: route.kind,
        method,
        path,
        body: bodyObj,
        label: route.label(path, bodyObj),
        state: "conflict",
        status: 409,
        current: parseConflictRow(text),
      });
      throw new QueuedWrite(op);
    }
    throw new Error(`${res.status} ${text}`);
  }
  return (res.status === 204 ? undefined : await res.json()) as T;
}

async function req<T>(path: string, init?: RequestInit, timeoutMs = 15000): Promise<T> {
  const method = (init?.method ?? "GET").toUpperCase();
  if (method === "GET") return reqGet(path, timeoutMs);
  return reqWrite(path, init ?? {}, timeoutMs);
}

export const api = {
  // `scope` narrows the feed to one memory namespace: a user slug, or "global"
  // for the continuous (un-owned) stream. Omitted = no namespace filter.
  journal: (limit = 50, offset = 0, scope?: string | null) => {
    const p = new URLSearchParams({ limit: String(limit), offset: String(offset) });
    if (scope) p.set("scope", scope);
    return req<JournalEntryView[]>(`/journal?${p}`);
  },
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

  search: (query: string, mode: "keyword" | "semantic" | "precision" = "keyword") =>
    req<SearchHit[]>(`/search?q=${encodeURIComponent(query)}&mode=${mode}`),
  mailAccounts: () => req<MailAccount[]>("/mail/accounts"),
  mailMessages: (q: { query?: string; account_id?: string } = {}) => {
    const p = new URLSearchParams();
    if (q.query) p.set("query", q.query);
    if (q.account_id) p.set("account_id", q.account_id);
    return req<MailMessageSummary[]>(`/mail/messages?${p}`);
  },
  mailThread: (threadId: string) => req<MailThread>(`/mail/thread/${encodeURIComponent(threadId)}`),
  // Account management (Settings). Connect is admin-only server-side.
  mailAccountsManage: () => req<MailAccountAdminView[]>("/mail/accounts/manage"),
  mailAccountConnect: (input: {
    address: string;
    jmap_url: string;
    username?: string;
    secret: string;
    owner?: string;
  }) => req<MailAccountAdminView>("/mail/accounts", { method: "POST", body: JSON.stringify(input) }),
  mailAccountDelete: (id: string) =>
    req<{ ok: boolean }>(`/mail/accounts/${encodeURIComponent(id)}`, { method: "DELETE" }),
  mailAccountSetEnabled: (id: string, enabled: boolean) =>
    req<{ ok: boolean; enabled: boolean }>(`/mail/accounts/${encodeURIComponent(id)}/enabled`, {
      method: "POST",
      body: JSON.stringify({ enabled }),
    }),
  mailAccountResync: (id: string) =>
    req<{ ok: boolean }>(`/mail/accounts/${encodeURIComponent(id)}/resync`, { method: "POST" }),
  mailMailboxes: (accountId: string) =>
    req<MailMailboxView[]>(`/mail/accounts/${encodeURIComponent(accountId)}/mailboxes`),
  mailMailboxSetIngest: (id: string, ingest: boolean) =>
    req<{ ok: boolean; ingest: boolean }>(`/mail/mailboxes/${encodeURIComponent(id)}/ingest`, {
      method: "POST",
      body: JSON.stringify({ ingest }),
    }),
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
  addPerson: (p: { name: string; kind?: "human" | "ai"; id?: string }) =>
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
    token_ttl_secs?: number;
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
  createToken: (actor: string, label: string, expiresInDays?: number, neverExpires = false) =>
    req<{ token: string; record: ApiToken }>("/tokens", {
      method: "POST",
      body: JSON.stringify({ actor, label, expiresInDays, neverExpires }),
    }),
  deleteToken: (id: string) => req<void>(`/tokens/${id}`, { method: "DELETE" }),

  // admin: connected OAuth apps — list clients with live token stats, revoke all
  // of a client's tokens (disconnects the app).
  oauthClients: () => req<OAuthClientStatus[]>("/oauth/clients"),
  revokeOAuthClient: (id: string) =>
    req<{ revoked: number }>(`/oauth/clients/${encodeURIComponent(id)}`, { method: "DELETE" }),

  // admin: bulk-reassign journal namespace ownership. Filters are ANDed; `to`
  // omitted/null makes matched entries global.
  reassignJournalScope: (body: {
    match_unscoped?: boolean;
    from_user?: string;
    author?: string;
    to?: string | null;
  }) =>
    req<{ changed: number }>("/journal/reassign-scope", {
      method: "POST",
      body: JSON.stringify(body),
    }),

  // admin: bulk import from a legacy hive.db (SQLite). Multipart upload — we let the
  // browser set the content-type/boundary, so this bypasses the JSON `req` helper.
  importSqlite: async (file: File): Promise<ImportResult & { warnings: string[] }> => {
    const fd = new FormData();
    fd.append("db", file);
    const res = await fetch("/api/import/sqlite", { method: "POST", credentials: "include", body: fd });
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    return res.json() as Promise<ImportResult & { warnings: string[] }>;
  },

  // ---- hosted Claude Code workspaces (hive → Claude Code) ----
  workspaces: (limit = 50) => req<CcSession[]>(`/workspaces?limit=${limit}`),
  workspace: (id: string) => req<CcSession>(`/workspaces/${id}`),
  createWorkspace: (input: { title?: string; runtime?: RuntimeKind | string; provider?: string; model?: string; prompt?: string; tags?: string[]; project?: string; linked_entities?: Array<{ kind: string; id: string; rel?: string }> }) =>
    req<CcSession>("/workspaces", { method: "POST", body: JSON.stringify(input) }),
  transcript: (id: string, after = 0, limit = 2000) =>
    req<CcMessage[]>(`/workspaces/${id}/messages?after=${after}&limit=${limit}`),
  sendInput: (id: string, text: string) =>
    req<CcMessage>(`/workspaces/${id}/input`, { method: "POST", body: JSON.stringify({ text }) }),
  archiveWorkspace: (id: string) =>
    req<{ ok: boolean }>(`/workspaces/${id}/archive`, { method: "POST" }),
  // Hard delete: transcript + conversation links go too; journal mirrors stay.
  deleteWorkspace: (id: string) =>
    req<{ ok: boolean }>(`/workspaces/${id}`, { method: "DELETE" }),

  // ---- flows (pluggable workflows — docs/FLOWS.md) ----
  flows: () => req<Flow[]>("/flows"),
  flow: (slug: string) => req<Flow>(`/flows/${slug}`),
  setFlowEnabled: (slug: string, enabled: boolean) =>
    req<Flow>(`/flows/${slug}`, { method: "PATCH", body: JSON.stringify({ enabled }) }),
  flowRuns: (slug: string, limit = 50) => req<FlowRun[]>(`/flows/${slug}/runs?limit=${limit}`),

  // ---- user-defined custom entity types ----
  entityTypes: (includeArchived = false) =>
    req<EntityTypeView[]>(`/entity-types${includeArchived ? "?include_archived=1" : ""}`),
  createEntityType: (input: NewEntityType) =>
    req<EntityTypeView>("/entity-types", { method: "POST", body: JSON.stringify(input) }),
  patchEntityType: (idOrSlug: string, patch: EntityTypePatch) =>
    req<EntityTypeView>(`/entity-types/${idOrSlug}`, { method: "PATCH", body: JSON.stringify(patch) }),
  deleteEntityType: (idOrSlug: string) =>
    req<void>(`/entity-types/${idOrSlug}`, { method: "DELETE" }),
  entities: (type: string, opts: { limit?: number; offset?: number; sort?: string; dir?: "asc" | "desc"; filters?: Record<string, string> } = {}) => {
    const p = new URLSearchParams({ type });
    if (opts.limit) p.set("limit", String(opts.limit));
    if (opts.offset) p.set("offset", String(opts.offset));
    if (opts.sort) p.set("sort", opts.sort);
    if (opts.dir) p.set("dir", opts.dir);
    for (const [k, v] of Object.entries(opts.filters ?? {})) if (v) p.set(`f.${k}`, v);
    return req<CustomEntity[]>(`/entities?${p}`);
  },
  entity: (id: string) => req<CustomEntity>(`/entities/${id}`),
  createEntity: (input: NewCustomEntity) =>
    req<CustomEntity>("/entities", { method: "POST", body: JSON.stringify(input) }),
  patchEntity: (id: string, patch: CustomEntityPatch) =>
    req<CustomEntity>(`/entities/${id}`, { method: "PATCH", body: JSON.stringify(patch) }),
  deleteEntity: (id: string) => req<void>(`/entities/${id}`, { method: "DELETE" }),

  // per-user Claude Code credentials (secret never returned)
  ccCredentials: () => req<CcCredentialView[]>("/cc-credentials"),
  saveCcCredential: (input: { kind: string; runtime?: RuntimeKind; provider?: string; label?: string; secret: string }) =>
    req<CcCredentialView>("/cc-credentials", { method: "POST", body: JSON.stringify(input) }),
  deleteCcCredential: (id: string) => req<void>(`/cc-credentials/${id}`, { method: "DELETE" }),
};

// ---- hosted Claude Code workspace types (kept local; mirror api/src/store) ----
export type RuntimeKind = "claude_code" | "codex" | "opencode";

export interface CcSession {
  id: string;
  owner: string;
  created_by: string;
  title: string;
  workdir: string;
  claude_session_id: string | null;
  runtime: RuntimeKind | string;
  status: string;
  /** 'hosted' (runner-driven) or 'captured' (SessionEnd ingest of a local
   *  session). Optional: the column lands in a parallel PR — absent = hosted. */
  origin?: string;
  model: string | null;
  usage: unknown;
  meta: unknown;
  repo_url: string | null;
  repo_ref: string | null;
  created_at: string;
  updated_at: string;
  last_activity_at: string | null;
}

export interface CcMessage {
  id: string;
  session_id: string;
  seq: number;
  role: string;
  kind: string;
  content: { text?: string; [k: string]: unknown };
  raw: unknown;
  tokens_in: number | null;
  tokens_out: number | null;
  created_at: string;
}

export interface CcCredentialView {
  id: string;
  owner: string;
  kind: string;
  runtime: RuntimeKind | string;
  provider: string | null;
  label: string;
  tail: string;
  created_at: string;
  last_used_at: string | null;
}
