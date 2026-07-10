// Key management for the durable layer (PR 1.4, D19/D27): master-key custody
// plus the wrap/unwrap primitives the oplog and blockstore build on. UI-free
// by design — this file is primitives only.
//
// Key hierarchy:
//
//   master key (32 bytes)          lives in the OS keychain (KeychainKeySource)
//     ├─ wraps per-segment keys    oplog segment headers (wrap_key)
//     ├─ wraps per-blob keys       BlobRef.wrapped_key (wrap_key) — destroying
//     │                            every wrapped copy IS the hard delete (D19)
//     └─ exportable under a KEK    passphrase_wrap (Argon2id, RFC 9106 params)
//        and as a recovery code    grouped base32 of the raw master key
//
// Randomness policy: this module is the ONE place in the durable layer allowed
// to touch the OS RNG, and only for key material that must be unpredictable
// (fresh master keys, Argon2 salts). The oplog and blockstore modules stay
// fully deterministic (enforced by the grep test in core/tests/determinism.rs)
// and receive key material through the `KeySource` seam instead.
//
// Wrap format (frozen with the PR 1.4 record format):
//
//   wrapped key   = nonce(24) ‖ XChaCha20-Poly1305(wrapping_key, nonce, key32)
//                 = 24 + 32 + 16 = 72 bytes (WRAPPED_KEY_LEN)
//   nonce         = blake3::keyed_hash(
//                       blake3::derive_key("hive-keywrap-nonce-v1", wrapping_key),
//                       key32)[..24]
//
// The nonce is a PRF of the plaintext key under a subkey of the wrapping key
// (SIV-style). A (key, nonce) pair therefore repeats only when the exact same
// key is wrapped under the same wrapping key — which yields the exact same
// ciphertext, never a keystream reuse. This makes wrapping a pure function,
// which the blockstore relies on for byte-identical BlobRefs on dedup hits.

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::rngs::OsRng;
use rand::RngCore;

/// Length in bytes of `wrap_key` output: 24-byte nonce + 32-byte key + 16-byte tag.
pub const WRAPPED_KEY_LEN: usize = 24 + 32 + 16;

/// blake3 derive_key context for the SIV-style wrap nonce (frozen).
const KEYWRAP_NONCE_CONTEXT: &str = "hive-keywrap-nonce-v1";

/// Argon2id parameters for the passphrase export wrap. RFC 9106 §4's second
/// recommended option ("if much less memory is available"): t=3 passes,
/// 64 MiB memory, 4 lanes — comfortably above the 2024+ OWASP minimum
/// (19 MiB / t=2 / p=1) while staying ~100ms-class on desktop hardware, which
/// is fine for an export/restore path that runs a handful of times ever.
const ARGON2_M_COST_KIB: u32 = 64 * 1024;
const ARGON2_T_COST: u32 = 3;
const ARGON2_P_COST: u32 = 4;
/// Salt length for the passphrase KEK (RFC 9106 recommends 128-bit).
const ARGON2_SALT_LEN: usize = 16;

/// Version byte of the `passphrase_wrap` container format.
const PASSPHRASE_WRAP_VERSION: u8 = 1;

/// Upper bounds accepted when *reading* a passphrase-wrapped container, so a
/// hostile blob can't make us allocate gigabytes or spin forever. Generous
/// headroom over today's constants for future parameter bumps.
const ARGON2_MAX_M_COST_KIB: u32 = 1024 * 1024; // 1 GiB
const ARGON2_MAX_T_COST: u32 = 64;
const ARGON2_MAX_P_COST: u32 = 32;

/// Where the durable layer gets its master key. The oplog and blockstore take
/// `&dyn KeySource` instead of raw bytes so tests inject fixed keys and the
/// app injects the OS keychain.
pub trait KeySource {
    fn master_key(&self) -> Result<[u8; 32]>;
}

/// Fixed-key source for tests (hermetic, no OS services).
pub struct MemoryKeySource(pub [u8; 32]);

impl KeySource for MemoryKeySource {
    fn master_key(&self) -> Result<[u8; 32]> {
        Ok(self.0)
    }
}

