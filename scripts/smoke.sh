#!/usr/bin/env bash
# The smoke tier in one command (docs/TESTING-STRATEGY.md §4.5): build the
# binaries the tier drives, export their ABSOLUTE paths, then run the
# crate-scoped suite with the tier armed.
#
# Why the binaries come from env and not `CARGO_BIN_EXE_*`: that variable only
# exists inside the crate that owns the binary, and a scenario like "the app
# serves the socket, the bridge talks to it" needs two at once. So multi-binary
# scenarios live in smoke/ and read HIVE_APP_BIN / HIVE_BRIDGE_BIN /
# HIVE_NODE_BIN; a test whose path is unset skips loudly instead of failing.
#
# The CI `smoke` job runs THIS script, so local and CI cannot drift. Extra args
# are passed through to cargo test (`scripts/smoke.sh --test restore`).
#
# Not built here: hive-app. It needs the webkit/gtk stack, and the one scenario
# that drives it (the app-boot bridge round-trip) needs a display — it rides
# the `ui-shots` container job with HIVE_APP_BIN set there (PR 5.0), and skips
# loudly everywhere else.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"

# "<crate dir>:<package>:<binary env var>", package name == binary name — a
# crate that has not landed yet is skipped, so this script keeps working
# unchanged as node/ and sync/ arrive (hive-node: PR 4.13).
binaries=(
  "bridge:hive-bridge:HIVE_BRIDGE_BIN"
  "node:hive-node:HIVE_NODE_BIN"
)
# "<crate dir>:<package>" — the tier's test crates. Crate-scoped on purpose:
# `--workspace` would drag hive-app (and webkit) into a job that does not need
# it, and the rest of the workspace is the `rust` job's business.
suites=(
  "bridge:hive-bridge"
  "node:hive-node"
  "sync:hive-sync"
  "smoke:hive-smoke"
)

build_pkgs=()
build_args=()
for entry in "${binaries[@]}"; do
  IFS=: read -r dir pkg _var <<<"$entry"
  if [ -f "$repo_root/$dir/Cargo.toml" ]; then
    build_pkgs+=("$pkg")
    build_args+=(-p "$pkg")
  fi
done
if [ ${#build_args[@]} -gt 0 ]; then
  echo "🔨 building smoke binaries: ${build_pkgs[*]}"
  cargo build "${build_args[@]}"
fi

for entry in "${binaries[@]}"; do
  IFS=: read -r dir pkg var <<<"$entry"
  [ -f "$repo_root/$dir/Cargo.toml" ] || continue
  bin="$target_dir/debug/$pkg"
  if [ ! -x "$bin" ]; then
    echo "✗ expected $bin after cargo build -p $pkg" >&2
    exit 1
  fi
  export "$var=$bin"
  echo "   $var=$bin"
done

test_pkgs=()
test_args=()
for entry in "${suites[@]}"; do
  IFS=: read -r dir pkg <<<"$entry"
  if [ -f "$repo_root/$dir/Cargo.toml" ]; then
    test_pkgs+=("$pkg")
    test_args+=(-p "$pkg")
  fi
done

echo "🧪 smoke tier: ${test_pkgs[*]}"
# HIVE_EMBED=hash for the same reason the CI rust job sets it: no run of any
# tier may reach the HF hub for a model.
export HIVE_SMOKE=1
export HIVE_EMBED=hash
exec cargo test "${test_args[@]}" "$@"
