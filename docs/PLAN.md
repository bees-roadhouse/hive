# Hive P2P Pivot: execution plan (Phases 1-5)

Status: approved 2026-07-10. Companion to [DIRECTION.md](./DIRECTION.md) (the
decision record, D16-D28); this document is the PR-by-PR execution program.
Grounded in a full repo inventory and a stress-tested Phase 1 sequencing review.

Target: Dioxus UI (system webview now, Blitz later), append-only event-sourced
storage (per-device op logs as truth, SQLCipher SQLite as rebuildable derived index),
blake3/FastCDC block blobstore with crypto-shredding, iroh device sync with a
self-hostable BR relay, WASM ingestion modules (wasmtime component model), PIM entity
core, MCP-only external API through `hive-bridge`, dreaming skill, zero telemetry.

## Ground rules

- All work lands on `main` via small PRs. PR 0 = merge `feature/p2p-pivot`
  (DIRECTION.md v2 + this file); workspace version bumps to 0.7.0 in it.
- Releases: v0.7.0 at end of Phase 2 (first usable app), v0.8.0 = Phase 3,
  v0.9.0 = Phase 4, v0.10.0 = Phase 5. Phase 1 lands untagged. Hosted-era endpoint
  stays the pre-pivot sha (untagged, per D28).
- Two deliberate outages, stated up front:
  - Mail sync pauses from Phase 1 teardown until the Phase 3 mail module; the
    importer preserves JMAP cursors (`cursor.set`) so Phase 3 resumes as a delta
    resync, not a re-backfill.
  - Claude integration pauses from Phase 1 teardown (REST `/mcp` dies) until the
    interim bridge at the end of Phase 1 (PR 1.8), then upgrades to the D25
    UDS-proxy in Phase 2.4.
- Each phase gate is its DIRECTION.md milestone, demonstrated by named tests/demos
  before the next phase starts.

## Phase 1: hive-core (8 PRs, `cargo test` green at every merge)

Design stance (from the sequencing review): teardown happens BEFORE the storage
cutover (never port auth/workspaces to SQLite just to delete them). Emergence
parsing stays in the command layer at append time; the fold is a mechanical
records-to-SQLite projector, so parser evolution can never change replay results.
Two verified facts shape the sequence: PR #91's ANN is pgvector HNSW in SQL
(`ann_candidates`, store/semantic.rs:752 — under core/src/ once PR 1.1 lands), and
`to_match_query` (semantic.rs:157) emits tsquery syntax that is invalid FTS5, so a
golden retrieval
fixture must be captured while Postgres still runs. Tailwind: `pgq.rs` exists
because queries were born with SQLite `?` placeholders.

- **PR 1.1 extract hive-core (pure move).** New `core/` crate: move
  `api/src/store/**` verbatim (emergence parser rides inside store/journal.rs),
  `api/src/pgq.rs`, `api/src/db.rs` (incl. `test_pool()`), `now_iso()` out of
  auth.rs; `Visibility` moves into core temporarily. worker/ and mail/ switch deps
  from hive-api to hive-core (severs the dependency landmine early). Tests + CI
  untouched, still Postgres.
- **PR 1.2 teardown A (Node, worker, mail daemon).** Move-then-delete: port the
  worker's chunked embedding backfill + budget into `core/src/store/embed_backfill.rs`
  with its tests (the api-side tests only cover single-chunk). Then delete worker/,
  mail/ crates, all of packages/ (web, reflector, runner, agent, cli, api, worker,
  shared), pnpm files. jmap-sync stays (its offline quote-corpus test keeps the mail
  parser alive through the pause); mail's stalwart_e2e dies with the daemon and
  returns in Phase 3 against the module. CI: drop the pnpm `check` job; trim
  release.yml image builds (runner/reflector/session-dev).
