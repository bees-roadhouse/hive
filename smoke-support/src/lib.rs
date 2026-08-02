// Smoke-tier seams and harness helpers (docs/TESTING-STRATEGY.md §4,
// PLAN-v2.1 PR 4.2).
//
// The smoke tier is the second test layer: real binaries, real sockets,
// multi-process scenarios. This crate is its ONE-SEAM half — what
// `core/tests/common/mod.rs::test_store()` is to hive-core's unit tests,
// `test_domain()` is to a smoke scenario, and it carries the same AGENTS.md
// clause: no test constructs a domain, device identity, node, or device pair
// any other way.
// That is what lets the seam grow without rewriting a single scenario body:
// `test_node()` arrived with the node's protocol (the listener + enrollment at
// PR 4.7) and `test_pair()` arrives with the push loop (4.8).
//
// `test_node()` drives the REAL `hive-node` binary from `HIVE_NODE_BIN` rather
// than embedding the library, and it is the crate's only process-owning seam.
// Two reasons, both about what the tier is for: an enrollment scenario has to
// exercise the operator's actual `hive-node enroll` command (a second process
// writing the same node-meta.db is the deployment, not an approximation of
// it), and this crate stays free of a hive-node dependency, so hive-node's own
// tests can keep dev-depending on it without a cycle.
//
// Three rules this crate exists to make unavoidable:
//
//   * LOUD-SKIP — every smoke test opens with `require_smoke!()`. Without
//     `HIVE_SMOKE=1` it prints one skip line and returns, still passing, so
//     plain `cargo test --workspace` stays green offline with no new tooling
//     (the idiom is the importer's `require_pg!`).
//   * KEY-INJECT — a domain's master key is a tempdir file in the ONE shared
//     64-hex format (`hive_core::keys::parse_master_key_hex`): the app's
//     `HIVE_MASTER_KEY_FILE` seam, the node's `FileKeySource`, the screenshot
//     fixture seeder, and this seam all read the same bytes. Device transport
//     identities (`test_identity`, PR 4.5) are injected the same way, derived
//     from their name. CI never touches an OS keychain.
//   * No bare sleeps — `wait_until` is the only sanctioned wait primitive.
//     Bounded (10s default), polled (50ms), and it names what it waited for
//     when it gives up, so a hung scenario fails with a sentence instead of a
//     60s test-harness timeout.
//
// Cross-crate binaries: `CARGO_BIN_EXE_*` only exists inside the crate that
// OWNS the binary, and a multi-binary scenario needs two of them at once, so
// the tier reads absolute paths from `HIVE_APP_BIN` / `HIVE_BRIDGE_BIN` /
// `HIVE_NODE_BIN` through `require_bin!` — exported by `scripts/smoke.sh` and
// the CI `smoke` job after they build the binaries. A test whose path env is
// unset skips loudly too.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hive_core::identity::DeviceIdentity;
use hive_core::keys::{KeySource, MemoryKeySource};
use hive_core::store::Store;

// ── tier gating (LOUD-SKIP) ─────────────────────────────────────────────────

/// The env var that arms the smoke tier.
pub const SMOKE_ENV: &str = "HIVE_SMOKE";

/// Whether the smoke tier is armed. Off unless `HIVE_SMOKE` is set to
/// something meaningful — the sense is inverted from the app's default-on
/// flags (`HIVE_MAIL_ENABLED` and friends) because a tier that ran by
/// accident would drag real binaries into every offline `cargo test`.
pub fn smoke_enabled() -> bool {
    enabled(std::env::var(SMOKE_ENV).ok().as_deref())
}

/// The pure half of `smoke_enabled`, so the parse is testable without
/// mutating process-global env (the PR 4.1 `resolve_data_dir` precedent).
fn enabled(raw: Option<&str>) -> bool {
    match raw {
        Some(v) => !matches!(v.trim(), "" | "0" | "false" | "no" | "off"),
        None => false,
    }
}

/// Opens every smoke test: skip loudly (and pass) unless `HIVE_SMOKE=1`.
#[macro_export]
macro_rules! require_smoke {
    () => {
        if !$crate::smoke_enabled() {
            eprintln!(
                "skipping: smoke tier disabled (set HIVE_SMOKE=1, or run scripts/smoke.sh \
                 which builds the binaries too)"
            );
            return;
        }
    };
}

