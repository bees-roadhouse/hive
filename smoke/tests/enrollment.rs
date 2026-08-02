// Unix-only, like the tier itself (the scenarios spawn processes and send
// signals).
#![cfg(unix)]

// ADVERSARIAL-SMOKE begins here (docs/TESTING-STRATEGY.md §2, §4.3, §4.4;
// PLAN-v2.1 PR 4.7).
//
// The convention this file opens: every threat-model amendment maps 1:1 to at
// least one test, or the PR writes down "untestable because X". These are the
// transport-auth claims PR 4.7 makes, each as a scenario against the REAL
// `hive-node` binary — its listener, its mTLS, its `enroll` and `device revoke`
// subcommands, its node-meta — with an IN-PROC device on the other end,
// because a device is what the claims are about:
//
//   happy path        a code mints, a device redeems it, the node pins the
//                     device's keys, and the device comes back on that pin
//   wrong code        a code no domain minted is refused
//   reused code       single-use means once, even for a different device
//   expired code      the TTL is enforced, not merely recorded
//   revoked cert      `device revoke` refuses the NEXT handshake, no restart
//   stale epoch       the control epoch is a high-water mark; a revocation
//                     tombstone survives a reboot and cannot be walked back
//
// Two things every scenario asserts beyond the wire outcome: the AUDIT TRAIL
// (a refusal an operator cannot read afterwards is not auth-failure logging)
// and, where it applies, the CONTROL EPOCH. The wire itself says the same
// sentence for every enrollment refusal on purpose — a redeemer that could
// tell "wrong" from "expired" apart could probe the code space — so the trail
// is where the distinction has to be visible.
//
// Deliberately NOT here (they are 4.12's, and the ledger says so): replaying a
// signed control STATEMENT with a regressed epoch, and folding `device.revoke`
// into a store. There is no control log on the wire in Stage A; the node's
// per-domain high-water mark is the half that exists, and it is what these
// pin.
//
// Harness rules (§4.1): `test_node()` binds `127.0.0.1:0` and the port is
// parsed from the node's own `listening on` line, child output is teed, the
// process is killed on drop, and there is not a bare sleep in the file.

use std::sync::Arc;

use hive_smoke_support::{require_bin, require_smoke, test_domain, test_identity, test_node};
use hive_sync::enroll;
use hive_sync::frame::EnrollGrant;
use hive_sync::tls::{self, DeviceCert, PinSet, UnpinnedEnrollment};
use hive_sync::{push_session, SessionConfig, SpkiPin};
use tokio::net::TcpStream;

/// The one domain every scenario enrolls into. Created before the node boots
/// (the seam does it): a node discovers its vault tree at open, so a domain
/// that appears afterwards is one the running listener cannot serve.
const DOMAIN: &str = "household/example.com";

/// Redeem `code` for `device`, from a fresh mTLS connection.
///
/// This is the device half of the ceremony as PR 4.8's app will run it: dial,
/// present the device's own certificate, send the request, read the verdict.
/// The dialer pins NOTHING on this one connection — it has not been told the
/// node's key yet, and the grant is what tells it ([`UnpinnedEnrollment`]).
/// What authorizes it is the code.
///
/// The pin the handshake landed on is carried into `redeem` rather than
/// discarded: accepting any server key is only safe because the grant is then
/// checked against the key that actually answered.
async fn redeem(
    addr: std::net::SocketAddr,
    identity: &hive_core::identity::DeviceIdentity,
    device: &str,
    code: &str,
) -> anyhow::Result<EnrollGrant> {
    let cert = DeviceCert::self_signed(identity)?;
    let connector = tls::connector(&cert, Arc::new(UnpinnedEnrollment))?;
    let (stream, node_pin) =
        tls::connect_pinned(&connector, TcpStream::connect(addr).await?).await?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let request = enroll::request(code, device, identity)?;
    enroll::redeem(&mut reader, &mut writer, request, &node_pin).await
}

