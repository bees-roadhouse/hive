// The DNS publisher (PLAN-v2.1 PR 4.6, D35 + threat-model delta 13): how a
// node says where it is, so pairing can collapse to "enter your address".
//
// What gets published, and nothing else:
//
//   _hive._tcp.<zone>        SRV, one target per interface class the node is
//                            reachable on (public, tailnet, ZeroTier, LAN)
//   <class>.hive.<zone>      A / AAAA, the node's own address on that class
//
// Multiple SRV targets and multiple addresses are the POINT, not an edge
// case: the dialer (PR 4.8) turns the whole record set into a candidate list
// and races it, because measurement decides which path is best, not a
// priority field (D35's documented RFC 2782 deviation). SRV priority survives
// here as an administrative override and tie-break only.
//
// The invariant this module exists to make mechanical:
//
//   **DNS is addressing, never authentication.**
//
// Nothing published here is trusted by anything. A spoofed answer, a hijacked
// zone, or a stolen credential degrades to a failed handshake and a skipped
// candidate, because the enrollment-pinned node key is what decides (delta
// 13). So the publisher's security job is not to protect what it writes — it
// is to write NOTHING ELSE. Every upsert is checked against
// [`allowed_names`] before a request is made, so a bug or a hostile config
// cannot turn the node's DNS credential into a way to rewrite MX. The
// credential itself must be scoped to exactly these names (a record-scoped
// API token, or an RFC 2136 update-policy limited to the same set); this
// crate enforces the CI-checkable half and the deployment enforces the rest.
//
// Providers are a trait (`ZoneApi`) with the record shaping above them, which
// is what lets the whole publisher be tested against a mock: no test in this
// tree performs a DNS lookup or an outbound request, ever.

use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;
use std::path::Path;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;

use crate::config::{AddressClass, AdvertisedAddress, DnsConfig, DnsProvider};

/// The SRV service label pair. `_hive._tcp.<zone>` is the whole discovery
/// surface — the MX pattern, one name to look up.
pub const SRV_PREFIX: &str = "_hive._tcp";

/// The subtree every hive address name lives under: `<class>.hive.<zone>`.
/// One subtree so a credential can be scoped to it in a single rule.
pub const ADDRESS_SUFFIX: &str = "hive";

/// SRV priority. Constant on purpose: priority is an administrative override
/// in this design, and a publisher that varied it would be quietly asserting
/// an ordering the dialer is documented to ignore (D35).
pub const SRV_PRIORITY: u16 = 10;

/// SRV weight. Equal weights, for the same reason.
pub const SRV_WEIGHT: u16 = 10;

/// Which record a desired value belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordType {
    Srv,
    A,
    Aaaa,
}

impl RecordType {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordType::Srv => "SRV",
            RecordType::A => "A",
            RecordType::Aaaa => "AAAA",
        }
    }

    fn for_ip(ip: IpAddr) -> RecordType {
        match ip {
            IpAddr::V4(_) => RecordType::A,
            IpAddr::V6(_) => RecordType::Aaaa,
        }
    }
}

impl fmt::Display for RecordType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One RRset the node wants to exist. Values are in DNS presentation form —
/// the shape both an RFC 2136 update and the Cloudflare API's `content` field
/// take, and the shape an operator sees in `dig` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRecord {
    pub name: String,
    pub rtype: RecordType,
    pub ttl: u32,
    /// Sorted, so a plan is a function of the config and not of iteration
    /// order — two runs produce byte-identical plans, which is what makes
    /// "nothing changed" cheap to detect.
    pub values: Vec<String>,
}

/// The names a hive node may ever write, and the exact set a credential must
/// be scoped to (delta 13). Nothing else is publishable by this code path.
pub fn allowed_names(zone: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    names.insert(srv_name(zone));
    for class in AddressClass::ALL {
        names.insert(address_name(class, zone));
    }
    names
}

/// `_hive._tcp.<zone>`.
pub fn srv_name(zone: &str) -> String {
    format!("{SRV_PREFIX}.{zone}")
}

