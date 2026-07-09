// Shared helpers for the hive-memory Claude Code plugin hooks. Zero deps —
// Node's built-in fetch only, so the plugin works from the marketplace cache
// without a Hive repo checkout.
//
// Auth model: hooks talk to Hive with the identity bearer token the user
// minted for this machine. Env:
//   HIVE_API_URL    base url, e.g. https://hive.example.com (no trailing /)
//   HIVE_API_TOKEN  identity bearer token (hive_pat_... or an OAuth access token)
// HIVE_URL / HIVE_TOKEN are accepted as legacy aliases. The server derives
// identity + namespace from the token; the client never asserts who it is.
//
// Failure discipline: a broken or unreachable Hive must NEVER block a session.
// Unconfigured -> exit 0 silently. Failures -> one stderr line, exit 0.

import { readFileSync } from "node:fs";

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

/** Resolved Hive config, or null when the plugin isn't configured. */
export function hiveConfig() {
  const base = (process.env.HIVE_API_URL || process.env.HIVE_URL || "")
    .trim()
    .replace(/\/+$/, "");
  const token = (process.env.HIVE_API_TOKEN || process.env.HIVE_TOKEN || "").trim();
  if (!base || !token) return null;
  const timeoutMs = Number(process.env.HIVE_HOOK_TIMEOUT_MS) || DEFAULT_TIMEOUT_MS;
  return { base, token, timeoutMs };
}

/**
 * Authenticated JSON request to Hive with a hard timeout. Returns the parsed
 * body or throws; transport-level failures (refused, DNS, timeout) carry
 * `err.network = true` so callers can skip follow-up calls to a dead server.
 */
export async function hive(cfg, method, path, body) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), cfg.timeoutMs);
  let res;
  try {
    res = await fetch(cfg.base + path, {
      method,
      headers: {
        authorization: `Bearer ${cfg.token}`,
        "content-type": "application/json",
        accept: "application/json",
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: controller.signal,
    });
  } catch (cause) {
    const detail =
      cause?.name === "AbortError"
        ? `timeout after ${cfg.timeoutMs}ms`
        : cause?.cause?.message || cause?.message || String(cause);
    const err = new Error(`hive ${method} ${path} -> ${detail}`);
    err.network = true;
    throw err;
  } finally {
    clearTimeout(timer);
  }
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`hive ${method} ${path} -> ${res.status} ${text.slice(0, 300)}`);
  }
  return text ? JSON.parse(text) : null;
}

/** Hooks must never break a session: one stderr line, exit 0. */
export function softFail(prefix, err) {
  process.stderr.write(`[hive-memory] ${prefix}: ${err?.message || err}\n`);
  process.exit(0);
}