/// Open a session as a device the node has already pinned, pushing an empty
/// store. Empty is the point: what is under test is the AUTHORIZATION, and a
/// session that completes its handshake and its Done exchange has proved the
/// pin on both sides (the data half is PR 4.8's push loop).
async fn open_session(
    addr: std::net::SocketAddr,
    identity: &hive_core::identity::DeviceIdentity,
    node_key: [u8; 32],
    domain: &str,
    device: &str,
) -> anyhow::Result<()> {
    let cert = DeviceCert::self_signed(identity)?;
    let expect = PinSet::one(SpkiPin::from_ed25519_public(&node_key));
    let connector = tls::connector(&cert, Arc::new(expect))?;
    let (stream, _pin) = tls::connect_pinned(&connector, TcpStream::connect(addr).await?).await?;
    let (reader, writer) = tokio::io::split(stream);

    let domain_fixture = test_domain();
    let store = domain_fixture.open_device(device);
    let cfg = SessionConfig::new(domain, device);
    let report = push_session(reader, writer, &store, &cfg).await;
    store.shutdown().await?;
    report?;
    Ok(())
}

/// Every audit `kind` the domain recorded, oldest first.
fn kinds(events: &[(String, String)]) -> Vec<&str> {
    events.iter().map(|(k, _)| k.as_str()).collect()
}

/// The happy path, end to end: the operator's command, the device's
/// redemption, the pin it leaves behind, and the session that pin then buys.
#[tokio::test]
async fn a_device_enrolls_with_a_minted_code_and_comes_back_on_its_pin() {
    require_smoke!();
    let bin = require_bin!("HIVE_NODE_BIN");
    let node = test_node(&bin, &[DOMAIN]).await;
    assert_eq!(node.domains(), [DOMAIN.to_string()]);

    let laptop = test_identity("laptop");
    // Before a code exists there is no way in at all: the pin set is the whole
    // guest list, and it is empty.
    let refused = redeem(node.addr(), &laptop, "dev-laptop", "AAAAAAAAAAAAAAAAAAAAA").await;
    assert!(refused.is_err(), "a closed node admits nobody");

    let code = node.mint_code(DOMAIN).await;
    let grant = redeem(node.addr(), &laptop, "dev-laptop", &code)
        .await
        .expect("the first device enrolls");
    assert_eq!(grant.domain, "example.com");
    assert_eq!(
        grant.node_ed25519_pk,
        node.node_key(),
        "the grant carries the key the device pins from here on"
    );
    assert_eq!(grant.epoch, 1, "an enrollment is a control event");

    // The pin is real: it is in node-meta, and it is what the operator sees.
    let listed = node.device_list(DOMAIN).await;
    assert_eq!(
        listed,
        vec![format!("dev-laptop {} active", SpkiPin::of(&laptop))]
    );

    // And it is what the next connection rides. No code is outstanding now —
    // the door is shut and this device gets in on its pin alone.
    open_session(
        node.addr(),
        &laptop,
        node.node_key(),
        "example.com",
        "dev-laptop",
    )
    .await
    .expect("a pinned device opens a session");

    let events = node.auth_events(DOMAIN);
    assert_eq!(
        kinds(&events),
        ["code_minted", "enrolled", "session_opened"],
        "{events:?}"
    );
    assert!(node.terminate().await.success(), "SIGTERM is a clean exit");
}

