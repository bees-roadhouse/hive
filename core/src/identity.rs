// Device transport identity (PLAN-v2.1 PR 4.5, D29/D30): the per-device
// asymmetric keypair a device is *known by* — on the wire (the mTLS carrier
// pins it, `hive_sync::tls`), in control records (device.add/revoke sign with
// it, PR 4.11), and in sharing (the pairwise wrap agrees with it, phase 6).
// ONE asymmetric introduction, reused everywhere; a second one would mean a
// second thing to enroll, revoke, and get wrong.
//
// This is NOT the master key. The master (keys.rs) is the DOMAIN's secret,
// identical on every device that has been let in, and it encrypts data at
// rest. The identity is one DEVICE's secret, different on every device,
// never shared, and it encrypts nothing — it only proves "this is the same
// device you enrolled". Losing the identity costs a re-enrollment; losing the
// master costs the data. They are deliberately separate secrets with separate
// custody, so a compromised transport key cannot read a single segment.
//
// Key hierarchy (frozen from here — a device that stored a seed must derive
// the same keys from it forever, or it silently becomes a different device):
//
//   device seed (32 bytes)         OS keychain (KeychainIdentitySource), or
//     │                            the 64-hex file seam for headless/tests
//     ├─ blake3::derive_key("hive-device-ed25519-v1", seed) → ed25519 signing
//     └─ blake3::derive_key("hive-device-x25519-v1",  seed) → x25519 agreement
//
// One stored secret, two keys, domain-separated: the halves are independent
// even though a device only ever backs up (or loses) the one seed. The
// derivation is a pure function — `core/tests/identity.rs` pins its bytes
// beside the RFC 8032/7748 vectors that pin the primitives themselves.
//
// Randomness policy: like keys.rs (and unlike the durable layer fenced by
// core/tests/determinism.rs), this module may touch the OS RNG — and only for
// a fresh seed, which is exactly the kind of material that must be
// unpredictable. Everything after the seed is derivation.
//
// Ordering note for callers: `KeychainIdentitySource` blocks on the OS
// keychain (Secret Service over D-Bus on Linux). Resolve it ONCE at startup,
// before the tokio runtime exists — the same rule the master key follows in
// `app/src/main.rs::boot`, for the same reason.

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use rand::RngCore;
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};

/// blake3 derive_key context for the ed25519 half (frozen).
const ED25519_CONTEXT: &str = "hive-device-ed25519-v1";
/// blake3 derive_key context for the x25519 half (frozen).
const X25519_CONTEXT: &str = "hive-device-x25519-v1";

/// Length of an ed25519 signature — the shape control records carry.
pub const SIGNATURE_LEN: usize = 64;

/// One device's transport identity: an ed25519 signing keypair (who signed
/// this) and an x25519 agreement keypair (who can I derive a shared secret
/// with), both derived from a single 32-byte seed.
///
/// Cheap to rebuild from the seed, so custody stores 32 bytes and nothing
/// else. Not `Clone` on purpose: a device has ONE identity, and copies of
/// secret material should be a deliberate `from_seed` call.
pub struct DeviceIdentity {
    seed: [u8; 32],
    signing: SigningKey,
    agreement: StaticSecret,
}

impl DeviceIdentity {
    /// Derive the identity from its stored seed. Pure, total, and frozen:
    /// same seed, same device, forever.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&blake3::derive_key(ED25519_CONTEXT, seed));
        let agreement = StaticSecret::from(blake3::derive_key(X25519_CONTEXT, seed));
        Self {
            seed: *seed,
            signing,
            agreement,
        }
    }

    /// A brand-new identity from the OS RNG — first boot on a device, and the
    /// only place in this module that samples anything.
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        Self::from_seed(&seed)
    }

    /// The stored secret, for custody code that must persist it (the keychain
    /// source below, the node's config at PR 4.6). Secret material: it goes
    /// to a keychain entry or a 0600 file, never to a log or a tool result.
    pub fn seed(&self) -> [u8; 32] {
        self.seed
    }

    /// The ed25519 public key — what a peer pins, what a control record's
    /// signature verifies against, and what the transport certificate
    /// carries (`hive_sync::tls`).
    pub fn ed25519_public(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// The x25519 public key — the agreement half enrollment publishes
    /// alongside the signing half (PR 4.7 pins both).
    pub fn x25519_public(&self) -> [u8; 32] {
        X25519Public::from(&self.agreement).to_bytes()
    }

    /// Sign a message with the device's ed25519 key.
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        use ed25519_dalek::Signer;
        self.signing.sign(message).to_bytes()
    }

    /// The raw X25519 shared secret with a peer's public key. Callers must
    /// run it through a KDF before using it as a key (blake3 derive_key with
    /// their own context) — this returns the primitive, not a session key.
    pub fn shared_secret(&self, peer_x25519_public: &[u8; 32]) -> [u8; 32] {
        self.agreement
            .diffie_hellman(&X25519Public::from(*peer_x25519_public))
            .to_bytes()
    }

    /// The ed25519 private key as PKCS#8 DER — the one export that hands out
    /// secret bytes, and it exists for exactly one caller: the TLS carrier,
    /// which mints the device's self-signed certificate through rcgen and
    /// needs the key in the format every X.509 tool speaks.
    pub fn ed25519_pkcs8_der(&self) -> Result<Vec<u8>> {
        Ok(self
            .signing
            .to_pkcs8_der()
            .map_err(|e| anyhow!("encoding the device signing key as PKCS#8: {e}"))?
            .as_bytes()
            .to_vec())
    }
}

