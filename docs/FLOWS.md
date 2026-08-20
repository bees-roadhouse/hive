# Flows: wasm-pluggable workflows, the wire as flow #1, a registry that surfaces itself

Design note, 2026-08-20. Builds on `docs/WEB-APP.md` (the architecture every
decision here must not violate), `AGENTS.md` (journal-first, RLS, the hybrid
schema convention), and the direction Nate set on 2026-08-20 (hive task 117).
For reaction. The first vertical slice (F1 below) lands with this note.

## The direction, stated

1. Workflows become pluggable via WebAssembly ... any language that compiles
   to wasm can author a flow. `wire` (watch-the-wire) becomes the first flow
   and stores data.
2. Flows are visualized dynamically in the SPA.
3. Everything is stored as a journal entry with a source attribution: user,
   ai, or automation/flow. The journal stays the source of truth; flows emit
   entries.
4. MCP tools dynamically expose themselves to align with installed flows ...
   a registered flow surfaces its operations as MCP tools automatically.
5. Skills are stored in the hive and easily viewable/editable in the
   interface.
6. Keep it simple, but modifiable by the user in the interface or via the AI
   plugins/MCP.
7. (Added later the same day) **The app stops hosting agents and chat.**
   Verbatim: disable the agent functionality; the runtime for agents will be
   completely external or in flows; chatting we're leaving behind for now.
   Claude Code, Claude Desktop, Codex, and ChatGPT desktop plugins all
   connect TO hive instead.

## What each piece maps onto

Most of this direction decomposes onto structures the tree already has. The
genuinely new surface is small: a registry, a dispatch seam, and (later) a
wasm host.

