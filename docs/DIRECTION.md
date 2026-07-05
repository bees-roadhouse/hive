# Hive Direction: Mail as Substrate

Status: reviewed and decided, 2026-07-01. Written from a full read of the codebase at
0.5.0 (branch `feat/hosted-claude-code-workspaces`), a five-position design panel, and
an adversarial verification pass against the code. Nothing in this document was taken
from the README or older design docs on faith; every load-bearing claim carries a
file:line citation.

## Summary

The mail vision survives review. The mental model it was written against does not.

Mail as an ingested corpus is the right move and Hive can carry it, but the design has
to target the system that exists: a journal-first entity store on PostgreSQL with
nanoid TEXT ids, brute-force BYTEA vector search, and a Solid.js SPA. There is no
block model, no UUIDv7, no pgvector, no client-side SQLite, and no Makepad anywhere in
the repo. The phrase "JMAP-to-blocks bridge" describes a bridge to a system that was
never built.

The decisions, in one paragraph: mail becomes a new first-class entity with its own
tables (not journal entries, not wire events, not a generic documents table), synced
by a third long-lived binary (`hive-mail`) that shares the api crate's Store, deduped
by a real `UNIQUE(account_id, jmap_id)` constraint, indexed into the existing FTS
`search` table, and embedded into the shared vector space only behind a gate until a
pgvector migration lands. Attachments go content-addressed by blake3 into a Postgres
`blobs` table. Phase 2 compose rides the existing outbox with a narrowed worker claim.
The public crate is `jmap-sync`, not a blocks bridge. The schema stays concrete;
the generalization budget goes into the polymorphic seams Hive already has.

## Where the brief and the codebase disagree

The review brief described Hive as "block-based append-only data model, UUIDv7 block
IDs, PostgreSQL + pgvector server-side, SQLite + sqlite-vec client-side, Makepad UI."
That is the Zibaldone paper design (BookStack, Zibaldone book, architecture decided
2026-05-06, "currently pre-scaffolding"). It is not this codebase, and the two were
never the same project in git:

| Brief claim | Reality in the repo |
| --- | --- |
| Repo may still say "zibaldone" in places | Zero occurrences. `git log --all -i --grep=zibaldone` over all 132 commits on all refs: nothing. `git grep -i zibaldone` over every historical blob: nothing. The repo has always been `hive`. There is no rename pass to execute. |
| Block-based data model | No blocks table, no block concept. The closest token is `TaskStatus::Blocked` (shared/src/lib.rs:425). The model is immutable journal prose rows plus materialized tasks/decisions/events. |
| UUIDv7 block IDs | Every id is `prefix_<nanoid(12)>` TEXT via `new_id()` (api/src/store/mod.rs:126-129). The only uuid call in the workspace is `Uuid::new_v4()` naming throwaway test schemas (api/src/db.rs:483). |
| pgvector server-side | Embeddings are packed little-endian f32 BYTEA rows; every semantic query loads all model-matched vectors and computes cosine in Rust (api/src/store/semantic.rs:474-491). |
| SQLite + sqlite-vec client-side | No client persistence beyond one localStorage key (packages/web/src/api.ts:55-64). SQLite exists only so the legacy import path can read an uploaded old `hive.db`. sqlite-vec appears nowhere. |
| Makepad UI | The only UI is the Solid.js SPA served by the Rust binary (api/src/routes/spa.rs). No Makepad, Tauri, Electron, egui, or iced anywhere. |
| Append-only | Convention, not enforcement. `UPDATE journal SET user_scope` exists (store/journal.rs:93), actor merge rewrites authorship (store/actors.rs:377), actor delete hard-DELETEs journal rows with a full cascade (actors.rs:191-241). |

What "Zibaldone has been renamed to Hive" actually means: the Zibaldone project
vision was re-homed onto the Hive codebase. The repo needs no rename. The BookStack
Zibaldone book still describes the unbuilt paper design and should be marked
historical or rewritten; this document supersedes it as the direction of record.
See decision D15.

## Current state, as built

