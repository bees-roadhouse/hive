// The tls_loopback harness (PLAN-v2.1 PR 4.5, docs/TESTING-STRATEGY.md §4.3):
// two throwaway device identities, a real rustls handshake between them, and
// an assertion about who got in.
//
// This file is the spike's deliverable and every later transport PR's
// substrate: PR 4.7's enrollment/revocation cases, PR 4.8's impostor-candidate
// scenario, and PR 4.14's second-device ceremony all reduce to "mint
// identities, decide what each side pins, attempt". So the harness is one
// `attempt(...)` function over a `Party`, and the cases are three lines each.
//
// Two deliberate shapes:
//
//   * The transport is an in-memory duplex, not a socket. The handshake is
//     the subject, and TLS over `AsyncRead + AsyncWrite` is exactly what a
//     socket would give it — so these run in the ordinary `cargo test`
//     (hermetic, no ports, no LOUD-SKIP gate) while `tests/loopback.rs`
//     stays the smoke-tier scenario.
//   * Every attempt goes past the handshake to a byte round-trip. In TLS 1.3
//     the client finishes before the server has judged its certificate, so a
//     rejected dialer can see `connect` succeed and only learn the truth on
//     its first read. "Refused" therefore means "could not exchange data",
//     which is the property the protocol actually depends on.

use std::sync::Arc;

use hive_smoke_support::test_identity;
use hive_sync::tls::{self, DeviceCert, PinSet, PinnedKeys, SpkiPin};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One end of an attempted connection: the certificate it presents and the
/// keys it will accept.
struct Party {
    cert: DeviceCert,
    pins: Arc<dyn PinnedKeys>,
}

impl Party {
    /// A device that presents `name`'s key and accepts exactly `expect`.
    fn new(name: &str, expect: impl IntoIterator<Item = SpkiPin>) -> Self {
        let cert = DeviceCert::self_signed(&test_identity(name)).expect("mint device cert");
        Self {
            cert,
            pins: Arc::new(PinSet::new(expect)),
        }
    }

    fn pin(&self) -> SpkiPin {
        self.cert.pin()
    }
}

/// The pin of a named device, as a peer would have stored it at enrollment —
/// from the bare public key, with no certificate involved.
fn pin_of(name: &str) -> SpkiPin {
    SpkiPin::of(&test_identity(name))
}

/// One connection attempt over an in-memory duplex. Returns what each side
/// learned: the peer's pin on success, an error on refusal.
///
/// Both halves run in their own task — a half that gives up drops its end of
/// the duplex, which is the EOF the other half needs to stop waiting (the
/// same discipline `tests/loopback.rs` uses for sessions).
async fn attempt(
    dialer: Party,
    listener: Party,
) -> (anyhow::Result<SpkiPin>, anyhow::Result<SpkiPin>) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);

    let client = tokio::spawn(async move {
        let connector = tls::connector(&dialer.cert, dialer.pins.clone())?;
        let (mut stream, peer) = tls::connect_pinned(&connector, client_io).await?;
        stream.write_all(b"ping").await?;
        stream.flush().await?;
        let mut pong = [0u8; 4];
        stream.read_exact(&mut pong).await?;
        anyhow::ensure!(&pong == b"pong", "unexpected reply {pong:?}");
        Ok::<_, anyhow::Error>(peer)
    });

    let server = tokio::spawn(async move {
        let acceptor = tls::acceptor(&listener.cert, listener.pins.clone())?;
        let (mut stream, peer) = tls::accept_pinned(&acceptor, server_io).await?;
        let mut ping = [0u8; 4];
        stream.read_exact(&mut ping).await?;
        anyhow::ensure!(&ping == b"ping", "unexpected greeting {ping:?}");
        stream.write_all(b"pong").await?;
        stream.flush().await?;
        Ok::<_, anyhow::Error>(peer)
    });

    // Bounded, because a refusal that deadlocked instead of erroring would
    // otherwise hang CI rather than fail it (the tier's no-unbounded-waits
    // rule, docs/TESTING-STRATEGY.md §4.1). Ten seconds is ~1000x the time a
    // loopback handshake takes.
    let both = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(client, server)
    });
    let (client, server) = both.await.expect("both halves settle within 10s");
    (client.expect("dialer task"), server.expect("listener task"))
}

