// Shared helpers for the hive Claude Code plugin hooks.
//
// Auth model: hooks talk to hive with the AI-identity OAuth token the user
// minted when they approved this app. Two env vars carry it:
//   HIVE_URL    base url, e.g. https://hive.home.beesroadhouse.com  (no trailing /)
//   HIVE_TOKEN  the identity bearer token (hive_pat_… or an OAuth access token)
// HIVE_ACTOR is optional and only used as a display fallback; the server always
// derives the real identity + namespace from the token, never from the client.

import { readFileSync } from "node:fs";

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

/** Resolved hive config, or null when the plugin isn't configured. */
export function hiveConfig() {
  const base = (process.env.HIVE_URL || "").trim().replace(/\/+$/, "");
  const token = (process.env.HIVE_TOKEN || "").trim();
  if (!base || !token) return null;
  return { base, token, actor: (process.env.HIVE_ACTOR || "").trim() || null };
}

/** Authenticated JSON request to hive. Returns parsed body or throws. */
export async function hive(cfg, method, path, body) {
  const res = await fetch(cfg.base + path, {
    method,
    headers: {
      authorization: `Bearer ${cfg.token}`,
      "content-type": "application/json",
      accept: "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`hive ${method} ${path} -> ${res.status} ${text.slice(0, 300)}`);
  }
  return text ? JSON.parse(text) : null;
}

/** Hooks must never break a session: log to stderr, exit 0. */
export function softFail(prefix, err) {
  process.stderr.write(`[hive-plugin] ${prefix}: ${err?.message || err}\n`);
  process.exit(0);
}
