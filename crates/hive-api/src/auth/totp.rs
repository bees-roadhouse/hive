//! RFC 6238 TOTP + at-rest secret encryption (hive-auth-mcp-design.md §4).
//!
//! TOTP is computed directly on HMAC-SHA1 (both already in the dependency tree)
//! rather than pulling the heavier `totp-rs` + QR/image stack: it's a small,
//! well-specified algorithm and keeping it in-tree keeps the build lean. We
//! implement the standard SHA1 / 6-digit / 30-second profile every authenticator
//! app (Google Authenticator, Authy, 1Password, …) defaults to.
//!
//! Secrets are stored ENCRYPTED at rest (§4) with ChaCha20-Poly1305 under a key
//! from `HIVE_MFA_ENC_KEY` (base64, 32 bytes). The encryptor refuses to operate
//! without a configured key in a production build; a dev build derives a clearly
//! marked throwaway key and logs loudly, so local dev works without ceremony but
//! can never be mistaken for real at-rest protection.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// Standard authenticator profile: 6 digits, 30-second step, SHA1.
const DIGITS: u32 = 6;
const STEP_SECS: u64 = 30;

/// Generate a fresh TOTP secret: 20 random bytes (160-bit, the RFC-recommended
/// SHA1 size), returned base32-encoded (no padding) for the otpauth:// URI.
pub fn generate_secret_base32() -> String {
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    base32::encode(base32::Alphabet::Rfc4648 { padding: false }, &bytes)
}

/// Build the `otpauth://totp/...` provisioning URI an authenticator app scans.
/// `issuer` + `account` label the entry; `secret_b32` is the base32 secret.
pub fn provisioning_uri(secret_b32: &str, issuer: &str, account: &str) -> String {
    let label = format!("{issuer}:{account}");
    format!(
        "otpauth://totp/{}?secret={}&issuer={}&algorithm=SHA1&digits={}&period={}",
        urlencode(&label),
        secret_b32,
        urlencode(issuer),
        DIGITS,
        STEP_SECS
    )
}

/// Compute the TOTP code for a base32 secret at a unix timestamp. Returns the
/// zero-padded 6-digit string, or `None` if the secret isn't valid base32.
pub fn code_at(secret_b32: &str, unix_secs: u64) -> Option<String> {
    let key = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret_b32)?;
    let counter = unix_secs / STEP_SECS;
    Some(hotp(&key, counter))
}

/// Verify a presented code against the secret with a ±1 step skew window (§4):
/// the current step plus the one before/after, to tolerate clock drift. Uses a
/// constant-time digit compare so a wrong code leaks no timing signal.
pub fn verify(secret_b32: &str, presented: &str, unix_secs: u64) -> bool {
    let key = match base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret_b32) {
        Some(k) => k,
        None => return false,
    };
    let presented = presented.trim();
    let counter = unix_secs / STEP_SECS;
    // step-1, step, step+1
    for delta in [-1i64, 0, 1] {
        let c = (counter as i64 + delta).max(0) as u64;
        let candidate = hotp(&key, c);
        if constant_time_eq(candidate.as_bytes(), presented.as_bytes()) {
            return true;
        }
    }
    false
}

/// HOTP (RFC 4226) — the per-counter building block of TOTP. SHA1-HMAC of the
/// 8-byte big-endian counter, dynamic truncation, mod 10^DIGITS.
fn hotp(key: &[u8], counter: u64) -> String {
    let mut mac = <HmacSha1 as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    // Dynamic truncation (RFC 4226 §5.3).
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let bin = ((u32::from(digest[offset]) & 0x7f) << 24)
        | ((u32::from(digest[offset + 1]) & 0xff) << 16)
        | ((u32::from(digest[offset + 2]) & 0xff) << 8)
        | (u32::from(digest[offset + 3]) & 0xff);
    let code = bin % 10u32.pow(DIGITS);
    format!("{code:0width$}", width = DIGITS as usize)
}

// ---------- at-rest secret encryption (§4) ----------

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("HIVE_MFA_ENC_KEY not set; refusing to store a TOTP secret unencrypted")]
    MissingKey,
    #[error("HIVE_MFA_ENC_KEY must be base64 of exactly 32 bytes")]
    BadKey,
    #[error("MFA secret encryption failed")]
    Encrypt,
    #[error("MFA secret decryption failed (wrong key or corrupt data)")]
    Decrypt,
}

