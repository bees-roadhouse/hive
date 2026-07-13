// The credential vault, encrypted at rest — today its ONLY consumer is mail
// account credentials (mail_accounts.cred_id names a row here; store/mail.rs
// writes and decrypts through it). Reversible (AES-256-GCM) because the mail
// sync driver needs the real secret. The key derives from the store's MASTER
// key (the OS-keychain secret, resolved at open) via a domain-separated
// SHA-256 — so the vault works wherever the store opens, with no external
// configuration. `HIVE_CRED_KEY`, when set, overrides it (a hosted-era / test
// escape hatch). Named cc_credentials for hosted-era reasons; Phase 3 folds
// it fully into the OS keychain.
//
// Cutover decision (PR 1.6): this stays a RUNTIME table in the derived index
// (see index/mod.rs) rather than becoming records — least churn, and the
// whole vault is scheduled to die in Phase 3. The AES-GCM layer is kept on
// top of SQLCipher so the trust story is unchanged from the Postgres era.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::RngCore;
use rusqlite::OptionalExtension;
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

/// Save-a-credential request. `secret` is plaintext in memory and is
/// encrypted immediately; it is never persisted in the clear.
#[derive(Debug, Clone, Deserialize)]
pub struct NewCcCredential {
    pub kind: String, // e.g. "api_key" | "oauth_token" | "subscription_login" | "provider_config"
    pub runtime: Option<String>,
    pub provider: Option<String>,
    pub label: Option<String>,
    pub secret: String,
}

/// Domain-separation label so the vault subkey is distinct from the SQLCipher
/// database key even though both descend from the same master key.
const VAULT_KDF_LABEL: &[u8] = b"hive:cc-credentials:v1";

/// Derive the 32-byte AES-GCM vault key. Normally `SHA-256(master ‖ label)` so
/// it is bound to the store's master key and available wherever the store
/// opens. `env_override` (from `HIVE_CRED_KEY`), when present and non-blank,
/// wins and is `SHA-256(override)` — kept as a hosted-era / test escape hatch.
/// Pure: the caller reads the env so this stays unit-testable without touching
/// process-global state.
fn vault_key(master: &[u8; 32], env_override: Option<&str>) -> [u8; 32] {
    let mut h = Sha256::new();
    match env_override {
        Some(raw) if !raw.trim().is_empty() => h.update(raw.as_bytes()),
        _ => {
            h.update(master);
            h.update(VAULT_KDF_LABEL);
        }
    }
    h.finalize().into()
}

/// The optional `HIVE_CRED_KEY` override, read once at a call site.
fn env_override() -> Option<String> {
    std::env::var("HIVE_CRED_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn encrypt(plaintext: &str, key: &[u8; 32]) -> Result<(String, String)> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow!("aes-gcm encrypt failed: {e}"))?;
    Ok((STANDARD.encode(ct), STANDARD.encode(nonce_bytes)))
}

fn decrypt(ciphertext_b64: &str, nonce_b64: &str, key: &[u8; 32]) -> Result<String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let ct = STANDARD
        .decode(ciphertext_b64)
        .context("bad ciphertext base64")?;
    let nonce_bytes = STANDARD.decode(nonce_b64).context("bad nonce base64")?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let pt = cipher
        .decrypt(nonce, ct.as_ref())
        .map_err(|e| anyhow!("aes-gcm decrypt failed (vault key mismatch): {e}"))?;
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
        let key = vault_key(&self.master, env_override().as_deref());
        let (ciphertext, nonce) = encrypt(&input.secret, &key)?;
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
        let view = CcCredentialView {
            id,
            owner: owner.to_string(),
            kind: input.kind,
            runtime,
            provider,
            label,
            tail,
            created_at: ts,
            last_used_at: None,
        };
        let row = view.clone();
        self.run(move |core| {
            core.conn().execute(
                "INSERT INTO cc_credentials (id, owner, kind, runtime, provider, label, ciphertext, nonce, tail, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    row.id, row.owner, row.kind, row.runtime, row.provider, row.label,
                    ciphertext, nonce, row.tail, row.created_at
                ],
            )?;
            Ok(())
        })
        .await?;
        self.emit(
            "credential.saved",
            owner,
            serde_json::json!({"id": view.id, "kind": view.kind, "runtime": view.runtime, "provider": view.provider}),
        )
        .await?;
        Ok(view)
    }

    /// Decrypt one credential by row id (INTERNAL only). Mail accounts name
    /// their vault row via `mail_accounts.cred_id`, so the most-recent-per-
    /// runtime picker above would be wrong the moment a second account
    /// exists.
    pub async fn cc_cred_decrypt_by_id(&self, id: &str) -> Result<Option<String>> {
        let id = id.to_string();
        let row: Option<(String, String, String)> = self
            .run(move |core| {
                let row = core
                    .conn()
                    .query_row(
                        "SELECT id, ciphertext, nonce FROM cc_credentials WHERE id = ?1",
                        rusqlite::params![id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                if let Some((rid, _, _)) = &row {
                    core.conn().execute(
                        "UPDATE cc_credentials SET last_used_at = ?1 WHERE id = ?2",
                        rusqlite::params![now_iso(), rid],
                    )?;
                }
                Ok(row)
            })
            .await?;
        let Some((_, ciphertext, nonce)) = row else {
            return Ok(None);
        };
        let key = vault_key(&self.master, env_override().as_deref());
        Ok(Some(decrypt(&ciphertext, &nonce, &key)?))
    }
}

#[cfg(test)]
mod tests {
    use super::vault_key;

    #[test]
    fn vault_key_is_deterministic_and_master_bound() {
        let m1 = [7u8; 32];
        let m2 = [9u8; 32];
        // Same master → same key; different master → different key.
        assert_eq!(vault_key(&m1, None), vault_key(&m1, None));
        assert_ne!(vault_key(&m1, None), vault_key(&m2, None));
    }

    #[test]
    fn env_override_wins_and_is_master_independent() {
        let m1 = [1u8; 32];
        let m2 = [2u8; 32];
        // A set override ignores the master (same key across masters)…
        assert_eq!(vault_key(&m1, Some("k")), vault_key(&m2, Some("k")));
        // …a blank/whitespace override falls back to master derivation…
        assert_eq!(vault_key(&m1, Some("   ")), vault_key(&m1, None));
        // …and the override key differs from the master-derived one.
        assert_ne!(vault_key(&m1, Some("k")), vault_key(&m1, None));
    }
}
