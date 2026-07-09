// Per-user Claude Code credentials, encrypted at rest. The runner needs the real
// token to launch Claude Code, so this is *reversible* (AES-256-GCM) — unlike PATs
// and passwords, which are hashed. The key derives from HIVE_CRED_KEY (any string;
// SHA-256 → 32-byte key). Plaintext is returned ONLY to the internal runtime-auth
// path (cc_cred_decrypt_for_runtime), never to a public route.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::workspaces::normalize_runtime;
use super::{new_id, now_iso, Store};

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
struct CredViewRow {
    id: String,
    owner: String,
    kind: String,
    runtime: String,
    provider: Option<String>,
    label: String,
    tail: String,
    created_at: String,
    last_used_at: Option<String>,
}

#[derive(sqlx::FromRow)]
struct CredSecretRow {
    id: String,
    kind: String,
    runtime: String,
    provider: Option<String>,
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

    /// Redacted list of an owner's credentials.
    pub async fn cc_cred_list(&self, owner: &str) -> Result<Vec<CcCredentialView>> {
        let rows = crate::pgq::query_as::<CredViewRow>(
            "SELECT id, owner, kind, runtime, provider, label, tail, created_at, last_used_at \
             FROM cc_credentials WHERE owner = ? ORDER BY created_at DESC",
        )
        .bind(owner)
        .fetch_all(self.db())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| CcCredentialView {
                id: r.id,
                owner: r.owner,
                kind: r.kind,
                runtime: normalize_runtime(Some(&r.runtime)),
                provider: r.provider,
                label: r.label,
                tail: r.tail,
                created_at: r.created_at,
                last_used_at: r.last_used_at,
            })
            .collect())
    }

    pub async fn cc_cred_delete(&self, owner: &str, id: &str) -> Result<bool> {
        let res = crate::pgq::query("DELETE FROM cc_credentials WHERE id = ? AND owner = ?")
            .bind(id)
            .bind(owner)
            .execute(self.db())
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Decrypt the owner's most recent credential for the requested runtime (INTERNAL only —
    /// the only path that ever yields plaintext). Returns `(kind, runtime, provider, secret)`.
    pub async fn cc_cred_decrypt_for_runtime(
        &self,
        owner: &str,
        runtime: &str,
    ) -> Result<Option<(String, String, Option<String>, String)>> {
        let runtime = normalize_runtime(Some(runtime));
        let row = crate::pgq::query_as::<CredSecretRow>(
            "SELECT id, kind, runtime, provider, ciphertext, nonce FROM cc_credentials \
             WHERE owner = ? AND runtime = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(owner)
        .bind(&runtime)
        .fetch_optional(self.db())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let secret = decrypt(&row.ciphertext, &row.nonce)?;
        crate::pgq::query("UPDATE cc_credentials SET last_used_at = ? WHERE id = ?")
            .bind(now_iso())
            .bind(&row.id)
            .execute(self.db())
            .await?;
        Ok(Some((
            row.kind,
            normalize_runtime(Some(&row.runtime)),
            row.provider,
            secret,
        )))
    }

    /// Decrypt one credential by row id (INTERNAL only). Mail accounts name
    /// their vault row via `mail_accounts.cred_id`, so the most-recent-per-
    /// runtime picker above would be wrong the moment a second account
    /// exists.
    pub async fn cc_cred_decrypt_by_id(&self, id: &str) -> Result<Option<String>> {
        let row = crate::pgq::query_as::<CredSecretRow>(
            "SELECT id, kind, runtime, provider, ciphertext, nonce FROM cc_credentials WHERE id = ?",
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