/// Master key held by the OS keychain (Secret Service / macOS Keychain /
/// Windows Credential Manager) via the `keyring` crate. On first use a fresh
/// 32-byte key is generated from the OS RNG and stored under the service
/// name; later calls read it back. Single-process by assumption (hive is a
/// desktop app): a first-use race between two processes would resolve to
/// whichever `set_password` lands last.
pub struct KeychainKeySource {
    service: String,
    user: String,
}

impl KeychainKeySource {
    /// The default hive identity: service "hive", entry "master-key".
    pub fn new() -> Self {
        Self::with_service_user("hive", "master-key")
    }

    /// Custom service/user pair — the `#[ignore]`d live test uses a throwaway
    /// entry name so it never touches a real hive key.
    pub fn with_service_user(service: &str, user: &str) -> Self {
        Self {
            service: service.to_string(),
            user: user.to_string(),
        }
    }
}

impl Default for KeychainKeySource {
    fn default() -> Self {
        Self::new()
    }
}

impl KeySource for KeychainKeySource {
    fn master_key(&self) -> Result<[u8; 32]> {
        let entry = keyring::Entry::new(&self.service, &self.user)
            .with_context(|| format!("keychain entry {}/{}", self.service, self.user))?;
        match entry.get_password() {
            Ok(stored) => {
                let bytes = data_encoding::BASE32_NOPAD
                    .decode(stored.trim().as_bytes())
                    .context("stored master key is not valid base32")?;
                let key: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| anyhow!("stored master key is not 32 bytes"))?;
                Ok(key)
            }
            Err(keyring::Error::NoEntry) => {
                let mut key = [0u8; 32];
                OsRng.fill_bytes(&mut key);
                entry
                    .set_password(&data_encoding::BASE32_NOPAD.encode(&key))
                    .context("storing freshly generated master key in the keychain")?;
                Ok(key)
            }
            Err(e) => Err(anyhow!(e)).with_context(|| {
                format!(
                    "reading master key from keychain {}/{}",
                    self.service, self.user
                )
            }),
        }
    }
}

/// Deterministic SIV-style nonce for `wrap_key` (see module header).
fn wrap_nonce(wrapping_key: &[u8; 32], plaintext_key: &[u8; 32]) -> [u8; 24] {
    let nonce_key = blake3::derive_key(KEYWRAP_NONCE_CONTEXT, wrapping_key);
    let digest = blake3::keyed_hash(&nonce_key, plaintext_key);
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&digest.as_bytes()[..24]);
    nonce
}

/// Wrap a 32-byte key under another 32-byte key. Output is
/// `WRAPPED_KEY_LEN` (72) bytes: `nonce(24) ‖ ciphertext(32) ‖ tag(16)`.
/// Deterministic: same inputs, same bytes (see module header for why the
/// derived nonce is safe).
pub fn wrap_key(wrapping_key: &[u8; 32], plaintext_key: &[u8; 32]) -> Result<Vec<u8>> {
    let nonce = wrap_nonce(wrapping_key, plaintext_key);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(wrapping_key));
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext_key.as_slice())
        .map_err(|e| anyhow!("key wrap encryption failed: {e}"))?;
    let mut out = Vec::with_capacity(WRAPPED_KEY_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    debug_assert_eq!(out.len(), WRAPPED_KEY_LEN);
    Ok(out)
}

/// Reverse of `wrap_key`. Fails on wrong wrapping key, wrong length, or any
/// bit flip (AEAD authentication).
pub fn unwrap_key(wrapping_key: &[u8; 32], wrapped: &[u8]) -> Result<[u8; 32]> {
    if wrapped.len() != WRAPPED_KEY_LEN {
        bail!(
            "wrapped key is {} bytes, expected {WRAPPED_KEY_LEN}",
            wrapped.len()
        );
    }
    let (nonce, ct) = wrapped.split_at(24);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(wrapping_key));
    let pt = cipher
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|_| anyhow!("key unwrap failed: wrong key or corrupted wrap"))?;
    let key: [u8; 32] = pt
        .try_into()
        .map_err(|_| anyhow!("unwrapped key is not 32 bytes"))?;
    Ok(key)
}

