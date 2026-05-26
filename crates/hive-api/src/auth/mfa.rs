//! MFA credential + recovery-code store (hive-auth-mcp-design.md §4).
//!
//! The DB side of TOTP MFA: the per-user encrypted secret + its confirm state +
//! lockout counters, and the hashed single-use recovery codes. Runtime sqlx
//! only (no compile-time macros), so it builds without a live DB.

use chrono::{DateTime, Utc};
use hive_db::PgPool;
use uuid::Uuid;

use super::store::StoreError;
use super::tokens;

/// Lockout policy for repeated TOTP failures (§4 account-lockdown). After
/// `MAX_FAILURES` bad codes the credential is locked for `LOCKOUT_SECS`.
const MAX_FAILURES: i32 = 5;
const LOCKOUT_SECS: i64 = 300; // 5 minutes

/// A user's MFA credential row.
#[derive(Debug, Clone)]
pub struct MfaCredential {
    pub secret_enc: Vec<u8>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub failed_attempts: i32,
    pub locked_until: Option<DateTime<Utc>>,
}

impl MfaCredential {
    /// Confirmed = the user proved possession and MFA is active for them.
    pub fn is_confirmed(&self) -> bool {
        self.confirmed_at.is_some()
    }

    /// Currently locked out from TOTP attempts?
    pub fn is_locked(&self) -> bool {
        self.locked_until.is_some_and(|t| t > Utc::now())
    }
}

/// Fetch a user's MFA credential, if any.
pub async fn get_credential(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<MfaCredential>, StoreError> {
    let row = sqlx::query_as::<_, (Vec<u8>, Option<DateTime<Utc>>, i32, Option<DateTime<Utc>>)>(
        "SELECT secret_enc, confirmed_at, failed_attempts, locked_until \
         FROM mfa_credentials WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| MfaCredential {
        secret_enc: r.0,
        confirmed_at: r.1,
        failed_attempts: r.2,
        locked_until: r.3,
    }))
}

/// Does the user have a CONFIRMED credential (i.e. MFA gates their login)?
pub async fn has_confirmed_mfa(pool: &PgPool, user_id: Uuid) -> Result<bool, StoreError> {
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT count(*) FROM mfa_credentials WHERE user_id = $1 AND confirmed_at IS NOT NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0 > 0)
}

/// Begin (or restart) enrollment: store the encrypted secret with
/// `confirmed_at = NULL`. Re-enrolling overwrites a prior *pending* secret and
/// resets counters. Returns nothing; the caller already holds the plaintext for
/// the provisioning URI.
pub async fn upsert_pending_secret(
    pool: &PgPool,
    user_id: Uuid,
    secret_enc: &[u8],
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO mfa_credentials (user_id, secret_enc, confirmed_at, failed_attempts, locked_until) \
         VALUES ($1, $2, NULL, 0, NULL) \
         ON CONFLICT (user_id) DO UPDATE SET \
           secret_enc = EXCLUDED.secret_enc, confirmed_at = NULL, \
           failed_attempts = 0, locked_until = NULL, updated_at = now()",
    )
    .bind(user_id)
    .bind(secret_enc)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark the credential confirmed (enrollment verified). Idempotent.
pub async fn confirm(pool: &PgPool, user_id: Uuid) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE mfa_credentials SET confirmed_at = now(), failed_attempts = 0, \
         locked_until = NULL, updated_at = now() WHERE user_id = $1",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a user's MFA entirely (disable MFA): drops the credential + all
/// recovery codes in one transaction.
pub async fn remove(pool: &PgPool, user_id: Uuid) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM mfa_recovery_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM mfa_credentials WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Record a failed TOTP attempt; lock the credential when the threshold is hit.
/// Returns whether the credential is now locked.
pub async fn record_failure(pool: &PgPool, user_id: Uuid) -> Result<bool, StoreError> {
    let row = sqlx::query_as::<_, (i32,)>(
        "UPDATE mfa_credentials SET failed_attempts = failed_attempts + 1, \
           locked_until = CASE WHEN failed_attempts + 1 >= $2 \
             THEN now() + ($3 || ' seconds')::interval ELSE locked_until END, \
           updated_at = now() \
         WHERE user_id = $1 RETURNING failed_attempts",
    )
    .bind(user_id)
    .bind(MAX_FAILURES)
    .bind(LOCKOUT_SECS.to_string())
    .fetch_one(pool)
    .await?;
    Ok(row.0 >= MAX_FAILURES)
}

/// Clear the failure counter after a successful factor (TOTP or recovery code).
pub async fn record_success(pool: &PgPool, user_id: Uuid) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE mfa_credentials SET failed_attempts = 0, locked_until = NULL, \
         updated_at = now() WHERE user_id = $1",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replace a user's recovery codes with fresh hashes (issued at confirm-time).
/// Caller holds the plaintext codes to show once; we store only the sha256 hex.
pub async fn replace_recovery_codes(
    pool: &PgPool,
    user_id: Uuid,
    plaintext_codes: &[String],
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM mfa_recovery_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for code in plaintext_codes {
        sqlx::query("INSERT INTO mfa_recovery_codes (user_id, code_hash) VALUES ($1, $2)")
            .bind(user_id)
            .bind(tokens::hash_token(code))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Redeem a recovery code: if an unused code matches, mark it used and return
/// true (single-use). A used or unknown code returns false. The hash compare
/// happens in the DB via the unique (user_id, code_hash) index.
pub async fn redeem_recovery_code(
    pool: &PgPool,
    user_id: Uuid,
    presented: &str,
) -> Result<bool, StoreError> {
    let hash = tokens::hash_token(presented.trim());
    let updated = sqlx::query(
        "UPDATE mfa_recovery_codes SET used_at = now() \
         WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(&hash)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// How many recovery codes remain unused (surfaced to the user).
pub async fn remaining_recovery_codes(pool: &PgPool, user_id: Uuid) -> Result<i64, StoreError> {
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT count(*) FROM mfa_recovery_codes WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Generate `n` recovery codes: 10 hex chars each (40 bits of entropy), grouped
/// `xxxxx-xxxxx` for readability. Returned plaintext (shown once); stored hashed.
pub fn generate_recovery_codes(n: usize) -> Vec<String> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| {
            let a: u32 = rng.gen_range(0..0x10_0000);
            let b: u32 = rng.gen_range(0..0x10_0000);
            format!("{a:05x}-{b:05x}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_codes_have_expected_shape() {
        let codes = generate_recovery_codes(8);
        assert_eq!(codes.len(), 8);
        for c in &codes {
            assert_eq!(c.len(), 11, "xxxxx-xxxxx");
            assert_eq!(c.as_bytes()[5], b'-');
        }
        // unique
        let mut sorted = codes.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len());
    }
}
