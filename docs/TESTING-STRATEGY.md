# Hive v2.1 — Testing Strategy

Status: adopted 2026-07-24. Owner: testing workstream. Scope: the whole v2.1 program —
hive-node/hive-sync replication, identity/control log, web head, sharing — plus the retrofits the
existing code needs regardless.

This is the standalone testing document. It defines four tiers, names the conventions every PR must
cite, specifies the smoke and screenshot harnesses in full, and wires them into CI. Paths are
repo-relative to `hive/`. All critique fixes from the design review are incorporated; §8 records
the disposition of each blocker/major and the deliberate rejections.

The four tiers at a glance:

| Tier | What | Where | Gate | Runs |
|---|---|---|---|---|
| Unit | Hermetic in-crate tests, golden fixtures, grep fences, proptests | each crate's `tests/` + inline | none (always on) | `rust` job, every PR |
| Smoke | Real binaries, real sockets, multi-process scenarios | `node/tests/`, `sync/tests/`, `smoke/` | `HIVE_SMOKE=1` | new `smoke` job, every PR |
| DOM snapshots | dioxus-ssr rendered markup, insta-pinned | `app`/`ui` test mods | none (deterministic) | inside `rust` job |
| Screenshots | Real pixels: WebKitGTK app under xvfb in a pinned container | `app/tests/shots/` + `ci/shots/` | `HIVE_SHOTS=1` | new `ui-shots` job, advisory then required |

Tier gating copies the repo's existing idiom — the importer's loud-skip macro
(`importer/tests/import_e2e.rs:35-40`), not cargo-nextest. `cargo test --workspace` stays green
offline with zero new tooling; CI jobs set the env vars.

---

## 0. Decisions this plan assumes (settled in the PR 4.0 decision record)

The three design docs disagreed on a few points that change what gets tested. This plan is written
against the following reconciliations, since recorded in the DIRECTION v2.1 amendment (D29-D34)
and executed at PR 4.0; if 4.0 lands differently, §3.6 carries the fallback.

1. **Control records live in a dedicated control log** (`<data_dir>/control/<device>/*.seg`), not
   as data-log `kind::ALL` widenings. Rationale for testing: old builds never look under
   `control/`, so the untestable "every device upgraded before the first record is written" fleet
   gate disappears, and the 11 existing data-log goldens stay untouched. The CI-checkable proxy for
   old-build safety is a compat test (§3.1).
2. **Control segments use a NEW key-derivation context** (`hive-ctl-segment-key-v1` or equivalent),
   never the frozen data-log derivation (which is keyed only by `(master, device, start_seq)` —
   reuse across roots would mint identical keys). The new context is an additive format decision
   with its own golden fixtures (§3.1).
3. **hive-node is a lib plus a thin `main`.** `CARGO_BIN_EXE_hive-node` spawning happens only in
   `node/tests/`; multi-party scenarios drive the real node BINARY from `smoke/`. The whole smoke
   tier assumes this shape.
4. **One key-file format everywhere: 64 hex chars = one raw 32-byte master.** The node's
   `FileKeySource` accepts this raw-hex mode (documented as test/ops mode; the KEK-sealed mode is
   additional), the app's new `HIVE_MASTER_KEY_FILE` seam reads it, `test_domain()` writes it, and
   the screenshot `seed-fixture` bin writes it beside the fixture. One format, four consumers —
   they must not drift.
5. **PR #129 (feature/bridge-proxy) merges before any app-touching v2.1 PR.** Its
   `bridge/tests/stdio.rs` is the repo's only real-binary suite and the template for the entire
   smoke tier; the #129-dependent retrofits (§7 item 2) land immediately after it, and the
   app-boot smoke half rides the PR 5.0 display rig.
6. **Gating split, decided now** (was an open question): merge-gating from day one = `rust`,
   `importer`, `smoke`, the wasm build gate, and DOM snapshots. `ui-shots` runs advisory for two
   weeks while flake rate is measured from retry logs, then becomes required. Nightly/dispatch
   only = cargo-fuzz, the perf canary, and the shots-container image rebuild. Details in §6.

---

## 1. Current inventory (what exists today)

**hive-core** (`core/tests/`, 19 integration files ≈ 109 fns + ~74 inline):

- `core/tests/common/mod.rs:23` — `test_store()`, the single store-construction seam (tempdir +
  `MemoryKeySource([7u8;32])` + `HashEmbedder`; `test_store_with` for injected 384-dim mocks).
  Codified in AGENTS.md:117: no test body constructs a store any other way. This seam is why the
  PR 1.6 backend swap was test-body-free.
- `core/tests/oplog.rs` — golden format freeze: 11 checked-in `.bin` fixtures in
  `core/tests/fixtures/oplog/` (one per kind in `kind::ALL` order, asserted at :146-151),
  byte-exact CBOR; deliberate regen via `HIVE_UPDATE_GOLDENS=1` which fails the run on purpose
  (:175, :188).
- `core/tests/golden_retrieval.rs` — cross-backend retrieval parity oracle against
  `fixtures/golden_retrieval.json`; label-keyed so nanoids never enter the fixture;
  `HIVE_GOLDEN_REGEN=1` to regenerate.
- `core/tests/determinism.rs:23-38` — source-text grep gate: `oplog`/`blockstore`/`fold`/`index`
  must contain no `SystemTime`/`now_iso`/`Utc::now`/`rand::`/`OsRng`/`getrandom`/`std::env`/
  `env::var`/`nanoid` tokens, code or comments. Cheap, total, undodgeable.
- `core/tests/crypto_shred.rs:183-276` — shred proven end-to-end (blocks gone, wrapped-key row
  destroyed, replay does not resurrect).
- `core/tests/cutover.rs` — two Stores on one data dir (single-writer flock exclusion); the
  nearest existing seed for any multi-store harness.
- `core/tests/keys.rs:87` — the one `#[ignore]`: live OS-keychain round-trip (headless CI has no
  Secret Service). Everything else is hermetic.
- Also: `blockstore.rs`, `fold_replay.rs`, `sqlite_index.rs`, `mail_store.rs`, `mcp_tools.rs`,
  `store_smoke.rs`, `semantic_query.rs`, `embedding.rs`, `events_store.rs`, `reaper.rs`,
  `tasks_store.rs`, `sqlcipher_spike.rs`, `vector_perf.rs` (the old 200k-vector perf gate is dead
  — header at :7-11 documents it died with Postgres).

**hive-import** (`importer/tests/`): `require_pg!` loud-skip macro (`import_e2e.rs:35-40`) — DB
tests skip-pass without `DATABASE_URL`; `no_postgres_gate.rs:22-124` — second grep gate, and it
already **auto-fences new workspace crates** by parsing workspace members (the shape §2's new
fences extend).

