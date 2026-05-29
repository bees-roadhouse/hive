# AGENTS.md

This file gives Codex project-level instructions for the `bees-roadhouse/hive`
repository. It applies to the whole repo unless a deeper `AGENTS.md` overrides
it.

## Project Shape

`hive` is a Rust workspace that replaces the legacy python `~/.hive/` toolchain.
It owns the shared Bee's Roadhouse / DTC AI state: journal entries, tasks, notes,
wire events, links, embeddings, and the web/API surfaces around them.

Primary crates:

- `crates/hive-db` ... Postgres/sqlx data layer, migrations, shared DB helpers.
- `crates/hive-core` ... shared DTO/domain types for API clients.
- `crates/hive-api` ... axum HTTP API over the canonical Postgres store.
- `crates/hive-cli` ... `hive` CLI client, using the API resolver.
- `crates/hive-ui` ... Leptos SSR + WASM hydration UI.
- `crates/hive-embed` ... embedding/reranking support.
- `crates/hive-md` ... markdown / Obsidian-style task parsing.

`crates/hive-migrate` is intentionally excluded from the workspace. It is a
legacy one-shot sqlite-to-postgres migration helper, not part of normal CI.

## Read First

Before changing behavior, read the nearby code and the relevant doc:

- `README.md` for repo status and branching.
- `DESIGN.md` for architecture and migration rationale.
- `docs/conventions.md` for link/task/journal vocabulary.
- `docs/portainer-deploy.md` for stack deployment, client URL resolution, and
  backup rules.
- `crates/hive-ui/README.md` before touching Leptos UI or hydration behavior.

## Safety Rules

- Check `git status --short` before editing. There is often active work in this
  repo; never overwrite or revert changes you did not make.
- Do not commit secrets, real passwords, tokens, private keys, or real PII.
- Do not run destructive DB operations, force-push, deploy to production, change
  Portainer stack settings, or modify live DNS/infra without explicit approval.
- Treat the Portainer/Postgres hive stack as canonical production state. Local
  sqlite files are legacy snapshots unless a task explicitly says otherwise.
- Do not edit generated build output under `target/`, crate-local `target/`, or
  copied lockfiles in excluded crates unless the task is specifically about
  cleanup.

## Rust Toolchain

The workspace pins Rust in `rust-toolchain.toml`.

- Current channel: `1.95.0`
- Required components: `rustfmt`, `clippy`

If bumping Rust, update `rust-toolchain.toml` and every workflow that installs
`dtolnay/rust-toolchain` in the same change.

## Cargo Target Directory

This checkout lives on SeaDrive. Cargo fingerprint writes under the repo-local
`target/` can fail intermittently there, so set `CARGO_TARGET_DIR` to a local
disk path before any cargo build, test, clippy, or leptos command.

PowerShell:

```powershell
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo-target\hive"
```

Use a unique suffix for parallel worktrees, for example
`$env:USERPROFILE\.cargo-target\hive-graph-work`.

## Standard Checks

Match CI before pushing or opening a PR:

```powershell
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo-target\hive"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

After code-changing clippy fixes, run `cargo fmt --all -- --check` again.

Targeted checks are fine while iterating, but do not treat them as the final gate
for workspace-wide changes.

## Local Stack

Local Postgres + hive-api:

```powershell
podman compose -f docker/docker-compose.local.yml up -d
podman compose -f docker/docker-compose.local.yml logs -f hive-api
podman compose -f docker/docker-compose.local.yml down
```

The local stack exposes:

- Postgres on `127.0.0.1:5432`
- hive-api on `127.0.0.1:7878`
- health check at `http://127.0.0.1:7878/healthz`

Default local DB URL:

```text
postgres://hive:hive@localhost:5432/hive
```

The production compose file is `docker/docker-compose.yml`; do not mix it with
the local dev compose file.

## Client URL Resolution

The Rust CLI and UI client resolver uses:

1. `HIVE_API_URL`
2. DNS-search match against `HIVE_DHCP_NAME_SEARCH_DOMAIN_NETWORK_AWARENESS`
3. `HIVE_PRIVATE_URL`
4. `HIVE_PUBLIC_URL`
5. localhost fallback

Set `HIVE_API_URL` only when intentionally pinning a process to one API base.

## UI / Leptos

`hive-ui` is dual-target:

- SSR/axum binary with `--features ssr`
- Browser WASM hydration with `--features hydrate --target wasm32-unknown-unknown`

Before UI work:

```powershell
rustup target add wasm32-unknown-unknown
```

Useful commands:

```powershell
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo-target\hive-ui"
cargo leptos serve
cargo leptos build --release
cargo build -p hive-ui --features ssr --no-default-features
cargo build -p hive-ui --features hydrate --no-default-features --target wasm32-unknown-unknown
cargo clippy -p hive-ui --features ssr --no-default-features -- -D warnings
cargo clippy -p hive-ui --features hydrate --no-default-features --target wasm32-unknown-unknown --lib -- -D warnings
```

Keep server-only code behind `ssr` cfgs/features and hydration-safe code shared.

## Data Model Conventions

Follow `docs/conventions.md` for journal/task/link behavior.

- Use the canonical `link_type` vocabulary unless a task explicitly extends it.
- `spawned_in` is the creation event; `inline_in` is the durable embedded-task
  binding.
- Old journal entries are immutable history. Task status changes should render
  through state, not by rewriting old entry bodies.
- Task project membership lives in `tasks.project`; non-project parentage belongs
  in `links` with `child_of`.

## Database Changes

- Put schema changes in `crates/hive-db/migrations`.
- Keep migrations compatible with Postgres and the pgvector-backed stack.
- Prefer sqlx query patterns already used in the crate.
- Include tests or a clear manual verification path for migrations and data
  model behavior.

## Branching And Publishing

BR branching canon for this repo:

- `development` is the default integration branch.
- `release` is stable/production.
- Work branches should be `feature/{slug}`, `bug/{slug}`,
  `improvement/{slug}`, or `refactor/{slug}` from `development`.
- Do not introduce `main` or `master`.

Publishing workflows build and push GHCR images for `hive-api` and `hive-ui`.
Do not change image tags, workflow triggers, or production compose image refs
casually; verify the deployment doc and ask before making visible infra changes.

