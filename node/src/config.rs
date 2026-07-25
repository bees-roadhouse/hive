// The node's configuration files (PLAN-v2.1 PR 4.6, D33/D35): `node.toml` at
// the root and one `tenant.toml` per tenant. Both are deliberately small, and
// what they leave OUT is the design:
//
//   * no IdP, KMS, or console fields (D33 — enterprise custody is designed on
//     paper, deferred to tenant 2; a schema with zero consumers is exactly the
//     speculation the closed-set discipline exists to prevent);
//   * no per-domain key source (D29 — Stage A is the BLIND tier: the node
//     holds no master key at all, and the trusted tier's `FileKeySource`
//     arrives with PR 4.13);
//   * no interface enumeration knobs. The addresses a node advertises are
//     declared, one per interface class, because "which of my addresses is the
//     public one" is a question about the deployment, not about the host.
//
// A tenant is grouping plus quotas plus console scope (D33). The data plane
// never learns any of it: `tenants/<t>/domains/<d>/` is a path this crate
// computes, and everything below it is ordinary store-shape bytes — which is
// what the tenancy GREP-FENCE (`tests/fences.rs`) mechanically enforces
// against core/.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The node's config file, looked for at `<root>/node.toml`.
pub const CONFIG_FILE: &str = "node.toml";

/// A tenant's config file, at `<root>/tenants/<tenant>/tenant.toml`.
pub const TENANT_FILE: &str = "tenant.toml";

/// The node key file, at `<root>/node.key` unless `node_key_file` says
/// otherwise: 64 hex chars, the ONE key-file format in this tree
/// (`hive_core::keys::parse_master_key_hex`, shared with the master-key seam
/// and the smoke tier). It is a device SEED, not a master — a blind node's
/// only secret, and it decrypts nothing.
pub const NODE_KEY_FILE: &str = "node.key";

/// Where the node listens when nothing says otherwise. The number is
/// arbitrary on purpose: the SRV record is what makes a node findable (D35),
/// so the port is a deployment detail rather than a protocol constant, and
/// nothing in the tree assumes it.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:7847";

/// TTL for published records. Short, because address changes are exactly what
/// the publisher exists to chase (D35: re-publish on start and on change).
pub const DEFAULT_DNS_TTL: u32 = 60;

/// `node.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Address the replication listener binds. The CLI's `--listen` overrides
    /// it (the smoke tier always binds `127.0.0.1:0`).
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// The node key's 64-hex seed file, relative to the root unless absolute.
    /// Absent means `<root>/node.key`, minted on first boot.
    #[serde(default)]
    pub node_key_file: Option<PathBuf>,
    /// The optional DNS publisher (D35). Absent = publish nothing, which is
    /// also what `provider = "none"` means; both are legitimate (an overlay
    /// network with its own name service, or a zone someone edits by hand).
    #[serde(default)]
    pub dns: Option<DnsConfig>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            listen: default_listen(),
            node_key_file: None,
            dns: None,
        }
    }
}

fn default_listen() -> SocketAddr {
    DEFAULT_LISTEN
        .parse()
        .expect("DEFAULT_LISTEN is a literal socket address")
}

impl NodeConfig {
    /// Parse one `node.toml`.
    pub fn parse(text: &str) -> Result<NodeConfig> {
        let config: NodeConfig = toml::from_str(text).context("parsing node.toml")?;
        config.validate()?;
        Ok(config)
    }

    /// Load `<root>/node.toml`, or the defaults if there is no such file. A
    /// node with no config is a legitimate node — it listens on the default
    /// port, mints its key, and publishes nothing — and that is what makes
    /// `hive-node --root <dir>` a complete command.
    pub fn load(root: &Path) -> Result<NodeConfig> {
        let path = root.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => NodeConfig::parse(&text)
                .with_context(|| format!("reading node config {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(NodeConfig::default()),
            Err(e) => Err(e).with_context(|| format!("reading node config {}", path.display())),
        }
    }

    /// The node key file this config selects, resolved against the root.
    pub fn node_key_path(&self, root: &Path) -> PathBuf {
        match &self.node_key_file {
            Some(p) if p.is_absolute() => p.clone(),
            Some(p) => root.join(p),
            None => root.join(NODE_KEY_FILE),
        }
    }

    fn validate(&self) -> Result<()> {
        if let Some(dns) = &self.dns {
            dns.validate()?;
        }
        Ok(())
    }
}

/// The `[dns]` block: what to publish, where, and with which credential.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsConfig {
    pub provider: DnsProvider,
    /// The zone the SRV record lives under — `_hive._tcp.<zone>`.
    pub zone: String,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    /// SRV port override. Defaults to the port the listener actually bound,
    /// which is the right answer everywhere except behind a port-mapping NAT
    /// — where the world's port and the socket's port genuinely differ.
    #[serde(default)]
    pub port: Option<u16>,
    /// One entry per interface class this node is reachable on. Empty is
    /// legal and means "publish the SRV shape, no addresses yet" — a node
    /// that has not been told where it lives publishes nothing rather than
    /// guessing.
    #[serde(default)]
    pub address: Vec<AdvertisedAddress>,
    /// Credential + target for `provider = "cloudflare"`.
    #[serde(default)]
    pub cloudflare: Option<CloudflareConfig>,
    /// Credential + target for `provider = "rfc2136"`.
    #[serde(default)]
    pub rfc2136: Option<Rfc2136Config>,
}