- **PR 1.3 teardown B (auth, REST, workspaces; MCP re-pin; tests move to core).**
  Move mcp.rs into core; replace `AuthCtx` with `LocalCtx { actor }` pinned to the
  owner; delete workspace/token/share/admin-multiuser tools. Delete the api crate
  entirely and, from core: workspaces, sessions, tokens, oauth, users, shares,
  cc_credentials, conversations, legacy import, `Visibility` + every viewer/scope
  parameter, the journal mail-scope downgrade guard. Rewrite surviving integration
  tests as `core/tests/` driving `Store` directly behind a `test_store()` seam
  (Postgres-backed for now), and capture the **golden retrieval fixture** (seed
  corpus, expected top-k with tolerances): the cross-backend parity oracle.
- **PR 1.4 op-log + blockstore + keys (additive; format freezes here).**
  `core/src/oplog/` (envelope, CBOR/ciborium encoding, segment files),
  `core/src/blockstore/` (blake3 over encrypted blocks, FastCDC, manifests,
  convergent-with-secret per-blob keys: dedup preserved AND shreddable),
  `core/src/keys.rs` (`KeySource` trait: OS keychain impl + in-memory test impl,
  Argon2id passphrase wrap, recovery code). Envelope: `v, device, seq (gapless),
  lc (Lamport), ts (exact now_iso shape, lexicographic ordering is load-bearing),
  actor, kind, prev (blake3 chain), payload`. Kinds: journal.append, entity.create,
  entity.update, link.add/remove, tombstone, redact, config.set, module.doc,
  cursor.set, alias. Segments: per-frame XChaCha20-Poly1305 under a wrapped
  segment key, 8 MiB rotation, torn-tail truncation. Byte-exact golden fixture
  files checked in. Day-one spike: rusqlite `bundled-sqlcipher` + FTS5 + JSON1
  compiles on Linux/mac/Windows.
- **PR 1.5 SQLite fold + FTS5 + ANN (additive; Postgres still runs the suite).**
  `core/src/fold/`: derived DDL, `apply(tx, &Record)` per kind, `fold_meta`
  watermark in the same tx, `PRAGMA user_version` fold version with drop-and-replay
  on mismatch. FTS5 external-content table + new `fts5_query()` (every token
  quoted + `*`, operators stripped) with pinned adversarial tests; `snippet()`/
  `bm25()` replace ts_headline/ts_rank (bm25 ascending: invert). Embeddings stay
  packed-LE-f32 BLOBs + `AnnIndex` trait (usearch HNSW) shaped like
  `ann_candidates` (candidates, kind weights, keyed hydration). Determinism test:
  scripted log, replay twice, canonical dump equality; CI grep-test asserting fold
  contains no `now_iso|nanoid|rand|SystemTime`.
- **PR 1.6 cutover.** `Store` rides a `CoreHandle`: one writer thread owns the
  rusqlite Connection + segment writer (mpsc commands, oneshot replies; async
  surface preserved). Each write = mint ids/timestamps in the command layer, build
  records, append + fsync, fold-apply in ONE SQLite transaction (the atomicity the
  Postgres path never had; crash healed by tail replay vs fold_meta). Port each
  store module's SQL (drop `?::jsonb`, ILIKE to LIKE COLLATE NOCASE, JSONB ops to
  json_extract), commit-per-module. `emit()` becomes bus-only (wire table dies; the
  op log is its durable successor). Embedder becomes injected `Arc<dyn Embedder>`
  (kills the OnceLock latch hazard in consolidated test binaries). `test_store()`
  flips to tempdir SQLite + mock keys + hash embedder; test bodies unchanged;
  golden fixture must pass. sqlx/pgvector leave core.
- **PR 1.7 importer.** Leaf crate `importer/` (binary `hive-import`, the only
  remaining sqlx user: "no Postgres" stays grep-auditable). One-shot (refuses
  non-empty data dir), synthetic device, seq in dependency order (actors, entity
  types, entities, journal+anchors, links, mail), original timestamps preserved,
  original nanoid ids ARE the new ids (citations resolve natively), `alias`
  records only for re-keyed blob hashes, provenance `origin:{source,table}` on
  every record. Mail: accounts minus credentials (keychain re-entry in Phase 3) +
  `cursor.set` JMAP state; messages as `module.doc{module:mail}`; attachment BYTEA
  through FastCDC into the blockstore. Not migrated: search/embeddings (derived;
  embed_backfill runs as the final step), wire/inbox/outbox, users/sessions/
  tokens/oauth/shares/cc_*, raw conversation transcripts (their reflections
  already live in journal entries, which do import). The old db.rs SCHEMA is
  reborn as `importer/tests/fixtures/legacy_schema.sql` for fixture DBs.
