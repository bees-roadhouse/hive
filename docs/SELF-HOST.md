# Self-hosting, the relay, and multi-user

> **Superseded. History, not orientation.** The architecture
> this note amends is gone: [WEB-APP.md](./WEB-APP.md) replaced the local-first
> design wholesale, and [RELAY.md](./RELAY.md) reversed this note's headline
> addressing decision (D44's path routing lost to subdomain + SNI passthrough,
> and the record of why lives there). Nothing below describes the tree. There
> is no desktop app that "is the server", no Node `hive-server`, no
> `node-core`, no SQLCipher `index.db`, no op log, no `sync/` crate, and no
> `SegmentVault`. The current shape is one Rust binary, `hive-api`, serving
> the JSON API, `POST /mcp`, and the SolidJS SPA over PostgreSQL with
> row-level security; the container path is `docker/docker-compose.rust.yml`.
>
> What landed anyway, in the rewrite's own shape:
>
> * **Accounts and multi-user** ... as orgs with RLS policies, not as a
>   tenancy console. "Authorization becomes a WHERE clause" below arrived
>   literally: it is a Postgres policy predicate now, not application code.
> * **The account-linking rules** ... link on `(issuer, subject)`, never on
>   email, not even `email_verified`. That rule is in force and under test
>   (`api/tests/oidc_linking.rs` cites this file).
>
> What did not land: the relay on/off switch gating credential setup (there is
> an onboarding form instead), shipped default OIDC registrations for Google
> and Microsoft (an IdP is bring-your-own via `OIDC_ISSUER` /
> `OIDC_CLIENT_ID` / `OIDC_REDIRECT_URI`), and the id/alias/reclaim namespace
> design (`relay/` is an SNI-passthrough spike; see RELAY.md). Stale passages
> are marked in place below rather than removed.

Design note, 2026-08-10. Amends D16, D29, and D33. For reaction.

## The shape

One artefact, two packagings, one relay.

```
   desktop app                          docker compose
   ┌────────────────────┐               ┌────────────────────┐
   │ Solid renderer     │               │  (no renderer)     │
   │ hive-server (Node) │               │  hive-server (Node)│
   │ node-core          │               │  node-core         │
   └─────────┬──────────┘               └─────────┬──────────┘
             │ outbound dial                      │ outbound dial
             └──────────────┬─────────────────────┘
                            ▼
                    public relay (ours)
                            ▲
                            │ outbound dial
                  external client (phone, laptop)
```

The desktop app **is** the server. Running it for yourself is loopback only.
Flipping the public-relay switch is what turns it into a multi-user tenant, and
that switch is what forces credentials to be set, because an exposed API with
no authentication is not a thing to offer.

The container is the same `hive-server` with the renderer left out, for people
who want it beside their other services rather than on a desktop.

Both dial **outbound** to the relay. That is the entire point: no port
forwarding, no DNS, no Traefik, no certificate wrangling. A person who wants to
reach their own data from a coffee shop should not have to learn what a reverse
proxy is.

> **The instinct landed; both boxes in the diagram are gone.** Outbound-only
> dialing is exactly what `hive-relay-agent` does today (RELAY.md). But the
> "desktop app" and the Node `hive-server` do not exist: the thing the agent
> fronts is `hive-api`, one Rust binary serving API and SPA, and the
> headless packaging is `docker/docker-compose.rust.yml`, not a renderer-less
> variant of a desktop app.

## What this amends

- **D16 — "single user, no accounts."** Superseded. Accounts exist, and a
  tenant is a multi-user boundary.
- **D29 — the blind tier.** Superseded for the *server*. The server holds keys,
  folds, and reads. It is a normal application server.
- **D33 — tenancy.** Shrinks. A tenant is grouping, quotas, and membership; it
  is no longer a console for data the operator cannot read.

**The relay is NOT amended, and stays opaque.** Blind-server and blind-relay
were conflated in earlier discussion and they are separate decisions. We host
the relay, so it carries other families' traffic, and it must not be able to
read it. That property is cheap to keep and expensive to lose.

Encryption at rest also stays. `index.db` is already SQLCipher; the server holds
the key while running, which is ordinary, and a stolen laptop or a stolen backup
is still protected.

> **Reversed by decision.** WEB-APP.md chose encryption at rest ONLY: full-text
> search and pgvector similarity cannot run on ciphertext, and search is the
> product. There is no `index.db` and no SQLCipher; Postgres holds plaintext.
> The exception is the credential vault (mail account secrets), which is
> AES-256-GCM under `HIVE_CRED_KEY` (`core/src/store/cc_credentials.rs`) — back
> that key up separately from the database.