/// `<class>.hive.<zone>`.
pub fn address_name(class: AddressClass, zone: &str) -> String {
    format!("{}.{ADDRESS_SUFFIX}.{zone}", class.as_str())
}

/// The records a node with these addresses wants to exist, in a deterministic
/// order (SRV first, then one address RRset per class in class order).
///
/// An empty address list yields an empty plan rather than an empty SRV: a
/// node that does not know where it lives publishes nothing, so a
/// misconfiguration leaves the previous answer standing instead of blanking
/// discovery for the whole domain.
pub fn plan(
    zone: &str,
    port: u16,
    ttl: u32,
    addresses: &[AdvertisedAddress],
) -> Vec<DesiredRecord> {
    let mut classes: Vec<AdvertisedAddress> = addresses.to_vec();
    classes.sort_by_key(|a| a.class);
    if classes.is_empty() {
        return Vec::new();
    }

    let mut records = Vec::with_capacity(classes.len() + 1);
    let targets: Vec<String> = classes
        .iter()
        .map(|a| {
            format!(
                "{SRV_PRIORITY} {SRV_WEIGHT} {port} {}",
                address_name(a.class, zone)
            )
        })
        .collect();
    records.push(DesiredRecord {
        name: srv_name(zone),
        rtype: RecordType::Srv,
        ttl,
        values: sorted(targets),
    });
    for addr in classes {
        records.push(DesiredRecord {
            name: address_name(addr.class, zone),
            rtype: RecordType::for_ip(addr.ip),
            ttl,
            values: vec![addr.ip.to_string()],
        });
    }
    records
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

/// A zone this node can write. Implementations are thin: everything about
/// WHAT to write is decided above them, so a provider only has to speak its
/// own API.
#[async_trait]
pub trait ZoneApi: Send + Sync {
    /// Current values of one RRset in presentation form; empty when the name
    /// does not exist. Used to make publishing idempotent — a node that
    /// re-publishes on every start (D35) must not rewrite an unchanged zone.
    async fn get(&self, name: &str, rtype: RecordType) -> Result<Vec<String>>;

    /// Make the RRset be exactly `values`.
    async fn upsert(
        &self,
        name: &str,
        rtype: RecordType,
        ttl: u32,
        values: &[String],
    ) -> Result<()>;

    /// What this provider is, for log lines.
    fn describe(&self) -> String;
}

/// What one publish did. Reported rather than logged-and-forgotten because
/// "nothing changed" is the normal, expected outcome and an operator chasing
/// a stale record needs to see which of the two happened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublishReport {
    /// Names whose RRset was written.
    pub upserted: Vec<String>,
    /// Names already correct.
    pub unchanged: Vec<String>,
}

impl PublishReport {
    pub fn total(&self) -> usize {
        self.upserted.len() + self.unchanged.len()
    }
}

/// The publisher: record shaping, containment, and idempotence over some
/// provider's zone API.
pub struct DnsPublisher {
    zone: String,
    ttl: u32,
    port: u16,
    api: Box<dyn ZoneApi>,
}

impl fmt::Debug for DnsPublisher {
    /// Never the credential: a provider describes itself by target, not by
    /// token (`CloudflareZone::describe`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DnsPublisher")
            .field("zone", &self.zone)
            .field("port", &self.port)
            .field("ttl", &self.ttl)
            .field("via", &self.api.describe())
            .finish()
    }
}

impl DnsPublisher {
    pub fn new(
        zone: impl Into<String>,
        port: u16,
        ttl: u32,
        api: Box<dyn ZoneApi>,
    ) -> DnsPublisher {
        DnsPublisher {
            zone: zone.into(),
            ttl,
            port,
            api,
        }
    }

