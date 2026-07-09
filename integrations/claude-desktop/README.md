# Claude Desktop

Two ways to point Claude at a hive: a **custom connector** (remote MCP over
OAuth — works in claude.ai and Claude Desktop) or the **`hive.mcpb` extension**
(a bearer API token over plain HTTP(S) — Claude Desktop only). Both land on the
same stateless MCP endpoint, `POST /mcp`, with the same tool surface.

|                       | Custom connector                          | `hive.mcpb` extension                    |
| --------------------- | ----------------------------------------- | ---------------------------------------- |
| Works in              | claude.ai + Claude Desktop                | Claude Desktop                           |
| Auth                  | OAuth 2.1 (PKCE + dynamic client registration) | Bearer API token                    |
| Hive must be reachable | over public HTTPS — for claude.ai the connection comes from Anthropic's servers | from your machine only — LAN / Tailscale / localhost all fine |
| Token minted by       | you, on hive's consent screen             | an admin (Account → API tokens)          |

## Custom connector (remote MCP)

1. **Settings → Connectors → Add custom connector**, URL:
   `https://<hive-host>/mcp`.
2. Claude discovers the OAuth server itself: the endpoint's 401 carries an
   RFC 9728 `www-authenticate` pointer to
   `/.well-known/oauth-protected-resource`, Claude registers via RFC 7591
   dynamic client registration (`POST /oauth/register`), and your browser opens
   hive's authorization page.
3. **Consent screen** (served by the SPA): sign in as yourself if you aren't
   already, then the card shows *"Claude wants to connect to hive as one of
   your AI identities."* Pick the identity under **Connect as**, pick a token
   lifetime under **Access lasts** (7 days – 1 year; "Never" when the server
   allows non-expiring tokens), and **Approve**.
4. Done. Claude now holds an MCP token that identifies every action as that AI
   identity. Revoke it anytime under **Account → Connected apps → Disconnect**
   (revokes all of that client's tokens).

Constraints, honestly stated:

- The hive host must be reachable **over HTTPS by whatever runs the
  connector**. For claude.ai that's Anthropic's servers — public DNS, real
  certificate. A LAN-only or Tailscale-only hive can't be a connector; use the
  `.mcpb` path below.
- Registration accepts HTTPS redirect URIs on any host and HTTP only on
  loopback (`localhost` / `127.0.0.1` / `::1`). Claude's callbacks are plain
  HTTPS, so this only ever bites hand-rolled dev clients.
- Dynamic client registration is unauthenticated and capped at **200
  registered clients** (`429 too_many_clients` past that). It's an abuse bound,
  not a lifecycle: disconnecting an app revokes its tokens but keeps the
  registration row.

## `hive.mcpb` (bearer token, LAN-friendly)

1. Grab `hive.mcpb` from the
   [latest release](https://github.com/bees-roadhouse/hive/releases) — or build
   it yourself: `./scripts/build-mcpb.sh` → `dist/hive.mcpb`.
2. Claude Desktop → **Settings → Extensions**, then drag `hive.mcpb` in (or
   open the file). Desktop for macOS/Windows bundles a Node runtime for `node`
   extensions; elsewhere have Node ≥ 18 on PATH.
3. In the extension settings, set **Hive URL** (base URL — `/mcp` is appended
   automatically, and a pasted `…/mcp` URL is also accepted) and **API token**.
4. The token comes from hive → **Account → API tokens** (admin-only): mint one
   for the AI identity this Claude should act as, and copy it at mint time —
   it's shown once. Non-admins connect via the consent flow above instead.

Under the hood: `manifest.json` wires up `server/bridge.mjs`, a
zero-dependency Node bridge that forwards Claude Desktop's stdio JSON-RPC
(newline-delimited) to `POST <hive-url>/mcp` with the Bearer header.
Notifications forward without a reply (hive answers `202`), upstream errors
come back as JSON-RPC error responses carrying hive's detail, and stderr —
which Claude Desktop collects into its MCP log files — carries the bridge's
logs. Hive's `/mcp` is stateless, so there is no session plumbing to break.

## Troubleshooting

| Symptom | Fix |
| ------- | --- |
| `401` / "Unauthorized — authorize via OAuth or provide a Bearer API token." | The token is mistyped, revoked, or expired — mint a fresh one (Account → API tokens) and update the extension settings. A brand-new hive also 401s `/mcp` until first-run onboarding completes. |
| Connector add fails before the consent screen | Open `https://<hive-host>/.well-known/oauth-authorization-server` in a browser. If that isn't reachable (from the public internet, for claude.ai), the connector can't reach it either — check DNS, certificate, and reverse-proxy routing for `/.well-known/*`, `/oauth/*`, and `/authorize`. |
| Consent page: "You don't own any AI identities to grant." | Your user has no AI identities assigned — an admin assigns one, then reconnect. |
| `429 too_many_clients` on connect | The 200-registration cap. Disconnecting apps only revokes tokens; clearing stale registrations is a DB operation today (`DELETE FROM oauth_clients WHERE …`). |
| Extension tools fail with "hive unreachable at …" | The URL isn't reachable from **your machine**. `curl -s -o /dev/null -w '%{http_code}\n' -X POST <hive-url>/mcp` should print `401` (or `406`) — anything else is network/proxy/port (`:7878` by default). |
| Wrong identity, or rotating a token | Tokens pin their AI identity when minted/granted. Mint a new one (or reconnect and pick again), update the client, then revoke the old token. |

Bridge logs land in Claude Desktop's MCP logs (the `mcp-server-*.log` files —
macOS: `~/Library/Logs/Claude/`, Windows: `%APPDATA%\Claude\logs\`).
