// The node itself (PLAN-v2.1 PR 4.6): a root directory, a key, some vaults,
// and a socket.
//
// `Node::open` is deliberately synchronous and runs BEFORE the tokio runtime
// exists — the custody ordering every binary in this tree follows (the
// keyring backend drives its own executor and panics on a tokio thread;
// app/src/main.rs and importer/src/main.rs order it the same way). A node's
// key is a file today, but the rule is about where key resolution HAPPENS,
// not about which source answered.
//
// What boots here, and what deliberately does not:
//
//   * The vault tree is DISCOVERED, not declared: `tenants/*/domains/*` is
//     scanned at open. That is what makes "rsync a vault directory onto a
//     second box and start a node" work (D36's cheap multi-node story) and
//     what keeps `node.toml` from carrying a second copy of the truth.
//   * Binding is HERE; serving is [`crate::listen`] (PR 4.7). The split is the
//     boot contract's: a `Bound` exists so the ACTUAL port can be announced
//     before a single connection is accepted, which is what makes
//     `--listen 127.0.0.1:0` a usable thing for a harness to parse.
//   * Nothing here holds a master key, opens a Store, or folds a record. The
//     blind tier is the whole product at this stage (D29).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use hive_core::identity::DeviceIdentity;
use tokio::net::TcpListener;

use crate::config::{NodeConfig, TenantConfig, Tier};
use crate::dns::DnsPublisher;
use crate::vault::{name_ok, SegmentVault, DOMAINS_DIR, TENANTS_DIR};

/// A booted node: its root, its key, and every domain vault it holds.
pub struct Node {
    root: PathBuf,
    config: NodeConfig,
    identity: DeviceIdentity,
    vaults: Vec<SegmentVault>,
}

impl std::fmt::Debug for Node {
    /// The public half of the node key only — `DeviceIdentity`'s own Debug
    /// keeps the seed out of every log line, and this must not undo that.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("root", &self.root)
            .field("node_key", &self.node_key_hex())
            .field("vaults", &self.vaults.len())
            .finish_non_exhaustive()
    }
}

impl Node {
    /// Open a node root: read `node.toml`, resolve the node key (minting one
    /// on first boot), and open every vault under `tenants/`.
    ///
    /// Synchronous, and called before the runtime — see the module header.
    pub fn open(root: impl Into<PathBuf>) -> Result<Node> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating node root {}", root.display()))?;
        let config = NodeConfig::load(&root)?;
        Node::open_with_config(root, config)
    }

    /// `open` with a config already in hand (the CLI, which applies its own
    /// overrides first).
    pub fn open_with_config(root: impl Into<PathBuf>, config: NodeConfig) -> Result<Node> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating node root {}", root.display()))?;
        let identity = resolve_node_key(&config.node_key_path(&root))?;
        let vaults = open_vaults(&root)?;
        Ok(Node {
            root,
            config,
            identity,
            vaults,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// The node's transport identity — the key a device pins at enrollment
    /// (PR 4.7) and the only secret a blind node has. It encrypts nothing.
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// The node key's public half, lowercase hex: what an operator reads off
    /// one box and compares on another, and what the boot line prints.
    pub fn node_key_hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.identity.ed25519_public())
    }

    /// Every vault, ordered by (tenant, domain).
    pub fn vaults(&self) -> &[SegmentVault] {
        &self.vaults
    }

    pub fn vault(&self, tenant: &str, domain: &str) -> Option<&SegmentVault> {
        self.vaults
            .iter()
            .find(|v| v.tenant() == tenant && v.domain() == domain)
    }

    /// Open (creating if absent) one domain's vault. Enrollment (PR 4.7) is
    /// what calls this in anger; until then it is how an operator's first
    /// domain comes into being.
    pub fn open_vault(&mut self, tenant: &str, domain: &str) -> Result<SegmentVault> {
        if let Some(existing) = self.vault(tenant, domain) {
            return Ok(existing.clone());
        }
        let vault = SegmentVault::open(&self.root, tenant, domain)?;
        self.vaults.push(vault.clone());
        self.vaults
            .sort_by(|a, b| (a.tenant(), a.domain()).cmp(&(b.tenant(), b.domain())));
        Ok(vault)
    }

    /// Bind the replication listener. Split from `serve` so the caller can
    /// announce the ACTUAL port before accepting — `--listen 127.0.0.1:0` is
    /// how every test binds, and the port only exists after the bind.
    ///
    /// The bind-scope check happens HERE and not only at config parse, because
    /// `--listen` overrides the file: the address that reaches the socket is
    /// the one that has to satisfy the scope (PR 4.7).
    pub async fn bind(&self, addr: SocketAddr) -> Result<Bound> {
        self.config.check_bind(addr)?;
        let tcp = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding the replication listener to {addr}"))?;
        let local = tcp
            .local_addr()
            .context("reading the bound listener address")?;
        Ok(Bound { tcp, local })
    }

    /// Publish this node's addresses, if `node.toml` asked for it. A publish
    /// failure is a warning, never a boot failure: DNS is addressing, and a
    /// node that cannot advertise itself still serves every device that
    /// already knows where it is (D35).
    pub async fn publish_dns(&self, bound_port: u16) -> Result<()> {
        let Some(dns) = &self.config.dns else {
            return Ok(());
        };
        // A config the node cannot honour IS a boot failure — it is the one
        // moment someone is watching.
        let Some(publisher) = DnsPublisher::from_config(dns, &self.root, bound_port)? else {
            return Ok(());
        };
        match publisher.publish(&dns.address).await {
            Ok(report) => tracing::info!(
                zone = publisher.zone(),
                via = publisher.describe(),
                upserted = report.upserted.len(),
                unchanged = report.unchanged.len(),
                "published node addresses"
            ),
            Err(e) => tracing::warn!(
                zone = publisher.zone(),
                "could not publish node addresses (devices that already know this node are \
                 unaffected): {e:#}"
            ),
        }
        Ok(())
    }
}