**Data model.** PostgreSQL, one database shared by the `hive-api` and `hive-worker`
binaries. The schema is two inline raw-SQL constants run idempotently at boot
(`CREATE TABLE IF NOT EXISTS` plus `ADD COLUMN IF NOT EXISTS`, api/src/db.rs:34-435).
There is no migration framework. Core tables: `journal` (append-only prose, the source
of truth), `anchors` (a `{start,end}` span of an entry that produced an entity),
`tasks`/`decisions`/`events` (materialized from anchors, carrying `origin_entry_id` +
`anchor_text` provenance), `links` (free-string rel between typed endpoints), `inbox`
(per-actor notifications), `wire` (append-only event log doubling as the SSE bus),
`sources`/`outbox` (worker config and job queue), `embeddings`, `search`, plus people,
shares, users, auth, and the hosted Claude Code workspace tables (`cc_sessions`,
`cc_messages`, `cc_credentials` with AES-256-GCM reversible encryption).

**Write path.** `journal_append` is "the one write path" (store/journal.rs:112).
It inserts the entry, indexes it into FTS, materializes anchors into entities, parses
bracket tokens (`[person:]`, `[task:]`, ...), fans inbox notifications, auto-creates
shares for mentions, and emits a wire event. Author is pinned to the authenticated
principal. Steps run sequentially with no wrapping transaction, though real
transactions exist elsewhere (actors.rs `remove_in_tx`), so transactional writes are
proven feasible in this codebase.

**Search.** FTS is a `search` table with a STORED generated tsvector and GIN index
(db.rs:388-398), maintained by app-level DELETE+INSERT from journal, tasks, decisions,
events, and import. Semantic search is a hybrid cascade (semantic.rs:430-641): cosine
over all BYTEA vectors in Rust, FTS blend, Markov-blanket boost from the links graph,
optional cross-encoder rerank, then viewer ACL filtering. Two ordering defects matter
for mail: the vector pass scores every user's vectors and filters visibility after the
fact, and it truncates to `limit` before applying the viewer scope (semantic.rs:616
then :637), unlike FTS which over-fetches 5x first.

**Embeddings.** Local ONNX (`Xenova/bge-small-en-v1.5`, 384 dims) behind the
hive-embed crate, with a deterministic hash-ngram fallback. One vector per item, no
chunking; the tokenizer truncates at model max length (~512 tokens, embed/src/onnx.rs:92-95).
The worker re-embeds when content hash or model tag changes, with one skip-check
SELECT per item per 30s cycle (worker/src/lib.rs:110-162). The embeddable corpus is
journal (newest 1000 only), tasks, decisions, events (semantic.rs:256-314). One ONNX
failure latches the process to the 256-dim hash embedder until restart, and the model
tag change then re-embeds the whole corpus as low-quality vectors.

**Sync layer (what exists).** There is no general sync layer. The worker tick loop
(30s, five sequential stages, any stage error aborts the cycle, worker/src/lib.rs:79-105)
polls RSS/scrape `sources` into `wire` events. That path has no cursor state, no
conditional fetch, dedup by SQL LIKE substring over JSON payloads (sources.rs:317-329),
and a 2000-row wire prune that doubles as dedup memory (worker/src/lib.rs:171-176).
Feed content never enters FTS, embeddings, or recall. It is a notification bus, not an
archive, and it is disqualified as a template for mail.

**Visibility.** `journal.user_scope` NULL means global; non-NULL means that user plus
admins. `visible_entry_ids` (journal.rs:903-960) is the single ACL boundary; non-journal
search hits scope through their `origin_entry_id`. AIs act inside their grantor's
namespace. Several MCP list tools (notably `inbox_list`, mcp.rs:1116-1125) take an
arbitrary recipient with no viewer gate today.

**Clients.** The Solid.js SPA (16 tabs, SSE revision-bump refetching), a stateless
CLI, the agent memory adapter (`/api/recall` session briefs + journal writes), three
agent memory plugins (Claude Code, Codex, Hermes), and on this branch the hosted
Claude Code workspace runner. Untrusted content renders through marked + DOMPurify
with one exception: search hit snippets are injected with raw innerHTML
(packages/web/src/Boards.tsx:454) from unescaped `ts_headline` output (semantic.rs:204-209).

**Deployment.** Docker compose: postgres:17, hive-api, hive-worker from one image,
behind Traefik. Credentials user/pass/db all "hive" on a plaintext volume. Backups are
the pgdata volume / pg_dump, unencrypted as far as the repo shows.

## Target architecture

### New tables

All inline in the db.rs SCHEMA constant, same idempotent style, nanoid prefix ids,
TEXT ISO-8601 UTC timestamps normalized to the exact `%Y-%m-%dT%H:%M:%S%.3fZ` shape
`now_iso` produces, so lexicographic ordering holds.