- **PR 1.8 milestone close + interim bridge.** Run the importer against the real
  namespace (verification report: counts + golden queries). Ship `bridge/` in
  interim mode: stdio MCP opening the store directly (no app exists yet to
  conflict with the single writer); repoint the Claude Code plugin
  (plugins/claude-code-hive-memory: .mcp.json http transport becomes a stdio
  server entry; hooks-handlers .mjs rewritten to call the bridge) and the .mcpb
  (integrations/claude-desktop/mcpb: Node bridge.mjs replaced by the binary).
  Rebuild-derived-state command (drop SQLite, replay) exercised in CI;
  crypto-shred e2e test (import attachment, shred, bytes unrecoverable, FTS +
  vector rows gone, tombstone present); threat-model doc (D27); AGENTS.md/README
  rewrite; final Phase 1 CI shape (fmt/clippy/test; Postgres service exists only
  for importer tests).

Phase 1 risks already mitigated by sequencing: worker deletion orphaning the
embedding pipeline (1.2 is move-then-delete); the embed OnceLock latch poisoning
consolidated test binaries (injection at 1.6); SQLite single-writer vs the
pool-level write path (writer thread + channel, designed at 1.6, proven at 1.5);
FTS5 not being a rename (golden fixture from 1.3); cutover blast radius (format
frozen at 1.4, fold proven at 1.5, zero test-assertion churn via test_store()).

Gate: fold replay byte-identical; imported real namespace passes the golden
retrieval fixture; crypto-shred e2e passes; Claude Code journals into the imported
namespace through the interim bridge; no server/auth/Node code remains.

## Phase 2: Dioxus shell + hive-bridge proxy (v0.7.0)

