# Hive Memory for Claude Code

**This plugin does not currently work. Skip to [Connecting Claude Code to hive
today](#connecting-claude-code-to-hive-today) for the path that does.**

The plugin's MCP entry (`.mcp.json`) and both hook handlers
(`hooks-handlers/*.mjs`) run a `hive-bridge` binary against a local hive data
directory. There is no `bridge/` crate and no `hive-bridge` binary: hive is now
an HTTP API over PostgreSQL ([docs/WEB-APP.md](../../docs/WEB-APP.md)), MCP is
served at `POST /mcp` by `hive-api`, and there is no local data dir to open.
Installing the plugin as-is gets you an MCP server that fails to spawn and two
hooks that soft-fail with "hive-bridge not found on PATH".

Porting it means rewriting `.mcp.json` as an HTTP server entry and rewriting the
hooks to call `POST /mcp` over HTTP with a bearer token. That work has not been
done.

## Connecting Claude Code to hive today

No plugin involved. Claude Code speaks MCP over HTTP, and so does hive.

### 1. Have a hive running

From a checkout, with a PostgreSQL 17 + pgvector database:

```bash
podman compose -f docker/docker-compose.local.yml up -d --build
```

That serves everything on <http://localhost:7878>. See the
[root README](../../README.md) for running `hive-api` from source instead.

### 2. Finish onboarding and mint a token

Open <http://localhost:7878>. A fresh database opens onto the onboarding form
(instance name, admin name, admin email, password); it creates the admin user
and signs you in.

Then, as an admin, open **Account** and mint an API token: pick the actor the
token writes as, give it a label, choose an expiry. The token is shown once.
It looks like `hive_pat_…`.

The same thing over HTTP, if you would rather not click:

```bash
curl -X POST http://localhost:7878/api/tokens \
  -H 'Content-Type: application/json' -b cookies.txt \
  -d '{"actor":"pia","label":"claude-code"}'
```

`cookies.txt` is a session from `POST /api/auth/login`. Token creation is
admin-only, and the acting org is fixed when the token is minted: a token reads
and writes in exactly one org, forever.

### 3. Point Claude Code at it

```bash
claude mcp add --transport http hive http://localhost:7878/mcp \
  --header "Authorization: Bearer hive_pat_…"
```

Check it:

```bash
claude mcp list
# hive: http://localhost:7878/mcp (HTTP) - ✔ Connected
```

53 tools land in the session: journal, tasks, search, semantic search, recall,
entities, people, workspaces, mail archive, conversation capture. Writes are
authored by the token's actor, not by whatever the caller claims.

### The OAuth alternative

`hive-api` is also an OAuth 2.1 authorization server, so a client that does
dynamic client registration can authorize itself instead of carrying a token
you pasted. Discovery is at `/.well-known/oauth-authorization-server` and
`/.well-known/oauth-protected-resource`; registration is `POST /oauth/register`;
the flow is authorization code + PKCE (S256 only), scope `mcp`.

One prerequisite that is easy to hit and gives a bad error: **consent only
offers AI identities that the signed-in human owns.** With none, the consent
screen has nothing to grant and the flow dead-ends. An AI identity is a `people`
row with `kind = 'ai'` and `owner` set to your slug. `POST /api/people` creates
the row but does not set `owner`, so it takes a second call:

```bash
curl -X POST  http://localhost:7878/api/people        -b cookies.txt \
  -H 'Content-Type: application/json' -d '{"name":"Pia","kind":"ai"}'
curl -X PATCH http://localhost:7878/api/people/pia    -b cookies.txt \
  -H 'Content-Type: application/json' -d '{"owner":"your-slug"}'
```

The SPA has no screen for either call yet.

## What the plugin still carries

Two pieces are independent of the transport and still make sense:

- `skills/hive-memory/SKILL.md` and `commands/save-hive-memory.md` teach Claude
  to save durable memory as journal prose. Nothing in them assumes a bridge.
- `hooks-handlers/session-start.mjs` composes a session brief from the `recall`
  tool and syncs the identity's enabled skills/agents/commands into `.claude/`.
  The logic is sound; only its transport is dead.

## Environment

The variables the hooks read (`HIVE_ACTOR`, `HIVE_BRIDGE_BIN`, `HIVE_PEER`,
`HIVE_HOOK_TIMEOUT_MS`) all describe the bridge that no longer exists. They are
documented here only so nobody wastes time setting them expecting an effect.
