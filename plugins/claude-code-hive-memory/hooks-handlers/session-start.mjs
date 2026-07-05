#!/usr/bin/env node

import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const here = dirname(fileURLToPath(import.meta.url));
const pluginRoot = process.env.CLAUDE_PLUGIN_ROOT || resolve(here, "..");
const repoCandidates = [
  process.env.HIVE_REPO_PATH,
  resolve(pluginRoot, "..", ".."),
  process.cwd(),
].filter(Boolean);

function emitContext(additionalContext) {
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "SessionStart",
        additionalContext,
      },
    }),
  );
}

function findHiveRepo() {
  for (const candidate of repoCandidates) {
    const root = resolve(candidate);
    if (
      existsSync(resolve(root, "package.json")) &&
      existsSync(resolve(root, "packages", "agent", "package.json"))
    ) {
      return root;
    }
  }
  return null;
}

const env = { ...process.env };
if (!env.HIVE_API_URL && env.HIVE_URL) env.HIVE_API_URL = env.HIVE_URL;
if (!env.HIVE_API_TOKEN && env.HIVE_TOKEN) env.HIVE_API_TOKEN = env.HIVE_TOKEN;

if (!env.HIVE_API_TOKEN) {
  emitContext(
    [
      "# Hive Session Memory",
      "Hive memory is installed but not configured.",
      "Set HIVE_API_URL and HIVE_API_TOKEN before starting Claude Code. HIVE_REPO_PATH is required when this plugin is installed outside the Hive checkout.",
    ].join("\n"),
  );
  process.exit(0);
}

const repo = findHiveRepo();
if (!repo) {
  emitContext(
    [
      "# Hive Session Memory",
      "Hive memory could not locate the Hive checkout.",
      "Set HIVE_REPO_PATH to the repo that contains packages/agent, then restart Claude Code.",
    ].join("\n"),
  );
  process.exit(0);
}

const agentEntry = resolve(repo, "packages", "agent", "src", "index.ts");
const result = spawnSync(
  process.execPath,
  ["--experimental-strip-types", agentEntry, "session-start"],
  {
    env,
    encoding: "utf8",
    timeout: Number(env.HIVE_CLAUDE_HOOK_TIMEOUT_MS || 30000),
    windowsHide: true,
  },
);

if (result.error || result.status !== 0) {
  const detail = result.error?.message || result.stderr || `exit ${result.status}`;
  emitContext(
    [
      "# Hive Session Memory",
      "Hive memory startup failed.",
      detail.trim().slice(0, 1000),
    ].join("\n\n"),
  );
  process.exit(0);
}

emitContext(result.stdout.trim());
