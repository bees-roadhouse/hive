#!/usr/bin/env node
// Hive ⇄ Claude Desktop MCP bridge (the `server` of the hive.mcpb bundle).
//
// Claude Desktop speaks MCP's stdio transport: one JSON-RPC message per line
// on stdin/stdout (newline-delimited JSON — no Content-Length headers, though
// LSP-style framing is tolerated defensively). Hive's MCP server is stateless
// Streamable HTTP: every message becomes `POST ${HIVE_URL}/mcp` with a Bearer
// token. Hive replies with plain application/json (never an SSE stream today;
// `text/event-stream` bodies are parsed anyway in case that changes).
//
// Rules of the road:
//   - stdout carries protocol frames ONLY; every log line goes to stderr.
//   - Requests (string|number id + method) expect exactly one response line.
//   - Notifications and client→server responses are forwarded and produce no
//     output — hive acknowledges them with 202 and an empty body.
//   - Upstream failures become JSON-RPC error responses carrying the upstream
//     detail, so Claude shows a reason instead of hanging until its timeout.
//
// Zero dependencies. Node >= 18 (global fetch); Claude Desktop bundles one.

const log = (...args) => console.error("[hive-mcpb]", ...args);

if (typeof fetch !== "function") {
  log(`Node >= 18 required (global fetch missing); running ${process.version}`);
  process.exit(1);
}

const rawUrl = (process.env.HIVE_URL || process.env.HIVE_API_URL || "").trim();
const token = (process.env.HIVE_TOKEN || process.env.HIVE_API_TOKEN || "").trim();
if (!rawUrl || !token) {
  log("HIVE_URL and HIVE_TOKEN must both be set.");
  log("Open the extension's settings in Claude Desktop and fill in the hive URL and an API token.");
  process.exit(1);
}
// Accept either a base URL or a full …/mcp URL in the setting.
const endpoint = `${rawUrl.replace(/\/+$/, "").replace(/\/mcp$/, "")}/mcp`;
const timeoutMs = Number(process.env.HIVE_TIMEOUT_MS) || 120_000;

let negotiated = null; // protocol version captured from the initialize response
let inFlight = 0;
let stdinClosed = false;

const isRequest = (m) =>
  m !== null &&
  typeof m === "object" &&
  typeof m.method === "string" &&
  (typeof m.id === "string" || typeof m.id === "number");
const requestIds = (msg) => (Array.isArray(msg) ? msg : [msg]).filter(isRequest).map((m) => m.id);

const emit = (msg) => process.stdout.write(`${JSON.stringify(msg)}\n`);
const errorResponse = (id, code, message) => ({ jsonrpc: "2.0", id, error: { code, message } });

/** Minimal SSE parse: each event's `data:` lines join into one JSON-RPC message. */
function sseMessages(text) {
  const out = [];
  for (const event of text.split(/\r?\n\r?\n/)) {
    const data = event
      .split(/\r?\n/)
      .filter((l) => l.startsWith("data:"))
      .map((l) => l.slice(5).replace(/^ /, ""))
      .join("\n");
    if (data) out.push(JSON.parse(data));
  }
  return out;
}

