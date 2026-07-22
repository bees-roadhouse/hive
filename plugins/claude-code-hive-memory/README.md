# Hive Memory for Claude Code

Claude Code plugin for Hive-backed AI memory — against the hive store on
**this machine**. Zero JS dependencies: the MCP server entry and the hooks
both run the `hive-bridge` binary, which opens the local data dir directly.
There is no server, no URL, and no API token (hive is personal and
local-first — [DIRECTION.md](../../docs/DIRECTION.md) D25); the OS user
boundary is the auth.

It provides:

- an MCP server entry (`hive-bridge` on stdio) exposing Hive's tools —
  journal, tasks, search, semantic search, recall, entities, dashboard;
- a `SessionStart` hook that injects the Hive recall brief (identity
  profile, open tasks, unread inbox, relevant journal, recent events,
  projects) and syncs the identity's enabled skills/agents/commands into the
  project's `.claude/` directory;
- a skill and a `/save-hive-memory` command teaching Claude to save durable
  memory as Hive journal prose.

Transcript capture at session end is paused: it died with the hosted
teardown and returns as an MCP-fed source in Phase 3
([PLAN.md](../../docs/PLAN.md) PR 3.6).

## Requirements

`hive-bridge` must be on `PATH`. From a hive checkout:

```bash
cargo install --path bridge
```

(Release packaging that bundles the binary lands with the Phase 2 app
bundles; until then, installing from the repo is the supported path.)

**The hive app must be running.** The bridge is a proxy (D25): it connects
to the app over `<data_dir>/bridge.sock` and has no store access of its
own. While the app is closed, MCP calls and hooks fail with "the hive app
is not running" — start the app to use Claude Code with hive. Any number
of bridge clients (Claude Code, Claude Desktop, hooks) can connect to the
running app at once.

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

All optional:

| Variable | Default | Purpose |
|---|---|---|
| `HIVE_ACTOR` | `$USER` | Acting identity for hook calls (authorship pin) — matches the app's author default |
| `HIVE_BRIDGE_BIN` | `hive-bridge` | Path to the bridge binary when it isn't on `PATH` |
| `HIVE_PEER` | — | Optional focus actor for the recall brief |
| `HIVE_HOOK_TIMEOUT_MS` | `10000` | Per-call timeout for hook bridge calls |

## Hook behavior

**SessionStart** runs `hive-bridge call recall` and injects the composed
brief as session context, then `hive-bridge call identity_artifacts_sync`
and writes the enabled artifacts into the session cwd:
`.claude/skills/<name>/SKILL.md`, `.claude/agents/<name>.md`,
`.claude/commands/<name>.md`. The sync records what it wrote in
`.claude/.hive-synced.json` and prunes only files listed there when they
leave the enabled set — it never touches files you authored.

The hook soft-fails: no `hive-bridge` on `PATH` means a silent no-op, and
any other failure (the hive app not running) costs one stderr line — a
broken hive never blocks a session.

Note: the brief's *relevant journal* section rides the semantic index, whose
backfill daemon is paused until the Phase 2/3 app loop — profile, tasks,
inbox, and events populate regardless.

## Uninstall

Removing the plugin stops the hook; it does not remove artifacts already
synced into projects. To clean a project, delete the files listed in
`.claude/.hive-synced.json` (and the index itself) — everything else under
`.claude/` is yours, not the plugin's.
