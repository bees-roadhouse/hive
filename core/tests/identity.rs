// Device transport identity (PLAN-v2.1 PR 4.5): the primitives pinned by the
// standards' own vectors, and our derivation pinned by its own goldens.
//
// Two layers, deliberately separate:
//
//   GOLDEN-BYTES (docs/TESTING-STRATEGY.md §3.3) — RFC 8032 §7.1 (Ed25519)
//   and RFC 7748 §5.2/§6.1 (X25519) imported as checked-in fixtures. These
//   answer "is the crate we chose the algorithm the RFC describes", which is
//   the only question worth asking of a primitive, and they survive a
//   dependency swap: aws-lc-rs or ring would have to pass the same file.
//
//   Our own goldens — `DeviceIdentity::from_seed` is a frozen derivation (a
//   device that stored a seed must derive the same keys from it forever, or
//   it silently becomes a different device to every peer that pinned it), so
//   the seed→keypair bytes are pinned here as hex constants.
//
// Hermetic throughout, except the `#[ignore]`d keychain round-trip, which
// needs a real Secret Service and therefore can't run on headless CI:
//   cargo test -p hive-core --test identity -- --ignored

use std::path::{Path, PathBuf};

use hive_core::identity::{
    self, DeviceIdentity, IdentitySource, KeychainIdentitySource, MemoryIdentitySource,
};

// ── fixture plumbing ────────────────────────────────────────────────────────

fn fixture(name: &str) -> serde_json::Value {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing fixture {name}: {e}"))
}

fn unhex(value: &serde_json::Value) -> Vec<u8> {
    let text = value.as_str().expect("fixture field is a hex string");
    data_encoding::HEXLOWER
        .decode(text.as_bytes())
        .unwrap_or_else(|e| panic!("fixture hex {text:?}: {e}"))
}

fn unhex32(value: &serde_json::Value) -> [u8; 32] {
    unhex(value).try_into().expect("fixture field is 32 bytes")
}

fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

// ── GOLDEN-BYTES: the standards' vectors ────────────────────────────────────

/// RFC 8032 §7.1. Every vector must reproduce its public key, its exact
/// signature bytes (Ed25519 is deterministic — no RNG, so a signature IS a
/// golden), and verify through OUR `identity::verify`, which is the path a
/// control record's signature check takes.
#[test]
fn rfc8032_ed25519_vectors_reproduce_keys_signatures_and_verification() {
    let fixture = fixture("rfc8032_ed25519.json");
    let vectors = fixture["vectors"].as_array().expect("vectors array");
    assert_eq!(vectors.len(), 4, "the imported vector set is complete");

    for vector in vectors {
        let name = vector["name"].as_str().unwrap();
        let secret = unhex32(&vector["secret_key"]);
        let public = unhex32(&vector["public_key"]);
        let message = unhex(&vector["message"]);
        let signature: [u8; 64] = unhex(&vector["signature"])
            .try_into()
            .expect("64-byte signature");

        let signing = ed25519_dalek::SigningKey::from_bytes(&secret);
        assert_eq!(
            signing.verifying_key().to_bytes(),
            public,
            "{name}: public key derived from the secret"
        );
        use ed25519_dalek::Signer;
        assert_eq!(
            signing.sign(&message).to_bytes(),
            signature,
            "{name}: signature bytes"
        );
        identity::verify(&public, &message, &signature)
            .unwrap_or_else(|e| panic!("{name}: verification failed: {e:#}"));

        // One flipped bit anywhere is a rejection, not a warning.
        let mut tampered = signature;
        tampered[0] ^= 0x01;
        assert!(
            identity::verify(&public, &message, &tampered).is_err(),
            "{name}: tampered signature must not verify"
        );
        let mut other_message = message.clone();
        other_message.push(0x00);
        assert!(
            identity::verify(&public, &other_message, &signature).is_err(),
            "{name}: signature must not carry over to another message"
        );
    }
}

