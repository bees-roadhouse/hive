# hive — Node.js + Solid.js

The shared state every Bee's Roadhouse / DTC AI config (Pia, Apis, Cera, and
peers) reads and writes (journal, tasks, **decisions**, events, notes,
knowledge-graph links, and a wire event log).

**Zero-infra**: a single-file SQLite database, so it spins up instantly in a
fresh container.

> Node.js + Solid.js (it replaced an earlier Rust/Postgres prototype; this is the
> system of record now). Auth is bearer API tokens plus cookie sessions; semantic
> search runs a local on-box embedder rather than a hosted model — see
> [Semantic search](#semantic-search).

## Stack

| Package        | What it is                                                    |
| -------------- | ------------------------------------------------------------- |
| `@hive/shared` | TypeScript domain types shared by every package              |
| `@hive/api`    | [Hono](https://hono.dev) HTTP API over SQLite (better-sqlite3) + FTS5 + an **MCP server** at `POST /mcp` |
| `@hive/web`    | [Solid.js](https://solidjs.com) + Vite single-page UI        |
| `@hive/cli`    | `hive` CLI — a stateless HTTP client over the API            |
| `@hive/worker` | long-running process: polls feeds → wire, drains the outbound queue, refreshes embeddings, DB maintenance |

Run via Node's native TypeScript stripping (`--experimental-strip-types`) — no
build step needed for dev.

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
pnpm seed        # create + seed data/hive.db (once)
pnpm dev         # api on :8787, web on :5173
```

Open http://localhost:5173. The Vite dev server proxies `/api/*` to the API.

### CLI

```bash
pnpm hive tasks
pnpm hive tasks add "write the docs" --priority=high --tags=docs
pnpm hive decisions add "Use SQLite" --decision="single-file db, zero infra" --status=accepted
pnpm hive search sqlite
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

Tasks, notes, journal, and decisions are all indexed into one SQLite **FTS5**
table for unified `/api/search`.

## Storage model — SQLite, not a document DB

The datastore is SQLite, and it stays SQLite. "JSON database vs SQLite" is a
false choice: SQLite *is* a first-class JSON store (JSON1). When we add **custom
entities** and **custom fields**, the shape is a `data JSON` column on a generic
`entities(kind, data, origin_entry_id, anchor_text, …)` table that the
journal-anchor flow emits into — exactly like `tasks`/`decisions`/`events` do
today. Custom fields you actually filter or sort on get a **generated column**
(`json_extract(data,'$.field')`) plus an index; everything else just lives in
the JSON.

Why not a separate document DB (Mongo/LowDB): it would cost us FTS5, atomic
transactions, the embeddings table, and the single-file zero-infra story — and
buy nothing at this scale. The corpus is thousands of rows; the brute-force
cosine scan for semantic search is sub-10ms. For a local single-node app,
in-process SQLite beats a networked document store on every axis that matters.

## Admin + knowledge graph

The **admin** tab (and the `/api/worker`, `/api/embeddings`, `/api/outbox`
routes) surfaces the worker heartbeat + last cycle, embedding coverage
(per-kind / per-model, with a pending count), and the outbound job queue. The
**graph** tab renders the `links` knowledge graph (`/api/graph`) as a
force-directed node-link diagram — every journal entry and the tasks/decisions/
events anchored from it, plus `supersedes` edges — click a node to focus its
neighborhood.

## Cloud dev env (GitHub / Claude Code on the web)

`/.claude/settings.json` registers a **SessionStart** hook that runs
[`dev-setup.sh`](./dev-setup.sh): it installs deps and seeds the DB so a
fresh web session comes up ready to `pnpm dev`. Re-running is safe.

## Config

| Env            | Default                 | Used by    |
| -------------- | ----------------------- | ---------- |
| `PORT`         | `8787`                  | api        |
| `HIVE_DB`      | `data/hive.db`          | api        |
| `HIVE_API_URL` | `http://localhost:8787` | cli, web proxy |
| `HIVE_ACTOR`   | `cli`                   | cli        |
| `HIVE_EMBED`   | `transformers`          | api, worker — set `hash` to skip the model download (CI/offline) |
| `HIVE_EMBED_MODEL` | `Xenova/bge-small-en-v1.5` | api, worker (transformers mode) |
| `HIVE_RERANK_MODEL` | `Xenova/bge-reranker-base` | api, worker (transformers mode) |

## Branching

[BR canon](https://kb.beesroadhouse.com/books/developer-operations-devops/page/branching-strategy):

- `development` … default branch.
- `release` … stable/production branch.
- `feature/{slug}`, `bug/{slug}`, `improvement/{slug}`, `refactor/{slug}` from
  development. No `main` / `master`.
