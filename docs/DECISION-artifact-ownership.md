# DRAFT — Artifact ownership: org, or user-within-org

Status: **DRAFT, 2026-08-18.** Proposed answer to `docs/WEB-APP.md` open
question 2 ("Does an org own artifacts, or a user within an org? It changes
what happens when someone leaves."). Not ratified; ratification is the repo
owner's call, and merging the PR that carries this file IS the ratification.

## The question

An artifact — a photo, a scan, a PDF, an uploaded file — lands in an org with
a human's name on it. When that human leaves the org, what happens to their
uploads? The options are the two ownership models: the **org owns** every
artifact (the uploader is provenance), or the **user-within-org owns** their
uploads (departure implies retain/export/purge policy).

## Ground truth in the tree

The org-ownership model is not one of the options on the table; it is what is
already built. Three load-bearing facts:

1. **The org is in the content address.** Bytes live at
   `<data_root>/artifacts/<org_id>/<hh>/<sha256>`
   (`core/src/artifact_storage.rs`). The module header states why: a delete
   must answer "is anything still referencing these bytes?", and that count is
   only answerable within the acting org — a globally shared address would
   make an org-scoped `DELETE` able to unlink bytes another org still holds,
   or require a `SECURITY DEFINER` escape out of the very policy that is
   supposed to be unbypassable.
2. **The row is org-scoped and nothing else.** `artifacts` carries `org_id`
   and sits under RLS like every content table; dedup is keyed
   `(org_id, sha256)`; the advisory lock, the refcount, and the sweeper all
   take the org as their unit (`core/src/store/artifacts.rs`). There is no
   per-user predicate on the table at all — it is not in `USER_SCOPE_TABLES`,
   so even the namespace widening that journal/mail/entities have does not
   apply. Every member of the org sees every artifact row in it.
3. **`created_by` is provenance, not ownership.** It is a free-text actor
   string with no FK and no policy attached. `docs/ARTIFACTS.md`'s authorship
   model (derived text is authored by its model; authorship is a rendering
   distinction, not an access boundary) is the same idea one layer up.

And the departure path, such as it is: **there is no member-removal
endpoint.** Nothing in `core/src/store/orgs.rs` removes a membership; the only
`DELETE FROM memberships` in the tree runs inside the admin actor-purge
cascade (`core/src/store/actors.rs`), which drops the membership, the org's
sessions and tokens, and the global `users` row only when no other org holds
it. Notably, that cascade touches **nothing in `artifacts`**: a purged
member's upload rows and bytes survive, still attributed to their actor
string. So today the de facto answer to "what happens when someone leaves" is
already *nothing happens to their uploads* — the org keeps them.

## Option A — org owns (ratify the de facto)

An artifact belongs to the org the moment it lands. `created_by` stays as
provenance for display and audit. A departing member's uploads remain,
readable and deletable by the org's remaining members exactly as before.

*For:* it is the only model consistent with the storage layer as built; it
matches how the rest of the content schema behaves (journal entries, tasks,
and entities all stay in the org when their author leaves — the actor-purge
cascade is a deliberate admin action, not a departure side effect); household
and team reality is that the scan of the insurance policy belongs to the
household, not to whoever held the phone. It also keeps the delete/refcount
invariant airtight: the counter that protects bytes never has to ask a
cross-owner question.

*Against, stated honestly:* a member cannot take "their" files with them as a
first-class operation, and cannot demand their uploads be purged on exit —
there is no per-user delete selector. If hive is ever used in a context where
uploads are personal effects rather than org records (a shared workspace of
freelancers rather than a household), this answer will chafe. Export, when
someone wants it, is a manual "download what you can read before you lose
access" — which RLS already makes exactly the right set.

## Option B — user-within-org owns

`artifacts` gains an owner column with real semantics: per-user read policies,
a retain/export/purge choice at member removal.

*For:* matches a multi-tenant SaaS intuition; gives departure a story.
*Against:* it is not a column, it is a second authorization axis through the
one layer that cannot afford one. The delete path's refcount
(`blob_refcount`, which must see *every* row pointing at the bytes or it will
unlink live data) would need to count rows the acting user cannot see —
punching a hole in the per-user policy or reaching for `SECURITY DEFINER`,
the exact escape hatch the org-scoped address exists to avoid. Dedup across
members of one org (the same photo uploaded by two people — the case the
storage comments call out as where the wins are) becomes either a shared row
with two owners or silently disabled. And it duplicates machinery the product
already has: per-viewer read sharing is the `shares` table's job, and it works
today. Per-user artifact ACLs would be a second, parallel sharing system
serving files only.

## Recommendation: the org owns artifacts; ratify the de facto

Adopt Option A. The decision is not really being made here so much as
*noticed*: the content address, the RLS shape, the refcount invariant, and the
existing departure behavior all already encode org ownership, and every one of
them would get worse under per-user ownership. The user-facing rule to write
down is:

> **Leaving an org does not move, export, or delete your uploads. They are
> the org's records; your name on them is history, not custody.**

Two corollaries fall out:

* **The departure story is access, not data.** Removing a member ends their
  ability to read (membership gone → no session can pin that org → RLS denies)
  and changes nothing they stored. A member who wants copies downloads them
  while they still hold access. This is the same posture as the journal and
  every other content table — artifacts stop being a special case, which is
  most of the value.
* **Org teardown is already handled; member teardown is a new endpoint that
  must not touch artifacts.** The sweeper is deliberately driven off the
  *storage driver's* org enumeration (`artifacts_sweep_all`), not the `orgs`
  table, precisely so bytes whose rows are all gone still get reclaimed — an
  org deleted wholesale leaves reclaimable litter, and the machinery exists.
  The gap is only that *removing one member* has no route today; when one is
  added (an admin `DELETE /api/orgs/{id}/members/{user}` or equivalent), this
  decision constrains it: drop the membership and org-scoped sessions/tokens,
  leave `artifacts` rows and bytes untouched.

If a genuine personal-custody use case arrives later, the honest shape is a
separate visibility seam (the way `shares` gates journal reads) decided as its
own record — not an owner column retrofit onto `artifacts`, which would
re-open the refcount hole this design closed.

## If ratified, what changes

**Schema:** none. `created_by` stays a free-text provenance string; no owner
column, no new policy.

**Code:** none today. The one named future change: a member-removal endpoint
(admin-gated, `ctx.is_admin()`), implemented as membership + org-scoped
session/token deletion — and a test in `api/tests/org_isolation.rs` pinning
that a removed member's artifacts remain visible to the org and unreachable by
them. The existing actor-purge cascade keeps its current, stronger semantics
(it is account deletion, not departure).

**Docs:** `docs/WEB-APP.md` "Open questions" loses item 2. `docs/ARTIFACTS.md`
gains a short ownership paragraph pointing here (it already carries a "live
design, stale mechanism" banner, so this rides that convention). The
`artifact_storage.rs` header comment stays the canonical statement of the
refcount invariant; this record is the canonical statement of the policy on
top of it.