/// RFC 7748 §5.2 (the X25519 function itself) and §6.1 (the Diffie-Hellman
/// exchange). The §5.2 scalars arrive unclamped on purpose: clamping is
/// inside X25519, and a stack that skipped it would fail here.
#[test]
fn rfc7748_x25519_vectors_reproduce_scalar_mult_and_the_shared_secret() {
    let fixture = fixture("rfc7748_x25519.json");

    for vector in fixture["scalar_mult"]
        .as_array()
        .expect("scalar_mult array")
    {
        let scalar = unhex32(&vector["scalar"]);
        let u_in = unhex32(&vector["u_in"]);
        let u_out = unhex32(&vector["u_out"]);
        assert_eq!(
            x25519_dalek::x25519(scalar, u_in),
            u_out,
            "X25519({}, {})",
            hex(&scalar),
            hex(&u_in)
        );
    }

    let dh = &fixture["diffie_hellman"];
    let alice = x25519_dalek::StaticSecret::from(unhex32(&dh["alice_private"]));
    let bob = x25519_dalek::StaticSecret::from(unhex32(&dh["bob_private"]));
    let alice_public = x25519_dalek::PublicKey::from(&alice);
    let bob_public = x25519_dalek::PublicKey::from(&bob);
    assert_eq!(alice_public.to_bytes(), unhex32(&dh["alice_public"]));
    assert_eq!(bob_public.to_bytes(), unhex32(&dh["bob_public"]));

    let shared = unhex32(&dh["shared_secret"]);
    assert_eq!(alice.diffie_hellman(&bob_public).to_bytes(), shared);
    assert_eq!(bob.diffie_hellman(&alice_public).to_bytes(), shared);
}

// ── our derivation, frozen ──────────────────────────────────────────────────

/// The seed→keypair derivation is frozen: these bytes are what a device with
/// this seed IS, on every platform and every future version. Changing either
/// blake3 context string would re-key every enrolled device — this test is
/// the tripwire, and its failure means "you just orphaned every pinned key".
#[test]
fn a_device_identity_is_a_frozen_pure_function_of_its_seed() {
    let identity = DeviceIdentity::from_seed(&[0x42; 32]);
    assert_eq!(
        hex(&identity.ed25519_public()),
        "9475da198d1409dab011f2eee949e0f625a0ed34935f95b9abfa61f0ec318312"
    );
    assert_eq!(
        hex(&identity.x25519_public()),
        "391873b723d6941eedcdb566130798442593ae3b6aea2d03144f184056b78918"
    );

    // Same seed, same device; the struct is rebuildable, not stateful.
    let again = DeviceIdentity::from_seed(&[0x42; 32]);
    assert_eq!(again.ed25519_public(), identity.ed25519_public());
    assert_eq!(again.x25519_public(), identity.x25519_public());
    assert_eq!(again.seed(), [0x42; 32]);

    // One flipped seed bit is a different device, related to nothing.
    let mut near = [0x42; 32];
    near[31] ^= 0x01;
    let near = DeviceIdentity::from_seed(&near);
    assert_ne!(near.ed25519_public(), identity.ed25519_public());
    assert_ne!(near.x25519_public(), identity.x25519_public());
}

/// The two halves are domain-separated: the x25519 secret is not the ed25519
/// secret under another name, and neither is the seed itself. (Reusing one
/// scalar for both signing and agreement is the classic cross-protocol
/// footgun; the derivation exists to make it impossible here.)
#[test]
fn the_signing_and_agreement_halves_are_domain_separated() {
    let seed = [0x11; 32];
    let identity = DeviceIdentity::from_seed(&seed);

    let ed_material = blake3::derive_key("hive-device-ed25519-v1", &seed);
    let x_material = blake3::derive_key("hive-device-x25519-v1", &seed);
    assert_ne!(
        ed_material, x_material,
        "different contexts, different keys"
    );
    assert_ne!(ed_material, seed, "the seed is never used raw");
    assert_ne!(x_material, seed);

    // And the derivation is the one the module actually performs.
    assert_eq!(
        identity.ed25519_public(),
        ed25519_dalek::SigningKey::from_bytes(&ed_material)
            .verifying_key()
            .to_bytes()
    );
    assert_eq!(
        identity.x25519_public(),
        x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(x_material)).to_bytes()
    );
}

#[test]
fn signatures_round_trip_and_reject_everything_else() {
    let device = DeviceIdentity::from_seed(&[1; 32]);
    let impostor = DeviceIdentity::from_seed(&[2; 32]);
    let message = b"device.add nate-laptop epoch 1";

    let signature = device.sign(message);
    identity::verify(&device.ed25519_public(), message, &signature).unwrap();

    assert!(
        identity::verify(&impostor.ed25519_public(), message, &signature).is_err(),
        "another device's key must not verify this signature"
    );
    assert!(
        identity::verify(
            &device.ed25519_public(),
            b"device.add nate-phone epoch 1",
            &signature
        )
        .is_err(),
        "the signature covers the message"
    );
    // A public key that is not a point on the curve is an Err, never a panic
    // — enrollment (PR 4.7) hands this function bytes off the wire.
    assert!(identity::verify(&[0xff; 32], message, &signature).is_err());

    // Interop the other way: the identity's signature is a plain RFC 8032
    // signature any implementation can check — nothing hive-shaped about it.
    let verifying =
        ed25519_dalek::VerifyingKey::from_bytes(&device.ed25519_public()).expect("valid key");
    use ed25519_dalek::Verifier;
    verifying
        .verify(message, &ed25519_dalek::Signature::from_bytes(&signature))
        .unwrap();
}

