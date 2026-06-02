import { createServer } from "node:http";
import { getRequestListener } from "@hono/node-server";
import { Hono } from "hono";
import type { DecisionPatch, NewJournalEntry, TaskPatch } from "@hive/shared";
import { migrate } from "./db.ts";
import { handleMcp } from "./mcp.ts";
import {
  dashboard,
  decisions,
  events,
  inbox,
  journal,
  links,
  projects,
  search,
  tasks,
  wire,
} from "./store.ts";

migrate();

const app = new Hono();

// CORS + actor shim. The real hive does EdDSA/JWT (9 phases); this fun port
// reads who's acting from a header so the journal/inbox/wire stay honest.
app.use("*", async (c, next) => {
  c.header("access-control-allow-origin", "*");
  c.header("access-control-allow-headers", "content-type, x-hive-actor");
  c.header("access-control-allow-methods", "GET,POST,PATCH,DELETE,OPTIONS");
  if (c.req.method === "OPTIONS") return c.body(null, 204);
  await next();
});

const actor = (c: { req: { header: (k: string) => string | undefined } }) =>
  c.req.header("x-hive-actor") ?? "anon";

app.get("/api/healthz", (c) =>
  c.json({ ok: true, service: "hive-node", mcp: "/mcp", ts: new Date().toISOString() }),
);

// ---- journal (the one write path) ----
app.get("/api/journal", (c) => c.json(journal.list(Number(c.req.query("limit") ?? 50))));
app.post("/api/journal", async (c) => {
  const body = (await c.req.json()) as NewJournalEntry;
  if (!body?.body?.trim()) return c.json({ error: "body required" }, 400);
  return c.json(journal.append({ ...body, author: body.author ?? actor(c) }, actor(c)), 201);
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

// ---- misc ----
app.get("/api/projects", (c) => c.json(projects.list()));
app.get("/api/links/:id", (c) => c.json(links.forEntity(c.req.param("id"))));
app.get("/api/search", (c) =>
  c.json(search(c.req.query("q") ?? "", Number(c.req.query("limit") ?? 25))),
);
app.get("/api/wire", (c) => c.json(wire(Number(c.req.query("limit") ?? 100))));
app.get("/api/dashboard", (c) => c.json(dashboard()));

// Raw Node server so /mcp gets the un-touched request stream the Streamable
// HTTP transport needs; everything else is delegated to Hono.
const honoListener = getRequestListener(app.fetch);
const port = Number(process.env.PORT ?? 8787);

createServer((req, res) => {
  const path = (req.url ?? "").split("?")[0];
  if (path === "/mcp") {
    handleMcp(req, res).catch((err) => {
      console.error("mcp error", err);
      if (!res.headersSent) res.writeHead(500).end();
    });
    return;
  }
  honoListener(req, res);
}).listen(port, () => {
  console.log(`🐝 hive-api (node) on http://localhost:${port}  ·  MCP at /mcp`);
});
