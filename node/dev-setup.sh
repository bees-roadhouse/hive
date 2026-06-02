#!/usr/bin/env bash
# Idempotent setup for the Node/Solid hive. Run by the SessionStart hook so a
# fresh Claude Code web container comes up ready to `pnpm dev`. Safe to re-run.
set -euo pipefail

cd "$(dirname "$0")"

echo "🐝 hive(node) setup — installing deps…"
corepack enable >/dev/null 2>&1 || true
pnpm install --silent

# Seed the SQLite db once, so a fresh checkout has something to show.
if [ ! -f data/hive.db ]; then
  echo "🌱 seeding fresh database…"
  pnpm --filter @hive/api seed
else
  echo "✓ database already present (data/hive.db)"
fi

echo "✅ ready. Run:  cd node && pnpm dev   (api :8787 + web :5173)"
