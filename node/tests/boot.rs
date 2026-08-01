// Node boot smoke (PLAN-v2.1 PR 4.6, listener assertions at 4.7; scenario 1 of
// docs/TESTING-STRATEGY.md §4.2): the REAL binary, a real socket, a real
// signal.
//
// This is the crate that owns the binary, so it is the one place
// `CARGO_BIN_EXE_hive-node` exists — multi-binary scenarios (and the
// enrollment ADVERSARIAL-SMOKE set) live in smoke/ and read `HIVE_NODE_BIN`
// instead (§4.1). Smoke tier, so every test opens with `require_smoke!()` and
// `cargo test --workspace` stays green offline.
//
// What the boot contract is, stated as assertions:
//
//   * `--listen 127.0.0.1:0` binds an ephemeral port and the node ANNOUNCES
//     it on stdout. No test may ever hard-code a port (parallel-safe), so
//     this line is load-bearing for every later harness.
//   * the socket is live, and what answers on it is mTLS: a peer with no
//     pinned key gets no session (PR 4.7). A fresh node has pinned nobody and
//     has no enrollment window open, so the door is shut to everyone —
//     which is the state where a fail-OPEN bug would be catastrophic and
//     silent, and therefore the one worth asserting at boot.
//   * SIGTERM is a clean exit, status 0. A node that had to be SIGKILLed
//     would leave the question of what it was mid-write on.
//   * a reboot on the same root re-opens the same node key and the same
//     domains. The key is the device identity a peer pins (PR 4.7): a node
//     that re-keyed on restart would become a different node.
//
// Harness rules (§4.1): bind :0 and parse the port, no bare sleeps, child
// output teed into the test log, kill-on-drop on every spawned process.

#![cfg(unix)]

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use hive_smoke_support::{require_smoke, test_identity};
use hive_sync::tls::{self, DeviceCert, PinSet, SpkiPin};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// How long we wait for the node to announce its port. The tier's cap is 60s
/// per test; a bind is instant, so 10s here is a stuck-process detector.
const BOOT_DEADLINE: Duration = Duration::from_secs(10);

/// The 32 bytes behind a `node key <hex>` line — the key a device pins.
fn parse_node_key(line: &str) -> [u8; 32] {
    let hex = line
        .strip_prefix("node key ")
        .unwrap_or_else(|| panic!("{line:?} is not a node-key line"));
    data_encoding::HEXLOWER
        .decode(hex.trim().as_bytes())
        .unwrap_or_else(|e| panic!("node key {hex:?} is not lowercase hex: {e}"))
        .try_into()
        .unwrap_or_else(|v: Vec<u8>| panic!("node key is {} bytes, want 32", v.len()))
}

/// A booted node process and everything it said before it started listening.
struct Booted {
    child: Child,
    addr: SocketAddr,
    /// The stdout contract lines, in order, up to and including `listening on`.
    lines: Vec<String>,
}

impl Booted {
    async fn spawn(root: &Path) -> Booted {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hive-node"))
            .arg("--root")
            .arg(root)
            .args(["--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The harness rule: a panicking test must not leave a listener
            // behind for the next one to collide with.
            .kill_on_drop(true)
            .spawn()
            .expect("spawning hive-node");

        // stderr is the node's log; tee it so a failure here reads like a
        // failure there.
        let stderr = child.stderr.take().expect("piped stderr");
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[hive-node] {line}");
            }
        });

        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout).lines();
        let mut lines = Vec::new();
        let addr = tokio::time::timeout(BOOT_DEADLINE, async {
            while let Some(line) = reader.next_line().await.expect("reading node stdout") {
                eprintln!("[hive-node] {line}");
                let listening = line.strip_prefix("listening on ").map(str::to_string);
                lines.push(line);
                if let Some(addr) = listening {
                    return addr.parse::<SocketAddr>().expect("an announced address");
                }
            }
            panic!("hive-node exited before it announced a port: {lines:?}");
        })
        .await
        .unwrap_or_else(|_| panic!("hive-node never announced a port within {BOOT_DEADLINE:?}"));

        // Whatever the node says after the listening line belongs in the log,
        // not in a buffer nobody drains (a full pipe would wedge it).
        tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                eprintln!("[hive-node] {line}");
            }
        });

        Booted { child, addr, lines }
    }

    fn line_starting(&self, prefix: &str) -> Option<&str> {
        self.lines
            .iter()
            .find(|l| l.starts_with(prefix))
            .map(|l| l.as_str())
    }

    fn lines_starting(&self, prefix: &str) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|l| l.starts_with(prefix))
            .map(|l| l.as_str())
            .collect()
    }

    /// SIGTERM, then wait for the exit status. Not `Child::kill` — that is
    /// SIGKILL, which would prove nothing about a clean shutdown.
    async fn terminate(mut self) -> std::process::ExitStatus {
        let pid = self.child.id().expect("a running child");
        let status = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await
            .expect("sending SIGTERM");
        assert!(status.success(), "kill -TERM {pid} failed");
        tokio::time::timeout(BOOT_DEADLINE, self.child.wait())
            .await
            .expect("hive-node did not exit after SIGTERM")
            .expect("waiting for hive-node")
    }
}

