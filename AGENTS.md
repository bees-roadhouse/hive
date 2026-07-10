# AGENTS.md

## Scope

These instructions apply to the whole `bees-roadhouse/hive` repository.

GitHub `main` is canonical (the old `development`/`release` pair collapsed into
it on 2026-07-05). This repo is mid-pivot to a personal P2P desktop app
(docs/DIRECTION.md D16+; teardown/rebuild sequence in docs/PLAN.md):

- Rust workspace: `shared`, `embed`, `core`, and `jmap-sync`. There is no
  Node/pnpm workspace anymore — the Solid SPA, the legacy Node packages, the
  worker/mail daemons (PR 1.2), and the api crate with its REST/auth/OAuth
  surface (PR 1.3) were deleted in Phase 1 teardown. No binary exists in the
  workspace right now — that is per plan (hive-import arrives in PR 1.7,
  hive-bridge in PR 1.8). Do not treat the old Node/SQLite README framing as
  the source of truth for new work.
- Postgres is the active datastore for the Rust store layer (until the Phase 1
  SQLite cutover in PR 1.6).

When docs disagree with code, workflows, or compose files, trust code and CI,
then update the stale doc in the same change. `README.md` and parts of
`RUST_REWRITE.md` may lag the current Rust/Postgres reality.

## Architecture

- `core/`: hive-core — the store layer (Postgres store, schema/migrations,
  pgq helpers, embedding backfill) plus the MCP tool layer (`core/src/mcp.rs`,
  transport-free: request/response over serde_json values; the PR 1.8 stdio
  bridge is its transport). `mcp::LocalCtx { actor }` supplies the acting
  identity — there is no authentication layer (single user, D16).
- `shared/`: Rust shared domain types.
- `embed/`: embedding seam, ONNX/BGE implementation, and hash fallback.
- `jmap-sync/`: JMAP mailbox sync library (kept through the pause; its offline
  quote-corpus test keeps the mail parser alive until mail returns in Phase 3).

## Core Invariants

- Journal-first model: journal entries are the source; tasks, decisions, events,
  and links derive from anchored spans or explicit structured operations.
- Old journal entries are history. Do not rewrite old bodies to reflect status
  changes; render from canonical state instead.
- hive-core reaches Postgres through `DATABASE_URL`.
- Single user, single human (D16): there are no accounts, sessions, tokens,
  scopes, or admin gates. Reads are unscoped. Do not reintroduce viewer/ACL
  parameters. The `user_scope`/`owner` COLUMNS and the values writes stamp
  are load-bearing — old data must stay readable and shape-stable for the
  PR 1.6 cutover and the PR 1.7 importer; only the filtering was removed.
- `db.rs::init` must stay idempotent against an EXISTING old-shape database:
  the deleted hosted-era tables are simply no longer created or read — never
  add DROPs for them (the 1.7 importer reads old instances itself).
- Every hive-core integration test constructs its store through
  `core/tests/common/mod.rs::test_store()` — the seam PR 1.6 swaps to SQLite.
  No test body touches Postgres construction outside common/.
- `core/tests/golden_retrieval.rs` + its checked-in fixture are the
  cross-backend parity oracle. Regenerate only consciously
  (`HIVE_GOLDEN_REGEN=1`) and diff; PR 1.6 may relax the score tolerance but
  must keep the label-set and top-3 order assertions.
- Use `HIVE_EMBED=hash` for CI, local smoke tests, and offline checks.

## Branching

- `main` is the only long-lived branch; it must stay releasable.
- Work branches start from `main` and use `feature/{slug}`, `bug/{slug}`,
  `improvement/{slug}`, or `refactor/{slug}`, merging back via PR.
- Releases are tag-driven: bump versions in a release PR, merge, then push
  `v{version}` on the merge commit. (Dormant until PR 1.8 — Phase 1 lands
  untagged and no release workflow exists right now.)

## Setup

Use the pinned Rust toolchain in `rust-toolchain.toml`. On Windows, prefer a
target dir outside the repo for Rust builds:

```powershell
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo-target\hive"
```

Tests expect Postgres (pgvector-capable) unless `DATABASE_URL` points
elsewhere; any pgvector/pgvector:pg17 container works:

```powershell
$env:DATABASE_URL = "postgres://hive:hive@localhost:5432/hive"
```

There is no compose path or shippable image between the PR 1.3 teardown and
the PR 1.8 bridge / Phase 2 app bundles.

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

`.github/workflows/ci.yml` is the only workflow, with one job, triggered on
PRs to `main`:

- `rust`: starts Postgres 17 (pgvector), runs `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo build --workspace --all-targets`, and `HIVE_EMBED=hash cargo test --workspace`.

There is no release workflow: nothing shippable exists between the PR 1.3
teardown and the PR 1.8 bridge / Phase 2.5 app bundles — releases return
then. The version of record is the `[workspace.package]` version in the root
`Cargo.toml`.

## Rust Code Style

- Keep store logic in `core/src/store/*` and the MCP tool layer in
  `core/src/mcp.rs` (pure — no transport/HTTP types in core).
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

- Journal-first write path (`store/journal.rs`): bracket-token and anchor
  parsing runs over untrusted prose at append time.
- The credential vault (`store/cc_credentials.rs`): AES-256-GCM under
  `HIVE_CRED_KEY`; plaintext must never reach a log or a tool result.
- Embedding/search index maintenance: deletes must scrub search/embeddings
  rows (actor cascade, mail redaction) so nothing orphaned resurfaces in
  retrieval.
- MCP tool layer (`core/src/mcp.rs`): every tool result is content the
  calling agent will read — treat stored data as untrusted input to it.

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
- `plugins/` and `integrations/` still point at the retired HTTP `/mcp`
  endpoint; they repoint to the stdio bridge in PR 1.8. Do not "fix" them
  earlier — there is nothing to point them at yet.

Fix these docs when touching the related area.