    /// Build the publisher a `[dns]` block asks for, resolving credential
    /// files against `root`. `None` means "publish nothing" — which is both
    /// `provider = "none"` and the honest answer for overlay-native name
    /// services that already resolve the node (MagicDNS, ZeroNS).
    pub fn from_config(
        dns: &DnsConfig,
        root: &Path,
        bound_port: u16,
    ) -> Result<Option<DnsPublisher>> {
        let port = dns.port.unwrap_or(bound_port);
        match dns.provider {
            DnsProvider::None => Ok(None),
            DnsProvider::Cloudflare => {
                let cf = dns
                    .cloudflare
                    .as_ref()
                    .context("provider = \"cloudflare\" without a [dns.cloudflare] table")?;
                let api = CloudflareZone::from_config(cf, root)?;
                Ok(Some(DnsPublisher::new(
                    dns.zone.clone(),
                    port,
                    dns.ttl,
                    Box::new(api),
                )))
            }
            // The record shaping, containment, and idempotence above are rail-
            // agnostic; what is missing is the UPDATE wire itself (a DNS
            // client plus TSIG). Rather than ship an untestable half — no test
            // in this tree may touch live DNS — the node says so out loud at
            // boot, on the box where someone can act on it.
            DnsProvider::Rfc2136 => bail!(
                "[dns] provider = \"rfc2136\" is configured, but this build ships only the \
                 Cloudflare rail — use provider = \"cloudflare\", or \"none\" and publish \
                 `{}` plus the {} address names yourself",
                srv_name(&dns.zone),
                AddressClass::ALL.len()
            ),
        }
    }

    /// The zone this publisher writes.
    pub fn zone(&self) -> &str {
        &self.zone
    }

    /// Publish the node's current addresses: read, compare, write only what
    /// differs. Called on start and on every address change (D35) — which is
    /// why it must be cheap and safe to call when nothing moved.
    pub async fn publish(&self, addresses: &[AdvertisedAddress]) -> Result<PublishReport> {
        self.publish_records(&plan(&self.zone, self.port, self.ttl, addresses))
            .await
    }

    /// The general form: publish an explicit plan. Containment is enforced
    /// here, per record, so it holds for every caller and not only for the
    /// one that builds its own plan — and so a test can point a hostile plan
    /// at a publisher and watch it refuse before a request is made.
    pub async fn publish_records(&self, records: &[DesiredRecord]) -> Result<PublishReport> {
        let allowed = allowed_names(&self.zone);
        let mut report = PublishReport::default();
        for record in records {
            // Containment. Checked here, on every record, before any request:
            // the credential's scope is the deployment's half of delta 13,
            // and this is the code's half.
            if !allowed.contains(&record.name) {
                bail!(
                    "refusing to publish {} {}: outside the hive name set for zone {} ({})",
                    record.rtype,
                    record.name,
                    self.zone,
                    allowed.iter().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            let current = sorted(
                self.api
                    .get(&record.name, record.rtype)
                    .await
                    .with_context(|| format!("reading {} {}", record.rtype, record.name))?,
            );
            if current == record.values {
                report.unchanged.push(record.name.clone());
                continue;
            }
            self.api
                .upsert(&record.name, record.rtype, record.ttl, &record.values)
                .await
                .with_context(|| format!("publishing {} {}", record.rtype, record.name))?;
            tracing::info!(
                name = %record.name,
                rtype = %record.rtype,
                values = %record.values.join(" / "),
                "published"
            );
            report.upserted.push(record.name.clone());
        }
        Ok(report)
    }

    /// What this publisher writes through, for the boot log.
    pub fn describe(&self) -> String {
        self.api.describe()
    }
}

// ── Cloudflare (D35's pragmatic rail) ───────────────────────────────────────

/// The Cloudflare v4 DNS-records API. Chosen as the rail this build ships
/// because it is plain HTTP against a token that Cloudflare itself can scope
/// to individual records — the deployment half of delta 13 is a checkbox
/// rather than a nameserver policy file.
pub struct CloudflareZone {
    client: reqwest::Client,
    api_base: String,
    zone_id: String,
    token: String,
}

impl CloudflareZone {
    /// Build from config, reading the token file. The token is a credential:
    /// it is trimmed of the trailing newline every editor adds and otherwise
    /// never touched, never logged, and never put in an error message.
    pub fn from_config(
        cf: &crate::config::CloudflareConfig,
        root: &Path,
    ) -> Result<CloudflareZone> {
        let path = if cf.api_token_file.is_absolute() {
            cf.api_token_file.clone()
        } else {
            root.join(&cf.api_token_file)
        };
        let token = std::fs::read_to_string(&path)
            .with_context(|| format!("reading Cloudflare API token {}", path.display()))?;
        let token = token.trim().to_string();
        if token.is_empty() {
            bail!("Cloudflare API token file {} is empty", path.display());
        }
        Ok(CloudflareZone::new(&cf.api_base, &cf.zone_id, token))
    }

    pub fn new(api_base: &str, zone_id: &str, token: String) -> CloudflareZone {
        CloudflareZone {
            client: reqwest::Client::new(),
            api_base: api_base.trim_end_matches('/').to_string(),
            zone_id: zone_id.to_string(),
            token,
        }
    }

    fn records_url(&self) -> String {
        format!("{}/zones/{}/dns_records", self.api_base, self.zone_id)
    }

    /// Existing record ids + contents for one RRset. Cloudflare models an
    /// RRset as N independent records, so an upsert is "rewrite the first,
    /// delete the rest, create what is missing".
    async fn existing(&self, name: &str, rtype: RecordType) -> Result<Vec<(String, String)>> {
        let body: serde_json::Value = self
            .send(
                self.client
                    .get(self.records_url())
                    .query(&[("type", rtype.as_str()), ("name", name)]),
            )
            .await?;
        let mut out = Vec::new();
        for record in body["result"].as_array().into_iter().flatten() {
            let (Some(id), Some(content)) = (record["id"].as_str(), record["content"].as_str())
            else {
                bail!("Cloudflare returned a {rtype} record for {name} with no id/content");
            };
            out.push((id.to_string(), content.to_string()));
        }
        Ok(out)
    }

    /// One request, with the API's success envelope turned into an error.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<serde_json::Value> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Cloudflare API request")?;
        let status = response.status();
        let text = response
            .text()
            .await
            .context("reading the Cloudflare API response")?;
        let body: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("Cloudflare API returned {status}: {text}"))?;
        if !status.is_success() || body["success"] != serde_json::Value::Bool(true) {
            bail!("Cloudflare API returned {status}: {}", body["errors"]);
        }
        Ok(body)
    }
}

