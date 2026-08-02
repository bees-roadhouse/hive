// Enrollment policy (PLAN-v2.1 PR 4.7, D30): who the node lets in, and on
// what evidence.
//
// The wire half of the ceremony is `hive_sync::enroll` (code format, at-rest
// hash, the redemption exchange). What is decided HERE is everything a peer
// does not get a vote on:
//
//   * ONE-TIME. A code is spent by the single UPDATE in
//     `NodeMeta::redeem_code`, conditional on still being unredeemed and
//     unexpired, so two devices racing the same code cannot both win.
//   * SHORT-LIVED. [`CODE_TTL`] is ten minutes — long enough to walk to the
//     other machine, short enough that a code left in a terminal scrollback
//     is not a standing invitation.
//   * HASHED AT REST. `node-meta.db` holds blake3 of the code and never the
//     code, so a stolen database is not a live credential.
//   * CHANNEL-BOUND. The ed25519 key in the request must be the key the TLS
//     certificate on THIS connection carried. Without that check a relay could
//     redeem someone else's code with keys of its own choosing — the exact
//     substitution D30's short-auth-string exists to prevent — and it is why
//     v1 can be node-solo at all.
//   * FIRST DEVICE ONLY. v1 enrolls the domain's first, already-master-holding
//     device: no master crosses the wire, so there is nothing for a node to
//     substitute keys in front of. A SECOND device needs the SAS ceremony
//     (PR 4.14) and is refused here rather than quietly approved node-solo,
//     which D30 permits only under escrowed custody — a tier v2.1 does not
//     ship.
//   * TOMBSTONES ARE PERMANENT. A revoked device id — and a revoked KEY under
//     any id — can never be re-enrolled by a fresh code. Un-revoking is a
//     thing an operator must do deliberately, and there is deliberately no
//     command for it.
//
// Every decision here, granted or refused, is written to the domain's audit
// trail before it is answered. The refusal a peer SEES is always the same
// sentence: a redeemer who could tell "expired" from "wrong" apart could probe
// the code space, and the distinction belongs in the operator's log.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use hive_core::oplog::device_id_ok;
use hive_sync::enroll::{code_hash, encode_code, format_code, normalize_code, CODE_BYTES};
use hive_sync::frame::{EnrollGrant, EnrollRequest, PROTO_VERSION};
use hive_sync::SpkiPin;
use rand::rngs::OsRng;
use rand::RngCore;

use crate::meta::{AuthEventKind, CodeVerdict};
use crate::vault::SegmentVault;

/// How long a minted code stays redeemable (D30: short-TTL).
pub const CODE_TTL: Duration = Duration::from_secs(10 * 60);

/// A freshly minted code. The plaintext exists here and on the operator's
/// terminal, and nowhere else — the node stored its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedCode {
    /// The code as typed: [`hive_sync::enroll::CODE_LEN`] base32 characters.
    pub code: String,
    /// The same code, grouped for reading aloud.
    pub display: String,
    /// When it stops being redeemable.
    pub expires_at: SystemTime,
}

/// Mint a one-time code for `vault`'s domain.
///
/// The randomness is the OS RNG, which is the one place this crate samples
/// anything: everything downstream of the code is derivation and comparison.
pub fn mint(vault: &SegmentVault, now: SystemTime) -> Result<MintedCode> {
    let mut entropy = [0u8; CODE_BYTES];
    OsRng.fill_bytes(&mut entropy);
    let code = encode_code(&entropy);
    let expires_at = now + CODE_TTL;
    vault
        .meta()
        .mint_code(&code_hash(&code), unix_seconds(expires_at)?)?;
    vault.meta().record_auth_event(
        AuthEventKind::CodeMinted,
        None,
        None,
        &format!(
            "an enrollment code was minted for {}/{}, valid {} minutes",
            vault.tenant(),
            vault.domain(),
            CODE_TTL.as_secs() / 60
        ),
    )?;
    Ok(MintedCode {
        display: format_code(&code),
        code,
        expires_at,
    })
}

