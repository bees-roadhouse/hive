# hive (node) — a fun Node.js + Solid.js rewrite

A playful reimplementation of [`hive`](../README.md) — the shared state every
Bee's Roadhouse / DTC AI config reads and writes (tasks, journal, notes,
**decisions**, knowledge-graph links, and a wire event log).

This is the *fun, not-that-serious* sibling of the production Rust workspace. It
trades Postgres + pgvector and the 9-phase auth stack for **zero-infra SQLite**
so it spins up instantly in a fresh container.

> Not a drop-in replacement for the Rust `hive`. Same domain, much smaller scope:
> no semantic/vector search, no auth, single-file DB.

## Stack

| Package        | What it is                                                    |
| -------------- | ------------------------------------------------------------- |
| `@hive/shared` | TypeScript domain types shared by every package              |
| `@hive/api`    | [Hono](https://hono.dev) HTTP API over SQLite (better-sqlite3) + FTS5 search |
| `@hive/web`    | [Solid.js](https://solidjs.com) + Vite single-page UI        |
| `@hive/cli`    | `hive` CLI — a stateless HTTP client over the API            |

Run via Node's native TypeScript stripping (`--experimental-strip-types`) — no
build step needed for dev.

## Quick start

```bash
cd node
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

## Cloud dev env (GitHub / Claude Code on the web)

`/.claude/settings.json` registers a **SessionStart** hook that runs
[`node/dev-setup.sh`](./dev-setup.sh): it installs deps and seeds the DB so a
fresh web session comes up ready to `pnpm dev`. Re-running is safe.

## Config

| Env            | Default                 | Used by    |
| -------------- | ----------------------- | ---------- |
| `PORT`         | `8787`                  | api        |
| `HIVE_DB`      | `node/data/hive.db`     | api        |
| `HIVE_API_URL` | `http://localhost:8787` | cli, web proxy |
| `HIVE_ACTOR`   | `cli`                   | cli        |