/// Resolve the 32-byte AEAD key. Production (no `dev` feature): require
/// `HIVE_MFA_ENC_KEY`. Dev build: fall back to a fixed, clearly-labeled throwaway
/// key with a loud warning, so local enrollment works without setup but can
/// never be mistaken for real protection.
fn aead_key() -> Result<[u8; 32], CryptoError> {
    match std::env::var("HIVE_MFA_ENC_KEY") {
        Ok(b64) if !b64.trim().is_empty() => {
            let raw = B64.decode(b64.trim()).map_err(|_| CryptoError::BadKey)?;
            let arr: [u8; 32] = raw.try_into().map_err(|_| CryptoError::BadKey)?;
            Ok(arr)
        }
        _ => {
            #[cfg(feature = "dev")]
            {
                tracing::warn!(
                    "HIVE_MFA_ENC_KEY unset — using a DEV throwaway key to encrypt TOTP \
                     secrets. NOT real at-rest protection; set HIVE_MFA_ENC_KEY for prod."
                );
                // Deterministic dev key so a restart can still decrypt dev secrets.
                Ok(*b"hive-dev-mfa-key-not-for-prod!!!")
            }
            #[cfg(not(feature = "dev"))]
            Err(CryptoError::MissingKey)
        }
    }
}

/// Encrypt a TOTP secret for storage: returns `nonce(12) || ciphertext`.
pub fn encrypt_secret(plaintext: &str) -> Result<Vec<u8>, CryptoError> {
    let key = aead_key()?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|_| CryptoError::Encrypt)?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a stored TOTP secret (`nonce(12) || ciphertext`).
pub fn decrypt_secret(blob: &[u8]) -> Result<String, CryptoError> {
    if blob.len() < 12 {
        return Err(CryptoError::Decrypt);
    }
    let key = aead_key()?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let (nonce_bytes, ct) = blob.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);
    let pt = cipher
        .decrypt(nonce, ct)
        .map_err(|_| CryptoError::Decrypt)?;
    String::from_utf8(pt).map_err(|_| CryptoError::Decrypt)
}

// ---------- helpers ----------

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 Appendix B test vector (SHA1, secret "12345678901234567890").
    // The ASCII secret base32-encodes to GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ.
    const RFC_SECRET_B32: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn rfc6238_vectors_match_8digit_truncation_base() {
        // RFC vectors are 8-digit; we run 6-digit, so check the low 6 digits of
        // the known 8-digit values. T=59s => 94287082 ; low 6 = 287082.
        assert_eq!(code_at(RFC_SECRET_B32, 59).unwrap(), "287082");
        // T=1111111109 => 07081804 ; low 6 = 081804.
        assert_eq!(code_at(RFC_SECRET_B32, 1111111109).unwrap(), "081804");
    }

    #[test]
    fn verify_accepts_current_and_skew_window() {
        let now = 1111111109u64;
        let code = code_at(RFC_SECRET_B32, now).unwrap();
        assert!(verify(RFC_SECRET_B32, &code, now));
        // within ±1 step (30s) still accepted
        assert!(verify(RFC_SECRET_B32, &code, now + 29));
        // two steps away is rejected
        assert!(!verify(RFC_SECRET_B32, &code, now + 90));
    }

    #[test]
    fn verify_rejects_wrong_code() {
        assert!(!verify(RFC_SECRET_B32, "000000", 59));
        assert!(!verify(RFC_SECRET_B32, "not-a-code", 59));
    }

    #[test]
    fn generated_secret_is_decodable_base32() {
        let s = generate_secret_base32();
        assert!(base32::decode(base32::Alphabet::Rfc4648 { padding: false }, &s).is_some());
        // a code computes without panicking
        assert!(code_at(&s, 1_700_000_000).is_some());
    }

    #[test]
    fn provisioning_uri_carries_secret_and_issuer() {
        let uri = provisioning_uri("ABC234", "hive", "nate");
        assert!(uri.starts_with("otpauth://totp/"));
        assert!(uri.contains("secret=ABC234"));
        assert!(uri.contains("issuer=hive"));
        assert!(uri.contains("digits=6"));
        assert!(uri.contains("period=30"));
    }

    // Encryption round-trip only runs under `dev` (where a key is always
    // available); prod requires HIVE_MFA_ENC_KEY which the test env doesn't set.
    #[cfg(feature = "dev")]
    #[test]
    fn encrypt_decrypt_roundtrips() {
        let secret = "GEZDGNBVGY3TQOJQ";
        let blob = encrypt_secret(secret).expect("encrypt");
        assert_ne!(&blob[12..], secret.as_bytes(), "ciphertext != plaintext");
        let back = decrypt_secret(&blob).expect("decrypt");
        assert_eq!(back, secret);
    }
}
