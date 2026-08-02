// hive-bridge — the stdio MCP doorway into the local hive (D25; PLAN.md
// PR 2.4, proxy mode).
//
// Proxy mode is the ONLY mode: the bridge holds no store access at all. It
// connects to the RUNNING hive app over the unix socket at
// `<data_dir>/bridge.sock` (wire contract: hive_shared::bridge_proto —
// one-line hello/ack handshake, then raw newline-delimited MCP JSON-RPC
// frames both ways) and pumps stdio ↔ socket. All protocol handling —
// initialize negotiation, tools, the store itself — lives app-side in
// hive-core; this binary is a pipe with a handshake. When the app is not
// running it says so and exits, rather than growing its own store access:
// the stable failure surface is stderr containing "the hive app is not
// running", exit code 1, and NOTHING on stdout (the plugin's soft-fail
// path depends on that shape).
//
// Interim mode (PR 1.8) — the bridge opening the store directly, one hive
// process per data dir — died here. So did HIVE_MEMORY_KEY_HEX: no store,
// no keychain, no key.
//
// Transport rules:
//   - stdout carries JSON-RPC frames ONLY — one message per line (the MCP
//     stdio framing). Nothing else may ever print there; every log line
//     goes to stderr.
//   - No HTTP stacks, no async runtime: std sync IO end to end.
//
// Bridge-only environment (deliberately NOT honored by core or the app):
//   HIVE_DATA_DIR   override the data dir the socket lives under
//                   (tests, nonstandard homes)

#[cfg(unix)]
mod proxy;

#[cfg(unix)]
fn main() {
    match proxy::run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("hive-bridge: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Windows: no unix sockets — named-pipe support lands with the Windows app
/// bundles (post-2.5), same story as the app-side server stub.
#[cfg(not(unix))]
fn main() {
    eprintln!(
        "hive-bridge: proxy mode is unix-only for now (Windows named pipes \
         land with the Windows app bundles)"
    );
    std::process::exit(1);
}