/// Redeem a code: check everything, spend it, pin the device, raise the epoch.
///
/// `peer_pin` is the pin of the certificate on the connection the request
/// arrived over — not something the request claims. An `Err` is a refusal; the
/// caller turns it into the one generic wire error and has already written the
/// specific reason to the audit trail.
pub fn redeem(
    vault: &SegmentVault,
    node_ed25519_pk: [u8; 32],
    request: &EnrollRequest,
    peer_pin: SpkiPin,
    now: SystemTime,
) -> Result<EnrollGrant> {
    let meta = vault.meta();
    let pin_hex = peer_pin.to_string();
    let refuse = |why: String| -> anyhow::Error {
        // The audit write must not swallow the refusal it is describing.
        if let Err(e) = meta.record_auth_event(
            AuthEventKind::EnrollRefused,
            Some(request.device.as_str()),
            Some(pin_hex.as_str()),
            &why,
        ) {
            tracing::error!("could not record an enrollment refusal ({why}): {e:#}");
        }
        anyhow!(why)
    };

    if request.proto != PROTO_VERSION {
        return Err(refuse(format!(
            "enrollment request speaks proto v{}, this build serves v{PROTO_VERSION}",
            request.proto
        )));
    }
    // The device id becomes a directory name under the vault the moment this
    // device pushes anything, so it is checked here, once, against the frozen
    // allowlist rather than at every later path join.
    if !device_id_ok(&request.device) {
        return Err(refuse(format!(
            "enrollment names device {:?}, outside the frozen device-id allowlist",
            request.device
        )));
    }
    // CHANNEL BINDING. The key being enrolled must be the key that terminated
    // this TLS connection.
    if SpkiPin::from_ed25519_public(&request.ed25519_pk) != peer_pin {
        return Err(refuse(format!(
            "enrollment for device {:?} presents a key the connection does not hold — \
             the request was relayed, not made",
            request.device
        )));
    }

    let pins = meta.device_pins()?;
    // FIRST DEVICE ONLY (see the module header): node-solo approval is v1's
    // whole ceremony, and it is only sound while there is no master to
    // substitute in front of.
    if let Some(existing) = pins.iter().find(|p| !p.revoked) {
        return Err(refuse(format!(
            "domain {}/{} already has an enrolled device ({:?}); a second device needs the \
             short-auth-string ceremony (PLAN-v2.1 PR 4.14), which this build does not serve",
            vault.tenant(),
            vault.domain(),
            existing.device
        )));
    }
    // TOMBSTONES ARE PERMANENT — by id and by key. A revoked device that
    // talked its way into a fresh code must not be able to walk back in.
    if let Some(prior) = pins.iter().find(|p| p.device == request.device) {
        return Err(refuse(format!(
            "device {:?} was enrolled here before (revoked: {}); a revocation is a permanent \
             tombstone, and re-admitting a device is not something a code can do",
            request.device, prior.revoked
        )));
    }
    if let Some(prior) = pins.iter().find(|p| p.ed25519_pk == request.ed25519_pk) {
        return Err(refuse(format!(
            "the key offered for device {:?} is already on file under {:?} (revoked: {})",
            request.device, prior.device, prior.revoked
        )));
    }

    // Everything the operator could still fix is checked; NOW spend the code.
    let hash = code_hash(&normalize_code(&request.code)?);
    match meta.redeem_code(&hash, unix_seconds(now)?, &request.device)? {
        CodeVerdict::Redeemed => {}
        verdict => {
            return Err(refuse(format!(
                "enrollment code refused for device {:?}: {verdict:?}",
                request.device
            )))
        }
    }

    // The row stores the KEYS; the SPKI pin the transport compares is
    // `SpkiPin::from_ed25519_public` of the first of them — a derivation, not
    // a second copy, so the two can never disagree (sync/src/tls.rs pins the
    // equality of the two constructions). The x25519 half is pinned now and
    // used at PR 4.14, where a second device's master-wrap is sealed to it.
    meta.pin_device(&request.device, &request.ed25519_pk, &request.x25519_pk)?;
    let epoch = meta.set_control_epoch(meta.control_epoch()? + 1)?;
    meta.record_auth_event(
        AuthEventKind::Enrolled,
        Some(&request.device),
        Some(&pin_hex),
        &format!(
            "device {:?} enrolled into {}/{} at control epoch {epoch}",
            request.device,
            vault.tenant(),
            vault.domain()
        ),
    )?;
    Ok(EnrollGrant {
        domain: vault.domain().to_string(),
        node_ed25519_pk,
        epoch,
    })
}

/// Revoke a device: unpin it (as a tombstone, never a deletion) and raise the
/// control epoch.
///
/// The epoch bump is what makes the revocation ORDERED. The tombstone is what
/// makes it permanent. Returns the new epoch.
pub fn revoke(vault: &SegmentVault, device: &str) -> Result<u64> {
    let meta = vault.meta();
    let Some(pin) = meta.device_pin(device)? else {
        bail!(
            "domain {}/{} has no device {device:?} to revoke",
            vault.tenant(),
            vault.domain()
        );
    };
    if pin.revoked {
        bail!(
            "device {device:?} is already revoked in {}/{} (a tombstone is permanent)",
            vault.tenant(),
            vault.domain()
        );
    }
    meta.revoke_device(device)?;
    let epoch = meta.set_control_epoch(meta.control_epoch()? + 1)?;
    meta.record_auth_event(
        AuthEventKind::DeviceRevoked,
        Some(device),
        Some(&SpkiPin::from_ed25519_public(&pin.ed25519_pk).to_string()),
        &format!(
            "device {device:?} revoked from {}/{} at control epoch {epoch}",
            vault.tenant(),
            vault.domain()
        ),
    )?;
    Ok(epoch)
}

