---
name: hive-memory
description: "Use Hive as Codex session memory: inject session-start recall, verify MCP access, and save durable memories as rich journal prose through the shared @hive/agent adapter."
---

# Hive Memory

Use this skill when the user asks Codex to use Hive memory, save memory to Hive,
load session context, or work with the Bee's Roadhouse Hive agent workspace.

Hive is journal-first. Do not replace it with Codex-local memory. Use the Hive
adapter in the checked-out repo so Claude Code, Codex, and Hermes share one
memory contract.

## Required environment

- `HIVE_API_URL`: Hive base URL, for example `https://hive.example.com` or
  `http://localhost:7878`.
- `HIVE_API_TOKEN`: long-lived or never-expiring Hive token.
- `HIVE_IDENTITY`: AI actor slug, for example `pia`.
- `HIVE_PEER`: optional human/user focus, for example `nate`.

## Session start

At session start, or when the user asks to load memory, run:

```bash
pnpm --dir /path/to/hive --filter @hive/agent start -- session-start
```

Inject the command stdout into working context. It already includes current
date/time, recent visible journal entries, high-score semantic journal recall,
and memory-writing rules. Do not make an extra startup MCP call just to
rediscover the same context.

If the checkout path is unknown, ask one concise question for the Hive repo
path. Do not guess paths outside the current machine.

## Saving memory

When saving durable memory, write prose, not terse facts. Include who was
present, what happened, dates, decisions, emotion or preference when relevant,
and why the memory should matter later.

Use:

```bash
pnpm --dir /path/to/hive --filter @hive/agent start -- journal-add --tags=session "..."
```

or pipe markdown into `journal-add`. Authorship comes from the Hive token.

## MCP verification

To verify a token can use Hive MCP:

```bash
pnpm --dir /path/to/hive --filter @hive/agent start -- mcp-smoke
```

The expected result includes `ok: true` and a positive tool count.
