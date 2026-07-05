# Hive Memory for Claude Code

Claude Code plugin for Hive-backed AI memory.

It provides:

- a `SessionStart` hook that injects Hive recall through `@hive/agent`;
- an HTTP MCP config for Hive's `/mcp` endpoint;
- a skill and slash command guidance for saving memory as Hive journal prose.

## Environment

```bash
HIVE_API_URL=https://hive.example.com
HIVE_API_TOKEN=hive_pat_...
HIVE_REPO_PATH=/path/to/hive
HIVE_IDENTITY=pia
HIVE_PEER=nate
```

`HIVE_REPO_PATH` is optional when this plugin is run directly from the Hive
checkout. It is required when Claude Code installs the plugin into its plugin
cache.

`HIVE_URL` and `HIVE_TOKEN` are accepted as legacy aliases by the hook, but the
MCP config uses `HIVE_API_URL` and `HIVE_API_TOKEN`.

## Install

Development:

```bash
claude --plugin-dir /path/to/hive/plugins/claude-code-hive-memory
```

The hook soft-fails with setup text instead of blocking Claude Code when Hive is
unconfigured or unavailable.
