# hive ... rust rewrite design

Rip-and-replace of the python toolchain at `~/.hive/` with a rust workspace.
Replaces `hive.py` (CLI), `hive_search.py` + `hive_semantic.py` + `hive_embed.py`
(hybrid search + embedder), and the `~/.claude/workspace/github/hive-ui` python
service.

Drives hive task #53. Schema is frozen against the post-task-8 `~/.hive/hive.db`
(projects has INTEGER PK id + UNIQUE name). No re-migration.

## Goals

1. **Single static binary** `hive` with full CLI parity to `python hive.py X Y Z`.
   Drop-in replacement for every caller across Pia/Apis/Cera configs (CLAUDE.md,
   agent-briefing.md, /tasks /journal /notes skill scripts, agent prompts).
2. **HTTP API** (`hive-api`) for browser/UI consumption, mirroring CLI grammar
   1:1 in REST routes.
3. **Rust UI** (`hive-ui`) replacing the existing svelte+python service.
4. **Multi-arch ghcr.io images** for amd64 + arm64 (BR runs both). This is the
   reference pattern for future BR rust services (Scoutarr / Nectar inherit
   later).
5. **Schema unchanged.** Same tables, indexes, FTS5 triggers. Documented in
   `crates/hive-db/SCHEMA.md`. The python `hive.py init` was an idempotent
   `CREATE TABLE IF NOT EXISTS ...` script; the rust `hive init` does the same
   exact statements.

## Non-goals

- No schema changes. `links.target_id ↔ projects.id` works today (task 8); we
  inherit that and don't touch it.
- No libsql, no wasm32-wasip2 (ironclaw concern, task 52).
- No CI auto-merge ... Nate gates merges.
- No SDK or library crate for external consumers in v1. The CLI and API are the
  contracts.

## Crate layout

```
hive/
├── Cargo.toml                # workspace root
├── DESIGN.md                 # this file
├── README.md
├── crates/
│   ├── hive-db/              # schema, types, sqlite/sqlx access layer
│   │   ├── src/lib.rs
│   │   ├── src/schema.rs     # CREATE TABLE statements (matches python init)
│   │   ├── src/types.rs      # Project, Task, JournalEntry, Note, WireEvent, Link
│   │   ├── src/queries/      # one module per domain
│   │   └── SCHEMA.md         # human-readable schema doc
│   ├── hive-cli/             # `hive` binary, clap-derived
│   │   ├── src/main.rs
│   │   ├── src/cmd/          # one file per top-level subcommand
│   │   └── src/format.rs     # column-aligned table printing + JSON output
│   ├── hive-api/             # `hive-api` axum binary
│   │   ├── src/main.rs
│   │   ├── src/routes/       # /tasks, /journal, /notes, /wire, /links, /graph, /search
│   │   └── src/error.rs      # axum IntoResponse mapping for hive-db errors
│   ├── hive-ui/              # `hive-ui` leptos SSR + WASM hydration
│   │   ├── src/main.rs       # SSR axum server
│   │   ├── src/app.rs        # leptos router
│   │   ├── src/pages/        # graph, journal, tasks, notes, wire
│   │   └── style/            # CSS
│   └── hive-embed/           # embedder client crate (see "Embedder" below)
│       └── src/lib.rs
├── docker/
│   ├── Dockerfile.hive-api
│   ├── Dockerfile.hive-ui
│   └── docker-compose.yml    # local dev stack
└── .github/workflows/
    ├── ci.yml                # cargo test + clippy + fmt
    └── publish.yml           # multi-arch ghcr push (tag + manual)
```

Workspace edition `2024`, MSRV pinned to whatever rust 1.93+ scoutarr targets.

## Branching

