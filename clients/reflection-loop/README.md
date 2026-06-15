# hive — reflection loop

Drains the conversation **reflection queue** for one AI identity: reads the
sessions captured by the Claude Code plugin (or any `/api/conversations`
producer), reflects on each with an LLM, and writes the result back to hive as
durable memory — a rolling summary plus a journal narrative and proposed
tasks/decisions.

```
GET  /api/conversations/pending      → unreflected sessions (namespace-scoped)
GET  /api/conversations/{id}         → full transcript
(LLM reflect)                        → summary + narrative + tasks/decisions
POST /api/journal                    → durable memory (anchors create tasks in auto mode)
POST /api/conversations/{id}/reflected → store summary, drain the queue
```

The token decides the identity and memory namespace; the loop reflects exactly
the conversations that token can see.

## Modes — `REFLECTION_MODE` (default `suggest`)

| Mode | Behaviour |
|---|---|
| `off` | Do nothing. The identity opted out. |
| `suggest` | Write the journal narrative + a plain **Proposed follow-ups** section (tagged `suggestion`). No anchors, so nothing is auto-created — a human reviews. Rolling summary still stored. |
| `auto` | Additionally **anchor** the tasks/decisions so hive materializes them immediately. |

## Setup

hive auth (the AI-identity token, same one the plugin uses):

```bash
export HIVE_URL="https://hive.home.beesroadhouse.com"
export HIVE_TOKEN="hive_pat_…"
```

LLM auth — **default is the Anthropic API key**:

```bash
export ANTHROPIC_API_KEY="sk-ant-…"
export REFLECTION_MODE="suggest"     # off | suggest | auto
export REFLECTION_MODEL="claude-fable-5"
```

### ⚠️ Subscription opt-in (policy + billing)

By default the loop uses the **Anthropic API** (`ANTHROPIC_API_KEY`) — clear
per-token billing and a policy surface intended for automation. You *can* point
it at a Claude **subscription** token instead, but this is opt-in and carries
real caveats:

```bash
export REFLECTION_USE_SUBSCRIPTION=1
export ANTHROPIC_AUTH_TOKEN="<subscription oauth token>"
```

When enabled, the loop prints a warning every run:

- Driving a **headless/automated** loop with a human Claude subscription may
  violate the Anthropic **Consumer Terms**. Use at your own risk.
- Billing and rate limits differ from the API (subscription limits, no
  per-token billing). For unattended automation, prefer an API key.

## Run

One pass (drain what's pending, then exit — good for cron):

```bash
node scripts/reflect.mjs
```

Continuous (`REFLECTION_INTERVAL_SECS`, default 300):

```bash
node scripts/reflect.mjs --watch
```

## Requirements

- Node 18+ (zero dependencies — built-in `fetch`).
- hive >= 0.6.0 (conversations API).

## Notes

- A failed reflection leaves that conversation **pending**; the next pass
  retries it. An empty transcript is drained with an empty summary so it doesn't
  loop forever.
- `thinking` content is never sent (the plugin doesn't persist it).
- Anchor offsets are UTF-16 code units, matching hive's `js_slice_utf16`; the
  runner assembles the body so the offsets are exact.
