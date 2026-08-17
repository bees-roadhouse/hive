# AGENTS.md

## Scope

These instructions apply to the whole `bees-roadhouse/hive` repository.

GitHub `main` is canonical (the old `development`/`release` pair collapsed into
it on 2026-07-05). Hive is a self-hostable web application: an axum API over
PostgreSQL with row-level security, serving a SolidJS SPA (design note in
docs/WEB-APP.md; test tiers and conventions in docs/TESTING-STRATEGY.md).
docs/DIRECTION.md, docs/PLAN.md, and docs/PLAN-v2.1.md are history. They
describe the local-first desktop architecture WEB-APP.md replaced and carry
superseded banners saying so ... read them for why a decision was made, never
for what the tree looks like.

- Rust workspace: `shared`, `embed`, `core`, `api`, `jmap-sync`, `relay`.
  `hive-api` is the shipping binary; `relay/` also builds `hive-relay` and
  `hive-relay-agent`. Beside it is a pnpm workspace, `packages/shared` and
  `packages/web` (`@hive/shared`, `@hive/web`), and the API serves the built
  SPA from `HIVE_WEB_DIST` (`api/src/routes/spa.rs`).
- The datastore is PostgreSQL with pgvector. Access control is row-level
  security keyed on an acting org, not application checks (`core/src/acting.rs`
  for the scope, the policy DDL in `core/src/db.rs`). sqlx keeps its `sqlite`
  feature for exactly one reason: reading an uploaded legacy `hive.db` during
  import (`api/src/legacy_import.rs`).

When docs disagree with code, workflows, or compose files, trust code and CI,
then update the stale doc in the same change. `README.md` and `RUST_REWRITE.md`
lag the current architecture (see Known Documentation Drift).

## Architecture

- `core/`: hive-core ... the store layer plus the database plumbing under it.
  `db.rs` owns the schema, the RLS policies, role provisioning, and the two
  pools: `open_admin` (table owner, DDL at boot, never handed to
  request-serving code) and `open_app` (the unprivileged `hive_app` role the
  API serves as). `acting.rs` is the acting-org scope, a tokio task-local with
  no setter, stamped onto every connection at checkout. `pgq.rs` rewrites the
  codebase's SQLite-era `?` placeholders to `$N` and delegates to sqlx, so call
  sites read like `sqlx::` minus the DB type param. `store/` is one
  `impl Store` block per module (`journal.rs`, `tasks.rs`, `mail.rs`,
  `orgs.rs`, and the rest); the `Store` struct is the pool plus an in-process
  SSE bus, so `emit()` persists the wire event and fans it to listeners, with
  the org riding along because a broadcast channel has no policies.
  `artifact_storage.rs` holds artifact BYTES at
  `<data_root>/artifacts/<org_id>/<hh>/<sha256>`: content addressed so the
  object-storage swap is a driver change, org IN the address so a delete can
  answer "is anything still referencing these bytes?" without escaping the
  policy that is supposed to be unbypassable.
- `api/`: hive-api ... the axum binary, and the only process that speaks to
  the database. `routes/` composes REST, the SSE stream, the OAuth AS, and
  `spa.rs` (serves `HIVE_WEB_DIST`). `mcp.rs` is the MCP tool layer, owning
  tools/list and tools/call dispatch as a parity port of the Node SDK toolset;
  `routes/mcp.rs` is its stateless HTTP transport at `POST /mcp`.
  `middleware.rs` resolves a credential to an `AuthCtx`, and the CREDENTIAL
  pins the org ... nothing reads an org from a header or a body.
  `legacy_import.rs` reads an uploaded legacy SQLite `hive.db` and writes it
  through the normal insert path. The store layer is re-exported here
  (`pub use hive_core::{db, pgq, store}`), so a `crate::store::` path inside
  `api/` resolves into hive-core.
- `relay/`: hive-relay ... reaching a self-hosted hive without a forwarded
  port or a certificate to wrangle. The daemon routes on the SNI in the
  ClientHello (in the clear before the handshake) and splices raw sockets
  without decrypting; the agent dials OUTBOUND from the house and terminates
  TLS in front of `hive-api`. Two binaries, `hive-relay` and
  `hive-relay-agent`. Design note: docs/RELAY.md.