| in the direction | in the tree |
|---|---|
| pluggable workflow modules | **new**: a `flows` registry (tables + store + REST) and, in F3, a wasmtime host. Module BYTES need no new storage: `artifacts` + `core/src/artifact_storage.rs` are already content-addressed, org-scoped bytes ... a wasm module is one more artifact row, pinned by sha256. |
| wire as flow #1 | `sources` + `poll_sources()` + the `wire` table + `POST /api/sources/poll`. All of it exists; it becomes the seeded `wire` flow (builtin tier, below), and its ingestion, log, and emit surface become declared operations. |
| everything a journal entry, source-attributed | `journal.author` already joins `people`, and `people.kind` already distinguishes `human` from `ai`. Attribution needs ONE new kind value (`automation`), not a new column ... D5. Provenance from an entry back to the run that wrote it is a `links` row, the house mechanism for cross-domain relations (and task 116's `link_types` registry will formalize the rel). |
| MCP tools align with installed flows | `api/src/mcp.rs` owns tools/list and dispatch; `initialize` already advertises `{"tools": {"listChanged": true}}`. The static array grows a registry-driven section ... D6. |
| skills stored, viewable, editable | `identity_artifacts` (kind = skill / agent / command, SKILL.md content, enabled flag) plus its REST and MCP surface already exist. What is missing is only the SPA editor ... D7. No new storage. |
| simple, user-modifiable via UI and MCP | the `entity_types` registry is the proven template: a slug-keyed registry, validated writes, admin-gated definition, CRUD over both REST and MCP. Flows follow its grain. |
| the app stops hosting agents and chat | the hosted-agent surface is real code today: `api/src/routes/workspaces.rs` (spawn/input/runtime-auth/runtime-oauth/cc-credentials), `api/src/routes/conversations.rs` (capture REST), the store's `workspaces` / `conversations` / `cc_credentials` modules, the `cc_sessions`/`cc_messages`/`cc_credentials`/`runtime_oauth_states` tables, and the SPA's Conversations tab + Settings runtime sign-in. D9 retires the surface (gated dark now, removed in F8) and keeps the data. |

Adjacent context filed the same day: task 115 (pinned entries + saved
filters) and task 116 (`parent_field` trees + the `link_types` registry).
Neither blocks this program; 116's registry should seed the `emitted_by` rel
that D5 uses, and the `/flows` page reuses the URL-encoded filter idiom 115
extends.

## Decisions

### D1 ... the runtime is wasmtime

wasmtime, embedded in `hive-api`, behind a `flow-exec` cargo feature until F3
proves it green.

* **It is tokio-native.** A flow op executes inside an axum request or a
  scheduler tick; wasmtime's async support lets host functions be real
  `async fn`s awaiting the store, on the runtime the process already has.
  Wasmer's async story is bolted on; WasmEdge is a C++ runtime behind FFI.
* **Untrusted code needs a leash the runtime provides.** Epoch interruption
  and fuel metering give per-run deadlines and budgets without spawning
  threads to babysit the guest. A flow that loops forever costs its run, not
  the request path.
* **It is the Bytecode Alliance reference implementation** with the strongest
  security posture and the component-model path already in it, which is where
  the ABI goes when guest toolchains mature (D2).

Prior art considered: Extism wraps exactly this shape (JSON over linear
memory, host functions, plugins in any language) over wasmtime. It would add
a large dependency to save us ~200 lines of ABI glue and would own our
plugin interface. The ABI below is small enough to own outright.

### D2 ... the ABI is host functions, not HTTP callback. Core-module JSON v1, component model later

Two ways a flow can touch hive data. It can be handed a credential and call
`POST /mcp` or the REST API back over HTTP, or the host can expose a small
set of imported functions that run in-process. **Host functions, and the
choice is security-load-bearing:**

* **No ambient authority in the guest.** An HTTP callback means a token
  inside the sandbox and a network stack (wasi-http) to use it, and a token
  that can call one endpoint can call every endpoint its principal can. That
  is the exfiltration-loop surface `docs/WEB-APP.md` spent its sharpest
  section closing. A guest with host imports has exactly the operations the
  host chose to lend it, and nothing else exists in its universe.
* **Org pinning comes free.** Host functions execute on the task that called
  the flow, inside the same `acting::scope` ... every store call the host
  makes runs under the run's acting org and RLS applies with no new
  enforcement code. An HTTP callback would re-derive auth from a credential
  and reopen the question the task-local closed.
* **No loopback coupling.** Works in tests, works before the listener binds,
  works when the operator moved the port.

**Guest ABI v1** is a core wasm module speaking JSON over linear memory ...
the lowest common denominator every toolchain can emit today (Rust, TinyGo,
C/C++, AssemblyScript, Javy). The component model / WIT is the better typed
interface and is explicitly v2, when authoring it stops requiring a
per-language bindings project.

Exports the host requires:

```
flow_manifest() -> (ptr, len)          ; the manifest JSON, read at install
flow_op(op_ptr, op_len,
        input_ptr, input_len) -> (ptr, len)  ; JSON in, JSON out
alloc(len) -> ptr                      ; host writes inputs through this
```

Imports the host provides (module `"hive"`, all JSON strings unless noted):

```
log(level: i32, ptr, len)              ; run log, capped
journal_append(ptr, len) -> (ptr, len) ; body/tags -> entry view (D5 stamps attribution)
wire_emit(ptr, len) -> (ptr, len)      ; {kind, payload} -> event; kind namespaced (D4)
wire_recent(limit: i32) -> (ptr, len)  ; recent wire events
http_get(ptr, len) -> (ptr, len)       ; url -> {status, body}; egress-allowlisted
```

`http_get` exists because flow #1's real job is fetching feeds. Egress is
allowlisted: the manifest declares `allowed_hosts`, the host enforces it, and
the allowlist is reviewable at install time. No filesystem, no sockets, no
clocks beyond a host-supplied `now` in the input envelope, no WASI beyond
what wasmtime needs to instantiate the module.

Limits, all host-enforced: a per-run deadline (default 10s, manifest may
only lower it), a memory cap, a fuel budget, an output size cap. Input
validation stays in the guest: the declared `input_schema` is the CLIENT
contract (it is what MCP serves), and the host passes arguments through as
JSON rather than growing a schema validator ... a flow that wants stricter
validation ships it, in its own language.

### D3 ... the registry is two tables; the manifest is the unit

```sql
-- Registered flows: one row per installed flow per org.
flows (
  id            TEXT PRIMARY KEY,          -- 'flow_<nanoid>'
  slug          TEXT NOT NULL,             -- ^[a-z][a-z0-9-]*$ ... no underscores (D6)
  name          TEXT NOT NULL,
  description   TEXT NOT NULL DEFAULT '',
  version       TEXT NOT NULL DEFAULT '0.0.0',
  abi           BIGINT NOT NULL DEFAULT 1, -- guest ABI major version
  kind          TEXT NOT NULL DEFAULT 'wasm',  -- 'builtin' | 'wasm'
  module_sha256 TEXT,                      -- content address into artifact storage; NULL for builtin
  manifest      TEXT NOT NULL DEFAULT '{}',-- the full manifest JSON, verbatim
  enabled       BOOLEAN NOT NULL DEFAULT TRUE,
  created_by    TEXT NOT NULL,
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL
)                                          -- + org_id, RLS, UNIQUE (org_id, slug)

-- One row per execution, whatever triggered it.
flow_runs (
  id          TEXT PRIMARY KEY,            -- 'frun_<nanoid>'
  flow_id     TEXT NOT NULL,
  op          TEXT NOT NULL,
  trigger     TEXT NOT NULL DEFAULT 'manual', -- manual | mcp | api | schedule | wire
  status      TEXT NOT NULL DEFAULT 'ok',  -- ok | error
  input       TEXT NOT NULL DEFAULT 'null',
  output      TEXT,                        -- compact result summary, not a payload dump
  error       TEXT,
  started_at  TEXT NOT NULL,
  finished_at TEXT NOT NULL,
  created_by  TEXT NOT NULL,
  created_at  TEXT NOT NULL
)                                          -- + org_id, RLS
```

Both tables follow the hybrid convention: inline idempotent DDL in
`core/src/db.rs`, entries in `ORG_TABLES` (RLS is the tenancy enforcement, so
an omission is a hole), the slug uniqueness per-org via the same rebuild list
every other slug uses. No sqlx migration ... these are new tables, not
reshapes.

Two shapes were considered and rejected:

* **Ops as rows** (the `entity_types` + `entity_fields` split). Entity fields
  are edited one at a time by users; flow operations are authored by the
  module and versioned with it. The manifest is one document with one author
  ... splitting it into rows adds a sync problem and no capability.
* **Flows as custom entities.** The entity registry validates user-shaped
  data. A flow carries executable surface, an ABI version, and a
  content-addressed module ... it is infrastructure, not a household record,
  and putting an "execute" seam on the entities table would hand every
  entity-CRUD caller a code-adjacent object.

The manifest (what `flow_manifest()` returns, stored verbatim):

```json
{
  "slug": "wire",
  "name": "The Wire",
  "description": "External signal ingestion: feeds and page monitors into wire events.",
  "version": "0.1.0",
  "abi": 1,
  "operations": [
    {"name": "recent", "title": "Recent wire events",
     "description": "...", "input_schema": {"type": "object", "properties": {"limit": {"type": "integer"}}}},
    {"name": "emit", "title": "Emit a wire event", "description": "...", "input_schema": {}},
    {"name": "poll", "title": "Poll sources now", "admin": true, "input_schema": {}}
  ],
  "triggers": [
    {"kind": "schedule", "config": {"note": "per-source interval_secs"}}
  ],
  "allowed_hosts": []
}
```

`operations[].admin: true` marks an op that requires `ctx.is_admin()`, the
same admin rule every other surface uses. `triggers` is declarative data
until F4 gives it a runtime.

### D4 ... wire is flow #1, as a builtin, and builtin is a tier rather than a hack

The seeded `wire` flow is `kind: 'builtin'`: its manifest sits in the
registry exactly like a wasm flow's, its operations surface over MCP exactly
the same way, and dispatch routes its ops to native store functions instead
of a wasm instance:

| op | maps to |
|---|---|
| `recent` | `Store::wire_log` (the `/api/wire` read) |
| `emit` | `Store::emit` with the kind namespaced (below) |
| `poll` | `Store::poll_sources` (the existing feed/scrape ingestion), admin-gated like `POST /api/sources/poll` |

Builtin earns its place twice. It proves the registry, the dynamic MCP
section, and dispatch end to end before a single byte of wasm exists ...
which is what makes F1 shippable green. And it stays useful afterward: some
flows will always belong in-process (anything that needs the SSE bus or a
transaction), and a tier that describes native capability in the same
registry keeps ONE list of "what can this hive do" rather than two.

**Wire-kind namespacing, host-reserved lifecycle kinds.** The wire table's
lifecycle kinds (`journal.created`, `task.updated`, `feed.item`, ...) drive
SSE consumers and inbox fan-out. A flow must not be able to forge them. Any
event a flow emits through `wire_emit` gets its kind prefixed
`flow.<slug>.<kind>` by the host, so flow signal is namespaced, filterable,
and cannot impersonate system events. (The builtin wire flow's `emit` op
follows the same rule it will impose on its wasm successors:
`flow.wire.<kind>`.)

The `sources` table, its CRUD, and the poll driver do not move in this
program. F7 decides whether feed-fetching itself migrates into a wasm module
(making `wire` a real dogfood flow) or stays native forever; the registry
shape is identical either way, which is the point of the tier.

### D5 ... source attribution rides `people.kind`, not a new journal column

The journal already attributes: `author` is NOT NULL and joins `people`, and
`people.kind` already says `human` or `ai`. The design:

* `people.kind` gains `automation`. `ActorKind` grows the variant; the TS
  mirror grows the union member.
* Registering a flow ensures a `people` row: slug = the flow's slug, kind =
  `automation`, owner = the registering admin. A flow-emitted journal entry
  is authored BY the flow (`author = <flow-slug>`), and "user / ai /
  automation" is a join away for every existing row ... **no journal schema
  change, no backfill, no wire-shape break.** Old rows derive their
  attribution from the author they already carry.
* Per-run provenance is a `links` row: `flow_run → journal_entry`, rel
  `emitted_by`, written by the host in the same transaction as the entry.
  Task 116's `link_types` registry seeds `emitted_by` when it lands; until
  then the free-form rel column carries it, as it carries everything else.

Rejected: a `journal.source` column (`'user' | 'ai' | 'flow'`). It duplicates
what the author join already knows, needs a backfill story for every
existing row, and adds a second place attribution can disagree with itself.
Also rejected: authoring flow entries as the owning human or as a house AI
... that is precisely the misattribution Nate's item 3 exists to prevent.

One consequence to accept deliberately: flow actors join `people_list` (they
are writers, so they should), but they are not enrollable in the fixed
`ACTORS` mention/inbox list and never hold credentials. `actor_delete`
cascades work unchanged when a flow is removed.

### D6 ... dynamic MCP exposure: tools/list becomes static ++ registry, and MCP becomes the front door

With D9 retiring the hosted-agent surface, `POST /mcp` is no longer one
integration among several ... it is THE way intelligence reaches this
application. Every agent is an external client (Claude Code, Claude Desktop,
Codex, a ChatGPT desktop plugin) or a wasm flow, and both meet hive through
the same registry: clients see flows as tools, flows see hive as host
functions. That is what "MCP tools dynamically expose themselves" buys ...
installing a flow extends every connected AI product at once, with no
per-client work.

`tools/list` serves the static parity array (unchanged, byte for byte)
followed by one tool per operation of every ENABLED flow, named
**`flow_<slug>_<op>`**:

* Flow slugs allow no underscores (`^[a-z][a-z0-9-]*$`), op names allow no
  hyphens (`^[a-z][a-z0-9_]*$`), so the name parses unambiguously: strip
  `flow_`, the first `_` ends the slug. `flow_watch-the-wire_mark_read` is
  slug `watch-the-wire`, op `mark_read`.
* The tool's title/description/inputSchema come straight from the manifest;
  the schema is served as declared (it is the client contract, D2).
* `tools/call` routes any `flow_`-prefixed name to registry dispatch:
  resolve flow, resolve op, apply the op's admin gate, execute (builtin now;
  wasm behind `flow-exec` until F3, answering with a clean "registered but
  execution is not built yet" error rather than a stub result). Every
  executed op records a `flow_runs` row.
* An unknown, disabled, or unregistered flow tool answers exactly like any
  unknown tool ("MCP error -32602: Tool X not found") ... a disabled flow's
  tools are absent from tools/list, so callers were told.
* Registry changes emit `flow.*` wire events, so the SPA updates live over
  SSE. MCP is a stateless HTTP transport here, so there is no push channel
  for `notifications/tools/list_changed`; `initialize` already advertises
  `listChanged: true`, and clients that re-list per session (Claude Code
  does) pick up new tools on their next connection. That is the honest
  capability of a stateless transport and it is enough.
* If the registry read fails, tools/list serves the static array and logs
  the failure ... a DB blip must not blind every MCP client to the parity
  toolset.

A static `flows_list` tool joins the Rust-branch additions so an agent can
inspect the registry (what is installed, enabled, which ops) without parsing
tool names.

### D7 ... skills are already stored; the work is the editor

`identity_artifacts` already stores skills, agents, and slash-commands per AI
identity, content included, enabled-flagged, with REST
(`/api/actors/{actor}/artifacts`, upsert/delete) and MCP
(`identity_artifacts_list` / `_get`) surfaces, and the Claude Code plugin
already syncs the enabled set. Item 5 therefore costs a UI, not a schema: an
editor pane (F6) listing artifacts by actor with in-place markdown editing,
the enable toggle, and create/delete ... gated by the same
`can_act_for_identity` rule the routes already enforce. Nothing else moves.

### D8 ... triggers are declared now, run in F4

The manifest declares them (`schedule` with an interval, `wire` with a kind
prefix filter, `journal_tag` with a tag, `manual`); F1 stores and displays
them; F4 makes them fire:

* A boot-spawned scheduler task in `main.rs` (the `spawn_artifact_sweeper`
  shape): tick, iterate `orgs_all()`, and enter each org exactly the way
  `backfill_identity_cards_all` does ...
  `acting::scope(ActingScope::new(org, "system", true), ...)` ... so every
  triggered run holds a real acting scope and RLS applies. Detached tasks
  losing scope is the deny-all invariant; the scheduler opens scopes
  deliberately, per org, per tick.
* Wire triggers subscribe to the in-process bus (`ScopedEvent` already
  carries the org); a matching event schedules a run inside that org's
  scope. Journal triggers are wire triggers on `journal.created`.
* Trigger runs record `flow_runs` rows with `trigger: 'schedule' | 'wire'`,
  which is what makes the SPA visualization honest about what ran and why.

### D9 ... hive stops hosting agents and chat: the substrate decision

Hive becomes the data-and-flows substrate. It holds the journal, the wire,
the entities, the skills, and the flow registry; it executes flows; it does
NOT run agent runtimes and it does not host a chat. All agent execution is
either an external client connecting over MCP or a wasm flow. The chat we
are leaving behind is the hosted-Conversations surface ... talking to an
agent happens in that agent's own product, with hive as its memory and its
tools.

**What retires, concretely:**

* `api/src/routes/workspaces.rs` ... workspace spawn/list/input, the external
  runner's transcript ingest, `runtime-auth` (handing decrypted credentials
  to the runner), the `runtime-oauth` handshake, and the `cc-credentials`
  CRUD.
