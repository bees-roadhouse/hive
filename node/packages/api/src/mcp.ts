import type { IncomingMessage, ServerResponse } from "node:http";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { z } from "zod";
import { ACTOR_NAMES } from "@hive/shared";
import {
  dashboard,
  decisions,
  events,
  inbox,
  journal,
  outbox,
  search,
  semanticSearch,
  sources,
  tasks,
  workerStatus,
} from "./store.ts";

// MCP-first: this is the primary surface. Every AI talks to hive through these
// tools over Streamable HTTP at POST /mcp. The HTTP REST routes mirror them for
// the browser UI, but the contract lives here.

const ok = (data: unknown) => ({ content: [{ type: "text" as const, text: JSON.stringify(data, null, 2) }] });

const anchorSchema = z
  .object({
    start: z.number().int().describe("start offset (chars) of the span in `body`"),
    end: z.number().int().describe("end offset (chars) of the span in `body`"),
    kind: z.enum(["task", "decision", "event"]),
    fields: z
      .object({
        title: z.string().optional(),
        status: z.string().optional(),
        priority: z.enum(["low", "normal", "high", "urgent"]).optional(),
        assignees: z.array(z.string()).optional(),
        tags: z.array(z.string()).optional(),
        project: z.string().nullish(),
        context: z.string().optional(),
        decision: z.string().optional(),
        consequences: z.string().optional(),
        supersedes: z.string().nullish(),
        at: z.string().nullish(),
      })
      .optional(),
  })
  .describe("a span of `body` that becomes a structured task/decision/event");

