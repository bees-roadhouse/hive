import { nanoid } from "nanoid";
import {
  type Anchor,
  type AnchorFields,
  type AutocompleteItem,
  type DashboardStats,
  type Decision,
  type DecisionPatch,
  type DecisionStatus,
  type EmbeddingStats,
  type EntityKind,
  type EventItem,
  type GraphData,
  type GraphNode,
  type InboxItem,
  type InboxReason,
  type JournalEntry,
  type JournalEntryView,
  type JournalRef,
  type JournalWriter,
  type Link,
  type NewAnchor,
  type NewJournalEntry,
  type NewShare,
  type NewSource,
  type OutboxJob,
  type OutboxStatus,
  type Person,
  type Phase,
  type PersonPatch,
  type Profile,
  type ProfilePatch,
  type ProfileSource,
  type Project,
  type ProjectRef,
  type RecallData,
  type RecallJournalHit,
  type RecallResult,
  type ResolvedAnchor,
  type SearchHit,
  type Severity,
  type Share,
  type Source,
  type SourcePatch,
  type Task,
  type TaskPatch,
  type TaskStatus,
  type Topic,
  type WireEvent,
  type WorkerStatus,
  type ApiToken,
  type OAuthClient,
  type OnboardingStatus,
  type SafeUser,
  type User,
  type UserRole,
  ACTORS,
  APP_VERSION,
  isAi,
  parseMentions,
  RECALL_DEFAULT_BUDGET,
  TASK_STATUSES,
  DECISION_STATUSES,
  API_TOKEN_MAX_EXPIRY_DAYS,
  API_TOKEN_DEFAULT_EXPIRY_DAYS,
  type LegacyImport,
  type ImportResult,
} from "@hive/shared";
import { db, tx } from "./db.ts";
import { publish } from "./bus.ts";
import {
  API_TOKEN_PREFIX,
  AUTH_CODE_PREFIX,
  AUTH_CODE_TTL_MS,
  generateToken,
  hashPassword,
  OAUTH_TOKEN_TTL_MS,
  SESSION_PREFIX,
  SESSION_TTL_MS,
  tokenHash,
  verifyPassword,
} from "./auth.ts";
import {
  contentHash,
  cosine,
  embed,
  embedQuery,
  EMBED_MODEL,
  fromBlob,
  rerank,
  RERANK_AVAILABLE,
  toBlob,
} from "./embed.ts";
import { parseFeed } from "./feed.ts";
import { parsePage } from "./scrape.ts";

const now = () => new Date().toISOString();
const id = (prefix: string) => `${prefix}_${nanoid(12)}`;
const json = <T>(s: string): T => JSON.parse(s) as T;
const snip = (s: string, n = 140) => (s.length > n ? `${s.slice(0, n)}…` : s);

/** lowercase, spaces→'-', strip non [a-z0-9-] */
const slugify = (s: string) =>
  s
    .toLowerCase()
    .replace(/\s+/g, "-")
    .replace(/[^a-z0-9-]/g, "");

// ---- search index helpers ----

function indexEntity(kind: string, refId: string, title: string, body: string, tags: string[] = []) {
  db.prepare("DELETE FROM search WHERE kind = ? AND ref_id = ?").run(kind, refId);
  db.prepare("INSERT INTO search (kind, ref_id, title, body) VALUES (?, ?, ?, ?)").run(
    kind,
    refId,
    title,
    `${body} ${tags.join(" ")}`,
  );
}

// ---- wire log ----

export function emit(kind: string, actor: string, payload: unknown): WireEvent {
  const ev: WireEvent = { id: id("wire"), kind, actor, payload, created_at: now() };
  db.prepare("INSERT INTO wire (id, kind, actor, payload, created_at) VALUES (?, ?, ?, ?, ?)").run(
    ev.id,
    ev.kind,
    ev.actor,
    JSON.stringify(ev.payload),
    ev.created_at,
  );
  // Fan out to SSE subscribers after the DB write succeeds.
  publish({ kind: ev.kind, actor: ev.actor, payload: ev.payload, at: ev.created_at });
  return ev;
}

export function wire(limit = 100): WireEvent[] {
  return db
    .prepare("SELECT * FROM wire ORDER BY created_at DESC LIMIT ?")
    .all(limit)
    .map((r) => {
      const row = r as Omit<WireEvent, "payload"> & { payload: string };
      return { ...row, payload: json(row.payload) };
    });
}

// ---- inbox ----

export const inbox = {
  add(
    recipient: string,
    from: string,
    reason: InboxReason,
    ref_kind: InboxItem["ref_kind"],
    ref_id: string,
    entry_id: string | null,
    snippet: string,
  ): InboxItem | null {
    if (recipient === from) return null; // don't notify yourself
    const item: InboxItem = {
      id: id("inb"),
      recipient,
      from,
      reason,
      ref_kind,
      ref_id,
      entry_id,
      snippet: snip(snippet),
      created_at: now(),
      read_at: null,
    };
    db.prepare(
      `INSERT INTO inbox (id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at)
       VALUES (@id, @recipient, @from, @reason, @ref_kind, @ref_id, @entry_id, @snippet, @created_at, @read_at)`,
    ).run(item);
    emit("inbox.delivered", from, { to: recipient, reason, ref_kind, ref_id });
    return item;
  },

  list(recipient: string, unreadOnly = false): InboxItem[] {
    const sql = `SELECT id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at, read_at
                 FROM inbox WHERE recipient = ?${unreadOnly ? " AND read_at IS NULL" : ""}
                 ORDER BY created_at DESC`;
    return db.prepare(sql).all(recipient) as InboxItem[];
  },

  markRead(itemId: string): boolean {
    return db.prepare("UPDATE inbox SET read_at = ? WHERE id = ? AND read_at IS NULL").run(now(), itemId)
      .changes > 0;
  },

  markAllRead(recipient: string): number {
    return db
      .prepare("UPDATE inbox SET read_at = ? WHERE recipient = ? AND read_at IS NULL")
      .run(now(), recipient).changes;
  },

  unreadCount(recipient: string): number {
    return (
      db
        .prepare("SELECT count(*) AS n FROM inbox WHERE recipient = ? AND read_at IS NULL")
        .get(recipient) as { n: number }
    ).n;
  },
};

// ---- projects ----

export const projects = {
  list: (): Project[] => db.prepare("SELECT * FROM projects ORDER BY name").all() as Project[],

  get(projectId: string): Project | undefined {
    return db.prepare("SELECT * FROM projects WHERE id = ?").get(projectId) as Project | undefined;
  },

  bySlug(slug: string): Project | undefined {
    return db.prepare("SELECT * FROM projects WHERE slug = ?").get(slug) as Project | undefined;
  },

  ensure(name: string): Project {
    const slug = slugify(name);
    const existing = db.prepare("SELECT * FROM projects WHERE slug = ?").get(slug) as Project | undefined;
    if (existing) return existing;
    const p: Project = { id: id("proj"), name, slug, created_at: now() };
    db.prepare("INSERT INTO projects (id, name, slug, created_at) VALUES (?, ?, ?, ?)").run(
      p.id, p.name, p.slug, p.created_at,
    );
    return p;
  },

  withChildren(projectId: string): Project & { tasks: Task[]; phases: Phase[] } | undefined {
    const p = projects.get(projectId);
    if (!p) return undefined;
    return {
      ...p,
      tasks: tasks.list({ project: projectId }),
      phases: phases.list(projectId),
    };
  },
};

// ---- people ----

export const people = {
  list: (): Person[] => db.prepare("SELECT * FROM people ORDER BY kind, slug").all() as Person[],

  get(idOrSlug: string): Person | undefined {
    return db.prepare("SELECT * FROM people WHERE slug = ? OR id = ?").get(idOrSlug, idOrSlug) as Person | undefined;
  },

  bySlug(slug: string): Person | undefined {
    return db.prepare("SELECT * FROM people WHERE slug = ?").get(slug) as Person | undefined;
  },

  /** AI identities a given human owns — the grantable set for OAuth consent. */
  aisOwnedBy(ownerSlug: string): Person[] {
    return db.prepare("SELECT * FROM people WHERE kind = 'ai' AND owner = ? ORDER BY slug").all(ownerSlug) as Person[];
  },

  ensure(name: string, kind: "human" | "ai" = "human"): Person {
    const slug = slugify(name);
    const existing = people.bySlug(slug);
    if (existing) return existing;
    const p: Person = { id: id("per"), name, slug, kind, owner: null, bio: null, role: null, created_at: now() };
    db.prepare(
      "INSERT INTO people (id, name, slug, kind, owner, bio, role, created_at) VALUES (@id, @name, @slug, @kind, @owner, @bio, @role, @created_at)",
    ).run(p);
    return p;
  },

  upsert(slug: string, name: string, kind: Person["kind"], owner: string | null = null): Person {
    const existing = people.bySlug(slug);
    if (existing) return existing;
    const p: Person = { id: id("per"), slug, name, kind, owner, bio: null, role: null, created_at: now() };
    db.prepare(
      "INSERT INTO people (id, slug, name, kind, owner, bio, role, created_at) VALUES (@id, @slug, @name, @kind, @owner, @bio, @role, @created_at)",
    ).run(p);
    return p;
  },

  create(input: { name: string; kind?: "human" | "ai" }, actor = "system"): Person {
    const p = people.ensure(input.name, input.kind ?? "human");
    emit("person.created", actor, { id: p.id, name: p.name, kind: p.kind });
    return p;
  },

  update(idOrSlug: string, patch: PersonPatch, actor = "system"): Person | undefined {
    const cur = people.get(idOrSlug);
    if (!cur) return undefined;
    const name = patch.name ?? cur.name;
    const kind = patch.kind ?? cur.kind;
    const owner = patch.owner !== undefined ? patch.owner : cur.owner;
    const bio = patch.bio !== undefined ? patch.bio : cur.bio;
    const role = patch.role !== undefined ? patch.role : cur.role;
    const slug = patch.name ? slugify(name) : cur.slug;
    db.prepare("UPDATE people SET name = ?, slug = ?, kind = ?, owner = ?, bio = ?, role = ? WHERE id = ?").run(
      name, slug, kind, owner, bio, role, cur.id,
    );
    // The profile card is the canonical identity store; mirror any bio/role edit
    // into it (as sections.bio / sections.role) so every writer (REST, MCP, UI)
    // converges on one source of truth. The column is kept for now (drop = later).
    if (patch.bio !== undefined || patch.role !== undefined) {
      const sections: Record<string, string> = {};
      if (patch.bio !== undefined) sections.bio = patch.bio ?? "";
      if (patch.role !== undefined) sections.role = patch.role ?? "";
      profiles.update(slug, { display_name: name, kind, sections }, actor);
    }
    const next: Person = { ...cur, name, slug, kind, owner, bio, role };
    emit("person.updated", actor, { id: cur.id, name, kind });
    return next;
  },
};

// ---- profile (mutable per-actor card; the durable-identity write target) ----

type ProfileRow = Omit<Profile, "body"> & { body: string };
const toProfile = (r: ProfileRow): Profile => {
  const parsed = json<{ sections?: Record<string, string> }>(r.body);
  return { ...r, body: { sections: parsed.sections ?? {} } };
};

export const profiles = {
  get(actor: string): Profile | undefined {
    const r = db.prepare("SELECT * FROM profile WHERE actor = ?").get(actor) as ProfileRow | undefined;
    return r ? toProfile(r) : undefined;
  },

  /** Deep-merge `sections` into body.sections (per-key replace), stamp updated_at,
   *  source='manual'. Creates the card on first write. */
  update(actor: string, patch: ProfilePatch, by = "system"): Profile {
    const cur = profiles.get(actor);
    const sections = { ...(cur?.body.sections ?? {}), ...(patch.sections ?? {}) };
    const next: Profile = {
      actor,
      kind: patch.kind ?? cur?.kind ?? (isAi(actor) ? "ai" : "human"),
      display_name: patch.display_name ?? cur?.display_name ?? "",
      body: { sections },
      source: "manual" as ProfileSource,
      derived_at: cur?.derived_at ?? null,
      updated_at: now(),
    };
    db.prepare(
      `INSERT INTO profile (actor, kind, display_name, body, source, derived_at, updated_at)
       VALUES (@actor, @kind, @display_name, @body, @source, @derived_at, @updated_at)
       ON CONFLICT(actor) DO UPDATE SET kind=excluded.kind, display_name=excluded.display_name,
         body=excluded.body, source=excluded.source, derived_at=excluded.derived_at, updated_at=excluded.updated_at`,
    ).run({ ...next, body: JSON.stringify(next.body) });
    emit("profile.updated", by, { actor, source: next.source });
    return next;
  },
};

