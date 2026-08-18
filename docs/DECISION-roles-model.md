# DRAFT — `memberships.role`: flat enum, or per-resource from the start

Status: **DRAFT, 2026-08-18.** Proposed answer to `docs/WEB-APP.md` open
question 3, which warns: "Flat is right until it very suddenly is not." Not
ratified; ratification is the repo owner's call, and merging the PR that
carries this file IS the ratification.

## The question

`memberships(user_id, org_id, role)` carries one TEXT column, flat:
`'admin' | 'member'`. Should it stay flat until a concrete requirement breaks
it, or should the schema carry per-resource roles (admin of *what*) from the
start, before rows accumulate?

## Ground truth in the tree

The flat model is small, fully built, and — this matters for the migration
arithmetic — **funneled through exactly three chokepoints**:

1. **The column.** `memberships.role TEXT NOT NULL DEFAULT 'member'`
   (`core/src/db.rs`). `users.role` is a *different* thing — an account
   default that seeds new memberships — and `middleware.rs` documents that
   nothing authorizes on it, after the defect where a global admin became an
   admin in every org they joined.
2. **One application gate.** `AuthCtx.is_admin()` (`api/src/middleware.rs`):
   the membership's role in the acting org is `admin`, AND the principal is a
   session or a token acting as the same human who granted it (delegated/AI
   tokens keep the namespace but not the admin bypass). Every HTTP admin
   route — users, tokens, actor delete/merge, entity types, journal
   reassign-scope, legacy import, dashboard/graph, OAuth client revocation,
   mail account administration — and every admin MCP tool
   (`api/src/mcp.rs`: `actor_delete`, `actor_merge`, `entity_type_*`,
   `dashboard`) calls this one method. The mcp.rs header records why: the old
   path matched a free-text token `actor` against the users list, so naming
   an AI identity after an admin was a privilege escalation. Gates now read
   the credential's role in its acting org, never a name it carries.
3. **One database predicate.** The pool stamps `hive.acting_admin` onto every
   connection from `ActingScope.admin` (`core/src/acting.rs` — "widens the RLS
   predicate past the per-user namespace, never past the org"), and exactly
   one function spends it: `org_predicate()` in `core/src/db.rs`, which for
   namespaced tables emits
   `org_id = acting_org AND (owner IS NULL OR owner = acting_user OR acting_admin)`.
   Policies are rebuilt from this function at every boot, so a predicate
   change is an edit here, not a migration framework.

Two adjacent facts keep the picture honest:

* **Per-resource *read* sharing already exists** and is not a role: the
  `shares` table grants a viewer an entry or a whole journal, and
  `journal_read_predicate()` widens reads by explicit grant while `WITH CHECK`
  refuses to widen writes. Anyone arguing "we already have per-resource
  machinery" is looking at `shares`; it answers "who may read this", never
  "who may administer what".
* **Admin is currently a bundle.** One flag buys: member/token management,
  actor merge and delete (full content reassignment), entity-type DDL,
  journal scope reassignment, legacy import, mail account administration — and
  through the RLS predicate, *read access to every user's private namespace in
  the org*: their journal, mail, entities, credentials views, and workspaces.

## The concrete requirement that breaks flat

Name it, per the question's own warning. It is **per-project (or
per-mailbox) administration**: "Maggie administers the household org's
projects — moves tasks, curates entity types — without being able to read
Nate's journal namespace." The first half is a role narrower than org-admin;
the second half is what makes it impossible to fake with today's flag, because
`acting_admin` in the RLS predicate is *precisely* the power to read every
namespace. Any role that means "manage some resources" but not "read
everything" cannot be expressed as a third string in the current predicate —
`org_predicate()` has exactly one escape hatch and it is total.

The same requirement arrives by a second road: the moment an org wants an AI
agent to do routine administration (triage entity types, resync a mailbox)
without handing its token org-wide read, the bundle splits. `is_admin()`
already refuses admin to delegated AI tokens *as a hard-coded rule*; a
per-resource model is that judgment made configurable.

