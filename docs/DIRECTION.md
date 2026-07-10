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