/// Absolute path to a binary the tier drives, from `$var`. `None` means the
/// var is unset — the caller skips loudly. A var that IS set but does not
/// point at a file is a harness bug, not a skip condition, so it panics: a
/// silent skip there would hide the scenario from CI forever.
pub fn bin_path(var: &str) -> Option<PathBuf> {
    let raw = std::env::var(var).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    assert!(
        path.is_file(),
        "{var} points at {} which is not a file — build it first (scripts/smoke.sh does)",
        path.display()
    );
    Some(path)
}

/// `require_bin!("HIVE_BRIDGE_BIN")` → the absolute path, or a loud skip.
#[macro_export]
macro_rules! require_bin {
    ($var:expr) => {
        match $crate::bin_path($var) {
            Some(path) => path,
            None => {
                eprintln!(
                    "skipping: {} is not set (scripts/smoke.sh builds that binary and exports it)",
                    $var
                );
                return;
            }
        }
    };
}

// ── waiting ─────────────────────────────────────────────────────────────────

/// How long `wait_until` waits before it gives up.
pub const WAIT_DEADLINE: Duration = Duration::from_secs(10);

/// How often `wait_until` re-checks its condition.
pub const WAIT_POLL: Duration = Duration::from_millis(50);

/// Poll `cond` until it holds, panicking with `what` if it never does. The
/// tier's ONLY wait primitive — a bare `sleep` either flakes on a loaded CI
/// runner or wastes the budget on every green run.
///
/// The condition is async because most smoke waits are store reads (`|| async
/// { store.canonical_dump().await.unwrap() == want }`); a synchronous check
/// wraps as `|| async { path.exists() }`.
pub async fn wait_until<F, Fut>(what: &str, cond: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    wait_until_within(what, WAIT_DEADLINE, WAIT_POLL, cond).await
}

/// `wait_until` with an explicit deadline/poll — for the rare wait whose
/// bound is known to be much shorter or much longer than the default. Every
/// smoke test still lives under the tier's 60s hard cap.
pub async fn wait_until_within<F, Fut>(what: &str, deadline: Duration, poll: Duration, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        if cond().await {
            return;
        }
        if start.elapsed() >= deadline {
            panic!("timed out after {deadline:?} waiting for {what}");
        }
        tokio::time::sleep(poll).await;
    }
}

// ── the domain seam (ONE-SEAM + KEY-INJECT) ─────────────────────────────────

/// The smoke tier's fixed test master key — the same fixed-key posture
/// `test_store()` takes with `MemoryKeySource([7u8; 32])`. Test-only, by
/// construction: it is a constant in a test-support crate.
pub const TEST_MASTER_KEY: [u8; 32] = [7u8; 32];

/// The domain's master-key file, in the data dir the scenario owns.
pub const MASTER_KEY_FILE: &str = "master.hex";

/// A throwaway domain: a tempdir plus its master key on disk in the shared
/// 64-hex format. Devices (and, from PR 4.5, the node) are opened underneath
/// it, all sharing that one master — which is what "one domain" means.
pub fn test_domain() -> TestDomain {
    test_domain_with_master(TEST_MASTER_KEY)
}

/// `test_domain()` with an explicit master, for scenarios that need two
/// domains that genuinely cannot read each other (foreign-domain rejection).
pub fn test_domain_with_master(master: [u8; 32]) -> TestDomain {
    let dir = tempfile::tempdir().expect("smoke domain tempdir");
    let key_file = dir.path().join(MASTER_KEY_FILE);
    let hex = data_encoding::HEXLOWER.encode(&master);
    std::fs::write(&key_file, format!("{hex}\n"))
        .unwrap_or_else(|e| panic!("writing {}: {e}", key_file.display()));
    // It is a master key: 0600 even in a tempdir, so nothing in the tier
    // models a key file the ops docs would call misconfigured.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600))
            .unwrap_or_else(|e| panic!("chmod 600 {}: {e}", key_file.display()));
    }
    TestDomain { dir, master }
}

/// One smoke domain. Holds its tempdir, so keep it alive for the whole test —
/// dropping it deletes the dir out from under any store still open on it.
pub struct TestDomain {
    dir: tempfile::TempDir,
    master: [u8; 32],
}