/// A bound listener, before anything is served on it.
#[derive(Debug)]
pub struct Bound {
    tcp: TcpListener,
    local: SocketAddr,
}

impl Bound {
    /// The address actually bound — with `:0` this is the only way to learn
    /// the port, and it is what the boot line announces.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The socket itself, for the accept loop in [`crate::listen`]. Consuming
    /// keeps "bound" and "serving" from being two live handles on one port.
    pub(crate) fn into_listener(self) -> TcpListener {
        self.tcp
    }
}

/// Read the node key, or mint one on first boot. The file is the ONE key-file
/// format in this tree — 64 hex chars, one raw 32-byte seed — shared with the
/// master-key seam and the smoke tier, so an operator learns it once.
fn resolve_node_key(path: &Path) -> Result<DeviceIdentity> {
    match hive_core::identity::read_device_seed_file(path) {
        Ok(identity) => Ok(identity),
        Err(e) if missing(&e) => {
            let identity = DeviceIdentity::generate();
            let hex = data_encoding::HEXLOWER.encode(&identity.seed());
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(path, format!("{hex}\n"))
                .with_context(|| format!("writing the node key {}", path.display()))?;
            restrict(path)?;
            tracing::info!(path = %path.display(), "minted a node key");
            Ok(identity)
        }
        Err(e) => Err(e),
    }
}

/// Whether an error chain bottoms out in "no such file". The seed reader
/// wraps io errors in context, so the chain is what has to be asked.
fn missing(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))
}

/// Windows ACLs are not mode bits; the file inherits the directory's, and the
/// node root is the operator's to place.
#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

/// Scan `tenants/*/domains/*` and open every vault found. Directory names are
/// checked against the same allowlist that guards a vault path, so a tree
/// someone copied in cannot introduce a name this node would refuse to write.
fn open_vaults(root: &Path) -> Result<Vec<SegmentVault>> {
    let mut vaults = Vec::new();
    for tenant in read_names(&root.join(TENANTS_DIR))? {
        let tenant_dir = root.join(TENANTS_DIR).join(&tenant);
        let config = TenantConfig::load(&tenant_dir, &tenant)?;
        if config.tier == Tier::Trusted {
            // A trusted tenant needs a master key and a Store per domain
            // (PR 4.13). Booting it as blind would silently serve a tier the
            // operator did not ask for.
            bail!(
                "tenant {tenant:?} is configured as the trusted tier, which this build does not \
                 serve — the trusted tier lands with PLAN-v2.1 PR 4.13"
            );
        }
        for domain in read_names(&tenant_dir.join(DOMAINS_DIR))? {
            vaults.push(SegmentVault::open(root, &tenant, &domain)?);
        }
    }
    vaults.sort_by(|a, b| (a.tenant(), a.domain()).cmp(&(b.tenant(), b.domain())));
    Ok(vaults)
}

