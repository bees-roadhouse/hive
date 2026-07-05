#!/usr/bin/env node
// Shared adapter for AI runtime plugins. It keeps Claude Code, Codex, and
// Hermes wrappers on one contract: session-start recall in, journal prose out.

import type { JournalEntryView, RecallResult } from "@hive/shared";

const BASE = (process.env.HIVE_API_URL ?? "http://localhost:7878").replace(/\/+$/, "");
const TOKEN = process.env.HIVE_API_TOKEN ?? "";
const DEFAULT_IDENTITY = process.env.HIVE_IDENTITY ?? process.env.HIVE_ACTOR ?? "pia";
const DEFAULT_PEER = process.env.HIVE_PEER;
const DEFAULT_BUDGET = Number(process.env.HIVE_RECALL_BUDGET ?? 1500);
const DEFAULT_THRESHOLD = Number(process.env.HIVE_RECALL_THRESHOLD ?? 0.72);

type Flags = Record<string, string | boolean>;

function flags(args: string[]): Flags {
  const out: Flags = {};
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (!a.startsWith("--")) continue;
    const eq = a.indexOf("=");
    if (eq !== -1) out[a.slice(2, eq)] = a.slice(eq + 1);
    else if (args[i + 1] && !args[i + 1].startsWith("--")) out[a.slice(2)] = args[++i];
    else out[a.slice(2)] = true;
  }
  return out;
}

function positional(args: string[]): string[] {
  const out: string[] = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (!a.startsWith("--")) {
      out.push(a);
      continue;
    }
    if (!a.includes("=") && args[i + 1] && !args[i + 1].startsWith("--")) i++;
  }
  return out;
}

function str(f: Flags, key: string, fallback?: string): string | undefined {
  const v = f[key];
  return typeof v === "string" && v.trim() ? v : fallback;
}

function num(f: Flags, key: string, fallback: number): number {
  const raw = f[key];
  const n = typeof raw === "string" ? Number(raw) : NaN;
  return Number.isFinite(n) ? n : fallback;
}

