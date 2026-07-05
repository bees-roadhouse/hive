#!/usr/bin/env node
// hive-runner — the control loop that turns a hive "workspace" record into a real,
// running Claude Code session.
//
// Per session it: claims a `provisioning` workspace, provisions an isolated sandbox
// (mkdir + git init), pulls the owner's decrypted credential via the API's internal
// runtime-auth, drives Claude Code with the Agent SDK (cwd = sandbox, bypass
// permissions), and streams every SDK message back to the API ingest endpoint as a
// transcript row. It then watches the transcript for new human inputs and runs each
// as a resumed turn — so the same loop covers one-shot and interactive sessions.
//
// Auth: the runner holds a hive service PAT (HIVE_API_TOKEN, admin) to read sessions,
// fetch runtime-auth, ingest, and set status across owners. The Anthropic credential
// for a given session comes per-owner from the vault; if none is stored, Claude Code
// falls back to the machine's own `claude` login.
//
// DRY_RUN=1 short-circuits the SDK and emits synthetic transcript messages, so the
// full plumbing (claim → sandbox → ingest → status → live UI) can be verified without
// an Anthropic credential or model spend.

import { execFileSync } from "node:child_process";
import { mkdirSync, existsSync, rmSync } from "node:fs";
import { join } from "node:path";

const BASE = (process.env.HIVE_API_URL ?? "http://localhost:7878").replace(/\/+$/, "");
const TOKEN = process.env.HIVE_API_TOKEN ?? "";
const POLL_MS = Number(process.env.HIVE_RUNNER_POLL_MS ?? 2000);
const MODEL = process.env.HIVE_RUNNER_MODEL || undefined;
const DRY_RUN = process.env.HIVE_RUNNER_DRY_RUN === "1";

if (!TOKEN) {
  console.error("hive-runner: HIVE_API_TOKEN (an admin/service PAT) is required.");
  process.exit(1);
}

type Json = Record<string, unknown>;
interface Workspace {
  id: string;
  owner: string;
  title: string;
  workdir: string;
  status: string;
  claude_session_id: string | null;
  runtime?: RuntimeKind | string;
}
type RuntimeKind = "claude_code" | "codex" | "opencode";
interface RuntimeAuth {
  owner: string;
  runtime: RuntimeKind | string;
  provider: string | null;
  kind: string;
  secret: string;
  workdir: string;
}
interface Message {
  seq: number;
  role: string;
  kind: string;
  content: { text?: string; [k: string]: unknown };
}
interface IngestMsg {
  role: string;
  kind: string;
  content: Json;
  raw?: unknown;
  tokens_in?: number | null;
  tokens_out?: number | null;
  claude_session_id?: string | null;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const log = (...a: unknown[]) => console.log(`[runner ${new Date().toISOString()}]`, ...a);

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    ...init,
    headers: { "content-type": "application/json", authorization: `Bearer ${TOKEN}`, ...init?.headers },
  });
  if (!res.ok) throw new Error(`${init?.method ?? "GET"} ${path} → ${res.status} ${await res.text()}`);
  return (res.status === 204 ? undefined : await res.json()) as T;
}

const listWorkspaces = () => api<Workspace[]>("/api/workspaces?limit=200");
const getWorkspace = (id: string) => api<Workspace>(`/api/workspaces/${id}`);
const getTranscript = (id: string) => api<Message[]>(`/api/workspaces/${id}/messages?limit=5000`);
const setStatus = (id: string, status: string) =>
  api(`/api/workspaces/${id}/status`, { method: "POST", body: JSON.stringify({ status }) });
const ingest = (id: string, m: IngestMsg) =>
  api(`/api/workspaces/${id}/messages`, { method: "POST", body: JSON.stringify(m) });

const runtimeOf = (ws: Workspace): RuntimeKind | string => ws.runtime || "claude_code";

async function getRuntimeAuth(ws: Workspace): Promise<RuntimeAuth | null> {
  try {
    return await api<RuntimeAuth>(`/api/workspaces/${ws.id}/runtime-auth`);
  } catch (e) {
    // Claude Code can fall back to an ambient machine login. Codex/OpenCode need
    // an explicit vault credential/config in this server-side contract.
    if (runtimeOf(ws) === "claude_code") {
      log(`no vault credential for ${ws.id} (${(e as Error).message.slice(0, 60)}); using ambient claude login`);
      return null;
    }
    throw e;
  }
}

function requireBinary(cmd: string): void {
  try {
    execFileSync(cmd, ["--version"], { stdio: "ignore" });
  } catch {
    throw new Error(`${cmd} binary is not installed or not on PATH`);
  }
}