async function forward(msg) {
  const ids = requestIds(msg);
  const fail = (code, message) => {
    if (ids.length === 0) log(`dropping failed notification: ${message}`);
    for (const id of ids) emit(errorResponse(id, code, message));
  };

  const headers = {
    "content-type": "application/json",
    accept: "application/json, text/event-stream", // hive's transport gate requires both
    authorization: `Bearer ${token}`,
  };
  if (negotiated) headers["mcp-protocol-version"] = negotiated;

  let res;
  let text;
  try {
    res = await fetch(endpoint, {
      method: "POST",
      headers,
      body: JSON.stringify(msg),
      signal: AbortSignal.timeout(timeoutMs),
    });
    text = await res.text();
  } catch (e) {
    const detail = e?.cause?.message || e?.message || String(e);
    log(`POST ${endpoint} failed: ${detail}`);
    return fail(-32000, `hive unreachable at ${endpoint}: ${detail}`);
  }

  // Notifications/responses only: hive answers 202 Accepted with an empty body.
  if (res.status === 202 || (res.ok && text.trim() === "")) return;

  let replies;
  try {
    replies = (res.headers.get("content-type") || "").includes("text/event-stream")
      ? sseMessages(text)
      : [JSON.parse(text)];
  } catch {
    log(`hive answered HTTP ${res.status} with a non-JSON body: ${text.slice(0, 200)}`);
    return fail(-32000, `hive answered HTTP ${res.status} with a non-JSON body`);
  }

  if (!res.ok) {
    // Hive's transport-level JSON-RPC errors (401 -32001, 406, 415, …) arrive
    // with id:null — re-address to our request id(s) so the client correlates.
    const err = replies[0]?.error;
    if (err && typeof err.code === "number") {
      log(`hive answered HTTP ${res.status}: ${err.message}`);
      return fail(err.code, err.message ?? `hive answered HTTP ${res.status}`);
    }
    return fail(-32000, `hive answered HTTP ${res.status}`);
  }

  for (const reply of replies) {
    // Remember the negotiated version; later requests echo it in the
    // MCP-Protocol-Version header, as the Streamable HTTP spec asks.
    const init = (Array.isArray(reply) ? reply : [reply]).find((r) => r?.result?.protocolVersion);
    if (init) negotiated = init.result.protocolVersion;
    emit(reply); // a batch response stays one line, exactly as hive framed it
  }
}

// ---- stdin framing ----

let buf = Buffer.alloc(0);

/** Pull one frame off the buffer, or return null until more bytes arrive. */
function nextFrame() {
  let start = 0;
  while (start < buf.length && (buf[start] === 0x0d || buf[start] === 0x0a)) start += 1;
  if (start > 0) buf = buf.subarray(start);
  if (buf.length === 0) return null;

  // Defensive: a client that frames LSP-style sends "Content-Length: N\r\n\r\n<N bytes>".
  if (/^content-length:/i.test(buf.subarray(0, 16).toString("utf8"))) {
    const sep = buf.indexOf("\r\n\r\n");
    if (sep === -1) return null;
    const len = Number(/content-length:\s*(\d+)/i.exec(buf.subarray(0, sep).toString("utf8"))?.[1]);
    if (!Number.isFinite(len)) {
      log("dropping malformed Content-Length header block");
      buf = buf.subarray(sep + 4);
      return nextFrame();
    }
    if (buf.length < sep + 4 + len) return null;
    const frame = buf.subarray(sep + 4, sep + 4 + len).toString("utf8");
    buf = buf.subarray(sep + 4 + len);
    return frame;
  }

  const nl = buf.indexOf(0x0a);
  if (nl === -1) return null;
  const frame = buf.subarray(0, nl).toString("utf8");
  buf = buf.subarray(nl + 1);
  return frame;
}

process.stdin.on("data", (chunk) => {
  buf = Buffer.concat([buf, chunk]);
  let frame;
  while ((frame = nextFrame()) !== null) {
    const line = frame.trim();
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      // Mirrors the SDK's stdio transport: unparseable lines are dropped, not answered.
      log(`skipping unparseable stdin line: ${line.slice(0, 200)}`);
      continue;
    }
    inFlight += 1;
    forward(msg)
      .catch((e) => log(`bridge error: ${e?.stack || e}`))
      .finally(() => {
        inFlight -= 1;
        maybeExit();
      });
  }
});

process.stdin.on("end", () => {
  stdinClosed = true;
  maybeExit();
});

function maybeExit() {
  if (stdinClosed && inFlight === 0) process.exit(0);
}

log(`bridging stdio ⇄ ${endpoint}`);
