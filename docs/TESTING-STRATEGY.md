# Hive testing strategy

Status: rewritten 2026-08-17 against the web-app architecture
([WEB-APP.md](./WEB-APP.md)). Scope: the whole workspace ... `shared`, `embed`,
`core`, `api`, `jmap-sync`, `relay` ... plus the SolidJS SPA in `packages/`.

This file says where tests live, how to run them, which conventions a PR is
expected to satisfy, and what is deliberately not covered. When it disagrees
with `.github/workflows/ci.yml` or with the code, the workflow and the code
win, and fixing this file belongs in the same change.

The previous version planned the v2.1 testing program: a smoke tier driving
`hive-node` and `hive-sync` binaries over sockets, a WebKitGTK screenshot tier
under xvfb, dioxus-ssr DOM snapshots, and grep fences over an op-log store.
None of those crates, tiers, or CI jobs exist. That version is replaced rather
than amended, for the reason WEB-APP.md gives about the docs it supersedes: a
plan describing a deleted architecture cannot be read as guidance.

## The tiers

| Tier | What | Where | Runs in |
|---|---|---|---|
| Inline unit | Pure functions, parsers, policy helpers, SQL shaping | `#[cfg(test)] mod tests` at the foot of the source file | `rust` job |
| Store integration | Real Postgres, one throwaway schema per test | `core/tests/` and the store's own inline mods | `rust` job |
| HTTP integration | The real axum router, middleware included | `api/tests/` | `rust` job |
| Corpus | Data-as-contract text pairs | `jmap-sync/tests/corpus/` | `rust` job |
| SPA | Typecheck, then the production bundle | `packages/web` | `web` job |
| Perf, opt in | 200k-vector ANN gate, `#[ignore]`d | `api/tests/vector_perf.rs` | on demand |

**The inline `#[cfg(test)] mod tests` block is the dominant idiom.** Roughly
forty source files carry one; a dozen or so sit under a `tests/` directory.
Reach for an integration file when the test needs the composition ... a router
with its middleware, a migration over an old-shape database, two orgs sharing
one schema ... and otherwise put the test next to the code it covers.

There is no env-gated tier. `cargo test --workspace` runs everything CI runs:
one command, no flags, no `HIVE_SMOKE`-style switches to remember.
`scripts/smoke.sh` was deleted alongside this rewrite. Every crate it built and
tested was gone, so its crate filter selected nothing and it silently ran the
whole workspace under a banner announcing a crate-scoped tier, and no CI job
ever called it.

## Running it

The suite needs a real PostgreSQL with pgvector. There is no in-memory or
SQLite mode: Postgres is the store, row-level security is the access control,
and a test against anything else would prove nothing about either.

```
./dev-setup.sh          # pgvector/pgvector:pg17 on :5432, idempotent

DATABASE_URL=postgres://hive:hive@127.0.0.1:5432/hive \
HIVE_EMBED=hash \
HIVE_CRED_KEY=dev-credential-vault-key \
  cargo test --workspace
```

- **`DATABASE_URL`** falls back to `postgres://hive:hive@127.0.0.1:5432/hive`
  (`hive_core::db::database_url`), which is exactly what `dev-setup.sh` brings
  up, so locally you can usually leave it unset. The role it names must be able
  to `CREATE ROLE`: the suite provisions the unprivileged serving role the same
  way the binary does, and `api/tests/org_isolation.rs` then serves as it.
- **`HIVE_EMBED=hash`** keeps the embedder offline. The default provider lazily
  pulls BGE models from the HF hub, and no test run may reach the network. The
  provider choice latches once per process, so a binary that needs a
  384-dimension provider installs its own mock engine before the first embed
  call (`core/tests/embedding.rs`, `api/tests/vector_perf.rs`).
- **`HIVE_CRED_KEY`** arms the credential vault
  (`core/src/store/cc_credentials.rs`). Any string works. Without it, code
  paths that store a mail account password fail loudly instead of quietly
  writing plaintext.
- Per-test schemas are dropped by `TestDb`'s `Drop`, so a passing or panicking
  run leaves nothing behind. A hard kill skips unwinding and leaks one;
  `./dev-setup.sh --drop-test-schemas` sweeps the leftovers.

The SPA:

```
pnpm install
pnpm typecheck          # tsc over both tsconfigs, the same gate CI runs
pnpm build              # @hive/shared, then the vite bundle
```

