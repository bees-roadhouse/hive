# Hive Direction v2: Personal P2P Desktop

Status: decided 2026-07-10. Supersedes the 2026-07-01 record "Hive Direction: Mail as
Substrate" (preserved in git history at this file's prior revision, and on BookStack as
"Hive Direction: Mail as Substrate", page 2360, to be marked historical). Written
against origin/main after the v0.6.0 train (PRs #74-#91 merged; the train closes
untagged by decision D28), from the v1 record, a mapped read of the codebase, and
Nate's decisions of 2026-07-10. Citations are file-level; v1's file:line rigor applies
again once Phase 1 code review starts.

## Summary

Hive becomes a personal, single-user, local-first desktop app in pure Rust. Dioxus
for the UI: Rust components (no JS framework, no Node, no Electron) rendered through
the system webview now, with Blitz native rendering as the tracked future option.
Storage is append-only and event-sourced: per-device op
logs are the source of truth, SQLite is a rebuildable derived index, and payload bytes
live in a content-addressed block store with a manifest layer. Devices sync peer to
peer over iroh through a relay Bee's Roadhouse operates and anyone can self-host.
Ingestion is pluggable through WASM modules configured in the app: mail (JMAP),
filesystem, CalDAV calendar, CardDAV contacts, and a real-time browser capture
extension. The entity core shrinks to a PIM: contacts, mail, calendar, tasks, plus the
journal substrate and user-defined custom types. The only external API is MCP, served
to Claude Code and Claude Desktop through a single bridge binary. A dreaming skill
reviews ingestion activity and writes journal entries that materialize structured data
through the normal emergence language. Privacy is the top constraint: everything
encrypted at rest, E2E in transit, embeddings computed locally, hard delete via
crypto-shredding, and zero telemetry of any kind.

Retired with this pivot: multi-user and multi-tenant operation, the entire auth stack
(sessions, PATs, OAuth 2.1 server, OIDC, onboarding), hosted Claude Code workspaces
and the runner, the REST API, the Solid.js SPA, and the Node packages.

## Lineage

v1's D15 retired the Zibaldone design (append-only blocks, native client, local-first)
as an architecture target and asked, in open question 6, whether that vision would
return as its own project consuming Hive's API or forking its storage. This document
answers it: the native local-first client does not consume Hive, it becomes Hive. The
surviving Zibaldone ideas return on Hive's substrate and vocabulary: the emergence
language, the entity seams, jmap-sync, and the retrieval pipeline all carry forward
unchanged in spirit. What does not return: Makepad (Dioxus instead), UUIDv7 (nanoid
prefix TEXT ids stay), and a generic blocks table for entities (records stay concrete,
per D12's grain; blocks exist only in the blobstore, where content-addressing earns
its keep).

## Carry-forward map (v1 decisions D1-D15)

- D1 mail as first-class entity: carries. Mail stays a concrete corpus, now fed by a
  module instead of a fourth binary.
- D2 surrogate ids + UNIQUE(account_id, jmap_id) idempotency: carries verbatim.
- D3 threads stay queries: carries.
- D4 separate hive-mail binary: amended by D22. The isolation rationale (EventSource
  lifetime, backfill starvation) is satisfied by the module host's task model inside
  the app process; a separate OS process is no longer the right isolation boundary.
- D5 sync state on account rows with loud poisoning: carries as module cursor state.
- D6 mutable mirror with tombstones: amended by D18. Mutation semantics survive at
  the derived-index layer; the durable layer beneath becomes append-only records.
- D7 blake3 blobs in Postgres: amended by D20. Content addressing and dedup carry;
  the backing store becomes a chunked block store, not a BYTEA table.
- D8 embeddings gated until pgvector: retired. The corpus is one person's; the
  multi-user starvation and ACL arithmetic that forced the gate no longer exist. The
  ANN work from PR #91 (HNSW candidates, kind weights, keyed hydration) carries
  conceptually; its index rebuilds in-process over the local store.
- D9 owner-only ACL in SQL: retired with multi-user. The actor concept survives for
  authorship and @mentions; visibility resolution is deleted, not ported.
- D10 wire and inbox carry ids only: carries. The inbox remains (mail arrivals,
  mentions from your AIs, dream output). The wire table's durable role is absorbed by
  the op log, which is what wire always gestured at; its SSE-bus role becomes
  in-process notification.
- D11 compose as outbox kind mail.send through the Stalwart smart-host: carries,
  drained by the mail module.
- D12 concrete tables, generic seams: carries as the record-schema principle. Corpora
  are concrete record types registered at the same seams (search, embeddings, links,
  citations); no generic documents table.
- D13 jmap-sync as the public crate with CursorStore/MailSink traits: carries, and is
  promoted: those traits are the seed of the module SPI.
- D14 sqlx migrations: carries in spirit. Destructive reshapes get cheap under D18:
  derived indexes are rebuilt by log replay, so "migration" mostly means bumping the
  fold version.
- D15 Zibaldone retirement: partially reversed, per Lineage above.

## Decision log

D16. **Product: a personal P2P desktop app.** Single user, single human. No hosted
deployment, no tenancy, no accounts. Delete: api/src/auth.rs, routes/auth.rs,
routes/oauth.rs, sessions, onboarding, the Visibility/user_scope machinery in
middleware and stores, hosted workspaces (store/workspaces.rs, routes/workspaces.rs,
cc_sessions/cc_messages/runtime_oauth_states), packages/runner, and the Node mirrors
(packages/api, packages/worker, packages/cli, packages/agent). The deletions are also
the security argument: v1's sharpest structural risks (the ACL ordering defects, the
benign exfiltration loop where an agent journals private mail into global scope) die
with their surfaces rather than getting fixed; the untrusted-content rendering class
does not die and is governed by explicit policy in D17. Mobile comes later; hive-core
(D17) is the layer it will reuse.

D17. **UI: Dioxus, all Rust; webview-rendered now, native-rendered later.** The app
is one process: a Dioxus shell over a new hive-core crate (store, emergence parser,
retrieval, module host, sync). Components are Rust (RSX and signals); event handlers
call hive-core directly in-process. There is no JS framework, no npm, no Node, no
Electron, and no IPC or serialization boundary between UI and core, which is also why
this is not a Tauri-plus-SPA revival: the retired SPA was a separate program talking
to a server, and this is one program. Rendering goes through the system webview
(dioxus-desktop on wry: WebView2, WKWebView, WebKitGTK), which is what earns document
fidelity everywhere hive is a document app: the journal editor (contenteditable, with
DOM selections mapping to byte-offset anchors), sanitized HTML mail, reader-mode page
captures, the calendar grid, plus IME, clipboard, accessibility, and find-in-page for
free. egui was evaluated first and rejected on this axis: it cannot render HTML at
all, and a native-webview overlay hack forfeits its simplicity without gaining
fidelity. The Solid SPA, Tiptap, and force-graph still do not port; screens are
rebuilt in RSX, and the graph view is a canvas.