/// Pinned-SPKI accept: each side pinned the other's key at enrollment, the
/// handshake completes, and both learn exactly which device they are talking
/// to — the fact PR 4.7 scopes a session with.
#[tokio::test]
async fn a_mutually_pinned_pair_completes_the_handshake_and_names_each_other() {
    let dialer = Party::new("laptop", [pin_of("node")]);
    let listener = Party::new("node", [pin_of("laptop")]);
    let (dialer_pin, listener_pin) = (dialer.pin(), listener.pin());

    let (client, server) = attempt(dialer, listener).await;
    assert_eq!(
        client.expect("dialer completes"),
        listener_pin,
        "the dialer learns which node answered"
    );
    assert_eq!(
        server.expect("listener completes"),
        dialer_pin,
        "the listener learns which device connected"
    );
}

/// Unknown cert reject, listener side: an un-enrolled device presents a
/// perfectly valid certificate for a key nobody pinned. This is the shape of
/// every "revoked device reconnects" case PR 4.7 adds — the pin set is simply
/// asked and answers no.
#[tokio::test]
async fn an_unpinned_device_cannot_get_past_the_listener() {
    let stranger = Party::new("stranger", [pin_of("node")]);
    let listener = Party::new("node", [pin_of("laptop")]);

    let (client, server) = attempt(stranger, listener).await;
    let err = format!("{:#}", server.expect_err("listener refuses"));
    assert!(err.contains("listener side"), "{err}");
    assert!(
        client.is_err(),
        "and the stranger cannot exchange a byte either"
    );
}

/// Unknown cert reject, dialer side: the listener answering is not the node
/// this device enrolled with. Pinning is what makes PR 4.8's spoofed SRV/mDNS
/// answer a dead end instead of a redirect — the addressing layer is never
/// the authentication layer (D35/delta 13).
#[tokio::test]
async fn a_dialer_refuses_an_impostor_listener() {
    let dialer = Party::new("laptop", [pin_of("node")]);
    // The impostor accepts anyone, so the refusal can only come from the
    // dialer's own pin check.
    let impostor = Party::new("impostor", [pin_of("laptop")]);

    let (client, server) = attempt(dialer, impostor).await;
    let err = format!("{:#}", client.expect_err("dialer refuses"));
    assert!(err.contains("dialer side"), "{err}");
    assert!(
        server.is_err(),
        "and the impostor gets no session out of it"
    );
}

/// An empty pin set is a closed door, not an open one — the state a node is
/// in before its first enrollment (PR 4.7), and the one where a fail-open bug
/// would be catastrophic and silent.
#[tokio::test]
async fn a_listener_with_no_pinned_devices_accepts_nobody() {
    let dialer = Party::new("laptop", [pin_of("node")]);
    let listener = Party::new("node", []);

    let (client, server) = attempt(dialer, listener).await;
    assert!(server.is_err(), "no pins means no peers");
    assert!(client.is_err());
}

/// The name is not the identity: the dialer sends a fixed reserved SNI name
/// that matches nothing in the listener's certificate, and the connection
/// succeeds anyway because the key is what was checked. Pinned here because
/// it looks like a bug to anyone reading the handshake code later.
#[tokio::test]
async fn the_certificate_name_plays_no_part_in_the_verdict() {
    assert!(
        tls::PINNED_SERVER_NAME.ends_with(".invalid"),
        "the SNI name resolves nowhere on purpose"
    );
    let (client, _) = attempt(
        Party::new("laptop", [pin_of("node")]),
        Party::new("node", [pin_of("laptop")]),
    )
    .await;
    client.expect("a name nothing vouches for is still a completed handshake");
}
