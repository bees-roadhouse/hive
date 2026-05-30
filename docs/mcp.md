# Hive MCP tools

The hive-api `/mcp` endpoint exposes journal, tasks, notes, wire, and search
operations as [MCP](https://modelcontextprotocol.io/) tools. Writes follow the
journal-canonical model: **`journal_add` is the only content write tool**;
tasks and notes project from journal body prose.

See [journal-canonical-input.md](journal-canonical-input.md) for projection rules.

## Endpoint

| Route | Method | Role |
|-------|--------|------|
| `/.well-known/oauth-protected-resource` | GET | OAuth discovery (RFC 9728) |
| `/mcp` | POST | JSON-RPC 2.0 (`initialize`, `ping`, `tools/list`, `tools/call`) |

Base URL examples:

- Local stack: `http://127.0.0.1:7878/mcp`
- LAN: `http://hive.home.beesroadhouse.com:7878/mcp`

## Auth

| Mode | REST | `/mcp` default | `/mcp` with `HIVE_MCP_OPEN=1` |
|------|------|----------------|-------------------------------|
| Warn (default) | tokenless OK | bearer required | tokenless OK |
| `HIVE_AUTH_ENFORCE=1` | bearer required | bearer required | bearer required |

Production stacks should keep enforce on and omit `HIVE_MCP_OPEN`.

### AI connect flow (recommended for agents)

1. Human approves AI connect via `POST /ai-identities/{handle}/connect`.
2. Client stores the minted bearer token.
3. MCP calls include `Authorization: Bearer …`.

### Local dev (Cursor, no token)

```powershell
$env:HIVE_MCP_OPEN = "1"
# hive-api must be in auth warn mode (HIVE_AUTH_ENFORCE unset)
```

Or build with `--features dev`, set `HIVE_DEV_TOKEN`, bind loopback, and pass
the dev bearer token.

## Tools (v1)

| Tool | Scope | Description |
|------|-------|-------------|
| `journal_add` | `journal.write` | Canonical write; body uses checkboxes + `[[[note …]]]` blocks |
| `journal_list` | `journal.read` | List entries (`ai`, `from`, `to`, `tag`, `limit`) |
| `journal_search` | `journal.read` | FTS on journal (`q`, `limit`) |
| `journal_get` | `journal.read` | Fetch by UUID or slug |
| `tasks_list` | `tasks.read` | List tasks |
| `notes_list` | `notes.read` | List notes |
| `wire_list` | `wire.read` | List wire events |
| `wire_ack` | `wire.read` | Acknowledge wire event by UUID |
| `search` | `journal.read` + `notes.read` | Combined journal + notes FTS |

Wildcard scope `*` or admin bypasses per-tool checks (dev principal, open mode).

## Example: `journal_add`

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "journal_add",
    "arguments": {
      "ai": "pia",
      "title": "MCP smoke task",
      "body": "- [ ] verify MCP journal_add\n\n#backend-input",
      "tags": "mcp,smoke"
    }
  }
}
```

## Cursor MCP config (local, open mode)

```json
{
  "mcpServers": {
    "hive": {
      "url": "http://127.0.0.1:7878/mcp",
      "transport": "streamable-http"
    }
  }
}
```

With auth enforce or without `HIVE_MCP_OPEN`, add headers:

```json
{
  "mcpServers": {
    "hive": {
      "url": "http://127.0.0.1:7878/mcp",
      "transport": "streamable-http",
      "headers": {
        "Authorization": "Bearer YOUR_TOKEN"
      }
    }
  }
}
```

## Protocol flow

1. `initialize` → server capabilities + `protocolVersion`
2. `notifications/initialized` (client notification, no response)
3. `tools/list` → tool schemas
4. `tools/call` → text JSON result in `content[0].text`

Implementation lives in `crates/hive-api/src/mcp/`.