Rendering untrusted content is an explicit policy, replacing v1's plaintext-only
rule: mail bodies and page captures are sanitized at ingest and again at render
(ammonia allowlist; scripts, forms, and event handlers stripped), displayed inside
sandboxed frames under a strict no-network CSP (the webview serves only the app's
custom protocol; content frames get script-src none), remote content is blocked by
default with per-sender opt-in placeholders, links open in the system browser, and
plaintext extraction remains the stored, indexed, and embedded form. Blitz (Dioxus's
native HTML/CSS renderer on Servo's Stylo and Vello) is the tracked future option:
when it matures, the same components drop the system webview without a rewrite,
which is the end state "no web frameworks" was reaching for. dioxus-mobile (webview
on iOS and Android) is the credible path for the later mobile app.

D18. **Storage: append-only, event-sourced, never overwrite.** This applies to all
storage, not just ingested data. The source of truth is a set of per-device,
single-writer, append-only op logs. Every record is immutable: journal appends,
entity creates and field updates, module-ingested documents, config changes,
tombstones, redactions. Updates append superseding records; deletes append
tombstones; nothing rewrites history. SQLite (SQLCipher, D27) holds only derived
state: current-entity tables, FTS5, and the vector/ANN index, all rebuildable by
replaying the logs through a versioned fold. Materialization resolves concurrent
field edits last-writer-wins per field, ordered by (device, seq) with a logical
clock. v1 documented honestly that append-only was "convention, not enforcement"
(journal user_scope rewrites, actor merge rewriting authorship, cascade hard
deletes); those paths get re-expressed as records (merge = a merge record the fold
applies) rather than carried as UPDATEs. Compaction, snapshotting, and GC are
deliberately deferred ("we'll figure out how to maintain this later"); the arithmetic
that makes deferral safe: personal text corpora are small (a 200k-message mailbox is
low single-digit GB of text; browser capture is tens of MB per month), so logs grow
slowly and replay stays cheap for a long time.

D19. **Hard delete: crypto-shredding.** Append-only and privacy collide exactly at
deletion, and hive will hold the most sensitive corpus a person has (mail, files,
browsing). Resolution: payload bodies (mail bodies, file text, page captures,
attachment bytes) are stored in the blobstore encrypted under per-blob content keys;
log records carry the blob reference plus the key wrapped by the master key. Hard
delete destroys the wrapped key everywhere and appends a tombstone; the log and DAG
structure remain intact and verifiable while the content becomes unrecoverable. The
tombstone propagates through sync so deleted content cannot resurrect from another
device, which generalizes the attachment redaction replay-resurrection invariant from
the v0.6.0 train into a storage-wide rule. Small metadata records are encrypted in
segment units; the shredding granularity users see (delete this page capture, this
message, this file) is the blob.

D20. **Blobstore: content-addressed blocks with a manifest layer.** blake3 ids over
encrypted blocks; FastCDC content-defined chunking for large payloads; manifest
objects (a tree of chunk hashes, itself a blob) assemble files, which gives the store
its virtual-filesystem shape. Small payloads are single-block. What this buys:
dedup (v1's 30-60% mail-attachment dedup carries), verifiable transfer, and
resumability: sync and backfill negotiate have/want by hash and restart mid-object
after failures instead of over. One addressing scheme covers attachments, file
captures, page snapshots, log segments, and embedding model files. The design aligns
with iroh-blobs (BLAKE3 verified streaming) so the store's unit is also the sync
protocol's unit.

D21. **Sync: iroh, with a BR relay anyone can replace.** Each device holds a keypair;
pairing is a ticket/QR exchange; transport is QUIC, E2E encrypted, hole-punched, with
relay fallback. BR operates the default relay and discovery service; both are small
self-hostable binaries, and a settings field points a hive at your own. Relays
forward ciphertext and see only node ids and IP addresses; we run them with no
logging, and the threat-model doc (D27) states exactly what a relay operator can
observe. Replication is the exchange of log segments and referenced blobs (resumable
per D20). Every record carries an author (actor and device), so person-to-person
sharing remains buildable later without reshaping the log; for now the peer set is
one human's devices.

D22. **Ingestion: WASM modules, implemented now.** The host is wasmtime with the
component model; the SPI is a WIT world (hive:module). A module exports: describe
(identity, config JSON schema for the settings UI, citation namespace, capability
requests), tick(cursor) for pull-based sync, and handle-push(payload) for push-fed
modules. The host provides capability-scoped imports, granted per module in settings:
http, sink-emit (records into the ingest pipeline: extract, chunk, embed, index,
provenance), cursor get/set, secret get (keychain-mediated, D27), log, and
subscriptions: long-lived streams like the JMAP EventSource are owned by the host,
which wakes the module's tick, satisfying D4's old isolation rationale inside one
process. Modules are sandboxed by construction, hot-loadable from files, and
enable/pause/configure at runtime in the UI. First-party modules are built as
components to prove the SPI, in order: mail (jmap-sync core intact, backfill cursors
and state strings per D5, compose per D11), filesystem (user-chosen roots, notify
watcher, ignore rules, MIME allowlist, extraction, [file:<id>] citations), caldav and
carddav (D23), browser (push receiver for D24, [web:<id>] citations). Fallback named
up front: if component-model tooling friction blows the budget, extism is the
replacement host with the same SPI shape.

D23. **Entities: a PIM core plus custom types.** Built-ins shrink to: journal (the
substrate, unchanged), task, mail, event upgraded to a real calendar entity (start,
end, timezone, RRULE recurrence, reminders), and contact, new. person splits: actor
stays as the slim authorship identity (you and your AIs, @mentions, dream authorship)
while contact carries the address-book payload (emails, phones, orgs), enriched from
mail correspondents and CardDAV. decision, topic, project, and phase leave the core
and ship as custom entity-type presets on the existing entity_types registry, one
click to restore; the emergence language keeps working for them through the custom
seams. We build no CalDAV or CardDAV server: the mail server (Stalwart) already
provides both, and hive connects as a client through the caldav/carddav modules.
Custom types and the pluggable-source SPI are the extension story; the core surface
is exactly contacts, mail, calendar, tasks.

D24. **Browser capture: real-time only, in-session.** An MV3 WebExtension (Chrome and
Firefox) captures readable text from the live DOM at visit time, inside the user's
existing session and cookies. It never re-fetches a URL, and there is no history
backfill of any kind: what you browse while it runs is what gets captured. A capture
stores extracted text (the indexed and embedded form) plus a Readability-simplified
HTML snapshot, sanitized at ingest, for display under D17's rendering policy. Delivery
is native messaging into hive-bridge (D25), so the app never opens a listening port
for it. Policy surface in settings: domain allow/deny lists, never in private
windows, a global pause, and an audit view of everything captured with one-click
delete (a crypto-shred, D19).

D25. **The only external API is MCP.** The Dioxus shell calls hive-core in-process; the
REST routes are deleted. One auxiliary binary, hive-bridge, provides every external
doorway: stdio MCP for Claude Code (the plugin repoints to it) and Claude Desktop
(the .mcpb repoints to it), and the native-messaging host entry for the browser
extension. The bridge proxies to the running app over a unix domain socket with
peer-credential checks; if the app is not running it says so rather than growing its
own store access. The MCP tool layer (api/src/mcp.rs) survives over hive-core minus
the auth, admin-multiuser, and workspace tools, plus new tools: ingest_activity
(D26), module management, and capture audit.

D26. **Dreaming.** A Claude Code skill, distributed through the existing
identity-artifacts system, plus one new MCP tool: ingest_activity(since), returning
per-module digests of what arrived (counts, notable items, spans) since the last
dream. The dream reviews that activity and writes journal entries tagged as dreams
using the standard emergence language: bracket tokens, anchors, and [mail:]/[file:]/
[web:] citations, so contacts, tasks, and events materialize through the one write
path (store/journal.rs) exactly as human prose does. The reflector generalizes into
this. Trigger is /dream in Claude Code, with an optional scheduled headless run
later. Everything a dream reads is module-fed content and stays framed as untrusted
input (see Risks).

D27. **Privacy: encryption everywhere, collection zero.** At rest: SQLCipher for the
derived database, per-blob keys for payloads (D19), master key in the OS keychain
with an optional passphrase (Argon2id) and a printed recovery code; losing the master
key must be survivable by design, not by luck (v1's HIVE_CRED_KEY lesson,
generalized). Module credentials (JMAP, CalDAV) move from env-var encryption keys to
keychain-wrapped storage. In transit: iroh E2E (D21). Embeddings remain local ONNX
(bge-small via hive-embed; the cross-encoder reranker ports into hive-embed as the
Node path dies), so content never leaves the machine to be indexed. No telemetry, no
analytics, no crash reporting by default, identifier-free update checks. A
threat-model document ships with Phase 1 and is honest about what remains observable:
relay operators see node ids and IPs; your mail server sees your mail; Anthropic sees
what your Claude sessions read through MCP when you use them.

D28. **Closeout and migration.** v0.6.0 ends untagged; the hosted era stops at the
merge of PR #91 and the sha can be tagged retroactively if ever needed. Phase 1
includes a one-shot importer: point it at the existing Postgres instance and a chosen
namespace, and it exports journal you authored (including formerly global entries you
wrote), your entities, links among imported items, your actors (you and your AIs),
and your mail accounts, messages, and attachments, re-expressed as authored records
with original ids preserved as aliases for citation continuity. Cross-human shares do
not migrate; there is no one to share with in a personal hive.

## Phased build plan

### Phase 1: hive-core

- Extract hive-core from the api crate: store, the emergence parser verbatim
  (parse_bracket_tokens, materialise_anchor, parse_mentions), retrieval, inbox.
- Implement the durable layer: op-log record format and fold, SQLite/SQLCipher
  derived index, FTS5 (replacing tsvector), blockstore (blake3, FastCDC, manifests),
  key management (keychain, per-blob keys, recovery code).
- Port retrieval: hybrid blend and rerank over FTS5 plus an in-process ANN index,
  carrying PR #91's shape (candidates, kind weights, keyed hydration).
- Delete D16's list. The Rust workspace becomes: core, embed, shared, jmap-sync, app
  (Phase 2), bridge, modules/.
- Build the Postgres importer (D28) and import the real namespace.

Milestone: the ported store/search test suite passes against an imported real
namespace; journal to emergence to search behaves identically to v0.6.0 for one user.

### Phase 2: the shell

- Dioxus app: journal (editor, anchor selection, autocomplete), search, tasks,
  dashboard, settings; wire the in-process notification path (the old SSE bus).
- Embed the MCP tool layer over core; ship hive-bridge (stdio MCP + UDS proxy);
  repoint the Claude Code plugin and the .mcpb.

Milestone: daily-drivable on one device; Claude Code and Claude Desktop both connect
through the bridge and journal_append/recall/semantic_search round-trip.

### Phase 3: modules, PIM, dreaming

- wasmtime host, WIT SPI, capability grants, settings UI rendering config schemas.
- Port mail onto the SPI (jmap-sync intact), including compose (outbox mail.send
  through the smart-host).
- Filesystem module; caldav and carddav modules; calendar and contact entities with
  their native views; decision/topic/project/phase become presets.
- ingest_activity tool and the dreaming skill.

Milestone: mailbox, chosen directories, calendar, and contacts are all searchable and
citable; /dream produces journal entries whose anchors materialize entities.

### Phase 4: sync

- iroh transport, pairing UX (ticket/QR), log-segment and blob replication,
  tombstone/shred propagation.
- Deploy the public relay and discovery service; write the self-host doc.

Milestone: two devices converge from cold; a transfer interrupted mid-blob resumes
where it left off; a crypto-shred on one device renders the content unrecoverable on
both.

### Phase 5: browser

- MV3 extension, native-messaging host entry in hive-bridge, capture policy UI,
  audit and delete.

Milestone: pages captured in real time are searchable with [web:] citations, and the
audit view's delete verifiably shreds.

Ordering rationale: sync waits for the record schema to settle (Phase 3 adds record
types), and the extension waits for the module host and policy surface. Dreaming
rides Phase 3 because it needs ingest activity to dream about.

## Risks and tar pits

**The UI rebuild is the schedule risk.** Sixteen SPA tabs get rebuilt in RSX, and
Dioxus is younger than the stack it replaces: expect framework friction, and note
that the Linux leg renders through WebKitGTK, the roughest of the three system
webviews and the daily-driver platform here, so it gets tested first, not last. The
journal editor stays the one genuinely hard widget (contenteditable is powerful and
fiddly); the calendar grid stops being hard in a DOM. Resist pixel-parity with the
old SPA.

**Sanitization discipline is back.** v1's XSS lesson (raw ts_headline output reaching
innerHTML) applies again the moment a webview renders mail or captured pages. The
rule is structural, not situational: content HTML never reaches the DOM unsanitized
(ammonia at ingest and at render), sandboxed frames and the no-network CSP backstop
it, and there is no just-this-once path around the sanitizer. This is the price of
D17, paid deliberately.

**Append-only is a design constraint, not a checkbox.** Several v1 paths literally
rewrite (actor merge, scope changes). Re-expressing each as a record the fold
understands is real work, and the fold's versioning discipline (replay must stay
deterministic across releases) is the part that bites late if skipped early.

