# Hive Memory for Claude Code

Claude Code plugin for Hive-backed AI memory. Zero dependencies — the hooks
talk to the Hive API directly with Node's built-in `fetch`, so the plugin
works from the marketplace cache with no Hive repo checkout.

It provides:

- a `SessionStart` hook that injects the Hive recall brief (identity profile,
  open tasks, unread inbox, relevant journal, recent events, projects) and
  syncs the identity's enabled skills/agents/commands into the project's
  `.claude/` directory;
- a `SessionEnd` hook that captures the session transcript into Hive
  Conversations for later reflection;
- an HTTP MCP config for Hive's `/mcp` endpoint (journal, tasks, search,
  recall, mail, conversations tools);
- a skill and a `/save-hive-memory` command teaching Claude to save durable
  memory as Hive journal prose.

## Install

Add the marketplace and install:

```bash
claude plugin marketplace add bees-roadhouse/hive
claude plugin install hive-memory@bees-roadhouse
```

Development (from a checkout):

```bash
claude --plugin-dir /path/to/hive/plugins/claude-code-hive-memory
```

## Environment

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `HIVE_API_URL` | yes | — | Hive base URL, e.g. `https://hive.example.com` (no trailing `/`) |
| `HIVE_API_TOKEN` | yes | — | Identity bearer token (`hive_pat_...` or an OAuth access token) |
| `HIVE_SESSION_CAPTURE` | no | on | Set `0` to disable SessionEnd transcript capture |
| `HIVE_PEER` | no | — | Optional focus actor for the recall brief (e.g. the human in the session) |
| `HIVE_HOOK_TIMEOUT_MS` | no | `10000` | Per-request timeout for hook API calls |

`HIVE_URL` and `HIVE_TOKEN` are accepted as legacy aliases by the hooks; the
MCP config uses `HIVE_API_URL` and `HIVE_API_TOKEN`.

The server derives your identity and memory namespace from the token — the
client never asserts who it is.

## Hook behavior

**SessionStart** posts `/api/recall` and injects the server-composed brief as
session context, then pulls `/api/identity/artifacts` and writes the enabled
artifacts into the session cwd: `.claude/skills/<name>/SKILL.md`,
`.claude/agents/<name>.md`, `.claude/commands/<name>.md`. The sync records
what it wrote in `.claude/.hive-synced.json` and prunes only files listed
there when they leave the enabled set — it never touches files you authored.

**SessionEnd** parses the session transcript (thinking blocks are dropped —
chain-of-thought is never persisted), upserts a conversation keyed on the
Claude Code session id, and replaces its stored transcript with the full one
(resumed sessions re-fire SessionEnd with the whole transcript, so replace
keeps the capture idempotent).

Both hooks soft-fail: unconfigured means a silent no-op, and an unreachable
Hive costs one stderr line — a broken Hive never blocks a session.

## Capture → reflection

Captured conversations land in your namespace with `origin='captured'` and
queue for reflection (`GET /api/conversations/pending`). The Hive reflector
drains that queue, distills each transcript into owner-scoped journal prose,
and stamps the reflection cursor — so what you did in Claude Code today
becomes memory the next session recalls. Nothing is journal-mirrored at
capture time; reflection is the only path from transcript to journal.

## Uninstall

Removing the plugin stops the hooks; it does not remove artifacts already
synced into projects. To clean a project, delete the files listed in
`.claude/.hive-synced.json` (and the index itself) — everything else under
`.claude/` is yours, not the plugin's.
