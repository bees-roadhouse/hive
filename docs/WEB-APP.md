# Hive as a web application: API, Postgres, Solid

Design note, 2026-08-10. Supersedes most of DIRECTION.md. For reaction.

## The decision

Hive becomes a self-hostable web application: an HTTP API over PostgreSQL, a
Solid.js frontend with an offline cache, multi-org from the start with users
belonging to several orgs, access enforced by Postgres row-level security,
encryption at rest only, and files served by the API from a local path with
object storage as a later swap.

## What this supersedes, stated rather than eroded

This is not an amendment. It replaces the architecture the following decisions
describe, and they should be marked superseded rather than left to be read as
current:

* **D16-D20** — local-first single-user desktop, the all-Rust UI, the
  content-addressed blockstore, crypto-shred as the delete.
* **D29-D36** — hive-node, hub-and-spoke mTLS replication, blind and trusted
  tiers, DNS-first addressing.
* **D41-D44** — the relay amendment written earlier today. D41's forwarding
  relay survives in spirit; D42's blind cache does not.

Three grep fences exist to enforce the replaced architecture and become
meaningless under it. They should be deleted deliberately, in the same change,
rather than left failing where a future reader cannot tell a broken build from
a superseded rule:

* the identity fence (`node/tests/fences.rs`) — "no sessions, tokens, or OIDC
  below the node binary"
* the tenancy fence (same file) — "the data plane is tenancy-blind"
* the Postgres fence (`importer/tests/no_postgres_gate.rs`) — "only the
  importer may speak Postgres"

## The security argument that killed v1, and what it means here

D16 did not drop hosted multi-user for taste. It dropped it as a security
argument, naming two defects:

> "v1's sharpest structural risks (**the ACL ordering defects**, **the benign
> exfiltration loop where an agent journals private mail into global scope**)
> die with their surfaces rather than getting fixed"

**RLS fixes the first properly.** ACL ordering defects come from authorization
being a sequence of application checks that can be composed wrong or skipped.
RLS has no ordering: it is a predicate the database applies to every query, and
there is no code path that forgets it. This is a genuine improvement on v1, not
a repeat of it.

**RLS does not touch the second, and it is the one that matters more now.** The
exfiltration loop is an agent reading from a scope it legitimately holds and
writing into another scope it also legitimately holds. Every policy returns
true at every hop. Postgres will not stop it, because nothing is being
violated.

It is also worse now than it was in v1: there are more agents writing (Pia and
Apis both append to the journal), and with multi-org the leak crosses
organizational boundaries rather than visibility scopes inside one workspace.

### The rule that addresses it

**An agent's write scope is pinned to its read scope for the duration of a
task.** An identity acting inside org A writes only to org A, regardless of
what else that identity is a member of. Crossing orgs is a human action, never
an agent one.

Mechanically: the session carries an acting org, not a set of orgs. An agent
authenticates into ONE org per session, RLS policies key off that session
variable, and there is no API shape that lets a single call read from one and
write to another. A human switching orgs starts a new session; an agent cannot
switch at all.

This costs a real capability ... an agent cannot summarise across two orgs in
one pass ... and that is the point. The capability is the defect.

## Schema shape

```
orgs(id, slug, name, created_at)

users(id, email, created_at)                    -- email is a LABEL, not a key

user_identities(id, user_id, issuer, subject,   -- OIDC / password
                UNIQUE (issuer, subject))       -- one identity, many providers

memberships(user_id, org_id, role,              -- who is in what
            PRIMARY KEY (user_id, org_id))

-- every content table carries org_id and is RLS-protected on it
journal(id, org_id, author, body, tags, created_at, ...)
tasks(id, org_id, ...)
artifacts(id, org_id, mime, bytes_path, sha256, ...)
```

`UNIQUE (issuer, subject)` is the rule from `docs/SELF-HOST.md` carried over:
one local identity may hold many provider identities, and a provider identity
maps to exactly one local identity. **Never link on email.** Email is a display
attribute; the join key is the `sub` claim, issuer-scoped. Linking a second
provider requires already being authenticated with the first.

### RLS

One session variable, set per request after authentication:

```sql
SET LOCAL hive.acting_org = '<org_id>';
SET LOCAL hive.acting_user = '<user_id>';
```

Policy shape on every content table:

```sql
ALTER TABLE journal ENABLE ROW LEVEL SECURITY;
ALTER TABLE journal FORCE ROW LEVEL SECURITY;   -- applies to the table owner too

CREATE POLICY journal_org ON journal
  USING      (org_id = current_setting('hive.acting_org')::uuid)
  WITH CHECK (org_id = current_setting('hive.acting_org')::uuid);
```

Three things that are easy to get wrong and expensive to discover:

1. **`FORCE ROW LEVEL SECURITY`**, or the table owner bypasses every policy and
   the API's own role is usually the owner.
2. **`WITH CHECK` as well as `USING`.** `USING` filters reads; without
   `WITH CHECK` a caller can INSERT rows into an org it cannot read.
3. **The API connects as a role that is not superuser and does not own the
   tables.** `BYPASSRLS` and superuser both defeat the entire mechanism.

Membership is checked once at session start to decide whether `acting_org` may
be set; RLS then enforces it on every statement without the application
re-deriving anything.

## Encryption

At rest only, and deliberately light. The server reads plaintext because it
must: full-text search and pgvector similarity cannot operate on ciphertext,
and search is the product.

* Volume or cluster-level encryption for the database and the file path.
* TLS in transit.
* **Column encryption for `cc_credentials` only** — mail account passwords are
  never searched, so encrypting them costs nothing, and their exposure is the
  one that is catastrophic rather than merely bad.

Two consequences, both accepted:

* **Crypto-shred is gone**, and so is the constraint it imposed. Delete is now
  `DELETE`, plus unlinking the file. That resolves "do not keep deleted data",
  which was genuinely in tension with an append-only log of immutable prose.
* **Blind hosting is gone**, not deferred. A server that reads cannot hold
  another household's journal without reading it. Hosting for other people
  becomes a trust statement, not a cryptographic guarantee.

## Files

API-first, storage-swappable:

```
GET /api/artifacts/:id/content   ->  streams bytes
```

Backed by a local path now (`<data>/artifacts/<hh>/<sha256>`), object storage
later behind the same route. Content-addressed by sha256 so the swap is a
driver change and not a data migration. Range requests from the start, because
the streaming tier in `docs/ARTIFACTS.md` depends on them and retrofitting
ranges is worse than having them.

`docs/ARTIFACTS.md` otherwise carries over unchanged: artifact types, derived
text from OCR / speech-to-text / captions, thumbnails eager and originals on
demand, derived text authored by its model.

## Offline cache

The Solid frontend caches in IndexedDB. Scope it honestly: this is a **cache**,
not local-first. Reads served from it when the network is gone; writes queued
and replayed.

The hard part is not caching, it is **write conflicts**, and the old
architecture answered them with a per-device append-only log that no longer
exists. Two options, and one should be chosen before the first offline write
ships rather than after:

* **Last-write-wins per field**, with the server as the authority. Simple,
  lossy, and honest about being lossy.
* **Queue and reject.** Offline writes replay; a conflicting one surfaces to
  the human instead of resolving itself.

Anything more ambitious is a CRDT, which is a different project.

## Open questions

1. Which offline conflict model, above.
2. Does an org own artifacts, or a user within an org? It changes what happens
   when someone leaves.
3. Roles: is `memberships.role` a flat enum now, or does it need to be
   per-resource from the start? Flat is right until it very suddenly is not.
