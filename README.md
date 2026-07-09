# hive — Rust + Solid.js

The shared state every Bee's Roadhouse / DTC AI config (Pia, Apis, Cera, and
peers) reads and writes (journal, tasks, **decisions**, events, notes,
knowledge-graph links, and a wire event log).

The current system of record is the Rust API on PostgreSQL, with a Solid/Vite
web UI and a Streamable HTTP MCP endpoint. Auth is bearer API tokens, OAuth MCP
tokens, OIDC/cookie sessions, and optional local email/password sessions.
Semantic search runs a local on-box embedder rather than a hosted model — see
[Semantic search](#semantic-search).

## Stack

| Package        | What it is                                                    |
| -------------- | ------------------------------------------------------------- |
| `hive-api`     | Rust/Axum HTTP API over PostgreSQL + an **MCP server** at `POST /mcp` |
| `hive-shared`  | Rust domain types shared by API/worker                       |
| `@hive/shared` | TypeScript domain types shared by the web/adapter packages   |
| `@hive/api`    | Legacy Node/Hono API kept for parity/reference tooling       |
| `@hive/web`    | [Solid.js](https://solidjs.com) + Vite single-page UI        |
| `@hive/cli`    | `hive` CLI — a stateless HTTP client over the API            |
| `@hive/agent`  | Thin runtime adapter used by Claude Code/Codex/Hermes plugins |
| `@hive/worker` | long-running process: polls feeds → wire, drains the outbound queue, refreshes embeddings, DB maintenance |

## Journal-first

The journal is the single, write-only input. You write prose; structured items
(**tasks / decisions / events**) emerge from it by *anchoring* a `{start,end}`
char span of the body — the entity keeps `origin_entry_id` + `anchor_text` so the
source sentence shows beside the card. In the UI you select text and tag it (no
offsets typed); over MCP the author passes spans in `journal_append`.
`@mentions` of known actors fan out to a per-actor **inbox** (humans *and* AIs).

## MCP-first

`POST /mcp` is a stateless [Streamable HTTP](https://modelcontextprotocol.io)
MCP server (official SDK). Tools: `journal_append` (with anchors), `journal_list`/
`_get`, `tasks_list`, `task_set_status`, `decisions_list`, `events_list`,
`inbox_list`/`_mark_read`, `search`, `semantic_search`, `dashboard`,
`sources_add`/`_list`/`_update`/`_remove`, `outbox_list`, `worker_status`.

Claude Desktop / claude.ai connect as an OAuth **custom connector** or via the
`hive.mcpb` bearer-token bundle — see
[integrations/claude-desktop](./integrations/claude-desktop/README.md).

## Worker

```bash
pnpm worker        # loop (every HIVE_WORKER_TICK secs, default 30)
pnpm worker:once   # one cycle then exit (CI / demo)
```

It polls every enabled **source** (RSS) into `feed.item` wire events (optionally
pinging an inbox), drains the **outbox** (webhooks, with retry/backoff), refreshes
**embeddings** for [semantic search](#semantic-search), and runs **maintenance**
(WAL checkpoint, FTS optimize, prune, vacuum). Sources are configured in the
**Settings** tab or via MCP. Embeddings carry the model they were made with, so
flipping the embedder makes the next worker cycle re-backfill automatically.

## Semantic search

Modeled on [`bees-roadhouse/bookstack-mcp`](https://github.com/bees-roadhouse/bookstack-mcp)'s
pipeline. `semanticSearch` (the `semantic_search` MCP tool and `/api/search?mode=semantic`)
runs a hybrid rank:

1. **Vector** — brute-force full-cosine over the corpus. Vectors are stored as
   packed little-endian f32 BLOBs; the scan is sub-10ms at this scale.
2. **Keyword blend** (`&hybrid=1`, default) — FTS5 keyword rank folded in
   (`0.7·vector + 0.2·keyword`).
3. **Markov-blanket boost** — entities whose link-graph neighbors also surfaced
   get a small bump.
4. **Cross-encoder rerank** (`&rerank=1`, opt-in) — re-orders the top-N.

Two embedders behind the `embed.ts` seam, chosen by `HIVE_EMBED`:

| `HIVE_EMBED`   | Model                          | Dim  | Notes |
| -------------- | ------------------------------ | ---- | ----- |
| `transformers` (default) | `Xenova/bge-small-en-v1.5` + `Xenova/bge-reranker-base` | 384 | The real local stack: a small, ARM/CPU-friendly BGE model via [@huggingface/transformers](https://github.com/huggingface/transformers.js) on onnxruntime. One-time model download (mount a models cache in prod). |
| `hash`         | `hash-ngram-v1`                | 256  | Deterministic, no download — instant offline. No reranker. CI selects this explicitly so the seed smoke stays fast + network-free. |

The default is the real local embedder — no env needed for a normal deploy.
Override the model with `HIVE_EMBED_MODEL` (e.g. `Xenova/bge-large-en-v1.5` for
1024d on a beefier host, or `Xenova/all-MiniLM-L6-v2` for a symmetric 384d model
— the BGE query instruction is applied only to BGE models). Set `HIVE_EMBED=hash`
to skip the model download (CI, offline dev). Flipping the model re-backfills only
rows whose stored model no longer matches, so the worker recomputes on the next cycle.

## Quick start

```bash
pnpm install
podman run --rm -d --name hive-dev-pg \
  -e POSTGRES_USER=hive -e POSTGRES_PASSWORD=hive -e POSTGRES_DB=hive \
  -p 5432:5432 docker.io/pgvector/pgvector:pg17

$env:DATABASE_URL="postgres://hive:hive@localhost:5432/hive"
$env:CARGO_TARGET_DIR="$env:USERPROFILE\.cargo-target\hive"
$env:HIVE_EMBED="hash"       # fast local/dev mode
cargo run -p hive-api        # api on :7878

pnpm dev:web                 # web on :5173, proxies to :7878
```

Open http://localhost:5173. The Vite dev server proxies `/api/*` to the API.

### CLI

```bash
set HIVE_API_URL=http://localhost:7878
pnpm hive tasks
pnpm hive tasks add "write the docs" --priority=high --tags=docs
pnpm hive decisions add "Use PostgreSQL" --decision="shared API, worker, and MCP state live in Postgres" --status=accepted
pnpm hive search postgres
```

## Entities

- **Task** — title, body, status (todo/doing/blocked/done), priority, tags, project
- **Decision** — ADR-style record: context → decision → consequences, with a
  lifecycle (`proposed → accepted → rejected → superseded`). Recording a
  decision that `supersedes` another auto-retires the old one and links them.
- **Note** — free-form titled notes
- **JournalEntry** — chronological log
- **Link** — directed edges forming the knowledge graph
- **WireEvent** — append-only event log; every mutation emits one

Journal, tasks, decisions, events, and wire items are indexed into the search
tables for unified `/api/search`.

## Storage model — PostgreSQL

The datastore is PostgreSQL, shared by the Rust API and worker. Journal entries
carry a `user_scope` namespace so each human gets their own memory stream while
mentions and explicit shares can surface selected entries to other humans and
AIs. Embeddings are stored in Postgres too, which keeps semantic recall, MCP,
the web UI, and worker maintenance on one durable state model.

## Admin + knowledge graph

The **admin** tab (and the `/api/worker`, `/api/embeddings`, `/api/outbox`
routes) surfaces the worker heartbeat + last cycle, embedding coverage
(per-kind / per-model, with a pending count), and the outbound job queue. The
**graph** tab renders the `links` knowledge graph (`/api/graph`) as a
force-directed node-link diagram — every journal entry and the tasks/decisions/
events anchored from it, plus `supersedes` edges — click a node to focus its
neighborhood.

## Conversations (hosted agent sessions)

The **Conversations** tab (`/api/workspaces`, the `cc_sessions`/`cc_messages`
tables) turns a prompt into a real agent run: `hive-runner` claims the session,
provisions a sandbox, drives Claude Code / Codex / OpenCode, and streams every
turn back as the transcript. Salient turns mirror into the journal as normal
entries.

**Isolation stance.** The runner executes agent turns with permission prompts
bypassed. That is safe only because each session runs in a **disposable
per-session container** (podman/docker) that is torn down on archive. If
`HIVE_SESSION_ISOLATION=host` (or no container engine is available by choice),
the same bypass would apply to the host itself, so the runner **refuses to
start** unless you explicitly set `HIVE_RUNNER_UNSAFE_HOST=1`.

**Runtime sign-in.** Users connect their own runtime credentials in Settings
(stored encrypted per user, decrypted only for the runner):

- **Claude Code / Codex subscriptions** — OAuth "connect" buttons, enabled by
  configuring `HIVE_CLAUDE_CODE_OAUTH_*` / `HIVE_CODEX_OAUTH_*` on the API:
  `_CLIENT_ID`, `_AUTH_URL`, `_TOKEN_URL` (required), plus optional
  `_CLIENT_SECRET`, `_REDIRECT_URI`, `_SCOPES`, `_PROVIDER`. Unconfigured
  runtimes return 501 and the UI falls back to token paste.
- **OpenCode** — manual paste of a provider API key (OpenRouter/Anthropic/
  OpenAI) in Settings; no OAuth flow.

**Lifecycle.** Archiving a conversation ends it and removes its sandbox; the
transcript stays. Ended conversations can be **deleted** from the UI
(`DELETE /api/workspaces/{id}`): the transcript and its graph links go, journal
mirror entries are history and stay. With `HIVE_CONVERSATION_RETENTION_DAYS`
set, the worker also hard-deletes archived conversations older than that many
days each cycle; unset (the default) keeps everything forever.

## Cloud dev env (GitHub / Claude Code on the web)

`/.claude/settings.json` registers a **SessionStart** hook that runs
[`dev-setup.sh`](./dev-setup.sh): it installs deps and seeds the DB so a
fresh web session has the Node/Solid tooling ready. Run the Rust API separately
with `cargo run -p hive-api`, then `pnpm dev:web` for the Vite UI. Re-running
setup is safe.

## Config

| Env            | Default                 | Used by    |
| -------------- | ----------------------- | ---------- |
| `PORT`         | `7878`                  | Rust API   |
| `DATABASE_URL` | `postgres://hive:hive@localhost:5432/hive` | Rust API, worker, tests |
| `HIVE_WEB_DIST` | auto-detect `packages/web/dist` | Rust API SPA fallback |
| `HIVE_API_URL` | `http://localhost:7878` | web proxy, CLI, agent adapters |
| `HIVE_LOCAL_AUTH_ENABLED` | `true` | enable/disable email/password login |
| `HIVE_OIDC_ENABLED` | `true` | allow OIDC when issuer/client env is present |
| `OIDC_ISSUER` / `OIDC_CLIENT_ID` / `OIDC_CLIENT_SECRET` / `OIDC_REDIRECT_URI` | unset | OIDC human login |
| `OIDC_ALLOWED_DOMAINS` | unset | auto-provision allowed OIDC email domains |
| `HIVE_OAUTH_ALLOW_NEVER_EXPIRES` | `true` | show/allow non-expiring OAuth/MCP tokens |
| `HIVE_EMBED`   | `transformers`          | api, worker — set `hash` to skip the model download (CI/offline) |
| `HIVE_EMBED_MODEL` | `Xenova/bge-small-en-v1.5` | api, worker (transformers mode) |
| `HIVE_RERANK_MODEL` | `Xenova/bge-reranker-base` | api, worker (transformers mode) |
| `HIVE_EMBED_STAGE_BUDGET_SECS` | `20` | worker — wall-clock budget per embedding backfill stage; stale items past it defer to the next cycle |
| `HIVE_CLAUDE_CODE_OAUTH_*` / `HIVE_CODEX_OAUTH_*` | unset (501) | Rust API — runtime subscription sign-in, see [Conversations](#conversations-hosted-agent-sessions) |
| `HIVE_CONVERSATION_RETENTION_DAYS` | unset (keep forever) | worker — hard-delete archived conversations older than N days |
| `HIVE_SESSION_ISOLATION` | `container` | runner — `host` refuses to start without `HIVE_RUNNER_UNSAFE_HOST=1` |

## Branching

Adapted from [BR canon](https://kb.beesroadhouse.com/books/developer-operations-devops/page/branching-strategy)
to a single-branch model (the `development`/`release` pair collapsed into
`main` on 2026-07-05):

- `main` … the only long-lived branch; always releasable.
- `feature/{slug}`, `bug/{slug}`, `improvement/{slug}`, `refactor/{slug}`
  branch from main and merge back via PR. Every PR publishes immutable
  `sha-{sha}` images; merging retags them `dev` (never rebuilds).
- Releases are tag-driven: bump versions in a release PR, merge it, then
  `git tag v{version} && git push origin v{version}` — the workflow retags
  that merge's images as `{version}` + `latest` and cuts the GitHub Release.