Authorization becomes a `WHERE` clause. Membership, groups, permissions. The
consequence worth stating: **revocation now actually revokes**, which the
cryptographic design could not do — it could only stop granting access to
future records.

> **Landed, one level down.** The `WHERE` clause is a row-level-security policy
> applied by Postgres itself (`core/src/db.rs`), keyed on an acting org stamped
> per connection, with `FORCE ROW LEVEL SECURITY` so not even the table owner
> ducks it. Revocation really does revoke: delete the membership and every
> subsequent statement sees nothing.

## Onboarding

> **Landed in a different shape.** There is no relay switch in the app, so the
> gate this section builds around it does not exist. A fresh database opens
> onto an onboarding form (instance name, first admin, password) and the
> instance is credentialled from the first write. OIDC is present but
> bring-your-own: it activates when `OIDC_ISSUER`, `OIDC_CLIENT_ID`, and
> `OIDC_REDIRECT_URI` are all set (`HIVE_OIDC_ENABLED` can force it off), and
> no Google/Microsoft registrations ship. The linking rules below, by
> contrast, landed as written and are covered by `api/tests/oidc_linking.rs`.

1. **First run, relay off.** Loopback only, single user, no credentials. The
   app works exactly as it does today.
2. **Relay switched on.** Blocks until a username and password exist. Optional
   identity providers can be added afterwards.
3. **Identity providers.** We ship default OIDC registrations for Google and
   Microsoft that an org can choose to trust. Orgs may add their own.

### Account linking rules

One local identity, many `(issuer, subject)` pairs. Unique index on
`(issuer, subject)`, so a provider account maps to exactly one identity ...
otherwise compromising it unlocks two, and "who is this" has no deterministic
answer.

**Never link on email address.** Link on the `sub` claim, issuer-scoped. Email
is a display attribute, not a join key, and not even `email_verified: true`
changes that, because it is an assertion by an IdP we do not control. The
classic takeover is registering the victim's address at a trusted IdP and
letting the match do the rest.

Which forces the flow: **adding a second provider requires already being
authenticated with the first.** "Sign in, then add Google." Never "sign in with
Google and we will find your account."

## Addressing and id ownership

> **Reversed before it shipped.** This section is D44's path routing, and
> RELAY.md carries the record of why it cannot work with a blind relay: routing
> on a path means reading the request line, the request line is inside TLS, and
> a relay that terminates TLS reads everything. What `relay/` implements is
> subdomain addressing plus SNI passthrough — the relay reads only the SNI in
> the ClientHello and splices raw sockets. The id/alias/reclaim namespace below
> is unimplemented; "addressing is never authentication" (D35) survived and is
> load-bearing precisely because per-instance names are enumerable through
> Certificate Transparency.

Decided 2026-08-10: **path routing, `relay.example/<id>`.** Not a subdomain.

Three reasons, the third of which is easy to miss:

1. One DNS name, one certificate. No wildcard, no per-instance issuance.
2. The relay still cannot read anything. It terminates the OUTER TLS to its own
   name, reads the path, routes, and forwards the inner client-to-instance
   stream blind. Path routing and an opaque relay are compatible; that is not
   obvious and it is why this is cheaper than SNI.
3. **Subdomains would publish every instance id.** A per-instance certificate
   lists its names in Certificate Transparency logs, so `<id>.relay.example`
   becomes public the moment it is issued. Path routing leaks nothing.

The relay does see which id a connection is for. That is unavoidable metadata in
any relay and is not worth contorting the design over.

### Two names, different jobs

**The canonical id is permanent and never recycled.** House format, so
`new_id()`'s `nanoid` rather than a UUID ... the tree already reads
`prefix_<nanoid(12)>` everywhere and a GUID would be a second convention for no
gain. Path routing removes the 63-character DNS label limit, so length is free:
use the enrollment code's 21 characters rather than 12, because this one is a
public identifier and 104 bits costs nothing.

**An alias is a mutable pointer.** User-chosen, releasable, re-assignable, and
optional. It resolves to the same instance the canonical id does.

### Why recycling an alias is safe

The rule is D35's, applied to a relay path instead of DNS: **addressing is never
authentication.** A client pins the instance key at enrollment, so if an alias
changes hands, a client still pointed at it fails the pin and refuses the
connection rather than quietly talking to a stranger. The name moved; the
identity did not.

That is what makes a relaxed reclaim policy affordable. Without key pinning, a
recycled alias would be a hijack primitive and aliases would have to be
permanent.

### Reclaim

Two paths, because losing everything is the case that matters:

- **Hold the instance key.** Normal reinstall or restore from backup: the
  instance proves possession and keeps its id. Nothing to do.
