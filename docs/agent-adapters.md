# AI agent adapters

Hive is the memory backend. Runtime-specific plugins should stay thin:

1. Run `hive-agent session-start` at session start and inject stdout into the
   model context.
2. Call `hive-agent journal-add` when the AI decides durable memory should be
   saved.
3. Configure MCP clients to use Hive's `/mcp` endpoint with the same Bearer
   token, or run the OAuth consent flow to mint a long-lived token.

## Shared command

```bash
pnpm --dir /path/to/hive --filter @hive/agent start -- session-start \
  --identity pia \
  --peer nate \
  --threshold 0.72
```

Required env:

```bash
HIVE_API_URL=https://hive.example.com
HIVE_API_TOKEN=hive_pat_...
```

Recommended optional env:

```bash
HIVE_IDENTITY=pia
HIVE_PEER=nate
HIVE_RECALL_THRESHOLD=0.72
HIVE_RECALL_BUDGET=1500
```

## Claude Code

Use the installable Claude Code plugin:

```bash
claude --plugin-dir /path/to/hive/plugins/claude-code-hive-memory
```

It provides a `SessionStart` hook, Hive MCP config, a Hive memory skill, and a
`/save-hive-memory` command guide. The hook calls the shared `@hive/agent`
adapter and injects stdout as Claude Code `additionalContext`.

Set `HIVE_REPO_PATH` when the plugin is installed outside the Hive checkout.

See [`integrations/claude-code/settings.example.json`](../integrations/claude-code/settings.example.json).
See [`plugins/claude-code-hive-memory`](../plugins/claude-code-hive-memory).

## Codex

Use the same `session-start` command from a Codex plugin or hook. Keep the Hive
repo path and token in local configuration, not in the plugin source. The Codex
plugin should only wrap this command and provide setup text.

See [`plugins/codex-hive-memory`](../plugins/codex-hive-memory).

## Hermes

Use the native Hermes memory-provider plugin:

```bash
mkdir -p ~/.hermes/plugins/memory
cp -r /path/to/hive/plugins/hermes-hive-memory ~/.hermes/plugins/memory/hive
hermes memory setup
```

Select `hive` as the active memory provider. The provider implements Hermes's
`MemoryProvider` lifecycle: `system_prompt_block()` injects session recall,
`prefetch()` recalls per turn, built-in memory writes mirror to Hive journal
entries, and tools expose Hive journal add, recall, and search.

See [`integrations/hermes/hive-memory.example.json`](../integrations/hermes/hive-memory.example.json).
See [`plugins/hermes-hive-memory`](../plugins/hermes-hive-memory).

## MCP

MCP clients use:

```text
POST https://hive.example.com/mcp
Authorization: Bearer hive_pat_...
Accept: application/json, text/event-stream
Content-Type: application/json
```

Run `hive-agent mcp-smoke` after configuring a token to verify the endpoint
returns the tool list.