impl TestDomain {
    /// The domain root: `master.hex` sits here, devices live under `devices/`.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The domain master, for in-proc consumers.
    pub fn master_key(&self) -> [u8; 32] {
        self.master
    }

    /// The 64-hex master-key file — what cross-PROCESS consumers get
    /// (`HIVE_MASTER_KEY_FILE` for the app, `--master-key-file` for the node).
    pub fn master_key_file(&self) -> PathBuf {
        self.dir.path().join(MASTER_KEY_FILE)
    }

    /// The domain master as a `KeySource`, for in-PROC consumers. Same bytes
    /// as the file; `MemoryKeySource` just skips the read.
    pub fn key_source(&self) -> Arc<dyn KeySource + Send + Sync> {
        Arc::new(MemoryKeySource(self.master))
    }

    /// Where device `name`'s data dir lives. Exposed because restore-style
    /// scenarios copy these dirs around before opening them.
    pub fn device_dir(&self, name: &str) -> PathBuf {
        self.dir.path().join("devices").join(name)
    }

    /// Device `name`'s TRANSPORT identity (PLAN-v2.1 PR 4.5) — the ed25519 +
    /// x25519 keypair a peer pins, as distinct from the domain master above.
    /// Same name, same device, every run: a scenario can mint the identity
    /// before the store exists (enrollment, PR 4.7) and get the same keys.
    pub fn device_identity(&self, name: &str) -> DeviceIdentity {
        test_identity(name)
    }

    /// Open a device store under this domain's master, creating its data dir
    /// if absent — and opening whatever is already there if not, which is how
    /// a restored (copied) dir gets opened. Deterministic hash embedder, so
    /// the tier never reaches for a model.
    pub fn open_device(&self, name: &str) -> Store {
        let dir = self.device_dir(name);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
        Store::new(&dir, self.key_source(), Arc::new(hive_embed::HashEmbedder))
            .unwrap_or_else(|e| panic!("opening smoke device {name:?} at {}: {e:#}", dir.display()))
    }
}

// ── device identities (ONE-SEAM + KEY-INJECT) ───────────────────────────────

/// blake3 derive_key context for the tier's device seeds. Test-only material
/// by construction: the context string says so, so a seed minted here can
/// never collide with one a real device generated.
const TEST_IDENTITY_CONTEXT: &str = "hive-smoke-device-identity-v1";

/// A named throwaway device identity — deterministic in the name, so a
/// failing transport scenario names the same keys on every rerun and a pinned
/// key can be written into a fixture. The identity half of KEY-INJECT: the
/// tier never asks an OS keychain for a device key, exactly as it never asks
/// for a master key.
pub fn test_identity(name: &str) -> DeviceIdentity {
    DeviceIdentity::from_seed(&blake3::derive_key(TEST_IDENTITY_CONTEXT, name.as_bytes()))
}

// ── the node seam (ONE-SEAM, PLAN-v2.1 PR 4.7) ──────────────────────────────

/// How long a scenario waits for the node to announce its port. The tier's cap
/// is 60s per test; a bind is instant, so this is a stuck-process detector.
pub const NODE_BOOT_DEADLINE: Duration = Duration::from_secs(10);

/// A throwaway node: its own root, the domains it was asked to hold, and a
/// REAL `hive-node` process serving them on an ephemeral port.
///
/// `bin` is `require_bin!("HIVE_NODE_BIN")` — the seam takes the path rather
/// than reading the env itself, because a missing binary is a LOUD-SKIP the
/// test body has to return from.
///
/// `domains` are `<tenant>/<domain>` strings, created BEFORE the node boots.
/// That ordering is load-bearing rather than cosmetic: the node discovers its
/// vault tree at open (`server.rs`), so a domain minted after boot is one the
/// running listener cannot serve until it restarts.
pub async fn test_node(bin: &Path, domains: &[&str]) -> TestNode {
    let root = tempfile::tempdir().expect("smoke node root");
    for domain in domains {
        let (tenant, name) = domain
            .split_once('/')
            .unwrap_or_else(|| panic!("{domain:?} is not <tenant>/<domain>"));
        let dir = root
            .path()
            .join("tenants")
            .join(tenant)
            .join("domains")
            .join(name);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {}: {e}", dir.display()));
    }
    TestNode::boot(bin.to_path_buf(), root).await
}

