# Hive v2.1: always-on node, web head, sharing — execution plan

Status: adopted 2026-07-24. Companion to [DIRECTION.md](./DIRECTION.md)
(D16-D28; v2.1 amends D16/D21/D27 at PR 4.0 below, D17/D18/D19/D20 stay untouched)
and successor to [PLAN.md](./PLAN.md)'s forward half: Phase 1 is complete, Phase 2
is at PR 2.4 (open as PR #129), the old Phase 4 (iroh sync + relay) is **replaced**
by the node program below, and Phase 3 / the browser extension carry with stated
deltas. Grounded in a five-track repo recon (storage core, store/command layer,
app/UI, bridge, ingestion, tests/CI) and a three-lens critique (privacy/threat-model,
simplicity/phasing, CI realism); every blocker and major critique finding is
incorporated, and deliberate rejections are listed near the end.

Target: `hive-node`, a headless always-on peer embedding the same hive-core the
desktop embeds — hub-and-spoke replication over mTLS TLS-1.3/TCP (iroh demoted to
optional later transport), blind (ciphertext-only) and trusted (master-holding)
domain tiers laid out `tenants/<t>/domains/<d>/` where each leaf is verbatim a
store data_dir; the replication protocol IS the device API; MCP stays the agent
API; no REST-for-humans. Then a Dioxus web head sharing the desktop's RSX
components (CSR-only, trusted-node, tailnet-authed), and per-recipient blob
sharing via signed control-log grants. Multi-tenancy by partition; hardening
deferred to a stated trigger. Frozen formats stay frozen: every new record kind,
derivation context, and signed payload is an explicit format decision with golden
fixtures, and the identity bright line — no sessions, tokens, or OIDC types below
the node binary — is enforced by grep fence, not convention.

## Where the repo actually stands

- Phase 1 shipped whole (op log, blockstore, keys, fold, SQLite cutover, importer,
  interim bridge). Phase 2 is nearly closed: the app is daily-drivable, PR 2.4
  (bridge proxy) is open as PR #129, PR 2.5 (packaging) is still ahead.
- Reality ran ahead of the old Phase 3 in two places: the mail engine came back
  early as core code (jmap-sync driven by `Store::mail_sync_tick`, Slice A driver
  live in-app — the "sync daemon is PAUSED" header in store/mail.rs is stale), and
  the mail/calendar/contacts/tasks UI shipped inside the Phase 2 app. What remains
  of Phase 3 is the wasmtime module host, filesystem module, entity upgrades,
  dreaming.
- Nothing sync-shaped exists: no network code in core, no asymmetric crypto in the
  tree, no ingest path for foreign records, lc is written but never read, the
  broadcast bus has zero subscribers, and a foreign log with a slug collision
  bricks store open. The node program builds exactly this.

## Ground rules

- All work lands on `main` via small PRs (single main branch; development/release
  are retired). **PR #129 merges before any other v2.1 PR that touches app/ or
  bridge/** — the smoke tier builds on its harness and W1 would conflict with it
  destructively.
- **Section order below is execution order.** Phase numbers are stable identifiers
  from the v2 plan (PR ids 3.x keep their meaning); the node program takes the
  Phase 4 slot and deliberately runs BEFORE Phase 3 — v2.1's priority is the node,
  the two share no surface except the C1 scheduler seam (PR 4.15), and old PR 3.1
  gets to embed into that seam instead of inventing it.
- Version targets, tagged in execution order: **v0.7.0** = Phase 2 close;
  **v0.8.0** = Phase 4 Stage A (milestone M1, the blind backup node); **v0.9.0** =
  Phase 4 Stage B (trusted node); **v0.10.0** = Phase 5 (web head); **v0.11.0** =
  Phase 6 (sharing); **v0.12.0** = Phase 3 (modules + dreaming); **v0.13.0** =
  Phase 7 (extension). Workspace version bumps in each phase-opening PR.
- No planned outages this time. The desktop stays fully local-first and
  daily-drivable at every merge; a down node degrades sync only. One stated
  non-replication: vault credentials are runtime state — a restored or new replica
  re-enters mail passwords (amended for headless at PR 4.18).
- Format discipline: the envelope, blockstore derivations, wrap format, and the
  data-log kind set do not change. v2.1's new records live in a **dedicated
  control log** (decided at PR 4.0) so the 11 data-log goldens stay untouched and
  un-upgraded devices never scan an unknown kind (reader.rs:206-208 hard-fails at
  scan AND heal — this was the fleet-brick hazard).
- Scope cut, stated once: **v2.1 ships personal custody only.** Zero OIDC code
  anywhere (the identity grep fence lands with an empty allowlist), enrollment is
  node-CLI one-time codes, web auth is tailnet + whois. Escrow/KMS custody,
  member.* kinds, the OIDC console, and enterprise offboarding are design-on-paper
  in the DIRECTION amendment, built at the tenant-2 trigger (below). Maggie's
  domain runs personal custody.
- Testing is first-class: the named conventions (GOLDEN-BYTES, ONE-SEAM,
  GREP-FENCE, HOSTILE-DECODE, CONVERGE-ORACLE, ADVERSARIAL-SMOKE) are defined in
  Cross-cutting; every PR description cites the conventions it satisfies. Tier
  gates copy the importer's loud-skip idiom: `require_smoke!()` behind
  `HIVE_SMOKE=1`, screenshots behind `HIVE_SHOTS=1`; `cargo test --workspace`
  stays green offline with zero new tooling.
- Each phase gate is a named demo before the next phase starts; real-device/manual
  gates are stated per repo convention ("tested targets" in the PR description).

## Phase 2 close (v0.7.0)

- **PR 2.4 bridge proxy mode (= PR #129, merge as-is).** Already reviewed scope:
  UDS JSON-RPC server in the app, bridge flips to proxy-only, plugin + .mcpb
  repoint, the repo's first real-binary smoke suite (bridge/tests/stdio.rs).
  Gate: merged before any other app/bridge-touching PR opens.
- **PR 2.5 packaging + release (carries from the old plan).** Flatpak first, then
  AppImage/msi/dmg; release.yml around app bundles + mcpb; identifier-free version
  check; WebKitGTK pass explicitly first; tag v0.7.0. Branding decision needed
  before the tag. Independent of the node track: PRs 4.0-4.2 may open the moment
  #129 merges — nothing in Phase 4 waits on packaging.

Gate: v0.7.0 tagged; Claude Desktop + Claude Code connect through the proxy
bridge; app daily-drivable.

## Phase 4: the node program (replaces old Phase 4; executes next)

Design stance. The server is a peer you never turn off: hive-node embeds hive-core
exactly as the app does (`Store::new` is proven headless), and hub-and-spoke with a
stable URL deletes NAT traversal — so transport is tokio-rustls mTLS over TCP, not
iroh, not QUIC (the framing layer stays stream-generic so those slot under later
with no wire change). **Verbatim sealed-segment bytes are the only transfer
primitive** — segment boundaries feed key derivation (segment.rs:38-45) and
rotation is writer policy, so re-encoding is unsafe and byte-identical transfer is
what preserves the determinism contract; the active tail is just a segment whose
length grows, landed whole-frames-only from a resumable offset. The blind tier is
first because one-way backup needs none of the merge surgery: the pushing device
never ingests foreign records, restore is verbatim-files-then-heal (today's only
path, formalized), and the vault folds nothing. Merge semantics (FOLD_VERSION 4)
gate bidirectional sync only. Two structural decisions front-load in PR 4.0 so no
later PR relitigates them: control records live in a dedicated control log with a
NEW segment-key derivation context (reusing the data-log derivation across roots
would mint identical keys at equal (device, start_seq) — an undeclared weakening
of a frozen invariant), and control-record signatures cover exact transmitted
bytes (body as CBOR bstr; verifiers never re-encode). hive-node is **lib + thin
main** — the smoke tier depends on that shape: in-proc node for multi-party
scenarios, `CARGO_BIN_EXE` spawn only in node/tests/.

### Stage A — the blind backup node (v0.8.0 at M1)

- **PR 4.0 v2.1 decision record (doc-only).** Amend DIRECTION (D29+ numbered on
  merge): D16 partition-tenancy amendment, D21 rewritten (hub-and-spoke, TLS-TCP,
  iroh optional-later, **hub-only boundary**: devices talk exclusively to their
  node — no device-to-device or device-to-foreign-node paths until a future
  decision record reopens them), D27 custody-tier amendment. Record the format
  decisions: control-log home at `<data_dir>/control/<device>/*.seg` (same frozen
  envelope v1, same segment file format, **new** `hive-ctl-segment-key-v1`
  derivation context, additive); initial closed set `ctlkind::ALL` =
  {domain.genesis, device.add, device.revoke, custody.grant} with share.* widening
  reserved for Phase 6 and node.* reserved for multi-node (D36, paper); signature construction (Ed25519 over "hive-ctl-v1" ‖ the
  transmitted body bstr bytes; domain_id + monotonic control epoch +
  prev-statement-hash in every body); authority key = blake3-derived from master
  (anyone holding master IS the authority — stated plainly; rotation deferred but
  ROADMAPPED as the post-v2.1 remediation for revoked-master-holders, not silently
  dropped); verbatim-segment transfer; full-log bootstrap, no compaction (D18
  arithmetic stands); rotation threshold stays writer policy; personal-custody
  scope cut; the D35 address grammar (`<localpart>@<zone>`, canonical address
  recorded in domain.genesis; "address"/"zone"/"domain" terminology fixed) and
  the `_hive._tcp.<zone>` SRV discovery shape (DNS is addressing, never
  authentication); the threat-model amendment worklist (landed at 4.20/5.4/6.4).
  Gate: merged doc = the citable source for every format-touching PR below.
- **PR 4.1 test seams retrofit (app + core).** `HIVE_DATA_DIR` override for the
  app (bridge precedent), **`HIVE_MASTER_KEY_FILE`** (64-hex-char raw master,
  honored before the keychain — the critique's blocker: without it the shot
  container and app-boot smoke render only Boot::Failed; same file format the
  smoke helpers and FileKeySource test mode use), `HIVE_TEST_NOW` frozen clock in
  `now_iso` (command layer only; the determinism-fenced dirs never read clocks, so
  the grep gate is untouched), mail.rs stale-header fix, dev-setup.sh rewrite
  (post-cutover reality + smoke tier). Tests: first broadcast-bus commit→event
  unit tests (the bus has zero subscribers and zero tests today); heal-brick
  regression pins (foreign log with UNIQUE-slug collision and update-before-create
  fail open_core — pin today's behavior so 4.11 must consciously change it); a
  documenting `#[ignore]` test pinning that mail FTS does not rebuild from replay
  (un-ignored at 4.10); HOSTILE-DECODE proptests over `Record::from_cbor_bytes`
  (Err-never-panic, length caps). Gate: workspace green; boot() seams covered by
  unit tests.
- **PR 4.2 smoke tier.** New `smoke-support` crate (ONE-SEAM extensions:
  `test_domain()` tempdir + raw-hex master file, `wait_until(cond, 10s, 50ms)`,
  `require_smoke!()`; `test_node()`/`test_pair()` grow in later PRs) and
  `hive-smoke` crate (`#![cfg(unix)]`, all tests behind `HIVE_SMOKE=1`); AGENTS.md
  clause extended: no test constructs domains/nodes outside these seams. CI job
  `smoke` (ubuntu-latest, crate-scoped, no webkit deps, cache key
  hive-rust-smoke), merge-gating from day one. Harness rules codified: bind `:0`
  and parse the port, no bare sleeps, 60s hard cap, child output teed,
  kill-on-drop. Tests: scenario 1 = dir-copy restore heals clean (two stores, one
  copied dir — pins the files+heal path the restore CLI formalizes); the bridge
  binary suite from #129 continues to run against the LOCAL app only (no node MCP
  endpoint exists or ships in v2.1). Gate: smoke job green and required.
- **PR 4.3 core sync read surface (no wire).** Public API: sealed-segment
  enumeration `(device, start_seq, len, sealed, blake3-of-file)` (list_segments is
  pub(crate) today), **keyless header parse** that stops before key-unwrap
  (magic/version/device/start_seq precede the wrapped key) + keyless frame walker
  (lengths and frame hashes are plaintext — tests already prove it by hand), a
  serializable heads snapshot type, public blockstore `has_block/get_block/
  put_block` by bare 32-byte ciphertext id with blake3-verify-on-write (the
  private read_block/write_block are 90% of it; `has()` probes only the manifest —
  "blob complete" stays a keyed-side judgment tracked per block), all exposed
  through Store's async surface so the app's push loop can call them. Tests: unit
  parity keyless-walker vs keyed-reader over test_store logs; HOSTILE-DECODE
  proptests over the keyless parser (network-facing decoder #1). Gate: no
  behavior change for existing paths; new surface fully covered.
- **PR 4.4 hive-sync protocol crate.** New `sync/` crate (lib + `hive-sync` bin
  stub): length-prefixed CBOR control frames + raw payload streams, generic over
  AsyncRead+AsyncWrite (the serve_bridge_connection discipline); session state
  machine scoped to one domain; heads exchange, WantSegment{device, start_seq,
  from_offset} → verbatim bytes, growing-tail extension (receiver lands whole
  frames only, re-requests from landed offset), HaveBlocks/WantBlocks/PutBlock by
  bare id. Tests: loopback two-dir sync (push-only), kill-mid-segment and
  kill-mid-blob resume, tail-extension-with-prefix-intact, encode/decode
  round-trips + HOSTILE-DECODE proptests over every frame. **Deliberate
  deviation, per critique:** wire frames get proptests + round-trips during churn,
  NOT byte goldens — byte goldens are reserved for on-disk artifacts; proto-v1
  compat fixtures are cut once, at the v0.8.0 tag. Gate: loopback sync + resume
  suite green.
- **PR 4.5 device transport identity (crypto foundation).** The ONE asymmetric
  crypto introduction (identity, control signing, and sharing all reuse it):
  ed25519 + x25519 dependency choice (dalek suite vs ring/aws-lc-rs) and rustls
  raw-public-key vs self-signed-cert carrier decided by a **timeboxed spike inside
  this PR** (extism-precedent); per-device keypair generated on-device, private
  halves in the OS keychain (keyring pattern, resolved before tokio — the
  Secret-Service-panics-on-runtime-threads ordering). Tests: GOLDEN-BYTES = RFC
  8032/7748 vectors imported as fixtures; fixed-seed keypair determinism; the
  `tls_loopback` harness (throwaway certs over localhost/duplex): pinned-SPKI
  accept, unknown-cert reject — this harness is the spike's deliverable and every
  later transport PR's substrate. Gate: vectors + loopback green; stack decision
  recorded in the PR.
- **PR 4.6 hive-node blind vault.** New `node/` crate, **lib + thin main**;
  `node.toml` minimal (listen addr, node key, optional `[dns]` publisher block:
  `provider = rfc2136 | cloudflare | none`, upserting the `_hive._tcp.<zone>` SRV
  plus per-interface-class A/AAAA — public, tailnet, ZeroTier, LAN — on start and
  address change, credential scoped to exactly those names per D35/delta 13);
  per-tenant `tenant.toml` carries
  name, tier, and quotas only — no IdP/KMS/console fields in v2.1, per D33;
  on-disk `tenants/<t>/domains/<d>/` where a blind
  domain is a `SegmentVault`, not a Store: raw `log/<device>/*.seg` +
  `blocks/<hh>/<id>` in exact store shape plus `node-meta.db` (plain SQLite:
  lengths, hashes, sizes, pinned pubkeys, control epochs, pending-forget queue).
  **Write-once invariant:** a differing re-upload of an existing sealed
  (device, start_seq) is an integrity ALARM and a refusal, never an overwrite; a
  growing tail may only extend with its existing byte prefix intact
  (vault-verified). GREP-FENCE lands here with the node crate's birth: identity
  fence (dependency/namespace tokens — `openidconnect::`, `jsonwebtoken`,
  `use oauth2` — plus a cargo-metadata assertion walking workspace crates, NOT
  bare-word grep: cc_credentials.rs legitimately contains "oauth_token") and
  tenancy fence (identifiers `tenant_id`/`Tenant`/`tenants/` under core/, not the
  English word). Tests: node boot smoke via CARGO_BIN_EXE in node/tests/ (spawn
  `--listen 127.0.0.1:0 --root tempdir`, parse the listening line, health, SIGTERM
  clean exit, reboot-reopens); vault write-once and tail-prefix kill-tests; DNS
  publisher idempotent-upsert + record-shape unit tests against a mock
  RFC 2136/API endpoint (no live DNS in CI). Gate:
  boot smoke + fences green and required.
- **PR 4.7 listener + enrollment v1.** rustls mTLS listener from 4.5's decision;
  `hive-node enroll` mints one-time codes (hashed at rest, 10-min TTL,
  single-use); v1 enrolls the domain's first, already-master-holding device — no
  master-wrap crosses the wire, so node-solo redemption here does not touch
  D30's SAS rule (the second-device ceremony is 4.14); client redemption
  presents {device_id, ed25519_pk, x25519_pk},
  node pins the SPKI into node-meta with a **monotonic per-domain control epoch**
  that never regresses; `hive-node device revoke` unpins + bumps the epoch
  (revocations are permanently retained tombstones in node-meta). Auth-failure
  audit logging, accept-loop rate limiting, connection caps. Tests
  (ADVERSARIAL-SMOKE begins): enrollment happy path, expired code, reused code,
  wrong code; revoked-cert reconnect refused; stale-epoch regression refused;
  listener bind-config validation unit test (refuses 0.0.0.0 when configured
  interface-scoped). Gate: enrollment + revocation smokes green; every
  transport-auth claim in the threat-model worklist maps to a test here or is
  written down as untestable-because-X.
- **PR 4.8 desktop push + restore.** App side: address-first enrollment settings
  panel — `nate@<zone>` resolves via `_hive._tcp.<zone>` SRV and same-LAN mDNS
  into a multi-space candidate set (public / tailnet / ZeroTier / LAN),
  happy-eyeballs dial ordered by SRV priority, pinned-key mTLS decides, last-good
  path cached per network, raw `hive://host:port` kept as the resolver-less
  fallback (D35) — and a background push loop beside the mail driver (read-only
  against 4.3's surface — segments and referenced blocks stream up;
  tombstone/redact records replicate as ordinary records). CLI side: `hive-sync restore --node --into
  <empty-dir>` — verbatim files down, then the store's own heal folds everything
  at next open; **zero core write API needed** (the app is closed, the flock is
  free). Tests: resolver unit tests over fixture DNS answers (multi-space
  candidates, priority ordering; garbage answers are HOSTILE-DECODE territory for
  the candidate builder); impostor-candidate smoke — dialer skips a wrong-key
  listener and lands on the pinned one; smoke = push a seeded domain,
  kill-mid-segment, resume, restore
  into a fresh dir, open a Store over it, canonical dump equals the source;
  restore honesty assertions (mail rows present but mail-FTS empty until 4.10 —
  cites the 4.1 pin; embeddings recompute per replica). Gate: full
  backup/restore loop green in CI (in-proc node), manual second-box restore
  reserved for the M1 demo.
- **PR 4.9 quotas + shred propagation (Forget).** Per-domain byte quota enforced
  at replication ingest (soft/warn for tenant 1); blind-cache **Forget{block_ids}**
  from the keyed pusher (enumerated before the manifest key is destroyed), queued
  persistently in node-meta, re-sent until acked, ack-deduped; no denylist —
  convergent encryption legitimately recreates ids on re-add. Tests: smoke =
  device shreds a blob → node's ciphertext blocks verifiably gone (crypto_shred
  assertions run against the vault); kill-node-mid-Forget → restart → re-sent and
  acked exactly once; quota-exceeded refusal. Gate: shred-on-device is
  shred-on-node, provably.

**Milestone M1 — the household backup (tag v0.8.0).** The first useful
increment, shipped before the full vision: the laptop backs up continuously to
the node; simulated loss; `hive-sync restore` onto a **second real machine**
boots the full store (journal, entities, tasks, mail bodies; stated and
demonstrated caveats: mail FTS returns at 4.10, vault creds re-entered,
embeddings recompute). Proto-v1 wire compat fixtures cut at this tag. Blind
means blind: the node held no key at any point — demonstrated by running the
whole demo with no KeySource configured.

### Stage B — the trusted node (v0.9.0)

- **PR 4.10 foreign ingest + fold events.** `Core::ingest_segments(device,
  bytes)` as a writer-thread op beside commit (LogWriter's foreign-device refusal
  stays): verify contiguity + chain, land verbatim under `log/<device>/`, fsync,
  fold past the watermark without store reopen. Content divergence at an
  already-folded (device, seq) is a **poison event**: quarantine + loud alert,
  never overwrite. No lc clamp at ingest — rejected in the DIRECTION amendment
  (D29): a Lamport clock legitimately runs far ahead after offline work; lc stays
  an untrusted writer assertion stated in the threat model, the control cutoff is
  hub receipt order, and 4.11's backdated-lc corpus pins arbitration determinism.
  Fold-applied
  events onto the broadcast bus (core only; the UI subscription is 4.16's).
  Post-fold **mail FTS re-stamp** pass (un-ignores the 4.1 pin: replicated and
  healed mail becomes keyword-searchable). Tests: ingest idempotence vs
  watermarks; poison-event quarantine (extends the heal-brick pins); FTS-after-
  replication unit; bus event emission. Gate: a foreign log lands and folds
  without reopen, and nothing can brick open anymore.
- **PR 4.11 merge semantics (FOLD_VERSION 4).** D18's stated-but-unbuilt
  semantics: commit sets lc = 1 + max(own, max-ingested); order-independent LWW
  via a `field_clock(entity, field, lc, device)` shadow table (heal's
  device-at-a-time order stays valid; every interleaving converges);
  update-before-create materializes a ghost row completed by the late create;
  UNIQUE slug collisions resolve deterministically (loser gets a `-<device>`
  suffix). Legacy decision, stated: pre-v2.1 records' lc=seq is a valid
  degenerate single-device Lamport history — (lc, device) comparisons apply
  as-is, no epoch tagging. FOLD_VERSION 4 ships it via the user_version
  drop-and-replay vehicle; no log bytes change. Tests: CONVERGE-ORACLE —
  seed-randomized delivery interleavings of a divergent-edit corpus fold to
  byte-identical canonical dumps + equal watermarks (seed printed on failure),
  **including a backdated-lc adversarial corpus** so the arbitration rule is
  pinned, not implied; table-driven LWW matrix; ghost-create and slug-suffix
  units. Gate: convergence goldens green; this PR precedes any bidirectional
  sync.
- **PR 4.12 control log + device/custody records.** Execute 4.0's format
  decision: `<data_dir>/control/` root under the NEW derivation context (its own
  GOLDEN-BYTES fixtures; the data-log derivation untouched byte-for-byte);
  `ctlkind::ALL` v1 with signed payloads (exact-bytes bstr construction, fixed
  checked-in test keypair so goldens are stable); fold projections into
  `devices`/`custody` tables + device.add alert events on the bus;
  self-grandfathering device.add at first v2.1 boot (a master-holding device
  signs its own pre-existing dev-id); historical writers (importer's device) get
  `pubkey: null, status: "historical"` — folded, never transport-authenticated.
  Revoke-dominates deterministic merge for control kinds; statements embed
  (domain_id, epoch, prev-hash) and the node's high-water enforcement from 4.7
  now reads folded control state on trusted domains. **Hub acceptance policy:**
  control records enter only over an authenticated session from a pinned,
  unrevoked device or via the enrollment ceremony. Mutual-revoke lockout and its
  recovery-code escape documented. Tests: GOLDEN-BYTES per ctlkind;
  HOSTILE-DECODE over the signature envelope (truncated sig, mismatched
  signer_pk, bstr length lies) BEFORE the first golden is cut; compat test that
  heal never descends into `control/` (a data dir containing it opens cleanly —
  the CI-checkable proxy for old-build invisibility); revoke-dominates
  interleavings; stale-statement replay refused; revoked-after-fold connection
  drop. Gate: control log live with zero effect on old-build data-log scans.
- **PR 4.13 trusted tier + domain manager.** `FileKeySource` behind the existing
  KeySource seam: production mode = master sealed under a node KEK (TPM/KMS-backed
  preferred even for tenant 1; KEK-on-same-disk explicitly documented as
  obfuscation, not custody separation), test mode = the 4.1 raw-hex format —
  one format, two modes, resolved before tokio. Store-per-domain manager (one
  Store per leaf dir, per-leaf flock composes, lazy open; embedder/model-cache
  config becomes per-Store — the process-global HIVE_MODEL_CACHE env collision
  dies here). **Promotion blind→trusted is a control-log fact:** provisioning a
  master (or recovery blob) to a node writes a signed `custody.grant` naming holder
  pubkey + tier — peers can SEE custody, the E2E story stops being an
  unverifiable operator claim. Tests: smoke = `test_pair()` two device stores +
  in-proc trusted node converge (canonical dumps byte-equal both directions);
  blind-tier variant (node keyless, still relays, devices converge);
  ADVERSARIAL-SMOKE revoked-device ingest refused at hub. Gate: trusted
  convergence green; custody visible in folded state.
- **PR 4.14 enrollment v2 — second-device ceremony + recovery.** Personal
  pairing polish: pairing string `hive://host:port#<node-pk-fp>/<code>`; for
  custody:"user" domains, approval REQUIRES an existing full device with a
  **mandatory short-auth-string** computed over {new ed25519_pk, x25519_pk,
  node_pk} compared on both screens (node-solo approval is escrow-only, i.e.
  nonexistent in v2.1 — a relay cannot substitute keys undetected); approver
  signs device.add, wraps master to the new device's X25519 (sealed box,
  XChaCha20-Poly1305 family), relayed through the node as an opaque blob; the
  pairing payload carries the current control-head hash so a fresh device detects
  a stale control view. Every device.add fold raises a first-class "new device
  joined — was this you?" event. Recovery: node-held recovery blob (the existing
  Argon2id container keyed by the existing 52-char recovery code), fetch
  throttled + audited + alerted on all devices; a fresh device unwraps locally,
  self-signs device.add, revokes the lost. Tests: enrollment smoke extended —
  SAS-mismatch abort, stale-control-head detection, recovery fetch throttle,
  wrong-code lockout; unit tests on the sealed-box wrap (GOLDEN-BYTES beside the
  frozen wrap format). Gate: a second trusted device enrolls end-to-end in the
  smoke tier; the real-hardware run is 4.16's manual gate.
- **PR 4.15 C1: the scheduler moves into hive-core.** One named PR, one owner —
  this is the seam BOTH later tracks assume (old PR 3.1's wasmtime host embeds
  into it; W1's app-shell slimming relocates drivers onto it). The mail tick +
  embed cadence + reaper loop leaves app/src/main.rs use_hooks and becomes a
  core-owned scheduler with per-Store config (env knobs become constructor
  config; kill-switches preserved). Deliberate naming: this is the **scheduler**;
  "module host" stays reserved for Phase 3's wasmtime work. Tests: scheduler
  unit tests (tick cadence, budget, pause latch) driven headless; app behavior
  unchanged (existing suites). Gate: app and a headless harness run the identical
  loop.
- **PR 4.16 desktop bidirectional sync + UI.** Pull+ingest loop in the app (via
  4.10), the **first UI bus subscription** — fold-applied events invalidate open
  views so sync-applied records actually repaint (today's manual refresh counters
  can't see them); device.add alert surfaced; custody tier chip per node.
  Tests: smoke = concurrent edits both ways through the node, LWW outcome
  asserted; DOM-level checks ride existing app unit tests. **Manual gate (stated
  in PR): real two-box validation** — laptop + node hardware, divergent-edit
  cases, shred-on-A-verifiably-gone-on-node-and-B. Gate: two real devices
  converge from cold; interrupted transfers resume.
- **PR 4.17 node ingestion: mail + embeddings 24/7.** hive-node runs the 4.15
  scheduler for trusted domains under the node's own device id through the typed
  surface (mail_ingest_batch precedent; import_batch stays importer-only). Build
  the **missing mail embed drain** (MAIL_EMBED_ELIGIBLE_SQL + embed_state exist;
  wire `jmap_sync::strip_quoted` — zero callers today) with the existing 20s
  stage budget; restore the JMAP EventSource **doorbell** on the always-on node
  (the tick stays the fallback); model-cache pre-warm for headless images (GPU
  via Ollama route or cuda feature, honest CPU default). Tests: smoke = node
  ingests against a fixture JMAP source, embeds under hash embedder, devices
  receive mail records via sync and FTS re-stamp; embed-drain unit tests
  (eligibility, budget, pause-don't-poison latch). Gate: node ingests + embeds
  unattended; desktop sees mail it never fetched.
- **PR 4.18 credentials + per-domain ops.** `hive-node cred set` provisions mail
  credentials into the node store's vault (master-derived AES-GCM — works
  wherever the store opens; the Phase-3 "fold vault into OS keychain" note is
  formally amended: the vault IS the headless path); per-domain config surface
  (tick cadence, embed limits, attachment caps) replacing process-env reads.
  Tests: cred round-trip under FileKeySource; config precedence units. Gate:
  node mail runs with no env vars and no keychain.
- **PR 4.19 ANN upgrade (usearch/HNSW).** Scheduled BEFORE node-scale mail
  embedding bites (~200k chunks vs the 10^4-10^5 brute-force envelope): MSRV
  1.84→1.85 bump, usearch behind the existing AnnIndex trait. Tests: parity
  suite vs brute-force on fixed corpora; the nightly perf canary (below) run
  before/after on the same runner class, both numbers in the PR description.
  Gate: recall parity within tolerance; no merge-gating perf assert (nightly
  only, ≥5x loose bound, trend artifact).
- **PR 4.20 ship + threat-model amendments.** Container image (single binary +
  pre-warmed model cache), podman-quadlet + compose examples, migration doc
  (tenant `household`, domains `nate`/`maggie`, both personal custody; enroll →
  trusted provisioning ceremony → full-log bootstrap → steady-state tail).
  **Backup guidance with exclusions:** the node KEK, cred vault, and recovery
  blobs are excluded from data-tree backups (or separately encrypted under an
  offline key) — a restic repo with all three collapses at-rest encryption into
  one artifact; node-theft blast radius documented including live mail creds.
  THREAT-MODEL.md amendments, written from the adversary's chair: (1) blast
  radius by TIER, not custody label — "E2E-against-node holds only for blind
  domains; a trusted domain's master is held by the node; node compromise yields
  that domain's plaintext AND control-plane signing; custody:'user' vs 'escrowed'
  says who ELSE holds master, not whether the node does"; (2) lc/ts are writer
  assertions — revocation cutoff is hub receipt order; (3) a revoked device that
  held master retains the authority key and can sign control records until
  master+authority rotation ships (roadmapped); (4) revocation protects future
  data only; (5) blind-node metadata enumerated fully: device set + pubkeys,
  IPs, per-frame sizes and write cadence (a behavioral fingerprint), Forget
  timing, segment/block counts and sizes, and known-file confirmation via
  FastCDC chunk-size fingerprints — the blockstore header's
  "no confirmation-of-file" overclaim corrected in the same PR; (6) recovery
  blobs reduce master security to code strength + node throttle/audit; (7) hub
  completeness/freshness are unprovable — append-only verification catches
  tampering, not withholding; epoch high-water, control-head-in-pairing, and
  per-peer last-synced UI are detection aids, not prevention; (8) enrollment
  trusts the SAS ceremony — without it a relaying node can substitute keys and
  receive master; (9) the node is a concentration point (KEK-sealed masters,
  recovery blobs, live mail credentials beside all ciphertext); (10) tenant
  isolation in v2.1 is partition + process-at-best (unwrapped masters share one
  address space until tenant 2), and the escrow TCB — node operator + tenant KMS
  controller + tenant IdP administrator — is stated now, as paper. That is the
  DIRECTION amendment's twelve-statement delta minus the web-oracle statement
  (lands at 5.4) and the sharing statements (land at 6.4). **ADVERSARIAL-SMOKE
  codified in
  AGENTS.md: every amendment maps 1:1 to a test or a written
  untestable-because-X.** Manual gates (stated): container deploy on the
  household box; restore-from-ZFS-snapshot boot; backup-set exclusion audit.
  Gate: tag v0.9.0 — the node runs household mail + embeddings 24/7, two real
  devices + node converge, revocation enforced at the hub, threat model tells
  the truth.

## Phase 5: web head (v0.10.0)

Design stance. One dioxus version rules both heads (0.6.3; the 0.7 WebKitGTK
blank-page re-A/B is its own post-phase decision gate — don't block shipping on
upstream). **CSR-only wasm, no SSR, no hydration** — matches desktop's actual
behavior and sidesteps the 0.6 hydration bug class. The web head embeds in-process
on the trusted node (server functions or plain axum behind one trait — bridge_proto
is explicitly wrong here and stays the agent API). Web writes commit under the
node's device id and replicate normally — 4.11's LWW landed first, so writes are
safe, but v1 scope stays read-mostly + journal append (create-only). Auth v1 is
tailnet **with per-request whois-derived identity→domain authorization, deny by
default** — interface binding alone is not auth, and one household member on the
tailnet must not reach the other's domain. No sessions, no cookies, no OIDC in
v2.1 at all.

- **PR 5.0 screenshot rig (test retrofit, lands before any code motion).**
  `ci/shots/Dockerfile` → digest-pinned GHCR image: apt-pinned libwebkit2gtk-4.1
  (+ the -dev headers the in-container app build needs: libgtk-3-dev, libxdo-dev,
  pkg-config, cmake, libssl-dev), xvfb, dbus, DejaVu-only + fontconfig pin
  (grayscale AA, rgba=none, fixed hint), odiff, ImageMagick, Rust toolchain
  (image size accepted for ~2min faster jobs); **WEBKIT_DISABLE_DMABUF_RENDERER=1
  AND the legacy compositing var; GTK settings.ini baking cursor-blink and
  animations off; SHOT-READY → fixed 200ms settle → capture, retry-once as
  backstop, logged loudly**; `--shm-size=1g` on the container job. `seed-fixture`
  bin (store APIs, fixed ts/content, writes the HIVE_MASTER_KEY_FILE beside the
  dir); `HIVE_SHOT_TOUR` manifest module driving nav/overlay signals (the 202
  stable DOM ids; signal-driving, not coordinates — at most one keyboard-only
  xdotool input-path test); **v0 scope: 5 shots** (journal, mail list, calendar
  month, settings, boot-failure), grown per screen-touching PR toward ~15;
  goldens in plain git keyed by image tag, current tag only, LFS at >20MB;
  `scripts/shots.sh --update` runs the same container via podman (local pixels ==
  CI pixels). Plus the **DOM-snapshot layer**: dioxus-ssr + pinned insta with a
  suspense-pump helper. Plus the deferred **app-boot bridge smoke** here (needs
  a display): job builds `-p hive-app -p hive-bridge`, passes HIVE_APP_BIN/
  HIVE_BRIDGE_BIN paths to a HIVE_SHOTS-gated test in smoke/ (the cross-crate
  CARGO_BIN_EXE mechanism, decided) — app under xvfb, bridge `call
  journal_append` then `recall`, closing the explicitly-untested app socket side.
  CI: `ui-shots` job **advisory for two weeks with flake measured from retry
  logs, then required** — that answers the gating question with data. Gate: 5
  shots + DOM snapshots + app-boot smoke green in the pinned container.
- **PR 5.1 W1 crate split (pure code motion).** `ui/` lib crate: screens/
  components move out of the 10,307-line main.rs; `app/` keeps wry glue (close
  handler), WindowBuilder/launch, keychain boot, drivers (already core-side per
  4.15). Features: portable default + `desktop-native` for Store/hive-import
  screens (Onboarding import, Settings keychain). **Acceptance gate doubling as
  the constraint: every portable screen renders under dioxus-ssr in a test** (a
  wry-context reach-in panics there — mechanically caught). Tests: DOM snapshots
  re-pinned; the 5 pixel shots byte-identical (pure motion proven). Gate: zero
  behavior change, both suites green.
- **PR 5.2 W2 data seam.** Portable screens read `Arc<dyn HiveUi>` (typed async
  DTO trait, ~8-10 methods) instead of `ReadOnlySignal<Store>`; `Platform`
  context absorbs now_iso (the two core reach-ins), save-file, open-external.
  Desktop impl = direct Store calls, semantics unchanged. Tests: trait-level
  unit tests with a mock HiveUi; DOM snapshots unchanged; shots unchanged. Gate:
  desktop pixel-identical behind the seam.
- **PR 5.3 W3 hive-web + node serving.** `web/` wasm bin (thin CSR shell);
  node's axum serves the bundle + the RPC (server-fn vs hand-rolled JSON decided
  by a timeboxed spike inside this PR — the trait makes either a drop-in); v1
  screens: journal feed, keyword/semantic search, read-only contacts/tasks/
  entity detail, plain-text journal append. Document CSP (`default-src 'self';
  script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; frame-ancestors
  'none'`, inline styles accepted post-ammonia) + SameSite/Origin CSRF checks on
  mutations. **The srcdoc sandbox rail and image proxy are NOT here** — they
  land inside the first web mail-read PR (post-v2.1 program), which is also when
  desktop adopts the same iframe rail for D17 parity. CI: `wasm32-unknown-
  unknown` build gate for ui/web; hive-web **excluded from the host workspace
  build** (spares the rust job the wasm-bindgen tree). Tests: Playwright shots
  in a digest-pinned browser container (same golden scheme + seeder + manifest
  names as 5.0 — no unpinned runner Chrome, ever); RPC round-trip tests. Gate:
  web head renders the shared components against a live node.
- **PR 5.4 W4 web auth + amendments.** Tailnet whois → identity → domain map,
  deny by default; web access/append logged as auditable events; bind-validation
  unit test (refuses non-tailnet binds in tailnet mode). THREAT-MODEL
  amendments: the web head makes a trusted node a remotely reachable plaintext
  oracle; tailnet compromise = that domain's read/write until revoked; web-head
  writes are indistinguishable from device writes downstream. **Manual gates
  (stated in PR):** non-tailnet client refused; cross-member access refused
  (Maggie's identity cannot read Nate's domain from the same tailnet); browser
  download/anchor semantics verified by hand once. Gate: tag v0.10.0 — journal
  + search + append from a phone browser on the tailnet, correctly scoped per
  member.

## Phase 6: sharing (v0.11.0)

Design stance. Sharing granularity is the blob (segments are master-wrapped —
wrong layer); text items materialize into a blob at grant time ("share = make a
blob"; shares are snapshots, stated). Recipients never ingest foreign records:
fetch ciphertext by id, verify keyless, unwrap, **re-ingest through their own
typed write path** under their own master — no new blockstore machinery,
recipient-side shred works today. The grant artifact is split BEFORE any golden is
cut (critique blocker): a node-facing serve stub (grant_id, block ids, recipient
transport key, expiry, revocation ref, authority sig — the minimum to pin/serve/
revoke) and a recipient-sealed payload (metadata snapshot + wrapped content key,
sealed to recipient X25519, relayed opaquely) — a blind node never reads item
metadata, and the amendment states what it still learns (the sharing graph).

- **PR 6.1 share format decision + crypto.** ctlkind widening (explicit format
  decision, own goldens): share.grant/share.revoke in the sharer's control log,
  share.received in the recipient's; the split serve-stub/sealed-payload shape;
  domain sharing keypair **derived from master like the authority key and
  published via control record** — no new custody object, no new exchange
  ceremony (within-tenant discovery = folded control state relayed by the
  household node + fingerprint display); K_AB = static-static ECDH →
  blake3-derive → **reuse the frozen 72-byte wrap verbatim**; `wrap_alg` field
  reserves HPKE. Honest limits stated (no FS/PCS). Tests: GOLDEN-BYTES for both
  artifact halves under fixed keypairs; HOSTILE-DECODE first; RFC 7748 vectors
  already in-tree from 4.5. Gate: formats frozen with the leak surface designed
  out, not discovered later.
- **PR 6.2 random-key put() mode.** Non-convergent per-blob keys as the
  **DEFAULT for any blob that becomes grant-referenced and for all re-ingestion
  after a shred** — not a user option; this is the only real answer to the
  convergent re-derivation trap (same plaintext re-derives the same key; a
  revoked recipient's retained key would decrypt future re-ingestions). BlobRef
  unchanged (one wrapped_key slot; that blob just loses dedup). Tests:
  re-ingest-after-shred yields a fresh key (golden); grant-path defaults
  asserted. Gate: the trap is closed by construction.
- **PR 6.3 grant flow + node relay/cache + serve ACL.** Grant → node pins the
  referenced blocks against GC and caches for offline sharers (blind suffices —
  it serves ciphertext it cannot read); **serve-time ACL per request: recipient
  identity + grant_id + id ∈ that grant's block set + revocation check** (bare-id
  fetch is otherwise an oracle into the sharer's blockstore); recipient
  materialization → shared_items projection (provenance-keyed, read-only UI),
  FTS at receive, embeddings via the normal per-replica pipeline. Tests: smoke =
  two-domain (two masters) round-trip on one node — grant → fetch → materialize
  → search-visible; out-of-grant block id refused; revoked-grant serve refused.
  Gate: a share survives the sharer being offline.
- **PR 6.4 revoke + shred cascade + amendments.** share.revoke stops serving and
  recipients tombstone cooperatively (stated: not cryptographic — delivered
  plaintext is unreachable forever); shred of a shared blob enumerates live
  grants via `grants_by_blob`, auto-appends revokes, then runs the existing
  shred; Forget stragglers documented. THREAT-MODEL amendments: a relaying node
  learns the sharing graph and grant timing even when blind; grant-held keys are
  immutable copies shred must chase; recipient replicas are out of reach;
  under escrow all of it is policy-mediated (Slack/M365 framing). Tests: full
  cascade integration (…revoke-refused → shred-cascade, per-replica crypto_shred
  assertions); ADVERSARIAL-SMOKE per amendment. **Manual gate (stated): a real
  Nate↔Maggie share round-trip on the household node.** Gate: tag v0.11.0.

## Phase 3: modules + PIM + dreaming (carries; executes here; v0.12.0)

What changes, and only this: **PR 3.1 is amended** to honor D17 in a headless
world, and reality's early deliveries shrink three PRs. Everything else carries
word-for-word from PLAN.md.

- **PR 3.1 module host (amended).** wasmtime component model, WIT world
  `hive:module` (describe/tick/handle-push; imports: http, sink-emit, cursor,
  secret, log, subscribe) — **landing in hive-core behind the 4.15 scheduler
  seam, UI-free and headless-embeddable**: hive-node hosts the identical runtime
  with per-tenant/per-domain isolation hooks, the desktop renders settings from
  config schema as before. The `secret get` import is **vault-mediated** (4.18's
  headless amendment), not keychain-mediated. Per-module capability grants +
  http allowlists; load/enable/pause from disk. Named fallback: extism, decided
  by a timeboxed spike inside this PR. Tests: host smoke in hive-smoke driving a
  fixture module in BOTH embeddings (app and node) — D17's "embeddable" claim
  made mechanical. Gate: same module runs on desktop and node.
- **PR 3.2 mail module (shrunk).** The engine is already live core code and
  running 24/7 on the node (4.17), and the doorbell landed there too — 3.2 is
  now: jmap-sync's transport seam so protocol logic can run in-guest over
  host-http, and the Stalwart e2e harness revived against the module (harness
  needs rework for >v0.15.5's DB-backed config; pin stays until then).
- **PR 3.3 filesystem module.** Carries unchanged.
- **PR 3.4 calendar + contacts (shrunk).** The views shipped in Phase 2; what
  remains: event entity upgrade (RRULE/reminders/tz), person→actor/contact split
  via migration records, caldav/carddav client modules, presets PR.
- **PR 3.5 mail UI (mostly shipped).** The reader/compose cluster exists under
  the ammonia half of D17; the remaining slice — the sandboxed-frame rail — now
  rides the first web mail-read PR (see 5.3) and lands on desktop in the same
  series for parity. This PR shrinks to that parity adoption.
- **PR 3.6 dreaming.** Carries unchanged (AI-visibility defaults decided here).

Gate (carried, adjusted): chosen directories, calendar, contacts searchable +
citable alongside the already-live mailbox; /dream materializes entities through
normal emergence; a module runs identically on desktop and node. Tag v0.12.0.

## Phase 7: browser extension (old Phase 5, ids re-stamped 7.x; v0.13.0)

Carries unchanged in content: 7.1 extension (MV3, Readability capture, policy
engine, native messaging), 7.2 bridge native-messaging entry + manifest
installers, 7.3 capture pipeline with `[web:<id>]` citations + audit
crypto-shred, 7.4 distribution + e2e capture test. Gate carried; tag v0.13.0.

## Deferred: multi-node + thin clients (D36)

Trigger: the first second-node deployment (offsite trusted replica, or DTC
hosting). v2.1 ships one node per domain; the formats already leave the door open
(node.* ctlkinds reserved at 4.0, SRV priority/weight is the roster vocabulary,
the D35 dialer generalizes to RTT-preferred node selection). The bill, recorded
now: epoch max-merge with revoke-dominates between nodes, Forget propagation
node-to-node, the node roster records, and open question 16 (multi-hub revocation
cutoff) decided in its own record. Until then, N blind backup nodes need no
protocol at all — replicate the vault directory with rsync/syncthing/ZFS send;
the write-once invariant makes file-level replication safe. Thin clients (partial
replicas, phone-class) ride the same future record; clients are never caches —
the smallest client remains the authoritative writer of its own log.

## Deferred: tenancy hardening (tenant 2)

Trigger, stated: **a signed commitment from the first external tenant (a DTC
hosting client org) — not before.** Until then, multi-tenancy is partition +
per-domain masters + quotas, and that is the whole story. The tenant-2 gate list,
recorded now so it cannot be discovered later: per-tenant KMS principals (no
single node credential may decrypt more than one tenant's root), enforced
per-tenant module-host/web-head processes or containers, per-tenant audit of KMS
unwrap operations, EscrowKeySource + escrow genesis + enterprise
offboarding (handoff/retain/shred), OIDC enrollment console + member.* control
kinds + IdP federation config, SCIM/group sync, admin RBAC, rate limiting/
billing, node-countersigned control checkpoints (finality) before any enterprise
tenant relies on revocation. The escrow-TCB threat-model statement (IdP
administrator and tenant KMS controller join the node operator in the TCB) is
already stated in v2.1 at 4.20, as paper; the enforcement machinery and its
per-tenant audit statements land WITH this work. v2.1's amendment (10) already
states today's process-at-best isolation honestly.

## Deliberate rejections (v2.1)

Decisions made and closed — reopening any of these is a new decision record:

- **iroh/QUIC now.** TLS-TCP under a stream-generic frame layer; iroh/QUIC slot
  underneath later with no wire change (D21 amendment).
- **Any device-to-device or device-to-foreign-node path.** Hub-only; shares are
  relayed and cached at the node; the keyless statement-export/verification
  surface and lateral-transfer quarantine machinery are deferred with it (the
  node pins keys first-hand at enrollment — third-party blind relays are the
  phase that needs exported statements).
- **A node MCP endpoint / bridge remote transport / Windows bridge.** The bridge
  stays a dependency-free local UDS pump; its failure-marker contract is NOT
  amended in v2.1. Agent access at the node, if ever wanted, is a scoped D25
  amendment with its own PR. Windows stays deferred with named pipes post-2.5.
- **OIDC anywhere.** No OIDC code ships in v2.1 — not in the node, not in the web
  head. The identity fence passes with an empty allowlist. Zitadel arrives with
  tenant 2.
- **Escrow custody, member.* kinds, enterprise ceremonies.** Paper only (see
  tenant-2 gate).
- **Wire-frame byte goldens during protocol churn.** Deviation from the initial
  suggested shape, per critique: proptests + round-trips while the wire moves;
  compat fixtures cut at the first tag carrying proto v1 (v0.8.0). Byte goldens
  stay reserved for on-disk formats.
- **Vector sync.** Embeddings remain per-replica derived state; each replica
  recomputes (ONNX float nondeterminism stays harmless). Revisit trigger:
  low-power devices hurting — a follow-on decision record for node-computed
  vector distribution with model/dim negotiation.
- **SSR/hydration for the web head.** CSR-only until the 0.6 hydration bug class
  is someone else's problem; dioxus 0.7 re-A/B is a post-Phase-5 decision gate.
- **Web mail-read + srcdoc sandbox rail + image proxy in web v1.** They are one
  PR, later, and desktop D17-rail parity rides it.
- **Log compaction / checkpoint bootstrap.** Full-log per D18's arithmetic.
- **Authority/master rotation.** Not silently dropped: deferred WITH a roadmap
  milestone, and the threat model states the gap it leaves (revoked master
  holders can forge until it ships).
- **Coordinate-click UI driving.** Signal-driven shot tour + at most one
  keyboard-only xdotool test; pixel-coordinate clicking is the flake class the
  rig exists to avoid.

## Cross-cutting

- **Named test conventions** (defined here, cited by name in every PR):
  - GOLDEN-BYTES — frozen encodings get checked-in byte-exact fixtures, regen
    only via HIVE_UPDATE_GOLDENS=1 which fails the run; extends to ctlkinds
    (fixed test keypair), share artifacts, recipient-wrap, RFC 8032/7748
    vectors. Golden churn must be explained in the PR — "a pixel or byte change
    is a decision, not a refresh."
  - ONE-SEAM — test_store() stays the only core store constructor;
    smoke-support adds test_domain()/test_node()/test_pair(); AGENTS.md clause
    extended.
  - GREP-FENCE — determinism + no-Postgres gates joined by the identity fence
    (namespace/dependency tokens + cargo-metadata walk, node/ only) and the
    tenancy fence (identifiers under core/). Cheap, total, undodgeable.
  - HOSTILE-DECODE — proptests (fixed seeds, bounded, in normal CI) over every
    network-facing decoder: envelope decode, keyless header/frame walk, every
    sync frame, the control signature envelope. Err-never-panic; length caps
    before allocation. Optional cargo-fuzz targets nightly, never gating.
  - CONVERGE-ORACLE — canonical-dump byte-equality across replicas over
    seed-randomized interleavings + watermark equality, including the
    backdated-lc adversarial corpus.
  - ADVERSARIAL-SMOKE — every THREAT-MODEL amendment maps 1:1 to a test or a
    written untestable-because-X; the six protocol scenarios (revoked-cert
    reconnect, stale-statement replay, cross-domain session scoping, quota at
    ingest, revoked-grant serve, hostile segment ingest) are the floor.
- **CI shape.** Merge-gating from day one: `rust` (fmt/clippy/build/release-app/
  workspace tests; hive-web excluded from host builds; DOM snapshots ride here),
  `importer` (unchanged, the only DB), `smoke` (HIVE_SMOKE=1, crate-scoped, no
  webkit), `wasm-gate` (from 5.3). `ui-shots` (pinned container by digest,
  HIVE_SHOTS=1): advisory two weeks with measured flake, then required. Nightly/
  dispatch lane: cargo-fuzz, perf canary (ANN scan latency, ≥5x loose bound,
  trend artifact — never merge-gating; the 4.19 swap cites before/after runs),
  shots-container rebuild.
- **Decision gates:** control-record home + derivation context + signature
  construction (4.0, decided in-doc); crypto stack + cert carrier (4.5 spike);
  server-fn vs axum (5.3 spike); ui-shots advisory→required flip (data, two
  weeks); MSRV 1.85 (4.19); dioxus 0.7 re-A/B (post-5.4); AI-visibility (3.6);
  tenant-2 trigger (external commitment).
- **Sequencing rules restated:** #129 before anything app/bridge-touching;
  4.0 before any format-touching PR; 4.11 before any bidirectional sync; 4.15
  (C1) before 4.17, 5.1, and 3.1; 5.0 before 5.1; 6.1 goldens only after its
  HOSTILE-DECODE suite.
- Rough calendar, held loosely: P2-close 1 wk; P4.A 2-3 wk; P4.B 3-4 wk; P5 2-3
  wk; P6 1-2 wk; P3 3-4 wk; P7 1-2 wk.

## Verification

- Every PR: cargo fmt + clippy + test green; conventions cited by name; CI
  evolves as described (smoke at 4.2, fences at 4.6, ui-shots at 5.0, wasm gate
  at 5.3; importer job untouched throughout).
- Phase gates are the named demos: proxy-bridge connections (2.x), **M1
  backup/restore on a second real box (4.9 → v0.8.0)**, two-box convergence +
  24/7 node ingestion (4.16/4.20 → v0.9.0), tailnet web scoped per member
  (5.4 → v0.10.0), Nate↔Maggie share round-trip (6.4 → v0.11.0), module parity
  desktop/node + /dream (3.x → v0.12.0), capture round-trip (7.x → v0.13.0).
- Real-device/manual gates, stated in each PR per repo convention (CI cannot
  boot two boxes or a tailnet): M1 restore on real second hardware **reached via
  its published candidates — tailnet path with LAN unplugged, LAN path with
  tailnet down (D35 multi-path dial on real networks)**; 4.16
  two-box divergent-edit + shred propagation; 4.20 container deploy on the
  household box, restore-from-ZFS-snapshot boot, backup-exclusion audit; 5.4
  non-tailnet refusal + cross-member refusal + browser download semantics; 6.4
  household share round-trip; WebKitGTK-first remains the rule for anything
  app-visual.
- Standing invariants at every merge: fold replay byte-identical; golden
  retrieval fixture passes; the 11 data-log goldens never change; a data dir
  containing `control/` opens under old heal; the desktop works offline with the
  node gone.