function ensureSandbox(workdir: string): void {
  mkdirSync(workdir, { recursive: true });
  mkdirSync(join(workdir, ".claude"), { recursive: true });
  if (!existsSync(join(workdir, ".git"))) {
    try {
      execFileSync("git", ["init", "-q"], { cwd: workdir });
      execFileSync("git", ["commit", "--allow-empty", "-qm", "hive: sandbox init"], {
        cwd: workdir,
        env: { ...process.env, GIT_AUTHOR_NAME: "hive", GIT_AUTHOR_EMAIL: "hive@local", GIT_COMMITTER_NAME: "hive", GIT_COMMITTER_EMAIL: "hive@local" },
      });
    } catch (e) {
      log(`git init warning in ${workdir}: ${(e as Error).message}`);
    }
  }
}

// Map one Agent-SDK message into zero or more transcript rows (lossless `raw`).
function mapSdkMessage(m: Json): IngestMsg[] {
  const type = m.type as string;
  if (type === "system") {
    return [{ role: "system", kind: m.subtype === "init" ? "init" : "text", content: m as Json, raw: m }];
  }
  if (type === "assistant") {
    const blocks = ((m.message as Json)?.content as Json[]) ?? [];
    return blocks.map((b) => {
      const t = b.type as string;
      if (t === "text") return { role: "assistant", kind: "text", content: { text: b.text }, raw: b };
      if (t === "thinking") return { role: "assistant", kind: "thinking", content: { text: b.thinking }, raw: b };
      if (t === "tool_use") return { role: "assistant", kind: "tool_use", content: { name: b.name, input: b.input, id: b.id }, raw: b };
      return { role: "assistant", kind: "text", content: b as Json, raw: b };
    });
  }
  if (type === "user") {
    const blocks = ((m.message as Json)?.content as Json[]) ?? [];
    return blocks.map((b) => ({
      role: "tool",
      kind: "tool_result",
      content: { tool_use_id: b.tool_use_id, output: typeof b.content === "string" ? b.content : JSON.stringify(b.content) },
      raw: b,
    }));
  }
  if (type === "result") {
    const u = (m.usage as Json) ?? {};
    return [
      {
        role: "system",
        kind: "result",
        content: { subtype: m.subtype, cost_usd: m.total_cost_usd, num_turns: m.num_turns, result: m.result },
        raw: m,
        tokens_in: (u.input_tokens as number) ?? null,
        tokens_out: (u.output_tokens as number) ?? null,
      },
    ];
  }
  return [{ role: "system", kind: "text", content: { type }, raw: m }];
}

// Run a single turn against Claude Code; stream messages to ingest; return session id.
async function runTurn(ws: Workspace, prompt: string, auth: RuntimeAuth | null, resume: string | null): Promise<string | null> {
  if (DRY_RUN) return dryTurn(ws, prompt, resume);

  const runtime = runtimeOf(ws);
  if (runtime === "codex") return runCodexTurn(ws, prompt, auth, resume);
  if (runtime === "opencode") return runOpenCodeTurn(ws, prompt, auth, resume);
  if (runtime !== "claude_code") throw new Error(`unsupported runtime: ${runtime}`);

  const env: Record<string, string> = { ...process.env } as Record<string, string>;
  if (auth?.secret) {
    if (auth.kind === "api_key") env.ANTHROPIC_API_KEY = auth.secret;
    else env.CLAUDE_CODE_OAUTH_TOKEN = auth.secret;
  }
  env.CLAUDE_CONFIG_DIR = join(ws.workdir, ".claude");

  const { query } = await import("@anthropic-ai/claude-agent-sdk");
  const q = query({
    prompt,
    options: {
      cwd: ws.workdir,
      permissionMode: "bypassPermissions",
      ...(MODEL ? { model: MODEL } : {}),
      ...(resume ? { resume } : {}),
      env,
    },
  } as Parameters<typeof query>[0]);

  let sid = resume;
  for await (const m of q as AsyncIterable<Json>) {
    sid = (m.session_id as string) ?? sid;
    for (const row of mapSdkMessage(m)) {
      await ingest(ws.id, { ...row, claude_session_id: sid });
    }
  }
  return sid;
}

async function runCodexTurn(ws: Workspace, _prompt: string, auth: RuntimeAuth | null, resume: string | null): Promise<string | null> {
  requireBinary("codex");
  if (!auth?.secret) throw new Error("codex runtime requires a saved codex credential");
  await ingest(ws.id, {
    role: "system",
    kind: "init",
    content: { subtype: "init", runtime: "codex", provider: auth.provider, model: MODEL ?? null, cwd: ws.workdir },
    claude_session_id: resume,
  });
  throw new Error("codex runtime execution is not implemented yet; credential plumbing is ready");
}