/**
 * One-time reconciliation (#31 → #37): the profile card is the canonical identity
 * store now. Fold any legacy people.bio/role into each actor's card as
 * sections.bio / sections.role. Idempotent and non-destructive — only fills a
 * card section that's missing/blank, so a card the actor has since edited (or a
 * value already migrated) is never clobbered. The people columns are left intact
 * (dropping them is a separate follow-up). Safe to run on every boot.
 */
export function backfillIdentityCards(): number {
  const rows = db
    .prepare("SELECT slug, name, kind, bio, role FROM people WHERE bio IS NOT NULL OR role IS NOT NULL")
    .all() as { slug: string; name: string; kind: Person["kind"]; bio: string | null; role: string | null }[];
  let migrated = 0;
  for (const p of rows) {
    const card = profiles.get(p.slug);
    const sections: Record<string, string> = {};
    if (p.bio?.trim() && !card?.body.sections.bio?.trim()) sections.bio = p.bio.trim();
    if (p.role?.trim() && !card?.body.sections.role?.trim()) sections.role = p.role.trim();
    if (Object.keys(sections).length === 0) continue;
    profiles.update(
      p.slug,
      { display_name: card?.display_name || p.name, kind: p.kind, sections },
      "migration",
    );
    migrated++;
  }
  return migrated;
}

// ---- config (per-instance key/value, v0.1.1) ----

