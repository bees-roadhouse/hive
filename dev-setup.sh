#!/usr/bin/env bash
# Idempotent setup for the hive dev environment (Rust API + Postgres). Run by
# the SessionStart hook so a fresh Claude Code container comes up ready to
# develop. Safe to re-run.
#
#   ./dev-setup.sh                       bring the dev Postgres up
#   ./dev-setup.sh --drop-test-schemas   also sweep leftover per-test schemas
set -euo pipefail

cd "$(dirname "$0")"

SWEEP=0
for arg in "$@"; do
  case "$arg" in
    --drop-test-schemas) SWEEP=1 ;;
    -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

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

# ---- leftover per-test schemas -------------------------------------------
#
# `hive_core::db::test_pool()` builds one throwaway schema per test, named
# `t_<uuid-simple>`. It hands back a `TestDb` whose Drop drops the schema, so a
# normal `cargo test` run — pass or fail — leaves nothing behind. This sweep is
# for the two cases Drop cannot cover: databases written to before that guard
# existed, and a test process killed hard enough to skip unwinding (SIGKILL,
# panic = abort).
#
# The pattern is anchored on the exact shape test_pool generates rather than a
# loose `t_%`, so a real schema that happens to start with `t_` is never in
# range. `\gexec` rather than a `DO` loop on purpose: a DO block is ONE
# transaction, and a few hundred schemas × ~42 tables each blows past
# max_locks_per_transaction ("out of shared memory") and rolls the whole sweep
# back. \gexec runs each DROP as its own statement, so each commits alone.
#
# Count what is there — the check that should read the same before and after a
# test run:
#
#   podman exec hive-pg psql -U hive -d <db> -t -c \
#     "SELECT count(*) FROM information_schema.schemata WHERE schema_name LIKE 't\_%';"
COUNT_SQL="SELECT count(*) FROM information_schema.schemata WHERE schema_name LIKE 't\\_%';"
SWEEP_SQL=$(cat <<'SQL'
SET client_min_messages TO warning;
SELECT format('DROP SCHEMA %I CASCADE', nspname)
  FROM pg_namespace
 WHERE nspname ~ '^t_[0-9a-f]{32}$'
\gexec
SQL
)

# $1 = human label, rest = the psql invocation to pipe SQL into.
sweep_one() {
  local label="$1"; shift
  local before after
  before="$("$@" -t -A -c "$COUNT_SQL")"
  printf '%s\n' "$SWEEP_SQL" | "$@" -q -v ON_ERROR_STOP=1
  after="$("$@" -t -A -c "$COUNT_SQL")"
  echo "🧹 $label: dropped $((before - after)) leftover test schema(s), $after left"
}

if [ "$SWEEP" = 1 ]; then
  if [ -n "$RUNTIME" ] && "$RUNTIME" ps --format '{{.Names}}' 2>/dev/null | grep -qx hive-pg; then
    # Every database on the dev instance: test runs land wherever DATABASE_URL
    # pointed that day, so the debris is rarely all in one.
    dbs="$("$RUNTIME" exec hive-pg psql -U hive -d postgres -t -A \
             -c 'SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate')"
    for db in $dbs; do
      sweep_one "$db" "$RUNTIME" exec -i hive-pg psql -U hive -d "$db"
    done
  elif command -v psql >/dev/null; then
    sweep_one "$DB_URL" psql "$DB_URL"
  else
    echo "⚠ --drop-test-schemas needs the hive-pg container or a local psql" >&2
    exit 1
  fi
fi

echo "✅ ready."
echo "   API:    DATABASE_URL=$DB_URL HIVE_EMBED=hash cargo run -p hive-api    (:7878, MCP at /mcp)"
echo "   Sweep:  ./dev-setup.sh --drop-test-schemas    (leftover t_<uuid> test schemas)"
