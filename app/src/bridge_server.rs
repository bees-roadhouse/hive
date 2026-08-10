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

/// Windows: the doorway is a named pipe (`\\.\pipe\hive-bridge-<hash>`, named
/// by `hive_shared::bridge_proto::pipe_path`) rather than a socket file,
/// because Windows has no filesystem sockets. Everything past the connect is
/// byte-identical to the unix path — same hello/ack, same newline-delimited
/// MCP frames, same `serve_bridge_connection` in core.
///
/// The unix side guards the door twice: chmod 0600 on the socket file, then an
/// SO_PEERCRED check on every accept. Windows needs BOTH properties too, and
/// gets them from the pipe's own security rather than after the fact:
///
///   * An explicit DACL naming exactly one trustee — the user this process
///     runs as. This is not belt-and-braces, it is REQUIRED: a named pipe
///     created with default security grants read access to Everyone and to
///     the anonymous account, so the frames would be readable by any other
///     user on the machine. The kernel evaluates the DACL on every open, so
///     unlike file permissions there is no window between create and chmod.
///   * FIRST_PIPE_INSTANCE on the first create. Pipe names live in a machine-
///     global namespace, so without it a process that got there first would
///     own the name and sit in the middle of the conversation. With it,
///     creation FAILS if the name is taken — the property the unix side got
///     for free by owning a path inside its own data dir.
///
/// Remote clients are refused outright: this doorway is for processes on this
/// machine, exactly like a unix socket.
///
/// Admins and SYSTEM can still reach it by taking ownership. So can root on a
/// 0600 socket; the threat model is unchanged.
#[cfg(windows)]
pub(crate) fn spawn(store: Store, actor_fallback: String) {
    tokio::spawn(async move {
        if let Err(e) = serve_pipe(store, actor_fallback).await {
            tracing::warn!("bridge pipe server stopped: {e:#}");
        }
    });
}

/// Platforms that are neither unix nor Windows have no doorway. The app still
/// runs; only the bridge is absent.
#[cfg(not(any(unix, windows)))]
pub(crate) fn spawn(_store: Store, _actor_fallback: String) {}

#[cfg(windows)]
async fn serve_pipe(store: Store, actor_fallback: String) -> anyhow::Result<()> {
    use anyhow::Context;
    use tokio::net::windows::named_pipe::ServerOptions;

    let name = hive_shared::bridge_proto::pipe_path(store.data_dir());
    let mut security =
        win_sec::OwnerOnly::new().context("building the bridge pipe security descriptor")?;

    // The store's flock already says this app is the only hive on this data
    // dir, so a name that is already taken means something ELSE holds it —
    // which is exactly what FIRST_PIPE_INSTANCE turns into a hard error.
    let mut server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(&name, security.as_ptr())
    }
    .with_context(|| format!("binding bridge pipe {name} (is another process holding it?)"))?;

    tracing::info!("bridge pipe listening at {name}");
    loop {
        server
            .connect()
            .await
            .context("accepting on the bridge pipe")?;
        let connected = server;

        // A pipe INSTANCE is consumed by the client that connects to it, so the
        // next instance must exist before this one is handed off or a second
        // client would find no door. Without first_pipe_instance here: the name
        // is already ours, and re-asserting exclusivity would fail by design.
        server = unsafe {
            ServerOptions::new()
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(&name, security.as_ptr())
        }
        .context("creating the next bridge pipe instance")?;

        let store = store.clone();
        let fallback = actor_fallback.clone();
        tokio::spawn(async move {
            if let Err(e) = pipe_connection(store, fallback, connected).await {
                tracing::warn!("bridge connection ended with error: {e:#}");
            }
        });
    }
}

