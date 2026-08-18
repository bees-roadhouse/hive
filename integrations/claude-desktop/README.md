# Claude Desktop

**The `hive.mcpb` extension this directory described is dead.** What remains is
`mcpb/manifest.json`, which launches a `hive-bridge` binary against a local
hive app. There is no `bridge/` crate, no `hive-bridge` binary, and no local
data directory: hive is now an HTTP API over PostgreSQL
([docs/WEB-APP.md](../../docs/WEB-APP.md)) and MCP is served at `POST /mcp` by
`hive-api`. Nothing here packs the bundle any more, and the manifest is kept
only as a record of the shape it had.

Connect Claude Desktop as a **custom connector** pointed at your hive's `/mcp`
URL instead. `hive-api` is a full OAuth 2.1 authorization server, so Desktop can
register itself and authorize without you pasting a token anywhere.

## What hive serves

Verified against a running instance:

| Endpoint | What it is |
| --- | --- |
| `POST /mcp` | Streamable HTTP MCP. Bearer auth; 401 carries `WWW-Authenticate` with an RFC 9728 `resource_metadata` pointer |
| `/.well-known/oauth-authorization-server` | RFC 8414 metadata |
| `/.well-known/oauth-protected-resource` | RFC 9728 metadata |
| `POST /oauth/register` | RFC 7591 dynamic client registration, no client secret |
| `GET /authorize` | Validates, then redirects to the SPA consent screen |
| `POST /oauth/token` | Authorization code + PKCE (S256 only), scope `mcp` |

## Connecting

1. Put your hive on an HTTPS origin Claude Desktop can reach. Localhost works
   only for a client running on the same machine.
2. In Claude Desktop, **Settings → Connectors → Add custom connector**, and give
   it `https://<your-hive>/mcp`.
3. Sign in to that hive in a browser when the consent screen appears, choose the
   AI identity the connector acts as, and pick a token lifetime.

Honest scope: the server half of this flow is verified end to end (registration,
`/authorize`, consent, code-plus-PKCE exchange, and the resulting token calling
tools on `/mcp`). The Claude Desktop side of it has not been run, so treat step
2 as the documented shape rather than a tested one.

## Three things that will stop you

**The consent screen needs an AI identity you own.** It only offers `people`
rows with `kind = 'ai'` whose `owner` is your slug. With none, the screen has
nothing to grant and the flow dead-ends. The SPA's **Admin** screen does both
halves: the Writers section creates the person (set the kind to `ai`) and
shows an owner picker on every AI row. Over HTTP instead, as an admin —
`POST /api/people` creates the row but leaves `owner` null, and moving
`owner` is admin-only, so it takes two calls:

```bash
curl -X POST  https://<your-hive>/api/people     -b cookies.txt \
  -H 'Content-Type: application/json' -d '{"name":"Pia","kind":"ai"}'
curl -X PATCH https://<your-hive>/api/people/pia -b cookies.txt \
  -H 'Content-Type: application/json' -d '{"owner":"your-slug"}'
```

**The advertised issuer has to be the public one.** Every OAuth URL derives from
it, so if hive advertises `http://localhost:7878` a remote client cannot follow
the flow. Resolution order is the `instance.url` config value, then
`HIVE_PUBLIC_URL`, then `X-Forwarded-Proto` / `X-Forwarded-Host`, then the
`Host` header. Behind a TLS-terminating proxy, either set `HIVE_PUBLIC_URL` or
make sure the proxy forwards both headers, or the metadata advertises `http://`
and OAuth 2.1 clients refuse it.

**Redirect URIs must be HTTPS**, except on loopback (`localhost`, `127.0.0.1`,
`::1`), where plain HTTP is allowed for development callbacks. Fragments are
rejected.

## Bearer token instead

If you would rather not run OAuth, mint a token in the SPA under **Account**
(admin only, shown once) and hand it to any MCP client that sends a header:

```
Authorization: Bearer hive_pat_…
```

A token's acting org is fixed when it is minted and never changes. That is the
design, not a limitation: an agent authenticates into one org and cannot read
from one and write to another.

## Tuning

| Variable | Default | Effect |
| --- | --- | --- |
| `HIVE_PUBLIC_URL` | unset | Public origin for OAuth metadata, unless `instance.url` config is set |
| `HIVE_OAUTH_ALLOW_NEVER_EXPIRES` | `true` | Lets the consent screen offer a non-expiring token |
| `HIVE_LOCAL_AUTH_ENABLED` | `true` | Set false for SSO-only; onboarding still works |
| `HIVE_OIDC_ENABLED` | `true` | OIDC login, dormant unless `OIDC_ISSUER`, `OIDC_CLIENT_ID` and `OIDC_REDIRECT_URI` are all set |

## Disconnecting

An admin can list registered clients and revoke every token one holds from
**Account**, or over the API: `GET /api/oauth/clients` and
`DELETE /api/oauth/clients/{client_id}`.
