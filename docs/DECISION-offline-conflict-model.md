# DRAFT — Offline write-conflict model

Status: **DRAFT, 2026-08-18.** Proposed answer to `docs/WEB-APP.md` open
question 1. Not ratified; ratification is the repo owner's call, and merging
the PR that carries this file IS the ratification. House style follows
DIRECTION.md's numbered decisions, but this is a standalone record — the
D-series numbering died with the architecture it recorded.

## The question

`docs/WEB-APP.md` scopes the SPA's IndexedDB layer honestly: a **cache**, not
local-first. Reads served offline, writes queued and replayed. The open choice
is what happens when a replayed write meets server state that moved while the
client was offline:

* **Last-write-wins per field** — the server is the authority and applies
  whatever arrives; the latest replay wins each field. Simple, lossy, and
  honest about being lossy.
* **Queue and reject** — a replayed write that conflicts surfaces to the human
  instead of resolving itself.

Anything more ambitious is a CRDT, which is a different project.

## Ground truth in the tree

The SPA is online-only today, so nothing here amends existing behavior:

* `packages/web/src/live.ts` is a single shared `EventSource` bumping a
  revision counter; reads refetch on bump. There is no cache, no queue.
* `packages/web/src/api.ts` is plain `fetch` with a 15 s abort. A failed write
  is a rejected promise and nothing else.

The write surface a queue would replay, and how each kind behaves on a blind
second application:

| write | endpoint | replayed blindly |
|---|---|---|
| journal append | `POST /api/journal` | **double-writes** — and emergence re-runs, double-materializing tasks/decisions/events and re-fanning inbox items |
| task / decision patch | `PATCH /api/tasks/{id}`, `PATCH /api/decisions/{id}` | converges — but read-modify-write in the store means a stale patch silently overwrites newer values of the fields it names |
| inbox mark-read | `POST /api/inbox/...` | idempotent (`UPDATE` to the same state) |
| creates (sources, people, shares, entities, workspaces, credentials) | `POST /api/...` | **double-create** |
| deletes | `DELETE /api/...` | second call 404s — the replayer can treat that as success |
| artifact upload | `POST /api/artifacts` (multipart) | bytes dedup by content address, but a **second row lands** — one row per upload is deliberate (`created_by`, when, name), so the row is the duplicate |
| admin surface (users, tokens, imports, reassign-scope, mail account ops) | assorted | out of scope: require connectivity |

Two store facts shape everything below:

* **The server mints every id.** `new_id("jrnl")` in `journal_append`
  (`core/src/store/journal.rs`), `new_id("task")` in `tasks_create`,
  `Uuid::new_v4()` for artifacts. No create endpoint accepts a client id.
* **Mutations already have a clock.** `tasks.updated_at` (and `decisions`,
  `entities`, …) is stamped on every update (`core/src/store/tasks.rs`). The
  journal itself has no `updated_at` because entries are immutable — which is
  the point: the journal cannot conflict.

## The framing that shrinks the problem

**Most of the offline write volume cannot conflict.** Journal appends are
create-only and entries are immutable history (`docs/conventions.md`: status
changes propagate through the renderer, never through retroactive edits).
Appends replay unconditionally and merge by construction. What remains that
CAN conflict is small and enumerable: patches to existing rows (task status
flips are the everyday case), and deletes racing re-uploads. The conflict
decision governs that residue, not the corpus.

## Option A — last-write-wins per field

Server applies each replayed patch onto current state and moves on.

*For:* no new server surface; no conflict UI; the queue drains and the user
never thinks about it. *Against:* this codebase's repeating stance is that
silent data loss is the one unacceptable failure — poisoned mail cursors alert
loudly, derived text is visibly machine-authored, a scoped-down journal write
gets a `scoped-by-policy` tag rather than passing silently. LWW is a silent
loss by definition. It is also not what the store does today: `tasks_update`
is read-modify-write over the whole row, so field-level arbitration would be
NEW machinery (per-field provenance or version columns), not a fallback to
existing behavior. "Simple" is borrowed against code that does not exist.

## Option B — queue and reject

A replayed mutation carries the `updated_at` the client based its edit on; the
server compares and answers 409 with the current row when they differ. The
client holds the queued op and the server's row side by side for a human.

*For:* conflicts are surfaced, which is the journal-first philosophy applied
to state. For the dominant real case — two flips of the same checkbox —
"reject" costs the human one re-toggle, because the server's current state is
usually what they meant anyway. *Against:* a conflict UI has to exist; a
409 path per mutating endpoint; the queue needs a "dead letter" state so a
rejected write neither blocks the queue nor vanishes.

## Recommendation: Option B, queue-and-reject — scoped to where conflicts can exist