## Conventions

Name these in a PR description when the PR touches them. They exist because
each one was, at some point, the thing that made a test lie.

### ONE-SEAM

Every test that needs a database gets it from `hive_core::db`, never by opening
a pool of its own:

- `test_pool()` ... the general case. A task with no acting scope falls back to
  the default org, so store-level tests (which never pass through auth
  middleware) still exercise real SQL.
- `test_pool_strict()` ... no fallback scope, exactly how the binary behaves.
  Anything asserting on isolation uses this, so the assertion is about the
  policy rather than about a helper.
- `test_pool_single_conn()` ... strict, one connection, so every statement
  provably rides the same physical connection. This is what turns "a pooled
  connection gets reused across two orgs" from a hope into a test.
- `test_pool_unmigrated()` ... empty schema, owner role, migrations not run, for
  tests that lay down an old-shape table by hand and migrate over it.
- `test_admin_pool()` ... an owner-role pool on the same schema, for setup that
  has to happen from outside any org: seeding a second org, or reading across
  orgs to prove a leak did not happen.

### SCHEMA-PER-TEST

`TestDb` holds the pool and its teardown in one value, and the schema lives
exactly as long as the test that owns it. It is `#[must_use]` for a reason:
bind it for the whole test (`let (app, store, _test_db) = ...`), and have any
helper that builds a `Store` or a `Router` hand it back with them. Dropping it
inside the helper drops the schema out from under what the helper returned, and
the next query fails with `relation "..." does not exist`.

### RLS-OR-NOTHING

Tests connect as the same unprivileged role the API serves as, and the helpers
prove it before handing back a pool: `assert_rls_applies` refuses a SUPERUSER or
BYPASSRLS connection outright, because either one reads every org's rows with no
policy firing. A test against a superuser pool would prove nothing.
(`test_pool_unmigrated` is the one exception, an owner-role pool for tests that
migrate by hand.)

`api/tests/org_isolation.rs` is the security model in test form. It drives the
real router rather than unit-testing a helper, because the thing under test is
the composition: auth resolves a credential, the credential pins one org, and
Postgres enforces it. If that file passes, a member of org A cannot read or
write org B; if it fails, nothing else in the tree matters.

### OFFLINE-DETERMINISTIC

No test run touches the network. Embeddings come from the hash provider or an
injected mock; the relay tunnel tests synthesize a ClientHello instead of doing
a real TLS handshake; JMAP parsing runs against the checked-in corpus and never
a mail server. A test that would need the network is a manual gate, and belongs
in the list at the end of this file.

### ENV-LOCK

Environment variables are process-global and test binaries run their tests in
parallel, so anything that sets one serializes on a mutex for the duration
(`ENV_LOCK` in `api/tests/parity_smoke.rs` and `core/tests/embedding.rs`).
Never assume test ordering, and never leave a variable set for the next test to
find.

### EPHEMERAL-PORTS

Anything that binds asks for `127.0.0.1:0` and reads back the assigned port
(`relay/tests/tunnel.rs`). No fixed ports, ever: they collide under a parallel
run and turn into flakes that only reproduce on a busy machine.

### CORPUS-AS-CONTRACT

`jmap-sync/tests/corpus/` holds paired `NN_name.in.txt` and `NN_name.out.txt`
files, and `quote_corpus.rs` walks them. Adding a regression case is adding two
text files, with no Rust change. Keep it that way: the moment a case needs
bespoke code, the corpus stops being readable as a spec of what quote stripping
does.

## Coverage rules by surface

- **Auth, sessions, and org isolation.** Through the real router with real
  credentials. A helper-level test of an authorization function proves the
  function, not the system. See `org_isolation.rs`, and `parity_smoke.rs` for
  the wider onboarding, session, token, and ACL sweep.
- **Migrations.** Both bases the hybrid convention promises to tolerate: a
  fresh database where the inline DDL builds the final shape and the migration
  no-ops, and an old-shape database the migration has to reshape
  (`api/tests/migrations.rs`). A migration with only the greenfield case tested
  is untested, because greenfield is the case that cannot break.
- **Upgrade paths.** Same principle, one level up: `api/tests/org_upgrade.rs`
  reconstructs the pre-orgs v0.6 shape and migrates it, because the install
  that matters is the one already full of prose.
