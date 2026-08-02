// The DNS publisher, against mocks only (PLAN-v2.1 PR 4.6; the
// "DNS-publisher containment" row of the ADVERSARIAL-SMOKE ledger,
// docs/TESTING-STRATEGY.md §4.4).
//
// **No test in this file performs a DNS lookup or reaches a network.** The
// zone is a trait (`ZoneApi`) with an in-memory implementation, and the
// Cloudflare rail is exercised against a loopback HTTP endpoint this file
// spawns — so the real reqwest client, the real URLs, and the real request
// bodies are all covered without an account, a token, or a zone.
//
// Two properties matter more than "it publishes":
//
//   * CONTAINMENT. The publisher writes `_hive._tcp.<zone>` and the node's
//     own per-class A/AAAA names, and nothing else, ever. That is the
//     code half of delta 13's scoped-credential claim (the other half is the
//     token's own scope, which is a deployment audit item at PR 4.20). Here
//     it is asserted twice: once at the publisher's refusal, and once over
//     every request that actually reached the wire.
//   * IDEMPOTENCE. A node re-publishes on every start and on every address
//     change (D35), so "nothing moved" has to cost reads and no writes.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use hive_node::config::{AddressClass, AdvertisedAddress, CloudflareConfig, DnsConfig};
use hive_node::dns::{allowed_names, plan, CloudflareZone, DesiredRecord, RecordType, ZoneApi};
use hive_node::{DnsPublisher, NodeConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ZONE: &str = "example.com";
const PORT: u16 = 7847;
const TTL: u32 = 60;

fn addr(class: AddressClass, ip: &str) -> AdvertisedAddress {
    AdvertisedAddress {
        class,
        ip: ip.parse().unwrap(),
    }
}

// ── an in-memory zone ───────────────────────────────────────────────────────

/// Every call the publisher made, in order — the containment evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Get(String, RecordType),
    Upsert(String, RecordType, Vec<String>),
}

#[derive(Clone, Default)]
struct MockZone {
    state: Arc<Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    records: BTreeMap<(String, String), Vec<String>>,
    calls: Vec<Call>,
}

impl MockZone {
    fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    fn written_names(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Upsert(name, _, _) => Some(name),
                Call::Get(..) => None,
            })
            .collect()
    }

    fn values(&self, name: &str, rtype: RecordType) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .records
            .get(&(name.to_string(), rtype.as_str().to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn clear_calls(&self) {
        self.state.lock().unwrap().calls.clear();
    }
}

#[async_trait]
impl ZoneApi for MockZone {
    async fn get(&self, name: &str, rtype: RecordType) -> Result<Vec<String>> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(Call::Get(name.to_string(), rtype));
        Ok(state
            .records
            .get(&(name.to_string(), rtype.as_str().to_string()))
            .cloned()
            .unwrap_or_default())
    }

    async fn upsert(
        &self,
        name: &str,
        rtype: RecordType,
        _ttl: u32,
        values: &[String],
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state
            .calls
            .push(Call::Upsert(name.to_string(), rtype, values.to_vec()));
        state.records.insert(
            (name.to_string(), rtype.as_str().to_string()),
            values.to_vec(),
        );
        Ok(())
    }

    fn describe(&self) -> String {
        "mock zone".to_string()
    }
}

fn publisher(zone: &MockZone) -> DnsPublisher {
    DnsPublisher::new(ZONE, PORT, TTL, Box::new(zone.clone()))
}

#[tokio::test]
async fn a_publish_writes_the_srv_and_the_addresses_and_nothing_else() {
    let zone = MockZone::default();
    let report = publisher(&zone)
        .publish(&[
            addr(AddressClass::Public, "203.0.113.10"),
            addr(AddressClass::Lan, "192.168.1.10"),
        ])
        .await
        .unwrap();

    assert_eq!(
        report.upserted,
        vec![
            "_hive._tcp.example.com",
            "public.hive.example.com",
            "lan.hive.example.com",
        ],
        "SRV first, then one record per class in D35's class order"
    );
    assert!(report.unchanged.is_empty());

    assert_eq!(
        zone.values("_hive._tcp.example.com", RecordType::Srv),
        vec![
            "10 10 7847 lan.hive.example.com",
            "10 10 7847 public.hive.example.com",
        ],
        "both classes are candidates; the dialer races them (D35)"
    );
    assert_eq!(
        zone.values("public.hive.example.com", RecordType::A),
        vec!["203.0.113.10"]
    );

    let allowed = allowed_names(ZONE);
    for name in zone.written_names() {
        assert!(allowed.contains(&name), "{name} is not ours to write");
    }
}

