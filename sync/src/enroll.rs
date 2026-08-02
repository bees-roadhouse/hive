// The enrollment ceremony, client half (PLAN-v2.1 PR 4.7, D30): how a device
// that holds a one-time code becomes a device the node pins.
//
// What lives HERE is the protocol — the code's shape, its normalization, the
// hash a node stores instead of the code, and the six lines that redeem one.
// What lives in `hive_node::enroll` is the POLICY: who may redeem, how long a
// code lives, what a redemption pins, and what it does to the control epoch.
// The split follows the dependency arrow: the desktop app redeems (PR 4.8) and
// must not link the node crate, while the node is the only thing that ever
// mints.
//
// The v1 ceremony is node-solo and enrolls the domain's FIRST device — the one
// that already holds the master, so nothing secret crosses the wire and D30's
// SAS rule is not in play. The second-device ceremony (a mandatory short auth
// string over {new ed25519_pk, x25519_pk, node_pk}, compared on both screens,
// master relayed as a sealed box) is PR 4.14, and the node refuses a second
// enrollment until it lands rather than quietly doing the escrow version.
//
// Three properties the code format is chosen for:
//
//   * ~100 bits from the OS RNG. A code is a bearer credential for ten
//     minutes; guessing must be hopeless, not merely rate-limited.
//   * Crockford-free base32 over the same alphabet the recovery code uses
//     (`data_encoding::BASE32_NOPAD`), so the tree has ONE way of writing a
//     secret a human retypes.
//   * Normalized before hashing — case-folded, dashes and spaces dropped — so
//     the grouping that makes it readable on the node's terminal is not part
//     of the secret.

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::frame::{
    read_frame_capped, write_frame, EnrollGrant, EnrollRequest, Frame, MAX_OPENING_FRAME,
    PROTO_VERSION,
};
use crate::tls::SpkiPin;

/// Bytes of entropy behind one code. 13 bytes is 104 bits, which encodes to 21
/// base32 characters with no padding — short enough to read aloud, far past
/// anything a ten-minute window could be brute-forced through.
pub const CODE_BYTES: usize = 13;

/// Characters in a minted code, before grouping dashes are added.
pub const CODE_LEN: usize = 21;

/// Longest code string this side will look at. A normalized code is
/// [`CODE_LEN`] characters; the slack is for grouping separators. Anything
/// longer is refused before it is hashed — the string arrived from a peer.
pub const MAX_CODE_INPUT: usize = 128;

/// blake3 derive_key context for the at-rest hash of an enrollment code.
/// FROZEN with the node-meta schema: a node that re-derived would refuse every
/// outstanding code after an upgrade.
const CODE_HASH_CONTEXT: &str = "hive-node-enroll-code-v1";

/// Case-fold a typed code and drop the separators that make it readable.
///
/// The node hashes the normalized form, so `k4rt-9x2m` and `K4RT9X2M` are the
/// same secret — which is what makes grouping a display choice rather than a
/// second thing to get right.
pub fn normalize_code(raw: &str) -> Result<String> {
    if raw.len() > MAX_CODE_INPUT {
        bail!(
            "enrollment code is {} characters, over the {MAX_CODE_INPUT}-character input cap",
            raw.len()
        );
    }
    let code: String = raw
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '_'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if code.is_empty() {
        bail!("enrollment code is empty");
    }
    Ok(code)
}

/// What a node stores instead of the code (D30: "single-use, short-TTL, hashed
/// at rest"). Domain-separated, so the 32 bytes in `node-meta.db` are useful
/// for exactly one comparison and nothing else.
///
/// A plain hash rather than a password KDF, deliberately: the input is
/// [`CODE_BYTES`] of OS randomness, not a human-chosen secret, so there is no
/// dictionary to slow down — and a node's enrollment table is not a password
/// database.
pub fn code_hash(normalized: &str) -> [u8; 32] {
    blake3::derive_key(CODE_HASH_CONTEXT, normalized.as_bytes())
}

