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

## Hosted session isolation

`hive-runner` can create one long-lived container per hosted workspace by using
the host Podman or Docker socket. Each human gets a named volume:

```text
hive-user-nate   -> mounted at /workspace in every Nate session container
hive-user-maggie -> mounted at /workspace in every Maggie session container
```

Every workspace gets its own container named:

```text
hive-session-<owner>-<session-id>
```

The runner labels containers with `hive.managed=true`, `hive.owner`, and
`hive.session`, mounts the per-user volume read/write, and writes the session
working directory under `/workspace/<session-id>`. User volume names include a
short hash of the owner slug so sanitized-name collisions cannot merge two
users' workspaces. Archiving a workspace removes that session container but
preserves the user's named volume.

The default session image is now `beesroadhouse/hive-session-dev:latest`, built
from `docker/Dockerfile.session-dev`. It includes the actual hosted-agent CLIs
(`claude`, `codex`, `opencode`), Rust/rustfmt/clippy, Node/pnpm/yarn, Python/uv/pytest,
.NET, Java/Maven/Gradle, Go, C/C++ build/debug tools, PostgreSQL/Redis clients,
Chromium, Firefox, Xvfb, Fluxbox, x11vnc, and noVNC. GUI is on by default via
`DISPLAY=:99`; set `HIVE_VNC=1` to expose VNC/noVNC inside the session container
when a human needs to watch or drive the desktop.

Deployment knobs:

```bash
HIVE_SESSION_ISOLATION=container
HIVE_CONTAINER_ENGINE=podman        # or docker / auto
HIVE_CONTAINER_SOCKET=/run/podman/podman.sock
HIVE_CONTAINER_SOCKET_TARGET=/run/podman/podman.sock
HIVE_SESSION_IMAGE=beesroadhouse/hive-session-dev:latest
HIVE_USER_VOLUME_PREFIX=hive-user
HIVE_SESSION_CONTAINER_PREFIX=hive-session
HIVE_SESSION_NETWORK=hive_hive      # docker-compose.rust.yml pins this network name
HIVE_SESSION_GUI=1                  # starts Xvfb/Fluxbox for GUI tests
HIVE_SESSION_VNC=0                  # set 1 to start x11vnc/noVNC inside sessions
```

For Docker, set:

```bash
HIVE_CONTAINER_ENGINE=docker
HIVE_CONTAINER_SOCKET=/var/run/docker.sock
HIVE_CONTAINER_SOCKET_TARGET=/var/run/docker.sock
```

The journal is the communications bus. Human inputs and assistant/system outputs
from hosted workspaces are mirrored into `/api/journal` with `workspace`, runtime,
and session-id tags. Transcript rows remain as a UI projection; durable
agent-to-agent and AI-to-human communication lives in the journal.

Hosted runtime sign-in uses a browser OAuth authorization-code + PKCE callback.
Configure each provider before exposing the Connect buttons:

```bash
HIVE_CODEX_OAUTH_CLIENT_ID=...
HIVE_CODEX_OAUTH_CLIENT_SECRET=...        # optional for public PKCE clients
HIVE_CODEX_OAUTH_AUTH_URL=https://provider.example/oauth/authorize
HIVE_CODEX_OAUTH_TOKEN_URL=https://provider.example/oauth/token
HIVE_CODEX_OAUTH_SCOPES="openid profile offline_access"
HIVE_CODEX_OAUTH_REDIRECT_URI=https://hive.example.com/api/runtime-oauth/codex/callback

HIVE_CLAUDE_CODE_OAUTH_CLIENT_ID=...
HIVE_CLAUDE_CODE_OAUTH_CLIENT_SECRET=...  # optional for public PKCE clients
HIVE_CLAUDE_CODE_OAUTH_AUTH_URL=https://provider.example/oauth/authorize
HIVE_CLAUDE_CODE_OAUTH_TOKEN_URL=https://provider.example/oauth/token
HIVE_CLAUDE_CODE_OAUTH_SCOPES="openid profile offline_access"
HIVE_CLAUDE_CODE_OAUTH_REDIRECT_URI=https://hive.example.com/api/runtime-oauth/claude_code/callback
```

If a redirect URI is omitted, Hive derives it from the request issuer. The callback
stores the returned refresh token when present, otherwise the access token, in the
encrypted `cc_credentials` vault for the signed-in human.

`HIVE_PROPAGATE_ENGINE_SOCKET=1` also mounts the host container socket into each
session container. Leave it off unless the agent inside that session must manage
nested containers; it gives that session container host-container control.

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