Mirrors BR canon ([branching strategy](https://kb.beesroadhouse.com/books/developer-operations-devops/page/branching-strategy)):

- `development` ... default branch, active work lands here.
- `release` ... stable branch for image promotion.
- `feature/{slug}`, `bug/{slug}`, `improvement/{slug}`, `refactor/{slug}` from
  `development`.
- No `main` / `master`. The repo was created on `master` by `gh repo create`;
  initial commit lands on `development` and `master` is deleted before push.

## DB access: rusqlite

**Choice: `rusqlite` (bundled, with `serde_json`, `chrono`, `bundled-full` for
fts5).** Defended over `sqlx`:

- Hive lives in a single sqlite file on the local filesystem ... no remote DB,
  no migrations system needed beyond the idempotent CREATE statements.
- `rusqlite` ships an embedded sqlite with FTS5 enabled via `bundled-full`. No
  system sqlite dependency on the runtime image.
- The CLI is fundamentally synchronous (it's a `clap` command that prints a
  table and exits). Forcing async via sqlx adds a tokio runtime to a process
  that doesn't otherwise need one.
- `hive-api` is async (axum) and will use `tokio::task::spawn_blocking` to call
  into rusqlite. This is fine for a low-QPS personal-tooling API ... we are not
  building a high-concurrency service.
- sqlx requires `prepare`/macros tied to a live DB or offline metadata, which
  is brittle for a tool whose schema lives inside its own binary.
- Scoutarr uses sqlx because it talks to **postgres**. Hive talks to **sqlite**.
  Different shape, different choice.

Connection management: `r2d2` + `r2d2_sqlite` pool in `hive-db`. CLI takes one
connection from a 1-size pool; API uses a small (e.g. 4) pool with WAL.

PRAGMA on every connection: `foreign_keys = ON`, `journal_mode = WAL`,
`synchronous = NORMAL`, `busy_timeout = 5000`.

## CLI parity strategy

Authoritative grammar lives in the python `hive.py`'s `argparse` setup
(`build_parser`, lines 1220-1446). Port verbatim using `clap` with `derive`.

Parity rules:

- **Same flag names.** `--ai`, `--owner`, `--from` (mapped to `from_date`),
  `--to`, `--tag`, `--limit`, `--json`, `--external-id`, `--unacknowledged`,
  `--include-meta`, `--no-rerank`, `--hybrid`, etc.
- **Same positional shape.** `tasks show <id>`, `journal show <id>`,
  `links show <ref>`, `wire ack <id>` ... all positional ints/strings stay.
- **Same output format.** Column-aligned table widths computed from row data
  (matching python's `max(len(...)` pattern), `--json` mode emits the same
  shape as `_print_json` does today (a list of row dicts, indent=2).
- **Same exit codes.** `1` for general error, `2` for "db not found, run init".
- **Same stderr/stdout split.** Errors to stderr with `error: <msg>` prefix.
- **Same validation messages.** Unknown owner / ai / severity / status returns
  the same `invalid X 'foo'. valid: ...` shape.

Output formatting lives in `crates/hive-cli/src/format.rs` so each subcommand
just produces a typed row vec and hands it off.

A snapshot test suite under `crates/hive-cli/tests/` runs the python and the
rust CLI against a temp DB seeded with fixtures and diffs the output. This is
the parity gate before cutover.

## API design

`hive-api` is a small axum service. Routes mirror CLI verbs:

```
GET    /tasks                       ?project=&owner=&status=&all=
POST   /tasks                       body: {project, title, body, owner, ...}
GET    /tasks/{id}
PATCH  /tasks/{id}                  partial update
POST   /tasks/{id}/done
POST   /tasks/{id}/block            body: {reason}

GET    /projects                    ?status=
POST   /projects
POST   /projects/{name}/archive

GET    /journal                     ?ai=&from=&to=&tag=&limit=
POST   /journal
GET    /journal/{id}
GET    /journal/search              ?q=&limit=&hybrid=&ai=

GET    /notes                       ?author=&project=&tag=&limit=
POST   /notes
GET    /notes/{id}
GET    /notes/search                ?q=&limit=&hybrid=&author=&project=

GET    /wire                        ?source=&severity=&unacknowledged=&limit=
POST   /wire
POST   /wire/{id}/ack

GET    /links                       ?source=&target=&type=
POST   /links
DELETE /links/{id}
GET    /links/types

GET    /graph                       ?min=&tags=&nodes=&include_meta=
GET    /search                      ?q=&limit=&hybrid=

GET    /healthz
```

All responses JSON. Errors use a uniform `{error: string, code: string}` body
with appropriate HTTP status.

Auth: **none in v1**. The service binds to `127.0.0.1:` by default and lives
inside the BR LAN. If exposed later, that's a Cera-side reverse-proxy concern.

## UI: axum + maud HTML SSR (v1) ... leptos+WASM deferred

**Scope decision: v1 ships pure HTML SSR via axum + maud, NOT full leptos
with WASM hydration.** Original plan was leptos SSR + WASM hydration; the
hive-ui PR pivoted before merge for these reasons:

- The user-visible result for the lists/forms/graph-tabular pages is the
  same. SSR HTML renders fine for browse-and-filter workflows.
- maud is `view!`-style HTML in rust without the cargo-leptos / hydration
  feature-flag dance. One binary, debian:bookworm-slim runtime, no asset
  pipeline. Ships in hours, not days.
- The genuinely-WASM-warranted pieces (interactive d3-style force layout
  on the graph page) are tracked as a follow-up. The graph page in v1
  renders the same `graph` payload as a tag-grouped tabular view plus a
  collapsible raw JSON pane, which covers exploration without the
  hydration overhead.
- Deferring leptos doesn't lock us out: a follow-up PR can replace
  individual pages with leptos islands without touching `hive-db` or
  `hive-api`.

The SSR server reads `hive-db` directly (no API hop) ... it is its own
axum binary that imports the `hive-db` crate and shares the same r2d2
pool model used by `hive-api`.

Build: regular cargo, no cargo-leptos. Single static binary, runtime
image identical in shape to `hive-api`.

If/when interactive d3 lands, the path is either (a) leptos islands
mounted into the maud-rendered page, or (b) a tiny vanilla-JS d3 setup
fed by inlined JSON. Both are reversible; v1 doesn't lock the choice.

## Embedder: open question, flag for Cera

**The brief assumed `hive_embed.py` posts to a remote embedder. It does not.**
Reading `~/.hive/hive_semantic.py:139-158`:

- Embedding model: `BAAI/bge-small-en-v1.5` (sentence-transformers, 384-dim),
  loaded **in-process** via `SentenceTransformer(EMBED_MODEL)`.
- Reranker: `cross-encoder/ms-marco-MiniLM-L-6-v2`, also **in-process** via
  `sentence_transformers.CrossEncoder`.

There is no remote embedder URL, no auth path. The python venv ships these
models locally and runs them on CPU.

This means the rust port has three real options. All have tradeoffs; we need
Cera's call before locking it in.

### Option A: native rust embedder via `fastembed-rs` (recommended)

- `fastembed-rs` supports BGE-small-en-v1.5 directly with ONNX runtime.
- Cross-encoder reranking: `fastembed-rs` added reranker support recently
  (BGE reranker), but `ms-marco-MiniLM-L-6-v2` specifically is not in its
  default model list. We'd either substitute a supported reranker (e.g.
  `BAAI/bge-reranker-base`) and accept the score change, or load the existing
  ONNX export of `ms-marco-MiniLM-L-6-v2` directly via `ort` (the ONNX runtime
  rust crate).
- Pure rust, single binary, no python dependency at runtime.
- ONNX runtime adds ~20MB to the image and needs a model cache directory.
- **Risk**: changing the reranker model changes search rankings. Existing
  embeddings (in the `embeddings` table) stay valid (same 384-dim BGE), but
  rerank scores for hybrid search will drift.

### Option B: keep python sidecar for embedding/rerank

- `hive-embed` crate is a thin HTTP/IPC client that calls a small python
  service running `sentence_transformers`.
- Zero ranking drift; identical search behavior.
- Adds python back into the deployment ... defeats the "single binary" goal
  for the embedder path. The CLI and API stay rust; only hybrid search needs
  the sidecar.
- The sidecar can be shipped as a sibling container in the docker-compose
  stack, only spun up when hybrid search is invoked.

### Option C: drop hybrid search; keep FTS5 only

- The basic `journal search`, `notes search`, `search` (combined) commands
  use FTS5 directly and need no embeddings.
- `--hybrid` flag becomes a no-op or returns an error explaining hybrid was
  removed.
- Simplest port. Loses search quality on semantic queries.

**Recommendation: Option A with `BAAI/bge-reranker-base` as the reranker
substitute.** Document the ranking-drift caveat in the cutover journal entry
and let semantic-search consumers re-validate. If Cera or Nate hate the new
ranks, fall back to Option B in a follow-up PR.

If Cera disagrees, open task #54 with the chosen direction.

## ghcr publish pipeline

Two images per release:

- `ghcr.io/bees-roadhouse/hive-api:{tag}` ... axum API
- `ghcr.io/bees-roadhouse/hive-ui:{tag}` ... leptos SSR

Both built `linux/amd64` and `linux/arm64`. The `hive` CLI binary ships as a
GitHub release artifact (raw binary), not as an image ... installers grab it
from the release assets.

### Cross-compile choice: `cargo-zigbuild`

Defended over `cross`:

- `cross` uses qemu emulation, which is brutally slow for arm64 builds (we'd
  watch CI compile rust for 20+ minutes).
- `cargo-zigbuild` uses zig as a cross-linker. Native-speed compilation, real
  arm64 binaries. The build is `cargo zigbuild --release --target
  aarch64-unknown-linux-gnu` and we link with `--zig`.
- ONNX runtime (if Option A wins) ships prebuilt arm64 + amd64 libraries; no
  qemu needed.
- Scoutarr and Nectar don't have publish workflows yet. This becomes the
  reference; theirs adopt it.

### Workflow shape

```yaml
# .github/workflows/publish.yml
on:
  push:
    tags: ['v*']
  workflow_dispatch:
    inputs:
      tag: { description: 'image tag', required: true }

jobs:
  build:
    strategy:
      matrix:
        target: [x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu]
    steps:
      - checkout
      - install rust + zig + cargo-zigbuild
      - cargo zigbuild --release --target ${{ matrix.target }}
      - upload artifact

  image:
    needs: build
    strategy:
      matrix:
        bin: [hive-api, hive-ui]
    steps:
      - download artifacts (both arches)
      - docker buildx build --platform linux/amd64,linux/arm64 \
          --tag ghcr.io/bees-roadhouse/${{ matrix.bin }}:${{ tag }} \
          --push -f docker/Dockerfile.${{ matrix.bin }} .
```

The Dockerfiles are *runtime-only* (FROM debian:bookworm-slim, COPY the
prebuilt binary in). No rust toolchain in the runtime image.

## Migration plan (cutover, not in this PR)

The build phase does not touch `~/.hive/`. Cutover is a coordinated window
Cera orchestrates once parity tests pass. Sketch:

1. **Build phase** (this work): rust workspace done, parity tests green,
   `target/release/hive` exists, ghcr images published.
2. **Cutover prep**: Cera installs the `hive` binary to a directory on PATH
   (e.g. `~/.local/bin/hive`). Both python and rust can run side-by-side
   against the same `~/.hive/hive.db` ... the schema is identical and rusqlite
   plays nice with python's sqlite3 over WAL.
3. **Caller cutover**: PR across all configs (Pia, Apis, Cera, hive-ui-python
   removal) replacing `python ~/.hive/hive.py X` with `hive X`. Single bundled
   PR, single review, single merge.
4. **Archive phase**: `~/.hive/hive.py`, `hive_search.py`, `hive_embed.py`,
   `hive_semantic.py`, the `.venv`, the `requirements.txt`, and the python
   `hive-ui` repo all move to a `~/.hive/_archived-2026-XX-XX/` directory.
   `hive.db` itself stays in place.
5. **Stack cutover**: BR Podman stack swaps `hive-ui-python` container for
   `ghcr.io/bees-roadhouse/hive-ui:vX.Y.Z` and adds `hive-api` alongside.

The migration scripts (`migrate_2026_05_11.py`, `migrate_2026_05_15_projects_id.py`,
`backfill_links_2026_05_11.py`, `migrate_br755_full.py`) are one-shot scripts
that already ran. They stay in `~/.hive/_archived-...` as historical record;
they are not ported.

## Open questions for Cera

1. **Embedder strategy** ... A vs B vs C above. Recommended A. Block on this
   before `hive-embed` work starts.
2. **Default branch** ... using `development` per BR canon. Confirm or
   override.
3. **Cutover timing** ... when do we burn the bridge? Once parity tests are
   green, or do we want a soft-launch period running both?
4. **Image registry visibility** ... ghcr packages are public by default for
   public repos. Confirm BR is OK with public images, otherwise mark them
   private.

## Risk register

| risk | likelihood | impact | mitigation |
|---|---|---|---|
| FTS5 ranking output diverges from python's `snippet()` literal output | medium | low | snapshot tests catch it; rusqlite uses same upstream sqlite |
| Hybrid-search ranking drift (Option A) | high | medium | document in cutover journal; offer Option B fallback |
| Windows path handling in CLI (`~/.hive/hive.db` resolution) | medium | low | use `directories` crate, mirror python's `Path(__file__).parent / "hive.db"` semantics |
| Concurrent writes from CLI + API + python during cutover transition | low | medium | WAL + busy_timeout=5000; document "one writer at a time" during cutover |
| `rusqlite` static link on arm64 in ghcr build | low | medium | `bundled-full` feature builds sqlite from source; cargo-zigbuild handles this |
| ONNX runtime arm64 binary availability (if Option A) | low | medium | `ort` crate has arm64 prebuilt; verify in CI before merging hive-embed |

## Out of scope (deferred)

- Web auth / multi-user (BR LAN only).
- `hive` as a library crate for third parties.
- Schema migrations beyond what python had (use the existing one-shot scripts
  if a future schema change is needed; or build a proper migrations system in
  a follow-up).
- Replacing `hive_embed.py status` / `index` operations ... the rust embedder
  port covers this.
- Mobile UI (the existing svelte UI was desktop-only; leptos port matches).
