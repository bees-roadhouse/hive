# AGENTS.md

## Scope

These instructions apply to the whole `bees-roadhouse/hive` repository.

GitHub `main` is canonical (the old `development`/`release` pair collapsed into
it on 2026-07-05). This repo is mid-pivot to a personal P2P desktop app
(docs/DIRECTION.md D16+; teardown/rebuild sequence in docs/PLAN.md):

- Rust workspace: `shared`, `embed`, `core`, `api`, and `jmap-sync`. There is
  no Node/pnpm workspace anymore — the Solid SPA, the legacy Node packages,
  and the worker/mail daemons were deleted in Phase 1 teardown. Do not treat
  the old Node/SQLite README framing as the source of truth for new work.
- Postgres is the active datastore for the Rust store layer (until the Phase 1
  SQLite cutover). SQLite remains for legacy import compatibility.

When docs disagree with code, workflows, or compose files, trust code and CI,
then update the stale doc in the same change. `README.md` and parts of
`RUST_REWRITE.md` may lag the current Rust/Postgres reality.

## Architecture

- `api/`: Axum API server, auth, OAuth/OIDC, MCP, SSE, and route wiring.
- `core/`: the store layer (Postgres store, schema/migrations, pgq helpers,
  embedding backfill) — hive-core, which the api depends on.
- `shared/`: Rust shared domain types.
- `embed/`: embedding seam, ONNX/BGE implementation, and hash fallback.
- `jmap-sync/`: JMAP mailbox sync library (kept through the pause; its offline
  quote-corpus test keeps the mail parser alive until mail returns in Phase 3).

## Core Invariants

- Journal-first model: journal entries are the source; tasks, decisions, events,
  and links derive from anchored spans or explicit structured operations.
- Old journal entries are history. Do not rewrite old bodies to reflect status
  changes; render from canonical state instead.
- The Rust API reaches Postgres through `DATABASE_URL`.
- Identity comes from a session cookie or a Bearer API token. Do not reintroduce
  trust in `x-hive-actor`.
- Non-public API routes require completed onboarding and authentication unless
  the route is intentionally public or self-authenticating.
- `/mcp` and `/api/stream` have special auth behavior. Preserve their client
  compatibility and 401 shapes.
- OAuth/OIDC work must preserve PKCE S256, single-use short-lived auth codes,
  replay revocation, redirect URI validation, consent CSRF, and token TTL caps.
- Use `HIVE_EMBED=hash` for CI, local smoke tests, and offline checks.

## Branching

- `main` is the only long-lived branch; it must stay releasable.
- Work branches start from `main` and use `feature/{slug}`, `bug/{slug}`,
  `improvement/{slug}`, or `refactor/{slug}`, merging back via PR.
- Releases are tag-driven: bump versions in a release PR, merge, then push
  `v{version}` on the merge commit. Never rebuild images at release time —
  the workflows retag the PR-built `sha-*` manifests.

## Setup

Use the pinned Rust toolchain in `rust-toolchain.toml`. On Windows, prefer a
target dir outside the repo for Rust builds:

```powershell
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo-target\hive"
```

Local Rust API expects Postgres unless `DATABASE_URL` points elsewhere:

```powershell
$env:DATABASE_URL = "postgres://hive:hive@localhost:5432/hive"
```

The Rust compose path starts Postgres and the API:

```bash
docker compose -f docker/docker-compose.rust.yml up --build
```

## Verification

Before handing off substantial changes, match the relevant CI gates.

Rust gate:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
HIVE_EMBED=hash cargo test --workspace
```

PowerShell hash-test equivalent:

```powershell
$env:HIVE_EMBED = "hash"
cargo test --workspace
Remove-Item Env:\HIVE_EMBED
```

There is no dedicated lint script today. Do not claim one ran unless you add it
or verify it exists.

## CI And Release

`.github/workflows/ci.yml` has one job, triggered on PRs to `main`:

- `rust`: starts Postgres 17, runs `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo build --workspace --all-targets`, and `HIVE_EMBED=hash cargo test --workspace`.

`.github/workflows/release.yml` builds `hive-rust` (the deployed image) on PRs
with immutable `sha-*` tags; merges retag to `dev` plus a merge-sha alias;
pushing a `v{version}` tag retags that merge's image as `{version}` + `latest`
and cuts the GitHub Release (attaching the `hive.mcpb` Claude Desktop bundle).
The version of record is the `[workspace.package]` version in the root
`Cargo.toml`.

## Rust Code Style

- Keep store logic in `core/src/store/*` (the hive-core crate; api re-exports it)
  and route wiring in `api/src/routes/*`.
- Keep middleware behavior centralized in `api/src/middleware.rs`.
- Keep OAuth/OIDC behavior in `api/src/routes/oauth.rs` and related store/auth
  helpers.
- Use sqlx with explicit queries that match the existing style.
- Migrations in `core/src/db.rs` must be idempotent and safe when concurrent
  processes race at startup. Schema management is hybrid: the inline DDL constants in
  `core/src/db.rs` are the base schema, and `core/migrations/` holds sqlx
  migrations reserved for reshapes the inline style cannot express. `migrate()`
  runs the sqlx migrator first, so every migration must tolerate both a fresh
  database (inline DDL has not run yet — guard with `IF EXISTS` or
  `information_schema` probes qualified by `table_schema = current_schema()`)
  and an existing database on the old shape. The PR that adds a reshape
  migration also updates the inline constants to the final shape. Never edit an
  applied migration (sqlx checksums them); add a new file instead.
- Preserve the established wire/API shapes (inherited from the Node stack)
  unless intentionally changing the public contract.
- Add comments only for non-obvious reasons, invariants, or security-sensitive
  behavior.

## Security Review Hotspots

Prioritize these when reviewing before real use:

- OIDC login start/callback, nonce/state cookies, issuer resolution, allowed
  domains, and token verification.
- OAuth 2.1 authorization server metadata, protected-resource metadata, dynamic
  client registration, consent, redirect handling, PKCE, auth-code replay, and
  token TTL boundaries.
- Session cookie flags, CSRF on consent, CORS reflection, forwarded host/proto
  handling, and admin gates.
- Bearer token namespace behavior: the token actor and the granting human's
  namespace/role must not get mixed up.
- Visibility scoping for journal/search/recall/shares across user namespaces.
- MCP and SSE auth behavior, especially browser preflight and unauthenticated
  error responses.

## Data And Generated Files

- Do not commit `target/`, `node_modules/`, package `dist/`, `.tsbuildinfo`, or
  generated database/model-cache files.
- `.claude/worktrees/` and `/node/` are historical/local state. Do not treat
  them as source.
- Do not add secrets, real tokens, credentials, or personal data.
- Use reserved fictional values in tests and docs.

## Known Documentation Drift

- `README.md` still describes the retired hosted system in places (full
  rewrite lands at the end of Phase 1).
- `RUST_REWRITE.md` contains useful Rust architecture notes but predates the
  P2P pivot and the Phase 1 teardown.

Fix these docs when touching the related area.
