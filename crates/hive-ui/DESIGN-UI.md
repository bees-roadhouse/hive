# hive-ui design

## Goal

Replace the python+svelte hive-ui with a pure-Rust app rendered by leptos.
Single binary. Talks to `~/.hive/hive.db` directly via the `hive-db` crate.
No separate API process. No node toolchain in the build.

## Staged plan

| stage | scope                                                   | hydration | tooling      | status    |
|-------|---------------------------------------------------------|-----------|--------------|-----------|
| v1    | SSR pages: `/`, `/journal`, `/journal/:id`, `/tasks`, `/notes`, `/wire` | none      | cargo only   | this PR   |
| v1.5  | cargo-leptos pipeline; component refactor               | none      | cargo-leptos | follow-up |
| v2    | islands of interactivity (filters, search, sort)        | partial   | cargo-leptos | follow-up |
| v3    | server functions + mutations (add task, ack wire, …)    | full      | cargo-leptos | later     |

v1 is **render-only**. axum handlers pull from hive-db, hand the typed rows to
view functions, view functions render via `leptos::ssr::render_to_string`. No
server functions yet — adding them requires the cargo-leptos pipeline to wire
WASM bundling, which is a separate build-system change. Doing both at once
risks the visible-UI deadline. Ship the SSR shell, then layer in hydration.

## Why skip hive-api

The proper REST API lives in `crates/hive-api` (sibling sister sibling's
work). hive-ui v1 deliberately short-circuits that layer and calls hive-db
directly: leptos server functions in v3 will follow the same pattern (DB-in-
process), so a separate REST hop adds latency and a process boundary without
buying anything for the SSR + hydration model. The REST API stays useful for
external consumers (CLI from another machine, scripts, future integrations).

## Why no WASM hydration in v1

- cargo-leptos install + first build of WASM bundle adds 5–15 minutes on
  Windows MSVC. Visible-UI deadline trumps client interactivity for v1.
- The journal/tasks/notes/wire pages are read-only displays; SSR alone gives
  Nate everything he needs to see state.
- Hydration is a contained refactor when added: same view! macros, just
  served through cargo-leptos' route table instead of plain axum handlers.

## Routing

```
GET /                  → home (last 30 journal entries)
GET /journal           → last 100 journal entries
GET /journal/:id       → entry detail with full body
GET /tasks             → active tasks (open + in_progress)
GET /notes             → notes list
GET /wire              → last 50 wire events
```

Port: `8091` (override via `HIVE_UI_PORT`). Skip `8090` since python hive-ui
may still be running.

## DB access

- Read-only intent. No mutations from the UI in v1.
- `r2d2` connection pool, max_size = 4. SQLite handles concurrent reads
  cleanly; v1 never blocks long enough to need more.
- All hive-db calls happen inside `tokio::task::spawn_blocking` — rusqlite
  is sync, the runtime stays responsive.

## Schema dependency

Frozen against post-task-8 hive.db. `projects` table has INTEGER PK + UNIQUE
name. hive-ui touches none of this directly; everything flows through
`hive_db::queries::*`.

## Branch / merge plan

This PR branches from `feature/hive-db-scaffold` (which adds the workspace
+ hive-db crate). Target review against that branch so the diff stays clean.
When `feature/hive-db-scaffold` lands on `development`, rebase this branch on
top.
