//! End-to-end tunnel behaviour, driven in-process.
//!
//! These do not use real TLS. That is deliberate: the property under test is
//! that the daemon routes on the SNI and then passes bytes through UNCHANGED,
//! and a synthetic ClientHello followed by arbitrary bytes tests exactly that
//! without a certificate in the way. The live demo (`relay/demo/run.sh`) covers
//! the real-TLS path.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hive_relay::control::{line, ClientMsg, ServerMsg};
use hive_relay::daemon::{Config, Daemon};
use hive_relay::limits::Limits;
use hive_relay::sni::test_client_hello;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const ZONE: &str = "relay.test";
const ID: &str = "hv7bqk2m9x";
const TOKEN: &str = "correct-token";

/// Bind three ephemeral ports and start a daemon on them.
async fn start_daemon() -> (Arc<Daemon>, u16, u16) {
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
        limits: Limits::default(),
        audit_tap: None,
    });

    tokio::spawn(daemon.clone().run_ingress());
    tokio::spawn(daemon.clone().run_control());
    tokio::time::sleep(Duration::from_millis(150)).await;
    (daemon, ingress_port, control_port)
}

/// A stand-in for the household agent: registers, then for each `Open` dials
/// back and echoes everything it receives, prefixed with a marker.
async fn spawn_fake_agent(control_port: u16, token: &str) -> anyhow::Result<()> {
    let sock = TcpStream::connect(("127.0.0.1", control_port)).await?;
    let mut ctrl = BufReader::new(sock);
    ctrl.get_mut()
        .write_all(
            line(&ClientMsg::Hello {
                instance: ID.to_string(),
                token: token.to_string(),
                label: None,
            })?
            .as_bytes(),
        )
        .await?;

    let mut buf = String::new();
    ctrl.read_line(&mut buf).await?;
    match serde_json::from_str::<ServerMsg>(buf.trim())? {
        ServerMsg::Welcome { .. } => {}
        ServerMsg::Error { msg } => anyhow::bail!("refused: {msg}"),
        other => anyhow::bail!("unexpected: {other:?}"),
    }

    tokio::spawn(async move {
        let mut buf = String::new();
        loop {
            buf.clear();
            if ctrl.read_line(&mut buf).await.unwrap_or(0) == 0 {
                return;
            }
            let Ok(msg) = serde_json::from_str::<ServerMsg>(buf.trim()) else {
                continue;
            };
            if let ServerMsg::Open { nonce } = msg {
                tokio::spawn(async move {
                    let Ok(mut data) = TcpStream::connect(("127.0.0.1", control_port)).await else {
                        return;
                    };
                    let Ok(l) = line(&ClientMsg::Data { nonce }) else {
                        return;
                    };
                    if data.write_all(l.as_bytes()).await.is_err() {
                        return;
                    }
                    // Echo whatever arrives, so the test can compare bytes.
                    let mut chunk = vec![0u8; 8192];
                    while let Ok(n) = data.read(&mut chunk).await {
                        if n == 0 || data.write_all(&chunk[..n]).await.is_err() {
                            return;
                        }
                    }
                });
            }
        }
    });
    Ok(())
}

#[tokio::test]
async fn routes_on_sni_and_passes_bytes_through_unchanged() {
    let (daemon, ingress_port, control_port) = start_daemon().await;
    spawn_fake_agent(control_port, TOKEN)
        .await
        .expect("agent registers");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(daemon.registry.count().await, 1);

    let hello = test_client_hello(&format!("{ID}.{ZONE}"));
    let payload = b"GET /api/journal HTTP/1.1\r\nHost: x\r\n\r\nCANARY-abc";

    let mut client = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect ingress");
    client.write_all(&hello).await.expect("send hello");
    client.write_all(payload).await.expect("send payload");

    let want = [hello.clone(), payload.to_vec()].concat();
    let mut got = vec![0u8; want.len()];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut got))
        .await
        .expect("not timed out")
        .expect("read echo");

    // Byte-for-byte: the ClientHello the daemon peeked at is replayed intact,
    // and nothing in the stream is rewritten.
    assert_eq!(got, want, "the tunnel altered the stream");
}

#[tokio::test]
async fn a_wrong_token_cannot_register() {
    let (daemon, _ingress, control_port) = start_daemon().await;
    let err = spawn_fake_agent(control_port, "wrong-token").await;
    assert!(err.is_err(), "a bad token must be refused");
    assert_eq!(daemon.registry.count().await, 0);
}

#[tokio::test]
async fn an_unknown_sni_is_dropped_without_a_backend() {
    let (_daemon, ingress_port, _control) = start_daemon().await;

    let mut client = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect");
    client
        .write_all(&test_client_hello(&format!("nobody-home.{ZONE}")))
        .await
        .expect("send hello");

    // Closed with no TLS alert, which is indistinguishable from a network
    // drop ... so this does not confirm whether the id exists.
    let mut got = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut got))
        .await
        .expect("not timed out")
        .expect("read");
    assert_eq!(read, 0, "nothing should be sent back");
}

#[tokio::test]
async fn a_non_tls_connection_is_refused() {
    let (_daemon, ingress_port, _control) = start_daemon().await;

    let mut client = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect");
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: relay.test\r\n\r\n")
        .await
        .expect("send");

    let mut got = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut got))
        .await
        .expect("not timed out")
        .expect("read");
    assert_eq!(
        read, 0,
        "plain HTTP gets nothing: this port only routes TLS"
    );
}

#[tokio::test]
async fn dropping_the_control_connection_deregisters_the_instance() {
    let (daemon, _ingress, control_port) = start_daemon().await;

    let sock = TcpStream::connect(("127.0.0.1", control_port))
        .await
        .expect("connect");
    let mut ctrl = BufReader::new(sock);
    ctrl.get_mut()
        .write_all(
            line(&ClientMsg::Hello {
                instance: ID.to_string(),
                token: TOKEN.to_string(),
                label: None,
            })
            .expect("line")
            .as_bytes(),
        )
        .await
        .expect("hello");
    let mut buf = String::new();
    ctrl.read_line(&mut buf).await.expect("welcome");
    assert_eq!(daemon.registry.count().await, 1);

    // This is the off switch: stop the agent and the relay routes nothing.
    drop(ctrl);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        daemon.registry.count().await,
        0,
        "a disconnected house must stop being routable"
    );
}