#[tokio::test]
async fn a_second_publish_of_the_same_addresses_writes_nothing() {
    let zone = MockZone::default();
    let addresses = [addr(AddressClass::Tailnet, "fd7a:115c:a1e0::1")];
    publisher(&zone).publish(&addresses).await.unwrap();
    zone.clear_calls();

    let report = publisher(&zone).publish(&addresses).await.unwrap();
    assert_eq!(report.upserted, Vec::<String>::new());
    assert_eq!(report.unchanged.len(), 2, "the SRV and the AAAA");
    assert!(
        zone.written_names().is_empty(),
        "re-publishing on every start must cost reads, not writes"
    );
}

#[tokio::test]
async fn a_moved_address_rewrites_only_the_record_that_moved() {
    let zone = MockZone::default();
    publisher(&zone)
        .publish(&[
            addr(AddressClass::Public, "203.0.113.10"),
            addr(AddressClass::Lan, "192.168.1.10"),
        ])
        .await
        .unwrap();
    zone.clear_calls();

    // The LAN lease changed; nothing else did.
    let report = publisher(&zone)
        .publish(&[
            addr(AddressClass::Public, "203.0.113.10"),
            addr(AddressClass::Lan, "192.168.1.99"),
        ])
        .await
        .unwrap();
    assert_eq!(report.upserted, vec!["lan.hive.example.com"]);
    assert_eq!(
        report.unchanged,
        vec!["_hive._tcp.example.com", "public.hive.example.com"],
        "the SRV names classes, not addresses, so it does not churn"
    );
    assert_eq!(
        zone.values("lan.hive.example.com", RecordType::A),
        vec!["192.168.1.99"]
    );
}

/// A class that goes away takes its SRV target with it — otherwise the dialer
/// keeps racing a candidate that cannot answer.
#[tokio::test]
async fn a_class_that_disappears_leaves_the_srv() {
    let zone = MockZone::default();
    publisher(&zone)
        .publish(&[
            addr(AddressClass::Public, "203.0.113.10"),
            addr(AddressClass::Lan, "192.168.1.10"),
        ])
        .await
        .unwrap();
    publisher(&zone)
        .publish(&[addr(AddressClass::Public, "203.0.113.10")])
        .await
        .unwrap();
    assert_eq!(
        zone.values("_hive._tcp.example.com", RecordType::Srv),
        vec!["10 10 7847 public.hive.example.com"]
    );
}

/// The containment refusal itself: a plan carrying a name outside the hive
/// set never reaches the API. This is the assertion delta 13's claim rests on
/// — a stolen node cannot rewrite MX even if something upstream asks it to.
#[tokio::test]
async fn a_name_outside_the_hive_set_is_refused_before_any_request() {
    let zone = MockZone::default();
    for hostile in [
        "example.com",
        "mail.example.com",
        "_hive._tcp.evil.com",
        "www.example.com",
    ] {
        let err = publisher(&zone)
            .publish_records(&[DesiredRecord {
                name: hostile.to_string(),
                rtype: RecordType::A,
                ttl: TTL,
                values: vec!["203.0.113.99".to_string()],
            }])
            .await
            .expect_err("outside the hive name set");
        let text = format!("{err:#}");
        assert!(text.contains("refusing to publish"), "{text}");
        assert!(text.contains(hostile), "{text}");
    }
    assert!(
        zone.calls().is_empty(),
        "not even a read: the refusal is before the request"
    );
}

/// Whatever the config says, the plan can only name the five records the
/// credential is scoped to.
#[tokio::test]
async fn every_class_a_node_can_declare_stays_inside_the_scope() {
    let addresses: Vec<AdvertisedAddress> = AddressClass::ALL
        .iter()
        .map(|c| addr(*c, "203.0.113.10"))
        .collect();
    let zone = MockZone::default();
    publisher(&zone).publish(&addresses).await.unwrap();

    let allowed = allowed_names(ZONE);
    assert_eq!(zone.written_names().len(), 5);
    for name in zone.written_names() {
        assert!(allowed.contains(&name));
    }
    assert_eq!(plan(ZONE, PORT, TTL, &addresses).len(), allowed.len());
}

// ── the Cloudflare rail, against a loopback mock ────────────────────────────

/// A minimal stand-in for the Cloudflare v4 DNS-records API: enough to make
/// the real client's URLs, verbs, auth header, and JSON bodies observable.
struct MockApi {
    addr: SocketAddr,
    state: Arc<Mutex<ApiState>>,
}

