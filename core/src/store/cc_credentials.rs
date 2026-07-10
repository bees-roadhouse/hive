// The credential vault, encrypted at rest — today its ONLY consumer is mail
// account credentials (mail_accounts.cred_id names a row here; store/mail.rs
// writes and decrypts through it). Reversible (AES-256-GCM) because the mail
// sync driver needs the real secret. The key derives from HIVE_CRED_KEY (any
// string; SHA-256 → 32-byte key). Named cc_credentials for hosted-era
// reasons; Phase 3 replaces it with the OS keychain.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{new_id, now_iso, Store};

/// Canonical runtime names (inherited shape; mail rows use "jmap").
fn normalize_runtime(runtime: Option<&str>) -> String {
    match runtime.unwrap_or("claude_code").trim() {
        "" | "claude" | "claude_code" => "claude_code".to_string(),
        other => other.to_string(),
    }
}

/// A stored credential, redacted for display — never the secret itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CcCredentialView {
    pub id: String,
    pub owner: String,
    pub kind: String,
    pub runtime: String,
    pub provider: Option<String>,
    pub label: String,
    pub tail: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

/// Save-a-credential request. `secret` is plaintext on the wire (TLS) and is
/// encrypted server-side immediately; it is never persisted in the clear.
#[derive(Debug, Clone, Deserialize)]
pub struct NewCcCredential {
    pub kind: String, // e.g. "api_key" | "oauth_token" | "subscription_login" | "provider_config"
    pub runtime: Option<String>,
    pub provider: Option<String>,
    pub label: Option<String>,
    pub secret: String,
}

#[derive(sqlx::FromRow)]
struct CredSecretRow {
    id: String,
    ciphertext: String,
    nonce: String,
}

fn cred_key() -> Result<[u8; 32]> {
    let raw = std::env::var("HIVE_CRED_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("HIVE_CRED_KEY is not set; the credential vault is disabled"))?;
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    Ok(h.finalize().into())
}

fn encrypt(plaintext: &str) -> Result<(String, String)> {
    let key = cred_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow!("aes-gcm encrypt failed: {e}"))?;
    Ok((STANDARD.encode(ct), STANDARD.encode(nonce_bytes)))
}

fn decrypt(ciphertext_b64: &str, nonce_b64: &str) -> Result<String> {
    let key = cred_key()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let ct = STANDARD
        .decode(ciphertext_b64)
        .context("bad ciphertext base64")?;
    let nonce_bytes = STANDARD.decode(nonce_b64).context("bad nonce base64")?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let pt = cipher
        .decrypt(nonce, ct.as_ref())
        .map_err(|e| anyhow!("aes-gcm decrypt failed (wrong HIVE_CRED_KEY?): {e}"))?;
    String::from_utf8(pt).context("decrypted credential is not utf-8")
}

fn tail_of(secret: &str) -> String {
    let n = secret.chars().count();
    let last4: String = secret.chars().skip(n.saturating_sub(4)).collect();
    format!("…{last4}")
}

impl Store {
    /// Encrypt and store a credential for `owner`. Returns the redacted view.
    pub async fn cc_cred_put(
        &self,
        owner: &str,
        input: NewCcCredential,
    ) -> Result<CcCredentialView> {
        let (ciphertext, nonce) = encrypt(&input.secret)?;
        let id = new_id("cred");
        let ts = now_iso();
        let runtime = normalize_runtime(input.runtime.as_deref());
        let provider = input
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let label = input.label.unwrap_or_default();
        let tail = tail_of(&input.secret);
        crate::pgq::query(
            "INSERT INTO cc_credentials (id, owner, kind, runtime, provider, label, ciphertext, nonce, tail, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(owner)
        .bind(&input.kind)
        .bind(&runtime)
        .bind(&provider)
        .bind(&label)
        .bind(&ciphertext)
        .bind(&nonce)
        .bind(&tail)
        .bind(&ts)
        .execute(self.db())
        .await?;
        self.emit(
            "credential.saved",
            owner,
            serde_json::json!({"id": id, "kind": input.kind, "runtime": runtime, "provider": provider}),
        )
        .await?;
        Ok(CcCredentialView {
            id,
            owner: owner.to_string(),
            kind: input.kind,
            runtime,
            provider,
            label,
            tail,
            created_at: ts,
            last_used_at: None,
        })
    }

    /// Decrypt one credential by row id (INTERNAL only). Mail accounts name
    /// their vault row via `mail_accounts.cred_id`, so the most-recent-per-
    /// runtime picker above would be wrong the moment a second account
    /// exists.
    pub async fn cc_cred_decrypt_by_id(&self, id: &str) -> Result<Option<String>> {
        let row = crate::pgq::query_as::<CredSecretRow>(
            "SELECT id, ciphertext, nonce FROM cc_credentials WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let secret = decrypt(&row.ciphertext, &row.nonce)?;
        crate::pgq::query("UPDATE cc_credentials SET last_used_at = ? WHERE id = ?")
            .bind(now_iso())
            .bind(&row.id)
            .execute(self.db())
            .await?;
        Ok(Some(secret))
    }
}