**Key management is now load-bearing.** Crypto-shredding means key destruction is
deletion; it also means key loss is total loss. The keychain plus passphrase plus
recovery-code story ships in Phase 1 or the encryption story is theater.

**WASM friction is real.** Component-model tooling is young; guests are tick-driven
because guest-side async is not worth fighting; host-owned subscriptions add host
complexity. The extism fallback is named in D22 so a mid-phase host swap is a
contained decision, not a redesign.

**Prompt injection survives the pivot.** Mail, files, and web captures feeding Claude
sessions are attacker-influenced input; the single-user model removes the cross-user
leak, not the injection. Every module-fed MCP surface keeps untrusted-content
framing, and dreams treat ingested text as quotes, not instructions.

**iroh and relay ops are a new operational surface.** Pin versions, write the relay
runbook, and decide the relay domain early: pairing tickets embed relay defaults, and
changing them later churns every paired device.

**Mobile stays deliberately unresolved.** dioxus-mobile (webview on iOS and Android)
makes shared components credible, but its tooling is young. hive-core stays
shell-agnostic so the mobile decision is free when it arrives. Do not let desktop
code grow Dioxus types below core.

## Open questions (not blockers)

1. Recall and dreams reading mail/web/file content: default-on for a personal hive,
   or a per-module "visible to AI" toggle? (Leaning toggle, default on for mail,
   off for browser.)
2. Which v1 mail questions still matter: read-only JMAP credential (still worth it),
   sent-identity allowlist for compose (carries to Phase 3).
