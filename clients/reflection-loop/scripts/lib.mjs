// Shared helpers for the hive reflection loop.
//
// hive auth: HIVE_URL + HIVE_TOKEN (the AI-identity token). The server derives
// identity + memory namespace from the token; the loop reflects exactly the
// conversations that token can see.
//
// LLM auth (reflection model): default is ANTHROPIC_API_KEY — clear per-token
// billing and a policy surface meant for automation. A subscription/OAuth token
// (ANTHROPIC_AUTH_TOKEN) is opt-in ONLY and triggers a loud warning: using a
// human Claude subscription to drive a headless automated loop may violate the
// Anthropic Consumer Terms and bills differently than the API.

export function hiveConfig() {
  const base = (process.env.HIVE_URL || "").trim().replace(/\/+$/, "");
  const token = (process.env.HIVE_TOKEN || "").trim();
  if (!base || !token) {
    throw new Error("HIVE_URL and HIVE_TOKEN are required");
  }
  return { base, token };
}

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
    throw new Error(`hive ${method} ${path} -> ${res.status} ${text.slice(0, 400)}`);
  }
  return text ? JSON.parse(text) : null;
}

/**
 * Resolve LLM auth. Returns the header set + a `subscription` flag. Default is
 * the API key; a subscription token is opt-in and the caller MUST warn.
 */
export function llmAuth() {
  const apiKey = (process.env.ANTHROPIC_API_KEY || "").trim();
  const subToken = (process.env.ANTHROPIC_AUTH_TOKEN || "").trim();
  const optedIntoSub = process.env.REFLECTION_USE_SUBSCRIPTION === "1" && subToken;

  if (optedIntoSub) {
    return {
      subscription: true,
      headers: { authorization: `Bearer ${subToken}`, "anthropic-version": "2023-06-01" },
    };
  }
  if (!apiKey) {
    throw new Error(
      "ANTHROPIC_API_KEY is required (or set REFLECTION_USE_SUBSCRIPTION=1 + ANTHROPIC_AUTH_TOKEN to opt into a subscription — see README)"
    );
  }
  return {
    subscription: false,
    headers: { "x-api-key": apiKey, "anthropic-version": "2023-06-01" },
  };
}

/** Print the subscription policy + billing warning. Call once at startup. */
export function warnIfSubscription(auth) {
  if (!auth.subscription) return;
  process.stderr.write(
    [
      "",
      "  ╭─────────────────────────────────────────────────────────────────────╮",
      "  │  WARNING: reflection is using a Claude SUBSCRIPTION token, not the   │",
      "  │  Anthropic API.                                                      │",
      "  │  • Driving a headless/automated loop with a human subscription may   │",
      "  │    violate the Anthropic Consumer Terms. Use at your own risk.       │",
      "  │  • Billing/limits differ from the API (subscription rate limits, no  │",
      "  │    per-token billing). For unattended automation, prefer an API key. │",
      "  ╰─────────────────────────────────────────────────────────────────────╯",
      "",
    ].join("\n") + "\n"
  );
}

/** Messages endpoint; ANTHROPIC_BASE_URL overrides the host (gateway/proxy). */
function anthropicUrl() {
  const base = (process.env.ANTHROPIC_BASE_URL || "https://api.anthropic.com").replace(/\/+$/, "");
  return `${base}/v1/messages`;
}

/** Single-shot Anthropic Messages call; returns the concatenated text. */
export async function llm(auth, { model, system, user, maxTokens = 2048 }) {
  const res = await fetch(anthropicUrl(), {
    method: "POST",
    headers: { ...auth.headers, "content-type": "application/json" },
    body: JSON.stringify({
      model,
      max_tokens: maxTokens,
      system,
      messages: [{ role: "user", content: user }],
    }),
  });
  const text = await res.text();
  if (!res.ok) throw new Error(`anthropic -> ${res.status} ${text.slice(0, 400)}`);
  const body = JSON.parse(text);
  return (body.content || [])
    .filter((b) => b.type === "text")
    .map((b) => b.text)
    .join("");
}

/** Extract the first JSON object from a model response (handles ```json fences). */
export function parseJsonObject(s) {
  const fenced = s.match(/```(?:json)?\s*([\s\S]*?)```/);
  const raw = fenced ? fenced[1] : s;
  const start = raw.indexOf("{");
  const end = raw.lastIndexOf("}");
  if (start === -1 || end === -1) throw new Error("no JSON object in model output");
  return JSON.parse(raw.slice(start, end + 1));
}
