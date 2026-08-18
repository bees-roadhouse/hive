//! The one claim the whole design exists to make, as a test rather than a
//! demo script nobody runs.
//!
//! `relay/demo/run.sh` proves this against a real `hive-api` and real curl,
//! and it is the better artefact for a human. It is also never invoked by CI,
//! which meant the headline property ... **the relay cannot read the traffic**
//! ... was the one thing in this crate with no test behind it.
//!
//! So: real TLS, in process. A certificate the INSTANCE holds and the relay
//! has never seen the key for, a real handshake between the client and the
//! instance through the relay's splice, and two captures of the same session.
//! One is grepped for a canary that must be there, the other for the same
//! canary which must not. The control half is not optional: without it,
//! "the grep found nothing" could just mean the grep was broken.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hive_relay::agent::AgentConfig;
use hive_relay::daemon::{Config, Daemon};
use hive_relay::limits::Limits;
use hive_relay::tap::Tap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

const ZONE: &str = "relay.test";
const ID: &str = "hv7bqk2m9x";
const TOKEN: &str = "s3cret-registration-token";

#[tokio::test]
async fn the_relay_cannot_read_what_it_forwards() {
    // rustls wants an explicit process-wide provider. Already installed is
    // fine ... another test in this binary may have got here first.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let host = format!("{ID}.{ZONE}");
    let dir = std::env::temp_dir().join(format!("hive-relay-blindness-{}", nanoid::nanoid!(10)));
    std::fs::create_dir_all(&dir).expect("temp dir");

    // The certificate is the instance's. The relay never reads either file:
    // it is not a party to the handshake and holds no key for this name.
    let issued = rcgen::generate_simple_self_signed(vec![host.clone()]).expect("issue certificate");
    let cert_path = dir.join("instance.crt");
    let key_path = dir.join("instance.key");
    std::fs::write(&cert_path, issued.cert.pem()).expect("write cert");
    std::fs::write(&key_path, issued.signing_key.serialize_pem()).expect("write key");

    let relay_capture = dir.join("relay-observed.bin");
    let plain_capture = dir.join("instance-plaintext.bin");

    // Two canaries: one the client sends, one the "hive-api" sends back, so
    // both directions are covered.
    let request_canary = "CANARY-REQ-loganton-pennsylvania";
    let response_canary = "CANARY-RES-the-roadhouse-journal";

    let api = fake_hive_api(response_canary).await;

    let (daemon, ingress_port, control_port) = start_daemon(&relay_capture).await;

    let agent = tokio::spawn(hive_relay::agent::run(AgentConfig {
        relay: format!("127.0.0.1:{control_port}"),
        instance: ID.to_string(),
        token: TOKEN.to_string(),
        target: api.to_string(),
        cert: cert_path,
        key: key_path,
        label: Some("The Roadhouse".to_string()),
        plaintext_tap: Some(Tap::create(&plain_capture).await.expect("plaintext tap")),
    }));

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while daemon.registry.count() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "agent never registered"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // ---- a real request, over real TLS, through the relay ------------------

    let mut roots = RootCertStore::empty();
    roots
        .add(issued.cert.der().clone())
        .expect("trust the instance certificate");
    let tls = TlsConnector::from(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));

    let sock = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect ingress");
    let name = ServerName::try_from(host.clone()).expect("server name");
    let mut stream = tokio::time::timeout(Duration::from_secs(10), tls.connect(name, sock))
        .await
        .expect("handshake did not time out")
        .expect("handshake with the INSTANCE, through the relay");

    let request = format!(
        "GET /api/healthz?trace={request_canary} HTTP/1.1\r\n\
         Host: {host}\r\n\
         X-Trace: {request_canary}\r\n\
         Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send request");

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
        .await
        .expect("response did not time out")
        .expect("read response");
    let response = String::from_utf8_lossy(&response).to_string();
    assert!(
        response.contains(response_canary),
        "the tunnel did not carry a real session: {response}"
    );

    agent.abort();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let relay_saw = std::fs::read(&relay_capture).expect("relay capture");
    let instance_saw = std::fs::read(&plain_capture).expect("plaintext capture");
    let _ = std::fs::remove_dir_all(&dir);

    // ---- control: the grep works ------------------------------------------
    //
    // Every assertion below this line is worthless without these three. A
    // capture that is empty, or a comparison that never matches anything,
    // would otherwise pass the real test silently.
    assert!(!relay_saw.is_empty(), "the relay captured nothing at all");
    assert!(
        contains(&relay_saw, b"instance->client"),
        "the relay only captured one direction, so the response was never on trial"
    );
    for (needle, what) in [
        (request_canary, "the request canary"),
        ("GET /api/healthz", "the request line"),
        ("X-Trace", "a request header"),
        (response_canary, "the response body"),
        ("HTTP/1.1", "the protocol version"),
    ] {
        assert!(
            contains(&instance_saw, needle.as_bytes()),
            "{what} is missing from the instance's own plaintext, so this test proves nothing"
        );
    }

    // ---- the claim ---------------------------------------------------------

    for (needle, what) in [
        (request_canary, "the request canary"),
        ("GET /api/healthz", "the request line"),
        ("X-Trace", "a request header"),
        (response_canary, "the response body"),
        ("HTTP/1.1", "the protocol version"),
    ] {
        assert!(
            !contains(&relay_saw, needle.as_bytes()),
            "{what} appeared in what the relay forwarded: it is not blind"
        );
    }

    // What it CAN see, stated as an assertion rather than a promise: the name
    // in the ClientHello, which is plaintext by construction and is the thing
    // it routes on. Nothing else in the session is legible to it.
    assert!(
        contains(&relay_saw, host.as_bytes()),
        "the SNI should be the one readable thing in the capture"
    );
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Stands in for `hive-api`: answers one request per connection and closes.
async fn fake_hive_api(canary: &str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind api");
    let addr = listener.local_addr().expect("addr");
    let body = format!("{{\"ok\":true,\"service\":\"hive-rust\",\"trace\":\"{canary}\"}}");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut chunk = [0u8; 1024];
                while let Ok(n) = sock.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&chunk[..n]);
                    if contains(&req, b"\r\n\r\n") {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

async fn start_daemon(audit_tap: &std::path::Path) -> (Arc<Daemon>, u16, u16) {
    let ingress = TcpListener::bind("127.0.0.1:0").await.expect("ingress");
    let control = TcpListener::bind("127.0.0.1:0").await.expect("control");
    let admin = TcpListener::bind("127.0.0.1:0").await.expect("admin");
    let (ingress_port, control_port) = (
        ingress.local_addr().expect("addr").port(),
        control.local_addr().expect("addr").port(),
    );
    let admin_addr = admin.local_addr().expect("addr");
    drop(ingress);
    drop(control);
    drop(admin);

    let mut tokens = HashMap::new();
    tokens.insert(ID.to_string(), TOKEN.to_string());

    let daemon = Daemon::new(Config {
        ingress_addr: format!("127.0.0.1:{ingress_port}").parse().expect("addr"),
        control_addr: format!("127.0.0.1:{control_port}").parse().expect("addr"),
        admin_addr,
        zone: ZONE.to_string(),
        tokens,
        limits: Limits {
            conns_per_ip: 100_000,
            control_conns_per_ip: 100_000,
            ..Default::default()
        },
        // The affordance this test exists to read. It records ciphertext the
        // relay cannot interpret, which is the point, and it must never be on
        // outside a demo or a test like this one.
        audit_tap: Some(Tap::create(audit_tap).await.expect("audit tap")),
    });

    tokio::spawn(daemon.clone().run_ingress());
    tokio::spawn(daemon.clone().run_control());
    tokio::time::sleep(Duration::from_millis(100)).await;
    (daemon, ingress_port, control_port)
}
