import { createServer } from "node:http";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { getRequestListener } from "@hono/node-server";
import { Hono } from "hono";
import { deleteCookie, getCookie, setCookie } from "hono/cookie";
import type { IncomingMessage, ServerResponse } from "node:http";
import type {
  DecisionPatch,
  LegacyImport,
  NewJournalEntry,
  NewShare,
  NewSource,
  OnboardingPayload,
  PersonPatch,
  SourcePatch,
  TaskPatch,
  UserRole,
} from "@hive/shared";
import { migrate } from "./db.ts";
import { readLegacyDb } from "./legacy-import.ts";
import { handleMcp } from "./mcp.ts";
import { subscribe } from "./bus.ts";
import { SESSION_COOKIE, SESSION_TTL_MS } from "./auth.ts";
import {
  onboarding,
  sessions,
  tokens,
  importLegacy,
  users,
  autocomplete,
  dashboard,
  decisions,
  embeddingStats,
  events,
  graph,
  inbox,
  journal,
  journalWriters,
  links,
  outbox,
  people,
  phases,
  projects,
  search,
  seedActors,
  semanticSearch,
  shares,
  sources,
  tasks,
  topics,
  visibleJournal,
  wire,
  workerStatus,
} from "./store.ts";

migrate();
seedActors();

type Principal = "session" | "token";
type Env = { Variables: { actor?: string; principal?: Principal; role?: UserRole } };
const app = new Hono<Env>();

// CORS. Credentials must flow (the session is an HttpOnly cookie), so we reflect
// the request Origin rather than using "*" (the two are mutually exclusive).
app.use("*", async (c, next) => {
  const origin = c.req.header("origin");
  if (origin) {
    c.header("access-control-allow-origin", origin);
    c.header("vary", "Origin");
    c.header("access-control-allow-credentials", "true");
  }
  c.header("access-control-allow-headers", "content-type, authorization, x-hive-actor");
  c.header("access-control-allow-methods", "GET,POST,PATCH,DELETE,OPTIONS");
  if (c.req.method === "OPTIONS") return c.body(null, 204);
  await next();
});

// Auth. v0.1.1 ends the trust-the-header model: identity comes from a session
// cookie (browser) or a Bearer API token (CLI / MCP / AI agents). The
// x-hive-actor header is no longer honored for identity.
const PUBLIC_PATHS = new Set([
  "/api/healthz",
  "/api/onboarding/status",
  "/api/onboarding",
  "/api/auth/login",
  "/api/auth/me",
]);

app.use("*", async (c, next) => {
  // Resolve the principal if any credentials are present.
  const auth = c.req.header("authorization");
  const bearer = auth?.startsWith("Bearer ") ? auth.slice(7).trim() : undefined;
  if (bearer) {
    const a = tokens.resolve(bearer);
    if (a) {
      c.set("actor", a);
      c.set("principal", "token");
    }
  }
  if (!c.get("actor")) {
    const cookie = getCookie(c, SESSION_COOKIE);
    if (cookie) {
      const u = sessions.resolve(cookie);
      if (u) {
        c.set("actor", u.actor);
        c.set("principal", "session");
        c.set("role", u.role);
      }
    }
  }

  if (PUBLIC_PATHS.has(c.req.path)) return next();
  // Before setup, everything non-public is locked until onboarding runs.
  if (onboarding.required()) return c.json({ error: "onboarding_required" }, 403);
  if (!c.get("actor")) return c.json({ error: "unauthenticated" }, 401);
  return next();
});

const actor = (c: { get: (k: "actor") => string | undefined }) => c.get("actor") ?? "anon";
const requireAdmin = (c: { get: (k: "role") => UserRole | undefined }) => c.get("role") === "admin";
// Admin gate that also accepts a Bearer token whose actor maps to an admin user
// (sessions carry role directly; tokens don't, so resolve via the user record).
const requireAdminActor = (c: { get: (k: "role" | "actor") => UserRole | string | undefined }): boolean =>
  c.get("role") === "admin" || users.list().find((u) => u.actor === c.get("actor"))?.role === "admin";

app.get("/api/healthz", (c) =>
  c.json({ ok: true, service: "hive-node", mcp: "/mcp", ts: new Date().toISOString() }),
);

// ---- onboarding (first-run) + auth ----
app.get("/api/onboarding/status", (c) => c.json(onboarding.status()));