```
mail_accounts   id 'macct_' PK, owner (people.slug, the ACL key), address,
                jmap_url, jmap_account_id, cred_id (cc_credentials-pattern row,
                AES-256-GCM reversible), email_state, mailbox_state,
                backfill_status, backfill_cursor (JSON {received_at, jmap_id}),
                attempts, next_attempt_at, last_error, last_synced_at,
                last_status, enabled, created_at

mail_mailboxes  id, account_id, jmap_id, name, role, ingest BOOL DEFAULT FALSE
                (per-mailbox opt-in is the spam gate)

mail_messages   id 'mail_' PK, account_id, jmap_id, jmap_thread_id (indexed),
                message_id_hdr, in_reply_to, references_json,
                from_addr, from_name, to_json, cc_json, reply_to_json, subject,
                sent_at, received_at (authoritative sort key), mailbox_ids_json,
                keywords_json, body_text (sanitized plaintext, never HTML),
                body_source ('plain'|'html2text'), snippet, size,
                has_attachments, embed_state, user_scope NOT NULL (= owner,
                never NULL: there is no global mail), deleted_at,
                created_at, updated_at
                UNIQUE(account_id, jmap_id); INDEX (user_scope, received_at),
                (account_id, jmap_thread_id), (message_id_hdr)

mail_attachments id 'att_' PK, message_id, blob_hash NULL, jmap_blob_id
                (always stored), filename, mime, size, content_id,
                disposition, skipped_reason, created_at
                UNIQUE NULLS NOT DISTINCT (message_id, jmap_blob_id, content_id)
                so cannotCalculateChanges replays and crash-retries cannot
                duplicate metadata rows

blobs           hash TEXT PK (lowercase blake3 hex), size, mime,
                data BYTEA (STORAGE EXTERNAL), created_at
```

### The sync daemon

`hive-mail` is a third long-lived binary in the workspace, depending on the api crate
exactly as hive-worker does, deployed as a third compose service from the same image.
It is not a worker tick stage: the tick loop is single-instance, sequential, and
aborts whole cycles on error; a permanently open JMAP EventSource cannot live in it,
and a multi-year backfill inside a stage would starve heartbeat, feeds, and outbox for
hours. It is not per-device: there is no device data layer to sync to.

Steady state per account: hold a jmap-client EventSource on Stalwart
`/jmap/eventsource` as a doorbell only. On wake, or every `HIVE_MAIL_POLL_SECS`
(default 300) when the stream is down, run `Email/changes` from the stored state
string, `Email/get` the created/updated ids with text bodyValues, upsert on
`(account_id, jmap_id)`, apply metadata updates for moves and flag changes, tombstone
destroys, and persist the new state string only after the batch commits. Correctness
comes from state-string polling; push is latency sugar.

`cannotCalculateChanges` (Stalwart can invalidate state strings after upgrades or
compaction) triggers a full reconciliation: paged `Email/query` diffed against stored
jmap_ids. The unique index makes it idempotent. This path gets exercised in CI against
a Stalwart container, because an unimplemented resync path means an account that
silently stalls forever, the same failure shape as sources whose `last_polled_at`
becomes unparseable.

Backfill: newest-first `Email/query` sorted receivedAt DESC, pages of 200, bodyValues
capped at 256KB, one transaction per batch with `ON CONFLICT DO NOTHING`, committed
`(received_at, jmap_id)` cursor for resumability, 250ms politeness sleep between
pages. Wire and inbox emission are suppressed during backfill (see D10). Message-level
parse failures store a stub row with a parse error and advance the cursor; per-message
index failures are isolated the same way so one oversized email cannot wedge the
account's replay loop.

Retry: outbox arithmetic at the account level (`min(2^attempts * 30s, 3600s)`), and
after 8 attempts the account flips disabled and notifies its owner through inbox plus
a wire event. Sources' silent retry-forever is the known-bad pattern; the outbox's
silent poison is only half-right. Fail loud.

### Seam registrations

This is the actual multi-corpus door, all additive:

1. `EntityKind` gains `Mail`, string `"mail"`, everywhere in lockstep (shared crate,
   api, web, MCP). Note the deployment hazard: `EntityKind::from_str_lossy` defaults
   unknown strings to Task (shared/src/lib.rs:586-597), so mail rows reaching `search`
   before the enum lands would surface mislabeled. Land the enum first, and make
   `from_str_lossy` fail closed for unknown kinds while in there.
