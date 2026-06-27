# AGENTS.md

## Scope

These instructions apply to the whole `bees-roadhouse/hive` repository.

GitHub `development` is canonical. This repo is in a Rust/Postgres transition:

- Rust workspace: `api`, `worker`, `shared`, and `embed`.
- Web UI: Solid.js/Vite under `packages/web`.
- Legacy/parity Node packages still exist under `packages/*` and are still built
  by CI, but do not treat the old Node/SQLite README framing as the source of
  truth for new backend work.
- Postgres is the active datastore for the Rust store layer. SQLite remains for
  legacy import compatibility.

When docs disagree with code, workflows, or compose files, trust code and CI,
then update the stale doc in the same change. `README.md` and parts of
`RUST_REWRITE.md` may lag the current Rust/Postgres reality.

## Architecture

- `api/`: Axum API server, auth, OAuth/OIDC, MCP, SSE, SPA serving, migrations,
  and store modules.
- `worker/`: Rust worker loop for sources, outbox, embeddings, and maintenance.
- `shared/`: Rust shared domain types.
- `embed/`: embedding seam, ONNX/BGE implementation, and hash fallback.
- `packages/web/`: Solid.js UI.
- `packages/shared/`: TypeScript types used by the UI/legacy Node packages.
- `packages/api`, `packages/worker`, `packages/cli`: legacy/parity Node
  surfaces that still participate in `pnpm build`.

## Core Invariants

- Journal-first model: journal entries are the source; tasks, decisions, events,
  and links derive from anchored spans or explicit structured operations.
- Old journal entries are history. Do not rewrite old bodies to reflect status
  changes; render from canonical state instead.
- Rust API and worker share Postgres through `DATABASE_URL`.
- Identity comes from a session cookie or a Bearer API token. Do not reintroduce
  trust in `x-hive-actor`.
- Non-public API routes require completed onboarding and authentication unless
  the route is intentionally public or self-authenticating.
- `/mcp` and `/api/stream` have special auth behavior. Preserve their client
  compatibility and 401 shapes.
- OAuth/OIDC work must preserve PKCE S256, single-use short-lived auth codes,
  replay revocation, redirect URI validation, consent CSRF, and token TTL caps.
- Keep Rust and TypeScript shared contracts aligned when changing API shapes.
- Use `HIVE_EMBED=hash` for CI, local smoke tests, and offline checks.

## Branching

- `development` is the default branch.
- `release` is the stable/production branch.
- Work branches should start from `development` and use `feature/{slug}`,
  `bug/{slug}`, `improvement/{slug}`, or `refactor/{slug}`.
- Do not introduce `main` or `master` as app branches.

## Setup

Install JS dependencies:

```bash
pnpm install
```

Use the pinned Rust toolchain in `rust-toolchain.toml`. On Windows, prefer a
target dir outside the repo for Rust builds:

```powershell
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo-target\hive"
```

Local Rust API expects Postgres unless `DATABASE_URL` points elsewhere:

```powershell
$env:DATABASE_URL = "postgres://hive:hive@localhost:5432/hive"
```

The Rust compose path starts Postgres, API, and worker:

```bash
docker compose -f docker/docker-compose.rust.yml up --build
```

For local UI work, build or run the Solid app from the pnpm workspace:

```bash
pnpm dev:web
pnpm --filter @hive/web build
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

Node/TypeScript gate:

```bash
pnpm build
HIVE_EMBED=hash pnpm seed
```

PowerShell seed equivalent:

```powershell
$env:HIVE_EMBED = "hash"
pnpm seed
Remove-Item Env:\HIVE_EMBED
```

There is no dedicated lint script today. Do not claim one ran unless you add it
or verify it exists.

## CI And Release

`.github/workflows/ci.yml` has two jobs:

- `check`: installs pnpm deps, runs `pnpm build`, then `HIVE_EMBED=hash pnpm seed`.
- `rust`: starts Postgres 17, runs `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo build --workspace --all-targets`, and `HIVE_EMBED=hash cargo test --workspace`.

`.github/workflows/release.yml` still builds Node images for `hive-api`,
`hive-web`, and `hive-worker`, and also builds the additive all-in-one
`hive-rust` image. Treat `hive-rust` as the forward backend/runtime path unless
Nate says otherwise.

## Rust Code Style

- Keep store logic in `api/src/store/*` and route wiring in `api/src/routes/*`.
- Keep middleware behavior centralized in `api/src/middleware.rs`.
- Keep OAuth/OIDC behavior in `api/src/routes/oauth.rs` and related store/auth
  helpers.
- Use sqlx with explicit queries that match the existing style.
- Migrations in `api/src/db.rs` must be idempotent and safe for API/worker race
  at startup.
- Preserve Node wire/API compatibility unless intentionally changing the public
  contract.
- Add comments only for non-obvious reasons, invariants, or security-sensitive
  behavior.

## UI Style

- `packages/web/src/App.tsx` owns routing and the authenticated shell.
- `packages/web/src/api.ts` owns browser API calls and session-cookie behavior.
- `packages/web/src/OAuthConsent.tsx`, `Account.tsx`, `Login.tsx`, and
  `Onboarding.tsx` are security-sensitive UX surfaces.
- Keep the UI dense, operational, and route-driven. This is a working hive
  console, not a marketing site.
- Verify UI changes in the browser when changing navigation, auth, consent,
  forms, or stateful workflows.

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

- `README.md` still describes the old Node/Solid SQLite system in places.
- `RUST_REWRITE.md` contains useful Rust architecture notes but may still claim
  SQLite compatibility where current code uses Postgres.
- `docker-compose.hybrid.yml` appears older than `docker/docker-compose.rust.yml`
  and may not reflect the current Postgres path.

Fix these docs when touching the related area.