app.post("/api/onboarding", async (c) => {
  if (!onboarding.required()) return c.json({ error: "already_completed" }, 409);
  const body = (await c.req.json()) as OnboardingPayload;
  const { instanceName, adminName, adminEmail, password } = body ?? {};
  if (!instanceName?.trim() || !adminName?.trim() || !adminEmail?.trim() || !password?.trim())
    return c.json({ error: "instanceName, adminName, adminEmail, password required" }, 400);
  if (password.length < 8) return c.json({ error: "password must be at least 8 characters" }, 400);
  const { user, session } = onboarding.complete({ instanceName, adminName, adminEmail, password });
  setCookie(c, SESSION_COOKIE, session, {
    httpOnly: true,
    sameSite: "Lax",
    path: "/",
    maxAge: SESSION_TTL_MS / 1000,
  });
  return c.json({ user }, 201);
});

app.post("/api/auth/login", async (c) => {
  const { email, password } = (await c.req.json()) as { email?: string; password?: string };
  if (!email || !password) return c.json({ error: "email and password required" }, 400);
  const u = users.authenticate(email, password);
  if (!u) return c.json({ error: "invalid credentials" }, 401);
  const session = sessions.create(u.id);
  setCookie(c, SESSION_COOKIE, session, {
    httpOnly: true,
    sameSite: "Lax",
    path: "/",
    maxAge: SESSION_TTL_MS / 1000,
  });
  return c.json({ user: users.safe(u) });
});

app.post("/api/auth/logout", (c) => {
  const cookie = getCookie(c, SESSION_COOKIE);
  if (cookie) sessions.destroy(cookie);
  deleteCookie(c, SESSION_COOKIE, { path: "/" });
  return c.json({ ok: true });
});

app.get("/api/auth/me", (c) => {
  const a = c.get("actor");
  const u = a ? users.list().find((x) => x.actor === a) : undefined;
  return c.json({ user: u ?? null, principal: c.get("principal") ?? null });
});

// ---- users (admin) ----
app.get("/api/users", (c) => (requireAdmin(c) ? c.json(users.list()) : c.json({ error: "forbidden" }, 403)));
app.post("/api/users", async (c) => {
  if (!requireAdmin(c)) return c.json({ error: "forbidden" }, 403);
  const body = (await c.req.json()) as {
    name: string;
    email: string;
    password: string;
    role?: UserRole;
    kind?: "human" | "ai";
  };
  if (!body?.name?.trim() || !body?.email?.trim() || !body?.password?.trim())
    return c.json({ error: "name, email, password required" }, 400);
  if (body.password.length < 8) return c.json({ error: "password must be at least 8 characters" }, 400);
  return c.json(users.create(body, actor(c)), 201);
});

// ---- API tokens (admin) ----
app.get("/api/tokens", (c) => (requireAdmin(c) ? c.json(tokens.list()) : c.json({ error: "forbidden" }, 403)));
app.post("/api/tokens", async (c) => {
  if (!requireAdmin(c)) return c.json({ error: "forbidden" }, 403);
  const body = (await c.req.json()) as { actor: string; label: string; expiresInDays?: number };
  if (!body?.actor?.trim() || !body?.label?.trim()) return c.json({ error: "actor and label required" }, 400);
  // The plaintext token is returned ONCE here and never again. expiresInDays is clamped
  // server-side to [1, API_TOKEN_MAX_EXPIRY_DAYS]; omitted → API_TOKEN_DEFAULT_EXPIRY_DAYS.
  const { token, record } = tokens.create(body, actor(c));
  return c.json({ token, record }, 201);
});
app.delete("/api/tokens/:id", (c) => {
  if (!requireAdmin(c)) return c.json({ error: "forbidden" }, 403);
  return tokens.remove(c.req.param("id")) ? c.body(null, 204) : c.json({ error: "not found" }, 404);
});

// ---- bulk historical import (admin) ----
// Backfill from a legacy hive.db. Idempotent (existing ids skipped). Admin-only; an
// admin's Bearer token qualifies (e.g. a one-shot programmatic migration).
app.post("/api/import", async (c) => {
  if (!requireAdminActor(c)) return c.json({ error: "forbidden" }, 403);
  const payload = (await c.req.json()) as LegacyImport;
  return c.json(importLegacy(payload));
});

