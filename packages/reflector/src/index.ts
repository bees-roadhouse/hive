#!/usr/bin/env node
// hive-reflector — turns captured conversations into durable journal memory.
//
// Drains the reflection queue for ONE identity (the HIVE_API_TOKEN):
//   GET  /api/conversations/pending        -> captured sessions, reflected_at IS NULL
//   GET  /api/conversations/{id}           -> transcript flattened to text
//   (LLM reflect)                          -> journal narrative + proposed tasks/decisions
//   POST /api/journal                      -> durable memory (anchors only in `auto` mode)
//   POST /api/conversations/{id}/reflected -> store rolling summary, drain the queue
//
// REFLECTION_MODE (default `suggest`):
//   off     — do nothing, exit immediately (the identity opted out).
//   suggest — journal narrative + a plain "Proposed follow-ups" prose section,
//             tags ['reflection','suggestion'], NO anchors: nothing is
//             auto-created, a human reviews. Summary still stored.
//   auto    — additionally anchor the tasks/decisions so hive materializes them.
//
// Privacy invariant (the plan's belt; the server-side journal mail-scope guard
// is the suspenders): reflection memory must be private by construction. At
// startup the token is verified against GET /api/auth/me — a public route that
// returns { user, principal } and resolves an invalid/expired Bearer to
// principal: null instead of a 401. A null principal is a namespace-less
// (anon) caller, so the reflector refuses to run. A resolved token principal
// is namespaced by construction on the server (tokens_resolve maps every
// token to granted_by || created_by, and POST /api/journal stamps that as
// user_scope) — and we re-assert it on every write: a journal response
// missing user_scope aborts the loop before the conversation is drained.
//
// Runs forever on REFLECTION_INTERVAL_SECS (compose service); `--once` drains
// what's pending and exits (cron / smoke tests).

const BASE = (process.env.HIVE_API_URL ?? process.env.HIVE_URL ?? "http://localhost:7878").replace(/\/+$/, "");
const TOKEN = (process.env.HIVE_API_TOKEN ?? process.env.HIVE_TOKEN ?? "").trim();
const MODE = (process.env.REFLECTION_MODE ?? "suggest").trim().toLowerCase();
const MODEL = process.env.REFLECTION_MODEL || "claude-sonnet-5";
const ANTHROPIC_KEY = (process.env.ANTHROPIC_API_KEY ?? "").trim();
// Test seam / gateway override; the reflector only ever calls ${base}/v1/messages.
const ANTHROPIC_BASE = (process.env.ANTHROPIC_BASE_URL ?? "https://api.anthropic.com").replace(/\/+$/, "");
const INTERVAL_MS = Number(process.env.REFLECTION_INTERVAL_SECS ?? 300) * 1000;
const BATCH = Number(process.env.REFLECTION_BATCH ?? 20);
const MAX_CHARS = Number(process.env.REFLECTION_MAX_CHARS ?? 200_000);
const MAX_TOKENS = Number(process.env.REFLECTION_MAX_TOKENS ?? 4096);
const ONCE = process.argv.includes("--once");

const log = (msg: string) => console.error(`reflector: ${msg}`);

// ---- hive API shapes (mirror hive_shared) ----

interface Conversation {
  id: string;
  owner: string;
  title: string;
  runtime: string;
  summary: string;
  reflected_at: string | null;
}
interface ConversationMessageFlat {
  seq: number;
  role: string;
  kind: string;
  content: string;
}
interface ConversationView extends Conversation {
  messages: ConversationMessageFlat[];
}
interface AuthMe {
  user: { actor: string; name: string } | null;
  principal: string | null;
}
interface NewAnchor {
  start: number;
  end: number;
  kind: "task" | "decision";
}
interface Reflection {
  summary?: string;
  narrative?: string;
  tasks?: { title?: string }[];
  decisions?: { text?: string }[];
}

// ---- hive client ----