/** Build a fresh server instance (stateless transport ⇒ one per request). */
export function buildMcpServer(): McpServer {
  const server = new McpServer(
    { name: "hive", version: "0.1.0" },
    {
      instructions:
        "hive is journal-first. Write prose with journal_append; attach `anchors` " +
        "(char-offset spans of the body) to emerge tasks/decisions/events anchored " +
        "to the exact text. @mention actors (" +
        ACTOR_NAMES.join(", ") +
        ") to notify their inbox. Read with the *_list / *_get / search / dashboard tools.",
    },
  );

  server.registerTool(
    "journal_append",
    {
      title: "Append a journal entry",
      description:
        "Write an immutable prose entry. Optionally attach anchors: each is a {start,end} char span of `body` that materialises a task/decision/event anchored to that text. @mentions notify inboxes.",
      inputSchema: {
        author: z.enum(ACTOR_NAMES as [string, ...string[]]),
        body: z.string().describe("the prose (Markdown supported); this is the source of truth"),
        tags: z.array(z.string()).optional(),
        anchors: z.array(anchorSchema).optional(),
      },
    },
    async (args) => ok(journal.append(args as any)),
  );

  server.registerTool(
    "journal_list",
    {
      title: "List journal entries",
      description: "Recent entries (newest first) with their resolved anchors.",
      inputSchema: { limit: z.number().int().min(1).max(200).optional() },
    },
    async ({ limit }) => ok(journal.list(limit ?? 30)),
  );

  server.registerTool(
    "journal_get",
    { title: "Get a journal entry", inputSchema: { id: z.string() } },
    async ({ id }) => ok(journal.get(id) ?? { error: "not found" }),
  );

  server.registerTool(
    "tasks_list",
    {
      title: "List tasks",
      description: "Tasks that emerged from the journal. Filter by status/assignee.",
      inputSchema: { status: z.string().optional(), assignee: z.string().optional() },
    },
    async (f) => ok(tasks.list(f)),
  );

  server.registerTool(
    "task_set_status",
    {
      title: "Advance a task",
      description: "Workflow update on a task (status is not journal-write).",
      inputSchema: { id: z.string(), status: z.enum(["todo", "doing", "blocked", "done"]) },
    },
    async ({ id, status }, _extra) =>
      ok(tasks.update(id, { status }, "mcp") ?? { error: "not found" }),
  );

  server.registerTool(
    "decisions_list",
    { title: "List decisions", inputSchema: { status: z.string().optional() } },
    async (f) => ok(decisions.list(f)),
  );

  server.registerTool("events_list", { title: "List events", inputSchema: {} }, async () =>
    ok(events.list()),
  );

  server.registerTool(
    "inbox_list",
    {
      title: "List an actor's inbox",
      description: "Unread-by-default notifications for a recipient (human or AI).",
      inputSchema: {
        recipient: z.enum(ACTOR_NAMES as [string, ...string[]]),
        unread_only: z.boolean().optional(),
      },
    },
    async ({ recipient, unread_only }) => ok(inbox.list(recipient, unread_only ?? true)),
  );

  server.registerTool(
    "inbox_mark_read",
    {
      title: "Mark inbox item(s) read",
      description: "Pass an item `id`, or a `recipient` to clear all their unread.",
      inputSchema: { id: z.string().optional(), recipient: z.string().optional() },
    },
    async ({ id, recipient }) => {
      if (id) return ok({ marked: inbox.markRead(id) });
      if (recipient) return ok({ marked: inbox.markAllRead(recipient) });
      return ok({ error: "provide id or recipient" });
    },
  );

  server.registerTool(
    "search",
    {
      title: "Full-text search",
      description: "Search across journal, tasks, decisions, events.",
      inputSchema: { q: z.string(), limit: z.number().int().optional() },
    },
    async ({ q, limit }) => ok(search(q, limit ?? 25)),
  );

  server.registerTool(
    "dashboard",
    { title: "Cross-board stats", inputSchema: {} },
    async () => ok(dashboard()),
  );

  server.registerTool(
    "semantic_search",
    {
      title: "Semantic search",
      description:
        "Hybrid semantic search across journal/tasks/decisions/events: vector + keyword blend, link-graph boost, optional cross-encoder rerank.",
      inputSchema: {
        q: z.string(),
        limit: z.number().int().optional(),
        hybrid: z.boolean().optional(),
        rerank: z.boolean().optional(),
        threshold: z.number().optional(),
      },
    },
    async ({ q, limit, hybrid, rerank, threshold }) =>
      ok(await semanticSearch(q, { limit: limit ?? 10, hybrid, rerank, threshold })),
  );

  // ---- worker configuration (sources) + status ----
  server.registerTool(
    "sources_list",
    { title: "List ingest sources", inputSchema: {} },
    async () => ok(sources.list()),
  );

  server.registerTool(
    "sources_add",
    {
      title: "Add an ingest source",
      description: "Register a feed (RSS) for the worker to poll into wire events.",
      inputSchema: {
        name: z.string(),
        url: z.string().url(),
        kind: z.enum(["rss"]).optional(),
        category: z.string().optional(),
        severity: z.enum(["critical", "high", "medium", "low", "info"]).optional(),
        interval_secs: z.number().int().min(30).optional(),
        notify: z.enum(ACTOR_NAMES as [string, ...string[]]).optional(),
      },
    },
    async (args) => ok(sources.create(args as any, "mcp")),
  );

  server.registerTool(
    "sources_update",
    {
      title: "Update an ingest source",
      inputSchema: {
        id: z.string(),
        enabled: z.boolean().optional(),
        interval_secs: z.number().int().min(30).optional(),
        severity: z.enum(["critical", "high", "medium", "low", "info"]).optional(),
        category: z.string().optional(),
        notify: z.string().optional(),
      },
    },
    async ({ id, ...patch }) => ok(sources.update(id, patch as any, "mcp") ?? { error: "not found" }),
  );

  server.registerTool(
    "sources_remove",
    { title: "Remove an ingest source", inputSchema: { id: z.string() } },
    async ({ id }) => ok({ removed: sources.remove(id, "mcp") }),
  );

  server.registerTool(
    "outbox_list",
    { title: "List outbound jobs", inputSchema: { limit: z.number().int().optional() } },
    async ({ limit }) => ok(outbox.list(limit ?? 50)),
  );

  server.registerTool(
    "worker_status",
    { title: "Worker heartbeat + last-run stats", inputSchema: {} },
    async () => ok(workerStatus()),
  );

  return server;
}

/** Handle a raw Node request at /mcp (stateless Streamable HTTP). */
export async function handleMcp(req: IncomingMessage, res: ServerResponse): Promise<void> {
  res.setHeader("access-control-allow-origin", "*");
  res.setHeader("access-control-allow-headers", "content-type, mcp-session-id, mcp-protocol-version");
  res.setHeader("access-control-allow-methods", "POST, OPTIONS");

  if (req.method === "OPTIONS") {
    res.writeHead(204).end();
    return;
  }
  if (req.method !== "POST") {
    res.writeHead(405, { "content-type": "application/json", allow: "POST" });
    res.end(
      JSON.stringify({
        jsonrpc: "2.0",
        error: { code: -32000, message: "Use POST for the stateless MCP endpoint." },
        id: null,
      }),
    );
    return;
  }

  const server = buildMcpServer();
  const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined });
  res.on("close", () => {
    transport.close();
    server.close();
  });
  await server.connect(transport);
  await transport.handleRequest(req, res); // transport reads/parses the body itself
}