export const config = {
  get(key: string): string | undefined {
    return (db.prepare("SELECT value FROM config WHERE key = ?").get(key) as { value: string } | undefined)?.value;
  },
  set(key: string, value: string): void {
    db.prepare(
      "INSERT INTO config (key, value, updated_at) VALUES (?, ?, ?) " +
        "ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    ).run(key, value, now());
  },
  bool(key: string): boolean {
    return config.get(key) === "true";
  },
};

// ---- users (login accounts) ----
// A user authenticates with email + password and writes as their `actor`
// (a people.slug) — so the authenticated identity, not a spoofable header,
// drives the journal/inbox.

type UserRow = User & { password_hash: string };

const safeUser = (u: User): SafeUser => ({
  id: u.id,
  actor: u.actor,
  email: u.email,
  name: u.name,
  role: u.role,
});

export const users = {
  count: (): number => (db.prepare("SELECT COUNT(*) AS n FROM users").get() as { n: number }).n,
  list: (): SafeUser[] =>
    db.prepare("SELECT id, actor, email, name, role FROM users ORDER BY created_at").all() as SafeUser[],
  safe: safeUser,
  byEmail: (email: string): UserRow | undefined =>
    db.prepare("SELECT * FROM users WHERE email = ?").get(email.trim().toLowerCase()) as UserRow | undefined,
  byId: (uid: string): UserRow | undefined =>
    db.prepare("SELECT * FROM users WHERE id = ?").get(uid) as UserRow | undefined,

  create(
    input: { name: string; email: string; password: string; role?: UserRole; actor?: string; kind?: "human" | "ai" },
    by = "system",
  ): SafeUser {
    // Tie the account to a person row (the actor it writes as).
    const person = people.ensure(input.actor ?? input.name, input.kind ?? "human");
    const u: User = {
      id: id("usr"),
      actor: person.slug,
      email: input.email.trim().toLowerCase(),
      name: input.name,
      role: input.role ?? "member",
      created_at: now(),
      last_login_at: null,
    };
    db.prepare(
      "INSERT INTO users (id, actor, email, name, role, password_hash, created_at, last_login_at) " +
        "VALUES (@id, @actor, @email, @name, @role, @password_hash, @created_at, @last_login_at)",
    ).run({ ...u, password_hash: hashPassword(input.password) });
    emit("user.created", by, { id: u.id, actor: u.actor, role: u.role });
    return safeUser(u);
  },

  /** Verify credentials; on success stamp last_login_at and return the row. */
  authenticate(email: string, password: string): UserRow | undefined {
    const u = users.byEmail(email);
    if (!u || !verifyPassword(password, u.password_hash)) return undefined;
    db.prepare("UPDATE users SET last_login_at = ? WHERE id = ?").run(now(), u.id);
    return u;
  },
};

// ---- sessions (browser cookie auth) ----

export const sessions = {
  create(userId: string): string {
    const token = generateToken(SESSION_PREFIX);
    const ts = now();
    db.prepare(
      "INSERT INTO sessions (id, token_hash, user_id, created_at, expires_at, last_seen) VALUES (?, ?, ?, ?, ?, ?)",
    ).run(id("ses"), tokenHash(token), userId, ts, new Date(Date.now() + SESSION_TTL_MS).toISOString(), ts);
    return token;
  },
  /** Resolve a session cookie to its user, or undefined if missing/expired. */
  resolve(token: string): UserRow | undefined {
    const row = db.prepare("SELECT id, user_id, expires_at FROM sessions WHERE token_hash = ?").get(tokenHash(token)) as
      | { id: string; user_id: string; expires_at: string }
      | undefined;
    if (!row) return undefined;
    if (new Date(row.expires_at).getTime() < Date.now()) {
      db.prepare("DELETE FROM sessions WHERE id = ?").run(row.id);
      return undefined;
    }
    db.prepare("UPDATE sessions SET last_seen = ? WHERE id = ?").run(now(), row.id);
    return users.byId(row.user_id);
  },
  destroy(token: string): void {
    db.prepare("DELETE FROM sessions WHERE token_hash = ?").run(tokenHash(token));
  },
};

// ---- API tokens (programmatic clients: CLI, MCP, AI agents) ----

const TOKEN_COLS =
  "id, actor, label, created_by, created_at, last_used_at, kind, client_id, granted_by, expires_at, scope";

export const tokens = {
  list: (): ApiToken[] =>
    db.prepare(`SELECT ${TOKEN_COLS} FROM api_tokens ORDER BY created_at DESC`).all() as ApiToken[],

  /**
   * Mint a bearer token. `expiresInDays` is clamped to [1, API_TOKEN_MAX_EXPIRY_DAYS];
   * omitted → API_TOKEN_DEFAULT_EXPIRY_DAYS. The plaintext is returned once and never stored.
   */
  create(
    input: { actor: string; label: string; expiresInDays?: number | null },
    by = "system",
  ): { token: string; record: ApiToken } {
    const person = people.ensure(input.actor, isAi(input.actor) ? "ai" : "human");
    const token = generateToken(API_TOKEN_PREFIX);
    const requested = input.expiresInDays ?? API_TOKEN_DEFAULT_EXPIRY_DAYS;
    const days = Math.min(API_TOKEN_MAX_EXPIRY_DAYS, Math.max(1, Math.floor(requested)));
    const createdAt = now();
    const record: ApiToken = {
      id: id("tok"),
      actor: person.slug,
      label: input.label,
      created_by: by,
      created_at: createdAt,
      last_used_at: null,
      // Personal access token: no OAuth client binding, but it DOES expire per
      // the #35 expiry policy (clamped [1, MAX] days above).
      kind: "pat",
      client_id: null,
      granted_by: null,
      expires_at: new Date(Date.parse(createdAt) + days * 86_400_000).toISOString(),
      scope: null,
    };
    db.prepare(
      "INSERT INTO api_tokens (id, token_hash, actor, label, created_by, created_at, last_used_at, kind, expires_at) " +
        "VALUES (@id, @token_hash, @actor, @label, @created_by, @created_at, @last_used_at, @kind, @expires_at)",
    ).run({
      id: record.id,
      token_hash: tokenHash(token),
      actor: record.actor,
      label: record.label,
      created_by: by,
      created_at: record.created_at,
      last_used_at: null,
      kind: record.kind,
      expires_at: record.expires_at,
    });
    emit("token.created", by, { id: record.id, actor: record.actor, label: record.label, expires_at: record.expires_at });
    return { token, record };
  },

  /** Mint a long-lived OAuth access token (consent flow), bound to the AI actor,
   *  the granting human, and the client. Returns the plaintext once. */
  createOAuth(input: { actor: string; clientId: string; grantedBy: string; scope: string; label?: string }): {
    token: string;
    record: ApiToken;
  } {
    const token = generateToken(API_TOKEN_PREFIX);
    const record: ApiToken = {
      id: id("tok"),
      actor: input.actor,
      label: input.label ?? `oauth · ${input.clientId}`,
      created_by: input.grantedBy,
      created_at: now(),
      last_used_at: null,
      kind: "oauth",
      client_id: input.clientId,
      granted_by: input.grantedBy,
      expires_at: new Date(Date.now() + OAUTH_TOKEN_TTL_MS).toISOString(),
      scope: input.scope,
    };
    db.prepare(
      "INSERT INTO api_tokens (id, token_hash, actor, label, created_by, created_at, last_used_at, kind, client_id, granted_by, expires_at, scope) " +
        "VALUES (@id, @token_hash, @actor, @label, @created_by, @created_at, NULL, 'oauth', @client_id, @granted_by, @expires_at, @scope)",
    ).run({
      id: record.id,
      token_hash: tokenHash(token),
      actor: record.actor,
      label: record.label,
      created_by: record.created_by,
      created_at: record.created_at,
      client_id: record.client_id,
      granted_by: record.granted_by,
      expires_at: record.expires_at,
      scope: record.scope,
    });
    emit("token.granted", input.grantedBy, { id: record.id, actor: record.actor, client_id: input.clientId });
    return { token, record };
  },

  /** Resolve a bearer token to its actor (and stamp last_used), honoring expiry
   *  (expires_at NULL = legacy non-expiring token; past expiry → reject + reap). */
  resolve(token: string): string | undefined {
    const row = db.prepare("SELECT id, actor, expires_at FROM api_tokens WHERE token_hash = ?").get(tokenHash(token)) as
      | { id: string; actor: string; expires_at: string | null }
      | undefined;
    if (!row) return undefined;
    if (row.expires_at && Date.parse(row.expires_at) < Date.now()) {
      db.prepare("DELETE FROM api_tokens WHERE id = ?").run(row.id);
      return undefined;
    }
    db.prepare("UPDATE api_tokens SET last_used_at = ? WHERE id = ?").run(now(), row.id);
    return row.actor;
  },

  remove: (tokenId: string): boolean => db.prepare("DELETE FROM api_tokens WHERE id = ?").run(tokenId).changes > 0,

  /** Revoke every token minted by a given OAuth client_id (used on code replay). */
  revokeByClient: (clientId: string): number =>
    db.prepare("DELETE FROM api_tokens WHERE client_id = ?").run(clientId).changes,
};

// ---- OAuth 2.1 authorization server: clients + codes ----

export const oauthClients = {
  register(input: { client_name: string; redirect_uris: string[]; grant_types?: string[] }): OAuthClient {
    const client: OAuthClient = {
      client_id: id("oauthc"),
      client_name: input.client_name,
      redirect_uris: input.redirect_uris,
      grant_types: input.grant_types ?? ["authorization_code"],
      created_at: now(),
    };
    db.prepare("INSERT INTO oauth_clients (client_id, client_name, redirect_uris, grant_types, created_at) VALUES (?, ?, ?, ?, ?)").run(
      client.client_id,
      client.client_name,
      JSON.stringify(client.redirect_uris),
      JSON.stringify(client.grant_types),
      client.created_at,
    );
    return client;
  },
  get(clientId: string): OAuthClient | undefined {
    const r = db.prepare("SELECT * FROM oauth_clients WHERE client_id = ?").get(clientId) as
      | { client_id: string; client_name: string; redirect_uris: string; grant_types: string; created_at: string }
      | undefined;
    if (!r) return undefined;
    return { ...r, redirect_uris: json<string[]>(r.redirect_uris), grant_types: json<string[]>(r.grant_types) };
  },
  count: (): number => (db.prepare("SELECT COUNT(*) AS n FROM oauth_clients").get() as { n: number }).n,
};

interface AuthCode {
  client_id: string;
  redirect_uri: string;
  code_challenge: string;
  ai_actor: string;
  granted_by: string;
  scope: string;
}

export const oauthCodes = {
  create(input: AuthCode): string {
    const code = generateToken(AUTH_CODE_PREFIX);
    db.prepare(
      "INSERT INTO oauth_auth_codes (code_hash, client_id, redirect_uri, code_challenge, ai_actor, granted_by, scope, created_at, expires_at, used_at) " +
        "VALUES (@code_hash, @client_id, @redirect_uri, @code_challenge, @ai_actor, @granted_by, @scope, @created_at, @expires_at, NULL)",
    ).run({
      ...input,
      code_hash: tokenHash(code),
      created_at: now(),
      expires_at: new Date(Date.now() + AUTH_CODE_TTL_MS).toISOString(),
    });
    return code;
  },

  /** Single-use redemption under a transaction. Returns the bound grant, or a
   *  reason: 'replay' (already used — caller should revoke), 'expired', or
   *  undefined (unknown). Marks the code used on success. */
  redeem(code: string): { ok: true; grant: AuthCode } | { ok: false; reason: "replay" | "expired" | "unknown" } {
    db.prepare("DELETE FROM oauth_auth_codes WHERE expires_at < ?").run(now()); // opportunistic sweep
    return tx(() => {
      const r = db.prepare("SELECT * FROM oauth_auth_codes WHERE code_hash = ?").get(tokenHash(code)) as
        | (AuthCode & { expires_at: string; used_at: string | null })
        | undefined;
      if (!r) return { ok: false, reason: "unknown" } as const;
      if (r.used_at) return { ok: false, reason: "replay" } as const;
      if (new Date(r.expires_at).getTime() < Date.now()) return { ok: false, reason: "expired" } as const;
      db.prepare("UPDATE oauth_auth_codes SET used_at = ? WHERE code_hash = ?").run(now(), tokenHash(code));
      return {
        ok: true,
        grant: {
          client_id: r.client_id,
          redirect_uri: r.redirect_uri,
          code_challenge: r.code_challenge,
          ai_actor: r.ai_actor,
          granted_by: r.granted_by,
          scope: r.scope,
        },
      } as const;
    });
  },
};

// ---- Bulk historical import (legacy hive.db → this instance) ----

/**
 * Idempotent bulk import. Rows keep their original ids + timestamps; an id that
 * already exists is left untouched (INSERT OR IGNORE) and counted as skipped — so
 * re-running is safe. Unlike journal.append this does NOT fan out inbox/anchor/share
 * side effects (inappropriate for backfilling history); it only persists + indexes.
 */
export function importLegacy(payload: LegacyImport): ImportResult {
  const count = () => ({ inserted: 0, skipped: 0 });
  const res: ImportResult = { journal: count(), projects: count(), tasks: count(), links: count() };

  return tx(() => {
    for (const p of payload.projects ?? []) {
      const r = db
        .prepare("INSERT OR IGNORE INTO projects (id, name, slug, created_at) VALUES (@id, @name, @slug, @created_at)")
        .run(p);
      r.changes ? res.projects.inserted++ : res.projects.skipped++;
    }

    for (const e of payload.journal ?? []) {
      people.ensure(e.author, isAi(e.author) ? "ai" : "human");
      const r = db
        .prepare(
          "INSERT OR IGNORE INTO journal (id, author, body, tags, mentions, created_at) " +
            "VALUES (@id, @author, @body, @tags, @mentions, @created_at)",
        )
        .run({
          id: e.id,
          author: e.author,
          body: e.body,
          tags: JSON.stringify(e.tags ?? []),
          mentions: JSON.stringify(parseMentions(e.body)),
          created_at: e.created_at,
        });
      if (r.changes) {
        indexEntity("journal", e.id, `${e.author}: ${snip(e.body, 50)}`, e.body, e.tags ?? []);
        res.journal.inserted++;
      } else {
        res.journal.skipped++;
      }
    }

    for (const t of payload.tasks ?? []) {
      const status = TASK_STATUSES.includes(t.status as TaskStatus) ? t.status : "todo";
      const r = db
        .prepare(
          "INSERT OR IGNORE INTO tasks (id, project, title, body, status, priority, tags, assignees, due, created_at, updated_at) " +
            "VALUES (@id, @project, @title, @body, @status, @priority, @tags, @assignees, @due, @created_at, @updated_at)",
        )
        .run({
          id: t.id,
          project: t.project,
          title: t.title,
          body: t.body ?? "",
          status,
          priority: t.priority || "normal",
          tags: JSON.stringify(t.tags ?? []),
          assignees: JSON.stringify(t.assignees ?? []),
          due: t.due,
          created_at: t.created_at,
          updated_at: t.updated_at,
        });
      if (r.changes) {
        indexEntity("task", t.id, t.title, t.body ?? "", t.tags ?? []);
        res.tasks.inserted++;
      } else {
        res.tasks.skipped++;
      }
    }

    for (const l of payload.links ?? []) {
      const r = db
        .prepare(
          "INSERT OR IGNORE INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at) " +
            "VALUES (@id, @source_kind, @source_id, @target_kind, @target_id, @rel, @created_at)",
        )
        .run(l);
      r.changes ? res.links.inserted++ : res.links.skipped++;
    }

    return res;
  });
}

// ---- onboarding (first-run setup, v0.1.1) ----

export const onboarding = {
  /** Setup is required for a fresh install (flag false) OR any instance with no
   *  login account yet — the latter keeps a pre-0.1.1 DB (auth is new, so it
   *  has zero users) from bricking itself behind a login it can't satisfy. */
  required: (): boolean => !config.bool("onboarding.completed") || users.count() === 0,

  status: (): OnboardingStatus => ({
    completed: !onboarding.required(),
    instanceName: config.get("instance.name") ?? null,
    version: config.get("app.version") ?? APP_VERSION,
  }),

  /** Create the first admin + name the instance, then mark setup complete and
   *  return a session so the wizard logs the admin straight in. */
  complete(input: { instanceName: string; adminName: string; adminEmail: string; password: string }): {
    user: SafeUser;
    session: string;
  } {
    const admin = users.create(
      {
        name: input.adminName,
        email: input.adminEmail,
        password: input.password,
        role: "admin",
        actor: input.adminName,
        kind: "human",
      },
      "onboarding",
    );
    config.set("instance.name", input.instanceName);
    config.set("app.version", APP_VERSION);
    config.set("onboarding.completed", "true");
    const session = sessions.create(admin.id);
    emit("onboarding.completed", admin.actor, { instance: input.instanceName });
    return { user: admin, session };
  },
};

// ---- topics ----

export const topics = {
  list: (): Topic[] => db.prepare("SELECT * FROM topics ORDER BY name").all() as Topic[],

  get(topicId: string): Topic | undefined {
    return db.prepare("SELECT * FROM topics WHERE id = ?").get(topicId) as Topic | undefined;
  },

  bySlug(slug: string): Topic | undefined {
    return db.prepare("SELECT * FROM topics WHERE slug = ?").get(slug) as Topic | undefined;
  },

  ensure(name: string): Topic {
    const slug = slugify(name);
    const existing = db.prepare("SELECT * FROM topics WHERE slug = ?").get(slug) as Topic | undefined;
    if (existing) return existing;
    const t: Topic = { id: id("top"), name, slug, created_at: now() };
    db.prepare("INSERT INTO topics (id, name, slug, created_at) VALUES (?, ?, ?, ?)").run(
      t.id, t.name, t.slug, t.created_at,
    );
    return t;
  },
};

// ---- phases ----

export const phases = {
  list(projectId?: string): Phase[] {
    if (projectId) {
      return db
        .prepare("SELECT * FROM phases WHERE project = ? ORDER BY position, created_at")
        .all(projectId) as Phase[];
    }
    return db.prepare("SELECT * FROM phases ORDER BY project, position, created_at").all() as Phase[];
  },

  get(phaseId: string): Phase | undefined {
    return db.prepare("SELECT * FROM phases WHERE id = ?").get(phaseId) as Phase | undefined;
  },

  bySlug(slug: string, projectId: string): Phase | undefined {
    return db
      .prepare("SELECT * FROM phases WHERE project = ? AND name = ? COLLATE NOCASE")
      .get(projectId, slug.replace(/-/g, " ")) as Phase | undefined ??
      db.prepare("SELECT * FROM phases WHERE project = ? AND LOWER(REPLACE(name,' ','-')) = ?")
        .get(projectId, slug) as Phase | undefined;
  },

  ensure(projectId: string, name: string): Phase {
    const existing = db
      .prepare("SELECT * FROM phases WHERE project = ? AND LOWER(name) = LOWER(?)")
      .get(projectId, name) as Phase | undefined;
    if (existing) return existing;
    const pos = (
      db.prepare("SELECT COALESCE(MAX(position)+1, 0) AS n FROM phases WHERE project = ?").get(projectId) as { n: number }
    ).n;
    const ph: Phase = { id: id("ph"), project: projectId, name, position: pos, created_at: now() };
    db.prepare("INSERT INTO phases (id, project, name, position, created_at) VALUES (?, ?, ?, ?, ?)").run(
      ph.id, ph.project, ph.name, ph.position, ph.created_at,
    );
    return ph;
  },
};

// ---- shares ----

export const shares = {
  create(input: NewShare): Share {
    // Idempotent — ignore if the same (scope, ref, viewer) triple already exists.
    const existing = db
      .prepare("SELECT * FROM shares WHERE scope=? AND ref=? AND viewer=?")
      .get(input.scope, input.ref, input.viewer) as Share | undefined;
    if (existing) return existing;
    const s: Share = { id: id("shr"), scope: input.scope, ref: input.ref, viewer: input.viewer, created_at: now() };
    db.prepare(
      "INSERT INTO shares (id, scope, ref, viewer, created_at) VALUES (@id, @scope, @ref, @viewer, @created_at)",
    ).run(s);
    emit("share.created", "system", { scope: s.scope, ref: s.ref, viewer: s.viewer });
    return s;
  },

  forViewer(viewer: string): Share[] {
    return db.prepare("SELECT * FROM shares WHERE viewer=? ORDER BY created_at DESC").all(viewer) as Share[];
  },
};

// ---- visible journal (scoped to a viewer) ----

export function visibleJournal(opts: {
  viewer: string;
  writers?: string[];
  limit?: number;
  offset?: number;
}): JournalEntryView[] {
  const { viewer, writers, limit = 50, offset = 0 } = opts;

  // 1. Authors whose entries the viewer can see by ownership / relationship.
  //    a) The viewer themselves.
  //    b) AI people whose owner = viewer.
  //    c) AI people who have ≥1 entry referencing viewer (links target_kind='person'
  //       OR mentions contains viewer).
  const ownedAiSlugs = (
    db.prepare("SELECT slug FROM people WHERE kind='ai' AND owner=?").all(viewer) as { slug: string }[]
  ).map((r) => r.slug);

  // AI authors that referenced viewer via links (target_kind='person', target_id=viewer).
  const linkedAiSlugs = (
    db
      .prepare(
        `SELECT DISTINCT j.author FROM journal j
         JOIN links l ON l.source_kind='journal' AND l.source_id=j.id
         WHERE l.target_kind='person' AND l.target_id=?
           AND EXISTS (SELECT 1 FROM people WHERE slug=j.author AND kind='ai')`,
      )
      .all(viewer) as { author: string }[]
  ).map((r) => r.author);

  // AI authors that @mentioned viewer in any entry.
  const mentionedAiSlugs = (
    db
      .prepare(
        `SELECT DISTINCT j.author FROM journal j
         WHERE j.mentions LIKE ?
           AND EXISTS (SELECT 1 FROM people WHERE slug=j.author AND kind='ai')`,
      )
      .all(`%"${viewer}"%`) as { author: string }[]
  ).map((r) => r.author);

  const visibleAuthors = new Set<string>([
    viewer,
    ...ownedAiSlugs,
    ...linkedAiSlugs,
    ...mentionedAiSlugs,
  ]);

  // 2. Entry-level shares (scope='entry') for this viewer.
  const sharedEntryIds = (
    db
      .prepare("SELECT ref FROM shares WHERE scope='entry' AND viewer=?")
      .all(viewer) as { ref: string }[]
  ).map((r) => r.ref);

  // 3. Journal-level shares (scope='journal') give visibility into entire author streams.
  const journalSharedAuthors = (
    db
      .prepare("SELECT ref FROM shares WHERE scope='journal' AND viewer=?")
      .all(viewer) as { ref: string }[]
  ).map((r) => r.ref);
  for (const a of journalSharedAuthors) visibleAuthors.add(a);

  // 4. Entries where viewer is @mentioned.
  const mentionedEntryIds = (
    db
      .prepare("SELECT id FROM journal WHERE mentions LIKE ?")
      .all(`%"${viewer}"%`) as { id: string }[]
  ).map((r) => r.id);

  // Union the visible entry id set (from shares + mentions).
  const extraIds = new Set([...sharedEntryIds, ...mentionedEntryIds]);

  // 5. Optional writers filter: intersect with requested authors.
  const authorSet = writers && writers.length > 0
    ? new Set([...visibleAuthors].filter((a) => writers.includes(a)))
    : visibleAuthors;

  // 6. Build and run the query. SQLite doesn't support arrays natively, so we
  //    use IN with placeholders assembled from the sets.
  const authorList = [...authorSet];
  const extraList = [...extraIds].filter((eid) => {
    // For entries in extraIds, still apply the writers filter if present.
    if (!writers || writers.length === 0) return true;
    // We'll need to check author at query time — handled via subquery below.
    return true;
  });

  const authorPlaceholders = authorList.length > 0 ? authorList.map(() => "?").join(",") : "'__never__'";
  const extraPlaceholders = extraList.length > 0 ? extraList.map(() => "?").join(",") : "'__never__'";

  // writers filter on the extra-id path: if writers specified, only include
  // extra entries whose author is in the writers filter.
  const writersFilter = writers && writers.length > 0
    ? `AND j.author IN (${writers.map(() => "?").join(",")})`
    : "";

  const sql = `
    SELECT j.* FROM journal j
    WHERE (
      j.author IN (${authorPlaceholders})
      OR (j.id IN (${extraPlaceholders}) ${writersFilter})
    )
    ORDER BY j.created_at DESC
    LIMIT ? OFFSET ?
  `;

  const params: unknown[] = [
    ...authorList,
    ...extraList,
    ...(writers && writers.length > 0 ? writers : []),
    limit,
    offset,
  ];

  const rows = db.prepare(sql).all(...params) as (Omit<JournalEntry, "tags" | "mentions"> & {
    tags: string;
    mentions: string;
  })[];

  return rows.map((r) => ({
    ...r,
    tags: json(r.tags),
    mentions: json(r.mentions),
    anchors: anchorsFor(r.id),
    refs: refsFor(r.id),
  }));
}

/** Writers visible to a viewer: themselves + their AIs + related AIs. */
export function journalWriters(viewer: string): JournalWriter[] {
  // Reuse the same author discovery logic as visibleJournal.
  const ownedAiSlugs = (
    db.prepare("SELECT slug FROM people WHERE kind='ai' AND owner=?").all(viewer) as { slug: string }[]
  ).map((r) => r.slug);

  const linkedAiSlugs = (
    db
      .prepare(
        `SELECT DISTINCT j.author FROM journal j
         JOIN links l ON l.source_kind='journal' AND l.source_id=j.id
         WHERE l.target_kind='person' AND l.target_id=?
           AND EXISTS (SELECT 1 FROM people WHERE slug=j.author AND kind='ai')`,
      )
      .all(viewer) as { author: string }[]
  ).map((r) => r.author);

  const mentionedAiSlugs = (
    db
      .prepare(
        `SELECT DISTINCT j.author FROM journal j
         WHERE j.mentions LIKE ?
           AND EXISTS (SELECT 1 FROM people WHERE slug=j.author AND kind='ai')`,
      )
      .all(`%"${viewer}"%`) as { author: string }[]
  ).map((r) => r.author);

  const journalSharedAuthors = (
    db
      .prepare("SELECT ref FROM shares WHERE scope='journal' AND viewer=?")
      .all(viewer) as { ref: string }[]
  ).map((r) => r.ref);

  const slugSet = new Set<string>([
    viewer,
    ...ownedAiSlugs,
    ...linkedAiSlugs,
    ...mentionedAiSlugs,
    ...journalSharedAuthors,
  ]);

  const result: JournalWriter[] = [];
  for (const slug of slugSet) {
    const p = people.get(slug);
    if (p) result.push({ slug: p.slug, name: p.name, kind: p.kind, owner: p.owner });
    else {
      // Viewer may not be in people table yet — return a minimal record.
      result.push({ slug, name: slug, kind: "human", owner: null });
    }
  }
  return result.sort((a, b) => a.slug.localeCompare(b.slug));
}

// ---- structured entities (created internally from journal anchors) ----

type TaskRow = Omit<Task, "tags" | "assignees"> & { tags: string; assignees: string };
const toTask = (r: TaskRow): Task => ({ ...r, tags: json(r.tags), assignees: json(r.assignees) });

export const tasks = {
  list(filter: { status?: string; assignee?: string; project?: string; phase?: string } = {}): Task[] {
    const rows = db
      .prepare(
        "SELECT * FROM tasks ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
      )
      .all() as TaskRow[];
    return rows
      .map(toTask)
      .filter((t) => !filter.status || t.status === filter.status)
      .filter((t) => !filter.project || t.project === filter.project)
      .filter((t) => !filter.phase || t.phase === filter.phase)
      .filter((t) => !filter.assignee || t.assignees.includes(filter.assignee));
  },

  get(taskId: string): Task | undefined {
    const r = db.prepare("SELECT * FROM tasks WHERE id = ?").get(taskId) as TaskRow | undefined;
    return r ? toTask(r) : undefined;
  },

  create(input: Partial<Task> & { title: string }, actor = "system"): Task {
    // Only ensure-by-name when the project value is not already a known project id.
    if (input.project && !projects.get(input.project)) projects.ensure(input.project);
    const t: Task = {
      id: id("task"),
      title: input.title,
      body: input.body ?? "",
      status: (input.status as TaskStatus) ?? "todo",
      priority: input.priority ?? "normal",
      tags: input.tags ?? [],
      assignees: input.assignees ?? [],
      project: input.project ?? null,
      phase: input.phase ?? null,
      due: input.due ?? null,
      origin_entry_id: input.origin_entry_id ?? null,
      anchor_text: input.anchor_text ?? null,
      created_at: now(),
      updated_at: now(),
    };
    db.prepare(
      `INSERT INTO tasks (id, project, phase, due, title, body, status, priority, tags, assignees, origin_entry_id, anchor_text, created_at, updated_at)
       VALUES (@id, @project, @phase, @due, @title, @body, @status, @priority, @tags, @assignees, @origin_entry_id, @anchor_text, @created_at, @updated_at)`,
    ).run({ ...t, tags: JSON.stringify(t.tags), assignees: JSON.stringify(t.assignees) });
    indexEntity("task", t.id, t.title, t.body, t.tags);
    emit("task.created", actor, { id: t.id, title: t.title });
    return t;
  },

  update(taskId: string, patch: TaskPatch, actor = "system"): Task | undefined {
    const current = tasks.get(taskId);
    if (!current) return undefined;
    const next: Task = { ...current, ...patch, id: current.id, updated_at: now() };
    db.prepare(
      `UPDATE tasks SET title=@title, body=@body, status=@status, priority=@priority,
       tags=@tags, assignees=@assignees, updated_at=@updated_at WHERE id=@id`,
    ).run({ ...next, tags: JSON.stringify(next.tags), assignees: JSON.stringify(next.assignees) });
    indexEntity("task", next.id, next.title, next.body, next.tags);
    emit("task.updated", actor, { id: next.id, status: next.status });
    return next;
  },
};

type DecisionRow = Omit<Decision, "tags" | "assignees"> & { tags: string; assignees: string };
const toDecision = (r: DecisionRow): Decision => ({
  ...r,
  tags: json(r.tags),
  assignees: json(r.assignees),
});

export const decisions = {
  list(filter: { status?: string } = {}): Decision[] {
    const rows = db.prepare("SELECT * FROM decisions ORDER BY created_at DESC").all() as DecisionRow[];
    return rows.map(toDecision).filter((d) => !filter.status || d.status === filter.status);
  },

  get(decisionId: string): Decision | undefined {
    const r = db.prepare("SELECT * FROM decisions WHERE id = ?").get(decisionId) as
      | DecisionRow
      | undefined;
    return r ? toDecision(r) : undefined;
  },

  create(input: Partial<Decision> & { title: string; decision: string }, actor = "system"): Decision {
    if (input.project && !projects.get(input.project)) projects.ensure(input.project);
    const d: Decision = {
      id: id("dec"),
      title: input.title,
      context: input.context ?? "",
      decision: input.decision,
      consequences: input.consequences ?? "",
      status: (input.status as DecisionStatus) ?? "proposed",
      tags: input.tags ?? [],
      assignees: input.assignees ?? [],
      project: input.project ?? null,
      supersedes: input.supersedes ?? null,
      origin_entry_id: input.origin_entry_id ?? null,
      anchor_text: input.anchor_text ?? null,
      created_at: now(),
      updated_at: now(),
    };
    db.prepare(
      `INSERT INTO decisions (id, title, context, decision, consequences, status, tags, assignees,
         project, supersedes, origin_entry_id, anchor_text, created_at, updated_at)
       VALUES (@id, @title, @context, @decision, @consequences, @status, @tags, @assignees,
         @project, @supersedes, @origin_entry_id, @anchor_text, @created_at, @updated_at)`,
    ).run({ ...d, tags: JSON.stringify(d.tags), assignees: JSON.stringify(d.assignees) });
    indexEntity("decision", d.id, d.title, `${d.context} ${d.decision} ${d.consequences}`, d.tags);
    if (d.supersedes) {
      const prior = decisions.get(d.supersedes);
      if (prior) {
        db.prepare("UPDATE decisions SET status='superseded', updated_at=? WHERE id=?").run(
          now(),
          prior.id,
        );
        links.create("decision", d.id, "decision", prior.id, "supersedes", actor);
      }
    }
    emit("decision.created", actor, { id: d.id, title: d.title, status: d.status });
    return d;
  },

  update(decisionId: string, patch: DecisionPatch, actor = "system"): Decision | undefined {
    const current = decisions.get(decisionId);
    if (!current) return undefined;
    const next: Decision = { ...current, ...patch, id: current.id, updated_at: now() };
    db.prepare(
      `UPDATE decisions SET title=@title, context=@context, decision=@decision, consequences=@consequences,
       status=@status, tags=@tags, assignees=@assignees, updated_at=@updated_at WHERE id=@id`,
    ).run({ ...next, tags: JSON.stringify(next.tags), assignees: JSON.stringify(next.assignees) });
    indexEntity(
      "decision",
      next.id,
      next.title,
      `${next.context} ${next.decision} ${next.consequences}`,
      next.tags,
    );
    emit("decision.updated", actor, { id: next.id, status: next.status });
    return next;
  },
};

type EventRow = Omit<EventItem, "tags" | "assignees"> & { tags: string; assignees: string };
const toEvent = (r: EventRow): EventItem => ({
  ...r,
  tags: json(r.tags),
  assignees: json(r.assignees),
});

export const events = {
  list: (): EventItem[] =>
    (db.prepare("SELECT * FROM events ORDER BY COALESCE(at, created_at) DESC").all() as EventRow[]).map(
      toEvent,
    ),

  get(eventId: string): EventItem | undefined {
    const r = db.prepare("SELECT * FROM events WHERE id = ?").get(eventId) as EventRow | undefined;
    return r ? toEvent(r) : undefined;
  },

  create(input: Partial<EventItem> & { title: string }, actor = "system"): EventItem {
    const e: EventItem = {
      id: id("evt"),
      title: input.title,
      body: input.body ?? "",
      at: input.at ?? null,
      tags: input.tags ?? [],
      assignees: input.assignees ?? [],
      origin_entry_id: input.origin_entry_id ?? null,
      anchor_text: input.anchor_text ?? null,
      created_at: now(),
    };
    db.prepare(
      `INSERT INTO events (id, title, body, at, tags, assignees, origin_entry_id, anchor_text, created_at)
       VALUES (@id, @title, @body, @at, @tags, @assignees, @origin_entry_id, @anchor_text, @created_at)`,
    ).run({ ...e, tags: JSON.stringify(e.tags), assignees: JSON.stringify(e.assignees) });
    indexEntity("event", e.id, e.title, e.body, e.tags);
    emit("event.created", actor, { id: e.id, title: e.title });
    return e;
  },
};

const entityById = (kind: string, refId: string): Task | Decision | EventItem | null => {
  if (kind === "task") return tasks.get(refId) ?? null;
  if (kind === "decision") return decisions.get(refId) ?? null;
  if (kind === "event") return events.get(refId) ?? null;
  return null;
};

// ---- anchors ----

const anchorsFor = (entryId: string): ResolvedAnchor[] =>
  (db.prepare('SELECT id, entry_id, start, "end", text, kind, ref_id, created_at FROM anchors WHERE entry_id = ? ORDER BY start').all(
    entryId,
  ) as Anchor[]).map((a) => ({ ...a, entity: entityById(a.kind, a.ref_id) }));

/** Regex to find bracket tokens like [person: Maggie Bierly] */
const TOKEN_RE = /\[(person|topic|project|phase|task):([^\]]+)\]/g;