2. `index_entity('mail', id, subject, body_text)` on ingest, with body truncated to
   ~200KB before indexing (tsvector has a hard ~1MB limit a large newsletter can hit).
   Tombstones and moves out of ingested mailboxes delete the search row.
3. Embeddings: a `mail` arm driven by the `embed_state` queue column, never by an
   `embeddable_items()` full scan (200k messages would mean 200k skip-check SELECTs
   per 30s tick before any real work). Gated per D8. Add the missing reaper while in
   there: nothing in the codebase deletes embedding rows that age out of eligibility
   (the only `DELETE FROM embeddings` is the actor-delete cascade, actors.rs:131), so
   messages leaving the newest-N window would orphan vectors that still get scanned
   on every query and, because hydration misses them, surface as hits titled by raw
   id (semantic.rs:601,630). A maintenance sweep deletes embedding rows whose ref is
   tombstoned or no longer embed-eligible; the same sweep serves journal's existing
   1000-entry window, which has the same latent leak today.
4. Visibility: refactor `scope_hits` from journal-centric routing into a per-kind
   resolver. journal resolves via `visible_entry_ids`; task/decision/event via
   `origin_entry_id` (unchanged); mail via `user_scope == viewer || admin`, no share
   or mention piercing in v1. This resolver is the seam every future corpus needs.
5. Embeddings ACL moves into SQL: `ADD COLUMN owner TEXT` on `embeddings`, candidate
   loading becomes `WHERE model=? AND dim=? AND (owner IS NULL OR owner=?)`, and the
   scope filter runs before truncation, not after. This fixes the cross-user memory
   cost, the privacy smell of scoring everyone's vectors on every query, and the
   result-starvation defect where mail dominance would empty other users' semantic
   results.
6. Wire gets `mail.received` with an ids-only payload. Content never lives in wire.

### Text extraction

Plaintext-first holds, and the stack requires no HTML engine: mail-parser 0.11
`body_text()` as the default (it converts HTML-only messages automatically), html2text
0.17 (html5ever parsing only) when structure matters. Raw HTML is never stored in
`body_text` and never rendered; the SPA renders mail like the Workspaces transcript
pane, plain text only. Quoted-reply stripping (leading `>` blocks, "On ... wrote:",
Outlook header blocks, `-- ` signatures, unsubscribe footers) runs before embedding
but not before storage. No mature Rust crate does this; budget roughly 300 lines of
heuristics with a test corpus. Without it, every message in a thread embeds
near-identically and recall returns one conversation N times.

### Attachments

Content-addressed blake3, as leaned, but the bytes live in Postgres, not a filesystem
CAS directory. The deciding constraint is the real deployment: two app containers and
a postgres volume, backup story = pg_dump/pgdata. A filesystem CAS needs a new shared
volume mounted into two services and splits the backup into artifacts that can drift;
S3/minio adds a service to a deliberately small box. The `blobs` table reuses existing
plumbing (BYTEA precedent in `embeddings.vec`), keeps blob+row insert transactional,
and dedup is the primary key itself (`ON CONFLICT (hash) DO NOTHING`). Mail corpora
are pathologically duplicated (signature images, re-attached PDFs down reply chains);
expect 30-60% byte dedup.

Policy: 15MB per-part cap (`HIVE_MAIL_MAX_ATTACHMENT_BYTES`); over-cap parts store
metadata plus `jmap_blob_id` with `skipped_reason='oversize'`, and Stalwart remains
the byte source of record for them. Store everything under cap including inline
images and text/calendar parts. `cid:` references resolve to attachment chips in
Phase 1; inline rendering of an image-type allowlist can come later via the
authenticated route. Serving is `GET /api/mail/attachments/{att_id}`, ACL-checked
through the owning message's namespace. Never serve by bare hash: in a multi-namespace
system a hash URL is an unscoped capability that pierces the ACL. Response headers:
nosniff, attachment disposition by default, ETag = hash, immutable private cache.

GC is a weekly refcount sweep (delete blobs with no referencing attachment row, 24h
grace). The actor-delete cascade (actors.rs:191-241) must extend to mail_accounts,
mail_messages, mail_attachments, and orphaned blobs, and an admin redact-message route
exists from day one: mail, unlike journal prose, routinely contains things you are
obligated to purge.

### Phase 2 send