- **Routes.** A new route gets an integration test that drives it through
  `Router::oneshot` with the middleware attached. Content routes assert bytes,
  not just status codes ... `api/tests/artifacts.rs` compares payloads and
  exact range slices, since a content route returning the right status and the
  wrong bytes is worse than one that fails.
- **Store methods.** Inline `#[cfg(test)] mod tests` against `test_pool()`,
  unless the case needs more than one org or more than one pool.
- **Search and ranking.** Both retrieval paths stay covered without ONNX in CI:
  `api/tests/semantic_query.rs` drives the brute-force path under the hash
  provider, and `api/tests/vector_perf.rs` drives the ANN path under a fake
  384-dimension engine.
- **Mail and JMAP.** Parsing and quote-stripping go in the corpus; anything
  that talks to a server does not get a test.
- **Relay.** In-process against `Daemon`, asserting the property that actually
  matters: route on the SNI, then pass bytes through unchanged.
- **The SPA.** `tsc` is the whole gate. There is no test runner in
  `packages/web` yet, so a component behaves as reviewed or not at all.

## CI

Two jobs, both merge-gating, both in `.github/workflows/ci.yml`. (The previous
version of this file named five merge-gating checks: `rust`, `importer`,
`smoke`, a wasm build gate, and DOM snapshots. `smoke`, the wasm gate, and the
snapshots were never written, and `importer` died with its crate. Its Postgres
service is now attached to `rust`.)

**`rust`** (ubuntu-latest, toolchain 1.94.1 matching `rust-toolchain.toml`, a
`pgvector/pgvector:pg17` service): `cargo fmt --check`, then
`clippy --workspace --all-targets -D warnings`, then
`build --workspace --all-targets`, then a release build of `hive-api`, then
`cargo test --workspace` with `HIVE_EMBED=hash`, `DATABASE_URL`, and
`HIVE_CRED_KEY` set.

Postgres is attached to the job itself rather than to one special test job.
That inverts the old file, which paired a no-database `rust` job with a single
`importer` job holding the only Postgres in CI. Under a Postgres store the two
crates carrying most of the suite, `core` and `api`, cannot run a test without
one. The extra release build of `hive-api` is there because the container
ships the release binary and borrow-check can differ between opt levels, so
that failure class is caught in CI instead of at packaging time.

**`web`** (ubuntu-latest, node 22, pnpm through corepack): `pnpm install
--frozen-lockfile`, then `@hive/web` typecheck, then `@hive/web` build.
Typecheck runs first on purpose: vite transpiles with esbuild, which strips
types without checking them, so a type error anywhere in the SPA still bundles
clean and only `tsc` will ever see it. The API serves the built SPA
(`api/src/routes/spa.rs` serves `HIVE_WEB_DIST`), so a frontend that fails to
build is a broken release even when Rust is green.

Nothing else gates. There is no nightly job, no fuzzing, no perf gate in CI, no
screenshot job, and no release workflow.

## What is not covered

Stated plainly, so nobody assumes otherwise:

- **The SPA has no behavioral tests.** Typecheck plus a clean bundle is the
  entire signal. Wiring a test runner into `packages/web` is the single largest
  gap in this document.
- **No browser drives the app.** The SPA-serving route has unit coverage, but
  nothing loads the built bundle and clicks through it.
- **Real TLS on the relay.** `relay/tests/tunnel.rs` uses a synthetic
  ClientHello deliberately, to test routing and pass-through without a
  certificate in the way. `relay/demo/run.sh` covers the real handshake by
  hand, when someone runs it.
- **Live mail.** No test authenticates to a JMAP server, so the sync loop is
  covered only as far as its parsing.
- **The ANN perf gate is opt-in** and takes minutes of setup:
  `cargo test -p hive-api --test vector_perf -- --ignored --nocapture`. Run it
  before and after any change to the vector path, and put the numbers in the PR.
  CI never runs it.
- **Real embedding models.** Everything runs under the hash embedder or a mock
  engine, so a model or tokenizer regression is invisible here.
- **Multiple processes.** Every test drives the router or the daemon in
  process. Nothing spawns a binary, so the actual startup path of `hive-api`
  (config resolution, role provisioning against a cold database, the SPA dist
  path) is only exercised by running it.
- **Concurrency and load.** Connection-pool exhaustion, lock contention, and
  behavior under real traffic have no coverage.
