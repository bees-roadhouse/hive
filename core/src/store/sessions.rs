// Browser sessions, cookie auth (store.ts `sessions`).

use anyhow::Result;
use chrono::Utc;
use hive_shared::User;
use sqlx::Row;
use uuid::Uuid;

use crate::auth::{generate_token, iso_in_secs, token_hash, SESSION_PREFIX, SESSION_TTL_SECS};

use super::{new_id, now_iso, Store};

impl Store {
    /// Create a session pinned to ONE org; returns the plaintext cookie value
    /// (hash stored). The org is decided here, once, and is immutable for the
    /// life of the session: there is no `sessions_set_org`. A human who wants a
    /// different org logs in again and gets a different cookie.
    pub async fn sessions_create(&self, user_id: &str, org: Uuid) -> Result<String> {
        let token = generate_token(SESSION_PREFIX);
        let ts = now_iso();
        crate::pgq::query(
            "INSERT INTO sessions (id, token_hash, user_id, org_id, created_at, expires_at, last_seen) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(new_id("ses"))
        .bind(token_hash(&token))
        .bind(user_id)
        .bind(org)
        .bind(&ts)
        .bind(iso_in_secs(SESSION_TTL_SECS))
        .bind(&ts)
        .execute(self.db())
        .await?;
        Ok(token)
    }

    /// Resolve a session cookie to its user and acting org, or None if
    /// missing/expired. The org comes off the session row, never off the
    /// request.
    pub async fn sessions_resolve(&self, token: &str) -> Result<Option<(User, Option<Uuid>)>> {
        let row = crate::pgq::query(
            "SELECT id, user_id, org_id, expires_at FROM sessions WHERE token_hash = ?",
        )
        .bind(token_hash(token))
        .fetch_optional(self.db())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session_id: String = row.try_get("id")?;
        let user_id: String = row.try_get("user_id")?;
        let org: Option<Uuid> = row.try_get("org_id")?;
        let expires_at: String = row.try_get("expires_at")?;
        let expired = chrono::DateTime::parse_from_rfc3339(&expires_at)
            .map(|t| t.with_timezone(&Utc) < Utc::now())
            .unwrap_or(true);
        if expired {
            crate::pgq::query("DELETE FROM sessions WHERE id = ?")
                .bind(&session_id)
                .execute(self.db())
                .await?;
            return Ok(None);
        }
        crate::pgq::query("UPDATE sessions SET last_seen = ? WHERE id = ?")
            .bind(now_iso())
            .bind(&session_id)
            .execute(self.db())
            .await?;
        // A membership revoked after the session was minted must take effect
        // now, not at expiry: revocation that only stops future grants is the
        // failure the cryptographic design had, and this one is a WHERE clause.
        let Some(org_id) = org else {
            return Ok(self.users_by_id(&user_id).await?.map(|u| (u, None)));
        };
        if self.membership_of(&user_id, org_id).await?.is_none() {
            crate::pgq::query("DELETE FROM sessions WHERE id = ?")
                .bind(&session_id)
                .execute(self.db())
                .await?;
            return Ok(None);
        }
        Ok(self.users_by_id(&user_id).await?.map(|u| (u, Some(org_id))))
    }

    pub async fn sessions_destroy(&self, token: &str) -> Result<()> {
        crate::pgq::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash(token))
            .execute(self.db())
            .await?;
        Ok(())
    }
}
