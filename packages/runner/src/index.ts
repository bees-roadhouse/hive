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

import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, existsSync, rmSync, writeFileSync, mkdtempSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";

const BASE = (process.env.HIVE_API_URL ?? "http://localhost:7878").replace(/\/+$/, "");
const TOKEN = process.env.HIVE_API_TOKEN ?? process.env.HIVE_RUNNER_TOKEN ?? "";
const POLL_MS = Number(process.env.HIVE_RUNNER_POLL_MS ?? 2000);
const MODEL = process.env.HIVE_RUNNER_MODEL || undefined;
const DRY_RUN = process.env.HIVE_RUNNER_DRY_RUN === "1";
const SESSION_ISOLATION = (process.env.HIVE_SESSION_ISOLATION ?? "container") !== "host";
const ENGINE_PREF = process.env.HIVE_CONTAINER_ENGINE ?? "auto";
const SESSION_IMAGE = process.env.HIVE_SESSION_IMAGE ?? process.env.HIVE_RUNNER_IMAGE ?? "beesroadhouse/hive-session-dev:latest";
const USER_VOLUME_PREFIX = process.env.HIVE_USER_VOLUME_PREFIX ?? "hive-user";
const SESSION_CONTAINER_PREFIX = process.env.HIVE_SESSION_CONTAINER_PREFIX ?? "hive-session";
const SESSION_NETWORK = process.env.HIVE_SESSION_NETWORK ?? "";
const ENGINE_SOCKET_TARGET = process.env.HIVE_CONTAINER_SOCKET_TARGET ?? (ENGINE_PREF === "docker" ? "/var/run/docker.sock" : "/run/podman/podman.sock");
const PROPAGATE_SOCKET = process.env.HIVE_PROPAGATE_ENGINE_SOCKET === "1";
const SESSION_GUI = process.env.HIVE_SESSION_GUI ?? "1";
const SESSION_VNC = process.env.HIVE_SESSION_VNC ?? "0";
const SESSION_VNC_PORT = process.env.HIVE_SESSION_VNC_PORT ?? "5900";
const SESSION_NOVNC_PORT = process.env.HIVE_SESSION_NOVNC_PORT ?? "6080";

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
  model?: string | null;
}
type RuntimeKind = "claude_code" | "codex" | "opencode";
interface RuntimeAuth {
  owner: string;
  runtime: RuntimeKind | string;
  provider: string | null;
  model?: string | null;
  kind: string;
  secret: string;
  workdir: string;
}
interface SessionContainer {
  engine: "podman" | "docker";
  name: string;
  userVolume: string;
  workspaceDir: string;
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

function safeName(s: string): string {
  return s.toLowerCase().replace(/[^a-z0-9_.-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 40) || "user";
}

function shortHash(s: string): string {
  return createHash("sha256").update(s).digest("hex").slice(0, 10);
}

function engineEnv(cmd: "podman" | "docker"): NodeJS.ProcessEnv {
  if (cmd === "podman") return { ...process.env, CONTAINER_HOST: `unix://${ENGINE_SOCKET_TARGET}` };
  return process.env;
}

function commandWorks(cmd: "podman" | "docker"): boolean {
  const r = spawnSync(cmd, ["--version"], { stdio: "ignore", env: engineEnv(cmd) });
  return r.status === 0;
}

function resolveEngine(): "podman" | "docker" | null {
  if (!SESSION_ISOLATION || ENGINE_PREF === "none" || ENGINE_PREF === "host") return null;
  const wanted = ENGINE_PREF === "auto" ? ["podman", "docker"] : [ENGINE_PREF];
  for (const candidate of wanted) {
    if ((candidate === "podman" || candidate === "docker") && commandWorks(candidate)) return candidate;
  }
  throw new Error("HIVE_SESSION_ISOLATION=container but no podman/docker CLI is available");
}

const ENGINE = resolveEngine();

// Safety interlock: without a session container (HIVE_SESSION_ISOLATION=host, or
// HIVE_CONTAINER_ENGINE=none/host), Claude Code turns run with bypassPermissions
// DIRECTLY on this machine — full read/write/exec as the runner user, driven by
// prompts and by whatever untrusted content the session pulls in. Bypass is only
// safe because sessions normally live in disposable per-session containers.
// Refuse to start unless the operator explicitly accepts the risk.
if (!ENGINE && !DRY_RUN && process.env.HIVE_RUNNER_UNSAFE_HOST !== "1") {
  console.error(
    "hive-runner: refusing to start — HIVE_SESSION_ISOLATION=host (no session container) would run agent " +
      "turns with permissions bypassed directly on this machine. Use container isolation (the default, " +
      "requires podman or docker), or set HIVE_RUNNER_UNSAFE_HOST=1 to accept running unsandboxed.",
  );
  process.exit(1);
}

function engine(args: string[], opts: { input?: string; quiet?: boolean } = {}): string {
  if (!ENGINE) throw new Error("container engine unavailable");
  const r = spawnSync(ENGINE, args, { encoding: "utf8", input: opts.input, stdio: opts.quiet ? "pipe" : "pipe", env: engineEnv(ENGINE) });
  if (r.status !== 0) {
    const err = (r.stderr || r.stdout || "").trim();
    throw new Error(`${ENGINE} ${args.join(" ")} failed${err ? `: ${err}` : ""}`);
  }
  return (r.stdout || "").trim();
}

function inspectOk(kind: "container" | "volume", name: string): boolean {
  if (!ENGINE) return false;
  const r = spawnSync(ENGINE, [kind, "inspect", name], { stdio: "ignore", env: engineEnv(ENGINE) });
  return r.status === 0;
}

function userVolumeName(owner: string): string {
  return `${USER_VOLUME_PREFIX}-${safeName(owner)}-${shortHash(owner)}`;
}

function containerName(ws: Workspace): string {
  return `${SESSION_CONTAINER_PREFIX}-${safeName(ws.owner)}-${shortHash(ws.owner)}-${safeName(ws.id)}`;
}

function socketMountArgs(): string[] {
  if (!PROPAGATE_SOCKET || !ENGINE) return [];
  if (ENGINE === "podman") return ["--volume", `${ENGINE_SOCKET_TARGET}:${ENGINE_SOCKET_TARGET}`];
  return ["--volume", `${ENGINE_SOCKET_TARGET}:${ENGINE_SOCKET_TARGET}`];
}

function ensureSessionContainer(ws: Workspace, auth: RuntimeAuth | null): SessionContainer | null {
  if (!ENGINE) return null;
  const vol = userVolumeName(ws.owner);
  const name = containerName(ws);
  const workspaceDir = `/workspace/${safeName(ws.id)}`;
  if (!inspectOk("volume", vol)) engine(["volume", "create", vol], { quiet: true });
  if (!inspectOk("container", name)) {
    const args = [
      "run", "-d", "--name", name,
      "--label", "hive.managed=true",
      "--label", `hive.owner=${ws.owner}`,
      "--label", `hive.session=${ws.id}`,
      "--volume", `${vol}:/workspace:rw`,
      ...(SESSION_NETWORK ? ["--network", SESSION_NETWORK] : []),
      ...socketMountArgs(),
      "--env", `HIVE_API_URL=${BASE}`,
      "--env", `HIVE_SESSION_ID=${ws.id}`,
      "--env", `HIVE_SESSION_OWNER=${ws.owner}`,
      "--env", `HIVE_RUNTIME=${runtimeOf(ws)}`,
      "--env", `HIVE_RUNTIME_PROVIDER=${auth?.provider ?? ""}`,
      "--env", `HIVE_RUNTIME_MODEL=${auth?.model ?? ws.model ?? MODEL ?? ""}`,
      "--env", `HIVE_GUI=${SESSION_GUI}`,
      "--env", `HIVE_VNC=${SESSION_VNC}`,
      "--env", `HIVE_VNC_PORT=${SESSION_VNC_PORT}`,
      "--env", `HIVE_NOVNC_PORT=${SESSION_NOVNC_PORT}`,
      SESSION_IMAGE,
      "sh", "-lc", `mkdir -p ${workspaceDir} && tail -f /dev/null`,
    ];
    engine(args, { quiet: true });
    log(`session container ${name} up · owner=${ws.owner} volume=${vol} image=${SESSION_IMAGE}`);
  }
  return { engine: ENGINE, name, userVolume: vol, workspaceDir };
}

function removeSessionContainer(ws: Workspace): void {
  if (!ENGINE) return;
  const name = containerName(ws);
  if (inspectOk("container", name)) engine(["rm", "-f", name], { quiet: true });
}

function writePromptFile(container: SessionContainer, prompt: string): string {
  const dir = mkdtempSync(join(tmpdir(), "hive-prompt-"));
  const local = join(dir, "prompt.txt");
  writeFileSync(local, prompt);
  engine(["cp", local, `${container.name}:${container.workspaceDir}/prompt.txt`], { quiet: true });
  return `${container.workspaceDir}/prompt.txt`;
}

function runInSession(container: SessionContainer, args: string[], env: Record<string, string> = {}): string {
  const envArgs = Object.entries(env).flatMap(([k, v]) => ["--env", `${k}=${v}`]);
  return engine(["exec", "--workdir", container.workspaceDir, ...envArgs, container.name, ...args], { quiet: true });
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
async function runTurn(
  ws: Workspace,
  prompt: string,
  auth: RuntimeAuth | null,
  resume: string | null,
  container: SessionContainer | null,
): Promise<string | null> {
  if (DRY_RUN) return dryTurn(ws, prompt, resume);

  const runtime = runtimeOf(ws);
  if (runtime === "codex") return runCodexTurn(ws, prompt, auth, resume, container);
  if (runtime === "opencode") return runOpenCodeTurn(ws, prompt, auth, resume, container);
  if (runtime !== "claude_code") throw new Error(`unsupported runtime: ${runtime}`);
  if (container) return runClaudeCodeContainerTurn(ws, prompt, auth, resume, container);

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

async function runClaudeCodeContainerTurn(
  ws: Workspace,
  prompt: string,
  auth: RuntimeAuth | null,
  resume: string | null,
  container: SessionContainer,
): Promise<string | null> {
  const promptFile = writePromptFile(container, prompt);
  await ingest(ws.id, {
    role: "system",
    kind: "init",
    content: { subtype: "init", runtime: "claude_code", model: auth?.model ?? MODEL ?? null, container: container.name, volume: container.userVolume },
    claude_session_id: resume,
  });
  const out = runInSession(container, ["sh", "-lc", `claude -p "$(cat ${promptFile})" --dangerously-skip-permissions`], {
    ANTHROPIC_API_KEY: auth?.kind === "api_key" ? auth.secret : "",
    CLAUDE_CODE_OAUTH_TOKEN: auth && auth.kind !== "api_key" ? auth.secret : "",
    CLAUDE_CONFIG_DIR: `${container.workspaceDir}/.claude`,
  });
  await ingest(ws.id, { role: "assistant", kind: "text", content: { text: out }, claude_session_id: resume });
  return resume;
}

async function runCodexTurn(
  ws: Workspace,
  prompt: string,
  auth: RuntimeAuth | null,
  resume: string | null,
  container: SessionContainer | null,
): Promise<string | null> {
  if (!auth?.secret) throw new Error("codex runtime requires a saved codex credential");
  if (!container) throw new Error("codex runtime requires a session container");
  const promptFile = writePromptFile(container, prompt);
  await ingest(ws.id, {
    role: "system",
    kind: "init",
    content: { subtype: "init", runtime: "codex", provider: auth.provider, model: auth.model ?? MODEL ?? null, container: container.name, volume: container.userVolume },
    claude_session_id: resume,
  });
  const out = runInSession(container, ["sh", "-lc", `codex exec \"$(cat ${promptFile})\"`], {
    OPENAI_API_KEY: auth.kind === "api_key" ? auth.secret : "",
    CODEX_OAUTH_TOKEN: auth.kind !== "api_key" ? auth.secret : "",
  });
  await ingest(ws.id, { role: "assistant", kind: "text", content: { text: out }, claude_session_id: resume });
  return resume;
}

async function runOpenCodeTurn(
  ws: Workspace,
  prompt: string,
  auth: RuntimeAuth | null,
  resume: string | null,
  container: SessionContainer | null,
): Promise<string | null> {
  if (!auth?.secret) throw new Error("opencode runtime requires a saved opencode provider/model credential");
  if (!container) throw new Error("opencode runtime requires a session container");
  const promptFile = writePromptFile(container, prompt);
  const model = auth.model ?? ws.model ?? MODEL ?? "";
  const modelArg = model ? ` --model "$OPENCODE_MODEL"` : "";
  await ingest(ws.id, {
    role: "system",
    kind: "init",
    content: { subtype: "init", runtime: "opencode", provider: auth.provider, model: model || null, container: container.name, volume: container.userVolume },
    claude_session_id: resume,
  });
  const out = runInSession(container, ["sh", "-lc", `opencode run \"$(cat ${promptFile})\"${modelArg}`], {
    OPENROUTER_API_KEY: auth.provider === "openrouter" ? auth.secret : "",
    ANTHROPIC_API_KEY: auth.provider === "anthropic" ? auth.secret : "",
    OPENAI_API_KEY: auth.provider === "openai" ? auth.secret : "",
    OPENCODE_MODEL: model,
  });
  await ingest(ws.id, { role: "assistant", kind: "text", content: { text: out }, claude_session_id: resume });
  return resume;
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
  let container: SessionContainer | null = null;
  try {
    auth = await getRuntimeAuth(ws);
    container = ensureSessionContainer(ws, auth);
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
          sid = await runTurn(ws, prompt, auth, sid, container);
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
      if (container) removeSessionContainer(ws);
      rmSync(ws.workdir, { recursive: true, force: true });
      log(`done ${ws.id} (archived) · removed sandbox ${ws.workdir}${container ? ` and container ${container.name}` : ""}`);
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
  log(`hive-runner up · api=${BASE} · poll=${POLL_MS}ms · dryRun=${DRY_RUN} · model=${MODEL ?? "default"} · isolation=${SESSION_ISOLATION ? ENGINE ?? "unavailable" : "host"}`);
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