/// Seconds since the epoch, as node-meta stores them.
fn unix_seconds(at: SystemTime) -> Result<i64> {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .context("a clock before 1970 cannot express an enrollment TTL")?
        .as_secs();
    i64::try_from(secs).context("a clock past year 292277026596")
}

#[cfg(test)]
mod tests {
    use super::*;

    use hive_core::identity::DeviceIdentity;

    /// A vault plus the node key a grant names. Deterministic identities, so a
    /// failing case names the same keys on every rerun.
    fn rig() -> (tempfile::TempDir, SegmentVault, [u8; 32]) {
        let root = tempfile::tempdir().expect("node root");
        let vault =
            SegmentVault::open(root.path(), "household", "bierlysmith.com").expect("open vault");
        let node = DeviceIdentity::from_seed(&[0x4e; 32]);
        (root, vault, node.ed25519_public())
    }

    fn device(seed: u8) -> DeviceIdentity {
        DeviceIdentity::from_seed(&[seed; 32])
    }

    fn request_for(code: &str, name: &str, id: &DeviceIdentity) -> EnrollRequest {
        EnrollRequest {
            proto: PROTO_VERSION,
            code: code.to_string(),
            device: name.to_string(),
            ed25519_pk: id.ed25519_public(),
            x25519_pk: id.x25519_public(),
        }
    }

