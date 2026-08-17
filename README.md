# hive

A self-hostable, multi-org memory engine for people and their AIs. You write
prose; tasks, decisions, events, and a knowledge graph emerge from it by
anchoring spans of the text. Claude Code and Claude Desktop read and write the
same store over MCP, so what an agent remembers and what you see in the browser
are one thing.

One Rust binary, `hive-api`, serves all of it: the JSON API, an OAuth 2.1
authorization server, the MCP endpoint at `POST /mcp`, and the Solid.js SPA.
PostgreSQL holds the data, and access is enforced by row-level security rather
than by application checks.

## Where it stands

**Early. Self-host at your own risk.**

This branch is under active repair following a security review that reproduced
cross-org data leaks. Fixes are landing. Until they have settled, treat a hive
instance as single-tenant: run it for one household or one team, and do not put
two parties who should not see each other's data in the same instance.

Other things worth knowing before you commit to it:

- The API is the surface, and it is not versioned. Routes still move.
- Offline behaviour is a cache, not local-first, and the write-conflict model is
  still an open question ([docs/WEB-APP.md](./docs/WEB-APP.md)).
- Mail sync is present but off by default (`HIVE_MAIL_ENABLED`).
- The relay that lets you reach a home instance from elsewhere is a spike with
  a demo, not a service ([docs/RELAY.md](./docs/RELAY.md)).
- The Claude Code plugin under `plugins/` is broken and the Claude Desktop
  `.mcpb` is dead. Both targeted the removed local-bridge architecture. Their
  READMEs explain what to do instead.

## Architecture

| Crate | What it is |
| --- | --- |
| `api/` | `hive-api` ... axum. JSON API, OAuth 2.1 AS, `POST /mcp`, and the SPA |
| `core/` | `hive-core` ... the store, the Postgres schema and migrations, RLS, the acting-org scope |
| `shared/` | domain types shared across the Rust crates |
| `embed/` | embedding seam: ONNX/BGE local models with a deterministic hash fallback |
| `jmap-sync/` | JMAP mail sync library |
| `relay/` | `hive-relay` and `hive-relay-agent` ... SNI-passthrough tunnel so a home instance is reachable without port forwarding |

| Package | What it is |
| --- | --- |
| `packages/web` | the Solid.js SPA, built by vite and served by `hive-api` |
| `packages/shared` | TypeScript types shared with the SPA |

### How access control works

Two rules carry the weight, both in [docs/WEB-APP.md](./docs/WEB-APP.md):

**Row-level security, not application checks.** Every content table carries an
`org_id` and a policy keyed to `hive.acting_org`, set per request. Policies use
`FORCE ROW LEVEL SECURITY` and `WITH CHECK` as well as `USING`, and the API
connects as a role that is neither superuser nor `BYPASSRLS`. There is no code
path that forgets the predicate, because it is not code.

**One acting org per credential.** A session or an API token is minted against
exactly one org and nothing afterwards changes it. No header, query parameter,
or body selects an org. A human switching orgs logs in again; an agent cannot
switch at all. That closes the leak RLS cannot see, where an identity reads from
one scope it legitimately holds and writes into another it also holds.

Encryption is at rest only, and deliberately so: full-text search and pgvector
similarity cannot run on ciphertext, and search is the product. Mail account
credentials are the exception and are column-encrypted, which is what
`HIVE_CRED_KEY` is for. Back that key up separately from the database.

## Run it locally

### Containers

```bash
podman compose -f docker/docker-compose.local.yml up -d --build
```

Postgres plus `hive-api` on <http://localhost:7878>. The credential key in that
file is a fixed dev value so it runs with no setup; for anything real use
`docker/docker-compose.rust.yml`, which refuses to start without a
`HIVE_CRED_KEY` you supply.

### From source

Bring up a pgvector Postgres (`./dev-setup.sh` creates a `hive-pg` container on
5432), build the SPA once, then run the API:

