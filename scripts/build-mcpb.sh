#!/usr/bin/env bash
# Packs the Claude Desktop extension (integrations/claude-desktop/mcpb) into
# dist/hive.mcpb. Run locally or by the release-on-tag job, which attaches the
# artifact to the GitHub Release.
#
# Prefers the official packer (`@anthropic-ai/mcpb`, which also validates the
# manifest); falls back to a plain zip — a .mcpb is a zip archive with
# manifest.json at its root — so the release never fails on the tool being
# unavailable or the registry being unreachable.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bundle_dir="$repo_root/integrations/claude-desktop/mcpb"
out="$repo_root/dist/hive.mcpb"

mkdir -p "$repo_root/dist"
rm -f "$out"

if command -v npx >/dev/null 2>&1 && npx --yes @anthropic-ai/mcpb pack "$bundle_dir" "$out"; then
  echo "packed with @anthropic-ai/mcpb"
else
  echo "mcpb CLI unavailable — packing with plain zip" >&2
  rm -f "$out" # zip appends to an existing archive; start clean
  (cd "$bundle_dir" && zip -r -X "$out" . -x '.*' -x '*/.*')
fi

echo "done: $(du -h "$out" | cut -f1) $out"
