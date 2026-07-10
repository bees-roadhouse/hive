#!/usr/bin/env bash
# Idempotent setup for the hive dev environment (Rust workspace + Postgres).
# Run by the SessionStart hook so a fresh Claude Code container comes up ready
# to develop. Safe to re-run.
set -euo pipefail

cd "$(dirname "$0")"

# The store layer is Postgres (the Rust rewrite retired SQLite). Bring up a
# local dev container when a container runtime is available; otherwise the
# printed commands rely on whatever DATABASE_URL the environment provides.
DB_URL="${DATABASE_URL:-postgres://hive:hive@localhost:5432/hive}"
RUNTIME="$(command -v podman || command -v docker || true)"
if [ -n "$RUNTIME" ]; then
  if "$RUNTIME" ps --format '{{.Names}}' 2>/dev/null | grep -qx hive-pg; then
    echo "✓ hive-pg already running"
  elif "$RUNTIME" ps -a --format '{{.Names}}' 2>/dev/null | grep -qx hive-pg; then
    echo "🐘 starting existing hive-pg container…"
    "$RUNTIME" start hive-pg >/dev/null
  else
    echo "🐘 creating hive-pg (pgvector-enabled postgres 17 on :5432)…"
    "$RUNTIME" run -d --name hive-pg \
      -e POSTGRES_USER=hive -e POSTGRES_PASSWORD=hive -e POSTGRES_DB=hive \
      -p 5432:5432 docker.io/pgvector/pgvector:pg17 >/dev/null
  fi
else
  echo "⚠ no container runtime found — set DATABASE_URL to a pgvector-capable Postgres 17"
fi

echo "✅ ready."
echo "   Tests: DATABASE_URL=$DB_URL HIVE_EMBED=hash cargo test --workspace"
