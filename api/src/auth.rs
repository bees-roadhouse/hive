use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Duration, Utc};
use hive_shared::{SafeUser, User, UserRole};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2.hash_password(password.as_bytes(), &salt)?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let parsed = match PasswordHash::new(hash) {
        Ok(h) => h,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

pub fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn generate_token(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("{}_{}", prefix, B64.encode(bytes).replace(['/', '+', '='], ""))
}

pub fn csrf_for(session_cookie: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_cookie.as_bytes());
    hasher.update(b":oauth-csrf");
    hex::encode(hasher.finalize())
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

pub fn expire(days: i64) -> DateTime<Utc> {
    Utc::now() + Duration::days(days)
}

// ---- JWT for potential future OIDC support ----

pub fn jwt_encode(claims: serde_json::Value, secret: &[u8]) -> anyhow::Result<String> {
    let header = jsonwebtoken::Header::default();
    let key = jsonwebtoken::EncodingKey::from_secret(secret);
    Ok(jsonwebtoken::encode(&header, &claims, &key)?)
}

pub fn jwt_decode(token: &str, secret: &[u8]) -> anyhow::Result<serde_json::Value> {
    let key = jsonwebtoken::DecodingKey::from_secret(secret);
    let mut validation = jsonwebtoken::Validation::default();
    validation.validate_exp = false;
    let data = jsonwebtoken::decode::<serde_json::Value>(token, &key, &validation)?;
    Ok(data.claims)
}