- `shared/`: Rust shared domain types.
- `embed/`: embedding seam, ONNX/BGE implementation, and hash fallback.
- `jmap-sync/`: JMAP mailbox sync library. No Hive types and no database
  dependency ... the consumer implements `CursorStore` and `MailSink`. The
  `jmap-client` dependency is contained in `client.rs`, so replacing it with
  reqwest+serde stays a one-module rewrite. Mail routes and the store's mail
  archive live behind `HIVE_MAIL_ENABLED`.
- `packages/web`: the SolidJS SPA (vite, TipTap editor), served in production
  by `hive-api` from `HIVE_WEB_DIST`. `packages/shared` holds the TypeScript
  domain types it shares with the API's wire shapes.

## Core Invariants

- Journal-first model: journal entries are the source; tasks, decisions, events,
  and links derive from anchored spans or explicit structured operations.
- Old journal entries are history. Do not rewrite old bodies to reflect status
  changes; render from canonical state instead.
- Writes go through the store layer: the module mints ids (`new_id`) and
  timestamps (`now_iso` ... millisecond ISO-8601 with a trailing `Z`, the
  shape the Node API wrote, so rows still sort lexicographically together),
  and calls `emit()` for anything other clients must see. `emit()` persists
  the wire event AND fans it to the SSE bus, so skipping it leaves connected
  clients stale. Emergence runs on the journal write path
  (`store/journal.rs`): anchors and bracket tokens are parsed at append time
  and the tasks, decisions, and events they name are created there.
- Multi-org, and Postgres enforces it. A session or a token pins exactly ONE
  acting org; `acting::scope` wraps a future and there is no setter, so
  nothing switches orgs mid-request. Absence is deny-all, not bypass: with no
  scope the GUC stamps empty, the predicate goes NULL, reads return nothing
  and writes trip `org_id NOT NULL`. Work moved off the request task with
  `tokio::spawn` loses the scope and hits that deny-all, deliberately. Do NOT
  compose org or namespace filters in the store layer ... the `user_scope`
  and `owner` COLUMNS are load-bearing, but they are policy predicates now
  (`core/src/db.rs`), and a hand-composed filter is exactly the ACL-ordering
  defect RLS was adopted to end. `api/tests/org_isolation.rs` is that model in
  test form: if it fails, nothing else in the tree matters.
- Schema is hybrid. sqlx migrations in `core/migrations/` run FIRST, then the
  inline idempotent DDL in `core/src/db.rs` (`CREATE TABLE IF NOT EXISTS`,
  `ADD COLUMN IF NOT EXISTS`), then org scoping. A new content table has to be
  added to the org-scoping list in `db.rs`: nothing else in the tree filters
  by org, so an omission is a hole. A reshape migration needs a test over BOTH
  bases the convention promises to tolerate ... a fresh database where the
  inline DDL builds the final shape and the migration no-ops, and an old-shape
  database the migration has to reshape (`api/tests/migrations.rs`). The
  greenfield case alone is not coverage, because greenfield is the case that
  cannot break.
- MCP is an HTTP surface, not a stdio one: `POST /mcp`, stateless, plain JSON
  responses. Keep tool definitions and dispatch in `api/src/mcp.rs` and
  transport concerns in `api/src/routes/mcp.rs`. The protocol constants, error
  codes, and result shapes there mirror the Node SDK on purpose, so changing
  one is a wire-contract change, not a cleanup.
- Every test that needs a database gets it from `hive_core::db`, never by
  opening a pool of its own: `test_pool()` for the general case,
  `test_pool_strict()` for no fallback scope (exactly how the binary behaves),
  `test_pool_single_conn()` when the claim is about one physical connection
  being reused, `test_pool_unmigrated()` for laying down an old-shape table by
  hand, `test_admin_pool()` for setup that has to happen from outside any org.
  No test body opens a pool any other way. `TestDb` is `#[must_use]` and owns
  its schema's lifetime: bind it for the whole test, and have any helper that
  builds a `Store` or a `Router` hand it back with them, or the schema drops
  out from under what the helper returned.