/// One smoke node. Holds its tempdir and its child process — keep it alive for
/// the whole test, and let it drop to kill the node (`kill_on_drop`).
pub struct TestNode {
    bin: PathBuf,
    root: tempfile::TempDir,
    child: tokio::process::Child,
    addr: std::net::SocketAddr,
    node_key: [u8; 32],
    domains: Vec<String>,
}

impl TestNode {
    async fn boot(bin: PathBuf, root: tempfile::TempDir) -> TestNode {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let mut child = tokio::process::Command::new(&bin)
            .arg("--root")
            .arg(root.path())
            .args(["--listen", "127.0.0.1:0"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // The harness rule: a panicking test must not leave a listener
            // behind for the next one to collide with.
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("spawning {}: {e}", bin.display()));

        tee(child.stderr.take().expect("piped stderr"));

        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout).lines();
        let mut node_key = None;
        let mut domains = Vec::new();
        let addr = tokio::time::timeout(NODE_BOOT_DEADLINE, async {
            while let Some(line) = reader.next_line().await.expect("reading node stdout") {
                eprintln!("[hive-node] {line}");
                if let Some(hex) = line.strip_prefix("node key ") {
                    node_key = Some(parse_key_hex(hex));
                } else if let Some(domain) = line.strip_prefix("domain ") {
                    domains.push(domain.to_string());
                } else if let Some(addr) = line.strip_prefix("listening on ") {
                    return addr.parse().expect("an announced address");
                }
            }
            panic!("hive-node exited before it announced a port");
        })
        .await
        .unwrap_or_else(|_| panic!("hive-node never announced a port in {NODE_BOOT_DEADLINE:?}"));

        // Whatever the node says afterwards belongs in the log, not in a
        // buffer nobody drains (a full pipe would wedge it).
        tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                eprintln!("[hive-node] {line}");
            }
        });

        TestNode {
            bin,
            root,
            child,
            addr,
            node_key: node_key.expect("the node announces the key a device pins"),
            domains,
        }
    }

    /// Where the listener is, from the node's own `listening on` line. Never a
    /// fixed port — the tier binds `:0` and reads what it got.
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// The node's ed25519 public key, from its `node key` line: what a device
    /// pins, and what a scenario checks a handshake against.
    pub fn node_key(&self) -> [u8; 32] {
        self.node_key
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// The `<tenant>/<domain>` lines the node announced at boot.
    pub fn domains(&self) -> &[String] {
        &self.domains
    }

    /// Run a one-shot `hive-node` subcommand against this node's root, the way
    /// an operator does beside a running node. Panics if the binary cannot be
    /// run at all; a non-zero exit is returned for the caller to assert on.
    pub async fn run(&self, args: &[&str]) -> std::process::Output {
        let out = tokio::process::Command::new(&self.bin)
            .args(args)
            .arg("--root")
            .arg(self.root.path())
            .output()
            .await
            .unwrap_or_else(|e| panic!("running hive-node {args:?}: {e}"));
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            eprintln!("[hive-node {}] {line}", args.first().unwrap_or(&""));
        }
        out
    }

    /// `hive-node enroll --domain <domain>` — the one-time code, as printed.
    pub async fn mint_code(&self, domain: &str) -> String {
        let out = self.run(&["enroll", "--domain", domain]).await;
        assert!(
            out.status.success(),
            "hive-node enroll failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout_field(&out, "enrollment code ")
            .unwrap_or_else(|| panic!("no `enrollment code` line: {out:?}"))
    }

    /// `hive-node device revoke` — the control epoch it left behind.
    pub async fn revoke(&self, domain: &str, device: &str) -> u64 {
        let out = self
            .run(&["device", "revoke", "--domain", domain, "--device", device])
            .await;
        assert!(
            out.status.success(),
            "hive-node device revoke failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout_field(&out, "control epoch ")
            .unwrap_or_else(|| panic!("no `control epoch` line: {out:?}"))
            .parse()
            .expect("an epoch")
    }

    /// `hive-node device list` — one `<id> <pin> active|revoked` line each.
    pub async fn device_list(&self, domain: &str) -> Vec<String> {
        let out = self.run(&["device", "list", "--domain", domain]).await;
        assert!(
            out.status.success(),
            "hive-node device list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix("device ").map(str::to_string))
            .collect()
    }

    /// One domain's `node-meta.db`. Plain SQLite by design (a blind node holds
    /// no key, so its bookkeeping is the operator's to read) — which is what
    /// makes the two affordances below possible without a production knob.
    pub fn meta_db(&self, domain: &str) -> PathBuf {
        let (tenant, name) = domain
            .split_once('/')
            .unwrap_or_else(|| panic!("{domain:?} is not <tenant>/<domain>"));
        self.root
            .path()
            .join("tenants")
            .join(tenant)
            .join("domains")
            .join(name)
            .join("node-meta.db")
    }

    /// Age every outstanding enrollment code in `domain` past its TTL,
    /// returning how many were aged.
    ///
    /// The alternative is waiting ten minutes for the real one, and a smoke
    /// tier with a 60s cap cannot. The node's own clock is untouched: what
    /// moves is the recorded expiry, which is the same fact the TTL is made
    /// of. (The TTL arithmetic itself is unit-tested against an injected clock
    /// in `hive_node::enroll`.)
    pub fn expire_codes(&self, domain: &str) -> usize {
        let db = self.meta_db(domain);
        let conn = rusqlite::Connection::open(&db)
            .unwrap_or_else(|e| panic!("opening {}: {e}", db.display()));
        conn.execute("UPDATE enroll_codes SET expires_at = 0", [])
            .unwrap_or_else(|e| panic!("expiring codes in {}: {e}", db.display()))
    }

    /// The domain's auth-audit trail as `(kind, detail)`, oldest first — the
    /// operator-facing evidence every enrollment scenario asserts against.
    pub fn auth_events(&self, domain: &str) -> Vec<(String, String)> {
        let db = self.meta_db(domain);
        let conn = rusqlite::Connection::open(&db)
            .unwrap_or_else(|e| panic!("opening {}: {e}", db.display()));
        let mut stmt = conn
            .prepare("SELECT kind, detail FROM auth_audit ORDER BY id")
            .unwrap_or_else(|e| panic!("reading the audit trail in {}: {e}", db.display()));
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or_else(|e| panic!("reading the audit trail in {}: {e}", db.display()));
        rows.map(|r| r.expect("an audit row")).collect()
    }

    /// SIGTERM, then the exit status. Not `Child::kill` — that is SIGKILL,
    /// which would prove nothing about a clean shutdown.
    #[cfg(unix)]
    pub async fn terminate(mut self) -> std::process::ExitStatus {
        self.stop().await
    }

    /// Stop and start again on the SAME root, keeping the tempdir alive.
    ///
    /// What a reboot is for, in this tier: everything a node remembers —
    /// pins, revocation tombstones, the control epoch — lives in node-meta and
    /// nowhere else, so "it survived a restart" is the only honest way to say
    /// it was persisted. The port is NOT preserved: `:0` means a new one, and
    /// the returned node has re-read its own `listening on` line.
    #[cfg(unix)]
    pub async fn reboot(mut self) -> TestNode {
        let status = self.stop().await;
        assert!(status.success(), "a reboot starts from a clean exit");
        TestNode::boot(self.bin, self.root).await
    }

    #[cfg(unix)]
    async fn stop(&mut self) -> std::process::ExitStatus {
        let pid = self.child.id().expect("a running child");
        let status = tokio::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await
            .expect("sending SIGTERM");
        assert!(status.success(), "kill -TERM {pid} failed");
        tokio::time::timeout(NODE_BOOT_DEADLINE, self.child.wait())
            .await
            .expect("hive-node did not exit after SIGTERM")
            .expect("waiting for hive-node")
    }
}

/// The node's own key line and every other 64-hex field share one parser, so a
/// malformed one fails here rather than three assertions later.
fn parse_key_hex(hex: &str) -> [u8; 32] {
    let raw = data_encoding::HEXLOWER
        .decode(hex.trim().as_bytes())
        .unwrap_or_else(|e| panic!("node key {hex:?} is not lowercase hex: {e}"));
    raw.try_into()
        .unwrap_or_else(|v: Vec<u8>| panic!("node key is {} bytes, want 32", v.len()))
}

/// The value after `prefix` on the one stdout line that carries it.
fn stdout_field(out: &std::process::Output, prefix: &str) -> Option<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix(prefix).map(str::to_string))
}