/// Encode [`CODE_BYTES`] of entropy as a code. Pure: the caller supplies the
/// randomness (the node samples the OS RNG), so this crate stays RNG-free and
/// the encoding lives beside the normalization that has to undo it.
pub fn encode_code(entropy: &[u8; CODE_BYTES]) -> String {
    data_encoding::BASE32_NOPAD.encode(entropy)
}

/// Group a code for display: `XXXXX-XXXXX-XXXXX-XXXXXX`. Cosmetic only —
/// [`normalize_code`] undoes it before anything is hashed.
pub fn format_code(code: &str) -> String {
    code.as_bytes()
        .chunks(5)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

/// Redeem a code over an already-established (mTLS) stream: send the request,
/// read the node's verdict, and CHECK THE VERDICT AGAINST THE PEER THAT SENT IT.
///
/// The caller MUST have dialed with the same device key it names in `request`
/// — the node checks the two against each other, because a relay that could
/// redeem with keys it does not hold would be exactly the substitution D30's
/// SAS exists to prevent.
///
/// `peer` is the pin the enrollment handshake actually terminated on, and the
/// grant is refused unless it names that same key. This is the OTHER HALF of
/// that channel binding, and it is what the enrollment dial's
/// [`crate::tls::UnpinnedEnrollment`] exception is paid for: the dial accepts
/// any server key precisely because the device has not been told the node's
/// key yet, so the grant is the first and only moment the device can learn it
/// — and a grant naming some third key means the connection was terminated by
/// something that is not the node that minted the code. Without this check a
/// machine-in-the-middle on a spoofed SRV answer reads the code off the wire,
/// redeems it upstream under ITS own device key (satisfying the node's half of
/// the binding, since it genuinely holds that key), and hands back a grant
/// naming ITSELF — which the device would then pin forever, shipping every
/// segment to the attacker while the real node never sees a backup.
pub async fn redeem<R, W>(
    reader: &mut R,
    writer: &mut W,
    request: EnrollRequest,
    peer: &SpkiPin,
) -> Result<EnrollGrant>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_frame(writer, &Frame::Enroll(request))
        .await
        .context("sending the enrollment request")?;
    match read_frame_capped(reader, MAX_OPENING_FRAME)
        .await
        .context("waiting for the node's enrollment verdict")?
    {
        Frame::Enrolled(grant) => {
            let granted = SpkiPin::from_ed25519_public(&grant.node_ed25519_pk);
            if granted != *peer {
                bail!(
                    "refusing an enrollment grant for node key {granted}: the connection was \
                     terminated by {peer}, so whatever answered is not the node that minted \
                     this code"
                );
            }
            Ok(grant)
        }
        Frame::Error(e) => bail!("the node refused enrollment: {} ({})", e.message, e.code),
        other => bail!(
            "the node answered an enrollment with a {} frame",
            other.variant()
        ),
    }
}

