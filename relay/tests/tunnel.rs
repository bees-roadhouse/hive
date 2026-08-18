//! End-to-end tunnel behaviour, driven in-process.
//!
//! These do not use real TLS. That is deliberate: the property under test is
//! that the daemon routes on the SNI and then passes bytes through UNCHANGED,
//! and a synthetic ClientHello followed by arbitrary bytes tests exactly that
//! without a certificate in the way. `tests/blindness.rs` does the real-TLS
//! path, because "the relay cannot read this" is not a claim a synthetic
//! handshake can support.
//!
//! The rest is what an unauthenticated stranger can do to this port. Every
//! test below with a cap or a timeout in it is a defect that was live: sixty
//! four silent connections took a house off the relay permanently, a stream
//! with no newline in it was read into memory unbounded, and a control
//! connection that reset at the wrong moment locked its instance out until the
//! process restarted.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hive_relay::control::{line, ClientMsg, ServerMsg};
use hive_relay::daemon::{Config, Daemon};
use hive_relay::limits::Limits;
use hive_relay::sni::{test_client_hello, test_client_hello_without_sni};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const ZONE: &str = "relay.test";
const ID: &str = "hv7bqk2m9x";
const TOKEN: &str = "correct-token";

/// Everything a loopback test needs out of the way: these tests drive dozens
/// of connections from one address, and the per-source rate limit is not what
/// any of them is measuring.
fn test_limits() -> Limits {
    Limits {
        conns_per_ip: 100_000,
        control_conns_per_ip: 100_000,
        ..Default::default()
    }
}

/// Bind three ephemeral ports and start a daemon on them.
async fn start_daemon() -> (Arc<Daemon>, u16, u16) {
    start_daemon_with(test_limits()).await
}

async fn start_daemon_with(limits: Limits) -> (Arc<Daemon>, u16, u16) {
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
        limits,
        audit_tap: None,
    });

    tokio::spawn(daemon.clone().run_ingress());
    tokio::spawn(daemon.clone().run_control());
    tokio::time::sleep(Duration::from_millis(150)).await;
    (daemon, ingress_port, control_port)
}

/// Poll until `f` holds, or give up. Beats sleeping for a guessed interval on
/// a loaded runner.
async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn live_sessions(daemon: &Arc<Daemon>) -> usize {
    daemon
        .registry
        .get(ID)
        .map(|i| i.live.load(std::sync::atomic::Ordering::SeqCst))
        .unwrap_or(0)
}

/// Read to EOF with a ceiling on how long the relay may take to hang up.
async fn read_to_eof<R: AsyncRead + Unpin>(mut r: R, within: Duration) -> Vec<u8> {
    let mut got = Vec::new();
    tokio::time::timeout(within, r.read_to_end(&mut got))
        .await
        .expect("the relay should have hung up by now")
        .expect("read");
    got
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
            if matches!(msg, ServerMsg::Ping) {
                let Ok(pong) = line(&ClientMsg::Pong) else {
                    return;
                };
                if ctrl.get_mut().write_all(pong.as_bytes()).await.is_err() {
                    return;
                }
            }
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
    assert_eq!(daemon.registry.count(), 1);

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
    assert_eq!(daemon.registry.count(), 0);
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
    assert_eq!(daemon.registry.count(), 1);

    // This is the off switch: stop the agent and the relay routes nothing.
    drop(ctrl);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        daemon.registry.count(),
        0,
        "a disconnected house must stop being routable"
    );
}

/// A house whose network drops must be able to come back.
///
/// Registration is exclusive, so a registry entry that outlives its control
/// connection is not a leak, it is a lockout: that id cannot re-register until
/// the relay process restarts. The reset here is forced with `SO_LINGER 0`,
/// which aims it at the welcome write ... one of four `?` operators that used
/// to return past the cleanup.
#[tokio::test]
async fn a_reset_control_connection_does_not_lock_the_instance_out() {
    let (daemon, _ingress, control_port) = start_daemon().await;

    for attempt in 0..3 {
        let mut sock = TcpStream::connect(("127.0.0.1", control_port))
            .await
            .expect("connect");
        // Zero linger makes close() send RST instead of FIN, which is what a
        // house falling off the internet looks like. Deprecated in tokio
        // because a non-zero linger blocks the thread on drop; zero does not.
        #[allow(deprecated)]
        sock.set_linger(Some(Duration::ZERO)).expect("linger");
        sock.write_all(
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
        // No read of the welcome: go away mid-registration, hard.
        drop(sock);

        until(&format!("deregistration after attempt {attempt}"), || {
            daemon.registry.count() == 0
        })
        .await;
    }

    // And the id is still usable, which is the property that actually matters.
    spawn_fake_agent(control_port, TOKEN)
        .await
        .expect("the instance can still register");
    assert_eq!(daemon.registry.count(), 1);
}

/// The headline denial of service, end to end.
///
/// Open the per-instance cap's worth of connections, send a well-formed
/// ClientHello on each, then say nothing and never close. Every permit used to
/// be held until the process restarted, which took the house off the internet
/// for everyone else permanently. Two IPs or eleven seconds got you the sixty
/// four connections; `conns_per_ip` never entered into it.
#[tokio::test]
async fn idle_connections_cannot_hold_an_instance_offline() {
    let cap = 64;
    let (daemon, ingress_port, control_port) = start_daemon_with(Limits {
        max_conns_per_instance: cap,
        idle_timeout: Duration::from_millis(600),
        ..test_limits()
    })
    .await;
    spawn_fake_agent(control_port, TOKEN)
        .await
        .expect("agent registers");
    until("registration", || daemon.registry.count() == 1).await;

    let hello = test_client_hello(&format!("{ID}.{ZONE}"));
    let mut squatters = Vec::new();
    for i in 0..cap {
        let mut sock = TcpStream::connect(("127.0.0.1", ingress_port))
            .await
            .unwrap_or_else(|e| panic!("squatter {i} connect: {e}"));
        sock.write_all(&hello).await.expect("send hello");
        squatters.push(sock); // held open, never written to again
    }
    until("all squatters to be spliced", || {
        live_sessions(&daemon) == cap
    })
    .await;

    // The cap is real: while they are held, nobody else gets in. This is the
    // half that used to be permanent.
    let mut refused = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect");
    refused.write_all(&hello).await.expect("send hello");
    assert!(
        read_to_eof(&mut refused, Duration::from_secs(5))
            .await
            .is_empty(),
        "over the cap, the relay closes without a word"
    );

    // Now the fix: the idle timeout reaps them without anyone having to close
    // anything, and the visitor after them gets a working session.
    until("idle sessions to be reaped", || live_sessions(&daemon) == 0).await;

    let payload = b"GET /api/journal HTTP/1.1\r\nHost: x\r\n\r\nCANARY-after-the-flood";
    let mut client = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect");
    client.write_all(&hello).await.expect("send hello");
    client.write_all(payload).await.expect("send payload");

    let want = [hello.clone(), payload.to_vec()].concat();
    let mut got = vec![0u8; want.len()];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut got))
        .await
        .expect("the instance must be reachable again")
        .expect("read echo");
    assert_eq!(got, want);

    // The squatters were closed by the relay, not by us.
    for (i, mut s) in squatters.into_iter().enumerate() {
        let mut sink = [0u8; 64];
        let mut closed = false;
        while let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(5), s.read(&mut sink)).await
        {
            if n == 0 {
                closed = true;
                break;
            }
        }
        assert!(closed, "squatter {i} was never hung up on");
    }
}