/// Tee a child's pipe into the test log, so a failure here reads like a
/// failure there (a harness rule, docs/TESTING-STRATEGY.md §4.1).
fn tee(pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[hive-node] {line}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tier_gate_is_off_unless_deliberately_armed() {
        assert!(!enabled(None), "unset means off — that is the whole point");
        assert!(!enabled(Some("")));
        assert!(!enabled(Some("0")));
        assert!(!enabled(Some("off")));
        assert!(enabled(Some("1")));
        assert!(enabled(Some(" 1 ")), "trimmed like the app's flag parsing");
        assert!(enabled(Some("true")));
    }

    /// KEY-INJECT anti-drift: the file this seam writes is the file hive-core
    /// reads. If either side ever invents its own encoding, this fails.
    #[test]
    fn the_master_key_file_round_trips_through_cores_parser() {
        let domain = test_domain_with_master([0xab; 32]);
        let read = hive_core::keys::read_master_key_file(&domain.master_key_file())
            .expect("core parses the file this seam writes");
        assert_eq!(read, domain.master_key());
        assert_eq!(read, [0xab; 32]);

        let raw = std::fs::read_to_string(domain.master_key_file()).unwrap();
        assert_eq!(raw.trim().len(), 64, "64 hex chars, nothing else");
    }

    #[tokio::test]
    async fn devices_open_under_the_domain_master_and_stay_separate() {
        let domain = test_domain();
        let alpha = domain.open_device("alpha");
        let beta = domain.open_device("beta");
        assert_ne!(
            alpha.device(),
            beta.device(),
            "two device dirs mint two device ids"
        );
        assert_eq!(alpha.data_dir(), domain.device_dir("alpha"));

        alpha.topics_ensure("Bees").await.unwrap();
        assert!(
            beta.topics_list().await.unwrap().is_empty(),
            "no replication yet: a device only sees its own writes"
        );
        alpha.shutdown().await.unwrap();
        beta.shutdown().await.unwrap();
    }

    /// The identity seam is deterministic in the name and separate per device
    /// — the two properties every pinning scenario leans on.
    #[test]
    fn named_device_identities_are_stable_and_distinct() {
        let alpha = test_identity("alpha");
        assert_eq!(
            alpha.ed25519_public(),
            test_identity("alpha").ed25519_public(),
            "same name, same device, every run"
        );
        assert_ne!(
            alpha.ed25519_public(),
            test_identity("beta").ed25519_public()
        );
        assert_eq!(
            test_domain().device_identity("alpha").ed25519_public(),
            alpha.ed25519_public(),
            "the domain method is the same seam"
        );
    }

    /// A store opened by the seam reopens on the same dir under the same key —
    /// the property every restore/reboot scenario leans on.
    #[tokio::test]
    async fn a_device_dir_reopens_under_the_same_domain_key() {
        let domain = test_domain();
        let store = domain.open_device("alpha");
        let device = store.device().to_string();
        store.topics_ensure("Bees").await.unwrap();
        store.shutdown().await.unwrap();

        let store = domain.open_device("alpha");
        assert_eq!(store.device(), device);
        assert_eq!(store.topics_list().await.unwrap().len(), 1);
        store.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wait_until_returns_as_soon_as_the_condition_holds() {
        let mut polls = 0;
        wait_until_within(
            "a counter to reach 3",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                polls += 1;
                std::future::ready(polls >= 3)
            },
        )
        .await;
        assert_eq!(polls, 3, "it stops polling the moment it holds");
    }

    #[tokio::test]
    #[should_panic(expected = "timed out")]
    async fn wait_until_names_what_it_was_waiting_for_when_it_gives_up() {
        wait_until_within(
            "something that never happens",
            Duration::from_millis(30),
            Duration::from_millis(5),
            || std::future::ready(false),
        )
        .await;
    }

    #[test]
    fn a_binary_path_env_resolves_or_skips() {
        let var = "HIVE_SMOKE_SUPPORT_BIN_PATH_TEST"; // unique: env is process-global
        assert!(bin_path(var).is_none(), "unset → the caller skips loudly");

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("pretend-binary");
        std::fs::write(&bin, b"#!/bin/true\n").unwrap();
        std::env::set_var(var, &bin);
        assert_eq!(bin_path(var), Some(bin));
        std::env::remove_var(var);
    }
}
