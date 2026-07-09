#!/usr/bin/env node
// SessionStart hook: boot the session Hive-aware, zero dependencies.
//
//   1. POST /api/recall -> the server-composed memory brief (profile cards,
//      open tasks, unread inbox, relevant journal, recent events, touched
//      projects) injected into the session as additionalContext.
//   2. GET /api/identity/artifacts -> the identity's ENABLED skills/agents/
//      commands, synced into <cwd>/.claude so Claude Code discovers them.
//      .claude/.hive-synced.json records what WE wrote; pruning only ever
//      touches paths listed there — user-authored files are never deleted.
//
// The server derives identity + namespace from the bearer token; the client
// never asserts who it is. Unconfigured -> silent no-op. Any failure ->
// stderr + exit 0 (a broken Hive must never block a session).

import {
  existsSync,
  mkdirSync,
  readFileSync,
  rmdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve, sep } from "node:path";
import { hive, hiveConfig, readHookInput, softFail } from "./lib.mjs";

const MANAGED_INDEX = ".hive-synced.json"; // under .claude/, tracks what we wrote
const SAFE_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]*$/; // server data, but never a path escape

function projectDir(input) {
  return input.cwd || process.env.CLAUDE_PROJECT_DIR || process.cwd();
}

// kind -> relative path under .claude for an artifact `name`.
function artifactRelPath(kind, name) {
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

/** Prior managed file list, or null when no index exists yet. */
function readIndex(indexPath) {
  if (!existsSync(indexPath)) return null;
  try {
    const files = JSON.parse(readFileSync(indexPath, "utf8")).files;
    return Array.isArray(files) ? files.filter((f) => typeof f === "string") : [];
  } catch {
    return [];
  }
}

function syncArtifacts(claudeDir, artifacts) {
  const indexPath = join(claudeDir, MANAGED_INDEX);
  const prior = readIndex(indexPath);

  const written = [];
  for (const a of artifacts) {
    if (!SAFE_NAME.test(a?.name || "")) {
      process.stderr.write(
        `[hive-memory] skipping artifact with unsafe name: ${JSON.stringify(a?.name ?? null)}\n`,
      );
      continue;
    }
    const rel = artifactRelPath(a.kind, a.name);
    if (!rel || typeof a.content !== "string") continue;
    const abs = join(claudeDir, rel);
    mkdirSync(dirname(abs), { recursive: true });
    writeFileSync(abs, a.content, "utf8");
    written.push(rel);
  }

  // Prune files we previously wrote that are no longer in the enabled set —
  // only ever paths recorded in MANAGED_INDEX, and only inside .claude/.
  const root = resolve(claudeDir) + sep;
  for (const rel of prior || []) {
    if (written.includes(rel)) continue;
    const abs = resolve(claudeDir, rel);
    if (!abs.startsWith(root)) continue; // never step outside .claude
    try {
      rmSync(abs, { force: true });
      rmdirSync(dirname(abs)); // clears an emptied skills/<name>/ dir
    } catch {
      /* parent non-empty or already gone */
    }
  }

  // Nothing tracked before or now -> leave the project untouched.
  if (prior === null && written.length === 0) return 0;
  mkdirSync(claudeDir, { recursive: true });
  writeFileSync(indexPath, `${JSON.stringify({ files: written }, null, 2)}\n`, "utf8");
  return written.length;
}

async function main() {
  const cfg = hiveConfig();
  if (!cfg) process.exit(0); // not configured — stay silent, don't nag every session
  const input = readHookInput();

  // Recall: the server composes the ready-to-inject markdown brief.
  let brief = "";
  let offline = false;
  try {
    const peer = (process.env.HIVE_PEER || "").trim();
    const recall = await hive(cfg, "POST", "/api/recall", peer ? { peer } : {});
    brief = (recall?.brief || "").trim();
  } catch (err) {
    process.stderr.write(`[hive-memory] recall failed: ${err.message}\n`);
    offline = Boolean(err.network);
  }

  // Artifact sync — skipped when the server is unreachable (one line, not two).
  let synced = 0;
  if (!offline) {
    try {
      const artifacts = await hive(cfg, "GET", "/api/identity/artifacts");
      if (Array.isArray(artifacts)) {
        synced = syncArtifacts(join(projectDir(input), ".claude"), artifacts);
      }
    } catch (err) {
      process.stderr.write(`[hive-memory] artifact sync failed: ${err.message}\n`);
    }
  }

  if (!brief && !synced) process.exit(0);
  const parts = [];
  if (brief) parts.push(brief);
  if (synced) {
    parts.push(`_Hive synced ${synced} identity artifact(s) into .claude/._`);
  }
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "SessionStart",
        additionalContext: parts.join("\n\n"),
      },
    }),
  );
  process.exit(0);
}

main().catch((err) => softFail("session-start", err));