/// Sorted subdirectory names of `dir`, or none if it does not exist.
fn read_names(dir: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("stat {}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name_ok(&name) {
            bail!(
                "refusing to open {}: {name:?} is not a usable tenant/domain name",
                entry.path().display()
            );
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_root_mints_a_key_and_holds_no_vaults() {
        let root = tempfile::tempdir().unwrap();
        let node = Node::open(root.path()).unwrap();
        assert!(node.vaults().is_empty());
        assert_eq!(node.node_key_hex().len(), 64);
        assert!(root.path().join("node.key").is_file());
    }

    #[test]
    fn the_node_key_survives_a_reboot() {
        let root = tempfile::tempdir().unwrap();
        let first = Node::open(root.path()).unwrap().node_key_hex();
        let second = Node::open(root.path()).unwrap().node_key_hex();
        assert_eq!(first, second, "a device that re-keys is a different device");
    }

    #[test]
    fn vaults_are_discovered_by_scanning_the_tree() {
        let root = tempfile::tempdir().unwrap();
        {
            let mut node = Node::open(root.path()).unwrap();
            node.open_vault("household", "example.com").unwrap();
            node.open_vault("household", "maggie").unwrap();
        }
        // The second boot is told nothing: the tree is the declaration.
        let node = Node::open(root.path()).unwrap();
        assert_eq!(node.vaults().len(), 2);
        assert_eq!(node.vaults()[0].domain(), "example.com");
        assert_eq!(node.vaults()[1].domain(), "maggie");
        assert!(node.vault("household", "maggie").is_some());
        assert!(node.vault("household", "nobody").is_none());
    }

    #[test]
    fn a_directory_copied_in_is_a_domain() {
        // D36's cheap multi-node story: replicate the vault directory with
        // ordinary tools and start a node over it.
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(
            root.path()
                .join("tenants/household/domains/example.com/log/alpha"),
        )
        .unwrap();
        let node = Node::open(root.path()).unwrap();
        assert_eq!(node.vaults().len(), 1);
        assert_eq!(node.vaults()[0].tenant(), "household");
    }

    #[test]
    fn a_trusted_tenant_refuses_to_boot_as_blind() {
        let root = tempfile::tempdir().unwrap();
        let tenant = root.path().join("tenants/household");
        std::fs::create_dir_all(tenant.join("domains/example.com")).unwrap();
        std::fs::write(
            tenant.join("tenant.toml"),
            "name = \"Household\"\ntier = \"trusted\"\n",
        )
        .unwrap();
        let err = Node::open(root.path()).unwrap_err();
        assert!(format!("{err:#}").contains("PR 4.13"), "{err:#}");
    }

    #[tokio::test]
    async fn binding_zero_reports_the_port_it_actually_got() {
        let root = tempfile::tempdir().unwrap();
        let node = Node::open(root.path()).unwrap();
        let bound = node.bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        assert_ne!(
            bound.local_addr().port(),
            0,
            "the announced port is the bound one — the whole reason bind and serve are split"
        );
        // Serving is `listen::serve` (PR 4.7); its own tests drive real
        // handshakes through this socket.
    }

    /// An interface-scoped node refuses the wildcard at the BIND, not at the
    /// parse — `--listen` overrides node.toml, so the address that reaches the
    /// socket is the one that has to satisfy the scope (PR 4.7).
    #[tokio::test]
    async fn an_interface_scoped_node_refuses_a_wildcard_override() {
        let root = tempfile::tempdir().unwrap();
        let config = NodeConfig::parse("listen = \"127.0.0.1:0\"\nbind_scope = \"interface\"\n")
            .expect("a scoped config with a concrete address");
        let node = Node::open_with_config(root.path(), config).unwrap();

        let err = node.bind("0.0.0.0:0".parse().unwrap()).await.unwrap_err();
        assert!(format!("{err:#}").contains("wildcard"), "{err:#}");
        // ...and the address the config named still binds.
        assert!(node.bind("127.0.0.1:0".parse().unwrap()).await.is_ok());
    }

    #[tokio::test]
    async fn a_node_with_no_dns_block_publishes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let node = Node::open(root.path()).unwrap();
        node.publish_dns(7847).await.unwrap();
    }
}