#[cfg(windows)]
async fn pipe_connection(
    store: Store,
    actor_fallback: String,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
) -> anyhow::Result<()> {
    use anyhow::Context;

    // No peer-credential check, deliberately. The unix side needs one because
    // the 0600 check and the accept are separate moments; here the DACL was
    // enforced by the kernel at the client's open, so a connection existing at
    // all IS the proof that the peer is us.
    let (read, write) = tokio::io::split(pipe);
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

/// A `SECURITY_ATTRIBUTES` whose DACL names one trustee: the user this process
/// runs as. The Windows spelling of `chmod 0600`.
#[cfg(windows)]
mod win_sec {
    use std::ffi::c_void;

    use anyhow::{bail, Result};
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// SDDL revision. The header constant is `SDDL_REVISION_1`; it is 1 and has
    /// been since NT 4, and naming it here avoids a windows-sys module whose
    /// path moves between versions.
    const SDDL_REVISION: u32 = 1;

    pub(crate) struct OwnerOnly {
        attrs: SECURITY_ATTRIBUTES,
        /// Owned by LocalAlloc inside the Convert call; freed on drop.
        descriptor: *mut c_void,
    }

    impl OwnerOnly {
        pub(crate) fn new() -> Result<Self> {
            let sid = current_user_sid()?;
            // D:P             a DACL, protected — no inherited ACE joins it
            // (A;;GA;;;<sid>) allow GENERIC_ALL to exactly that one user
            // Nothing else. In particular no Everyone ACE, which is the whole
            // reason this type exists.
            let sddl = wide(&format!("D:P(A;;GA;;;{sid})"));

            let mut descriptor: *mut c_void = std::ptr::null_mut();
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                bail!(
                    "building the bridge pipe DACL failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            Ok(Self {
                attrs: SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                },
                descriptor,
            })
        }

        /// Pointer for `create_with_security_attributes_raw`. Borrows `self`,
        /// so the descriptor cannot be freed while a create is using it.
        pub(crate) fn as_ptr(&mut self) -> *mut c_void {
            std::ptr::addr_of_mut!(self.attrs) as *mut c_void
        }
    }

    impl Drop for OwnerOnly {
        fn drop(&mut self) {
            if !self.descriptor.is_null() {
                unsafe { LocalFree(self.descriptor) };
            }
        }
    }

    // SAFETY: the accept loop lives in a tokio task and holds this across every
    // `connect().await`, so the type has to cross threads. What it owns is one
    // LocalAlloc'd security-descriptor blob plus a SECURITY_ATTRIBUTES pointing
    // at it — plain data with no thread affinity and no interior sharing. The
    // kernel copies the descriptor during CreateNamedPipe rather than retaining
    // the pointer, ownership is exclusive, and Drop frees it exactly once. Only
    // Send: nothing hands out a shared reference, so Sync is not needed.
    unsafe impl Send for OwnerOnly {}

    /// The current process's user SID, in the string form SDDL wants.
    fn current_user_sid() -> Result<String> {
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            bail!(
                "opening this process's token failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let token = TokenHandle(token);

        // Two-call idiom: the first asks how big the answer is.
        let mut needed: u32 = 0;
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            bail!(
                "sizing the token user failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut buf = vec![0u8; needed as usize];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buf.as_mut_ptr().cast::<c_void>(),
                needed,
                &mut needed,
            )
        } == 0
        {
            bail!(
                "reading the token user failed: {}",
                std::io::Error::last_os_error()
            );
        }

        // SAFETY: the buffer holds a TOKEN_USER the kernel just wrote, and it
        // outlives the SID pointer read out of it on the next line.
        let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
        let mut raw: *mut u16 = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut raw) } == 0 {
            bail!(
                "formatting the user SID failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let sid = unsafe { from_wide(raw) };
        unsafe { LocalFree(raw.cast::<c_void>()) };
        Ok(sid)
    }

    /// Closes the token however the function exits.
    struct TokenHandle(HANDLE);

    impl Drop for TokenHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// SAFETY: `raw` must be a NUL-terminated wide string.
    unsafe fn from_wide(raw: *mut u16) -> String {
        let mut len = 0usize;
        while *raw.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(raw, len))
    }
}

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