#[test]
fn a_shared_secret_is_symmetric_and_peer_specific() {
    let alice = DeviceIdentity::from_seed(&[3; 32]);
    let bob = DeviceIdentity::from_seed(&[4; 32]);
    let mallory = DeviceIdentity::from_seed(&[5; 32]);

    let ab = alice.shared_secret(&bob.x25519_public());
    let ba = bob.shared_secret(&alice.x25519_public());
    assert_eq!(ab, ba, "both sides derive the same secret");
    assert_ne!(
        ab,
        mallory.shared_secret(&alice.x25519_public()),
        "a third device derives something else entirely"
    );

    // Interop: the peer half is an ordinary X25519 key, so a counterparty
    // built on any other library agrees with us.
    let raw_peer = x25519_dalek::StaticSecret::from([9u8; 32]);
    let raw_public = x25519_dalek::PublicKey::from(&raw_peer);
    assert_eq!(
        alice.shared_secret(&raw_public.to_bytes()),
        raw_peer
            .diffie_hellman(&x25519_dalek::PublicKey::from(alice.x25519_public()))
            .to_bytes()
    );
}

/// The one export that hands out secret bytes is the bridge to the TLS
/// carrier (`hive_sync::tls` feeds it to rcgen). Pinned here so a dalek
/// upgrade that changed the encoding fails in core's suite with a clear
/// message instead of in a handshake.
#[test]
fn the_pkcs8_export_round_trips_back_to_the_same_key() {
    use ed25519_dalek::pkcs8::DecodePrivateKey;

    let device = DeviceIdentity::from_seed(&[7; 32]);
    let der = device.ed25519_pkcs8_der().unwrap();
    let parsed = ed25519_dalek::SigningKey::from_pkcs8_der(&der).expect("valid PKCS#8");
    assert_eq!(parsed.verifying_key().to_bytes(), device.ed25519_public());
}

// ── custody ─────────────────────────────────────────────────────────────────

#[test]
fn the_memory_source_returns_its_fixed_seed() {
    let source = MemoryIdentitySource([8; 32]);
    let identity = source.device_identity().unwrap();
    assert_eq!(identity.seed(), [8; 32]);
    assert_eq!(
        identity.ed25519_public(),
        DeviceIdentity::from_seed(&[8; 32]).ed25519_public()
    );
}

/// The seed file is the SAME 64-hex format as the master key file — one shape
/// for every key this project writes to disk (TESTING-STRATEGY §0.4).
#[test]
fn the_seed_file_seam_reads_the_shared_64_hex_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device.hex");
    std::fs::write(&path, format!("{}\n", "5c".repeat(32))).unwrap();

    let identity = identity::read_device_seed_file(&path).unwrap();
    assert_eq!(identity.seed(), [0x5c; 32]);

    // Errors name the offending path, like the master-key file's.
    std::fs::write(&path, "not hex").unwrap();
    let err = format!("{:#}", identity::read_device_seed_file(&path).unwrap_err());
    assert!(err.contains("device.hex"), "{err}");
    let err = format!(
        "{:#}",
        identity::read_device_seed_file(&dir.path().join("absent.hex")).unwrap_err()
    );
    assert!(err.contains("absent.hex"), "{err}");
}

/// Fresh identities are actually fresh — the only randomness in the module.
#[test]
fn generated_identities_are_distinct() {
    let a = DeviceIdentity::generate();
    let b = DeviceIdentity::generate();
    assert_ne!(a.seed(), b.seed());
    assert_ne!(a.ed25519_public(), b.ed25519_public());
}

/// Secret material must not reach a log line through a stray `{:?}`.
#[test]
fn the_debug_impl_prints_public_halves_only() {
    let device = DeviceIdentity::from_seed(&[0xa5; 32]);
    let rendered = format!("{device:?}");
    assert!(rendered.contains(&hex(&device.ed25519_public())));
    assert!(!rendered.contains(&hex(&device.seed())), "{rendered}");
    assert!(!rendered.contains("a5a5a5"), "{rendered}");
}

/// Live keychain round-trip: first call mints and stores, second reads back
/// the same device. Needs a real Secret Service, so it never runs in CI.
#[test]
#[ignore]
fn keychain_identity_persists_across_sources() {
    let user = format!("test-device-identity-{}", std::process::id());
    let first = KeychainIdentitySource::with_service_user("hive-test", &user)
        .device_identity()
        .expect("keychain available");
    let second = KeychainIdentitySource::with_service_user("hive-test", &user)
        .device_identity()
        .unwrap();
    assert_eq!(first.ed25519_public(), second.ed25519_public());

    keyring::Entry::new("hive-test", &user)
        .unwrap()
        .delete_credential()
        .unwrap();
}