One queue, one drainer, one relay. Compose enqueues an outbox row kind `mail.send`;
`hive-mail` drains that kind with the existing backoff/poison helpers. Precondition:
narrow the worker's outbox claim to its known kinds first, because `drain_outbox`
currently marks unknown kinds as successful no-ops (outbox.rs:142-145) and would
silently swallow every send. Relay through Stalwart's smart-host to smtp2go rather
than calling smtp2go directly: Stalwart keeps the Sent copy and JMAP sync ingests it
back automatically, with the unique index making that idempotent. mail-send direct to
smtp2go is the documented fallback.

Scope: plain-text body, To/Cc, subject, and correct `In-Reply-To`/`References` headers
on replies. Reply headers are not scope creep; without them every recipient sees
broken threading, and adding two headers does not make a mail client. No drafts, no
attachments in compose v1, no address book, no HTML composition. Each of those is the
event horizon. Surface: one MCP tool `mail_send` plus a thin SPA pane.

### The public crate

`jmap-sync` (not "JMAP-to-blocks"; there are no blocks): session discovery, the
EventSource loop with reconnect, `Email/changes` delta driving, `Email/query` backfill
pagination, body fetch, and plaintext extraction, emitting a `NormalizedMessage`
struct through two traits: `CursorStore` (load/save state strings and backfill anchor)
and `MailSink` (`upsert_batch`, `tombstone`), with save-cursor-after-sink-Ok as the
at-least-once contract. No Hive types, no Postgres in the crate; hive-mail implements
both traits over Store and adds the FTS/wire/inbox side effects in its sink. This seam
is real, not aspirational, and it also isolates the jmap-client dependency (dormant
2024-01 through 2025-09, revived since, 0.4.2 as of 2026-06-19) behind one internal
module sized for a reqwest+serde rewrite if it goes dark again.

## Decision log

D1. **Mail is a new first-class entity.** Not journal entries: `journal_append` pins
authorship to an authenticated principal, third-party mail has none, and an archive
stored as journal would evict actual memory from the capped embed corpus. Not wire:
LIKE dedup, 2000-row retention, and no FTS/embedding path disqualify it. Not a generic
documents table: the entity tables here are deliberately concrete per kind, and the
wire experiment already proved that corpus membership comes from seam registration,
not from which table rows sit in.

D2. **Identity: surrogate `mail_<nanoid12>` PK; idempotency via
`UNIQUE(account_id, jmap_id)`; Message-ID is provenance, not identity.** UUIDv7 is
irrelevant (no UUIDs exist here) and UUIDv5-from-Message-ID is wrong on the merits:
Message-ID is attacker-supplied (forgery would silently suppress distinct messages),
sometimes absent, and legitimately duplicated across accounts whose rows must stay
separate because their `user_scope` differs. Keep `message_id_hdr` indexed for
threading and export.

D3. **Threads stay queries in Phase 1.** JMAP `threadId` is a server-computed grouping
key; a `mail_threads` table would cache a GROUP BY and add an entity kind with no
Phase 1 consumer. Materialize lazily if thread-level linking or thread-level
embedding ever needs a durable id.

D4. **Sync runs in a separate `hive-mail` binary.** Three of five panel positions
mandated this and the dissent (a worker tick stage) failed adversarial review on
EventSource lifetime and backfill starvation. Single daemon per deployment, matching
the single-writer assumptions already baked into outbox claiming.

D5. **Sync state lives on `mail_accounts` rows**: JMAP state strings as the cursor,
backfill cursor for resumability, outbox-style backoff with loud poisoning. The
sources table contributed the anti-pattern list, not the template.

D6. **Mutation semantics: mail is a mutable mirror with tombstones.** Moves and flag
changes update metadata and re-evaluate index membership (a message moved to Junk or
Trash leaves FTS and embeddings). JMAP destroys set `deleted_at` and delete the search
row and embedding row in the same batch, immediately; deleted mail must not remain
searchable until a sweep. Append-only purity would make mail more rigid than the
journal actually is.

D7. **Attachments: blake3 content-addressed `blobs` table in Postgres**, 15MB cap,
eager fetch under cap, metadata-plus-blobId above, serve by attachment id through the
message ACL, never by hash. GC by refcount sweep. `blob_put`/`blob_get`/`blob_stat` is
the seam for a future backend swap if the corpus outgrows the box.