fn default_ttl() -> u32 {
    DEFAULT_DNS_TTL
}

impl DnsConfig {
    fn validate(&self) -> Result<()> {
        if self.zone.trim().is_empty() {
            bail!("[dns] zone must not be empty");
        }
        if self.zone.starts_with('.') || self.zone.ends_with('.') {
            // The publisher composes names by concatenation, and a stray dot
            // is how a credential scoped to `<zone>` ends up asked to write
            // `_hive._tcp..example.com`. Refuse it at the config, once.
            bail!(
                "[dns] zone {:?} must be a bare zone name, without leading or trailing dots",
                self.zone
            );
        }
        match self.provider {
            DnsProvider::None => {}
            DnsProvider::Cloudflare => {
                if self.cloudflare.is_none() {
                    bail!("[dns] provider = \"cloudflare\" needs a [dns.cloudflare] table");
                }
            }
            DnsProvider::Rfc2136 => {
                if self.rfc2136.is_none() {
                    bail!("[dns] provider = \"rfc2136\" needs a [dns.rfc2136] table");
                }
            }
        }
        let mut seen: Vec<AddressClass> = Vec::new();
        for addr in &self.address {
            if seen.contains(&addr.class) {
                // One address per class is what makes an upsert idempotent:
                // two entries for the same class would fight over the same
                // record name on every publish.
                bail!(
                    "[dns] interface class {} is declared twice — one address per class",
                    addr.class.as_str()
                );
            }
            seen.push(addr.class);
        }
        Ok(())
    }
}

/// Which zone-writing rail the publisher uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsProvider {
    /// RFC 2136 dynamic update with a TSIG key — the standard path (D35).
    #[serde(rename = "rfc2136")]
    Rfc2136,
    /// The Cloudflare API — the pragmatic path (D35).
    Cloudflare,
    /// Publish nothing. The honest option for overlay-native name services
    /// (MagicDNS, ZeroNS) and hand-edited zones.
    None,
}

/// `[dns.cloudflare]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareConfig {
    /// The zone's Cloudflare id (the API addresses zones by id, not name).
    pub zone_id: String,
    /// File holding the API token. A file, not an inline string, so the
    /// credential is not in the config someone pastes into a chat window —
    /// and so it can be a mounted secret. The token itself must be
    /// record-scoped to the names in `dns::allowed_names` (delta 13): this
    /// crate refuses to write anything else, but only the token stops a
    /// stolen node from rewriting MX.
    pub api_token_file: PathBuf,
    /// API base, overridable so tests can point at a loopback mock (no live
    /// DNS in CI, ever).
    #[serde(default = "default_cloudflare_api")]
    pub api_base: String,
}

/// Cloudflare's v4 API root.
pub const CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";

fn default_cloudflare_api() -> String {
    CLOUDFLARE_API_BASE.to_string()
}

/// `[dns.rfc2136]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rfc2136Config {
    /// The primary nameserver that accepts UPDATE, `host:port`.
    pub server: String,
    /// TSIG key name, e.g. `hive-update.`.
    pub key_name: String,
    /// TSIG algorithm, e.g. `hmac-sha256`.
    #[serde(default = "default_tsig_algorithm")]
    pub key_algorithm: String,
    /// File holding the base64 TSIG secret (a file, for the same reason the
    /// Cloudflare token is one). The server's update-policy must be limited
    /// to `dns::allowed_names` (delta 13).
    pub key_file: PathBuf,
}

/// The one TSIG algorithm worth defaulting to.
pub const DEFAULT_TSIG_ALGORITHM: &str = "hmac-sha256";

fn default_tsig_algorithm() -> String {
    DEFAULT_TSIG_ALGORITHM.to_string()
}

/// One address the node advertises, and which kind of network it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvertisedAddress {
    pub class: AddressClass,
    pub ip: IpAddr,
}

/// The interface classes D35 names. They are separate records rather than one
/// multi-valued name because the dialer races them as distinct candidates and
/// caches a per-network ranking — "which address won" is only a useful memory
/// if the addresses have names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AddressClass {
    Public,
    Tailnet,
    Zerotier,
    Lan,
}