// Upload a legacy hive.db (SQLite) straight from the dashboard. We persist it to a
// temp file (better-sqlite3 needs a path), read it READ-ONLY, map → import, then delete.
app.post("/api/import/sqlite", async (c) => {
  if (!requireAdminActor(c)) return c.json({ error: "forbidden" }, 403);
  const form = await c.req.parseBody();
  const file = form["db"];
  if (!(file instanceof File)) return c.json({ error: "multipart field 'db' (the .db file) required" }, 400);

  const dir = mkdtempSync(join(tmpdir(), "hive-import-"));
  const dbPath = join(dir, "legacy.db");
  try {
    writeFileSync(dbPath, Buffer.from(await file.arrayBuffer()));
    const { payload, warnings } = readLegacyDb(dbPath);
    const result = importLegacy(payload);
    return c.json({ ...result, warnings });
  } catch (e) {
    return c.json({ error: `import failed: ${(e as Error).message}` }, 400);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// ---- journal (the one write path) ----
app.get("/api/journal/writers", (c) => {
  const viewer = c.req.query("viewer");
  if (!viewer) return c.json({ error: "viewer required" }, 400);
  return c.json(journalWriters(viewer));
});
app.get("/api/journal", (c) => {
  const limit = Number(c.req.query("limit") ?? 50);
  const offset = Number(c.req.query("offset") ?? 0);
  const viewer = c.req.query("viewer");
  if (viewer) {
    const writersParam = c.req.query("writers");
    const writers = writersParam ? writersParam.split(",").map((s) => s.trim()).filter(Boolean) : undefined;
    return c.json(visibleJournal({ viewer, writers, limit, offset }));
  }
  return c.json(journal.list(limit, offset));
});
app.post("/api/journal", async (c) => {
  const body = (await c.req.json()) as NewJournalEntry;
  if (!body?.body?.trim()) return c.json({ error: "body required" }, 400);
  // Author is the authenticated identity — a client can't write as someone else.
  return c.json(journal.append({ ...body, author: actor(c) }, actor(c)), 201);
});
app.get("/api/journal/:id", (c) => {
  const e = journal.get(c.req.param("id"));
  return e ? c.json(e) : c.json({ error: "not found" }, 404);
});

// ---- structured entities (read + workflow only; creation flows via journal) ----
app.get("/api/tasks", (c) => {
  const { status, assignee, project } = c.req.query();
  return c.json(tasks.list({ status, assignee, project }));
});
app.get("/api/tasks/:id", (c) => {
  const t = tasks.get(c.req.param("id"));
  return t ? c.json(t) : c.json({ error: "not found" }, 404);
});
app.patch("/api/tasks/:id", async (c) => {
  const patch = (await c.req.json()) as TaskPatch;
  const t = tasks.update(c.req.param("id"), patch, actor(c));
  return t ? c.json(t) : c.json({ error: "not found" }, 404);
});

app.get("/api/decisions", (c) => c.json(decisions.list({ status: c.req.query("status") })));
app.get("/api/decisions/:id", (c) => {
  const d = decisions.get(c.req.param("id"));
  return d ? c.json(d) : c.json({ error: "not found" }, 404);
});
app.patch("/api/decisions/:id", async (c) => {
  const patch = (await c.req.json()) as DecisionPatch;
  const d = decisions.update(c.req.param("id"), patch, actor(c));
  return d ? c.json(d) : c.json({ error: "not found" }, 404);
});

app.get("/api/events", (c) => c.json(events.list()));
app.get("/api/events/:id", (c) => {
  const e = events.get(c.req.param("id"));
  return e ? c.json(e) : c.json({ error: "not found" }, 404);
});

// ---- inbox (per actor; humans + AIs) ----
app.get("/api/inbox/:recipient", (c) => {
  const unread = c.req.query("unread") === "1" || c.req.query("unread") === "true";
  return c.json(inbox.list(c.req.param("recipient"), unread));
});
app.post("/api/inbox/:recipient/read", (c) => c.json({ marked: inbox.markAllRead(c.req.param("recipient")) }));
app.post("/api/inbox/item/:id/read", (c) => c.json({ marked: inbox.markRead(c.req.param("id")) }));

// ---- people (writers: humans + AIs with ownership) ----
app.get("/api/people", (c) => c.json(people.list()));
app.get("/api/people/:slug", (c) => {
  const p = people.get(c.req.param("slug"));
  return p ? c.json(p) : c.json({ error: "not found" }, 404);
});
app.patch("/api/people/:slug", async (c) => {
  const patch = (await c.req.json()) as PersonPatch;
  const p = people.update(c.req.param("slug"), patch, actor(c));
  return p ? c.json(p) : c.json({ error: "not found" }, 404);
});
app.post("/api/people", async (c) => {
  const body = (await c.req.json()) as { name: string; kind?: "human" | "ai" };
  if (!body?.name?.trim()) return c.json({ error: "name required" }, 400);
  return c.json(people.create(body, actor(c)), 201);
});

// ---- shares ----
app.post("/api/shares", async (c) => {
  const body = (await c.req.json()) as NewShare;
  if (!body?.scope || !body?.ref || !body?.viewer) return c.json({ error: "scope, ref, viewer required" }, 400);
  return c.json(shares.create(body), 201);
});
app.get("/api/shares", (c) => {
  const viewer = c.req.query("viewer");
  if (!viewer) return c.json({ error: "viewer required" }, 400);
  return c.json(shares.forViewer(viewer));
});

// ---- misc ----
app.get("/api/topics", (c) => c.json(topics.list()));
app.get("/api/topics/:id", (c) => {
  const t = topics.get(c.req.param("id"));
  return t ? c.json(t) : c.json({ error: "not found" }, 404);
});
app.get("/api/phases", (c) => {
  const project = c.req.query("project");
  return c.json(phases.list(project || undefined));
});
app.get("/api/phases/:id", (c) => {
  const ph = phases.get(c.req.param("id"));
  return ph ? c.json(ph) : c.json({ error: "not found" }, 404);
});
app.get("/api/projects", (c) => c.json(projects.list()));
app.get("/api/projects/:id", (c) => {
  const p = projects.withChildren(c.req.param("id"));
  return p ? c.json(p) : c.json({ error: "not found" }, 404);
});
app.get("/api/autocomplete", (c) => {
  const q = c.req.query("q") ?? "";
  const kindsParam = c.req.query("kinds");
  const kinds = kindsParam ? kindsParam.split(",").map((k) => k.trim()) : undefined;
  return c.json(autocomplete(q, kinds));
});
app.get("/api/links/:id", (c) => c.json(links.forEntity(c.req.param("id"))));
app.get("/api/search", async (c) => {
  const q = c.req.query("q") ?? "";
  const limit = Number(c.req.query("limit") ?? 25);
  // ?mode=semantic uses the local embedder; default is FTS keyword search.
  // Semantic flags: &hybrid=0 to disable the keyword blend, &rerank=1 for the
  // cross-encoder pass, &threshold=<n> to drop weak vector matches.
  // Results are scoped to the acting user's visible entries (permission-honoring,
  // like bookstack-mcp). ?viewer= overrides the x-hive-actor header.
  const viewer = c.req.query("viewer") ?? actor(c);
  if (c.req.query("mode") === "semantic") {
    const flag = (name: string) => c.req.query(name) === "1" || c.req.query(name) === "true";
    const thr = c.req.query("threshold");
    return c.json(
      await semanticSearch(q, {
        limit,
        hybrid: c.req.query("hybrid") !== "0" && c.req.query("hybrid") !== "false",
        rerank: flag("rerank"),
        threshold: thr ? Number(thr) : undefined,
        viewer,
      }),
    );
  }
  return c.json(search(q, limit, viewer));
});
app.get("/api/wire", (c) => c.json(wire(Number(c.req.query("limit") ?? 100))));
app.get("/api/dashboard", (c) => c.json(dashboard()));
app.get("/api/graph", (c) => c.json(graph()));

// ---- worker config: sources (GUI + MCP configurable) ----
app.get("/api/sources", (c) => {
  const ownerParam = c.req.query("owner");
  // ?owner=<actor> returns global + that actor's; omit for all.
  return c.json(sources.list(ownerParam || undefined));
});
app.post("/api/sources", async (c) => {
  const body = (await c.req.json()) as NewSource & { scope?: "global" | "me" };
  if (!body?.name || !body?.url) return c.json({ error: "name and url required" }, 400);
  // scope:"me" → owner = actor header; scope:"global" or absent → owner = null (global).
  const owner = body.scope === "me" ? actor(c) : (body.owner ?? null);
  return c.json(sources.create({ ...body, owner }, actor(c)), 201);
});
app.patch("/api/sources/:id", async (c) => {
  const patch = (await c.req.json()) as SourcePatch;
  const s = sources.update(c.req.param("id"), patch, actor(c));
  return s ? c.json(s) : c.json({ error: "not found" }, 404);
});
app.delete("/api/sources/:id", (c) =>
  sources.remove(c.req.param("id"), actor(c)) ? c.body(null, 204) : c.json({ error: "not found" }, 404),
);

// ---- worker status + outbox + embeddings (admin) ----
app.get("/api/worker", (c) => c.json(workerStatus()));
app.get("/api/outbox", (c) => c.json(outbox.list(Number(c.req.query("limit") ?? 50))));
app.get("/api/embeddings", (c) => c.json(embeddingStats()));

// A locally-served sample RSS feed so feed ingestion is real (and demoable)
// without depending on outbound network in the sandbox.
app.get("/api/_fixtures/sample.xml", (c) => {
  const items = [
    ["bee-rss-1", "pgvector 0.8 released", "Postgres vector search gets faster ANN indexes."],
    ["bee-rss-2", "Solid 2.0 roadmap", "Fine-grained reactivity, same tiny runtime."],
    ["bee-rss-3", "SQLite ships native JSON5", "Looser JSON parsing lands in the amalgamation."],
  ];
  const xml = `<?xml version="1.0"?><rss version="2.0"><channel><title>Bee feed</title>${items
    .map(
      ([g, t, d]) =>
        `<item><guid>${g}</guid><title>${t}</title><link>https://example.com/${g}</link><description>${d}</description></item>`,
    )
    .join("")}</channel></rss>`;
  return c.body(xml, 200, { "content-type": "application/rss+xml" });
});

// A locally-served sample HTML page so scrape ingestion is demoable without
// depending on outbound network.
app.get("/api/_fixtures/sample.html", (c) => {
  const html = `<!DOCTYPE html><html><head><title>Bee scrape fixture</title></head><body>
<h1>Bee's Roadhouse dev feed</h1>
<h2>Latest picks</h2>
<ul>
  <li><a href="https://example.com/bee-scrape-1">Hono v4 ships — faster routing, smaller core</a></li>
  <li><a href="https://example.com/bee-scrape-2">SolidJS fine-grained signals land in v2</a></li>
  <li><a href="https://example.com/bee-scrape-3">better-sqlite3 adds WAL2 support</a></li>
</ul>
<nav><a href="/">home</a> <a href="/about">about</a></nav>
</body></html>`;
  return c.body(html, 200, { "content-type": "text/html" });
});

// ---- SSE live-push stream ----
// Every mutation calls emit() → publish() → here. Served from the raw Node http
// server (not Hono) so `res.write` flushes each frame to the socket immediately —
// streamSSE through node-server buffers mid-stream writes. Clients reconnect
// automatically via EventSource; a heartbeat comment every 25 s keeps idle
// connections alive through proxies.
// Resolve identity for the raw-server endpoints (/mcp, /api/stream), which
// bypass Hono. Bearer token → actor; else session cookie → actor.
function rawActor(req: IncomingMessage): { actor: string; principal: Principal } | undefined {
  const auth = req.headers["authorization"];
  if (auth?.startsWith("Bearer ")) {
    const a = tokens.resolve(auth.slice(7).trim());
    if (a) return { actor: a, principal: "token" };
  }
  const raw = req.headers["cookie"] ?? "";
  const m = raw.split(";").map((s) => s.trim()).find((s) => s.startsWith(`${SESSION_COOKIE}=`));
  if (m) {
    const u = sessions.resolve(decodeURIComponent(m.slice(SESSION_COOKIE.length + 1)));
    if (u) return { actor: u.actor, principal: "session" };
  }
  return undefined;
}

function handleStream(res: ServerResponse): void {
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
    "access-control-allow-origin": "*",
  });
  res.write(": connected\n\n");
  const unsub = subscribe((ev) => res.write(`data: ${JSON.stringify(ev)}\n\n`));
  const heartbeat = setInterval(() => res.write(": heartbeat\n\n"), 25_000);
  res.on("close", () => {
    clearInterval(heartbeat);
    unsub();
  });
}

// Raw Node server so /mcp gets the un-touched request stream the Streamable
// HTTP transport needs; everything else is delegated to Hono.
const honoListener = getRequestListener(app.fetch);
const port = Number(process.env.PORT ?? 8787);

createServer((req, res) => {
  const path = (req.url ?? "").split("?")[0];
  if (path === "/mcp") {
    // MCP is the programmatic surface — require a valid Bearer token (OPTIONS
    // preflight passes through so handleMcp can answer CORS).
    if (req.method !== "OPTIONS" && (onboarding.required() || !rawActor(req))) {
      res.writeHead(401, { "content-type": "application/json", "access-control-allow-origin": "*" });
      res.end(
        JSON.stringify({
          jsonrpc: "2.0",
          error: { code: -32001, message: "Unauthorized — provide a Bearer API token." },
          id: null,
        }),
      );
      return;
    }
    handleMcp(req, res).catch((err) => {
      console.error("mcp error", err);
      if (!res.headersSent) res.writeHead(500).end();
    });
    return;
  }
  if (path === "/api/stream") {
    if (onboarding.required() || !rawActor(req)) {
      res.writeHead(401, { "content-type": "application/json", "access-control-allow-origin": "*" });
      res.end(JSON.stringify({ error: "unauthenticated" }));
      return;
    }
    handleStream(res);
    return;
  }
  honoListener(req, res);
}).listen(port, () => {
  console.log(`🐝 hive-api (node) on http://localhost:${port}  ·  MCP at /mcp`);
});
