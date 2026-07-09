#!/usr/bin/env node
// SessionEnd hook: capture the session transcript into Hive Conversations.
//
//   1. Parse the JSONL transcript at input.transcript_path (thinking blocks
//      are dropped — chain-of-thought is never persisted).
//   2. POST /api/conversations -> idempotent upsert keyed on
//      (runtime='claude_code', external_id=<claude session id>).
//   3. POST /api/conversations/{id}/messages {replace: true} -> the FULL
//      transcript replaces the stored one (a resumed session re-fires
//      SessionEnd with the whole transcript; appending would duplicate turns).
//
// The conversation lands in the token's namespace as origin='captured' and
// queues for the reflection loop (reflected_at IS NULL). Opt out with
// HIVE_SESSION_CAPTURE=0. Soft-fails everywhere — never blocks shutdown.

import { existsSync, readFileSync } from "node:fs";
import { basename } from "node:path";
import { hive, hiveConfig, readHookInput, softFail } from "./lib.mjs";

function captureDisabled() {
  const v = (process.env.HIVE_SESSION_CAPTURE || "").trim().toLowerCase();
  return ["0", "false", "no", "off"].includes(v);
}

// Render a transcript content field (string OR array of content blocks) to text.
function contentToText(content) {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((b) => {
        if (typeof b === "string") return b;
        if (b?.type === "text") return b.text || "";
        if (b?.type === "tool_use") return `[tool_use ${b.name || ""}]`;
        if (b?.type === "tool_result") {
          const c = b.content;
          return `[tool_result] ${typeof c === "string" ? c : contentToText(c)}`;
        }
        if (b?.type === "thinking") return ""; // don't persist chain-of-thought
        return "";
      })
      .filter(Boolean)
      .join("\n");
  }
  return "";
}

// Claude Code transcript lines wrap the turn in `.message`; older/simple shapes
// put role/content at top level. Normalize both into {role, content}.
function lineToMessage(obj) {
  const msg = obj.message || obj;
  const role = msg.role || obj.type; // 'user' | 'assistant' | 'system' | 'tool'
  if (!role || !["user", "assistant", "system", "tool"].includes(role)) return null;
  const content = contentToText(msg.content);
  if (!content || !content.trim()) return null;
  return { role, content };
}

function parseTranscript(path) {
  if (!path || !existsSync(path)) return [];
  const out = [];
  for (const line of readFileSync(path, "utf8").split("\n")) {
    const t = line.trim();
    if (!t) continue;
    let obj;
    try {
      obj = JSON.parse(t);
    } catch {
      continue;
    }
    const m = lineToMessage(obj);
    if (m) out.push(m);
  }
  return out;
}

// "<cwd basename>: <first user line>" with fallbacks — the Conversations title.
function sessionTitle(input, messages) {
  const dir = input.cwd ? basename(input.cwd) : "";
  const firstUser = messages.find((m) => m.role === "user");
  const line = firstUser
    ? firstUser.content.split("\n").find((l) => l.trim()) || ""
    : "";
  const head = line.trim().slice(0, 80);
  if (dir && head) return `${dir}: ${head}`;
  if (head) return head;
  const sid = input.session_id || "";
  return `claude-code ${sid ? sid.slice(0, 8) : "session"}`;
}

async function main() {
  if (captureDisabled()) process.exit(0);
  const cfg = hiveConfig();
  if (!cfg) process.exit(0); // not configured — silent no-op

  const input = readHookInput();
  const sessionId = (input.session_id || "").trim();
  if (!sessionId) process.exit(0); // no capture key
  const messages = parseTranscript(input.transcript_path);
  if (!messages.length) process.exit(0);

  const upserted = await hive(cfg, "POST", "/api/conversations", {
    runtime: "claude_code",
    external_id: sessionId,
    title: sessionTitle(input, messages),
  });
  const id = upserted?.id;
  if (!id) process.exit(0);

  await hive(cfg, "POST", `/api/conversations/${id}/messages`, {
    replace: true,
    messages,
  });
  process.stderr.write(
    `[hive-memory] captured ${messages.length} turns -> conversation ${id}\n`,
  );
  process.exit(0);
}

main().catch((err) => softFail("session-end", err));