#[async_trait]
impl ZoneApi for CloudflareZone {
    async fn get(&self, name: &str, rtype: RecordType) -> Result<Vec<String>> {
        Ok(self
            .existing(name, rtype)
            .await?
            .into_iter()
            .map(|(_, content)| content)
            .collect())
    }

    async fn upsert(
        &self,
        name: &str,
        rtype: RecordType,
        ttl: u32,
        values: &[String],
    ) -> Result<()> {
        let existing = self.existing(name, rtype).await?;
        for (i, value) in values.iter().enumerate() {
            let payload = serde_json::json!({
                "type": rtype.as_str(),
                "name": name,
                "content": value,
                "ttl": ttl,
            });
            match existing.get(i) {
                Some((id, _)) => {
                    self.send(
                        self.client
                            .put(format!("{}/{id}", self.records_url()))
                            .json(&payload),
                    )
                    .await?;
                }
                None => {
                    self.send(self.client.post(self.records_url()).json(&payload))
                        .await?;
                }
            }
        }
        // An RRset that shrank: the leftovers are records this node published
        // and no longer wants (a class that went away).
        for (id, _) in existing.iter().skip(values.len()) {
            self.send(self.client.delete(format!("{}/{id}", self.records_url())))
                .await?;
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("cloudflare zone {}", self.zone_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(class: AddressClass, ip: &str) -> AdvertisedAddress {
        AdvertisedAddress {
            class,
            ip: ip.parse().unwrap(),
        }
    }

    #[test]
    fn the_name_set_is_the_srv_plus_the_nodes_own_addresses() {
        let names = allowed_names("example.com");
        assert_eq!(
            names.iter().cloned().collect::<Vec<_>>(),
            vec![
                "_hive._tcp.example.com",
                "lan.hive.example.com",
                "public.hive.example.com",
                "tailnet.hive.example.com",
                "zerotier.hive.example.com",
            ]
        );
        for hostile in ["example.com", "www.example.com", "mail"] {
            assert!(!names.contains(hostile), "{hostile} is not ours to write");
        }
    }

    #[test]
    fn every_class_becomes_an_srv_target_and_its_own_address_record() {
        let records = plan(
            "example.com",
            7847,
            60,
            &[
                addr(AddressClass::Tailnet, "fd7a:115c:a1e0::1"),
                addr(AddressClass::Public, "203.0.113.10"),
            ],
        );
        assert_eq!(records.len(), 3);

        assert_eq!(records[0].name, "_hive._tcp.example.com");
        assert_eq!(records[0].rtype, RecordType::Srv);
        assert_eq!(
            records[0].values,
            vec![
                "10 10 7847 public.hive.example.com",
                "10 10 7847 tailnet.hive.example.com",
            ],
            "multiple targets are the point: the dialer races them"
        );

        assert_eq!(records[1].name, "public.hive.example.com");
        assert_eq!(records[1].rtype, RecordType::A);
        assert_eq!(records[1].values, vec!["203.0.113.10"]);

        assert_eq!(records[2].name, "tailnet.hive.example.com");
        assert_eq!(records[2].rtype, RecordType::Aaaa, "v6 goes in AAAA");
    }

    /// Two runs, same plan — the property that makes "publish on every start"
    /// a no-op instead of a rewrite.
    #[test]
    fn a_plan_is_a_function_of_the_config_not_of_iteration_order() {
        let a = plan(
            "example.com",
            7847,
            60,
            &[
                addr(AddressClass::Lan, "192.168.1.10"),
                addr(AddressClass::Public, "203.0.113.10"),
            ],
        );
        let b = plan(
            "example.com",
            7847,
            60,
            &[
                addr(AddressClass::Public, "203.0.113.10"),
                addr(AddressClass::Lan, "192.168.1.10"),
            ],
        );
        assert_eq!(a, b);
    }

    #[test]
    fn a_node_that_does_not_know_where_it_lives_publishes_nothing() {
        assert!(plan("example.com", 7847, 60, &[]).is_empty());
    }

    #[test]
    fn every_planned_name_is_inside_the_credential_scope() {
        let records = plan(
            "example.com",
            7847,
            60,
            &AddressClass::ALL
                .iter()
                .map(|c| addr(*c, "203.0.113.10"))
                .collect::<Vec<_>>(),
        );
        let allowed = allowed_names("example.com");
        for record in &records {
            assert!(allowed.contains(&record.name), "{} escaped", record.name);
        }
        assert_eq!(records.len(), 5, "one SRV + one per class");
    }

    #[test]
    fn rfc2136_says_so_at_boot_rather_than_at_publish_time() {
        let dns = crate::config::NodeConfig::parse(
            r#"
            [dns]
            provider = "rfc2136"
            zone = "example.com"

            [dns.rfc2136]
            server = "192.0.2.53:53"
            key_name = "hive-update."
            key_file = "tsig.key"
            "#,
        )
        .unwrap()
        .dns
        .unwrap();
        let err = DnsPublisher::from_config(&dns, Path::new("/srv/hive"), 7847).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("rfc2136"), "{text}");
        assert!(text.contains("_hive._tcp.example.com"), "{text}");
    }

    #[test]
    fn provider_none_builds_no_publisher() {
        let dns = crate::config::NodeConfig::parse(
            "[dns]\nprovider = \"none\"\nzone = \"example.com\"\n",
        )
        .unwrap()
        .dns
        .unwrap();
        assert!(
            DnsPublisher::from_config(&dns, Path::new("/srv/hive"), 7847)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_missing_token_file_is_named_not_swallowed() {
        let dns = crate::config::NodeConfig::parse(
            r#"
            [dns]
            provider = "cloudflare"
            zone = "example.com"

            [dns.cloudflare]
            zone_id = "zone-1"
            api_token_file = "cloudflare.token"
            "#,
        )
        .unwrap()
        .dns
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let err = DnsPublisher::from_config(&dns, root.path(), 7847).unwrap_err();
        assert!(format!("{err:#}").contains("cloudflare.token"), "{err:#}");
    }
}