3. Branding: does "hive" survive the pivot? A rename is cheaper now (tokens and
   cookies die with auth) but still touches crates, binaries, env vars, and images.
4. Relay and discovery domain, and where their ops live (same box as mail, or apart).
5. Importer scope for Maggie or other household members: a second personal hive
   imports its own namespace from the same Postgres; confirm that is the model rather
   than any shared instance surviving.

## Next steps

1. Review this record; contest any decision in the log before Phase 1 code starts.
2. Open the Phase 1 PR series on this branch: hive-core extraction first (pure move),
   then the record format and fold, then the FTS5 port, then the importer.
3. Mark the BookStack "Hive Direction: Mail as Substrate" page historical with a
   pointer here; update the Zibaldone book note, since D18 revives its append-only
   idea on hive's substrate.
4. Decide the relay domain (open question 4) before Phase 4 so pairing defaults are
   stable from the first build.

# Hive Direction v2.1: The Node Amendment

Status: adopted 2026-07-24. Amends — does not supersede — "Hive Direction v2: Personal
P2P Desktop" (decided 2026-07-10). Decisions here start at D29; v2's D16-D28 stand
except where an amendment note below says otherwise, and D17, D18, D19, and D20 are
explicitly untouched. Written against main plus feature/bridge-proxy (open PR #129),
the four v2.1 design documents of 2026-07-24, and their three-lens critique (privacy,
simplicity/phasing, CI realism); every blocker and major critique finding is
incorporated below, and the one rejected sub-fix is flagged in-line. No frozen format
(envelope v1, segment format, blockstore derivations, wrap format) changes anywhere in
this amendment; every new artifact is an explicit additive format decision with golden
fixtures. This file lands as an amendment appended to docs/DIRECTION.md via its own PR.

## Summary

Hive gains hive-node: a headless always-on peer embedding the same hive-core the
desktop app does, deployed hub-and-spoke. Identity is an email-shaped address
(`nate@bierlysmith.com`); the node is found through DNS across public and private
address spaces alike (D35), and pairing collapses to "enter your address plus a
one-time code"; NAT traversal and pairing tickets become optional later work. The replication protocol — new, binary, over mTLS
TLS 1.3 on TCP — IS the device API; the node is a peer you never turn off, not a
server with a REST surface; MCP stays the only agent API. Each domain lives on a node
at one of two tiers: blind (sealed segments and ciphertext blocks, no keys — verify,
dedup, and serve by content address) or trusted (holds the domain master, runs 24/7
ingestion and embeddings, serves a web head sharing the desktop's RSX components).
Identity gets three layers — actor strings for authorship, per-device keypairs for
transport, domain custody as policy — carried in a new signed per-domain control log;
OIDC exists only at the node's enrollment/console edge, and v2.1 ships zero OIDC code.
Sharing becomes per-recipient re-wrapped blob keys with node caching. Tenancy is
partition, not ACL: `tenants/<tenant>/domains/<domain>/` on the node's disk, the data plane
tenancy-blind; tenant 1 is the household (Nate and Maggie as two personal-custody
domains), and every enterprise mechanism — escrow, KMS, IdP federation, member records
— is designed on paper and deferred to tenant 2. The first shipped milestone is
deliberately the smallest honest one: a blind backup node, every device replicating
one way to a box that cannot read any of it; bidirectional merge, the trusted tier,
the web head, and sharing follow in that order.

## Amendment notes on v2 decisions

**D16 — amended: the personal app gains a personal server; nothing multi-user returns
below the node edge.** What changes: "no hosted deployment, no tenancy, no accounts"
relaxes to a self-hosted always-on peer (D29) and tenancy-by-partition (D33); "single
user" becomes single human per domain, multiple domains per household. What survives:
everything D16 deleted stays deleted — no sessions, tokens, or OIDC types below the
node binary, now enforced by a grep fence (D30); the desktop app remains fully
local-first and daily-drivable with the node down; multi-human operation exists only
as sibling domains plus explicit sharing (D31), never shared state or ACLs.

**D21 — amended: hub-and-spoke first; iroh demoted to optional transport.** What
changes: the stable-name node replaces NAT traversal and ticket/QR pairing; discovery
is DNS (D35), not a discovery service; transport is mTLS TLS-over-TCP; the relay and
discovery binaries are not built; pairing is the domain's address (resolved per D35)
+ one-time code + a mandatory short-auth-string (D30). Devices
talk exclusively to their node: no device-to-device and no device-to-foreign-node
paths exist in v2.1, which closes the designs' three P2P-shaped open questions rather
than letting them creep back one PR at a time; reopening any of them is a future
decision record. What survives: replication as the exchange of log segments and
referenced blobs, resumable and verifiable; device keypairs, formalized in D30; E2E in
transit, and E2E at rest against blind nodes; "person-to-person sharing buildable
later without reshaping the log" — D31 now builds it. The relay-operator observability
sentence survives, considerably expanded, as threat-model delta 5. The message layer
is stream-generic, so iroh or QUIC slot underneath later without a wire change — the
demotion is reversible.

**D27 — amended: custody becomes per-domain policy; the E2E claim gets tiered.** What
changes: "master key in the OS keychain" generalizes to domain custody — personal
(keychain plus recovery code, unchanged, the only mode v2.1 ships), node-held (trusted
tier: KEK-sealed master file, TPM-backed where available; a file KEK on the same disk
is honestly obfuscation, not custody separation), or tenant-escrowed (paper only,
tenant 2). "Content never leaves the machine to be indexed" becomes "never leaves
machines holding your domain master" — a trusted node computes embeddings. What
survives: SQLCipher, per-blob keys, keychain + passphrase + recovery for personal
custody, local ONNX, zero telemetry; D19's crypto-shred mechanics untouched, with the
multi-party rider that shred now chases every grant and every replica and is
policy-mediated against any party holding master (threat-model delta 1, 10).

**D17, D18, D19, D20 — untouched.** D29 implements D18's "last-writer-wins per field,
ordered by (device, seq) with a logical clock" sentence rather than amending it. D32
makes D17's sandboxed-frame half mandatory on the web head and backports it to desktop
with the first mail-read PR. One overclaim is corrected in passing: the blockstore
header's "no confirmation-of-file attack" comment is true of the hash but not of
FastCDC chunk-size fingerprints (delta 5); the comment is fixed in the PR that
documents it.

**D25 — stands, one boundary made explicit.** The bridge remains a local,
dependency-free UDS pump to the running app. No node MCP endpoint and no remote bridge
transport ship in v2.1; agent access to the node, if ever wanted, is its own D25
amendment — not a side effect of a test plan.

## Decision log

D29. **hive-node: hub-and-spoke replication; the server is a peer you never turn
off.** One new crate/binary embedding hive-core exactly as the app does (Store::new is
UI-free and proven headless). Transport is TLS 1.3 over TCP with mTLS device
certificates — not iroh, not QUIC: the stable URL removes NAT traversal (iroh's core
value), rustls is already in-tree, and the framing layer is generic over byte streams
(the serve_bridge_connection discipline) so QUIC or iroh can slot underneath later.
Two storage tiers per domain. Blind: a SegmentVault — verbatim sealed segments and
ciphertext blocks in store shape, no Store, no keys; structure and content addresses
verify keylessly. Sealed segments are write-once in the vault: a differing re-upload
of an existing (device, start_seq) is an integrity alarm propagated to trusted peers,
never an overwrite, and a growing tail may only extend with its byte prefix intact.
Trusted: a full Store per domain, minting its own device id like any peer; ingest
treats content divergence at an already-folded (device, seq) as a poison event —
quarantine and loud alert, never a silent fold. Segment transfer is verbatim file
bytes, always: boundaries feed key derivation and rotation is writer policy, so
byte-identical transfer is the only primitive that preserves the determinism contract.
Blocks move by bare ciphertext id, blake3-verified keyless on write; want-lists come
from the keyed side; blob-complete is a keyed-side judgment tracked per block.

Merge semantics implement D18's stated rule: Lamport lc maintenance at commit and
ingest, LWW-per-field arbitrated by (lc, device) through a derived field-clock shadow
table so the fold is order-independent and every interleaving converges; ghost-create
tolerance for update-before-create; deterministic slug-collision resolution so
independent emergence can no longer brick store open; a fold-applied event on the
currently subscriber-less bus plus the first real UI subscription; and a post-fold FTS
re-stamp pass, since mail FTS does not rebuild from replay. All under FOLD_VERSION 4 —
the user_version rebuild is the designed shipping vehicle; no log bytes change. lc
stays a writer assertion and the threat model says so; control-adjacent arbitration
(the revocation cutoff) uses hub receipt order, which the node records. Rejected from
critique: clamping lc at ingest to own-max-plus-bounded-skew — a Lamport clock
legitimately runs far ahead after offline work, so we declare the clock untrusted and
pin arbitration determinism with a backdated-lc golden corpus instead of pretending it
is honest.

Shipping order is part of the decision, not a footnote: the core sync read surface
(public segment/heads enumeration, keyless header parse, block APIs — foreign ingest
via ingest_segments follows with the trusted tier) and the
blind vault ship first, and the first milestone is "blind backup node: every device
backed up one way; restore = drop files and heal." One-way backup folds nothing
foreign, so FOLD_VERSION 4 gates only bidirectional sync and the trusted tier — not
first value. The trusted tier then runs the core-owned scheduler (mail tick, the new
mail-embed drain, doorbell, reaper) — the same loop the desktop runs, moved into
hive-core in one owned PR (see Risks); mail credentials are provisioned against the
node's own store, since the vault is runtime state that never replicates, amending the
Phase-3 "fold the vault into the OS keychain" note for headless; the HNSW swap (MSRV
1.85) lands in the same stage, before node-scale corpora outgrow the brute-force
envelope. Custody is visible, not configured: a
node holding a domain master or recovery blob is recorded as a signed control-log fact
(custody.grant, D30) that folds and surfaces on every device — a tier change is an
alert peers see, not a node.toml edit they cannot. Deployment is a container with one
/data volume and quadlet/compose examples; backup guidance explicitly excludes — or
separately encrypts under an offline key — the node KEK, the credential vault, and
recovery blobs, because snapshotting them beside the ciphertext collapses at-rest
encryption into one exfiltratable artifact (delta 9).

D30. **Identity: actor, device keys, domain custody — and a dedicated signed control
log.** Actor stays a free string: authorship, not authentication. Every device
generates Ed25519 (signing) and X25519 (agreement) keys on-device; private halves
never leave it; transport auth is the enrollment-pinned device certificate. The frozen
envelope's device field stays a free string — binding is a record, not a format
change. Control records live in a new per-domain control log at
`<data_dir>/control/<device>/` (beside `log/`) — same frozen envelope v1, same segment
format, but a
new segment-key derivation context (hive-ctl-segment-key-v1), declared as an additive
format decision with its own goldens: reusing the data-log derivation would give a
control and a data segment identical keys at the same (device, start_seq), an
undeclared weakening of a frozen derivation's stated uniqueness. Why a separate log
rather than new data-log kinds: kind::ALL is enforced at append and scan
(core/src/oplog/writer.rs, reader.rs) and heal walks every log dir at open, so one
new-kind record in the data log bricks every un-upgraded device at boot; old builds
never look under control/, which is the entire migration story — no fleet-wide version
gate, the 11 data-log goldens untouched, and a compat test pins that a store dir
containing control/ opens cleanly on current heal. The v2.1 control set is exactly
{domain.genesis, device.add, device.revoke, custody.grant}; share.* widen it when
sharing lands (D31); node.* are reserved for multi-node (D36); member.* stay on
paper (D33).

Signing: the domain authority key derives from master (a blake3 derive_key context),
recorded in domain.genesis so verifiers never need master. The signed unit is the
exact transmitted bytes: payload = { body: a CBOR byte string containing the encoded
body, signer_pk, sig over "hive-ctl-v1" ‖ the byte-string contents }; every body
embeds (domain_id, monotonic control epoch, prev-statement hash); verifiers never
re-encode — frozen with goldens under a fixed test keypair and fuzzed before the first
golden is cut. The honest cost, stated in the threat model: anyone who ever held
master can forge control records, so a stolen-then-revoked device that held master
retains signing power until master-plus-authority rotation — which is roadmap with a
milestone (before any second tenant), not "deferred". Until then, revocation is hub
enforcement: control updates enter only over authenticated sessions from
already-pinned, unrevoked devices or through the enrollment ceremony; the node keeps a
monotonic per-domain control epoch and refuses regressions; revokes are permanent
tombstones in node state; receipt order is the cutoff. The identity design's keyless
exported-statement verification surface is deferred along with third-party relays: in
hub-and-spoke the node is the enrollment endpoint and pins pubkeys first-hand.

Enrollment, personal custody (the only kind v2.1 ships): the enrolling device takes
the domain's address (`nate@bierlysmith.com`, resolved to node candidates per D35;
raw `hive://host:port#<node-pk-fp>/<code>` survives as the resolver-less fallback)
plus a code — single-use, short-TTL, hashed at rest.
Approval requires an existing device with a mandatory short-auth-string computed over
{new ed25519_pk, x25519_pk, node_pk} and compared on both screens — without it a
relaying node can substitute keys and receive master; master travels only as a sealed
box to the SAS-verified X25519 key. Every device.add fold raises a first-class alert
on all peer devices ("new device joined — was this you?"), and enrollment writes an
audit control record. Node-solo approval exists only under escrowed custody, which
v2.1 does not ship. Recovery: the existing recovery code plus a node-held recovery
blob (the Argon2id container keyed by the code), fetch throttled and audited; the node
never sees master. Zero OIDC code ships in v2.1, and the bright line is mechanical: a
grep fence over dependency and namespace tokens (openidconnect::, jsonwebtoken, the
oauth2 crate — not bare words, since "oauth_token" already exists legitimately in the
credential vault) plus a cargo-metadata walk asserting those crates resolve nowhere
outside node/ — landing with an empty allowlist because there is nothing to allow.

D31. **Sharing: per-recipient rewrap; a grant is a serve stub plus a sealed payload.**
Granularity is the blob — segments are master-wrapped, the wrong layer — and text
items are materialized into a blob at grant time, so shares are snapshots and one
mechanism covers journal entries, messages, and files. Crypto: X25519 static-static
ECDH derives a pairwise wrap key; the frozen 72-byte wrap format is reused verbatim; a
wrap_alg field reserves the HPKE upgrade. The domain sharing keypair derives from
master and is published in genesis/control — no new custody object, no new exchange
ceremony, and the same forgery caveat as the authority key, stated. The grant is two
artifacts, split before any golden is cut: a node-facing serve stub — grant_id, block
ids, recipient transport key, expiry, revocation reference, authority signature; the
minimum needed to pin, serve, and revoke — and a recipient-sealed payload (metadata
snapshot plus wrapped content key) the node relays opaquely. A single readable grant
would hand every node, blind ones included, the sharing graph plus item metadata; the
split keeps the blind tier's promise honest, and the residual leak — graph shape and
grant timing — is stated in delta 5. Serving is authorized per request: recipient
identity, grant_id, block id ∈ that grant's set, revocation checked at serve time — a
live grant is never a bare-id oracle into the sharer's blockstore. Random-key
(non-convergent) put() ships in v2.1 and is the default for any blob that becomes
grant-referenced and for all re-ingestion after a shred — the only real answer to the
convergent trap, where identical plaintext re-derives the identical key and a revoked
recipient's retained key would decrypt future re-ingestions; the price is dedup loss
on exactly those blobs, paid deliberately. Recipients materialize through their own
typed write path: fetch ciphertext by id, verify keylessly, unwrap, re-put under their
own master, one share.received record with provenance — no foreign records ever enter
the recipient's log, no cross-domain dedup (already ruled out by design), and
recipient-side crypto-shred works today. Revocation is cooperative and delivered
plaintext is irrevocable — both stated plainly; shred of shared content enumerates and
revokes every live grant first. Sharing is sequenced after the trusted tier as its own
format decision; the stub/sealed split and the random-key default are decided now so
nothing wrong gets frozen in the meantime.

D32. **Web head: the same RSX, CSR-only, in-process on the trusted node.** Dioxus 0.6
web, client-side rendering only — no SSR, no hydration, which is 0.6's documented
flaky class — matching what desktop effectively does today (use_resource polling). One
Dioxus version rules both heads; 0.7 is re-A/B'd against the WebKitGTK blank-page bug
as its own PR, never a version fork. The crate split: ui (shared screens and
components out of the 10,307-line app/src/main.rs), app (desktop shell: wry glue,
keychain boot, drivers), web (thin CSR shell); the node serves the wasm bundle and the
RPC. The data seam is `Arc<dyn HiveUi>` — typed DTO methods for exactly the v1 web
screens; desktop implements it over Store directly, web over server functions, with
hand-rolled axum JSON named as the drop-in fallback; a Platform context absorbs clock,
downloads, and open-external. The web head runs in-process on the trusted node: the
bridge protocol is explicitly wrong for it (no server push, sequential frames,
JSON-in-a-text-block) and stays the agent API. Web writes commit under the node's
device id and replicate normally, so FOLD_VERSION 4 gates web write-enable; v1 ships
read-mostly plus plain-text journal append, which is create-only and conflict-free.
Auth v1 is tailnet plus required identity: Tailscale whois maps caller → member →
authorized domain, deny by default — interface binding alone is not authentication,
and one household member on the shared tailnet must not reach the other's domain. The
listener refuses non-tailnet binds (unit-tested), web logins are logged, and tailnet
reachability from a non-tailnet client is a stated manual test target. OIDC (Zitadel)
cookie sessions are v2, node-binary-only, with session lifetime and revocation defined
before any cookie ships. Web v1 renders no untrusted HTML; D17's sandboxed-frame half
becomes mandatory in a browser — iframe sandbox srcdoc, camo-style image proxy,
document CSP, SameSite=Strict plus Origin checks — and lands with the first mail-read
PR, not v1; desktop adopts the same rail afterward for D17 parity. The sentence "a web
head makes a trusted node a remotely reachable plaintext oracle" goes into the threat
model verbatim (delta 7).

D33. **Multi-tenancy by partition; enterprise custody is designed, not shipped.** On
disk: `tenants/<tenant>/domains/<domain>/`, each leaf a verbatim store data dir with its own
flock, master, and derivations — per-domain masters already kill cross-tenant
convergent-dedup side channels. The data plane is tenancy-blind, fenced mechanically:
tenancy identifiers (Tenant, tenant_id, tenants/) banned under core/ — identifiers,
not the English word, which legitimate comments will use. A tenant is grouping plus
quotas plus console scope; tenant.toml carries name, tier, and quotas only. Tenant 1
is the household: two personal-custody domains, and v2.1 exercises no escrow path —
Maggie enrolls her own devices through the same personal ceremony. Everything
enterprise — EscrowKeySource, per-tenant KMS roots, IdP federation,
member.add/member.remove, the OIDC console, offboarding handoff/retain/shred — remains
design-on-paper, deferred to tenant 2, with the hardening bill written down now so it
cannot be waved through later: distinct per-tenant KMS principals (no single
credential that decrypts every tenant), enforced per-tenant module-host and web-head
processes, and per-tenant audit of every KMS unwrap. Rationale: D16 retired tenancy
wholesale; this amendment restores the partition because it is cheap now and painful
to retrofit, while golden-freezing member and escrow schemas with zero consumers would
be exactly the speculation the closed-set discipline exists to prevent.

D34. **Testing: every threat-model sentence maps to a test.** Four tiers — unit; smoke
(real binaries, real sockets); DOM snapshots (dioxus-ssr plus insta, the zero-flake
net under the crate split); screenshots (real pixels in a digest-pinned WebKitGTK
container) — gated by the repo's existing loud-skip idiom, no new tooling. Conventions
citable by name in every PR: GOLDEN-BYTES applies to on-disk artifacts only — control
kinds and signed payloads under a fixed test keypair, the control-segment derivation,
the grant stub and sealed payload, the recovery blob; replication wire frames get
round-trip and hostile-decode tests instead, until proto v1 is in a tagged release,
because the wire churns during development and "frozen" must keep meaning frozen.
HOSTILE-DECODE: proptests over every network-facing decoder before its first golden —
Err, never panic; lengths capped before allocation. CONVERGE-ORACLE: canonical-dump
byte equality across seed-randomized interleavings, including the backdated-lc corpus.
ADVERSARIAL-SMOKE, new: every statement in the threat-model delta maps 1:1 to at least
one test or a written "untestable because X" — revoked-cert reconnect refused,
stale-epoch control replay refused, wrong/expired/reused pairing codes, SAS-mismatch
abort, grant serve-after-revoke refused, quota enforcement at ingest, mid-tail AEAD
garbage rejects-and-resyncs without bricking, Forget-queue crash/restart resends
exactly once, three-party shred propagation. The enrollment/mTLS loopback harness
(pinned-accept, unknown-cert reject, revoked-after-fold reject) is the deliverable of
the crypto-stack spike and an acceptance criterion for the listener and trusted-tier
PRs. Prerequisite zero, before any UI tier: the app gains a HIVE_MASTER_KEY_FILE seam
(a 64-hex file honored before the keychain — the same format the node and test helpers
use) or every screenshot renders the boot-failure screen; the HIVE_DATA_DIR override
follows. Cross-binary scenarios receive binary paths from the CI job via env, since
CARGO_BIN_EXE is crate-local. Gating: deterministic suites (unit, smoke, the wasm
build gate, DOM snapshots) are required from day one; the pixel suite runs advisory
for two weeks of measured flake, then becomes required; fuzz targets and the perf
canary (nightly, deliberately loose order-of-magnitude bound) never gate merges. When
web screenshots arrive with the web head, they pin their browser exactly as the
desktop tier pins WebKitGTK — an unpinned Chromium over committed pixels is the
flaky-then-deleted pattern with a different logo.

D35. **Addresses and discovery: email-shaped identity; DNS-first, multi-path, never
authoritative.** The user-facing identifier is an *address*, `<localpart>@<zone>`
(`nate@bierlysmith.com`): the localpart names the hive domain, the DNS zone names the
tenant — the on-disk `tenants/<t>/domains/<d>/` partition made nameable, and for
enterprise exactly the email property (control the zone, control the namespace).
Terminology is part of the decision: "address" for the identifier, "zone" for DNS,
and "domain" keeps meaning the key domain — the collision ends here, before it
infects schemas and UI copy. The canonical address is recorded in `domain.genesis`
and carried in grants, which is why this decision lands now, before those schemas
take goldens.

Discovery is DNS SRV in the standard DNS-SD-compatible shape —
`_hive._tcp.<zone>. IN SRV <prio> <weight> <port> <target>` — and it deliberately
yields a **candidate set, never a single truth**. Targets may resolve, in one answer
or across split-horizon views, to every address space the node inhabits: public
A/AAAA, tailnet (Tailscale MagicDNS), ZeroTier (ZeroNS), LAN RFC1918/ULA. Publishing
private addresses in public answers is legal, useful, and chosen per zone with delta
14 in view (split-horizon preferred; the household zones already run it). The same
record shape over mDNS (`_hive._tcp.local`) gives zero-config same-LAN discovery —
default-on for personal deployments, off in the enterprise paper design. Dialing is
happy-eyeballs across the candidate set: SRV priority/weight orders the stagger,
first candidate to complete pinned-key mTLS wins, last-good path is cached per
network. The invariant over all of it: **DNS and mDNS are addressing, never
authentication** — the enrollment-pinned node key decides, so a spoofed answer, a
hijacked zone, or a hostile LAN beacon degrades to a failed handshake and a skipped
candidate (delta 13), and discovery never introduces a key.

Registration is a node-side publisher (`provider = rfc2136 | cloudflare | none`): on
start and on address change it enumerates its interface classes (public, tailnet,
ZeroTier, LAN) and upserts the SRV plus per-class A/AAAA records — RFC 2136 with a
TSIG key as the standard path, the Cloudflare API as the pragmatic one, and
overlay-native DNS (MagicDNS, ZeroNS) is discovery for free where it already exists.
An optional TXT fingerprint of the node key is published as a hint — meaningful only
under DNSSEC, never a substitute for the ceremony. The DNS credential joins the
node's concentration point and is scoped to exactly the hive names (record-scoped
API token, or an RFC 2136 update-policy limited to `_hive._tcp.<zone>` and the
node's own A/AAAA) so node theft cannot rewrite MX (delta 13). Federation rides the
same rail later: a cross-zone share resolves the recipient's node through their
zone's SRV — the MX pattern — settling the transport half of open question 14; first
contact with a foreign zone is TOFU plus grant-carried key confirmation, with
TXT/TLSA under DNSSEC as a hint. The web head is unaffected: multi-path dialing is
the sync plane's affair; web-head v1 auth remains tailnet-whois regardless of which
path sync took.

D36. **Multi-node and thin clients: designed now, shipped later; truth stays the
logs.** v2.1 ships exactly one node per domain — but the formats stop pretending it
is forever. First, the framing that governs everything here: **"source of truth" is
not a place.** Each device is the sole authority for its own log (it mints the
gapless seq and the hash chain; no node can rewrite or reorder, only hold and
forward). A node is the *anchor*: the most complete, always-on replica — everything
"source of truth" means operationally (all data, always reachable, restores flow
from it, new devices bootstrap from it) with none of the server-authority semantics.
Clients are never caches: the smallest client is still the authoritative writer of
its own log. What thin gets named instead is a **partial replica** (phone-class,
future): control log plus recent segments held locally, older segments and blobs
hydrated on demand from a node, writes local-first and pushed like any device.

Multi-node, when it comes, is the same protocol spoken node-to-node. The data plane
multi-nodes for free: sealed segments are write-once and single-writer, so two nodes
converge by union, and the derived state converges through the same
order-independent fold — no consensus, no quorum, no election. The control plane is
the honest bill, and it is written down now: revocation's "hub receipt order" is one
timeline only while there is one hub, so multi-node requires epoch max-merge with
revoke-dominates between nodes, Forget-queue propagation so shreds chase every node,
and a node roster in the control log. `ctlkind` therefore reserves `node.add` /
`node.remove` alongside `share.*` — reserved, not defined, so the v2.1 schemas do
not fight the extension. Selection is the D35 dialer generalized: candidates from
every rostered node, preferred by measured RTT with SRV priority as override,
last-good cached. And the cheap path is stated so nobody builds protocol where files
suffice: a blind vault is verbatim write-once files — N offsite backup nodes are
achievable on day one by replicating the vault directory with ordinary tools (rsync,
syncthing, ZFS send); protocol-native node-to-node earns its keep only at the
trusted tier and the control plane.

## Threat-model delta

THREAT-MODEL.md must gain these statements (near-verbatim; each maps to an
ADVERSARIAL-SMOKE test or a written untestability note per D34):

1. Tier defines blast radius. E2E-against-the-node holds only for blind-tier domains.
   A trusted-tier domain's master is held by the node: compromise of the node, its
   disk, or its backups yields that domain's full plaintext and its control-plane
   signing capability. Custody "user" vs "escrowed" determines who else holds master,
   not whether the node does. A node KEK sealed on the same disk is obfuscation, not
   custody separation; TPM-backed sealing is preferred even for tenant 1.
2. Control-plane forgery. The authority key derives from master: anyone who ever held
   master — including a stolen-then-revoked device — can sign control records
   indefinitely. Device revocation is hub transport enforcement, advisory against a
   master holder, until master-plus-authority rotation ships (roadmapped, pre-tenant-
   2). Two master holders can mutually revoke; recovery from that lockout is the
   recovery code plus re-enrollment, documented.
3. Clocks are assertions. lc and ts are writer-controlled; the revocation cutoff is
   hub receipt order, never record clocks; LWW arbitration over lc is pinned
   deterministic (backdated-lc corpus), not made honest.
4. Revocation protects future data only. A revoked device keeps everything it already
   replicated, and per (2) keeps signing power if it held master.
5. Blind-node observability, full enumeration (expands D21's relay sentence): device
   set and pubkeys, device names carried in control traffic, connection times and IPs,
   segment and block counts and sizes, per-frame record sizes and write cadence — a
   behavioral fingerprint of journaling and mail activity — deletion timing via
   Forget, the sharing graph and grant timing via serve stubs, and confirmation of a
   candidate known file via FastCDC chunk-size fingerprints. Size bucketing/padding is
   a deferred mitigation, stated rather than solved.
6. Hub completeness and freshness are unprovable. Append-only verification catches
   tampering, not withholding: a hub can serve a truncated-but-valid view. Shipped
   mitigations — monotonic control epoch with refused regressions, pairing payloads
   carrying the current control head, per-peer last-synced state surfaced in the UI —
   are detection aids, not prevention.
7. The web head makes a trusted node a remotely reachable plaintext oracle. Tailnet or
   session compromise yields that domain's full read and write until revoked, and
   web-head writes replicate under the node's device id, indistinguishable downstream
   from device writes. Logins are logged; sessions (v2) get defined lifetime and
   revocation before any cookie ships.
8. A node-held recovery blob reduces master-key security to recovery-code strength
   plus node-side throttling and audit.
9. The node is a concentration point. It holds KEK-sealed masters, recovery blobs, and
   the credential vault (live third-party mail credentials) beside all ciphertext.
   Backups of the node tree must exclude — or separately encrypt under an offline key —
   the KEK, the vault, and recovery blobs; node theft is a stated blast radius that
   includes live mail credentials.
10. Sharing limits. Delivered plaintext is irrevocable; revocation is cooperative;
    shred of shared content must chase every grant copy and cannot reach recipient
    replicas. Convergent encryption re-derives identical keys for identical plaintext,
    so grant-referenced and post-shred blobs use random keys by default.
11. Escrow, when it ships (paper until tenant 2): the TCB for an escrowed domain is
    the node operator plus the tenant KMS controller plus the tenant IdP administrator
    — any one of them reaches plaintext or can enroll a reader. Tenant isolation in
    v2.1 is partition plus at-best process separation; unwrapped masters coexist in
    one address space until tenant-2 hardening.
12. Enrollment trusts the ceremony. Without the mandatory short-auth-string
    comparison, a relaying node can substitute keys and receive master; every
    device.add is surfaced as an alert on all peer devices.
13. Discovery is unauthenticated by design. SRV/A/AAAA answers, mDNS beacons, and
    DNS-published fingerprints are hints; only the enrollment-pinned node key
    authenticates. Compromise of the zone, the resolver path, or the local network
    yields misdirection — a failed handshake, a skipped candidate, at worst denial
    of sync — never a peer. The node's DNS-publisher credential can rewrite its zone
    if over-scoped: it is confined to the hive names exactly (record-scoped token or
    RFC 2136 update-policy), and node theft with that credential still cannot touch
    MX or other records.
14. Discovery publishes topology. SRV/A/AAAA records naming tailnet, ZeroTier, and
    LAN addresses disclose internal address space to whoever can query the zone,
    and mDNS announces hive's presence to the local network. Both are chosen per
    zone (split-horizon or omission for the public view; mDNS default-on personal,
    off enterprise), stated rather than solved.

## Risks addendum

**The trusted tier is the trust story's sharp edge.** It exists so the node can ingest
and embed 24/7, and it puts a domain's plaintext on an always-on box. The mitigations
are structural, not rhetorical: blind tier ships first, trusted is opt-in per domain,
custody is a folded control-log fact every peer displays, and the threat model speaks
in tiers, not custody labels.

**FOLD_VERSION 4 is the deep water.** Order-independent LWW, ghost creates, and slug
tolerance are fold surgery under a convergence oracle, and they gate every
bidirectional feature (trusted tier, web writes, sharing). The blind backup milestone
exists precisely so the program ships real value while this bakes.

**Control-plane debt is real until rotation.** Authority-from-master makes every
master holder a potential forger; hub receipt-order enforcement is a bouncer, not
cryptography. The debt is stated (delta 2) and rotation carries a milestone — if that
milestone slips past the first non-household tenant, stop and build it.

**Two workstreams collide in one 10,307-line file.** PR #129 merges before any
app-touching v2.1 PR opens; the driver-scheduler move into hive-core is one PR with
one owner, named the scheduler — "module host" stays reserved for Phase 3's wasmtime
work, which later embeds at the same seam.

**Format fan-out tests the closed-set discipline.** This amendment mints five new
frozen artifacts (control kinds and signed payload, the control-segment derivation
context, grant serve stub, recipient-sealed payload, recovery blob). Each is additive,
each gets goldens before first use, none touches envelope v1 or the existing
derivations — and each lands through its own format-decision note, not inside a
feature PR.

**Pixel suites die young unless pinned.** The screenshot tier stands only on the
digest-pinned container, the known WebKitGTK workarounds (DMABUF renderer off,
animations and caret frozen, fixed fonts and clock), and the master-key/data-dir
seams; DOM snapshots sit underneath so the crate split is never hostage to the pixel
harness.

## Open questions

Retired from v2: #4 (relay and discovery domain — no relay is built; the stable name
that matters is now the domain's address and the node key, per D35) and #5
(Maggie's import — resolved: her own personal-custody domain under the household
tenant, importing her own namespace; no shared instance survives). v2's #1
(AI-visibility toggles), #2 (mail credential hardening), and #3 (branding) remain open
and unchanged. Resolved by this amendment, previously open in the designs: the control
records' home (dedicated control log), enterprise and OIDC scope (zero in v2.1), the
hub boundary (hub-only), Maggie's custody (personal), web v1 writes (journal append
only), sandbox-rail timing (first mail-read PR), random-key put (ships, default for
shared and post-shred), node MCP (not in v2.1), keyless statement export (deferred
with third-party relays).

New, numbered continuing from v2:

6. Rotation mechanics: what master-plus-authority rotation actually is (epoch-keyed
   re-wrap of blob keys and segment keys, or full re-encryption) and what a rotation
   record looks like — the remediation for delta 2; its own decision record, due
   before tenant 2 and ideally before sharing ships.
7. Legacy clocks: under FOLD_VERSION 4, arbitrate pre-v2.1 histories (lc=seq) against
   true-Lamport records as-is, or epoch-tag old records during the rebuild?
8. Revocation depth: hub transport cutoff only, or additionally refuse ingest of seqs
   first seen after the revoke's hub receipt? (Receipt-order bookkeeping now exists
   either way.)
