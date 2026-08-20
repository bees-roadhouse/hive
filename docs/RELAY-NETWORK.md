# The network around the relay: discovery, failover, and choosing your own root

Design note, 2026-08-19. Builds on `docs/RELAY.md` (the spike that built and
proved the blind relay), `docs/WEB-APP.md` (the architecture the relay serves),
and `docs/DECISION-offline-conflict-model.md` (why there is exactly one
authoritative server per tenant). This note answers an architecture question
Nate asked on 2026-08-19 and turns the answer into a workstream.

## The question, stated

Should hive go back to peer-to-peer: full nodes and light nodes, where mobiles
and tablets are light nodes and 24/7 full nodes serve API access? Should
bees-roadhouse run a transparent full node that stores every tenant's data so
peers can discover each other? Or store nothing and be only a transparent
relay / connection broker? Can other people run their own to serve the public?
And can a user point the app at their own root service instead of the default
one?

Short answer: the transparent broker is the shape that got built, and it is
the only one of those shapes consistent with the threat model. The rest of the
sketch decomposes onto things that already exist or are small. What genuinely
does not exist is the network AROUND the relay: how an agent finds one, how it
fails over, and how strangers register. That is this note.

## What each piece of the sketch maps to

| in the sketch | in the tree |
|---|---|
| full nodes on 24/7 serving API access | `hive-api`. Any machine running it is a full node. Nothing to build. |
| light nodes (mobile, tablet) | the SPA, plus the offline read cache and write queue (PR #139). A native shell later is one more client of the same API, not a new tier of node. |
| transparent full node storing tenant data for discovery | rejected below. Discovery needs a directory; it does not need anybody's journal. |
| transparent relay / connection broker storing nothing | `hive-relay`: SNI-routed, spliced, holds no keys, stores no payload. Proven by `relay/tests/blindness.rs` and the live demo. |
| other people run their own to serve the public | `hive-relay` is already a standalone binary and needs no access to tenant data. The gap is enrollment (N3), not the daemon. |
| host your own for your tenant alone | the default. `HIVE_RELAY_ENABLED=0` and the network never exists for you. |
| root lookup changeable client-side | already true everywhere it matters; the next section says precisely how. |

## The root lookup is already changeable, so say precisely how

* **Browser:** the URL bar. The SPA is served by the instance itself and calls
  a relative `/api` (`packages/web/src/api.ts`), so switching hives is
  switching URLs. There is no instance switcher to build into the web app; the
  lookup is DNS, and it should stay DNS.
* **Household agent:** `HIVE_RELAY_URL` in `relay/src/bin/agent.rs` picks the
  relay. Env-configured today, single-valued, no failover.
* **AI clients (MCP):** the endpoint URL lives in the client's own config ...
  Claude Desktop, an MCP plugin in any agent harness, anything speaking
  `POST /mcp`. Changing roots is changing that URL. Hive does not own that
  config and should not try to.

So "the app can change its root service from the default" is not a feature to
add. It is a property to document, plus one place to stop being
single-valued: the agent.

## The portability property nobody has stated yet

Your address is married to your relay's zone. `<id>.relay.beesroadhouse.com`
resolves because that relay operator's wildcard DNS points at that relay.
Leave the relay and the name dies with it. Three ways to hold an address
across relays, in the order we would recommend them:

1. **Your own domain.** CNAME `hive.example.com` at the current relay's zone;
   moving relays is re-pointing the CNAME and the address never changes. This
   also simplifies ACME DNS-01 (RELAY.md Open #1): your own zone, your own
   challenge, and the instance still generates its own key.
2. **Accept the address change.** Instance ids are unguessable by design, and
   addressing is never authentication (D35), so a new address costs a
   redirect notice to your people and nothing cryptographic.
3. **(Later, and resist it) a name service in the directory** mapping a stable
   name to a current zone. This is the only proposal that would make the
   directory load-bearing for addressing, and a hint you can ignore is safer
   than a lookup you must trust. Do not build it until 1 and 2 have actually
   hurt someone.

## The three build items

### N1 ... the agent learns more than one relay

`HIVE_RELAY_URL` becomes `HIVE_RELAY_URLS` (comma-separated, ordered; the
singular stays as an alias). The agent tries in order, sticks while the
control session is healthy, and falls to the next after repeated failure.

One instance holds exactly one registration at a time. Registering the same
id at two relays does not double reachability, it splits the address ... the
zone is part of the name, so the same id on two zones is two different hives
as far as any browser is concerned. Failover therefore CHANGES the household's
address unless the household runs its own CNAME (above), and the operator docs
should say that plainly rather than pretend multi-relay is transparent.

Tests: `relay/tests/tunnel.rs` style ... two daemons on `127.0.0.1:0`, one
agent, kill the first daemon, assert the second ends up serving.

### N2 ... the directory: a list of relays, never of instances

The directory answers one question: "I want to run a hive; which relays will
have me?" It is a versioned JSON document listing relay ZONES ... zone, control
`host:port`, operator contact, one line of terms, a capacity hint, date added.
Two carriers, both boring on purpose: a file in a public repo that operators
PR themselves into (the DNS-registry model: reviewable, no new service to run),
and later the same document at a well-known path on each root operator's site.

Two rules keep the directory safe to exist:

* **It lists relays, never instances.** Certificate Transparency already makes
  instance ids on a zone enumerable; the directory must not amplify that into
  a browsable census of households.
* **It is a hint, not an authority.** The agent consumes it only as fallback
  candidates when its configured list is exhausted, behind
  `HIVE_RELAY_DISCOVER=1`, default off. Worst case, a malicious entry points
  an agent at a hostile relay ... and a hostile relay's worst case is already
  bounded by blindness: it can count you, correlate you, and refuse you, and
  it cannot read you or impersonate you, because it never held your key.

### N3 ... enrollment: RELAY.md Open #4 made actionable

Today a relay's registration tokens are env vars shared out of band, which is
why "run a relay" works and "run a relay for the public" does not. The build:
the daemon grows an admin surface (the same listener family as `/status`,
behind an admin token, never on the public ingress listener) that can mint an
instance id plus registration token, hold a reserved-name list, and record the
label at registration time rather than trusting the agent's hello.

Private relays keep env vars forever. Enrollment is a feature of a relay that
hosts strangers, not a tax on one that hosts your family.

## Why all of this is safe to build

Every new party introduced above ... public relay operators, the directory,
the enrollment flow ... inherits the same bounded worst case, because the
load-bearing property is already tested rather than asserted: the relay is
blind (`relay/tests/blindness.rs`, the control half included). What a relay
can hurt is metadata and availability. What it cannot hurt is content and
identity.

The one rule that must survive every phase: **the instance generates its own
key** (RELAY.md Open #1). A directory entry, relay, or enrollment flow that
ever holds an instance private key turns "cannot read your journal" into
"cannot read it today," and no convenience is worth that sentence.

## Explicitly not this workstream

* **Full-node replication between tenants.** Two 24/7 nodes each accepting
  writes for the same org is multi-master: divergent journals, double-emerged
  tasks, and conflict resolution that `docs/WEB-APP.md` priced as "a different
  project" when it superseded the P2P architecture on 2026-08-10. The offline
  model works precisely because it is a client cache in front of ONE
  authoritative server (`docs/DECISION-offline-conflict-model.md`). Reopening
  this is a decision-record reversal, not a feature.
* **A "transparent" node that stores tenant data.** Search and embeddings
  need plaintext, so a node holding a household's journal can read it, which
  is the opposite of transparent. WEB-APP.md retired blind hosting with the
  sentence that still stands: hosting for other people is a trust statement,
  not a cryptographic guarantee.
* **Hosted instances as a bees-roadhouse product.** Nate, 2026-08-19: yes,
  eventually. The architecture already supports it ... multi-org RLS IS tenant
  isolation, and one `hive-api` can host many families today. The gap is
  operational and legal, not architectural: per-org backup/export, usage
  metering, the roles tripwire in `docs/DECISION-roles-model.md` (per-project
  admin is the requirement that forces that migration), and terms that price
  hosting as the trust statement it is. Track it; do not build it in this
  program.

## Workstream breakdown

| item | owns | depends on |
|---|---|---|
| D1: multi-relay agent + operator docs | `relay/src/agent.rs`, `relay/src/bin/agent.rs`, `relay/tests/` | nothing |
| D2: directory format + repo-file carrier + agent hint consumption | new `docs/RELAY-DIRECTORY.md`, small `relay/src/directory.rs`, agent config | D1 (the consumer exists) |
| D3: enrollment admin surface in the daemon | `relay/src/daemon.rs`, `relay/src/control.rs`, `relay/src/bin/relay.rs` | nothing, but it gates "public" relays |
| D4: docs ... "choosing your root" section for self-hosters, RELAY.md cross-refs | `docs/SELF-HOST.md`, `docs/RELAY.md` | D1-D3 (docs describe what exists) |

D1 and D3 are independent and can run in parallel under the multi-agent
pattern. D4 lands last so the docs never describe unbuilt behavior ... the
rule this repo keeps relearning.

Sequencing against the current program: none of this touches `core/`, `api/`,
or `packages/web`, so it does not contend with Phases 3-5. It can start
whenever a relay-phase slot opens, and it should not preempt the merge queue
(#141, #143, #144, #142, then #139).