#[derive(Default)]
struct ApiState {
    /// (method, path) of every request, in order.
    requests: Vec<(String, String)>,
    /// Bearer tokens seen — the credential must actually be presented.
    tokens: Vec<String>,
    /// id -> (name, type, content)
    records: BTreeMap<u32, (String, String, String)>,
    next_id: u32,
}

impl MockApi {
    async fn start() -> MockApi {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(ApiState {
            next_id: 1,
            ..ApiState::default()
        }));
        let served = state.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = served.clone();
                tokio::spawn(async move { serve(stream, state).await });
            }
        });
        MockApi { addr, state }
    }

    fn base(&self) -> String {
        format!("http://{}/client/v4", self.addr)
    }

    fn requests(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().requests.clone()
    }

    fn writes(&self) -> Vec<(String, String)> {
        self.requests()
            .into_iter()
            .filter(|(method, _)| method != "GET")
            .collect()
    }

    fn contents(&self) -> Vec<(String, String, String)> {
        self.state
            .lock()
            .unwrap()
            .records
            .values()
            .cloned()
            .collect()
    }

    fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.requests.clear();
        state.tokens.clear();
    }
}

/// One connection: HTTP/1.1, keep-alive, exactly the verbs the rail uses.
async fn serve(mut stream: tokio::net::TcpStream, state: Arc<Mutex<ApiState>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        // Read until at least one whole request (headers + declared body) is
        // buffered. Small requests, so this stays a loop and not a parser.
        let head_end = loop {
            if let Some(i) = find(&buf, b"\r\n\r\n") {
                break i + 4;
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();

        let mut length = 0usize;
        let mut token = String::new();
        for line in lines {
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
            if let Some(v) = lower.strip_prefix("authorization: bearer ") {
                token = v.trim().to_string();
            }
        }
        while buf.len() < head_end + length {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let body = String::from_utf8_lossy(&buf[head_end..head_end + length]).to_string();
        buf.drain(..head_end + length);

        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target.clone(), String::new()),
        };
        let response = {
            let mut state = state.lock().unwrap();
            state.requests.push((method.clone(), path.clone()));
            state.tokens.push(token);
            handle(&mut state, &method, &path, &query, &body)
        };
        let payload = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            response.len(),
            response
        );
        if stream.write_all(payload.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn handle(state: &mut ApiState, method: &str, path: &str, query: &str, body: &str) -> String {
    let ok = |result: String| format!("{{\"success\":true,\"errors\":[],\"result\":{result}}}");
    match method {
        "GET" => {
            let want = |key: &str| {
                query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
                    .unwrap_or_default()
                    .replace("%2E", ".")
            };
            let (name, rtype) = (want("name"), want("type"));
            let hits: Vec<String> = state
                .records
                .iter()
                .filter(|(_, (n, t, _))| *n == name && *t == rtype)
                .map(|(id, (n, t, content))| {
                    format!(
                        "{{\"id\":\"{id}\",\"name\":\"{n}\",\"type\":\"{t}\",\"content\":\"{content}\"}}"
                    )
                })
                .collect();
            ok(format!("[{}]", hits.join(",")))
        }
        "POST" => {
            let record: serde_json::Value = serde_json::from_str(body).expect("a JSON body");
            let id = state.next_id;
            state.next_id += 1;
            state.records.insert(
                id,
                (
                    record["name"].as_str().unwrap().to_string(),
                    record["type"].as_str().unwrap().to_string(),
                    record["content"].as_str().unwrap().to_string(),
                ),
            );
            ok(format!("{{\"id\":\"{id}\"}}"))
        }
        "PUT" => {
            let record: serde_json::Value = serde_json::from_str(body).expect("a JSON body");
            let id: u32 = path.rsplit('/').next().unwrap().parse().unwrap();
            state.records.insert(
                id,
                (
                    record["name"].as_str().unwrap().to_string(),
                    record["type"].as_str().unwrap().to_string(),
                    record["content"].as_str().unwrap().to_string(),
                ),
            );
            ok(format!("{{\"id\":\"{id}\"}}"))
        }
        "DELETE" => {
            let id: u32 = path.rsplit('/').next().unwrap().parse().unwrap();
            state.records.remove(&id);
            ok(format!("{{\"id\":\"{id}\"}}"))
        }
        _ => ok("null".to_string()),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn cloudflare_publisher(api: &MockApi, root: &std::path::Path) -> DnsPublisher {
    let config = DnsConfig {
        provider: hive_node::DnsProvider::Cloudflare,
        zone: ZONE.to_string(),
        ttl: TTL,
        port: Some(PORT),
        address: Vec::new(),
        cloudflare: Some(CloudflareConfig {
            zone_id: "zone-1".to_string(),
            api_token_file: root.join("cloudflare.token"),
            api_base: api.base(),
        }),
        rfc2136: None,
    };
    DnsPublisher::from_config(&config, root, PORT)
        .expect("a cloudflare publisher")
        .expect("provider is not none")
}

#[tokio::test]
async fn the_cloudflare_rail_creates_updates_and_then_stays_quiet() {
    let api = MockApi::start().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("cloudflare.token"), "test-token\n").unwrap();

    // First publish: nothing exists, so every record is created.
    let report = cloudflare_publisher(&api, root.path())
        .publish(&[addr(AddressClass::Public, "203.0.113.10")])
        .await
        .unwrap();
    assert_eq!(report.upserted.len(), 2);
    assert!(
        api.writes().iter().all(|(method, _)| method == "POST"),
        "creates: {:?}",
        api.writes()
    );
    assert!(
        api.contents().contains(&(
            "public.hive.example.com".to_string(),
            "A".to_string(),
            "203.0.113.10".to_string()
        )),
        "{:?}",
        api.contents()
    );
    assert!(
        api.state
            .lock()
            .unwrap()
            .tokens
            .iter()
            .all(|t| t == "test-token"),
        "the credential is presented on every request"
    );

    // Second publish, nothing moved: reads only.
    api.clear();
    let report = cloudflare_publisher(&api, root.path())
        .publish(&[addr(AddressClass::Public, "203.0.113.10")])
        .await
        .unwrap();
    assert_eq!(report.unchanged.len(), 2);
    assert!(api.writes().is_empty(), "{:?}", api.requests());

    // The address moved: the existing record is updated in place.
    api.clear();
    cloudflare_publisher(&api, root.path())
        .publish(&[addr(AddressClass::Public, "203.0.113.11")])
        .await
        .unwrap();
    assert!(
        api.writes().iter().any(|(method, _)| method == "PUT"),
        "{:?}",
        api.writes()
    );
    assert!(api.contents().contains(&(
        "public.hive.example.com".to_string(),
        "A".to_string(),
        "203.0.113.11".to_string()
    )));
}

/// The containment claim, asserted where it is checkable: over every request
/// that actually reached the wire.
#[tokio::test]
async fn every_request_the_cloudflare_rail_makes_is_about_a_hive_name() {
    let api = MockApi::start().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("cloudflare.token"), "test-token").unwrap();

    cloudflare_publisher(&api, root.path())
        .publish(
            &AddressClass::ALL
                .iter()
                .map(|c| addr(*c, "203.0.113.10"))
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();

    for (_, path) in api.requests() {
        assert!(
            path.starts_with("/client/v4/zones/zone-1/dns_records"),
            "the rail talks to one zone's records and nothing else: {path}"
        );
    }
    let allowed = allowed_names(ZONE);
    for (name, _, _) in api.contents() {
        assert!(allowed.contains(&name), "{name} escaped the hive name set");
    }
}

/// The `[dns]` block round-trips from a real `node.toml` into a publisher —
/// the path an operator actually takes.
#[test]
fn a_node_toml_dns_block_builds_the_publisher_it_describes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("cloudflare.token"), "test-token").unwrap();
    let config = NodeConfig::parse(
        r#"
        [dns]
        provider = "cloudflare"
        zone = "example.com"

        [dns.cloudflare]
        zone_id = "zone-1"
        api_token_file = "cloudflare.token"

        [[dns.address]]
        class = "public"
        ip = "203.0.113.10"
        "#,
    )
    .unwrap();
    let dns = config.dns.unwrap();
    let publisher = DnsPublisher::from_config(&dns, root.path(), 9999)
        .unwrap()
        .expect("cloudflare is not none");
    assert_eq!(publisher.zone(), ZONE);
    assert!(publisher.describe().contains("zone-1"));
    // No `port` in the block, so the SRV carries the port actually bound.
    assert!(format!("{publisher:?}").contains("9999"));
}

/// A token file is a credential: it must never be echoed anywhere.
#[test]
fn the_credential_is_not_in_the_publishers_own_description() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("cloudflare.token"), "super-secret-token").unwrap();
    let zone = CloudflareZone::new("https://example.invalid", "zone-1", "super-secret".into());
    assert!(!zone.describe().contains("super-secret"));
    let _ = root;
}
