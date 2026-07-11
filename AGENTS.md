# AGENTS.md

## Scope

These instructions apply to the whole `bees-roadhouse/hive` repository.

GitHub `main` is canonical (the old `development`/`release` pair collapsed into
it on 2026-07-05). This repo is mid-pivot to a personal P2P desktop app
(docs/DIRECTION.md D16+; teardown/rebuild sequence in docs/PLAN.md):

- Rust workspace: `shared`, `embed`, `core`, `jmap-sync`, `app`, `bridge`,
  and `importer`. There is no Node/pnpm workspace anymore — the Solid SPA,
  the legacy Node packages, the worker/mail daemons (PR 1.2), and the api
  crate with its REST/auth/OAuth surface (PR 1.3) were deleted in Phase 1
  teardown. The shipping binaries are the app, `hive-bridge` (PR 1.8), and
  the `hive-import` one-shot (PR 1.7).
- The datastore is the append-only op log + SQLCipher SQLite index under a
  local data dir (the PR 1.6 cutover; D18). Postgres left the workspace —
  the PR 1.7 importer is the one remaining Postgres reader and declares its
  own sqlx (never in `[workspace.dependencies]`). The app may depend on the
  hive-import LIBRARY (it ships the first-launch GUI import) but its sources
  stay sqlx-token-free, and no other crate may depend on hive-import at all
  — the grep gate in `importer/tests/no_postgres_gate.rs` states and
  enforces the whole rule.

When docs disagree with code, workflows, or compose files, trust code and CI,
then update the stale doc in the same change. `README.md` and parts of
`RUST_REWRITE.md` may lag the current Rust/SQLite reality.

## Architecture

- `core/`: hive-core — the op log (`oplog`), blockstore, key custody
  (`keys`), the SQLCipher index + fold projector (`index`, `fold`), and the
  store layer riding ONE writer thread (`store/core.rs`: mpsc commands,
  oneshot replies; the public `Store` surface stays async), plus the MCP tool
  layer (`core/src/mcp.rs`, transport-free: request/response over serde_json
  values; the PR 1.8 stdio bridge is its transport). `mcp::LocalCtx { actor }`
  supplies the acting identity — there is no authentication layer (single
  user, D16).
- `bridge/`: the `hive-bridge` binary — the ONLY external doorway (D25).
  A thin stdio transport over `core::mcp` (serve mode: JSON-RPC 2.0, one
  message per line; `call` mode: one tool call for hooks/scripts). Interim
  mode opens the store directly via `Store::new` with the app's exact
  data-dir/keychain/actor resolution; Phase 2.4 flips it to a UDS proxy.
  `HIVE_DATA_DIR` and `HIVE_MEMORY_KEY_HEX` are bridge-only escape hatches —
  never teach core or the app to read them.