/// Export the master key under a passphrase: Argon2id derives a KEK from the
/// passphrase, the KEK wraps the master key. Container layout (frozen, all
/// integers LE):
///
///   offset 0  : u8   container version (=1)
///   offset 1  : u32  Argon2 m_cost (KiB)
///   offset 5  : u32  Argon2 t_cost
///   offset 9  : u32  Argon2 p_cost
///   offset 13 : 16B  salt (fresh from the OS RNG per export)
///   offset 29 : 72B  wrap_key(KEK, master)
///   total    : 101 bytes
///
/// Parameters ride inside the container so old exports keep restoring after
/// future parameter bumps.
pub fn passphrase_wrap(master: &[u8; 32], passphrase: &str) -> Result<Vec<u8>> {
    let mut salt = [0u8; ARGON2_SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let kek = derive_kek(
        passphrase,
        &salt,
        ARGON2_M_COST_KIB,
        ARGON2_T_COST,
        ARGON2_P_COST,
    )?;
    let wrapped = wrap_key(&kek, master)?;
    let mut out = Vec::with_capacity(1 + 12 + ARGON2_SALT_LEN + WRAPPED_KEY_LEN);
    out.push(PASSPHRASE_WRAP_VERSION);
    out.extend_from_slice(&ARGON2_M_COST_KIB.to_le_bytes());
    out.extend_from_slice(&ARGON2_T_COST.to_le_bytes());
    out.extend_from_slice(&ARGON2_P_COST.to_le_bytes());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&wrapped);
    Ok(out)
}

/// Restore a master key from a `passphrase_wrap` container. A wrong
/// passphrase fails the AEAD open (there is no oracle distinguishing wrong
/// passphrase from corrupted container — that is inherent and fine).
pub fn passphrase_unwrap(container: &[u8], passphrase: &str) -> Result<[u8; 32]> {
    let expected = 1 + 12 + ARGON2_SALT_LEN + WRAPPED_KEY_LEN;
    if container.len() != expected {
        bail!(
            "passphrase container is {} bytes, expected {expected}",
            container.len()
        );
    }
    if container[0] != PASSPHRASE_WRAP_VERSION {
        bail!("unsupported passphrase container version {}", container[0]);
    }
    let m = u32::from_le_bytes(container[1..5].try_into().unwrap());
    let t = u32::from_le_bytes(container[5..9].try_into().unwrap());
    let p = u32::from_le_bytes(container[9..13].try_into().unwrap());
    if m > ARGON2_MAX_M_COST_KIB || t > ARGON2_MAX_T_COST || p > ARGON2_MAX_P_COST {
        bail!("passphrase container demands unreasonable Argon2 cost (m={m} KiB, t={t}, p={p})");
    }
    let salt = &container[13..13 + ARGON2_SALT_LEN];
    let wrapped = &container[13 + ARGON2_SALT_LEN..];
    let kek = derive_kek(passphrase, salt, m, t, p)?;
    unwrap_key(&kek, wrapped).context("wrong passphrase or corrupted container")
}

fn derive_kek(passphrase: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<[u8; 32]> {
    let params = argon2::Params::new(m, t, p, Some(32))
        .map_err(|e| anyhow!("argon2 params rejected: {e}"))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut kek = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut kek)
        .map_err(|e| anyhow!("argon2 derivation failed: {e}"))?;
    Ok(kek)
}

/// Render the master key as a printable recovery code: RFC 4648 base32
/// (A–Z, 2–7, no padding) of the 32 raw bytes — 52 characters — grouped in
/// fours with dashes for transcription: `XXXX-XXXX-…-XXXX` (13 groups).
pub fn recovery_code(master: &[u8; 32]) -> String {
    let raw = data_encoding::BASE32_NOPAD.encode(master);
    raw.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("base32 is ascii"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Parse a recovery code back into the master key. Forgiving about dashes,
/// whitespace, and case; strict about everything else.
pub fn parse_recovery_code(code: &str) -> Result<[u8; 32]> {
    let cleaned: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let bytes = data_encoding::BASE32_NOPAD
        .decode(cleaned.as_bytes())
        .context("recovery code is not valid base32")?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("recovery code does not decode to 32 bytes"))?;
    Ok(key)
}