/** Resolve bracket tokens in a body string against the DB at read time. */
function refsFor(body: string): JournalRef[] {
  const refs: JournalRef[] = [];
  for (const m of body.matchAll(new RegExp(TOKEN_RE.source, "g"))) {
    const kind = m[1] as JournalRef["kind"];
    const rawName = m[2].trim();
    const start = m.index!;
    const end = start + m[0].length;
    let entity: { id: string; slug: string; name: string } | undefined;
    if (kind === "person") {
      entity = people.bySlug(slugify(rawName)) ?? undefined;
    } else if (kind === "topic") {
      entity = topics.bySlug(slugify(rawName)) ?? undefined;
    } else if (kind === "project") {
      entity = projects.bySlug(slugify(rawName)) ?? undefined;
    } else if (kind === "phase") {
      // phase resolution without a project context: find by name across all phases
      const ph = db
        .prepare("SELECT * FROM phases WHERE LOWER(name) = LOWER(?) LIMIT 1")
        .get(rawName) as Phase | undefined;
      if (ph) entity = { id: ph.id, slug: slugify(ph.name), name: ph.name };
    } else {
      // task — find the most recent task with matching title
      type TR = { id: string; title: string };
      const t = db
        .prepare("SELECT id, title FROM tasks WHERE LOWER(title) = LOWER(?) ORDER BY created_at DESC LIMIT 1")
        .get(rawName) as TR | undefined;
      if (t) entity = { id: t.id, slug: slugify(t.title), name: t.title };
    }
    if (entity) {
      refs.push({ kind, id: entity.id, slug: entity.slug, name: entity.name, start, end });
    }
  }
  return refs;
}