    fn now() -> SystemTime {
        // A fixed instant, so nothing here depends on the wall clock.
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    #[test]
    fn the_first_device_enrolls_and_the_epoch_moves() {
        let (_root, vault, node_pk) = rig();
        let laptop = device(1);
        let minted = mint(&vault, now()).unwrap();
        assert_eq!(vault.meta().control_epoch().unwrap(), 0);

        let grant = redeem(
            &vault,
            node_pk,
            &request_for(&minted.code, "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .expect("the first device enrolls");
        assert_eq!(grant.domain, "bierlysmith.com");
        assert_eq!(grant.node_ed25519_pk, node_pk, "the key the device pins");
        assert_eq!(grant.epoch, 1, "an enrollment is a control event");

        let pin = vault.meta().device_pin("dev-laptop").unwrap().unwrap();
        assert_eq!(pin.ed25519_pk, laptop.ed25519_public());
        assert_eq!(pin.x25519_pk, laptop.x25519_public());
        assert!(!pin.revoked);
    }

    /// The three code refusals, each with the code spent (or not) as it
    /// should be. ADVERSARIAL-SMOKE's unit half.
    #[test]
    fn wrong_expired_and_reused_codes_are_all_refused() {
        let (_root, vault, node_pk) = rig();
        let laptop = device(1);
        let minted = mint(&vault, now()).unwrap();

        // Wrong code: never minted here.
        let err = redeem(
            &vault,
            node_pk,
            &request_for("AAAAAAAAAAAAAAAAAAAAA", "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Unknown"), "{err:#}");

        // Expired: one second past the TTL.
        let err = redeem(
            &vault,
            node_pk,
            &request_for(&minted.code, "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now() + CODE_TTL + Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Expired"), "{err:#}");
        assert!(
            vault.meta().device_pin("dev-laptop").unwrap().is_none(),
            "a refused enrollment pins nothing"
        );

        // Inside the window it works — once.
        redeem(
            &vault,
            node_pk,
            &request_for(&minted.code, "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .expect("still live");
        // ...and the second use is refused for BOTH reasons; the first-device
        // rule fires before the code is even looked at, which is the stricter
        // of the two and the one an operator needs to read.
        let err = redeem(
            &vault,
            node_pk,
            &request_for(&minted.code, "dev-second", &device(2)),
            SpkiPin::of(&device(2)),
            now(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("PR 4.14"), "{err:#}");
    }

    /// A reused code with no first-device rule in the way (the device that
    /// held it was revoked) is still refused: single-use is a property of the
    /// code, not a side effect of the policy above it.
    #[test]
    fn a_spent_code_stays_spent_after_a_revocation() {
        let (_root, vault, node_pk) = rig();
        let laptop = device(1);
        let minted = mint(&vault, now()).unwrap();
        redeem(
            &vault,
            node_pk,
            &request_for(&minted.code, "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .unwrap();
        revoke(&vault, "dev-laptop").unwrap();

        let err = redeem(
            &vault,
            node_pk,
            &request_for(&minted.code, "dev-replacement", &device(3)),
            SpkiPin::of(&device(3)),
            now(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("AlreadyRedeemed"), "{err:#}");

        // A FRESH code enrolls the replacement — this is the lost-laptop path,
        // and it must keep working or a revocation would strand the domain.
        let second = mint(&vault, now()).unwrap();
        let grant = redeem(
            &vault,
            node_pk,
            &request_for(&second.code, "dev-replacement", &device(3)),
            SpkiPin::of(&device(3)),
            now(),
        )
        .expect("a replacement device enrolls after a revocation");
        assert_eq!(grant.epoch, 3, "enroll, revoke, enroll — never backwards");
    }

    /// The tombstone half, by id and by key. Neither a revoked device id nor a
    /// revoked KEY may come back, however fresh the code.
    #[test]
    fn a_revoked_device_cannot_be_re_enrolled() {
        let (_root, vault, node_pk) = rig();
        let laptop = device(1);
        let code = mint(&vault, now()).unwrap();
        redeem(
            &vault,
            node_pk,
            &request_for(&code.code, "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .unwrap();
        assert_eq!(revoke(&vault, "dev-laptop").unwrap(), 2);

        let fresh = mint(&vault, now()).unwrap();
        let err = redeem(
            &vault,
            node_pk,
            &request_for(&fresh.code, "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("tombstone"), "{err:#}");

        // Same key, a new name: refused on the key.
        let err = redeem(
            &vault,
            node_pk,
            &request_for(&fresh.code, "dev-laptop-again", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("already on file"), "{err:#}");
    }

    /// The channel binding: a relay holding a valid code but a key of its own
    /// cannot enroll the key it is claiming to carry.
    #[test]
    fn a_relayed_request_cannot_enroll_keys_it_does_not_hold() {
        let (_root, vault, node_pk) = rig();
        let victim = device(1);
        let relay = device(9);
        let minted = mint(&vault, now()).unwrap();

        let err = redeem(
            &vault,
            node_pk,
            // The request names the victim's keys...
            &request_for(&minted.code, "dev-laptop", &victim),
            // ...but the connection was terminated by the relay's.
            SpkiPin::of(&relay),
            now(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("relayed"), "{err:#}");
        assert_eq!(
            vault
                .meta()
                .live_codes(unix_seconds(now()).unwrap())
                .unwrap(),
            1,
            "a refusal before the spend leaves the code for the real device"
        );
    }

    #[test]
    fn a_traversing_device_id_never_reaches_a_path() {
        let (_root, vault, node_pk) = rig();
        let laptop = device(1);
        let minted = mint(&vault, now()).unwrap();
        let err = redeem(
            &vault,
            node_pk,
            &request_for(&minted.code, "../../etc", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("allowlist"), "{err:#}");
    }

    #[test]
    fn revoking_names_what_it_cannot_do() {
        let (_root, vault, node_pk) = rig();
        let err = revoke(&vault, "dev-nobody").unwrap_err();
        assert!(format!("{err:#}").contains("no device"), "{err:#}");

        let laptop = device(1);
        let minted = mint(&vault, now()).unwrap();
        redeem(
            &vault,
            node_pk,
            &request_for(&minted.code, "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .unwrap();
        revoke(&vault, "dev-laptop").unwrap();
        let err = revoke(&vault, "dev-laptop").unwrap_err();
        assert!(format!("{err:#}").contains("already revoked"), "{err:#}");
    }

    /// Every decision leaves a line an operator can read afterwards.
    #[test]
    fn the_audit_trail_records_the_grant_and_the_refusal() {
        let (_root, vault, node_pk) = rig();
        let laptop = device(1);
        let minted = mint(&vault, now()).unwrap();
        let _ = redeem(
            &vault,
            node_pk,
            &request_for("AAAAAAAAAAAAAAAAAAAAA", "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now(),
        );
        redeem(
            &vault,
            node_pk,
            &request_for(&minted.code, "dev-laptop", &laptop),
            SpkiPin::of(&laptop),
            now(),
        )
        .unwrap();
        revoke(&vault, "dev-laptop").unwrap();

        let kinds: Vec<String> = vault
            .meta()
            .auth_events()
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                "code_minted",
                "enroll_refused",
                "enrolled",
                "device_revoked"
            ]
        );
    }

    #[test]
    fn a_minted_code_is_not_stored_and_is_readable_aloud() {
        let (_root, vault, _pk) = rig();
        let minted = mint(&vault, now()).unwrap();
        assert_eq!(minted.code.len(), hive_sync::enroll::CODE_LEN);
        assert_eq!(normalize_code(&minted.display).unwrap(), minted.code);
        assert_eq!(minted.expires_at, now() + CODE_TTL);
        assert_ne!(
            mint(&vault, now()).unwrap().code,
            minted.code,
            "two mints are two codes"
        );
    }
}
