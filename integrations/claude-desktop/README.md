# Claude Desktop

One way to point Claude Desktop at hive now: the **`hive.mcpb` extension**,
which launches the `hive-bridge` binary over MCP's stdio transport against
the running hive app on this machine. The hosted-era paths (OAuth custom
connector, bearer tokens, `POST /mcp`) died with the P2P pivot — there is no
server to connect to ([DIRECTION.md](../../docs/DIRECTION.md) D25).

## Requirements

- `hive-bridge` on `PATH`. From a hive checkout:

  ```bash
  cargo install --path bridge
  ```

  The `.mcpb` does not bundle the binary yet — real packaging (bundled
  per-platform binaries) arrives with the Phase 2 app bundles
  ([PLAN.md](../../docs/PLAN.md) PR 2.5).

- The hive desktop app **running**. The bridge is a proxy (D25): it
  connects to the app over `<data_dir>/bridge.sock` and has no store
  access of its own. While the app is closed, calls fail with "the hive
  app is not running". Claude Desktop, Claude Code, and hooks can all be
  connected at the same time.

## Install

1. Build the extension: `./scripts/build-mcpb.sh` → `dist/hive.mcpb`
   (releases attach it once the Phase 2 release pipeline returns).
2. Claude Desktop → **Settings → Extensions**, then drag `hive.mcpb` in (or
   open the file). There is nothing to configure — no URL, no token.

Under the hood: `manifest.json` tells Claude Desktop to run `hive-bridge`,
which speaks MCP's stdio framing (one JSON-RPC message per line) and pumps
it to the running app over the unix socket at `<data_dir>/bridge.sock`
(`$XDG_DATA_HOME/hive`, fallback `~/.local/share/hive`). The app owns the
store and the keychain; the bridge touches neither. stdout carries protocol
frames only; the bridge's logs go to stderr, which Claude Desktop collects
into its MCP log files.

## Troubleshooting

| Symptom | Fix |
| ------- | --- |
| "spawn hive-bridge ENOENT" / extension fails to start | `hive-bridge` isn't on the PATH Claude Desktop sees. `cargo install --path bridge`, then restart Claude Desktop. GUI apps don't always inherit your shell PATH — on macOS, install to a standard location or launch Desktop from a terminal to verify. |
| "the hive app is not running" | The bridge proxies to the app and cannot work without it. Start the hive app, then retry (reconnect the extension if the session already failed). |
| "did not answer the hive handshake" | Something other than the hive app is bound at `<data_dir>/bridge.sock`, or app and bridge are from very different builds. Update both to the same release. |
| Tools answer but the store looks empty | The bridge found a different socket than the app you meant. Both derive it from `$XDG_DATA_HOME/hive` (fallback `~/.local/share/hive`); a flatpak-installed app serves `~/.var/app/com.beesroadhouse.Hive/data/hive/bridge.sock` — point the bridge there with `HIVE_DATA_DIR` until Phase 2.5 packaging aligns the two out of the box. |

Bridge logs land in Claude Desktop's MCP logs (the `mcp-server-*.log` files —
macOS: `~/Library/Logs/Claude/`, Windows: `%APPDATA%\Claude\logs\`).
