//! argon2id password hashing for the built-in AS (hive-auth-mcp-design.md
//! §8 Phase 2, §4/§5). Stores + verifies PHC strings (salt embedded).

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("password hashing failed")]
    Hash,
    #[error("password too short: {0} < {1} minimum")]
    TooShort(usize, usize),
}

/// Hash a plaintext password into an argon2id PHC string (salt generated +
/// embedded). `min_length` is the policy minimum (BR: 14) — enforced here so
/// no short password ever reaches the store.
pub fn hash_password(plaintext: &str, min_length: usize) -> Result<String, PasswordError> {
    // Count chars, not bytes, for the length policy.
    let len = plaintext.chars().count();
    if len < min_length {
        return Err(PasswordError::TooShort(len, min_length));
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| PasswordError::Hash)
}

/// Verify a plaintext password against a stored argon2id PHC string. Returns
/// `false` on any mismatch or malformed hash (never leaks which).
pub fn verify_password(plaintext: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(plaintext.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify_roundtrips() {
        let phc = hash_password("correct horse battery staple", 14).expect("hash");
        assert!(phc.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong password entirely", &phc));
    }

    #[test]
    fn rejects_password_below_min_length() {
        let got = hash_password("short", 14);
        assert!(matches!(got, Err(PasswordError::TooShort(5, 14))));
    }

    #[test]
    fn verify_rejects_malformed_hash() {
        assert!(!verify_password("anything", "not-a-phc-string"));
    }
}