9. Recovery-blob fetch policy: throttle-plus-audit only, or gated behind approval from
   a surviving enrolled device?
10. Crypto stack spike outcome: dalek family vs aws-lc-rs, and rustls raw-public-key
    vs self-signed-cert carrier — one spike PR before the listener lands, delivering
    the tls_loopback harness as its artifact either way.
11. Vector strategy: recompute-per-replica stands for v2.1; does a low-power device
    class eventually force node-computed vector distribution (a new non-record sync
    surface with model negotiation) onto the roadmap?
12. Windows: the TCP transport incidentally enables the first Windows-capable
    bridge/sync path — pull into scope, or keep deferred with named pipes to post-2.5?
13. Dioxus 0.7 re-A/B timing: before the HiveUi trait refactor lands, or after web v1
    ships?
14. Offline-recipient grant delivery: a durable node mailbox with acknowledgements, or
    best-effort relay — decide before the first cross-domain share. (The transport
    half is settled by D35 — the recipient's node resolves via their zone's SRV, the
    MX pattern; what remains open is delivery semantics.)
15. Segment rotation threshold: per-writer policy stands (tail streaming absorbs
    latency), or standardize lower so whole-file re-verification of large tails stays
    cheap?
16. Multi-node revocation semantics (D36): when nodes sync control state, is a
    device's cutoff the earliest hub receipt across nodes, or per-hub until epoch
    merge — and does a lagging node's acceptance window need a bound? Its own
    decision record, due with the first second-node deployment.
