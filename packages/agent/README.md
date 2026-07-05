# @hive/agent

Shared command surface for AI runtime integrations. Claude Code, Codex, and
Hermes wrappers should call this package instead of each implementing their own
Hive HTTP client.

Runtime packages live at:

- `plugins/claude-code-hive-memory`
- `plugins/codex-hive-memory`
- `plugins/hermes-hive-memory`

## Environment

```bash
HIVE_API_URL=http://localhost:7878
HIVE_API_TOKEN=hive_pat_...
HIVE_IDENTITY=pia
HIVE_PEER=nate
```

Use a long-lived or non-expiring token granted to the AI identity. The server
uses the token to decide both authorship and the human memory namespace.

## Commands

```bash
pnpm --filter @hive/agent start -- session-start --identity pia --peer nate
pnpm --filter @hive/agent start -- journal-add --title "Session notes" --tags=session "..."
pnpm --filter @hive/agent start -- mcp-smoke
```

`session-start` prints a ready-to-inject block containing:

- current date/time;
- the AI identity and optional peer/user;
- the last visible journal entries;
- high-confidence semantic journal recall via `/api/recall`;
- instructions for saving memory as rich first-person journal prose.

`journal-add` writes immutable prose to `/api/journal`. Authorship comes from
the token, not from a client-supplied author field.
