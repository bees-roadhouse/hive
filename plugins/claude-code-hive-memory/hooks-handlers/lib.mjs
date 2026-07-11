// Shared helpers for the hive-memory Claude Code plugin hooks. Zero deps —
// hooks spawn the `hive-bridge` binary in one-shot mode (`hive-bridge call
// <tool> --json '<args>'`) against the hive store on THIS machine. There is
// no server and no token anymore (DIRECTION.md D25): hive is personal and
// local-first, and the OS user boundary is the auth.
//
// Requirements: `hive-bridge` on PATH (`cargo install --path bridge` from
// the hive repo, or the release tarball). Interim-mode caveat: the bridge
// opens the same data dir as the hive app under a single-writer lock, so a
// hook that fires while the app is open soft-fails with one stderr line.
//
// Env:
//   HIVE_ACTOR            acting identity for hook calls (default: $USER —
//                         the same default the app and bridge use)
//   HIVE_BRIDGE_BIN       bridge binary override (default: hive-bridge)
//   HIVE_HOOK_TIMEOUT_MS  per-call timeout (default 10000)
//
// Failure discipline: a missing or failing hive must NEVER block a session.
// Bridge not installed -> exit 0 silently. Failures -> one stderr line, exit 0.

import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";

export const DEFAULT_TIMEOUT_MS = 10_000;

/** Read the JSON object Claude Code delivers on stdin to every hook. */
export function readHookInput() {
  let raw = "";
  try {
    raw = readFileSync(0, "utf8"); // fd 0 = stdin
  } catch {
    return {};
  }
  raw = raw.trim();
  if (!raw) return {};
  try {
    return JSON.parse(raw);
  } catch {
    return {};
  }
}

/** Resolved bridge config. Always defined — "not configured" is now just
 *  "hive-bridge not on PATH", detected at call time. */
export function hiveConfig() {
  const bin = (process.env.HIVE_BRIDGE_BIN || "hive-bridge").trim();
  const actor =
    (process.env.HIVE_ACTOR || process.env.USER || "").trim() || "owner";
  const timeoutMs = Number(process.env.HIVE_HOOK_TIMEOUT_MS) || DEFAULT_TIMEOUT_MS;
  return { bin, actor, timeoutMs };
}

/** The text of a CallToolResult's first content block, or "". */
function toolText(result) {
  const block = result?.content?.find?.((b) => b?.type === "text");
  return typeof block?.text === "string" ? block.text : "";
}

/**
 * One MCP tool call through `hive-bridge call`. Returns the tool's payload
 * (the text content block parsed as JSON; raw text when it isn't JSON) or
 * throws. Failures to run at all — binary missing, store locked by the app,
 * keychain unavailable — carry `err.unavailable = true` so callers can skip
 * follow-up calls; a plain missing binary also sets `err.notInstalled`.
 */
export function hiveCall(cfg, tool, args) {
  const res = spawnSync(
    cfg.bin,
    ["call", tool, "--json", JSON.stringify(args ?? {}), "--actor", cfg.actor],
    { encoding: "utf8", timeout: cfg.timeoutMs, windowsHide: true },
  );
  if (res.error) {
    const missing = res.error.code === "ENOENT";
    const err = new Error(
      `hive-bridge ${tool} -> ${missing ? "hive-bridge not found on PATH" : res.error.message}`,
    );
    err.unavailable = true;
    err.notInstalled = missing;
    throw err;
  }
  let result = null;
  try {
    result = JSON.parse((res.stdout || "").trim());
  } catch {
    /* no result JSON — fall through to the status check */
  }
  if (res.status !== 0) {
    // Exit 1 = tool-level isError (result text carries the reason); other
    // failures (lock, keychain) report on stderr before any result exists.
    const detail =
      toolText(result) ||
      (res.stderr || "").trim().split("\n").pop() ||
      `exit ${res.status}`;
    const err = new Error(`hive-bridge ${tool} -> ${detail}`);
    err.unavailable = result === null;
    throw err;
  }
  const text = toolText(result);
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/** Hooks must never break a session: one stderr line, exit 0. */
export function softFail(prefix, err) {
  process.stderr.write(`[hive-memory] ${prefix}: ${err?.message || err}\n`);
  process.exit(0);
}