/// The request a device sends for `code`, from its own identity.
pub fn request(
    code: &str,
    device: &str,
    identity: &hive_core::identity::DeviceIdentity,
) -> Result<EnrollRequest> {
    Ok(EnrollRequest {
        proto: PROTO_VERSION,
        code: normalize_code(code)?,
        device: device.to_string(),
        ed25519_pk: identity.ed25519_public(),
        x25519_pk: identity.x25519_public(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answer one enrollment with a grant naming `granted_node_pk`, then hand
    /// back what [`redeem`] made of it while pinning `peer`.
    ///
    /// The two keys being separate arguments is the whole point: an honest node
    /// passes the same key twice, and a machine-in-the-middle is exactly the
    /// case where they differ.
    async fn grant_naming(
        granted_node_pk: [u8; 32],
        peer: &SpkiPin,
    ) -> Result<crate::frame::EnrollGrant> {
        let (mut node_end, device_end) = tokio::io::duplex(64 * 1024);
        let (mut reader, mut writer) = tokio::io::split(device_end);
        let answering = tokio::spawn(async move {
            // Read the request the device sends, then answer it.
            let _ = read_frame_capped(&mut node_end, MAX_OPENING_FRAME).await;
            write_frame(
                &mut node_end,
                &Frame::Enrolled(crate::frame::EnrollGrant {
                    domain: "household/example.com".to_string(),
                    node_ed25519_pk: granted_node_pk,
                    epoch: 1,
                }),
            )
            .await
        });
        let request = EnrollRequest {
            proto: PROTO_VERSION,
            code: "K4RT9X2MABCDE".to_string(),
            device: "dev-laptop".to_string(),
            ed25519_pk: [9u8; 32],
            x25519_pk: [10u8; 32],
        };
        let out = redeem(&mut reader, &mut writer, request, peer).await;
        let _ = answering.await;
        out
    }

    #[tokio::test]
    async fn a_grant_naming_a_key_that_did_not_answer_is_refused() {
        // The node that actually terminated the connection.
        let real = [1u8; 32];
        // What a machine-in-the-middle would hand back: itself.
        let impostor = [2u8; 32];

        let honest = grant_naming(real, &SpkiPin::from_ed25519_public(&real))
            .await
            .expect("a grant naming the key that answered is the whole happy path");
        assert_eq!(honest.node_ed25519_pk, real);

        let err = grant_naming(impostor, &SpkiPin::from_ed25519_public(&real))
            .await
            .expect_err(
                "a grant naming a key other than the one that terminated the connection is the \
                 substitution the ceremony exists to refuse",
            );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not the node that minted"),
            "the refusal must say what happened, got: {msg}"
        );
    }

    #[test]
    fn a_typed_code_normalizes_to_what_the_node_hashed() {
        let minted = "K4RT9X2MABCDE";
        for typed in [
            "K4RT9X2MABCDE",
            "k4rt9x2mabcde",
            "K4RT9-X2MAB-CDE",
            "k4rt 9x2m abcde",
        ] {
            assert_eq!(normalize_code(typed).unwrap(), minted, "{typed}");
            assert_eq!(
                code_hash(&normalize_code(typed).unwrap()),
                code_hash(minted)
            );
        }
    }

    #[test]
    fn a_hostile_code_string_is_refused_before_it_is_hashed() {
        assert!(normalize_code(&"A".repeat(MAX_CODE_INPUT + 1)).is_err());
        assert!(normalize_code("").is_err());
        assert!(normalize_code("---").is_err(), "separators are not a code");
    }

    /// Different codes hash apart, and the context is domain-separated from
    /// every other blake3 derivation in the tree.
    #[test]
    fn the_hash_is_a_one_way_function_of_the_code_alone() {
        assert_ne!(code_hash("AAAA"), code_hash("AAAB"));
        assert_eq!(code_hash("AAAA"), code_hash("AAAA"));
        assert_ne!(
            code_hash("AAAA"),
            blake3::derive_key("hive-device-ed25519-v1", b"AAAA"),
            "the enrollment context must not collide with the identity contexts"
        );
    }

    #[test]
    fn a_code_is_twenty_one_base32_characters_of_os_entropy() {
        let code = encode_code(&[0xA7; CODE_BYTES]);
        assert_eq!(code.len(), CODE_LEN);
        assert_eq!(normalize_code(&code).unwrap(), code, "already normal");
        assert_ne!(code, encode_code(&[0xA8; CODE_BYTES]));
    }

    #[test]
    fn display_grouping_is_cosmetic() {
        let code = "ABCDEFGHIJKLMNOPQRSTU";
        assert_eq!(code.len(), CODE_LEN);
        let shown = format_code(code);
        assert_eq!(shown, "ABCDE-FGHIJ-KLMNO-PQRST-U");
        assert_eq!(normalize_code(&shown).unwrap(), code);
    }
}