- **Hold the recovery code.** Issued at registration, in the same
  `BASE32_NOPAD` alphabet as the enrollment and recovery codes ... the tree's
  stated rule is ONE way of writing a secret a human retypes, and this is one.

Abandoned **aliases** expire after a period of no connections and return to the
pool. Abandoned **canonical ids** never return; the id space is effectively
infinite and reuse buys nothing.

Reserved alias list from the start: `www`, `api`, `admin`, `mail`, `relay`,
`app`, `status`, `_acme-challenge`, and the usual protocol names. Impersonation
of well-known brands needs a policy before the relay is public, not after ...
a free public namespace attracts exactly that.

**The honest tension:** any reclaim path is also an attack path. Whatever proves
"this id is mine" to a legitimate owner proves it to whoever else can satisfy
it. The recovery code IS the security boundary for id ownership, so it deserves
the same care as the master key: shown once, stored by the human, never
recoverable from us.

## Offline cache and incremental sync

> **Substrate deleted.** There is no op log, no gapless `seq`, no blake3 chain,
> and no `sync/` offer/want session — this section describes replication over
> storage that WEB-APP.md replaced with a shared Postgres. Offline in the
> current SPA is a cache, not local-first, and the write-conflict model is an
> open question there.

The op log already gives this its shape. Every record carries a gapless `seq`
per device and a blake3 chain, so incremental sync is "everything after seq N"
and nothing more. `sync/`'s existing offer/want session is the right protocol;
what changes is that the peer can now read what it holds.

A client caches only what its identity may see, so the server filters before the
offer rather than after.

## Open questions

**1. Does the relay terminate TLS?** It must not. Which means either SNI-based
forwarding (the instance holds a cert for its own relay name, which needs cert
delegation or ACME DNS-01 through our zone) or a tunnel where the real
client-to-server TLS runs *inside* the relay connection and the relay only sees
framed ciphertext. The tunnel is simpler to reason about and needs no cert
delegation. It is also what "the relay is RPC over opaque frames" already
implied.

> **Answered in code.** SNI passthrough: the relay reads the SNI from the
> ClientHello (plaintext, before key agreement) and splices sockets without
> decrypting; the agent terminates TLS in front of `hive-api`. RELAY.md.

**2. How does a client address its instance?** DECIDED: by path,
`relay.example/<id>`. See *Addressing and id ownership* below.

**3. "Don't keep deleted data" versus an append-only log.** These are in
tension and the note should not pretend otherwise. Blobs are fine ... destroying
the wrapped content key is already the delete, and it is real. But journal prose
lives *in* the log record, and `journal.rs` persists "immutable prose" behind a
hash chain. Deleting it means either rewriting history, which breaks the chain,
or accepting that the log retains it.

> **Moot — the log is gone.** Rows in Postgres delete plainly. Journal entries
> are still treated as history by convention (status changes render from
> canonical state rather than rewriting old bodies), but that is a modeling
> rule, not an append-only substrate. Artifact bytes are content-addressed
> under `<data>/artifacts/<org_id>/` and delete is a refcounted unlink
> (`core/src/artifact_storage.rs`).

Two readings, and they need different work:
   - *Cache hygiene* (likely what was meant): the offline cache drops anything
     deleted upstream or that the identity lost access to. Straightforward.
   - *True erasure*: the log itself must be able to forget. That is a real
     feature with a real design, and it should be scoped separately.

**4. Auth for loopback.** With the relay off and a single user, requiring a
password is friction protecting nothing. With the relay on, everything needs
auth. The switch between those two states is the interesting part: what happens
to data written before credentials existed, and does turning the relay back off
relax anything? Proposal: it does not. Once a tenant has accounts, it has
accounts.

## What survives from the current tree

> **None of the three items below survived.** `sync/src/enroll.rs`, the op
> log, and the blind relay's `frame.rs` were all deleted with the local-first
> stack. What did survive is the posture this section was really about: the
> relay assumes an untrusted intermediary, and `relay/` (a spike, RELAY.md)
> keeps it.

Not everything built for the blind design is wasted:

- **The enrollment ceremony** (`sync/src/enroll.rs`) is still the right way to
  add a device. Pinning plus a short auth string compared on both screens beats
  a password prompt, and it is already written.
- **The op log** is still the right storage shape. It is what makes backup
  trivial, incremental sync obvious, and point-in-time free.
- **The relay threat model** already assumes an untrusted intermediary
  (`frame.rs:212`, `enroll.rs:113`), which is exactly the posture we keep.

What does shrink is `node/`'s `SegmentVault` and most of what surrounds it. It
exists *because* the node cannot fold. A node that can read is a store.