* `api/src/routes/conversations.rs` ... the capture/reflection REST.
* Store modules `workspaces`, `conversations`, `cc_credentials`; tables
  `cc_sessions`, `cc_messages`, `cc_credentials`, `runtime_oauth_states`.
  (`workerstatus` is NOT on this list: it is the feed worker's heartbeat ...
  wire infrastructure the `wire` flow keeps, not agent hosting.)
* SPA: the Conversations tab (`Workspaces.tsx`, the sidebar's lead slot),
  the Settings "Agent runtime sign-in" + credentials panel, the palette's
  Conversations entry, and Dashboard's agent-feed framing.

**The slice disables; F8 removes.** Disabling is a config gate, not a
deletion: `HIVE_AGENTS_ENABLED`, default OFF, in the exact shape of the mail
gate ... every workspaces/conversations route answers the standard 404, the
SPA reads `agentsEnabled` from `/api/auth/config` and drops the
Conversations slot and the Settings runtime section, and the data stays
intact. The captured history in `cc_sessions`/`cc_messages` remains readable
over the MCP `workspace_*` / `conversation_*` tools (read paths and the
external SessionEnd capture stay useful to external clients ... that is the
mail precedent: the REST surface gates, MCP reads don't lie about stored
data). Full code removal, the SPA excision, and the data-disposition call
are F8, done deliberately rather than as a drive-by.

**Open question for Nate (flagged, not decided): the credential vault.**
`cc_credentials` stored hive's OUTBOUND credentials ... the tokens hive used
to drive Claude Code / Codex as a client ... and was scrypt-hardened
literally today (#144). With runtimes external, hive no longer needs to hold
anyone's runtime token, and the vault has no remaining consumer. Two
dispositions: retire it with the runner in F8, or repurpose the (good,
freshly hardened) AES-256-GCM + scrypt machinery as the SECRETS store for
flows ... an API key a wasm flow needs for `http_get` has exactly the same
shape (encrypted at rest, never in a tool result, decrypted only at use).
Do not confuse either with INBOUND client auth: external clients
authenticate to hive through the OAuth AS and bearer tokens
(`api_tokens`/`sessions`), which is untouched by this retirement and is the
multi-client front door below.

## One hive, many clients

The practical shape of D6+D9: several AI products, one hive, one registry.
What each client needs is an URL and an OAuth dance ... hive already ships
the whole server side: `POST /mcp` (stateless, JSON responses), RFC 9728
protected-resource metadata on the 401, an OAuth 2.1 authorization server
with dynamic client registration (RFC 7591) and PKCE, and bearer API tokens
for headless clients.

* **Claude Code**: `claude mcp add --transport http hive https://<your-hive>/mcp`
  ... it hits the 401, discovers the AS from the `www-authenticate` metadata,
  registers, and walks the browser consent. Headless (CI, hooks): mint an
  API token and set the Authorization header instead.
* **Claude Desktop**: a custom connector pointed at `https://<your-hive>/mcp`;
  same discovery, same consent screen.
* **Codex**: an `mcp_servers` entry in `config.toml` with the streamable-HTTP
  URL; where a build lacks the OAuth walk, a bearer token in the headers
  config carries it.
* **ChatGPT desktop / plugins**: a custom connector by URL, OAuth where the
  client supports it, bearer token where it doesn't.

Every one of them sees the same tools/list: the parity toolset plus
`flow_<slug>_<op>` for whatever flows THIS org has enabled. Install a flow
and every connected product grows the same new tools on its next list;
disable it and they all lose them. Per-client dynamics are only about WHEN
the client re-lists: the SPA hears `flow.*` wire events over SSE
immediately, while MCP clients (stateless transport, no push) pick the
change up at their next tools/list ... in practice, the next session.
The consent screen is also where multi-user shows up: each person
authorizes their own client, each credential pins one org, and RLS does the
rest ... nothing about multi-client is client-side configuration in hive.

## The SPA surface

F1 ships `/flows`: the registry as a list ... name, slug, version, tier
badge, enabled toggle (admin), operations as chips, trigger summary. It is a
registered tab reachable through the command palette, like `wire` and the
other boards. F1 also swings the D9 gate through the shell: the
Conversations sidebar slot and the Settings runtime section render only when
`agentsEnabled` comes back true, exactly as the mail surface already gates.

F5 is the visualization Nate asked for: a flow detail page rendering the
declared shape (triggers → ops → what they emit) as a live diagram, run
history with status and duration from `flow_runs`, and the flow's recent
emissions (its `flow.<slug>.*` wire events and `emitted_by`-linked journal
entries) streaming over the SSE the store already fans out. Dynamic means:
rendered from the manifest + runs + wire, never hand-drawn per flow.

## Security posture, collected

* The guest holds no credential and no socket; every capability is a host
  function executing under the run's acting scope, so RLS bounds a flow
  exactly as it bounds a request (D2).
* Module bytes are content-addressed; the registry pins the sha256; install
  and enable/disable are admin-gated writes. Installing a flow is trusting
  its author with the v1 host surface inside your org ... say that in the
  install UI rather than pretending sandboxing makes code neutral.
* Flow-emitted wire kinds are namespaced; lifecycle kinds cannot be forged
  (D4). Flow-authored journal entries are attributed as automation and
  cannot impersonate a human or an AI author (D5; MCP authorship pinning
  already prevents the reverse).
* Deadlines, fuel, memory and output caps on every run (D2) ... a flow can
  fail its run, not the process.
* Everything a flow returns through MCP is untrusted content to the calling
  agent, the same rule `api/src/mcp.rs` already carries for stored data.

## Workstream breakdown

| item | owns | depends on |
|---|---|---|
| F1: registry + builtin wire + dynamic tools/list + REST + `/flows` list page + the D9 gate (`HIVE_AGENTS_ENABLED`, default off) | `core/src/db.rs`, `core/src/store/flows.rs`, `api/src/mcp.rs`, `api/src/routes/{flows,mod,workspaces}.rs`, `packages/web` | nothing ... **this PR** |
| F2: attribution: `ActorKind::Automation`, flow people rows, `emitted_by` links, author badges in the SPA | `shared`, `core/src/store/{flows,people,journal}.rs`, `packages/*` | F1 |
| F3: the wasm host: wasmtime behind `flow-exec`, ABI v1, install path (module upload → artifact storage → register), guest template repo | `api` (executor module), `core/src/store/flows.rs`, `docs/FLOWS.md` ABI section as contract | F1 |
| F4: trigger runtime: scheduler + wire subscriptions, per-org scopes, runs from triggers | `api/src/main.rs`, `core/src/store/flows.rs` | F3 (schedule/wire runs execute wasm; builtin-only triggers could start after F1 if a need appears first) |
| F5: flow visualization: detail page, live run history, manifest-driven diagram | `packages/web` | F1 |
| F6: skills editor: identity_artifacts pane in Settings | `packages/web` | nothing |
| F7: dogfood decision: port feed/scrape polling into a wasm `wire` module, or keep it builtin and write the reference flow fresh | tbd after F3 | F3, F4 |
| F8: the retirement: delete the gated agent/chat surface (routes, store modules, SPA components), decide the cc_credentials vault (retire vs flow-secrets), data disposition for cc_sessions history | `api/src/routes/{workspaces,conversations}.rs`, `core/src/store/{workspaces,conversations,cc_credentials}.rs`, `packages/web`, `docs/` | F1 (the gate proves nothing depends on the surface); the vault call is Nate's |

F2, F3, F6, and F8 are mutually independent and can run in parallel under
the multi-agent pattern once F1 merges. F5 needs only F1 but reads better
after F4 exists to populate runs.

## What F1 (this PR) actually contains

The registry tables and store module; the seeded builtin `wire` flow with
`recent` / `emit` / `poll` mapped to the existing store functions; tools/list
serving the registry-driven section and tools/call dispatching it (builtin
ops execute for real; a wasm-kind flow answers with the clean
not-yet-executable error; the `flow-exec` feature is declared as the mount
point); `flow_runs` recorded for executed ops; REST (`GET /api/flows`,
`GET /api/flows/{slug}`, `GET /api/flows/{slug}/runs`, admin
`PATCH /api/flows/{slug}`); the static `flows_list` MCP tool; the minimal
`/flows` SPA page; and the D9 gate ... `HIVE_AGENTS_ENABLED` default off,
every workspaces/conversations/cc-credentials route answering 404,
`agentsEnabled` in `/api/auth/config`, and the SPA hiding the Conversations
slot and the Settings runtime section. Attribution (F2), execution (F3), and
the agent-code removal (F8) are designed here and deliberately not in the
slice.

## Explicitly not this program

* **A workflow engine.** No DAGs, no step orchestration, no retry graphs, no
  Temporal-shaped anything. A flow is one module with declared operations;
  composition happens through the journal and the wire, which is the
  journal-first model doing its job.
* **A marketplace / untrusted third-party installs.** Install is admin-only
  and the trust statement is explicit (above). Revisit per-flow capability
  grants when a flow written by a stranger actually exists.
* **Cross-org flows.** A run holds ONE acting scope. Summarizing across orgs
  is the capability WEB-APP.md deliberately killed; flows do not resurrect
  it.
* **The component-model ABI.** v2, when authoring components is a toolchain
  default rather than a bindings project per language.
* **Replacing `sources`/`outbox` infrastructure with flows on day one.** The
  wire flow WRAPS the existing ingestion; migrating its internals is F7's
  question, answered after the host exists.
* **Flow-to-flow calls.** A flow that wants another flow's output reads what
  it emitted (wire, journal). Direct invocation is an orchestrator in
  disguise.
* **A new chat surface.** D9 is a direction, not a gap: talking to an agent
  happens in that agent's own product. If hosted conversations ever come
  back, they come back as a decision reversal with its own note, not as
  feature creep on the flows program.