Adopt queue-and-reject for mutations of existing rows, and declare creates
conflict-free-but-idempotent (below). The deciding argument is not that LWW is
wrong in general; it is that LWW is wrong *here*: the store has no arbitration
data to make it field-level, the product treats silent loss as the cardinal
sin, and the actual conflict residue is small enough that surfacing it is
cheap. If usage proves the conflict rate is noise rather than signal, the
reject path can later downgrade per-endpoint to accept-and-log — that direction
is a relaxation. The reverse (retrofitting rejection onto a user base trained
that writes always stick) is not.

Two implementation questions the design note does not answer, answered here.

### (a) Replayed writes need idempotency, and the shape is client-generated ids, not a key header

The naive option is a generic `Idempotency-Key` header plus a server-side
key→response table. Rejected: it is a new table with a TTL/GC story, every
endpoint opts in one at a time, and the key names a *request* — which does
nothing for question (b), where what is needed is a name for the *resource*.

Instead: **create endpoints accept an optional client-minted `id` in the
request body.** Validation pins the house shape — the id must carry the
endpoint's own prefix and the `nanoid(12)` charset/length (`jrnl_…` on
`/api/journal`, `task_…` on a future task create, a UUID on
`/api/artifacts`), so no endpoint can be used to mint another kind's
namespace. The insert becomes `INSERT … ON CONFLICT (id) DO NOTHING
RETURNING …`: a conflict means *this exact create already landed*, so the
store returns the existing row with `200 OK` instead of `201 Created`, and —
critically for the journal — **skips emergence, inbox fan-out, indexing, and
the SSE emit**, all of which are the real double-apply damage of a replayed
append. The id is the idempotency key, stored where it is most useful: on the
row itself, forever, at zero operational cost.

This is additive to the wire contract (an optional body field; existing
clients change nothing), which is the AGENTS.md bar for touching inherited
shapes.

Patches and deletes need no key: a patch converges on re-application and a
replayed delete's 404 is success. Admin and mail-account operations stay
online-only at first; none of them is a thing a user does on a train.

### (b) Dependent queued writes are named by the same client ids — no placeholder substitution

The hard case the design note doesn't mention: an offline user writes an entry
containing `- [ ] call the dentist`, then opens the Tasks board and checks it
off. Queued: `POST /journal`, then `PATCH /tasks/???` — the task does not
exist yet; it will be *emerged* by the append, server-side, with a
server-minted id.

With client-generated ids the rule becomes simple and worth stating as the
invariant:

> **A queued mutation may only name an id the client itself minted or learned
> from the server. Replay is strictly ordered. There is no id-rewriting layer.**

Concretely:

* A client that creates an entity offline mints its id and references that id
  in later queued operations. The ids are stable across the boundary, so no
  substitution engine is needed.
* Emergence-dependent references resolve by *name*, not id — which is already
  the product's semantics. `docs/conventions.md` says an unanchored checkbox
  fuzzy-matches find-or-create by title at save time, and `^task_…` anchors
  pin a specific id. A queued "check off the task from that entry" replays as
  a patch against the client-minted anchor id it wrote into the entry body,
  or as a title match the server resolves exactly as it does online.
* The honest residual: a queued patch that references a server-side id the
  server no longer has (deleted while the client was offline) fails at replay
  with a 404 and surfaces to the human through the same conflict UI as a 409.
  That is correct behavior, named rather than hidden.

## If ratified, what changes

**Schema:** none. Client ids ride the existing primary keys; `updated_at`
already exists on every mutable content table.

**Store (`core/src/store/*`):**

* `journal_append` and the other create paths accept an optional caller id,
  validate its prefix/charset, insert with `ON CONFLICT (id) DO NOTHING`, and
  on the conflict arm return the existing row WITHOUT running emergence,
  inbox fan-out, indexing, or `emit()`. One behavior test per create path:
  same id twice, one row, one `journal.created` event, one set of emerged
  tasks.
* Patch paths (`tasks_update`, `decisions_update`, entity update) accept an
  optional base `updated_at`; a mismatch returns a typed conflict error the
  API maps to 409 with the current row in the body. Emergence writes and
  online patches that omit the precondition behave exactly as today.

**API (`api/src/routes/*`):** `POST /api/journal` (and the other creates as
they gain offline clients) accept the optional `id`; mutating endpoints map
the conflict error to `409` with `{"error":"conflict","current":{…}}`.
`Idempotency-Key` headers are NOT introduced.

**SPA (`packages/web`):** new and deliberately small — an IndexedDB outbox
with ordered replay, minting `jrnl_…`-shaped ids at compose time; a conflict
surface (queued op vs server's current row, human picks); a dead-letter state
for rejected writes. `live.ts`'s bump-on-event already gives the replayer its
"state changed, refetch" signal for free. Admin, mail, and workspace writes
stay online-only; artifact uploads queue metadata-only or not at all in the
first ship (bytes are the wrong thing to spool in a browser cache).

**Docs:** `docs/WEB-APP.md` "Open questions" loses item 1; the "Offline cache"
section gains a pointer here.
