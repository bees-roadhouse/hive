# hive

A personal memory engine: journal-first, local-first, yours. You (and your
AIs) write prose; tasks, decisions, events, and a knowledge graph emerge from
it by anchoring spans of the text. Everything lives in an append-only,
encrypted store on your own machine — no server, no accounts, no telemetry —
with device-to-device sync and pluggable ingestion (mail, files, calendar,
browser) on the roadmap.

This is the P2P pivot of what used to be a hosted multi-user system. The
decision record is [docs/DIRECTION.md](./docs/DIRECTION.md) (D16–D28), the
PR-by-PR execution program is [docs/PLAN.md](./docs/PLAN.md), and the
security posture is [docs/THREAT-MODEL.md](./docs/THREAT-MODEL.md).

## Where it stands

Phase 1 (the engine) is complete; Phase 2 (the app) is underway:

- **Storage** — per-device append-only op log (CBOR records, encrypted
  segments, blake3 hash chain) as the source of truth; SQLCipher SQLite as
  a derived index (FTS5 + vector ANN) rebuilt by deterministic replay.
  Deleting `index.db` loses nothing — CI proves the rebuild byte-identical.
- **Blobs** — content-addressed encrypted blockstore (blake3 over
  ciphertext, FastCDC chunking) with **crypto-shred** hard delete: destroy
  the wrapped per-blob key and the bytes are unrecoverable, verified end to
  end in `core/tests/crypto_shred.rs`.
- **Keys** — master key in the OS keychain; passphrase export wrap
  (Argon2id) and printable recovery code primitives in `core/src/keys.rs`.
- **MCP** — the only external API (D25): 47 tools (journal, tasks, search,
  semantic search, recall, entities, mail archive) in `core/src/mcp.rs`,
  served over stdio by the `hive-bridge` binary.
- **App shell** — Dioxus desktop window with journal + search riding the
  engine, plus a flatpak manifest (`packaging/flatpak/`).
- **Importer** — `hive-import` (PR 1.7), the one-shot bridge out of the
  hosted era: replays an old instance's Postgres into a fresh data dir as
  op-log records, original ids and timestamps intact, attachment bytes
  crypto-sharded into the blockstore. The one Postgres client left anywhere
  in the workspace.

Honest gaps, per plan: **mail sync is paused** (the archive is readable;
the sync daemon returns as a WASM module in Phase 3), **device sync is
Phase 4** (single machine until then), and in the interim bridge mode
**the app and hive-bridge can't run at the same time** (single-writer lock;
the Phase 2.4 proxy lifts this).

## Workspace

| Crate | What it is |
| --- | --- |
| `core/` | hive-core — op log, blockstore, key custody, SQLCipher index + fold projector, the store (one writer thread), and the MCP tool layer |
| `app/` | Dioxus desktop shell (journal + search) |
| `bridge/` | `hive-bridge` — stdio MCP server over the local store; also `hive-bridge call` one-shot mode for hooks/scripts |
| `importer/` | `hive-import` — one-shot hosted-Postgres → data-dir migration (the only sqlx in the workspace) |
| `shared/` | domain types |
| `embed/` | embedding seam: ONNX/BGE local models + deterministic hash fallback |
| `jmap-sync/` | JMAP mail sync library (kept through the pause; returns in-module in Phase 3) |

## Build and run

Rust toolchain is pinned by `rust-toolchain.toml`.

```bash
# The desktop app (journal + search)
cargo run -p hive-app

# The MCP bridge (installs `hive-bridge` on PATH)
cargo install --path bridge

# Migrate a hosted-era instance (one-shot; --dry-run to preview the plan)
cargo run -p hive-import -- --from postgres://user:pass@host/hive --dry-run

# Flatpak (Bazzite/Fedora daily-driver path)
flatpak-builder --user --install build-dir packaging/flatpak/com.beesroadhouse.Hive.yml

# The full gate
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
HIVE_EMBED=hash cargo test --workspace
```

Tests are hermetic: tempdir data dirs, in-memory keys, hash embedder — no
database service, no network. The one exception: the importer's fixture
tests run only when `DATABASE_URL` points at a pgvector Postgres (they skip
loudly otherwise), mirroring the CI split.

## How Claude connects

Both integrations run through `hive-bridge` on `PATH` (D25):

- **Claude Code** — the [`hive-memory` plugin](./plugins/claude-code-hive-memory/)
  wires the bridge as a stdio MCP server and injects a session-start recall
  brief through a hook.
- **Claude Desktop** — the [`hive.mcpb` extension](./integrations/claude-desktop/)
  launches the same binary (build with `./scripts/build-mcpb.sh`).

One-shot tool calls for scripts and hooks:

```bash
hive-bridge call journal_append --json '{"body": "Note to self about [topic: Bridges]."}'
hive-bridge call recall --json '{"identity": "nate"}'
```

The acting identity defaults to `$USER` (`--actor` overrides), matching the
app. Remember the interim caveat: close the app while a bridge is connected.

## Data dir

`$XDG_DATA_HOME/hive` (fallback `~/.local/share/hive`; the flatpak app maps
this under `~/.var/app/com.beesroadhouse.Hive/data/hive`):

```text
device                      this installation's device id
lock                        single-writer advisory lock (one hive process at a time)
log/<device>/<seq>.seg      the op log: encrypted, append-only, the truth
blocks/<hh>/<blake3>        encrypted content-addressed blob blocks
index.db                    SQLCipher derived index — delete it and replay rebuilds it
```

Back up the whole directory (plus your OS keychain's `hive/master-key`
entry — without it a backup is noise, which is the point). See
[docs/THREAT-MODEL.md](./docs/THREAT-MODEL.md) for what that protects
against, exactly.

## Branching

- `main` is the only long-lived branch; it must stay releasable.
- Work branches: `feature/{slug}`, `bug/{slug}`, `improvement/{slug}`,
  `refactor/{slug}`, merging back via PR (CI: fmt, clippy, build, test).
- Releases are tag-driven and return with the Phase 2.5 packaging pipeline;
  Phase 1 lands untagged. The version of record is `[workspace.package]` in
  the root `Cargo.toml`.