impl AddressClass {
    /// The label that becomes the record's leftmost label.
    pub fn as_str(self) -> &'static str {
        match self {
            AddressClass::Public => "public",
            AddressClass::Tailnet => "tailnet",
            AddressClass::Zerotier => "zerotier",
            AddressClass::Lan => "lan",
        }
    }

    /// Every class, in the order records are published — deterministic, so a
    /// publish plan is comparable between runs and diffable in a test.
    pub const ALL: [AddressClass; 4] = [
        AddressClass::Public,
        AddressClass::Tailnet,
        AddressClass::Zerotier,
        AddressClass::Lan,
    ];
}

/// `tenant.toml` — name, tier, and quotas, and nothing else (D33).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    /// Human label for the console and the logs. Never a key, never a path:
    /// the directory name is the tenant's identity.
    pub name: String,
    #[serde(default)]
    pub tier: Tier,
    #[serde(default)]
    pub quotas: Quotas,
}

impl TenantConfig {
    pub fn parse(text: &str) -> Result<TenantConfig> {
        toml::from_str(text).context("parsing tenant.toml")
    }

    /// Load `<tenant_dir>/tenant.toml`. A tenant directory with no config is
    /// a blind tenant named after its directory — the household case, where
    /// the file would carry no information the path does not.
    pub fn load(tenant_dir: &Path, tenant: &str) -> Result<TenantConfig> {
        let path = tenant_dir.join(TENANT_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => TenantConfig::parse(&text)
                .with_context(|| format!("reading tenant config {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TenantConfig {
                name: tenant.to_string(),
                tier: Tier::Blind,
                quotas: Quotas::default(),
            }),
            Err(e) => Err(e).with_context(|| format!("reading tenant config {}", path.display())),
        }
    }
}

/// Blind or trusted (D29). v2.1 Stage A ships blind only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// The node holds no key and folds nothing: verbatim ciphertext in,
    /// verbatim ciphertext out.
    #[default]
    Blind,
    /// The node holds the domain master and runs a Store. PR 4.13.
    Trusted,
}

