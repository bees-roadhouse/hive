//! Ed25519 signing keys for the local AS (§8 Phase 1, open decision #4: EdDSA).
//!
//! Phase 1 needs exactly one keypair: enough to publish a JWKS at `/jwks.json`
//! and to verify EdDSA JWTs. Minting tokens is Phase 2; here we only generate +
//! load + expose the verifying side.
//!
//! Keygen uses `ring` (already in-tree via rustls) to produce a PKCS#8 v2
//! Ed25519 key. `jsonwebtoken` consumes the PKCS#8 DER for the (future) signing
//! side and the raw 32-byte public key for the verifying side. The JWK is an
//! OKP/Ed25519 key per RFC 8037.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{DecodingKey, EncodingKey};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};

use hive_db::PgPool;

/// A loaded signing key: the kid, the (future) signing key, the verifying key,
/// and the published JWK.
pub struct SigningKey {
    pub kid: String,
    /// Signing side. Unused in Phase 1 (no minting yet) but loaded so Phase 2
    /// can issue without re-deriving. Held for the AS token endpoint.
    #[allow(dead_code)]
    pub encoding: EncodingKey,
    pub decoding: DecodingKey,
    pub public_jwk: Value,
}

/// Error type local to key handling, folded into `anyhow` at the call site.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("ed25519 keygen failed")]
    Keygen,
    #[error("ed25519 key parse failed")]
    Parse,
    #[error("jsonwebtoken key load failed: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error(transparent)]
    Db(#[from] hive_db::Error),
}

/// Build a `SigningKey` from a PKCS#8 DER private key + the kid.
fn from_pkcs8(kid: String, pkcs8_der: &[u8]) -> Result<SigningKey, KeyError> {
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8_der).map_err(|_| KeyError::Parse)?;
    let public_raw = pair.public_key().as_ref().to_vec();

    let encoding = EncodingKey::from_ed_der(pkcs8_der);
    let decoding = DecodingKey::from_ed_der(&public_raw);
    let public_jwk = ed25519_jwk(&kid, &public_raw);

    Ok(SigningKey {
        kid,
        encoding,
        decoding,
        public_jwk,
    })
}

/// The OKP/Ed25519 public JWK (RFC 8037) for `/jwks.json`.
fn ed25519_jwk(kid: &str, public_raw: &[u8]) -> Value {
    json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "use": "sig",
        "alg": "EdDSA",
        "kid": kid,
        "x": URL_SAFE_NO_PAD.encode(public_raw),
    })
}

/// Generate a fresh Ed25519 keypair. Returns (kid, pkcs8_der, public_jwk).
fn generate() -> Result<(String, Vec<u8>, Value), KeyError> {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| KeyError::Keygen)?;
    let pkcs8_der = pkcs8.as_ref().to_vec();
    // kid is a stable hash-free identifier; a UUIDv7 keeps it sortable + unique.
    let kid = uuid::Uuid::now_v7().to_string();
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8_der).map_err(|_| KeyError::Parse)?;
    let public_jwk = ed25519_jwk(&kid, pair.public_key().as_ref());
    Ok((kid, pkcs8_der, public_jwk))
}

/// Load the active signing key from `signing_keys`, generating + persisting one
/// on first run. Single-active invariant is enforced by the partial unique
/// index in migration 0004.
pub async fn load_or_create_active(pool: &PgPool) -> Result<SigningKey, KeyError> {
    if let Some(row) = sqlx::query_as::<_, (String, Vec<u8>)>(
        "SELECT kid, private_key_der FROM signing_keys WHERE active = TRUE LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(hive_db::Error::from)?
    {
        return from_pkcs8(row.0, &row.1);
    }

    let (kid, pkcs8_der, public_jwk) = generate()?;
    sqlx::query(
        "INSERT INTO signing_keys (kid, alg, private_key_der, public_jwk, active) \
         VALUES ($1, 'EdDSA', $2, $3, TRUE)",
    )
    .bind(&kid)
    .bind(&pkcs8_der)
    .bind(&public_jwk)
    .execute(pool)
    .await
    .map_err(hive_db::Error::from)?;

    tracing::info!(kid = %kid, "generated new Ed25519 signing key for local AS");
    from_pkcs8(kid, &pkcs8_der)
}

/// The full JWKS document published at `/jwks.json`.
pub fn jwks_document(key: &SigningKey) -> Value {
    json!({ "keys": [key.public_jwk.clone()] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_then_load_roundtrips() {
        let (kid, der, jwk) = generate().expect("keygen");
        let loaded = from_pkcs8(kid.clone(), &der).expect("from_pkcs8");
        assert_eq!(loaded.kid, kid);
        assert_eq!(loaded.public_jwk, jwk);
        assert_eq!(jwk["kty"], "OKP");
        assert_eq!(jwk["crv"], "Ed25519");
        assert_eq!(jwk["alg"], "EdDSA");
        assert_eq!(jwk["kid"], kid);
        assert!(jwk["x"].as_str().is_some_and(|x| !x.is_empty()));
    }

    #[test]
    fn jwks_document_wraps_key_in_keys_array() {
        let (kid, der, _) = generate().expect("keygen");
        let key = from_pkcs8(kid, &der).expect("from_pkcs8");
        let doc = jwks_document(&key);
        assert_eq!(doc["keys"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(doc["keys"][0]["crv"], "Ed25519");
    }
}
