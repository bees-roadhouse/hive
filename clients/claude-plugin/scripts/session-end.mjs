// SessionEnd hook: capture the session transcript into hive's conversations.
//
//   1. Parse the JSONL transcript at input.transcript_path.
//   2. POST /api/conversations          -> idempotent upsert on (app, external_id);
//      external_id = session_id, so re-runs reuse the same conversation row.
//   3. POST /api/conversations/{id}/messages -> append the turns.
//
// The conversation lands in the identity's memory namespace (server-derived
// from HIVE_TOKEN) and sits pending until the reflection loop processes it.

import { existsSync, readFileSync } from "node:fs";
import { readHookInput, hiveConfig, hive, softFail } from "./lib.mjs";

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

async function main() {
  const cfg = hiveConfig();
  if (!cfg) process.exit(0);
  const input = readHookInput();

  const messages = parseTranscript(input.transcript_path);
  if (!messages.length) process.exit(0);

  const sessionId = input.session_id || null;
  const upserted = await hive(cfg, "POST", "/api/conversations", {
    app: "claude-code",
    instance: process.env.HOSTNAME || process.env.COMPUTERNAME || null,
    external_id: sessionId,
    name: `claude-code ${sessionId ? sessionId.slice(0, 8) : "session"}`,
  });
  const id = upserted?.id;
  if (!id) process.exit(0);

  await hive(cfg, "POST", `/api/conversations/${id}/messages`, { messages });
  process.stderr.write(`[hive-plugin] captured ${messages.length} turns -> conversation ${id}\n`);
  process.exit(0);
}

main().catch((err) => softFail("session-end", err));