D8. **Embeddings are gated until pgvector.** Phase 1 embeds only ingest-enabled
mailboxes, newest-N per account (default 5,000), quote-stripped, driven by
`embed_state`. Arithmetic: 100k unbounded messages means ~150MB of BYTEA loaded and
cosine-scored per query on the current path, plus a full-corpus text load at query
time (semantic.rs:449); the wall arrives around 25-50k vectors. The 5k gate adds
roughly 7.5MB per query, which the current scan absorbs. Lifting the gate requires
the Phase 1.5 pgvector migration (HNSW, chunked rows, SQL-side ACL). Chunking arrives
with pgvector, because a chunk column changes the `(ref_kind, ref_id)` PK shape and
should not fork the table twice. Name the consequence of the gate honestly: mail that
is FTS-indexed but not embedded is invisible to `semantic_search`, because the hybrid
blend drops keyword-only hits as noise (vector==0 with keyword>0, semantic.rs:548-555).
Until Phase 1.5, the bulk of the archive is reachable through plain keyword `search`
and the mail MCP tools, and semantically only for the gated slice. That is a fork in
the retrieval surface, accepted deliberately, removed by Phase 1.5.

D9. **ACL: owner-only, enforced in SQL, fail closed.** `user_scope` is never NULL for
mail. The per-kind visibility resolver plus the embeddings owner column move scoping
into the query. Recall stays journal-only, and its kind filter moves into the
semantic_search call rather than post-filtering the returned pool (today recall
filters to Journal after the limit-8 pool is formed, recall.rs:129-146, so mail would
silently crowd agent memory briefs toward empty). No mail sharing in v1; when it
comes, `ShareScope` needs a real enum variant because `from_str_lossy` coerces unknown
scopes to Entry.

D10. **Wire and inbox carry ids only.** `mail.received` payload is message id plus
owner, nothing else: wire is globally readable (GET /api/wire, dashboard, SSE) and
pruned to 2000 rows. Both wire and inbox emission are suppressed or batched during
backfill, or a 100k-message import floods the activity log and drives every open SPA
client into refetch storms. Mail inbox notifications carry no subject or sender
snippet until `inbox_list` is viewer-gated; that MCP tool currently returns any
recipient's inbox to any authenticated actor.

D11. **Phase 2 send: outbox kind `mail.send`, drained by hive-mail, relayed through
Stalwart's smart-host to smtp2go**, after narrowing the worker's outbox claim.
Plain text, reply headers correct, nothing else. The send capability is the single
largest risk step in the whole program (a leaked PAT or a prompt-injected agent can
then send mail as the user), which is why it stays a phase behind the archive.

D12. **Schema strategy: concrete tables, generic seams.** No corpus/document
abstraction now. The `(kind, ref_id)` sidecars (search, embeddings, links) plus the
per-kind visibility resolver are the multi-corpus substrate; a third corpus arrives
the way mail does, a concrete table registered at the seams. The wire experiment is
the evidence: generic storage without seam registration produced a second-class
corpus. This repo's history (Python to Rust to Node to Rust to Postgres, a 9-phase
auth program written off) prices speculative architecture at total loss; build with
that grain.

D13. **The public artifact is `jmap-sync`** with the `MailSink`/`CursorStore` trait
boundary. The design keeps it cleanly extractable: nothing in the sync loop imports
Hive types.

D14. **Adopt sqlx migrations at Phase 1.5.** The inline idempotent schema has reached
its limit; the pgvector cutover rewrites the embeddings table and needs a rollback
story. Mail's Phase 1 tables can still ride the inline pattern; the migration
framework lands before the first destructive reshape.

D15. **Zibaldone reconciliation.** Hive-as-built is the substrate going forward. The
Zibaldone block/event-sourced/Makepad design is retired as an architecture target;
its surviving ideas (append-only discipline, AI-collaborative journaling, local-first
ambitions) either already exist here in different clothes or belong to a possible
future native client project that is out of scope for this document. There is no
repo rename to execute; the BookStack Zibaldone book gets marked historical. If a
hive-to-something rename is ever wanted for branding, know the cost up front: ~29
HIVE_* env vars, crate/binary/image/npm names, the Postgres identity, and critically
the `hive_pat_`/`hive_sess` credential prefixes and `hive_session` cookie name, which
invalidate every issued token on change.

## Phased build plan

### Phase 0: hardening preconditions (small, mostly independent, all required before mail rows land)

1. Fix the stored-XSS sink: escape or sanitize search snippets before innerHTML
   (Boards.tsx:454) and HTML-escape ts_headline input (semantic.rs:204-216). Mail is
   attacker-controlled content; indexing it turns this from theoretical into certain.