// ---- journal (write-only source of truth) ----

export const journal = {
  list(limit = 100, offset = 0): JournalEntryView[] {
    const rows = db
      .prepare("SELECT * FROM journal ORDER BY created_at DESC LIMIT ? OFFSET ?")
      .all(limit, offset) as (Omit<JournalEntry, "tags" | "mentions"> & { tags: string; mentions: string })[];
    return rows.map((r) => ({
      ...r,
      tags: json(r.tags),
      mentions: json(r.mentions),
      anchors: anchorsFor(r.id),
      refs: refsFor(r.body),
    }));
  },

  get(entryId: string): JournalEntryView | undefined {
    const r = db.prepare("SELECT * FROM journal WHERE id = ?").get(entryId) as
      | (Omit<JournalEntry, "tags" | "mentions"> & { tags: string; mentions: string })
      | undefined;
    if (!r) return undefined;
    return { ...r, tags: json(r.tags), mentions: json(r.mentions), anchors: anchorsFor(r.id), refs: refsFor(r.body) };
  },

  /**
   * The one write path. Persist immutable prose, then materialise each anchored
   * span into a structured entity and fan out inbox notifications.
   * Also parses inline [person:], [topic:], [project:], [phase:], [task:] tokens
   * to emerge/link entities and feed inboxes.
   */
  append(input: NewJournalEntry, actorOverride?: string): JournalEntryView {
    return tx(() => {
      const author = actorOverride ?? input.author;
      const mentions = parseMentions(input.body);
      const entry: JournalEntry = {
        id: id("jrnl"),
        author,
        body: input.body,
        tags: input.tags ?? [],
        mentions,
        created_at: now(),
      };
      db.prepare(
        "INSERT INTO journal (id, author, body, tags, mentions, created_at) VALUES (@id, @author, @body, @tags, @mentions, @created_at)",
      ).run({ ...entry, tags: JSON.stringify(entry.tags), mentions: JSON.stringify(entry.mentions) });
      indexEntity("journal", entry.id, `${author}: ${snip(input.body, 50)}`, input.body, entry.tags);

      const assignedMentions = new Set<string>();
      for (const a of input.anchors ?? []) {
        materialiseAnchor(entry, a, author, assignedMentions);
      }

      // Parse bracket tokens: emerge/link entities, fan to inboxes.
      parseBracketTokens(entry, author, assignedMentions);

      // Anyone @mentioned but not already pulled into an anchor gets a plain
      // "mention" inbox item — humans and AIs alike.
      for (const m of mentions) {
        if (!assignedMentions.has(m)) {
          inbox.add(m, author, "mention", "journal", entry.id, entry.id, input.body);
        }
      }

      // Auto-share: every @mentioned actor gets an entry-level share so the
      // entry is visible in their scoped journal view.
      for (const m of mentions) {
        if (m !== author) {
          shares.create({ scope: "entry", ref: entry.id, viewer: m });
        }
      }

      emit("journal.created", author, { id: entry.id, anchors: (input.anchors ?? []).length });
      return { ...entry, anchors: anchorsFor(entry.id), refs: refsFor(entry.body) };
    });
  },
};

function materialiseAnchor(
  entry: JournalEntry,
  a: NewAnchor,
  author: string,
  assignedMentions: Set<string>,
): void {
  const text = entry.body.slice(a.start, a.end).trim();
  if (!text) return;
  const f: AnchorFields = a.fields ?? {};
  const spanMentions = parseMentions(text);
  // Auto-assign to the entry author when no explicit assignees and no @mentions in the span.
  const rawAssignees = f.assignees ?? (spanMentions.length > 0 ? spanMentions : [author]);
  const assignees = rawAssignees.filter((x) => x !== author);
  const assigneesForTask = rawAssignees.length > 0 ? rawAssignees : [author];
  const title = (f.title ?? text.split(/[.\n]/)[0]).slice(0, 120).trim();

  let refId: string;
  let reason: InboxReason;
  if (a.kind === "task") {
    const t = tasks.create(
      {
        title,
        body: text,
        status: (f.status as TaskStatus) ?? "todo",
        priority: f.priority,
        tags: f.tags,
        assignees: assigneesForTask,
        project: f.project ?? null,
        origin_entry_id: entry.id,
        anchor_text: text,
      },
      author,
    );
    refId = t.id;
    reason = "assignment";
  } else if (a.kind === "decision") {
    const d = decisions.create(
      {
        title,
        context: f.context ?? "",
        decision: f.decision ?? text,
        consequences: f.consequences ?? "",
        status: (f.status as DecisionStatus) ?? "proposed",
        tags: f.tags,
        assignees,
        supersedes: f.supersedes ?? null,
        project: f.project ?? null,
        origin_entry_id: entry.id,
        anchor_text: text,
      },
      author,
    );
    refId = d.id;
    reason = "decision";
  } else {
    const e = events.create(
      {
        title,
        body: text,
        at: f.at ?? null,
        tags: f.tags,
        assignees,
        origin_entry_id: entry.id,
        anchor_text: text,
      },
      author,
    );
    refId = e.id;
    reason = "event";
  }

  db.prepare(
    'INSERT INTO anchors (id, entry_id, start, "end", text, kind, ref_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)',
  ).run(id("anc"), entry.id, a.start, a.end, text, a.kind, refId, now());
  links.create("journal", entry.id, a.kind, refId, "anchors", author);

  // For inbox delivery use the full assignee list (including author when auto-assigned).
  const inboxRecipients = a.kind === "task" ? assigneesForTask : assignees;
  for (const who of inboxRecipients) {
    assignedMentions.add(who);
    inbox.add(who, author, reason, a.kind, refId, entry.id, text);
  }
}

/**
 * Parse [person:], [topic:], [project:], [phase:], [task:] tokens from an entry body.
 * Find-or-create each entity, create a links row, and fan to inboxes where relevant.
 * Context tracking: if the entry mentions a [project:] and/or [phase:], any [task:]
 * that emerges is related to that project/phase.
 */
function parseBracketTokens(
  entry: JournalEntry,
  author: string,
  assignedMentions: Set<string>,
): void {
  // First pass: collect context (project + phase referenced in this entry)
  let contextProjectId: string | null = null;
  let contextPhaseId: string | null = null;

  for (const m of entry.body.matchAll(new RegExp(TOKEN_RE.source, "g"))) {
    const kind = m[1] as JournalRef["kind"];
    const rawName = m[2].trim();
    if (kind === "project") {
      const p = projects.ensure(rawName);
      contextProjectId = p.id;
    } else if (kind === "phase" && contextProjectId) {
      const ph = phases.ensure(contextProjectId, rawName);
      contextPhaseId = ph.id;
    }
  }

  // Second pass: process all tokens
  for (const m of entry.body.matchAll(new RegExp(TOKEN_RE.source, "g"))) {
    const kind = m[1] as JournalRef["kind"];
    const rawName = m[2].trim();

    if (kind === "person") {
      // Resolve against ACTORS first (known actors), then ensure as a people row.
      const slug = slugify(rawName);
      const actorMatch = ACTORS.find((a) => a.name === slug || slugify(a.name) === slug);
      const person = actorMatch
        ? people.ensure(actorMatch.name.charAt(0).toUpperCase() + actorMatch.name.slice(1), actorMatch.kind)
        : people.ensure(rawName);
      links.create("journal", entry.id, "person", person.id, "mentions", author);
      // Fan to inbox if this person is a known actor (same as @mention)
      if (actorMatch) {
        assignedMentions.add(actorMatch.name);
        inbox.add(actorMatch.name, author, "mention", "journal", entry.id, entry.id, entry.body);
      }

    } else if (kind === "topic") {
      const topic = topics.ensure(rawName);
      links.create("journal", entry.id, "topic", topic.id, "tagged", author);

    } else if (kind === "project") {
      const proj = projects.ensure(rawName);
      links.create("journal", entry.id, "project", proj.id, "about", author);

    } else if (kind === "phase") {
      const projId = contextProjectId;
      if (projId) {
        const ph = phases.ensure(projId, rawName);
        links.create("journal", entry.id, "phase", ph.id, "about", author);
      }

    } else if (kind === "task") {
      // Emerge a task anchored to this entry, auto-assigned to the author.
      const t = tasks.create(
        {
          title: rawName,
          body: "",
          assignees: [author],
          project: contextProjectId,
          phase: contextPhaseId,
          origin_entry_id: entry.id,
          anchor_text: rawName,
        },
        author,
      );
      links.create("journal", entry.id, "task", t.id, "anchors", author);
      // author is assigned; inbox.add silently skips self-notification (recipient===from)
      inbox.add(author, author, "assignment", "task", t.id, entry.id, rawName);
    }
  }
}

// ---- links (knowledge graph) ----

export const links = {
  create(
    source_kind: Link["source_kind"],
    source_id: string,
    target_kind: Link["target_kind"],
    target_id: string,
    rel = "relates",
    actor = "system",
  ): Link {
    const l: Link = {
      id: id("link"),
      source_kind,
      source_id,
      target_kind,
      target_id,
      rel,
      created_at: now(),
    };
    db.prepare(
      `INSERT INTO links (id, source_kind, source_id, target_kind, target_id, rel, created_at)
       VALUES (@id, @source_kind, @source_id, @target_kind, @target_id, @rel, @created_at)`,
    ).run(l);
    return l;
  },

  forEntity: (refId: string): Link[] =>
    db
      .prepare("SELECT * FROM links WHERE source_id = ? OR target_id = ? ORDER BY created_at DESC")
      .all(refId, refId) as Link[],
};

/** The whole knowledge graph: every linked entity as a node, every link as an
 * edge. Node titles are resolved from the entities themselves; an endpoint with
 * no resolvable title falls back to its id.
 *
 * Derived edges (computed at query time, not stored):
 * - chain: per-author consecutive journal entry pairs (chronological within author)
 * - project→task, project→phase, phase→task from column relationships
 */
