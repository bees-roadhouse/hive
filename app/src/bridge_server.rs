// The bridge socket (D25; PLAN.md PR 2.4) — the app side of hive-bridge's
// proxy mode. A newline-delimited JSON-RPC (MCP) server on a unix domain
// socket at `<data_dir>/bridge.sock`: hive-bridge connects, hands over a
// one-line hello (protocol version + acting identity), gets a one-line ack,
// then every further line each way is a raw MCP frame dispatched through
// hive-core's transport-free `mcp::handle_frame`. The wire contract lives
// in `hive_shared::bridge_proto` so the two binaries can never drift.
//
// Trust model (single user, D16): accept() verifies the peer's uid equals
// this process's euid (SO_PEERCRED) and the socket file is chmod 0600 —
// same-user hive-bridge processes only. The actor in the hello is then
// taken at its word, exactly like the interim bridge's `--actor` was.
//
// Lifecycle: the store's flock already guarantees this app is the ONLY hive
// process on the data dir, so any pre-existing socket file is a stale
// leftover from a kill (the app exits via process::exit, never unwinding) —
// unlink-then-bind is always correct. Connections are served concurrently
// (that is the point of proxy mode: Claude Desktop and a Claude Code hook
// may talk at once — every call funnels into the store's single writer
// thread); frames WITHIN a connection are handled sequentially, so a client
// vanishing mid-call never cancels an in-flight store write.

use hive_core::store::Store;

#[cfg(unix)]
pub(crate) fn spawn(store: Store, actor_fallback: String) {
    tokio::spawn(async move {
        if let Err(e) = serve(store, actor_fallback).await {
            tracing::warn!("bridge socket server stopped: {e:#}");
        }
    });
}

/// Windows: no unix sockets; named pipes land with the Windows bundles
/// (Phase 2.5+). hive-bridge is unix-only until then too.
#[cfg(not(unix))]
pub(crate) fn spawn(_store: Store, _actor_fallback: String) {}

#[cfg(unix)]
async fn serve(store: Store, actor_fallback: String) -> anyhow::Result<()> {
    use anyhow::Context;

    let path = store
        .data_dir()
        .join(hive_shared::bridge_proto::BRIDGE_SOCKET_FILE);
    // Stale from a previous kill, never live — see the module comment.
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("binding bridge socket {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting bridge socket {}", path.display()))?;
    }
    tracing::info!("bridge socket listening at {}", path.display());
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let store = store.clone();
                let fallback = actor_fallback.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(store, fallback, stream).await {
                        tracing::warn!("bridge connection ended with error: {e:#}");
                    }
                });
            }
            Err(e) => {
                // Transient accept failures (EMFILE under fd pressure) must
                // not kill the doorway; pause and keep listening.
                tracing::warn!("bridge socket accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
}

#[cfg(unix)]
async fn connection(
    store: Store,
    actor_fallback: String,
    stream: tokio::net::UnixStream,
) -> anyhow::Result<()> {
    use anyhow::Context;

    // D25 peer-credential check: same-user clients only. Belt over the
    // socket file's 0600 braces (perms guard the path, this guards the fd).
    let cred = stream
        .peer_cred()
        .context("reading bridge peer credentials")?;
    let own = rustix::process::geteuid().as_raw();
    if cred.uid() != own {
        tracing::warn!(
            "refusing bridge connection from uid {} (app runs as {own})",
            cred.uid()
        );
        return Ok(());
    }

    // Handshake + frame loop live in core (shared with the bridge's tests,
    // so the exact code the app runs is the code CI exercises).
    let (read, write) = stream.into_split();
    hive_core::mcp::serve_bridge_connection(
        &store,
        &actor_fallback,
        tokio::io::BufReader::new(read),
        write,
    )
    .await
    .context("serving bridge connection")?;
    Ok(())
}