**hive-bridge** (branch `feature/bridge-proxy` = open PR #129): `bridge/tests/stdio.rs` — the only
test in the repo spawning a real built binary (`env!("CARGO_BIN_EXE_hive-bridge")` at :70),
`#![cfg(unix)]`, 5 tests: in-test UDS host running the real `mcp::serve_bridge_connection`, MCP
initialize/tools/call round-trips, one-shot call mode, concurrent clients, the app-not-running
stderr contract (:350-378). Explicitly untested (:64-67): the app side — peer-cred check, socket
bind.

**jmap-sync**: `jmap-sync/tests/quote_corpus.rs` — data-as-contract corpus (9 paired
`NN_name.in.txt`/`.out.txt` cases); adding a regression case is adding two files.

**hive-app**: one inline `#[cfg(test)]` mod (`app/src/main.rs:9295-10307`, ~38 fns) covering pure
functions only — the hostile-corpus HTML sanitizer test (:9306) is the security contract; calendar
math; reply addressing. Zero of the 139 `rsx!` sites are tested. No headless entry point:
`main()` goes straight to `dioxus::LaunchBuilder::desktop()`; `data_dir()` hard-codes XDG
(:197-209); `boot()` unconditionally resolves the OS keychain (:211-221).

**CI** (`.github/workflows/ci.yml`, the only workflow): job `rust` (ubuntu-latest, toolchain
1.94.1, webkit/gtk -dev deps, Swatinem cache `hive-rust`, fmt/clippy/`build --all-targets`/release
app build/`HIVE_EMBED=hash cargo test --workspace`, **no DATABASE_URL** as a stated invariant) and
job `importer` (pgvector/pg17 service — the only database in CI). Warm runs ~4-5 min; the latest
branch run's `rust` job took 13m34s cold. Nothing boots the app, renders a component, or takes a
screenshot; no xvfb, no snapshot crate, no fuzz/proptest, no test tiering, no release workflow.

**Known-stale substrate** (feeds §7): `dev-setup.sh` still claims a Postgres store (pre-1.6) and
spins a `hive-pg` container from the SessionStart hook; `ci/stalwart/` references a `mail-e2e` job
and test file that no longer exist (parked until PR 3.2); `core/src/store/mail.rs:4-5` claims the
sync daemon is paused (it is live in-app).

---

## 2. Conventions (named, citable)

Every v2.1 PR description cites, by name, the conventions it satisfies. Each convention gets
codified in AGENTS.md in the PR that first lands it.

### GOLDEN-BYTES

Every **frozen on-disk encoding** gets checked-in `.bin` fixtures, byte-exact asserts, and
deliberate regeneration via `HIVE_UPDATE_GOLDENS=1` which fails the run on purpose
(`core/tests/oplog.rs` is the template, including its freeze-message wording: a golden failure is
a format decision, not a fixture refresh).

Scope discipline (revised per critique): goldens are for artifacts with a **freeze promise** —
envelope kinds, control-record payloads, wrap formats, key-derivation outputs, segment headers.
The **versioned sync wire is deliberately excluded** while it churns during PRs 4.4-4.16: wire frames get
ROUND-TRIP + HOSTILE-DECODE coverage instead, and cross-version compat fixtures arrive at the
first tagged release that carries proto v1 (see §3.4 and §8 rejections).

### ONE-SEAM

`test_store()` (`core/tests/common/mod.rs:23`) remains the only way core tests build a store
(AGENTS.md:117). v2.1 extends the family in a new `smoke-support` helper crate:

- `test_domain()` — tempdir domain dir + a written 64-hex master file (the §0.4 format).
- `test_node(root, tier)` — the real `hive-node` from `HIVE_NODE_BIN` on `127.0.0.1:0`, port parsed from its own stdout;
  `tier` selects blind (no key) vs trusted (key file).
- `test_pair()` — two device Stores + one node over the same master; the standard two-device rig.
- `wait_until(cond, deadline, poll)` — the only sanctioned wait primitive (see §4.1).

Same AGENTS.md clause extension: no test constructs a domain, node, or device pair any other way.

### KEY-INJECT

CI never touches an OS keychain. In-proc tests use `MemoryKeySource`; cross-process tests use the
raw-hex key file via `FileKeySource` (node) and `HIVE_MASTER_KEY_FILE` (app — §5.3/§7.0). The one
live-keychain test stays `#[ignore]`d and manual (`core/tests/keys.rs:87`). Fixed test keypairs
for signature fixtures are checked into the repo and marked test-only.

### GREP-FENCE

Copy the total-grep architecture gates (`core/tests/determinism.rs`,
`importer/tests/no_postgres_gate.rs` — the latter auto-walks workspace members so new crates
cannot dodge). Two new fences, **with the token lists revised per critique** — the naive versions
go red on day one against existing legitimate code (`core/src/store/cc_credentials.rs:53` defines
the credential kind string `"oauth_token"`; OAuth/token appear in comments at
`core/tests/mcp_tools.rs:105`, `core/tests/store_smoke.rs:4`, `importer/src/lib.rs:323`):

- **Identity fence** ("no sessions/tokens/OIDC below the node binary"): fence
  dependency/namespace tokens, not bare words — `openidconnect`, `jsonwebtoken`, `use oauth2`,
  `oauth2::`, plus the session/cookie crate names the node adopts (`tower-sessions`,
  `axum-extra::cookie`, ...) — everywhere except `node/`. Additionally a **cargo-metadata
  assertion** (extending the no_postgres_gate shape): the OIDC/session crates appear in no
  workspace crate's dependency tree except hive-node's. The fence must land green against the
  current tree; the four known benign hits above stay legal because bare `oauth`/`token` words are
  not fenced.
- **Tenancy-blind fence** ("the data plane is tenancy-blind"): fence identifiers — `tenant_id`,
  `Tenant`, `tenants/` — under `core/`, not the English word "tenant", so the explanatory comment
  ("core is deliberately tenancy-blind") remains writable.

### HOSTILE-DECODE

New with v2.1: the node accepts bytes from the network, and golden fixtures are the only format
guard today. Proptest (fixed seeds, bounded case counts — runs in normal CI) over **every
network-facing decoder**: `Record::from_cbor_bytes`, the new keyless segment-header parse and
frame walker, every replication frame type, and the signed-statement envelope (truncated sig,
mismatched signer_pk, bstr length lies). Invariant: `Err`, never panic; every length field capped
before allocation. Optional `fuzz/` cargo-fuzz targets exist for the same decoders but run
nightly, never in PR CI.

### CONVERGE-ORACLE

Multi-replica convergence is asserted as canonical-dump **byte-equality** across replicas after
seed-randomized delivery interleavings (the failing seed is printed), plus `applied_seq` watermark
equality per device (`core/src/index/mod.rs:257-259`). Fold-merge semantics get table-driven
units: the LWW `(lc, device)` matrix, update-before-create, UNIQUE-slug collision. The
interleaving corpus **includes backdated-lc records** (a hostile writer claiming an old clock) so
the arbitration rule — hub receipt order for the revocation cutoff; deterministic (lc, device) LWW
over an untrusted, unclamped lc (the clamp was rejected in D29: a Lamport clock legitimately runs
far ahead after offline work) — is pinned in goldens, not implied (privacy-critique fix).

### ADVERSARIAL-SMOKE

New convention (privacy-critique fix): **every THREAT-MODEL amendment maps 1:1 to at least one
test**, or the amendment PR carries an explicit written "untestable because X". The initial
scenario ledger is in §4.4. ADVERSARIAL-SMOKE completeness gates the Stage B close (PR 4.20 /
v0.9.0) the same way the convergence goldens (PR 4.11) gate bidirectional sync.

### LOUD-SKIP

The tiering idiom itself: `require_smoke!()` and `require_shots!()` macros modeled on
`require_pg!` — print one loud "skipping: <tier> disabled (set HIVE_SMOKE=1)" line and return,
still passing. Tests behind them must also skip loudly when a required `HIVE_*_BIN` path env is
unset (§4.1).

---

## 3. Unit layer — coverage rules for the new surfaces

### 3.1 Control log and record kinds

- **Fixture set**: new dir `core/tests/fixtures/ctl/`, one `.bin` golden per kind in
  `ctlkind::ALL` order (`domain.genesis`, `device.add`, `device.revoke`, `custody.grant`;
  `share.grant`/`share.revoke`/`share.received` join when Phase 6 widens the set; `member.*` stay
  design-on-paper until tenant 2), mirrored on the oplog.rs pattern
  including the `payloads.len() == ctlkind::ALL.len()` assert. Payloads are **signed with a fixed,
  checked-in, test-only keypair** so bytes are stable (GOLDEN-BYTES + KEY-INJECT).
- **Sign-the-transmitted-bytes**: the signature construction is `sig` over the domain-separation
  prefix ‖ the exact CBOR `bstr` containing the encoded body — verifiers never re-encode.
  HOSTILE-DECODE proptests over the signature envelope land **before the first golden is cut**
  (privacy-critique fix: malleability/re-serialization traps become permanent once frozen).
- **Derivation-context golden**: a fixture asserting the control-segment key for a fixed
  `(master, device, start_seq)` under the new `hive-ctl-segment-key-v1` context — and a test that
  it differs from the data-log derivation for the same inputs (§0.2; the frozen data derivation's
  own goldens stay byte-identical).
- **Old-build compat test** (replaces the untestable fleet-epoch gate): a store data dir
  containing a populated `control/` tree opens cleanly under the **current, control-unaware**
  heal/open path — the CI-checkable proxy for "old builds never look under control/". Once
  control-aware code lands: a fold-ordering test that control folds before data (device set gates
  verification).
- **Projection tests**: device.add/revoke fold into the new `devices` table; revoke-dominates
  merge (a concurrent add via a revoked signer is dead after fold); historical writers
  (`pubkey: null, status: "historical"`) recognized, never transport-eligible. FOLD_VERSION bump
  test: user_version mismatch drops and rebuilds derived tables by replay.

### 3.2 Replication core surface and merge semantics

- **New public core APIs** (PRs 4.3/4.10) each get direct integration tests through `test_store()`:
  sealed-segment enumeration (device, start_seq, len, sealed flag, whole-file hash); keyless
  header parse that stops before key-unwrap; keyless frame walk over plaintext lengths with
  frame-hash recomputation; block `has/get/put` by bare 32-byte id with blake3-verify-on-write —
  including the **manifest-present-but-chunks-missing** case, which must not read as "blob
  complete" (pins the partial-blob semantics the naive `has()` would misread).
- **`ingest_segments`**: verbatim foreign bytes land under `log/<device>/`, fold past the
  watermark without a store reopen; idempotent re-ingest; gapless enforcement; torn-final-frame
  held back; a **byte-divergent re-ingest at an already-folded `(device, seq)` is a poison event**
  (quarantine + error, never overwrite).
- **Fold-merge semantics (FOLD_VERSION 4)**, table-driven: LWW per field by `(lc, device)`
  including tiebreaks; update-before-create materializes a ghost row completed by the late create;
  UNIQUE slug collision resolves deterministically (loser suffixed) instead of aborting the fold —
  each of these is **first pinned as a failing/bricking behavior test against today's fold**
  (§7.3) and then flipped when 4.10/4.11 land.
- **CONVERGE-ORACLE goldens**: N seeded interleavings of a divergent-edit corpus fold to identical
  canonical dumps, including the backdated-lc adversarial corpus; lc maintenance units (commit
  sets `1 + max(own, ingested)`; ingest max-merges; legacy `lc = seq` histories arbitrate per the
  decided rule).
- **Bus**: `commit` and `ingest/heal` both emit a fold-applied event a subscriber receives
  (`core/src/store/mod.rs:209-238` currently has zero subscribers and zero tests — §7.4);
  replication liveness and web-head invalidation will sit on this.
- **Forget queue** (blind-cache shred propagation): unit tests for persistence across reopen and
  ack dedupe (exactly-once effect); the crash half lives in smoke (§4.4).

### 3.3 Enrollment and re-wrap crypto

- **External vectors as fixtures**: RFC 7748 (x25519) and RFC 8032 (Ed25519); RFC 9180 vectors if
  HPKE is chosen over static-static ECDH (the spike PR that picks the stack imports the vectors —
  one crypto introduction, cited by all later PRs).
- **Determinism**: fixed-seed keypair generation reproduces bytes; the pairwise wrap key
  derivation (`hive-share-wrap-v1`) gets a golden; the recipient-wrap format golden sits beside
  the existing frozen wrap format (`core/src/keys.rs:161-192`), which stays byte-identical.
- **Properties**: re-wrap round-trip (unwrap-then-wrap); wrong-key AEAD failure; SAS derivation
  vector (the pairing short-auth string is a deterministic function of the two pubkeys + node pk).
- **Shred extensions of `crypto_shred.rs`**: shred chases **every** wrapped copy (per-recipient
  grant copies included); **re-ingest-after-shred yields a fresh key** under the random-key
  (non-convergent) put mode — a golden proving the blob id and wrapped key differ from the
  pre-shred ones (privacy-critique fix: this is the only real answer to the convergent trap, so it
  is tested, not documented).

### 3.4 Replication wire frames (deliberately NOT goldens)

Round-trip encode/decode tests plus HOSTILE-DECODE proptests for every frame type (hello, heads,
want, segment-chunk, block put/get, ack, error, Forget). No per-frame byte goldens while the wire
churns through PRs 4.4-4.16 — byte-freeze discipline is reserved for on-disk artifacts (accepted
simplicity-critique fix). Cross-version wire compat fixtures are added at the first tagged release
carrying proto v1 (the v0.8.0 / M1 tag), or when a proto v2 appears — when compat becomes a real
promise.

### 3.5 Node config and listener validation

Unit-testable halves of the CI-unreachable surfaces: `node.toml` parse/defaults/rejects; quota
config; **tailnet mode refuses `0.0.0.0`/public bind addresses** and accepts only the configured
interface address (bind-addr validation is testable in CI even though tailnet reachability is not
— the reachability half is a manual gate, §6.4).

### 3.6 Fallback if the format decision goes to kind-widening

If control records end up in the data log as `kind::ALL` widenings after all: the fixtures move to
`core/tests/fixtures/oplog/` in `kind::ALL` order per the existing assert, and the only
CI-checkable half of the rollout gate is a **two-epoch sync test** — a node never streams
new-kind records to a peer whose hello advertises the older format epoch. The "all devices
upgraded before the first record is written" half is then a manual/process gate and must be stated
in writing in the PR. This plan recommends against that path (§0.1).

---

## 4. Smoke layer — real binaries, real sockets

### 4.1 Crates, layout, harness rules

**Layout** (follows the proven stdio.rs split — `CARGO_BIN_EXE_*` is only available in the crate
that owns the binary):

- `node/` = hive-node, **lib + thin main**. `node/tests/boot.rs` spawns the real binary via
  `env!("CARGO_BIN_EXE_hive-node")`.
- `sync/` = hive-sync. `sync/tests/` holds framing round-trips, the TLS loopback suite (§4.3), and
  loopback two-dir sync + kill/resume tests — all in-proc, no spawned binaries.
- `smoke-support/` = the ONE-SEAM helper lib (`test_domain`/`test_node`/`test_pair`/`wait_until`,
  `require_smoke!`); depends on hive-core + hive-embed ONLY — never hive-node or hive-sync; consumed as a
  dev-dependency (dev-dep cycles are permitted by cargo where needed).
- `smoke/` = hive-smoke, the cross-component scenario crate: `#![cfg(unix)]`, every test opens
  with `require_smoke!()`. Spawns the node BINARY and holds device Stores in-proc for multi-party
  scenarios.

**Cross-crate binary paths** (critique fix — the app-boot round-trip needs two real binaries and
`CARGO_BIN_EXE` cannot provide both anywhere): single-binary smoke lives in the owning crate's
`tests/`; multi-binary scenarios live in `smoke/` and read **absolute binary paths from env** —
`HIVE_APP_BIN`, `HIVE_BRIDGE_BIN`, `HIVE_NODE_BIN` — exported by the CI job (or
`scripts/smoke.sh`) after `cargo build -p hive-app -p hive-bridge -p hive-node`. A test whose
required path env is unset skips loudly (LOUD-SKIP).

**Harness rules** (mandatory, cited in review):

- Always bind `127.0.0.1:0`; the node prints a machine-readable `listening on 127.0.0.1:<port>`
  line as part of its CLI contract; tests parse it (parallel-safe, no fixed ports ever).
- One `wait_until(cond, deadline=10s, poll=50ms)` helper — **no bare sleeps anywhere** in the
  tier. 60s hard cap per test.
- Keys via KEY-INJECT only: tempdir + raw-hex master file (§0.4).
- Child stdout/stderr teed into the test log; kill-on-drop guards on every spawned process.

### 4.2 The three core smoke scenarios

1. **Node boot + connect** (`node/tests/boot.rs`): spawn the real
   `hive-node --listen 127.0.0.1:0 --root <tempdir> --master-key-file <hex>`; parse the listening
   line (deadline 10s); connect and complete a sync hello with a pinned throwaway cert; health
   probe; SIGTERM → clean exit; reboot on the same root re-opens and serves the same heads. Blind
   variant: boot with no key file, hello succeeds, tier reported blind.
2. **Two-device convergence through a node** (`smoke/`): `test_pair()` — tempdir domain, file
   KeySource, the node binary on `:0`. Device A `journal_append` → sync to node; device B syncs from
   node; `wait_until` canonical dumps are byte-equal and per-device watermarks match (the bounded
   convergence wait — never an unbounded loop). Then concurrent edits in both directions → the
   LWW outcome asserted per the CONVERGE-ORACLE matrix. Blind-tier variant: node holds no key,
   still relays verbatim segments, devices converge identically.
3. **Bridge round-trip** (local app socket — the bridge's only v2.1 target): the merged PR #129
   suite is the base (in-test UDS host + real `hive-bridge` binary: initialize, tools/list,
   tools/call, one-shot mode, failure marker). v2.1 adds the **app-boot round-trip** (runs in the
   `ui-shots` job because it needs a display): spawn the real `hive-app` under xvfb with
   `HIVE_DATA_DIR=<tempdir>`, `HIVE_MASTER_KEY_FILE`, `HIVE_MAIL_ENABLED=0`, `HIVE_EMBED=hash`;
   `wait_until` `bridge.sock` exists; run `hive-bridge call journal_append` then `recall`; assert
   results; kill the app. This closes the explicitly-untested app side — peer-cred check and
   socket bind (`bridge/tests/stdio.rs:64-67`).

   Deliberate rejection recorded here: **no bridge→node MCP smoke.** No node MCP endpoint exists
   in v2.1 scope; the bridge stays a local UDS pump (thin-bridge law). If agent access to the node
   is ever wanted, that is a scoped DIRECTION amendment with its own tests, not a test-plan side
   effect.

### 4.3 Security-protocol suites (PR 4.5/4.7 acceptance criteria; extended at 4.14)

The mTLS/enrollment path is the first network-facing security surface in the project's history and
ships **with protocol-level tests, not just primitive vectors** (critique fix — previously only
RFC vectors were planned):

- **`sync/tests/tls_loopback.rs`**: throwaway self-signed Ed25519 device certs minted in-test
  (rcgen, or the enrollment codepath itself once it exists) over localhost TCP or an in-memory
  duplex. Cases: pinned-SPKI accept; unknown cert reject; **revoked-after-fold reject** (fold a
  `device.revoke`, assert the next connection with that cert is refused); wrong-domain session
  scoping (a session authenticated for domain A cannot address domain B). This suite is the
  required artifact of the rustls raw-public-key-vs-cert-carrier spike (PR 4.5).
- **Enrollment smoke** (`smoke/`): the real node CLI mints a one-time code; an in-proc client
  redeems it — happy path (device.add folded, pubkey pinned, master-wrap blob relayed opaquely);
  expired code refused; **reused code refused**; wrong code refused; code TTL enforced.

### 4.4 ADVERSARIAL-SMOKE scenario ledger

Each maps to a THREAT-MODEL amendment (§2 convention). Initial set — grows with each amendment:

| Scenario | Asserts | Lands with |
|---|---|---|
| Revoked-cert reconnect | Connection refused after device.revoke folds | 4.7/4.12 |
| Stale control-statement replay | Node persists per-domain high-water mark; a regressed statement set is refused; revoke tombstones permanent | 4.7/4.12 |
| Pairing-code abuse | wrong/expired/reused all refused (§4.3) | 4.7 |
| Hostile segment ingest | AEAD-garbage frame mid-tail → reject + resync from last good offset; store never bricks (contrast today's heal-brick, §7.3) | 4.10 |
| SegmentVault write-once | Differing re-upload of an existing sealed `(device, start_seq)` → integrity alarm, never overwrite; tail extension must preserve the existing byte prefix | 4.6 |
| Backdated-lc arbitration | Backdated lc cannot slip records under a revoke (the cutoff is hub receipt order, not record clocks); LWW under hostile lc stays deterministic per the pinned corpus (lc untrusted and unclamped — the D29 rejection) | 4.11 goldens + 4.13 smoke |
| Quota at ingest | Per-domain byte quota enforced at replication ingest, over-quota push refused cleanly | 4.9 |
| Kill-mid-Forget | Node killed mid-Forget queue drain; restart → Forget re-sent, acked, effect exactly once | 4.9 |
| Three-party shred | A shreds a blob → blind node drops ciphertext blocks via Forget → B folds the tombstone → per-replica crypto_shred assertions (blocks gone, wrapped keys gone, replay does not resurrect) on all three parties | 4.13 |
| Revoked-grant serve | Node refuses block fetches for a revoked grant; fetches scoped to (recipient, grant_id, id ∈ grant block set) — never bare-id oracle | 6.3 |
| Fault-injection resume | kill-mid-segment and kill-mid-blob transfer → resume from landed offset/have-set, no re-transfer of verified bytes | 4.4/4.8 |
| Impostor discovery candidate | Spoofed SRV/mDNS answer points at a wrong-key listener → handshake fails, dialer skips to the pinned node, sync completes (DNS/mDNS are addressing, never authentication — delta 13); hostile/garbage DNS answers hit the candidate builder's HOSTILE-DECODE suite, Err never panic | 4.8 |
| DNS-publisher containment | Publisher upserts only `_hive._tcp.<zone>` + the node's own A/AAAA against the mock endpoint; any write outside that name set fails the test (delta 13's scoped-credential claim, CI-checkable half; live token scoping is a 4.20 deploy-audit item) | 4.6 |

### 4.5 Local ergonomics

One command: `scripts/smoke.sh` — builds the needed binaries, exports `HIVE_*_BIN`, then runs
`HIVE_SMOKE=1 HIVE_EMBED=hash cargo test -p hive-node -p hive-sync -p hive-smoke -p hive-bridge`.
Plain `cargo test --workspace` remains green offline and skips the tier loudly.

---

## 5. Screenshot layer

### 5.1 Decision

**Primary: the real desktop binary (WebKitGTK) under xvfb inside a digest-pinned container,
external capture, odiff compare.** Not Playwright-on-dioxus-web first, for two hard reasons:
(a) the web head does not exist yet — it needs the crate split and a wasm data seam, mid-plan work
— and (b) Chromium is the wrong renderer: the 0.7.9 regression the repo already ate was
*WebKitGTK-only blank rendering* (`app/Cargo.toml:13-16`, verified by manual screenshot A/B) —
exactly the class a Chromium harness cannot catch. The shipping artifact is the WebKitGTK app;
test that.

When the web head lands (W3), Playwright+Chromium screenshots arrive **as that PR's acceptance
suite**, reusing the same fixture seeder and manifest names — and pinned the same way from the
first commit: browser locked inside a digest-pinned container (or the Playwright-bundled browser
at a locked version), web goldens keyed by that image/browser tag (critique fix — do not re-import
the drift problem the desktop tier just solved).

### 5.2 The deterministic sub-layer: dioxus-ssr DOM snapshots (insta)

Zero-flake, seconds-fast, and the regression net for **both** renderers once components are
shared:

- dioxus-ssr on the 0.6 line renders a pumped VirtualDom to an HTML string; snapshot per screen
  with fixture props/store via insta (version pinned in workspace deps so snapshot formatting
  never churns). Starts now for leaf components inside app's existing `#[cfg(test)]` mod; runs
  inside the existing `rust` job.
- A small pump helper lands with the first snapshot: build VirtualDom with contexts provided
  (store or the future HiveUi mock) → rebuild → `wait_for_suspense` → render, so `use_resource`
  screens produce settled markup.
- **W1 acceptance gate** (critique fix): *every portable screen renders under dioxus-ssr in a
  test*. That test IS the enforcement that no wry-touching component slips into the shared `ui`
  crate — consuming a desktop context (wry event handlers etc.) panics outside a desktop launch,
  so the constraint is mechanical, not a review nicety.
- Update flow mirrors GOLDEN-BYTES discipline: `cargo insta review` locally; CI never
  auto-accepts.

W1 (the pure code-motion crate split, PR 5.1) lands pinned by **DOM snapshots plus a v0 pixel set of five
shots** — not the full pixel harness — so the crate split is not serialized behind weeks of CI
plumbing (accepted simplicity-critique fix; §5.7).

### 5.3 Prerequisites (retrofits #0 and #1 — nothing renders without them)

- **#0 — master-key injection seam.** `app/src/main.rs` `boot()` (:211-221) unconditionally
  resolves `KeychainKeySource` before anything else; the shot container has no Secret Service
  provider (dbus-run-session provides a bus, not `org.freedesktop.secrets`), so as designed every
  shot renders the Boot::Failed screen — and even a container keyring would mint a key that cannot
  open the seeded fixture. Fix: `HIVE_MASTER_KEY_FILE` (64-hex, §0.4 format) honored before the
  keychain path — optionally compiled only under a `test-hooks` cargo feature that the `ui-shots`
  job builds with. The `seed-fixture` bin writes the same key file beside the fixture dir.
  Deliberate rejection: gnome-keyring-in-container (more moving parts, still needs exact-key
  injection).
- **#1 — `HIVE_DATA_DIR` override** in the app (`data_dir()` hard-codes XDG at :197-209; the
  bridge already established the env-override precedent).

### 5.4 Container spec (`ci/shots/Dockerfile` → `ghcr.io/.../hive-ci-shots:<date-tag>`, pinned by digest)

- apt-pinned WebKitGTK runtime **and the -dev build set** — the job builds hive-app inside the
  container: `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libxdo-dev`, `pkg-config`, `cmake`,
  `libssl-dev` (the same set ci.yml installs today) (critique fix — the original spec listed only
  runtime bits).
- xvfb, dbus, ImageMagick (`import`), xdotool (escape hatch + the one keyboard canary), the odiff
  static binary, Rust toolchain 1.94.1 preinstalled (bigger image, ~2 min faster jobs).
- **Fonts**: DejaVu only, plus a fontconfig drop-in — grayscale AA, `rgba=none`, fixed hintstyle
  (Linux webview text is fontconfig/FreeType-sensitive with documented WebKitGTK weight quirks).
- **GTK determinism baked in** (critique fix): a `settings.ini` with `gtk-cursor-blink=false` and
  `gtk-enable-animations=false` — otherwise the focused-compose-input shot nondeterministically
  captures caret-on vs caret-off.
- Image rebuilt only via a rare `workflow_dispatch` workflow; goldens are keyed by image tag, and
  an image bump regenerates goldens **in the same PR**.

### 5.5 Runtime environment

```
HIVE_DATA_DIR=<seeded fixture>   HIVE_MASTER_KEY_FILE=<fixture>/master.hex
HIVE_MAIL_ENABLED=0              HIVE_BRIDGE_SOCKET=0
HIVE_EMBED=hash                  HIVE_TEST_NOW=2026-01-15T12:00:00Z
WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1
LIBGL_ALWAYS_SOFTWARE=1
xvfb-run -s "-screen 0 1280x900x24" dbus-run-session -- <hive-app>
```

- **Both** WebKit vars (critique fix): `WEBKIT_DISABLE_COMPOSITING_MODE` is tied to the pre-Skia
  renderer and is likely a no-op on the ≥2.46 line; the documented blank-rendering workaround for
  containers/software-GL on current WebKitGTK is `WEBKIT_DISABLE_DMABUF_RENDERER=1`. Belt and
  braces across lines; llvmpipe software raster via xvfb.
- **Frozen clock**: `HIVE_TEST_NOW` is consumed by `store::now_iso` in the command layer only. The
  determinism grep fence is untouched by construction — the fenced dirs
  (`oplog`/`blockstore`/`fold`/`index`) forbid `std::env` and never read clocks; the env read
  lives outside the fence. The app's `today_ymd`/`current_hour` reach-ins route through the same
  source.
- Fixture data dir is written by a `seed-fixture` bin in `smoke/` using normal store APIs with
  fixed ts/content (nanoid ids never render as pixels; labels do).

### 5.6 Driving and capture protocol

- A ~150-line `HIVE_SHOT_TOUR=<manifest>` module in the app: each manifest step sets the **same
  nav/overlay signals the real onclick handlers set**, waits for resource settle, prints
  `SHOT-READY <name>` to stdout. Signal-driving over coordinate clicking — the 202 stable DOM ids
  remain the anchor vocabulary, but pixel-coordinate clicking is the flake this design exists to
  avoid.
- **Paint-settle wait** (critique fix): `SHOT-READY` is emitted at vdom/resource settle, not
  compositor paint — the harness waits a fixed 200ms (~two frames) after `SHOT-READY` before
  `import -window root shots/actual/<name>.png`, then writes a newline to the app's stdin to
  advance (lockstep). Retry-once (recapture after 500ms, logged loudly) remains a backstop, not
  the mechanism.
- One optional **keyboard-only xdotool canary** (type into the journal composer via key events,
  shot the result) as the single true-input-path test — keyboard avoids the coordinate flake that
  kills these suites. No coordinate-click tests.

### 5.7 Shot list

**v0 (lands with/before W1): five shots** — journal, mail list, calendar month, settings,
boot-failure. Grows per screen-touching PR toward ~15: onboarding steps, mail reader + compose
overlay, calendar week/day, contacts + card, tasks, identities, entity detail. Every new screen PR
adds its shot in the same PR.

### 5.8 Compare and golden management

- `odiff --antialiasing --threshold 0.1` (per-pixel color threshold) plus a global
  diff-pixel-ratio budget of 0.05%, plus per-shot ignore-regions declared in the manifest
  (including any focused-input caret zone).
- Goldens at `app/tests/shots/golden/<image-tag>/` in plain git (~150KB each; ~15 shots ≈ 3MB).
  **Only the current image tag is kept in-tree**; LFS only if the set ever exceeds ~20MB.
- **Per-platform goldens: no.** One platform — the pinned Linux/WebKitGTK container. macOS/Windows
  rendering is out of v2.1 scope (§5.10, §6.4). The web head later gets its own browser-tagged
  golden set under the same scheme.
- **Update workflow**: `scripts/shots.sh --update` runs the *same digest-pinned container* locally
  via podman — local pixels == CI pixels byte-for-byte; regenerated PNGs are reviewed in the PR
  (GitHub renders side-by-side image diffs). House rule, mirroring oplog.rs: **golden churn must
  be explained in the PR description — a pixel change is a UI decision, not a fixture refresh.**
  `--compare` and `--only <shot>` complete the interface.

### 5.9 Flake ledger (cause → mitigation)

| Risk | Mitigation |
|---|---|
| WebKitGTK version drift | Digest-pinned container; goldens keyed by image tag; bump = regen in same PR |
| Fonts/AA | Single font family + fontconfig drop-in; grayscale AA pinned |
| GPU/compositor variance | xvfb → llvmpipe; both WEBKIT_DISABLE_* vars; `LIBGL_ALWAYS_SOFTWARE=1`; odiff AA tolerance |
| Caret blink / GTK animations | `settings.ini` (cursor-blink off, animations off); SPIN_CSS disabled under tour mode; caret ignore-region |
| Clock-derived pixels | `HIVE_TEST_NOW` frozen clock |
| Async load timing | Settle protocol + fixed 200ms paint wait after SHOT-READY |
| Residual 1-frame races | Retry-once per shot (500ms), loudly logged; permitted while the job is advisory, revisited with measured data at promotion to required |

### 5.10 What screenshots will NOT catch (honest list)

- Interactions outside the tour: drag, hover states, scroll behavior, context menus.
- Native window chrome, the close-to-quit path (`process::exit` on CloseRequested), OS menu
  integration.
- The real first-run path: OS keychain onboarding is bypassed by the key seam by design (the one
  keychain test stays manual, `core/tests/keys.rs`).
- Mail sync and every network path (kill-switched off for determinism).
- Non-Linux rendering: macOS/Windows fonts, DPI, and webview differences — out of scope, no CI
  runners exercise them (§6.4).
- Real-GPU rendering differences (llvmpipe is the pinned rasterizer, not a user's driver).
- Timing/perf regressions, animation smoothness, accessibility-tree correctness.
- Anything below the fold of the fixed viewport, and any state the seeded fixture does not
  contain.
- Logic that renders identically while being wrong — that is the unit/DOM-snapshot layers' job.
- Web-head browser semantics (CSP, downloads, popups) until the W3 Playwright suite exists.

---

## 6. CI wiring

### 6.1 Jobs

Keep `rust` and `importer` exactly as they are (ci.yml:13-111). DOM snapshots ride inside `rust`
(they are ordinary `cargo test` targets). Add:

- **`smoke`** (ubuntu-latest): toolchain 1.94.1; Swatinem cache key `hive-rust-smoke`;
  `cargo build -p hive-node -p hive-bridge` then export `HIVE_*_BIN`;
  `HIVE_SMOKE=1 HIVE_EMBED=hash cargo test -p hive-node -p hive-sync -p hive-smoke -p hive-bridge`
  (crate-scoped ⇒ no webkit deps). Est. warm 4-6 min.
- **`wasm-gate`** (from 5.3 on): `cargo build --target wasm32-unknown-unknown -p hive-ui -p
  hive-web` with `default-features = false`. Est. 2-3 min. Companion change (critique fix):
  exclude `hive-web` from host workspace builds (workspace `default-members` or `--exclude
  hive-web` in the rust job) so the rust job never host-compiles the wasm-only dep tree and stays
  in its current envelope.
- **`ui-shots`** (`container: image: ghcr.io/...@sha256:...`, `options: --shm-size=1g` — GH
  container jobs default to Docker's 64MB `/dev/shm`, which WebKit will hit): cache key
  `hive-rust-ui`; `cargo build -p hive-app -p hive-bridge`; run `seed-fixture` (writes data dir +
  `master.hex`); `scripts/shots.sh --compare`; then the app-boot bridge round-trip
  (`HIVE_SMOKE=1 HIVE_SHOTS=1` with `HIVE_APP_BIN`/`HIVE_BRIDGE_BIN` set). On failure:
  `actions/upload-artifact` of `shots/actual/` + `shots/diff/`, 7-day retention. Est. warm 6-8
  min, cold 12-15 (app build dominates).
- **`nightly`** (schedule + dispatch): cargo-fuzz targets (bounded wall time per target); the perf
  canary — ANN scan latency asserted against a deliberately loose bound (≥5x the documented
  envelope in `core/src/index/ann.rs`; shared-runner variance swings 2-3x, so tight bounds are
  flake generators — the old vector_perf gate (core/tests/vector_perf.rs) was retired for exactly this class), measured latency
  uploaded as a trend artifact so drift is visible before the bound trips. The usearch/HNSW swap
  PR must show a before/after canary run on the same runner class in its description.
- **`shots-image`** (workflow_dispatch only): rebuild + push the pinned container from
  `ci/shots/Dockerfile`.

### 6.2 Gating policy (decided, not open)

Merge-gating from day one: `rust`, `importer`, `smoke`, `wasm-gate`, DOM snapshots. `ui-shots`:
**advisory for the first two weeks**, flake rate measured from the retry logs, then flipped to
required (and retry-amnesty policy revisited with that data). Nightly jobs never gate.

### 6.3 Budget

| Job | Warm | Cold |
|---|---|---|
| rust (existing) | ~5 min | ~13.5 min (observed) |
| importer (existing) | ~4 min | ~6 min |
| smoke (new) | ~4-6 min | ~10 min |
| wasm-gate (new) | ~2-3 min | ~6 min |
| ui-shots (new) | ~6-8 min | ~12-15 min |

All jobs parallelize; PR wall time stays bounded by the slowest job — ~8 min warm, ~13-15 min
cold, i.e. no worse than today's cold `rust` job.

### 6.4 Manual gates (CI-unreachable surfaces, named owners)

CI cannot reach these; they are explicit checklist items in the PR templates ("tested targets", in
the repo's existing convention of stating tested targets in the PR):

- **4.16**: real two-box enrollment + sync validation (laptop ↔ node).
- **4.20**: restore-from-snapshot boot of a node root; second-box enrollment end-to-end; backup
  guidance verified against a real restic/ZFS cycle.
- **5.3/5.4 (W3/W4)**: tailnet reachability — a **non-tailnet client must fail to connect** (the
  bind-validation half is a CI unit test, §3.5; reachability is manual).
- Live keychain round-trip stays the one `#[ignore]`d manual test.
- macOS/Windows (keychain backends, bundled SQLCipher, named pipes) remain out of v2.1 CI scope —
  confirmed, and stated here so nobody assumes otherwise.

---

## 7. Retrofit gap list (existing code, ordered by value)

0. **`HIVE_MASTER_KEY_FILE` seam in app `boot()` + `FileKeySource` raw-hex mode — one shared
   format (§0.4).** Prerequisite zero: without it the entire pixel tier and the app-boot smoke
   render Boot::Failed (§5.3). Small, safe (env-gated / feature-gated), unblocks everything.
1. **`HIVE_DATA_DIR` override in the app** (`app/src/main.rs:197-209` hard-codes XDG; bridge
   precedent exists). Required by every app-level test.
2. **Merge PR #129, then land the app-boot bridge smoke** — the app side (peer-cred check, socket
   bind, startup resolution) is explicitly untested today (`bridge/tests/stdio.rs:64-67`), and the
   smoke tier's harness idioms live in that PR. The app-boot half rides the PR 5.0 display rig
   (it needs the shot container).
3. **Heal-brick regression pins**: foreign log with a UNIQUE-slug collision or
   update-before-create currently fails `open_core` → app boots to Boot::Failed
   (`app/src/main.rs:234-241`). Pin today's bricking behavior in tests now, then drive the 4.10/4.11
   quarantine-not-brick fix against those tests.
4. **Broadcast bus tests**: `subscribe()` has zero callers and zero tests
   (`core/src/store/mod.rs:209-238`). Add commit→event tests now — replication liveness and
   web-head invalidation will both sit on this channel.
5. **Frozen-clock env (`HIVE_TEST_NOW`) for `now_iso`/`today_ymd`** + the shot-tour module — the
   two app-side testability hooks the pixel tier needs (§5.5, §5.6).
6. **HOSTILE-DECODE proptests on `Record::from_cbor_bytes`** — zero adversarial-input coverage
   today on a format about to face the network.
7. **Land the revised GREP-FENCE token lists green** against the existing tree (the naive lists
   trip on `cc_credentials.rs:53` "oauth_token" and three comments — §2); add the cargo-metadata
   dependency assertion alongside.
8. **Pin the mail-FTS hole**: mail FTS does not rebuild from replay
   (`core/src/store/mail.rs:1-14`) — a documenting `#[ignore]`d/failing-by-design test so the
   replication work confronts it instead of rediscovering it.
9. **Refresh `dev-setup.sh`** (still claims a Postgres store pre-1.6 and spins `hive-pg` from the
   SessionStart hook) to include the smoke tier; fix the stale "sync daemon is PAUSED" header
   (`core/src/store/mail.rs:4-5`) in the same pass.
10. **Perf canary** (nightly, §6.1) — replaces the dead vector_perf gate before node-scale mail embeddings (PR 4.19)
    (~200k chunks, `core/src/store/semantic.rs:76`) make ANN latency matter.

---

## 8. Critique disposition

### 8.1 Blocker/major fixes incorporated

| Finding (lens) | Disposition |
|---|---|
| App cannot boot in the shot container — keychain hard-required (CI blocker) | Fixed: `HIVE_MASTER_KEY_FILE` seam as retrofit #0; seed-fixture writes the key file; `test-hooks` feature option (§5.3, §7.0) |
| Identity/tenancy grep fences red on day one (CI major) | Fixed: namespace/dependency tokens + cargo-metadata assertion; identifier-based tenancy fence; land-green requirement (§2 GREP-FENCE, §7.7) |
| Control-record placement conflict across designs (CI + simplicity) | Fixed: plan written for the control-log home with ctlkind fixtures + compat test replacing the untestable epoch gate; §3.6 carries the kind-widening fallback with its one CI-checkable test |
| Control segments silently reusing the frozen segment-key derivation (simplicity blocker) | Fixed: new derivation context with its own goldens + differs-from-data-derivation test (§0.2, §3.1) |
| mTLS/enrollment path had zero protocol tests (CI + privacy) | Fixed: tls_loopback suite + enrollment smoke as 4.5/4.7 acceptance criteria; the 4.5 spike delivers the harness (§4.3) |
| Shot-container flake holes: DMABUF var, caret blink, paint race, missing -dev packages, /dev/shm (CI major) | Fixed: both WEBKIT vars, GTK settings.ini, 200ms paint-settle wait, -dev package list, `--shm-size=1g` (§5.4-5.6, §6.1) |
| App-boot round-trip impossible under the CARGO_BIN_EXE rule (CI major) | Fixed: `HIVE_APP_BIN`/`HIVE_BRIDGE_BIN`/`HIVE_NODE_BIN` env mechanism, exported by job/script; single-binary tests stay in owning crates (§4.1) |
| `test_domain()` vs `FileKeySource` file-format mismatch (CI minor, promoted) | Fixed: one raw-hex format across all four consumers (§0.4) |
| No adversarial protocol scenarios / amendments untested (privacy) | Fixed: ADVERSARIAL-SMOKE convention with 1:1 amendment mapping; ledger in §4.4 gates the v0.9.0 close (4.20) |
| Backdated-lc trust in FOLD_VERSION-4 goldens (privacy) | Fixed: backdated-lc corpus in CONVERGE-ORACLE (§2, §3.2) |
| SegmentVault write-once / equivocation untested (privacy) | Fixed: write-once + tail-prefix + poison-divergence tests (§3.2, §4.4) |
| Forget-queue / multi-replica shred untested (CI minor, promoted) | Fixed: unit persistence/dedupe + kill-mid-Forget + three-party shred smoke (§3.2, §4.4) |
| dioxus-ssr constraints unstated for W1 (CI minor) | Fixed: every-portable-screen-renders-under-ssr as the W1 acceptance gate; pump helper; insta pinned (§5.2) |
| Screenshot harness serializing W1 (simplicity) | Fixed: W1 pinned by DOM snapshots + a five-shot v0; full list grows per PR (§5.2, §5.7) |
| Manual-only surfaces unowned (CI minor) | Fixed: §6.4 manual-gate checklists + the CI-testable bind-validation unit (§3.5) |
| Web screenshots inheriting browser drift (CI minor) | Fixed: W3 suite pinned in a container with tagged goldens from day one (§5.1) |
| Perf canary as a merge-gate flake (CI minor) | Fixed: nightly-only, ≥5x loose bound, trend artifact, usearch PR before/after rule (§6.1) |
| Sign-then-golden malleability trap (privacy minor, format-adjacent) | Fixed: sign-exact-transmitted-bytes construction + HOSTILE-DECODE before the first ctl golden (§3.1) |

### 8.2 Deliberate rejections (with reasons)

1. **cargo-nextest for tiering** — the loud-skip env idiom is repo precedent, zero new tooling,
   and keeps `cargo test --workspace` the one true command.
2. **Playwright/Chromium as the primary screenshot tier** — wrong renderer for the shipping
   artifact; the known regression class is WebKitGTK-specific. Web screenshots arrive with W3,
   pinned (§5.1).
3. **gnome-keyring inside the shot container** — strictly worse than the key-file seam: more
   moving parts, and it still requires injecting the fixture's exact key (§5.3).
4. **Per-frame byte goldens for the sync wire during v2.1** — golden discipline is reserved for
   frozen on-disk artifacts; a churning negotiated wire gets round-trip + HOSTILE-DECODE, with
   compat fixtures at the first tagged proto release (§3.4). Diluting "frozen" would cost more
   than it protects.
5. **Bridge→node MCP smoke** — no such endpoint exists in v2.1 scope; the bridge stays a local UDS
   pump under the thin-bridge law. Testing must not quietly create API surface (§4.2).
6. **Coordinate-based xdotool driving** — signal-driving is the interaction layer; one
   keyboard-only xdotool canary covers the true input path without coordinate flake (§5.6).
7. **ui-shots merge-gating from day one** — advisory for two weeks with flake measured from retry
   logs, then required; retry-once amnesty is re-decided with that data (§6.2).
8. **Windows/macOS test coverage in v2.1** — no runners, no named-pipe transport, keychain
   backends untestable headless; explicitly out of scope and listed as manual/deferred (§6.4).
