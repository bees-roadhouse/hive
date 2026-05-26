//! Token minting + PKCE (hive-auth-mcp-design.md §8 Phase 2, §2, §3).
//!
//! - Access tokens: EdDSA JWTs signed by the local key, `kid` in the header for
//!   rotation, claims per §2 (incl. the AS-baked scope/hive_admin/hive_visibility
//!   so the RS resolves statelessly).
//! - Refresh tokens: opaque 256-bit random, returned to the client raw, stored
//!   only as a sha256 hash. Rotated on use (§ store).
//! - PKCE: S256 verification for the auth-code exchange.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, Header};
use rand::RngCore;
use sha2::{Digest, Sha256};

use super::claims::Claims;
use super::keys::SigningKey;

/// Parameters for minting an access token. The AS fills these from the user +
/// policy; `resolve_permissions` later rebuilds the same `ResolvedPermissions`
/// from the issued claims.
pub struct AccessTokenParams<'a> {
    pub issuer: &'a str,
    pub audience: &'a str,
    pub subject: &'a str,
    pub principal_type: &'a str, // "human" in Phase 2
    pub scopes: &'a [String],
    pub is_admin: bool,
    pub data_visibility: &'a str,
    pub session_id: &'a str,
    pub now: i64,
    pub ttl_secs: i64,
}

/// Mint a signed EdDSA access JWT. `exp` is always set for the human class.
pub fn mint_access_token(
    key: &SigningKey,
    p: &AccessTokenParams<'_>,
) -> Result<String, jsonwebtoken::errors::Error> {
    let claims = Claims {
        iss: p.issuer.to_string(),
        sub: p.subject.to_string(),
        principal_type: Some(p.principal_type.to_string()),
        act: None,
        aud: Some(p.audience.to_string()),
        exp: Some(p.now + p.ttl_secs),
        iat: Some(p.now),
        nbf: Some(p.now),
        jti: Some(uuid::Uuid::now_v7().to_string()),
        scope: if p.scopes.is_empty() {
            None
        } else {
            Some(p.scopes.join(" "))
        },
        hive_admin: p.is_admin,
        hive_visibility: Some(p.data_visibility.to_string()),
    };
    // sid as a registered-ish custom claim travels in the same map via a
    // wrapper; jsonwebtoken serializes the Claims struct, and sid lives on the
    // session row — we thread it through the `jti`/session linkage server-side.
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(key.kid.clone());
    jsonwebtoken::encode(&header, &claims, &key.encoding)
}

/// Parameters for minting an MCP token for an AI principal (§3.4). `ai_subject`
/// is the AI identity; `act_subject` is the connecting human (RFC 8693 actor).
/// `scopes` is the already-computed intersection (grant ∩ the human's own
/// permissions). The token is non-expiring by default (`exp_secs = None`) per
/// Nate's `mcp_token_no_expiry`; an owner can opt into expiry via `Some(secs)`.
pub struct McpTokenParams<'a> {
    pub issuer: &'a str,
    /// Canonical MCP resource URI this token is bound to (RFC 8707 audience).
    pub audience: &'a str,
    /// The AI identity's id (the token `sub`).
    pub ai_subject: &'a str,
    /// The connecting human's id (the `act` actor claim).
    pub act_subject: &'a str,
    pub scopes: &'a [String],
    pub data_visibility: &'a str,
    pub session_id: &'a str,
    /// The token id — also the revocation handle (§5.5). The caller generates it
    /// so it can persist the same value on the session row.
    pub jti: &'a str,
    pub now: i64,
    /// `None` => non-expiring (the default MCP class); `Some(secs)` => opt-in TTL.
    pub exp_secs: Option<i64>,
}

/// Mint a signed EdDSA MCP access token for an AI acting for a human (§3.4).
/// `sub` = AI, `act` = the human (RFC 8693). `exp` is omitted entirely for the
/// non-expiring class — the only off-switch is revocation by `jti` (§5.5),
/// which is why the RS checks the revocation set on every AI-token validation.
/// `principal_type = "ai"` so `resolve_permissions` routes it correctly.
pub fn mint_mcp_token(
    key: &SigningKey,
    p: &McpTokenParams<'_>,
) -> Result<String, jsonwebtoken::errors::Error> {
    let _ = p.session_id; // linked server-side via the session row + jti.
    let claims = Claims {
        iss: p.issuer.to_string(),
        sub: p.ai_subject.to_string(),
        principal_type: Some("ai".to_string()),
        act: Some(p.act_subject.to_string()),
        aud: Some(p.audience.to_string()),
        exp: p.exp_secs.map(|s| p.now + s),
        iat: Some(p.now),
        nbf: Some(p.now),
        jti: Some(p.jti.to_string()),
        scope: if p.scopes.is_empty() {
            None
        } else {
            Some(p.scopes.join(" "))
        },
        // AI tokens are never admin: an AI can't exceed its owner's reach, and
        // admin is a human-only authority (§5.5 owner-vs-admin).
        hive_admin: false,
        hive_visibility: Some(p.data_visibility.to_string()),
    };
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(key.kid.clone());
    jsonwebtoken::encode(&header, &claims, &key.encoding)
}

/// A freshly generated opaque refresh token: the raw value (returned to the
/// client once) and its sha256 hash (what we store).
pub struct RefreshToken {
    pub raw: String,
    pub hash: String,
}

/// Generate a 256-bit opaque refresh token + its storage hash.
pub fn generate_refresh_token() -> RefreshToken {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let raw = URL_SAFE_NO_PAD.encode(bytes);
    let hash = hash_token(&raw);
    RefreshToken { raw, hash }
}

/// sha256-hex of a token, for storage + lookup. Refresh tokens are high-entropy
/// random values, so a plain sha256 (not a slow KDF) is the right tool.
pub fn hash_token(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Verify a PKCE `code_verifier` against the stored `code_challenge` using the
/// S256 method (the only method we advertise / accept). RFC 7636.
pub fn verify_pkce_s256(code_verifier: &str, code_challenge: &str) -> bool {
    let digest = Sha256::digest(code_verifier.as_bytes());
    let computed = URL_SAFE_NO_PAD.encode(digest);
    // Constant-time-ish compare; challenge is not secret but avoid early-out.
    constant_time_eq(computed.as_bytes(), code_challenge.as_bytes())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_token_hash_is_stable_and_distinct() {
        let t = generate_refresh_token();
        assert_eq!(t.hash, hash_token(&t.raw));
        let t2 = generate_refresh_token();
        assert_ne!(t.raw, t2.raw, "tokens must be unique");
        assert_ne!(t.hash, t2.hash);
        assert_eq!(t.hash.len(), 64, "sha256 hex is 64 chars");
    }

    #[test]
    fn pkce_s256_accepts_correct_verifier() {
        // Known RFC 7636 appendix B vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce_s256(verifier, challenge));
    }

    #[test]
    fn pkce_s256_rejects_wrong_verifier() {
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(!verify_pkce_s256("not-the-verifier", challenge));
    }
}