async function runOpenCodeTurn(ws: Workspace, _prompt: string, auth: RuntimeAuth | null, resume: string | null): Promise<string | null> {
  requireBinary("opencode");
  if (!auth?.secret) throw new Error("opencode runtime requires a saved opencode provider/model credential");
  await ingest(ws.id, {
    role: "system",
    kind: "init",
    content: { subtype: "init", runtime: "opencode", provider: auth.provider, model: MODEL ?? null, cwd: ws.workdir },
    claude_session_id: resume,
  });
  throw new Error("opencode runtime execution is not implemented yet; credential plumbing is ready");
}

async function dryTurn(ws: Workspace, prompt: string, resume: string | null): Promise<string> {
  const sid = resume ?? "dry-" + Math.random().toString(36).slice(2, 10);
  const send = (m: IngestMsg) => ingest(ws.id, { ...m, claude_session_id: sid });
  await send({ role: "system", kind: "init", content: { subtype: "init", runtime: runtimeOf(ws), model: "dry-run", cwd: ws.workdir } });
  await sleep(250);
  await send({ role: "assistant", kind: "thinking", content: { text: "(dry-run) reading the request…" } });
  await sleep(250);
  await send({ role: "assistant", kind: "tool_use", content: { name: "Bash", input: { command: "ls -1" } } });
  await send({ role: "tool", kind: "tool_result", content: { output: "README.md\nCargo.toml\npackages/" } });
  await sleep(250);
  await send({ role: "assistant", kind: "text", content: { text: `(dry-run) You said: "${prompt}". I would do this in ${ws.workdir}.` } });
  await send({ role: "system", kind: "result", content: { subtype: "success", note: "dry-run complete" }, tokens_in: 0, tokens_out: 0 });
  return sid;
}

// Drive a session from claim through every turn until it is archived.
async function drive(ws: Workspace): Promise<void> {
  log(`claim ${ws.id} (owner=${ws.owner}, runtime=${runtimeOf(ws)}, dir=${ws.workdir})`);
  await setStatus(ws.id, "running");
  ensureSandbox(ws.workdir);
  let auth: RuntimeAuth | null = null;
  try {
    auth = await getRuntimeAuth(ws);
  } catch (e) {
    log(`runtime auth failed: ${(e as Error).message}`);
    await ingest(ws.id, { role: "system", kind: "error", content: { error: (e as Error).message } });
    await setStatus(ws.id, "failed");
    return;
  }
  let sid: string | null = ws.claude_session_id;
  let lastInputSeq = 0;
  let archived = false;

  while (true) {
    let fresh: Workspace;
    try {
      fresh = await getWorkspace(ws.id);
    } catch {
      break;
    }
    if (!fresh || fresh.status === "archived") {
      archived = fresh?.status === "archived";
      break;
    }

    const transcript = await getTranscript(ws.id);
    const inputs = transcript.filter((m) => m.role === "user" && m.kind === "input" && m.seq > lastInputSeq);

    if (inputs.length > 0) {
      for (const inp of inputs) {
        lastInputSeq = inp.seq;
        const prompt = (inp.content?.text as string) ?? "";
        log(`turn ${ws.id} #${inp.seq}: ${prompt.slice(0, 60)}`);
        await setStatus(ws.id, "running");
        try {
          sid = await runTurn(ws, prompt, auth, sid);
        } catch (e) {
          log(`turn failed: ${(e as Error).message}`);
          await ingest(ws.id, { role: "system", kind: "error", content: { error: (e as Error).message }, claude_session_id: sid });
          await setStatus(ws.id, "failed");
          return;
        }
      }
      await setStatus(ws.id, "idle");
    }
    await sleep(POLL_MS);
  }
  // Teardown: archived sessions get their throwaway sandbox removed.
  if (archived) {
    try {
      rmSync(ws.workdir, { recursive: true, force: true });
      log(`done ${ws.id} (archived) · removed sandbox ${ws.workdir}`);
    } catch (e) {
      log(`done ${ws.id} (archived) · teardown warning: ${(e as Error).message}`);
    }
  } else {
    log(`done ${ws.id}`);
  }
}

// ---- top-level poll loop ----
const active = new Set<string>();

async function tick(): Promise<void> {
  let list: Workspace[];
  try {
    list = await listWorkspaces();
  } catch (e) {
    log(`poll error: ${(e as Error).message}`);
    return;
  }
  for (const ws of list) {
    if (active.has(ws.id)) continue;
    if (ws.status === "provisioning") {
      active.add(ws.id);
      drive(ws)
        .catch((e) => log(`driver crashed for ${ws.id}: ${(e as Error).message}`))
        .finally(() => active.delete(ws.id));
    }
  }
}

async function main(): Promise<void> {
  log(`hive-runner up · api=${BASE} · poll=${POLL_MS}ms · dryRun=${DRY_RUN} · model=${MODEL ?? "default"}`);
  // eslint-disable-next-line no-constant-condition
  while (true) {
    await tick();
    await sleep(POLL_MS);
  }
}

main().catch((e) => {
  console.error("hive-runner fatal:", e);
  process.exit(1);
});