async function api<T>(path: string, init?: RequestInit, timeoutMs = 20000): Promise<T> {
  if (!TOKEN) {
    throw new Error("HIVE_API_TOKEN is required. Mint a long-lived or never-expiring token in Hive.");
  }
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(new Error("request timed out")), timeoutMs);
  try {
    const res = await fetch(`${BASE}${path}`, {
      ...init,
      signal: ctrl.signal,
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${TOKEN}`,
        ...init?.headers,
      },
    });
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    return (res.status === 204 ? undefined : await res.json()) as T;
  } finally {
    clearTimeout(timer);
  }
}

async function readStdin(): Promise<string> {
  if (process.stdin.isTTY) return "";
  const chunks: Buffer[] = [];
  for await (const chunk of process.stdin) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks).toString("utf8");
}

function nowLine(timeZone?: string): string {
  const date = new Date();
  const zone = timeZone || Intl.DateTimeFormat().resolvedOptions().timeZone || "local";
  const human = new Intl.DateTimeFormat("en-US", {
    dateStyle: "full",
    timeStyle: "long",
    timeZone: zone,
  }).format(date);
  return `${human} (${zone}; ${date.toISOString()})`;
}

function snippet(body: string, max = 240): string {
  const oneLine = body.replace(/\s+/g, " ").trim();
  return oneLine.length <= max ? oneLine : `${oneLine.slice(0, max - 3)}...`;
}

function renderRecent(entries: JournalEntryView[]): string {
  if (!entries.length) return "## Last journal entries\nNo visible journal entries yet.";
  return [
    "## Last journal entries",
    ...entries.map((e) => `- ${e.created_at} - ${e.author}: ${snippet(e.body)}`),
  ].join("\n");
}

function renderSessionBlock(args: {
  identity: string;
  peer?: string;
  threshold?: number;
  generatedAt: string;
  recent: JournalEntryView[];
  recall: RecallResult;
}): string {
  const semanticCount = args.recall.data.journal.length;
  return [
    "# Hive Session Memory",
    `Generated: ${args.generatedAt}`,
    `AI identity: ${args.identity}`,
    args.peer ? `Session peer/user: ${args.peer}` : undefined,
    args.threshold === undefined
      ? undefined
      : `Semantic journal cutoff: score >= ${args.threshold} (${semanticCount} high-confidence hit${semanticCount === 1 ? "" : "s"})`,
    "",
    "This block is already injected from Hive. Do not make a startup memory call just to rediscover it.",
    "",
    renderRecent(args.recent),
    "",
    args.recall.brief,
    "",
    "## Memory write protocol",
    "- Save durable memory as first-person journal prose, not terse key-value notes.",
    "- Include concrete names, dates, decisions, feelings, context, and why the memory matters.",
    "- Write as the authenticated AI identity; Hive takes authorship from the token.",
    "- Mention humans or AIs with @name when the entry should be shared into their visible journal.",
  ]
    .filter((part) => part !== undefined)
    .join("\n");
}

async function sessionStart(args: string[]): Promise<void> {
  const f = flags(args);
  const identity = str(f, "identity", DEFAULT_IDENTITY)!;
  const peer = str(f, "peer", DEFAULT_PEER);
  const query = str(f, "query", positional(args).join(" "));
  const budget = Math.max(1, Math.floor(num(f, "budget", DEFAULT_BUDGET)));
  const threshold = f["no-threshold"] ? undefined : num(f, "threshold", DEFAULT_THRESHOLD);
  const timeZone = str(f, "timezone", process.env.TZ);

  const [recall, recent] = await Promise.all([
    api<RecallResult>("/api/recall", {
      method: "POST",
      body: JSON.stringify({ identity, peer, query, budget, threshold }),
    }),
    api<JournalEntryView[]>("/api/journal?limit=3"),
  ]);

  process.stdout.write(
    renderSessionBlock({
      identity,
      peer,
      threshold,
      generatedAt: nowLine(timeZone),
      recent,
      recall,
    }),
  );
}

async function journalAdd(args: string[]): Promise<void> {
  const f = flags(args);
  const fromArgs = positional(args).join(" ");
  // Only block on stdin when no body was supplied via --body or positional prose —
  // otherwise a non-EOF stdin (hook/agent context, no TTY) hangs the process forever.
  const provided = str(f, "body", fromArgs.trim() ? fromArgs : undefined);
  const body = (provided ?? (await readStdin()))?.trim();
  if (!body) throw new Error("journal-add needs --body, positional prose, or stdin.");
  const title = str(f, "title");
  const tags = str(f, "tags")
    ?.split(",")
    .map((t) => t.trim())
    .filter(Boolean);
  const fullBody = title ? `# ${title}\n\n${body}` : body;
  const entry = await api<JournalEntryView>("/api/journal", {
    method: "POST",
    body: JSON.stringify({ body: fullBody, tags }),
  });
  process.stdout.write(JSON.stringify({ id: entry.id, author: entry.author, created_at: entry.created_at }, null, 2));
}

async function mcpSmoke(): Promise<void> {
  const res = await fetch(`${BASE}/mcp`, {
    method: "POST",
    headers: {
      accept: "application/json, text/event-stream",
      "content-type": "application/json",
      authorization: `Bearer ${TOKEN}`,
      "mcp-protocol-version": "2025-06-18",
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
  });
  const body = await res.json();
  if (!res.ok || body.error) throw new Error(`${res.status} ${JSON.stringify(body)}`);
  process.stdout.write(
    JSON.stringify(
      {
        ok: true,
        tools: body.result?.tools?.length ?? 0,
        firstTools: body.result?.tools?.slice(0, 5).map((t: { name: string }) => t.name) ?? [],
      },
      null,
      2,
    ),
  );
}

const HELP = `hive-agent

  hive-agent session-start [--identity pia] [--peer nate] [--query "..."]
      Print a ready-to-inject session memory block: current date/time, last
      visible journal entries, high-confidence semantic recall, and write rules.

  hive-agent journal-add [--title "..."] [--tags=a,b] <prose...>
      Save rich journal prose as the authenticated AI identity. Also accepts stdin.

  hive-agent mcp-smoke
      Verify the Bearer token can call POST /mcp tools/list.

env: HIVE_API_URL (${BASE}), HIVE_API_TOKEN, HIVE_IDENTITY (${DEFAULT_IDENTITY}), HIVE_PEER`;

async function main(): Promise<void> {
  const [cmd, ...rest] = process.argv.slice(2);
  switch (cmd) {
    case "session-start":
    case "start":
      return sessionStart(rest);
    case "journal-add":
    case "journal":
      return journalAdd(rest);
    case "mcp-smoke":
      return mcpSmoke();
    default:
      process.stdout.write(HELP);
  }
}

main().catch((e) => {
  console.error(`hive-agent: ${e instanceof Error ? e.message : String(e)}`);
  process.exit(1);
});
