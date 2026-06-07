# Hive Rust Rewrite

A complete Rust rewrite of the Hive API (Axum) and Worker (ONNX Runtime) with cross-platform identity mapping for multi-user memory.

## Architecture

| Component | Tech | Port | Notes |
|-----------|------|------|-------|
| API | Rust (Axum) | 7878 | Full REST + SSE + MCP |
| Worker | Rust (ONNX Runtime) | — | Feed polling, embeddings |
| Web | Node/TS (kept for now) | 8091 | Will be re-implemented later |
| DB | SQLite (WAL) | — | Shared via volume |

## Quick Start

```bash
# Build the dev image (Rust + .NET + Zig + 1Password CLI + Node)
docker build -f Dockerfile.dev -t beesroadhouse/hive-dev:latest .

# Run everything
docker compose -f docker-compose.hybrid.yml up --build
```

## Cargo Workspace

```
.
├── Cargo.toml          # Workspace root
├── shared/             # Domain types (hive-shared crate)
├── api/                # Axum HTTP server
│   ├── src/
│   │   ├── main.rs     # Entry point
│   │   ├── db.rs       # SQLite + migrations
│   │   ├── auth.rs     # Argon2, JWT, sessions
│   │   ├── store.rs    # All DB operations
│   │   ├── routes.rs   # Axum handlers
│   │   └── mcp.rs      # MCP JSON-RPC protocol
│   └── migrations/
│       └── 001_initial.sql
├── worker/             # Background worker
│   ├── src/
│   │   ├── main.rs     # Entry point
│   │   ├── lib.rs      # Polling loop
│   │   └── embed.rs    # ONNX Runtime embeddings
└── mcp/                # MCP client SDK (placeholder)
```

## Features

- **Cross-platform identity mapping**: Discord, Telegram, Slack user IDs → centralized actor slug
- **MCP-first**: Model Context Protocol server with `identity_link`, `identity_resolve`, `recall`, `journal_create`, `tasks_list`, etc.
- **SSE event bus**: Real-time wire log fan-out via `/api/events`
- **FTS5 search**: Full-text search over journal entries
- **Semantic search**: ONNX Runtime embeddings with cosine similarity
- **Auth**: Session cookies + Bearer API tokens + OAuth 2.1 consent flow (placeholder)

## Docker Image

The `Dockerfile.dev` includes:
- Rust 1.84 + cargo-watch + sqlx-cli + trunk + wasm-bindgen
- .NET 9.0 SDK
- Node 22 + pnpm
- Zig 0.13.0
- 1Password CLI (op)
- Python 3 + uv + pyjwt

Perfect for Hermes agent containers needing multi-toolchain support.