/// Wrong, expired, and reused, at the level a stranger meets them. Each leaves
/// a line in the operator's trail naming WHICH refusal it was, while the wire
/// says the same sentence every time.
///
/// The order is chosen so each refusal is reached by its OWN rule rather than
/// by an earlier one — which is itself worth reading, because the earlier
/// rules are the stricter ones:
///
///   * with no code outstanding at all, the door is shut and a stranger cannot
///     complete a handshake (asserted in the happy-path scenario);
///   * with a device already enrolled, v1's first-device rule refuses every
///     redemption before the code is examined (PR 4.14 is the second-device
///     ceremony), so a reused code is only reachable as a reused code once a
///     revocation has cleared that rule.
#[tokio::test]
async fn a_wrong_expired_or_reused_code_is_refused() {
    require_smoke!();
    let bin = require_bin!("HIVE_NODE_BIN");
    let node = test_node(&bin, &[DOMAIN]).await;
    let laptop = test_identity("laptop");
    let stranger = test_identity("stranger");

    // A live code opens the door for unpinned keys — that is the only way a
    // key nobody has seen can be enrolled at all — so the refusals below are
    // reached by POLICY, which is where they belong.
    let _door = node.mint_code(DOMAIN).await;

    // WRONG: a code no domain on this node minted.
    let err = redeem(
        node.addr(),
        &stranger,
        "dev-stranger",
        "AAAAAAAAAAAAAAAAAAAAA",
    )
    .await
    .expect_err("a code no domain minted is refused");
    assert!(format!("{err:#}").contains("enrollment refused"), "{err:#}");

    // EXPIRED: a real code of this domain's, aged past its TTL without waiting
    // ten minutes for it. Ageing takes every outstanding code with it — the
    // door included — so a fresh one has to hold the door open, or this would
    // be refused at the handshake and the expiry would go untested.
    let stale = node.mint_code(DOMAIN).await;
    assert_eq!(node.expire_codes(DOMAIN), 2, "both outstanding codes aged");
    let _door = node.mint_code(DOMAIN).await;
    let err = redeem(node.addr(), &stranger, "dev-stranger", &stale)
        .await
        .expect_err("an expired code is refused");
    assert!(format!("{err:#}").contains("enrollment refused"), "{err:#}");

    // A fresh code, and the first device enrolls on it.
    let first = node.mint_code(DOMAIN).await;
    let grant = redeem(node.addr(), &laptop, "dev-laptop", &first)
        .await
        .expect("the first device enrolls");
    assert_eq!(grant.epoch, 1);

    // REUSED: a spent code is spent. Revoking the laptop first is what takes
    // the stricter first-device rule out of the way — otherwise this would be
    // refused for being a second device and the single-use property would go
    // untested here.
    assert_eq!(node.revoke(DOMAIN, "dev-laptop").await, 2);
    let unspent = node.mint_code(DOMAIN).await;
    let err = redeem(node.addr(), &stranger, "dev-stranger", &first)
        .await
        .expect_err("a spent code is spent");
    assert!(format!("{err:#}").contains("enrollment refused"), "{err:#}");

    // The trail is where the three are told apart. The wire never
    // distinguished them: it said "enrollment refused" every time.
    let events = node.auth_events(DOMAIN);
    assert_eq!(
        kinds(&events),
        [
            "code_minted",    // the door
            "enroll_refused", // wrong
            "code_minted",    // the one that gets aged
            "code_minted",    // the door again, after the ageing took the first
            "enroll_refused", // expired
            "code_minted",    // the one that works
            "enrolled",
            "device_revoked",
            "code_minted",    // the door once more
            "enroll_refused", // reused
        ],
        "{events:?}"
    );
    let refusals: Vec<&String> = events
        .iter()
        .filter(|(k, _)| k == "enroll_refused")
        .map(|(_, detail)| detail)
        .collect();
    assert!(refusals[0].contains("no domain"), "{refusals:?}");
    assert!(refusals[1].contains("Expired"), "{refusals:?}");
    assert!(refusals[2].contains("AlreadyRedeemed"), "{refusals:?}");

    // Nothing but the (now revoked) laptop was ever pinned, and the code that
    // opened the last door is still unspent.
    assert_eq!(
        node.device_list(DOMAIN).await,
        vec![format!("dev-laptop {} revoked", SpkiPin::of(&laptop))]
    );
    assert!(
        !unspent.is_empty(),
        "the code that opened the last door was minted, never redeemed"
    );
    assert!(node.terminate().await.success());
}

