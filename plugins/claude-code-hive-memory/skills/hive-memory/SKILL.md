---
name: hive-memory
description: Use Hive as Claude Code session memory: session-start recall, Hive MCP, and durable journal memory saves.
---

# Hive Memory

Use Hive as the durable memory backend. Do not replace it with Claude Code's
local memory files.

At session start this plugin injects a Hive memory block through its
`SessionStart` hook. The block already contains current date/time, the AI
identity, recent journal entries, high-confidence semantic recall, and memory
write rules. Do not make an extra startup MCP call just to rediscover the same
context.

When saving durable memory, write first-person prose through `@hive/agent`:

```bash
pnpm --dir "$HIVE_REPO_PATH" --filter @hive/agent start -- journal-add --tags=session "..."
```

Include concrete names, dates, decisions, feelings or preferences when relevant,
and why this memory should matter later. Mention humans or AIs with `@name`
when the memory should be shared into their visible journal.

Use the bundled Hive MCP server for live work with tasks, recall, search, and
journal tools after startup.