- Tests connect as the same unprivileged role the API serves as.
  `test_pool_with` calls `assert_rls_applies`, which refuses a SUPERUSER or
  BYPASSRLS pool outright, because either one reads every org's rows with no
  policy firing. Never work around it by widening the role.
- There is no env-gated tier and no test-support crate.
  `cargo test --workspace` runs exactly what CI runs: one command, no flags,
  no switches to remember. The inline `#[cfg(test)] mod tests` block at the
  foot of the source file is the dominant idiom; reach for a file under
  `tests/` when the test needs the composition (a router with its middleware,
  a migration over an old-shape database, two orgs sharing one schema). Tiers,
  conventions, and what is deliberately uncovered: docs/TESTING-STRATEGY.md.
- Anything that binds asks for `127.0.0.1:0` and reads back the assigned port
  (`relay/tests/tunnel.rs`). Never a fixed port: they collide under a parallel
  run and turn into flakes that only reproduce on a busy machine.
- No test run touches the network. `HIVE_EMBED=hash` in CI and for local runs
  keeps the embedder offline, since the default provider lazily pulls BGE
  models from the HF hub. The provider latches once per process, so a binary
  that needs a 384-dimension provider installs its own mock engine before the
  first embed call.

## Branching

- `main` is the only long-lived branch; it must stay releasable.
- Work branches start from `main` and use `feature/{slug}`, `bug/{slug}`,
  `improvement/{slug}`, or `refactor/{slug}`, merging back via PR.
- Releases are tag-driven: bump versions in a release PR, merge, then push
  `v{version}` on the merge commit. (Dormant: there is no release workflow
  right now, and nothing is being tagged.)

## Setup

Use the pinned Rust toolchain in `rust-toolchain.toml`. On Windows, prefer a
target dir outside the repo for Rust builds:

```powershell
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo-target\hive"
```

The suite needs a real PostgreSQL with pgvector. There is no in-memory or
SQLite mode: Postgres is the store, RLS is the access control, and a test
against anything else would prove nothing about either.

```bash
./dev-setup.sh          # pgvector/pgvector:pg17 on :5432, idempotent

DATABASE_URL=postgres://hive:hive@localhost:5432/hive \
HIVE_EMBED=hash \
HIVE_CRED_KEY=dev-credential-vault-key \
  cargo test --workspace
```

`DATABASE_URL` falls back to that same URL (`hive_core::db::database_url`), so
locally you can usually leave it unset. The role it names must be able to
`CREATE ROLE`: the suite provisions the unprivileged serving role the same way
the binary does. `HIVE_CRED_KEY` arms the credential vault, any string.
Per-test schemas drop with `TestDb`; a hard kill leaks one, and
`./dev-setup.sh --drop-test-schemas` sweeps the leftovers.

The SPA:

```bash
pnpm install
pnpm typecheck
pnpm build
```

Local run: `cargo run -p hive-api` (port 7878, MCP at `/mcp`), with the SPA
either on the vite dev server (`pnpm dev`) or built and pointed at by
`HIVE_WEB_DIST`. The compose path is `docker/docker-compose.rust.yml`
(hive-api plus a pgvector Postgres).

## Verification

Before handing off substantial changes, match the relevant CI gates.

Rust gate:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --all-targets
cargo build --release -p hive-api
HIVE_EMBED=hash DATABASE_URL=postgres://hive:hive@localhost:5432/hive \
  HIVE_CRED_KEY=dev-credential-vault-key cargo test --workspace
