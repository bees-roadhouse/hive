// Auth primitives — parity port of packages/api/src/auth.ts. Passwords use
// scrypt (N=16384, r=8, p=1, keylen=64) stored as `scrypt$<saltHex>$<hashHex>`
// so accounts created by the Node API keep working. Session + API tokens are
// random and stored only as a sha256 hex hash; plaintext is shown once.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const SESSION_PREFIX: &str = "hive_sess";
pub const API_TOKEN_PREFIX: &str = "hive_pat";
pub const AUTH_CODE_PREFIX: &str = "hive_ac";
pub const SESSION_COOKIE: &str = "hive_session";
/// Session lifetime: 30 days.
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60;
/// OAuth authorization-code lifetime: 60 seconds (single-use).
pub const AUTH_CODE_TTL_SECS: i64 = 60;
/// OAuth access-token lifetime: 1 year (renewable via re-consent). This is the
/// DEFAULT when the consent flow doesn't request a specific lifetime.
pub const OAUTH_TOKEN_TTL_SECS: i64 = 365 * 24 * 60 * 60;
/// OAuth access-token lifetime ceiling: 1 year. A requested lifetime is clamped
/// up to this.
pub const OAUTH_TOKEN_TTL_MAX_SECS: i64 = 365 * 24 * 60 * 60;
/// OAuth access-token lifetime floor: 1 hour. A requested lifetime is clamped
/// down to this.
pub const OAUTH_TOKEN_TTL_MIN_SECS: i64 = 60 * 60;

const SCRYPT_KEYLEN: usize = 64;

fn scrypt_params() -> scrypt::Params {
    // Node's scryptSync defaults: N=16384 (log2=14), r=8, p=1.
    scrypt::Params::new(14, 8, 1, SCRYPT_KEYLEN).expect("static scrypt params are valid")
}

/// Hash a password as `scrypt$<saltHex>$<hashHex>` (Node format).
pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let mut derived = [0u8; SCRYPT_KEYLEN];
    scrypt::scrypt(password.as_bytes(), &salt, &scrypt_params(), &mut derived)
        .expect("scrypt with valid params cannot fail");
    format!("scrypt${}${}", hex::encode(salt), hex::encode(derived))
}

/// Constant-time verify against a stored `scrypt$salt$hash` string.
pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some(scheme), Some(salt_hex), Some(hash_hex)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if scheme != "scrypt" || salt_hex.is_empty() || hash_hex.is_empty() {
        return false;
    }
    let (Ok(salt), Ok(expected)) = (hex::decode(salt_hex), hex::decode(hash_hex)) else {
        return false;
    };
    let mut actual = vec![0u8; expected.len()];
    if scrypt::scrypt(password.as_bytes(), &salt, &scrypt_params(), &mut actual).is_err() {
        return false;
    }
    expected.ct_eq(&actual).into()
}

/// A URL-safe opaque token, e.g. `hive_pat_<random>` (24 random bytes, base64url).
pub fn generate_token(prefix: &str) -> String {
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{prefix}_{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// sha256 hex — how tokens are stored and looked up.
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// PKCE S256 verify: base64url(sha256(verifier)) === challenge (constant-time).
pub fn verify_pkce(verifier: &str, challenge: &str) -> bool {
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    computed.as_bytes().ct_eq(challenge.as_bytes()).into()
}

/// CSRF token bound to the session cookie (stateless double-submit).
pub fn csrf_for(session_cookie: &str) -> String {
    token_hash(&format!("{session_cookie}:oauth-csrf"))
}

/// Current instant in the exact shape JS `new Date().toISOString()` produces —
/// millisecond precision, trailing `Z` — so rows sort lexicographically next to
/// rows written by the Node API.
pub fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// now + days, same ISO shape as `now_iso`.
pub fn iso_in_days(days: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::days(days))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// now + seconds, same ISO shape as `now_iso`.
pub fn iso_in_secs(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("correct horse");
        assert!(h.starts_with("scrypt$"));
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn verify_fails_closed_on_garbage() {
        assert!(!verify_password("x", "not-a-hash"));
        assert!(!verify_password("x", ""));
        assert!(!verify_password("x", "scrypt$$"));
        assert!(!verify_password("x", "scrypt$zz$zz"));
    }

    #[test]
    fn token_shapes_match_node() {
        let t = generate_token(API_TOKEN_PREFIX);
        assert!(t.starts_with("hive_pat_"));
        // 24 bytes base64url, unpadded → 32 chars.
        assert_eq!(t.len(), "hive_pat_".len() + 32);
        assert_eq!(
            token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn pkce_s256_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce(verifier, challenge));
        assert!(!verify_pkce("wrong", challenge));
    }

    #[test]
    fn iso_format_matches_js() {
        let s = now_iso();
        assert_eq!(s.len(), 24);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[19..20], ".");
    }
}