export function graph(): GraphData {
  const rows = db
    .prepare("SELECT source_kind, source_id, target_kind, target_id, rel FROM links ORDER BY created_at")
    .all() as {
    source_kind: EntityKind;
    source_id: string;
    target_kind: EntityKind;
    target_id: string;
    rel: string;
  }[];
  const titleOf = new Map(embeddableItems().map((i) => [`${i.kind}:${i.id}`, i.title]));
  for (const p of people.list()) titleOf.set(`person:${p.id}`, p.name);
  for (const t of topics.list()) titleOf.set(`topic:${t.id}`, t.name);
  for (const p of projects.list()) titleOf.set(`project:${p.id}`, p.name);
  for (const ph of phases.list()) titleOf.set(`phase:${ph.id}`, ph.name);

  const nodes = new Map<string, GraphNode>();
  const addNode = (kind: EntityKind, refId: string) => {
    const key = `${kind}:${refId}`;
    if (!nodes.has(key)) nodes.set(key, { id: key, kind, title: titleOf.get(key) ?? refId });
  };
  const edges: { source: string; target: string; rel: string }[] = rows.map((r) => {
    addNode(r.source_kind, r.source_id);
    addNode(r.target_kind, r.target_id);
    return { source: `${r.source_kind}:${r.source_id}`, target: `${r.target_kind}:${r.target_id}`, rel: r.rel };
  });

  // Derived: per-author journal chain edges
  const journalRows = db
    .prepare("SELECT id, author FROM journal ORDER BY author, created_at ASC")
    .all() as { id: string; author: string }[];
  let prevAuthor: string | null = null;
  let prevId: string | null = null;
  for (const jr of journalRows) {
    if (jr.author === prevAuthor && prevId) {
      addNode("journal", prevId);
      addNode("journal", jr.id);
      edges.push({ source: `journal:${prevId}`, target: `journal:${jr.id}`, rel: "chain" });
    } else if (jr.author !== prevAuthor) {
      prevAuthor = jr.author;
    }
    prevId = jr.id;
  }

  // Derived: project→task and project→phase edges from column values
  for (const t of tasks.list()) {
    if (t.project) {
      addNode("project", t.project);
      addNode("task", t.id);
      edges.push({ source: `project:${t.project}`, target: `task:${t.id}`, rel: "has_task" });
    }
    if (t.phase) {
      addNode("phase", t.phase);
      addNode("task", t.id);
      edges.push({ source: `phase:${t.phase}`, target: `task:${t.id}`, rel: "has_task" });
    }
  }
  for (const ph of phases.list()) {
    addNode("project", ph.project);
    addNode("phase", ph.id);
    edges.push({ source: `project:${ph.project}`, target: `phase:${ph.id}`, rel: "has_phase" });
  }

  return { nodes: [...nodes.values()], edges };
}

// ---- search ----

/** Journal entry ids visible to a viewer — the permission boundary every read
 * (feed, search, entity reads) filters through. Mirrors visibleJournal's rule:
 * own entries + owned/related/mentioned AIs + shared entries + journal shares. */
export function visibleEntryIds(viewer: string): Set<string> {
  const col = (rows: unknown[], key: string) => (rows as Record<string, string>[]).map((r) => r[key]);
  const ownedAi = col(db.prepare("SELECT slug FROM people WHERE kind='ai' AND owner=?").all(viewer), "slug");
  const linkedAi = col(
    db
      .prepare(
        `SELECT DISTINCT j.author author FROM journal j
           JOIN links l ON l.source_kind='journal' AND l.source_id=j.id
          WHERE l.target_kind='person' AND l.target_id=?
            AND EXISTS (SELECT 1 FROM people WHERE slug=j.author AND kind='ai')`,
      )
      .all(viewer),
    "author",
  );
  const mentionedAi = col(
    db
      .prepare(
        `SELECT DISTINCT j.author author FROM journal j
          WHERE j.mentions LIKE ? AND EXISTS (SELECT 1 FROM people WHERE slug=j.author AND kind='ai')`,
      )
      .all(`%"${viewer}"%`),
    "author",
  );
  const journalShared = col(db.prepare("SELECT ref FROM shares WHERE scope='journal' AND viewer=?").all(viewer), "ref");
  const authors = new Set<string>([viewer, ...ownedAi, ...linkedAi, ...mentionedAi, ...journalShared]);

  const ids = new Set<string>([
    ...col(db.prepare("SELECT ref FROM shares WHERE scope='entry' AND viewer=?").all(viewer), "ref"),
    ...col(db.prepare("SELECT id FROM journal WHERE mentions LIKE ?").all(`%"${viewer}"%`), "id"),
  ]);
  if (authors.size) {
    const ph = [...authors].map(() => "?").join(",");
    for (const r of db.prepare(`SELECT id FROM journal WHERE author IN (${ph})`).all(...authors) as { id: string }[]) {
      ids.add(r.id);
    }
  }
  return ids;
}

const ORIGIN_TABLE: Record<string, string> = { task: "tasks", decision: "decisions", event: "events" };

/** Drop search hits a viewer can't see: a journal entry they can read, or an
 * entity that emerged from one. (bookstack-mcp does the equivalent ACL filter.) */
function scopeHits(hits: SearchHit[], viewer: string): SearchHit[] {
  const visible = visibleEntryIds(viewer);
  return hits.filter((h) => {
    if (h.kind === "journal") return visible.has(h.id);
    const table = ORIGIN_TABLE[h.kind];
    if (!table) return false;
    const r = db.prepare(`SELECT origin_entry_id FROM ${table} WHERE id = ?`).get(h.id) as
      | { origin_entry_id: string | null }
      | undefined;
    return r?.origin_entry_id ? visible.has(r.origin_entry_id) : false;
  });
}

export function search(query: string, limit = 25, viewer?: string): SearchHit[] {
  if (!query.trim()) return [];
  // Over-fetch when scoping so permission filtering doesn't starve the result.
  const fetch = viewer ? limit * 5 : limit;
  const rows = db
    .prepare(
      `SELECT kind, ref_id, title, snippet(search, 3, '[', ']', '…', 12) AS snip, bm25(search) AS rank
       FROM search WHERE search MATCH ? ORDER BY rank LIMIT ?`,
    )
    .all(toMatchQuery(query), fetch) as {
    kind: SearchHit["kind"];
    ref_id: string;
    title: string;
    snip: string;
    rank: number;
  }[];
  const hits = rows.map((r) => ({
    kind: r.kind,
    id: r.ref_id,
    title: r.title,
    snippet: r.snip,
    score: Math.round((1 / (1 + Math.abs(r.rank))) * 1000) / 1000,
  }));
  return (viewer ? scopeHits(hits, viewer) : hits).slice(0, limit);
}

function toMatchQuery(q: string): string {
  return q
    .split(/\s+/)
    .filter(Boolean)
    .map((term) => `${term.replace(/[^\p{L}\p{N}]/gu, "")}*`)
    .filter((t) => t.length > 1)
    .join(" ");
}

/** Typeahead autocomplete: matching people, open tasks, projects, topics, phases. */
export function autocomplete(q: string, kinds?: string[]): AutocompleteItem[] {
  const lower = q.toLowerCase();
  const want = kinds ?? ["person", "task", "project", "topic", "phase"];
  const results: AutocompleteItem[] = [];

  if (want.includes("person")) {
    for (const p of people.list()) {
      if (p.name.toLowerCase().includes(lower)) {
        results.push({ kind: "person", id: p.id, slug: p.slug, label: p.name });
      }
    }
  }
  if (want.includes("project")) {
    for (const p of projects.list()) {
      if (p.name.toLowerCase().includes(lower)) {
        results.push({ kind: "project", id: p.id, slug: p.slug, label: p.name });
      }
    }
  }
  if (want.includes("topic")) {
    for (const t of topics.list()) {
      if (t.name.toLowerCase().includes(lower)) {
        results.push({ kind: "topic", id: t.id, slug: t.slug, label: t.name });
      }
    }
  }
  if (want.includes("phase")) {
    for (const ph of phases.list()) {
      if (ph.name.toLowerCase().includes(lower)) {
        results.push({ kind: "phase", id: ph.id, slug: slugify(ph.name), label: ph.name });
      }
    }
  }
  if (want.includes("task")) {
    for (const t of tasks.list({ status: "todo" }).concat(tasks.list({ status: "doing" }))) {
      if (t.title.toLowerCase().includes(lower)) {
        results.push({ kind: "task", id: t.id, slug: slugify(t.title), label: t.title });
      }
    }
  }

  return results.slice(0, 8);
}

/** Ensure the 5 known actors exist as people rows. Safe to call multiple times. */
export function seedActors(): void {
  const FULL_NAMES: Record<string, string> = {
    nate: "Nate Smith",
    maggie: "Maggie Bierly",
    pia: "Pia (Apiara)",
    apis: "Apis",
    cera: "Cera",
  };
  for (const a of ACTORS) {
    // AIs default to nate's ownership so his journal view surfaces their activity.
    people.upsert(a.name, FULL_NAMES[a.name] ?? a.name, a.kind, a.kind === "ai" ? "nate" : null);
  }
}

// ---- dashboard ----

export function dashboard(): DashboardStats {
  const count = (sql: string, ...args: unknown[]) =>
    (db.prepare(sql).get(...args) as { n: number }).n;

  const taskStats = { total: count("SELECT count(*) n FROM tasks") } as DashboardStats["tasks"];
  for (const s of TASK_STATUSES) taskStats[s] = count("SELECT count(*) n FROM tasks WHERE status=?", s);

  const decStats = { total: count("SELECT count(*) n FROM decisions") } as DashboardStats["decisions"];
  for (const s of DECISION_STATUSES)
    decStats[s] = count("SELECT count(*) n FROM decisions WHERE status=?", s);

  const byAuthor = db
    .prepare("SELECT author, count(*) AS entries FROM journal GROUP BY author ORDER BY entries DESC")
    .all() as { author: string; entries: number }[];

  const inboxStats = ACTORS.map((a) => ({
    recipient: a.name,
    kind: a.kind,
    unread: count('SELECT count(*) n FROM inbox WHERE recipient=? AND read_at IS NULL', a.name),
    total: count('SELECT count(*) n FROM inbox WHERE recipient=?', a.name),
  }));

  // Open tasks with a due date (for calendar overlay).
  type TaskDueRow = { id: string; title: string; due: string; status: string; assignees: string };
  const tasksWithDue = (
    db.prepare(
      "SELECT id, title, due, status, assignees FROM tasks WHERE due IS NOT NULL AND status != 'done' ORDER BY due ASC",
    ).all() as TaskDueRow[]
  ).map((r) => ({
    id: r.id,
    title: r.title,
    due: r.due,
    status: r.status as TaskStatus,
    assignees: json<string[]>(r.assignees),
  }));

  // Entry counts per day for last 30 days (SQLite substr gives YYYY-MM-DD).
  const entriesByDay = db
    .prepare(
      `SELECT substr(created_at, 1, 10) AS day, count(*) AS count
       FROM journal
       WHERE created_at >= datetime('now', '-30 days')
       GROUP BY day ORDER BY day ASC`,
    )
    .all() as { day: string; count: number }[];

  // Entry counts per author (total — for the author bar chart).
  const entriesByAuthor = db
    .prepare("SELECT author, count(*) AS count FROM journal GROUP BY author ORDER BY count DESC")
    .all() as { author: string; count: number }[];

  // Callouts: how often each person is referenced via links (target_kind='person').
  type CalloutRow = { target_id: string; count: number };
  const calloutRows = db
    .prepare(
      `SELECT target_id, count(*) AS count FROM links WHERE target_kind = 'person'
       GROUP BY target_id ORDER BY count DESC`,
    )
    .all() as CalloutRow[];
  const calloutsByPerson = calloutRows
    .map((r) => {
      const p = people.get(r.target_id);
      return p ? { name: p.name, slug: p.slug, count: r.count } : null;
    })
    .filter((x): x is { name: string; slug: string; count: number } => x !== null);

  return {
    entries: count("SELECT count(*) n FROM journal"),
    events: count("SELECT count(*) n FROM events"),
    tasks: taskStats,
    decisions: decStats,
    inbox: inboxStats,
    byAuthor,
    recent: wire(12),
    tasksWithDue,
    entriesByDay,
    entriesByAuthor,
    calloutsByPerson,
  };
}

// ============================================================================
// Worker surface: sources, outbox, embeddings, ingestion, status.
// ============================================================================

type SourceRow = Omit<Source, "enabled"> & { enabled: number };
const toSource = (r: SourceRow): Source => ({ ...r, enabled: !!r.enabled });

