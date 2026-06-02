import { serve } from "@hono/node-server";
import { Hono } from "hono";
import type {
  DecisionPatch,
  NewDecision,
  NewJournalEntry,
  NewNote,
  NewTask,
  TaskPatch,
} from "@hive/shared";
import { migrate } from "./db.ts";
import {
  decisions,
  emit,
  journal,
  links,
  notes,
  projects,
  search,
  tasks,
  wire,
} from "./store.ts";

migrate();

const app = new Hono();

// Tiny CORS + actor shim. The rust hive does real EdDSA/JWT auth (9 phases);
// this fun port just reads who's acting from a header so the wire log stays
// honest about which Bee (pia/apis/cera/nate/maggie) did what.
app.use("*", async (c, next) => {
  c.header("access-control-allow-origin", "*");
  c.header("access-control-allow-headers", "content-type, x-hive-actor");
  c.header("access-control-allow-methods", "GET,POST,PATCH,DELETE,OPTIONS");
  if (c.req.method === "OPTIONS") return c.body(null, 204);
  await next();
});

const actor = (c: { req: { header: (k: string) => string | undefined } }) =>
  c.req.header("x-hive-actor") ?? "anon";

app.get("/api/healthz", (c) => c.json({ ok: true, service: "hive-node", ts: new Date().toISOString() }));

// ---- tasks ----
app.get("/api/tasks", (c) => {
  const { status, project } = c.req.query();
  return c.json(tasks.list({ status, project }));
});
app.post("/api/tasks", async (c) => {
  const body = (await c.req.json()) as NewTask;
  if (!body?.title) return c.json({ error: "title required" }, 400);
  return c.json(tasks.create(body, actor(c)), 201);
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
app.delete("/api/tasks/:id", (c) => {
  const ok = tasks.remove(c.req.param("id"), actor(c));
  return ok ? c.body(null, 204) : c.json({ error: "not found" }, 404);
});

// ---- notes ----
app.get("/api/notes", (c) => c.json(notes.list()));
app.post("/api/notes", async (c) => {
  const body = (await c.req.json()) as NewNote;
  if (!body?.title) return c.json({ error: "title required" }, 400);
  return c.json(notes.create(body, actor(c)), 201);
});
app.get("/api/notes/:id", (c) => {
  const n = notes.get(c.req.param("id"));
  return n ? c.json(n) : c.json({ error: "not found" }, 404);
});
app.delete("/api/notes/:id", (c) => {
  const ok = notes.remove(c.req.param("id"), actor(c));
  return ok ? c.body(null, 204) : c.json({ error: "not found" }, 404);
});

// ---- decisions ----
app.get("/api/decisions", (c) => {
  const { status, project } = c.req.query();
  return c.json(decisions.list({ status, project }));
});
app.post("/api/decisions", async (c) => {
  const body = (await c.req.json()) as NewDecision;
  if (!body?.title || !body?.decision)
    return c.json({ error: "title and decision required" }, 400);
  return c.json(decisions.create(body, actor(c)), 201);
});
app.get("/api/decisions/:id", (c) => {
  const d = decisions.get(c.req.param("id"));
  return d ? c.json(d) : c.json({ error: "not found" }, 404);
});
app.patch("/api/decisions/:id", async (c) => {
  const patch = (await c.req.json()) as DecisionPatch;
  const d = decisions.update(c.req.param("id"), patch, actor(c));
  return d ? c.json(d) : c.json({ error: "not found" }, 404);
});
app.delete("/api/decisions/:id", (c) => {
  const ok = decisions.remove(c.req.param("id"), actor(c));
  return ok ? c.body(null, 204) : c.json({ error: "not found" }, 404);
});

// ---- journal ----
app.get("/api/journal", (c) => {
  const limit = Number(c.req.query("limit") ?? 100);
  return c.json(journal.list(limit));
});
app.post("/api/journal", async (c) => {
  const body = (await c.req.json()) as NewJournalEntry;
  if (!body?.body) return c.json({ error: "body required" }, 400);
  return c.json(journal.create(body, actor(c)), 201);
});

// ---- projects ----
app.get("/api/projects", (c) => c.json(projects.list()));

// ---- links (knowledge graph) ----
app.get("/api/links/:id", (c) => c.json(links.forEntity(c.req.param("id"))));
app.post("/api/links", async (c) => {
  const b = (await c.req.json()) as {
    source_kind: any;
    source_id: string;
    target_kind: any;
    target_id: string;
    rel?: string;
  };
  return c.json(
    links.create(b.source_kind, b.source_id, b.target_kind, b.target_id, b.rel, actor(c)),
    201,
  );
});

// ---- search ----
app.get("/api/search", (c) => {
  const q = c.req.query("q") ?? "";
  const limit = Number(c.req.query("limit") ?? 25);
  return c.json(search(q, limit));
});

// ---- wire (event log) ----
app.get("/api/wire", (c) => c.json(wire(Number(c.req.query("limit") ?? 100))));
app.post("/api/wire", async (c) => {
  const b = (await c.req.json()) as { kind: string; payload?: unknown };
  if (!b?.kind) return c.json({ error: "kind required" }, 400);
  return c.json(emit(b.kind, actor(c), b.payload ?? null), 201);
});

const port = Number(process.env.PORT ?? 8787);
serve({ fetch: app.fetch, port }, (info) => {
  console.log(`🐝 hive-api (node) listening on http://localhost:${info.port}`);
});

export default app;
