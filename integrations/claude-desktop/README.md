# Claude Desktop

One way to point Claude Desktop at hive now: the **`hive.mcpb` extension**,
which launches the `hive-bridge` binary over MCP's stdio transport against
the hive store on this machine. The hosted-era paths (OAuth custom
connector, bearer tokens, `POST /mcp`) died with the P2P pivot — there is no
server to connect to ([DIRECTION.md](../../docs/DIRECTION.md) D25).

## Requirements

- `hive-bridge` on `PATH`. From a hive checkout:

  ```bash
  cargo install --path bridge
  ```

  The `.mcpb` does not bundle the binary yet — real packaging (bundled
  per-platform binaries) arrives with the Phase 2 app bundles
  ([PLAN.md](../../docs/PLAN.md) PR 2.4/2.5).

- The hive desktop app **closed**. Interim-mode caveat: the bridge opens the
  same data dir as the app under a single-writer lock, so only one of them
  can run at a time. The Phase 2.4 proxy (bridge talks to the running app
  over a unix socket) removes this restriction.

## Install

1. Build the extension: `./scripts/build-mcpb.sh` → `dist/hive.mcpb`
   (releases attach it once the Phase 2 release pipeline returns).
2. Claude Desktop → **Settings → Extensions**, then drag `hive.mcpb` in (or
   open the file). There is nothing to configure — no URL, no token.

Under the hood: `manifest.json` tells Claude Desktop to run `hive-bridge`,
which speaks MCP's stdio framing (one JSON-RPC message per line), opens the
data dir (`$XDG_DATA_HOME/hive`, fallback `~/.local/share/hive`), resolves
the master key from the OS keychain once at startup, and dispatches tool
calls to hive-core's MCP layer. stdout carries protocol frames only; the
bridge's logs go to stderr, which Claude Desktop collects into its MCP log
files.

## Troubleshooting

| Symptom | Fix |
| ------- | --- |
| "spawn hive-bridge ENOENT" / extension fails to start | `hive-bridge` isn't on the PATH Claude Desktop sees. `cargo install --path bridge`, then restart Claude Desktop. GUI apps don't always inherit your shell PATH — on macOS, install to a standard location or launch Desktop from a terminal to verify. |
| "another hive process has this data dir open" | The hive desktop app (or another bridge) is running. Close it and retry — one process per data dir in interim mode. |
| "OS keychain unavailable" | The bridge resolves the master key from the OS keychain (Secret Service / macOS Keychain / Windows Credential Manager) exactly like the app. Make sure a keychain/secret service is available to your session; run the hive app once first so the key exists. |
| Tools answer but the store looks empty | The bridge opened a different data dir than the app. Both use `$XDG_DATA_HOME/hive` (fallback `~/.local/share/hive`); a flatpak-installed app uses `~/.var/app/com.beesroadhouse.Hive/data/hive`, which the bridge doesn't see yet — bridge/flatpak alignment lands with Phase 2 packaging. |

Bridge logs land in Claude Desktop's MCP logs (the `mcp-server-*.log` files —
macOS: `~/Library/Logs/Claude/`, Windows: `%APPDATA%\Claude\logs\`).