/// Public halves only — the seed never reaches a log line through `{:?}`.
impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field(
                "ed25519_public",
                &data_encoding::HEXLOWER.encode(&self.ed25519_public()),
            )
            .field(
                "x25519_public",
                &data_encoding::HEXLOWER.encode(&self.x25519_public()),
            )
            .finish()
    }
}

/// Verify an ed25519 signature against a device's published public key.
/// Free-standing because the verifier never holds the identity — it holds a
/// pinned public key it got at enrollment (PR 4.7) or from a control record.
pub fn verify(
    ed25519_public: &[u8; 32],
    message: &[u8],
    signature: &[u8; SIGNATURE_LEN],
) -> Result<()> {
    let key = VerifyingKey::from_bytes(ed25519_public)
        .map_err(|e| anyhow!("not a valid ed25519 public key: {e}"))?;
    key.verify(message, &Signature::from_bytes(signature))
        .map_err(|_| anyhow!("signature does not verify under that device key"))
}

/// Where a device gets its identity — the `KeySource` pattern, one layer up:
/// tests inject a fixed seed, the desktop app reads the OS keychain, headless
/// deployments point at a file.
pub trait IdentitySource {
    fn device_identity(&self) -> Result<DeviceIdentity>;
}

/// Fixed-seed source for tests (hermetic, no OS services).
pub struct MemoryIdentitySource(pub [u8; 32]);

impl IdentitySource for MemoryIdentitySource {
    fn device_identity(&self) -> Result<DeviceIdentity> {
        Ok(DeviceIdentity::from_seed(&self.0))
    }
}

/// Device seed held by the OS keychain, beside the master key and by the same
/// rules as `keys::KeychainKeySource`: generated from the OS RNG on first use,
/// stored base32 under the service name, read back after that. Single-process
/// by assumption (hive is a desktop app).
///
/// Blocking, D-Bus-backed on Linux: resolve it before the tokio runtime
/// starts (see the module header).
pub struct KeychainIdentitySource {
    service: String,
    user: String,
}

impl KeychainIdentitySource {
    /// The default hive identity entry: service "hive", entry
    /// "device-identity" — deliberately a different entry from the master
    /// key's, so custody can differ (a device may keep its identity while the
    /// master lives only in a recovery code).
    pub fn new() -> Self {
        Self::with_service_user("hive", "device-identity")
    }

    /// Custom service/user pair — the `#[ignore]`d live test uses a throwaway
    /// entry name so it never touches a real device identity.
    pub fn with_service_user(service: &str, user: &str) -> Self {
        Self {
            service: service.to_string(),
            user: user.to_string(),
        }
    }
}

impl Default for KeychainIdentitySource {
    fn default() -> Self {
        Self::new()
    }
}

impl IdentitySource for KeychainIdentitySource {
    fn device_identity(&self) -> Result<DeviceIdentity> {
        let entry = keyring::Entry::new(&self.service, &self.user)
            .with_context(|| format!("keychain entry {}/{}", self.service, self.user))?;
        match entry.get_password() {
            Ok(stored) => {
                let bytes = data_encoding::BASE32_NOPAD
                    .decode(stored.trim().as_bytes())
                    .context("stored device seed is not valid base32")?;
                let seed: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyhow!("stored device seed is not 32 bytes"))?;
                Ok(DeviceIdentity::from_seed(&seed))
            }
            Err(keyring::Error::NoEntry) => {
                let identity = DeviceIdentity::generate();
                entry
                    .set_password(&data_encoding::BASE32_NOPAD.encode(&identity.seed()))
                    .context("storing a freshly generated device seed in the keychain")?;
                Ok(identity)
            }
            Err(e) => Err(anyhow!(e)).with_context(|| {
                format!(
                    "reading device seed from keychain {}/{}",
                    self.service, self.user
                )
            }),
        }
    }
}

/// Read a device seed from a 64-hex file — the same ONE format the master key
/// uses (`keys::parse_master_key_hex`), so headless deployments, the node's
/// config (PR 4.6) and the smoke tier all handle exactly one key-file shape.
/// Test/ops seam: production custody on the desktop stays the OS keychain.
pub fn read_device_seed_file(path: &std::path::Path) -> Result<DeviceIdentity> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading device seed file {}", path.display()))?;
    let seed = crate::keys::parse_master_key_hex(&text)
        .with_context(|| format!("device seed file {}", path.display()))?;
    Ok(DeviceIdentity::from_seed(&seed))
}