/// A complete ClientHello with no SNI is a decided answer. Treating it as
/// "read more" held a connection for the whole handshake deadline and then
/// failed anyway, which made it the cheapest resource hold on the port.
#[tokio::test]
async fn a_hello_without_a_server_name_is_refused_immediately() {
    let (_daemon, ingress_port, _control) = start_daemon_with(Limits {
        handshake_timeout: Duration::from_secs(30),
        ..test_limits()
    })
    .await;

    let mut client = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect");
    client
        .write_all(&test_client_hello_without_sni())
        .await
        .expect("send hello");

    let started = std::time::Instant::now();
    assert!(read_to_eof(&mut client, Duration::from_secs(5))
        .await
        .is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it waited for the handshake deadline instead of deciding"
    );
}

/// The control port is public, and `read_line` grows its target until it sees
/// a newline. Stream it bytes with no newline in them and the daemon must stop
/// reading at its ceiling rather than allocating whatever the peer felt like
/// sending.
#[tokio::test]
async fn a_headerless_stream_at_the_control_port_is_bounded() {
    let (daemon, ingress_port, control_port) = start_daemon().await;

    let mut flood = TcpStream::connect(("127.0.0.1", control_port))
        .await
        .expect("connect");
    let chunk = vec![b'A'; 64 * 1024];
    let mut wrote = 0usize;
    let ceiling = 32 * 1024 * 1024;
    let stopped = tokio::time::timeout(Duration::from_secs(20), async {
        while wrote < ceiling {
            if flood.write_all(&chunk).await.is_err() {
                return true; // the daemon hung up on us
            }
            wrote += chunk.len();
        }
        false
    })
    .await
    .expect("the write loop should not have to be timed out");

    assert!(
        stopped,
        "the daemon accepted {wrote} bytes of a line that has no end"
    );
    assert!(
        wrote < 8 * 1024 * 1024,
        "it swallowed {wrote} bytes before giving up, which is not a bound"
    );

    // And it is still a relay afterwards.
    spawn_fake_agent(control_port, TOKEN)
        .await
        .expect("agent registers");
    until("registration", || daemon.registry.count() == 1).await;

    let hello = test_client_hello(&format!("{ID}.{ZONE}"));
    let mut client = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect");
    client.write_all(&hello).await.expect("send hello");
    let mut got = vec![0u8; hello.len()];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut got))
        .await
        .expect("still routing")
        .expect("read echo");
    assert_eq!(got, hello);
}

/// Concurrency is not arrival rate. Sixty connections per ten seconds is
/// unlimited connections if each one is held open, so there is a ceiling that
/// counts residents.
#[tokio::test]
async fn the_global_ceiling_refuses_the_connection_after_it() {
    let (_daemon, ingress_port, _control) = start_daemon_with(Limits {
        max_ingress_conns: 4,
        handshake_timeout: Duration::from_secs(30),
        ..test_limits()
    })
    .await;

    // Four connections that have said nothing hold every slot: they are inside
    // the handshake deadline, which is where a slow loris lives.
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(
            TcpStream::connect(("127.0.0.1", ingress_port))
                .await
                .expect("connect"),
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut over = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect");
    assert!(
        read_to_eof(&mut over, Duration::from_secs(5))
            .await
            .is_empty(),
        "past the ceiling the relay hangs up instead of queueing"
    );

    drop(held);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut after = TcpStream::connect(("127.0.0.1", ingress_port))
        .await
        .expect("connect");
    after
        .write_all(&test_client_hello(&format!("nobody.{ZONE}")))
        .await
        .expect("send hello");
    // Nothing is registered, so this closes ... but it got a slot, which is
    // the point: the ceiling releases.
    assert!(read_to_eof(&mut after, Duration::from_secs(5))
        .await
        .is_empty());
}
