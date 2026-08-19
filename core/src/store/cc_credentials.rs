// Per-user Claude Code credentials, encrypted at rest. The runner needs the real
// token to launch Claude Code, so this is *reversible* (AES-256-GCM) — unlike PATs
// and passwords, which are hashed. Plaintext is returned ONLY to the internal
// runtime-auth path (cc_cred_decrypt_for_runtime), never to a public route.
//
// The AES key derives from HIVE_CRED_KEY, and the derivation is versioned per row
// (`cc_credentials.kdf_version`): 1 = bare SHA-256 (the original derivation —
// existing rows stay readable), 2 = scrypt under a fixed domain-separation salt
// (every new write). If ciphertexts ever leak, a v2 row's key is brute-forceable
// at scrypt speed instead of SHA-256 speed.
//
// scrypt SLOWS brute force; it does not rescue a dictionary word. HIVE_CRED_KEY
// must be a high-entropy generated string (32+ random bytes, base64'd, is the
// right shape), backed up separately from the database: losing it strands every
// row, and leaking it together with the database exposes every row.

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
    kdf_version: i32,
}

/// Row marker for the original derivation: bare SHA-256(HIVE_CRED_KEY). Rows
/// sealed under it predate `kdf_version`; the migration's DEFAULT 1 labels
/// them, and they stay decryptable here forever.
const KDF_V1_SHA256: i32 = 1;
/// Row marker for the current write path: scrypt under [KDF_V2_SALT].
const KDF_V2_SCRYPT: i32 = 2;

/// Domain-separation salt for the v2 scrypt derivation. Fixed on purpose: the
/// vault key is the ONLY secret in this system, so a per-row salt would add
/// storage without meaningfully slowing a brute-force of that one key — while
/// a fixed salt unique to this purpose keeps the derived key from colliding
/// with any other scrypt user of the same passphrase.
const KDF_V2_SALT: &[u8] = b"hive/cc_credentials/kdf-v2";

fn derive_key(raw: &str, version: i32) -> Result<[u8; 32]> {
    match version {
        KDF_V1_SHA256 => {
            let mut h = Sha256::new();
            h.update(raw.as_bytes());
            Ok(h.finalize().into())
        }
        KDF_V2_SCRYPT => {
            // N=2^15, r=8, p=1: ~32 MiB and tens of milliseconds per
            // derivation — a work factor an offline brute-force pays per
            // guess, cheap on the rare decrypt-on-use paths here. A step up
            // from the 2^14 auth.rs is bound to by Node password parity.
            let params = scrypt::Params::new(15, 8, 1, 32).expect("static scrypt params are valid");
            let mut key = [0u8; 32];
            scrypt::scrypt(raw.as_bytes(), KDF_V2_SALT, &params, &mut key)
                .expect("scrypt with valid params cannot fail");
            Ok(key)
        }
        other => Err(anyhow!(
            "cc_credentials row has unknown kdf_version {other}; refusing to guess the derivation"
        )),
    }
}

fn cred_key(version: i32) -> Result<[u8; 32]> {
    let raw = std::env::var("HIVE_CRED_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("HIVE_CRED_KEY is not set; the credential vault is disabled"))?;
    derive_key(&raw, version)
}

fn encrypt_with(key: &[u8; 32], plaintext: &str) -> Result<(String, String)> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow!("aes-gcm encrypt failed: {e}"))?;
    Ok((STANDARD.encode(ct), STANDARD.encode(nonce_bytes)))
}