```

PowerShell test equivalent:

```powershell
$env:HIVE_EMBED = "hash"
$env:HIVE_CRED_KEY = "dev-credential-vault-key"
cargo test --workspace
Remove-Item Env:\HIVE_EMBED, Env:\HIVE_CRED_KEY
```

Web gate:

```bash
pnpm install --frozen-lockfile
pnpm typecheck
pnpm build
```

There is no dedicated lint script today. Do not claim one ran unless you add it
or verify it exists.

## CI And Release

`.github/workflows/ci.yml` is the only workflow, with two jobs, both
merge-gating, triggered on PRs to `main` and pushes to `main`:

- `rust`: toolchain 1.94.1 matching `rust-toolchain.toml`, with a
  `pgvector/pgvector:pg17` service attached. Runs `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo build --workspace --all-targets`, `cargo build --release -p hive-api`,
  then `cargo test --workspace` with `HIVE_EMBED=hash`, `DATABASE_URL`, and
  `HIVE_CRED_KEY` set. Postgres hangs off the job rather than one special test
  job because every crate with a meaningful test needs a database. The extra
  release build is there because the container ships the release binary and
  borrow-check can differ between opt levels.
- `web`: node 22, pnpm through corepack. `pnpm install --frozen-lockfile`,
  then `@hive/web` typecheck, then `@hive/web` build. Typecheck runs FIRST on
  purpose: vite transpiles with esbuild, which strips types without checking
  them, so a type error still bundles clean and only `tsc` will ever see it.

There is no release workflow, no nightly job, and no perf gate in CI. The
version of record is the `[workspace.package]` version in the root
`Cargo.toml`.

## Rust Code Style

- Keep store logic in `core/src/store/*`, one `impl Store` block per module,
  and keep transport out of it: no axum or HTTP types below `api/`. The MCP
  tool layer is `api/src/mcp.rs`.
- Use explicit SQL through `core/src/pgq.rs` (`pgq::query`, `query_as`,
  `query_scalar`), matching the surrounding style. Placeholders are written
  `?` and rewritten to `$N` at call time ... do not renumber them by hand and
  do not mix in bare `sqlx::query` for new code.
- The schema lives in `core/src/db.rs` (inline idempotent DDL plus the org
  scoping) with reshapes in `core/migrations/`. A change to either is a
  migration question, not a drive-by: see the hybrid rule under Core
  Invariants.
- Preserve the established wire/API shapes (inherited from the Node stack)
  unless intentionally changing the public contract.
- Add comments only for non-obvious reasons, invariants, or security-sensitive
  behavior.

## Security Review Hotspots

Prioritize these when reviewing before real use:

- Journal-first write path (`store/journal.rs`): bracket-token and anchor
  parsing runs over untrusted prose at append time.
- The credential vault (`store/cc_credentials.rs`): AES-256-GCM under
  `HIVE_CRED_KEY`; plaintext must never reach a log or a tool result.
- Embedding/search index maintenance: deletes must scrub search/embeddings
  rows (actor cascade, mail redaction) so nothing orphaned resurfaces in
  retrieval.
- MCP tool layer (`api/src/mcp.rs`): every tool result is content the calling
  agent will read ... treat stored data as untrusted input to it.

## Data And Generated Files

- Do not commit `target/`, `node_modules/`, package `dist/`, `.tsbuildinfo`, or
  generated database/model-cache files.
- `.claude/worktrees/` is local state. Do not treat it as source.
- Do not add secrets, real tokens, credentials, or personal data.
- Use reserved fictional values in tests and docs.

## Known Documentation Drift

- `README.md` still describes the local-first desktop app: op-log storage,
  crypto-shred, a Dioxus shell, a stdio bridge. None of that is in the tree.
- `RUST_REWRITE.md` has useful notes on the Node-to-Rust port, but it predates
  orgs and RLS, and the worker binary it names is gone.
- `docs/mail-ops.md` describes hosted-era mail operations against a compose
  stack that no longer matches. Mail routes and the mail archive do exist,
  behind `HIVE_MAIL_ENABLED`.
- `plugins/claude-code-hive-memory/` and `integrations/claude-desktop/` both
  tell the reader to `cargo install --path bridge` and run `hive-bridge` on
  stdio. There is no bridge crate; MCP is `POST /mcp` on `hive-api`.
- `docs/ARTIFACTS.md` and `docs/SELF-HOST.md` predate the current shape in
  places (blockstore naming, a Node `hive-server`).

Fix these docs when touching the related area.