async function hive<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(BASE + path, {
    method,
    headers: {
      authorization: `Bearer ${TOKEN}`,
      "content-type": "application/json",
      accept: "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`hive ${method} ${path} -> ${res.status} ${text.slice(0, 400)}`);
  }
  return (text ? JSON.parse(text) : null) as T;
}

/**
 * Startup namespace guard. GET /api/auth/me is public: a bad token comes back
 * as { user: null, principal: null } rather than a 401, so a null principal
 * means the Bearer resolves to NO principal (anon = namespace-less). Refuse —
 * reflection memory must land in a per-user namespace, never the global feed.
 */
async function assertNamespacedPrincipal(): Promise<void> {
  let me: AuthMe;
  try {
    me = await hive<AuthMe>("GET", "/api/auth/me");
  } catch (err) {
    log(`cannot verify HIVE_API_TOKEN against ${BASE}/api/auth/me: ${(err as Error).message}`);
    process.exit(1);
  }
  if (!me || typeof me.principal !== "string" || me.principal.length === 0) {
    log("HIVE_API_TOKEN does not resolve to an authenticated principal (namespace-less).");
    log("Refusing to run: reflection memory must be private by construction — mint a token for the AI identity (its namespace is the granting human's).");
    process.exit(1);
  }
  const who = me.user ? `${me.user.actor}` : "an AI identity (no matching login user)";
  log(`token ok: principal=${me.principal}, acting as ${who}`);
}

// ---- Anthropic Messages client (zero-dep raw fetch) ----

async function llm(system: string, user: string): Promise<string> {
  const res = await fetch(`${ANTHROPIC_BASE}/v1/messages`, {
    method: "POST",
    headers: {
      "x-api-key": ANTHROPIC_KEY,
      "anthropic-version": "2023-06-01",
      "content-type": "application/json",
    },
    // No `thinking` config: REFLECTION_MODEL is operator-chosen and explicit
    // thinking configs are rejected by some models; omitting works on all.
    body: JSON.stringify({
      model: MODEL,
      max_tokens: MAX_TOKENS,
      system,
      messages: [{ role: "user", content: user }],
    }),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`anthropic -> ${res.status} ${text.slice(0, 400)}`);
  const body = JSON.parse(text) as { content?: { type: string; text?: string }[] };
  return (body.content ?? [])
    .filter((b) => b.type === "text")
    .map((b) => b.text ?? "")
    .join("");
}

/** Extract the first JSON object from a model response (tolerates ```json fences). */
function parseJsonObject(s: string): Reflection {
  const fenced = s.match(/```(?:json)?\s*([\s\S]*?)```/);
  const raw = fenced ? fenced[1] : s;
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start === -1 || end === -1) throw new Error("no JSON object in model output");
  return JSON.parse(raw.slice(start, end + 1)) as Reflection;
}

// ---- reflection ----

const SYSTEM = `You are a reflection process for an AI's long-term memory ("hive").
You receive a transcript of a past session the AI had. Produce a faithful,
concise reflection. Return ONLY a JSON object, no prose, with this shape:
{
  "summary": "1-3 sentence rolling summary of what happened (durable memory cue)",
  "narrative": "a short markdown reflection: what was discussed, decided, learned",
  "tasks": [{"title": "actionable follow-up, imperative, <100 chars"}],
  "decisions": [{"text": "a decision that was made, stated as a fact"}]
}
Only include tasks/decisions that genuinely occurred. Empty arrays are fine.
Never invent. Write in the AI's own voice (first person).`;

function transcriptToText(view: ConversationView): string {
  return (view.messages ?? [])
    .map((m) => `### ${m.role}\n${m.content}`)
    .join("\n\n");
}

/**
 * Assemble the journal body and (in auto mode) the anchors. Anchor offsets
 * are UTF-16 code units — exactly what JS string indexing yields, so
 * indexOf/length match hive's `js_slice_utf16` semantics natively.
 */
function buildEntry(reflection: Reflection, anchored: boolean): { body: string; anchors?: NewAnchor[] } {
  const tasks = Array.isArray(reflection.tasks) ? reflection.tasks : [];
  const decisions = Array.isArray(reflection.decisions) ? reflection.decisions : [];
  let body = (reflection.narrative ?? "").trim();
  const anchors: NewAnchor[] = [];

  const section = (heading: string, items: { title?: string; text?: string }[], kind: NewAnchor["kind"]) => {
    if (!items.length) return;
    body += `\n\n## ${heading}\n`;
    for (const it of items) {
      const label = (it.title ?? it.text ?? "").trim();
      if (!label) continue;
      const line = `- ${label}\n`;
      const at = body.length + 2; // after "- "
      body += line;
      if (anchored) {
        anchors.push({ start: at, end: at + label.length, kind });
      }
    }
  };

  section(anchored ? "Follow-ups" : "Proposed follow-ups", tasks, "task");
  section(anchored ? "Decisions" : "Proposed decisions", decisions, "decision");

  return { body, anchors: anchored ? anchors : undefined };
}

interface ReflectResult {
  id: string;
  skipped?: string;
  tasks?: number;
  decisions?: number;
  anchored?: boolean;
}

async function reflectOne(convo: Conversation): Promise<ReflectResult> {
  const view = await hive<ConversationView>("GET", `/api/conversations/${convo.id}`);
  if (!view || !(view.messages ?? []).length) {
    // Nothing to reflect on — still drain it so it doesn't loop forever.
    await hive("POST", `/api/conversations/${convo.id}/reflected`, { summary: "" });
    return { id: convo.id, skipped: "empty" };
  }

  const text = transcriptToText(view);
  if (text.length > MAX_CHARS) {
    // Cost guard: drain oversize transcripts with a note instead of an LLM call.
    await hive("POST", `/api/conversations/${convo.id}/reflected`, {
      summary: `Reflection skipped: transcript is ${text.length} chars, over REFLECTION_MAX_CHARS=${MAX_CHARS}.`,
    });
    return { id: convo.id, skipped: "oversize" };
  }

  const out = await llm(SYSTEM, `Transcript of session "${view.title || convo.id}":\n\n${text}`);
  const reflection = parseJsonObject(out);

  const anchored = MODE === "auto";
  const { body, anchors } = buildEntry(reflection, anchored);

  if (body.trim()) {
    const tags = anchored ? ["reflection"] : ["reflection", "suggestion"];
    const entry = await hive<{ id: string; user_scope: string | null }>("POST", "/api/journal", {
      body,
      tags,
      anchors,
    });
    // Belt-and-suspenders re-assert of the startup guard: a token write must
    // come back owner-scoped. A null scope means the entry landed in the
    // GLOBAL feed — stop immediately (conversation stays pending) so an
    // exfiltration loop can't run unattended.
    if (!entry || typeof entry.user_scope !== "string" || entry.user_scope.length === 0) {
      log(`FATAL: journal entry ${entry?.id ?? "?"} landed without an owner scope — refusing to continue.`);
      process.exit(1);
    }
  }

  await hive("POST", `/api/conversations/${convo.id}/reflected`, {
    summary: (reflection.summary ?? "").trim(),
  });

  return {
    id: convo.id,
    tasks: (reflection.tasks ?? []).length,
    decisions: (reflection.decisions ?? []).length,
    anchored,
  };
}

async function onePass(): Promise<number> {
  const pending = (await hive<Conversation[]>("GET", `/api/conversations/pending?limit=${BATCH}`)) ?? [];
  if (!pending.length) {
    log("queue empty");
    return 0;
  }
  log(`${pending.length} pending (mode=${MODE}, model=${MODEL})`);
  for (const convo of pending) {
    try {
      const r = await reflectOne(convo);
      log(`${convo.id} -> ${JSON.stringify(r)}`);
    } catch (err) {
      // Per-conversation isolation: one bad transcript logs + stays pending
      // (a later pass retries); it never takes the whole drain down.
      log(`${convo.id} FAILED: ${(err as Error).message}`);
    }
  }
  return pending.length;
}

async function main(): Promise<void> {
  if (MODE === "off") {
    log("REFLECTION_MODE=off — nothing to do.");
    process.exit(0);
  }
  if (MODE !== "suggest" && MODE !== "auto") {
    log(`unknown REFLECTION_MODE '${MODE}' (expected off | suggest | auto).`);
    process.exit(1);
  }
  if (!TOKEN) {
    log("HIVE_API_TOKEN is required (the AI identity's hive PAT).");
    process.exit(1);
  }
  if (!ANTHROPIC_KEY) {
    log("ANTHROPIC_API_KEY is required (per-token billing; the surface meant for automation).");
    process.exit(1);
  }

  await assertNamespacedPrincipal();

  if (ONCE) {
    await onePass();
    return;
  }
  log(`watching ${BASE} every ${INTERVAL_MS / 1000}s`);
  for (;;) {
    try {
      await onePass();
    } catch (err) {
      // Queue-level failures (hive down, network) also just wait for the
      // next tick — restart:unless-stopped semantics without the restart.
      log(`pass failed: ${(err as Error).message}`);
    }
    await new Promise((r) => setTimeout(r, INTERVAL_MS));
  }
}

main().catch((err) => {
  log(`fatal: ${(err as Error).message}`);
  process.exit(1);
});
