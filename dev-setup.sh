#!/usr/bin/env bash
# Idempotent setup for the Node/Solid hive. Run by the SessionStart hook so a
# fresh Claude Code web container comes up ready to `pnpm dev`. Safe to re-run.
set -euo pipefail

cd "$(dirname "$0")"

echo "🐝 hive setup — installing deps…"
corepack enable >/dev/null 2>&1 || true
# CI=true: non-interactive. minimumReleaseAge=0: this sandbox's registry policy
# can flag very-recently-published transitives; we don't gate on release age.
CI=true pnpm install --no-frozen-lockfile --config.minimumReleaseAge=0

# Seed the SQLite db once, so a fresh checkout has something to show.
if [ ! -f data/hive.db ]; then
  echo "🌱 seeding fresh database…"
  pnpm --filter @hive/api seed
else
  echo "✓ database already present (data/hive.db)"
fi

echo "✅ ready."
echo "   API + web:  pnpm dev        (api :8787 + web :5173, MCP at /mcp)"
echo "   worker:     pnpm --filter @hive/worker start   (or: once)"