Lesser triggers that do NOT break flat, so they don't get to argue: a display
label ("owner" vs "admin"), audit granularity, and anything about reads
(`shares` covers them).

## Option A — per-resource from the start

A grants table now: `role_grants(user_id, org_id, resource_kind, resource_id,
role)`, predicates join it, `is_admin()` gains a resource argument at every
call site.

*For:* no migration later; the bundle never forms. *Against:* it is
speculative machinery with zero current consumers — every resource kind today
wants exactly the same two answers. It multiplies the RLS predicates' cost and
review surface at the exact layer where AGENTS.md says an omission is a hole;
it adds a join (or an EXISTS per grant scope) to statements issued on every
request; and it re-opens the settled `is_admin()` call sites to carry a
resource context most of them cannot know (what project is `POST
/api/journal/reassign-scope` *about*?). Building it now means designing the
resource taxonomy before the requirement that would inform it exists — the
taxonomy *is* the hard part, and guessing it does not de-risk the migration,
it just moves the mistake earlier.

## Option B — flat, with the tripwire and the seams named

Keep `memberships.role` flat. Write down what breaks it (above) and keep the
funnels clean so the later migration is mechanical.

*For:* matches the evidence — one org, one household-scale user count, zero
resource kinds asking for differentiation. The migration cost is capped by
construction (below). *Against, stated honestly:* the day per-project admin
arrives, every `is_admin()` call site needs re-review, not just a recompile;
and the longer flat ships, the more an org's admins have accumulated a
de-facto power (reading all namespaces) that narrowing will feel like taking
away. That is a product-conversation cost, not a data cost.

## Recommendation: flat — Option B — with the migration bill priced now

Stay flat. The reason is not "simple is better" sentiment; it is that the
expensive half of per-resource roles is the *taxonomy and the predicate
design*, both of which are unknowable until the requirement arrives, while the
cheap half — the plumbing — is already positioned to make the swap mechanical:

* one gate method (`AuthCtx.is_admin()`),
* one policy builder (`org_predicate()` / `journal_read_predicate()`),
* policies rebuilt from code at every boot (no migration framework needed to
  change predicate text),
* and `shares` proving the per-resource *read* pattern in production, ready to
  be generalized from rather than invented.

**The priced bill, when the tripwire fires:** add `role_grants`; extend
`ActingScope` beyond one bool (likely a small granted-scope set resolved with
the membership at credential resolution — the same one lookup that happens
today); re-express `org_predicate()`'s `acting_admin` arm as a grants-aware
predicate; re-review each `is_admin()` call site against the new taxonomy
(~15 HTTP routes, ~6 MCP tools — enumerable, not open-ended); backfill
`role='admin'` rows as org-wide grants. Days of work, not a re-architecture,
*because* the funnels held.

**The tripwire, stated as a rule:** the first PR that reaches for a third
value in `memberships.role`, or that special-cases an admin check on a
specific resource kind, stops and writes the per-resource decision record
first. Flat is right until it very suddenly is not — so the *sudden* part is
made a deliberate gate instead of a drive-by.

One hygiene item worth doing NOW, independent of this choice, because it is
cheap and preserves the funnel: nothing. The tree is already disciplined —
`users.role` authorizes nothing, `is_admin()` is the only gate, and mcp.rs's
comment names the old escalation so it stays fixed. The recommendation is
genuinely "change nothing, write the tripwire down."

## If ratified, what changes

**Schema:** none. `memberships.role` stays `TEXT DEFAULT 'member'`; no grants
table.

**Code:** none. This decision is a constraint on *future* diffs: keep
authorization funneled through `AuthCtx.is_admin()` and the `db.rs` predicate
builders; no per-endpoint role inventions; no third `role` value without the
successor decision record.

**Docs:** `docs/WEB-APP.md` "Open questions" loses item 3; its
`memberships.role` schema sketch gains a pointer here. When per-resource
administration does arrive, its decision record amends this one — the same
convention DIRECTION.md used, where reopening a settled question is a record,
not a drift.
