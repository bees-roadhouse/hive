# hive

The shared state every Bee's Roadhouse / DTC AI config (Pia, Apis, Cera, and
peers) reads and writes: a **journal-first**, **MCP-first** brain for tasks,
decisions, events, notes, and the knowledge-graph links between them.

A Node.js + Solid.js app, zero-infra (single-file SQLite). It lives in
[`node/`](./node).

```bash
cd node
pnpm install
pnpm seed        # create + seed data/hive.db
pnpm dev         # api :8787 (+ MCP at /mcp), web :5173
pnpm worker:once # one worker cycle (feeds → wire, embeddings, maintenance)
```

See [`node/README.md`](./node/README.md) for the full tour and
[`docs/conventions.md`](./docs/conventions.md) for the domain vocabulary.

## Shape

- **Journal-first** — the journal is the single write-only input. Tasks,
  decisions, and events *emerge* from prose by anchoring the exact text span
  they came from. `@mentions` drive a per-actor inbox (humans **and** AIs).
- **MCP-first** — a Streamable-HTTP MCP server at `POST /mcp` is the primary
  surface; the REST API and Solid UI mirror it.
- **Worker** — a long-running process polls RSS sources into the wire, drains an
  outbound queue, refreshes embeddings for semantic search, and keeps the DB
  tidy.

## Packages (`node/`, a pnpm workspace)

| Package | What |
|---|---|
| `@hive/shared` | shared TypeScript domain types |
| `@hive/api` | Hono HTTP API over SQLite + FTS5, and the MCP server |
| `@hive/web` | Solid.js + Vite single-page UI |
| `@hive/cli` | `hive` CLI over the API |
| `@hive/worker` | feeds → wire, outbound queue, embeddings, DB maintenance |

## History

This replaced a Rust workspace (which itself replaced a Python toolchain at
`~/.hive/`). The Rust crates were removed once this Node rewrite landed; see the
git history before this point if you need them.

## Branching

[BR canon](https://kb.beesroadhouse.com/books/developer-operations-devops/page/branching-strategy):

- `development` … default branch.
- `release` … stable/production branch.
- `feature/{slug}`, `bug/{slug}`, `improvement/{slug}`, `refactor/{slug}` from
  development. No `main` / `master`.
