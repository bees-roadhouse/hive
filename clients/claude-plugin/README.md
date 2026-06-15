# hive — Claude Code plugin

Boot a Claude Code session as a known **hive AI identity**. No manual tool calls.

On session start the plugin:

1. **Recall brief** — `POST /api/recall` pulls your identity profile/bio, a
   semantic journal brief, open tasks, and unread inbox, and injects it as
   session context.
2. **Identity sync** — `GET /api/identity/artifacts` pulls *your* enabled
   skills/agents/commands and writes them into `<project>/.claude/` so Claude
   Code discovers them. Only files this plugin wrote are managed (tracked in
   `.claude/.hive-synced.json`); your own files are never touched.
3. **Hive MCP tools** — the bundled MCP server exposes hive's journal, tasks,
   recall, conversations, and artifact tools in-session.

On session end:

4. **Transcript capture** — the conversation is upserted to
   `POST /api/conversations` (idempotent on the Claude Code session id) and its
   turns appended, landing in your memory namespace pending reflection.

The server derives your identity and memory namespace from the token — the
client never asserts who it is.

## Setup

The plugin authenticates as the AI identity the user approved. Mint that token
in hive (Account → Connected apps / OAuth consent) and set two env vars before
launching Claude Code:

```bash
export HIVE_URL="https://hive.home.beesroadhouse.com"   # no trailing slash
export HIVE_TOKEN="hive_pat_…"                            # the identity token
```

PowerShell:

```powershell
$env:HIVE_URL  = "https://hive.home.beesroadhouse.com"
$env:HIVE_TOKEN = "hive_pat_…"
```

Both the hooks and the bundled MCP server read these. If `HIVE_URL`/`HIVE_TOKEN`
are unset the plugin stays silent (no recall, no capture) so it never blocks a
session.

## Install

Local directory (dev):

```bash
claude --plugin-dir /path/to/hive-claude-plugin
```

Via the bundled marketplace:

```bash
claude plugin marketplace add /path/to/hive-claude-plugin/.claude-plugin/marketplace.json
claude plugin install hive@bees-roadhouse
```

## Requirements

- Node 18+ (bundled with Claude Code) — hooks are zero-dependency `.mjs`.
- A reachable hive (>= 0.6.0, with the conversations + identity-artifacts API).

## Notes

- Transcript capture runs once at `SessionEnd`; message-level dedupe on
  re-ingestion is not yet implemented (the conversation row is idempotent on the
  session id, but re-appending would duplicate turns).
- `thinking` blocks are not persisted.