export const sources = {
  /**
   * List sources. If `owner` is provided, returns global (owner=null) + that actor's.
   * Omit to get all sources (worker uses this path).
   */
  list(owner?: string): Source[] {
    const all = (db.prepare("SELECT * FROM sources ORDER BY created_at").all() as SourceRow[]).map(toSource);
    if (owner === undefined) return all;
    return all.filter((s) => s.owner === null || s.owner === owner);
  },

  get(sourceId: string): Source | undefined {
    const r = db.prepare("SELECT * FROM sources WHERE id = ?").get(sourceId) as SourceRow | undefined;
    return r ? toSource(r) : undefined;
  },

  create(input: NewSource, actor = "system"): Source {
    const s: Source = {
      id: id("src"),
      name: input.name,
      url: input.url,
      kind: input.kind ?? "rss",
      category: input.category ?? null,
      severity: input.severity ?? "info",
      interval_secs: input.interval_secs ?? 900,
      notify: input.notify ?? null,
      enabled: input.enabled ?? true,
      owner: input.owner ?? null,
      last_polled_at: null,
      last_status: null,
      created_at: now(),
    };
    db.prepare(
      `INSERT INTO sources (id, name, url, kind, category, severity, interval_secs, notify, enabled, owner, last_polled_at, last_status, created_at)
       VALUES (@id, @name, @url, @kind, @category, @severity, @interval_secs, @notify, @enabled, @owner, @last_polled_at, @last_status, @created_at)`,
    ).run({ ...s, enabled: s.enabled ? 1 : 0 });
    emit("source.added", actor, { id: s.id, name: s.name, url: s.url });
    return s;
  },

  update(sourceId: string, patch: SourcePatch, actor = "system"): Source | undefined {
    const cur = sources.get(sourceId);
    if (!cur) return undefined;
    const next: Source = { ...cur, ...patch, id: cur.id };
    db.prepare(
      `UPDATE sources SET name=@name, url=@url, kind=@kind, category=@category, severity=@severity,
       interval_secs=@interval_secs, notify=@notify, enabled=@enabled, owner=@owner WHERE id=@id`,
    ).run({ ...next, enabled: next.enabled ? 1 : 0 });
    emit("source.updated", actor, { id: next.id });
    return next;
  },

  remove(sourceId: string, actor = "system"): boolean {
    const ok = db.prepare("DELETE FROM sources WHERE id = ?").run(sourceId).changes > 0;
    if (ok) emit("source.removed", actor, { id: sourceId });
    return ok;
  },

  /** Enabled sources whose poll interval has elapsed. */
  due(): Source[] {
    const t = Date.now();
    return sources
      .list()
      .filter((s) => s.enabled)
      .filter((s) => !s.last_polled_at || t - new Date(s.last_polled_at).getTime() >= s.interval_secs * 1000);
  },

  markPolled(sourceId: string, status: string): void {
    db.prepare("UPDATE sources SET last_polled_at = ?, last_status = ? WHERE id = ?").run(
      now(),
      status,
      sourceId,
    );
  },
};

/** Ingest fetched feed items into wire events (deduped by guid). */
export function ingest(
  source: Source,
  items: { guid: string; title: string; url?: string; body?: string }[],
): number {
  let added = 0;
  for (const it of items) {
    const dupe = db
      .prepare("SELECT 1 FROM wire WHERE kind = 'feed.item' AND payload LIKE ? LIMIT 1")
      .get(`%${JSON.stringify(it.guid).slice(1, -1)}%`);
    if (dupe) continue;
    emit("feed.item", source.name, {
      guid: it.guid,
      title: it.title,
      url: it.url ?? null,
      body: it.body ?? "",
      source: source.name,
      category: source.category,
      severity: source.severity,
    });
    if (source.notify) {
      inbox.add(source.notify, source.name, "mention", "journal", source.id, null, `${source.name}: ${it.title}`);
    }
    added++;
  }
  return added;
}

/**
 * Poll feed/scrape sources into wire events. The single implementation shared by
 * the worker's tick loop and the on-demand `POST /api/sources/poll` route.
 * With no `id`, polls every due+enabled source (the worker path); with an `id`,
 * polls that one source if it's enabled (the "refresh now" path), ignoring its
 * interval. Per-source failures are recorded via markPolled, never thrown, so one
 * bad feed can't abort the batch. Emits feed.item/scrape.item (→ SSE) via ingest.
 */
export async function pollSources(opts: { id?: string } = {}): Promise<{ polled: number; ingested: number }> {
  let targets: Source[];
  if (opts.id) {
    const s = sources.get(opts.id);
    targets = s && s.enabled ? [s] : [];
  } else {
    targets = sources.due();
  }

  let polled = 0;
  let ingested = 0;
  for (const source of targets) {
    polled++;
    try {
      const ctrl = new AbortController();
      const t = setTimeout(() => ctrl.abort(), 10_000);
      const res = await fetch(source.url, { signal: ctrl.signal });
      clearTimeout(t);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      if (source.kind === "scrape") {
        const items = parsePage(await res.text(), source.url);
        const count = ingestScrape(source, items);
        ingested += count;
        sources.markPolled(source.id, `ok · ${count} new of ${items.length} items`);
      } else {
        const items = parseFeed(await res.text());
        ingested += ingest(source, items);
        sources.markPolled(source.id, `ok · ${items.length} items`);
      }
    } catch (err) {
      sources.markPolled(source.id, `error · ${(err as Error).message}`);
    }
  }
  return { polled, ingested };
}

/** Ingest scraped page items into wire events (deduped by guid = resolved URL). */
export function ingestScrape(
  source: Source,
  items: { guid: string; title: string; url: string }[],
): number {
  let added = 0;
  for (const it of items) {
    const dupe = db
      .prepare("SELECT 1 FROM wire WHERE kind = 'scrape.item' AND payload LIKE ? LIMIT 1")
      .get(`%${JSON.stringify(it.guid).slice(1, -1)}%`);
    if (dupe) continue;
    emit("scrape.item", source.name, {
      guid: it.guid,
      title: it.title,
      url: it.url,
      source: source.name,
      category: source.category,
      severity: source.severity,
    });
    if (source.notify) {
      inbox.add(source.notify, source.name, "mention", "journal", source.id, null, `${source.name}: ${it.title}`);
    }
    added++;
  }
  return added;
}

// ---- outbox ----

const toJob = (r: Omit<OutboxJob, "payload"> & { payload: string }): OutboxJob => ({
  ...r,
  payload: json(r.payload),
});

export const outbox = {
  enqueue(kind: string, payload: unknown, runAfter = now(), actor = "system"): OutboxJob {
    const job: OutboxJob = {
      id: id("out"),
      kind,
      payload,
      status: "pending",
      attempts: 0,
      last_error: null,
      run_after: runAfter,
      created_at: now(),
      completed_at: null,
    };
    db.prepare(
      `INSERT INTO outbox (id, kind, payload, status, attempts, last_error, run_after, created_at, completed_at)
       VALUES (@id, @kind, @payload, @status, @attempts, @last_error, @run_after, @created_at, @completed_at)`,
    ).run({ ...job, payload: JSON.stringify(job.payload) });
    emit("outbox.enqueued", actor, { id: job.id, kind });
    return job;
  },

  list: (limit = 50): OutboxJob[] =>
    (db.prepare("SELECT * FROM outbox ORDER BY created_at DESC LIMIT ?").all(limit) as any[]).map(toJob),

  claim(limit = 10): OutboxJob[] {
    const rows = db
      .prepare("SELECT * FROM outbox WHERE status = 'pending' AND run_after <= ? ORDER BY run_after LIMIT ?")
      .all(now(), limit) as any[];
    return rows.map(toJob);
  },

  complete(jobId: string): void {
    db.prepare("UPDATE outbox SET status='done', completed_at=? WHERE id=?").run(now(), jobId);
  },

  fail(jobId: string, error: string, attempts: number): void {
    const backoffSecs = Math.min(3600, 2 ** attempts * 30);
    const runAfter = new Date(Date.now() + backoffSecs * 1000).toISOString();
    const status: OutboxStatus = attempts >= 5 ? "failed" : "pending";
    db.prepare("UPDATE outbox SET status=?, attempts=?, last_error=?, run_after=? WHERE id=?").run(
      status,
      attempts,
      error,
      runAfter,
      jobId,
    );
  },

  counts: () =>
    ["pending", "done", "failed"].reduce(
      (acc, s) => {
        acc[s as OutboxStatus] = (
          db.prepare("SELECT count(*) n FROM outbox WHERE status = ?").get(s) as { n: number }
        ).n;
        return acc;
      },
      {} as Record<OutboxStatus, number>,
    ),
};

// ---- embeddings + semantic search ----

/** Every item worth embedding. `text` is the clean body (for reranking +
 * display); `embedText` carries a `[kind] title` context prefix the way
 * bookstack-mcp prepends `[shelf > book > chapter > page]` before embedding. */
export function embeddableItems(): {
  kind: string;
  id: string;
  title: string;
  text: string;
  embedText: string;
  hash: string;
}[] {
  const out: { kind: string; id: string; title: string; text: string; embedText: string; hash: string }[] = [];
  const push = (kind: string, id: string, title: string, text: string) => {
    const embedText = `[${kind}] ${title}\n\n${text}`;
    out.push({ kind, id, title, text, embedText, hash: contentHash(embedText) });
  };
  for (const e of journal.list(1000)) push("journal", e.id, `${e.author}: ${e.body.slice(0, 40)}`, e.body);
  for (const t of tasks.list()) push("task", t.id, t.title, `${t.title} ${t.body}`);
  for (const d of decisions.list())
    push("decision", d.id, d.title, `${d.title} ${d.context} ${d.decision} ${d.consequences}`);
  for (const ev of events.list()) push("event", ev.id, ev.title, `${ev.title} ${ev.body}`);
  return out;
}

export const embeddings = {
  count: () => (db.prepare("SELECT count(*) n FROM embeddings").get() as { n: number }).n,

  async upsert(ref_kind: string, ref_id: string, embedText: string): Promise<boolean> {
    const hash = contentHash(embedText);
    const existing = db
      .prepare("SELECT hash, model FROM embeddings WHERE ref_kind = ? AND ref_id = ?")
      .get(ref_kind, ref_id) as { hash: string; model: string } | undefined;
    // Re-embed when the text changed OR the active model changed — so flipping
    // $HIVE_EMBED makes the next backfill recompute even unchanged rows.
    if (existing?.hash === hash && existing.model === EMBED_MODEL) return false;
    const vec = await embed(embedText);
    // Vector is stored as a packed little-endian f32 BLOB (see embed.toBlob).
    db.prepare(
      `INSERT INTO embeddings (ref_kind, ref_id, model, dim, vec, hash, created_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(ref_kind, ref_id) DO UPDATE SET model=excluded.model, dim=excluded.dim, vec=excluded.vec, hash=excluded.hash, created_at=excluded.created_at`,
    ).run(ref_kind, ref_id, EMBED_MODEL, vec.length, toBlob(vec), hash, now());
    return true;
  },

  /** Backfill any missing/stale embeddings; returns how many were (re)computed. */
  async backfill(): Promise<number> {
    let n = 0;
    for (const it of embeddableItems()) if (await embeddings.upsert(it.kind, it.id, it.embedText)) n++;
    return n;
  },
};

/** Admin view of the embedding corpus: coverage + per-kind/per-model breakdown. */
export function embeddingStats(): EmbeddingStats {
  const items = embeddableItems();
  const stored = new Map(
    (
      db.prepare("SELECT ref_kind, ref_id, hash FROM embeddings").all() as {
        ref_kind: string;
        ref_id: string;
        hash: string;
      }[]
    ).map((r) => [`${r.ref_kind}:${r.ref_id}`, r.hash]),
  );
  let pending = 0;
  for (const it of items) if (stored.get(`${it.kind}:${it.id}`) !== it.hash) pending++;
  return {
    total: embeddings.count(),
    model: EMBED_MODEL,
    embeddable: items.length,
    pending,
    byKind: db
      .prepare("SELECT ref_kind AS kind, count(*) AS count FROM embeddings GROUP BY ref_kind ORDER BY count DESC")
      .all() as { kind: string; count: number }[],
    byModel: db
      .prepare("SELECT model, dim, count(*) AS count FROM embeddings GROUP BY model, dim ORDER BY count DESC")
      .all() as { model: string; dim: number; count: number }[],
  };
}

export interface SemanticOptions {
  limit?: number;
  /** Drop vector matches scoring below this cosine value (default 0). */
  threshold?: number;
  /** Blend FTS keyword ranks into the score (default true). */
  hybrid?: boolean;
  /** Re-order the top-N with the cross-encoder, when one is available. */
  rerank?: boolean;
  /** Scope results to entries this viewer may see (own + owned/shared/mentioned). */
  viewer?: string;
  /** Boost (not filter) hits whose author/mentions/assignees include this actor. */
  identity?: string;
  /** Boost (not filter) hits whose author/mentions/assignees include this actor. */
  peer?: string;
}

/** The actors associated with a hit: journal → author + mentions; task/decision/
 *  event → assignees. Used to softly boost results toward the recall focus. */
