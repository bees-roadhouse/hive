# hive

Rust workspace replacing the python toolchain at `~/.hive/`. Houses the shared
state used by every Bee's Roadhouse / DTC AI config (Pia, Apis, Cera, future
peers): tasks, journal, notes, wire events, knowledge graph.

## Crates

- `hive-db` ... shared sqlite (rusqlite) layer + types.
- `hive-cli` ... `hive` binary, full CLI parity to the legacy
  `python ~/.hive/hive.py`.
- `hive-api` ... axum HTTP service exposing the same operations.
- `hive-ui` ... leptos SSR + WASM hydration UI; replaces the python+svelte
  `hive-ui`.
- `hive-embed` ... embedder client (model strategy: see `DESIGN.md`).

See [DESIGN.md](./DESIGN.md) for the rationale, layout, and migration plan.

## Status

In active build. Cutover from python is a coordinated window orchestrated
by Cera once parity tests pass. Build phase does not touch `~/.hive/`.

Driving hive task #53.

## Branching

[BR canon](https://kb.beesroadhouse.com/books/developer-operations-devops/page/branching-strategy):

- `development` ... default branch.
- `release` ... stable/production branch.
- `feature/{slug}`, `bug/{slug}`, `improvement/{slug}`, `refactor/{slug}` from
  development.
- No `main` / `master`.