/// Revoked-cert reconnect (ledger §4.4): the connection is refused on the NEXT
/// handshake, with nothing restarted — the guest list is node-meta, re-read
/// per connection, which is the whole reason `PinnedKeys` is a trait.
#[tokio::test]
async fn a_revoked_device_cannot_reconnect_and_cannot_come_back() {
    require_smoke!();
    let bin = require_bin!("HIVE_NODE_BIN");
    let node = test_node(&bin, &[DOMAIN]).await;
    let laptop = test_identity("laptop");

    let code = node.mint_code(DOMAIN).await;
    let grant = redeem(node.addr(), &laptop, "dev-laptop", &code)
        .await
        .expect("enrolled");
    assert_eq!(grant.epoch, 1);
    open_session(
        node.addr(),
        &laptop,
        node.node_key(),
        "example.com",
        "dev-laptop",
    )
    .await
    .expect("a pinned device opens a session");

    // The operator revokes it. Same process, same socket, no restart.
    assert_eq!(
        node.revoke(DOMAIN, "dev-laptop").await,
        2,
        "a revocation is a control event"
    );
    let err = open_session(
        node.addr(),
        &laptop,
        node.node_key(),
        "example.com",
        "dev-laptop",
    )
    .await
    .expect_err("a revoked device is refused");
    // Not asserted on text: in TLS 1.3 a rejected dialer's `connect` can
    // return before the listener has judged its certificate, so the refusal
    // surfaces as either a handshake error or a dead stream (the same reason
    // sync/tests/tls_loopback.rs defines "refused" as "could not exchange
    // data"). What matters is that no session opened, which the trail says.
    eprintln!("[refused as expected] {err:#}");
    assert_eq!(
        kinds(&node.auth_events(DOMAIN))
            .iter()
            .filter(|k| **k == "session_opened")
            .count(),
        1,
        "exactly the one session from before the revocation"
    );

    // The tombstone is permanent: a FRESH code cannot walk the same device
    // back in, by id or by key.
    let fresh = node.mint_code(DOMAIN).await;
    let err = redeem(node.addr(), &laptop, "dev-laptop", &fresh)
        .await
        .expect_err("a revoked device cannot be re-enrolled");
    assert!(format!("{err:#}").contains("enrollment refused"), "{err:#}");
    let err = redeem(node.addr(), &laptop, "dev-laptop-again", &fresh)
        .await
        .expect_err("nor under a new name with the same key");
    assert!(format!("{err:#}").contains("enrollment refused"), "{err:#}");

    // A REPLACEMENT device still enrolls — the lost-laptop path has to keep
    // working, or a revocation would strand the domain.
    let replacement = test_identity("replacement");
    let grant = redeem(node.addr(), &replacement, "dev-replacement", &fresh)
        .await
        .expect("a replacement device enrolls after a revocation");
    assert_eq!(grant.epoch, 3, "enroll, revoke, enroll — never backwards");

    let listed = node.device_list(DOMAIN).await;
    assert_eq!(
        listed,
        vec![
            format!("dev-laptop {} revoked", SpkiPin::of(&laptop)),
            format!("dev-replacement {} active", SpkiPin::of(&replacement)),
        ],
        "a revoked device is a retained tombstone, never a deletion"
    );
    assert!(node.terminate().await.success());
}

/// Stale-epoch regression (ledger §4.4, the 4.7 half): the control epoch is a
/// per-domain HIGH-WATER MARK. It survives a reboot, it only ever rises, and a
/// revocation recorded under it cannot be walked back by anything a peer says.
///
/// The wire half — replaying a signed control statement that carries an older
/// epoch — lands with the control log at 4.12; there is no such statement on
/// the wire in Stage A. What exists now is the mark itself, and this pins it.
#[tokio::test]
async fn the_control_epoch_never_regresses_across_a_reboot() {
    require_smoke!();
    let bin = require_bin!("HIVE_NODE_BIN");
    let node = test_node(&bin, &[DOMAIN]).await;
    let laptop = test_identity("laptop");

    let code = node.mint_code(DOMAIN).await;
    redeem(node.addr(), &laptop, "dev-laptop", &code)
        .await
        .expect("enrolled");
    assert_eq!(node.revoke(DOMAIN, "dev-laptop").await, 2);

    // A second revoke is refused rather than counted: the tombstone is already
    // there, and a bump with no event behind it would be a lie about ordering.
    let out = node
        .run(&[
            "device",
            "revoke",
            "--domain",
            DOMAIN,
            "--device",
            "dev-laptop",
        ])
        .await;
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already revoked"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Reboot the node on the same root. Nothing about the epoch, the pins, or
    // the tombstone is in memory — it is all node-meta, and node-meta is what
    // an operator backs up.
    let node = node.reboot().await;
    assert_eq!(node.domains(), [DOMAIN.to_string()]);
    assert_eq!(
        node.device_list(DOMAIN).await,
        vec![format!("dev-laptop {} revoked", SpkiPin::of(&laptop))],
        "the tombstone survived the reboot"
    );
    let out = node.run(&["device", "list", "--domain", DOMAIN]).await;
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("control epoch 2"),
        "the high-water mark survived with it: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // And the revocation still holds on the far side of the restart: a
    // rebooted node re-reads the tombstone, so the revoked key is refused by
    // the handshake exactly as it was before.
    let err = open_session(
        node.addr(),
        &laptop,
        node.node_key(),
        "example.com",
        "dev-laptop",
    )
    .await
    .expect_err("a revocation is not undone by a restart");
    eprintln!("[refused as expected] {err:#}");
    assert!(node.terminate().await.success());
}