2. Apply viewer scope before truncation in semantic_search (semantic.rs:616 vs :637),
   and pass recall's journal filter into the search call instead of post-filtering.
3. Gate `inbox_list` (and audit the other MCP list tools) by viewer.
4. Guard the embedding latch: when TRANSFORMERS_FAILED trips, pause backfill instead
   of re-embedding the corpus as 256-dim hash vectors. ~20 lines.
5. Land `EntityKind::Mail` in lockstep across shared/api/web/MCP; make
   `from_str_lossy` fail closed.
6. Narrow the worker outbox claim to known kinds (needed for Phase 2, one line, do it
   now).

Milestone: a hostile email body indexed into search cannot execute script, leak
across namespaces, or wedge the pipeline.

### Phase 1: read-only archive

- `mail_accounts`/`mail_mailboxes`/`mail_messages`/`mail_attachments`/`blobs` tables.
- `hive-mail` binary: backfill (resumable, newest-first) plus incremental sync
  (state strings, EventSource doorbell, cannotCalculateChanges resync), account-level
  backoff, per-message failure isolation.
- FTS indexing with the 200KB pre-tsvector truncation; tombstone and move handling.
- Gated embeddings per D8, quote-stripping heuristics with a test corpus.
- Attachment ingest with blake3 dedup and the authenticated serving route.
- SPA Mail tab rendering plaintext (Workspaces transcript pattern); mail chips in
  search results.
- MCP: `mail_search`, `mail_thread_get`, `mail_accounts_list`, all viewer-gated from
  day one. Mail excluded from recall's default brief.
- `[mail:<id>]` bracket token in journal_append creating journal-to-mail links, so
  tasks emerge from journal entries that cite messages. Anchors stay journal spans;
  a task does not anchor to an email.
- Ops: LUKS on the host, encrypted backups, and the runbook sentence that an
  unencrypted pg_dump is now a copy of the mailbox. Read-only JMAP credential for the
  sync account if Stalwart supports it. `HIVE_CRED_KEY` becomes a hard runtime
  dependency for the first time: the credential vault errors when it is unset
  (cc_credentials.rs:57-64), no compose file sets it today, and hive-mail cannot
  start without it. Document it, generate it, and back it up separately from the
  database, since losing it orphans every stored mail credential.

Milestone: the full mailbox is searchable next to the journal, agents can cite mail
in context, deleting mail in Stalwart removes it from search here, and re-running
backfill from zero produces zero duplicates.

### Phase 1.5: retrieval at scale

- Adopt sqlx migrations (D14).
- pgvector + HNSW; chunked embedding rows (~450 tokens with overlap); owner/kind
  filtering in the index query; migrate the existing few thousand vectors; rewrite
  the vector pass as `ORDER BY vec <=> $q LIMIT k` with ACL in the WHERE clause,
  and replace the per-query `embeddable_items()` text hydration with a keyed fetch
  of only the top-N hits.
- The extension is an ops change, not just SQL: the deployed `postgres:17` image
  does not ship pgvector. Move compose to `pgvector/pgvector:pg17` (same Postgres
  major, same `hive-pgdata` volume) before `CREATE EXTENSION vector` can run.
- Lift the mail embedding gate; per-kind rank weights in the hybrid blend so 200k
  mail documents cannot drown ~1k journal entries.

Milestone: semantic search stays sub-100ms with the full archive embedded, and one
user's corpus size cannot degrade another user's results.

### Phase 2: compose

- Outbox `mail.send`, hive-mail drainer, Stalwart smart-host relay to smtp2go.
- MCP `mail_send` plus the thin SPA pane. Plain text, correct reply headers, hard
  stop there.

Milestone: a reply sent from Hive threads correctly in the recipient's client, the
Sent copy appears in the archive via sync without duplication, and a failed relay
retries with backoff and poisons loudly.

### Phase 3, when earned: extraction and the next corpus

- Extract `jmap-sync` to its own repo/crates.io once a second consumer or the
  stability of the trait seam justifies it.
- Apply the mail template (concrete table + seam registration) to the next corpus
  (documents, agent memory) if and when one is commissioned.

## Risks and tar pits

**Numbers first.** A 100-200k message mailbox: backfill fetch a few hours at polite
paging; gated embedding of 5k messages 15-25 minutes of CPU; full-archive embedding
4-16 hours one-time (Phase 1.5, acceptable) but catastrophic if ever re-triggered by
the model-tag latch. `search` table footprint at that scale roughly 1.5-3GB including
the tsvector and GIN; attachment blobs 3-15GB post-dedup under the 15MB cap. All fine
on disk; all material to pg_dump duration and backup size, so surface totals on the
dashboard.

