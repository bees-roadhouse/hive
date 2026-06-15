// SessionStart hook: boot the session hive-aware.
//
//   1. POST /api/recall  -> identity profile/bio + semantic journal brief,
//      open tasks, unread inbox. Injected into the session as context.
//   2. GET  /api/identity/artifacts -> the identity's ENABLED skills/agents/
//      commands, synced into <project>/.claude so Claude Code discovers them.
//
// The server derives identity + memory namespace from HIVE_TOKEN; the client
// never asserts who it is. Any failure soft-fails (stderr + exit 0) so a hive
// outage never blocks a session.

import { mkdirSync, writeFileSync, rmSync, existsSync, readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { readHookInput, hiveConfig, hive, softFail } from "./lib.mjs";

const MANAGED_INDEX = ".hive-synced.json"; // under .claude/, tracks what we wrote

function projectDir(input) {
  return input.cwd || process.env.CLAUDE_PROJECT_DIR || process.cwd();
}

// kind -> relative path under .claude for an artifact `name`.
function artifactPath(kind, name) {
  switch (kind) {
    case "skill":
      return join("skills", name, "SKILL.md");
    case "agent":
      return join("agents", `${name}.md`);
    case "command":
      return join("commands", `${name}.md`);
    default:
      return null;
  }
}

function syncArtifacts(claudeDir, artifacts) {
  const indexPath = join(claudeDir, MANAGED_INDEX);
  let prior = [];
  if (existsSync(indexPath)) {
    try {
      prior = JSON.parse(readFileSync(indexPath, "utf8")).files || [];
    } catch {
      prior = [];
    }
  }

  const written = [];
  for (const a of artifacts) {
    const rel = artifactPath(a.kind, a.name);
    if (!rel) continue;
    const abs = join(claudeDir, rel);
    mkdirSync(dirname(abs), { recursive: true });
    writeFileSync(abs, a.content, "utf8");
    written.push(rel);
  }

  // Prune files we previously wrote that are no longer in the enabled set —
  // only ever touches paths WE created (tracked in MANAGED_INDEX).
  for (const rel of prior) {
    if (!written.includes(rel)) {
      try {
        rmSync(join(claudeDir, rel), { force: true });
      } catch {
        /* ignore */
      }
    }
  }

  mkdirSync(claudeDir, { recursive: true });
  writeFileSync(indexPath, JSON.stringify({ files: written }, null, 2), "utf8");
  return written.length;
}

function renderBrief(recall, syncedCount) {
  const lines = [];
  const profiles = recall.profiles || recall.profile || [];
  const list = Array.isArray(profiles) ? profiles : [profiles].filter(Boolean);
  const me = list[0];
  if (me) {
    lines.push(`# hive recall — you are ${me.name || me.id}`);
    if (me.bio || me.summary) lines.push("", me.bio || me.summary);
  } else {
    lines.push("# hive recall");
  }

  const tasks = recall.openTasks || recall.open_tasks || [];
  if (tasks.length) {
    lines.push("", "## Open tasks");
    for (const t of tasks.slice(0, 10)) lines.push(`- [${t.status}] ${t.title}`);
  }

  const unread = recall.unread || recall.inbox || [];
  if (unread.length) {
    lines.push("", `## Unread inbox (${unread.length})`);
    for (const m of unread.slice(0, 5)) {
      lines.push(`- ${m.from || m.sender || "?"}: ${m.subject || m.body || ""}`.slice(0, 200));
    }
  }

  const journal = recall.journal || recall.entries || [];
  if (journal.length) {
    lines.push("", "## Relevant memory");
    for (const e of journal.slice(0, 8)) {
      lines.push(`- ${e.title}${e.id ? ` (${e.id})` : ""}`);
    }
  }

  if (syncedCount) lines.push("", `_Synced ${syncedCount} identity artifact(s) into .claude._`);
  return lines.join("\n");
}

async function main() {
  const cfg = hiveConfig();
  if (!cfg) {
    // Not configured — stay silent, don't nag every session.
    process.exit(0);
  }
  const input = readHookInput();

  let recall = {};
  try {
    recall = (await hive(cfg, "POST", "/api/recall", { query: null })) || {};
  } catch (err) {
    process.stderr.write(`[hive-plugin] recall failed: ${err.message}\n`);
  }

  let syncedCount = 0;
  try {
    const artifacts = (await hive(cfg, "GET", "/api/identity/artifacts")) || [];
    if (Array.isArray(artifacts) && artifacts.length) {
      const claudeDir = join(projectDir(input), ".claude");
      syncedCount = syncArtifacts(claudeDir, artifacts);
    }
  } catch (err) {
    process.stderr.write(`[hive-plugin] artifact sync failed: ${err.message}\n`);
  }

  const context = renderBrief(recall, syncedCount);
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "SessionStart",
        additionalContext: context,
      },
    })
  );
  process.exit(0);
}

main().catch((err) => softFail("session-start", err));