/// Per-tenant quotas. Parsed here, enforced at replication ingest by PR 4.9 —
/// the field exists now so the file shape does not change under an operator
/// when enforcement lands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quotas {
    /// Ceiling on one domain's stored bytes. `None` is unlimited.
    #[serde(default)]
    pub domain_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_config_is_a_complete_config() {
        let config = NodeConfig::parse("").unwrap();
        assert_eq!(config, NodeConfig::default());
        assert_eq!(config.listen.port(), 7847);
        assert!(config.dns.is_none(), "publishing is opt-in");
        assert_eq!(
            config.node_key_path(Path::new("/srv/hive")),
            PathBuf::from("/srv/hive/node.key")
        );
    }

    #[test]
    fn the_listen_address_and_key_file_come_from_the_file() {
        let config = NodeConfig::parse(
            r#"
            listen = "127.0.0.1:9000"
            node_key_file = "keys/node.key"
            "#,
        )
        .unwrap();
        assert_eq!(config.listen, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(
            config.node_key_path(Path::new("/srv/hive")),
            PathBuf::from("/srv/hive/keys/node.key")
        );
    }

    #[test]
    fn an_absolute_key_file_is_left_alone() {
        let config = NodeConfig::parse("node_key_file = \"/run/secrets/node.key\"").unwrap();
        assert_eq!(
            config.node_key_path(Path::new("/srv/hive")),
            PathBuf::from("/run/secrets/node.key")
        );
    }

    #[test]
    fn a_typo_is_a_refusal_not_a_default() {
        let err = NodeConfig::parse("listne = \"127.0.0.1:9000\"").unwrap_err();
        assert!(
            format!("{err:#}").contains("listne"),
            "deny_unknown_fields names the typo: {err:#}"
        );
    }

    #[test]
    fn the_dns_block_parses_with_classes_and_a_provider_table() {
        let config = NodeConfig::parse(
            r#"
            [dns]
            provider = "cloudflare"
            zone = "bierlysmith.com"
            ttl = 120
            port = 443

            [dns.cloudflare]
            zone_id = "zone-1"
            api_token_file = "cloudflare.token"

            [[dns.address]]
            class = "public"
            ip = "203.0.113.10"

            [[dns.address]]
            class = "tailnet"
            ip = "fd7a:115c:a1e0::1"
            "#,
        )
        .unwrap();
        let dns = config.dns.expect("a [dns] block");
        assert_eq!(dns.provider, DnsProvider::Cloudflare);
        assert_eq!(dns.zone, "bierlysmith.com");
        assert_eq!(dns.ttl, 120);
        assert_eq!(dns.port, Some(443));
        assert_eq!(dns.address.len(), 2);
        assert_eq!(dns.address[0].class, AddressClass::Public);
        assert_eq!(dns.address[1].class, AddressClass::Tailnet);
        assert_eq!(
            dns.cloudflare.unwrap().api_base,
            "https://api.cloudflare.com/client/v4"
        );
    }

    #[test]
    fn the_ttl_defaults_short_because_addresses_move() {
        let config = NodeConfig::parse(
            r#"
            [dns]
            provider = "none"
            zone = "bierlysmith.com"
            "#,
        )
        .unwrap();
        assert_eq!(config.dns.unwrap().ttl, DEFAULT_DNS_TTL);
    }

    #[test]
    fn a_provider_without_its_credential_table_is_refused() {
        for (provider, table) in [
            ("cloudflare", "[dns.cloudflare]"),
            ("rfc2136", "[dns.rfc2136]"),
        ] {
            let err = NodeConfig::parse(&format!(
                "[dns]\nprovider = \"{provider}\"\nzone = \"bierlysmith.com\"\n"
            ))
            .unwrap_err();
            assert!(
                format!("{err:#}").contains(table),
                "{provider} must name the table it needs: {err:#}"
            );
        }
    }

    #[test]
    fn the_rfc2136_table_carries_a_tsig_key_and_defaults_its_algorithm() {
        let config = NodeConfig::parse(
            r#"
            [dns]
            provider = "rfc2136"
            zone = "bierlysmith.com"

            [dns.rfc2136]
            server = "192.0.2.53:53"
            key_name = "hive-update."
            key_file = "tsig.key"
            "#,
        )
        .unwrap();
        let rfc = config.dns.unwrap().rfc2136.unwrap();
        assert_eq!(rfc.server, "192.0.2.53:53");
        assert_eq!(rfc.key_algorithm, DEFAULT_TSIG_ALGORITHM);
    }

    /// A dotted zone would compose into `_hive._tcp..example.com` — a name
    /// outside every credential scope, discovered at publish time on a box
    /// nobody is watching. Refuse it while someone is reading the error.
    #[test]
    fn a_zone_with_stray_dots_is_refused() {
        for zone in ["", ".bierlysmith.com", "bierlysmith.com."] {
            let err =
                NodeConfig::parse(&format!("[dns]\nprovider = \"none\"\nzone = \"{zone}\"\n"))
                    .unwrap_err();
            assert!(format!("{err:#}").contains("zone"), "{err:#}");
        }
    }

    #[test]
    fn one_address_per_interface_class() {
        let err = NodeConfig::parse(
            r#"
            [dns]
            provider = "none"
            zone = "bierlysmith.com"

            [[dns.address]]
            class = "lan"
            ip = "192.168.1.10"

            [[dns.address]]
            class = "lan"
            ip = "192.168.1.11"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("declared twice"), "{err:#}");
    }

    #[test]
    fn a_tenant_carries_name_tier_and_quotas() {
        let tenant = TenantConfig::parse(
            r#"
            name = "Household"
            tier = "blind"

            [quotas]
            domain_bytes = 107374182400
            "#,
        )
        .unwrap();
        assert_eq!(tenant.name, "Household");
        assert_eq!(tenant.tier, Tier::Blind);
        assert_eq!(tenant.quotas.domain_bytes, Some(107374182400));
    }

    /// D33 in one assertion: the enterprise fields are designed on paper and
    /// absent from the file. A config that carried them would be a schema
    /// with no consumer, which is the speculation this refuses to ship.
    #[test]
    fn a_tenant_may_not_carry_idp_kms_or_console_fields() {
        for field in [
            "oidc_issuer = \"https://id.example.com\"",
            "kms_key = \"arn:aws:kms:...\"",
            "console_url = \"https://console.example.com\"",
        ] {
            let err = TenantConfig::parse(&format!("name = \"Household\"\n{field}\n")).unwrap_err();
            assert!(
                format!("{err:#}").contains("unknown field"),
                "{field} must be refused, not ignored: {err:#}"
            );
        }
    }

    #[test]
    fn a_tenant_dir_with_no_file_is_a_blind_tenant_named_after_itself() {
        let dir = tempfile::tempdir().unwrap();
        let tenant = TenantConfig::load(dir.path(), "household").unwrap();
        assert_eq!(tenant.name, "household");
        assert_eq!(tenant.tier, Tier::Blind);
        assert_eq!(tenant.quotas.domain_bytes, None);
    }

    #[test]
    fn a_root_with_no_node_toml_loads_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(NodeConfig::load(dir.path()).unwrap(), NodeConfig::default());

        std::fs::write(dir.path().join(CONFIG_FILE), "listen = \"127.0.0.1:1\"\n").unwrap();
        assert_eq!(
            NodeConfig::load(dir.path()).unwrap().listen,
            "127.0.0.1:1".parse().unwrap()
        );
    }
}