fn decrypt_with(key: &[u8; 32], ciphertext_b64: &str, nonce_b64: &str) -> Result<String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
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
        let key = cred_key(KDF_V2_SCRYPT)?;
        let (ciphertext, nonce) = encrypt_with(&key, &input.secret)?;
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
            "INSERT INTO cc_credentials (id, owner, kind, runtime, provider, label, ciphertext, nonce, tail, kdf_version, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(KDF_V2_SCRYPT)
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
            "SELECT id, kind, runtime, provider, ciphertext, nonce, kdf_version FROM cc_credentials \
             WHERE owner = ? AND runtime = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(owner)
        .bind(&runtime)
        .fetch_optional(self.db())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let key = cred_key(row.kdf_version)?;
        let secret = decrypt_with(&key, &row.ciphertext, &row.nonce)?;
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
            "SELECT id, kind, runtime, provider, ciphertext, nonce, kdf_version FROM cc_credentials WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        let Some(row) = row else { return Ok(None) };
        let key = cred_key(row.kdf_version)?;
        let secret = decrypt_with(&key, &row.ciphertext, &row.nonce)?;
        crate::pgq::query("UPDATE cc_credentials SET last_used_at = ? WHERE id = ?")
            .bind(now_iso())
            .bind(&row.id)
            .execute(self.db())
            .await?;
        Ok(Some(secret))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known-answer pins for both derivations. If either of these ever changes,
    // every row sealed under that version becomes unreadable — that is a data
    // loss event, not a refactor.
    #[test]
    fn kdf_v1_is_bare_sha256() {
        let key = derive_key("cred-kdf-fixture-key", KDF_V1_SHA256).unwrap();
        assert_eq!(
            hex::encode(key),
            "c651eefdaaa39d0e4921628e144935749cb1c97b0b5c97776255451fb3a55cbe"
        );
    }

    #[test]
    fn kdf_v2_is_scrypt_with_domain_salt() {
        let key = derive_key("cred-kdf-fixture-key", KDF_V2_SCRYPT).unwrap();
        assert_eq!(
            hex::encode(key),
            "5bf88402d2f776470c2d944ccf9847f0ff7e909d76f70c84eddc9ff698dcc45a"
        );
    }

    #[test]
    fn unknown_kdf_version_is_an_error_not_a_guess() {
        assert!(derive_key("any-key", 7).is_err());
    }

    #[test]
    fn round_trip_both_versions_and_wrong_key_fails_both() {
        for version in [KDF_V1_SHA256, KDF_V2_SCRYPT] {
            let key = derive_key("right-key", version).unwrap();
            let (ct, nonce) = encrypt_with(&key, "sekrit").unwrap();
            assert_eq!(decrypt_with(&key, &ct, &nonce).unwrap(), "sekrit");
            let wrong = derive_key("wrong-key", version).unwrap();
            assert!(
                decrypt_with(&wrong, &ct, &nonce).is_err(),
                "kdf_version {version} must fail decryption under a wrong HIVE_CRED_KEY"
            );
        }
    }

    /// Backward compatibility: a row sealed under the original SHA-256
    /// derivation (the shape every pre-`kdf_version` row has) still decrypts
    /// through the read path, which picks the KDF by row version.
    #[tokio::test]
    async fn v1_rows_still_decrypt() {
        // Every vault-touching test in this binary sets HIVE_CRED_KEY to the
        // same value (the documented suite key), so the fixture below can be
        // derived from that literal and the store's env read always agrees,
        // no matter how tests interleave.
        std::env::set_var("HIVE_CRED_KEY", "dev-credential-vault-key");
        let test_db = crate::db::test_pool().await;
        let store = Store::new(test_db.pool.clone());

        let key = derive_key("dev-credential-vault-key", KDF_V1_SHA256).unwrap();
        let (ct, nonce) = encrypt_with(&key, "fixture-secret-v1").unwrap();
        crate::pgq::query(
            "INSERT INTO cc_credentials (id, owner, kind, runtime, label, ciphertext, nonce, tail, kdf_version, created_at) \
             VALUES ('cred-v1-fixture', 'kdf-owner', 'api_key', 'claude_code', '', ?, ?, '…v1', 1, '2026-01-01T00:00:00.000Z')",
        )
        .bind(&ct)
        .bind(&nonce)
        .execute(store.db())
        .await
        .unwrap();

        let secret = store
            .cc_cred_decrypt_by_id("cred-v1-fixture")
            .await
            .unwrap();
        assert_eq!(secret.as_deref(), Some("fixture-secret-v1"));
    }

    /// New writes are always v2 and round-trip through both read paths.
    #[tokio::test]
    async fn new_writes_are_v2_and_round_trip() {
        std::env::set_var("HIVE_CRED_KEY", "dev-credential-vault-key");
        let test_db = crate::db::test_pool().await;
        let store = Store::new(test_db.pool.clone());

        let view = store
            .cc_cred_put(
                "kdf-owner",
                NewCcCredential {
                    kind: "api_key".into(),
                    runtime: None,
                    provider: None,
                    label: None,
                    secret: "fresh-secret-v2".into(),
                },
            )
            .await
            .unwrap();

        let version: i32 =
            crate::pgq::query_scalar::<i32>("SELECT kdf_version FROM cc_credentials WHERE id = ?")
                .bind(&view.id)
                .fetch_one(store.db())
                .await
                .unwrap();
        assert_eq!(version, KDF_V2_SCRYPT);
        let secret = store.cc_cred_decrypt_by_id(&view.id).await.unwrap();
        assert_eq!(secret.as_deref(), Some("fresh-secret-v2"));
        let picked = store
            .cc_cred_decrypt_for_runtime("kdf-owner", "claude_code")
            .await
            .unwrap();
        assert_eq!(picked.map(|t| t.3).as_deref(), Some("fresh-secret-v2"));
    }
}