- `importer/`: the `hive-import` binary (PR 1.7) — one-shot migration of a
  hosted-era Postgres into a fresh data dir (refuses a non-empty one).
  Records ride the `#[doc(hidden)]` `Store::import_batch` seam
  (`core/src/store/import.rs`): original nanoid ids and timestamps preserved,
  `origin` provenance on every payload (fold v3), commits still flow through
  `Core::commit`. Honors `HIVE_DATA_DIR`; its keychain escape hatch is
  `HIVE_IMPORT_KEY_HEX` (importer-only, mirroring the bridge's — each binary
  names its own). Embeds nothing: the app backfills embeddings later.
  Also a LIBRARY: `run(&Opts)` returns `RunOutcome::{Plan, Imported}` as
  data (the CLI formats it; the app's onboarding renders it), and after a
  real import the store is fully shut down so the same process can
  immediately `Store::new` the dir — the app's first-launch GUI import
  depends on both facts.
- `shared/`: Rust shared domain types.
- `embed/`: embedding seam, ONNX/BGE implementation, and hash fallback.
- `jmap-sync/`: JMAP mailbox sync library (kept through the pause; its offline
  quote-corpus test keeps the mail parser alive until mail returns in Phase 3).

## Core Invariants

- Journal-first model: journal entries are the source; tasks, decisions, events,
  and links derive from anchored spans or explicit structured operations.
- Old journal entries are history. Do not rewrite old bodies to reflect status
  changes; render from canonical state instead.
- Writes are RECORDS (D18): the command layer (store modules) mints ids and
  timestamps, pre-computes emergence, and commits one record batch per
  logical write — LogWriter::append_batch (fsync), then fold::apply in one
  SQLite transaction. The fold (`core/src/fold`) is deterministic and never
  mints anything; its module header is the payload contract. Never write
  fold-owned tables directly from production code (the raw_sql seam is
  tests/diagnostics only — direct writes do not survive a rebuild-by-replay).
- Single user, single human (D16): there are no accounts, sessions, tokens,
  scopes, or admin gates. Reads are unscoped. Do not reintroduce viewer/ACL
  parameters. The `user_scope`/`owner` COLUMNS and the values writes stamp
  are load-bearing — old data must stay readable and shape-stable for the
  PR 1.6 cutover and the PR 1.7 importer; only the filtering was removed.
- (Historical: the Postgres `db.rs::init`/no-DROP rule died with the PR 1.6
  cutover. Old Postgres instances remain untouched on their servers for the
  PR 1.7 importer to read.) Schema changes now mean bumping
  `fold::FOLD_VERSION` — the index drops derived tables and rebuilds by
  replaying the op log at next open.
- ONE hive process per data dir: `Store::new` takes an exclusive advisory
  flock on `<data_dir>/lock` and holds it until shutdown/exit, so the app
  and an interim-mode bridge can never co-write the log/index. The refusal
  message contains "another hive process" (tests and the plugin's soft-fail
  matching depend on that text). flock, not fcntl, deliberately: a second
  open in the same process must conflict too. `Store::shutdown` releasing
  the lock is what lets reopen-style tests (and users switching between app
  and bridge) proceed.
- The bridge's stdout is the MCP protocol channel — frames only, one JSON
  message per line. Every diagnostic goes to stderr. Never add a print to
  stdout in `bridge/` (and keep HTTP stacks out of it: no reqwest/hyper/
  axum — stdio is the transport, grep-auditably).
- Every hive-core integration test constructs its store through
  `core/tests/common/mod.rs::test_store()` (tempdir data dir + in-memory keys
  + the injected hash embedder; `test_store_with` for mock 384-dim engines).
  No test body constructs a store any other way, and core never reads
  HIVE_EMBED — the embedder is injected at `Store::new`.
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
  `v{version}` on the merge commit. (Dormant until Phase 2.5 rebuilds the
  release pipeline — Phase 1 lands untagged and no release workflow exists
  right now.)

## Setup

Use the pinned Rust toolchain in `rust-toolchain.toml`. On Windows, prefer a
target dir outside the repo for Rust builds:

```powershell
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo-target\hive"
```

Tests are hermetic: tempdir data dirs, in-memory keys, the hash embedder —
no database service, no network — with ONE deliberate exception: the
importer's fixture tests are DATABASE_URL-gated (they skip loudly and pass
without it, so `cargo test --workspace` stays green offline). To run them
for real, point them at a pgvector Postgres:

```bash
DATABASE_URL=postgres://hive:hive@localhost:5432/hive cargo test -p hive-import
```

Everything else stays Postgres-free by construction — the grep gate
(`importer/tests/no_postgres_gate.rs`) fails on any `sqlx`/`pgvector` token
outside `importer/`, and on any crate other than `app/` depending on
hive-import (the app rides the importer library for GUI import; the engine
crates and the bridge stay Postgres-free even transitively).

There is no compose path or shippable image anymore. The local binaries are
the app (`cargo run -p hive-app`) and the bridge
(`cargo install --path bridge`); packaged bundles arrive with Phase 2.5.

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

`.github/workflows/ci.yml` is the only workflow, with two jobs, triggered on
PRs to `main`:

- `rust`: runs `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo build --workspace --all-targets`, and `HIVE_EMBED=hash cargo test --workspace`.
  No database service and no DATABASE_URL — that absence is the invariant
  (the importer's DB tests self-skip); `HIVE_EMBED=hash` stays as
  belt-and-braces against hive-embed's default provider downloading models.
- `importer`: the PR 1.7 exception — a `pgvector/pgvector:pg17` service +
  DATABASE_URL, running `cargo test -p hive-import` only. The only Postgres
  anywhere in CI.

There is no release workflow: the bridge installs from the repo
(`cargo install --path bridge`) and app bundles land with Phase 2.5 —
releases return then. The version of record is the `[workspace.package]`
version in the root `Cargo.toml`.

## Rust Code Style

- Keep store logic in `core/src/store/*` and the MCP tool layer in
  `core/src/mcp.rs` (pure — no transport/HTTP types in core).
- Use rusqlite with explicit queries that match the existing style; reads
  run inside `Store::run` closures on the writer thread, writes go through
  record drafts + `Core::commit`.
- The derived schema lives in `core/src/index/mod.rs` (`DDL`), owned by the
  fold contract: any change to it, to payload interpretation, or to handler
  behavior bumps `fold::FOLD_VERSION` (drop-and-replay is the migration
  story, D14/D18). The op-log record ENVELOPE and encodings stay frozen
  (PR 1.4) — widening payload semantics is a documented fold-contract
  amendment, not a drive-by.
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

- `RUST_REWRITE.md` contains useful Rust architecture notes but predates the
  P2P pivot and the Phase 1 teardown.
- `docs/mail-ops.md` describes hosted-era mail operations; mail returns as a
  module in Phase 3 and the runbook gets rewritten then.

(`README.md`, `plugins/`, and `integrations/` were rewritten for the pivot
in PR 1.8 — the plugin and the `.mcpb` run through the stdio `hive-bridge`;
the hosted-era Codex/Hermes adapters were deleted with the server they
called.)

Fix these docs when touching the related area.