- **PR 2.1 app scaffold.** `app/` crate (dioxus-desktop): window/nav/theme, tokio
  + signals wiring to core, notifications off the existing Store broadcast channel
  (the SSE route's successor), settings as records.
- **PR 2.2 journal UI.** Entry list/detail, contenteditable editor with
  bracket-token autocomplete, DOM-selection to byte-offset anchor creation,
  anchored-entity chips.
- **PR 2.3 views + sanitization rail.** Search, dashboard, tasks board, inbox; the
  D17 pipeline (ammonia at ingest + render, sandboxed frames, no-network CSP) with
  a hostile-corpus fixture set (v1's XSS lesson as tests).
- **PR 2.4 bridge proxy mode.** UDS JSON-RPC server inside the app; bridge flips
  from interim direct-open to D25 proxy-only (clean error when app not running;
  removes the second-writer hazard permanently); plugin + .mcpb final repoint.
- **PR 2.5 packaging + release.** Flatpak first (Bazzite daily driver), then
  AppImage/msi/dmg; release.yml rebuilt around app bundles + mcpb; identifier-free
  version check against a static manifest (no auto-update in 0.7.0); WebKitGTK
  pass explicitly first; tag v0.7.0. Branding decision (DIRECTION.md open
  question 3) needed before this tag.

Gate: daily-drivable single-device app; journal-emergence-search round-trip
in-app; Claude Desktop and Claude Code connect through the proxy bridge.

## Phase 3: WASM modules + PIM + dreaming (v0.8.0)

- **PR 3.1 module host.** wasmtime component model, WIT world `hive:module`
  (describe/tick/handle-push; imports: http, sink-emit, cursor, secret, log,
  subscribe), per-module capability grants + http allowlists, load/enable/pause
  from disk, settings UI rendered from config JSON schema. Named fallback: extism,
  decided by a timeboxed spike inside this PR.
- **PR 3.2 mail module.** jmap-sync grows a transport seam so protocol logic runs
  in-guest over host-http; EventSource doorbell becomes a host-owned subscription
  waking tick; delta resync from the imported cursors closes the pause; compose =
  outbox `mail.send` via JMAP EmailSubmission through Stalwart; stalwart_e2e
  returns to CI against the module (Stalwart pin v0.15.5 unchanged).
- **PR 3.3 filesystem module.** Root grants, host notify watcher as subscription
  events, host-side extract_text builtins (txt/md/pdf), ignore rules + MIME
  allowlist, `[file:<id>]` citations.
- **PR 3.4 calendar + contacts.** Event entity upgrade (start/end/tz/RRULE/
  reminders), contact entity + person split into actor/contact via migration
  records, caldav + carddav client modules (host-http; Stalwart provides the
  servers), calendar grid + contact views; presets PR converting decision/topic/
  project/phase built-ins into custom-type templates.
- **PR 3.5 mail UI.** Reader under D17 policy (sanitized HTML, remote content
  blocked, per-sender opt-in), thread view, plain-text compose with correct reply
  headers.
- **PR 3.6 dreaming.** `ingest_activity` MCP tool (per-module digests since last
  dream), dream-tagged journal entries through the one write path, skill artifact
  via identity_artifacts; conversation capture returns here as an MCP-fed source
  (plugin session-end posts transcript digests through the bridge), replacing the
  deleted Node reflector. AI-visibility defaults (open question 1) decided here.

Gate: mailbox, chosen directories, calendar, contacts searchable + citable;
/dream materializes entities through normal emergence.

## Phase 4: iroh sync + relay (v0.9.0)

- **PR 4.1 identity + pairing.** iroh node keypairs, ticket/QR pairing UX, paired
  device table + revocation.
- **PR 4.2 replication.** Per-device head exchange, sealed-segment want/have, blob
  fetch via iroh-blobs, remote records through the fold (lc-based LWW per field),
  tombstone/shred propagation; kill-mid-blob resume tests.
- **PR 4.3 relay + discovery.** Deploy iroh relay + discovery on BR infra (domain
  = open question 4, decided BEFORE this PR: pairing tickets embed relay
  defaults), custom-relay setting, self-host doc, relay-observability statement in
  the threat model.
- **PR 4.4 two-device hardening.** Simulated two-node CI harness + real two-box
  validation; divergent-edit LWW cases; shred-on-A-verifiably-gone-on-B.

Gate: two real devices converge from cold; interrupted transfers resume; shreds
propagate.

## Phase 5: browser extension (v0.10.0)

- **PR 5.1 extension.** TS/MV3 (the one surviving JS artifact): Readability
  capture at visit time with SPA-mutation debounce, policy engine (domain
  allow/deny, private-window guard, global pause), native-messaging client;
  Chrome + Firefox.
- **PR 5.2 bridge native-messaging entry** + per-browser manifest installers
  driven from the app's settings screen.
- **PR 5.3 capture pipeline.** Ingest-sanitized reader HTML + extracted text,
  `[web:<id>]` citations, audit view with crypto-shred delete.
- **PR 5.4 distribution.** Store listings or self-install doc; e2e capture test.

Gate: real-time captures searchable with citations; audit delete verifiably
shreds.

## Cross-cutting

- Architecture-guarding tests: fold determinism + no-clock/no-RNG grep, record
  encoding byte-goldens, golden retrieval fixture, sanitizer hostile corpus,
  two-device sim, crypto-shred e2e.
- Decision gates: branding (before v0.7.0 tag), packaging priorities (2.5),
  AI-visibility defaults (3.6), relay domain (before 4.3), store listings (5.4).
- BookStack: mark the v1 direction page historical when PR 0 merges.
- Rough calendar, held loosely: P1 2-3 wk, P2 2-3 wk, P3 3-4 wk, P4 2 wk,
  P5 1-2 wk.

## Verification

- Every PR: cargo fmt + clippy + test green; CI evolves as described (pnpm job
  dies at 1.2; Postgres service scoped to importer from 1.6; Stalwart e2e revived
  at 3.2; bundle matrix from 2.5).
- Phase gates: the named demos (importer report at 1.8, Claude connections at 2.x,
  /dream at 3.6, two-device convergence at 4.4, capture round-trip at 5.3).
- Real-device validation stated in PRs per repo convention (two-box for Phase 4,
  WebKitGTK-first for Phase 2).
