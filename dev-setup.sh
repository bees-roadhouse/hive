#!/usr/bin/env bash
# Idempotent orientation for the hive dev environment. Run by the
# SessionStart hook so a fresh Claude Code container comes up ready to
# develop. Safe to re-run.
#
# Post-cutover reality (PR 1.6, PLAN-v2.1): the store is the append-only op
# log + a SQLCipher SQLite index under a local data dir. There is NO database
# service to start — the workspace suite is hermetic (tempdir stores,
# in-memory keys, the hash embedder) and passes offline. The ONE Postgres
# consumer left is the importer's fixture tests, which skip loudly without
# DATABASE_URL; bring a throwaway pgvector container up on demand with
# `./dev-setup.sh --importer-db`.
set -euo pipefail

cd "$(dirname "$0")"

if [ "${1:-}" = "--importer-db" ]; then
  RUNTIME="$(command -v podman || command -v docker || true)"
  if [ -z "$RUNTIME" ]; then
    echo "⚠ no container runtime found — point DATABASE_URL at any pgvector-capable Postgres 17 instead"
    exit 1
  fi
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
fi

echo "✅ ready — no services required (SQLite store; tests are hermetic)."
echo
echo "   Everyday gates (what CI's rust job runs):"
echo "     cargo fmt --all --check"
echo "     cargo clippy --workspace --all-targets -- -D warnings"
echo "     HIVE_EMBED=hash cargo test --workspace"
echo
echo "   Test tiers beyond that (docs/TESTING-STRATEGY.md; loud-skip without the var):"
echo "     ./scripts/smoke.sh                                    # smoke tier: real binaries/sockets (what CI's smoke job runs)"
echo "     HIVE_SHOTS=1 ...                                      # screenshot tier: pinned container only (PR 5.0)"
echo
echo "   Importer DB tests (the one Postgres exception; needs ./dev-setup.sh --importer-db):"
echo "     DATABASE_URL=postgres://hive:hive@localhost:5432/hive cargo test -p hive-import"