```bash
corepack enable          # pnpm comes from the packageManager pin, not your PATH
pnpm install
pnpm --filter @hive/web build

DATABASE_URL=postgres://hive:hive@127.0.0.1:5432/hive \
HIVE_CRED_KEY=local-dev-credential-key-not-for-production \
HIVE_EMBED=hash \
HIVE_WEB_DIST=packages/web/dist \
cargo run -p hive-api
```

`HIVE_WEB_DIST` is optional when you run from the repo root, since `packages/web/dist`
is the default candidate; without a built SPA the API still serves the JSON API
and MCP, and non-API paths 404.

For SPA work, `pnpm --filter @hive/web dev` runs vite on 5173 and proxies `/api`,
`/oauth`, `/authorize`, `/.well-known` and `/mcp` to `hive-api`.

### First run

A fresh database opens onto onboarding: instance name, admin name, admin email,
password. That creates the admin user, the default org, and signs you in.

### Configuration

| Variable | Default | Purpose |
| --- | --- | --- |
| `DATABASE_URL` | ... | pgvector-capable PostgreSQL 17. The role needs `CREATEROLE`, or set `HIVE_APP_DATABASE_URL` to a serving role you provisioned |
| `HIVE_CRED_KEY` | ... | Required. AES-GCM key for the mail credential vault. Back it up separately from the database |
| `PORT` | `7878` | HTTP listener |
| `HIVE_EMBED` | ONNX | `hash` for a deterministic offline embedder with no model download |
| `HIVE_WEB_DIST` | `packages/web/dist`, then `/app/web` | Where the built SPA lives |
| `HIVE_PUBLIC_URL` | unset | Public origin for OAuth metadata. Also settable as the `instance.url` config value |
| `HIVE_MAIL_ENABLED` | `0` | JMAP mail sync |

## Connecting Claude

`POST /mcp` is a streamable HTTP MCP endpoint carrying 53 tools (journal, tasks,
search, semantic search, recall, entities, people, workspaces, mail archive,
conversation capture), implemented in `api/src/mcp.rs` and served by
`api/src/routes/mcp.rs`.

Mint a token under **Account** in the SPA (admin only, shown once), then:

```bash
claude mcp add --transport http hive http://localhost:7878/mcp \
  --header "Authorization: Bearer hive_pat_…"
```

Clients that prefer OAuth can register themselves: hive implements RFC 8414 and
RFC 9728 discovery, RFC 7591 dynamic client registration, and authorization code
with PKCE. See
[integrations/claude-desktop/README.md](./integrations/claude-desktop/README.md)
for the flow and the three things that reliably break it.

## The gate

Rust toolchain is pinned by `rust-toolchain.toml` so local runs and CI agree.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
DATABASE_URL=postgres://hive:hive@127.0.0.1:5432/hive \
HIVE_CRED_KEY=ci-credential-vault-key-not-a-secret \
HIVE_EMBED=hash cargo test --workspace

corepack enable
pnpm install --frozen-lockfile
pnpm --filter @hive/web build
```

Tests need a real pgvector Postgres: each one builds its own schema, migrates
it, and drops it on the way out. The `DATABASE_URL` role must be able to
`CREATE ROLE`, because the org-isolation tests provision an unprivileged role
and serve as it to prove the policies actually fire.

## Documentation

- [docs/WEB-APP.md](./docs/WEB-APP.md) ... the current architecture. Authoritative.
- [docs/ARTIFACTS.md](./docs/ARTIFACTS.md) ... artifact types, derived text, thumbnails.
- [docs/RELAY.md](./docs/RELAY.md) ... reaching a self-hosted hive from elsewhere.
- [docs/DIRECTION.md](./docs/DIRECTION.md) ... the decision record. D16-D20, D29-D36 and D41-D44 describe the superseded local-first design; read them as history.

## Branching

- `main` is the only long-lived branch and must stay releasable.
- Work branches are `feature/{slug}`, `bug/{slug}`, `improvement/{slug}`,
  `refactor/{slug}`, merged by PR. CI runs fmt, clippy, build, test, and the SPA
  build.
- The version of record is `[workspace.package]` in the root `Cargo.toml`.
