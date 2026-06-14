# Hive Rust Rewrite

A drop-in Rust replacement for the Node hive API + worker: one Axum binary
serves the full REST surface, the MCP server, the SSE stream, AND the Solid.js
SPA (no nginx container). A second binary runs the worker. Both speak the exact
wire format, auth formats, and SQLite schema the Node API uses — an existing
database, its scrypt password hashes, its `hive_pat_*` tokens, and its
timestamps all keep working unchanged.

## Architecture

| Component | Tech | Port | Notes |
|-----------|------|------|-------|
| API + Web | Rust (Axum) | 7878 | REST + OAuth 2.1 AS + MCP + SSE + SPA static serving |
| Worker | Rust | — | Feed polling, outbox drain, embeddings backfill, maintenance |
| DB | SQLite (WAL, FTS5) | — | Shared via volume; schema identical to the Node API |
| Embeddings | ort (ONNX Runtime) | — | bge-small + bge-reranker via `hive-embed`; hash fallback offline |

## Quick Start

```bash
# Local dev (API on :7878, serves packages/web/dist if built)
pnpm --filter @hive/web build
cargo run -p hive-api

# Worker (HIVE_WORKER_TICK seconds per cycle, default 30; `--once` for one cycle)
cargo run -p hive-worker

# Containers (single image, both binaries, no nginx)
docker compose -f docker/docker-compose.rust.yml up --build
```

Key env: `HIVE_DB` (default `data/hive.db`), `PORT` (7878), `HIVE_WEB_DIST`
(SPA dist dir), `HIVE_EMBED` (`transformers`|`hash`), `HIVE_EMBED_MODEL`,
`HIVE_RERANK_MODEL`, `HIVE_MODEL_CACHE` (default `/data/models`),
`HIVE_PUBLIC_URL`, `OIDC_ISSUER`/`OIDC_CLIENT_ID`/`OIDC_CLIENT_SECRET`/
`OIDC_ALLOWED_DOMAINS`.

## Cargo Workspace

```
.
├── Cargo.toml            # Workspace root (toolchain pinned in rust-toolchain.toml)
├── shared/               # hive-shared: domain types, exact @hive/shared parity
├── embed/                # hive-embed: embedder seam — ort BGE engine (feature
│                         #   "onnx", default on) + hash-ngram-v1 fallback latch
├── api/                  # hive-api: the server
│   └── src/
│       ├── main.rs       # boot: open DB, migrate, backfill cards, serve
│       ├── db.rs         # schema parity with packages/api/src/db.ts
│       ├── auth.rs       # scrypt, sha256 token hashes, PKCE, cookie consts
│       ├── middleware.rs # CORS + principal resolution + onboarding/auth gate
│       ├── store/        # one module per resource (impl Store blocks)
│       ├── routes/       # one router per area, merged in routes/mod.rs
│       ├── mcp.rs        # MCP tools (full Node toolset + identity_* extras)
│       └── legacy_import.rs  # legacy hive.db reader (read-only)
└── worker/               # hive-worker: tick loop reusing the api store
```

## Parity notes

- **Auth/data compat is bit-level**: scrypt `scrypt$salt$hash` (N=16384,r=8,p=1),
  sha256-hex token storage, `hive_sess_/hive_pat_/hive_ac_` prefixes, JS
  `toISOString()` timestamps (lexicographic sort-compatible), `prefix_nanoid(12)` ids.
- **Search**: FTS5 (porter unicode61) keyword + semantic standard|precision
  cascade (hybrid blend, Markov-blanket boost, cross-encoder rerank) with the
  #46/#47 degrade paths; embeddings stored as LE-f32 blobs with FNV-1a content
  hashes — interchangeable with rows the Node worker wrote.
- **OAuth 2.1**: discovery, dynamic client registration, consent flow with CSRF
  double-submit, PKCE S256, single-use 60s codes with replay revocation.
- **SSE** at `/api/stream` (25s heartbeat), `data:` frames in bus.ts shape.
- **Identity mapping** (Rust-branch addition): `identities` table + REST + MCP
  tools mapping Discord/Telegram/Slack user ids → actor slugs.

## CI

The `rust` job gates `cargo fmt --check`, `clippy -D warnings`, build, and
tests (hash embedder, offline). Toolchain pinned to match `rust-toolchain.toml`.