**Prompt injection into agent memory.** Recall briefs and semantic results feed Claude
Code sessions as additionalContext, and this branch's runner executes with
bypassPermissions in non-isolated sandboxes. Inbound spam becomes attacker
instructions inside agent sessions. Mitigations: mailbox allowlist ingest, spam-flag
exclusion, mail out of recall's default brief, untrusted-content framing on any
agent-facing mail surface. None is complete; treat every mail string an agent reads
as hostile input.

**The benign exfiltration loop.** The sanctioned dreaming/memory-write pattern has
agents summarize what they read into journal prose, and a system-principal
journal_append lands with `user_scope = NULL`, which is globally visible. An agent
with mail access that journals a mail summary publishes private correspondence to
everyone. Phase 1 ships a policy rule in the memory-write protocol (mail-derived
prose must carry the owner's scope); enforcement options stay an open question below.

**At-rest posture.** pgcrypto column encryption is rejected: it is incompatible with
the generated tsvector column and with embedding (plaintext must exist at index time,
and the key would sit on the same box). The honest posture is disk encryption plus
encrypted offsite backups plus the AES-GCM credential store for secrets, and saying
plainly that extracted text is plaintext inside Postgres.

**Deliverability and ops.** Outbound is solved (smtp2go smart-host). Inbound MX
behind DDNS is not solvable in code: ISP port-25 policy, dynamic-IP lapses, rDNS
mismatch. Accept it as stated risk or front it with an MX relay. Stalwart moves fast
(0.16.11 fixed a JMAP query bug in June 2026); pin and test upgrades, and keep the
cannotCalculateChanges path healthy since compactions invalidate cursors.

**Dependency bus factor.** jmap-client went dormant for 21 months before its 2025-09
revival and has ~60k downloads. The jmap-sync internal client module is the
containment: hand-rolling JMAP over reqwest+serde is a realistic escape hatch.

**Compose scope creep.** The panel's sharpest self-observation: minimum viable
compose that threads correctly is slightly larger than "thin pane," and everything
past it (drafts, quoting UI, attachments, address book) is the full-mail-client event
horizon. The line is drawn at reply headers. Hold it.

**Timestamp discipline.** All ordering is lexicographic TEXT comparison, and
`...:00Z` sorts differently from `...:00.000Z`. Normalize every mail timestamp to the
exact now_iso shape at the crate boundary or merged feeds silently mis-order.

## Open questions (not blockers)

1. Enforcement for the dreaming write-back leak: policy only, or should
   journal_append refuse global-scope entries containing `[mail:]` references
   authored by non-human principals?
2. Does Stalwart support a read-only JMAP credential for the Phase 1 sync account,
   and is scoping it worth the setup friction?
3. Should recall ever include mail (opt-in flag with quoted-untrusted framing), or is
   journal-only permanent policy?
4. Mail sharing semantics: when someone wants to share a thread into another
   namespace, is it a `ShareScope::Mail` grant or a journal entry quoting the mail?
5. The Correspondence book in BookStack currently captures important email threads by
   hand. Does mail-in-Hive replace that workflow, feed it, or leave it alone?
6. Does the local-first native client ambition (the surviving Zibaldone UI vision:
   Makepad, Apple Pencil, offline SQLite) get its own project charter later, and if
   so does it consume Hive's API or fork the storage design?
7. Sent-mail identity: which From addresses/aliases does Phase 2 permit, and where
   does that allowlist live (mail_accounts vs Stalwart config)?
8. Multi-account future: the schema supports N accounts per owner; does the household
   (Maggie) onboard in Phase 1 or after Phase 1.5 scaling?

## Next steps

1. Review this document; contest any decision in the log before code starts.
2. Ship Phase 0 as one small hardening PR series (XSS fix first; it is exploitable
   today without mail).
3. Stand up Stalwart at the Roadhouse with the smtp2go smart-host and a test mailbox;
   wire a Stalwart container into CI for the resync test.
4. Build Phase 1 behind `HIVE_MAIL_ENABLED`, backfill a real mailbox, and measure:
   backfill wall-clock, search latency with mail indexed, dashboard blob/search
   totals.
5. Mark the BookStack Zibaldone book historical and point it at this document.
6. Decide open question 1 (write-back enforcement) before agents get mail tools.