#[tokio::test]
async fn a_node_boots_announces_its_port_serves_and_exits_on_sigterm() {
    require_smoke!();
    let root = tempfile::tempdir().expect("node root");
    let node = Booted::spawn(root.path()).await;

    assert_ne!(node.addr.port(), 0, "the announced port is the bound one");
    assert!(node.addr.ip().is_loopback());
    let key = node
        .line_starting("node key ")
        .expect("the node announces the key a device pins");
    assert_eq!(key.len(), "node key ".len() + 64, "{key}");
    assert!(
        root.path().join("node.key").is_file(),
        "minted on first boot"
    );

    // Health: the socket is live, and mTLS is what answers on it. A fresh
    // node has pinned nobody and no enrollment window is open, so this
    // perfectly valid device certificate is refused — proving both that the
    // listener is really there and that its default is CLOSED.
    let stranger = test_identity("stranger");
    let cert = DeviceCert::self_signed(&stranger).expect("mint a device cert");
    let node_pin = SpkiPin::from_ed25519_public(&parse_node_key(key));
    let connector = tls::connector(&cert, Arc::new(PinSet::one(node_pin))).expect("build a dialer");
    let refused = tokio::time::timeout(BOOT_DEADLINE, async {
        let tcp = tokio::net::TcpStream::connect(node.addr).await?;
        // Past the handshake to a byte exchange: in TLS 1.3 a rejected dialer
        // can see `connect` return before the listener has judged its
        // certificate, so "refused" means "could not exchange data" (the
        // definition sync/tests/tls_loopback.rs uses).
        let (mut stream, _) = tls::connect_pinned(&connector, tcp).await?;
        stream.write_all(b"hello?").await?;
        stream.flush().await?;
        let mut buf = [0u8; 1];
        anyhow::ensure!(stream.read(&mut buf).await? > 0, "no answer");
        Ok::<_, anyhow::Error>(())
    })
    .await
    .expect("the node settled the probe well inside the deadline");
    let err = refused.expect_err("a node that has pinned nobody accepts nobody");
    eprintln!("[refused as expected] {err:#}");

    let status = node.terminate().await;
    assert!(
        status.success(),
        "SIGTERM must be a clean exit, got {status:?}"
    );
}

#[tokio::test]
async fn a_reboot_reopens_the_same_node_and_the_same_domains() {
    require_smoke!();
    let root = tempfile::tempdir().expect("node root");
    // A vault tree that arrived by ordinary means (rsync, a restored
    // snapshot — D36). The node discovers domains; it is not told them.
    let domain = root
        .path()
        .join("tenants/household/domains/bierlysmith.com");
    std::fs::create_dir_all(&domain).unwrap();

    let first = Booted::spawn(root.path()).await;
    let first_key = first
        .line_starting("node key ")
        .expect("a key line")
        .to_string();
    let first_domains: Vec<String> = first
        .lines_starting("domain ")
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(first_domains, vec!["domain household/bierlysmith.com"]);
    assert!(
        domain.join("node-meta.db").is_file(),
        "the vault got its bookkeeping at open"
    );
    assert!(domain.join("blocks").is_dir(), "and the store shape");
    assert!(first.terminate().await.success());

    let second = Booted::spawn(root.path()).await;
    assert_eq!(
        second.line_starting("node key "),
        Some(first_key.as_str()),
        "a node that re-keyed on restart would be a different node to every peer"
    );
    assert_eq!(
        second
            .lines_starting("domain ")
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        first_domains
    );
    assert_ne!(
        second.addr.port(),
        0,
        "and it binds again — :0 means a new port, which is why nothing caches one"
    );
    assert!(second.terminate().await.success());
}

/// A config the node cannot honour must fail at boot, on the box where
/// someone can act on it — not silently, and not at the first publish.
#[tokio::test]
async fn a_config_the_node_cannot_honour_fails_the_boot() {
    require_smoke!();
    let root = tempfile::tempdir().expect("node root");
    std::fs::write(
        root.path().join("node.toml"),
        "[dns]\nprovider = \"rfc2136\"\nzone = \"bierlysmith.com\"\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_hive-node"))
        .arg("--root")
        .arg(root.path())
        .args(["--listen", "127.0.0.1:0"])
        .output()
        .await
        .expect("running hive-node");
    assert!(
        !out.status.success(),
        "a broken [dns] block is a boot failure"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[dns.rfc2136]"),
        "the refusal names what is missing: {stderr}"
    );
}

/// `--root` is what a node IS; refusing without it beats inventing a default
/// directory under someone's home.
#[tokio::test]
async fn the_binary_refuses_to_guess_its_root() {
    require_smoke!();
    let out = Command::new(env!("CARGO_BIN_EXE_hive-node"))
        .output()
        .await
        .expect("running hive-node");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--root is required"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