function hitActors(kind: string, refId: string): string[] {
  if (kind === "journal") {
    const r = db.prepare("SELECT author, mentions FROM journal WHERE id = ?").get(refId) as
      | { author: string; mentions: string }
      | undefined;
    if (!r) return [];
    return [r.author, ...json<string[]>(r.mentions)];
  }
  // Hit kinds are singular; the tables are plural.
  const table = { task: "tasks", decision: "decisions", event: "events" }[kind];
  if (table) {
    const r = db.prepare(`SELECT assignees FROM ${table} WHERE id = ?`).get(refId) as
      | { assignees: string }
      | undefined;
    return r ? json<string[]>(r.assignees) : [];
  }
  return [];
}

const refKey = (kind: string, id: string) => `${kind}:${id}`;
function splitKey(k: string): [string, string] {
  const ix = k.indexOf(":");
  return [k.slice(0, ix), k.slice(ix + 1)];
}

/** Neighbors of an entity in the links graph (either direction) — the Markov
 * blanket bookstack-mcp uses to boost results whose neighbors also surfaced. */
function blanketNeighbors(kind: string, id: string): string[] {
  const rows = db
    .prepare(
      `SELECT target_kind AS k, target_id AS i FROM links WHERE source_kind = ? AND source_id = ?
       UNION
       SELECT source_kind AS k, source_id AS i FROM links WHERE target_kind = ? AND target_id = ?`,
    )
    .all(kind, id, kind, id) as { k: string; i: string }[];
  return rows.map((r) => refKey(r.k, r.i));
}

/**
 * Semantic search, mirroring bookstack-mcp's hybrid pipeline: a brute-force
 * cosine vector pass, an optional FTS keyword blend (0.7 vector / 0.2 keyword),
 * a Markov-blanket boost from the links graph, and an optional cross-encoder
 * rerank of the top-N. Falls back to top-k vector hits so a non-empty corpus
 * never returns nothing.
 */
export async function semanticSearch(query: string, opts: SemanticOptions = {}): Promise<SearchHit[]> {
  const limit = opts.limit ?? 10;
  const threshold = opts.threshold ?? 0;
  const hybrid = opts.hybrid ?? true;
  const useRerank = (opts.rerank ?? false) && RERANK_AVAILABLE;
  if (!query.trim()) return [];

  const items = embeddableItems();
  const titleOf = new Map(items.map((i) => [refKey(i.kind, i.id), i.title]));
  const textOf = new Map(items.map((i) => [refKey(i.kind, i.id), i.text]));

  // 1. Vector pass — full cosine over model-matched blobs. The model+dim filter
  // means a partial backfill (mixed models) never compares across dimensions.
  const q = await embedQuery(query);
  const rows = db
    .prepare("SELECT ref_kind, ref_id, vec FROM embeddings WHERE model = ? AND dim = ?")
    .all(EMBED_MODEL, q.length) as { ref_kind: string; ref_id: string; vec: Buffer }[];
  const scoredAll = rows
    .map((r) => ({ k: refKey(r.ref_kind, r.ref_id), score: cosine(q, fromBlob(r.vec)) }))
    .sort((a, b) => b.score - a.score);
  const passing = scoredAll.filter((h) => h.score >= threshold);
  const rawHitKeys = new Set(passing.map((h) => h.k));
  const vhits = passing.slice(0, Math.max(limit * 2, limit));

  type Score = { vector: number; keyword: number; blanket: number };
  const scores = new Map<string, Score>();
  for (const h of vhits) scores.set(h.k, { vector: h.score, keyword: 0, blanket: 0 });

  // 2. Keyword pass (FTS) — rank-based score, decaying from the top.
  if (hybrid) {
    const kw = search(query, limit * 2);
    const total = kw.length || 1;
    kw.forEach((r, i) => {
      const kk = refKey(r.kind, r.id);
      const s = scores.get(kk) ?? { vector: 0, keyword: 0, blanket: 0 };
      s.keyword = 1 - i / total;
      scores.set(kk, s);
    });
  }

  // 3. Markov-blanket boost: neighbor in the final set (+0.05, cap 0.15),
  // neighbor that had a vector hit but didn't make the cut (+0.02, cap 0.06).
  const scoredKeys = new Set(scores.keys());
  for (const [kk, s] of scores) {
    const [k, id] = splitKey(kk);
    let strong = 0;
    let weak = 0;
    for (const nk of blanketNeighbors(k, id)) {
      if (scoredKeys.has(nk)) strong++;
      else if (rawHitKeys.has(nk)) weak++;
    }
    if (strong || weak) s.blanket = Math.min(strong * 0.05, 0.15) + Math.min(weak * 0.02, 0.06);
  }

  // Drop keyword-only noise — a keyword hit with zero semantic relevance.
  if (hybrid) for (const [kk, s] of [...scores]) if (s.vector === 0 && s.keyword > 0) scores.delete(kk);

  // 4. Blended sort. When recall passes identity/peer, softly boost hits whose
  // actors intersect that focus set (+0.1 if either matches) — a nudge, not a
  // filter, so unrelated-but-relevant material still surfaces.
  const focus = new Set([opts.identity, opts.peer].filter(Boolean) as string[]);
  const scoped = (kk: string): number => {
    if (!focus.size) return 0;
    const [k, id] = splitKey(kk);
    return hitActors(k, id).some((a) => focus.has(a)) ? 0.1 : 0;
  };
  let ranked = [...scores.entries()]
    .map(([k, s]) => ({
      k,
      score:
        (s.keyword > 0 && s.vector > 0 ? s.vector * 0.7 + s.keyword * 0.2 + s.blanket : s.vector + s.blanket) +
        scoped(k),
    }))
    .sort((a, b) => b.score - a.score)
    .slice(0, limit);

  // 5. Cross-encoder rerank of the top-N (re-orders only; candidate set stays).
  if (useRerank && ranked.length) {
    const rr = await rerank(query, ranked.map((r) => textOf.get(r.k) ?? ""));
    if (rr) ranked = ranked.map((r, i) => ({ k: r.k, score: rr[i] })).sort((a, b) => b.score - a.score);
  }

  // 6. Fallback — never return empty when vectors exist.
  if (ranked.length === 0 && scoredAll.length) {
    ranked = scoredAll.slice(0, limit).map((h) => ({ k: h.k, score: h.score }));
  }

  const hits = ranked.map((r) => {
    const [kind, id] = splitKey(r.k);
    return {
      kind: kind as SearchHit["kind"],
      id,
      title: titleOf.get(r.k) ?? id,
      snippet: "",
      score: Math.round(r.score * 1000) / 1000,
    };
  });
  return opts.viewer ? scopeHits(hits, opts.viewer) : hits;
}

// ---- recall (the read/inject composition: cards + scoped retrieval) ----

/** Rough token estimate (~4 chars/token) used only to trim the brief to budget. */
const estTokens = (s: string) => Math.ceil(s.length / 4);

/** Append sections to the brief until the next one would exceed the token budget. */
function assembleBrief(sections: string[], budget: number): string {
  const out: string[] = [];
  let used = 0;
  for (const sec of sections) {
    const cost = estTokens(sec) + 1;
    if (out.length && used + cost > budget) break;
    out.push(sec);
    used += cost;
  }
  return out.join("\n\n");
}

function profileCard(p: Profile): string {
  const head = `## ${p.display_name || p.actor} (${p.kind})`;
  const secs = Object.entries(p.body.sections)
    .filter(([, v]) => v.trim())
    .map(([k, v]) => `**${k.replace(/_/g, " ")}:** ${v.trim()}`);
  return [head, ...secs].join("\n");
}

/** Journal entries have no stored title — derive one from the prose. Prefer the
 *  first Markdown heading (`# …`), else the first non-empty line, truncated. */
function deriveJournalTitle(body: string): string {
  for (const raw of body.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const h = line.match(/^#{1,6}\s+(.*)$/);
    return snip((h ? h[1] : line).trim(), 80);
  }
  return "(untitled)";
}

/**
 * Compose a ready-to-inject memory brief for `identity` (optionally focused on
 * `peer`): profile cards, open tasks, unread inbox, recent relevant journal,
 * recent events, touched projects. Deterministic assembly (no LLM), trimmed to
 * `budget` tokens. Returns the markdown `brief` plus the structured `data`.
 */
export async function recall(opts: {
  identity: string;
  peer?: string;
  query?: string;
  budget?: number;
}): Promise<RecallResult> {
  const { identity, peer } = opts;
  const budget = opts.budget ?? RECALL_DEFAULT_BUDGET;

  const profileList: Profile[] = [];
  const idCard = profiles.get(identity);
  if (idCard) profileList.push(idCard);
  const peerCard = peer ? profiles.get(peer) : undefined;
  if (peerCard) profileList.push(peerCard);

  const openTasks = tasks
    .list({ assignee: identity })
    .filter((t) => t.status !== "done");

  const unread = inbox.list(identity, true);

  // Default query (no explicit topic): the actors in focus plus the open-task
  // titles — pulls "recent + open threads" toward the recall.
  const query =
    opts.query?.trim() ||
    [identity, peer, ...openTasks.slice(0, 5).map((t) => t.title)].filter(Boolean).join(" ");

  const rawHits = query ? await semanticSearch(query, { limit: 8, identity, peer }) : [];
  const journalHits: RecallJournalHit[] = rawHits
    .filter((h) => h.kind === "journal")
    .map((h) => {
      const e = journal.get(h.id);
      // Title is derived from the body (no title column); adapters fold a
      // heading into the prose, so prefer the first Markdown `#` heading.
      return e
        ? { ...h, title: deriveJournalTitle(e.body), author: e.author, created_at: e.created_at }
        : undefined;
    })
    .filter((h): h is RecallJournalHit => h !== undefined);

  const recentEvents = events.list().slice(0, 5);

  // Projects touched by the identity's open tasks.
  const projIds = new Set(openTasks.map((t) => t.project).filter(Boolean) as string[]);
  const touchedProjects: ProjectRef[] = [...projIds]
    .map((pid) => projects.get(pid))
    .filter((p): p is Project => p !== undefined)
    .map((p) => ({ id: p.id, name: p.name, slug: p.slug }));

  const data: RecallData = {
    profiles: profileList,
    journal: journalHits,
    tasks: openTasks,
    inbox: unread,
    events: recentEvents,
    projects: touchedProjects,
  };

  // Deterministic markdown brief — cards first, then the working sections.
  const sections: string[] = [`# Recall for ${identity}${peer ? ` · focus: ${peer}` : ""}`];
  for (const p of profileList) sections.push(profileCard(p));
  if (openTasks.length)
    sections.push(
      `## Open tasks (${identity})\n` +
        openTasks.map((t) => `- [${t.status}] ${t.title}${t.due ? ` (due ${t.due})` : ""}`).join("\n"),
    );
  if (unread.length)
    sections.push(
      `## Unread inbox\n` +
        unread.map((i) => `- from ${i.from} (${i.reason}): ${i.snippet}`).join("\n"),
    );
  if (journalHits.length)
    sections.push(
      `## Recent relevant journal\n` +
        journalHits.map((h) => `- ${h.author}: ${h.title}`).join("\n"),
    );
  if (recentEvents.length)
    sections.push(
      `## Recent events\n` +
        recentEvents.map((e) => `- ${e.title}${e.at ? ` (${e.at})` : ""}`).join("\n"),
    );
  if (touchedProjects.length)
    sections.push(`## Projects\n` + touchedProjects.map((p) => `- ${p.name}`).join("\n"));

  return { brief: assembleBrief(sections, budget), data };
}

// ---- worker status ----

export function setHeartbeat(): void {
  db.prepare(
    "INSERT INTO worker_status (id, heartbeat) VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET heartbeat = excluded.heartbeat",
  ).run(now());
}

export function setLastRun(stats: NonNullable<WorkerStatus["last_run"]>): void {
  db.prepare(
    "INSERT INTO worker_status (id, last_run) VALUES (1, ?) ON CONFLICT(id) DO UPDATE SET last_run = excluded.last_run",
  ).run(JSON.stringify(stats));
}

export function workerStatus(): WorkerStatus {
  const row = db.prepare("SELECT heartbeat, last_run FROM worker_status WHERE id = 1").get() as
    | { heartbeat: string | null; last_run: string | null }
    | undefined;
  const all = sources.list();
  return {
    heartbeat: row?.heartbeat ?? null,
    last_run: row?.last_run ? json(row.last_run) : null,
    sources: { total: all.length, enabled: all.filter((s) => s.enabled).length },
    outbox: outbox.counts(),
    embeddings: { count: embeddings.count(), model: EMBED_MODEL },
  };
}
